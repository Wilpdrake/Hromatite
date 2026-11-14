//! Guarded reads of *our own* address space.
//!
//! `catch_unwind` does not catch an access violation, so every raw pointer read
//! in the payload has to be proven readable *first* — a bad read inside the
//! stub thread takes the whole game down with "Fatal error in GC".
//!
//! Rules of the house:
//!   * a helper that returns `Option` means "the address was unreadable";
//!   * `readable_at` is a `VirtualQuery` call, so do not put it in a per-word
//!     loop — validate the region once (`Span`) and walk it;
//!   * a `PAGE_GUARD` page faults on first touch even though it is committed,
//!     so it is treated as unreadable.
//!
//! Containers here are deliberately allocation-light: the hot paths (heap scan)
//! call only `read_u64`/`read_bytes`.

use crate::ffi::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery};

/// Give the game's GC a window: `SuspendThread` fails ("Fatal error in GC") when
/// our injected thread is busy inside a long memory scan, and the game freezes.
/// Every full-memory scan in the console calls this once per region.
pub fn scan_yield() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static MS: AtomicU64 = AtomicU64::new(u64::MAX);
    let mut ms = MS.load(Ordering::Relaxed);
    if ms == u64::MAX {
        ms = std::env::var("HROMATITE_SCAN_PAUSE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(5);
        MS.store(ms, Ordering::Relaxed);
    }
    if ms > 0 {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// One committed, readable, guard-free region as reported by `VirtualQuery`.
#[derive(Clone, Copy, Debug)]
pub struct Span {
    pub base: usize,
    pub size: usize,
    pub protect: u32,
}

impl Span {
    pub fn end(&self) -> usize {
        self.base.saturating_add(self.size)
    }
    pub fn contains(&self, addr: usize, len: usize) -> bool {
        addr >= self.base && addr.saturating_add(len) <= self.end()
    }
}

/// The readable region that contains `addr`, or `None` if it is free, reserved,
/// `PAGE_NOACCESS` or guarded.
pub fn span_at(addr: usize) -> Option<Span> {
    if addr == 0 {
        return None;
    }
    unsafe {
        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        let n = VirtualQuery(
            addr as *const core::ffi::c_void,
            &mut mbi,
            core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        );
        if n == 0 || mbi.State != MEM_COMMIT || mbi.RegionSize == 0 {
            return None;
        }
        let p = mbi.Protect & 0xFF;
        if p == PAGE_NOACCESS || p == 0 || (mbi.Protect & PAGE_GUARD) != 0 {
            return None;
        }
        Some(Span {
            base: mbi.BaseAddress,
            size: mbi.RegionSize,
            protect: mbi.Protect,
        })
    }
}

/// Is the single byte/word at `addr` safe to touch?
pub fn readable_at(addr: usize) -> bool {
    span_at(addr).is_some()
}

/// Is every byte of `addr..addr+len` readable? Walks region boundaries, so a
/// buffer that straddles two committed regions still passes.
pub fn readable_range(addr: usize, len: usize) -> bool {
    if len == 0 {
        return true;
    }
    if addr == 0 {
        return false;
    }
    let end = match addr.checked_add(len) {
        Some(e) => e,
        None => return false,
    };
    let mut p = addr;
    let mut hops = 0usize;
    while p < end {
        match span_at(p) {
            Some(s) => {
                let s_end = s.end();
                if s_end <= p {
                    return false; // no forward progress: bogus region
                }
                p = s_end;
            }
            None => return false,
        }
        hops += 1;
        if hops > 4096 {
            return false;
        }
    }
    true
}

/// Copy `buf.len()` bytes from `addr` if the whole range is readable.
pub fn read_copy(addr: usize, buf: &mut [u8]) -> bool {
    if !readable_range(addr, buf.len()) {
        return false;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(addr as *const u8, buf.as_mut_ptr(), buf.len());
    }
    true
}

/// Copy as much of `addr..addr+len` as is contiguous-readable. Partial reads are
/// what a human wants from `b <addr> n`; machine paths should use `read_bytes`.
pub fn read_upto(addr: usize, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len.min(64 * 1024));
    if len == 0 || addr == 0 {
        return out;
    }
    let end = addr.saturating_add(len);
    let mut p = addr;
    let mut cur = span_at(p);
    while p < end {
        let s = match cur {
            Some(s) if s.contains(p, 1) => s,
            _ => match span_at(p) {
                Some(s) => s,
                None => break,
            },
        };
        cur = Some(s);
        let stop = end.min(s.end());
        let n = stop - p;
        if n == 0 {
            break;
        }
        out.extend_from_slice(unsafe { core::slice::from_raw_parts(p as *const u8, n) });
        p = stop;
    }
    out
}

/// All-or-nothing read.
pub fn read_bytes(addr: usize, len: usize) -> Option<Vec<u8>> {
    let v = read_upto(addr, len);
    (v.len() == len).then_some(v)
}

pub fn read_u64(addr: usize) -> Option<u64> {
    let mut b = [0u8; 8];
    read_copy(addr, &mut b).then(|| u64::from_le_bytes(b))
}

pub fn read_u32(addr: usize) -> Option<u32> {
    let mut b = [0u8; 4];
    read_copy(addr, &mut b).then(|| u32::from_le_bytes(b))
}

pub fn read_u16(addr: usize) -> Option<u16> {
    let mut b = [0u8; 2];
    read_copy(addr, &mut b).then(|| u16::from_le_bytes(b))
}

pub fn read_u8(addr: usize) -> Option<u8> {
    let mut b = [0u8; 1];
    read_copy(addr, &mut b).then(|| b[0])
}

pub fn read_usize(addr: usize) -> Option<usize> {
    read_u64(addr).map(|v| v as usize)
}

/// Write one word back (PEB unlink). Checks readability first; the loader's
/// list lives in `RW` memory, so readability is the only failure worth catching.
pub fn write_usize(addr: usize, v: usize) -> bool {
    if !readable_range(addr, core::mem::size_of::<usize>()) {
        return false;
    }
    unsafe {
        core::ptr::write_unaligned(addr as *mut usize, v);
    }
    true
}

fn span_of(cur: &mut Option<Span>, p: usize) -> Option<Span> {
    if let Some(s) = *cur {
        if s.contains(p, 1) {
            return Some(s);
        }
    }
    let s = span_at(p)?;
    *cur = Some(s);
    Some(s)
}

/// Strict NUL-terminated printable ASCII at `p`, or `None`. Used to validate
/// names found by scanning unknown structures: one non-printable byte and the
/// candidate is rejected, not truncated.
pub fn ascii_at(p: usize, max: usize) -> Option<String> {
    if p < 0x10000 {
        return None;
    }
    let mut s = String::with_capacity(max.min(64));
    let mut cur = None;
    for i in 0..max {
        let a = p.checked_add(i)?;
        span_of(&mut cur, a)?;
        let b = unsafe { *(a as *const u8) };
        if b == 0 {
            return Some(s);
        }
        if !(32..127).contains(&b) {
            return None;
        }
        s.push(b as char);
    }
    Some(s)
}

/// Display-oriented ASCII: stops at NUL / unreadable, non-printables become '.'.
pub fn ascii_lossy(p: usize, max: usize) -> String {
    if p < 0x10000 {
        return String::new();
    }
    let mut s = String::with_capacity(max.min(64));
    let mut cur = None;
    for i in 0..max {
        let a = match p.checked_add(i) {
            Some(a) => a,
            None => break,
        };
        if span_of(&mut cur, a).is_none() {
            break;
        }
        let b = unsafe { *(a as *const u8) };
        if b == 0 {
            break;
        }
        s.push(if (32..127).contains(&b) { b as char } else { '.' });
    }
    s
}

/// Display-oriented UTF-16 (console `ws`), same partial-read semantics.
pub fn utf16_lossy(p: usize, max: usize) -> String {
    let mut units = Vec::with_capacity(max.min(128));
    for i in 0..max {
        let a = match i.checked_mul(2).and_then(|o| p.checked_add(o)) {
            Some(a) => a,
            None => break,
        };
        let c = match read_u16(a) {
            Some(c) => c,
            None => break,
        };
        if c == 0 {
            break;
        }
        units.push(c);
    }
    String::from_utf16_lossy(&units)
}

/// Offsets (relative to `hay`) of every 8-byte word equal to `want`, capped.
/// The caller passes a validated `Span`, so the loop itself does no per-word
/// `VirtualQuery`.
pub fn find_qword_in_span(span: &Span, want: u64, max: usize, out: &mut Vec<usize>) {
    let mut a = (span.base + 7) & !7usize;
    let end = span.end().saturating_sub(8);
    while a <= end && out.len() < max {
        if unsafe { *(a as *const u64) } == want {
            out.push(a);
        }
        a += 8;
    }
}
