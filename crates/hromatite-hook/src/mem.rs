//! OS memory allocation, protection changes, instruction-cache flushing, and
//! atomic 8-byte / 16-byte (`lock cmpxchg16b`) code-window patching.

use hromatite_win::{ffi, log};

use crate::tramp::rel32_ok;

pub const PAGE_SIZE: usize = 0x1000;
/// Stay well inside the ±2 GiB reach of a `jmp rel32`.
pub const REL32_SPAN: usize = 0x7F00_0000;
/// ±512 MiB span used by the 12-byte absolute hook engine.
pub const NEAR_SPAN: usize = 0x2000_0000;

/// Select the atomic store width for a 5-byte patch at `offset_in_16` (`target & 15`).
pub fn write_mode(offset_in_16: usize) -> Option<u8> {
    match offset_in_16 {
        0..=3 => Some(8),
        4..=11 => Some(16),
        _ => None, // the two-byte non-atomic fallback is forbidden
    }
}

/// Nearest `MEM_FREE` region within `max_span` of `target` (and within `rel32`
/// reach), via `VirtualQuery`.
fn nearest_free_near(target: usize, size: usize, max_span: usize) -> Option<usize> {
    let lo = target.saturating_sub(max_span).max(0x10000);
    let hi = target.saturating_add(max_span);
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
        if mbi.State == ffi::MEM_FREE && mbi.RegionSize >= size {
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

/// Brute-force fallback: try `target ± k*64K` outward within `max_span`.
fn enum_near(target: usize, size: usize, max_span: usize, protect: u32) -> Option<usize> {
    let step = 0x10000usize;
    let max_k = (max_span / step).clamp(1, 4096);
    let mut logged = 0u32;
    let mut tries = 0u32;
    for k in 1..=max_k {
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
                    protect,
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
    alloc_near_within(target, size, REL32_SPAN, false, ffi::PAGE_READWRITE)
}

/// Allocate `size` bytes within `max_span` of `target` with `protect`, optionally
/// falling back to an unconstrained `VirtualAlloc(0, ...)` if no near page is free.
pub fn alloc_near_within(
    target: usize,
    size: usize,
    max_span: usize,
    allow_anywhere: bool,
    protect: u32,
) -> Option<usize> {
    let size = (size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    if let Some(c) = nearest_free_near(target, size, max_span) {
        // SAFETY: `c` was computed from a free region large enough for `size`.
        let p = unsafe {
            ffi::VirtualAlloc(
                c,
                size,
                ffi::MEM_RESERVE | ffi::MEM_COMMIT,
                protect,
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
    if let Some(got) = enum_near(target, size, max_span, protect) {
        log::log_tag(
            "hook",
            &format!("alloc_near: enum fallback page {got:#x} (target {target:#x})"),
        );
        return Some(got);
    }
    if allow_anywhere {
        // SAFETY: unconstrained OS allocation of `size` bytes.
        let p = unsafe {
            ffi::VirtualAlloc(
                0,
                size,
                ffi::MEM_RESERVE | ffi::MEM_COMMIT,
                protect,
            )
        };
        if !p.is_null() {
            return Some(p as usize);
        }
    }
    None
}

/// Replace the 16-byte aligned block at `base16` atomically with `bytes` using
/// `lock cmpxchg16b`.
///
/// # Safety
/// `base16` must be 16-byte aligned and point to a readable and writable 16-byte
/// memory block.
pub unsafe fn cas16(base16: usize, bytes: &[u8; 16]) -> bool {
    let mut lo_src = [0u8; 8];
    let mut hi_src = [0u8; 8];
    lo_src.copy_from_slice(&bytes[..8]);
    hi_src.copy_from_slice(&bytes[8..]);
    let new_lo = u64::from_le_bytes(lo_src);
    let new_hi = u64::from_le_bytes(hi_src);
    for _ in 0..16 {
        // SAFETY: aligned 16-byte read of the writable patch window.
        let (old_lo, old_hi) = unsafe {
            (
                core::ptr::read_volatile(base16 as *const u64),
                core::ptr::read_volatile((base16 + 8) as *const u64),
            )
        };
        let mut rax = old_lo;
        let mut rdx = old_hi;
        // `rbx` is reserved by LLVM, so it is saved/restored inside the asm
        // block instead of being declared as an operand.
        // SAFETY: `base16` is 16-byte aligned and writable; rax:rdx holds the
        // expected value, rcx:rbx the replacement; rsp is restored by `pop`.
        unsafe {
            core::arch::asm!(
                "push rbx",
                "mov rbx, {lo}",
                "lock cmpxchg16b [{ptr}]",
                "pop rbx",
                ptr = in(reg) base16,
                lo = in(reg) new_lo,
                in("rcx") new_hi,
                inout("rax") rax,
                inout("rdx") rdx,
                options(),
            );
        }
        if rax == old_lo && rdx == old_hi {
            return true;
        }
    }
    false
}

/// Write `new32` into the 16-byte window at `base16` atomically according to `wmode`.
///
/// # Safety
/// `base16` must be 16-byte aligned and point to a readable and writable 16-byte
/// (for `wmode == 16`) or 8-byte (for `wmode == 8`) target window covering `target`.
pub unsafe fn write_window(base16: usize, target: usize, new32: &[u8; 32], wmode: u8) -> bool {
    let o16 = target - base16;
    match wmode {
        8 => {
            let base = if o16 >= 8 { base16 + 8 } else { base16 };
            let off = base - base16;
            let mut w = [0u8; 8];
            w.copy_from_slice(&new32[off..off + 8]);
            // SAFETY: base16 is readable/writable target code and wmode 8 proves
            // the full aligned qword is inside the original 32-byte snapshot window.
            unsafe { core::ptr::write_volatile(base as *mut u64, u64::from_le_bytes(w)) };
            true
        }
        16 => {
            let mut window = [0u8; 16];
            window.copy_from_slice(&new32[..16]);
            // SAFETY: base16 is 16-byte aligned and writable.
            unsafe { cas16(base16, &window) }
        }
        _ => false,
    }
}

/// Change page protection on the pages covering `addr..addr+len`, returning the
/// previous protection.
pub fn set_range_protect(addr: usize, len: usize, prot: u32) -> Option<u32> {
    let page = addr & !(PAGE_SIZE - 1);
    let end = (addr + len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
    let mut old = 0u32;
    // SAFETY: `old` is a valid output pointer and page..end is computed within
    // the caller's already allocated target range.
    let ok = unsafe { ffi::VirtualProtect(page as *mut u8, end - page, prot, &mut old) };
    (ok != 0).then_some(old)
}

/// Flush the CPU instruction cache over `addr..addr+len`.
pub fn flush(addr: usize, len: usize) {
    // SAFETY: the pseudo-process handle is always valid; addr/len describe the
    // generated code range supplied by the caller.
    unsafe {
        ffi::FlushInstructionCache(ffi::GetCurrentProcess(), addr as *const u8, len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
