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
//! The module is three parts:
//!
//! * `parser.rs` — [`Report`], the parsed recording: its 512-byte frames in
//!   recorded order, the messages they form, and the skipped/truncated
//!   accounting;
//! * `generator.rs` — [`GenConfig`] and [`Generator`], which rebuild every frame
//!   of the recording as one consistent message instead of replaying its bytes;
//! * `stream.rs` — [`Stream`], the cursor `eos` pulls frames from, and
//!   [`retarget`], which moves the live session's identity into a frame.
//!
//! [`Config`] below holds the `EAC_REPLAY_*` knobs, read once at install time.

use std::path::PathBuf;

use crate::forge::StreamConfig;

mod generator;
mod parser;
mod stream;

pub use generator::{GenConfig, Generator};
pub use parser::{Report, record};
pub use stream::{Slot, Stream, retarget};

/// Default `EAC_REPLAY_AFTER_MS`: how long the live stream stays real.
pub const DEFAULT_AFTER_MS: u64 = 30_000;
/// Default `EAC_REPLAY_MIN_FRAMES`: live 512-byte frames learned first, because
/// the template of the live session supplies the `SessionId` to retarget to.
pub const DEFAULT_MIN_FRAMES: u64 = 20;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::forge::StreamBody;
    use crate::report::parser::fixtures::REPORT_FULL;

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
}
