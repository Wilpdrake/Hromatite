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

/// A numeric region description plus an optional *owned* byte snapshot.
///
/// Numeric fields never authorise dereferencing. `read_*_in_span` either reads
/// this owned copy, or asks the OS to copy memory with `ReadProcessMemory`.
#[derive(Clone, Debug)]
pub struct Span {
    base: usize,
    size: usize,
    protect: u32,
    snapshot_base: usize,
    bytes: Vec<u8>,
}

impl Span {
    /// Construct an empty range. Reads use OS copies until `refresh` fills the
    /// owned snapshot. No address is ever dereferenced merely because it is here.
    pub fn from_parts(base: usize, size: usize, protect: u32) -> Self {
        Self {
            base,
            size,
            protect,
            snapshot_base: 0,
            bytes: Vec::new(),
        }
    }

    /// Copy a region once and iterate the owned bytes without further FFI calls.
    pub fn from_snapshot(base: usize, size: usize, protect: u32) -> Option<Self> {
        let mut span = Self::from_parts(base, size, protect);
        span.refresh(base, size, protect).then_some(span)
    }

    /// Refresh the snapshot, reusing its allocation across 64 KiB scan chunks.
    pub fn refresh(&mut self, base: usize, size: usize, protect: u32) -> bool {
        self.base = base;
        self.size = size;
        self.protect = protect;
        self.snapshot_base = base;
        self.bytes.resize(size, 0);
        if size == 0 {
            return true;
        }
        if read_copy(base, &mut self.bytes) {
            true
        } else {
            self.bytes.clear();
            false
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

    fn writable(&self) -> bool {
        matches!(
            self.protect & 0xFF,
            crate::ffi::PAGE_READWRITE
                | crate::ffi::PAGE_WRITECOPY
                | crate::ffi::PAGE_EXECUTE_READWRITE
                | crate::ffi::PAGE_EXECUTE_WRITECOPY
        ) && self.protect & crate::ffi::PAGE_GUARD == 0
    }

    fn snapshot(&self, addr: usize, len: usize) -> Option<&[u8]> {
        if self.bytes.is_empty() || self.snapshot_base != self.base {
            return None;
        }
        let offset = addr.checked_sub(self.snapshot_base)?;
        let end = offset.checked_add(len)?;
        self.bytes.get(offset..end)
    }

    /// Borrow the owned snapshot. Its lifetime is tied to this Span, never to
    /// the live region it was copied from.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    #[cfg(test)]
    fn from_owned_bytes(bytes: &[u8], protect: u32) -> Self {
        Self {
            base: bytes.as_ptr() as usize,
            size: bytes.len(),
            protect,
            snapshot_base: bytes.as_ptr() as usize,
            bytes: bytes.to_vec(),
        }
    }

    #[cfg(test)]
    fn set_range(&mut self, base: usize, size: usize) {
        self.base = base;
        self.size = size;
        self.snapshot_base = base;
    }
}

/// One 64-bit word from an owned snapshot or an OS-mediated copy.
pub fn read_u64_in_span(span: &Span, addr: usize) -> Option<u64> {
    if !span.contains(addr, 8) {
        return None;
    }
    if let Some(bytes) = span.snapshot(addr, 8) {
        return Some(u64::from_le_bytes(bytes.try_into().ok()?));
    }
    read_u64(addr)
}

pub fn read_usize_in_span(span: &Span, addr: usize) -> Option<usize> {
    read_u64_in_span(span, addr).map(|v| v as usize)
}

pub fn read_u8_in_span(span: &Span, addr: usize) -> Option<u8> {
    if !span.contains(addr, 1) {
        return None;
    }
    if let Some(bytes) = span.snapshot(addr, 1) {
        return bytes.first().copied();
    }
    read_u8(addr)
}

/// Compare a complete byte pattern inside one validated region snapshot.
pub fn matches_in_span(span: &Span, addr: usize, pat: &[u8]) -> bool {
    if pat.is_empty() || !span.contains(addr, pat.len()) {
        return false;
    }
    if read_u8_in_span(span, addr) != pat.first().copied() {
        return false;
    }
    if pat.len() == 1 {
        return true;
    }
    if let Some(bytes) = span.snapshot(addr, pat.len()) {
        return bytes == pat;
    }
    read_bytes(addr, pat.len()).is_some_and(|bytes| bytes == pat)
}

/// Reused bounded cursor over a large address range. `next_chunk` refreshes
/// the owned Span to the next window, extending it by `overlap` bytes for
/// patterns that straddle the boundary: candidates never start in that tail.
pub struct ScanChunks {
    next: usize,
    end: usize,
    protect: u32,
    window: usize,
    overlap: usize,
    span: Span,
    failed: bool,
}

impl ScanChunks {
    pub fn new(base: usize, size: usize, protect: u32, window: usize, overlap: usize) -> Self {
        let window = window.clamp(1, 1024 * 1024);
        Self {
            next: base,
            end: base.saturating_add(size),
            protect,
            window,
            overlap: overlap.min(window),
            span: Span::from_parts(base, 0, protect),
            failed: false,
        }
    }

    pub fn next_chunk(&mut self) -> bool {
        if self.next >= self.end {
            return false;
        }
        let remaining = self.end - self.next;
        let body = remaining.min(self.window);
        let tail = self.overlap.min(remaining - body);
        self.next = self.next.saturating_add(body);
        let ok = self
            .span
            .refresh(self.next - body, body + tail, self.protect);
        self.failed = !ok;
        ok
    }

    /// Exclusive start limit for candidates. The overlap tail is lookahead for
    /// the current chunk, not a second range of candidate start addresses.
    pub fn candidate_end(&self) -> usize {
        self.next
    }

    pub fn failed(&self) -> bool {
        self.failed
    }

    pub fn span(&self) -> &Span {
        &self.span
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
    // SAFETY: `mbi` is a live output buffer of exactly the passed size; the
    // query only borrows `addr` as an opaque address and does not dereference it.
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
        Some(Span::from_parts(
            mbi.BaseAddress,
            mbi.RegionSize,
            mbi.Protect,
        ))
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

fn writable_range(addr: usize, len: usize) -> bool {
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
    // SAFETY: `writable_range` accepted the complete output range, and the
    // unaligned store cannot overlap any Rust reference held by this function.
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

/// Offsets (relative to `hay`) of every 8-byte word equal to `want`, capped.
/// The caller passes a validated `Span`, so the loop itself does no per-word
/// `VirtualQuery`.
pub fn find_qword_in_span(span: &Span, want: u64, max: usize, out: &mut Vec<usize>) {
    let Some(mut a) = span.base.checked_add(7).map(|v| v & !7usize) else {
        return;
    };
    let end = span.end().saturating_sub(8);
    while a <= end && out.len() < max {
        if read_u64_in_span(span, a) == Some(want) {
            out.push(a);
        }
        a += 8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> [u8; 24] {
        [
            1, 2, 3, 4, 5, 6, 7, 8, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]
    }

    fn fixture_span(bytes: &[u8]) -> Span {
        Span::from_owned_bytes(bytes, crate::ffi::PAGE_READWRITE)
    }

    #[test]
    fn span_reads_are_bounds_checked() {
        let bytes = fixture();
        let mut span = fixture_span(&bytes);
        let base = span.base();
        span.set_range(base, 16);
        let first = u64::from_le_bytes([1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(read_u64_in_span(&span, base), Some(first));
        assert_eq!(read_u64_in_span(&span, base + 9), None);
        assert_eq!(read_u8_in_span(&span, base + 15), Some(0));
        assert_eq!(read_u8_in_span(&span, base + 16), None);
        assert!(matches_in_span(&span, base + 7, &[8, 9]));
        assert!(!matches_in_span(&span, base + 9, &[9, 0]));
        assert!(!span.contains(usize::MAX - 4, 8));
    }

    #[test]
    fn candidate_limit_does_not_duplicate_the_next_chunk_start() {
        let base = 0x10000usize;
        let mut chunks = ScanChunks::new(
            base,
            64 * 1024 + 32,
            crate::ffi::PAGE_READONLY,
            64 * 1024,
            4,
        );
        // Position the cursor exactly after the first logical body, as
        // next_chunk() leaves it. The extra four bytes are lookahead only.
        chunks.next = base + 64 * 1024;
        let last = chunks.candidate_end().saturating_sub(4);
        assert!(
            base + 64 * 1024 > last,
            "a match at the next chunk base would duplicate"
        );
        assert_eq!(last, base + 64 * 1024 - 4);
    }

    #[test]
    fn refresh_failure_is_reported_not_confused_with_end_of_scan() {
        let mut chunks = ScanChunks::new(1, 4096, crate::ffi::PAGE_READONLY, 64, 0);
        assert!(
            !chunks.next_chunk(),
            "address 1 must not be treated as a valid region"
        );
        assert!(chunks.failed());
    }

    #[test]
    fn qword_scan_handles_alignment_and_address_overflow() {
        let bytes = fixture();
        let mut span = fixture_span(&bytes);
        let base = span.base();
        span.set_range(base + 1, 23);
        let mut out = Vec::new();
        find_qword_in_span(&span, 0, 8, &mut out);
        assert_eq!(out.len(), 1);

        let invalid = Span::from_parts(usize::MAX - 4, 8, crate::ffi::PAGE_READWRITE);
        out.clear();
        find_qword_in_span(&invalid, 0, 8, &mut out);
        assert!(out.is_empty());
    }
}
