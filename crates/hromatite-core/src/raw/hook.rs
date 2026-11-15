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
//! Plan 4 (`HROMATITE_SEAL=1` / `HROMATITE_L2B=1`) removes the last limitation of
//! this file: in `--seal` the injector XORs the image in place as soon as
//! `DllMain` returns, so a detour *inside* the image would turn to garbage. The
//! tick is therefore entered through a hand-written shim page allocated outside
//! the image, which decrypts the image, calls the Rust detour, and encrypts it
//! again — see the "L2b" section below. Without those variables nothing changes:
//! `qpc_detour` is installed directly.

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
    let mut saw_rex = false;
    let mut opsize_66 = false;
    // legacy prefixes, then at most one REX (which must come last)
    let mut guard = 0;
    loop {
        guard += 1;
        if guard > 9 {
            return None;
        }
        let c = *b.get(i)?;
        match c {
            0x66 => {
                if saw_rex {
                    return None;
                }
                opsize_66 = true;
                i += 1;
            }
            0x67 | 0xF0 | 0xF2 | 0xF3 | 0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65 => {
                // Reject legacy prefixes only when they follow REX. All other
                // bytes continue into instruction decoding, including opcodes.
                if saw_rex {
                    return None;
                }
                i += 1;
            }
            0x40..=0x4F => {
                rex_w = (c & 0x08) != 0;
                saw_rex = true;
                i += 1;
            }
            _ => break,
        }
    }
    let op = *b.get(i)?;
    let imm16 = opsize_66 && !rex_w;
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
                | 0xD6 // movq
                | 0xEF // pxor
                | 0x1F => { // multi-byte nop
                    let m = modrm_len(b, i + 2)?;
                    if let Some(o) = m.rip {
                        rip = Some(i + 2 + o);
                    }
                    total = i + 2 + m.bytes;
                }
                0xC6 => { // shuffle, with an immediate byte after ModRM
                    let m = modrm_len(b, i + 2)?;
                    if let Some(o) = m.rip {
                        rip = Some(i + 2 + o);
                    }
                    total = i + 2 + m.bytes + 1;
                }
                _ => return None,
            }
        }
        // push/pop r64
        0x50..=0x5F => total = i + 1,
        0x90 | 0x98 | 0x99 | 0x9C | 0x9D | 0xC3 | 0xC9 | 0xCC | 0xF4 => total = i + 1,
        0xC2 => total = i + 3, // ret imm16
        0x6A => total = i + 2, // push imm8
        0x68 => total = i + if imm16 { 3 } else { 5 }, // push imm16/imm32
        0xA8 => total = i + 2,
        0xA9 => total = i + if imm16 { 3 } else { 5 },
        0xB0..=0xB7 => total = i + 2, // mov r8, imm8
        0xB8..=0xBF => {
            total = i + if rex_w { 9 } else if opsize_66 { 3 } else { 5 };
        } // REX.W: imm64; 0x66: imm16 (unless REX.W wins); default: imm32
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
            if let Some(o) = m.rip {
                rip = Some(i + 1 + o);
            }
            total = i + 1 + m.bytes + 1;
        }
        0x81 => {
            let m = modrm_len(b, i + 1)?;
            if let Some(o) = m.rip {
                rip = Some(i + 1 + o);
            }
            total = i + 1 + m.bytes + if imm16 { 2 } else { 4 };
        }
        0x83 => {
            let m = modrm_len(b, i + 1)?;
            if let Some(o) = m.rip {
                rip = Some(i + 1 + o);
            }
            total = i + 1 + m.bytes + 1;
        }
        0xC0 | 0xC1 => {
            let m = modrm_len(b, i + 1)?;
            if let Some(o) = m.rip {
                rip = Some(i + 1 + o);
            }
            total = i + 1 + m.bytes + 1;
        }
        0xC6 => {
            let m = modrm_len(b, i + 1)?;
            if let Some(o) = m.rip {
                rip = Some(i + 1 + o);
            }
            total = i + 1 + m.bytes + 1;
        }
        0xC7 => {
            let m = modrm_len(b, i + 1)?;
            if let Some(o) = m.rip {
                rip = Some(i + 1 + o);
            }
            total = i + 1 + m.bytes + if imm16 { 2 } else { 4 };
        }
        // group 3: only `test` carries an immediate
        0xF6 | 0xF7 => {
            let m = modrm_len(b, i + 1)?;
            if let Some(o) = m.rip {
                rip = Some(i + 1 + o);
            }
            let reg = (b[i + 1] >> 3) & 7;
            let imm = if reg <= 1 {
                if op == 0xF6 { 1 } else if imm16 { 2 } else { 4 }
            } else {
                0
            };
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

fn adjusted_disp32(original: i32, delta: i64) -> Option<i32> {
    // delta = trampoline - old_address. The relocated instruction's next RIP
    // moved by +delta, so its displacement must move by -delta, not +delta.
    (original as i64 - delta).try_into().ok()
}

fn write_mode(offset_in_16: usize) -> Option<u8> {
    match offset_in_16 {
        0..=3 => Some(8),
        4..=11 => Some(16),
        _ => None, // the two-byte non-atomic fallback is forbidden
    }
}

fn branch_into_prologue(target: usize, prologue: usize, off: usize, len: usize, disp: i64) -> bool {
    let absolute = target as i64 + (off + len) as i64 + disp;
    absolute >= target as i64 && absolute < target as i64 + prologue as i64
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
        // SAFETY: mbi is a correctly sized live output buffer; `addr` is an
        // opaque address for the synchronous query and is not dereferenced.
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
            // SAFETY: requested address/size/protection are valid for allocation;
            // no pointers to Rust data cross the FFI boundary.
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
        // SAFETY: `c` was computed from a free region large enough for `size`.
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
            // SAFETY: base16 is readable target code and wmode 8 proves the full
            // aligned qword is inside the original 32-byte snapshot window.
            unsafe { core::ptr::write_volatile(base as *mut u64, u64::from_le_bytes(w)) };
            true
        }
        16 => {
            // SAFETY: base16 is 16-byte aligned and the complete 16-byte block
            // lies within the validated target-code snapshot.
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
                // SAFETY: base16 points to a 16-byte writable aligned block;
                // expected and desired register pairs match cmpxchg16b and rbx is preserved.
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
                // SAFETY: unchanged from the initial read: base16 is a complete,
                // aligned, readable 16-byte target-code block.
                let fresh = unsafe { core::ptr::read_volatile(base16 as *const [u8; 16]) };
                for i in 0..2 {
                    let mut w = [0u8; 8];
                    w.copy_from_slice(&fresh[i * 8..i * 8 + 8]);
                    oldv[i] = u64::from_le_bytes(w);
                }
            }
            false
        }
        // wmode 0 was the old non-atomic, two-block fallback. It is never
        // selected any more: write_mode refuses those alignments up front.
        _ => false,
    }
}

fn set_range_protect(addr: usize, len: usize, prot: u32) -> Option<u32> {
    let page = addr & !(PAGE_SIZE - 1);
    let end = (addr + len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let mut old = 0u32;
    // SAFETY: `old` is a valid output pointer and page..end is computed within
    // the caller's already allocated target range.
    let ok = unsafe { ffi::VirtualProtect(page as *mut u8, end - page, prot, &mut old) };
    (ok != 0).then_some(old)
}

fn flush(addr: usize, len: usize) {
    // SAFETY: the pseudo-process handle is always valid; addr/len describe the
    // generated code range supplied by the caller.
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
            let displacement = if bits == 32 {
                i32::from_le_bytes(chunk[ro..ro + 4].try_into().ok()?) as i64
            } else {
                chunk[ro] as i8 as i64
            };
            if branch_into_prologue(target, prologue, off, ins.len, displacement) {
                log::log_tag(
                    "hook",
                    &format!("prepare: relative branch at {target:#x}+{off} targets the overwritten prologue, refusing"),
                );
                return None;
            }
            if bits == 32 {
                let Some(v) = adjusted_disp32(
                    i32::from_le_bytes(chunk[ro..ro + 4].try_into().ok()?),
                    delta,
                ) else {
                    log::log_tag("hook", &format!("prepare: rel32 at {target:#x}+{off} out of range"));
                    return None;
                };
                chunk[ro..ro + 4].copy_from_slice(&v.to_le_bytes());
            } else {
                let v = chunk[ro] as i8 as i64 - delta;
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
            let Some(v) = adjusted_disp32(
                i32::from_le_bytes(chunk[ro..ro + 4].try_into().ok()?),
                delta,
            ) else {
                log::log_tag("hook", &format!("prepare: rip disp at {target:#x}+{off} out of range"));
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
        log::log_tag("hook", &format!("prepare: trampoline code {} bytes, relay slot busy", code.len()));
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
    // SAFETY: relay points into our allocated page at RELAY_OFF; relay+6..+14
    // is the qword slot reserved by the 14-byte relay encoding.
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
            &format!("patch: {:#x} straddles a 16-byte boundary; non-atomic fallback refused", p.target),
        );
        return None;
    }
    let wmode = write_mode(o16)?;
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
    // SAFETY: new32 contains the full original target window plus the decoded
    // five-byte jump; target was made writable and wmode was derived from its alignment.
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
        if self.wmode == 0 {
            log::log_tag("hook", "unhook: refusing a non-atomic 16-byte-straddling restore");
            return false;
        }
        // the whole 16-byte window is rewritten, so the original snapshot is
        // the payload — no read-back needed
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
// L2b: the image lives encrypted; a stub page holds the only plaintext code
// ---------------------------------------------------------------------------
//
// Plan 4. With `--seal` the injector XORs the whole mapping the moment
// `DllMain` returns, so everything of ours — the detour included — turns to
// ciphertext and the next tick executes noise. Plan 4 fixes that by moving the
// only code that ever runs *while the image is sealed* into a page of its own,
// allocated with `VirtualAlloc` (never part of the image, never sealed):
//
//     QPC prologue ──jmp rel32──► relay ──jmp abs──► shim (this page)
//                                                    ├ decrypt [base,base+size)
//                                                    ├ VirtualProtect → RX
//                                                    ├ call <Rust detour>   ← image is plaintext
//                                                    ├ VirtualProtect → RW
//                                                    ├ encrypt back
//                                                    └ jmp <trampoline>     ← the real QPC
//
// The shim therefore touches *no* memory of the image except through the Rust
// function it calls: every address it needs (image range, the data page, the
// detour, the trampoline, `VirtualProtect`, `FlushInstructionCache`,
// `GetTickCount64`) is an immediate patched in at install time. Statics, the
// mutexes, the console reader — all of that lives in the image and is only ever
// read *inside* the decrypted window, which is exactly what the design allows.
//
// Two invariants make the window safe:
//
//   * one window at a time. The data page (also outside the image) holds an
//     owner thread id; the shim opens a window with `lock cmpxchg` and the
//     nested case (a QPC call from inside our own detour) skips the whole dance
//     instead of decrypting twice — XOR is an involution, a second pass would
//     re-encrypt the image under the first caller's feet;
//   * never touch the image before the injector sealed it. `SEAL_PROBE` is a
//     magic word inside the image: if it still reads back as the magic, the
//     image is plaintext and the shim only forwards to the trampoline (and the
//     injector's seal is still in flight — the first tick is 2 s away anyway).
//
// Exception dispatch: the vectored handler installed at `DllMain` is image code
// too, so a single exception in the game while sealed would execute garbage.
// A second shim is registered as a *vectored handler* (registered later with
// `first = 1`, so ntdll calls it before ours): it decrypts, marks the window
// "leaked" (the plaintext must stay readable for the handlers that follow) and
// returns; the next tick closes the window — i.e. re-encrypts — on its way out.

/// Magic the probe holds while the image is in plaintext (never equal to its
/// ciphertext but with probability 2^-64).
const SEAL_MAGIC: u64 = 0x4C32_425F_5348_494D; // "L2B_SHIM"

/// Window bookkeeping, in a page of its own so that the shim can read it while
/// every byte of the image is ciphertext. Layout is fixed: the asm hard-codes
/// the offsets, `SealData` mirrors them.
#[repr(C)]
#[derive(Default)]
pub struct SealData {
    /// 0x00 owner thread id, 0 = window closed
    pub lock: u64,
    /// 0x08 absolute `GetTickCount64` deadline (written by the Rust detour)
    pub due: u64,
    /// 0x10 how many times the shim was entered (kept for the `tick` command)
    pub hits: u64,
    /// 0x18 the image is plaintext because an exception shim opened the window
    pub leaked: u64,
    /// 0x20 scratch for `VirtualProtect`'s old protection
    pub old_prot: u64,
    /// 0x28 number of ticks that ran inside a window (diagnostics)
    pub windows: u64,
    /// 0x30 this call opened a window, i.e. the image must be encrypted on the
    /// way out (0 when we merely ran the detour over an unsealed image)
    pub crypted: u64,
}

// Referenced by the asm byte offsets; `D_LOCK`/`D_WINDOWS` are only spelled out
// here (and asserted against `SealData` in the tests) so the layout is in one
// place.
#[allow(dead_code)]
const D_LOCK: usize = 0x00;
const D_DUE: usize = 0x08;
const D_HITS: usize = 0x10;
const D_LEAKED: usize = 0x18;
const D_OLD: usize = 0x20;
#[allow(dead_code)]
const D_WINDOWS: usize = 0x28;
const D_CRYPTED: usize = 0x30;

/// How long after install the first tick may run: the injector seals only after
/// `DllMain` returns, and we must not decrypt a half-sealed image.
const SEAL_START_DELAY_MS: u64 = 2_000;

/// The probe lives in the image (that is the point) and is compared by the asm.
#[allow(dead_code)]
static SEAL_PROBE: AtomicU64 = AtomicU64::new(SEAL_MAGIC);
/// Data page address, published once at install (0 = L2b not active).
static SEAL_DATA: AtomicUsize = AtomicUsize::new(0);
/// Entering the shim page.
static SEAL_ENTRY: AtomicUsize = AtomicUsize::new(0);
static SEAL_VEH: AtomicUsize = AtomicUsize::new(0);

pub fn seal_active() -> bool {
    SEAL_DATA.load(Ordering::Acquire) != 0
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// `[base, size)` of our own mapping. A manual map is one `VirtualAllocEx`, so
/// `VirtualQuery` on any address inside the image answers with the allocation
/// base and its size — the same numbers the injector sealed. `HROMATITE_IMAGE`
/// (`base:size`, hex) overrides it for tests.
fn own_image_range() -> Option<(usize, usize)> {
    // The injector's own numbers win: it sealed exactly this range.
    let (hb, hs) = (
        IMAGE_BASE.load(Ordering::Acquire),
        IMAGE_SIZE.load(Ordering::Acquire),
    );
    if hb != 0 && hs >= PAGE_SIZE {
        return Some((hb, hs));
    }
    if let Ok(v) = std::env::var("HROMATITE_IMAGE") {
        let (b, s) = v.split_once(':')?;
        let parse = |t: &str| usize::from_str_radix(t.trim().trim_start_matches("0x"), 16).ok();
        if let (Some(b), Some(s)) = (parse(b), parse(s))
            && b >= 0x10000
            && s >= PAGE_SIZE
        {
            log::log_tag("hook", &format!("l2b: image range from HROMATITE_IMAGE {b:#x}+{s:#x}"));
            return Some((b, s));
        }
        log::log_tag("hook", &format!("l2b: HROMATITE_IMAGE={v:?} unparsable, falling back to VirtualQuery"));
    }
    let mut mbi = ffi::MEMORY_BASIC_INFORMATION::default();
    // SAFETY: mbi is a valid output buffer and SEAL_PROBE has static lifetime;
    // its address only identifies our own allocation for VirtualQuery.
    let n = unsafe {
        ffi::VirtualQuery(
            &SEAL_PROBE as *const _ as *const core::ffi::c_void,
            &mut mbi,
            core::mem::size_of::<ffi::MEMORY_BASIC_INFORMATION>(),
        )
    };
    if n == 0 || mbi.AllocationBase == 0 || mbi.RegionSize < PAGE_SIZE {
        return None;
    }
    Some((mbi.AllocationBase, mbi.RegionSize))
}

// ---- the assembler ---------------------------------------------------------

/// Byte emitter with labels, so the shim can be built (and tested) on the host.
/// Deliberately tiny: only the handful of x64 forms the shim needs.
struct Asm {
    c: Vec<u8>,
    /// (offset of the rel32 field, label)
    jumps: Vec<(usize, u8)>,
    labels: Vec<Option<usize>>,
}

#[allow(dead_code)]
impl Asm {
    fn new() -> Asm {
        Asm { c: Vec::with_capacity(1024), jumps: Vec::new(), labels: Vec::new() }
    }
    fn b(&mut self, x: u8) {
        self.c.push(x);
    }
    fn bs(&mut self, s: &[u8]) {
        self.c.extend_from_slice(s);
    }
    fn len(&self) -> usize {
        self.c.len()
    }
    fn label(&mut self) -> u8 {
        self.labels.push(None);
        (self.labels.len() - 1) as u8
    }
    fn bind(&mut self, l: u8) {
        self.labels[l as usize] = Some(self.c.len());
    }
    /// conditional jump, rel32 (`0F 8x`)
    fn jcc(&mut self, cond: u8, l: u8) {
        self.bs(&[0x0F, 0x80 | cond]);
        self.jumps.push((self.c.len(), l));
        self.bs(&[0, 0, 0, 0]);
    }
    fn jmp(&mut self, l: u8) {
        self.b(0xE9);
        self.jumps.push((self.c.len(), l));
        self.bs(&[0, 0, 0, 0]);
    }
    /// `mov r64, imm64` (`REX.W B8+rd`)
    fn mov_imm(&mut self, reg: u8, v: u64) {
        self.b(if reg >= 8 { 0x49 } else { 0x48 });
        self.b(0xB8 | (reg & 7));
        self.bs(&v.to_le_bytes());
    }
    /// `mov r64, r64` (`REX.W 89 /r`)
    fn mov_rr(&mut self, dst: u8, src: u8) {
        let rex = 0x48 | if src >= 8 { 0x04 } else { 0 } | if dst >= 8 { 0x01 } else { 0 };
        self.b(rex);
        self.b(0x89);
        self.b(0xC0 | ((src & 7) << 3) | (dst & 7));
    }
    fn push(&mut self, reg: u8) {
        if reg >= 8 {
            self.b(0x41);
        }
        self.b(0x50 | (reg & 7));
    }
    fn pop(&mut self, reg: u8) {
        if reg >= 8 {
            self.b(0x41);
        }
        self.b(0x58 | (reg & 7));
    }
    /// `call rax`
    fn call_rax(&mut self) {
        self.bs(&[0xFF, 0xD0]);
    }
    /// `cmp rax, [rdi+disp8]`
    fn cmp_rax_mem_rdi(&mut self, disp: u8) {
        self.bs(&[0x48, 0x3B, 0x47, disp]);
    }
    /// `cmp qword [rdi+disp8], imm8`
    fn cmp_mem_rdi_imm8(&mut self, disp: u8, v: u8) {
        self.bs(&[0x48, 0x83, 0x7F, disp, v]);
    }
    /// `mov qword [rdi+disp8], imm8`
    fn mov_mem_rdi_imm8(&mut self, disp: u8, v: u8) {
        self.bs(&[0x48, 0xC7, 0x47, disp, v, 0, 0, 0]);
    }
    /// `mov qword [rdi], imm8`
    fn mov_mem_rdi_imm8_0(&mut self, v: u8) {
        self.bs(&[0x48, 0xC7, 0x07, v, 0, 0, 0]);
    }
    /// `add qword [rdi+disp8], imm8`
    fn add_mem_rdi_imm8(&mut self, disp: u8, v: u8) {
        self.bs(&[0x48, 0x83, 0x47, disp, v]);
    }
    /// `mov r10, gs:[0x48]` — the current thread id (TEB.ClientId.UniqueThread)
    fn mov_r10_tid(&mut self) {
        self.bs(&[0x65, 0x4C, 0x8B, 0x14, 0x25, 0x48, 0x00, 0x00, 0x00]);
    }
    /// `mov rax, <fn>` + `call rax`
    fn call_imm(&mut self, f: u64) {
        self.mov_imm(0, f);
        self.call_rax();
    }
    /// `mov rcx, imm64` / `mov rdx, imm64` / `mov r8d, imm32` / `mov r9, imm64`
    fn set_args(&mut self, a: u64, b: u64, c: u32, d: u64) {
        self.mov_imm(1, a);
        self.mov_imm(2, b);
        self.b(0x41);
        self.b(0xB8);
        self.bs(&c.to_le_bytes());
        self.mov_imm(9, d);
    }
    /// `lock cmpxchg [rdi], r10` with zeroed rax (expect 0); ZF = acquired
    fn try_lock(&mut self) {
        self.bs(&[0x31, 0xC0]); // xor eax, eax
        self.bs(&[0xF0, 0x4C, 0x0F, 0xB1, 0x17]); // lock cmpxchg [rdi], r10
    }
    /// `cmp qword [rdi], r10`
    fn cmp_mem_rdi_r10(&mut self) {
        self.bs(&[0x4C, 0x3B, 0x17]);
    }
    /// `jmp qword [rip+0]` followed by the address (14 bytes, touches no register)
    fn jmp_abs(&mut self, to: u64) {
        self.bs(&[0xFF, 0x25, 0, 0, 0, 0]);
        self.bs(&to.to_le_bytes());
    }
    fn finish(mut self) -> Vec<u8> {
        for (at, l) in core::mem::take(&mut self.jumps) {
            let to = self.labels[l as usize].expect("unbound label") as i64;
            let rel = to - (at as i64 + 4);
            let rel = rel as i32;
            self.c[at..at + 4].copy_from_slice(&rel.to_le_bytes());
        }
        self.c
    }
}

/// Push the ten registers the shim borrows, then realign the stack to 16 so the
/// `call`s below see the ABI's `rsp % 16 == 8` inside the callee.
///
/// Entry state: `rsp % 16 == 8` (a `call`/the dispatcher pushed the return
/// address). Ten pushes = 80 bytes ≡ 0 (mod 16) → still 8; the extra `sub rsp, 8`
/// brings it to 0, and a `call` from there hands the callee 8 again.
const PUSHED: [u8; 10] = [0, 1, 2, 3, 6, 7, 8, 9, 10, 11]; // rax rcx rdx rbx rsi rdi r8 r9 r10 r11

fn prologue(a: &mut Asm) {
    for r in PUSHED {
        a.push(r);
    }
    a.bs(&[0x48, 0x83, 0xEC, 0x08]); // sub rsp, 8
}

fn epilogue(a: &mut Asm) {
    a.bs(&[0x48, 0x83, 0xC4, 0x08]); // add rsp, 8
    for r in PUSHED.iter().rev() {
        a.pop(*r);
    }
}

/// XOR `[base, base+size)` with `crypto::xor_page`'s stream, in place.
///
/// Registers: `rsi` cursor, `rbx` bytes left, `rdi` page index, `rax` xorshift
/// state, `rdx` the 8-byte key, `rcx` loop counter, `r8` scratch. Nothing here
/// is callee-saved except what the prologue already pushed, so the block can be
/// dropped between two `call`s.
fn crypt(a: &mut Asm, base: u64, size: u64) {
    let (page, step, blk, tail, tail_loop, page_done, done) =
        (a.label(), a.label(), a.label(), a.label(), a.label(), a.label(), a.label());

    a.mov_imm(6, base); // mov rsi, base
    a.mov_imm(3, size); // mov rbx, size
    a.bs(&[0x31, 0xFF]); // xor edi, edi      — page index

    a.bind(page);
    // rax = SEAL_KEY ^ page * GOLDEN
    a.mov_rr(0, 7); // mov rax, rdi
    a.mov_imm(1, crate::crypto::GOLDEN);
    a.bs(&[0x48, 0x0F, 0xAF, 0xC1]); // imul rax, rcx
    a.mov_imm(1, crate::crypto::SEAL_KEY);
    a.bs(&[0x48, 0x31, 0xC8]); // xor rax, rcx
    // r9 = min(rbx, 4096): one page, the last one may be short
    a.bs(&[0x49, 0xC7, 0xC1, 0x00, 0x10, 0x00, 0x00]); // mov r9, 4096
    a.bs(&[0x4C, 0x39, 0xCB]); // cmp rbx, r9
    a.bs(&[0x4C, 0x0F, 0x42, 0xCB]); // cmovb r9, rbx

    // eight xorshift rounds, low byte of each state → the repeating 8-byte key
    a.bs(&[0x31, 0xD2]); // xor edx, edx      — key accumulator
    a.bs(&[0xB9, 0x08, 0x00, 0x00, 0x00]); // mov ecx, 8
    a.bind(step);
    a.mov_rr(8, 0); // mov r8, rax
    a.bs(&[0x49, 0xC1, 0xE0, 0x0D]); // shl r8, 13
    a.bs(&[0x4C, 0x31, 0xC0]); // xor rax, r8
    a.mov_rr(8, 0);
    a.bs(&[0x49, 0xC1, 0xE8, 0x07]); // shr r8, 7
    a.bs(&[0x4C, 0x31, 0xC0]); // xor rax, r8
    a.mov_rr(8, 0);
    a.bs(&[0x49, 0xC1, 0xE0, 0x11]); // shl r8, 17
    a.bs(&[0x4C, 0x31, 0xC0]); // xor rax, r8
    a.bs(&[0x44, 0x0F, 0xB6, 0xC0]); // movzx r8d, al
    a.bs(&[0x48, 0xC1, 0xE2, 0x08]); // shl rdx, 8
    a.bs(&[0x4C, 0x09, 0xC2]); // or rdx, r8
    a.bs(&[0xFF, 0xC9]); // dec ecx
    a.jcc(5, step); // jnz step
    a.bs(&[0x48, 0x0F, 0xCA]); // bswap rdx   (big-endian accumulation → LE word)

    a.bind(blk);
    a.bs(&[0x49, 0x83, 0xF9, 0x08]); // cmp r9, 8   — bytes left in *this* page
    a.jcc(2, tail); // jb tail
    a.bs(&[0x48, 0x31, 0x16]); // xor [rsi], rdx
    a.bs(&[0x48, 0x83, 0xC6, 0x08]); // add rsi, 8
    a.bs(&[0x49, 0x83, 0xE9, 0x08]); // sub r9, 8
    a.bs(&[0x48, 0x83, 0xEB, 0x08]); // sub rbx, 8
    a.jmp(blk);

    a.bind(tail); // 0..7 trailing bytes of a short last page
    a.bs(&[0x4D, 0x85, 0xC9]); // test r9, r9
    a.jcc(4, page_done); // jz
    a.mov_rr(0, 2); // mov rax, rdx
    a.bind(tail_loop);
    a.bs(&[0x41, 0x88, 0xC0]); // mov r8b, al
    a.bs(&[0x44, 0x30, 0x06]); // xor [rsi], r8b
    a.bs(&[0x48, 0xC1, 0xE8, 0x08]); // shr rax, 8
    a.bs(&[0x48, 0xFF, 0xC6]); // inc rsi
    a.bs(&[0x48, 0xFF, 0xCB]); // dec rbx
    a.bs(&[0x49, 0xFF, 0xC9]); // dec r9   (flags from here: loop while r9 != 0)
    a.jcc(5, tail_loop); // jnz
    a.bind(page_done);
    a.bs(&[0x48, 0x85, 0xDB]); // test rbx, rbx
    a.jcc(4, done); // jz done
    a.bs(&[0x48, 0xFF, 0xC7]); // inc rdi
    a.jmp(page);
    a.bind(done);
}


/// Stage breadcrumb: `mov qword [rdi+D_OLD], imm8`. `old_prot` is only used
/// inside `protect_call`, so between calls it is a free slot that the host can
/// read from `/proc/<pid>/mem` — the shim has no logger of its own.
fn stage(a: &mut Asm, n: u8) {
    // +0x38 is past `SealData` in a full page — `old_prot` (+0x20) is written by
    // every `VirtualProtect`, so it cannot hold a breadcrumb.
    // `mov qword [rdi+0x38], imm32` — the immediate is 4 bytes, and emitting
    // only five bytes here desynchronised every following instruction (the CPU
    // ate three bytes of the next opcode, so the shim faulted at +0x2A).
    a.bs(&[0x48, 0xC7, 0x47, 0x38, n, 0, 0, 0]);
}

/// Stage logger for the shim. The shim has no logger of its own, but the image is
/// plaintext while it runs, so a plain Rust call is legal. Printed once per stage
/// (a static array dedupes), which makes the log itself the breadcrumb trail.
static mut NOTE_SEEN: [bool; 16] = [false; 16];

/// # Safety
/// Called from generated Win64 shim code with the process image temporarily
/// decrypted. The logger and its runtime state must be readable and executable.
pub unsafe extern "system" fn l2b_note(stage: u64) {
    let i = (stage as usize) & 0xF;
    // SAFETY: i is masked to 0..15 and this callback is entered serially by the
    // shim while holding the L2b window, so adjacent bools cannot race.
    unsafe {
        if NOTE_SEEN[i] {
            return;
        }
        NOTE_SEEN[i] = true;
    }
    log::log_tag("l2b", &format!("stage {stage}"));
}

/// A note call is emitted inline where needed so the emitted code cannot drift
/// from the register/data-page setup at that call site.
/// `VirtualProtect(base, size, prot, &old_prot)`
fn protect_call(a: &mut Asm, p: &ShimParams, prot: u32) {
    // Only `.text` changes protection: making `.data` RX kills the Rust detour
    // (it writes its own statics), and leaving `.text` RW is not executable under
    // NX. Everything else stays as the seal left it (RW).
    let (lo, hi) = if p.text_hi > p.text_lo && p.text_lo >= p.base {
        (p.text_lo, p.text_hi - p.text_lo)
    } else {
        (p.base, p.size)
    };
    a.set_args(lo, hi, prot, p.data + D_OLD as u64);
    a.call_imm(p.vprotect);
}

/// Everything the two shims need to know, baked in as immediates.
#[derive(Clone, Copy)]
pub struct ShimParams {
    pub base: u64,
    pub size: u64,
    pub data: u64,
    pub probe: u64,
    pub trampoline: u64,
    pub detour: u64,
    pub vprotect: u64,
    pub flush: u64,
    pub gettick: u64,
    pub note: u64,
    /// The payload's `.text` (executable) range: only these pages may be `RX`,
    /// `.data` must stay `RW` or the Rust detour faults on its own statics.
    pub text_lo: u64,
    pub text_hi: u64,
}

/// The tick shim, entered by every `QueryPerformanceCounter` call.
///
/// Fast path (the common case, ~350 k/s): decrypt state untouched, only a
/// `GetTickCount64` and a compare against the deadline, then jump to the
/// trampoline. The expensive path runs at most once per `HROMATITE_TICK_MS`.
fn build_tick(p: &ShimParams) -> Vec<u8> {
    let mut a = Asm::new();
    let (sealed, plain, leaked, call, release, done) =
        (a.label(), a.label(), a.label(), a.label(), a.label(), a.label());
    prologue(&mut a);
    a.mov_imm(7, p.data); // mov rdi, data page
    stage(&mut a, 1); // shim entered
    a.mov_imm(10, p.probe); // mov r10, probe
    a.mov_imm(0, SEAL_MAGIC);
    a.bs(&[0x49, 0x3B, 0x02]); // cmp [r10], rax   — plaintext or ciphertext?
    a.jcc(5, sealed); // jnz sealed — the probe no longer reads back as the magic
    a.jmp(plain);

    // ---- sealed: the normal path -------------------------------------------
    a.bind(sealed);
    stage(&mut a, 3);
    a.call_imm(p.gettick);
    stage(&mut a, 4);
    a.cmp_rax_mem_rdi(D_DUE as u8);
    a.jcc(2, done); // jb done — not due
    a.mov_r10_tid();
    a.cmp_mem_rdi_r10();
    a.jcc(4, done); // je done — nested call from inside our own detour
    a.try_lock();
    a.jcc(5, done); // jnz — another thread owns the window
    stage(&mut a, 5);
    a.add_mem_rdi_imm8(D_HITS as u8, 1);
    a.mov_mem_rdi_imm8(D_CRYPTED as u8, 1);
    stage(&mut a, 5);
    // Order matters: the pages are RW (that is what the seal left behind), so
    // decrypt first and only then make them executable — a store into RX faults.
    crypt(&mut a, p.base, p.size);
    a.mov_imm(7, p.data); // crypt uses rdi as its page counter — restore the data page
    stage(&mut a, 6); // decrypt done
    // (no logging inside the shim: log_tag -> QPC -> shim -> recursion)
    protect_call(&mut a, p, ffi::PAGE_EXECUTE_READ);
    stage(&mut a, 7); // RX
    // No FlushInstructionCache here: measured live (`stage=7` stuck, window held
    // forever) the call blocks inside the shim. x86 has a coherent I-cache and
    // `VirtualProtect` is a serialising round-trip through the kernel anyway, so
    // the freshly decrypted bytes are already visible to the instruction fetcher.
    stage(&mut a, 8); // pages executable
    a.jmp(call);

    // ---- plaintext ---------------------------------------------------------
    a.bind(plain);
    a.cmp_mem_rdi_imm8(D_LEAKED as u8, 0);
    a.jcc(5, leaked); // jnz leaked
    // Not sealed (yet): run the detour straight away, nothing to encrypt. The
    // deadline gate still applies, so a tick never races the injector's seal.
    a.call_imm(p.gettick);
    a.cmp_rax_mem_rdi(D_DUE as u8);
    a.jcc(2, done); // jb done
    a.mov_r10_tid();
    a.cmp_mem_rdi_r10();
    a.jcc(4, done);
    a.try_lock();
    a.jcc(5, done);
    a.mov_mem_rdi_imm8(D_CRYPTED as u8, 0);
    a.jmp(call);

    // The exception shim decrypted the image and could not encrypt it again
    // (the handlers after it still need readable code): close it here.
    a.bind(leaked);
    a.mov_r10_tid();
    a.try_lock();
    a.jcc(5, done);
    a.mov_mem_rdi_imm8(D_CRYPTED as u8, 1);

    a.bind(call);
    // `crypt` uses rdi as its page counter (`xor edi, edi`), so it must be
    // reloaded here: passing the counter instead of the data page makes the Rust
    // detour write into 0x122 and fault on the game's thread (measured: no ticks,
    // image left decrypted).
    a.mov_imm(7, p.data);
    a.mov_rr(1, 7); // mov rcx, rdi — the detour takes the data page
    a.call_imm(p.detour);
    stage(&mut a, 9); // detour returned
    a.cmp_mem_rdi_imm8(D_CRYPTED as u8, 0);
    a.jcc(4, release); // je release — no window was opened
    protect_call(&mut a, p, ffi::PAGE_READWRITE);
    crypt(&mut a, p.base, p.size);
    a.mov_imm(7, p.data); // crypt clobbered rdi
    stage(&mut a, 4);
    a.mov_mem_rdi_imm8(D_LEAKED as u8, 0);
    a.mov_mem_rdi_imm8(D_CRYPTED as u8, 0);
    a.bind(release);
    stage(&mut a, 10); // about to release the window
    a.mov_mem_rdi_imm8_0(0); // release the window
    a.bind(done);
    epilogue(&mut a);
    a.jmp_abs(p.trampoline);
    a.finish()
}

/// The vectored-exception shim: decrypt, mark the window leaked, return
/// `EXCEPTION_CONTINUE_SEARCH`. The handlers that run after it (`veh`'s own,
/// the game's) then find readable code; the next tick re-encrypts.
fn build_veh(p: &ShimParams) -> Vec<u8> {
    let mut a = Asm::new();
    let done = a.label();
    prologue(&mut a);
    a.mov_imm(7, p.data);
    stage(&mut a, 0xF); // the vectored shim was entered (15 = fault path)
    // Record what faulted: `rcx` = EXCEPTION_POINTERS*, [rcx] = EXCEPTION_RECORD*,
    // +0x10 = ExceptionAddress, +0x00 = ExceptionCode. Written into the data page
    // (outside the image) so the host can read it with /proc/<pid>/mem.
    a.bs(&[0x48, 0x8B, 0x01]); // mov rax, [rcx]
    a.bs(&[0x48, 0x8B, 0x50, 0x10]); // mov rdx, [rax+0x10]
    a.bs(&[0x48, 0x89, 0x57, 0x40]); // mov [rdi+0x40], rdx
    a.bs(&[0x8B, 0x10]); // mov edx, [rax]
    a.bs(&[0x48, 0x89, 0x57, 0x48]); // mov [rdi+0x48], rdx
    a.mov_imm(10, p.probe);
    a.mov_imm(0, SEAL_MAGIC);
    a.bs(&[0x49, 0x3B, 0x02]); // cmp [r10], rax
    a.jcc(4, done); // je — already plaintext
    // A window that is already open means the image is plaintext too (the tick
    // shim decrypted it and has not closed it yet). Decrypting again would XOR it
    // *back* into ciphertext while the faulting code is being executed.
    a.bs(&[0x48, 0x83, 0x7F, D_LOCK as u8, 0x00]); // cmp qword [rdi+lock], 0
    a.jcc(5, done); // jnz done
    a.mov_r10_tid();
    a.try_lock();
    a.jcc(5, done); // jnz — a tick owns the window (then it is plaintext already)
    // decrypt while the pages are still RW, only then make them executable
    crypt(&mut a, p.base, p.size);
    a.mov_imm(7, p.data); // crypt clobbers rdi
    protect_call(&mut a, p, ffi::PAGE_EXECUTE_READ);
    a.mov_imm(1, p.base);
    a.mov_imm(2, p.size);
    a.bs(&[0x41, 0xB8, 0xFF, 0xFF, 0xFF, 0xFF]);
    a.mov_mem_rdi_imm8(D_LEAKED as u8, 1);
    a.mov_mem_rdi_imm8_0(0); // release
    a.bind(done);
    epilogue(&mut a);
    // Continue the VEH chain: this shim only repairs readability, it does not
    // prove that the faulting instruction itself is safe to retry.
    a.bs(&[0xB8, 0, 0, 0, 0]); // mov eax, EXCEPTION_CONTINUE_SEARCH (0)
    a.b(0xC3); // ret
    a.finish()
}

/// Allocate the shim page + data page, write both shims, make the code RX.
///
/// Layout: page 0 = code (tick shim at +0, exception shim at `VEH_OFF`),
/// page 1 = `SealData`. Neither is ever part of the image, so `--seal` does not
/// touch them.
const VEH_OFF: usize = 0x800;

fn install_shims(mut p: ShimParams) -> Option<(usize, usize)> {
    // The data page is an operand of the shim itself (`&old_prot`), so it has to
    // exist before the code is generated.
    // SAFETY: no input buffers are shared with FFI; two full pages are allocated
    // and owned by this installation, with code and data kept at distinct offsets.
    let mem = unsafe { ffi::VirtualAlloc(0, 2 * PAGE_SIZE, ffi::MEM_COMMIT | ffi::MEM_RESERVE, ffi::PAGE_READWRITE) };
    if mem.is_null() {
        log::log_tag("hook", "l2b: VirtualAlloc(2 pages) failed");
        return None;
    }
    let code = mem as usize;
    let data = code + PAGE_SIZE;
    p.data = data as u64;
    let tick = build_tick(&p);
    let veh = build_veh(&p);
    if tick.len() > VEH_OFF {
        log::log_tag("hook", &format!("l2b: tick shim {} bytes > slot {VEH_OFF:#x}", tick.len()));
        return None;
    }
    if VEH_OFF + veh.len() > PAGE_SIZE {
        log::log_tag("hook", &format!("l2b: exception shim {} bytes overflows the page", veh.len()));
        return None;
    }
    // SAFETY: code points to two writable pages; the size checks above prove
    // tick and VEH payloads fit their non-overlapping code-page slots.
    unsafe {
        core::ptr::copy_nonoverlapping(tick.as_ptr(), code as *mut u8, tick.len());
        core::ptr::copy_nonoverlapping(veh.as_ptr(), (code + VEH_OFF) as *mut u8, veh.len());
    }
    let mut old = 0u32;
    // SAFETY: old is a valid output pointer and code names our allocated page.
    if unsafe { ffi::VirtualProtect(code as *mut u8, PAGE_SIZE, ffi::PAGE_EXECUTE_READ, &mut old) } == 0 {
        log::log_tag("hook", "l2b: VirtualProtect(RX) on the shim page failed");
        return None;
    }
    flush(code, PAGE_SIZE);
    log::log_tag(
        "hook",
        &format!(
            "l2b: shim page {code:#x} (tick {} B at +0, exception {} B at +{VEH_OFF:#x}), \
             data page {data:#x}, image {:#x}+{:#x}, crypto=block8(repeat8)",
            tick.len(),
            veh.len(),
            p.base,
            p.size
        ),
    );
    Some((code, data))
}

/// Detour called by the shim *inside* the decrypted window. It must not touch
/// anything outside the image: it is image code running from plaintext.
///
/// # Safety
/// `data` is the shim's `SealData` page (passed in `rcx` by the asm).
pub unsafe extern "system" fn seal_detour(data: *mut SealData) {
    if data.is_null() {
        return;
    }
    let now = now_ms();
    let iv = INTERVAL_MS.load(Ordering::Relaxed).max(1);
    // The shim asks again in `iv` ms; the deadline lives outside the image so
    // that the sealed fast path can read it without decrypting anything.
    // SAFETY: data was checked non-null and points to the dedicated writable
    // SealData page provided by install_shims; due is within that page.
    unsafe { core::ptr::write_volatile(&mut (*data).due, now + iv) };
    // SAFETY: data is the validated writable SealData page; reads/writes below
    // stay within that page and are ordered against the shim by volatile stores.
    let wins = unsafe { (*data).windows.wrapping_add(1) };
    // SAFETY: as above, windows belongs to the validated writable SealData page.
    unsafe { core::ptr::write_volatile(&mut (*data).windows, wins) };
    if wins == 1 {
        // Proof that the shim reached Rust: if this line never appears, the
        // window is being opened but the `call detour` goes somewhere else.
        log::log_tag(
            "l2b",
            &format!(
                "first window: now {now}, iv {iv}, hits {}, leaked {}",
                // SAFETY: data points to the same validated SealData page.
                unsafe { (*data).hits },
                // SAFETY: as above, leaked is inside the dedicated SealData page.
                unsafe { (*data).leaked }
            ),
        );
    }

    let n = TICKS.fetch_add(1, Ordering::Relaxed) + 1;
    let prev = WORK_MS.swap(now, Ordering::Relaxed);
    if BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    let _ = log::guard("hook::tick", || work(n, now, now.wrapping_sub(prev)));
    BUSY.store(false, Ordering::Release);
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
/// Set when a stub-thread console loop exists (`HROMATITE_LIVE > 0`): the tick then
/// does no file IO, only the heartbeat.
static LIVE_MODE: AtomicU64 = AtomicU64::new(0);
static HOOKS: Mutex<Vec<Hook>> = Mutex::new(Vec::new());

fn now_ms() -> u64 {
    // SAFETY: parameterless Win32 query with no pointer preconditions.
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
    // In LIVE mode the stub thread owns the command channel; polling it from
    // inside a QPC detour makes Unity's GC `SuspendThread` fail ("Fatal error in
    // GC — SuspendThread loop failed"), so the tick stays passive there.
    if LIVE_MODE.load(Ordering::Relaxed) > 0 {
        return;
    }
    // The stub console loop reaped dead Boehm records every ~0.8 s, but the
    // frame-hook path bypassed that loop entirely. Crash 2026-11-15_031016139
    // proved the gap: tid 0x3bc, errcode 0x5. Run the same table scan on the
    // existing tick (250 ms): no new thread, no file IO, only Win32 handle
    // checks. Log only repairs/diagnostics, not another periodic heartbeat.
    let reaped = log::guard("gct tick reap", crate::gcthreads::reap).unwrap_or_default();
    for line in reaped {
        log::log_tag("gct", &line);
    }
    let _ = log::guard("hook::work", || {
        let mut reader = {
            let mut g = match READER.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            g.take().unwrap_or_default()
        };
        // Never hold READER's mutex across a longjmp boundary: a failed poll
        // simply leaves `None`, and the next poll constructs a fresh reader.
        let cmds = log::guard("reader.poll", || reader.poll()).unwrap_or_default();
        {
            let mut g = match READER.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            *g = Some(reader);
        }
        // At most once per 5 s: what the reader actually sees. In frame-hook mode
        // this is the only way to tell "nothing was appended" from "file IO fails
        // inside a QPC detour".
        let lg2 = POLL_MS.load(Ordering::Relaxed);
        if (lg2 == 0 || now.wrapping_sub(lg2) >= 5_000)
            && POLL_MS
                .compare_exchange(lg2, now, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
        {
            log::log_tag(
                "hook",
                &format!("poll: {} command(s), {}", cmds.len(), console::cmd_pos()),
            );
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

static GC_CONTEXT_ORIG: AtomicUsize = AtomicUsize::new(0);

/// Atomic race repair *inside* Boehm's own GetThreadContext call.
///
/// # Safety
/// The generated detour forwards the original pointers unchanged. `context`
/// must point to a writable Win64 `CONTEXT` when non-null, and `original` must
/// contain the trampoline generated by `install_at` before this is patched in.
pub unsafe extern "system" fn gc_context_detour(thread: *mut u8, context: *mut u8) -> i32 {
    if let Some(id) = crate::gcthreads::unlink_if_dead(thread as u64) {
        if crate::safe::write_bytes(context as usize, &[0u8; 0x4f0]) {
            log::log_tag("gct", &format!("inline guard: stale record id={id:#x} unlinked inside GetThreadContext"));
            return 1;
        }
        log::log_tag("gct", &format!("inline guard: id={id:#x} has no writable CONTEXT buffer"));
    }
    let original = GC_CONTEXT_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return 0;
    }
    // SAFETY: original stores the GetThreadContext trampoline installed by
    // hook_inline, whose ABI exactly matches the declared two pointer arguments.
    let f: unsafe extern "system" fn(*mut u8, *mut u8) -> i32 = unsafe { core::mem::transmute(original) };
    // SAFETY: both pointers are forwarded unchanged from the OS caller.
    unsafe { f(thread, context) }
}

pub fn install_gc_context_guard() -> bool {
    let Some(target) = try_resolve("kernel32.dll", "GetThreadContext") else {
        log::log_tag("gct", "GetThreadContext was not resolved; inline guard disabled");
        return false;
    };
    let Some(prep) = prepare(target) else {
        log::log_tag("gct", "GetThreadContext prologue rejected; inline guard disabled");
        return false;
    };
    // Publish before patching, exactly like QPC: a collector that enters during
    // the patch window must always have a valid original trampoline to call.
    GC_CONTEXT_ORIG.store(prep.trampoline, Ordering::Release);
    match patch(&prep, gc_context_detour as *const () as usize) {
        Some(h) => {
            register(h);
            log::log_tag("gct", "inline GetThreadContext guard installed");
            true
        }
        None => {
            GC_CONTEXT_ORIG.store(0, Ordering::Release);
            log::log_tag("gct", "inline GetThreadContext guard installation failed");
            false
        }
    }
}

// ---- worker-body provenance (why did a GC record leak?) -------------------

static GC_DISPATCH_ORIG: AtomicUsize = AtomicUsize::new(0);
static GC_UNREGISTER_ORIG: AtomicUsize = AtomicUsize::new(0);

pub const WORKER_DISPATCH_RVA: u32 = 0xbf5ce0;
pub const GC_UNREGISTER_RVA: u32 = 0xc41920;

/// Record the real worker body before the shared OS trampoline can free its
/// argument block. `RCX=inner`, with `{thread,fn,arg}` at offsets 0/8/16.
///
/// # Safety
/// The generated detour preserves the dispatcher ABI. `inner` is forwarded from
/// il2cpp and is read through the checked OS-backed reader; the trampoline must
/// name the same function before it is published.
pub unsafe extern "system" fn gc_worker_detour(inner: *mut u8) -> usize {
    let tid = crate::gcthreads::own_tid();
    if let Some([entry, arg]) = crate::safe::read_words::<2>((inner as usize).wrapping_add(8)) {
        crate::gcthreads::note_worker_body(tid, entry, arg);
    }
    let original = GC_DISPATCH_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return 0;
    }
    // SAFETY: original is the trampoline for the one-argument worker dispatcher;
    // forwarding preserves RAX even though il2cpp ignores it on this path.
    let f: unsafe extern "system" fn(*mut u8) -> usize = unsafe { core::mem::transmute(original) };
    // SAFETY: inner is the original, unmodified dispatcher argument.
    unsafe { f(inner) }
}

/// Mark an actual unregister attempt for the same tid. The original returns
/// zero in EAX on this build; usize is ABI-compatible and preserves it.
///
/// # Safety
/// No arguments are invented and the original trampoline is called only after
/// it has been published by the installer.
pub unsafe extern "system" fn gc_unregister_detour() -> usize {
    let tid = crate::gcthreads::own_tid();
    crate::gcthreads::note_unregister(tid);
    let original = GC_UNREGISTER_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return 0;
    }
    // SAFETY: original is the trampoline for the no-argument, EAX-returning
    // unregister function at GC_UNREGISTER_RVA.
    let f: unsafe extern "system" fn() -> usize = unsafe { core::mem::transmute(original) };
    // SAFETY: the no-argument trampoline supplies the original execution context.
    unsafe { f() }
}

fn install_gc_trace_at(
    ga: usize,
    rva: u32,
    original: &AtomicUsize,
    detour: usize,
    label: &str,
) -> bool {
    if original.load(Ordering::Acquire) != 0 {
        return true;
    }
    let target = ga.wrapping_add(rva as usize);
    let Some(prep) = prepare(target) else {
        log::log_tag("gct", &format!("worker trace: prologue rejected at {label} ({target:#x})"));
        return false;
    };
    original.store(prep.trampoline, Ordering::Release);
    match patch(&prep, detour) {
        Some(h) => {
            register(h);
            log::log_tag("gct", &format!("worker trace: {label} hooked at {target:#x}"));
            true
        }
        None => {
            original.store(0, Ordering::Release);
            log::log_tag("gct", &format!("worker trace: patch failed at {label} ({target:#x})"));
            false
        }
    }
}

/// Trace only in plaintext stub/live mode: these detours execute from the image
/// and must not survive into a later full seal.
pub fn install_gc_worker_trace() -> bool {
    if seal_active() {
        log::log_tag("gct", "worker trace disabled under full sealing");
        return false;
    }
    if crate::symbols::GAMEASSEMBLY_SHA256
        != "648f61b13e4f6daf6b144d03ff0fcdd0b294ca4b812ade90b7f7f9fcbd25903b"
    {
        log::log_tag("gct", "worker trace disabled: RVA map belongs to another GameAssembly build");
        return false;
    }
    let Some((ga, _)) = crate::procs::own_module(crate::GAME_ASSEMBLY) else {
        log::log_tag("gct", "worker trace disabled: GameAssembly is not loaded");
        return false;
    };
    let dispatch = install_gc_trace_at(
        ga,
        WORKER_DISPATCH_RVA,
        &GC_DISPATCH_ORIG,
        gc_worker_detour as *const () as usize,
        "worker dispatcher",
    );
    let unregister = install_gc_trace_at(
        ga,
        GC_UNREGISTER_RVA,
        &GC_UNREGISTER_ORIG,
        gc_unregister_detour as *const () as usize,
        "GC unregister",
    );
    log::log_tag(
        "gct",
        &format!("worker trace installed: dispatcher={dispatch}, unregister={unregister}"),
    );
    dispatch && unregister
}

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
            // SAFETY: counter was just checked non-null and is forwarded from
            // QueryPerformanceCounter, which requires a writable LARGE_INTEGER.
            unsafe { *counter = (now_ms() as i64) * 10_000 };
        }
        return 1;
    }
    // SAFETY: orig is the QPC trampoline generated for this exact signature.
    let f: unsafe extern "system" fn(*mut i64) -> i32 = unsafe { core::mem::transmute(orig) };
    // SAFETY: counter is the original QPC argument forwarded unchanged.
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
            // SAFETY: orig is the D3D11CreateDevice trampoline with this exact ABI.
            unsafe { core::mem::transmute(orig) };
        // SAFETY: all ten arguments are forwarded unchanged from the game.
        unsafe { f(a1, a2, a3, a4, a5, a6, a7, a8, a9, a10) }
    } else {
        -1
    };
    tick();
    ret
}

// ---- concrete IL2CPP LateUpdate -------------------------------------------

static LATE_ORIG: AtomicUsize = AtomicUsize::new(0);
static LATE_ENTRY: AtomicUsize = AtomicUsize::new(0);
static LATE_ATTACHED: AtomicBool = AtomicBool::new(false);
static LATE_CLASS: AtomicUsize = AtomicUsize::new(0);
static LATE_INFO: AtomicUsize = AtomicUsize::new(0);
static LATE_HITS: AtomicU64 = AtomicU64::new(0);
static LATE_TID: AtomicU64 = AtomicU64::new(0);

fn managed_entry_in_module(entry: usize, module: Option<(usize, usize)>) -> bool {
    let Some((base, size)) = module else {
        return false;
    };
    entry >= base && entry < base.saturating_add(size)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LateInstallAction {
    Patch,
    AlreadyAttached,
}

fn late_install_action(last_entry: usize, attached: bool, candidate: usize) -> Result<LateInstallAction, String> {
    if attached && last_entry == candidate {
        return Ok(LateInstallAction::AlreadyAttached);
    }
    if last_entry == 0 || (!attached && last_entry == candidate) {
        return Ok(LateInstallAction::Patch);
    }
    Err("another LateUpdate target is configured; restart before changing it".into())
}

/// IL2CPP instance-method ABI: `this` in rcx and hidden `MethodInfo*` in rdx.
/// The real LateUpdate runs first; our throttled work happens after it.
///
/// # Safety
/// Both arguments are forwarded unchanged to the trampoline for the exact
/// managed method selected by `install_late_update`.
pub unsafe extern "system" fn late_update_detour(
    this: *mut core::ffi::c_void,
    method: *mut core::ffi::c_void,
) {
    let original = LATE_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    // SAFETY: install_late_update publishes the trampoline for the same
    // two-argument IL2CPP method before patching the target.
    let f: unsafe extern "system" fn(*mut core::ffi::c_void, *mut core::ffi::c_void) =
        unsafe { core::mem::transmute(original) };
    // SAFETY: the original arguments are opaque to us and are not modified.
    unsafe { f(this, method) };
    let hit = LATE_HITS.fetch_add(1, Ordering::Relaxed) + 1;
    let tid = crate::gcthreads::own_tid() as u64;
    LATE_TID.store(tid, Ordering::Relaxed);
    if hit == 1 {
        log::log_tag(
            "late",
            &format!("first LateUpdate hit: tid={tid:#x}, this={this:?}, MethodInfo={method:?}"),
        );
    }
    tick();
}

/// Make a lazily-created class available to the startup installer. This calls
/// IL2CPP and is therefore only for the injection stub thread, never a QPC hook.
pub fn materialise_late_class(selector: crate::lateupdate::Selector) -> bool {
    use crate::lateupdate::Selector;
    let index = match selector {
        Selector::Address(_) => return crate::lateupdate::resolve(selector).is_some(),
        Selector::Index(index) => index,
    };
    if crate::lateupdate::resolve(selector).is_some() {
        return true;
    }
    let Some((ga, gasize)) = crate::procs::own_module(crate::GAME_ASSEMBLY) else {
        return false;
    };
    let f = ga.wrapping_add(crate::m3::IMAGE_GET_CLASS_RVA as usize);
    if f < ga || f >= ga.saturating_add(gasize) {
        return false;
    }
    let Some(corlib) = crate::il2cpp::find_domain() else {
        return false;
    };
    let Some(image) = crate::m3::image_of(corlib, "Assembly-CSharp") else {
        return false;
    };
    // SAFETY: f is the build-measured il2cpp_image_get_class(image, index) and
    // is called only from the injection stub. The class may initialize lazily,
    // but no managed constructor is invoked here and VEH contains runtime faults.
    let class = unsafe { crate::veh::call2(f, image, index) }.unwrap_or(0);
    class >= 0x10000
        && crate::safe::readable_at(class)
        && crate::lateupdate::resolve(selector).is_some()
}

/// Resolve and hook one concrete class's own `LateUpdate` method.
pub fn install_late_update(selector: crate::lateupdate::Selector) -> Result<crate::lateupdate::Candidate, String> {
    if env_flag("HROMATITE_SEAL") || env_flag("HROMATITE_L2B") {
        return Err("LateUpdate is an in-image detour; full sealing is not supported for it yet".into());
    }
    let candidate = crate::lateupdate::resolve(selector)
        .ok_or_else(|| "class has no materialised own method named `LateUpdate`".to_string())?;
    let ga = crate::procs::own_module(crate::GAME_ASSEMBLY);
    if !managed_entry_in_module(candidate.entry, ga) {
        return Err(format!("managed entry {:#x} is outside GameAssembly.dll", candidate.entry));
    }
    if late_install_action(
        LATE_ENTRY.load(Ordering::Acquire),
        LATE_ATTACHED.load(Ordering::Acquire),
        candidate.entry,
    )? == LateInstallAction::AlreadyAttached
    {
        return Ok(candidate);
    }
    let prep = prepare(candidate.entry)
        .ok_or_else(|| format!("cannot prepare a trampoline at {:#x}", candidate.entry))?;
    if let Some(interval) = std::env::var("HROMATITE_TICK_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
        INTERVAL_MS.store(interval.max(1), Ordering::Relaxed);
    }
    let live = std::env::var("HROMATITE_LIVE").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    LIVE_MODE.store(live, Ordering::Relaxed);
    let _ = START_MS.compare_exchange(0, now_ms(), Ordering::AcqRel, Ordering::Relaxed);
    LATE_HITS.store(0, Ordering::Relaxed);
    LATE_CLASS.store(candidate.class, Ordering::Relaxed);
    LATE_INFO.store(candidate.info, Ordering::Relaxed);
    LATE_ORIG.store(prep.trampoline, Ordering::Release);
    let installed = patch(&prep, late_update_detour as *const () as usize);
    let Some(hook) = installed else {
        LATE_ORIG.store(0, Ordering::Release);
        LATE_ATTACHED.store(false, Ordering::Release);
        LATE_CLASS.store(0, Ordering::Relaxed);
        LATE_INFO.store(0, Ordering::Relaxed);
        return Err("patch failed (prologue, atomic window or page protection)".into());
    };
    LATE_ENTRY.store(candidate.entry, Ordering::Release);
    LATE_ATTACHED.store(true, Ordering::Release);
    register(hook);
    let class_name = crate::m3::class_full_name(candidate.class);
    let rva = ga.map(|(base, _)| candidate.entry.saturating_sub(base)).unwrap_or(0);
    log::log_tag(
        "late",
        &format!(
            "hooked {class_name} ({:#x}) index={:?}, MethodInfo={:#x}, entry={:#x} (GA+{rva:#x})",
            candidate.class, candidate.index, candidate.info, candidate.entry
        ),
    );
    Ok(candidate)
}

fn late_attachment_present(entry: usize, hooks: &[(usize, bool)]) -> bool {
    entry != 0 && hooks.iter().any(|(target, detached)| *target == entry && !*detached)
}

pub fn late_update_status() -> Vec<String> {
    let entry = LATE_ENTRY.load(Ordering::Acquire);
    if entry == 0 {
        return vec!["LateUpdate: not hooked (use `latec`, then `latehook #index`)".into()];
    }
    if !LATE_ATTACHED.load(Ordering::Acquire) {
        return vec![format!(
            "LateUpdate: detached entry={entry:#x}; repeat `latehook` for the same target to reinstall"
        )];
    }
    let hits = LATE_HITS.load(Ordering::Relaxed);
    let uptime = now_ms().saturating_sub(START_MS.load(Ordering::Relaxed));
    let rate = if uptime == 0 { 0.0 } else { hits as f64 * 1000.0 / uptime as f64 };
    let class = LATE_CLASS.load(Ordering::Relaxed);
    vec![format!(
        "LateUpdate: class {class:#x} {}, MethodInfo={:#x}, entry={entry:#x}, hits={hits} ({rate:.1}/s), last tid={:#x}",
        crate::m3::class_full_name(class),
        LATE_INFO.load(Ordering::Relaxed),
        LATE_TID.load(Ordering::Relaxed)
    )]
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
            // SAFETY: orig is the four-integer trampoline generated by install_at.
            unsafe { core::mem::transmute(orig) };
        // SAFETY: all four integer arguments are forwarded unchanged.
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
    crate::raw::sys::export_address(module, func)
}

fn register(h: Hook) {
    let mut g = match HOOKS.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    g.push(h);
}

/// Trampoline the current shim jumps back to (an install can be retried on a
/// second anchor, and the address is baked into the code).
static SEAL_TRAMP: AtomicUsize = AtomicUsize::new(0);
/// Image range measured by the injector (`base, size`). Preferred over
/// `VirtualQuery`, which reports the section split rather than `SizeOfImage` —
/// with the wrong size the L2b shim decrypts a sub-range, the probe keeps reading
/// ciphertext and every QPC call pays a full decrypt (~150k/s → frozen game).
static IMAGE_BASE: AtomicUsize = AtomicUsize::new(0);
static IMAGE_SIZE: AtomicUsize = AtomicUsize::new(0);

/// Executable sections only: the range the shim protects `RX` inside a window.
static CODE_LO: AtomicUsize = AtomicUsize::new(0);
static CODE_HI: AtomicUsize = AtomicUsize::new(0);

pub fn set_code_range(lo: usize, hi: usize) {
    CODE_LO.store(lo, Ordering::Release);
    CODE_HI.store(hi, Ordering::Release);
    log::log_tag("hook", &format!("code range for RX: {lo:#x}..{hi:#x}"));
}

/// Range that stays plaintext when the image is sealed (the payload's `.text`).
/// With `HROMATITE_L2B_PARTIAL=1` the tick is the ordinary in-image detour and
/// the injector simply skips these pages — no shim, no decrypt-on-call.
static KEEP_LO: AtomicUsize = AtomicUsize::new(0);
static KEEP_HI: AtomicUsize = AtomicUsize::new(0);

/// Called from `DllMain` with the values the injector passed (r8).
pub fn set_keep_range(lo: usize, hi: usize) {
    KEEP_LO.store(lo, Ordering::Release);
    KEEP_HI.store(hi, Ordering::Release);
    log::log_tag("hook", &format!("code range kept plaintext: {lo:#x}..{hi:#x}"));
}

/// Called from `DllMain` with the values the injector passed (r8).
pub fn set_image_range(base: usize, size: usize) {
    if base != 0 && size != 0 {
        IMAGE_BASE.store(base, Ordering::Release);
        IMAGE_SIZE.store(size, Ordering::Release);
        log::log_tag("hook", &format!("image range from injector: {base:#x}+{size:#x}"));
    }
}

/// Build the shim page for this trampoline and arm it — idempotent.
fn arm_l2b(p: &Prep) -> Option<usize> {
    if seal_active() {
        let t = SEAL_TRAMP.load(Ordering::Acquire);
        if t == p.trampoline {
            return Some(SEAL_ENTRY.load(Ordering::Acquire));
        }
        log::log_tag(
            "hook",
            &format!(
                "l2b: shim is bound to trampoline {t:#x}, this anchor has {:#x} — building a second page",
                p.trampoline
            ),
        );
    }
    let (base, size) = match own_image_range() {
        Some(v) => v,
        None => {
            log::log_tag("hook", "l2b: cannot determine own image range (VirtualQuery failed)");
            return None;
        }
    };
    // Page-align: the injector seals whole `SizeOfImage` pages, and every page
    // index must mean the same thing on both sides.
    let base = base & !(PAGE_SIZE - 1);
    let size = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let (vprotect, flush_fn, gettick) = match (
        try_resolve("kernel32.dll", "VirtualProtect"),
        try_resolve("kernel32.dll", "FlushInstructionCache"),
        try_resolve("kernel32.dll", "GetTickCount64"),
    ) {
        (Some(a), Some(b), Some(c)) => (a as u64, b as u64, c as u64),
        _ => {
            log::log_tag("hook", "l2b: kernel32!VirtualProtect/FlushInstructionCache/GetTickCount64 missing");
            return None;
        }
    };
    // The probe must *read back as the magic* while the image is plaintext: the
    // headers are erased to zero, so stamp the magic into page 0 (it is dead
    // space either way) and let the seal turn it into ciphertext.
    // SAFETY: base is page-aligned and belongs to our own mapping; old is a live
    // out pointer and the qword store stays inside that RW page. Result ignored
    // deliberately: payload headers are already free/dead space.
    unsafe {
        let mut old = 0u32;
        ffi::VirtualProtect(base as *mut u8, PAGE_SIZE, ffi::PAGE_READWRITE, &mut old);
        core::ptr::write_volatile(base as *mut u64, SEAL_MAGIC);
    }
    let params = ShimParams {
        base: base as u64,
        size: size as u64,
        data: 0, // filled in by `install_shims` once the page exists
        // Page 0 of the image: the injector erases the PE headers, so these
        // bytes are free. The magic is written there below; after a seal they
        // become ciphertext, which is exactly what the shim tests for.
        // (A Rust static is NOT usable here: statics can be placed outside the
        // mapped image — measured 11 MB below it — so they survive the seal and
        // the shim keeps believing the image is plaintext.)
        probe: base as u64,
        trampoline: p.trampoline as u64,
        detour: seal_detour as *const () as u64,
        vprotect,
        flush: flush_fn,
        gettick,
        note: l2b_note as *const () as u64,
        text_lo: CODE_LO.load(Ordering::Acquire) as u64,
        text_hi: CODE_HI.load(Ordering::Acquire) as u64,
    };
    let (code, data) = install_shims(params)?;
    let entry = code;
    let vein = code + VEH_OFF;
    SEAL_DATA.store(data, Ordering::Release);
    SEAL_ENTRY.store(entry, Ordering::Release);
    SEAL_VEH.store(vein, Ordering::Release);
    SEAL_TRAMP.store(p.trampoline, Ordering::Release);
    // First tick no earlier than `SEAL_START_DELAY_MS`: the injector seals only
    // after `DllMain` returns, and a half-sealed image must not be decrypted.
    // SAFETY: data is the dedicated second page returned by install_shims and
    // is writable/aligned for SealData; only fields within that page are used.
    unsafe {
        let d = data as *mut SealData;
        core::ptr::write_volatile(&mut (*d).due, now_ms() + SEAL_START_DELAY_MS);
        (*d).lock = 0;
        (*d).leaked = 0;
        (*d).hits = 0;
        (*d).windows = 0;
    }
    // SAFETY: vein points to the fully emitted, RX exception-shim code which
    // remains allocated for the process lifetime and follows the VEH ABI.
    let h = unsafe {
        ffi::AddVectoredExceptionHandler(1, vein as *const core::ffi::c_void)
    };
    log::log_tag(
        "hook",
        &format!(
            "l2b: armed — decrypt-on-entry at {entry:#x}, exception shim {vein:#x} registered={}, \
             first tick in {SEAL_START_DELAY_MS}ms, probe {:#x}",
            !h.is_null(),
            params.probe
        ),
    );
    Some(entry)
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

    // L2b: with a seal ahead of us the detour must live outside the image, so the
    // tick is entered through a shim page instead of `qpc_detour` directly.
    // `HROMATITE_SEAL` and `HROMATITE_L2B` both arm it (the wrapper exports SEAL
    // as 0 when neither is set, which keeps the old in-image behaviour).
    // `HROMATITE_L2B_PARTIAL=1`: the injector seals everything except our `.text`,
    // so the tick runs as a plain in-image detour (no shim page, no per-call
    // decrypt). Cheaper and far less risky than the full shim; the region is still
    // `rw-p` and not PE, only the code pages stay executable.
    let partial = env_flag("HROMATITE_L2B_PARTIAL");
    let live = std::env::var("HROMATITE_LIVE").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
    LIVE_MODE.store(live, Ordering::Relaxed);
    log::log_tag(
        "hook",
        &format!(
            "live_mode={live} (tick polling {})",
            if live > 0 { "off" } else { "on" }
        ),
    );
    let l2b = (env_flag("HROMATITE_L2B") || env_flag("HROMATITE_SEAL")) && !partial;
    log::log_tag(
        "hook",
        &format!(
            "l2b shim={l2b} partial_seal={partial}, kept {:x}..{:x}",
            KEEP_LO.load(Ordering::Acquire),
            KEEP_HI.load(Ordering::Acquire)
        ),
    );
    log::log_tag("hook", &format!("l2b requested={l2b} (HROMATITE_L2B/HROMATITE_SEAL)"));

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
        // L2b: swap the in-image detour for the shim page. If that fails we
        // still hook (the seal then breaks the tick, which the log says loudly).
        let detour = if l2b {
            match arm_l2b(&p) {
                Some(d) => d,
                None => {
                    log::log_tag(
                        "hook",
                        "l2b: shim not armed, falling back to the in-image detour (a seal will break it)",
                    );
                    detour
                }
            }
        } else {
            detour
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
                // The GC detour is still in-image code. Full L2b cannot keep
                // every callback in that code inside the external shim's window,
                // so keep only the tick-side reaper in this mode, not this hook.
                let gc_guard = if l2b {
                    log::log_tag("gct", "inline GetThreadContext guard disabled under full L2b sealing");
                    false
                } else {
                    install_gc_context_guard()
                };
                log::log_tag("hook", &format!("inline GC guard installed={gc_guard}; waiting for the game to call us (no thread of our own)"));
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
    if seal_active() {
        log::log_tag("hook", "hookat: in-image diagnostic hooks are disabled under full sealing");
        return None;
    }
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
        // `l2b` — self-inspection of the shim from inside the payload: the host
        // cannot read /proc/<pid>/mem under ptrace_scope=1, but the payload can
        // read its own address space.
        "l2b" => {
            let entry = SEAL_ENTRY.load(Ordering::Acquire);
            let data = SEAL_DATA.load(Ordering::Acquire);
            let tramp = SEAL_TRAMP.load(Ordering::Acquire);
            let mut out = vec![format!(
                "l2b: entry {entry:#x}, data {data:#x}, trampoline {tramp:#x}, probe {:#x}",
                IMAGE_BASE.load(Ordering::Acquire)
            )];
            if entry != 0 {
                let head = crate::safe::read_upto(entry, 16);
                let hex: Vec<String> = head.iter().map(|b| format!("{b:02x}")).collect();
                out.push(format!("  shim bytes: {}", hex.join(" ")));
            }
            if data != 0 {
                let f = |off: usize| crate::safe::read_u64(data + off).unwrap_or(0);
                out.push(format!(
                    "  data: lock={:#x} due={:#x} hits={} leaked={} windows={} crypted={} stage={:#x}",
                    f(0x00),
                    f(0x08),
                    f(0x10),
                    f(0x18),
                    f(0x28),
                    f(0x30),
                    f(0x38)
                ));
            }
            if let Ok(g) = HOOKS.lock() {
                for h in g.iter() {
                    let b = crate::safe::read_upto(h.target, 8);
                    let hex: Vec<String> = b.iter().map(|x| format!("{x:02x}")).collect();
                    out.push(format!(
                        "  anchor {:#x} -> detour {:#x}, bytes {}",
                        h.target,
                        h.detour,
                        hex.join(" ")
                    ));
                }
            }
            out.push(format!("  ticks={} installed={}", TICKS.load(Ordering::Relaxed), INSTALLED.load(Ordering::Relaxed)));
            Some(out)
        }
        "latec" => {
            let found = crate::lateupdate::candidates(120_000);
            let ga = crate::procs::own_module(crate::GAME_ASSEMBLY);
            let mut out = vec![format!("LateUpdate candidates: {} (materialised classes only)", found.len())];
            for candidate in found.iter().take(128) {
                let index = candidate.index.map(|i| format!("#{i}")).unwrap_or_else(|| "-".into());
                let rva = ga.map(|(base, _)| candidate.entry.saturating_sub(base)).unwrap_or(0);
                out.push(format!(
                    "  {index:>10} class={:#x} {}, entry={:#x} (GA+{rva:#x})",
                    candidate.class,
                    crate::m3::class_full_name(candidate.class),
                    candidate.entry
                ));
            }
            if found.len() > 128 {
                out.push(format!("  … {} more", found.len() - 128));
            }
            Some(out)
        }
        "latehook" => {
            let Some(selector) = crate::lateupdate::parse_selector(a1) else {
                return Some(vec!["usage: latehook #<type-index> | <class-address>".into()]);
            };
            let result = log::guard("latehook", || install_late_update(selector))
                .unwrap_or_else(|| Err("installer panicked (see PANIC line)".into()));
            Some(vec![match result {
                Ok(candidate) => format!("LateUpdate hook installed at {:#x}; use `late` for hits/tid", candidate.entry),
                Err(error) => format!("latehook: {error}"),
            }])
        }
        "late" => Some(late_update_status()),
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
            let installed: Vec<(usize, bool)> = g.iter().map(|h| (h.target, h.detached)).collect();
            LATE_ATTACHED.store(
                late_attachment_present(LATE_ENTRY.load(Ordering::Acquire), &installed),
                Ordering::Release,
            );
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
            let d = SEAL_DATA.load(Ordering::Acquire) as *const SealData;
            // Reading this is only legal *because* we are inside the detour, i.e.
            // the image is decrypted right now (see the module comment).
            let l2b = if d.is_null() {
                "l2b=off".to_string()
            } else {
                // SAFETY: d is the dedicated SealData page returned by
                // install_shims and remains allocated for the process lifetime.
                let s = unsafe { &*d };
                format!(
                    "l2b=on shim_hits={} windows={} leaked={} due_in={}ms",
                    s.hits,
                    s.windows,
                    s.leaked,
                    s.due.saturating_sub(now_ms())
                )
            };
            Some(vec![format!(
                "tick {n}, uptime {up}ms ({rate:.1}/s), interval {}ms, hooks={}, busy={}, {l2b}",
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

    // ---- L2b shim ---------------------------------------------------------

    /// The shim's operands are hard-coded byte offsets; Rust must agree.
    #[test]
    fn seal_data_layout_matches_the_asm() {
        use core::mem::offset_of;
        assert_eq!(offset_of!(SealData, lock), D_LOCK);
        assert_eq!(offset_of!(SealData, due), D_DUE);
        assert_eq!(offset_of!(SealData, hits), D_HITS);
        assert_eq!(offset_of!(SealData, leaked), D_LEAKED);
        assert_eq!(offset_of!(SealData, old_prot), D_OLD);
        assert_eq!(offset_of!(SealData, windows), D_WINDOWS);
        assert_eq!(offset_of!(SealData, crypted), D_CRYPTED);
        assert_eq!(core::mem::size_of::<SealData>(), 0x38);
    }

    fn shim_params() -> ShimParams {
        ShimParams {
            base: 0x1000_0000,
            size: 0x4_0000,
            data: 0x2000_0000,
            probe: 0x1000_1000,
            trampoline: 0x3000_0000,
            detour: 0x1000_2000,
            vprotect: 0x7f00_0001_0000,
            flush: 0x7f00_0001_0100,
            gettick: 0x7f00_0001_0200,
            note: 0x7f00_0001_0300,
            text_lo: 0x1000_3000,
            text_hi: 0x1000_4000,
        }
    }

    /// Dump the two shims so the bytes can be disassembled outside the test run
    /// (`objdump -D -b binary -m i386:x86-64`): the point of this module is that
    /// the machine code is readable, so it has to be checkable.
    #[test]
    fn dump_shims() {
        let p = shim_params();
        let tick = build_tick(&p);
        let veh = build_veh(&p);
        let dir = std::env::temp_dir();
        let t = dir.join("hromatite-shim-tick.bin");
        let v = dir.join("hromatite-shim-veh.bin");
        std::fs::write(&t, &tick).unwrap();
        std::fs::write(&v, &veh).unwrap();
        for (name, b) in [("tick", &tick), ("veh", &veh)] {
            println!("{name}: {} bytes -> {}", b.len(), dir.display());
            println!("{name}_hex={}", b.iter().map(|x| format!("{x:02x}")).collect::<String>());
        }
        assert!(tick.len() < VEH_OFF, "tick shim {} bytes does not fit", tick.len());
        assert!(VEH_OFF + veh.len() <= PAGE_SIZE);
    }

    // Execute the emitted cipher block on the host and compare it with
    // `crypto::xor_page`: the asm and the Rust reference must agree byte for
    // byte, otherwise the payload decrypts garbage after the injector sealed.
    #[cfg(target_os = "linux")]
    #[test]
    fn crypt_asm_matches_crypto() {
        use crate::crypto;
        unsafe extern "C" {
            fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
        }
        let size = 3 * PAGE_SIZE + 37; // two whole pages, one short tail
        let mut buf = vec![0u8; size];
        for (i, b) in buf.iter_mut().enumerate() {
            *b = (i * 31 + 7) as u8;
        }
        let orig = buf.clone();
        // blob: push rbx/rsi/rdi, crypt, pop rdi/rsi/rbx, ret
        let mut a = Asm::new();
        a.bs(&[0x53, 0x56, 0x57]); // push rbx, rsi, rdi
        crypt(&mut a, buf.as_ptr() as u64, size as u64);
        a.bs(&[0x5F, 0x5E, 0x5B, 0xC3]); // pop rdi, rsi, rbx, ret
        let code = a.finish();
        // SAFETY: test-only anonymous 4096-byte mapping; arguments are valid and
        // the resulting pointer is checked for NULL/MAP_FAILED before use.
        let mem = unsafe { mmap(core::ptr::null_mut(), 4096, 7, 0x22, -1, 0) }; // RWX, MAP_ANON|PRIVATE
        assert!(!mem.is_null() && mem as isize != -1, "mmap failed");
        // SAFETY: mem is a checked 4096-byte executable mapping and code fits;
        // it contains the expected prologue/crypt/epilogue/ret instruction stream.
        unsafe {
            core::ptr::copy_nonoverlapping(code.as_ptr(), mem, code.len());
            let f: extern "C" fn() = core::mem::transmute(mem);
            f();
        }
        let mut want = orig.clone();
        for (i, chunk) in want.chunks_mut(PAGE_SIZE).enumerate() {
            crypto::xor_page(i as u64, chunk);
        }
        if let Some(i) = (0..buf.len()).find(|&i| buf[i] != want[i]) {
            let page = i / PAGE_SIZE;
            let first = page * PAGE_SIZE;
            let n = (i - first).min(24);
            panic!(
                "asm and crypto::xor_page disagree: first at {i} (page {page}+{:#x});\n asm  {:02x?}\n rust {:02x?}",
                i - first,
                &buf[first..first + n],
                &want[first..first + n]
            );
        }
        assert_ne!(&want[..16], &orig[..16], "the cipher did nothing");
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

    #[test]
    fn relocation_preserves_the_absolute_target() {
        // old next RIP=0x2005, original target=0x200a. Moving the instruction
        // to 0x3000 changes its next RIP to 0x3005, so disp=-0xffb (-4091).
        assert_eq!(adjusted_disp32(5, 0x1000), Some(-4091));
        assert_eq!(adjusted_disp32(i32::MIN, 0x1000), None);
        assert_eq!(adjusted_disp32(i32::MAX, -0x1000), None);
    }

    #[test]
    fn immediate_modrm_forms_have_exact_lengths_and_rip_offsets() {
        let shuffle = [0x0F, 0xC6, 0x05, 4, 0, 0, 0, 0x7f];
        let shuffle_ins = decode_one(&shuffle).unwrap();
        assert_eq!(shuffle_ins.len, shuffle.len());
        assert_eq!(shuffle_ins.rip, Some(3));

        let store = [0xC7, 0x05, 4, 0, 0, 0, 0x78, 0x56, 0x34, 0x12];
        let store_ins = decode_one(&store).unwrap();
        assert_eq!(store_ins.len, store.len());
        assert_eq!(store_ins.rip, Some(2));
    }

    #[test]
    fn non_atomic_patch_alignment_is_refused() {
        assert_eq!(write_mode(3), Some(8));
        assert_eq!(write_mode(11), Some(16));
        for offset in 12..16 {
            assert_eq!(write_mode(offset), None);
        }
    }

    #[test]
    fn operand_size_prefix_selects_imm16() {
        let mov_ax = [0x66, 0xB8, 0x34, 0x12];
        assert_eq!(decode_one(&mov_ax).unwrap().len, mov_ax.len());
        assert!(decode_one(&[0x48, 0x66, 0xB8, 0x34, 0x12]).is_none());
        let push_rex = [0x66, 0x48, 0x68, 0x78, 0x56, 0x34, 0x12];
        assert_eq!(decode_one(&push_rex).unwrap().len, push_rex.len());
        let test_rex = [0x66, 0x48, 0xA9, 0x78, 0x56, 0x34, 0x12];
        assert_eq!(decode_one(&test_rex).unwrap().len, test_rex.len());

        let push = [0x66, 0x68, 0x34, 0x12];
        assert_eq!(decode_one(&push).unwrap().len, push.len());

        let sub = [0x66, 0x81, 0xEC, 0x34, 0x12];
        assert_eq!(decode_one(&sub).unwrap().len, sub.len());

        let store = [0x66, 0xC7, 0x05, 4, 0, 0, 0, 0x78, 0x56];
        let ins = decode_one(&store).unwrap();
        assert_eq!(ins.len, store.len());
        assert_eq!(ins.rip, Some(3));
    }

    #[test]
    fn relative_branches_into_the_overwrite_window_are_rejected() {
        assert!(branch_into_prologue(0x2000, 5, 0, 2, 1)); // 0x2000 -> 0x2003
        assert!(branch_into_prologue(0x2000, 5, 0, 2, -1)); // backward to 0x2001
        assert!(!branch_into_prologue(0x2000, 5, 0, 2, 0x100)); // external target
        assert!(!branch_into_prologue(0x2000, 5, 0, 2, 3)); // original continuation
    }

    // Console command wiring is covered by the live integration check: running
    // `command()` in a host test would pull the Win32-only tick/link symbols in.
    #[test]
    fn detached_history_does_not_mask_a_still_attached_late_update() {
        let entry = 0x1000;
        assert!(late_attachment_present(entry, &[(entry, true), (entry, false)]));
        assert!(!late_attachment_present(entry, &[(entry, true)]));
        assert!(!late_attachment_present(entry, &[(0x2000, false)]));
    }

    #[test]
    fn late_update_lifecycle_distinguishes_attached_from_retained_trampoline() {
        use LateInstallAction::{AlreadyAttached, Patch};
        assert_eq!(late_install_action(0, false, 0x1000), Ok(Patch));
        assert_eq!(late_install_action(0x1000, true, 0x1000), Ok(AlreadyAttached));
        assert_eq!(late_install_action(0x1000, false, 0x1000), Ok(Patch));
        assert!(late_install_action(0x1000, true, 0x2000).is_err());
        assert!(late_install_action(0x1000, false, 0x2000).is_err());
    }

    #[test]
    fn late_update_entry_is_limited_to_game_assembly() {
        let module = (0x6fff_0000, 0x0f44_0000);
        assert!(managed_entry_in_module(0x6fff_1000, Some(module)));
        assert!(managed_entry_in_module(0x6fff_0000 + 0x0f44_0000 - 1, Some(module)));
        assert!(!managed_entry_in_module(0x6fff_0000 + 0x0f44_0000, Some(module)));
        assert!(!managed_entry_in_module(0x1000, Some(module)));
        assert!(!managed_entry_in_module(0x6fff_1000, None));
    }

    #[test]
    fn exception_shim_continues_the_veh_chain() {
        let code = build_veh(&shim_params());
        let marker = [0xB8, 0, 0, 0, 0, 0xC3];
        assert!(
            code.windows(marker.len()).any(|window| window == marker),
            "exception shim must end with EXCEPTION_CONTINUE_SEARCH (0), not -1"
        );
        assert!(!code.windows(6).any(|w| w == [0xB8, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]));
    }
}
