//! `EAC_REPLAY_REPORT=<path>` — replay a whole recorded 512-byte report.
//!
//! `EAC_FORGE=1` rebuilds *one* frame from the last live one, so every
//! synthesized heartbeat carries the same 488-byte fragment. A recorded report
//! (117 frames of one `SessionId` in
//! `evidence/report_full/report_a_sid158b.bin`) is a *sequence*: frame 0 starts
//! the message (`Flags & 1`), frames 1.. continue it, and the body bytes differ
//! from frame to frame. Replaying that sequence in order is the next-cheapest
//! hypothesis after "one fragment forever".
//!
//! # Source format
//!
//! The capture format of this crate ([`crate::capture`]): records of
//! `u32 len | u32 dt_ms | len bytes`, back to back, `len == 512` for every
//! heartbeat. Reading tolerates a truncated tail exactly like the capture
//! reader: a crash mid-write loses only the last record.
//!
//! # What is rewritten
//!
//! The recorded frame is used byte for byte except for the three fields that
//! belong to the *live* session (see [`crate::forge`] for the layout):
//!
//! ```text
//! [2:4]   u16 Flags     kept from the recording unless EAC_REPLAY_FLAGS=live
//! [4:12]  u64 SessionId always the live template's session
//! [12:20] u64 Sequence  the generator's running counter (continues the live one)
//! [20:24] u32 CRC-32C   recomputed over the retargeted frame
//! ```
//!
//! Everything else — `Version`, the 488-byte body, its inner checksums — is the
//! recorded bytes, which is the whole point: those are what the live session
//! could not produce.
//!
//! Pure `std`, no atomics and no FFI: `eos` owns the clock, the environment and
//! the delivery, this file owns the bytes.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use crate::capture::Frame;
use crate::digest::crc32c_eac;
use crate::forge::{CHECKSUM_OFFSET, FRAME_LEN, HEADER_LEN};

/// Default `EAC_REPLAY_AFTER_MS`: how long the live stream stays real.
pub const DEFAULT_AFTER_MS: u64 = 30_000;
/// Default `EAC_REPLAY_MIN_FRAMES`: live 512-byte frames learned first, because
/// the template of the live session supplies the `SessionId` to retarget to.
pub const DEFAULT_MIN_FRAMES: u64 = 20;

/// A parsed report: only the 512-byte frames, in recorded order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    frames: Vec<Frame>,
    /// Records whose `len` was not [`FRAME_LEN`] (handshake fragments, ACKs).
    skipped: usize,
    /// A record header or payload was cut off by the end of the file.
    truncated: bool,
}

impl Report {
    /// Parse a report from raw capture bytes.
    ///
    /// Non-512-byte records are counted in [`Report::skipped`] rather than
    /// rejected: a report capture may legitimately start with a handshake
    /// fragment, and silently dropping it keeps `idx` aligned with the
    /// heartbeat stream only.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        let mut report = Self::default();
        let mut at = 0usize;
        while at + 8 <= bytes.len() {
            let len = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes")) as usize;
            let dt_ms = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().expect("4 bytes"));
            let start = at + 8;
            let Some(end) = start.checked_add(len).filter(|end| *end <= bytes.len()) else {
                report.truncated = true;
                break;
            };
            if len == FRAME_LEN {
                report.frames.push(Frame {
                    dt_ms,
                    data: bytes[start..end].to_vec(),
                });
            } else {
                report.skipped += 1;
            }
            at = end;
        }
        report.truncated = report.truncated || at < bytes.len();
        report
    }

    /// Read and parse a report file.
    pub fn load(path: impl AsRef<Path>) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(Self::from_bytes(&bytes))
    }

    /// Number of replayable 512-byte frames.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// True when the file held no 512-byte frame at all.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Records that were not 512-byte frames.
    pub fn skipped(&self) -> usize {
        self.skipped
    }

    /// True when the file ended in the middle of a record.
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Message fragment of frame `index` — everything after the 24-byte
    /// header, i.e. the bytes replay must preserve.
    pub fn body(&self, index: usize) -> Option<&[u8]> {
        self.frames
            .get(index)
            .map(|frame| &frame.data[HEADER_LEN..])
    }

    /// Recorded inter-arrival times, for the log and the tests.
    pub fn dt_ms(&self) -> impl Iterator<Item = u32> + '_ {
        self.frames.iter().map(|frame| frame.dt_ms)
    }
}

/// One `EAC_REPLAY_*` frame handed out by a [`Stream`], already copied out of
/// the report so the caller can retarget it without holding the lock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    /// Zero-based index inside the report.
    pub index: usize,
    /// Frames in the report (`idx of of` in the log line).
    pub of: usize,
    /// Recorded `dt_ms` of this frame.
    pub dt_ms: u32,
    /// The recorded 512 bytes, untouched.
    pub data: Vec<u8>,
}

/// Cursor over a [`Report`]: hands out frames in order and restarts at `start`
/// when looping.
#[derive(Debug)]
pub struct Stream {
    report: Report,
    start: usize,
    cursor: usize,
    looping: bool,
    finished: bool,
    wraps: u64,
}

impl Stream {
    /// A stream starting at `start` (clamped into the report) that either
    /// restarts at `start` once the report ends (`looping`) or stops.
    pub fn new(report: Report, start: usize, looping: bool) -> Self {
        let start = if report.frames.is_empty() {
            0
        } else {
            start.min(report.frames.len() - 1)
        };
        Self {
            report,
            start,
            cursor: start,
            looping,
            finished: false,
            wraps: 0,
        }
    }

    /// Frames in the report.
    pub fn total(&self) -> usize {
        self.report.len()
    }

    /// Records the report carried that were not 512-byte frames.
    pub fn skipped(&self) -> usize {
        self.report.skipped()
    }

    /// Index the next [`Stream::next_frame`] would hand out.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// Times the report was restarted (`EAC_REPLAY_REPORT_LOOP=1`).
    pub fn wraps(&self) -> u64 {
        self.wraps
    }

    /// True when the stream restarts instead of stopping at the last frame.
    pub fn looping(&self) -> bool {
        self.looping
    }

    /// Frames left before the report would end.
    pub fn remaining(&self) -> usize {
        self.report.frames.len().saturating_sub(self.cursor)
    }

    /// True once a non-looping stream ran out of frames.
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Rewind to the `start` index for a new session epoch: a recorded report
    /// belongs to one `SessionId`, so the next room starts it over.
    pub fn reset(&mut self) {
        self.cursor = self.start;
        self.finished = false;
        self.wraps = 0;
    }

    /// Take the next recorded frame.
    ///
    /// Returns `None` only when the report is empty or a non-looping stream is
    /// exhausted; the first `None` sets [`Stream::finished`], later calls keep
    /// returning `None` without further logging on the caller's side.
    pub fn next_frame(&mut self) -> Option<Slot> {
        if self.report.frames.is_empty() {
            self.finished = true;
            return None;
        }
        if self.cursor >= self.report.frames.len() {
            if !self.looping {
                self.finished = true;
                return None;
            }
            self.cursor = self.start;
            self.wraps += 1;
        }
        let index = self.cursor;
        self.cursor += 1;
        let frame = &self.report.frames[index];
        Some(Slot {
            index,
            of: self.report.frames.len(),
            dt_ms: frame.dt_ms,
            data: frame.data.clone(),
        })
    }
}

/// Rewrite a recorded frame for the live session and fix its checksum.
///
/// `flags`: `None` keeps the recording's `Flags`, `Some` overrides them
/// (`EAC_REPLAY_FLAGS=live` passes the live template's value). Returns `None`
/// for anything that is not a 512-byte frame — a 24-byte ACK cannot be a
/// heartbeat.
pub fn retarget(frame: &[u8], session_id: u64, seq: u64, flags: Option<u16>) -> Option<Vec<u8>> {
    if frame.len() != FRAME_LEN {
        return None;
    }
    let mut out = frame.to_vec();
    if let Some(flags) = flags {
        out[2..4].copy_from_slice(&flags.to_le_bytes());
    }
    out[4..12].copy_from_slice(&session_id.to_le_bytes());
    out[12..20].copy_from_slice(&seq.to_le_bytes());
    out[CHECKSUM_OFFSET..HEADER_LEN].fill(0);
    let crc = crc32c_eac(&out);
    out[CHECKSUM_OFFSET..HEADER_LEN].copy_from_slice(&crc.to_le_bytes());
    Some(out)
}

/// `EAC_REPLAY_*` knobs, read once at install time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    /// `EAC_REPLAY_REPORT`: the record file to replay.
    pub path: PathBuf,
    /// `EAC_REPLAY_REPORT_LOOP=1`: restart at [`Config::start`] when the report
    /// ends instead of stopping.
    pub looping: bool,
    /// `EAC_REPLAY_FLAGS=live`: take `Flags` from the live template instead of
    /// the recording.
    pub flags_live: bool,
    /// `EAC_REPLAY_START`: first frame index to replay.
    pub start: usize,
    /// `EAC_REPLAY_AFTER_MS`, default [`DEFAULT_AFTER_MS`]; 0 disables.
    pub after_ms: u64,
    /// `EAC_REPLAY_MIN_FRAMES`, default [`DEFAULT_MIN_FRAMES`].
    pub min_frames: u64,
}

impl Config {
    /// Parse the environment; `None` when `EAC_REPLAY_REPORT` is absent or
    /// blank (then the generator keeps building frames from the template).
    ///
    /// Takes a lookup so the function is testable without the process
    /// environment, exactly like [`crate::forge::Config::parse`].
    pub fn parse(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let path = get("EAC_REPLAY_REPORT")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())?;
        let flag = |name: &str| {
            get(name)
                .map(|value| {
                    let value = value.trim();
                    !value.is_empty() && value != "0"
                })
                .unwrap_or(false)
        };
        let number = |name: &str, default: u64| {
            get(name)
                .and_then(|value| value.trim().parse::<u64>().ok())
                .unwrap_or(default)
        };
        Some(Self {
            path: PathBuf::from(path),
            looping: flag("EAC_REPLAY_REPORT_LOOP"),
            flags_live: get("EAC_REPLAY_FLAGS")
                .map(|value| value.trim().eq_ignore_ascii_case("live"))
                .unwrap_or(false),
            start: number("EAC_REPLAY_START", 0) as usize,
            after_ms: number("EAC_REPLAY_AFTER_MS", DEFAULT_AFTER_MS),
            min_frames: number("EAC_REPLAY_MIN_FRAMES", DEFAULT_MIN_FRAMES),
        })
    }
}

/// Record one frame in the capture format (used by the tests and by anything
/// that has to build a report fixture).
pub fn record(out: &mut Vec<u8>, dt_ms: u32, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&dt_ms.to_le_bytes());
    out.extend_from_slice(data);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::{BodyMode, Template, build_frame};

    /// A synthetic 512-byte frame whose body is unique per `index`.
    fn frame_at(session: u64, seq: u64, flags: u16, tag: u8) -> Vec<u8> {
        let template = Template {
            session_id: session,
            flags,
            last_seq: seq,
            body: vec![tag; FRAME_LEN - HEADER_LEN],
        };
        build_frame(&template, seq, BodyMode::Stale)
    }

    /// Three recorded frames of `session`, the first one starting a message.
    fn sample_report(session: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (index, tag) in [0x11u8, 0x22, 0x33].into_iter().enumerate() {
            let flags = if index == 0 { 1 } else { 0 };
            record(
                &mut bytes,
                if index == 0 { 0 } else { 2_007 },
                &frame_at(session, index as u64 + 1, flags, tag),
            );
        }
        bytes
    }

    #[test]
    fn a_report_loads_every_512_byte_record_in_order() {
        let report = Report::from_bytes(&sample_report(0x158b));
        assert_eq!(report.len(), 3);
        assert_eq!(report.skipped(), 0);
        assert!(!report.truncated());
        assert_eq!(report.dt_ms().collect::<Vec<_>>(), vec![0, 2_007, 2_007]);
        let stream = Stream::new(report, 0, false);
        assert_eq!(stream.total(), 3);
    }

    #[test]
    fn short_records_are_skipped_and_a_truncated_tail_is_tolerated() {
        let mut bytes = Vec::new();
        record(&mut bytes, 0, &[0xAA; 24]); // handshake ACK
        record(&mut bytes, 2_007, &frame_at(0x158b, 2, 0, 0x44));
        record(&mut bytes, 2_007, &[0xBB; 144]); // protect frame
        // A record header with no payload behind it.
        bytes.extend_from_slice(&512u32.to_le_bytes());
        bytes.extend_from_slice(&2_007u32.to_le_bytes());

        let report = Report::from_bytes(&bytes);
        assert_eq!(report.len(), 1, "only the 512-byte frame is replayable");
        assert_eq!(report.skipped(), 2);
        assert!(report.truncated());
        assert_eq!(report.frames[0].dt_ms, 2_007);
    }

    #[test]
    fn retargeting_moves_the_live_identity_in_and_fixes_the_crc() {
        let recorded = frame_at(0x158b, 7, 0x0000, 0x55);
        let live_sid = 0x0000_0000_00ab_cdefu64;
        let out = retarget(&recorded, live_sid, 900, None).expect("512-byte frame");

        assert_eq!(out.len(), FRAME_LEN);
        assert_eq!(&out[0..2], &recorded[0..2], "version is the recording's");
        assert_eq!(&out[2..4], &0u16.to_le_bytes(), "flags kept by default");
        assert_eq!(&out[4..12], &live_sid.to_le_bytes(), "sid is the live one");
        assert_eq!(
            &out[12..20],
            &900u64.to_le_bytes(),
            "seq is the generator's"
        );
        assert_eq!(
            &out[HEADER_LEN..],
            &recorded[HEADER_LEN..],
            "body untouched"
        );

        let mut zeroed = out.clone();
        zeroed[CHECKSUM_OFFSET..HEADER_LEN].fill(0);
        assert_eq!(
            crc32c_eac(&zeroed),
            u32::from_le_bytes(
                out[CHECKSUM_OFFSET..HEADER_LEN]
                    .try_into()
                    .expect("4 bytes")
            ),
            "the stored CRC is CRC-32C over the retargeted frame"
        );

        let live = retarget(&recorded, live_sid, 900, Some(0x0001)).expect("512-byte frame");
        assert_eq!(&live[2..4], &1u16.to_le_bytes(), "flags override");
        let mut zeroed = live.clone();
        zeroed[CHECKSUM_OFFSET..HEADER_LEN].fill(0);
        assert_eq!(
            crc32c_eac(&zeroed),
            u32::from_le_bytes(
                live[CHECKSUM_OFFSET..HEADER_LEN]
                    .try_into()
                    .expect("4 bytes")
            ),
            "an overridden flag is covered by the CRC too"
        );

        assert!(
            retarget(&[0u8; 24], live_sid, 1, None).is_none(),
            "24-byte ACK"
        );
    }

    #[test]
    fn a_stream_walks_the_report_in_order_and_then_stops() {
        let report = Report::from_bytes(&sample_report(0x158b));
        let mut stream = Stream::new(report, 1, false);
        let first = stream.next_frame().expect("frame 1");
        assert_eq!(first.index, 1, "EAC_REPLAY_START skips frame 0");
        assert_eq!(first.of, 3);
        assert_eq!(first.dt_ms, 2_007);
        assert_eq!(stream.next_frame().expect("frame 2").index, 2);
        assert!(stream.next_frame().is_none(), "no loop: the stream stops");
        assert!(stream.finished());
        assert!(stream.next_frame().is_none(), "stays stopped");
        assert_eq!(stream.wraps(), 0);
    }

    #[test]
    fn a_looping_stream_restarts_at_the_start_index() {
        let report = Report::from_bytes(&sample_report(0x158b));
        let mut stream = Stream::new(report, 1, true);
        let indices: Vec<usize> = (0..5)
            .map(|_| stream.next_frame().expect("frame").index)
            .collect();
        assert_eq!(indices, vec![1, 2, 1, 2, 1], "wraps back to start, not 0");
        assert_eq!(stream.wraps(), 2);
        assert!(!stream.finished());
    }

    #[test]
    fn resetting_a_stream_rewinds_to_the_start_index() {
        let report = Report::from_bytes(&sample_report(0x158b));
        let mut stream = Stream::new(report, 1, false);
        assert!(stream.next_frame().is_some());
        assert!(stream.next_frame().is_some());
        assert!(stream.next_frame().is_none());
        stream.reset();
        assert!(!stream.finished());
        assert_eq!(stream.next_frame().expect("rewound").index, 1);
    }

    #[test]
    fn an_empty_report_never_yields_a_frame() {
        let mut stream = Stream::new(Report::from_bytes(&[]), 0, true);
        assert!(stream.next_frame().is_none());
        assert!(stream.finished());
        assert_eq!(stream.total(), 0);
    }

    #[test]
    fn the_config_defaults_to_the_documented_knobs() {
        assert!(Config::parse(|_| None).is_none(), "no path, no mode");

        let config = Config::parse(|name| match name {
            "EAC_REPLAY_REPORT" => Some(" /tmp/a.bin ".to_string()),
            _ => None,
        })
        .expect("path set");
        assert_eq!(config.path, PathBuf::from("/tmp/a.bin"));
        assert_eq!(config.after_ms, DEFAULT_AFTER_MS);
        assert_eq!(config.min_frames, DEFAULT_MIN_FRAMES);
        assert_eq!(config.start, 0);
        assert!(!config.looping);
        assert!(!config.flags_live);

        let explicit = Config::parse(|name| match name {
            "EAC_REPLAY_REPORT" => Some("/tmp/a.bin".to_string()),
            "EAC_REPLAY_REPORT_LOOP" => Some("1".to_string()),
            "EAC_REPLAY_FLAGS" => Some("live".to_string()),
            "EAC_REPLAY_START" => Some("5".to_string()),
            "EAC_REPLAY_AFTER_MS" => Some("1000".to_string()),
            "EAC_REPLAY_MIN_FRAMES" => Some("3".to_string()),
            _ => None,
        })
        .expect("path set");
        assert!(explicit.looping);
        assert!(explicit.flags_live);
        assert_eq!(explicit.start, 5);
        assert_eq!(explicit.after_ms, 1_000);
        assert_eq!(explicit.min_frames, 3);
    }

    #[test]
    fn the_shipped_evidence_report_parses_when_present() {
        let path = Path::new("/home/wilpdrake/EAC-RE/evidence/report_full/report_a_sid158b.bin");
        if !path.exists() {
            return; // the evidence tree is optional on other machines
        }
        let report = Report::load(path).expect("report");
        assert_eq!(report.len(), 117, "whole capture, no skipped records");
        assert_eq!(report.skipped(), 0);
        assert!(!report.truncated());
        assert_eq!(report.dt_ms().next(), Some(0));
        let first = &report.frames[0];
        assert_eq!(
            u16::from_le_bytes([first.data[2], first.data[3]]),
            1,
            "StartData"
        );
        assert_eq!(
            u64::from_le_bytes(first.data[4..12].try_into().expect("8 bytes")),
            0x158b
        );

        // The whole recorded stream retargets to another session: every frame
        // keeps its body, takes the new identity and gets a CRC that verifies.
        let mut stream = Stream::new(Report::load(path).expect("report"), 0, false);
        let mut bodies = std::collections::HashSet::new();
        let mut count = 0usize;
        while let Some(slot) = stream.next_frame() {
            let out = retarget(&slot.data, 0x42, 1_000 + count as u64, None).expect("frame");
            assert_eq!(&out[HEADER_LEN..], &slot.data[HEADER_LEN..], "body kept");
            assert_eq!(
                u64::from_le_bytes(out[4..12].try_into().expect("8 bytes")),
                0x42,
                "frame {count} retargeted"
            );
            let mut zeroed = out.clone();
            zeroed[CHECKSUM_OFFSET..HEADER_LEN].fill(0);
            assert_eq!(
                crc32c_eac(&zeroed),
                u32::from_le_bytes(
                    out[CHECKSUM_OFFSET..HEADER_LEN]
                        .try_into()
                        .expect("4 bytes")
                ),
                "frame {count} checksum"
            );
            bodies.insert(out[HEADER_LEN..].to_vec());
            count += 1;
        }
        assert_eq!(count, 117, "every recorded frame replays");
        assert!(bodies.len() > 1, "the report's bodies actually differ");
    }
}
