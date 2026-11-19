//! The pure half of the locator: what `/proc/self/maps` says and what that
//! means for *where* an image is.
//!
//! The anti-cheat client image is mapped by *native* code (`dlopen` of a
//! `/memfd:` file), so `VirtualQuery` reports the whole span as unmapped and
//! `ReadProcessMemory` cannot touch it; only the Linux view of the address
//! space names it. Everything here works on the text of `maps` (or on the
//! `VirtualQuery` shape of a region) and is host-tested without touching a
//! process.

/// Regions walked per call before the walk gives up (and says so).
pub const MAX_REGIONS: usize = 65_536;
/// Pages probed inside large executable regions when the page walk is asked for.
pub const MAX_PAGES: usize = 4_096;
/// Page size used by the optional page walk.
pub const PAGE: usize = 4_096;
/// Probe once every this many pages, then yield to the game's GC.
pub const YIELD_EVERY: usize = 256;
/// Default snapshot budget for one candidate region (2 MiB).
pub const DEFAULT_READ_BYTES: usize = 2 << 20;
/// Upper bound a caller may raise the snapshot budget to (64 MiB).
pub const MAX_READ_BYTES: usize = 64 << 20;
/// One region of our own address space, as `VirtualQuery` describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    /// `BaseAddress`.
    pub base: usize,
    /// `RegionSize`.
    pub size: usize,
    /// `Protect`.
    pub protect: u32,
    /// True when the region is committed and neither guarded nor inaccessible.
    pub readable: bool,
}

impl Region {
    /// `rw-`, `r-x`, … in the `/proc/self/maps` style the docs use.
    pub fn permissions(&self) -> String {
        const EXECUTE: u32 = 0x10 | 0x20 | 0x40 | 0x80;
        let mut out = String::with_capacity(3);
        out.push(if self.protect & 0x01 == 0 { 'r' } else { '-' });
        out.push(if self.protect & 0x04 != 0 || self.protect & 0x08 != 0 {
            'w'
        } else {
            '-'
        });
        out.push(if self.protect & EXECUTE != 0 {
            'x'
        } else {
            '-'
        });
        out
    }
}

/// Knobs of one `eac.module` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Probe {
    /// Walk page starts inside large executable regions too.
    pub pages: bool,
    /// Bytes one candidate image may contribute to the snapshot.
    pub read_bytes: usize,
    /// Import names printed per candidate.
    pub import_limit: usize,
}

impl Default for Probe {
    fn default() -> Self {
        Self {
            pages: false,
            read_bytes: DEFAULT_READ_BYTES,
            import_limit: 24,
        }
    }
}

impl Probe {
    /// Parse the command argument: `pages`, `read=<bytes|NNm>`, `imports=<n>`.
    ///
    /// Unknown tokens are ignored on purpose: a diagnostic must not fail
    /// because of a typo, and the defaults are the safe ones.
    pub fn parse(arg: &str) -> Self {
        let mut probe = Self::default();
        for token in arg.split_whitespace() {
            match token {
                "pages" => probe.pages = true,
                other => {
                    if let Some(value) = other.strip_prefix("read=") {
                        if let Some(bytes) = parse_size(value) {
                            probe.read_bytes = bytes.clamp(4096, MAX_READ_BYTES);
                        }
                    } else if let Some(value) = other.strip_prefix("imports=")
                        && let Ok(limit) = value.parse::<usize>()
                    {
                        probe.import_limit = limit.min(256);
                    }
                }
            }
        }
        probe
    }
}

/// `4096`, `512k`, `4m` → bytes.
fn parse_size(value: &str) -> Option<usize> {
    let (digits, factor) = match value.as_bytes().last() {
        Some(b'k') | Some(b'K') => (&value[..value.len() - 1], 1024),
        Some(b'm') | Some(b'M') => (&value[..value.len() - 1], 1024 * 1024),
        _ => (value, 1),
    };
    digits
        .parse::<usize>()
        .ok()
        .and_then(|number| number.checked_mul(factor))
}

// ---------------------------------------------------------------- unix view
//
// The anti-cheat client image is mapped into this process by *native* code
// (`dlopen` of a `/memfd:` file by the launcher), not by Wine: `VirtualQuery`
// reports the whole span as unmapped and `ReadProcessMemory` cannot touch it.
// The Linux view of our own address space is the only way to name it, and Wine
// exposes that view at `\\?\unix\...` — verified live 2026-11-18: the payload
// read `\\?\unix\proc\self\maps` while a nonexistent path returned
// "File not found".

/// One line of `/proc/self/maps`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MapEntry {
    /// First address of the mapping.
    pub start: u64,
    /// One past the last address.
    pub end: u64,
    /// `rwxp`-style permission string.
    pub perms: String,
    /// File offset the mapping starts at.
    pub offset: u64,
    /// Everything after the inode column (`/memfd:… (deleted)`, a path, empty).
    pub path: String,
}

impl MapEntry {
    /// Mapping length in bytes.
    pub fn size(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// A readable, file-backed memfd mapping that starts a file.
    ///
    /// This is how the anti-cheat client image looks: `/memfd:<uuid> (deleted)`,
    /// first segment `r--p` at offset 0. Anonymous regions and the `(deleted)`
    /// tail of a PE are excluded by the `offset == 0` rule.
    pub fn is_memfd_image(&self) -> bool {
        self.path.contains("memfd") && self.perms.starts_with('r') && self.offset == 0
    }
}

/// Parse the text of `/proc/self/maps`; a malformed line is skipped, never fatal.
pub fn parse_maps(text: &str) -> Vec<MapEntry> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(range) = fields.next() else { continue };
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end)) = (u64::from_str_radix(start, 16), u64::from_str_radix(end, 16))
        else {
            continue;
        };
        let Some(perms) = fields.next() else { continue };
        let Some(offset) = fields.next().and_then(|o| u64::from_str_radix(o, 16).ok()) else {
            continue;
        };
        // device and inode columns, then the (possibly absent) path
        let _device = fields.next();
        let _inode = fields.next();
        let path = fields.collect::<Vec<_>>().join(" ");
        out.push(MapEntry {
            start,
            end,
            perms: perms.to_string(),
            offset,
            path,
        });
    }
    out
}

/// Bases of memfd-backed images, ascending and deduplicated.
pub fn memfd_bases(maps: &[MapEntry]) -> Vec<u64> {
    let mut bases: Vec<u64> = maps
        .iter()
        .filter(|m| m.is_memfd_image())
        .map(|m| m.start)
        .collect();
    bases.sort_unstable();
    bases.dedup();
    bases
}

/// `(first, last)` addresses of every mapping that shares the memfd path of the
/// mapping starting at `base` — the image's real span, not one segment.
pub fn image_span(maps: &[MapEntry], base: u64) -> (u64, u64) {
    let Some(first) = maps.iter().find(|m| m.start == base) else {
        return (base, base);
    };
    let mut lo = first.start;
    let mut hi = first.end;
    for map in maps.iter().filter(|m| m.path == first.path) {
        lo = lo.min(map.start);
        hi = hi.max(map.end);
    }
    (lo, hi)
}
/// One mapped file segment of the image: `offset` in the file, `size` bytes,
/// at `address` in our address space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    /// File offset the segment starts at.
    pub offset: u64,
    /// Segment length in bytes.
    pub size: u64,
    /// Address the segment is mapped at.
    pub address: u64,
}

/// Translation table from `maps` entries that belong to one image.
///
/// ELF offsets handed to [`ReadAt`] are *file* offsets (the header and program
/// headers are addressed that way), and the loader maps the last segment at a
/// different `p_vaddr` than its `p_offset` — for the live EAC image the program
/// headers live at file offset `0x655258` but at address `base + 0x65e258`.
/// Only `maps` knows the difference.
pub fn segments_of(maps: &[MapEntry], base: u64) -> Vec<Segment> {
    let Some(first) = maps.iter().find(|m| m.start == base) else {
        return Vec::new();
    };
    let mut segments: Vec<Segment> = maps
        .iter()
        .filter(|m| m.path == first.path)
        .map(|m| Segment {
            offset: m.offset,
            size: m.size(),
            address: m.start,
        })
        .collect();
    segments.sort_by_key(|segment| segment.offset);
    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn probe_arguments_are_parsed_defensively() {
        let probe = Probe::parse("pages read=512k imports=7 typo");
        assert!(probe.pages);
        assert_eq!(probe.read_bytes, 512 * 1024);
        assert_eq!(probe.import_limit, 7);

        let probe = Probe::parse("read=1m");
        assert_eq!(probe.read_bytes, 1024 * 1024);
        let probe = Probe::parse("read=1 imports=999");
        assert_eq!(probe.read_bytes, 4096, "clamped up to one page");
        assert_eq!(probe.import_limit, 256, "clamped");
        let probe = Probe::parse("");
        assert_eq!(probe, Probe::default());
        assert!(parse_size("2m") == Some(2 << 20));
        assert!(parse_size("nope").is_none());
    }

    #[test]
    fn region_permissions_are_rendered_like_proc_maps() {
        let region = Region {
            base: 0,
            size: 0x1000,
            protect: 0x04,
            readable: true,
        };
        assert_eq!(region.permissions(), "rw-");
        let region = Region {
            protect: 0x20,
            ..region
        };
        assert_eq!(region.permissions(), "r-x");
        let region = Region {
            protect: 0x01,
            readable: false,
            ..region
        };
        assert_eq!(
            region.permissions(),
            "---",
            "PAGE_NOACCESS is no access at all"
        );
    }

    #[test]
    fn proc_maps_parser_names_the_memfd_image() {
        // Two lines taken verbatim from the live game process (2026-11-18).
        let text = "7c4eb05a0000-7c4eb05a1000 rw-p 0011f000 00:01 281203 /wine/x86_64-unix/ntdll.so\n\
                    7c4eb05a1000-7c4eb05c1000 r--p 00000000 00:01 281203 /memfd:b2e3ac3c-c96e-1f40-9a39 (deleted)\n\
                    7c4eb05c1000-7c4eb0704000 r-xp 00020000 00:01 281203 /memfd:b2e3ac3c-c96e-1f40-9a39 (deleted)\n\
                    7c4eb07b9000-7c4eb07bb000 rw-p 00217000 00:01 281203 /memfd:b2e3ac3c-c96e-1f40-9a39 (deleted)\n\
                    100000000-104000000 rw-p 00000000 00:00 0";
        let maps = parse_maps(text);
        assert_eq!(maps.len(), 5);
        assert_eq!(
            memfd_bases(&maps),
            vec![0x7c4eb05a1000],
            "only offset 0 starts a file"
        );
        let (lo, hi) = image_span(&maps, 0x7c4eb05a1000);
        assert_eq!(lo, 0x7c4eb05a1000);
        assert_eq!(
            hi, 0x7c4eb07bb000,
            "the rw-p tail belongs to the same image"
        );
        assert_eq!(hi - lo, 0x21a000);
        assert!(
            !maps[0].is_memfd_image(),
            "an ELF .so of Wine is not a memfd image"
        );
        assert!(!maps[4].is_memfd_image(), "an anonymous region has no path");
    }

    #[test]
    fn malformed_maps_lines_are_skipped_not_fatal() {
        let maps = parse_maps(
            "garbage\n7c4e-zzzz r-xp 0 0:0 0 x\n7c4e0000-7c4e1000 r--p 00000000 00:01 1 /memfd:abc (deleted)\n",
        );
        assert_eq!(maps.len(), 1, "only the well-formed line survives");
        assert_eq!(maps[0].size(), 0x1000);
        assert_eq!(memfd_bases(&maps), vec![0x7c4e0000]);
        assert_eq!(
            image_span(&maps, 0xdead),
            (0xdead, 0xdead),
            "unknown base is empty"
        );
    }
}
