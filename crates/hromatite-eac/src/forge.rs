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
//! The module is pure `std` and has no atomics: `eos` owns the clock, the
//! environment and the delivery, this file owns the bytes.

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
}
