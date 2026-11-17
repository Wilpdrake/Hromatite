//! Inline (trampoline) hooks and the managed LateUpdate command tick.
//!
//! The generic decoder/trampoline/patch engine is shared by the managed hook,
//! the GC GetThreadContext guard and opt-in diagnostics. Runtime work runs on
//! Unity's own managed thread; no payload-owned polling thread is created.
//! Managed method bodies always run after best-effort guarded instrumentation.

use crate::console;
use crate::ffi;
use crate::log;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
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
    Some(Insn {
        len: total,
        rel,
        rip,
    })
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
        log::log_tag(
            "hook",
            &format!("alloc_near: VirtualAlloc at {c:#x} refused"),
        );
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
            // Source qword must have the same alignment as the destination:
            // o16 is the target offset, not the start of the patch window.
            let off = base - base16;
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

pub(crate) struct Prep {
    target: usize,
    pub(crate) prologue: usize,
    pub(crate) trampoline: usize,
    /// `trampoline + RELAY_OFF`; holds `ff 25 00000000 <qword detour>`
    relay: usize,
    alloc_base: usize,
    alloc_size: usize,
}

/// Build the trampoline (relocated prologue copy + absolute jump back) and
/// return it *without* patching the target, so the caller can publish the
/// trampoline address to its detour before any thread can enter it.
pub(crate) fn prepare(target: usize) -> Option<Prep> {
    prepare_span(target, 5)
}

/// Prepare a trampoline for a call-site patch whose copied span may need to
/// include control flow after the overwritten five-byte jump.
fn prepare_span(target: usize, min: usize) -> Option<Prep> {
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
        if let Some((ro, bits)) = ins.rel {
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
        if let Some(ro) = ins.rip {
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
pub(crate) fn patch(p: &Prep, detour: usize) -> Option<Hook> {
    // The patched instruction can only reach the relay (same ±2 GiB); the relay
    // then jumps absolutely to the detour wherever it lives.
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
    // 32 bytes: a 5-byte patch can start at offset 13 of a 16-byte block
    let mut orig32 = [0u8; 32];
    let got = crate::safe::read_upto(base16, 32);
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
            log::log_tag(
                "hook",
                "unhook: refusing a non-atomic 16-byte-straddling restore",
            );
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
static START_MS: AtomicU64 = AtomicU64::new(0);
static INTERVAL_MS: AtomicU64 = AtomicU64::new(250);
static BUSY: AtomicBool = AtomicBool::new(false);
static READER: Mutex<Option<console::Reader>> = Mutex::new(None);
static HOOKS: Mutex<Vec<Hook>> = Mutex::new(Vec::new());

fn now_ms() -> u64 {
    // SAFETY: parameterless Win32 query with no pointer preconditions.
    unsafe { ffi::GetTickCount64() }
}

/// Throttle command-file IO on Unity's managed thread. Atomic claiming and the
/// BUSY flag keep re-entrant calls from overlapping a poll already in progress.
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    let now = now_ms();
    let iv = crate::raw::hot::tick_interval_ms().max(1);
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
    // Never unwind through the game's managed frame.
    let _ = log::guard("hook::tick", work);
    BUSY.store(false, Ordering::Release);
}

fn work() {
    // Inline repairs publish lock-free counters in GetThreadContext; formatting
    // and file IO happen only after the collector's stop-the-world detour.
    if let Some((count, last_tid)) = take_inline_repairs() {
        log::log_tag(
            "gct",
            &format!("inline guard: {count} stale record(s) reaped; last id={last_tid:#x}"),
        );
    }
    for line in crate::raw::gcguard::report_lines() {
        log::log_tag("gct", &line);
    }
    for line in crate::gcthreads::drain_worker_events() {
        log::log_tag("gct", &line);
    }

    // Versioned feature work uses this same already-established game tick. It
    // never creates a worker and never replaces the resident GC guard.
    crate::raw::hot::tick();

    let _ = log::guard("hook::work", || {
        let mut reader = {
            let mut g = match READER.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            g.take().unwrap_or_default()
        };
        // Never hold READER's mutex across a longjmp boundary.
        let cmds = log::guard("reader.poll", || reader.poll()).unwrap_or_default();
        {
            let mut g = match READER.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            *g = Some(reader);
        }
        for line in cmds {
            log::log_tag("con", &format!("> {line}"));
            if let Some(out) = crate::raw::hot::command(&line) {
                for line in out {
                    log::log_tag("con", &line);
                }
                continue;
            }
            if !crate::raw::hot::resident_command(&line) && crate::raw::hot::dispatch(&line) {
                continue;
            }
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
                    log::log_tag(
                        "con",
                        "seal/quit is unavailable with the managed in-image runtime",
                    );
                }
                console::Action::None => {}
            }
        }
    });
}

// ---- detours --------------------------------------------------------------

/// `GC_suspend_all`'s indirect `kernel32!GetThreadContext` call, followed by
/// `test eax,eax` and `jne`. The trampoline carries all three instructions;
/// live and synthetic results therefore share Boehm's original success branch
/// instead of falling into its retry loop.
/// The site itself is per build (`GcAnchors::context_call`); only the shape is
/// stable: `call qword [rip+disp32]; test eax,eax`.
const GC_CONTEXT_CALL_PROLOGUE: usize = 8;

fn valid_gc_context_call_site(bytes: &[u8]) -> bool {
    bytes.len() >= GC_CONTEXT_CALL_PROLOGUE
        && bytes[0] == 0xff
        && bytes[1] == 0x15
        && bytes[6] == 0x85
        && bytes[7] == 0xc0
}

static GC_CONTEXT_ORIG: AtomicUsize = AtomicUsize::new(0);
static GC_THREADS_BASE: AtomicUsize = AtomicUsize::new(0);
static INLINE_REPAIRS: AtomicU64 = AtomicU64::new(0);
static INLINE_LAST_TID: AtomicU32 = AtomicU32::new(0);

/// Record only lock-free counters here. Allocation, formatting and file I/O
/// belong to `work`, after the collector has released the world.
fn note_inline_repair(id: u32) {
    INLINE_LAST_TID.store(id, Ordering::Relaxed);
    INLINE_REPAIRS.fetch_add(1, Ordering::Release);
}

fn take_inline_repairs() -> Option<(u64, u32)> {
    let count = INLINE_REPAIRS.swap(0, Ordering::AcqRel);
    (count != 0).then(|| (count, INLINE_LAST_TID.load(Ordering::Acquire)))
}

const WIN64_CONTEXT_FLAGS: usize = 0x30;
const WIN64_CONTEXT_SIZE: usize = ffi::CONTEXT_QWORDS * core::mem::size_of::<u64>();
const _: () = assert!(WIN64_CONTEXT_SIZE == 0x4d0);

/// Overwrite one caller-owned Win64 CONTEXT without an OS round trip or an
/// intermediate heap/stack buffer. ContextFlags is an input, so preserve it.
///
/// # Safety
/// `context..context+0x4d0` must be valid for unaligned reads and writes and
/// must not overlap a live Rust reference. This is Boehm's output buffer at the
/// exact GetThreadContext call-site, not an arbitrary address.
unsafe fn write_synthetic_dead_context_raw(context: *mut u8) {
    // SAFETY: the caller guarantees a complete 0x4d0-byte CONTEXT. The field
    // offsets and unaligned accesses are valid for the Win64 x64 layout.
    let flags =
        unsafe { core::ptr::read_unaligned(context.add(WIN64_CONTEXT_FLAGS).cast::<u32>()) };
    for index in 0..(WIN64_CONTEXT_SIZE / core::mem::size_of::<u64>()) {
        // SAFETY: index covers exactly 0x4d0 / 8 complete qwords. Explicit
        // unaligned stores prevent LLVM from calling memset in the stop-world.
        let out = unsafe { context.add(index * core::mem::size_of::<u64>()) };
        // SAFETY: out points to one valid qword in the caller's CONTEXT; the
        // instruction requires no stack, preserves flags and accepts misalignment.
        unsafe {
            core::arch::asm!(
                "mov qword ptr [{out}], 0",
                out = in(reg) out,
                options(nostack, preserves_flags)
            );
        }
    }
    // SAFETY: the same complete CONTEXT is live, and this four-byte field is
    // inside it; unaligned access supports the ABI-provided buffer verbatim.
    unsafe {
        core::ptr::write_unaligned(context.add(WIN64_CONTEXT_FLAGS).cast::<u32>(), flags);
    }
}

/// Find and unlink the record owning `handle` while Boehm holds its stop-world
/// lock. Every access is a direct unaligned scalar access into static table /
/// record memory already validated by the collector's own traversal.
///
/// # Safety
/// `base` must address all 256 writable GC_threads bucket pointers. Reachable
/// records must have the measured next/id/handle layout and remain allocated
/// and writable for the duration of the stop-world traversal.
unsafe fn unlink_by_handle_raw(base: usize, handle: usize) -> Option<u32> {
    if base == 0 {
        return None;
    }
    for bucket in 0..256usize {
        // SAFETY: the caller supplies all 256 writable bucket pointers.
        let link = base.wrapping_add(bucket * core::mem::size_of::<usize>());
        let mut previous = 0usize;
        // SAFETY: The caller supplies all 256 writable bucket pointers; link is
        // one complete pointer-sized cell in that table.
        let mut current = unsafe { core::ptr::read_unaligned(link as *const usize) };
        for _ in 0..8 {
            if current == 0 {
                break;
            }
            // SAFETY: caller guarantees the measured GC_Thread_Rep layout and
            // live record memory; unaligned loads avoid imposing Rust alignment
            // on Boehm's allocator.
            let id = unsafe { core::ptr::read_unaligned(current.wrapping_add(8) as *const u32) };
            // SAFETY: current is a traversed live GC record under the caller's
            // stop-world contract, and 0x10 is its complete handle field.
            let record_handle =
                unsafe { core::ptr::read_unaligned(current.wrapping_add(0x10) as *const usize) };
            if id != 0 && record_handle == handle {
                // SAFETY: same live record contract as the field loads above.
                let next = unsafe { core::ptr::read_unaligned(current as *const usize) };
                let predecessor = if previous == 0 { link } else { previous };
                // SAFETY: predecessor is either the validated bucket link or a
                // previously traversed live record's writable next field.
                unsafe { core::ptr::write_unaligned(predecessor as *mut usize, next) };
                return Some(id);
            }
            previous = current;
            // SAFETY: same live record contract as the field loads above.
            current = unsafe { core::ptr::read_unaligned(current as *const usize) };
        }
    }
    None
}

/// Complete the already-classified dead-thread call. Returning success does
/// not depend on finding the record: another Boehm detach may have removed it
/// after its handle was signalled, and falling back to GetThreadContext would
/// put this exact call back into the unbounded retry loop.
///
/// # Safety
/// Combines the contracts of `write_synthetic_dead_context_raw` and
/// `unlink_by_handle_raw`; invoked only from Boehm's GetThreadContext call-site.
unsafe fn complete_dead_gc_call_raw(
    base: usize,
    handle: usize,
    context: *mut u8,
) -> (i32, Option<u32>) {
    // SAFETY: forwarded caller contract; construct a valid output before the
    // best-effort unlink so resume can never observe a corrupt context.
    unsafe { write_synthetic_dead_context_raw(context) };
    // SAFETY: Boehm owns the stop-world lock for both operations.
    let removed = unsafe { unlink_by_handle_raw(base, handle) };
    (1, removed)
}

/// Select the copied instruction stream for this call-site. A signalled handle
/// enters at the copied `test eax,eax`; every other result takes the copied OS
/// call from its first byte. The naked caller sets EAX=1 before either jump:
/// the live OS call overwrites it, while the dead path preserves it for `test`.
///
/// # Safety
/// Called only by `gc_context_detour` with the exact Boehm call-site arguments.
unsafe extern "system" fn gc_context_route(thread: *mut u8, context: *mut u8) -> usize {
    let original = GC_CONTEXT_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return 0;
    }
    let base = GC_THREADS_BASE.load(Ordering::Acquire);
    if base != 0 {
        // SAFETY: RCX is Boehm's own duplicated thread handle. A zero-timeout
        // wait is nonblocking and borrows the handle without closing it.
        let wait = unsafe { ffi::WaitForSingleObject(thread as ffi::HANDLE, 0) };
        if wait == 0 {
            // SAFETY: WAIT_OBJECT_0 proves full termination. The exact call-site
            // contract and stop-world lock validate raw output/table/records.
            let (_, removed) = unsafe { complete_dead_gc_call_raw(base, thread as usize, context) };
            if let Some(id) = removed {
                note_inline_repair(id);
            }
            // Skip only the six-byte indirect OS call in the trampoline. The
            // naked wrapper supplies EAX=1 for the copied `test` and `jne`.
            return original.wrapping_add(6);
        }
    }
    original
}

/// Call-site entry must not `ret`: it was reached by a jump, so its stack return
/// address belongs to GC_suspend_all's caller. Calling an ordinary Rust detour
/// and returning 1 would therefore leave GC_suspend_all early and change its ABI.
/// Instead this tiny frame tail-jumps into the copied call or its following test.
///
/// # Safety
/// May only be patched over the validated 14-byte GetThreadContext call-site.
/// RCX/RDX are forwarded under Win64 ABI and RSP is restored before the jump.
#[unsafe(naked)]
pub unsafe extern "system" fn gc_context_detour() -> ! {
    core::arch::naked_asm!(
        // Entry RSP is 16-byte aligned. Reserve the Win64 shadow area plus two
        // slots for the original arguments: the router is allowed to clobber
        // both RCX and RDX under the same ABI as GetThreadContext itself.
        "sub rsp, 0x30",
        "mov [rsp + 0x20], rcx",
        "mov [rsp + 0x28], rdx",
        "call {route}",
        "mov rcx, [rsp + 0x20]",
        "mov rdx, [rsp + 0x28]",
        "add rsp, 0x30",
        "mov r11, rax",
        "mov eax, 1",
        "jmp r11",
        route = sym gc_context_route,
    );
}

#[allow(dead_code)]
fn install_legacy_gc_context_guard() -> bool {
    let Some((ga, size)) = crate::procs::own_module(crate::GAME_ASSEMBLY) else {
        log::log_tag("gct", "inline guard disabled: GameAssembly is not loaded");
        return false;
    };
    let Some(anchors) = crate::buildinfo::gc_anchors(ga, size) else {
        log::log_tag(
            "gct",
            "inline guard disabled: this GameAssembly build has no GC anchors",
        );
        return false;
    };
    let target = ga.wrapping_add(anchors.context_call as usize);
    let Some(head) = crate::safe::read_bytes(target, 14) else {
        log::log_tag(
            "gct",
            "inline guard disabled: Boehm call-site is unreadable",
        );
        return false;
    };
    if !valid_gc_context_call_site(&head) || head[8..] != [0x0f, 0x85, 0x8b, 0x00, 0x00, 0x00] {
        log::log_tag(
            "gct",
            "inline guard disabled: loaded Boehm call-site signature mismatch",
        );
        return false;
    }
    let Some(prep) = prepare_span(target, 14) else {
        log::log_tag(
            "gct",
            "Boehm GetThreadContext call-site trampoline rejected",
        );
        return false;
    };
    if prep.prologue != 14 {
        log::log_tag(
            "gct",
            "inline guard disabled: trampoline did not copy call; test; jne exactly",
        );
        return false;
    }
    let table_base = ga.wrapping_add(anchors.threads as usize);
    // Resolve the table once. The hot path only performs a zero-timeout wait,
    // direct scalar memory accesses and lock-free counter updates.
    GC_THREADS_BASE.store(table_base, Ordering::Release);
    // Publish before patching: a collector entering during the patch window
    // must always have a valid original trampoline to call.
    GC_CONTEXT_ORIG.store(prep.trampoline, Ordering::Release);
    match patch(&prep, gc_context_detour as *const () as usize) {
        Some(h) => {
            register(h);
            log::log_tag(
                "gct",
                &format!(
                    "Boehm GetThreadContext call-site guard installed at {target:#x}, table {table_base:#x}"
                ),
            );
            true
        }
        None => {
            GC_CONTEXT_ORIG.store(0, Ordering::Release);
            GC_THREADS_BASE.store(0, Ordering::Release);
            log::log_tag(
                "gct",
                "Boehm GetThreadContext call-site guard installation failed",
            );
            false
        }
    }
}

/// Install the synchronized entry-Guard used by every active payload mode.
/// The previous mid-function installer remains above only for its regression
/// fixtures and disassembly references; it is not used at runtime.
pub fn install_gc_context_guard() -> bool {
    crate::raw::gcguard::install()
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

static GC_REMOVE_ORIG: AtomicUsize = AtomicUsize::new(0);
static EXIT_THREAD_ORIG: AtomicUsize = AtomicUsize::new(0);
static END_THREAD_EX_ORIG: AtomicUsize = AtomicUsize::new(0);

pub const GC_REMOVE_RVA: u32 = 0xc417b0;

/// The actual deletion of a Boehm record. `FUN_180c417b0(DWORD tid)`: it walks
/// the bucket without a NULL check, `CloseHandle`s the duplicated handle and
/// `GC_free`s the record. Hooking the wrapper `GC_unregister_my_thread` only
/// proves somebody asked; this proves the record went away.
///
/// # Safety
/// The detour preserves the one-DWORD argument in `ECX` and forwards the
/// original return value; nothing is invented.
pub unsafe extern "system" fn gc_remove_detour(tid: u32) -> usize {
    crate::gcthreads::note_gc_remove(tid);
    let original = GC_REMOVE_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return 0;
    }
    // SAFETY: original is the trampoline for the one-argument removal function
    // at GC_REMOVE_RVA; the argument is forwarded unchanged.
    let f: unsafe extern "system" fn(u32) -> usize = unsafe { core::mem::transmute(original) };
    // SAFETY: as above — tid is the original caller's value.
    unsafe { f(tid) }
}

/// A thread that dies through `ExitThread`/`_endthreadex` never returns from its
/// body, so the body's `GC_unregister_my_thread` is skipped and Boehm keeps a
/// record for a thread that no longer exists. Recording this names the leak.
///
/// # Safety
/// Only the exit code / return address is read and both are forwarded unchanged.
pub unsafe extern "system" fn exit_thread_detour(code: u32) {
    crate::gcthreads::note_thread_exit(crate::gcthreads::own_tid(), "ExitThread", code);
    let original = EXIT_THREAD_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    // SAFETY: original is the kernel32!ExitThread trampoline (one DWORD, no
    // return); the code is forwarded unchanged and this call never returns.
    let f: unsafe extern "system" fn(u32) = unsafe { core::mem::transmute(original) };
    // SAFETY: as above.
    unsafe { f(code) }
}

/// `_endthreadex` — the msvcrt counterpart of `ExitThread`, same leak.
///
/// # Safety
/// Only the caller's code is forwarded unchanged.
pub unsafe extern "system" fn endthreadex_detour(code: u32) {
    crate::gcthreads::note_thread_exit(crate::gcthreads::own_tid(), "_endthreadex", code);
    let original = END_THREAD_EX_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    // SAFETY: original is the _endthreadex trampoline (one DWORD, no return).
    let f: unsafe extern "system" fn(u32) = unsafe { core::mem::transmute(original) };
    // SAFETY: as above.
    unsafe { f(code) }
}

/// Install a detour on a named export of a loaded module.
fn install_named(
    module: &str,
    func: &str,
    original: &AtomicUsize,
    detour: usize,
    label: &str,
) -> bool {
    if original.load(Ordering::Acquire) != 0 {
        return true;
    }
    let Some(target) = try_resolve(module, func) else {
        log::log_tag("gct", &format!("{label}: {module}!{func} not found"));
        return false;
    };
    let Some(prep) = prepare(target) else {
        log::log_tag(
            "gct",
            &format!("{label}: prologue rejected at {module}!{func} ({target:#x})"),
        );
        return false;
    };
    // Publish before patching: a thread that enters in the gap calls the original.
    original.store(prep.trampoline, Ordering::Release);
    match patch(&prep, detour) {
        Some(h) => {
            register(h);
            log::log_tag(
                "gct",
                &format!("{label}: hooked {module}!{func} at {target:#x}"),
            );
            true
        }
        None => {
            original.store(0, Ordering::Release);
            log::log_tag("gct", &format!("{label}: patch failed at {target:#x}"));
            false
        }
    }
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
        log::log_tag(
            "gct",
            &format!("worker trace: prologue rejected at {label} ({target:#x})"),
        );
        return false;
    };
    original.store(prep.trampoline, Ordering::Release);
    match patch(&prep, detour) {
        Some(h) => {
            register(h);
            log::log_tag(
                "gct",
                &format!("worker trace: {label} hooked at {target:#x}"),
            );
            true
        }
        None => {
            original.store(0, Ordering::Release);
            log::log_tag(
                "gct",
                &format!("worker trace: patch failed at {label} ({target:#x})"),
            );
            false
        }
    }
}

/// Install worker provenance hooks only when explicitly requested. These
/// diagnostic detours execute from the same plaintext image as LateUpdate.
pub fn install_gc_worker_trace() -> bool {
    if crate::symbols::GAMEASSEMBLY_SHA256
        != "648f61b13e4f6daf6b144d03ff0fcdd0b294ca4b812ade90b7f7f9fcbd25903b"
    {
        log::log_tag(
            "gct",
            "worker trace disabled: dispatcher/unregister RVAs are only known for build 648f61b1",
        );
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
    // The wrapper only proves that somebody asked; the removal itself proves the
    // record went away. Without it "no unregister line" cannot distinguish
    // "the body skipped the call" from "our hook sat on the wrong wrapper".
    let remove = install_gc_trace_at(
        ga,
        GC_REMOVE_RVA,
        &GC_REMOVE_ORIG,
        gc_remove_detour as *const () as usize,
        "GC remove thread",
    );
    // Last piece of the puzzle: a thread that leaves through ExitThread never
    // returns from its body, so its cleanup call cannot run. These are plain
    // kernel32/msvcrt exports, not GameAssembly code, so they get their own
    // installer; a missing one is not fatal.
    let exit_thread = install_named(
        "kernel32.dll",
        "ExitThread",
        &EXIT_THREAD_ORIG,
        exit_thread_detour as *const () as usize,
        "worker exit",
    );
    let end_thread_ex = install_named(
        "ucrtbase.dll",
        "_endthreadex",
        &END_THREAD_EX_ORIG,
        endthreadex_detour as *const () as usize,
        "worker exit",
    ) || install_named(
        "msvcrt.dll",
        "_endthreadex",
        &END_THREAD_EX_ORIG,
        endthreadex_detour as *const () as usize,
        "worker exit",
    );
    log::log_tag(
        "gct",
        &format!(
            "worker trace installed: dispatcher={dispatch}, unregister={unregister}, remove={remove}, ExitThread={exit_thread}, _endthreadex={end_thread_ex}"
        ),
    );
    // `remove` and `unregister` are the diagnostics that matter; exit hooks are a
    // bonus (wine may route exits through ntdll instead).
    dispatch && unregister && remove
}

// ---- concrete IL2CPP LateUpdate -------------------------------------------

static LATE_ORIG: AtomicUsize = AtomicUsize::new(0);
static LATE_ENTRY: AtomicUsize = AtomicUsize::new(0);
static LATE_ATTACHED: AtomicBool = AtomicBool::new(false);
static LATE_CLASS: AtomicUsize = AtomicUsize::new(0);
static LATE_INFO: AtomicUsize = AtomicUsize::new(0);
static LATE_HITS: AtomicU64 = AtomicU64::new(0);
static LATE_TID: AtomicU64 = AtomicU64::new(0);

fn reset_late_observation() {
    LATE_HITS.store(0, Ordering::Relaxed);
    LATE_TID.store(0, Ordering::Relaxed);
}

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

fn late_install_action(
    last_entry: usize,
    attached: bool,
    candidate: usize,
) -> Result<LateInstallAction, String> {
    if attached && last_entry == candidate {
        return Ok(LateInstallAction::AlreadyAttached);
    }
    if last_entry == 0 || (!attached && last_entry == candidate) {
        return Ok(LateInstallAction::Patch);
    }
    Err("another LateUpdate target is configured; restart before changing it".into())
}

fn tick_then_original<T, E>(
    original_first: bool,
    tick_fn: impl FnOnce() -> Result<(), E>,
    original: impl FnOnce() -> T,
) -> T {
    // Instrumentation is explicitly best-effort; the managed body is mandatory.
    // Reload transactions run it before mapping/init/shutdown; ordinary tick work
    // retains the measured pre-original ordering.
    if original_first {
        let result = original();
        let _ = tick_fn();
        return result;
    }
    let _ = tick_fn();
    original()
}

/// IL2CPP instance-method ABI: `this` in rcx and hidden `MethodInfo*` in rdx.
/// Our throttled work runs before this method's original body. This gives
/// priority over the selected script only: Unity still owns ordering between
/// different MonoBehaviours, so a LateUpdate hook cannot mean "before every
/// script in the game".
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
    tick_then_original(
        crate::raw::hot::original_first_required(),
        || {
            // A payload fault must not skip the game's method. The guard is
            // inside the pre-original half, so the original call remains the
            // next operation even when instrumentation fails.
            log::guard("late_update_detour::pre", || {
                let hit = LATE_HITS.fetch_add(1, Ordering::Relaxed) + 1;
                let tid = crate::gcthreads::own_tid() as u64;
                LATE_TID.store(tid, Ordering::Relaxed);
                if hit == 1 {
                    log::log_tag(
                        "late",
                        &format!("first pre-original LateUpdate hit: tid={tid:#x}, this={this:?}, MethodInfo={method:?}"),
                    );
                }
                tick();
            })
            .map(|_| ())
            .ok_or(())
        },
        // SAFETY: the original arguments are opaque to us and are not modified.
        || unsafe { f(this, method) },
    );
}

/// Make a lazily-created class available to the startup installer. This calls
/// IL2CPP and is therefore only for the injection stub, never a managed hook.
pub fn materialise_late_class(selector: crate::lateupdate::Selector) -> bool {
    use crate::lateupdate::Selector;
    if crate::lateupdate::resolve(selector).is_some() {
        return true;
    }
    let index = match selector {
        Selector::Address(_) => return false,
        Selector::Index(index) => index,
        Selector::Auto => {
            static LOGGED: AtomicBool = AtomicBool::new(false);
            if !LOGGED.swap(true, Ordering::Relaxed) {
                log::log_tag(
                    "late",
                    &format!(
                        "auto target is exactly {}; no arbitrary fallback",
                        crate::lateupdate::AUTO_CLASS
                    ),
                );
            }
            if crate::symbols::GAMEASSEMBLY_SHA256 != crate::lateupdate::AUTO_BUILD_SHA256 {
                return false;
            }
            crate::lateupdate::AUTO_TYPE_INDEX
        }
    };
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
        // In auto mode this re-checks the fully-qualified name. A changed type
        // index must never turn into a hook on an unrelated method.
        && crate::lateupdate::resolve(selector).is_some()
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

pub(crate) fn prepare_tick() {
    if let Some(interval) = std::env::var("HROMATITE_TICK_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        INTERVAL_MS.store(interval.max(1), Ordering::Relaxed);
    }
    let _ = START_MS.compare_exchange(0, now_ms(), Ordering::AcqRel, Ordering::Relaxed);
    if let Ok(mut g) = READER.lock()
        && g.is_none()
    {
        *g = Some(console::Reader::new());
    }
}

/// Resolve and hook one concrete class's own `LateUpdate` method.
pub fn install_late_update(
    selector: crate::lateupdate::Selector,
) -> Result<crate::lateupdate::Candidate, String> {
    if env_flag("HROMATITE_SEAL") || env_flag("HROMATITE_L2B") {
        return Err(
            "LateUpdate is an in-image detour; full sealing is not supported for it yet".into(),
        );
    }
    let candidate = crate::lateupdate::resolve(selector).ok_or_else(|| match selector {
        crate::lateupdate::Selector::Auto => format!(
            "auto target {} is absent/ambiguous or has no valid instance void LateUpdate()",
            crate::lateupdate::AUTO_CLASS
        ),
        _ => "class has no materialised own method named `LateUpdate`".to_string(),
    })?;
    let ga = crate::procs::own_module(crate::GAME_ASSEMBLY);
    if !managed_entry_in_module(candidate.entry, ga) {
        return Err(format!(
            "managed entry {:#x} is outside GameAssembly.dll",
            candidate.entry
        ));
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
    prepare_tick();
    log::log_tag(
        "late",
        &format!("console file {}", console::cmd_path().display()),
    );
    reset_late_observation();
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
    let rva = ga
        .map(|(base, _)| candidate.entry.saturating_sub(base))
        .unwrap_or(0);
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
    entry != 0
        && hooks
            .iter()
            .any(|(target, detached)| *target == entry && !*detached)
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
    let rate = if uptime == 0 {
        0.0
    } else {
        hits as f64 * 1000.0 / uptime as f64
    };
    let class = LATE_CLASS.load(Ordering::Relaxed);
    vec![format!(
        "LateUpdate: pre-original (selected method only; Unity script order not guaranteed), class {class:#x} {}, MethodInfo={:#x}, entry={entry:#x}, hits={hits} ({rate:.1}/s), last tid={:#x}",
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

pub(crate) fn register(h: Hook) {
    let mut g = match HOOKS.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    g.push(h);
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
        "latec" => {
            let found = crate::lateupdate::candidates(120_000);
            let ga = crate::procs::own_module(crate::GAME_ASSEMBLY);
            let mut out = vec![format!(
                "LateUpdate candidates: {} (materialised classes only)",
                found.len()
            )];
            for candidate in found.iter().take(128) {
                let index = candidate
                    .index
                    .map(|i| format!("#{i}"))
                    .unwrap_or_else(|| "-".into());
                let rva = ga
                    .map(|(base, _)| candidate.entry.saturating_sub(base))
                    .unwrap_or(0);
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
                return Some(vec![
                    "usage: latehook auto | #<type-index> | <class-address>".into(),
                ]);
            };
            let result = log::guard("latehook", || install_late_update(selector))
                .unwrap_or_else(|| Err("installer panicked (see PANIC line)".into()));
            Some(vec![match result {
                Ok(candidate) => format!(
                    "LateUpdate hook installed at {:#x}; use `late` for hits/tid",
                    candidate.entry
                ),
                Err(error) => format!("latehook: {error}"),
            }])
        }
        "late" => Some(late_update_status()),
        "unhook" => {
            if !crate::raw::gcguard::before_unhook() {
                return Some(vec!["GC guard is busy; unhook refused, retry later".into()]);
            }
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
            crate::raw::gcguard::after_unhook(&installed);
            LATE_ATTACHED.store(
                late_attachment_present(LATE_ENTRY.load(Ordering::Acquire), &installed),
                Ordering::Release,
            );
            crate::playerloop::refresh_attachment(&installed);
            Some(vec![format!(
                "{n} hook(s) detached ({} still listed)",
                g.len()
            )])
        }
        "hooks" => {
            let g = match HOOKS.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if g.is_empty() {
                return Some(vec![
                    "no hooks installed (try `latehook auto`, or `hookat <addr> [n]`)".into(),
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
            let rate = if up > 0 {
                n as f64 * 1000.0 / up as f64
            } else {
                0.0
            };
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
    use crate::raw::gcguard::{
        GcRecordAction, can_recover_boehm_abort, gc_record_action, valid_gc_suspend_entry,
    };

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
    fn aligned_patch_window_does_not_move_target_bytes_left() {
        #[repr(align(16))]
        struct Window([u8; 32]);

        let mut window = Window([0x90; 32]);
        let original_prefix = [
            0x10, 0x00, 0xff, 0x15, 0x60, 0x78, 0xac, 0x0a, 0x85, 0xc0, 0x0f, 0x85,
        ];
        window.0[..original_prefix.len()].copy_from_slice(&original_prefix);
        let base = window.0.as_mut_ptr() as usize;
        let original = window.0;

        for o16 in 0..=11 {
            let mut expected = original;
            expected[o16] = 0xe9;
            expected[o16 + 1..o16 + 5].copy_from_slice(&[0x29, 0xe6, 0x3a, 0xff]);
            let wmode = write_mode(o16).expect("aligned target's patch mode");
            // SAFETY: target lies in window; write_mode chooses a complete
            // aligned word/quadword within this live 16-byte-aligned buffer.
            assert!(unsafe { write_window(base, base + o16, &expected, wmode) });
            assert_eq!(window.0, expected, "target alignment {o16:#x}");
            // SAFETY: same live buffer, target and per-alignment write mode:
            // restore the original 32-byte snapshot into its owned window.
            assert!(unsafe { write_window(base, base + o16, &original, wmode) });
            assert_eq!(window.0, original, "restore alignment {o16:#x}");
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
        assert!(late_attachment_present(
            entry,
            &[(entry, true), (entry, false)]
        ));
        assert!(!late_attachment_present(entry, &[(entry, true)]));
        assert!(!late_attachment_present(entry, &[(0x2000, false)]));
    }

    #[test]
    fn late_update_lifecycle_distinguishes_attached_from_retained_trampoline() {
        use LateInstallAction::{AlreadyAttached, Patch};
        assert_eq!(late_install_action(0, false, 0x1000), Ok(Patch));
        assert_eq!(
            late_install_action(0x1000, true, 0x1000),
            Ok(AlreadyAttached)
        );
        assert_eq!(late_install_action(0x1000, false, 0x1000), Ok(Patch));
        assert!(late_install_action(0x1000, true, 0x2000).is_err());
        assert!(late_install_action(0x1000, false, 0x2000).is_err());
    }

    #[test]
    fn late_update_rearm_clears_hits_and_thread_identity() {
        LATE_HITS.store(7, Ordering::Relaxed);
        LATE_TID.store(0x1234, Ordering::Relaxed);
        reset_late_observation();
        assert_eq!(LATE_HITS.load(Ordering::Relaxed), 0);
        assert_eq!(LATE_TID.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn late_update_tick_runs_before_the_original_method() {
        let events = std::cell::RefCell::new(Vec::new());
        let returned = tick_then_original(
            false,
            || {
                events.borrow_mut().push("tick");
                Ok::<_, ()>(())
            },
            || {
                events.borrow_mut().push("original");
                42
            },
        );
        assert_eq!(returned, 42);
        assert_eq!(events.into_inner(), ["tick", "original"]);
    }

    #[test]
    fn reload_phase_runs_the_original_before_instrumentation() {
        let events = std::cell::RefCell::new(Vec::new());
        let returned = tick_then_original(
            true,
            || {
                events.borrow_mut().push("instrumentation");
                Err::<(), _>("feature delay")
            },
            || {
                events.borrow_mut().push("original");
                64usize
            },
        );
        assert_eq!(returned, 64);
        assert_eq!(events.into_inner(), ["original", "instrumentation"]);
    }

    #[test]
    fn late_update_original_survives_a_guarded_tick_failure() {
        let events = std::cell::RefCell::new(Vec::new());
        let returned = tick_then_original(
            false,
            || {
                events.borrow_mut().push("guard failed");
                Err::<(), _>("instrumentation fault")
            },
            || {
                events.borrow_mut().push("original");
                42
            },
        );
        assert_eq!(returned, 42);
        assert_eq!(events.into_inner(), ["guard failed", "original"]);
    }

    #[test]
    fn late_update_entry_is_limited_to_game_assembly() {
        let module = (0x6fff_0000, 0x0f44_0000);
        assert!(managed_entry_in_module(0x6fff_1000, Some(module)));
        assert!(managed_entry_in_module(
            0x6fff_0000 + 0x0f44_0000 - 1,
            Some(module)
        ));
        assert!(!managed_entry_in_module(
            0x6fff_0000 + 0x0f44_0000,
            Some(module)
        ));
        assert!(!managed_entry_in_module(0x1000, Some(module)));
        assert!(!managed_entry_in_module(0x6fff_1000, None));
    }

    #[test]
    fn gc_context_call_site_requires_the_measured_boehm_signature() {
        const CALL_AND_TEST: [u8; 8] = [0xff, 0x15, 0x60, 0x78, 0xac, 0x0a, 0x85, 0xc0];
        const CONTINUATION: [u8; 6] = [0x0f, 0x85, 0x8b, 0x00, 0x00, 0x00];
        let mut loaded = [0u8; 14];
        loaded[..8].copy_from_slice(&CALL_AND_TEST);
        loaded[8..].copy_from_slice(&CONTINUATION);

        assert!(valid_gc_context_call_site(&loaded));
        assert_eq!(prologue_len(&loaded, 14), Some(14));
        // The displacement belongs to the build; only the opcode and the
        // test/jne shape must hold.
        for index in [0usize, 1, 6, 7] {
            let mut changed = loaded;
            changed[index] ^= 1;
            assert!(
                !valid_gc_context_call_site(&changed),
                "byte {index} belongs to the version gate"
            );
        }
        assert!(!valid_gc_context_call_site(&loaded[..7]));
    }

    #[test]
    fn synthetic_dead_context_matches_win64_and_preserves_requested_flags() {
        const CONTEXT_CONTROL: u32 = 0x0010_0001;
        const CONTEXT_INTEGER: u32 = 0x0010_0002;
        const FLAGS: u32 = CONTEXT_CONTROL | CONTEXT_INTEGER;
        let mut storage = [0xa5u8; WIN64_CONTEXT_SIZE + 2];
        storage[1 + WIN64_CONTEXT_FLAGS..1 + WIN64_CONTEXT_FLAGS + 4]
            .copy_from_slice(&FLAGS.to_le_bytes());
        // SAFETY: storage keeps one extra byte on each side of the WINDOW.
        let context_ptr = unsafe { storage.as_mut_ptr().add(1) };
        // SAFETY: storage owns exactly one complete writable Win64 CONTEXT,
        // deliberately offset by one byte to exercise unaligned raw accesses.
        unsafe { write_synthetic_dead_context_raw(context_ptr) };

        assert_eq!(storage[0], 0xa5, "left canary");
        assert_eq!(storage[WIN64_CONTEXT_SIZE + 1], 0xa5, "right canary");
        // SAFETY: the writer touched only this exact live 0x4d0-byte extent.
        let context = unsafe { core::slice::from_raw_parts(context_ptr, WIN64_CONTEXT_SIZE) };
        assert_eq!(context.len(), 0x4d0, "Win64 CONTEXT is 1232 bytes");
        assert_eq!(
            u32::from_le_bytes(context[0x30..0x34].try_into().unwrap()),
            FLAGS,
            "ContextFlags is an input selecting the registers to retrieve"
        );
        assert!(context[..0x30].iter().all(|b| *b == 0));
        assert!(context[0x34..].iter().all(|b| *b == 0));
    }

    #[test]
    fn inline_repairs_are_deferred_and_coalesced_for_the_tick_logger() {
        INLINE_REPAIRS.store(0, Ordering::Relaxed);
        INLINE_LAST_TID.store(0, Ordering::Relaxed);

        note_inline_repair(0x1234);
        note_inline_repair(0x5678);
        assert_eq!(take_inline_repairs(), Some((2, 0x5678)));
        assert_eq!(take_inline_repairs(), None);
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct TestGcRecord {
        next: usize,
        id: u32,
        pad: u32,
        handle: usize,
    }

    impl TestGcRecord {
        fn new(id: u32, handle: usize) -> Self {
            Self {
                next: 0,
                id,
                pad: 0,
                handle,
            }
        }
    }

    #[test]
    fn raw_gc_unlink_removes_a_nested_record_without_disturbing_its_neighbours() {
        let mut table = [0usize; 256];
        let mut records = [
            TestGcRecord::new(0x111, 0x1010),
            TestGcRecord::new(0x222, 0x2020),
            TestGcRecord::new(0x333, 0x3030),
        ];
        let third = &mut records[2] as *mut TestGcRecord as usize;
        let second = &mut records[1] as *mut TestGcRecord as usize;
        records[0].next = second;
        records[1].next = third;
        table[37] = &mut records[0] as *mut TestGcRecord as usize;

        // SAFETY: table and all linked records are exclusively owned fixtures
        // with the measured ABI layout; every pointer remains live and writable.
        let removed = unsafe { unlink_by_handle_raw(table.as_mut_ptr() as usize, 0x2020) };

        assert_eq!(removed, Some(0x222));
        assert_eq!(records[0].next, third);
        assert_eq!(
            records[1].next, third,
            "the removed record's links stay inert"
        );
        assert_eq!(table[37], &mut records[0] as *mut TestGcRecord as usize);
    }

    #[test]
    fn raw_gc_unlink_is_bounded_when_a_bucket_contains_a_cycle() {
        let mut table = [0usize; 256];
        let mut record = TestGcRecord::new(0x111, 0x1010);
        let ptr = &mut record as *mut TestGcRecord as usize;
        record.next = ptr;
        table[0] = ptr;

        // SAFETY: the cycle is intentional and lives entirely in this fixture;
        // the helper must stop at its fixed per-bucket bound instead of hanging.
        let removed = unsafe { unlink_by_handle_raw(table.as_mut_ptr() as usize, 0x9990) };
        assert_eq!(removed, None);
        assert_eq!(record.next, ptr);
        assert_eq!(table[0], ptr);
    }

    #[test]
    fn dead_gc_call_succeeds_even_when_the_record_is_already_absent() {
        let mut table = [0usize; 256];
        let mut context = [0xa5u8; WIN64_CONTEXT_SIZE];
        context[WIN64_CONTEXT_FLAGS..WIN64_CONTEXT_FLAGS + 4]
            .copy_from_slice(&0x0010_0003u32.to_le_bytes());

        // SAFETY: this is exactly the hot-path completion after a signalled
        // handle was observed. The empty fixture models a concurrent detach:
        // success is mandatory; absence must not re-enter the original retry.
        let (result, removed) = unsafe {
            complete_dead_gc_call_raw(table.as_mut_ptr() as usize, 0x2020, context.as_mut_ptr())
        };

        assert_eq!(result, 1);
        assert_eq!(removed, None);
        assert_eq!(
            u32::from_le_bytes(context[0x30..0x34].try_into().unwrap()),
            0x0010_0003
        );
        assert!(context[0x34..].iter().all(|b| *b == 0));
    }

    #[test]
    fn gc_record_action_unlinks_only_on_the_original_signalled_handle() {
        assert_eq!(gc_record_action(0), GcRecordAction::Unlink);
        assert_eq!(gc_record_action(0x102), GcRecordAction::Keep);
        assert_eq!(gc_record_action(u32::MAX), GcRecordAction::Keep);
        assert_eq!(gc_record_action(0x80), GcRecordAction::Keep);
    }

    #[test]
    fn entry_signature_matches_the_measured_boehm_prologue() {
        assert!(valid_gc_suspend_entry(&[
            0x48, 0x89, 0x5c, 0x24, 0x08, 0x48, 0x89, 0x6c, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24,
            0x18,
        ]));
    }

    #[test]
    fn recovery_access_and_activation_are_explicit() {
        assert!(!can_recover_boehm_abort(false, 0x123, 0x123));
        assert!(!can_recover_boehm_abort(true, 0x123, 0x456));
        assert!(!can_recover_boehm_abort(true, 0, 0x123));
        assert!(can_recover_boehm_abort(true, 0x123, 0x123));
    }
}
