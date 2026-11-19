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
//! # `EAC_FORGE_STREAM=<path>` — the whole report, not one replay
//!
//! `EAC_REPLAY_REPORT` above is fidelity replay: the recorded bytes go out as
//! they were, only the live identity is swapped in. `EAC_FORGE_STREAM` is the
//! *generator*: the recording is treated as one message
//! ([`Report::messages`]), and every frame is rebuilt — `Flags` with
//! `StartData` on the first and `EndData` on the last frame of the message,
//! `Sequence` strictly +1 from the generator's base, and the 32-byte message
//! header of the first frame patched (`total_len` from the frame count,
//! `counter` +1 per message, `nonce` +1 per report, `action`/`mode`/`kind` as
//! recorded, per-session blob ours in the `notes`/`bump` modes). `nonce`,
//! `counter` and the blob are the fields a frame source can actually keep
//! *consistent* with the message it declares, which is what `EAC_FORGE`'s
//! one-fragment-per-heartbeat never did (`findings/report_stream.md` §2).
//!
//! Two transports carry the result:
//!
//! * `EAC_FORGE_VIA=wrapper` (default) — the existing EOSSDK takeover: `eos`
//!   pulls one frame per slot through [`Stream::next_frame`] and hands it to the
//!   game's `MessageToServer` callback, exactly like `EAC_REPLAY_REPORT`;
//! * `EAC_FORGE_VIA=photon` — a sender thread of our own renders the same
//!   stream into the ring of [`crate::forge::push_frame`] and calls
//!   [`hromatite_core::photon::emulate_send`], i.e. `RaiseEvent(253, {244: 66,
//!   245: frame})` straight from the managed `PhotonPeer`. No EOS module and no
//!   EOSSDK callback is involved, so the cadence has to come from the thread.
//!   `EAC_FORGE_SPOOL=<path>` additionally appends every frame in capture
//!   format, which is what `EAC_EVENT_SEND=<path>` (photon's own
//!   `tick_send`) reads.
//!
//! Everything is `std`; the only shared state is the ring, and `eos` still owns
//! the clock and the wrapper delivery.

use std::fs::File;
use std::io::{self, Read};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::capture::Frame;
use crate::digest::crc32c_eac;
use crate::forge::{
    self, CHECKSUM_OFFSET, FLAG_END, FLAG_START, FRAME_LEN, HEADER_LEN, MSG_BLOB_LEN,
    MSG_HEADER_LEN, StreamBody, StreamConfig, TotalMode, Via,
};

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
    /// Frame ranges of the messages the recording holds, split on `StartData`.
    messages: Vec<Range<usize>>,
    /// Set when `EAC_FORGE_STREAM` asked for a generated stream out of this
    /// report; [`Stream::new`] then rebuilds every frame instead of replaying
    /// it.
    generator: Option<GenConfig>,
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
    /// [`Stream::new`] rebuilds every frame instead of replaying it. With the
    /// variable unset the report behaves exactly as before.
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
fn split_messages(frames: &[Frame]) -> Vec<Range<usize>> {
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
/// Pure state machine, no locks and no I/O: [`Stream::next_frame`] owns one
/// behind its mutex, and the Photon sender thread owns a second one built from
/// the same report, so both transports produce the same bytes.
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
fn spawn_photon_sender(report: &Report, config: &GenConfig) {
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
    /// `EAC_FORGE_STREAM*`, when that mode asked for this report: the parsed
    /// [`StreamConfig`] plus the render settings it implies.
    pub stream: Option<StreamConfig>,
}

impl Config {
    /// Parse the environment; `None` when neither `EAC_REPLAY_REPORT` nor
    /// `EAC_FORGE_STREAM` is set (then the generator keeps building frames from
    /// the template).
    ///
    /// `EAC_FORGE_STREAM=<path>` is the same takeover with a different frame
    /// source, so it arms the same machinery: the path doubles as the report
    /// path, `EAC_FORGE_STREAM_LOOP` as the loop switch, and the stream's own
    /// deadline/readiness knobs win over the `EAC_REPLAY_*` ones (which stay the
    /// fallback, as before).
    ///
    /// Takes a lookup so the function is testable without the process
    /// environment, exactly like [`crate::forge::Config::parse`].
    pub fn parse(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let stream = StreamConfig::parse(&get);
        let path = get("EAC_REPLAY_REPORT")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .or_else(|| {
                stream
                    .as_ref()
                    .map(|config| config.path.display().to_string())
            })?;
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
        let (stream_loop, stream_after, stream_min) = stream
            .as_ref()
            .map(|config| (config.looping, config.after_ms, config.min_frames))
            .unwrap_or((false, DEFAULT_AFTER_MS, DEFAULT_MIN_FRAMES));
        Some(Self {
            path: PathBuf::from(path),
            looping: flag("EAC_REPLAY_REPORT_LOOP") || stream_loop,
            flags_live: get("EAC_REPLAY_FLAGS")
                .map(|value| value.trim().eq_ignore_ascii_case("live"))
                .unwrap_or(false),
            start: number("EAC_REPLAY_START", 0) as usize,
            after_ms: number("EAC_REPLAY_AFTER_MS", stream_after),
            min_frames: number("EAC_REPLAY_MIN_FRAMES", stream_min),
            stream,
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

    // ---------------------------------------------------------- stream mode

    /// The live capture every invariant below is proven against.
    const REAL_CAPTURE: &str = "/home/wilpdrake/EAC-RE/evidence/control_0009/capture.bin";

    /// The whole recorded report of that capture (message 1, 117 frames).
    const REPORT_FULL: &str = "/home/wilpdrake/EAC-RE/evidence/report_full/report_a_sid158b.bin";

    fn real_capture() -> Option<Vec<u8>> {
        std::fs::read(REAL_CAPTURE).ok()
    }

    fn crc_holds(frame: &[u8]) -> bool {
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

    fn flags_of(frame: &[u8]) -> u16 {
        u16::from_le_bytes([frame[2], frame[3]])
    }

    fn seq_of(frame: &[u8]) -> u64 {
        u64::from_le_bytes(frame[12..20].try_into().expect("8 bytes"))
    }

    fn sid_of(frame: &[u8]) -> u64 {
        u64::from_le_bytes(frame[4..12].try_into().expect("8 bytes"))
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

    #[test]
    fn the_stream_config_arms_the_report_for_generation() {
        let config = Config::parse(|name| match name {
            "EAC_FORGE_STREAM" => Some(REPORT_FULL.to_string()),
            "EAC_FORGE_STREAM_LOOP" => Some("1".to_string()),
            "EAC_FORGE_STREAM_BODY" => Some("notes".to_string()),
            "EAC_FORGE_STREAM_AFTER_MS" => Some("45000".to_string()),
            _ => None,
        })
        .expect("stream mode arms the replay machinery");
        assert_eq!(config.path, PathBuf::from(REPORT_FULL));
        assert!(config.looping);
        let stream = config.stream.expect("stream knobs");
        assert_eq!(stream.body, StreamBody::Notes);
        assert!(stream.looping);
        assert_eq!(config.after_ms, 45_000, "stream deadline wins");

        // Backwards compatibility: `EAC_REPLAY_REPORT` alone still arms the
        // plain replay and nothing else.
        let replay = Config::parse(|name| match name {
            "EAC_REPLAY_REPORT" => Some(REPORT_FULL.to_string()),
            _ => None,
        })
        .expect("replay mode");
        assert!(replay.stream.is_none());
        assert_eq!(replay.after_ms, DEFAULT_AFTER_MS);

        // And a report loaded by path without `EAC_FORGE_STREAM` does not
        // generate anything, even when the file exists.
        if let Ok(report) = Report::load(REPORT_FULL) {
            assert!(!report.generates(), "plain replay stays plain");
        }
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
