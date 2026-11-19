//! The delivery layer: [`Stream`], [`Slot`] and [`retarget`].
//!
//! [`Stream`] is the cursor `eos` pulls one frame per slot from; with
//! `EAC_FORGE_STREAM` armed the frames it hands out are the generator's, not
//! the recorded bytes. [`retarget`] then moves the *live* session's identity
//! into the frame, because a recorded report belongs to the recording's
//! `SessionId`.
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
//! Two transports carry the result, and both are armed here:
//!
//! * `EAC_FORGE_VIA=wrapper` (default) — the existing EOSSDK takeover: `eos`
//!   pulls one frame per slot through [`Stream::next_frame`] and hands it to the
//!   game's `MessageToServer` callback, exactly like `EAC_REPLAY_REPORT`;
//! * `EAC_FORGE_VIA=photon` — the sender thread [`Stream::new`] starts renders
//!   the stream into the ring of [`crate::forge::push_frame`] and calls
//!   [`hromatite_core::photon::emulate_send`].
//!
//! Everything is `std`; the only shared state is the ring, and `eos` still owns
//! the clock and the wrapper delivery.

use crate::digest::crc32c_eac;
use crate::forge::{self, CHECKSUM_OFFSET, FRAME_LEN, HEADER_LEN};

use super::generator::{Generator, spawn_photon_sender};
use super::parser::Report;

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
///
/// With `EAC_FORGE_STREAM` armed the frames it hands out are not the recorded
/// bytes but freshly built ones ([`Generator`]); `index`/`of` then describe the
/// position inside the message being rendered, and `wraps` counts finished
/// reports.
#[derive(Debug)]
pub struct Stream {
    report: Report,
    start: usize,
    cursor: usize,
    looping: bool,
    finished: bool,
    wraps: u64,
    generator: Option<Generator>,
}

impl Stream {
    /// A stream starting at `start` (clamped into the report) that either
    /// restarts at `start` once the report ends (`looping`) or stops.
    ///
    /// A report armed by [`Report::load`] with `EAC_FORGE_STREAM` gets a
    /// [`Generator`] instead of the plain cursor — and then `start`
    /// (`EAC_REPLAY_START`) is ignored, because a generated stream always starts
    /// at the first message of the recording. `EAC_FORGE_VIA=photon`
    /// additionally starts the report's Photon sender thread (once per
    /// process: a second stream is another epoch, not a second emitter).
    pub fn new(report: Report, start: usize, looping: bool) -> Self {
        let start = if report.frames.is_empty() {
            0
        } else {
            start.min(report.frames.len() - 1)
        };
        let generator = match report.generator.as_ref() {
            Some(config) => {
                spawn_photon_sender(&report, config);
                Generator::new(&report, config)
            }
            None => None,
        };
        let looping = if generator.is_some() {
            report
                .generator
                .as_ref()
                .is_some_and(|config| config.looping)
        } else {
            looping
        };
        Self {
            report,
            start,
            cursor: start,
            looping,
            finished: false,
            wraps: 0,
            generator,
        }
    }

    /// True when this stream builds frames instead of replaying them.
    pub fn generates(&self) -> bool {
        self.generator.is_some()
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
        match self.generator.as_ref() {
            Some(generator) => generator.remaining(),
            None => self.report.frames.len().saturating_sub(self.cursor),
        }
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
        if let Some(generator) = self.generator.as_mut() {
            generator.rewind();
        }
    }

    /// Take the next frame.
    ///
    /// Returns `None` only when the report is empty or a non-looping stream is
    /// exhausted; the first `None` sets [`Stream::finished`], later calls keep
    /// returning `None` without further logging on the caller's side.
    pub fn next_frame(&mut self) -> Option<Slot> {
        if let Some(generator) = self.generator.as_mut() {
            let index = generator.position();
            let of = generator.message_len();
            // Take the frame first, then mirror the state *even on exhaustion*:
            // `eos` reads `finished()` to log the stop exactly once.
            let frame = generator.next_frame();
            self.cursor = generator.position();
            self.wraps = generator.loops();
            self.finished = generator.finished();
            let frame = frame?;
            let dt_ms = self
                .report
                .frames
                .get(index)
                .map(|frame| frame.dt_ms)
                .unwrap_or(forge::INTERVAL_MS as u32);
            return Some(Slot {
                index,
                of,
                dt_ms,
                data: frame,
            });
        }
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

#[cfg(test)]
mod tests {
    use super::super::parser::fixtures::*;
    use super::*;
    use crate::forge::{FLAG_START, Via};
    use crate::report::GenConfig;
    use std::time::Duration;

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
    fn the_photon_via_renders_into_the_ring_of_the_emitter() {
        let Some(bytes) = real_capture() else {
            return;
        };
        let mut config = GenConfig::plain();
        config.via = Via::Photon;
        config.session = Some(0x158b);
        config.interval_ms = 50;
        let report = Report::stream_from_bytes(&bytes, config);
        crate::forge::clear_ring();
        let before = crate::forge::ring_pushed();
        // `Stream::new` is where `eos` arms the source, so that is where the
        // sender thread has to start.
        let _stream = Stream::new(report, 0, false);

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while crate::forge::ring_pushed() == before && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            crate::forge::ring_pushed() > before,
            "the sender queued at least one frame"
        );
        let lines = crate::forge::take_log();
        assert!(
            lines.iter().any(|line| line.contains("photon sender")),
            "the sender logged its arming: {lines:?}"
        );
        let queued = crate::forge::drain_frames();
        assert!(!queued.is_empty());
        let first = &queued[0];
        assert_eq!(first.len(), FRAME_LEN);
        assert_eq!(sid_of(first), 0x158b, "frames are ready to send");
        assert!(crc_holds(first), "the emitter gets sealed frames");
        assert_eq!(flags_of(first) & FLAG_START, FLAG_START, "StartData");
        assert!(seq_of(first) >= 2, "sequence continues the recording");
        crate::forge::clear_ring();
    }
}
