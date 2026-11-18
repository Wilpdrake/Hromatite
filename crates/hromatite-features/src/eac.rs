//! `eac.*` command family: anti-cheat session epochs, heartbeat frames and the
//! JSONL correlation log.
//!
//! Everything heavy lives in `hromatite-eac` (pure std, host-testable); this
//! module only owns payload-side state and the console contract. Phase 2 adds
//! the inline hooks on the `EOS_AntiCheatClient_*` exports: the managed tick
//! installs them lazily (only when `EOSSDK-Win64-Shipping.dll` is mapped) and
//! drains their observations into the epoch model here. The console commands and
//! the `[AntiCheatClient]` log tailer remain as fallbacks when the hooks are
//! absent or disabled.
//!
//! **Two sources, one model.** When the hooks are owned by the resident image
//! (the normal case: the resident patches them before the first managed tick)
//! this image sees no outbound frames and no outbound counters. `eac.tap`
//! therefore attaches an incremental reader to the resident's capture file and
//! feeds the epoch model with the frames that really went out: real
//! `SessionId`/`Sequence`, transport `dt`, real 512-byte size and a verified
//! CRC. The synthetic path (`eac.tick`, `eac_auto=1`) stays available but is
//! disabled automatically while a tap is attached, so the JSONL never mixes
//! made-up heartbeats with observed ones.

use std::path::PathBuf;

use hromatite_abi::{STATUS_BAD_ARGUMENT, STATUS_ERROR, STATUS_NOT_READY, STATUS_OK};
use hromatite_eac::codec::{ChecksumKind, FRAME_LEN, HEADER_LEN, WIRE_HEARTBEAT_LEN};
use hromatite_eac::digest::sha256;
use hromatite_eac::encode_beat;
use hromatite_eac::eventlog::{Event, JsonlLog};
use hromatite_eac::payload::{integrity_digest, module_list_hash};
use hromatite_eac::session::{EacSession, MAX_INTERVAL_MS, MIN_INTERVAL_MS};
use hromatite_eac::tailer::{Tailer, announces_begin, announces_end};
use hromatite_eac::tap::CaptureTap;
use hromatite_eac::{Frame as WireFrame, HookEvent, Hooks};

/// Checksum used for the outgoing header and for verifying captured frames.
/// Measured, not guessed: raw CRC-32C over the whole frame with the four
/// checksum bytes zeroed (`codec::ChecksumKind::compute_frame`).
const CHECKSUM: ChecksumKind = ChecksumKind::Crc32c;

/// Default capture path written by the resident hooks: `%TEMP%\…`.
const DEFAULT_CAPTURE_NAME: &str = "hromatite_eac_capture.bin";

/// Heartbeat MAC key. **HYPOTHESIS:** the real key comes from the EOS session
/// handshake; until phase 2 this is a fixed derived constant so output is stable
/// and diffable between runs.
fn mac_key() -> [u8; 32] {
    sha256(b"hromatite-eac-heartbeat/v1")
}

/// Placeholder integrity catalog: the four files EAC provably signs. Phase 2
/// replaces the digests with hashes of the real on-disk files.
fn placeholder_integrity() -> [u8; 32] {
    let files = [
        ("VRChat.exe", sha256(b"VRChat.exe")),
        ("GameAssembly.dll", sha256(b"GameAssembly.dll")),
        (
            "EOSSDK-Win64-Shipping.dll",
            sha256(b"EOSSDK-Win64-Shipping.dll"),
        ),
        ("EasyAntiCheat_EOS.sys", sha256(b"EasyAntiCheat_EOS.sys")),
    ];
    integrity_digest(files)
}

/// Placeholder module list; the resident host knows the real one.
fn placeholder_modules() -> [u8; 32] {
    let modules = [
        ("VRChat.exe", None),
        ("GameAssembly.dll", None),
        ("UnityPlayer.dll", None),
        ("EOSSDK-Win64-Shipping.dll", None),
    ];
    module_list_hash(modules)
}

/// State owned by one feature generation.
pub struct EacFeature {
    enabled: bool,
    auto: bool,
    autosession: bool,
    session: EacSession,
    log: Option<JsonlLog>,
    tailer: Option<Tailer>,
    mac_key: [u8; 32],
    integrity: [u8; 32],
    modules: [u8; 32],
    last_game_begin_epoch: u64,
    last_error: Option<String>,
    hooks: Hooks,
    hooks_enabled: bool,
    /// Incremental reader over the resident's outbound capture file.
    tap: Option<CaptureTap>,
    /// Captured frames whose header checksum reproduced.
    tap_crc_ok: u64,
    /// Captured frames whose header checksum did not.
    tap_crc_bad: u64,
}

impl Default for EacFeature {
    fn default() -> Self {
        Self::new()
    }
}

impl EacFeature {
    /// Enabled by default like `m3`/`telemetry`, but idle until `eac.log` /
    /// `eac.begin`: nothing is written and no epoch exists.
    pub fn new() -> Self {
        Self {
            enabled: true,
            auto: true,
            autosession: false,
            session: EacSession::default(),
            log: None,
            tailer: None,
            mac_key: mac_key(),
            integrity: placeholder_integrity(),
            modules: placeholder_modules(),
            last_game_begin_epoch: 0,
            last_error: None,
            hooks: Hooks::new(),
            hooks_enabled: true,
            tap: None,
            tap_crc_ok: 0,
            tap_crc_bad: 0,
        }
    }

    /// Family switch (`configure eac=0|1`).
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// `configure eac=0|1`.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// `configure eac_auto=0|1` — heartbeat from the managed tick.
    pub fn set_auto(&mut self, auto: bool) {
        self.auto = auto;
    }

    /// `configure eac_autosession=0|1` — let `[AntiCheatClient]` log lines open
    /// and close epochs until the EOSSDK hooks exist.
    pub fn set_autosession(&mut self, autosession: bool) {
        self.autosession = autosession;
    }

    /// `configure eac_hooks=0|1` — enable/disable lazy EOSSDK hook install.
    pub fn set_hooks_enabled(&mut self, enabled: bool) {
        self.hooks_enabled = enabled;
        self.hooks.set_enabled(enabled);
    }

    /// Apply hook observations to the epoch model and the JSONL log.
    ///
    /// Public so the host-testable feature tests can feed synthetic events
    /// without a Windows process. Console lines are appended for epoch changes
    /// only; per-message observations go to the JSONL log.
    pub fn apply_hook_events(&mut self, events: Vec<HookEvent>, lines: &mut Vec<String>) {
        for event in events {
            match event {
                HookEvent::SessionBegin { mode, ts_ms } => {
                    if self.session.running()
                        && let Some(end) = self.session.end(ts_ms)
                    {
                        let _ = self.emit(
                            Event::at(end.timestamp_ms, "session_end")
                                .num("epoch", end.epoch)
                                .num("heartbeats", u64::from(end.heartbeats))
                                .text("reason", "hook reopen"),
                        );
                    }
                    let begin = self.session.begin(ts_ms, None);
                    lines.push(format!(
                        "eac: BeginSession epoch={} mode={mode} (hook)",
                        begin.epoch
                    ));
                    let _ = self.emit(
                        Event::at(begin.timestamp_ms, "session_begin")
                            .num("epoch", begin.epoch)
                            .text("sid", format!("0x{:016x}", begin.session_id))
                            .num("mode", u64::try_from(mode).unwrap_or(0))
                            .text("source", "hook"),
                    );
                }
                HookEvent::SessionEnd { ts_ms } => {
                    if let Some(end) = self.session.end(ts_ms) {
                        lines.push(format!(
                            "eac: EndSession epoch={} heartbeats={} (hook)",
                            end.epoch, end.heartbeats
                        ));
                        let _ = self.emit(
                            Event::at(end.timestamp_ms, "session_end")
                                .num("epoch", end.epoch)
                                .num("heartbeats", u64::from(end.heartbeats))
                                .num("total_heartbeats", end.total_heartbeats)
                                .text("reason", "hook"),
                        );
                    }
                }
                HookEvent::OutMessage { len, dt_ms, ts_ms } => {
                    let _ = self.emit(
                        Event::at(ts_ms, "ac_out")
                            .num("epoch", self.session.epoch())
                            .num("len", u64::from(len))
                            .num("dt_ms", u64::from(dt_ms)),
                    );
                }
                HookEvent::InMessage { len, ts_ms } => {
                    let _ = self.emit(
                        Event::at(ts_ms, "ac_in")
                            .num("epoch", self.session.epoch())
                            .num("len", u64::from(len)),
                    );
                }
                HookEvent::Protect { in_len, out_len } => {
                    if in_len != 0 && out_len >= in_len {
                        let _ = self.emit(
                            Event::at(now(), "ac_protect")
                                .num("in_len", u64::from(in_len))
                                .num("out_len", u64::from(out_len))
                                .num("overhead", u64::from(out_len - in_len)),
                        );
                    }
                }
            }
        }
    }

    fn hooks_command(&mut self, arg: &str) -> (Vec<String>, i32) {
        let mut parts = arg.split_whitespace();
        match parts.next().unwrap_or("") {
            "" | "status" => {
                let mut lines = self.hooks.status_lines();
                lines.push(format!(
                    "eac.hooks: enabled={} installs_on_tick={}",
                    self.hooks_enabled, self.hooks_enabled
                ));
                let counters = self.hooks.counters();
                if counters.out_messages == 0 && counters.in_messages > 0 {
                    // Static counters are per image: the inbound hooks landed
                    // here, the outbound wrapper stayed with the resident.
                    lines.push(format!(
                        "eac.hooks: out=0 while in={} — outbound counters live in the resident; \
                         read the frames with eac.tap",
                        counters.in_messages
                    ));
                }
                (lines, STATUS_OK)
            }
            "install" => {
                self.hooks_enabled = true;
                self.hooks.set_enabled(true);
                let line = self.hooks.install_now();
                let mut lines = vec![line];
                lines.extend(self.hooks.status_lines());
                (lines, STATUS_OK)
            }
            "capture" => {
                let path = parts.collect::<Vec<_>>().join(" ");
                if path.is_empty() {
                    return (
                        vec![format!(
                            "eac.hooks capture: {}",
                            self.hooks
                                .capture_path()
                                .map(|path| path.display().to_string())
                                .unwrap_or_else(|| "-".to_string())
                        )],
                        STATUS_OK,
                    );
                }
                match self.hooks.set_capture_path(std::path::Path::new(&path)) {
                    Ok(path) => (
                        vec![format!("eac.hooks capture: {}", path.display())],
                        STATUS_OK,
                    ),
                    Err(error) => (
                        vec![format!("eac.hooks capture: {error}")],
                        STATUS_NOT_READY,
                    ),
                }
            }
            "on" | "off" => {
                let enabled = arg.trim() == "on";
                self.set_hooks_enabled(enabled);
                (vec![format!("eac.hooks: enabled={enabled}")], STATUS_OK)
            }
            other => (
                vec![format!(
                    "eac.hooks: unknown argument '{other}' (status|install|capture <path>|on|off)"
                )],
                STATUS_BAD_ARGUMENT,
            ),
        }
    }

    /// `configure eac_interval_ms=<200..60000>`, clamped.
    pub fn set_interval_ms(&mut self, interval_ms: u64) -> u64 {
        self.session.set_interval_ms(interval_ms);
        self.session.interval_ms()
    }

    /// Attach (or replace) the capture tap. Returns the path it opened.
    ///
    /// `skip_to_end` is the live-attach behaviour: only frames written from now
    /// on are reported, so attaching does not dump a whole session into the
    /// JSONL at once. `false` replays the file from its start.
    pub fn set_tap(&mut self, path: &std::path::Path, skip_to_end: bool) -> Result<PathBuf, String> {
        if path.as_os_str().is_empty() {
            return Err("empty capture path".into());
        }
        let tap = CaptureTap::new(path);
        let tap = if skip_to_end { tap.skip_to_end() } else { tap };
        self.tap_crc_ok = 0;
        self.tap_crc_bad = 0;
        self.tap = Some(tap);
        Ok(path.to_path_buf())
    }

    /// Detach the tap (synthetic beats may resume).
    pub fn clear_tap(&mut self) {
        self.tap = None;
    }

    fn tap_status(&self) -> String {
        match self.tap.as_ref() {
            Some(tap) => format!(
                "eac.tap: {} offset={} frames={} pending={}B crc_ok={} crc_bad={}",
                tap.path().display(),
                tap.offset(),
                tap.frames(),
                tap.pending_len(),
                self.tap_crc_ok,
                self.tap_crc_bad
            ),
            None => format!(
                "eac.tap: detached (default {}); attach with 'eac.tap' or 'eac.tap <path>'",
                default_capture_path().display()
            ),
        }
    }

    fn tap_command(&mut self, arg: &str) -> (Vec<String>, i32) {
        let arg = arg.trim();
        if arg == "off" {
            self.clear_tap();
            return (
                vec!["eac.tap: detached; synthetic beats may resume".to_string()],
                STATUS_OK,
            );
        }
        if arg.is_empty() && self.tap.is_some() {
            return (vec![self.tap_status()], STATUS_OK);
        }
        // `from <path>` replays the file; a bare path and the default start at
        // the next frame written.
        let (path, replay) = if arg.is_empty() {
            (default_capture_path(), false)
        } else if let Some(rest) = arg.strip_prefix("from ") {
            (PathBuf::from(rest.trim()), true)
        } else {
            (PathBuf::from(arg), false)
        };
        match self.set_tap(&path, !replay) {
            Ok(path) => (
                vec![format!(
                    "eac.tap: {} ({})",
                    path.display(),
                    if replay {
                        "replaying from the start"
                    } else {
                        "from the next frame"
                    }
                )],
                STATUS_OK,
            ),
            Err(error) => (vec![format!("eac.tap: {error}")], STATUS_BAD_ARGUMENT),
        }
    }

    /// Feed the epoch model with every frame the resident wrote since the last
    /// poll. Heartbeats become `heartbeat` records carrying the **wire** values
    /// (real sid/seq, transport `dt`, 512-byte size, verified CRC); a frame that
    /// starts a logical message also produces a `report` record with the
    /// message's `total_len`/`action`.
    fn poll_tap(&mut self, lines: &mut Vec<String>) {
        let Some(mut tap) = self.tap.take() else {
            return;
        };
        let result = tap.poll();
        self.tap = Some(tap);
        let frames = match result {
            Ok(frames) => frames,
            Err(error) => {
                let message = format!("eac.tap: {error}");
                if self.last_error.as_deref() != Some(message.as_str()) {
                    lines.push(message.clone());
                    self.last_error = Some(message);
                }
                return;
            }
        };
        for frame in frames {
            let len = frame.data.len();
            if len < HEADER_LEN {
                continue;
            }
            let parsed = match WireFrame::parse(&frame.data, CHECKSUM) {
                Ok(parsed) => parsed,
                Err(_) => {
                    self.tap_crc_bad = self.tap_crc_bad.saturating_add(1);
                    continue;
                }
            };
            self.tap_crc_ok = self.tap_crc_ok.saturating_add(1);
            if len == WIRE_HEARTBEAT_LEN {
                let beat = self.session.record_observed(
                    now(),
                    parsed.header.session_id,
                    parsed.header.sequence,
                    u64::from(frame.dt_ms),
                    len as u32,
                );
                let _ = self.emit(
                    Event::at(beat.timestamp_ms, "heartbeat")
                        .num("epoch", beat.epoch)
                        .text("sid", format!("0x{:016x}", beat.session_id))
                        .num("seq", beat.seq)
                        .num("hb_in_epoch", u64::from(beat.hb_in_epoch))
                        .opt_num("dt_ms", beat.dt_ms)
                        .num("frame_len", u64::from(beat.frame_len))
                        .num("flags", u64::from(parsed.header.flags))
                        .text("source", "wire"),
                );
                if beat.gap
                    && let Some(dt_ms) = beat.dt_ms
                {
                    let _ = self.emit(
                        Event::at(beat.timestamp_ms, "gap")
                            .num("epoch", beat.epoch)
                            .num("dt_ms", dt_ms)
                            .num("expected_ms", self.session.interval_ms())
                            .num("hb_in_epoch", u64::from(beat.hb_in_epoch))
                            .num("seq", beat.seq),
                    );
                }
            } else if parsed.header.flags & 1 != 0 && parsed.payload.len() >= 8 {
                // StartData: this body begins a logical report message.
                let total_len = u32::from_le_bytes(
                    parsed.payload[0..4].try_into().unwrap_or_default(),
                );
                let action = u32::from_le_bytes(
                    parsed.payload[4..8].try_into().unwrap_or_default(),
                );
                let _ = self.emit(
                    Event::at(now(), "report")
                        .num("epoch", self.session.epoch())
                        .num("frame_len", len as u64)
                        .num("total_len", u64::from(total_len))
                        .num("action", u64::from(action))
                        .num("flags", u64::from(parsed.header.flags)),
                );
            }
        }
    }

    /// Attach (or replace) the JSONL output path.
    pub fn set_log(&mut self, path: &str) -> Result<PathBuf, String> {
        let log = JsonlLog::open(path).map_err(|error| format!("{path}: {error}"))?;
        let resolved = log.path().to_path_buf();
        self.log = Some(log);
        Ok(resolved)
    }

    /// Attach (or replace) the VRChat `output_log` path.
    pub fn set_tail(&mut self, path: &str) -> Result<PathBuf, String> {
        // Open eagerly so a wrong path fails at configuration, not silently on
        // every tick; then start a fresh tailer positioned at end-of-file.
        Tailer::open(path).map_err(|error| format!("{path}: {error}"))?;
        self.tailer = Some(Tailer::new(path));
        Ok(PathBuf::from(path))
    }

    /// One managed tick: hook housekeeping, then game lines, then heartbeat.
    ///
    /// The hooks are installed lazily here (managed tick, no new thread, L3):
    /// until `EOSSDK-Win64-Shipping.dll` is mapped the probe costs one
    /// `GetModuleHandleA` every two seconds.
    ///
    /// Returns console lines (only on a state change or a fresh error), so a
    /// 250 ms tick does not flood the log.
    pub fn tick(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        if !self.enabled {
            return lines;
        }
        if self.hooks_enabled {
            if let Some(line) = self.hooks.poll(now()) {
                lines.push(line);
            }
            let events = self.hooks.drain_events();
            if !events.is_empty() {
                self.apply_hook_events(events, &mut lines);
            }
        }
        self.poll_tail(&mut lines);
        self.poll_tap(&mut lines);
        // A tap reports what really went out; synthesizing beats next to it
        // would make the JSONL lie about the wire.
        if self.tap.is_none() && self.auto && self.session.running() && self.session.due(now()) {
            match self.beat(now()) {
                // A successful beat clears the "same error once" latch, so a
                // recurring failure is reported again after a recovery.
                Ok(_) => self.last_error = None,
                Err(error) => {
                    if self.last_error.as_deref() != Some(error.as_str()) {
                        lines.push(format!("eac: {error}"));
                        self.last_error = Some(error);
                    }
                }
            }
        }
        lines
    }

    /// Console entry point for `eac.*`.
    pub fn dispatch(&mut self, command: &str, arg: &str) -> (Vec<String>, i32) {
        match command {
            "eac" | "eac.help" => (self.help(), STATUS_OK),
            "eac.status" => (self.status_lines(), STATUS_OK),
            "eac.begin" => self.begin_command(arg),
            "eac.tick" => self.tick_command(),
            "eac.end" => self.end_command(arg),
            "eac.hooks" => self.hooks_command(arg),
            "eac.tap" => self.tap_command(arg),
            // Read-only: locate the in-process ELF image of the anti-cheat
            // client module (it is not a PE, so no module list can show it).
            "eac.module" => (hromatite_eac::eos::module_report(arg), STATUS_OK),
            "eac.log" => self.log_command(arg),
            "eac.tail" => self.tail_command(arg),
            other => (
                vec![format!("unknown eac command: {other} (try eac.help)")],
                STATUS_BAD_ARGUMENT,
            ),
        }
    }

    fn help(&self) -> Vec<String> {
        // Generated from the shared command table: every `eac.*` entry is listed
        // once, with the same wording the host palette shows.
        hromatite_commands::SPECS
            .iter()
            .filter(|spec| spec.name.starts_with("eac."))
            .map(|spec| spec.help_line())
            .collect()
    }

    fn status_lines(&self) -> Vec<String> {
        let status = self.session.status();
        let mut lines = vec![format!(
            "eac.status: epoch={} sid=0x{:016x} running={} seq={} hb_in_epoch={} \
             total={} observed={} interval_ms={} dt_ms={} gaps={} frame_len={}",
            status.epoch,
            status.session_id,
            status.running,
            status.seq,
            status.hb_in_epoch,
            status.total_heartbeats,
            status.observed,
            status.interval_ms,
            status
                .last_dt_ms
                .map(|dt| dt.to_string())
                .unwrap_or_else(|| "-".to_string()),
            status.gaps,
            status.frame_len
        )];
        lines.push(format!(
            "eac.status: source={} wire_len={} tap_crc={}/{} log={} tail={} tail_lines={} \
             checksum={} auto={} autosession={}",
            if self.tap.is_some() { "wire" } else { "model" },
            WIRE_HEARTBEAT_LEN,
            self.tap_crc_ok,
            self.tap_crc_ok.saturating_add(self.tap_crc_bad),
            self.log
                .as_ref()
                .map(|log| log.path().display().to_string())
                .unwrap_or_else(|| "-".to_string()),
            self.tailer
                .as_ref()
                .map(|tailer| tailer.path().display().to_string())
                .unwrap_or_else(|| "-".to_string()),
            self.tailer
                .as_ref()
                .map(|tailer| tailer.lines_seen().to_string())
                .unwrap_or_else(|| "0".to_string()),
            CHECKSUM.name(),
            self.auto as u8,
            self.autosession as u8,
        ));
        lines.push(self.tap_status());
        if let Some(line) = self.hooks.status_lines().into_iter().nth(1) {
            lines.push(line);
        }
        lines
    }

    fn begin_command(&mut self, arg: &str) -> (Vec<String>, i32) {
        let sid = if arg.is_empty() {
            None
        } else {
            Some(parse_id(arg))
        };
        let begin = self.session.begin(now(), sid);
        let mut lines = vec![format!(
            "eac.begin: epoch={} sid=0x{:016x}",
            begin.epoch, begin.session_id
        )];
        let event = Event::at(begin.timestamp_ms, "session_begin")
            .num("epoch", begin.epoch)
            .text("sid", format!("0x{:016x}", begin.session_id))
            .num("interval_ms", self.session.interval_ms());
        match self.emit(event) {
            Ok(()) => lines.push(format!("eac.begin: log={}", self.log_path())),
            Err(error) => lines.push(format!("eac.begin: {error}")),
        }
        (lines, STATUS_OK)
    }

    fn tick_command(&mut self) -> (Vec<String>, i32) {
        if !self.session.running() {
            return (
                vec!["eac.tick: no active epoch (try eac.begin)".to_string()],
                STATUS_NOT_READY,
            );
        }
        match self.beat(now()) {
            Ok(beat) => (
                vec![format!(
                    "eac.tick: epoch={} seq={} hb_in_epoch={} dt_ms={} gap={} \
                     frame_len={FRAME_LEN} (model; the wire heartbeat is {WIRE_HEARTBEAT_LEN})",
                    beat.epoch,
                    beat.seq,
                    beat.hb_in_epoch,
                    beat.dt_ms
                        .map(|dt| dt.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    beat.gap,
                )],
                STATUS_OK,
            ),
            Err(error) => (vec![format!("eac.tick: {error}")], STATUS_ERROR),
        }
    }

    fn end_command(&mut self, arg: &str) -> (Vec<String>, i32) {
        let reason = if arg.is_empty() { "close()" } else { arg };
        let Some(end) = self.session.end(now()) else {
            return (
                vec!["eac.end: no active epoch".to_string()],
                STATUS_NOT_READY,
            );
        };
        let event = Event::at(end.timestamp_ms, "session_end")
            .num("epoch", end.epoch)
            .num("heartbeats", u64::from(end.heartbeats))
            .num("total_heartbeats", end.total_heartbeats)
            .text("reason", reason);
        let mut lines = vec![format!(
            "eac.end: epoch={} heartbeats={} total={} reason={reason}",
            end.epoch, end.heartbeats, end.total_heartbeats
        )];
        if let Err(error) = self.emit(event) {
            lines.push(format!("eac.end: {error}"));
        }
        (lines, STATUS_OK)
    }

    fn log_command(&mut self, arg: &str) -> (Vec<String>, i32) {
        if arg.is_empty() {
            return (vec![format!("eac.log: {}", self.log_path())], STATUS_OK);
        }
        match self.set_log(arg) {
            Ok(path) => (vec![format!("eac.log: {}", path.display())], STATUS_OK),
            Err(error) => (vec![format!("eac.log: {error}")], STATUS_BAD_ARGUMENT),
        }
    }

    fn tail_command(&mut self, arg: &str) -> (Vec<String>, i32) {
        if arg.is_empty() {
            return (vec![format!("eac.tail: {}", self.tail_path())], STATUS_OK);
        }
        match self.set_tail(arg) {
            Ok(path) => (
                vec![format!("eac.tail: {} (appends only)", path.display())],
                STATUS_OK,
            ),
            Err(error) => (vec![format!("eac.tail: {error}")], STATUS_BAD_ARGUMENT),
        }
    }

    /// Heartbeat at `timestamp_ms`: update the model, encode the frame, write
    /// `heartbeat` (and `gap`) records.
    fn beat(&mut self, timestamp_ms: u64) -> Result<hromatite_eac::Beat, String> {
        let beat = self.session.beat(timestamp_ms);
        let (frame, _) = encode_beat(
            &beat,
            0,
            self.integrity,
            self.modules,
            &self.mac_key,
            CHECKSUM,
        );
        let sid = format!("0x{:016x}", beat.session_id);
        self.emit(
            Event::at(beat.timestamp_ms, "heartbeat")
                .num("epoch", beat.epoch)
                .text("sid", sid.clone())
                .num("seq", beat.seq)
                .num("hb_in_epoch", u64::from(beat.hb_in_epoch))
                .opt_num("dt_ms", beat.dt_ms)
                .num("frame_len", frame.len() as u64)
                .text("source", "model"),
        )?;
        if beat.gap
            && let Some(dt_ms) = beat.dt_ms
        {
            self.emit(
                Event::at(beat.timestamp_ms, "gap")
                    .num("epoch", beat.epoch)
                    .num("dt_ms", dt_ms)
                    .num("expected_ms", self.session.interval_ms())
                    .num("hb_in_epoch", u64::from(beat.hb_in_epoch))
                    .num("seq", beat.seq),
            )?;
        }
        Ok(beat)
    }

    /// Mirror new marker lines from the game log; optionally drive epochs.
    fn poll_tail(&mut self, lines: &mut Vec<String>) {
        let Some(tailer) = self.tailer.as_mut() else {
            return;
        };
        let fresh = match tailer.poll() {
            Ok(fresh) => fresh,
            Err(error) => {
                let message = format!("eac.tail: {error}");
                if self.last_error.as_deref() != Some(message.as_str()) {
                    lines.push(message.clone());
                    self.last_error = Some(message);
                }
                return;
            }
        };
        if fresh.is_empty() {
            return;
        }
        let epoch = self.session.epoch();
        let now = now();
        for line in fresh {
            let _ = self.emit(
                Event::at(now, "game")
                    .num("epoch", epoch)
                    .text("line", line.clone()),
            );
            // The hooks own the epoch when they are installed; the log lines
            // are only a fallback for builds without EOSSDK.
            if !self.autosession || self.hooks.installed() {
                continue;
            }
            if announces_begin(&line)
                && (!self.session.running() || epoch > self.last_game_begin_epoch)
            {
                let begin = self.session.begin(now, None);
                self.last_game_begin_epoch = begin.epoch;
                lines.push(format!(
                    "eac: game began epoch={} sid=0x{:016x}",
                    begin.epoch, begin.session_id
                ));
                let _ = self.emit(
                    Event::at(begin.timestamp_ms, "session_begin")
                        .num("epoch", begin.epoch)
                        .text("sid", format!("0x{:016x}", begin.session_id)),
                );
            } else if announces_end(&line)
                && let Some(end) = self.session.end(now)
            {
                lines.push(format!("eac: game ended epoch={}", end.epoch));
                let _ = self.emit(
                    Event::at(end.timestamp_ms, "session_end")
                        .num("epoch", end.epoch)
                        .num("heartbeats", u64::from(end.heartbeats))
                        .text("reason", "game log"),
                );
            }
        }
    }

    fn emit(&mut self, event: Event) -> Result<(), String> {
        match self.log.as_mut() {
            Some(log) => log.write(&event).map_err(|error| format!("log: {error}")),
            None => Err("no log path (use eac.log <path>)".to_string()),
        }
    }

    fn log_path(&self) -> String {
        self.log
            .as_ref()
            .map(|log| log.path().display().to_string())
            .unwrap_or_else(|| "-".to_string())
    }

    fn tail_path(&self) -> String {
        self.tailer
            .as_ref()
            .map(|tailer| tailer.path().display().to_string())
            .unwrap_or_else(|| "-".to_string())
    }
}

fn now() -> u64 {
    hromatite_eac::clock::now_ms()
}

/// The capture file the resident hooks write: `%TEMP%\hromatite_eac_capture.bin`.
fn default_capture_path() -> PathBuf {
    std::env::temp_dir().join(DEFAULT_CAPTURE_NAME)
}

/// Parse `0x…` hex or decimal; anything unparsable yields 0, which the session
/// model replaces with a derived id.
fn parse_id(text: &str) -> u64 {
    let text = text.trim();
    let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => text.parse::<u64>(),
    };
    parsed.unwrap_or(0)
}

/// Interval bounds exposed for `configure` validation messages.
pub const INTERVAL_RANGE: (u64, u64) = (MIN_INTERVAL_MS, MAX_INTERVAL_MS);
