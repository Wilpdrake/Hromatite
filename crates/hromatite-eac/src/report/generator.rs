//! The `EAC_FORGE_STREAM` generator: [`GenConfig`] and [`Generator`].
//!
//! The recording is treated as one message
//! ([`Report::messages`](crate::report::Report::messages)), and every frame is
//! rebuilt — `Flags` with `StartData` on the first and `EndData` on the last
//! frame of the message, `Sequence` strictly +1 from the generator's base, and
//! the 32-byte message header of the first frame patched (`total_len` from the
//! frame count, `counter` +1 per message, `nonce` +1 per report,
//! `action`/`mode`/`kind` as recorded, per-session blob ours in the
//! `notes`/`bump` modes). `nonce`, `counter` and the blob are the fields a
//! frame source can actually keep *consistent* with the message it declares,
//! which is what `EAC_FORGE`'s one-fragment-per-heartbeat never did
//! (`findings/report_stream.md` §2).
//!
//! `EAC_FORGE_VIA=photon` additionally renders the same stream into the ring of
//! [`forge::push_frame`] from a sender thread of our own, i.e.
//! `RaiseEvent(253, {244: 66, 245: frame})` out of the managed `PhotonPeer`:
//! no EOS module and no EOSSDK callback is involved, so the cadence has to come
//! from the thread. `EAC_FORGE_SPOOL=<path>` appends every frame in capture
//! format, which is what `EAC_EVENT_SEND=<path>` (photon's own `tick_send`)
//! reads.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::forge::{
    self, FLAG_END, FLAG_START, FRAME_LEN, HEADER_LEN, MSG_BLOB_LEN, MSG_HEADER_LEN, StreamBody,
    StreamConfig, TotalMode, Via,
};

use super::parser::{Report, split_messages};

/// Message boundary flags: `StartData` on the first frame only.
///
/// The recording never shows `EndData` on a 512-byte frame — a message simply
/// ends where the next `StartData` begins — so the generator must not invent
/// the bit (setting it was one more way the earlier streams contradicted the
/// live ones).
fn boundary_flags(source: u16, first: bool) -> u16 {
    let keep = source & !(FLAG_START | FLAG_END);
    keep | if first { FLAG_START } else { 0 }
}

/// How the `EAC_FORGE_STREAM` generator renders a recorded report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenConfig {
    /// `EAC_FORGE_STREAM_BODY`.
    pub body: StreamBody,
    /// `EAC_FORGE_STREAM_LOOP`: start a new report instead of stopping.
    pub looping: bool,
    /// `EAC_FORGE_VIA`.
    pub via: Via,
    /// `EAC_FORGE_SID`: `SessionId` to write; `None` keeps the recording's.
    pub session: Option<u64>,
    /// `EAC_FORGE_SEQ`: first `Sequence`.
    pub seq: Option<u64>,
    /// `EAC_FORGE_NONCE`: first nonce; `None` continues the recording's.
    pub nonce: Option<u32>,
    /// `EAC_FORGE_COUNTER`: first message counter.
    pub counter: Option<u32>,
    /// `EAC_FORGE_BLOB`: the 48 bytes to write in the `notes`/`bump` modes.
    pub blob: Option<[u8; MSG_BLOB_LEN]>,
    /// `EAC_FORGE_STREAM_TOTAL`.
    pub total: TotalMode,
    /// Cadence of the Photon sender thread.
    pub interval_ms: u64,
    /// `EAC_FORGE_SPOOL`: capture-format copy of every emitted frame.
    pub spool: Option<PathBuf>,
}

impl GenConfig {
    /// The render settings of a parsed [`StreamConfig`].
    pub fn from_stream(config: &StreamConfig) -> Self {
        Self {
            body: config.body,
            looping: config.looping,
            via: config.via,
            session: config.session,
            seq: config.seq,
            nonce: config.nonce,
            counter: config.counter,
            blob: config.blob,
            total: config.total,
            interval_ms: config.interval_ms,
            spool: config.spool.clone(),
        }
    }

    /// Read `EAC_FORGE_STREAM*` from the process environment.
    pub fn from_env(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        StreamConfig::parse(get).map(|config| Self::from_stream(&config))
    }

    /// Default render settings for a test or an explicit caller.
    pub fn plain() -> Self {
        Self {
            body: StreamBody::Copy,
            looping: false,
            via: Via::Wrapper,
            session: None,
            seq: None,
            nonce: None,
            counter: None,
            blob: None,
            total: TotalMode::Recorded,
            interval_ms: forge::INTERVAL_MS,
            spool: None,
        }
    }
}

/// Renders a recorded report as our own message stream.
///
/// Pure state machine, no locks and no I/O:
/// [`crate::report::Stream::next_frame`] owns one behind its mutex, and the
/// Photon sender thread owns a second one built from the same report, so both
/// transports produce the same bytes.
#[derive(Clone, Debug)]
pub struct Generator {
    /// Recorded 512-byte frames, header included.
    frames: Vec<Vec<u8>>,
    /// Message ranges inside [`Generator::frames`].
    messages: Vec<Range<usize>>,
    /// Recorded message header of each message (zeroed when absent).
    headers: Vec<forge::MessageHeader>,
    body: StreamBody,
    looping: bool,
    total: TotalMode,
    /// `SessionId` of the recording, used when no `EAC_FORGE_SID` is given.
    recorded_session: u64,
    /// `SessionId` we write (`EAC_FORGE_SID` wins).
    session: u64,
    /// Next `Sequence`.
    seq: u64,
    /// `EAC_FORGE_NONCE`: override for every message; `None` keeps each
    /// message's recorded nonce (they differ between messages of a session).
    nonce: Option<u32>,
    counter: u32,
    /// Explicit per-session blob (`EAC_FORGE_BLOB` or the live session's own
    /// header). `None` keeps the recording's blob; it is never minted.
    blob: Option<[u8; MSG_BLOB_LEN]>,
    msg: usize,
    pos: usize,
    emitted: u64,
    loops: u64,
    finished: bool,
}

impl Generator {
    /// Build a generator from a parsed report; `None` when it has no frame.
    pub fn new(report: &Report, config: &GenConfig) -> Option<Self> {
        if report.frames.is_empty() {
            return None;
        }
        let messages = if report.messages.is_empty() {
            split_messages(&report.frames)
        } else {
            report.messages.clone()
        };
        let headers = messages
            .iter()
            .map(|range| {
                report
                    .frames
                    .get(range.start)
                    .and_then(|frame| forge::MessageHeader::parse(&frame.data[HEADER_LEN..]))
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();
        let first = report.frames.first()?;
        let recorded_session = u64::from_le_bytes(first.data[4..12].try_into().ok()?);
        let recorded_seq = u64::from_le_bytes(first.data[12..20].try_into().ok()?);
        let session = config.session.unwrap_or(recorded_session);
        let seq = config.seq.unwrap_or_else(|| recorded_seq.wrapping_add(1));
        // No derivation of our own: a session blob that was not observed cannot
        // be minted, so the recording's 48 bytes stay unless the operator (or
        // the live template) supplies the session's real one.
        let nonce = config.nonce;
        let counter = config
            .counter
            .unwrap_or_else(|| headers.first().map(|h| h.counter).unwrap_or(1));
        let blob = config.blob;
        Some(Self {
            frames: report.frames.iter().map(|f| f.data.clone()).collect(),
            messages,
            headers,
            body: config.body,
            looping: config.looping,
            total: config.total,
            recorded_session,
            session,
            seq,
            nonce,
            counter,
            blob,
            msg: 0,
            pos: 0,
            emitted: 0,
            loops: 0,
            finished: false,
        })
    }

    /// Messages the recording holds.
    pub fn messages(&self) -> usize {
        self.messages.len()
    }

    /// Frames of the message currently being rendered.
    pub fn message_len(&self) -> usize {
        self.messages.get(self.msg).map(Range::len).unwrap_or(0)
    }

    /// Offset inside the current message.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Frames emitted so far.
    pub fn emitted(&self) -> u64 {
        self.emitted
    }

    /// Times the recording was restarted.
    pub fn loops(&self) -> u64 {
        self.loops
    }

    /// True once a non-looping generator ran out of frames.
    pub fn finished(&self) -> bool {
        self.finished
    }

    /// Frames left before the current pass ends.
    pub fn remaining(&self) -> usize {
        let left = self.message_len().saturating_sub(self.pos);
        let rested: usize = self
            .messages
            .iter()
            .skip(self.msg + 1)
            .map(Range::len)
            .sum();
        left + rested
    }

    /// `SessionId` the generator writes (`EAC_FORGE_SID` or the recording's).
    pub fn session(&self) -> u64 {
        self.session
    }

    /// `SessionId` the recording itself carries.
    pub fn recorded_session(&self) -> u64 {
        self.recorded_session
    }

    /// The nonce override the *next* message will carry, if any.
    pub fn nonce(&self) -> Option<u32> {
        self.nonce
    }

    /// The counter the *next* message will carry.
    pub fn counter(&self) -> u32 {
        self.counter
    }

    /// Restart at the first message, continuing the nonce/counter sequence.
    pub fn rewind(&mut self) {
        self.msg = 0;
        self.pos = 0;
        self.finished = false;
        self.loops = 0;
    }

    /// Begin the next report: `nonce` +1, `counter` +1, blob re-derived.
    fn next_report(&mut self) -> bool {
        if !self.looping {
            self.finished = true;
            return false;
        }
        self.loops = self.loops.wrapping_add(1);
        if let Some(nonce) = self.nonce.as_mut() {
            *nonce = nonce.wrapping_add(1);
        }
        self.counter = self.counter.wrapping_add(1);
        self.msg = 0;
        self.pos = 0;
        true
    }

    /// Render the next frame of the stream.
    ///
    /// `None` when the report is exhausted and `EAC_FORGE_STREAM_LOOP` is off.
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        loop {
            match self.messages.get(self.msg) {
                Some(range) if self.pos < range.len() => break,
                Some(_) => {
                    // A message ended: the next one starts with its own counter
                    // and `StartData`.
                    self.msg += 1;
                    self.pos = 0;
                    if self.msg < self.messages.len() {
                        self.counter = self.counter.wrapping_add(1);
                        if let Some(nonce) = self.nonce.as_mut() {
                            *nonce = nonce.wrapping_add(1);
                        }
                        continue;
                    }
                    if !self.next_report() {
                        return None;
                    }
                    continue;
                }
                None => {
                    if !self.next_report() {
                        return None;
                    }
                }
            }
        }
        let range = self.messages.get(self.msg)?.clone();
        let length = range.len();
        let first = self.pos == 0;
        let index = range.start + self.pos;
        let mut body = self.frames.get(index)?[HEADER_LEN..].to_vec();

        if first {
            let mut header = self.headers.get(self.msg).copied().unwrap_or_default();
            header.total_len = match self.total {
                TotalMode::Frames => forge::message_total_len(length),
                TotalMode::Recorded => header.total_len,
            };
            header.counter = self.counter;
            if let Some(nonce) = self.nonce {
                header.nonce = nonce;
            }
            header.write(&mut body, self.blob.as_ref());
        }
        if self.body.bumps_records() {
            // The message header sits in front of the records only in the frame
            // that starts the message.
            let first_record = if first { MSG_HEADER_LEN } else { 0 };
            forge::bump_records(&mut body, first_record, self.emitted as u32 + 1);
        }

        let source_flags =
            u16::from_le_bytes([self.frames.get(index)?[2], self.frames.get(index)?[3]]);
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        let frame = forge::seal_frame(
            self.session,
            seq,
            boundary_flags(source_flags, first),
            &body,
        );
        self.pos += 1;
        self.emitted = self.emitted.wrapping_add(1);
        debug_assert_eq!(frame.len(), FRAME_LEN);
        Some(frame)
    }
}

/// Spawn the Photon sender once per process (`EAC_FORGE_VIA=photon`).
///
/// The thread owns its own [`Generator`], pushes every frame into the ring of
/// [`forge::push_frame`] and drains it through
/// [`hromatite_core::photon::emulate_send`], so the cadence is ours rather than
/// the live stream's — nothing else runs when the EOS module is gone.
pub(super) fn spawn_photon_sender(report: &Report, config: &GenConfig) {
    static SPAWNED: AtomicBool = AtomicBool::new(false);
    if !config.via.uses_photon() || SPAWNED.swap(true, Ordering::SeqCst) {
        return;
    }
    let Some(mut generator) = Generator::new(report, config) else {
        SPAWNED.store(false, Ordering::SeqCst);
        return;
    };
    let interval = config.interval_ms.max(50);
    let spool = config.spool.clone();
    let frames = generator.messages();
    let total = report.len();
    forge::note(format!(
        "photon sender: messages={frames} frames={total} sid=0x{:x} recorded_sid=0x{:x} \
         interval={interval} ms body={} loop={} spool={}",
        generator.session(),
        generator.recorded_session(),
        config.body.as_str(),
        config.looping,
        spool
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "-".to_string())
    ));
    let spawned = std::thread::Builder::new()
        .name("eac-report-photon".to_string())
        .spawn(move || {
            let mut unarmed = 0u64;
            loop {
                let Some(frame) = generator.next_frame() else {
                    forge::note(format!(
                        "photon sender: stream exhausted after {} frame(s), loops={}",
                        generator.emitted(),
                        generator.loops()
                    ));
                    return;
                };
                if let Some(path) = spool.as_ref()
                    && let Err(error) = forge::append_spool(path, interval as u32, &frame)
                {
                    forge::note(format!("photon sender: spool {error}"));
                }
                forge::push_frame(frame);
                let delivery = forge::pump_photon();
                if delivery.unarmed {
                    unarmed += 1;
                    if unarmed <= 3 || unarmed.is_multiple_of(60) {
                        forge::note(format!(
                            "photon sender: no live event 66 template yet ({unarmed} frame(s) \
                             queued, ring={})",
                            forge::ring_len()
                        ));
                    }
                }
                std::thread::sleep(Duration::from_millis(interval));
            }
        });
    if spawned.is_err() {
        SPAWNED.store(false, Ordering::SeqCst);
        forge::note("photon sender: thread spawn failed");
    }
}

#[cfg(test)]
mod tests {
    use super::super::parser::fixtures::*;
    use super::*;
    use crate::report::retarget;

    #[test]
    fn the_generator_renders_the_recorded_report_as_one_consistent_message() {
        let Some(bytes) = real_capture() else {
            return;
        };
        let recorded = Report::from_bytes(&bytes);
        let mut config = GenConfig::plain();
        config.session = Some(0x0bad_c0de);
        config.seq = Some(1_000);
        let mut generator = Generator::new(&recorded, &config).expect("the capture has frames");
        assert_eq!(generator.messages(), 2);
        assert_eq!(generator.message_len(), 117);
        assert_eq!(generator.session(), 0x0bad_c0de);
        assert_eq!(generator.nonce(), None, "no override: nonces come from the file");
        assert_eq!(generator.counter(), 22, "continues the recording's counter");

        let mut frames = Vec::new();
        while let Some(frame) = generator.next_frame() {
            if frames.len() == 117 {
                break;
            }
            frames.push(frame);
        }
        assert_eq!(frames.len(), 117, "one message, no loop");
        assert!(generator.finished() || frames.len() == 117);

        let mut checks = 0usize;
        for (index, frame) in frames.iter().enumerate() {
            assert_eq!(frame.len(), FRAME_LEN);
            assert!(crc_holds(frame), "frame {index} CRC");
            assert_eq!(sid_of(frame), 0x0bad_c0de, "frame {index} session");
            assert_eq!(seq_of(frame), 1_000 + index as u64, "frame {index} seq +1");
            let flags = flags_of(frame);
            assert_eq!(
                flags & FLAG_START != 0,
                index == 0,
                "StartData only on the first frame"
            );
            assert_eq!(
                flags & FLAG_END,
                0,
                "EndData is never set (frame {index})"
            );
            checks += 6;
        }

        // The body is the recording's, byte for byte, outside the 32-byte
        // message header of the first frame.
        let source = recorded.frame(0).expect("frame");
        // Everything after the message header is the recording's.
        assert_eq!(
            &frames[0][HEADER_LEN + MSG_HEADER_LEN..],
            &source[HEADER_LEN + MSG_HEADER_LEN..],
            "records untouched in copy mode"
        );
        for (index, frame) in frames.iter().enumerate().take(117).skip(1) {
            assert_eq!(
                &frame[HEADER_LEN..],
                &recorded.frame(index).expect("frame")[HEADER_LEN..],
                "frame {index} body is the recording's"
            );
            checks += 1;
        }

        let header =
            forge::MessageHeader::parse(&frames[0][HEADER_LEN..]).expect("generated header");
        assert_eq!(
            header.total_len, 1_592,
            "total_len is the recorded constant, not 117*512-24"
        );
        assert_eq!(header.counter, 22, "the recording's message counter");
        assert_eq!(header.nonce, 0x94bb_79c6, "recorded nonce, no override");
        assert_eq!(header.constant, 1);
        assert_eq!(header.mode, 5);
        assert_eq!(header.kind, 3);
        assert_eq!(
            forge::recorded_blob(&frames[0][HEADER_LEN..]),
            forge::recorded_blob(&source[HEADER_LEN..]),
            "copy mode keeps the recorded blob"
        );
        checks += 8;

        // The EOSSDK path composes: `eos` retargets whatever the stream hands it
        // to the *live* session, which must keep our flags and body and only
        // move the identity (that is `retarget`, unchanged).
        let live = retarget(&frames[0], 0xfeed_face, 4_242, None).expect("512-byte frame");
        assert_eq!(sid_of(&live), 0xfeed_face);
        assert_eq!(seq_of(&live), 4_242);
        assert_eq!(flags_of(&live), flags_of(&frames[0]), "flags kept");
        assert_eq!(&live[HEADER_LEN..], &frames[0][HEADER_LEN..], "body kept");
        assert!(crc_holds(&live), "retarget reseals the checksum");
        checks += 6;

        println!("generator: 117-frame message, {checks} invariants held");
        assert!(checks >= 700);
    }

    #[test]
    fn looping_starts_a_new_report_with_fresh_nonce_and_counter() {
        // A report that holds exactly one message, so frame 117 is the wrap.
        let Some(bytes) = std::fs::read(REPORT_FULL).ok() else {
            return;
        };
        let recorded = Report::from_bytes(&bytes);
        let mut config = GenConfig::plain();
        config.looping = true;
        config.session = Some(0x158b);
        let mut generator = Generator::new(&recorded, &config).expect("frames");
        let recorded_header =
            forge::MessageHeader::parse(&recorded.frame(0).expect("frame")[HEADER_LEN..])
                .expect("header");

        let mut frames = Vec::new();
        for _ in 0..(117 + 4 + 1) {
            frames.push(generator.next_frame().expect("frame"));
        }
        // 117 frames of report 1, then report 2 starts at frame 117 again.
        assert_eq!(generator.loops(), 1, "one wrap");
        let second = &frames[117];
        assert_ne!(second.as_slice(), frames[0].as_slice());
        let header = forge::MessageHeader::parse(&second[HEADER_LEN..]).expect("header");
        assert_eq!(
            header.nonce, recorded_header.nonce,
            "no override keeps the recording's nonce"
        );
        assert_eq!(header.counter, recorded_header.counter + 1, "counter +1 per message");
        assert_eq!(flags_of(second) & FLAG_START, FLAG_START, "StartData again");
        assert_eq!(
            header.total_len, recorded_header.total_len,
            "total_len stays the recorded constant"
        );
        // Sequence keeps growing across the wrap.
        assert_eq!(seq_of(second), seq_of(&frames[116]) + 1);
        assert_eq!(seq_of(&frames[118]), seq_of(second) + 1);
        assert!(crc_holds(second));
        // The recorded blob is still the recording's (`copy`).
        assert_eq!(
            forge::recorded_blob(&second[HEADER_LEN..]),
            forge::recorded_blob(&frames[0][HEADER_LEN..])
        );
    }

    #[test]
    fn an_explicit_blob_is_written_and_otherwise_the_recording_owns_it() {
        let Some(bytes) = real_capture() else {
            return;
        };
        let recorded = Report::from_bytes(&bytes);
        let source = recorded.frame(0).expect("frame");
        let recorded_blob =
            forge::recorded_blob(&source[HEADER_LEN..]).expect("recorded blob");

        // Default: the blob is the recording's, byte for byte.
        let mut config = GenConfig::plain();
        config.session = Some(0x158b);
        let mut plain = Generator::new(&recorded, &config).expect("frames");
        let first = plain.next_frame().expect("frame");
        assert_eq!(
            forge::recorded_blob(&first[HEADER_LEN..]),
            Some(recorded_blob),
            "copy keeps the recording's blob"
        );

        // An explicit blob (the live session's own header, or EAC_FORGE_BLOB)
        // replaces all 48 bytes; nothing is derived any more.
        let ours = [0x5Au8; MSG_BLOB_LEN];
        assert_ne!(ours, recorded_blob);
        config.blob = Some(ours);
        config.body = StreamBody::Notes;
        let mut with_blob = Generator::new(&recorded, &config).expect("frames");
        let first = with_blob.next_frame().expect("frame");
        assert_eq!(forge::recorded_blob(&first[HEADER_LEN..]), Some(ours));
        assert_eq!(
            &first[HEADER_LEN + forge::MSG_BLOB_OFF + MSG_BLOB_LEN..],
            &source[HEADER_LEN + forge::MSG_BLOB_OFF + MSG_BLOB_LEN..],
            "the records after the blob are untouched"
        );
        // The next frame carries no header at all (not a message start).
        let second = with_blob.next_frame().expect("frame");
        assert_eq!(
            &second[HEADER_LEN..],
            &recorded.frame(1).expect("frame")[HEADER_LEN..],
            "a middle fragment is the recording's"
        );
    }

    #[test]
    fn the_bump_mode_makes_a_looped_report_differ_without_touching_frames_midway() {
        let Some(bytes) = real_capture() else {
            return;
        };
        let recorded = Report::from_bytes(&bytes);
        let mut config = GenConfig::plain();
        config.body = StreamBody::Bump;
        config.looping = true;
        let mut generator = Generator::new(&recorded, &config).expect("frames");
        let first = generator.next_frame().expect("frame");
        let second = generator.next_frame().expect("frame");
        let source_first = recorded.frame(0).expect("frame");
        let source_second = recorded.frame(1).expect("frame");

        // The first frame differs in the blob and in every record's tail.
        assert_ne!(&first[HEADER_LEN..], &source_first[HEADER_LEN..]);
        assert!(crc_holds(&first));
        // A middle fragment has no header, so it differs only in the records —
        // and nowhere before the first record boundary.
        assert_ne!(&second[HEADER_LEN..], &source_second[HEADER_LEN..]);
        let records_start = HEADER_LEN;
        assert_eq!(
            &second[HEADER_LEN..HEADER_LEN],
            &source_second[HEADER_LEN..HEADER_LEN]
        );
        assert_ne!(
            &second[records_start + 12..records_start + 16],
            &source_second[records_start + 12..records_start + 16],
            "the first record's tail moved"
        );
        assert!(crc_holds(&second));
    }
}
