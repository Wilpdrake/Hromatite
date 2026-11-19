//! `EAC_FORGE=1` — assemble our own 512-byte heartbeat frames.
//!
//! The captured frames are **not encrypted** (`findings/frame_structure.md`,
//! verified on 451/451 frames from five sessions):
//!
//! ```text
//! frame[0:2]   u16  Version  = 2
//! frame[2:4]   u16  Flags    (bit0 StartData, bit1 EndData)
//! frame[4:12]  u64  SessionId
//! frame[12:20] u64  Sequence (scoped over every frame, not just 512-byte ones)
//! frame[20:24] u32  CRC-32C over the frame with these 4 bytes zeroed
//!                   (init=0, xorout=0 — [`crate::digest::crc32c_eac`])
//! frame[24:]        message fragment (488 bytes in a 512-byte frame)
//! ```
//!
//! So a "heartbeat" can be synthesized: keep the live session's `SessionId`,
//! `Flags` and the body of the last real frame, advance `Sequence` by one per
//! frame and recompute the CRC. What the body *means* is unknown (it is a
//! fragment of a long integrity report); repeating the last fragment is the
//! cheapest hypothesis and the live run is what decides it.
//!
//! # The whole-report stream
//!
//! `EAC_FORGE_STREAM=<path>` (see [`StreamConfig`]) is different in kind: the
//! heartbeat is not a self-contained frame but a *fragment of a long report*
//! (144 frames, ~70 KiB, ~288 s at 2007 ms), and the report starts with a
//! plaintext 32-byte message header ([`MessageHeader`]) inside the body of the
//! frame that carries `StartData`. A frame source that emits one recorded
//! fragment forever therefore lies about `total_len`, about the message counter
//! and about the per-session blob. The stream mode renders a whole recorded
//! report as one message: `total_len`, `counter`, `nonce`, the boundary flags
//! ([`FLAG_START`]/[`FLAG_END`]) and the per-session blob are ours, the 488-byte
//! bodies stay the recording's unless [`StreamBody::Notes`] or
//! [`StreamBody::Bump`] asks
//! otherwise.
//!
//! Two transports can carry those frames: the EOSSDK `MessageToServer` callback
//! (the existing `EAC_FORGE` path, driven by `eos`) and the managed Photon
//! emitter ([`Via::Photon`], [`pump_photon`]) which does not touch EOSSDK at
//! all. The ring between generator and emitter ([`push_frame`]) is this file's
//! only shared state.
//!
//! The rest of the module is pure `std`: `eos` owns the clock, the environment
//! and the wrapper delivery, this file owns the bytes.

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::digest::crc32c_eac;

/// Size of the frame that carries the heartbeat on the wire.
pub const FRAME_LEN: usize = 512;
/// Packed header length.
pub const HEADER_LEN: usize = 24;
/// Message fragment carried by one frame.
pub const BODY_LEN: usize = FRAME_LEN - HEADER_LEN;
/// Protocol version in every captured frame.
pub const VERSION: u16 = 2;
/// Median inter-frame interval of the live 512-byte stream (1.9–2.15 s in
/// 114 of 118 measured steps). The real cadence comes from the EAC module.
pub const INTERVAL_MS: u64 = 2_007;
/// Offset of the checksum inside the header.
pub const CHECKSUM_OFFSET: usize = 20;
/// Offset of the per-message `u32` counter inside a message (only meaningful
/// when `Flags & StartData`, i.e. the frame begins a message).
pub const MESSAGE_COUNTER_OFFSET: usize = 0x18;
/// Flag bit: this frame starts a logical message.
pub const FLAG_START: u16 = 0x0001;
/// Default deadline after `BeginSession` when `EAC_FORGE=1` is set but
/// `EAC_FORGE_AFTER_MS` is not.
pub const DEFAULT_AFTER_MS: u64 = 30_000;
/// Bounds for `EAC_FORGE_INTERVAL_MS`.
pub const MIN_INTERVAL_MS: u64 = 500;
/// Bounds for `EAC_FORGE_INTERVAL_MS`.
pub const MAX_INTERVAL_MS: u64 = 10_000;

/// Everything needed to rebuild a frame of the current session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Template {
    /// `SessionId` of the live session the frame belongs to.
    pub session_id: u64,
    /// `Flags` of the frame being imitated.
    pub flags: u16,
    /// `Sequence` of the live frame this template was learned from.
    pub last_seq: u64,
    /// Message fragment, exactly [`BODY_LEN`] bytes.
    pub body: Vec<u8>,
}

impl Template {
    /// Learn a template from a captured 512-byte frame.
    ///
    /// Rejects anything that is not a 512-byte `Version == 2` frame: a 24-byte
    /// ACK or a handshake fragment cannot stand in for the heartbeat.
    pub fn from_frame(frame: &[u8]) -> Option<Self> {
        if frame.len() != FRAME_LEN {
            return None;
        }
        let version = u16::from_le_bytes([frame[0], frame[1]]);
        if version != VERSION {
            return None;
        }
        Some(Self {
            session_id: u64::from_le_bytes(frame[4..12].try_into().ok()?),
            flags: u16::from_le_bytes([frame[2], frame[3]]),
            last_seq: u64::from_le_bytes(frame[12..20].try_into().ok()?),
            body: frame[HEADER_LEN..].to_vec(),
        })
    }

    /// True when the imitated frame starts a message, so its body carries the
    /// message header (`total_len`, `inner_ck`, `action`, …).
    pub fn starts_message(&self) -> bool {
        self.flags & FLAG_START != 0
    }

    /// `Sequence` the next synthesized frame should carry.
    pub fn next_seq(&self) -> u64 {
        self.last_seq.wrapping_add(1)
    }
}

/// Why a frame is being built — kept in the type so the `bump` variant cannot
/// be switched on by accident for a middle-of-message fragment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BodyMode {
    /// Reuse the learned fragment byte for byte.
    #[default]
    Stale,
    /// Reuse it, but write `n` into the per-message `u32` counter at `msg+0x18`
    /// when the frame starts a message. The emitter owns `n`, so the counter
    /// really advances from frame to frame — a cheap "the data is fresh"
    /// variant that the live run can falsify.
    Counter(u32),
}

impl BodyMode {
    /// Parse `EAC_FORGE_BODY` (`stale` default, `bump` opt-in).
    ///
    /// The parsed value only carries the *intent*; the counter itself is owned
    /// by the emitter (see `eos::emit_forge_frame`).
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("bump") | Some("fresh") => Self::Counter(0),
            _ => Self::Stale,
        }
    }

    /// True when the body's message counter is refreshed per frame.
    pub fn bumped(self) -> bool {
        matches!(self, Self::Counter(_))
    }
}

/// `SessionId` of a 512-byte frame, without copying the body.
///
/// The epoch gate asks this of every live heartbeat, including the ones the
/// takeover drops: the identity of the live session is the only evidence that
/// the generator still belongs to an existing epoch.
pub fn frame_session_id(frame: &[u8]) -> Option<u64> {
    if frame.len() != FRAME_LEN || u16::from_le_bytes([*frame.first()?, *frame.get(1)?]) != VERSION
    {
        return None;
    }
    Some(u64::from_le_bytes(frame.get(4..12)?.try_into().ok()?))
}

/// Build one frame: header with `seq` and a fresh CRC over `template.body`.
pub fn build_frame(template: &Template, seq: u64, mode: BodyMode) -> Vec<u8> {
    let mut frame = Vec::with_capacity(FRAME_LEN);
    frame.extend_from_slice(&VERSION.to_le_bytes());
    frame.extend_from_slice(&template.flags.to_le_bytes());
    frame.extend_from_slice(&template.session_id.to_le_bytes());
    frame.extend_from_slice(&seq.to_le_bytes());
    frame.extend_from_slice(&[0u8; 4]); // checksum placeholder
    frame.extend_from_slice(&template.body);
    if let BodyMode::Counter(counter) = mode
        && template.starts_message()
    {
        write_message_counter(&mut frame, counter);
    }
    let crc = crc32c_eac(&frame);
    frame[CHECKSUM_OFFSET..HEADER_LEN].copy_from_slice(&crc.to_le_bytes());
    frame
}

/// Assemble a frame out of a prepared body and seal its checksum.
///
/// The body is used as given (it may already carry a rewritten message header),
/// so this is the constructor the whole-report stream uses, while
/// [`build_frame`] is the one-fragment forge.
pub fn seal_frame(session_id: u64, seq: u64, flags: u16, body: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(HEADER_LEN + body.len());
    frame.extend_from_slice(&VERSION.to_le_bytes());
    frame.extend_from_slice(&flags.to_le_bytes());
    frame.extend_from_slice(&session_id.to_le_bytes());
    frame.extend_from_slice(&seq.to_le_bytes());
    frame.extend_from_slice(&[0u8; 4]); // checksum placeholder
    frame.extend_from_slice(body);
    let crc = crc32c_eac(&frame);
    frame[CHECKSUM_OFFSET..HEADER_LEN].copy_from_slice(&crc.to_le_bytes());
    frame
}

/// Write `counter` into the `u32` at `frame[HEADER_LEN + 0x18]`.
///
/// The body is a message header only when the frame starts one, so the caller
/// checks [`Template::starts_message`] first; a short body degrades to a no-op.
fn write_message_counter(frame: &mut [u8], counter: u32) {
    let at = HEADER_LEN + MESSAGE_COUNTER_OFFSET;
    if let Some(slot) = frame.get_mut(at..at + 4) {
        slot.copy_from_slice(&counter.to_le_bytes());
    }
}

/// Which live frame becomes the template.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TemplateMode {
    /// The newest 512-byte frame of any kind (a middle fragment most of the
    /// time, `Flags = 0`).
    #[default]
    Last,
    /// The newest frame that *starts* a message (`Flags & 1`), so the body
    /// carries the message header (`total_len`, `inner_ck`, counter, blob).
    Start,
}

impl TemplateMode {
    /// Parse `EAC_FORGE_TEMPLATE` (`last` default, `start` opt-in).
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("start") | Some("header") => Self::Start,
            _ => Self::Last,
        }
    }
}

/// `EAC_FORGE*` knobs, read once at install time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
    /// `EAC_FORGE_AFTER_MS`: delay from `BeginSession`; 0 disables the forge.
    pub after_ms: u64,
    /// `EAC_FORGE_MIN_FRAMES`: how many live 512-byte frames to learn first.
    pub min_frames: u64,
    /// `EAC_FORGE_INTERVAL_MS`, clamped to [`MIN_INTERVAL_MS`]..=[`MAX_INTERVAL_MS`].
    pub interval_ms: u64,
    /// `EAC_FORGE_BODY`.
    pub body: BodyMode,
    /// `EAC_FORGE_KEEP_SMALL`: let the live 24-byte ACK/handshake frames keep
    /// flowing while the 512-byte heartbeat is synthesized. Off (replace
    /// everything) by default, which is the pure "forge carries the session"
    /// experiment.
    pub keep_small: bool,
    /// `EAC_FORGE_TEMPLATE`.
    pub template: TemplateMode,
}

impl Config {
    /// Disabled configuration (the default for every run that does not ask).
    pub const fn disabled() -> Self {
        Self {
            after_ms: 0,
            min_frames: 0,
            interval_ms: INTERVAL_MS,
            body: BodyMode::Stale,
            keep_small: false,
            template: TemplateMode::Last,
        }
    }

    /// True when `EAC_FORGE=1` (or `EAC_FORGE_AFTER_MS` alone) was requested.
    pub fn enabled(&self) -> bool {
        self.after_ms != 0
    }

    /// Parse the environment. `flag` is the already-read `EAC_FORGE` value and
    /// `get` a lookup for the remaining variables, so the function is testable
    /// without touching the process environment.
    pub fn parse(flag: bool, get: impl Fn(&str) -> Option<String>) -> Self {
        let number = |name: &str| {
            get(name)
                .and_then(|value| value.trim().parse::<u64>().ok())
                .unwrap_or(0)
        };
        let after_ms = if flag {
            let value = number("EAC_FORGE_AFTER_MS");
            if get("EAC_FORGE_AFTER_MS").is_none() {
                DEFAULT_AFTER_MS
            } else {
                value
            }
        } else {
            number("EAC_FORGE_AFTER_MS")
        };
        let interval = number("EAC_FORGE_INTERVAL_MS");
        // `EAC_FORGE_STREAM=<path>` alone is enough to arm the generator: the
        // report path is read by `report::Config`, but the deadline and the
        // readiness knob belong here so a stream run does not need EAC_FORGE=1.
        let mut after_ms = after_ms;
        if after_ms == 0 && get("EAC_FORGE_STREAM").is_some_and(|value| !value.trim().is_empty()) {
            after_ms = DEFAULT_AFTER_MS;
        }
        Self {
            after_ms,
            min_frames: number("EAC_FORGE_MIN_FRAMES"),
            interval_ms: if interval == 0 {
                INTERVAL_MS
            } else {
                interval.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS)
            },
            body: BodyMode::parse(get("EAC_FORGE_BODY").as_deref()),
            keep_small: get("EAC_FORGE_KEEP_SMALL")
                .map(|value| !value.trim().is_empty() && value.trim() != "0")
                .unwrap_or(false),
            template: TemplateMode::parse(get("EAC_FORGE_TEMPLATE").as_deref()),
        }
    }
}

// ===========================================================================
// Whole-report stream (`EAC_FORGE_STREAM=<path>`)
// ===========================================================================

/// Bytes of a message-start body that precede the integrity records.
///
/// Measured on every capture in `~/EAC-RE/evidence`: seven `u32` fields
/// (`0x00..0x1C`), the 48-byte per-session blob (`0x1C..0x4C`) and two more
/// bytes (`0x4C..0x4E`) before the first record.
pub const MSG_HEADER_LEN: usize = 0x4E;
/// Length of the fixed `u32` fields in front of the blob (7 x `u32`).
pub const MSG_FIELDS_LEN: usize = 0x1C;
/// `+0x00 u32` — declared block length. **Not** the message size: it stays
/// `1592` for action 13 and `2302` for action 4 across messages that are 35 to
/// 209 frames long, so a generator must copy it rather than recompute it.
pub const MSG_TOTAL_LEN_OFF: usize = 0x00;
/// `+0x04 u32` — constant `1` in every observed message (meaning unknown).
pub const MSG_CONSTANT_OFF: usize = 0x04;
/// `+0x08 u32` — per-message nonce (differs between messages of one session).
pub const MSG_NONCE_OFF: usize = 0x08;
/// `+0x0C u32` — constant `5` in every observed report (unknown meaning).
pub const MSG_MODE_OFF: usize = 0x0C;
/// `+0x10 u32` — report action (`13` first report, `4` the following ones).
pub const MSG_ACTION_OFF: usize = 0x10;
/// `+0x14 u32` — constant `3` in every observed report (unknown meaning).
pub const MSG_KIND_OFF: usize = 0x14;
/// `+0x18 u32` — message counter, `+1` per message (`22 -> 35` in
/// `control_0009`, `157` in the 2026-11-18 live session). The low byte of this
/// counter is what earlier notes mis-read as a "flags byte".
pub const MSG_COUNTER_OFF: usize = 0x18;
/// `+0x1C` — 48-byte per-session blob (constant inside one session, different
/// between sessions; `findings/frame_structure.md` §5).
pub const MSG_BLOB_OFF: usize = 0x1C;
/// Length of that blob.
pub const MSG_BLOB_LEN: usize = 48;
/// Length of one integrity record inside a message.
pub const RECORD_LEN: usize = 16;
/// `Flags` bit 1: this frame ends a logical message.
///
/// Never observed on a 512-byte frame (a message simply ends where the next
/// `StartData` begins), so the generator must not invent it.
pub const FLAG_END: u16 = 0x0002;

/// Declared message length for a report carried by `frames` full frames.
///
/// Each frame contributes [`BODY_LEN`] bytes, so `frames * 512 - 24` — the
/// formula the recorded headers follow whenever the last frame is full. The
/// recording also shows *partial* tails (`0x638` = 3x488 + 128, `0x8fe` =
/// 4x488 + 350), i.e. the field is a byte count of the block, not a frame
/// count; [`TotalMode`] keeps both hypotheses reachable.
pub fn message_total_len(frames: usize) -> u32 {
    (frames * FRAME_LEN - HEADER_LEN) as u32
}

/// How the 32-byte message header's `total_len` is produced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TotalMode {
    /// Copy the recorded value from the source report byte for byte
    /// (the default: `+0x00` is a constant per action, not a frame count).
    #[default]
    Recorded,
    /// `frames * 512 - 24`, the old hypothesis. Kept only as an experiment;
    /// it contradicts every measured message (35..209 frames, same value).
    Frames,
}

impl TotalMode {
    /// Parse `EAC_FORGE_STREAM_TOTAL` (`recorded` default, `frames` opt-in).
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("frames") | Some("count") => Self::Frames,
            _ => Self::Recorded,
        }
    }

    /// Lowercase name for log lines.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Frames => "frames",
            Self::Recorded => "recorded",
        }
    }
}

/// What happens to the recorded 488-byte body of every emitted frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum StreamBody {
    /// Recorded body, byte for byte (the replay fidelity mode).
    #[default]
    Copy,
    /// Recorded body, but the 48-byte per-session blob is replaced by one
    /// derived from the *live* `SessionId` (or `EAC_FORGE_BLOB=<hex>`).
    Notes,
    /// `Notes` plus a per-frame increment of the trailing `u32` of every
    /// 16-byte record, so the records are not byte-identical when a report is
    /// repeated by `EAC_FORGE_STREAM_LOOP=1`.
    Bump,
}

impl StreamBody {
    /// Parse `EAC_FORGE_STREAM_BODY` (`copy` default, `notes`/`bump` opt-in).
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("notes") | Some("blob") => Self::Notes,
            Some("bump") | Some("fresh") => Self::Bump,
            _ => Self::Copy,
        }
    }

    /// Lowercase name for log lines.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Copy => "copy",
            Self::Notes => "notes",
            Self::Bump => "bump",
        }
    }

    /// True when the per-session blob is ours rather than the recording's.
    pub fn owns_blob(self) -> bool {
        !matches!(self, Self::Copy)
    }

    /// True when the integrity records are nudged per frame.
    pub fn bumps_records(self) -> bool {
        matches!(self, Self::Bump)
    }
}

/// Transport that carries the generated frames out of the process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Via {
    /// The EOSSDK `MessageToServer` callback (the existing `EAC_FORGE` path).
    #[default]
    Wrapper,
    /// The managed `PhotonPeer.SendOperation` RaiseEvent emitter
    /// ([`hromatite_core::photon::emulate_send`]) — no EOS module involved.
    Photon,
    /// Both: the wrapper path *and* the Photon ring.
    Both,
}

impl Via {
    /// Parse `EAC_FORGE_VIA` (`wrapper` default, `photon`, `both`).
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("photon") | Some("event") | Some("raise") => Self::Photon,
            Some("both") | Some("all") => Self::Both,
            _ => Self::Wrapper,
        }
    }

    /// Lowercase name for log lines.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Wrapper => "wrapper",
            Self::Photon => "photon",
            Self::Both => "both",
        }
    }

    /// True when the frames also go into the Photon ring.
    pub fn uses_photon(self) -> bool {
        matches!(self, Self::Photon | Self::Both)
    }
}

/// The plaintext message header of a report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MessageHeader {
    /// `+0x00` — declared block length (constant per action; copy, never derive
    /// it from the frame count).
    pub total_len: u32,
    /// `+0x04` — constant `1` in every observed message.
    pub constant: u32,
    /// `+0x08` — per-message nonce.
    pub nonce: u32,
    /// `+0x0C` — constant `5`.
    pub mode: u32,
    /// `+0x10` — report action (`13` first, `4` later).
    pub action: u32,
    /// `+0x14` — constant `3`.
    pub kind: u32,
    /// `+0x18` — message counter, `+1` per message.
    pub counter: u32,
    /// `+0x1C` — 48-byte per-session blob.
    pub blob: [u8; MSG_BLOB_LEN],
}

impl Default for MessageHeader {
    /// Constants stay at their observed values so a message whose header could
    /// not be parsed is still shaped like a real one; the rest is zero, which
    /// makes a bad write visible in the capture instead of plausible.
    fn default() -> Self {
        Self {
            total_len: 0,
            constant: 1,
            nonce: 0,
            mode: 5,
            action: 0,
            kind: 3,
            counter: 0,
            blob: [0u8; MSG_BLOB_LEN],
        }
    }
}

/// Little-endian `u32` at `at`, or `None` when `buf` is too short.
fn u32_at(buf: &[u8], at: usize) -> Option<u32> {
    let bytes: [u8; 4] = buf.get(at..at + 4)?.try_into().ok()?;
    Some(u32::from_le_bytes(bytes))
}

/// Write a little-endian `u32`, silently doing nothing when the slot is short.
fn put_u32(buf: &mut [u8], at: usize, value: u32) {
    if let Some(slot) = buf.get_mut(at..at + 4) {
        slot.copy_from_slice(&value.to_le_bytes());
    }
}

impl MessageHeader {
    /// Parse the header at the start of a `StartData` frame's body.
    pub fn parse(body: &[u8]) -> Option<Self> {
        if body.len() < MSG_BLOB_OFF + MSG_BLOB_LEN {
            return None;
        }
        Some(Self {
            total_len: u32_at(body, MSG_TOTAL_LEN_OFF)?,
            constant: u32_at(body, MSG_CONSTANT_OFF)?,
            nonce: u32_at(body, MSG_NONCE_OFF)?,
            mode: u32_at(body, MSG_MODE_OFF)?,
            action: u32_at(body, MSG_ACTION_OFF)?,
            kind: u32_at(body, MSG_KIND_OFF)?,
            counter: u32_at(body, MSG_COUNTER_OFF)?,
            blob: body
                .get(MSG_BLOB_OFF..MSG_BLOB_OFF + MSG_BLOB_LEN)?
                .try_into()
                .ok()?,
        })
    }

    /// Write the header back into `body`.
    ///
    /// `blob` overrides the per-session blob (`EAC_FORGE_BLOB` or the live
    /// session's own); `None` keeps the one this header was parsed with.
    /// Returns `false` when `body` is too short.
    pub fn write(&self, body: &mut [u8], blob: Option<&[u8; MSG_BLOB_LEN]>) -> bool {
        if body.len() < MSG_BLOB_OFF + MSG_BLOB_LEN {
            return false;
        }
        put_u32(body, MSG_TOTAL_LEN_OFF, self.total_len);
        put_u32(body, MSG_CONSTANT_OFF, self.constant);
        put_u32(body, MSG_NONCE_OFF, self.nonce);
        put_u32(body, MSG_MODE_OFF, self.mode);
        put_u32(body, MSG_ACTION_OFF, self.action);
        put_u32(body, MSG_KIND_OFF, self.kind);
        put_u32(body, MSG_COUNTER_OFF, self.counter);
        let blob = blob.unwrap_or(&self.blob);
        body[MSG_BLOB_OFF..MSG_BLOB_OFF + MSG_BLOB_LEN].copy_from_slice(blob);
        true
    }
}

/// The 48-byte per-session blob of a message start, if the body carries one.
pub fn recorded_blob(body: &[u8]) -> Option<[u8; MSG_BLOB_LEN]> {
    body.get(MSG_BLOB_OFF..MSG_BLOB_OFF + MSG_BLOB_LEN)?
        .try_into()
        .ok()
}

/// Add `delta` to the trailing `u32` of every whole 16-byte record of `body`.
///
/// `EAC_FORGE_STREAM_BODY=bump` uses this so a looped report is not a
/// byte-identical copy of the previous pass. Whether the server's records are
/// hashes (and therefore indifferent to a +n on their tail) or counters is
/// exactly what a live run answers.
pub fn bump_records(body: &mut [u8], first_record: usize, delta: u32) {
    let mut at = first_record;
    while at + RECORD_LEN <= body.len() {
        let tail = at + RECORD_LEN - 4;
        if let Some(value) = u32_at(body, tail) {
            put_u32(body, tail, value.wrapping_add(delta));
        }
        at += RECORD_LEN;
    }
}

// --------------------------------------------------------------- config

/// `EAC_FORGE_STREAM*` knobs: a whole recorded report rendered as our own
/// stream, one frame per slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StreamConfig {
    /// `EAC_FORGE_STREAM`: capture-format file holding the recorded report.
    pub path: PathBuf,
    /// `EAC_FORGE_STREAM_BODY`.
    pub body: StreamBody,
    /// `EAC_FORGE_STREAM_LOOP=1`: start a new report when the recording ends.
    pub looping: bool,
    /// `EAC_FORGE_VIA`.
    pub via: Via,
    /// `EAC_FORGE_SID`: `SessionId` to write; `None` keeps the recording's (the
    /// EOSSDK path overrides it with the live one anyway).
    pub session: Option<u64>,
    /// `EAC_FORGE_SEQ`: first `Sequence` of our stream.
    pub seq: Option<u64>,
    /// `EAC_FORGE_NONCE`: first nonce; `None` continues the recording's.
    pub nonce: Option<u32>,
    /// `EAC_FORGE_COUNTER`: first message counter; `None` continues the
    /// recording's.
    pub counter: Option<u32>,
    /// `EAC_FORGE_BLOB`: 48 bytes of per-session material as hex.
    pub blob: Option<[u8; MSG_BLOB_LEN]>,
    /// `EAC_FORGE_STREAM_TOTAL`.
    pub total: TotalMode,
    /// `EAC_FORGE_STREAM_MS`, clamped like [`Config::interval_ms`].
    pub interval_ms: u64,
    /// `EAC_FORGE_SPOOL`: append every emitted frame here (capture format), so
    /// `EAC_EVENT_SEND=<that file>` can hand the same bytes to
    /// `photon::tick_send`.
    pub spool: Option<PathBuf>,
    /// `EAC_FORGE_STREAM_AFTER_MS` (falls back to `EAC_FORGE_AFTER_MS`).
    pub after_ms: u64,
    /// `EAC_FORGE_STREAM_MIN_FRAMES`.
    pub min_frames: u64,
}

impl StreamConfig {
    /// Parse the environment; `None` when `EAC_FORGE_STREAM` is absent or blank.
    ///
    /// Takes a lookup so the function is testable without the process
    /// environment, exactly like [`Config::parse`].
    pub fn parse(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let path = get("EAC_FORGE_STREAM")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())?;
        let number = |name: &str| -> Option<u64> {
            get(name).and_then(|value| {
                let value = value.trim();
                value
                    .strip_prefix("0x")
                    .map(|hex| u64::from_str_radix(hex, 16).ok())
                    .unwrap_or_else(|| value.parse::<u64>().ok())
            })
        };
        let flag = |name: &str| {
            get(name)
                .map(|value| {
                    let value = value.trim();
                    !value.is_empty() && value != "0"
                })
                .unwrap_or(false)
        };
        let interval = number("EAC_FORGE_STREAM_MS").unwrap_or(0);
        Some(Self {
            path: PathBuf::from(path),
            body: StreamBody::parse(get("EAC_FORGE_STREAM_BODY").as_deref()),
            looping: flag("EAC_FORGE_STREAM_LOOP"),
            via: Via::parse(get("EAC_FORGE_VIA").as_deref()),
            session: number("EAC_FORGE_SID"),
            seq: number("EAC_FORGE_SEQ"),
            nonce: number("EAC_FORGE_NONCE").map(|value| value as u32),
            counter: number("EAC_FORGE_COUNTER").map(|value| value as u32),
            blob: get("EAC_FORGE_BLOB").and_then(|value| parse_blob(value.trim())),
            total: TotalMode::parse(get("EAC_FORGE_STREAM_TOTAL").as_deref()),
            interval_ms: if interval == 0 {
                INTERVAL_MS
            } else {
                interval.clamp(MIN_INTERVAL_MS, MAX_INTERVAL_MS)
            },
            spool: get("EAC_FORGE_SPOOL")
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
                .map(PathBuf::from),
            after_ms: number("EAC_FORGE_STREAM_AFTER_MS")
                .or_else(|| number("EAC_FORGE_AFTER_MS"))
                .unwrap_or(DEFAULT_AFTER_MS),
            min_frames: number("EAC_FORGE_STREAM_MIN_FRAMES")
                .or_else(|| number("EAC_FORGE_MIN_FRAMES"))
                .unwrap_or(0),
        })
    }
}

/// Exactly 48 bytes of hex (96 digits) → the per-session blob.
fn parse_blob(value: &str) -> Option<[u8; MSG_BLOB_LEN]> {
    let digits: String = value.chars().filter(|c| !c.is_whitespace()).collect();
    let digits = digits.strip_prefix("0x").unwrap_or(&digits);
    if digits.len() != MSG_BLOB_LEN * 2 {
        return None;
    }
    let mut out = [0u8; MSG_BLOB_LEN];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digits[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(out)
}

// ----------------------------------------------------- photon delivery ring

/// Capacity of the ring between the stream generator and the Photon sender: a
/// full report (144 frames) plus slack.
pub const RING_MAX: usize = 256;
/// Lines kept by [`take_log`] (the module has no logger of its own: the EOS
/// hook owns that one, and a frame source must not depend on it).
pub const LOG_MAX: usize = 1024;

static RING: Mutex<VecDeque<Vec<u8>>> = Mutex::new(VecDeque::new());
static PUSHED: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);
static PHOTON_SENT: AtomicU64 = AtomicU64::new(0);
static PHOTON_FAILED: AtomicU64 = AtomicU64::new(0);
static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Append a line to the in-memory log (bounded, oldest dropped first).
pub fn note(line: impl Into<String>) {
    if let Ok(mut log) = LOG.lock() {
        log.push(line.into());
        let excess = log.len().saturating_sub(LOG_MAX);
        if excess > 0 {
            log.drain(..excess);
        }
    }
}

/// Take every line [`note`] recorded since the last call.
pub fn take_log() -> Vec<String> {
    LOG.lock()
        .map(|mut log| log.drain(..).collect())
        .unwrap_or_default()
}

/// Frames pushed into the ring since the process started.
pub fn ring_pushed() -> u64 {
    PUSHED.load(Ordering::Relaxed)
}

/// Frames the ring had to drop because the sender could not keep up.
pub fn ring_dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// Frames the Photon emitter accepted / rejected.
pub fn photon_counters() -> (u64, u64) {
    (
        PHOTON_SENT.load(Ordering::Relaxed),
        PHOTON_FAILED.load(Ordering::Relaxed),
    )
}

/// Number of frames waiting in the ring.
pub fn ring_len() -> usize {
    RING.lock().map(|ring| ring.len()).unwrap_or(0)
}

/// Drop everything in the ring (a new session epoch starts from scratch).
pub fn clear_ring() {
    if let Ok(mut ring) = RING.lock() {
        ring.clear();
    }
}

/// Queue one ready-to-send frame. Returns `false` when it did not fit.
pub fn push_frame(frame: Vec<u8>) -> bool {
    let Ok(mut ring) = RING.lock() else {
        return false;
    };
    if ring.len() >= RING_MAX {
        ring.pop_front();
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
    ring.push_back(frame);
    PUSHED.fetch_add(1, Ordering::Relaxed);
    true
}

/// Take everything queued, oldest first.
pub fn drain_frames() -> Vec<Vec<u8>> {
    RING.lock()
        .map(|mut ring| ring.drain(..).collect())
        .unwrap_or_default()
}

/// What one [`pump_photon`] pass achieved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Delivery {
    /// Frames the emitter accepted.
    pub sent: u64,
    /// Frames it rejected; they go back to the front of the ring.
    pub failed: u64,
    /// True when no live event-66 template was known yet, so nothing was tried.
    pub unarmed: bool,
}

/// Hand every queued frame to the managed Photon emitter.
///
/// This is the transport that does not touch EOSSDK at all: the bytes are
/// written into the live `byte[]` captured from the game's own
/// `RaiseEvent(253, {244: 66, 245: payload})` and the original `SendOperation`
/// is called through its trampoline ([`hromatite_core::photon::emulate_send`]).
///
/// Only the `runtime` feature has an il2cpp layer to call, so the pure build
/// keeps the ring and the counters and reports the emitter as unarmed: a
/// feature image can queue frames for the resident to send, it just cannot send
/// them itself.
#[cfg(feature = "runtime")]
pub fn pump_photon() -> Delivery {
    if hromatite_core::photon::template().is_none() {
        return Delivery {
            unarmed: true,
            ..Delivery::default()
        };
    }
    let frames = drain_frames();
    let mut delivery = Delivery::default();
    let mut failed = VecDeque::new();
    for frame in frames {
        match hromatite_core::photon::emulate_send(&frame) {
            Ok(_) => {
                delivery.sent += 1;
                PHOTON_SENT.fetch_add(1, Ordering::Relaxed);
            }
            Err(reason) => {
                delivery.failed += 1;
                PHOTON_FAILED.fetch_add(1, Ordering::Relaxed);
                if PHOTON_FAILED.load(Ordering::Relaxed).is_multiple_of(10) {
                    note(format!("photon send failed: {reason}"));
                }
                failed.push_back(frame);
            }
        }
    }
    if !failed.is_empty()
        && let Ok(mut ring) = RING.lock()
    {
        for frame in failed.into_iter().rev() {
            ring.push_front(frame);
        }
        let excess = ring.len().saturating_sub(RING_MAX);
        if excess > 0 {
            ring.drain(..excess);
            DROPPED.fetch_add(excess as u64, Ordering::Relaxed);
        }
    }
    delivery
}

/// No il2cpp layer in this build: the ring stays queued and the pass reports
/// "not armed", exactly as it does before a live template is captured.
#[cfg(not(feature = "runtime"))]
pub fn pump_photon() -> Delivery {
    Delivery {
        unarmed: true,
        ..Delivery::default()
    }
}

/// Append one frame to `path` in the capture format (`u32 len | u32 dt | bytes`).
///
/// `EAC_FORGE_SPOOL` uses it so the very same ring can also be handed to
/// `photon::tick_send` through `EAC_EVENT_SEND=<path>`; that reader parses the
/// file once, so the spool is what a *second* pass would pick up.
pub fn append_spool(path: &Path, dt_ms: u32, frame: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    let mut record = Vec::with_capacity(8 + frame.len());
    record.extend_from_slice(&(frame.len() as u32).to_le_bytes());
    record.extend_from_slice(&dt_ms.to_le_bytes());
    record.extend_from_slice(frame);
    file.write_all(&record)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Body used for the cross-checked vector: `(i*7+3) % 251`, 488 bytes.
    fn sample_template() -> Template {
        Template {
            session_id: 0x297e,
            flags: 0,
            last_seq: 41,
            body: (0..BODY_LEN).map(|i| ((i * 7 + 3) % 251) as u8).collect(),
        }
    }

    #[test]
    fn a_built_frame_has_the_verified_header_and_crc() {
        let frame = build_frame(&sample_template(), 42, BodyMode::Stale);
        assert_eq!(frame.len(), FRAME_LEN);
        assert_eq!(&frame[0..2], &2u16.to_le_bytes());
        assert_eq!(&frame[2..4], &0u16.to_le_bytes());
        assert_eq!(&frame[4..12], &0x297e_u64.to_le_bytes());
        assert_eq!(&frame[12..20], &42u64.to_le_bytes());
        // Cross-checked with `tools/frame_analyze.py` (`build_frame` + `eac_crc32c`).
        assert_eq!(&frame[20..24], &0x538b_b492u32.to_le_bytes());
        assert_eq!(&frame[24..], &sample_template().body[..]);
    }

    #[test]
    fn the_crc_is_over_the_frame_with_the_field_zeroed() {
        let mut template = sample_template();
        template.session_id = 0x22d1;
        template.last_seq = 7;
        template.flags = 0x0000;
        let mut frame = build_frame(&template, 8, BodyMode::Stale);
        let stored = u32::from_le_bytes(frame[20..24].try_into().expect("4 bytes"));
        frame[20..24].copy_from_slice(&[0; 4]);
        assert_eq!(crc32c_eac(&frame), stored);

        // Sequence is the only header field that changes between frames.
        let next = build_frame(&template, 9, BodyMode::Stale);
        assert_eq!(&next[12..20], &9u64.to_le_bytes());
        assert_ne!(&next[20..24], &frame[20..24]);
    }

    #[test]
    fn a_template_is_learned_only_from_a_512_byte_version_2_frame() {
        let frame = build_frame(&sample_template(), 42, BodyMode::Stale);
        let learned = Template::from_frame(&frame).expect("512-byte frame");
        assert_eq!(learned.session_id, 0x297e);
        assert_eq!(learned.last_seq, 42);
        assert_eq!(learned.body.len(), BODY_LEN);
        assert_eq!(learned.next_seq(), 43);
        assert!(!learned.starts_message());

        assert!(Template::from_frame(&frame[..24]).is_none(), "24-byte ACK");
        let mut wrong_version = frame.clone();
        wrong_version[0] = 3;
        assert!(Template::from_frame(&wrong_version).is_none());
    }

    #[test]
    fn the_bump_mode_only_touches_a_message_counter() {
        let middle = sample_template();
        let plain = build_frame(&middle, 5, BodyMode::Stale);
        let bumped_middle = build_frame(&middle, 5, BodyMode::Counter(23));
        assert_eq!(plain, bumped_middle, "a middle fragment has no header");

        let mut start = sample_template();
        start.flags = FLAG_START;
        start.body[MESSAGE_COUNTER_OFFSET..MESSAGE_COUNTER_OFFSET + 4]
            .copy_from_slice(&22u32.to_le_bytes());
        let fresh = build_frame(&start, 5, BodyMode::Counter(23));
        assert_eq!(
            u32::from_le_bytes(
                fresh[HEADER_LEN + MESSAGE_COUNTER_OFFSET..HEADER_LEN + MESSAGE_COUNTER_OFFSET + 4]
                    .try_into()
                    .expect("4 bytes")
            ),
            23
        );
        let mut zeroed = fresh.clone();
        zeroed[20..24].copy_from_slice(&[0; 4]);
        assert_eq!(
            crc32c_eac(&zeroed),
            u32::from_le_bytes(fresh[20..24].try_into().expect("4 bytes"))
        );
    }

    #[test]
    fn config_is_off_unless_asked_and_defaults_to_the_measured_cadence() {
        let none = Config::parse(false, |_| None);
        assert!(!none.enabled());
        assert_eq!(none.interval_ms, INTERVAL_MS);
        assert!(!none.body.bumped());

        let flag_only = Config::parse(true, |_| None);
        assert!(flag_only.enabled());
        assert_eq!(flag_only.after_ms, DEFAULT_AFTER_MS);

        let explicit = Config::parse(true, |name| match name {
            "EAC_FORGE_AFTER_MS" => Some("90000".to_string()),
            "EAC_FORGE_MIN_FRAMES" => Some("20".to_string()),
            "EAC_FORGE_INTERVAL_MS" => Some("50".to_string()),
            "EAC_FORGE_BODY" => Some("bump".to_string()),
            "EAC_FORGE_KEEP_SMALL" => Some("1".to_string()),
            "EAC_FORGE_TEMPLATE" => Some("start".to_string()),
            _ => None,
        });
        assert_eq!(explicit.after_ms, 90_000);
        assert_eq!(explicit.min_frames, 20);
        assert_eq!(explicit.interval_ms, MIN_INTERVAL_MS, "clamped up");
        assert!(explicit.body.bumped());
        assert!(explicit.keep_small);
        assert_eq!(explicit.template, TemplateMode::Start);
        assert!(!none.keep_small);

        // `EAC_FORGE_AFTER_MS` alone is enough to switch the forge on.
        let after_only = Config::parse(false, |name| {
            (name == "EAC_FORGE_AFTER_MS").then(|| "15000".to_string())
        });
        assert!(after_only.enabled());
        assert_eq!(after_only.after_ms, 15_000);
    }

    /// A body carrying a message header with known fields.
    fn body_with_header() -> Vec<u8> {
        let header = MessageHeader {
            total_len: 0x638,
            constant: 1,
            nonce: 0x94bb_79c6,
            mode: 5,
            action: 13,
            kind: 3,
            counter: 157,
            blob: [0x11; MSG_BLOB_LEN],
        };
        let mut body = vec![0xA5u8; BODY_LEN];
        assert!(header.write(&mut body, None));
        body
    }

    #[test]
    fn the_message_header_parses_and_rewrites_field_by_field() {
        let mut body = body_with_header();
        let parsed = MessageHeader::parse(&body).expect("message header");
        assert_eq!(parsed.total_len, 0x638);
        assert_eq!(parsed.constant, 1, "0x04 is a constant, not a counter");
        assert_eq!(parsed.counter, 157, "the counter lives at 0x18");
        assert_eq!(parsed.nonce, 0x94bb_79c6);
        assert_eq!(parsed.mode, 5);
        assert_eq!(parsed.action, 13, "action of the first report");
        assert_eq!(parsed.kind, 3);
        assert_eq!(parsed.blob, [0x11; MSG_BLOB_LEN]);

        // A plain write keeps the blob and only touches the fields.
        let mut rewritten = body.clone();
        let mut next = parsed;
        next.total_len = 0x638;
        next.counter = 158;
        next.nonce = parsed.nonce.wrapping_add(1);
        assert!(next.write(&mut rewritten, None));
        let again = MessageHeader::parse(&rewritten).expect("header");
        assert_eq!(again.total_len, 0x638);
        assert_eq!(again.counter, 158);
        assert_eq!(again.constant, 1);
        assert_eq!(again.nonce, 0x94bb_79c7, "nonce advances per message");
        assert_eq!(again.action, 13, "action stays the recording's");
        assert_eq!(&rewritten[MSG_HEADER_LEN..], &body[MSG_HEADER_LEN..]);

        // An explicit blob replaces all 48 recorded bytes.
        let ours = [0x42u8; MSG_BLOB_LEN];
        let before = recorded_blob(&body).expect("recorded blob");
        assert!(next.write(&mut rewritten, Some(&ours)));
        assert_eq!(recorded_blob(&rewritten), Some(ours));
        assert_ne!(ours, before, "our blob is not the recording's");

        assert!(MessageHeader::parse(&body[..31]).is_none(), "short body");
        assert!(!next.write(&mut body[..8], None), "short body");
    }

    #[test]
    fn the_declared_length_is_a_constant_per_action_not_a_frame_count() {
        // Measured on 17 sessions and 2106 frames: `+0x00` stays 1592 for
        // action 13 and 2302 for action 4 while the message carrying it is
        // 35..209 frames long, so `TotalMode::Frames` must stay opt-in.
        assert_eq!(message_total_len(1), 488, "legacy helper, opt-in only");
        assert_eq!(message_total_len(4), 2_024);
        assert_eq!(MSG_HEADER_LEN, 0x4E, "records start after the blob");
        assert_eq!(MSG_BLOB_OFF + MSG_BLOB_LEN, 0x4C);
    }

    #[test]
    fn a_blob_is_copied_from_the_recording_or_from_the_live_header() {
        let body = body_with_header();
        assert_eq!(recorded_blob(&body), Some([0x11; MSG_BLOB_LEN]));
        assert_eq!(
            &body[MSG_BLOB_OFF..MSG_BLOB_OFF + MSG_BLOB_LEN],
            &[0x11; MSG_BLOB_LEN]
        );
        // There is no derivation of our own any more: a session blob that was
        // not observed cannot be minted, and pretending otherwise is what made
        // the earlier stream tests meaningless.
    }

    #[test]
    fn bumping_records_only_touches_the_trailing_counter() {
        let mut body: Vec<u8> = (0..BODY_LEN).map(|i| i as u8).collect();
        let original = body.clone();
        bump_records(&mut body, MSG_HEADER_LEN, 7);
        // Everything up to the first record (the message header) is untouched.
        assert_eq!(&body[..MSG_HEADER_LEN], &original[..MSG_HEADER_LEN]);
        let mut at = MSG_HEADER_LEN;
        let mut touched = 0usize;
        while at + RECORD_LEN <= BODY_LEN {
            let head = at..at + RECORD_LEN - 4;
            let tail = at + RECORD_LEN - 4..at + RECORD_LEN;
            assert_eq!(&body[head.clone()], &original[head], "record head intact");
            let expected = original[tail.clone()]
                .try_into()
                .map(u32::from_le_bytes)
                .expect("4 bytes")
                .wrapping_add(7);
            assert_eq!(
                u32::from_le_bytes(body[tail].try_into().expect("4 bytes")),
                expected,
                "record at {at} nudged"
            );
            touched += 1;
            at += RECORD_LEN;
        }
        assert_eq!(touched, (BODY_LEN - MSG_HEADER_LEN) / RECORD_LEN);
    }

    #[test]
    fn the_stream_config_defaults_to_copy_from_the_recording() {
        assert!(
            StreamConfig::parse(|_| None).is_none(),
            "no path, no stream"
        );

        let config = StreamConfig::parse(|name| match name {
            "EAC_FORGE_STREAM" => Some(" /tmp/a.bin ".to_string()),
            _ => None,
        })
        .expect("path set");
        assert_eq!(config.path, PathBuf::from("/tmp/a.bin"));
        assert_eq!(config.body, StreamBody::Copy);
        assert_eq!(config.via, Via::Wrapper);
        assert_eq!(config.total, TotalMode::Recorded, "copy the recorded value");
        assert_eq!(config.interval_ms, INTERVAL_MS);
        assert_eq!(config.after_ms, DEFAULT_AFTER_MS);
        assert!(!config.looping);
        assert!(config.session.is_none());

        let explicit = StreamConfig::parse(|name| match name {
            "EAC_FORGE_STREAM" => Some("/tmp/a.bin".to_string()),
            "EAC_FORGE_STREAM_BODY" => Some("bump".to_string()),
            "EAC_FORGE_STREAM_LOOP" => Some("1".to_string()),
            "EAC_FORGE_VIA" => Some("photon".to_string()),
            "EAC_FORGE_SID" => Some("0x158b".to_string()),
            "EAC_FORGE_SEQ" => Some("900".to_string()),
            "EAC_FORGE_NONCE" => Some("17".to_string()),
            "EAC_FORGE_COUNTER" => Some("4".to_string()),
            "EAC_FORGE_STREAM_TOTAL" => Some("recorded".to_string()),
            "EAC_FORGE_STREAM_MS" => Some("50".to_string()),
            "EAC_FORGE_SPOOL" => Some("/tmp/spool.bin".to_string()),
            "EAC_FORGE_STREAM_AFTER_MS" => Some("5000".to_string()),
            _ => None,
        })
        .expect("path set");
        assert_eq!(explicit.body, StreamBody::Bump);
        assert!(explicit.body.owns_blob());
        assert!(explicit.body.bumps_records());
        assert!(explicit.looping);
        assert_eq!(explicit.via, Via::Photon);
        assert!(explicit.via.uses_photon());
        assert_eq!(explicit.session, Some(0x158b));
        assert_eq!(explicit.seq, Some(900));
        assert_eq!(explicit.nonce, Some(17));
        assert_eq!(explicit.counter, Some(4));
        assert_eq!(explicit.total, TotalMode::Recorded);
        assert_eq!(explicit.interval_ms, MIN_INTERVAL_MS, "clamped up");
        assert_eq!(explicit.spool, Some(PathBuf::from("/tmp/spool.bin")));
        assert_eq!(explicit.after_ms, 5_000);

        // The blob knob wants exactly 48 bytes of hex.
        let hex = "ab".repeat(MSG_BLOB_LEN);
        let with_blob = StreamConfig::parse(|name| match name {
            "EAC_FORGE_STREAM" => Some("/tmp/a.bin".to_string()),
            "EAC_FORGE_BLOB" => Some(hex.clone()),
            _ => None,
        })
        .expect("path set");
        assert_eq!(with_blob.blob, Some([0xABu8; MSG_BLOB_LEN]));
        let short = StreamConfig::parse(|name| match name {
            "EAC_FORGE_STREAM" => Some("/tmp/a.bin".to_string()),
            "EAC_FORGE_BLOB" => Some("abcd".to_string()),
            _ => None,
        })
        .expect("path set");
        assert_eq!(short.blob, None, "a short blob is ignored, not fatal");

        // `EAC_FORGE_STREAM` alone arms the template forge's deadline too.
        let forge = Config::parse(false, |name| {
            (name == "EAC_FORGE_STREAM").then(|| "/tmp/a.bin".to_string())
        });
        assert!(forge.enabled(), "the stream is a forge mode of its own");
        assert_eq!(forge.after_ms, DEFAULT_AFTER_MS);
    }

    #[test]
    fn the_ring_queues_frames_and_reports_an_unarmed_emitter() {
        clear_ring();
        let before = ring_pushed();
        assert!(push_frame(vec![0x11; FRAME_LEN]));
        assert!(push_frame(vec![0x22; FRAME_LEN]));
        // Another test may run the Photon sender thread in parallel, so the
        // ring is checked by content, not by exact counts.
        assert!(ring_len() >= 2);
        assert!(ring_pushed() >= before + 2);
        let frames = drain_frames();
        assert!(frames.contains(&vec![0x11; FRAME_LEN]));
        assert!(frames.contains(&vec![0x22; FRAME_LEN]));
        assert_eq!(ring_len(), 0);

        // Host builds have no managed `SendOperation`, and no live event-66
        // template has been captured, so the pass reports "not armed" and
        // leaves the ring alone.
        push_frame(vec![0x33; FRAME_LEN]);
        let delivery = pump_photon();
        assert!(delivery.unarmed);
        assert_eq!(delivery.sent, 0);
        assert_eq!(ring_len(), 1, "nothing consumed while unarmed");
        clear_ring();

        // The ring is bounded: the oldest frame goes.
        for index in 0..=RING_MAX {
            push_frame(vec![index as u8; FRAME_LEN]);
        }
        assert_eq!(ring_len(), RING_MAX, "the ring is bounded");
        let dropped = drain_frames();
        assert!(
            dropped.len() == RING_MAX,
            "the ring never holds more than RING_MAX"
        );
        clear_ring();
    }
}
