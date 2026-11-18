//! 5-byte `jmp rel32` + relay-page inline hooks with atomic window stores and
//! `unhook()` restoration.

use hromatite_win::{ffi, log, memory as safe};

use crate::decode::decode_span;
use crate::mem::{
    PAGE_SIZE, alloc_near, flush, set_range_protect, write_mode, write_window,
};
use crate::tramp::{adjusted_disp32, branch_into_prologue};

/// Offset of the relay slot inside the trampoline page. The detour usually lives
/// far outside rel32 reach (wine maps ntdll at 0x6fff_xxxx_xxxx while a
/// manual-mapped payload sits near 0x1xx_xxxx), so the patched `jmp rel32` lands
/// on this relay, which does an absolute jump to the real detour.
pub const RELAY_OFF: usize = 0x100;

/// A prepared trampoline page and relay slot, before patching the target entry.
pub struct Prep {
    pub target: usize,
    pub prologue: usize,
    pub trampoline: usize,
    /// `trampoline + RELAY_OFF`; holds `ff 25 00000000 <qword detour>`
    pub relay: usize,
    alloc_base: usize,
    alloc_size: usize,
}

/// One installed 5-byte inline hook. `unhook` puts the original bytes back.
pub struct Hook {
    pub target: usize,
    pub detour: usize,
    pub trampoline: usize,
    pub prologue: usize,
    alloc_base: usize,
    alloc_size: usize,
    /// 16-byte aligned original bytes (32 of them: the 5-byte patch may straddle
    /// a 16-byte boundary, see `wmode 0`)
    orig32: [u8; 32],
    base16: usize,
    /// 8 = single 8-byte atomic store, 16 = `cmpxchg16b`, 0 = narrow fallback
    wmode: u8,
    detached: bool,
}

/// Build the trampoline (relocated prologue copy + absolute jump back) and
/// return it *without* patching the target, so the caller can publish the
/// trampoline address to its detour before any thread can enter it.
pub fn prepare(target: usize) -> Option<Prep> {
    prepare_span(target, 5)
}

/// Prepare a trampoline for a call-site patch whose copied span may need to
/// include control flow after the overwritten five-byte jump.
pub fn prepare_span(target: usize, min: usize) -> Option<Prep> {
    if target < 0x10000 || !safe::readable_range(target, 16) {
        log::log_tag("hook", &format!("prepare: {target:#x} not readable"));
        return None;
    }
    // 32 bytes cover any prologue we would accept; if the function sits at the
    // very end of its section, 16 is still enough for the 5 bytes we patch.
    let head = match safe::read_bytes(target, 32) {
        Some(h) => h,
        None => match safe::read_bytes(target, 16) {
            Some(h) => h,
            None => {
                log::log_tag(
                    "hook",
                    &format!("prepare: {target:#x} — 16 bytes at entry are not readable"),
                );
                return None;
            }
        },
    };
    let (prologue, insns) = match decode_span(&head, min) {
        Some(v) => v,
        None => {
            log::log_tag(
                "hook",
                &format!(
                    "prepare: unknown opcode in prologue at {target:#x} (bytes {}), not patching",
                    head.iter()
                        .take(8)
                        .map(|b| format!("{b:02x}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                ),
            );
            return None;
        }
    };
    if prologue > 32 {
        log::log_tag(
            "hook",
            &format!("prepare: prologue {prologue} bytes at {target:#x}, too long"),
        );
        return None;
    }
    let alloc_size = PAGE_SIZE;
    let tramp = alloc_near(target, alloc_size)?;
    let delta = tramp as i64 - target as i64;

    // relocated copy: fix rel8/rel32 and rip-relative disp32 by the shift
    let mut code: Vec<u8> = Vec::with_capacity(64);
    let mut off = 0usize;
    for ins in &insns {
        let mut chunk = head[off..off + ins.len].to_vec();
        if let Some((ro, bits)) = ins.rel_field() {
            let displacement = if bits == 32 {
                i32::from_le_bytes(chunk[ro..ro + 4].try_into().ok()?) as i64
            } else {
                chunk[ro] as i8 as i64
            };
            if branch_into_prologue(target, prologue, off, ins.len, displacement) {
                log::log_tag(
                    "hook",
                    &format!(
                        "prepare: relative branch at {target:#x}+{off} targets the overwritten prologue, refusing"
                    ),
                );
                return None;
            }
            if bits == 32 {
                let Some(v) = adjusted_disp32(
                    i32::from_le_bytes(chunk[ro..ro + 4].try_into().ok()?),
                    delta,
                ) else {
                    log::log_tag(
                        "hook",
                        &format!("prepare: rel32 at {target:#x}+{off} out of range"),
                    );
                    return None;
                };
                chunk[ro..ro + 4].copy_from_slice(&v.to_le_bytes());
            } else {
                let v = chunk[ro] as i8 as i64 - delta;
                if v < i8::MIN as i64 || v > i8::MAX as i64 {
                    log::log_tag(
                        "hook",
                        &format!(
                            "prepare: rel8 at {target:#x}+{off} needs a closer page, refusing"
                        ),
                    );
                    return None;
                }
                chunk[ro] = v as i8 as u8;
            }
        }
        if let Some(ro) = ins.rip_field() {
            let Some(v) = adjusted_disp32(
                i32::from_le_bytes(chunk[ro..ro + 4].try_into().ok()?),
                delta,
            ) else {
                log::log_tag(
                    "hook",
                    &format!("prepare: rip disp at {target:#x}+{off} out of range"),
                );
                return None;
            };
            chunk[ro..ro + 4].copy_from_slice(&v.to_le_bytes());
        }
        code.extend_from_slice(&chunk);
        off += ins.len;
    }
    // absolute jump back: ff 25 00000000 <qword target+prologue>
    code.extend_from_slice(&[0xFF, 0x25, 0, 0, 0, 0]);
    let back = (target + prologue) as u64;
    code.extend_from_slice(&back.to_le_bytes());
    if code.len() > RELAY_OFF {
        log::log_tag(
            "hook",
            &format!(
                "prepare: trampoline code {} bytes, relay slot busy",
                code.len()
            ),
        );
        return None;
    }

    // relay slot: ff 25 00000000 <qword detour> — filled in by `patch`
    let mut relay_bytes = [0u8; 14];
    relay_bytes[0] = 0xFF;
    relay_bytes[1] = 0x25;

    // SAFETY: tramp is a newly allocated full page owned by us. Length checks
    // above keep code before RELAY_OFF and the 14-byte relay inside that page.
    unsafe {
        core::ptr::copy_nonoverlapping(code.as_ptr(), tramp as *mut u8, code.len());
        core::ptr::copy_nonoverlapping(relay_bytes.as_ptr(), (tramp + RELAY_OFF) as *mut u8, 14);
    }
    // left RWX until `patch` fills the relay and flips it to RX
    if set_range_protect(tramp, PAGE_SIZE, ffi::PAGE_EXECUTE_READWRITE).is_none() {
        log::log_tag("hook", "prepare: VirtualProtect(RWX) on trampoline failed");
        return None;
    }
    Some(Prep {
        target,
        prologue,
        trampoline: tramp,
        relay: tramp + RELAY_OFF,
        alloc_base: tramp,
        alloc_size: PAGE_SIZE,
    })
}

/// Install the hook for an already prepared trampoline.
pub fn patch(p: &Prep, detour: usize) -> Option<Hook> {
    let rel = p.relay as i64 - (p.target as i64 + 5);
    if rel < i32::MIN as i64 || rel > i32::MAX as i64 {
        log::log_tag(
            "hook",
            &format!(
                "patch: relay {:#x} out of rel32 reach of {:#x}",
                p.relay, p.target
            ),
        );
        return None;
    }
    // A retained Prep was already published as RX on a previous install. Restore
    // write access before replacing its relay; then return it to RX below.
    if set_range_protect(p.trampoline, PAGE_SIZE, ffi::PAGE_EXECUTE_READWRITE).is_none() {
        log::log_tag("hook", "patch: VirtualProtect(RWX) on relay page failed");
        return None;
    }
    // publish the detour in the relay *before* the target can jump to it
    // SAFETY: relay points into our allocated writable page at RELAY_OFF; its
    // relay+6..+14 range is the qword slot reserved by the 14-byte relay encoding.
    unsafe {
        core::ptr::write_volatile((p.relay + 6) as *mut u64, detour as u64);
    }
    if set_range_protect(p.trampoline, PAGE_SIZE, ffi::PAGE_EXECUTE_READ).is_none() {
        log::log_tag("hook", "patch: VirtualProtect(RX) on trampoline failed");
        return None;
    }
    flush(p.trampoline, PAGE_SIZE);
    let base16 = p.target & !15usize;
    let o16 = p.target - base16;
    if o16 > 11 {
        log::log_tag(
            "hook",
            &format!(
                "patch: {:#x} straddles a 16-byte boundary; non-atomic fallback refused",
                p.target
            ),
        );
        return None;
    }
    let wmode = write_mode(o16)?;
    let mut orig32 = [0u8; 32];
    let got = safe::read_upto(base16, 32);
    if got.len() < o16 + 5 || got.len() < 16 {
        log::log_tag(
            "hook",
            &format!(
                "patch: only {} bytes readable at {base16:#x}, target {:#x}",
                got.len(),
                p.target
            ),
        );
        return None;
    }
    orig32[..got.len()].copy_from_slice(&got);

    let old_prot = set_range_protect(p.target, p.prologue + 8, ffi::PAGE_EXECUTE_READWRITE)?;

    let mut new32 = orig32;
    new32[o16] = 0xE9;
    new32[o16 + 1..o16 + 5].copy_from_slice(&(rel as i32).to_le_bytes());
    // SAFETY: new32 contains the full original target window plus the decoded
    // five-byte jump; target was made writable and wmode was derived from its alignment.
    let ok = unsafe { write_window(base16, p.target, &new32, wmode) };

    // restore: the old protection when it was executable, RX otherwise
    let back = if old_prot & 0xF0 != 0 {
        old_prot
    } else {
        ffi::PAGE_EXECUTE_READ
    };
    let _ = set_range_protect(p.target, p.prologue + 8, back);
    flush(p.target, 16);

    if !ok {
        log::log_tag(
            "hook",
            "patch: atomic store failed (window kept changing), target left intact",
        );
        return None;
    }
    log::log_tag(
        "hook",
        &format!(
            "patch: {:#x} -> jmp relay {:#x} (rel32 {rel:+#x}) -> detour {detour:#x}, prologue {} bytes, window {} bytes at {base16:#x}, protect {old_prot:#x}",
            p.target,
            p.relay,
            p.prologue,
            if wmode == 8 { 8 } else { 16 }
        ),
    );
    Some(Hook {
        target: p.target,
        detour,
        trampoline: p.trampoline,
        prologue: p.prologue,
        alloc_base: p.alloc_base,
        alloc_size: p.alloc_size,
        orig32,
        base16,
        wmode,
        detached: false,
    })
}

/// Install an inline hook at `target`: the first instruction(s) are copied to a
/// newly allocated page (forced near, so the patched `jmp` can reach it) and the
/// target is patched to jump to a relay in that page, which jumps absolutely to
/// `detour`.
pub fn hook_inline(target: usize, detour: usize) -> Option<Hook> {
    let p = prepare(target)?;
    patch(&p, detour)
}

impl Hook {
    /// True once [`Hook::unhook`] has restored the original bytes.
    pub fn is_detached(&self) -> bool {
        self.detached
    }

    /// Put the original bytes back and leave the trampoline page allocated.
    pub fn unhook(&mut self) -> bool {
        if self.detached {
            return true;
        }
        if self.wmode == 0 {
            log::log_tag(
                "hook",
                "unhook: refusing a non-atomic 16-byte-straddling restore",
            );
            return false;
        }
        let cur = self.orig32;
        let old = set_range_protect(self.target, self.prologue + 8, ffi::PAGE_EXECUTE_READWRITE);
        let ok = if old.is_some() {
            // SAFETY: cur is the original 32-byte snapshot for this exact
            // target; protection was restored to writable and wmode is unchanged.
            unsafe { write_window(self.base16, self.target, &cur, self.wmode) }
        } else {
            false
        };
        if let Some(v) = old {
            let back = if v & 0xF0 != 0 {
                v
            } else {
                ffi::PAGE_EXECUTE_READ
            };
            let _ = set_range_protect(self.target, self.prologue + 8, back);
        }
        flush(self.target, 16);
        if ok {
            self.detached = true;
            log::log_tag(
                "hook",
                &format!(
                    "unhook: {:#x} restored, trampoline {:#x} left allocated",
                    self.target, self.trampoline
                ),
            );
        } else {
            log::log_tag(
                "hook",
                &format!("unhook: {:#x} FAILED (protect or store)", self.target),
            );
        }
        let _ = (self.alloc_base, self.alloc_size);
        ok
    }

    pub fn describe(&self) -> String {
        format!(
            "{}target {:#x} -> detour {:#x}, trampoline {:#x}, prologue {}b, window {}b{}",
            if self.detached { "detached " } else { "" },
            self.target,
            self.detour,
            self.trampoline,
            self.prologue,
            if self.wmode == 8 { 8 } else { 16 },
            if self.wmode == 0 { " (narrow)" } else { "" },
        )
    }
}
