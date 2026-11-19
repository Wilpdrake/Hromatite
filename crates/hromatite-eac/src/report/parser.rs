//! Reading a recorded report: [`Report`] and the capture-format reader.
//!
//! The capture format of this crate ([`crate::capture`]): records of
//! `u32 len | u32 dt_ms | len bytes`, back to back, `len == 512` for every
//! heartbeat. Reading tolerates a truncated tail exactly like the capture
//! reader: a crash mid-write loses only the last record.
//!
//! A recording is a *sequence*, not one frame: frame 0 starts the message
//! (`Flags & 1`), frames 1.. continue it, and the body bytes differ from frame
//! to frame. That is the material the replay layer hands out unchanged and the
//! generator rebuilds.

use std::fs::File;
use std::io::{self, Read};
use std::ops::Range;
use std::path::Path;

use crate::capture::Frame;
use crate::forge::{self, FLAG_START, FRAME_LEN, HEADER_LEN, StreamConfig};

use super::generator::GenConfig;

/// A parsed report: only the 512-byte frames, in recorded order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// `pub(super)` so `generator`/`stream` read the recording without a
    /// second copy of the frames.
    pub(super) frames: Vec<Frame>,
    /// Records whose `len` was not [`FRAME_LEN`] (handshake fragments, ACKs).
    skipped: usize,
    /// A record header or payload was cut off by the end of the file.
    truncated: bool,
    /// Frame ranges of the messages the recording holds, split on `StartData`.
    pub(super) messages: Vec<Range<usize>>,
    /// Set when `EAC_FORGE_STREAM` asked for a generated stream out of this
    /// report; [`crate::report::Stream::new`] then rebuilds every frame instead
    /// of replaying it.
    pub(super) generator: Option<GenConfig>,
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
        report.messages = split_messages(&report.frames);
        report
    }

    /// Read and parse a report file.
    ///
    /// `EAC_FORGE_STREAM` is read here, once, at install time: when it names
    /// this same file the report is armed as a generator source and
    /// [`crate::report::Stream::new`] rebuilds every frame instead of replaying
    /// it. With the variable unset the report behaves exactly as before.
    pub fn load(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let mut file = File::open(path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let mut report = Self::from_bytes(&bytes);
        if let Some(config) = StreamConfig::parse(|name| std::env::var(name).ok()) {
            if config.path == path {
                forge::note(format!(
                    "stream: armed {} frames={} messages={} body={} loop={} via={} total={}",
                    path.display(),
                    report.len(),
                    report.messages().len(),
                    config.body.as_str(),
                    config.looping,
                    config.via.as_str(),
                    config.total.as_str()
                ));
                report.generator = Some(GenConfig::from_stream(&config));
            } else {
                forge::note(format!(
                    "stream: EAC_FORGE_STREAM={} but loading {}; replaying as recorded",
                    config.path.display(),
                    path.display()
                ));
            }
        }
        Ok(report)
    }

    /// Parse a report from raw bytes and arm it as a stream source with `generator`.
    ///
    /// The explicit counterpart of the `EAC_FORGE_STREAM` path in
    /// [`Report::load`], used by the tests and by anything that already knows
    /// what it wants.
    pub fn stream_from_bytes(bytes: &[u8], generator: GenConfig) -> Self {
        let mut report = Self::from_bytes(bytes);
        report.generator = Some(generator);
        report
    }

    /// Length of the message `index` covers, in frames.
    pub fn message_len(&self, index: usize) -> Option<usize> {
        self.messages.get(index).map(Range::len)
    }

    /// Frame ranges of the messages in this report, in recorded order.
    pub fn messages(&self) -> &[Range<usize>] {
        &self.messages
    }

    /// True when this report was armed as an `EAC_FORGE_STREAM` source.
    pub fn generates(&self) -> bool {
        self.generator.is_some()
    }

    /// The generator configuration this report carries, if any.
    pub fn generator(&self) -> Option<&GenConfig> {
        self.generator.as_ref()
    }

    /// The recorded frame `index`, header included.
    pub fn frame(&self, index: usize) -> Option<&[u8]> {
        self.frames.get(index).map(|frame| frame.data.as_slice())
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

/// Split the recorded frames into messages on `StartData` (flag bit 0).
///
/// Every capture of `~/EAC-RE/evidence` shows exactly one `StartData` per
/// report and none inside it, so a message is `[start..next_start)`. A
/// recording that begins mid-message (a capture cut in the middle) yields a
/// leading range that has no header of its own — the generator reads whatever
/// the first frame of the range carries.
pub(super) fn split_messages(frames: &[Frame]) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if frames.is_empty() {
        return out;
    }
    let mut start = 0usize;
    for (index, frame) in frames.iter().enumerate() {
        let flags = u16::from_le_bytes([frame.data[2], frame.data[3]]);
        if flags & FLAG_START != 0 && index > start {
            out.push(start..index);
            start = index;
        }
    }
    out.push(start..frames.len());
    out
}

/// Record one frame in the capture format (used by the tests and by anything
/// that has to build a report fixture).
pub fn record(out: &mut Vec<u8>, dt_ms: u32, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&dt_ms.to_le_bytes());
    out.extend_from_slice(data);
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! Synthetic frames and recordings shared by the submodules' tests.

    use crate::digest::crc32c_eac;
    use crate::forge::{BodyMode, CHECKSUM_OFFSET, Template, build_frame};

    use super::{FRAME_LEN, HEADER_LEN, record};

    /// A synthetic 512-byte frame whose body is unique per `index`.
    pub(crate) fn frame_at(session: u64, seq: u64, flags: u16, tag: u8) -> Vec<u8> {
        let template = Template {
            session_id: session,
            flags,
            last_seq: seq,
            body: vec![tag; FRAME_LEN - HEADER_LEN],
        };
        build_frame(&template, seq, BodyMode::Stale)
    }

    /// Three recorded frames of `session`, the first one starting a message.
    pub(crate) fn sample_report(session: u64) -> Vec<u8> {
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

    /// The live capture every invariant below is proven against.
    pub(crate) const REAL_CAPTURE: &str =
        "/home/wilpdrake/EAC-RE/evidence/control_0009/capture.bin";

    /// The whole recorded report of that capture (message 1, 117 frames).
    pub(crate) const REPORT_FULL: &str =
        "/home/wilpdrake/EAC-RE/evidence/report_full/report_a_sid158b.bin";

    pub(crate) fn real_capture() -> Option<Vec<u8>> {
        std::fs::read(REAL_CAPTURE).ok()
    }

    pub(crate) fn crc_holds(frame: &[u8]) -> bool {
        if frame.len() != FRAME_LEN {
            return false;
        }
        let stored = u32::from_le_bytes(
            frame[CHECKSUM_OFFSET..HEADER_LEN]
                .try_into()
                .expect("4 bytes"),
        );
        let mut zeroed = frame.to_vec();
        zeroed[CHECKSUM_OFFSET..HEADER_LEN].fill(0);
        crc32c_eac(&zeroed) == stored
    }

    pub(crate) fn flags_of(frame: &[u8]) -> u16 {
        u16::from_le_bytes([frame[2], frame[3]])
    }

    pub(crate) fn seq_of(frame: &[u8]) -> u64 {
        u64::from_le_bytes(frame[12..20].try_into().expect("8 bytes"))
    }

    pub(crate) fn sid_of(frame: &[u8]) -> u64 {
        u64::from_le_bytes(frame[4..12].try_into().expect("8 bytes"))
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use crate::digest::crc32c_eac;
    use crate::forge::CHECKSUM_OFFSET;
    use crate::report::{Stream, retarget};

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

    /// Every invariant the model claims about a recorded report, checked on one
    /// file. Returns the number of individual assertions that held.
    ///
    /// The model (`findings/report_stream.md`, `frame_structure.md`) says:
    ///
    /// 1. the stream is made of `Version == 2` 512-byte frames, each with a
    ///    CRC-32C (init/xorout 0) over the frame with the field zeroed;
    /// 2. `StartData` (bit 0) marks the first frame of a report and appears
    ///    exactly once per report — never inside it;
    /// 3. `Sequence` only grows, +1 per frame that the capture actually holds
    ///    (a dropped record leaves a gap, never a rewind);
    /// 4. the frame that starts a report carries the 32-byte plaintext message
    ///    header: `mode == 5`, `kind == 3`, `counter == 1` for the first report,
    ///    a per-report `nonce` and a per-session 48-byte blob constant across
    ///    the session's reports;
    /// 5. `total_len` is the byte count of a *block* of whole 488-byte
    ///    fragments plus a partial tail, so it decomposes as
    ///    `k * 488 + tail` with `1 <= tail <= 488`.
    fn validate_recorded(report: &Report) -> usize {
        let mut checks = 0usize;
        assert_eq!(report.len(), 121, "512-byte frames in the capture");
        assert_eq!(report.skipped(), 49, "24/144-byte ACK and protect frames");
        assert!(!report.truncated(), "the capture ends on a record boundary");
        checks += 3;

        // (1) every frame is a Version 2 frame with a valid CRC.
        for index in 0..report.len() {
            let frame = report.frame(index).expect("frame");
            assert_eq!(
                u16::from_le_bytes([frame[0], frame[1]]),
                2,
                "frame {index} version"
            );
            assert!(crc_holds(frame), "frame {index} CRC-32C");
            checks += 2;
        }

        // (2) StartData exactly once per report, never inside one.
        let starts: Vec<usize> = (0..report.len())
            .filter(|index| flags_of(report.frame(*index).expect("frame")) & FLAG_START != 0)
            .collect();
        assert_eq!(starts, vec![0, 117], "two reports: 117 + 4 recorded frames");
        assert_eq!(report.messages(), &[0..117, 117..121], "message ranges");
        checks += 3;

        // (3) Sequence only grows; a gap is a lost record, not a rewind.
        let mut gaps = 0usize;
        for index in 1..report.len() {
            let previous = seq_of(report.frame(index - 1).expect("frame"));
            let current = seq_of(report.frame(index).expect("frame"));
            assert!(current > previous, "frame {index}: {previous} -> {current}");
            if current != previous + 1 {
                gaps += 1;
            }
            checks += 1;
        }
        assert_eq!(gaps, 6, "six records lost to capture epochs");
        checks += 1;
        // The two reports are exactly one report apart (144 frames of slot).
        assert_eq!(
            seq_of(report.frame(117).expect("frame")) - seq_of(report.frame(0).expect("frame")),
            144,
            "report period"
        );
        checks += 1;

        // (4) the message header of both starts.
        let first = forge::MessageHeader::parse(&report.frame(0).expect("frame")[HEADER_LEN..])
            .expect("first header");
        let second = forge::MessageHeader::parse(&report.frame(117).expect("frame")[HEADER_LEN..])
            .expect("second header");
        assert_eq!(first.mode, 5);
        assert_eq!(first.kind, 3);
        assert_eq!(first.constant, 1, "0x04 is the constant, not the counter");
        assert_eq!(first.counter, 22, "message counter of control_0009");
        assert_eq!(second.counter, 35, "counter +1 per message");
        assert_eq!(first.action, 13, "first report action");
        assert_eq!(second.action, 4, "later report action");
        assert_ne!(first.nonce, second.nonce, "nonce is per message");
        checks += 8;

        let blob_a = forge::recorded_blob(&report.frame(0).expect("frame")[HEADER_LEN..])
            .expect("first blob");
        let blob_b = forge::recorded_blob(&report.frame(117).expect("frame")[HEADER_LEN..])
            .expect("second blob");
        assert_eq!(blob_a, blob_b, "the per-session blob is constant");
        assert_eq!(&blob_a[..4], &first.blob[..4], "blob starts at 0x1C");
        checks += 2;

        // (5) `total_len` is a constant per action, not a frame count: both
        // messages stay at their own value while their lengths differ (117 vs
        // 4 frames).
        assert_eq!(first.total_len, 1_592, "action 13 header");
        assert_eq!(second.total_len, 2_302, "action 4 header");

        // The cadence the generator reproduces.
        let mut dts: Vec<u32> = report.dt_ms().collect();
        dts.sort_unstable();
        assert_eq!(dts[dts.len() / 2], 2_007, "median inter-frame interval");
        checks += 1;
        checks
    }

    #[test]
    fn the_real_capture_holds_every_invariant_the_stream_model_claims() {
        let Some(bytes) = real_capture() else {
            return; // the evidence tree is optional on other machines
        };
        let report = Report::from_bytes(&bytes);
        let checks = validate_recorded(&report);
        println!(
            "{REAL_CAPTURE}: {} frames, {} message(s), {checks} invariants held",
            report.len(),
            report.messages().len()
        );
        assert!(checks >= 250, "the validator ran, not short-circuited");
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
