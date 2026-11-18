//! EOSSDK anti-cheat export hooks: the Rust port of the working C shim.
//!
//! Twelve `EOS_AntiCheatClient_*` exports of `EOSSDK-Win64-Shipping.dll` get a
//! 12-byte inline patch (see [`crate::hook`]). The handlers are pure observers
//! except for the two places the shim also touched:
//!
//! * `AddNotifyMessageToServer` registers **our** callback with the SDK, so every
//!   client→server frame passes through [`message_to_server_wrapper`]: it is
//!   logged (size + first 64 bytes) and appended to a length-prefixed capture
//!   file, then forwarded to the game's callback — unless
//!   `EACSHIM_DROP_AFTER_MS` cut the heartbeat, or `EAC_REPLAY=1` asks us to
//!   feed the game the captured frames instead.
//! * `EAC_FORGE=1` ([`crate::forge`]) goes one step further than replay: the last
//!   live 512-byte frame of the session is learned as a template and, after
//!   `EAC_FORGE_AFTER_MS`, every live frame is replaced by a synthesized one
//!   (same `SessionId`/`Flags`/body, next `Sequence`, fresh CRC-32C) at the
//!   measured 2007 ms cadence, from a thread of our own.
//! * `EAC_REPLAY_REPORT=<path>` ([`crate::report`]) keeps that generator and the
//!   takeover, but each slot takes the *next recorded frame* of a whole 512-byte
//!   report instead of rebuilding one frame from the template: only
//!   `SessionId`, `Sequence` and the CRC are rewritten for the live session, so
//!   the recorded bodies (message starts, inner checksums, counters) stay
//!   intact.
//! * `ReceiveMessageFromServer` observes inbound frames and can drop them after
//!   the same deadline (`EACSHIM_DROP_IN=1`).
//!
//! `BeginSession` / `EndSession` open and close epochs (replacing the log-driven
//! `eac_autosession`), and `ProtectMessage` / `UnprotectMessage` /
//! `GetProtectMessageOutputLength` / `PollStatus` only count: their in/out
//! lengths prove the documented 22-byte NetProtect overhead live.
//!
//! Handlers run on whichever thread the SDK pumps on. They therefore keep no
//! references, use `try_lock` (a missing capture line is better than a dead
//! game), and `catch_unwind` every body before tail-calling the trampoline.
//! The metadata that the managed tick needs is pushed into a bounded event
//! queue; only [`Hooks::poll`] and [`Hooks::drain_events`] touch game state
//! from the feature thread.
//!
//! No external crates: the Win32 calls used here are declared below.

// The Win32 install path is compiled out on the host; keep its private helpers
// quiet there without hiding warnings on the real target.
#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

use std::collections::VecDeque;
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};

use crate::capture::{CaptureFile, Frame, hex_prefix, read_frames};
use crate::clock::now_ms;
use crate::forge::{self, BodyMode, Template};
use crate::report;

/// Mutable forge state that belongs to one learned frame.
#[derive(Clone)]
struct Learned {
    template: Template,
    /// `EAC_FORGE_BODY=bump`: rewrite the message counter per frame.
    bump: bool,
    /// Next value for the message counter at `msg+0x18`.
    counter: u32,
}
use crate::hook;

/// DLL that owns every AC export we hook.
pub const EOSSDK: &str = "EOSSDK-Win64-Shipping.dll";

/// Hook slots, in the shim's order.
const HK_BEGIN: usize = 0;
const HK_END: usize = 1;
const HK_RECEIVE: usize = 2;
const HK_PROTECT: usize = 3;
const HK_UNPROTECT: usize = 4;
const HK_OUTLEN: usize = 5;
const HK_ADD_NOTIFY: usize = 6;
const HK_POLL: usize = 7;
const HK_COUNT: usize = 8;

/// The exports actually hooked. The shim's other four only forward; they are
/// left untouched so the patch surface stays as small as the requirement.
const EXPORTS: [&str; HK_COUNT] = [
    "EOS_AntiCheatClient_BeginSession",
    "EOS_AntiCheatClient_EndSession",
    "EOS_AntiCheatClient_ReceiveMessageFromServer",
    "EOS_AntiCheatClient_ProtectMessage",
    "EOS_AntiCheatClient_UnprotectMessage",
    "EOS_AntiCheatClient_GetProtectMessageOutputLength",
    "EOS_AntiCheatClient_AddNotifyMessageToServer",
    "EOS_AntiCheatClient_PollStatus",
];

const MAX_BINDS: usize = 4;
const MAX_EVENTS: usize = 4096;
/// Log lines printed in full before switching to every-Nth sampling.
const CALLS_FULL: u64 = 200;
/// `EACSHIM_DROP_AFTER_MS` fallback when the variable is present but unparsable.
///
/// Zero means "no cut". A non-numeric value used to fall back to 20 s, which
/// silently armed the cut whenever a stale `EACSHIM_DROP_AFTER_MS=cmd` entry
/// from the C shim was left in `~/.hromatite-mode` (observed 2026-11-17: the
/// payload dropped every outbound frame 20 s into each session and the game
/// churned BeginSession/EndSession every 24-30 s).
const DEFAULT_DROP_MS: u64 = 0;

// ---------------------------------------------------------------- state

/// Trampoline per hook slot; written once at install, read by every handler.
static TRAMPS: [AtomicUsize; HK_COUNT] = [const { AtomicUsize::new(0) }; HK_COUNT];
/// Original export address recovered from a caller's IAT slot.
///
/// The IAT walk records what used to sit in the slot, so a handler still reaches
/// the real function when the inline patch of the export entry never landed (or
/// landed after the game had already cached the pointer).
static IAT_ORIG: [AtomicUsize; HK_COUNT] = [const { AtomicUsize::new(0) }; HK_COUNT];
/// Slots the IAT walk patched, kept for a repeated [`install_iat`] call.
static IAT_SLOTS: AtomicUsize = AtomicUsize::new(0);
/// Set once the module walk ran, so a second caller does not re-walk or re-log.
static IAT_TRIED: AtomicBool = AtomicBool::new(false);
/// Calls seen per hook slot.
static CALLS: [AtomicU64; HK_COUNT] = [const { AtomicU64::new(0) }; HK_COUNT];
static OUT_MESSAGES: AtomicU64 = AtomicU64::new(0);
static IN_MESSAGES: AtomicU64 = AtomicU64::new(0);
static FORWARDED: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);
static REPLAYED: AtomicU64 = AtomicU64::new(0);
static PROTECT_IN: AtomicU64 = AtomicU64::new(0);
static PROTECT_OUT: AtomicU64 = AtomicU64::new(0);
static PROTECT_SAMPLES: AtomicU64 = AtomicU64::new(0);
static PROTECT_22: AtomicU64 = AtomicU64::new(0);
static OVERHEAD_MIN: AtomicI32 = AtomicI32::new(i32::MAX);
static OVERHEAD_MAX: AtomicI32 = AtomicI32::new(0);
static LAST_OUT_MS: AtomicU64 = AtomicU64::new(0);
static SESSION_START_MS: AtomicU64 = AtomicU64::new(0);
static DROP_AFTER_MS: AtomicU64 = AtomicU64::new(0);
static DROP_IN: AtomicBool = AtomicBool::new(false);
static REPLAY: AtomicBool = AtomicBool::new(false);
static LAST_MESSAGE_LOG: AtomicU64 = AtomicU64::new(0);

// ------------------------------------------------------------------- forge
//
// `EAC_FORGE=1`: the configuration is copied into atomics at install time (a
// handler must never read the environment), the template is the last live
// 512-byte frame and is kept under its own mutex so a forge tick never waits
// behind the capture sink.
static FORGE_AFTER_MS: AtomicU64 = AtomicU64::new(0);
static FORGE_MIN_FRAMES: AtomicU64 = AtomicU64::new(0);
static FORGE_INTERVAL_MS: AtomicU64 = AtomicU64::new(forge::INTERVAL_MS);
static FORGE_BUMP: AtomicBool = AtomicBool::new(false);
/// `EAC_FORGE_KEEP_SMALL`: forward live frames that are not the 512-byte
/// heartbeat (ACKs, handshake fragments) instead of replacing them too.
static FORGE_KEEP_SMALL: AtomicBool = AtomicBool::new(false);
/// `EAC_FORGE_TEMPLATE=start`: keep only a message-start frame as template.
static FORGE_PREFER_START: AtomicBool = AtomicBool::new(false);
/// Set once the forge has replaced the live stream; from then on the wrapper
/// drops live frames and the generator thread owns the cadence.
static FORGE_STARTED: AtomicBool = AtomicBool::new(false);
/// Live 512-byte frames learned in this epoch (the readiness counter).
static FORGE_SEEN: AtomicU64 = AtomicU64::new(0);
/// Frames the generator handed to the game callback.
static FORGE_EMITTED: AtomicU64 = AtomicU64::new(0);
/// Next `Sequence` the generator will put in a frame.
static FORGE_NEXT_SEQ: AtomicU64 = AtomicU64::new(0);
/// Timestamp of the previous emission (for the capture file's `dt_ms`).
static FORGE_LAST_MS: AtomicU64 = AtomicU64::new(0);
/// Set once so a double install cannot start two generators.
static FORGE_THREAD: AtomicBool = AtomicBool::new(false);
/// The learned template *and* the body mode it must be emitted with. Both
/// live under the same lock because they belong together: the image that
/// learns the frame also decides how its body is refreshed, and the counter
/// must advance across emissions, not restart from the learned value.
static FORGE_TEMPLATE: Mutex<Option<Learned>> = Mutex::new(None);
static FORGE_CAPTURE: Mutex<Option<CaptureFile>> = Mutex::new(None);

// ------------------------------------------------------------------ report
//
// `EAC_REPLAY_REPORT=<path>`: the same generator thread, but the frame it emits
// comes from a recorded report instead of the template. The report and its
// cursor live under their own mutex, so a slow reader never blocks the SDK
// thread's template learning.
/// True when a recorded report is being replayed instead of building frames.
static REPORT_ENABLED: AtomicBool = AtomicBool::new(false);
/// `EAC_REPLAY_FROM_WRAPPER=1`: deliver from the SDK callback thread instead
/// of a generator thread (the latter deadlocks the game's message pump).
static REPORT_FROM_WRAPPER: AtomicBool = AtomicBool::new(false);
/// `EAC_REPLAY_SELF=1`: at takeover, replay the *current* session's own stream
/// from its last message start, taken out of the live capture file. This is the
/// ASLR hypothesis: if the 16-byte records hash memory addresses, only frames
/// of the same process can validate.
static REPORT_SELF: AtomicBool = AtomicBool::new(false);
/// Capture path of the running epoch, so `EAC_REPLAY_SELF` can read it back.
static CAPTURE_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);
/// In-memory copy of the live 512-byte frames of this epoch.
///
/// `EAC_REPLAY_SELF` cannot read the capture file back: the writer holds it open
/// under Wine and a second read only ever saw one frame (`replayself_0403`).
/// Keeping the last `SELF_RING_MAX` frames in memory costs ~250 KiB and is the
/// same data the stream needs.
static SELF_RING: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
/// Last time the "waiting for a complete message" line was printed.
///
/// [`forge_begin`] retries every 50 ms, so an unthrottled line would bury the
/// log while the module is still producing its first report.
static SELF_WAIT_MS: AtomicU64 = AtomicU64::new(0);
/// How many live frames the ring keeps (≈ 17 minutes at 2007 ms).
const SELF_RING_MAX: usize = 512;
/// `EAC_REPLAY_FLAGS=live`: `Flags` come from the live template, not the file.
static REPORT_FLAGS_LIVE: AtomicBool = AtomicBool::new(false);
/// The report being replayed, or `None` outside `EAC_REPLAY_REPORT` runs.
static REPORT_STREAM: Mutex<Option<report::Stream>> = Mutex::new(None);
/// Set once the report ran out (`EAC_REPLAY_REPORT_LOOP` off), so the
/// exhaustion line reaches the log exactly once per epoch.
static REPORT_STOPPED: AtomicBool = AtomicBool::new(false);

/// One `(ClientData, callback)` pair registered by the game.
struct Bind {
    client_data: AtomicUsize,
    function: AtomicUsize,
}

impl Bind {
    const fn new() -> Self {
        Self {
            client_data: AtomicUsize::new(0),
            function: AtomicUsize::new(0),
        }
    }
}

static BINDS: [Bind; MAX_BINDS] = [const { Bind::new() }; MAX_BINDS];
static BIND_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Capture + hook log, created at install time.
struct Sink {
    capture: Option<CaptureFile>,
    log: Option<LogFile>,
    replay: Vec<Frame>,
    replay_cursor: usize,
    replay_base_ms: u64,
    replay_due_ms: u64,
}

impl Sink {
    fn log_line(&mut self, line: &str) {
        if let Some(log) = self.log.as_mut() {
            log.write_line(line);
        }
    }
}

static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// Serializes `reset_state` with the tests that depend on the global counters:
/// the hook state is process-global and the harness runs tests in parallel.
static STATE_LOCK: Mutex<()> = Mutex::new(());

/// Bounded observation queue drained by the managed tick.
static EVENTS: Mutex<VecDeque<HookEvent>> = Mutex::new(VecDeque::new());

/// Hook log with 32 MiB rotation (`*.1` archive).
struct LogFile {
    path: PathBuf,
    file: File,
    bytes: u64,
}

impl LogFile {
    fn open(path: PathBuf) -> Option<Self> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        let bytes = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Some(Self { path, file, bytes })
    }

    fn write_line(&mut self, line: &str) {
        let record = format!("[{:>10}] {line}\r\n", now_ms() % 100_000_000);
        if self.bytes + record.len() as u64 > crate::capture::DEFAULT_MAX_BYTES {
            let archive = self.path.with_extension("log.1");
            if std::fs::rename(&self.path, &archive).is_ok()
                && let Ok(file) = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)
            {
                self.file = file;
                self.bytes = 0;
            }
        }
        if self.file.write_all(record.as_bytes()).is_ok() {
            self.bytes += record.len() as u64;
        }
    }
}

/// Log a line from a handler without ever blocking the SDK thread.
pub(crate) fn hook_log(line: impl AsRef<str>) {
    if let Ok(mut sink) = SINK.try_lock()
        && let Some(sink) = sink.as_mut()
    {
        sink.log_line(line.as_ref());
    }
}

/// Log a line from the feature thread (blocking, but only at install/status).
fn hook_log_blocking(line: &str) {
    if let Ok(mut sink) = SINK.lock()
        && let Some(sink) = sink.as_mut()
    {
        sink.log_line(line);
    }
}

fn push_event(event: HookEvent) {
    if let Ok(mut queue) = EVENTS.try_lock()
        && queue.len() < MAX_EVENTS
    {
        queue.push_back(event);
    }
}

/// Observation pushed at the hook site and consumed by the managed tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    /// `EOS_AntiCheatClient_BeginSession` returned.
    SessionBegin {
        /// `Mode` from the options struct (`-1` when unreadable).
        mode: i32,
        /// Timestamp (ms since the Unix epoch).
        ts_ms: u64,
    },
    /// `EOS_AntiCheatClient_EndSession` returned.
    SessionEnd {
        /// Timestamp (ms since the Unix epoch).
        ts_ms: u64,
    },
    /// One client→server message reached the wrapper.
    OutMessage {
        /// Frame length in bytes.
        len: u32,
        /// Milliseconds since the previous captured frame.
        dt_ms: u32,
        /// Timestamp (ms since the Unix epoch).
        ts_ms: u64,
    },
    /// One server→client message reached the SDK entry.
    InMessage {
        /// Frame length in bytes.
        len: u32,
        /// Timestamp (ms since the Unix epoch).
        ts_ms: u64,
    },
    /// `ProtectMessage`/`GetProtectMessageOutputLength` produced a length pair.
    Protect {
        /// Input length.
        in_len: u32,
        /// Output length reported by the SDK.
        out_len: u32,
    },
}

/// Counter snapshot for `eac.status` / `eac.hooks`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counters {
    /// Frames handed to the wrapper.
    pub out_messages: u64,
    /// Frames seen at `ReceiveMessageFromServer`.
    pub in_messages: u64,
    /// Frames delivered to the game callback.
    pub forwarded: u64,
    /// Frames silently dropped after the cut deadline.
    pub dropped: u64,
    /// Frames injected from a capture file during replay.
    pub replayed: u64,
    /// Synthesized heartbeat frames handed to the game callback (`EAC_FORGE`).
    pub forged: u64,
    /// `ProtectMessage` calls.
    pub protect: u64,
    /// Sum of `ProtectMessage` input lengths.
    pub protect_in: u64,
    /// Sum of `ProtectMessage` output lengths.
    pub protect_out: u64,
    /// Samples where `out - in == 22`.
    pub overhead_22: u64,
    /// Length-pair samples taken.
    pub samples: u64,
    /// Smallest observed overhead.
    pub overhead_min: i32,
    /// Largest observed overhead.
    pub overhead_max: i32,
    /// Calls per hook slot (same order as [`EXPORTS`]).
    pub calls: [u64; HK_COUNT],
}

/// Per-export install result, kept for `eac.hooks`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// Export name.
    pub name: &'static str,
    /// True when the entry now jumps to our handler (fresh or reused).
    pub ok: bool,
    /// 1 fresh, 2 reused from the shared table, 3 foreign patch.
    pub mode: u8,
    /// Failure reason or relocation note.
    pub detail: String,
    /// Trampoline address, if any.
    pub tramp: usize,
}

/// Owned hook lifecycle. One instance per feature generation.
pub struct Hooks {
    enabled: bool,
    installed: bool,
    reports: Vec<Report>,
    absent_logged: bool,
}

impl Default for Hooks {
    fn default() -> Self {
        Self::new()
    }
}

impl Hooks {
    /// Idle: nothing is probed until the managed tick runs.
    pub fn new() -> Self {
        Self {
            enabled: true,
            installed: false,
            reports: Vec::new(),
            absent_logged: false,
        }
    }

    /// `configure eac_hooks=0|1`.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// True once the exports are patched in this process.
    pub fn installed(&self) -> bool {
        self.installed
    }

    /// Per-export results (empty before the first successful probe).
    pub fn reports(&self) -> &[Report] {
        &self.reports
    }

    /// Lazy, idempotent install. Called from every managed tick until it lands:
    /// `GetModuleHandleA` is cheap and `AddNotifyMessageToServer` is registered
    /// only a few hundred milliseconds after EOSSDK loads, so a coarse probe
    /// would miss the game's callback. Never panics when EOSSDK is absent.
    ///
    /// Returns a console line the first time state changes (installed, or first
    /// "module absent" observation), `None` otherwise.
    pub fn poll(&mut self, _now: u64) -> Option<String> {
        if !self.enabled || self.installed {
            return None;
        }
        match self.install() {
            Ok(reports) => {
                let ok = reports.iter().filter(|report| report.ok).count();
                self.installed = ok > 0;
                for report in &reports {
                    hook_log_blocking(&format!(
                        "[hook] {} ok={} mode={} tramp={:#x} detail={}",
                        report.name, report.ok, report.mode, report.tramp, report.detail
                    ));
                }
                let line = format!("eac: hooks installed {ok}/{} on {EOSSDK}", EXPORTS.len());
                hook_log_blocking(&line);
                self.reports = reports;
                Some(line)
            }
            Err(reason) => {
                if !self.absent_logged {
                    self.absent_logged = true;
                    hook_log_blocking(&format!("[hook] not installed: {reason}"));
                    return Some(format!("eac: hooks pending ({reason})"));
                }
                None
            }
        }
    }

    /// Attempt installation now (also used by `eac.hooks install`).
    ///
    /// Returns a short status line and, on success, fills [`Self::reports`].
    pub fn install_now(&mut self) -> String {
        match self.install() {
            Ok(reports) => {
                let ok = reports.iter().filter(|report| report.ok).count();
                self.installed = ok > 0;
                self.reports = reports;
                let line = format!("eac: hooks installed {ok}/{}", EXPORTS.len());
                hook_log_blocking(&line);
                line
            }
            Err(reason) => {
                self.installed = false;
                format!("eac: hooks not installed ({reason})")
            }
        }
    }

    /// Counter snapshot for status lines.
    pub fn counters(&self) -> Counters {
        Counters {
            out_messages: OUT_MESSAGES.load(Ordering::Relaxed),
            in_messages: IN_MESSAGES.load(Ordering::Relaxed),
            forwarded: FORWARDED.load(Ordering::Relaxed),
            dropped: DROPPED.load(Ordering::Relaxed),
            replayed: REPLAYED.load(Ordering::Relaxed),
            forged: FORGE_EMITTED.load(Ordering::Relaxed),
            protect: CALLS[HK_PROTECT].load(Ordering::Relaxed),
            protect_in: PROTECT_IN.load(Ordering::Relaxed),
            protect_out: PROTECT_OUT.load(Ordering::Relaxed),
            overhead_22: PROTECT_22.load(Ordering::Relaxed),
            samples: PROTECT_SAMPLES.load(Ordering::Relaxed),
            overhead_min: OVERHEAD_MIN.load(Ordering::Relaxed),
            overhead_max: OVERHEAD_MAX.load(Ordering::Relaxed),
            calls: core::array::from_fn(|index| CALLS[index].load(Ordering::Relaxed)),
        }
    }

    /// Retarget the capture file (`eac.hooks capture <path>`).
    ///
    /// Before install this only records `HROMATITE_EAC_CAPTURE`, which the
    /// install path reads; after install the sink is reopened immediately.
    pub fn set_capture_path(&self, path: &std::path::Path) -> Result<PathBuf, String> {
        // SAFETY: called from the serialized managed tick; no other thread reads
        // the environment concurrently in this process.
        unsafe { std::env::set_var("HROMATITE_EAC_CAPTURE", path) };
        if self.installed {
            let capture =
                CaptureFile::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
            let mut slot = SINK
                .lock()
                .map_err(|_| "capture lock poisoned".to_string())?;
            if let Some(sink) = slot.as_mut() {
                sink.capture = Some(capture);
            }
        }
        Ok(path.to_path_buf())
    }

    /// Path of the capture file, when a sink exists.
    pub fn capture_path(&self) -> Option<PathBuf> {
        SINK.lock().ok().and_then(|sink| {
            sink.as_ref().and_then(|sink| {
                sink.capture
                    .as_ref()
                    .map(|capture| capture.path().to_path_buf())
            })
        })
    }

    /// Queue an observation exactly as a handler would push it.
    ///
    /// Used by the feature tests and by a dry run without a Windows process;
    /// the queue is process-global, so any `Hooks` instance can inject.
    pub fn observe(event: HookEvent) {
        push_event(event);
    }

    /// Take every observation accumulated since the last call.
    pub fn drain_events(&self) -> Vec<HookEvent> {
        EVENTS
            .lock()
            .map(|mut queue| queue.drain(..).collect())
            .unwrap_or_default()
    }

    /// Human-readable install summary for `eac.hooks`.
    pub fn status_lines(&self) -> Vec<String> {
        let counters = self.counters();
        let mode = if FORGE_AFTER_MS.load(Ordering::Relaxed) != 0 {
            "forge"
        } else if REPLAY.load(Ordering::Relaxed) {
            "replay"
        } else if DROP_AFTER_MS.load(Ordering::Relaxed) != 0 {
            "drop"
        } else {
            "observe"
        };
        let mut lines = vec![format!(
            "eac.hooks: installed={} enabled={} mode={mode} exports={}/{} capture={}",
            self.installed,
            self.enabled,
            self.reports.iter().filter(|report| report.ok).count(),
            EXPORTS.len(),
            self.capture_path()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "-".to_string()),
        )];
        // `EAC_IAT=1` evidence: which caller slots hold a detour and what they
        // held before, so a live run can tell an IAT hit from an export hit.
        lines.push(hromatite_core::iat::status());
        lines.push(format!(
            "eac.hooks: out={} in={} forwarded={} dropped={} replayed={} forged={} \
             seen512={} forge_seq={} begins={} ends={} addnotify={} protect={} \
             overhead22={}/{} min={} max={}",
            counters.out_messages,
            counters.in_messages,
            counters.forwarded,
            counters.dropped,
            counters.replayed,
            counters.forged,
            FORGE_SEEN.load(Ordering::Relaxed),
            FORGE_NEXT_SEQ.load(Ordering::Relaxed),
            counters.calls[HK_BEGIN],
            counters.calls[HK_END],
            counters.calls[HK_ADD_NOTIFY],
            counters.protect,
            counters.overhead_22,
            counters.samples,
            counters.overhead_min,
            counters.overhead_max,
        ));
        for report in &self.reports {
            lines.push(format!(
                "  {}: ok={} mode={} tramp={:#x} {}",
                report.name, report.ok, report.mode, report.tramp, report.detail
            ));
        }
        lines
    }

    #[cfg(windows)]
    fn install(&mut self) -> Result<Vec<Report>, String> {
        // SAFETY: the module name is a NUL-terminated ASCII literal and the call
        // only queries the loader.
        let module = unsafe { win::GetModuleHandleA(c"EOSSDK-Win64-Shipping.dll".as_ptr()) };
        if module == 0 {
            return Err(format!("{EOSSDK} is not loaded"));
        }
        reset_state();

        let capture_path = std::env::var_os("HROMATITE_EAC_CAPTURE")
            .map(PathBuf::from)
            .unwrap_or_else(|| default_dir().join("hromatite_eac_capture.bin"));
        let log_path = std::env::var_os("HROMATITE_EAC_HOOK_LOG")
            .map(PathBuf::from)
            .unwrap_or_else(|| default_dir().join("hromatite_eac_hooks.log"));

        let replay_enabled = env_flag("EAC_REPLAY");
        let replay_source = std::env::var_os("HROMATITE_EAC_REPLAY")
            .map(PathBuf::from)
            .unwrap_or_else(|| capture_path.clone());
        let replay = if replay_enabled {
            read_frames(&replay_source).unwrap_or_default()
        } else {
            Vec::new()
        };
        REPLAY.store(replay_enabled, Ordering::Relaxed);

        // A non-numeric value means "off", not "cut after 20 s": the variable is
        // shared with the C shim, which used `=cmd` for a file trigger.
        let drop_after = std::env::var("EACSHIM_DROP_AFTER_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_DROP_MS);
        DROP_AFTER_MS.store(drop_after, Ordering::Relaxed);
        DROP_IN.store(env_flag("EACSHIM_DROP_IN"), Ordering::Relaxed);

        // The cut deadline is measured from BeginSession; if another image owns
        // that entry (a mode-3 reuse) no Begin event reaches this copy, so the
        // install moment is the fallback epoch start.
        SESSION_START_MS.store(now_ms(), Ordering::Relaxed);
        if let Ok(mut slot) = CAPTURE_PATH.lock() {
            *slot = Some(capture_path.clone());
        }
        let sink = Sink {
            capture: CaptureFile::open(&capture_path).ok(),
            log: LogFile::open(log_path),
            replay,
            replay_cursor: 0,
            replay_base_ms: 0,
            replay_due_ms: 0,
        };
        if let Ok(mut slot) = SINK.lock() {
            *slot = Some(sink);
        }

        // `EAC_FORGE=1`: parse once, publish into atomics, open the evidence
        // capture for the synthesized frames and start the generator thread.
        let mut forge_config =
            forge::Config::parse(env_flag("EAC_FORGE"), |name| std::env::var(name).ok());
        // `EAC_REPLAY_REPORT=<path>`: the same generator and the same takeover,
        // but each slot emits the next recorded frame. Its own deadline and
        // readiness knobs (`EAC_REPLAY_AFTER_MS`, `EAC_REPLAY_MIN_FRAMES`)
        // replace the forge ones, because a report run must still learn the
        // live `SessionId` before it can retarget anything.
        let report_config = report::Config::parse(|name| std::env::var(name).ok());
        let self_replay = env_flag("EAC_REPLAY_SELF");
        if let Some(config) = report_config.as_ref() {
            forge_config.after_ms = config.after_ms;
            forge_config.min_frames = config.min_frames;
        } else if self_replay {
            // `EAC_REPLAY_SELF=1` alone arms the takeover: the stream is built
            // from this session's own frames, so there is no file to name. The
            // deadline has to be long enough for one complete message (the
            // module reports are 35..209 frames, i.e. 70 s..7 min).
            if forge_config.after_ms == 0 {
                forge_config.after_ms = 90_000;
            }
            if forge_config.min_frames == 0 {
                forge_config.min_frames = 20;
            }
        }
        FORGE_AFTER_MS.store(forge_config.after_ms, Ordering::Relaxed);
        FORGE_MIN_FRAMES.store(forge_config.min_frames, Ordering::Relaxed);
        FORGE_INTERVAL_MS.store(forge_config.interval_ms, Ordering::Relaxed);
        FORGE_BUMP.store(forge_config.body.bumped(), Ordering::Relaxed);
        FORGE_KEEP_SMALL.store(forge_config.keep_small, Ordering::Relaxed);
        FORGE_PREFER_START.store(
            forge_config.template == forge::TemplateMode::Start,
            Ordering::Relaxed,
        );
        FORGE_STARTED.store(false, Ordering::Relaxed);
        FORGE_SEEN.store(0, Ordering::Relaxed);
        FORGE_EMITTED.store(0, Ordering::Relaxed);
        FORGE_NEXT_SEQ.store(0, Ordering::Relaxed);
        FORGE_LAST_MS.store(0, Ordering::Relaxed);
        if let Ok(mut slot) = FORGE_TEMPLATE.lock() {
            *slot = None;
        }
        // The report itself is read here, once: a handler must never touch the
        // filesystem, and `install` already reads the replay capture for the
        // same reason. A failed load degrades to the template forge.
        REPORT_ENABLED.store(report_config.is_some() || self_replay, Ordering::Relaxed);
        REPORT_FROM_WRAPPER.store(env_flag("EAC_REPLAY_FROM_WRAPPER"), Ordering::Relaxed);
        REPORT_SELF.store(self_replay, Ordering::Relaxed);
        REPORT_STOPPED.store(false, Ordering::Relaxed);
        REPORT_FLAGS_LIVE.store(
            report_config
                .as_ref()
                .map(|config| config.flags_live)
                .unwrap_or(false),
            Ordering::Relaxed,
        );
        if let Ok(mut slot) = REPORT_STREAM.lock() {
            *slot = None;
        }
        if let Some(config) = report_config.as_ref() {
            match report::Report::load(&config.path) {
                Ok(loaded) if loaded.is_empty() => {
                    REPORT_ENABLED.store(false, Ordering::Relaxed);
                    hook_log_blocking(&format!(
                        "[report] {} has no 512-byte frame (skipped={}); \
                         falling back to the template forge",
                        config.path.display(),
                        loaded.skipped()
                    ));
                }
                Ok(loaded) => {
                    let frames = loaded.len();
                    let skipped = loaded.skipped();
                    let truncated = loaded.truncated();
                    if let Ok(mut slot) = REPORT_STREAM.lock() {
                        *slot = Some(report::Stream::new(loaded, config.start, config.looping));
                    }
                    hook_log_blocking(&format!(
                        "[report] armed path={} frames={frames} skipped={skipped} \
                         truncated={truncated} start={} loop={} flags={} after_ms={} \
                         min_frames={}",
                        config.path.display(),
                        config.start,
                        config.looping,
                        if config.flags_live { "live" } else { "file" },
                        config.after_ms,
                        config.min_frames
                    ));
                }
                Err(error) => {
                    REPORT_ENABLED.store(false, Ordering::Relaxed);
                    hook_log_blocking(&format!(
                        "[report] cannot read {}: {error}; falling back to the template forge",
                        config.path.display()
                    ));
                }
            }
        }
        if forge_config.enabled() {
            let forge_path = std::env::var_os("HROMATITE_EAC_FORGE_CAPTURE")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    capture_path
                        .parent()
                        .unwrap_or_else(|| std::path::Path::new("."))
                        .join("forge_frames.bin")
                });
            if let Ok(mut slot) = FORGE_CAPTURE.lock() {
                *slot = CaptureFile::open(&forge_path).ok();
            }
            hook_log_blocking(&format!(
                "[forge] armed mode={} after_ms={} min_frames={} interval_ms={} body={:?} out={}",
                if REPORT_ENABLED.load(Ordering::Relaxed) {
                    "report"
                } else {
                    "template"
                },
                forge_config.after_ms,
                forge_config.min_frames,
                forge_config.interval_ms,
                forge_config.body,
                forge_path.display()
            ));
            // The generator thread is started from the first wrapper call, not
            // here: `install` runs inside `DllMain` (resident startup thread) and
            // creating a thread while the loader lock is held hung the SDK's
            // message pump in the first live run (`EAC_FORGE=1`, zero frames).
        }

        let handlers = handlers();

        let mut reports = Vec::with_capacity(HK_COUNT);
        hook_log_blocking(&format!(
            "[hook] {EOSSDK} base={module:#x} mode={} drop_after_ms={drop_after} drop_in={} \
             replay={} replay_frames={} capture={}",
            if replay_enabled {
                "replay"
            } else if drop_after != 0 {
                "drop"
            } else {
                "observe"
            },
            DROP_IN.load(Ordering::Relaxed),
            replay_enabled,
            SINK.lock()
                .ok()
                .and_then(|sink| sink.as_ref().map(|sink| sink.replay.len()))
                .unwrap_or(0),
            capture_path.display(),
        ));

        for (index, name) in EXPORTS.iter().enumerate() {
            // Resolve by name through the export directory, like the shim: the
            // SDK's PE headers are intact (it is a normal loaded module).
            let target = resolve_export(module, name);
            match target {
                Some(target) => {
                    // SAFETY: `target` is the entry of an exported function and
                    // `handlers[index]` an `extern "system"` detour of matching
                    // arity for the slot.
                    match unsafe { hook::install(target, handlers[index], name) } {
                        Ok(installed) => {
                            TRAMPS[index].store(installed.tramp, Ordering::Release);
                            reports.push(Report {
                                name,
                                ok: true,
                                mode: installed.mode,
                                detail: if installed.copied == 0 {
                                    "reused trampoline".to_string()
                                } else {
                                    // the whole list, so a live log shows every
                                    // relative field the trampoline had to move
                                    format!(
                                        "prologue={} copied, reloc[{}]",
                                        installed.copied, installed.note
                                    )
                                },
                                tramp: installed.tramp,
                            });
                        }
                        Err(reason) => reports.push(Report {
                            name,
                            ok: false,
                            mode: 0,
                            detail: reason,
                            tramp: 0,
                        }),
                    }
                }
                None => reports.push(Report {
                    name,
                    ok: false,
                    mode: 0,
                    detail: "export not found".to_string(),
                    tramp: 0,
                }),
            }
        }

        // `EAC_IAT=1`: also rewrite the *callers'* import slots. An inline patch
        // of the export entry is only seen by a game that has not cached the
        // pointer yet; VRChat caches `AddNotifyMessageToServer` about 20 s in,
        // so a late injection needs the IAT hop to see any outbound frame.
        if env_flag("EAC_IAT") {
            let patched = install_iat();
            hook_log_blocking(&format!("[iat] {patched} slot(s) now hold a detour"));
        }

        // `EAC_M3_PROBE=1`: locate the il2cpp class table of *this* build.
        if env_flag("EAC_M3_PROBE") {
            for line in crate::m3probe::probe() {
                hook_log_blocking(&line);
            }
        }

        // `EAC_EVP=1`: probe the statically linked BoringSSL inside the SDK.
        // Nothing here is a documented export, so each target is matched by its
        // recorded prologue and skipped on drift. The point is the `Init`
        // shape, whose `r9` is the AES-256-GCM key the protector really uses.
        if env_flag("EAC_EVP") {
            // SAFETY: `module` is the base of the loaded EOSSDK module, checked
            // non-null above, and stays mapped for the process lifetime.
            let evp_reports = unsafe { crate::evp::install(module as isize) };
            let ok = evp_reports.iter().filter(|report| report.ok).count();
            hook_log_blocking(&format!("[evp] installed {ok}/{}", evp_reports.len()));
            for report in &evp_reports {
                hook_log_blocking(&format!("[evp]{}", report.line()));
            }
        }
        Ok(reports)
    }

    #[cfg(not(windows))]
    fn install(&mut self) -> Result<Vec<Report>, String> {
        Err("host target (build for x86_64-pc-windows-gnu to hook)".to_string())
    }
}

fn default_dir() -> PathBuf {
    std::env::temp_dir()
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| !value.is_empty() && value != "0")
        .unwrap_or(false)
}

// ---------------------------------------------------------------------- iat

/// Detours in [`EXPORTS`] order.
///
/// The inline install and the IAT walk have to agree on one handler per slot, so
/// both read this table instead of keeping their own copy.
#[cfg(windows)]
fn handlers() -> [usize; HK_COUNT] {
    [
        hook_begin as *const () as usize,
        hook_end as *const () as usize,
        hook_receive as *const () as usize,
        hook_protect as *const () as usize,
        hook_unprotect as *const () as usize,
        hook_outlen as *const () as usize,
        hook_add_notify as *const () as usize,
        hook_poll_status as *const () as usize,
    ]
}

/// Emit one IAT line where a live run can find it: the hook log (next to the
/// frame evidence) and `hromatite.log`.
fn iat_line(line: &str) {
    hook_log_blocking(line);
    hromatite_core::log::log_tag("iat", line);
}

/// `EAC_IAT=1`: rewrite the EOSSDK import slots of every loaded module.
///
/// The inline hook of an export entry only wins if the game has not cached that
/// pointer yet. VRChat registers `AddNotifyMessageToServer` ~20 s into the
/// session and then calls through the cached value, which is why a late
/// injection captured 0 outbound frames; the caller's IAT slot is re-read on
/// every call, so patching it is immune to that cache.
///
/// Returns the number of slots that hold a detour. Idempotent, and a no-op
/// outside the Windows target.
pub fn install_iat() -> usize {
    if IAT_TRIED.swap(true, Ordering::SeqCst) {
        return IAT_SLOTS.load(Ordering::Relaxed);
    }
    #[cfg(windows)]
    let patched = iat_install();
    #[cfg(not(windows))]
    let patched = 0;
    IAT_SLOTS.store(patched, Ordering::Relaxed);
    hook_log_blocking(&format!("[iat] patched {patched} slots"));
    patched
}

/// Windows body of [`install_iat`].
#[cfg(windows)]
fn iat_install() -> usize {
    use hromatite_core::iat;
    let handlers = handlers();
    // The three game modules first, then every other module of the process: an
    // EOS plugin can live in a module we have never heard of, and the walk is
    // cheap (one header read per module).
    let mut names: Vec<String> = vec![
        hromatite_core::GAME_EXE.to_string(),
        hromatite_core::GAME_ASSEMBLY.to_string(),
        hromatite_core::UNITY_PLAYER.to_string(),
    ];
    for (name, _, _) in hromatite_core::procs::modules(std::process::id()) {
        if !names.iter().any(|known| known.eq_ignore_ascii_case(&name)) {
            names.push(name);
        }
    }
    let wanted: Vec<(&str, usize)> = EXPORTS.iter().copied().zip(handlers).collect();
    let mut patched = 0usize;
    for module in names {
        // The SDK's own imports are the export entries already patched above;
        // intercepting them again only risks re-entering a handler from its body.
        if module.eq_ignore_ascii_case(EOSSDK) {
            continue;
        }
        if !iat::module_imports(&module, EOSSDK) {
            continue;
        }
        match iat::patch_module(&module, EOSSDK, &wanted) {
            Ok(outcomes) => {
                let mut found = 0usize;
                for outcome in outcomes {
                    if outcome.slot == 0 {
                        continue; // this module simply does not import it
                    }
                    found += 1;
                    if outcome.ok {
                        patched += 1;
                        // Keep the first real original: a later "already
                        // patched" outcome would only record our own detour.
                        if outcome.old != outcome.new
                            && let Some(index) =
                                EXPORTS.iter().position(|name| *name == outcome.func)
                        {
                            let _ = IAT_ORIG[index].compare_exchange(
                                0,
                                outcome.old,
                                Ordering::AcqRel,
                                Ordering::Relaxed,
                            );
                        }
                    }
                    iat_line(&format!("[iat] {}", outcome.line(&module, EOSSDK)));
                }
                iat_line(&format!("[iat] {module}: {found} slot(s)"));
            }
            Err(reason) => iat_line(&format!("[iat] {module}: {reason}")),
        }
    }
    patched
}

fn reset_state() {
    let _serial = STATE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    for tramp in &TRAMPS {
        tramp.store(0, Ordering::Relaxed);
    }
    // `IAT_ORIG` is *not* cleared: those slots already point at our handlers, so
    // forgetting the original would leave a caller's next frame unrouted between
    // this reset and the export patches below. A stale address still names the
    // real function, which is exactly what the fallback needs.
    for calls in &CALLS {
        calls.store(0, Ordering::Relaxed);
    }
    for bind in &BINDS {
        bind.client_data.store(0, Ordering::Relaxed);
        bind.function.store(0, Ordering::Relaxed);
    }
    BIND_COUNT.store(0, Ordering::Relaxed);
    OUT_MESSAGES.store(0, Ordering::Relaxed);
    IN_MESSAGES.store(0, Ordering::Relaxed);
    FORWARDED.store(0, Ordering::Relaxed);
    DROPPED.store(0, Ordering::Relaxed);
    REPLAYED.store(0, Ordering::Relaxed);
    PROTECT_IN.store(0, Ordering::Relaxed);
    PROTECT_OUT.store(0, Ordering::Relaxed);
    PROTECT_SAMPLES.store(0, Ordering::Relaxed);
    PROTECT_22.store(0, Ordering::Relaxed);
    OVERHEAD_MIN.store(i32::MAX, Ordering::Relaxed);
    OVERHEAD_MAX.store(0, Ordering::Relaxed);
    LAST_OUT_MS.store(0, Ordering::Relaxed);
    SESSION_START_MS.store(0, Ordering::Relaxed);
    FORGE_STARTED.store(false, Ordering::Relaxed);
    FORGE_SEEN.store(0, Ordering::Relaxed);
    FORGE_EMITTED.store(0, Ordering::Relaxed);
    FORGE_NEXT_SEQ.store(0, Ordering::Relaxed);
    FORGE_LAST_MS.store(0, Ordering::Relaxed);
    LAST_MESSAGE_LOG.store(0, Ordering::Relaxed);
    if let Ok(mut events) = EVENTS.lock() {
        events.clear();
    }
}

fn note_overhead(delta: i32) {
    PROTECT_SAMPLES.fetch_add(1, Ordering::Relaxed);
    if delta == 22 {
        PROTECT_22.fetch_add(1, Ordering::Relaxed);
    }
    OVERHEAD_MIN.fetch_min(delta, Ordering::Relaxed);
    OVERHEAD_MAX.fetch_max(delta, Ordering::Relaxed);
}

// ------------------------------------------------------------- Win32 surface

#[cfg(windows)]
mod win {
    use core::ffi::c_char;

    pub type Handle = isize;

    unsafe extern "system" {
        pub fn GetModuleHandleA(name: *const c_char) -> Handle;
    }

    #[repr(C)]
    pub struct DosHeader {
        pub magic: u16,
        pub rest: [u16; 29],
        pub lfanew: i32,
    }

    /// Just enough of `IMAGE_EXPORT_DIRECTORY`.
    #[repr(C)]
    pub struct ExportDirectory {
        pub _characteristics: u32,
        pub _timestamp: u32,
        pub _major_version: u16,
        pub _minor_version: u16,
        pub name: u32,
        pub base: u32,
        pub number_of_functions: u32,
        pub number_of_names: u32,
        pub address_of_functions: u32,
        pub address_of_names: u32,
        pub address_of_name_ordinals: u32,
    }
}

/// Resolve an export by name through the PE export directory.
///
/// `GetProcAddress` would do, but walking the directory is what the shim does
/// and it also works for a module whose loader entry points were redirected by
/// another tool. Returns `None` for a missing name or a malformed image.
#[cfg(windows)]
fn resolve_export(module: isize, name: &str) -> Option<usize> {
    use win::{DosHeader, ExportDirectory};
    let base = module as usize;
    if base == 0 {
        return None;
    }
    // SAFETY: a loaded module starts with a readable DOS header.
    let dos = unsafe { &*(base as *const DosHeader) };
    if dos.magic != 0x5A4D {
        return None;
    }
    let nt = base + dos.lfanew as usize;
    // SAFETY: `e_lfanew` points at the NT headers of a loaded PE image.
    let signature = unsafe { core::ptr::read_unaligned(nt as *const u32) };
    if signature != 0x0000_4550 {
        return None;
    }
    // Optional header: magic u16 at +24, data directories at +112 (PE32+).
    // SAFETY: PE32+ images have the fixed optional-header prefix.
    let magic = unsafe { core::ptr::read_unaligned((nt + 24) as *const u16) };
    if magic != 0x20B {
        return None;
    }
    // SAFETY: export directory entry 0 is at optional header + 112.
    let export_rva = unsafe { core::ptr::read_unaligned((nt + 24 + 112) as *const u32) } as usize;
    if export_rva == 0 {
        return None;
    }
    let directory = base + export_rva;
    // SAFETY: the export directory RVA points inside the loaded image.
    let exports = unsafe { &*(directory as *const ExportDirectory) };
    let names_base = base + exports.address_of_names as usize;
    let ordinals_base = base + exports.address_of_name_ordinals as usize;
    let functions_base = base + exports.address_of_functions as usize;

    for index in 0..exports.number_of_names as usize {
        // SAFETY: the name pointer table has `number_of_names` entries.
        let name_rva =
            unsafe { core::ptr::read_unaligned((names_base + index * 4) as *const u32) } as usize;
        let candidate = base + name_rva;
        // SAFETY: export names are NUL-terminated ASCII inside the image.
        let candidate = unsafe { std::ffi::CStr::from_ptr(candidate as *const core::ffi::c_char) };
        if candidate.to_bytes() == name.as_bytes() {
            // SAFETY: ordinal and function RVAs are parallel arrays.
            let ordinal =
                unsafe { core::ptr::read_unaligned((ordinals_base + index * 2) as *const u16) }
                    as usize;
            if ordinal >= exports.number_of_functions as usize {
                return None;
            }
            // SAFETY: ordinal indexes the function RVA table.
            let rva =
                unsafe { core::ptr::read_unaligned((functions_base + ordinal * 4) as *const u32) }
                    as usize;
            return (rva != 0).then_some(base + rva);
        }
    }
    None
}

// ---------------------------------------------------------------- handlers

/// `EOS_AntiCheatClient_*` detour shape: six integer slots (win64 `rcx`..`r9`
/// plus two stack slots), returning the original `rax`.
type Fn6 = unsafe extern "system" fn(u64, u64, u64, u64, u64, u64) -> u64;

/// Tail-call the relocated prologue. Never call the patched entry: that is this
/// handler, and the shim's bug #1 was exactly that recursion.
///
/// # Safety
///
/// `index` must be a hooked slot whose trampoline is published, and the caller
/// must pass the original argument registers unchanged.
unsafe fn call_tramp(index: usize, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> u64 {
    let mut tramp = TRAMPS[index].load(Ordering::Acquire);
    if tramp == 0 {
        // IAT-only install (or a caller intercepted before the export patch
        // landed): fall back to the address the caller's slot held, which is the
        // real function and needs no relocation.
        tramp = IAT_ORIG[index].load(Ordering::Acquire);
    }
    if tramp == 0 {
        return 0;
    }
    // SAFETY: the trampoline is a relocated prologue ending in an absolute jump
    // back into the original function; its ABI is the target's.
    let function: Fn6 = unsafe { core::mem::transmute(tramp) };
    // SAFETY: same ABI and argument registers as the interrupted call.
    unsafe { function(a1, a2, a3, a4, a5, a6) }
}

fn verbose(calls: u64) -> bool {
    calls <= CALLS_FULL || calls.is_multiple_of(200)
}

/// Options headers shared by the AC calls (`ApiVersion` first, always).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct OptionsBegin {
    api_version: i32,
    mode: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct OptionsRecv {
    api_version: i32,
    length: u32,
    data: *const u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct OptionsProtect {
    api_version: i32,
    length: u32,
    data: *const u8,
    out_capacity: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct OptionsOutLen {
    api_version: i32,
    length: u32,
    out: *mut u32,
}

#[repr(C)]
struct MessageInfo {
    client_data: *mut c_void,
    data: *const u8,
    size: u32,
}

type MessageCallback = unsafe extern "system" fn(*const MessageInfo);

#[cfg(windows)]
unsafe extern "system" fn hook_begin(a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> u64 {
    let calls = CALLS[HK_BEGIN].fetch_add(1, Ordering::Relaxed) + 1;
    let ts = now_ms();
    let mode = if a2 != 0 {
        // SAFETY: the caller guarantees a readable options struct for the call.
        unsafe { core::ptr::read_unaligned(a2 as *const OptionsBegin) }.mode
    } else {
        -1
    };
    SESSION_START_MS.store(ts, Ordering::Relaxed);
    // A new `SessionId` starts a new frame sequence upstream, so the learned
    // template of the previous room must not be reused.
    forge_reset_epoch();
    push_event(HookEvent::SessionBegin { mode, ts_ms: ts });
    if verbose(calls) {
        hook_log(format!(
            "[session] BeginSession #{calls} handle={a1:#x} mode={mode} ts={ts}"
        ));
    }
    // SAFETY: forward the untouched argument registers to the original code.
    let result = unsafe { call_tramp(HK_BEGIN, a1, a2, a3, a4, a5, a6) };
    if verbose(calls) {
        hook_log(format!(
            "[session] BeginSession #{calls} -> ret={result:#x}"
        ));
    }
    result
}

#[cfg(windows)]
unsafe extern "system" fn hook_end(a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> u64 {
    let calls = CALLS[HK_END].fetch_add(1, Ordering::Relaxed) + 1;
    let ts = now_ms();
    push_event(HookEvent::SessionEnd { ts_ms: ts });
    if verbose(calls) {
        hook_log(format!(
            "[session] EndSession #{calls} handle={a1:#x} ts={ts}"
        ));
    }
    // SAFETY: untouched argument registers.
    let result = unsafe { call_tramp(HK_END, a1, a2, a3, a4, a5, a6) };
    if verbose(calls) {
        hook_log(format!("[session] EndSession #{calls} -> ret={result:#x}"));
    }
    result
}

/// The callback handed to the SDK instead of the game's own.
///
/// # Safety
///
/// `info` must be the SDK-built `{ClientData, MessageData, SizeBytes}` struct,
/// valid for the duration of this call.
#[cfg(windows)]
unsafe extern "system" fn message_to_server_wrapper(info: *const MessageInfo) {
    let result = std::panic::catch_unwind(|| {
        if info.is_null() {
            return;
        }
        // SAFETY: the SDK builds this struct on its own stack for the call.
        let message = unsafe { &*info };
        let length = message.size;
        // SAFETY: MessageData is a borrowed buffer of SizeBytes bytes, valid
        // for the duration of the callback.
        let data = if message.data.is_null() || length == 0 {
            &[][..]
        } else {
            // SAFETY: the SDK lends a buffer of `SizeBytes` bytes for this call.
            unsafe { core::slice::from_raw_parts(message.data, length as usize) }
        };

        let ts = now_ms();
        let previous = LAST_OUT_MS.swap(ts, Ordering::Relaxed);
        let dt = if previous == 0 {
            0
        } else {
            ts.saturating_sub(previous).min(u64::from(u32::MAX)) as u32
        };
        let count = OUT_MESSAGES.fetch_add(1, Ordering::Relaxed) + 1;

        push_event(HookEvent::OutMessage {
            len: length,
            dt_ms: dt,
            ts_ms: ts,
        });

        // Capture + log under one try_lock: a missing line is preferable to
        // blocking the SDK thread behind the feature thread's file I/O.
        if let Ok(mut slot) = SINK.try_lock()
            && let Some(sink) = slot.as_mut()
        {
            if let Some(capture) = sink.capture.as_mut() {
                let _ = capture.append(dt, data);
            }
            if count <= CALLS_FULL || count.is_multiple_of(20) {
                sink.log_line(&format!(
                    "[msg] out #{count} len={length} dt_ms={dt} hex={}",
                    hex_prefix(data, 64)
                ));
            }
        }

        // Keep the live heartbeat frames for `EAC_REPLAY_SELF`.
        if length as usize == forge::FRAME_LEN
            && let Ok(mut ring) = SELF_RING.try_lock()
        {
            ring.push(data.to_vec());
            let excess = ring.len().saturating_sub(SELF_RING_MAX);
            if excess > 0 {
                ring.drain(..excess);
            }
        }

        // `EAC_REPLAY_FROM_WRAPPER=1`: deliver from *this* thread, i.e. inside
        // the very callback the game registered, instead of from our own. A
        // separate thread deadlocked the game ~12 s after the takeover
        // (`replayreport_0339` stopped logging with the process spinning, and
        // `forge_0206` showed the same shape): the game's message pump is not
        // reentrant from a foreign thread. Cadence becomes the live stream's
        // ~2 s, which is exactly the module's cadence while it is alive.
        if REPORT_FROM_WRAPPER.load(Ordering::Relaxed)
            && REPORT_ENABLED.load(Ordering::Relaxed)
            && FORGE_AFTER_MS.load(Ordering::Relaxed) != 0
        {
            if length as usize == forge::FRAME_LEN {
                forge_learn(data, length as usize);
                // `forge_begin` re-seeds the sequence from the live template, so
                // it must run exactly once per epoch: calling it per frame made
                // every retargeted frame carry `seq=2` (observed in
                // `replaywrap2`, where the server saw duplicate sequences).
                if !FORGE_STARTED.load(Ordering::Relaxed) && !forge_begin(ts) {
                    return;
                }
                emit_report_frame(ts);
                return;
            }
            if !FORGE_KEEP_SMALL.load(Ordering::Relaxed) && FORGE_STARTED.load(Ordering::Relaxed) {
                // The takeover owns the heartbeat channel; only the small
                // control frames would still be forwarded, and only when
                // `EAC_FORGE_KEEP_SMALL=1`. Keep both variants.
                let dropped = DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
                if dropped <= 20 || dropped.is_multiple_of(50) {
                    hook_log(format!(
                        "[report] live small frame dropped len={length} dropped={dropped}"
                    ));
                }
                return;
            }
        }

        // `EAC_FORGE=1`: learn the heartbeat while the live stream is still
        // real, and from the moment the generator takes over let it own the
        // channel — a forwarded live frame would carry a `Sequence` the
        // generator has already passed.
        if FORGE_AFTER_MS.load(Ordering::Relaxed) != 0 {
            if FORGE_STARTED.load(Ordering::Relaxed) {
                let heartbeat = length as usize == forge::FRAME_LEN;
                if !heartbeat && FORGE_KEEP_SMALL.load(Ordering::Relaxed) {
                    // ACKs and handshake fragments stay live; only the
                    // heartbeat itself is ours.
                } else {
                    let dropped = DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
                    if dropped <= 20 || dropped.is_multiple_of(50) {
                        hook_log(format!(
                            "[forge] live frame replaced len={length} dropped={dropped} \
                         hex={}",
                            hex_prefix(data, 24)
                        ));
                    }
                    return;
                }
            } else {
                forge_learn(data, length as usize);
                spawn_forge_loop();
            }
        }

        if drop_active(ts) {
            let dropped = DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped <= 50 || dropped.is_multiple_of(100) {
                hook_log(format!(
                    "[cut] dropped out #{dropped} len={length} hex={}",
                    hex_prefix(data, 32)
                ));
            }
            return;
        }

        // SAFETY: `info` is the SDK-built struct for this call.
        let replayed = REPLAY.load(Ordering::Relaxed) && unsafe { replay_one(info, ts) };
        if replayed {
            return;
        }

        // SAFETY: `info` is the SDK-built struct for this call.
        unsafe { forward(info) };
    });
    if result.is_err() {
        // A panicking body must still deliver the frame: never swallow it.
        // SAFETY: `info` is the SDK-built struct for this call.
        unsafe { forward(info) };
    }
}

/// True once `EACSHIM_DROP_AFTER_MS` has elapsed inside the current epoch.
fn drop_active(now: u64) -> bool {
    let deadline = DROP_AFTER_MS.load(Ordering::Relaxed);
    if deadline == 0 {
        return false;
    }
    let start = SESSION_START_MS.load(Ordering::Relaxed);
    start != 0 && now.saturating_sub(start) >= deadline
}

// ------------------------------------------------------------------- forge

/// Remember a live 512-byte frame as the template for the current epoch.
///
/// Called from the SDK thread, so it uses `try_lock`: a skipped learning step
/// only costs one frame, the next one arrives in ~2 s anyway.
fn forge_learn(data: &[u8], length: usize) {
    if length != forge::FRAME_LEN {
        return;
    }
    let Some(template) = Template::from_frame(data) else {
        return;
    };
    let seen = FORGE_SEEN.fetch_add(1, Ordering::Relaxed) + 1;
    let sid = template.session_id;
    let flags = template.flags;
    let seq = template.last_seq;
    // `EAC_FORGE_TEMPLATE=start`: a middle fragment has no message header, so
    // only a start frame can be refreshed field by field (see `BodyMode::Bump`).
    let store = !FORGE_PREFER_START.load(Ordering::Relaxed) || template.starts_message();
    if store && let Ok(mut slot) = FORGE_TEMPLATE.try_lock() {
        let counter = template
            .body
            .get(forge::MESSAGE_COUNTER_OFFSET..forge::MESSAGE_COUNTER_OFFSET + 4)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_le_bytes)
            .unwrap_or(0);
        *slot = Some(Learned {
            template,
            bump: FORGE_BUMP.load(Ordering::Relaxed),
            counter,
        });
    }
    if seen <= 3 || seen.is_multiple_of(20) {
        hook_log(format!(
            "[forge] learned #{seen} sid=0x{sid:x} seq={seq} flags=0x{flags:x} len={length}"
        ));
    }
}

/// True when the forge may take over: the deadline elapsed, enough live frames
/// were learned and a template exists.
fn forge_due(now: u64) -> bool {
    let after = FORGE_AFTER_MS.load(Ordering::Relaxed);
    if after == 0 {
        return false;
    }
    let start = SESSION_START_MS.load(Ordering::Relaxed);
    if start == 0 || now.saturating_sub(start) < after {
        return false;
    }
    if FORGE_SEEN.load(Ordering::Relaxed) < FORGE_MIN_FRAMES.load(Ordering::Relaxed) {
        return false;
    }
    FORGE_TEMPLATE
        .try_lock()
        .map(|slot| slot.is_some())
        .unwrap_or(false)
}

/// Flip the forge on: seed the sequence from the learned frame and log the
/// transition. Runs on the generator thread, so a blocking lock is fine.
fn forge_begin(now: u64) -> bool {
    if !forge_due(now) {
        return false;
    }
    let Ok(slot) = FORGE_TEMPLATE.lock() else {
        return false;
    };
    let Some(learned) = slot.as_ref() else {
        return false;
    };
    let template = &learned.template;
    if REPORT_SELF.load(Ordering::Relaxed) && self_stream(template.session_id).is_none() {
        // No complete message of this session yet: stay with the live stream
        // and retry on the next tick instead of taking over with a fragment.
        return false;
    }
    let seq = template.next_seq();
    FORGE_NEXT_SEQ.store(seq, Ordering::Relaxed);
    FORGE_STARTED.store(true, Ordering::Release);
    hook_log_blocking(&format!(
        "[forge] start mode={} sid=0x{:x} flags=0x{:x} from_seq={seq} seen={} after_ms={} \
         interval_ms={} body={:?} uptime_ms={}",
        if REPORT_ENABLED.load(Ordering::Relaxed) {
            "report"
        } else {
            "template"
        },
        template.session_id,
        template.flags,
        FORGE_SEEN.load(Ordering::Relaxed),
        FORGE_AFTER_MS.load(Ordering::Relaxed),
        FORGE_INTERVAL_MS.load(Ordering::Relaxed),
        if FORGE_BUMP.load(Ordering::Relaxed) {
            BodyMode::Counter(0)
        } else {
            BodyMode::Stale
        },
        now.saturating_sub(SESSION_START_MS.load(Ordering::Relaxed)),
    ));
    if REPORT_ENABLED.load(Ordering::Relaxed)
        && let Ok(stream) = REPORT_STREAM.lock()
        && let Some(stream) = stream.as_ref()
    {
        hook_log_blocking(&format!(
            "[report] start idx={} of={} loop={} remaining={}",
            stream.cursor(),
            stream.total(),
            stream.looping(),
            stream.remaining(),
        ));
    }
    true
}

/// Print a self-replay wait line at most once every five seconds.
fn self_wait_log(message: String) {
    let now = now_ms();
    if now.saturating_sub(SELF_WAIT_MS.load(Ordering::Relaxed)) < 5_000 {
        return;
    }
    SELF_WAIT_MS.store(now, Ordering::Relaxed);
    hook_log(message);
}

/// Rebuild the replay stream out of this session's **own** captured frames.
///
/// The 16-byte records are the one part of the body the server can compare
/// against something it knows, and if they hash memory (addresses included),
/// frames of another process can never validate. This keeps one **complete**
/// message the live module produced in *this* epoch — from a `StartData` up to
/// the next one — and replays it with a fresh `Sequence`.
///
/// A partial message is useless for the experiment: it would contradict the
/// framing (a message that never ends) and hide the only thing this run is
/// meant to measure. Until the ring holds two message starts of this session
/// the takeover therefore waits, and [`forge_begin`] retries on the next tick.
fn self_stream(sid: u64) -> Option<usize> {
    let ring = SELF_RING.lock().ok()?.clone();
    let frames: Vec<&Vec<u8>> = ring
        .iter()
        .filter(|data| {
            data.len() == forge::FRAME_LEN
                && u64::from_le_bytes(data[4..12].try_into().unwrap_or([0; 8])) == sid
        })
        .collect();
    let starts: Vec<usize> = frames
        .iter()
        .enumerate()
        .filter(|(_, data)| u16::from_le_bytes([data[2], data[3]]) & forge::FLAG_START != 0)
        .map(|(index, _)| index)
        .collect();
    let Some((from, to)) = starts
        .len()
        .checked_sub(2)
        .map(|second_last| (starts[second_last], starts[starts.len() - 1]))
    else {
        self_wait_log(format!(
            "[report] self: {} frame(s), {} message start(s) for sid=0x{sid:x} — \
             waiting for one complete message",
            frames.len(),
            starts.len()
        ));
        return None;
    };
    let length = to - from;
    if length < 4 {
        self_wait_log(format!(
            "[report] self: the last complete message is {length} frame(s); waiting for a longer one"
        ));
        return None;
    }
    let mut buf: Vec<u8> = Vec::with_capacity(length * (forge::FRAME_LEN + 8));
    for data in &frames[from..to] {
        buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes());
        buf.extend_from_slice(data);
    }
    let report = report::Report::from_bytes(&buf);
    if report.len() != length {
        hook_log(format!(
            "[report] self: parsed {} of {length} frame(s); not taking over",
            report.len()
        ));
        return None;
    }
    let bytes = buf.len();
    if let Ok(mut slot) = REPORT_STREAM.lock() {
        *slot = Some(report::Stream::new(report, 0, true));
    }
    hook_log_blocking(&format!(
        "[report] self stream sid=0x{sid:x} frames={length} bytes={bytes} \
         ring={} (one complete message, replayed and looped)",
        ring.len()
    ));
    Some(length)
}

/// Forget the template when a new `BeginSession` opens the next epoch.
fn forge_reset_epoch() {
    FORGE_STARTED.store(false, Ordering::Release);
    FORGE_SEEN.store(0, Ordering::Relaxed);
    FORGE_NEXT_SEQ.store(0, Ordering::Relaxed);
    if let Ok(mut slot) = FORGE_TEMPLATE.try_lock() {
        *slot = None;
    }
    // A recorded report belongs to one `SessionId`; the next room starts it
    // over from `EAC_REPLAY_START` rather than resuming mid-report.
    REPORT_STOPPED.store(false, Ordering::Relaxed);
    if REPORT_ENABLED.load(Ordering::Relaxed)
        && let Ok(mut slot) = REPORT_STREAM.try_lock()
        && let Some(stream) = slot.as_mut()
    {
        stream.reset();
    }
}

/// Start the generator thread once. It owns the cadence, so the heartbeat
/// survives even if the EAC module stops calling the wrapper.
fn spawn_forge_loop() {
    if REPORT_FROM_WRAPPER.load(Ordering::Relaxed) {
        // The wrapper itself advances the stream on every live heartbeat; a
        // second generator would double-deliver and re-introduce the deadlock.
        return;
    }
    if FORGE_THREAD.swap(true, Ordering::SeqCst) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("eac-forge".to_string())
        .spawn(|| {
            let mut due = 0u64;
            let mut announced = false;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(50));
                let now = now_ms();
                if !announced {
                    announced = true;
                    hook_log(format!(
                        "[forge] generator thread alive pid={} after_ms={}",
                        std::process::id(),
                        FORGE_AFTER_MS.load(Ordering::Relaxed)
                    ));
                }
                if !FORGE_STARTED.load(Ordering::Relaxed) {
                    if !forge_begin(now) {
                        continue;
                    }
                    due = 0;
                }
                if due == 0 || now >= due {
                    emit_forge_frame(now);
                    due = now.saturating_add(FORGE_INTERVAL_MS.load(Ordering::Relaxed));
                }
            }
        });
    if spawned.is_err() {
        FORGE_THREAD.store(false, Ordering::SeqCst);
    }
}

/// `ClientData` of the first registered callback, for a synthetic message.
fn first_client_data() -> *mut c_void {
    let count = BIND_COUNT.load(Ordering::Acquire).min(MAX_BINDS);
    BINDS
        .iter()
        .take(count)
        .map(|bind| bind.client_data.load(Ordering::Relaxed) as *mut c_void)
        .find(|client_data| !client_data.is_null())
        .unwrap_or(core::ptr::null_mut())
}

/// Build one heartbeat and hand it to the game's `MessageToServer` callback.
///
/// With `EAC_REPLAY_REPORT` the frame is the next recorded one instead (see
/// [`emit_report_frame`]); everything after the bytes are chosen is shared.
fn emit_forge_frame(now: u64) {
    if REPORT_ENABLED.load(Ordering::Relaxed) {
        emit_report_frame(now);
        return;
    }
    let Ok(mut slot) = FORGE_TEMPLATE.lock() else {
        return;
    };
    let Some(learned) = slot.as_mut() else {
        return;
    };
    learned.counter = learned.counter.wrapping_add(1);
    let mode = if learned.bump {
        BodyMode::Counter(learned.counter)
    } else {
        BodyMode::Stale
    };
    let seq = FORGE_NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
    let template = &learned.template;
    let frame = forge::build_frame(template, seq, mode);
    let sid = template.session_id;
    let flags = template.flags;
    let counter = learned.counter;
    drop(slot);
    debug_assert_eq!(frame.len(), forge::FRAME_LEN);
    let crc = u32::from_le_bytes([frame[20], frame[21], frame[22], frame[23]]);
    let (delivered, dt) = deliver_frame(&frame, now);
    let count = FORGE_EMITTED.load(Ordering::Relaxed);
    hook_log(format!(
        "[forge] sid=0x{sid:x} seq={seq} crc=0x{crc:08x} len=512 flags=0x{flags:x} \
         n={count} counter={counter} dt_ms={dt} delivered={delivered}"
    ));
}

/// Emit the next recorded frame of the `EAC_REPLAY_REPORT` stream.
///
/// Only `SessionId`, `Sequence` and the checksum belong to the live session, so
/// the recorded body (message start, inner checksums, counters) survives
/// untouched — that is the whole point of replaying a report instead of
/// rebuilding one fragment forever. `dt_ms` in the log line is the *recorded*
/// inter-frame time of the source report; the capture file keeps the real
/// emission spacing.
fn emit_report_frame(now: u64) {
    // The live identity comes from the learned template, so the report's
    // frames land in *this* session. Take it and drop the lock: the report has
    // its own.
    let live = {
        let Ok(slot) = FORGE_TEMPLATE.lock() else {
            return;
        };
        let Some(learned) = slot.as_ref() else {
            return;
        };
        (learned.template.session_id, learned.template.flags)
    };
    let flags_override = REPORT_FLAGS_LIVE.load(Ordering::Relaxed).then_some(live.1);
    let next = {
        let Ok(mut slot) = REPORT_STREAM.lock() else {
            return;
        };
        let Some(stream) = slot.as_mut() else {
            return;
        };
        match stream.next_frame() {
            Some(next) => next,
            None => {
                // Log the stop once; the thread keeps its cadence and stays
                // alive, so a later `BeginSession` resets and resumes.
                if !stream.finished() {
                    return;
                }
                let (index, of, wraps, looping) = (
                    stream.cursor(),
                    stream.total(),
                    stream.wraps(),
                    stream.looping(),
                );
                drop(slot);
                if REPORT_STOPPED.swap(true, Ordering::Relaxed) {
                    return;
                }
                hook_log(format!(
                    "[report] stream exhausted idx={index} of={of} wraps={wraps} \
                     loop={looping} delivered={}",
                    FORGE_EMITTED.load(Ordering::Relaxed)
                ));
                return;
            }
        }
    };
    let seq = FORGE_NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
    let Some(frame) = report::retarget(&next.data, live.0, seq, flags_override) else {
        hook_log(format!(
            "[report] idx={} of={} len={} is not a 512-byte frame; skipped",
            next.index,
            next.of,
            next.data.len()
        ));
        return;
    };
    debug_assert_eq!(frame.len(), forge::FRAME_LEN);
    let crc = u32::from_le_bytes(
        frame[forge::CHECKSUM_OFFSET..forge::HEADER_LEN]
            .try_into()
            .expect("4 bytes"),
    );
    let flags = u16::from_le_bytes([frame[2], frame[3]]);
    let (delivered, _) = deliver_frame(&frame, now);
    hook_log(format!(
        "[report] sid=0x{:x} seq={seq} idx={} of={} flags=0x{flags:x} crc=0x{crc:08x} \
         delivered={delivered} dt_ms={}",
        live.0, next.index, next.of, next.dt_ms
    ));
}

/// Append `frame` to the evidence capture and hand it to the game callback.
///
/// Returns `(delivered, dt_ms)`: whether a callback was bound, and the real
/// spacing since the previous emission (the capture file's `dt_ms`).
fn deliver_frame(frame: &[u8], now: u64) -> (bool, u32) {
    let previous = FORGE_LAST_MS.swap(now, Ordering::Relaxed);
    let dt = if previous == 0 {
        0
    } else {
        now.saturating_sub(previous).min(u64::from(u32::MAX)) as u32
    };

    // Evidence: the exact bytes we hand over, in the capture format, so
    // `frame_analyze.py checksum` can verify every CRC offline.
    if let Ok(mut slot) = FORGE_CAPTURE.lock()
        && let Some(capture) = slot.as_mut()
    {
        let _ = capture.append(dt, frame);
    }

    let info = MessageInfo {
        client_data: first_client_data(),
        data: frame.as_ptr(),
        size: frame.len() as u32,
    };
    let delivered = match bound_callback(info.client_data as usize) {
        Some(function) => {
            // SAFETY: `bound_callback` returns a function the game registered
            // through `AddNotifyMessageToServer`, i.e. the exact
            // `extern "system" fn(*const MessageInfo)` ABI.
            let callback: MessageCallback = unsafe { core::mem::transmute(function) };
            // SAFETY: `info` and the bytes behind `info.data` live on this
            // stack frame for the whole call, exactly as the SDK's own call.
            unsafe { callback(&info) };
            FORGE_EMITTED.fetch_add(1, Ordering::Relaxed);
            true
        }
        None => false,
    };
    (delivered, dt)
}

/// Replay one captured frame into the game callback when it is due.
///
/// The stored `dt_ms` values are kept: frames are released as their cumulative
/// offset elapses, so the original cadence shape survives even though the
/// driving clock is the arrival of a live message.
///
/// # Safety
///
/// `info` must be a valid [`MessageInfo`] built by the SDK for this call.
unsafe fn replay_one(info: *const MessageInfo, now: u64) -> bool {
    let Ok(mut slot) = SINK.try_lock() else {
        return false;
    };
    let Some(sink) = slot.as_mut() else {
        return false;
    };
    if sink.replay.is_empty() || sink.replay_cursor >= sink.replay.len() {
        return false;
    }
    if sink.replay_base_ms == 0 {
        sink.replay_base_ms = now;
        sink.replay_due_ms = 0;
    }
    let elapsed = now.saturating_sub(sink.replay_base_ms);
    let mut emitted = false;
    while sink.replay_cursor < sink.replay.len() {
        let frame = &sink.replay[sink.replay_cursor];
        let due = sink.replay_due_ms + u64::from(frame.dt_ms);
        if elapsed < due {
            break;
        }
        let synthetic = MessageInfo {
            // SAFETY: the caller proved `info` is readable.
            client_data: unsafe { (*info).client_data },
            data: frame.data.as_ptr(),
            size: frame.data.len() as u32,
        };
        // SAFETY: the game callback is `extern "system" fn(*const MessageInfo)`
        // and was registered by `AddNotifyMessageToServer`.
        if let Some(callback) = bound_callback(synthetic.client_data as usize) {
            // SAFETY: bound_callback returns a function the game registered with
            // the exact `AddNotifyMessageToServer` callback ABI.
            let callback: MessageCallback = unsafe { core::mem::transmute(callback) };
            // SAFETY: synthetic lives on this stack frame for the call.
            unsafe { callback(&synthetic) };
            REPLAYED.fetch_add(1, Ordering::Relaxed);
        }
        sink.replay_due_ms = due;
        sink.replay_cursor += 1;
        emitted = true;
    }
    emitted
}

/// Game callback for a `ClientData` value (falls back to the first binding,
/// exactly like the shim: the SDK may pass a different context on replay).
fn bound_callback(client_data: usize) -> Option<usize> {
    let count = BIND_COUNT.load(Ordering::Acquire).min(MAX_BINDS);
    for bind in BINDS.iter().take(count) {
        if bind.client_data.load(Ordering::Relaxed) == client_data {
            let function = bind.function.load(Ordering::Relaxed);
            if function != 0 {
                return Some(function);
            }
        }
    }
    BINDS
        .iter()
        .take(count)
        .map(|bind| bind.function.load(Ordering::Relaxed))
        .find(|function| *function != 0)
}

/// Forward a message to the game's own callback.
///
/// # Safety
///
/// `info` must be the SDK-built struct for this call.
unsafe fn forward(info: *const MessageInfo) {
    if info.is_null() {
        return;
    }
    // SAFETY: caller proved the struct is readable for this call.
    let client_data = unsafe { (*info).client_data } as usize;
    let Some(function) = bound_callback(client_data) else {
        return;
    };
    FORWARDED.fetch_add(1, Ordering::Relaxed);
    // SAFETY: the game registered this function with the exact AC callback ABI.
    let callback: MessageCallback = unsafe { core::mem::transmute(function) };
    // SAFETY: the info pointer is valid for this call.
    unsafe { callback(info) };
}

#[cfg(windows)]
unsafe extern "system" fn hook_receive(
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
    a6: u64,
) -> u64 {
    let calls = CALLS[HK_RECEIVE].fetch_add(1, Ordering::Relaxed) + 1;
    let ts = now_ms();
    let (length, data) = if a2 != 0 {
        // SAFETY: the caller guarantees a readable options struct.
        let options = unsafe { core::ptr::read_unaligned(a2 as *const OptionsRecv) };
        (options.length, options.data)
    } else {
        (0, core::ptr::null())
    };
    IN_MESSAGES.fetch_add(1, Ordering::Relaxed);
    push_event(HookEvent::InMessage {
        len: length,
        ts_ms: ts,
    });
    if verbose(calls) {
        // SAFETY: MessageData is a borrowed buffer of DataLengthBytes bytes.
        let bytes = if data.is_null() || length == 0 {
            &[][..]
        } else {
            // SAFETY: MessageData is a borrowed buffer of DataLengthBytes bytes.
            unsafe { core::slice::from_raw_parts(data, length as usize) }
        };
        hook_log(format!(
            "[msg] in #{calls} len={length} hex={}",
            hex_prefix(bytes, 64)
        ));
    }
    if DROP_IN.load(Ordering::Relaxed) && drop_active(ts) {
        let dropped = DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
        if dropped <= 50 || dropped.is_multiple_of(100) {
            hook_log(format!("[cut] dropped in #{dropped} len={length}"));
        }
        return 0; // EOS_Success
    }
    // SAFETY: untouched argument registers.
    let result = unsafe { call_tramp(HK_RECEIVE, a1, a2, a3, a4, a5, a6) };
    if verbose(calls) {
        hook_log(format!("[msg] in #{calls} -> ret={result:#x}"));
    }
    result
}

#[cfg(windows)]
unsafe extern "system" fn hook_protect(
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
    a6: u64,
) -> u64 {
    let calls = CALLS[HK_PROTECT].fetch_add(1, Ordering::Relaxed) + 1;
    let (in_len, _out_cap) = if a2 != 0 {
        // SAFETY: readable options struct per the call contract.
        let options = unsafe { core::ptr::read_unaligned(a2 as *const OptionsProtect) };
        (options.length, options.out_capacity)
    } else {
        (0, 0)
    };
    // SAFETY: untouched argument registers.
    let result = unsafe { call_tramp(HK_PROTECT, a1, a2, a3, a4, a5, a6) };
    let out_len = if a4 != 0 {
        // SAFETY: the API writes the produced length through this pointer.
        unsafe { core::ptr::read_unaligned(a4 as *const u32) }
    } else {
        0
    };
    if in_len != 0 && out_len != 0 {
        PROTECT_IN.fetch_add(u64::from(in_len), Ordering::Relaxed);
        PROTECT_OUT.fetch_add(u64::from(out_len), Ordering::Relaxed);
        note_overhead(out_len as i32 - in_len as i32);
        push_event(HookEvent::Protect { in_len, out_len });
    }
    if verbose(calls) {
        hook_log(format!(
            "[call] Protect #{calls} in={in_len} out={out_len} overhead={} ret={result:#x}",
            out_len as i64 - in_len as i64
        ));
    }
    result
}

#[cfg(windows)]
unsafe extern "system" fn hook_unprotect(
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
    a6: u64,
) -> u64 {
    let calls = CALLS[HK_UNPROTECT].fetch_add(1, Ordering::Relaxed) + 1;
    let in_len = if a2 != 0 {
        // SAFETY: readable options struct per the call contract.
        unsafe { core::ptr::read_unaligned(a2 as *const OptionsProtect) }.length
    } else {
        0
    };
    // SAFETY: untouched argument registers.
    let result = unsafe { call_tramp(HK_UNPROTECT, a1, a2, a3, a4, a5, a6) };
    let out_len = if a4 != 0 {
        // SAFETY: the API writes the produced length through this pointer.
        unsafe { core::ptr::read_unaligned(a4 as *const u32) }
    } else {
        0
    };
    if verbose(calls) {
        hook_log(format!(
            "[call] Unprotect #{calls} in={in_len} out={out_len} overhead={} ret={result:#x}",
            in_len as i64 - out_len as i64
        ));
    }
    result
}

#[cfg(windows)]
unsafe extern "system" fn hook_outlen(a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> u64 {
    let calls = CALLS[HK_OUTLEN].fetch_add(1, Ordering::Relaxed) + 1;
    let in_len = if a2 != 0 {
        // SAFETY: readable options struct per the call contract.
        unsafe { core::ptr::read_unaligned(a2 as *const OptionsOutLen) }.length
    } else {
        0
    };
    // SAFETY: untouched argument registers.
    let result = unsafe { call_tramp(HK_OUTLEN, a1, a2, a3, a4, a5, a6) };
    let needed = if a2 != 0 {
        // SAFETY: the API writes the required length through `Out`.
        let options = unsafe { core::ptr::read_unaligned(a2 as *const OptionsOutLen) };
        if options.out.is_null() {
            0
        } else {
            // SAFETY: `Out` is a writable u32 owned by the caller of the API.
            unsafe { core::ptr::read_unaligned(options.out) }
        }
    } else {
        0
    };
    if in_len != 0 && needed != 0 {
        note_overhead(needed as i32 - in_len as i32);
        push_event(HookEvent::Protect {
            in_len,
            out_len: needed,
        });
    }
    if verbose(calls) {
        hook_log(format!(
            "[call] GetProtectMessageOutputLength #{calls} in={in_len} need={needed} \
             overhead={} ret={result:#x}",
            needed as i64 - in_len as i64
        ));
    }
    result
}

#[cfg(windows)]
unsafe extern "system" fn hook_add_notify(
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
    a6: u64,
) -> u64 {
    let calls = CALLS[HK_ADD_NOTIFY].fetch_add(1, Ordering::Relaxed) + 1;
    let api = if a2 != 0 {
        // SAFETY: readable options struct per the call contract.
        unsafe { core::ptr::read_unaligned(a2 as *const i32) }
    } else {
        -1
    };
    let slot = BIND_COUNT.fetch_add(1, Ordering::Relaxed);
    if slot < MAX_BINDS {
        BINDS[slot]
            .client_data
            .store(a3 as usize, Ordering::Relaxed);
        BINDS[slot].function.store(a4 as usize, Ordering::Relaxed);
    }
    hook_log(format!(
        "[cut] AddNotifyMessageToServer #{calls} handle={a1:#x} api={api} \
         clientData={a3:#x} game_fn={a4:#x} -> wrapper"
    ));
    let wrapper = (message_to_server_wrapper as *const ()) as usize as u64;
    // SAFETY: slot 6's trampoline; only argument 4 (the callback) is replaced.
    let result = unsafe { call_tramp(HK_ADD_NOTIFY, a1, a2, a3, wrapper, a5, a6) };
    hook_log(format!(
        "[cut] AddNotifyMessageToServer #{calls} -> notification={result:#x}"
    ));
    result
}

#[cfg(windows)]
unsafe extern "system" fn hook_poll_status(
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
    a6: u64,
) -> u64 {
    let calls = CALLS[HK_POLL].fetch_add(1, Ordering::Relaxed) + 1;
    let result = {
        // SAFETY: untouched argument registers.
        unsafe { call_tramp(HK_POLL, a1, a2, a3, a4, a5, a6) }
    };
    if calls <= 10 || calls.is_multiple_of(500) {
        hook_log(format!(
            "[call] PollStatus #{calls} handle={a1:#x} -> ret={result:#x}"
        ));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_are_bounded_and_drained() {
        reset_state();
        for index in 0..MAX_EVENTS + 10 {
            push_event(HookEvent::OutMessage {
                len: index as u32,
                dt_ms: 0,
                ts_ms: 0,
            });
        }
        let drained = EVENTS
            .lock()
            .map(|mut queue| queue.drain(..).count())
            .unwrap_or(0);
        assert_eq!(drained, MAX_EVENTS);
    }

    #[test]
    fn capture_paths_default_to_the_temp_dir() {
        let capture = default_dir().join("hromatite_eac_capture.bin");
        assert!(capture.starts_with(std::env::temp_dir()));
    }

    #[test]
    fn drop_deadline_is_measured_from_the_session_start() {
        SESSION_START_MS.store(1_000, Ordering::Relaxed);
        DROP_AFTER_MS.store(5_000, Ordering::Relaxed);
        assert!(!drop_active(5_999));
        assert!(drop_active(6_000));
        DROP_AFTER_MS.store(0, Ordering::Relaxed);
        assert!(!drop_active(60_000));
    }

    #[test]
    fn overhead_histogram_recognises_the_documented_22_bytes() {
        // Only the counters used here are reset: `reset_state` also clears the
        // event queue, and tests run in parallel with the queue test.
        PROTECT_SAMPLES.store(0, Ordering::Relaxed);
        PROTECT_22.store(0, Ordering::Relaxed);
        OVERHEAD_MIN.store(i32::MAX, Ordering::Relaxed);
        OVERHEAD_MAX.store(0, Ordering::Relaxed);
        note_overhead(22);
        note_overhead(22);
        note_overhead(23);
        let counters = Counters {
            samples: PROTECT_SAMPLES.load(Ordering::Relaxed),
            overhead_22: PROTECT_22.load(Ordering::Relaxed),
            overhead_min: OVERHEAD_MIN.load(Ordering::Relaxed),
            overhead_max: OVERHEAD_MAX.load(Ordering::Relaxed),
            ..Counters::default()
        };
        assert_eq!(counters.samples, 3);
        assert_eq!(counters.overhead_22, 2);
        assert_eq!(counters.overhead_min, 22);
        assert_eq!(counters.overhead_max, 23);
    }

    #[test]
    fn a_sinkless_hook_log_does_not_panic() {
        hook_log("no sink yet");
    }

    // ------------------------------------------------------------- forge
    //
    // These tests touch the forge globals, so they take a lock and reset the
    // whole hook state instead of relying on ordering.

    static DELIVERED: AtomicU64 = AtomicU64::new(0);
    static DELIVERED_LEN: AtomicU64 = AtomicU64::new(0);

    /// Reset only the forge state: `reset_state` also clears the event queue and
    /// the tests run in parallel with the queue test.
    fn forge_state_reset() {
        FORGE_AFTER_MS.store(0, Ordering::Relaxed);
        FORGE_MIN_FRAMES.store(0, Ordering::Relaxed);
        FORGE_INTERVAL_MS.store(forge::INTERVAL_MS, Ordering::Relaxed);
        FORGE_BUMP.store(false, Ordering::Relaxed);
        FORGE_STARTED.store(false, Ordering::Relaxed);
        FORGE_SEEN.store(0, Ordering::Relaxed);
        FORGE_EMITTED.store(0, Ordering::Relaxed);
        FORGE_NEXT_SEQ.store(0, Ordering::Relaxed);
        FORGE_LAST_MS.store(0, Ordering::Relaxed);
        *FORGE_TEMPLATE.lock().expect("template lock") = None;
        REPORT_ENABLED.store(false, Ordering::Relaxed);
        REPORT_FLAGS_LIVE.store(false, Ordering::Relaxed);
        REPORT_STOPPED.store(false, Ordering::Relaxed);
        *REPORT_STREAM.lock().expect("report lock") = None;
    }

    /// Stand-in for the game's callback: records one delivery.
    unsafe extern "system" fn recording_callback(info: *const MessageInfo) {
        if info.is_null() {
            return;
        }
        // SAFETY: the caller builds the struct for the duration of the call.
        let message = unsafe { &*info };
        DELIVERED.fetch_add(1, Ordering::Relaxed);
        DELIVERED_LEN.store(u64::from(message.size), Ordering::Relaxed);
        assert!(!message.data.is_null());
    }

    fn forge_frame(tag: u8) -> Vec<u8> {
        let template = Template {
            session_id: 0x158b,
            flags: 0,
            last_seq: 10,
            body: vec![tag; forge::BODY_LEN],
        };
        forge::build_frame(&template, 11, BodyMode::Stale)
    }

    #[test]
    fn the_forge_only_learns_512_byte_frames_and_waits_for_its_deadline() {
        let _serial = STATE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        forge_state_reset();
        FORGE_AFTER_MS.store(30_000, Ordering::Relaxed);
        FORGE_MIN_FRAMES.store(2, Ordering::Relaxed);
        SESSION_START_MS.store(1_000, Ordering::Relaxed);

        forge_learn(&forge_frame(0xAA)[..24], 24);
        assert_eq!(FORGE_SEEN.load(Ordering::Relaxed), 0, "a 24-byte ACK");
        assert!(!forge_due(60_000), "no 512-byte frame learned yet");

        forge_learn(&forge_frame(0xAA), forge::FRAME_LEN);
        assert_eq!(FORGE_SEEN.load(Ordering::Relaxed), 1);
        assert!(!forge_due(60_000), "the minimum frame count is not reached");

        forge_learn(&forge_frame(0xBB), forge::FRAME_LEN);
        assert_eq!(FORGE_SEEN.load(Ordering::Relaxed), 2);
        assert!(
            !forge_due(30_999),
            "the deadline is measured from BeginSession"
        );
        assert!(forge_due(31_000));

        let template = FORGE_TEMPLATE
            .lock()
            .expect("template lock")
            .clone()
            .expect("template")
            .template;
        assert_eq!(template.session_id, 0x158b);
        assert_eq!(template.last_seq, 11, "the newest live frame wins");
        assert_eq!(template.body[0], 0xBB);

        // A new epoch forgets everything learned in the previous room.
        forge_reset_epoch();
        assert!(!forge_due(60_000));
        assert!(FORGE_TEMPLATE.lock().expect("template lock").is_none());
        forge_state_reset();
    }

    #[test]
    fn the_bump_mode_is_wired_through_the_emitter() {
        let _serial = STATE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        forge_state_reset();
        let mut template = Template::from_frame(&forge_frame(0xDD)).expect("template");
        template.flags = forge::FLAG_START;
        template.body[forge::MESSAGE_COUNTER_OFFSET..forge::MESSAGE_COUNTER_OFFSET + 4]
            .copy_from_slice(&7u32.to_le_bytes());
        *FORGE_TEMPLATE.lock().expect("template lock") = Some(Learned {
            template,
            bump: true,
            counter: 7,
        });
        FORGE_BUMP.store(true, Ordering::Relaxed);
        FORGE_NEXT_SEQ.store(1, Ordering::Relaxed);
        // No callback bound: the function still builds, captures and logs.
        emit_forge_frame(1_000);
        emit_forge_frame(3_000);
        let slot = FORGE_CAPTURE.lock().expect("capture lock");
        if let Some(capture) = slot.as_ref() {
            let frames = crate::capture::read_frames(capture.path()).expect("capture");
            let counters: Vec<u32> = frames
                .iter()
                .rev()
                .take(2)
                .map(|frame| {
                    u32::from_le_bytes(
                        frame.data[24 + forge::MESSAGE_COUNTER_OFFSET
                            ..24 + forge::MESSAGE_COUNTER_OFFSET + 4]
                            .try_into()
                            .expect("4 bytes"),
                    )
                })
                .collect();
            assert_eq!(counters, vec![9, 8], "counter advances per emitted frame");
        }
        forge_state_reset();
    }

    #[test]
    fn an_emitted_frame_reaches_the_registered_callback_with_a_valid_crc() {
        let _serial = STATE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        forge_state_reset();
        DELIVERED.store(0, Ordering::Relaxed);
        DELIVERED_LEN.store(0, Ordering::Relaxed);
        BINDS[0].client_data.store(0x33, Ordering::Relaxed);
        BINDS[0]
            .function
            .store(recording_callback as *const () as usize, Ordering::Relaxed);
        BIND_COUNT.store(1, Ordering::Relaxed);

        let template = Template::from_frame(&forge_frame(0xCC)).expect("template");
        *FORGE_TEMPLATE.lock().expect("template lock") = Some(Learned {
            template,
            bump: false,
            counter: 0,
        });
        FORGE_NEXT_SEQ.store(42, Ordering::Relaxed);
        FORGE_BUMP.store(false, Ordering::Relaxed);

        emit_forge_frame(5_000);
        assert_eq!(DELIVERED.load(Ordering::Relaxed), 1, "callback invoked");
        assert_eq!(DELIVERED_LEN.load(Ordering::Relaxed), 512);
        assert_eq!(FORGE_EMITTED.load(Ordering::Relaxed), 1);
        assert_eq!(
            FORGE_NEXT_SEQ.load(Ordering::Relaxed),
            43,
            "the sequence advances per frame"
        );

        // Rebuild the same frame and check the CRC field the way the Python
        // reference does: recompute over the frame with 20..24 zeroed.
        let frame = {
            let slot = FORGE_TEMPLATE.lock().expect("template lock");
            let learned = slot.as_ref().expect("template");
            forge::build_frame(&learned.template, 42, BodyMode::Stale)
        };
        let stored = u32::from_le_bytes(frame[20..24].try_into().expect("4 bytes"));
        let mut zeroed = frame.clone();
        zeroed[20..24].copy_from_slice(&[0; 4]);
        assert_eq!(crate::digest::crc32c_eac(&zeroed), stored);
        BINDS[0].function.store(0, Ordering::Relaxed);
        BIND_COUNT.store(0, Ordering::Relaxed);
        forge_state_reset();
    }

    /// Two recorded frames: the first starts a message, the second continues
    /// it. `tag` makes their bodies distinguishable.
    fn recorded_report(session: u64, tags: [u8; 2]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (index, tag) in tags.into_iter().enumerate() {
            let template = Template {
                session_id: session,
                flags: if index == 0 { forge::FLAG_START } else { 0 },
                last_seq: index as u64 + 1,
                body: vec![tag; forge::BODY_LEN],
            };
            let frame = forge::build_frame(&template, index as u64 + 1, BodyMode::Stale);
            report::record(&mut bytes, index as u32 * 2_007, &frame);
        }
        bytes
    }

    fn capture_path(tag: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "hromatite-eac-report-{tag}-{}.bin",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn the_report_mode_retargets_recorded_frames_and_keeps_their_bodies() {
        let _serial = STATE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        forge_state_reset();
        DELIVERED.store(0, Ordering::Relaxed);
        DELIVERED_LEN.store(0, Ordering::Relaxed);
        BINDS[0].client_data.store(0x44, Ordering::Relaxed);
        BINDS[0]
            .function
            .store(recording_callback as *const () as usize, Ordering::Relaxed);
        BIND_COUNT.store(1, Ordering::Relaxed);

        let recorded = recorded_report(0x158b, [0xA1, 0xB2]);
        let path = capture_path("replay");
        *FORGE_CAPTURE.lock().expect("capture lock") = CaptureFile::open(&path).ok();
        *REPORT_STREAM.lock().expect("report lock") = Some(report::Stream::new(
            report::Report::from_bytes(&recorded),
            0,
            false,
        ));
        REPORT_ENABLED.store(true, Ordering::Relaxed);
        FORGE_NEXT_SEQ.store(100, Ordering::Relaxed);

        // The live session is a different one than the recording.
        let mut live = Template::from_frame(&forge_frame(0xCC)).expect("template");
        live.session_id = 0x77aa;
        live.flags = forge::FLAG_START;
        *FORGE_TEMPLATE.lock().expect("template lock") = Some(Learned {
            template: live,
            bump: false,
            counter: 0,
        });

        emit_forge_frame(1_000);
        emit_forge_frame(3_000);
        assert_eq!(
            DELIVERED.load(Ordering::Relaxed),
            2,
            "both frames delivered"
        );
        assert_eq!(DELIVERED_LEN.load(Ordering::Relaxed), 512);
        assert_eq!(FORGE_NEXT_SEQ.load(Ordering::Relaxed), 102, "seq advances");

        // The evidence capture holds exactly what was handed over: recorded
        // bodies, live `SessionId`, running `Sequence`, valid CRCs.
        let frames = crate::capture::read_frames(&path).expect("capture");
        assert_eq!(frames.len(), 2);
        let source = report::Report::from_bytes(&recorded);
        for (index, frame) in frames.iter().enumerate() {
            let data = &frame.data;
            assert_eq!(data.len(), forge::FRAME_LEN);
            assert_eq!(
                &data[24..],
                source.body(index).expect("recorded frame"),
                "the recorded body survives byte for byte"
            );
            assert_eq!(
                u64::from_le_bytes(data[4..12].try_into().expect("8 bytes")),
                0x77aa,
                "retargeted to the live session"
            );
            assert_eq!(
                u64::from_le_bytes(data[12..20].try_into().expect("8 bytes")),
                100 + index as u64,
                "the sequence continues the live one"
            );
            let mut zeroed = data.clone();
            zeroed[forge::CHECKSUM_OFFSET..forge::HEADER_LEN].fill(0);
            assert_eq!(
                crate::digest::crc32c_eac(&zeroed),
                u32::from_le_bytes(
                    data[forge::CHECKSUM_OFFSET..forge::HEADER_LEN]
                        .try_into()
                        .expect("4 bytes")
                ),
                "the CRC covers the retargeted frame"
            );
        }
        // `EAC_REPLAY_FLAGS` is off: the recording's own flags are kept.
        assert_eq!(
            u16::from_le_bytes([frames[0].data[2], frames[0].data[3]]),
            forge::FLAG_START
        );
        assert_eq!(
            u16::from_le_bytes([frames[1].data[2], frames[1].data[3]]),
            0,
            "the second recorded frame is a continuation, not a start"
        );

        // No loop: the third slot has nothing left to say and delivers nothing.
        emit_forge_frame(5_000);
        assert_eq!(DELIVERED.load(Ordering::Relaxed), 2, "stream exhausted");
        assert!(REPORT_STOPPED.load(Ordering::Relaxed));
        emit_forge_frame(7_000);
        assert_eq!(DELIVERED.load(Ordering::Relaxed), 2, "stays stopped");

        *FORGE_CAPTURE.lock().expect("capture lock") = None;
        let _ = std::fs::remove_file(&path);
        BINDS[0].function.store(0, Ordering::Relaxed);
        BIND_COUNT.store(0, Ordering::Relaxed);
        forge_state_reset();
    }

    #[test]
    fn the_report_mode_can_take_the_live_flags_and_loops_when_asked() {
        let _serial = STATE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        forge_state_reset();
        DELIVERED.store(0, Ordering::Relaxed);
        BINDS[0].client_data.store(0x55, Ordering::Relaxed);
        BINDS[0]
            .function
            .store(recording_callback as *const () as usize, Ordering::Relaxed);
        BIND_COUNT.store(1, Ordering::Relaxed);

        let recorded = recorded_report(0x158b, [0xD1, 0xD2]);
        let path = capture_path("loop");
        *FORGE_CAPTURE.lock().expect("capture lock") = CaptureFile::open(&path).ok();
        let mut stream = report::Stream::new(report::Report::from_bytes(&recorded), 1, true);
        // Prove the loop at the stream level first: idx 1, then back to 1.
        assert_eq!(stream.next_frame().expect("frame 1").index, 1);
        assert_eq!(stream.next_frame().expect("wrapped").index, 1);
        stream.reset();
        *REPORT_STREAM.lock().expect("report lock") = Some(stream);

        REPORT_ENABLED.store(true, Ordering::Relaxed);
        REPORT_FLAGS_LIVE.store(true, Ordering::Relaxed);
        FORGE_NEXT_SEQ.store(7, Ordering::Relaxed);

        let mut live = Template::from_frame(&forge_frame(0xCC)).expect("template");
        live.session_id = 0x9001;
        live.flags = forge::FLAG_START;
        *FORGE_TEMPLATE.lock().expect("template lock") = Some(Learned {
            template: live,
            bump: false,
            counter: 0,
        });

        emit_forge_frame(1_000);
        emit_forge_frame(3_000);
        emit_forge_frame(5_000);
        assert_eq!(DELIVERED.load(Ordering::Relaxed), 3, "the loop never stops");

        let frames = crate::capture::read_frames(&path).expect("capture");
        assert_eq!(frames.len(), 3);
        for frame in &frames {
            assert_eq!(
                u16::from_le_bytes([frame.data[2], frame.data[3]]),
                forge::FLAG_START,
                "EAC_REPLAY_FLAGS=live overrides the recording"
            );
        }
        // Frame 1 is the continuation; its body is what the loop replays.
        assert_eq!(&frames[0].data[24..], &vec![0xD2u8; forge::BODY_LEN][..]);

        *FORGE_CAPTURE.lock().expect("capture lock") = None;
        let _ = std::fs::remove_file(&path);
        BINDS[0].function.store(0, Ordering::Relaxed);
        BIND_COUNT.store(0, Ordering::Relaxed);
        forge_state_reset();
    }
}
