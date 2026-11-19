//! Everything that touches a real address space: the `VirtualQuery` walk of our
//! own regions, the `proc/self` view through the Wine unix namespace, the
//! `maps`-translated direct reads of the game's pid, and the report lines they
//! produce.
//!
//! Only the pure `runtime` build compiles this module, and only on Windows do
//! the `VirtualQuery`/`veh` arms exist; on the host the pieces are either
//! absent (`cfg(windows)`) or exercised by the tests below through `std::fs`.
//!
//! Page-by-page scanning is the thing that froze the game before
//! (`Fatal error in GC — SuspendThread loop failed`): a long tight memory scan
//! on the tick thread starves the GC's `SuspendThread`. Every walk here is
//! therefore budgeted and yields with `safe::scan_yield`.

use super::*;

#[cfg(windows)]
use hromatite_win::memory as safe;

// ------------------------------------------------------ the Windows view

/// [`ReadAt`] over one region of our own address space, with a byte budget.
#[cfg(windows)]
pub struct RegionRead {
    base: usize,
    size: usize,
    budget: usize,
    spent: usize,
}

#[cfg(windows)]
impl RegionRead {
    fn new(base: usize, size: usize, budget: usize) -> Self {
        Self {
            base,
            size,
            budget,
            spent: 0,
        }
    }

    /// Bytes actually copied out of the game's memory by this reader.
    pub fn spent(&self) -> usize {
        self.spent
    }
}

#[cfg(windows)]
impl ReadAt for RegionRead {
    fn read_at(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        let remaining = self.budget.checked_sub(self.spent)?;
        if remaining == 0 {
            return None;
        }
        let offset = usize::try_from(offset).ok()?;
        if offset >= self.size {
            return None;
        }
        let room = self.size - offset;
        let len = len.min(remaining).min(room);
        if len == 0 {
            return None;
        }
        let address = self.base.checked_add(offset)?;
        let bytes = safe::read_bytes(address, len)?;
        self.spent += bytes.len();
        Some(bytes)
    }
}

/// Walk the target's own regions, applying `visit` to every committed one.
///
/// The walk is bounded by [`MAX_REGIONS`] and yields with
/// [`safe::scan_yield`] so the game's GC keeps winning `SuspendThread`.
#[cfg(windows)]
fn walk(mut visit: impl FnMut(Region) -> bool) -> (usize, bool) {
    let mut address = 0usize;
    let mut seen = 0usize;
    let mut stopped_early = false;
    while address < usize::MAX - 0x1000 {
        let Some(info) = safe::query_region(address) else {
            break;
        };
        let Some(next) = info.base.checked_add(info.size) else {
            break;
        };
        if next <= address {
            break;
        }
        address = next;
        seen += 1;
        if seen > MAX_REGIONS {
            // The region budget is a hard stop: a partial walk is reported
            // as such instead of pretending the process has no ELF image.
            stopped_early = true;
            break;
        }
        if seen.is_multiple_of(64) {
            safe::scan_yield();
        }
        let region = Region {
            base: info.base,
            size: info.size,
            protect: info.protect,
            readable: info.is_readable(),
        };
        if !visit(region) {
            stopped_early = true;
            break;
        }
    }
    (seen, stopped_early)
}

/// True when the region starts with `\x7fELF`.
#[cfg(windows)]
fn starts_with_magic(base: usize) -> bool {
    safe::read_bytes(base, MAGIC.len()).is_some_and(|bytes| bytes == MAGIC)
}

/// Describe one candidate and return its report lines.
#[cfg(windows)]
fn describe_region(region: Region, probe: &Probe) -> (Vec<String>, usize) {
    let mut reader = RegionRead::new(region.base, region.size, probe.read_bytes);
    let mut lines = vec![format!(
        "eac.module: ELF image at 0x{:x} size=0x{:x} protect={}",
        region.base,
        region.size,
        region.permissions()
    )];
    let result = describe(&mut reader, probe.import_limit);
    let spent = reader.spent();
    lines.push(format!(
        "eac.module: snapshot budget {} B, copied {spent} B",
        probe.read_bytes
    ));
    match result {
        Ok(summary) => lines.extend(summary.lines()),
        Err(error) => lines.push(format!("eac.module: parse failed: {error}")),
    }
    (lines, spent)
}

/// Run the locator and return the lines to log.
#[cfg(windows)]
pub fn probe_command(probe: &Probe) -> Vec<String> {
    let started = std::time::Instant::now();
    let mut committed = 0usize;
    let mut magic_hits = 0usize;
    let mut copied = 0usize;
    let mut pages_walked = 0usize;
    let mut report: Vec<String> = Vec::new();

    let (seen, stopped_early) = walk(|region| {
        if !region.readable {
            return true;
        }
        committed += 1;
        if starts_with_magic(region.base) {
            magic_hits += 1;
            let (lines, spent) = describe_region(region, probe);
            copied += spent;
            report.extend(lines);
            if magic_hits >= 2 {
                return false;
            }
            return true;
        }
        if probe.pages
            && region.size >= 2 * PAGE
            && region.protect & (0x10 | 0x20 | 0x40 | 0x80) != 0
        {
            // Only the page starts of a large executable region, and only
            // within the global page budget.
            let pages = (region.size / PAGE).saturating_sub(1);
            for index in 1..=pages {
                if pages_walked >= MAX_PAGES {
                    return false;
                }
                pages_walked += 1;
                if pages_walked.is_multiple_of(YIELD_EVERY) {
                    safe::scan_yield();
                }
                let at = region.base + index * PAGE;
                if starts_with_magic(at)
                    && let Some(region) = region_at(at)
                {
                    magic_hits += 1;
                    let (lines, spent) = describe_region(region, probe);
                    copied += spent;
                    report.extend(lines);
                    if magic_hits >= 2 {
                        return false;
                    }
                }
            }
        }
        true
    });

    let mut lines = vec![format!(
        "eac.module: walked {seen} region(s), {committed} committed/readable, \
         {magic_hits} ELF magic hit(s), pages_walked={pages_walked}, \
         copied={copied} B, {} ms, stopped_early={stopped_early}",
        started.elapsed().as_millis()
    )];
    if magic_hits == 0 {
        lines.push(
            "eac.module: no ELF image found at a region start (try `eac.module pages`)".to_string(),
        );
    }
    lines.extend(report);
    lines
}

/// `VirtualQuery` for one address: the region containing it.
#[cfg(windows)]
fn region_at(address: usize) -> Option<Region> {
    let info = safe::query_region(address)?;
    Some(Region {
        base: info.base,
        size: info.size,
        protect: info.protect,
        readable: info.is_readable(),
    })
}

// ----------------------------------------------------- the unix view

/// `/proc/self/maps` of *this* process, through the Wine unix namespace.
const UNIX_MAPS: &str = r"\\?\unix\proc\self\maps";
/// The same process's memory as a seekable file.
const UNIX_MEM: &str = r"\\?\unix\proc\self\mem";

/// [`ReadAt`] over `/proc/self/mem`, with a byte budget.
///
/// `/proc/self/mem` answers at *virtual* offsets, so `base + offset` names
/// the same byte the payload would read with `safe::*` — if the mapping were
/// visible to `VirtualQuery`, which the memfd image is not.
pub struct UnixMemRead {
    file: std::fs::File,
    base: u64,
    budget: usize,
    spent: usize,
}

impl UnixMemRead {
    /// Open `/proc/self/mem` and anchor it at `base`.
    pub fn open(base: u64, budget: usize) -> Option<Self> {
        let file = std::fs::File::open(UNIX_MEM).ok()?;
        Some(Self {
            file,
            base,
            budget,
            spent: 0,
        })
    }

    /// Bytes copied out of the image by this reader.
    pub fn spent(&self) -> usize {
        self.spent
    }
}

impl ReadAt for UnixMemRead {
    fn read_at(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        if self.spent.checked_add(len)? > self.budget {
            return None;
        }
        let at = self.base.checked_add(offset)?;
        self.file.seek(SeekFrom::Start(at)).ok()?;
        let mut buffer = vec![0u8; len];
        self.file.read_exact(&mut buffer).ok()?;
        self.spent += len;
        Some(buffer)
    }
}

/// Locate memfd images through the Linux view and describe them.
///
/// Costs one small file read plus a 64-byte probe per candidate; nothing is
/// walked page by page, and no budget can be exceeded.
pub fn locate_unix(probe: &Probe) -> Vec<String> {
    let text = match std::fs::read_to_string(UNIX_MAPS) {
        Ok(text) => text,
        Err(error) => {
            return vec![format!("eac.module: cannot read {UNIX_MAPS}: {error}")];
        }
    };
    let maps = parse_maps(&text);
    let mut lines = vec![format!(
        "eac.module: unix view: {} mapping(s) in /proc/self/maps, {} bytes",
        maps.len(),
        text.len()
    )];
    if let Some(first) = maps.first() {
        lines.push(format!(
            "eac.module: unix view: first=0x{:x} {} perms={} path={}",
            first.start,
            first.size(),
            first.perms,
            if first.path.is_empty() {
                "<anon>"
            } else {
                &first.path
            }
        ));
    }
    let mut found = 0usize;
    for base in memfd_bases(&maps) {
        let Some(mut reader) = UnixMemRead::open(base, probe.read_bytes) else {
            lines.push("eac.module: cannot open /proc/self/mem".to_string());
            break;
        };
        let Some(head) = reader.read_at(0, MAGIC.len()) else {
            continue;
        };
        if head != MAGIC {
            continue;
        }
        found += 1;
        let (lo, hi) = image_span(&maps, base);
        let segments: Vec<String> = maps
            .iter()
            .filter(|m| m.start >= lo && m.end <= hi)
            .map(|m| format!("{}+0x{:x}({})", m.perms, m.size(), m.offset))
            .collect();
        lines.push(format!(
            "eac.module: unix ELF image at 0x{base:x} span=0x{:x} (0x{lo:x}..0x{hi:x}) \
             segments={}",
            hi.saturating_sub(lo),
            segments.join(" ")
        ));
        let result = describe(&mut reader, probe.import_limit);
        lines.push(format!(
            "eac.module: snapshot budget {} B, copied {} B",
            probe.read_bytes,
            reader.spent()
        ));
        match result {
            Ok(summary) => lines.extend(summary.lines()),
            Err(error) => lines.push(format!("eac.module: parse failed: {error}")),
        }
        if found >= 2 {
            break;
        }
    }
    if found == 0 {
        lines.push("eac.module: unix view: no memfd ELF image".to_string());
    }
    lines
}

// ----------------------------------------------------- the native view
//
// Measured live 2026-11-18: in Wine a `\\?\unix\proc\self\...` path is opened by
// the *wineserver*, so `self` names wineserver (111 mappings, first one
// `.../bin/wineserver`), not the game. The game's own Linux pid has to be found
// by name; its `maps` is world-readable, and the memfd image it names is
// readable by a plain load even though `VirtualQuery` reports it unmapped —
// Wine simply does not track mappings that native code created.

/// `/proc` of the host namespace, as seen through Wine.
#[cfg(feature = "runtime")]
const UNIX_PROC: &str = r"\\?\unix\proc";

/// The Linux pid of `VRChat.exe`, found by reading `cmdline` of `/proc/<pid>`.
#[cfg(feature = "runtime")]
pub fn find_process_pid() -> Option<u32> {
    let entries = std::fs::read_dir(UNIX_PROC).ok()?;
    for (index, entry) in entries.flatten().enumerate() {
        if index > 4096 {
            break;
        }
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(cmdline) = std::fs::read(format!("{UNIX_PROC}/{pid}/cmdline")) else {
            continue;
        };
        if cmdline
            .split(|byte| *byte == 0)
            .any(|arg| arg.ends_with(b"VRChat.exe"))
        {
            return Some(pid);
        }
    }
    None
}

/// `maps` text of one pid.
#[cfg(feature = "runtime")]
pub fn maps_of(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("{UNIX_PROC}/{pid}/maps")).ok()
}

/// [`ReadAt`] over raw addresses of our own process, bounded by the segments.
///
/// The segments come from `/proc/<pid>/maps`, so every read is inside a mapping
/// the kernel reports; the VEH guard turns a stale mapping into `None` instead
/// of a dead game.
#[cfg(feature = "runtime")]
pub struct NativeMemRead {
    segments: Vec<Segment>,
    budget: usize,
    spent: usize,
}

#[cfg(feature = "runtime")]
impl NativeMemRead {
    /// Reader over the image whose segments are `segments`.
    pub fn new(segments: Vec<Segment>, budget: usize) -> Self {
        Self {
            segments,
            budget,
            spent: 0,
        }
    }

    /// Bytes copied out of the image by this reader.
    pub fn spent(&self) -> usize {
        self.spent
    }

    /// Address of a file offset, when a mapped segment contains all of `[offset, offset+len)`.
    fn address_of(&self, offset: u64, len: u64) -> Option<u64> {
        let end = offset.checked_add(len)?;
        self.segments.iter().find_map(|segment| {
            let segment_end = segment.offset.checked_add(segment.size)?;
            (segment.offset <= offset && end <= segment_end)
                .then(|| segment.address.checked_add(offset - segment.offset))
                .flatten()
        })
    }
}

#[cfg(feature = "runtime")]
impl ReadAt for NativeMemRead {
    fn read_at(&mut self, offset: u64, len: usize) -> Option<Vec<u8>> {
        let at = self.address_of(offset, u64::try_from(len).ok()?)?;
        if self.spent.checked_add(len)? > self.budget {
            return None;
        }
        let mut buffer = vec![0u8; len];
        let result = hromatite_win::veh::call_with(|| {
            // SAFETY: `at` comes from a `maps` entry of this process and `len`
            // stays inside that mapping; the vectored handler converts a fault
            // into `Err` instead of killing the game.
            unsafe {
                core::ptr::copy_nonoverlapping(at as *const u8, buffer.as_mut_ptr(), len);
            }
        });
        if result.is_err() {
            return None;
        }
        self.spent += len;
        Some(buffer)
    }
}

/// Locate the memfd image through `maps` + direct reads, and describe it.
#[cfg(feature = "runtime")]
pub fn locate_native(probe: &Probe) -> Vec<String> {
    let Some(pid) = find_process_pid() else {
        return vec!["eac.module: VRChat.exe not found in /proc (wine unix view)".to_string()];
    };
    let Some(text) = maps_of(pid) else {
        return vec![format!("eac.module: cannot read /proc/{pid}/maps")];
    };
    let maps = parse_maps(&text);
    let bases = memfd_bases(&maps);
    let mut lines = vec![format!(
        "eac.module: pid {pid}, {} mapping(s), {} memfd candidate(s)",
        maps.len(),
        bases.len()
    )];
    let mut found = 0usize;
    for base in bases {
        let (lo, hi) = image_span(&maps, base);
        let segments_of_image = segments_of(&maps, base);
        let mut reader = NativeMemRead::new(segments_of_image.clone(), probe.read_bytes);
        let Some(head) = reader.read_at(0, MAGIC.len()) else {
            lines.push(format!("eac.module: 0x{base:x}: not readable"));
            continue;
        };
        if head != MAGIC {
            continue;
        }
        found += 1;
        let segments: Vec<String> = maps
            .iter()
            .filter(|m| m.start >= lo && m.end <= hi)
            .map(|m| format!("{}+0x{:x}({})", m.perms, m.size(), m.offset))
            .collect();
        let file_map: Vec<String> = segments_of_image
            .iter()
            .map(|segment| format!("file 0x{:x} -> 0x{:x}", segment.offset, segment.address))
            .collect();
        lines.push(format!(
            "eac.module: native ELF image at 0x{base:x} span=0x{:x} (0x{lo:x}..0x{hi:x}) segments={}",
            hi.saturating_sub(lo),
            segments.join(" ")
        ));
        lines.push(format!("eac.module: file mapping: {}", file_map.join(", ")));
        let result = describe(&mut reader, probe.import_limit);
        lines.push(format!(
            "eac.module: snapshot budget {} B, copied {} B",
            probe.read_bytes,
            reader.spent()
        ));
        match result {
            Ok(summary) => lines.extend(summary.lines()),
            Err(error) => lines.push(format!("eac.module: parse failed: {error}")),
        }
        if found >= 2 {
            break;
        }
    }
    if found == 0 {
        lines.push("eac.module: no readable memfd ELF image in our own maps".to_string());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "runtime")]
    #[test]
    fn segment_table_translates_file_offsets_the_way_the_loader_mapped_them() {
        // Live lines (2026-11-18): the last LOAD has p_offset 0x219000 but
        // p_vaddr 0x222000, and the program headers live at file offset 0x655258.
        let text = "7c4eb05a1000-7c4eb05c1000 r--p 00000000 00:01 1 /memfd:x (deleted)\n\
                    7c4eb05c1000-7c4eb0704000 r-xp 00020000 00:01 1 /memfd:x (deleted)\n\
                    7c4eb07c3000-7c4ec0000000 r-xp 00219000 00:01 1 /memfd:x (deleted)";
        let maps = parse_maps(text);
        let base = 0x7c4eb05a1000u64;
        let segments = segments_of(&maps, base);
        assert_eq!(segments.len(), 3);
        assert_eq!(segments[2].offset, 0x219000);
        assert_eq!(segments[2].address, 0x7c4eb07c3000, "p_vaddr, not p_offset");
        let reader = NativeMemRead::new(segments, 4096);
        assert_eq!(
            reader.address_of(0x655258, 56),
            Some(0x7c4eb07c3000 + (0x655258 - 0x219000)),
            "the program headers are addressed by file offset, mapped at p_vaddr"
        );
        assert_eq!(
            reader.address_of(0x2186c0, 64),
            None,
            "section headers fall in an unmapped gap between two LOAD segments"
        );
        assert_eq!(reader.address_of(0x1000, 0x100), Some(base + 0x1000));
    }
}
