//! Inline (trampoline) hook + frame-based tick source.
//!
//! Plan 3: the payload must stop owning a thread. A stub thread that lives
//! inside `DllMain` is a) visible (an extra thread on the module's account) and
//! b) incompatible with `--seal`, which waits for the entry to return. So we
//! patch a function the game already calls *every frame* (`QueryPerformanceCounter`
//! is the cheapest such anchor: Unity calls it constantly from several threads)
//! and do our work on the game's own stack, for a few microseconds every
//! `HROMATITE_TICK_MS`.
//!
//! Layout: the patched `jmp rel32` lands on a *relay* inside the trampoline page
//! (allocated within ±2 GiB of the target), and the relay performs an absolute
//! jump to the detour. That indirection is not optional: wine maps ntdll around
//! 0x6fff_xxxx_xxxx while a manual-mapped payload lives near 0x1xx_xxxx, a gap
//! of ~25 TB that no rel32 can span.
//!
//! Rules of the house (same spirit as `safe`):
//!   * never patch what we have not decoded — an unknown opcode in the prologue
//!     means "refuse the hook", not "guess";
//!   * the patched jump is written with one atomic store (8 or 16 bytes) so a
//!     thread that is executing the target's prologue right now cannot fetch a
//!     half-written instruction;
//!   * the tick body runs inside `log::guard`: a Rust panic unwinding through
//!     the game's frame is UB, and under Proton it shows up as a dead game.
//!
//! Known interaction: in `--seal` the injector XORs the image in place as soon
//! as `DllMain` returns, so the detour (which lives in the image) turns to
//! garbage and the next tick executes random bytes. Frame hook and seal are
//! therefore mutually exclusive until plan 4 moves the detour into the
//! trampoline page.

use crate::console;
use crate::ffi;
use crate::log;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

const MEM_FREE: u32 = 0x0001_0000;
const PAGE_SIZE: usize = 0x1000;
/// Stay well inside the ±2 GiB reach of a `jmp rel32`.
const REL32_SPAN: usize = 0x7F00_0000;

// ---------------------------------------------------------------------------
// x64 instruction-length decoder (prologue subset)
// ---------------------------------------------------------------------------

/// One decoded instruction: where its relative/rip-relative field sits, so the
/// trampoline copy can be relocated.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Insn {
    pub len: usize,
    /// `(offset of the field inside the instruction, bits)` for a rel8/rel32
    /// branch target.
    pub rel: Option<(usize, u8)>,
    /// Offset of a `RIP`-relative `disp32` inside the instruction.
    pub rip: Option<usize>,
}

struct ModRm {
    /// bytes occupied by modrm (+sib, +disp)
    bytes: usize,
    /// offset of a rip-relative disp32, relative to the modrm byte
    rip: Option<usize>,
}

fn modrm_len(b: &[u8], at: usize) -> Option<ModRm> {
    let m = *b.get(at)?;
    let md = m >> 6;
    let rm = m & 7;
    let mut bytes = 1usize;
    let mut rip = None;
    if md != 3 {
        let mut disp = 0usize;
        if rm == 4 {
            // SIB follows; base == 5 with mod == 0 means disp32-only
            let sib = *b.get(at + bytes)?;
            bytes += 1;
            if md == 0 && (sib & 7) == 5 {
                disp = 4;
            }
        } else if md == 0 && rm == 5 {
            disp = 4;
            rip = Some(bytes);
        }
        match md {
            0 => bytes += disp,
            1 => bytes += 1,
            2 => bytes += 4,
            _ => {}
        }
    }
    Some(ModRm { bytes, rip })
}

/// Decode one instruction. `None` = opcode outside the subset we trust.
pub fn decode_one(b: &[u8]) -> Option<Insn> {
    let mut i = 0usize;
    let mut rex_w = false;
    // legacy prefixes, then at most one REX (which must come last)
    let mut guard = 0;
    loop {
        guard += 1;
        if guard > 9 {
            return None;
        }
        let c = *b.get(i)?;
        match c {
            0x66 | 0x67 | 0xF0 | 0xF2 | 0xF3 | 0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65 => i += 1,
            0x40..=0x4F => {
                rex_w = (c & 0x08) != 0;
                i += 1;
            }
            _ => break,
        }
    }
    let op = *b.get(i)?;
    let mut rel = None;
    let mut rip = None;
    let total;

    match op {
        0x0F => {
            let op2 = *b.get(i + 1)?;
            match op2 {
                // endbr64 / endbr32 (CET landing pad — very common at entry)
                0x1E => {
                    let m3 = *b.get(i + 2)?;
                    if m3 == 0xFA || m3 == 0xFB {
                        total = i + 3;
                    } else {
                        return None;
                    }
                }
                0x05 | 0x0B => total = i + 2, // syscall / ud2
                0x80..=0x8F => {
                    rel = Some((i + 2, 32));
                    total = i + 6;
                }
                0x10..=0x17 // movups/movss/movupd
                | 0x28 | 0x29 // movaps
                | 0x2E | 0x2F // ucomiss
                | 0x40..=0x4F // cmovcc
                | 0x51 // sqrtss
                | 0x54..=0x5F // andps .. divps
                | 0x6E | 0x6F // movd / movdqa load
                | 0x7E | 0x7F // movd / movdqa store
                | 0x90..=0x9F // setcc
                | 0xB6 | 0xB7 | 0xBE | 0xBF // movzx / movsx
                | 0xC6 // shuffle
                | 0xD6 // movq
                | 0xEF // pxor
                | 0x1F => { // multi-byte nop
                    let m = modrm_len(b, i + 2)?;
                    if let Some(o) = m.rip {
                        rip = Some(i + 2 + o);
                    }
                    total = i + 2 + m.bytes;
                }
                _ => return None,
            }
        }
        // push/pop r64
        0x50..=0x5F => total = i + 1,
        0x90 | 0x98 | 0x99 | 0x9C | 0x9D | 0xC3 | 0xC9 | 0xCC | 0xF4 => total = i + 1,
        0xC2 => total = i + 3, // ret imm16
        0x6A => total = i + 2, // push imm8
        0x68 => total = i + 5, // push imm32
        0xA8 => total = i + 2,
        0xA9 => total = i + 5,
        0xB0..=0xB7 => total = i + 2, // mov r8, imm8
        0xB8..=0xBF => total = i + if rex_w { 9 } else { 5 }, // mov r64, imm64 (opcode + 8)
        0xE8 | 0xE9 => {
            rel = Some((i + 1, 32));
            total = i + 5;
        }
        0xEB => {
            rel = Some((i + 1, 8));
            total = i + 2;
        }
        0x70..=0x7F | 0xE0..=0xE3 => {
            rel = Some((i + 1, 8));
            total = i + 2;
        }
        // group 1 / moves with immediate
        0x80 => {
            let m = modrm_len(b, i + 1)?;
            total = i + 1 + m.bytes + 1;
        }
        0x81 => {
            let m = modrm_len(b, i + 1)?;
            total = i + 1 + m.bytes + 4;
        }
        0x83 => {
            let m = modrm_len(b, i + 1)?;
            total = i + 1 + m.bytes + 1;
        }
        0xC0 | 0xC1 => {
            let m = modrm_len(b, i + 1)?;
            total = i + 1 + m.bytes + 1;
        }
        0xC6 => {
            let m = modrm_len(b, i + 1)?;
            total = i + 1 + m.bytes + 1;
        }
        0xC7 => {
            let m = modrm_len(b, i + 1)?;
            total = i + 1 + m.bytes + 4;
        }
        // group 3: only `test` carries an immediate
        0xF6 | 0xF7 => {
            let m = modrm_len(b, i + 1)?;
            let reg = (b[i + 1] >> 3) & 7;
            let imm = if reg <= 1 { if op == 0xF6 { 1 } else { 4 } } else { 0 };
            total = i + 1 + m.bytes + imm;
        }
        // plain modrm forms: alu/mov/lea/test/xchg/shift/inc-dec
        0x00..=0x03
        | 0x08..=0x0B
        | 0x10..=0x13
        | 0x18..=0x1B
        | 0x20..=0x23
        | 0x28..=0x2B
        | 0x30..=0x33
        | 0x38..=0x3B
        | 0x63 // movsxd
        | 0x84..=0x8F
        | 0xD0..=0xD3
        | 0xD8..=0xDF // x87 (modrm, no immediate)
        | 0xFE | 0xFF => {
            let m = modrm_len(b, i + 1)?;
            if let Some(o) = m.rip {
                rip = Some(i + 1 + o);
            }
            total = i + 1 + m.bytes;
        }
        _ => return None,
    }

    if total > b.len() {
        return None;
    }
    Some(Insn { len: total, rel, rip })
}

/// Decode until at least `min` bytes are covered; return the covered length and
/// the decoded instructions (needed to relocate the copy).
fn decode_span(bytes: &[u8], min: usize) -> Option<(usize, Vec<Insn>)> {
    let mut off = 0usize;
    let mut out = Vec::with_capacity(8);
    while off < min {
        if out.len() > 32 || off >= bytes.len() {
            return None;
        }
        let ins = decode_one(&bytes[off..])?;
        if ins.len == 0 {
            return None;
        }
        off += ins.len;
        out.push(ins);
    }
    Some((off, out))
}

/// Length of a prologue that is long enough to hold a 5-byte `jmp rel32`.
/// `None` = an opcode we do not decode was hit, so the caller must not patch.
pub fn prologue_len(bytes: &[u8], min: usize) -> Option<usize> {
    decode_span(bytes, min).map(|(n, _)| n)
}

// ---------------------------------------------------------------------------
// Trampoline allocation
// ---------------------------------------------------------------------------

fn rel32_ok(from: usize, to: usize) -> bool {
    let d = to as i64 - from as i64;
    d >= i32::MIN as i64 && d <= i32::MAX as i64
}

/// Nearest `MEM_FREE` region within rel32 reach, via `VirtualQuery` (cheap: one
/// call per region, not per page).
fn nearest_free_near(target: usize, size: usize) -> Option<usize> {
    let lo = target.saturating_sub(REL32_SPAN).max(0x10000);
    let hi = target.saturating_add(REL32_SPAN);
    let mut best: Option<(usize, usize)> = None; // (distance, address)
    let mut addr = lo & !0xFFFFusize;
    let mut hops = 0usize;
    while addr < hi && hops < 100_000 {
        hops += 1;
        let mut mbi = ffi::MEMORY_BASIC_INFORMATION::default();
        let n = unsafe {
            ffi::VirtualQuery(
                addr as *const core::ffi::c_void,
                &mut mbi,
                core::mem::size_of::<ffi::MEMORY_BASIC_INFORMATION>(),
            )
        };
        if n == 0 || mbi.RegionSize == 0 {
            break;
        }
        let region_end = mbi.BaseAddress.saturating_add(mbi.RegionSize);
        if mbi.State == MEM_FREE && mbi.RegionSize >= size {
            let cand = (mbi.BaseAddress + 0xFFFF) & !0xFFFFusize;
            if cand >= 0x10000
                && cand.saturating_add(size) <= region_end
                && rel32_ok(cand, target)
                && rel32_ok(target, cand)
            {
                let d = cand.abs_diff(target);
                if best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, cand));
                }
            }
        }
        if region_end <= addr {
            break;
        }
        addr = region_end;
    }
    best.map(|(_, a)| a)
}

/// Brute-force fallback: try `target ± k*64K` outward, as the plan specifies.
fn enum_near(target: usize, size: usize) -> Option<usize> {
    let step = 0x10000usize;
    let mut logged = 0u32;
    let mut tries = 0u32;
    for k in 1..=4096usize {
        for sgn in [1i64, -1] {
            let cand = target as i64 + sgn * (k * step) as i64;
            if cand <= 0x10000 {
                continue;
            }
            let cand = cand as usize;
            if !rel32_ok(cand, target) {
                continue;
            }
            tries += 1;
            let p = unsafe {
                ffi::VirtualAlloc(
                    cand,
                    size,
                    ffi::MEM_RESERVE | ffi::MEM_COMMIT,
                    ffi::PAGE_READWRITE,
                )
            };
            if !p.is_null() {
                return Some(p as usize);
            }
            if logged < 5 {
                logged += 1;
                log::log_tag("hook", &format!("alloc_near: {cand:#x} busy ({tries})"));
            }
        }
    }
    None
}

/// Allocate `size` bytes (RW, page-aligned) within ±2 GiB of `target`, so a
/// 5-byte `jmp rel32` can reach both ways.
pub fn alloc_near(target: usize, size: usize) -> Option<usize> {
    let size = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    if let Some(c) = nearest_free_near(target, size) {
        let p = unsafe {
            ffi::VirtualAlloc(
                c,
                size,
                ffi::MEM_RESERVE | ffi::MEM_COMMIT,
                ffi::PAGE_READWRITE,
            )
        };
        if !p.is_null() {
            let got = p as usize;
            log::log_tag(
                "hook",
                &format!(
                    "alloc_near: trampoline page {got:#x} (target {target:#x}, delta {:.1} MB)",
                    (got as i64 - target as i64) as f64 / (1024.0 * 1024.0)
                ),
            );
            return Some(got);
        }
        log::log_tag("hook", &format!("alloc_near: VirtualAlloc at {c:#x} refused"));
    }
    let p = enum_near(target, size);
    if let Some(got) = p {
        log::log_tag(
            "hook",
            &format!("alloc_near: enum fallback page {got:#x} (target {target:#x})"),
        );
    }
    p
}

// ---------------------------------------------------------------------------
// Hook
// ---------------------------------------------------------------------------

/// One installed inline hook. `unhook` puts the original bytes back.
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

/// Write `new` into the 16-byte window at `base16` atomically.
///
/// * `wmode 8` — the 5 patched bytes sit in one 8-byte aligned word; a plain
///   aligned `mov` store is atomic on x86-64.
/// * `wmode 16` — `lock cmpxchg16b` (Nehalem+); retried if the game wrote the
///   window meanwhile.
/// * `wmode 0` — `target & 15` is 12..15, so the 5 bytes straddle two 16-byte
///   blocks: write the `rel32` tail first, then the opcode. A thread that
///   fetches in between sees the original opcode with a patched displacement —
///   the only non-atomic case.
unsafe fn write_window(base16: usize, target: usize, new32: &[u8; 32], wmode: u8) -> bool {
    let o16 = target - base16;
    match wmode {
        8 => {
            let base = if o16 >= 8 { base16 + 8 } else { base16 };
            let off = o16 - (base - base16);
            let mut w = [0u8; 8];
            w.copy_from_slice(&new32[off..off + 8]);
            unsafe { core::ptr::write_volatile(base as *mut u64, u64::from_le_bytes(w)) };
            true
        }
        16 => {
            let old = unsafe { core::ptr::read_volatile(base16 as *const [u8; 16]) };
            let mut oldv = [0u64; 2];
            for i in 0..2 {
                let mut w = [0u8; 8];
                w.copy_from_slice(&old[i * 8..i * 8 + 8]);
                oldv[i] = u64::from_le_bytes(w);
            }
            let mut newv = [0u64; 2];
            for i in 0..2 {
                let mut w = [0u8; 8];
                w.copy_from_slice(&new32[i * 8..i * 8 + 8]);
                newv[i] = u64::from_le_bytes(w);
            }
            for _ in 0..8 {
                let lo = oldv[0];
                let hi = oldv[1];
                let ok: u8;
                // rbx is reserved by LLVM, so it is borrowed inside the asm
                // (cmpxchg16b insists on rbx:rcx as the new value).
                unsafe {
                    core::arch::asm!(
                        "push rbx",
                        "mov rbx, {nlo}",
                        "lock cmpxchg16b [{p}]",
                        "sete {ok}",
                        "pop rbx",
                        p = in(reg) base16,
                        nlo = in(reg) newv[0],
                        ok = out(reg_byte) ok,
                        inout("rax") lo => _,
                        inout("rdx") hi => _,
                        in("rcx") newv[1],
                    );
                }
                if ok != 0 {
                    return true;
                }
                // window changed under us: re-read and use the fresh compare value
                let fresh = unsafe { core::ptr::read_volatile(base16 as *const [u8; 16]) };
                for i in 0..2 {
                    let mut w = [0u8; 8];
                    w.copy_from_slice(&fresh[i * 8..i * 8 + 8]);
                    oldv[i] = u64::from_le_bytes(w);
                }
            }
            false
        }
        _ => {
            // narrow: rel32 part first, opcode last
            let tail = [new32[o16 + 1], new32[o16 + 2], new32[o16 + 3], new32[o16 + 4]];
            unsafe {
                core::ptr::write_volatile((target + 1) as *mut [u8; 4], tail);
                core::ptr::write_volatile(target as *mut u8, new32[o16]);
            }
            true
        }
    }
}

fn set_range_protect(addr: usize, len: usize, prot: u32) -> Option<u32> {
    let page = addr & !(PAGE_SIZE - 1);
    let end = (addr + len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let mut old = 0u32;
    let ok = unsafe { ffi::VirtualProtect(page as *mut u8, end - page, prot, &mut old) };
    (ok != 0).then_some(old)
}

fn flush(addr: usize, len: usize) {
    unsafe {
        ffi::FlushInstructionCache(ffi::GetCurrentProcess(), addr as *const u8, len);
    }
}

/// Offset of the relay slot inside the trampoline page. The detour usually lives
/// far outside rel32 reach (wine maps ntdll at 0x6fff_xxxx_xxxx while a
/// manual-mapped payload sits near 0x1xx_xxxx), so the patched `jmp rel32` lands
/// on this relay, which does an absolute jump to the real detour.
const RELAY_OFF: usize = 0x100;

struct Prep {
    target: usize,
    prologue: usize,
    trampoline: usize,
    /// `trampoline + RELAY_OFF`; holds `ff 25 00000000 <qword detour>`
    relay: usize,
    alloc_base: usize,
    alloc_size: usize,
}

/// Build the trampoline (relocated prologue copy + absolute jump back) and
/// return it *without* patching the target, so the caller can publish the
/// trampoline address to its detour before any thread can enter it.
fn prepare(target: usize) -> Option<Prep> {
    if target < 0x10000 || !crate::safe::readable_range(target, 16) {
        log::log_tag("hook", &format!("prepare: {target:#x} not readable"));
        return None;
    }
    // 32 bytes cover any prologue we would accept; if the function sits at the
    // very end of its section, 16 is still enough for the 5 bytes we patch.
    let head = match crate::safe::read_bytes(target, 32) {
        Some(h) => h,
        None => match crate::safe::read_bytes(target, 16) {
            Some(h) => h,
            None => {
                log::log_tag("hook", &format!("prepare: {target:#x} — 16 bytes at entry are not readable"));
                return None;
            }
        },
    };
    let (prologue, insns) = match decode_span(&head, 5) {
        Some(v) => v,
        None => {
            log::log_tag(
                "hook",
                &format!(
                    "prepare: unknown opcode in prologue at {target:#x} (bytes {}), not patching",
                    head.iter().take(8).map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
                ),
            );
            return None;
        }
    };
    if prologue > 32 {
        log::log_tag("hook", &format!("prepare: prologue {prologue} bytes at {target:#x}, too long"));
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
        if let Some((ro, bits)) = ins.rel {
            if bits == 32 {
                let v = i32::from_le_bytes(chunk[ro..ro + 4].try_into().ok()?) as i64 + delta;
                if v < i32::MIN as i64 || v > i32::MAX as i64 {
                    log::log_tag("hook", &format!("prepare: rel32 at {target:#x}+{off} out of range"));
                    return None;
                }
                chunk[ro..ro + 4].copy_from_slice(&(v as i32).to_le_bytes());
            } else {
                let v = chunk[ro] as i8 as i64 + delta;
                if v < i8::MIN as i64 || v > i8::MAX as i64 {
                    log::log_tag(
                        "hook",
                        &format!("prepare: rel8 at {target:#x}+{off} needs a closer page, refusing"),
                    );
                    return None;
                }
                chunk[ro] = v as i8 as u8;
            }
        }
        if let Some(ro) = ins.rip {
            let v = i32::from_le_bytes(chunk[ro..ro + 4].try_into().ok()?) as i64 + delta;
            if v < i32::MIN as i64 || v > i32::MAX as i64 {
                log::log_tag("hook", &format!("prepare: rip disp at {target:#x}+{off} out of range"));
                return None;
            }
            chunk[ro..ro + 4].copy_from_slice(&(v as i32).to_le_bytes());
        }
        code.extend_from_slice(&chunk);
        off += ins.len;
    }
    // absolute jump back: ff 25 00000000 <qword target+prologue>
    code.extend_from_slice(&[0xFF, 0x25, 0, 0, 0, 0]);
    let back = (target + prologue) as u64;
    code.extend_from_slice(&back.to_le_bytes());
    if code.len() > RELAY_OFF {
        log::log_tag("hook", &format!("prepare: trampoline code {} bytes, relay slot busy", code.len()));
        return None;
    }

    // relay slot: ff 25 00000000 <qword detour> — filled in by `patch`
    let mut relay_bytes = [0u8; 14];
    relay_bytes[0] = 0xFF;
    relay_bytes[1] = 0x25;

    unsafe {
        core::ptr::copy_nonoverlapping(code.as_ptr(), tramp as *mut u8, code.len());
        core::ptr::copy_nonoverlapping(relay_bytes.as_ptr(), (tramp + RELAY_OFF) as *mut u8, 14);
    }
    // left RW on purpose: `patch` still has to fill the relay, and only after
    // that does anyone jump here (the target is patched last)
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
fn patch(p: &Prep, detour: usize) -> Option<Hook> {
    // The patched instruction can only reach the relay (same ±2 GiB); the relay
    // then jumps absolutely to the detour wherever it lives.
    let rel = p.relay as i64 - (p.target as i64 + 5);
    if rel < i32::MIN as i64 || rel > i32::MAX as i64 {
        log::log_tag("hook", &format!("patch: relay {:#x} out of rel32 reach of {:#x}", p.relay, p.target));
        return None;
    }
    // publish the detour in the relay *before* the target can jump to it
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
    let wmode: u8 = if o16 <= 3 { 8 } else if o16 <= 11 { 16 } else { 0 };
    // 32 bytes: a 5-byte patch can start at offset 13 of a 16-byte block
    let mut orig32 = [0u8; 32];
    let got = crate::safe::read_upto(base16, 32);
    if got.len() < o16 + 5 || got.len() < 16 {
        log::log_tag(
            "hook",
            &format!("patch: only {} bytes readable at {base16:#x}, target {:#x}", got.len(), p.target),
        );
        return None;
    }
    orig32[..got.len()].copy_from_slice(&got);

    let old_prot = set_range_protect(p.target, p.prologue + 8, ffi::PAGE_EXECUTE_READWRITE)?;

    let mut new32 = orig32;
    new32[o16] = 0xE9;
    new32[o16 + 1..o16 + 5].copy_from_slice(&(rel as i32).to_le_bytes());
    let ok = unsafe { write_window(base16, p.target, &new32, wmode) };

    // restore: the old protection when it was executable, RX otherwise
    let back = if old_prot & 0xF0 != 0 { old_prot } else { ffi::PAGE_EXECUTE_READ };
    let _ = set_range_protect(p.target, p.prologue + 8, back);
    flush(p.target, 16);

    if !ok {
        log::log_tag("hook", "patch: atomic store failed (window kept changing), target left intact");
        return None;
    }
    log::log_tag(
        "hook",
        &format!(
            "patch: {:#x} -> jmp relay {:#x} (rel32 {rel:+#x}) -> detour {detour:#x}, prologue {} bytes, window {} bytes at {base16:#x}, protect {old_prot:#x}",
            p.target, p.relay, p.prologue,
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
/// `detour` — the detour itself may live anywhere in the address space (wine
/// keeps ntdll near 0x6fff_xxxx_xxxx, far outside rel32 reach of a payload).
///
/// Thread safety: the patch itself is one atomic store, so a concurrently
/// executing thread sees either the old or the new instruction stream. It cannot
/// see *both* halves of the 5-byte jump. A thread already inside the prologue
/// range when the store lands keeps executing the old bytes it fetched — the
/// trampoline holds the same bytes, so behaviour is unchanged.
pub fn hook_inline(target: usize, detour: usize) -> Option<Hook> {
    let p = prepare(target)?;
    patch(&p, detour)
}

impl Hook {
    /// Put the original bytes back and release the trampoline.
    pub fn unhook(&mut self) -> bool {
        if self.detached {
            return true;
        }
        // the whole 16-byte window is rewritten, so the original snapshot is
        // the payload — no read-back needed
        let cur = self.orig32;
        let old = set_range_protect(self.target, self.prologue + 8, ffi::PAGE_EXECUTE_READWRITE);
        let ok = if old.is_some() {
            unsafe { write_window(self.base16, self.target, &cur, self.wmode) }
        } else {
            false
        };
        if let Some(v) = old {
            let back = if v & 0xF0 != 0 { v } else { ffi::PAGE_EXECUTE_READ };
            let _ = set_range_protect(self.target, self.prologue + 8, back);
        }
        flush(self.target, 16);
        if ok {
            self.detached = true;
            log::log_tag(
                "hook",
                &format!("unhook: {:#x} restored, trampoline {:#x} left allocated", self.target, self.trampoline),
            );
        } else {
            log::log_tag("hook", &format!("unhook: {:#x} FAILED (protect or store)", self.target));
        }
        // keep the page: another thread may still be executing the trampoline
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

// ---------------------------------------------------------------------------
// Tick source
// ---------------------------------------------------------------------------

static TICKS: AtomicU64 = AtomicU64::new(0);
static LAST_MS: AtomicU64 = AtomicU64::new(0);
static WORK_MS: AtomicU64 = AtomicU64::new(0);
static LOG_MS: AtomicU64 = AtomicU64::new(0);
static START_MS: AtomicU64 = AtomicU64::new(0);
static INTERVAL_MS: AtomicU64 = AtomicU64::new(250);
static BUSY: AtomicBool = AtomicBool::new(false);
static INSTALLED: AtomicBool = AtomicBool::new(false);
static ORIG: AtomicUsize = AtomicUsize::new(0);
static READER: Mutex<Option<console::Reader>> = Mutex::new(None);
static POLL_MS: AtomicU64 = AtomicU64::new(0);
static HOOKS: Mutex<Vec<Hook>> = Mutex::new(Vec::new());

fn now_ms() -> u64 {
    unsafe { ffi::GetTickCount64() }
}

/// Called by every detour, from the game's own threads. Contract: cheap, no
/// allocation, no panic (the detour guards the work body).
///
/// Concurrency: `QueryPerformanceCounter` is called from several game threads
/// at once, so the work body is claimed with a CAS on `LAST_MS` and a `BUSY`
/// re-entrancy flag — at most one thread does file IO per interval, and a
/// nested QPC call inside our own work (logger, VirtualQuery) just counts.
pub fn tick() {
    let n = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    let now = now_ms();
    let iv = INTERVAL_MS.load(Ordering::Relaxed).max(1);
    let last = LAST_MS.load(Ordering::Relaxed);
    if now.wrapping_sub(last) < iv {
        return;
    }
    if LAST_MS
        .compare_exchange(last, now, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    if BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    let prev = WORK_MS.swap(now, Ordering::Relaxed);
    // The outer guard matters: a panic unwinding out of the detour would cross
    // the game's `extern "system"` frame. The inner one names the failing stage.
    let _ = log::guard("hook::tick", || work(n, now, now.wrapping_sub(prev)));
    BUSY.store(false, Ordering::Release);
}

fn work(n: u64, now: u64, dt: u64) {
    // periodic heartbeat, at most once per 5 s
    let lg = LOG_MS.load(Ordering::Relaxed);
    if (lg == 0 || now.wrapping_sub(lg) >= 5_000)
        && LOG_MS
            .compare_exchange(lg, now, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
    {
        log::log_tag(
            "hook",
            &format!("tick {n}, dt={dt}ms (interval {iv}ms)", iv = INTERVAL_MS.load(Ordering::Relaxed)),
        );
    }
    let _ = log::guard("hook::work", || {
        let cmds = {
            let mut g = match READER.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if g.is_none() {
                *g = Some(console::Reader::new());
            }
            match g.as_mut() {
                Some(r) => r.poll(),
                None => Vec::new(),
            }
        };
        // At most once per 5 s: what the reader actually sees. In frame-hook mode
        // this is the only way to tell "nothing was appended" from "file IO fails
        // inside a QPC detour".
        let lg2 = POLL_MS.load(Ordering::Relaxed);
        if lg2 == 0 || now.wrapping_sub(lg2) >= 5_000 {
            if POLL_MS
                .compare_exchange(lg2, now, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                log::log_tag(
                    "hook",
                    &format!("poll: {} command(s), {}", cmds.len(), console::cmd_pos()),
                );
            }
        }
        for line in cmds {
            log::log_tag("con", &format!("> {line}"));
            let answered = log::guard("console::dispatch", || console::dispatch(&line));
            let (out, action) = match answered {
                Some(v) => v,
                None => (
                    vec!["command panicked (see PANIC line above)".to_string()],
                    console::Action::None,
                ),
            };
            for l in out {
                log::log_tag("con", &l);
            }
            match action {
                console::Action::Seal | console::Action::Quit => {
                    log::log_tag("con", "seal/quit is a no-op while ticking inside the game thread");
                }
                console::Action::None => {}
            }
        }
    });
}

// ---- detours --------------------------------------------------------------

/// Detour for `kernel32!QueryPerformanceCounter` / `ntdll!RtlQueryPerformanceCounter`.
///
/// The original is called *first*: the game's timing must not change, and a
/// tick that runs before the call could add jitter to the counter it is
/// measuring.
///
/// # Safety
/// `counter` must be a writable `LARGE_INTEGER` (or NULL) — it is whatever the
/// game passed to the real function, and we only ever forward it.
pub unsafe extern "system" fn qpc_detour(counter: *mut i64) -> i32 {
    let orig = ORIG.load(Ordering::Acquire);
    if orig == 0 {
        // The patch is live but the trampoline address is not published yet
        // (a few instructions wide). Keep the counter monotonic instead of
        // handing the game a zero.
        if !counter.is_null() {
            unsafe { *counter = (now_ms() as i64) * 10_000 };
        }
        return 1;
    }
    let f: unsafe extern "system" fn(*mut i64) -> i32 = unsafe { core::mem::transmute(orig) };
    let ret = unsafe { f(counter) };
    tick();
    ret
}

/// Detour for `d3d11!D3D11CreateDevice` — last-resort anchor when neither QPC
/// symbol resolves. Ten arguments, x64 fastcall, so the register file is
/// preserved by a plain Rust `extern "system"` function.
///
/// # Safety
/// The arguments are the game's; they are forwarded to the original unchanged.
#[allow(clippy::too_many_arguments)]
pub unsafe extern "system" fn d3d_detour(
    a1: usize,
    a2: u32,
    a3: usize,
    a4: u32,
    a5: usize,
    a6: u32,
    a7: u32,
    a8: usize,
    a9: usize,
    a10: usize,
) -> i32 {
    let orig = ORIG.load(Ordering::Acquire);
    let ret = if orig != 0 {
        let f: unsafe extern "system" fn(usize, u32, usize, u32, usize, u32, u32, usize, usize, usize) -> i32 =
            unsafe { core::mem::transmute(orig) };
        unsafe { f(a1, a2, a3, a4, a5, a6, a7, a8, a9, a10) }
    } else {
        -1
    };
    tick();
    ret
}

// `hookat` slot: one diagnostic hook at a time (a generic detour cannot tell
// which target it belongs to, and per-hook thunks are not worth the asm yet).
static HOOKAT_ORIG: AtomicUsize = AtomicUsize::new(0);
static HOOKAT_HITS: AtomicU64 = AtomicU64::new(0);
static HOOKAT_LOG: AtomicU64 = AtomicU64::new(0);

/// Four-integer detour used by `hookat`: safe for x64 callers whose arguments
/// are integers (rcx/rdx/r8/r9 and rax are preserved); float arguments
/// (xmm0..3) are clobbered, so this is a *diagnostic* hook, not a wrapper.
unsafe extern "system" fn count_detour(a: u64, b: u64, c: u64, d: u64) -> u64 {
    let orig = HOOKAT_ORIG.load(Ordering::Acquire);
    let ret = if orig != 0 {
        let f: unsafe extern "system" fn(u64, u64, u64, u64) -> u64 =
            unsafe { core::mem::transmute(orig) };
        unsafe { f(a, b, c, d) }
    } else {
        0
    };
    let n = HOOKAT_HITS.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= HOOKAT_LOG.load(Ordering::Relaxed) {
        log::log_tag("hook", &format!("hookat hit {n}, ret {ret:#x}"));
    }
    tick();
    ret
}

// ---- install --------------------------------------------------------------

fn try_resolve(module: &str, func: &str) -> Option<usize> {
    let (_, _) = crate::procs::own_module(module)?;
    let name = ffi::cstr(func);
    let m = unsafe {
        let w = ffi::wide(module);
        ffi::GetModuleHandleW(w.as_ptr())
    };
    if m.is_null() {
        return None;
    }
    let p = unsafe { ffi::GetProcAddress(m, name.as_ptr()) };
    if p.is_null() {
        None
    } else {
        Some(p as usize)
    }
}

fn register(h: Hook) {
    let mut g = match HOOKS.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    g.push(h);
}

/// Install the tick hook. Returns true once the game is calling us.
///
/// Anchor choice, in order: `kernel32!QueryPerformanceCounter` (called
/// thousands of times per second by Unity), `ntdll!RtlQueryPerformanceCounter`
/// (same shape), `d3d11!D3D11CreateDevice` (once at startup — still a tick).
pub fn install_tick() -> bool {
    if INSTALLED.load(Ordering::Acquire) {
        return true;
    }
    if let Some(n) = std::env::var("HROMATITE_TICK_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
        INTERVAL_MS.store(n.max(1), Ordering::Relaxed);
        log::log_tag("hook", &format!("tick interval set to {}ms", n.max(1)));
    }
    START_MS.store(now_ms(), Ordering::Relaxed);
    WORK_MS.store(now_ms(), Ordering::Relaxed);
    if let Ok(mut g) = READER.lock()
        && g.is_none()
    {
        *g = Some(console::Reader::new());
    }
    log::log_tag("hook", &format!("console file {}", console::cmd_path().display()));

    let cands: [(&str, &str, usize); 3] = [
        ("kernel32.dll", "QueryPerformanceCounter", qpc_detour as *const () as usize),
        ("ntdll.dll", "RtlQueryPerformanceCounter", qpc_detour as *const () as usize),
        ("d3d11.dll", "D3D11CreateDevice", d3d_detour as *const () as usize),
    ];
    for (module, func, detour) in cands {
        let addr = match try_resolve(module, func) {
            Some(a) => a,
            None => {
                log::log_tag("hook", &format!("resolve: {module}!{func} not found"));
                continue;
            }
        };
        let p = match prepare(addr) {
            Some(p) => p,
            None => {
                log::log_tag("hook", &format!("install: cannot prepare {module}!{func} at {addr:#x}"));
                continue;
            }
        };
        // publish the trampoline *before* the patch: a thread entering the
        // detour in the gap calls the original through it, not through us
        ORIG.store(p.trampoline, Ordering::Release);
        match patch(&p, detour) {
            Some(h) => {
                log::log_tag(
                    "hook",
                    &format!(
                        "installed {module}!{func} at {addr:#x} -> detour {detour:#x}, trampoline {:#x}, tick every {}ms",
                        p.trampoline,
                        INTERVAL_MS.load(Ordering::Relaxed)
                    ),
                );
                register(h);
                INSTALLED.store(true, Ordering::Release);
                if let Some(at) = std::env::var("HROMATITE_HOOK_AT").ok().and_then(|v| {
                    let t = v.trim().trim_start_matches("0x");
                    usize::from_str_radix(t, 16).ok()
                }) && let Some(o) = install_at(at, 8)
                {
                    log::log_tag("hook", &format!("HROMATITE_HOOK_AT: counting hook at {o:#x}"));
                }
                log::log_tag("hook", "waiting for the game to call us (no thread of our own)");
                return true;
            }
            None => {
                ORIG.store(0, Ordering::Release);
                log::log_tag("hook", &format!("install: patch failed for {addr:#x}"));
            }
        }
    }
    log::log_tag("hook", "install FAILED: no usable tick anchor");
    false
}

/// Install a counting hook at an arbitrary address (`hookat`).
fn install_at(addr: usize, log_first: u64) -> Option<usize> {
    if HOOKAT_ORIG.load(Ordering::Acquire) != 0 {
        return None;
    }
    let p = prepare(addr)?;
    HOOKAT_LOG.store(log_first.max(1), Ordering::Relaxed);
    HOOKAT_HITS.store(0, Ordering::Relaxed);
    HOOKAT_ORIG.store(p.trampoline, Ordering::Release);
    match patch(&p, count_detour as *const () as usize) {
        Some(h) => {
            register(h);
            Some(addr)
        }
        None => {
            HOOKAT_ORIG.store(0, Ordering::Release);
            None
        }
    }
}

/// True once a hook is installed and the game is ticking us.
pub fn installed() -> bool {
    match HOOKS.lock() {
        Ok(g) => !g.is_empty(),
        Err(p) => !p.into_inner().is_empty(),
    }
}

// ---------------------------------------------------------------------------
// Console commands (`hook` is not in console.rs' match — it lands here)
// ---------------------------------------------------------------------------

fn parse_addr(s: &str) -> Option<usize> {
    let t = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    usize::from_str_radix(t, 16).ok()
}

/// Handle `hook …` / `tick` style commands. `None` = "not mine", so
/// `console.rs` reports an unknown command.
pub fn command(line: &str) -> Option<Vec<String>> {
    let mut it = line.splitn(3, char::is_whitespace);
    let cmd = it.next().unwrap_or("").to_ascii_lowercase();
    let a1 = it.next().unwrap_or("");
    let a2 = it.next().unwrap_or("");
    match cmd.as_str() {
        "hook" => {
            let ok = log::guard("hook::install", install_tick).unwrap_or(false);
            Some(vec![format!(
                "frame hook: installed={ok}, ticks={}, interval={}ms",
                TICKS.load(Ordering::Relaxed),
                INTERVAL_MS.load(Ordering::Relaxed)
            )])
        }
        "unhook" => {
            let mut n = 0;
            let mut g = match HOOKS.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            for h in g.iter_mut() {
                if h.unhook() {
                    n += 1;
                }
            }
            Some(vec![format!("{n} hook(s) detached ({} still listed)", g.len())])
        }
        "hooks" => {
            let g = match HOOKS.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if g.is_empty() {
                return Some(vec![
                    "no hooks installed (try `hook` — QPC tick, or `hookat <addr> [n]`)".into(),
                ]);
            }
            let mut out = vec![format!("{} hook(s):", g.len())];
            for (i, h) in g.iter().enumerate() {
                out.push(format!("  [{i}] {}", h.describe()));
            }
            Some(out)
        }
        "hookat" => {
            let addr = match parse_addr(a1) {
                Some(a) => a,
                None => return Some(vec!["usage: hookat <hexaddr> [log-first-n]".into()]),
            };
            let n = a2.trim().parse::<u64>().unwrap_or(8);
            match log::guard("hook::install_at", || install_at(addr, n)).flatten() {
                Some(a) => Some(vec![format!(
                    "hookat: counting hook at {a:#x} (first {n} hits logged, integer args only)"
                )]),
                None => Some(vec![format!(
                    "hookat: {addr:#x} failed (not readable / unknown opcode / one slot busy — see [hook] lines)"
                )]),
            }
        }
        "tick" => {
            let n = TICKS.load(Ordering::Relaxed);
            let start = START_MS.load(Ordering::Relaxed);
            let up = now_ms().saturating_sub(start);
            let rate = if up > 0 { n as f64 * 1000.0 / up as f64 } else { 0.0 };
            Some(vec![format!(
                "tick {n}, uptime {up}ms ({rate:.1}/s), interval {}ms, hooks={}, busy={}",
                INTERVAL_MS.load(Ordering::Relaxed),
                match HOOKS.lock() {
                    Ok(g) => g.len(),
                    Err(p) => p.into_inner().len(),
                },
                BUSY.load(Ordering::Relaxed),
            )])
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn len(b: &[u8]) -> Option<usize> {
        decode_one(b).map(|i| i.len)
    }

    #[test]
    fn decodes_common_prologues() {
        assert_eq!(len(&[0x48, 0x83, 0xEC, 0x28]), Some(4)); // sub rsp, 0x28
        assert_eq!(len(&[0x48, 0x81, 0xEC, 0x28, 0x01, 0, 0]), Some(7)); // sub rsp, 0x128
        assert_eq!(len(&[0xF3, 0x0F, 0x1E, 0xFA]), Some(4)); // endbr64
        assert_eq!(len(&[0x48, 0x89, 0x5C, 0x24, 0x08]), Some(5)); // mov [rsp+8], rbx
        assert_eq!(len(&[0x40, 0x55]), Some(2)); // push rbp
        assert_eq!(len(&[0x48, 0x8B, 0xC4]), Some(3)); // mov rax, rsp
        assert_eq!(len(&[0x48, 0x8B, 0x05, 0, 0, 0, 0]), Some(7)); // mov rax, [rip+0]
        assert_eq!(len(&[0x48, 0x8D, 0x0D, 0, 0, 0, 0]), Some(7)); // lea rcx, [rip+0]
        assert_eq!(len(&[0xE9, 0, 0, 0, 0]), Some(5)); // jmp rel32
        assert_eq!(len(&[0xE8, 0, 0, 0, 0]), Some(5)); // call rel32
        assert_eq!(len(&[0xFF, 0x25, 0, 0, 0, 0]), Some(6)); // jmp [rip+0]
        assert_eq!(len(&[0x0F, 0x1F, 0x44, 0x00, 0x00]), Some(5)); // nop dword [rax+rax]
        assert_eq!(len(&[0x48, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0]), Some(10)); // mov rax, imm64
        assert_eq!(len(&[0xC3]), Some(1));
        assert_eq!(len(&[0x6A, 0x00]), Some(2)); // push 0
        assert_eq!(len(&[0xC5, 0xF8, 0x77]), None); // VEX: unknown on purpose
    }

    /// Real bytes from wine (Proton 10.0, `files/lib/wine/x86_64-windows`),
    /// dumped from the actual export tables. These are the prologues we will be
    /// patching in the game, so they are part of the contract.
    #[test]
    fn decodes_wine_anchors() {
        // ntdll!RtlQueryPerformanceCounter — the real body behind both
        // kernelbase!QueryPerformanceCounter (forwarder) and kernel32!
        let qpc = [
            0x48, 0x8D, 0xA4, 0x24, 0, 0, 0, 0, // lea rsp, [rsp]
            0x55, // push rbp
            0x48, 0x89, 0xE5, // mov rbp, rsp
            0x48, 0x83, 0xEC, 0x20, // sub rsp, 0x20
            0x31, 0xD2, // xor edx, edx
        ];
        assert_eq!(prologue_len(&qpc, 5), Some(8)); // one 8-byte lea is enough
        // d3d11!D3D11CreateDevice — a hotpatch trampoline (jmp rel32); the copy
        // must be relocated, so the rel field has to be reported
        let d3d = [0xE9u8, 0x2B, 0xF0, 0x00, 0x00, 0x90, 0x90];
        assert_eq!(prologue_len(&d3d, 5), Some(5));
        assert_eq!(decode_one(&d3d).unwrap().rel, Some((1, 32)));
        // and the first wine prologue carries no relative operand at all
        let first = decode_one(&qpc).unwrap();
        assert_eq!(first.len, 8);
        assert_eq!(first.rel, None);
        assert_eq!(first.rip, None);
    }

    #[test]
    fn prologue_covers_five_bytes() {
        // push rbp; sub rsp, 0x28 -> 5 bytes only after both
        let b = [0x40, 0x55, 0x48, 0x83, 0xEC, 0x28];
        assert_eq!(prologue_len(&b, 5), Some(6));
        // rip-relative first instruction: rip field must be recorded
        let ins = decode_one(&[0x48, 0x8B, 0x05, 1, 0, 0, 0]).unwrap();
        assert_eq!(ins.rip, Some(3));
    }
}
