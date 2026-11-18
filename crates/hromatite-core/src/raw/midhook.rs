//! Mid-function inline hooks — the safetyhook capability `hook.rs` lacks.
//!
//! `hook.rs` patches a function *entry*: the first instruction(s) move to a
//! trampoline and the entry jumps to the detour. A mid-function hook picks an
//! arbitrary instruction boundary inside a function, so a detour can observe or
//! replace a value half-way through. The trampoline is the relocated copy of
//! the overwritten window followed by an absolute jump back to
//! `target + prologue`; the original is patched with `jmp rel32` through the
//! same relay slot the entry engine uses, so the detour may live anywhere.
//!
//! Relocation policy is deliberately exact: every copied instruction must keep
//! its absolute meaning. An opcode the decoder does not know, a relative
//! branch whose destination lies inside the overwritten window, or a `rel8`
//! displacement that no longer fits after the move is *refused* with a log
//! line instead of being approximated. safetyhook can synthesise a continue
//! block for those cases; that machinery is out of scope here.
//!
//! [`plan`] is the pure half (bytes in, bytes out) and is unit-tested on the
//! host; [`install`] adds the OS writes and only does real work where the
//! Win32 memory APIs exist.

use crate::ffi;
use crate::log;
use super::hook::{
    adjusted_disp32, alloc_near, branch_into_prologue, decode_span, flush, rel32_ok,
    set_range_protect, write_mode, write_window, PAGE_SIZE, RELAY_OFF,
};

/// Bytes replaced by the patched `jmp rel32`.
const PATCH_LEN: usize = 5;
/// Bytes of target code snapshotted for decoding and relocation.
const HEAD_LEN: usize = 32;
/// Smallest readable extent accepted when the 32-byte snapshot is truncated.
const MIN_HEAD_LEN: usize = 16;

// ---------------------------------------------------------------------------
// Pure planning
// ---------------------------------------------------------------------------

/// Why [`plan`] refused a mid-function hook.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlanError {
    /// An opcode outside the decoder subset appears in the window.
    Undecodable,
    /// The relocated window does not fit below the relay slot.
    WindowTooLong,
    /// A `rel8`/`rel32` branch targets the overwritten window.
    BranchIntoWindow,
    /// A `rel8` displacement no longer fits in `i8` after relocation.
    Rel8OutOfRange,
    /// A `rel32`/RIP displacement no longer fits in `i32` after relocation.
    Disp32OutOfRange,
    /// The five patched bytes would straddle a 16-byte boundary, so the patch
    /// could not be written atomically.
    UnalignedWindow,
}

/// The pure relocation result: everything needed to write the trampoline and
/// the patch, computed from the target bytes and the trampoline address.
#[derive(Debug, PartialEq)]
pub(crate) struct Plan {
    /// Length of the overwritten instruction window.
    pub prologue: usize,
    /// Relocated window followed by `jmp [rip+0] <target+prologue>`.
    pub code: Vec<u8>,
    /// The exact bytes the patch overwrites.
    pub orig: Vec<u8>,
    /// 16-byte aligned base of the patch window.
    pub base16: usize,
    /// Atomic write mode for the patch window (8 or 16).
    pub wmode: u8,
}

/// Signed displacement of a `rel8`/`rel32` field inside one instruction.
fn rel_displacement(chunk: &[u8], off: usize, bits: u8) -> Option<i64> {
    if bits == 32 {
        let raw = chunk.get(off..off + 4)?.try_into().ok()?;
        Some(i32::from_le_bytes(raw) as i64)
    } else {
        Some(chunk.get(off).copied()? as i8 as i64)
    }
}

/// Relocate the overwritten window for a trampoline at `trampoline`.
///
/// `head` must be the bytes at `target`; `trampoline` is the page the copy will
/// live in. The returned [`Plan::code`] is the copy plus the absolute jump back
/// to `target + prologue`.
pub(crate) fn plan(target: usize, head: &[u8], trampoline: usize) -> Result<Plan, PlanError> {
    let (prologue, insns) = decode_span(head, PATCH_LEN).ok_or(PlanError::Undecodable)?;
    if prologue > head.len() {
        return Err(PlanError::WindowTooLong);
    }
    let wmode = write_mode(target & 15).ok_or(PlanError::UnalignedWindow)?;
    let delta = trampoline as i64 - target as i64;
    let mut code: Vec<u8> = Vec::with_capacity(64);
    let mut off = 0usize;
    for ins in &insns {
        let mut chunk = head[off..off + ins.len].to_vec();
        if let Some((rel_off, bits)) = ins.rel {
            let disp = rel_displacement(&chunk, rel_off, bits).ok_or(PlanError::Undecodable)?;
            if branch_into_prologue(target, prologue, off, ins.len, disp) {
                return Err(PlanError::BranchIntoWindow);
            }
            if bits == 32 {
                let adjusted =
                    adjusted_disp32(disp as i32, delta).ok_or(PlanError::Disp32OutOfRange)?;
                chunk[rel_off..rel_off + 4].copy_from_slice(&adjusted.to_le_bytes());
            } else {
                let adjusted = disp - delta;
                let adjusted = i8::try_from(adjusted).map_err(|_| PlanError::Rel8OutOfRange)?;
                chunk[rel_off] = adjusted as u8;
            }
        }
        if let Some(rip_off) = ins.rip {
            let raw = chunk[rip_off..rip_off + 4]
                .try_into()
                .map_err(|_| PlanError::Undecodable)?;
            let adjusted = adjusted_disp32(i32::from_le_bytes(raw), delta)
                .ok_or(PlanError::Disp32OutOfRange)?;
            chunk[rip_off..rip_off + 4].copy_from_slice(&adjusted.to_le_bytes());
        }
        code.extend_from_slice(&chunk);
        off += ins.len;
    }
    // Absolute jump back to the first instruction after the window.
    code.extend_from_slice(&[0xFF, 0x25, 0x00, 0x00, 0x00, 0x00]);
    code.extend_from_slice(&(target + prologue).to_le_bytes());
    if code.len() > RELAY_OFF {
        return Err(PlanError::WindowTooLong);
    }
    Ok(Plan {
        prologue,
        code,
        orig: head[..prologue].to_vec(),
        base16: target & !15,
        wmode,
    })
}

// ---------------------------------------------------------------------------
// Live install
// ---------------------------------------------------------------------------

/// One installed mid-function hook. `unhook` puts the original bytes back.
pub struct MidHook {
    pub target: usize,
    pub detour: usize,
    pub trampoline: usize,
    pub prologue: usize,
    /// 16-byte aligned original bytes (32 of them: the 5-byte patch may straddle
    /// a 16-byte boundary).
    orig32: [u8; 32],
    base16: usize,
    /// 8 = single 8-byte atomic store, 16 = `cmpxchg16b`.
    wmode: u8,
    alloc_base: usize,
    alloc_size: usize,
    detached: bool,
}

/// Install a mid-function hook at `target`.
///
/// The instruction window starting at `target` is copied to a fresh page near
/// the target (so the patched `jmp rel32` can reach it), relocated, and
/// followed by an absolute jump back to `target + prologue`. The target is then
/// patched to jump to a relay in that page, which jumps absolutely to `detour`.
///
/// Returns `None` (with a log line) when the address is unreadable, the window
/// contains an opcode we do not decode, a relative branch lands inside the
/// window, or a displacement stops fitting after relocation. See [`plan`].
pub fn install(target: usize, detour: usize) -> Option<MidHook> {
    if target < 0x10000 {
        log::log_tag("midhook", &format!("install: {target:#x} is below 0x10000, refusing"));
        return None;
    }
    let head = match crate::safe::read_bytes(target, HEAD_LEN) {
        Some(head) => head,
        None => match crate::safe::read_bytes(target, MIN_HEAD_LEN) {
            Some(head) => head,
            None => {
                log::log_tag("midhook", &format!("install: {target:#x} is not readable"));
                return None;
            }
        },
    };
    let trampoline = match alloc_near(target, PAGE_SIZE) {
        Some(tramp) => tramp,
        None => {
            log::log_tag(
                "midhook",
                &format!("install: no trampoline page within rel32 of {target:#x}"),
            );
            return None;
        }
    };
    let p = match plan(target, &head, trampoline) {
        Ok(p) => p,
        Err(reason) => {
            log::log_tag("midhook", &format!("install: {target:#x} refused: {reason:?}"));
            return None;
        }
    };

    // Trampoline: relocated window at 0, relay slot at RELAY_OFF.
    let mut relay = [0u8; 14];
    relay[0] = 0xFF;
    relay[1] = 0x25;
    // SAFETY: `trampoline` is a fresh page owned by us; `plan` checked that the
    // code fits below RELAY_OFF and the 14-byte relay fits inside the same page.
    unsafe {
        core::ptr::copy_nonoverlapping(p.code.as_ptr(), trampoline as *mut u8, p.code.len());
        core::ptr::copy_nonoverlapping(relay.as_ptr(), (trampoline + RELAY_OFF) as *mut u8, 14);
    }
    // Left RW on purpose: the relay still has to be filled, and only the final
    // target patch makes anyone jump here.
    if set_range_protect(trampoline, PAGE_SIZE, ffi::PAGE_EXECUTE_READWRITE).is_none() {
        log::log_tag("midhook", "install: VirtualProtect(RWX) on trampoline failed");
        return None;
    }

    let relay_addr = trampoline + RELAY_OFF;
    if !rel32_ok(target + PATCH_LEN, relay_addr) {
        log::log_tag(
            "midhook",
            &format!("install: relay {relay_addr:#x} is out of rel32 reach of {target:#x}"),
        );
        return None;
    }
    if set_range_protect(trampoline, PAGE_SIZE, ffi::PAGE_EXECUTE_READWRITE).is_none() {
        log::log_tag("midhook", "install: VirtualProtect(RWX) on relay page failed");
        return None;
    }
    // Publish the detour before the target can jump to the relay.
    // SAFETY: `relay_addr + 6` is the qword slot of the relay in our own page.
    unsafe {
        core::ptr::write_volatile((relay_addr + 6) as *mut u64, detour as u64);
    }
    if set_range_protect(trampoline, PAGE_SIZE, ffi::PAGE_EXECUTE_READ).is_none() {
        log::log_tag("midhook", "install: VirtualProtect(RX) on trampoline failed");
        return None;
    }
    flush(trampoline, PAGE_SIZE);

    // Patch the target window.
    let base16 = p.base16;
    let o16 = target - base16;
    let mut orig32 = [0u8; 32];
    let got = crate::safe::read_upto(base16, 32);
    if got.len() < o16 + PATCH_LEN || got.len() < 16 {
        log::log_tag(
            "midhook",
            &format!(
                "install: only {} bytes readable at {base16:#x} for target {target:#x}",
                got.len()
            ),
        );
        return None;
    }
    orig32[..got.len()].copy_from_slice(&got);
    let rel = relay_addr as i64 - (target as i64 + PATCH_LEN as i64);
    let old_prot = match set_range_protect(target, p.prologue + 8, ffi::PAGE_EXECUTE_READWRITE) {
        Some(old) => old,
        None => {
            log::log_tag("midhook", &format!("install: VirtualProtect(RWX) at {target:#x} failed"));
            return None;
        }
    };
    let mut new32 = orig32;
    new32[o16] = 0xE9;
    new32[o16 + 1..o16 + PATCH_LEN].copy_from_slice(&(rel as i32).to_le_bytes());
    // SAFETY: `new32` is the original 32-byte window plus the decoded 5-byte
    // jump; the target was made writable and `wmode` came from its alignment.
    let ok = unsafe { write_window(base16, target, &new32, p.wmode) };
    let back = if old_prot & 0xF0 != 0 {
        old_prot
    } else {
        ffi::PAGE_EXECUTE_READ
    };
    let _ = set_range_protect(target, p.prologue + 8, back);
    flush(target, 16);
    if !ok {
        log::log_tag(
            "midhook",
            &format!("install: atomic store failed at {target:#x}, target left intact"),
        );
        return None;
    }
    log::log_tag(
        "midhook",
        &format!(
            "install: {target:#x} -> jmp relay {relay_addr:#x} (rel32 {rel:+#x}) -> detour {detour:#x}, window {}b, trampoline {trampoline:#x}",
            p.prologue
        ),
    );
    Some(MidHook {
        target,
        detour,
        trampoline,
        prologue: p.prologue,
        orig32,
        base16,
        wmode: p.wmode,
        alloc_base: trampoline,
        alloc_size: PAGE_SIZE,
        detached: false,
    })
}

impl MidHook {
    /// Write the saved window back atomically, without the OS protection calls.
    ///
    /// Split out of [`unhook`] so the restore payload can be exercised on an
    /// owned buffer; `unhook` adds the `VirtualProtect` bracket around it.
    fn restore_atomic(&self) -> bool {
        if self.wmode == 0 {
            return false;
        }
        // SAFETY: `orig32` is the snapshot of exactly this target window;
        // `base16` is 16-byte aligned and `wmode` was derived from the target's
        // alignment when the hook was installed.
        unsafe { write_window(self.base16, self.target, &self.orig32, self.wmode) }
    }

    /// Put the original bytes back. The trampoline page is kept: another thread
    /// may still be executing it.
    pub fn unhook(&mut self) -> bool {
        if self.detached {
            return true;
        }
        if self.wmode == 0 {
            log::log_tag(
                "midhook",
                "unhook: refusing a non-atomic 16-byte-straddling restore",
            );
            return false;
        }
        let old = set_range_protect(self.target, self.prologue + 8, ffi::PAGE_EXECUTE_READWRITE);
        let ok = old.is_some_and(|_| self.restore_atomic());
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
                "midhook",
                &format!(
                    "unhook: {:#x} restored, trampoline {:#x} left allocated",
                    self.target, self.trampoline
                ),
            );
        } else {
            log::log_tag(
                "midhook",
                &format!("unhook: {:#x} FAILED (protect or store)", self.target),
            );
        }
        // Keep the page: another thread may still be executing the trampoline.
        let _ = (self.alloc_base, self.alloc_size);
        ok
    }

    pub fn describe(&self) -> String {
        format!(
            "{}mid target {:#x} -> detour {:#x}, trampoline {:#x}, window {}b, patch {}b",
            if self.detached { "detached " } else { "" },
            self.target,
            self.detour,
            self.trampoline,
            self.prologue,
            if self.wmode == 8 { 8 } else { 16 },
        )
    }

    /// Test-only constructor: build a hook whose restore payload points at an
    /// owned buffer, so `restore_atomic` can be exercised without OS calls.
    #[cfg(test)]
    fn from_parts(target: usize, base16: usize, wmode: u8, orig32: [u8; 32], prologue: usize) -> Self {
        Self {
            target,
            detour: 0,
            trampoline: 0,
            prologue,
            orig32,
            base16,
            wmode,
            alloc_base: 0,
            alloc_size: 0,
            detached: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TARGET: usize = 0x10_0000;

    #[test]
    fn plan_relocates_rip_relative_and_jumps_back_past_the_window() {
        // mov rax, [rip+0x1000] (7 bytes); sub rsp, 0x28
        let head = [
            0x48, 0x8B, 0x05, 0x00, 0x10, 0x00, 0x00, 0x48, 0x83, 0xEC, 0x28,
        ];
        let tramp = TARGET + 0x1_0000;
        let p = plan(TARGET, &head, tramp).expect("plan");
        assert_eq!(p.prologue, 7, "the first instruction alone covers the jmp");
        assert_eq!(p.orig, head[..7], "the exact overwritten bytes are recorded");
        // disp 0x1000 at next-rip TARGET+7; moved to tramp+7 it must shift by
        // -delta so the absolute address is unchanged.
        let got = i32::from_le_bytes(p.code[3..7].try_into().unwrap());
        assert_eq!(got, 0x1000 - 0x1_0000);
        // Tail: ff 25 00 00 00 00 + qword(TARGET + 7).
        let tail = &p.code[p.code.len() - 14..];
        assert_eq!(&tail[..6], &[0xFF, 0x25, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(
            u64::from_le_bytes(tail[6..].try_into().unwrap()),
            (TARGET + 7) as u64,
            "the trampoline continues at target + prologue"
        );
    }

    #[test]
    fn a_branch_into_the_overwritten_window_is_refused() {
        // jmp rel8 +0 -> TARGET+2, inside the 5-byte window; nops fill to 5.
        let rel8 = [0xEB, 0x00, 0x90, 0x90, 0x90];
        assert_eq!(
            plan(TARGET, &rel8, TARGET + 0x1000),
            Err(PlanError::BranchIntoWindow)
        );
        // jmp rel32 -3 -> TARGET+2, also inside.
        let rel32 = [0xE9, 0xFD, 0xFF, 0xFF, 0xFF];
        assert_eq!(
            plan(TARGET, &rel32, TARGET + 0x1000),
            Err(PlanError::BranchIntoWindow)
        );
        // A branch to the continuation (TARGET+5) is fine: it is outside.
        // rel32, so the 4 KiB move cannot push it out of range.
        let cont = [0xE9, 0x00, 0x00, 0x00, 0x00];
        assert!(plan(TARGET, &cont, TARGET + 0x1000).is_ok());
    }

    #[test]
    fn a_rel8_that_cannot_reach_after_the_move_is_refused() {
        // jmp rel8 +5 -> TARGET+7, outside the window.
        let head = [0xEB, 0x05, 0x90, 0x90, 0x90];
        assert_eq!(
            plan(TARGET, &head, TARGET + 0x1000),
            Err(PlanError::Rel8OutOfRange),
            "a 4 KiB move cannot stay in i8"
        );
        // The same bytes survive a zero-distance move.
        let p = plan(TARGET, &head, TARGET).expect("delta zero");
        assert_eq!(p.code[1], 5);
    }

    #[test]
    fn an_unknown_opcode_is_refused() {
        // VEX prefix: deliberately outside the decoder subset.
        let head = [0xC5, 0xF8, 0x77, 0x90, 0x90];
        assert_eq!(plan(TARGET, &head, TARGET + 0x1000), Err(PlanError::Undecodable));
    }

    #[test]
    fn a_window_that_straddles_a_16_byte_boundary_is_refused() {
        let head = [0x90, 0x90, 0x90, 0x90, 0x90];
        for off in 12..16 {
            assert_eq!(
                plan(TARGET + off, &head, TARGET),
                Err(PlanError::UnalignedWindow),
                "target offset {off} cannot be patched atomically"
            );
        }
    }

    #[test]
    fn unhook_writes_the_saved_window_back_verbatim() {
        #[repr(align(16))]
        struct Window([u8; 32]);

        let mut window = Window([0xAA; 32]);
        let original = window.0;
        let base = window.0.as_mut_ptr() as usize;
        for o16 in [0usize, 3, 8, 11] {
            let target = base + o16;
            let wmode = write_mode(o16).expect("aligned patch mode");
            let mut patched = original;
            patched[o16] = 0xE9;
            patched[o16 + 1..o16 + 5].copy_from_slice(&[0x10, 0x20, 0x30, 0x40]);
            // SAFETY: `base` is a live 16-byte-aligned 32-byte buffer and
            // `wmode` was derived from this exact target alignment.
            assert!(unsafe { write_window(base, target, &patched, wmode) });
            assert_ne!(window.0, original, "the patch must land first");
            let hook = MidHook::from_parts(target, base, wmode, original, 5);
            assert!(hook.restore_atomic(), "restore at offset {o16}");
            assert_eq!(window.0, original, "window restored at offset {o16}");
        }
    }

    #[test]
    fn install_refuses_an_unreadable_target() {
        assert!(install(0, 0x20_0000).is_none(), "null page");
        assert!(install(0x1000, 0x20_0000).is_none(), "below 0x10000");
    }
}
