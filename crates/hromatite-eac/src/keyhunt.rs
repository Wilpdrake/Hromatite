//! `EAC_KEYHUNT=1` — live hunt for the key that protects the 512-byte AC frames.
//!
//! Everything here is **observation only** and runs behind an environment flag:
//!
//! * the region map of the process is read from `/proc/self/maps` (the game is a
//!   Wine process, so the same file describes the NT address space and names the
//!   EAC `.so`), written out verbatim for the report;
//! * memory is scanned for the SDK's hard-coded seed key and a small set of
//!   plausible derivations (SHA-256, HKDF-SHA256 with candidate `info` labels);
//! * the buffer the module hands to `AddNotifyMessageToServer` for a 512-byte
//!   frame is probed (region + 1 KiB window dump) and memory is scanned for the
//!   frame's own 12-byte stable prefix, which finds the place it is assembled.
//!
//! Scans are budgeted: [`Hunter::tick`] consumes at most
//! `EAC_KEYHUNT_CHUNK_MB` (64 MiB default) per managed tick, so a multi-GB
//! address space never freezes the game for more than a frame or two. Two
//! passes are made, 15 s apart, so a re-keyed session is still caught.
//!
//! Only the raw reads are `unsafe`; each carries a `// SAFETY:` note. The region
//! parser, the key-derivation table and the matcher are pure and unit-tested on
//! the host.
//!
//! `Hunter` and every reader it drives (`read_maps`, `read_block`,
//! `read_window`, `enumerate_regions`, `find_in_region`) are behind the
//! `runtime` feature: they read the live address space through `hromatite-win`.
//! The pure half (region parsing, key derivation, the matcher) compiles into the
//! `default-features = false` core that `hromatite-features` links.

// Without the OS layer the scan driver and its readers are simply absent, which
// leaves the matcher and a few budgets unused in that build only.
#![cfg_attr(not(feature = "runtime"), allow(dead_code, unused_imports))]

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::clock::now_ms;
use crate::digest::{hex, hkdf_sha256, sha256};

/// The 32-byte constant the SDK's NetProtect constructor (`0x180405AF0`)
/// installs into both AES-256-GCM contexts before any session key arrives.
pub const SEED_KEY: [u8; 32] = [
    0xb6, 0x8d, 0x74, 0x70, 0x4c, 0x4e, 0x9e, 0x43, 0xd4, 0x91, 0xef, 0x36, 0x73, 0x45, 0x37, 0x3d,
    0xb7, 0x42, 0x51, 0x4a, 0xe1, 0xeb, 0x3b, 0xb9, 0xa1, 0x67, 0xa3, 0x0b, 0x3a, 0x35, 0xc7, 0xea,
];

/// `info` labels tried when expanding the seed with HKDF-SHA256. A wrong label
/// still costs nothing: a miss is simply not found in memory.
const HKDF_INFOS: [&str; 10] = [
    "",
    "eac",
    "EAC",
    "eos",
    "EOS",
    "anticheat",
    "AntiCheat",
    "session",
    "session-key",
    "heartbeat",
];

/// Longest needle the matcher accepts (the frame prefix is 12 bytes).
const MAX_NEEDLE: usize = 32;
/// Frames at least this large are treated as heartbeat candidates.
pub const BIG_FRAME: usize = 256;
/// Default per-tick scan budget.
const DEFAULT_CHUNK_MB: usize = 64;
/// Delay after the first big frame before the first pass (let the session settle).
const DEFAULT_FIRST_DELAY_MS: u64 = 2_500;
/// Delay between the two passes.
const DEFAULT_GAP_MS: u64 = 15_000;
/// Hits logged per needle before the rest are only counted.
const HIT_CAP: usize = 48;

// ------------------------------------------------------------------ needles

/// One 32-byte key candidate plus the derivation label shown in the log.
pub fn candidate_keys() -> Vec<(String, [u8; 32])> {
    let mut out = Vec::with_capacity(2 + 2 * HKDF_INFOS.len());
    out.push(("seed".to_string(), SEED_KEY));
    out.push(("sha256(seed)".to_string(), sha256(&SEED_KEY)));
    for info in HKDF_INFOS {
        let label = if info.is_empty() { "empty" } else { info };
        let plain = hkdf_sha256(&[], &SEED_KEY, info.as_bytes(), 32);
        out.push((format!("hkdf(seed,info={label})"), to32(&plain)));
        let salted = hkdf_sha256(b"eac", &SEED_KEY, info.as_bytes(), 32);
        out.push((format!("hkdf(salt=eac,seed,info={label})"), to32(&salted)));
    }
    out
}

fn to32(bytes: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    let take = bytes.len().min(32);
    out[..take].copy_from_slice(&bytes[..take]);
    out
}

// ------------------------------------------------------------------- regions

/// One `/proc/self/maps` line: `start-end perms offset dev inode path`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Region {
    /// First address of the mapping.
    pub start: usize,
    /// One past the last address.
    pub end: usize,
    /// `rw-p`, `r-xp`, … (as in `/proc/self/maps`).
    pub perms: String,
    /// Backing file, or `[heap]`/`[anon]` — empty for an anonymous mapping.
    pub path: String,
}

impl Region {
    /// True when the mapping can be read without faulting.
    pub fn readable(&self) -> bool {
        self.perms.starts_with('r') && self.end > self.start
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.end.saturating_sub(self.start)
    }

    /// True when the region carries no bytes.
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }

    /// True when this is the EAC module (Linux `.so`) or an EOS image.
    pub fn is_eac(&self) -> bool {
        let path = self.path.to_ascii_lowercase();
        path.contains("easyanticheat") || path.contains("eac") || path.contains("eossdk")
    }
}

/// Parse `/proc/self/maps` text. Unparsable lines are skipped, never fatal.
pub fn parse_maps(text: &str) -> Vec<Region> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let Some(range) = fields.next() else { continue };
        let Some((start, end)) = range.split_once('-') else {
            continue;
        };
        let (Ok(start), Ok(end)) = (
            usize::from_str_radix(start, 16),
            usize::from_str_radix(end, 16),
        ) else {
            continue;
        };
        let perms = fields.next().unwrap_or("").to_string();
        // offset, dev, inode
        let _ = fields.next();
        let _ = fields.next();
        let _ = fields.next();
        let path: String = fields.collect::<Vec<_>>().join(" ");
        let path = if path.is_empty() {
            "[anon]".to_string()
        } else {
            path
        };
        out.push(Region {
            start,
            end,
            perms,
            path,
        });
    }
    out
}

/// Read the live region map. Wine exposes the host `/proc` through `Z:`.
#[cfg(feature = "runtime")]
fn read_maps() -> Option<String> {
    for path in [
        "Z:\\proc\\self\\maps",
        "Z:/proc/self/maps",
        "/proc/self/maps",
    ] {
        if let Ok(text) = fs::read_to_string(path)
            && text.contains('-')
        {
            return Some(text);
        }
    }
    None
}

/// Block size for fault-tolerant reads: one kernel transition per 64 KiB.
#[cfg(feature = "runtime")]
const BLOCK: usize = 64 * 1024;

/// Copy `dst.len()` bytes from `addr` into `dst`, returning how many landed.
///
/// Never faults: an unmapped or guard page only shortens the copy. A region can
/// be unmapped between the `/proc/self/maps` snapshot and the read, so this is
/// the difference between a scan and a crash.
#[cfg(feature = "runtime")]
#[cfg(windows)]
fn read_block(addr: usize, dst: &mut [u8]) -> usize {
    hromatite_win::memory::read_partial(addr, dst)
}

/// Linux variant of [`read_block`], used on the host target (tests) and by any
/// build that runs the feature image natively.
#[cfg(feature = "runtime")]
#[cfg(not(windows))]
fn read_block(addr: usize, dst: &mut [u8]) -> usize {
    #[repr(C)]
    struct IoVec {
        base: *mut core::ffi::c_void,
        len: usize,
    }
    unsafe extern "C" {
        fn process_vm_readv(
            pid: i32,
            local: *const IoVec,
            local_count: usize,
            remote: *const IoVec,
            remote_count: usize,
            flags: usize,
        ) -> isize;
        fn getpid() -> i32;
    }
    let local = IoVec {
        base: dst.as_mut_ptr() as *mut core::ffi::c_void,
        len: dst.len(),
    };
    let remote = IoVec {
        base: addr as *mut core::ffi::c_void,
        len: dst.len(),
    };
    // SAFETY: one iovec on each side, both valid for `len` bytes; the kernel
    // copies as much as it can and never raises a fault in this thread.
    let read = unsafe { process_vm_readv(getpid(), &local, 1, &remote, 1, 0) };
    if read <= 0 {
        0
    } else {
        (read as usize).min(dst.len())
    }
}

/// Read a window of the address space, zero-filling whatever could not be read.
#[cfg(feature = "runtime")]
fn read_window(addr: usize, len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let mut done = 0usize;
    while done < len {
        let take = (len - done).min(BLOCK);
        let got = read_block(addr + done, &mut out[done..done + take]);
        if got == 0 {
            break;
        }
        done += got;
    }
    out.truncate(done);
    out
}


/// Enumerate committed, readable regions of this process.
///
/// `/proc/self/maps` cannot be used for that on Wine: opening `Z:\proc\self`
/// from a Windows process yields the **wineserver**'s map, not the game's (that
/// mistake cost one whole run: 110 regions, 51 MiB, no EOSSDK image in it).
/// `VirtualQuery` walks the NT view of the calling process, which is the one
/// the payload lives in.
#[cfg(feature = "runtime")]
#[cfg(windows)]
fn enumerate_regions() -> Vec<Region> {
    use hromatite_win::ffi::{MEM_IMAGE, MEM_MAPPED};
    hromatite_win::procs::regions(usize::MAX)
        .into_iter()
        .filter(|r| r.readable() && r.size > 0)
        .map(|r| {
            let perms = if r.protect & 0xcc != 0 {
                "rw-p"
            } else {
                "r--p"
            };
            let path = match r.ty {
                MEM_IMAGE => "nt:image",
                MEM_MAPPED => "nt:mapped",
                _ => "nt:private",
            };
            Region {
                start: r.base,
                end: r.base + r.size,
                perms: perms.to_string(),
                path: path.to_string(),
            }
        })
        .collect()
}

/// Host variant: the plain `/proc/self/maps` walk (correct off-Wine).
#[cfg(feature = "runtime")]
#[cfg(not(windows))]
fn enumerate_regions() -> Vec<Region> {
    parse_maps(&read_maps().unwrap_or_default())
}

// ------------------------------------------------------------------- matcher

struct Needle {
    name: String,
    bytes: Vec<u8>,
    prefix: u16,
}

/// Multi-needle search with a 64 Ki-entry prefix bitmap, so one pass over a
/// region finds every candidate instead of one pass per needle.
struct Matcher {
    needles: Vec<Needle>,
    mask: Vec<u64>,
    /// Address spans of the needle buffers themselves (plus the caller's key
    /// table). A hit inside one of them is the search pattern, not a find.
    spans: Vec<(usize, usize)>,
}

impl Matcher {
    fn new(needles: Vec<(String, Vec<u8>)>, spans: Vec<(usize, usize)>) -> Self {
        let mut mask = vec![0u64; 65536 / 64];
        let mut list = Vec::with_capacity(needles.len());
        let mut spans = spans;
        // The needle table's own buffer holds strings and vectors that may
        // themselves look like a match; exclude the whole allocation.
        spans.push((
            needles.as_ptr() as usize,
            needles.as_ptr() as usize + needles.len() * core::mem::size_of::<(String, Vec<u8>)>(),
        ));
        for (name, bytes) in needles {
            if bytes.len() < 8 || bytes.len() > MAX_NEEDLE {
                continue;
            }
            let prefix = u16::from_le_bytes([bytes[0], bytes[1]]);
            mask[usize::from(prefix) / 64] |= 1 << (usize::from(prefix) % 64);
            spans.push((
                bytes.as_ptr() as usize,
                bytes.as_ptr() as usize + bytes.len(),
            ));
            list.push(Needle {
                name,
                bytes,
                prefix,
            });
        }
        Self {
            needles: list,
            mask,
            spans,
        }
    }

    /// Number of excluded spans (diagnostics).
    fn span_count(&self) -> usize {
        self.spans.len()
    }

    /// The excluded spans, rendered for the log (diagnostics).
    fn span_list(&self) -> String {
        self.spans
            .iter()
            .map(|(start, end)| format!("{start:#x}-{end:#x}"))
            .collect::<Vec<_>>()
            .join(",")
    }

    /// True when `addr` points into the needle table itself.
    fn is_self(&self, addr: usize) -> bool {
        self.spans
            .iter()
            .any(|(start, end)| addr >= *start && addr < *end)
    }

    fn may_match(&self, prefix: u16) -> bool {
        self.mask[usize::from(prefix) / 64] & (1 << (usize::from(prefix) % 64)) != 0
    }

    /// Record every match of every needle inside `bytes`. `base` is the address
    /// `bytes[0]` lives at, so hits are reported as absolute addresses.
    fn find(&self, bytes: &[u8], base: usize, out: &mut Vec<Hit>) {
        if bytes.len() < 8 {
            return;
        }
        // Two lanes four bytes apart cover 4-byte aligned allocations too; key
        // material is essentially always at least 4-byte aligned.
        for lane in [0usize, 4] {
            let mut at = lane;
            while at + 8 <= bytes.len() {
                let word = u64::from_le_bytes([
                    bytes[at],
                    bytes[at + 1],
                    bytes[at + 2],
                    bytes[at + 3],
                    bytes[at + 4],
                    bytes[at + 5],
                    bytes[at + 6],
                    bytes[at + 7],
                ]);
                let prefix = word as u16;
                if self.may_match(prefix) {
                    for (index, needle) in self.needles.iter().enumerate() {
                        if needle.prefix != prefix {
                            continue;
                        }
                        let end = at + needle.bytes.len();
                        if end <= bytes.len() && bytes[at..end] == needle.bytes[..] {
                            out.push(Hit {
                                needle: index,
                                addr: base + at,
                            });
                        }
                    }
                }
                at += 8;
            }
        }
    }
}

/// One needle hit: which candidate matched and where.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    /// Index into the matcher's needle list.
    pub needle: usize,
    /// Absolute address of the first matching byte.
    pub addr: usize,
}

// --------------------------------------------------------------- frame probe

/// What the `AddNotifyMessageToServer` wrapper saw, remembered for the scan.
#[derive(Clone, Copy, Debug)]
pub struct FrameProbe {
    /// Buffer the module lent the callback.
    pub ptr: usize,
    /// Frame length.
    pub len: usize,
    /// First 16 bytes (a copy — the borrow ends with the callback).
    pub head: [u8; 16],
    /// Timestamp of the first captured big frame.
    pub ts_ms: u64,
}

static PROBE: Mutex<Option<FrameProbe>> = Mutex::new(None);
static ENABLED: AtomicU8 = AtomicU8::new(u8::MAX);

/// Cached `EAC_KEYHUNT` lookup (the flag never changes at runtime).
pub fn enabled() -> bool {
    let cached = ENABLED.load(Ordering::Relaxed);
    if cached != u8::MAX {
        return cached == 1;
    }
    let flag = std::env::var("EAC_KEYHUNT")
        .map(|value| !value.is_empty() && value != "0")
        .unwrap_or(false);
    ENABLED.store(u8::from(flag), Ordering::Relaxed);
    flag
}

/// Called from the AC callback thread: remember the first big frame. Uses
/// `try_lock`, so a missed frame is preferred to blocking the SDK.
pub fn record_frame(ptr: usize, len: usize, head: &[u8], ts_ms: u64) {
    if !enabled() || ptr == 0 || len < BIG_FRAME {
        return;
    }
    if let Ok(mut slot) = PROBE.try_lock()
        && slot.is_none()
    {
        let mut copy = [0u8; 16];
        let take = head.len().min(16);
        copy[..take].copy_from_slice(&head[..take]);
        *slot = Some(FrameProbe {
            ptr,
            len,
            head: copy,
            ts_ms,
        });
    }
}

/// Snapshot of the remembered frame, if any.
pub fn probe() -> Option<FrameProbe> {
    PROBE.lock().ok().and_then(|slot| *slot)
}

/// Fallback: take the first big frame out of the capture file.
///
/// The resident image owns `AddNotifyMessageToServer` in the two-image setup,
/// so its `PROBE` is filled while the feature image's stays empty. The capture
/// file is the shared state both images already write/read, and its first
/// 512-byte frame carries the same 12-byte session prefix the scan needs.
pub fn probe_from_capture() -> Option<FrameProbe> {
    let path = std::env::var_os("HROMATITE_EAC_CAPTURE")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("hromatite_eac_capture.bin"));
    let frames = crate::capture::read_frames(&path).ok()?;
    // A 512-byte heartbeat is what the hunt is about; any other big frame is a
    // fallback so a handshake-only session still produces something.
    let frame = frames
        .iter()
        .find(|frame| frame.data.len() == 512)
        .or_else(|| frames.iter().find(|frame| frame.data.len() >= BIG_FRAME))?;
    let frame = frame.clone();
    let mut head = [0u8; 16];
    let take = frame.data.len().min(16);
    head[..take].copy_from_slice(&frame.data[..take]);
    Some(FrameProbe {
        // The buffer address is only known to the image that ran the callback;
        // the scan finds the copies itself.
        ptr: 0,
        len: frame.data.len(),
        head,
        ts_ms: now_ms(),
    })
}

// -------------------------------------------------------------------- hunter

#[cfg(feature = "runtime")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Waiting for a big frame and the settle delay.
    Wait,
    Scan,
    /// Between the two passes.
    Settle,
    Done,
}

/// Scan driver owned by the feature generation and driven from the managed tick.
#[cfg(feature = "runtime")]
pub struct Hunter {
    enabled: bool,
    dir: PathBuf,
    chunk_bytes: usize,
    first_delay_ms: u64,
    gap_ms: u64,
    phase: Phase,
    pass: u32,
    due_ms: u64,
    regions: Vec<Region>,
    matcher: Option<Matcher>,
    /// Resume point: region index and byte offset inside it.
    cursor: (usize, usize),
    scanned: u64,
    read_bytes: u64,
    hits: Vec<Hit>,
    /// Key table kept alive for the whole pass — the matcher excludes its bytes.
    keys: Vec<(String, [u8; 32])>,
    self_hits: usize,
    seen: BTreeMap<String, usize>,
    dropped: BTreeMap<String, usize>,
    started_ms: u64,
    log_written: bool,
}

#[cfg(feature = "runtime")]
impl Default for Hunter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "runtime")]
impl Hunter {
    /// Read the flag and the output directory once.
    pub fn new() -> Self {
        let dir = std::env::var_os("EAC_KEYHUNT_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::temp_dir().join("hromatite_keyhunt"));
        let chunk_mb = std::env::var("EAC_KEYHUNT_CHUNK_MB")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_CHUNK_MB);
        Self {
            enabled: enabled(),
            dir,
            chunk_bytes: chunk_mb * 1024 * 1024,
            first_delay_ms: env_u64("EAC_KEYHUNT_DELAY_MS", DEFAULT_FIRST_DELAY_MS),
            gap_ms: env_u64("EAC_KEYHUNT_GAP_MS", DEFAULT_GAP_MS),
            phase: Phase::Wait,
            pass: 0,
            due_ms: 0,
            regions: Vec::new(),
            matcher: None,
            cursor: (0, 0),
            scanned: 0,
            read_bytes: 0,
            hits: Vec::new(),
            keys: Vec::new(),
            self_hits: 0,
            seen: BTreeMap::new(),
            dropped: BTreeMap::new(),
            started_ms: 0,
            log_written: false,
        }
    }

    /// True when `EAC_KEYHUNT=1` was set at construction.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Output directory (created on first write).
    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    fn log_path(&self) -> PathBuf {
        self.dir.join("keyhunt.log")
    }

    fn append(&self, line: &str) {
        let record = format!("[{:>10}] {line}\r\n", now_ms() % 100_000_000);
        if fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path())
        {
            let _ = file.write_all(record.as_bytes());
        }
    }

    /// Advance the hunt. Returns console lines (at most a couple per tick).
    pub fn tick(&mut self) -> Vec<String> {
        if !self.enabled || self.phase == Phase::Done {
            return Vec::new();
        }
        let now = now_ms();
        let mut lines = Vec::new();
        match self.phase {
            Phase::Wait => {
                let Some(frame) = probe().or_else(probe_from_capture) else {
                    return lines;
                };
                if self.started_ms == 0 {
                    self.started_ms = now;
                    self.due_ms = now + self.first_delay_ms;
                    let mut line = format!(
                        "keyhunt: frame ptr={:#x} len={} head={} dir={}",
                        frame.ptr,
                        frame.len,
                        hex(&frame.head),
                        self.dir.display()
                    );
                    self.append(&line);
                    line.insert_str(0, "eac: ");
                    lines.push(line);
                }
                if now < self.due_ms {
                    return lines;
                }
                self.start_pass(&mut lines);
            }
            Phase::Settle => {
                if now >= self.due_ms {
                    self.start_pass(&mut lines);
                }
            }
            Phase::Scan => {
                let budget = self.chunk_bytes;
                if self.scan_chunk(budget) {
                    self.finish_pass(&mut lines);
                }
            }
            Phase::Done => {}
        }
        lines
    }

    fn start_pass(&mut self, lines: &mut Vec<String>) {
        self.pass += 1;
        self.phase = Phase::Scan;
        self.cursor = (0, 0);
        self.scanned = 0;
        self.read_bytes = 0;
        self.hits.clear();
        self.self_hits = 0;
        self.seen.clear();
        self.dropped.clear();

        let text = read_maps().unwrap_or_default();
        self.regions = enumerate_regions();
        // One copy of the map per pass: the report needs to show which mapping
        // every hit belongs to.
        if !text.is_empty() && fs::create_dir_all(&self.dir).is_ok() {
            let path = self.dir.join(format!("maps_pass{}.txt", self.pass));
            let _ = fs::write(path, &text);
        }

        self.keys = candidate_keys();
        let mut spans: Vec<(usize, usize)> = self
            .keys
            .iter()
            .map(|(_, key)| (key.as_ptr() as usize, key.as_ptr() as usize + 32))
            .collect();
        // The key table itself: every candidate is laid out inline, so its
        // whole buffer is a match target for the very bytes it holds.
        spans.push((
            self.keys.as_ptr() as usize,
            self.keys.as_ptr() as usize
                + self.keys.len() * core::mem::size_of::<(String, [u8; 32])>(),
        ));
        let mut needles: Vec<(String, Vec<u8>)> = self
            .keys
            .iter()
            .map(|(name, key)| (name.clone(), key.to_vec()))
            .collect();
        let mut frame_note = "none".to_string();
        if let Some(frame) = probe().or_else(probe_from_capture) {
            // 12 bytes: type/flags, SessionId, and the always-zero padding. The
            // per-frame counter at +12 deliberately stays out so older copies of
            // the same session's frames still match.
            needles.push(("frame-head".to_string(), frame.head[..12].to_vec()));
            frame_note = hex(&frame.head[..12]);
        }
        self.matcher = Some(Matcher::new(needles, spans));

        let total: usize = self
            .regions
            .iter()
            .filter(|region| region.readable())
            .map(Region::len)
            .sum();
        let line = format!(
            "keyhunt: pass {} start regions={} readable_bytes={} needles={} frame={} chunk_mb={} self_spans={}",
            self.pass,
            self.regions.len(),
            total,
            self.matcher
                .as_ref()
                .map_or(0, |matcher| matcher.needles.len()),
            frame_note,
            self.chunk_bytes / (1024 * 1024),
            self.matcher.as_ref().map_or(0, Matcher::span_count)
        );
        self.append(&line);
        if std::env::var_os("EAC_KEYHUNT_TRACE").is_some()
            && let Some(matcher) = self.matcher.as_ref()
        {
            let spans = matcher.span_list();
            self.append(&format!("keyhunt: self spans {spans}"));
        }
        self.probe_frame_buffer();

        let mut console = line;
        console.insert_str(0, "eac: ");
        lines.push(console);
    }

    /// Region info + a 1 KiB window around the buffer the module handed over.
    fn probe_frame_buffer(&self) {
        let Some(frame) = probe().filter(|frame| frame.ptr != 0).or_else(probe_from_capture)
        else {
            return;
        };
        if frame.ptr == 0 {
            self.append(&format!(
                "keyhunt: frame-buffer unknown (frame taken from capture, len={} head={})",
                frame.len,
                hex(&frame.head[..12])
            ));
            return;
        }
        let region = self
            .regions
            .iter()
            .find(|region| frame.ptr >= region.start && frame.ptr < region.end);
        match region {
            Some(region) => self.append(&format!(
                "keyhunt: frame-buffer {:#x} len={} region={:#x}..{:#x} perms={} path={} offset={:#x}",
                frame.ptr,
                frame.len,
                region.start,
                region.end,
                region.perms,
                region.path,
                frame.ptr - region.start
            )),
            None => self.append(&format!(
                "keyhunt: frame-buffer {:#x} len={} region=not-in-maps",
                frame.ptr, frame.len
            )),
        }
        // Window: 256 B before the frame, 768 B after it — enough to see the
        // allocation header, the length fields and any key material kept next
        // to the frame in the same struct.
        let before = 256usize;
        let after = 768usize;
        let start = frame.ptr.saturating_sub(before);
        let end = frame.ptr + frame.len + after;
        let clamped_start = region.map_or(start, |region| start.max(region.start));
        let clamped_end = region.map_or(end, |region| end.min(region.end));
        if clamped_end <= clamped_start {
            return;
        }
        let window = read_window(clamped_start, clamped_end - clamped_start);
        if window.is_empty() {
            return;
        }
        if fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        let tag = format!("pass{}", self.pass);
        let _ = fs::write(
            self.dir.join(format!("frame_buffer_{tag}.txt")),
            hexdump(&window, clamped_start),
        );
        let _ = fs::write(self.dir.join(format!("frame_buffer_{tag}.bin")), &window);
        self.append(&format!(
            "keyhunt: frame-buffer window {:#x}..{:#x} dumped ({} B)",
            clamped_start,
            clamped_end,
            window.len()
        ));
    }

    /// Scan at most `budget` bytes. Returns true when every readable region was
    /// covered.
    fn scan_chunk(&mut self, budget: usize) -> bool {
        let mut left = budget;
        let mut buffer = vec![0u8; BLOCK];
        while left > 0 {
            let Some(region) = self.regions.get(self.cursor.0).cloned() else {
                return true;
            };
            if !region.readable() {
                self.cursor = (self.cursor.0 + 1, 0);
                continue;
            }
            // Overlap the previous chunk by 64 bytes so a needle straddling the
            // boundary is still seen; duplicates are filtered when logging.
            let base = region.start + self.cursor.1;
            let take = region.end.saturating_sub(base).min(left);
            if take == 0 {
                self.cursor = (self.cursor.0 + 1, 0);
                continue;
            }
            let from = base.saturating_sub(64).max(region.start);
            let to = base + take;
            if std::env::var_os("EAC_KEYHUNT_TRACE").is_some() {
                eprintln!(
                    "[trace] {from:#x}..{to:#x} perms={} path={}",
                    region.perms, region.path
                );
            }
            let mut at = from;
            while at < to && !buffer.is_empty() {
                let want = (to - at).min(BLOCK);
                let got = read_block(at, &mut buffer[..want]);
                if got == 0 {
                    break;
                }
                let mut hits = Vec::new();
                if let Some(matcher) = self.matcher.as_ref() {
                    matcher.find(&buffer[..got], at, &mut hits);
                }
                self.read_bytes += got as u64;
                for hit in hits {
                    self.note_hit(hit, &region);
                }
                at += got;
            }
            self.scanned += take as u64;
            left -= take;
            self.cursor.1 += take;
            if self.cursor.1 >= region.len() {
                self.cursor = (self.cursor.0 + 1, 0);
            }
        }
        self.cursor.0 >= self.regions.len()
    }

    fn note_hit(&mut self, hit: Hit, region: &Region) {
        if self
            .matcher
            .as_ref()
            .is_some_and(|matcher| matcher.is_self(hit.addr))
        {
            self.self_hits += 1;
            return;
        }
        let name = self
            .matcher
            .as_ref()
            .and_then(|matcher| matcher.needles.get(hit.needle))
            .map(|needle| needle.name.clone())
            .unwrap_or_else(|| format!("needle#{}", hit.needle));
        // A chunk overlap can report the same address twice.
        if self
            .hits
            .iter()
            .any(|known| known.addr == hit.addr && known.needle == hit.needle)
        {
            return;
        }
        self.hits.push(hit);
        let seen = {
            let entry = self.seen.entry(name.clone()).or_insert(0);
            *entry += 1;
            *entry
        };
        if seen > HIT_CAP {
            *self.dropped.entry(name).or_insert(0) += 1;
            return;
        }
        // Re-read the match in place: it proves the address and gives the
        // report the bytes to reason about.
        let width = self
            .matcher
            .as_ref()
            .and_then(|matcher| matcher.needles.get(hit.needle))
            .map_or(32, |needle| needle.bytes.len().min(32));
        let bytes = read_window(hit.addr, width);
        self.append(&format!(
            "keyhunt: hit needle={} addr={:#x} bytes={} region={:#x}..{:#x} perms={} path={} offset={:#x}",
            name,
            hit.addr,
            hex(&bytes),
            region.start,
            region.end,
            region.perms,
            region.path,
            hit.addr - region.start
        ));
        if seen <= 8 {
            self.dump_around(&name, hit.addr, seen, region);
        }
    }

    /// 512 B before / 512 B after a hit, as hex + ASCII.
    fn dump_around(&self, name: &str, addr: usize, index: usize, region: &Region) {
        let start = addr.saturating_sub(512).max(region.start);
        let end = (addr + 512).min(region.end);
        if end <= start || fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        let window = read_window(start, end - start);
        if window.is_empty() {
            return;
        }
        let tag = format!("pass{}_{}_{}", self.pass, sanitise(name), index);
        let _ = fs::write(
            self.dir.join(format!("hit_{tag}.txt")),
            hexdump(&window, start),
        );
        let _ = fs::write(self.dir.join(format!("hit_{tag}.bin")), window);
    }

    fn finish_pass(&mut self, lines: &mut Vec<String>) {
        let summary = self
            .seen
            .iter()
            .map(|(name, count)| format!("{name}={count}"))
            .collect::<Vec<_>>();
        let dropped = self
            .dropped
            .iter()
            .map(|(name, count)| format!("{name}+{count}"))
            .collect::<Vec<_>>();
        let line = format!(
            "keyhunt: pass {} done scanned={} MiB read={} MiB self={} hits: {} droplog: {}",
            self.pass,
            self.scanned / (1024 * 1024),
            self.read_bytes / (1024 * 1024),
            self.self_hits,
            if summary.is_empty() {
                "none".to_string()
            } else {
                summary.join(" ")
            },
            if dropped.is_empty() {
                "-".to_string()
            } else {
                dropped.join(" ")
            }
        );
        self.append(&line);
        let mut console = line;
        console.insert_str(0, "eac: ");
        lines.push(console);
        self.log_written = true;
        if self.pass >= 2 {
            self.phase = Phase::Done;
        } else {
            self.phase = Phase::Settle;
            self.due_ms = now_ms() + self.gap_ms;
        }
    }
}

fn env_u64(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(fallback)
}

fn sanitise(name: &str) -> String {
    name.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect()
}

/// Hex + ASCII dump with absolute addresses.
pub fn hexdump(bytes: &[u8], base: usize) -> String {
    let mut out = String::with_capacity(bytes.len() * 4);
    for (index, chunk) in bytes.chunks(16).enumerate() {
        let at = base + index * 16;
        out.push_str(&format!("{at:016x}  "));
        for byte in chunk {
            out.push_str(&format!("{byte:02x} "));
        }
        for _ in chunk.len()..16 {
            out.push_str("   ");
        }
        out.push_str(" |");
        for byte in chunk {
            out.push(if byte.is_ascii_graphic() || *byte == b' ' {
                char::from(*byte)
            } else {
                '.'
            });
        }
        out.push_str("|\n");
    }
    out
}

/// Find `needle` inside one already-known-readable region (used by tests and by
/// the report tooling).
#[cfg(feature = "runtime")]
pub fn find_in_region(region: &Region, needle: &[u8]) -> Vec<usize> {
    if !region.readable() || region.len() < 8 {
        return Vec::new();
    }
    let bytes = read_window(region.start, region.len());
    if bytes.is_empty() {
        return Vec::new();
    }
    let matcher = Matcher::new(vec![("probe".to_string(), needle.to_vec())], Vec::new());
    let mut hits = Vec::new();
    matcher.find(&bytes, region.start, &mut hits);
    hits.into_iter().map(|hit| hit.addr).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = include_str!("../tests/fixtures/maps_sample.txt");

    #[test]
    fn maps_parse_addresses_perms_and_paths() {
        let regions = parse_maps(ROOT);
        assert_eq!(regions.len(), 4);
        assert_eq!(regions[0].start, 0x55d2_c000_0000);
        assert_eq!(regions[0].perms, "r--p");
        assert_eq!(regions[0].path, "/home/u/.steam/VRChat.exe");
        assert_eq!(regions[2].perms, "rw-p");
        assert!(regions[2].path.starts_with("[heap]"));
        assert_eq!(regions[3].path, "[anon]");
        assert!(regions[1].is_eac());
        assert!(!regions[0].is_eac());
        assert!(regions[0].readable());
        assert!(
            !parse_maps("garbage line\n")
                .iter()
                .any(|region| region.readable())
        );
    }

    #[test]
    fn matcher_finds_every_candidate_including_unaligned_copies() {
        let keys = candidate_keys();
        let (name, key) = &keys[3];
        let mut hay = vec![0u8; 4096];
        hay[1004..1036].copy_from_slice(key);
        let matcher = Matcher::new(
            vec![(name.clone(), key.to_vec()), ("zz".into(), vec![9u8; 8])],
            Vec::new(),
        );
        let mut hits = Vec::new();
        matcher.find(&hay, 0x1000, &mut hits);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].addr, 0x1000 + 1004);
        assert_eq!(matcher.needles[hits[0].needle].name, *name);
    }

    #[cfg(feature = "runtime")]
    #[test]
    fn matcher_finds_a_needle_in_live_memory() {
        let needle = b"hromatite-keyhunt-needle-0123456";
        let buffer = needle.to_vec();
        let region = Region {
            start: buffer.as_ptr() as usize,
            end: buffer.as_ptr() as usize + buffer.len(),
            perms: "rw-p".to_string(),
            path: "[test]".to_string(),
        };
        let hits = find_in_region(&region, needle);
        assert_eq!(hits, vec![buffer.as_ptr() as usize]);
    }

    #[test]
    fn hexdump_is_addressed_and_bounded() {
        let text = hexdump(&[0x41, 0x42], 0x1000);
        assert!(text.starts_with("0000000000001000  41 42 "));
        assert!(text.contains("|AB|"));
    }

    #[test]
    fn seed_key_is_the_re_constant_and_derivations_are_distinct() {
        assert_eq!(
            hex(&SEED_KEY),
            "b68d74704c4e9e43d491ef367345373db742514ae1eb3bb9a167a30b3a35c7ea"
        );
        let keys = candidate_keys();
        let mut seen: Vec<[u8; 32]> = Vec::new();
        for (_, key) in &keys {
            assert!(!seen.contains(key), "duplicate candidate");
            seen.push(*key);
        }
    }
}
