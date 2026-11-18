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

#[cfg(target_os = "windows")]
use crate::ffi::{GetCurrentProcess, ReadProcessMemory, VirtualQuery, WriteProcessMemory};
use crate::ffi::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS};

// Linux unit tests link pure-Rust modules without Win32. They exercise only
// owned snapshots, so unsupported calls deterministically fail closed.
#[cfg(not(target_os = "windows"))]
#[unsafe(no_mangle)]
/// # Safety
/// Unsupported host stub: pointers are accepted only under the same optional
/// output-buffer contract as the Win32 declaration; no guest memory is accessed.
pub unsafe extern "system" fn VirtualQuery(
    _addr: *const core::ffi::c_void,
    _mbi: *mut MEMORY_BASIC_INFORMATION,
    _len: usize,
) -> usize {
    0
}

#[cfg(not(target_os = "windows"))]
#[unsafe(no_mangle)]
/// # Safety
/// Unsupported host stub: pointers are accepted only under the same optional
/// output-buffer contract as the Win32 declaration; no guest memory is accessed.
pub unsafe extern "system" fn GetCurrentProcess() -> crate::ffi::HANDLE {
    core::ptr::null_mut()
}

#[cfg(not(target_os = "windows"))]
#[unsafe(no_mangle)]
/// # Safety
/// Unsupported host stub: pointers are accepted only under the same optional
/// output-buffer contract as the Win32 declaration; no guest memory is accessed.
pub unsafe extern "system" fn ReadProcessMemory(
    _process: crate::ffi::HANDLE,
    _addr: *const u8,
    _buf: *mut u8,
    _size: usize,
    copied: *mut usize,
) -> i32 {
    if !copied.is_null() {
        // SAFETY: the caller supplies an optional count output pointer just as
        // Win32 does; null is explicitly accepted and left untouched.
        unsafe { *copied = 0 };
    }
    0
}

#[cfg(not(target_os = "windows"))]
#[unsafe(no_mangle)]
/// # Safety
/// Unsupported host stub: pointers are accepted only under the same optional
/// output-buffer contract as the Win32 declaration; no guest memory is accessed.
pub unsafe extern "system" fn WriteProcessMemory(
    _process: crate::ffi::HANDLE,
    _addr: *mut u8,
    _buf: *const u8,
    _size: usize,
    written: *mut usize,
) -> i32 {
    if !written.is_null() {
        // SAFETY: same optional output-pointer contract as ReadProcessMemory.
        unsafe { *written = 0 };
    }
    0
}

#[cfg(not(target_os = "windows"))]
#[unsafe(no_mangle)]
/// # Safety
/// Unsupported host stub: the arguments follow the Win32 allocation contract
/// but no guest page is reserved; a null result makes every caller fail closed.
pub unsafe extern "system" fn VirtualAlloc(
    _addr: usize,
    _size: usize,
    _ty: u32,
    _protect: u32,
) -> *mut u8 {
    core::ptr::null_mut()
}

#[cfg(not(target_os = "windows"))]
#[unsafe(no_mangle)]
/// # Safety
/// Unsupported host stub: `old` is the same optional output pointer as in the
/// Win32 declaration and is left untouched; protection changes are refused.
pub unsafe extern "system" fn VirtualProtect(
    _addr: *mut u8,
    _size: usize,
    _newp: u32,
    _old: *mut u32,
) -> i32 {
    0
}

#[cfg(not(target_os = "windows"))]
#[unsafe(no_mangle)]
/// # Safety
/// Unsupported host stub: the handle and range use the Win32 contract but no
/// cache flush is performed; the host needs no instruction-cache coherence.
pub unsafe extern "system" fn FlushInstructionCache(
    _h: crate::ffi::HANDLE,
    _base: *const u8,
    _size: usize,
) -> i32 {
    0
}

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

/// One queried virtual memory region returned by [`query_region`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryRegion {
    pub base: usize,
    pub size: usize,
    pub state: u32,
    pub protect: u32,
    pub ty: u32,
}

impl MemoryRegion {
    pub fn end(&self) -> usize {
        self.base.saturating_add(self.size)
    }

    pub fn is_committed(&self) -> bool {
        self.state == MEM_COMMIT
    }

    pub fn is_readable(&self) -> bool {
        let p = self.protect & 0xFF;
        self.state == MEM_COMMIT
            && p != PAGE_NOACCESS
            && p != 0
            && (self.protect & PAGE_GUARD) == 0
    }
}

/// Query the virtual memory region containing `addr` via `VirtualQuery`.
pub fn query_region(addr: usize) -> Option<MemoryRegion> {
    let mut mbi = MEMORY_BASIC_INFORMATION::default();
    // SAFETY: `mbi` is a live output buffer of the exact size passed to
    // VirtualQuery; `addr` is only inspected as an opaque address.
    let n = unsafe {
        VirtualQuery(
            addr as *const core::ffi::c_void,
            &mut mbi,
            core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if n == 0 || mbi.RegionSize == 0 {
        return None;
    }
    Some(MemoryRegion {
        base: mbi.BaseAddress,
        size: mbi.RegionSize,
        state: mbi.State,
        protect: mbi.Protect,
        ty: mbi.Type,
    })
}

/// A numeric region description for validated bounds checks.
#[derive(Clone, Debug)]
pub struct Span {
    base: usize,
    size: usize,
    protect: u32,
}

impl Span {
    /// Construct a region range without dereferencing any address.
    pub fn from_parts(base: usize, size: usize, protect: u32) -> Self {
        Self {
            base,
            size,
            protect,
        }
    }

    pub fn base(&self) -> usize {
        self.base
    }

    pub fn protect(&self) -> u32 {
        self.protect
    }

    pub fn end(&self) -> usize {
        self.base.saturating_add(self.size)
    }

    pub fn contains(&self, addr: usize, len: usize) -> bool {
        addr >= self.base && addr.checked_add(len).is_some_and(|end| end <= self.end())
    }

    pub fn writable(&self) -> bool {
        matches!(
            self.protect & 0xFF,
            crate::ffi::PAGE_READWRITE
                | crate::ffi::PAGE_WRITECOPY
                | crate::ffi::PAGE_EXECUTE_READWRITE
                | crate::ffi::PAGE_EXECUTE_WRITECOPY
        ) && self.protect & crate::ffi::PAGE_GUARD == 0
    }
}

/// Read the fixed-size bootstrap record passed by the injector stub.
pub fn read_words<const N: usize>(addr: usize) -> Option<[usize; N]> {
    let mut out = [0usize; N];
    for (i, word) in out.iter_mut().enumerate() {
        *word = read_usize(addr.checked_add(i.checked_mul(8)?)?)?;
    }
    Some(out)
}

/// Deliberate raw read used by the `av` diagnostic. The VEH makes an access
/// violation an ordinary `Err` instead of terminating the target process.
///
/// # Safety
/// `addr..addr+8` must be valid for unaligned reads. Any other address is
/// outside the contract: this diagnostic deliberately exercises that violation
/// under VEH, whose OS-level recovery does not promise to reverse arbitrary UB.
pub unsafe fn diagnostic_raw_read(addr: usize) -> Result<u64, crate::veh::Fault> {
    crate::veh::call_with(|| {
        // SAFETY: caller deliberately supplies the probe address. It is read
        // unaligned so every mapped address is representation-compatible; VEH
        // captures this exact thread before the operation as the diagnostic requires.
        unsafe { core::ptr::read_unaligned(addr as *const u64) }
    })
}

/// The readable region that contains `addr`, or `None` if it is free, reserved,
/// `PAGE_NOACCESS` or guarded.
pub fn span_at(addr: usize) -> Option<Span> {
    if addr == 0 {
        return None;
    }
    let region = query_region(addr)?;
    if !region.is_readable() {
        return None;
    }
    Some(Span::from_parts(region.base, region.size, region.protect))
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

pub fn writable_range(addr: usize, len: usize) -> bool {
    if len == 0 {
        return false;
    }
    let Some(end) = addr.checked_add(len) else {
        return false;
    };
    let mut p = addr;
    while p < end {
        let Some(span) = span_at(p) else {
            return false;
        };
        if !span.writable() {
            return false;
        }
        let next = span.end();
        if next <= p {
            return false;
        }
        p = next.min(end);
    }
    true
}

/// Copy `buf.len()` bytes from `addr` if the whole range is readable.
pub fn read_copy(addr: usize, buf: &mut [u8]) -> bool {
    if buf.is_empty() || !readable_range(addr, buf.len()) {
        return false;
    }
    let mut copied = 0usize;
    // SAFETY: buf is a valid writable output slice for the synchronous OS copy.
    // The OS accesses the arbitrary source address and reports failures instead
    // of letting Rust dereference it; copied is a valid count out-pointer.
    let ok = unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            addr as *const u8,
            buf.as_mut_ptr(),
            buf.len(),
            &mut copied,
        )
    };
    ok != 0 && copied == buf.len()
}

/// Copy as many bytes from `addr` into `buf` as the OS can transfer in one
/// `ReadProcessMemory` call, returning the number of bytes actually copied
/// (even on a partial copy across an unmapped hole where Win32 returns 0).
pub fn read_partial(addr: usize, buf: &mut [u8]) -> usize {
    if buf.is_empty() || addr == 0 {
        return 0;
    }
    let mut copied = 0usize;
    // SAFETY: `buf` is a live writable slice of `buf.len()` bytes and `copied`
    // is a valid output pointer; the OS copies out of the current process
    // without dereferencing `addr` in Rust.
    let _ok = unsafe {
        ReadProcessMemory(
            GetCurrentProcess(),
            addr as *const u8,
            buf.as_mut_ptr(),
            buf.len(),
            &mut copied,
        )
    };
    copied.min(buf.len())
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
        let region_end = if cur.as_ref().is_some_and(|s| s.contains(p, 1)) {
            cur.as_ref().map(Span::end)
        } else {
            let next = span_at(p);
            let end = next.as_ref().map(Span::end);
            cur = next;
            end
        };
        let Some(region_end) = region_end else {
            break;
        };
        let stop = end.min(region_end);
        let n = stop - p;
        if n == 0 {
            break;
        }
        let mut chunk = vec![0u8; n];
        if !read_copy(p, &mut chunk) {
            break;
        }
        out.extend_from_slice(&chunk);
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

/// Write one word back (PEB unlink). Every crossed region must be writable;
/// readable-but-RX pages are rejected instead of raising an access violation.
pub fn write_usize(addr: usize, v: usize) -> bool {
    if !writable_range(addr, core::mem::size_of::<usize>()) {
        return false;
    }
    write_bytes(addr, &v.to_le_bytes())
}

/// Write a byte slice (console `wb`) after checking every crossed region.
pub fn write_bytes(addr: usize, data: &[u8]) -> bool {
    if data.is_empty() || !writable_range(addr, data.len()) {
        return false;
    }
    let mut written = 0usize;
    // SAFETY: data is a valid readable source slice. The OS performs the write
    // into the arbitrary destination and reports an error instead of letting
    // Rust dereference it; written is a valid count out-pointer.
    let ok = unsafe {
        WriteProcessMemory(
            GetCurrentProcess(),
            addr as *mut u8,
            data.as_ptr(),
            data.len(),
            &mut written,
        )
    };
    ok != 0 && written == data.len()
}

fn span_of(cur: &mut Option<Span>, p: usize) -> Option<&Span> {
    if cur.as_ref().is_some_and(|s| s.contains(p, 1)) {
        return cur.as_ref();
    }
    *cur = span_at(p);
    cur.as_ref()
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
        let b = read_u8(a)?;
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
        let Some(b) = read_u8(a) else {
            break;
        };
        if b == 0 {
            break;
        }
        s.push(if (32..127).contains(&b) {
            b as char
        } else {
            '.'
        });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_bounds_and_writability_are_checked() {
        let rw = Span::from_parts(0x1000, 16, crate::ffi::PAGE_READWRITE);
        assert_eq!(rw.base(), 0x1000);
        assert_eq!(rw.end(), 0x1010);
        assert_eq!(rw.protect(), crate::ffi::PAGE_READWRITE);
        assert!(rw.contains(0x1000, 8));
        assert!(rw.contains(0x1008, 8));
        assert!(!rw.contains(0x1009, 8));
        assert!(!rw.contains(usize::MAX - 4, 8));
        assert!(rw.writable());

        let ro = Span::from_parts(0x1000, 16, crate::ffi::PAGE_READONLY);
        assert!(!ro.writable());

        let guarded = Span::from_parts(
            0x1000,
            16,
            crate::ffi::PAGE_READWRITE | crate::ffi::PAGE_GUARD,
        );
        assert!(!guarded.writable());
    }

    #[test]
    fn memory_region_readability_rejects_uncommitted_and_guard_pages() {
        let ok = MemoryRegion {
            base: 0x1000,
            size: 0x1000,
            state: MEM_COMMIT,
            protect: crate::ffi::PAGE_READONLY,
            ty: crate::ffi::MEM_IMAGE,
        };
        assert!(ok.is_committed());
        assert!(ok.is_readable());
        assert_eq!(ok.end(), 0x2000);

        let guard = MemoryRegion {
            protect: crate::ffi::PAGE_READWRITE | PAGE_GUARD,
            ..ok
        };
        assert!(!guard.is_readable());

        let reserved = MemoryRegion {
            state: crate::ffi::MEM_RESERVE,
            ..ok
        };
        assert!(!reserved.is_committed());
        assert!(!reserved.is_readable());
    }

    #[test]
    fn host_reads_fail_closed_without_panicking() {
        let mut buf = [0u8; 8];
        assert!(!read_copy(0, &mut buf));
        assert_eq!(read_partial(0, &mut buf), 0);
        assert!(read_bytes(0, 8).is_none());
        assert!(read_upto(0, 8).is_empty());
    }
}
