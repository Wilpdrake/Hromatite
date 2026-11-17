//! Bounded scans over *this* process's readable memory.
//!
//! The IL2CPP layout moves on every game patch, so `m3` has to rediscover it
//! from runtime data instead of trusting recorded RVAs. Two primitives make
//! that cheap enough to do once per run:
//!
//!   * [`readable_spans`]/[`RegionMap`] — one `VirtualQuery` walk of the address
//!     space, cached as numbers so a candidate pointer can be rejected without
//!     a syscall;
//!   * [`scan_ascii`]/[`scan_qwords`] — chunked (1 MiB) snapshots of a region,
//!     so a multi-gigabyte heap costs a handful of `ReadProcessMemory` calls
//!     instead of one read per word.
//!
//! [`call3`] is the one place that leaves safe code: it invokes a verified
//! il2cpp export under the VEH guard, and only on the Windows target.

// Reconnaissance helpers: some are only reachable from a `#[cfg(windows)]`
// caller or from tests, and the module itself is not part of the ABI surface.
#![allow(dead_code)]

use std::sync::{Mutex, PoisonError};

use crate::procs;
use crate::safe;

/// Bytes copied per OS read. One megabyte keeps the snapshot allocation small
/// while amortising `ReadProcessMemory` call overhead over a large region.
const WINDOW: usize = 1 << 20;

/// A readable range, already clipped by whoever produced it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanSpan {
    pub base: usize,
    pub size: usize,
    pub protect: u32,
}

impl ScanSpan {
    pub fn end(&self) -> usize {
        self.base.saturating_add(self.size)
    }
}

/// Every committed, readable region of our own address space.
pub fn readable_spans(max_regions: usize) -> Vec<ScanSpan> {
    procs::regions(max_regions)
        .into_iter()
        .filter(|region| region.readable() && region.size > 0)
        .map(|region| ScanSpan {
            base: region.base,
            size: region.size,
            protect: region.protect,
        })
        .collect()
}

/// Readable regions clipped to `[base, base + size)` — the image only, never
/// the page before or after it (a stray candidate outside the module is how the
/// old scan produced garbage).
pub fn spans_within(base: usize, size: usize, max_regions: usize) -> Vec<ScanSpan> {
    let end = base.saturating_add(size);
    readable_spans(max_regions)
        .into_iter()
        .filter_map(|span| {
            let from = span.base.max(base);
            let to = span.end().min(end);
            (to > from).then_some(ScanSpan {
                base: from,
                size: to - from,
                protect: span.protect,
            })
        })
        .collect()
}

/// Sorted ranges for cheap readability tests: a candidate array pointer must be
/// *somewhere* mapped before it is worth a `ReadProcessMemory`.
pub struct RegionMap {
    ranges: Vec<(usize, usize)>,
}

impl RegionMap {
    pub fn capture(max_regions: usize) -> Self {
        Self::from_ranges(
            readable_spans(max_regions)
                .into_iter()
                .map(|span| (span.base, span.end()))
                .collect(),
        )
    }

    pub fn from_ranges(mut ranges: Vec<(usize, usize)>) -> Self {
        ranges.sort_unstable();
        Self { ranges }
    }

    /// Does any captured readable range cover `addr..addr + len`?
    pub fn contains(&self, addr: usize, len: usize) -> bool {
        let Some(end) = addr.checked_add(len) else {
            return false;
        };
        let index = self.ranges.partition_point(|(start, _)| *start <= addr);
        index > 0 && self.ranges[index - 1].1 >= end
    }

    pub fn spans(&self) -> &[(usize, usize)] {
        &self.ranges
    }
}

/// Byte offsets of `needle` inside one snapshot chunk. `body_len` is how much of
/// `bytes` belongs to this chunk's candidate range; the tail is lookahead for a
/// needle that straddles the boundary, so it may only be *read*, never reported
/// as a start (it is reported by the next chunk).
pub(crate) fn ascii_offsets(
    bytes: &[u8],
    needle: &[u8],
    body_len: usize,
    limit: usize,
) -> Vec<usize> {
    let mut out = Vec::new();
    if needle.is_empty() || bytes.len() < needle.len() {
        return out;
    }
    let body_len = body_len.min(bytes.len());
    let last = bytes.len() - needle.len();
    for offset in 0..=last {
        if offset >= body_len {
            break;
        }
        if bytes[offset..offset + needle.len()] == *needle {
            out.push(offset);
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

/// `(offset, value)` of every complete 8-byte qword whose *start* lies in the
/// chunk body.
pub(crate) fn qwords(bytes: &[u8], body_len: usize) -> impl Iterator<Item = (usize, usize)> + '_ {
    let count = body_len.min(bytes.len()) / 8;
    (0..count).map(move |index| {
        let offset = index * 8;
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&bytes[offset..offset + 8]);
        (offset, u64::from_le_bytes(raw) as usize)
    })
}

/// Every address inside `spans` where `needle` starts. The snapshot overlap is
/// one byte short of the needle, so a string straddling a 1 MiB boundary is
/// still found exactly once.
pub fn scan_ascii(spans: &[ScanSpan], needle: &[u8], limit: usize) -> Vec<usize> {
    let mut out = Vec::new();
    if needle.is_empty() || limit == 0 {
        return out;
    }
    for span in spans {
        if out.len() >= limit {
            break;
        }
        let mut chunks = safe::ScanChunks::new(
            span.base,
            span.size,
            span.protect,
            WINDOW,
            needle.len().saturating_sub(1),
        );
        let mut body = span.base;
        while chunks.next_chunk() {
            let end = chunks.candidate_end();
            let body_len = end.saturating_sub(body);
            for offset in ascii_offsets(chunks.span().bytes(), needle, body_len, limit - out.len())
            {
                out.push(body + offset);
                if out.len() >= limit {
                    return out;
                }
            }
            body = end;
        }
    }
    out
}

/// Visit every 8-byte-aligned qword of `spans`; `visit` returns `true` to stop
/// the whole scan (a confirmed hit).
pub fn scan_qwords(spans: &[ScanSpan], mut visit: impl FnMut(usize, usize) -> bool) {
    for span in spans {
        let mut chunks = safe::ScanChunks::new(span.base, span.size, span.protect, WINDOW, 0);
        let mut body = span.base;
        while chunks.next_chunk() {
            let end = chunks.candidate_end();
            let body_len = end.saturating_sub(body);
            for (offset, word) in qwords(chunks.span().bytes(), body_len) {
                if visit(body + offset, word) {
                    return;
                }
            }
            body = end;
        }
    }
}

/// Every qword-aligned address whose value is one of `wanted`.
pub fn scan_words(spans: &[ScanSpan], wanted: &[usize], limit: usize) -> Vec<usize> {
    let mut out = Vec::new();
    if wanted.is_empty() || limit == 0 {
        return out;
    }
    scan_qwords(spans, |address, word| {
        if wanted.contains(&word) {
            out.push(address);
        }
        out.len() >= limit
    });
    out
}

/// Stable NUL-terminated ASCII copies for functions that take `const char*`.
///
/// il2cpp's `class_from_name` only reads the bytes during the call, so a leaked
/// 256-byte box per lookup is cheaper (and far simpler) than an allocation the
/// game could move.
pub fn stash_cstr(text: &str) -> Option<usize> {
    const CAP: usize = 256;
    // Boxed so that `Vec` growth never moves a buffer il2cpp may still hold;
    // the addresses handed out must stay valid for the process lifetime.
    #[allow(clippy::vec_box)]
    static POOL: Mutex<Vec<Box<[u8; CAP]>>> = Mutex::new(Vec::new());
    let bytes = text.as_bytes();
    if bytes.len() >= CAP {
        return None;
    }
    let mut buffer = Box::new([0u8; CAP]);
    buffer[..bytes.len()].copy_from_slice(bytes);
    let address = buffer.as_ptr() as usize;
    let mut pool = POOL.lock().unwrap_or_else(PoisonError::into_inner);
    pool.push(buffer);
    Some(address)
}

/// Call a verified two-argument il2cpp export under the VEH guard.
#[cfg(target_os = "windows")]
pub fn call2(f: usize, a: usize, b: usize) -> Option<usize> {
    if f == 0 {
        return None;
    }
    // SAFETY: callers pass a verified il2cpp export address inside GameAssembly
    // with exactly this Win64 signature; VEH turns a bad call into `Err`.
    unsafe { crate::veh::call2(f, a, b) }.ok()
}

#[cfg(not(target_os = "windows"))]
pub fn call2(_f: usize, _a: usize, _b: usize) -> Option<usize> {
    None
}

/// Call a verified three-argument il2cpp export under the VEH guard.
#[cfg(target_os = "windows")]
pub fn call3(f: usize, a: usize, b: usize, c: usize) -> Option<usize> {
    if f == 0 {
        return None;
    }
    // SAFETY: see `call2`; `il2cpp_class_from_name(image, namespaze, name)`.
    unsafe { crate::veh::call3(f, a, b, c) }.ok()
}

#[cfg(not(target_os = "windows"))]
pub fn call3(_f: usize, _a: usize, _b: usize, _c: usize) -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::{RegionMap, ascii_offsets, qwords, stash_cstr};

    #[test]
    fn region_map_answers_without_a_single_syscall() {
        let map = RegionMap::from_ranges(vec![(0x4000, 0x5000), (0x1000, 0x2000)]);
        assert!(map.contains(0x1000, 8));
        assert!(map.contains(0x1ff8, 8));
        assert!(!map.contains(0x1fff, 8));
        assert!(!map.contains(0x2000, 1));
        assert!(map.contains(0x4000, 0x1000));
        assert!(!map.contains(usize::MAX - 3, 8));
    }

    #[test]
    fn ascii_offsets_finds_repeats_and_skips_the_overlap_tail() {
        let haystack = b"xxPhotonPeeryyPhotonPeerzz";
        assert_eq!(
            ascii_offsets(haystack, b"PhotonPeer", haystack.len(), 8),
            vec![2, 14]
        );
        // The tail is lookahead only: a match that starts in it is reported by
        // the next chunk, not twice.
        assert_eq!(
            ascii_offsets(haystack, b"PhotonPeer", 2, 8),
            Vec::<usize>::new()
        );
        assert_eq!(
            ascii_offsets(haystack, b"PhotonPeer", haystack.len(), 1),
            vec![2]
        );
        assert!(ascii_offsets(b"short", b"PhotonPeer", 5, 8).is_empty());
        assert!(ascii_offsets(haystack, b"", haystack.len(), 8).is_empty());
    }

    #[test]
    fn qword_offsets_are_aligned_and_clipped_to_the_body() {
        let words: [u64; 4] = [7, 0x1122_3344, 7, 9];
        let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
        let seen: Vec<(usize, usize)> = qwords(&bytes, bytes.len()).collect();
        assert_eq!(seen.len(), 4);
        assert_eq!(seen[1], (8, 0x1122_3344));
        // A body that ends mid-word does not expose a partial qword.
        assert_eq!(qwords(&bytes, 20).count(), 2);
        assert_eq!(qwords(&bytes, 7).count(), 0);
    }

    #[test]
    fn stash_cstr_returns_distinct_stable_nul_terminated_copies() {
        let first = stash_cstr("PhotonPeer").expect("short string fits");
        let second = stash_cstr("PhotonPeer").expect("short string fits");
        assert_ne!(first, second, "each lookup gets its own copy");
        for address in [first, second] {
            // SAFETY: the pool owns each 256-byte box for the process lifetime.
            let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, 16) };
            assert_eq!(&bytes[..11], b"PhotonPeer\0");
        }
        assert!(stash_cstr(&"x".repeat(256)).is_none());
    }
}
