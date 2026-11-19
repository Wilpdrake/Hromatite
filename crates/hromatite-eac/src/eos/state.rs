//! Pure, always-compiled EAC hook **state**: the observation contract that a
//! feature image (and the host tests) read without any OS layer.
//!
//! `hromatite-features` links this crate with `default-features = false`, i.e.
//! without `hromatite-win` / `hromatite-hook`, and still needs [`Hooks`],
//! [`HookEvent`], [`Counters`], [`Report`], `Hooks::observe` / `counters` /
//! `status_lines` and the global atomics behind them. They live here, behind no
//! feature, so the pure protocol core stays free of Win32.
//!
//! The install path, the twelve detours, the IAT walk and the BoringSSL EVP
//! probe stay in the sibling [`super::hooks`] module, which only compiles with
//! the `runtime` feature. [`Hooks::install`] here is the no-runtime sibling: it
//! reports why nothing can be patched instead of pulling in `hromatite-hook`.
//!
//! State is process-global by design: every handler (runtime) and every feature
//! generation reads the same atomics, so a generation swap never loses a count.

#![cfg_attr(
    not(all(windows, feature = "runtime")),
    allow(dead_code, unused_imports)
)]

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::capture::{CaptureFile, Frame};
use crate::clock::now_ms;
use crate::epoch;
use crate::forge::{self, Template};
use crate::report;

/// Mutable forge state that belongs to one learned frame.
#[derive(Clone)]
pub(crate) struct Learned {
    pub(crate) template: Template,
    /// `EAC_FORGE_BODY=bump`: rewrite the message counter per frame.
    pub(crate) bump: bool,
    /// Next value for the message counter at `msg+0x18`.
    pub(crate) counter: u32,
}

/// DLL that owns every AC export we hook.
pub const EOSSDK: &str = "EOSSDK-Win64-Shipping.dll";

/// Hook slots, in the shim's order.
pub(crate) const HK_BEGIN: usize = 0;
pub(crate) const HK_END: usize = 1;
pub(crate) const HK_RECEIVE: usize = 2;
pub(crate) const HK_PROTECT: usize = 3;
pub(crate) const HK_UNPROTECT: usize = 4;
pub(crate) const HK_OUTLEN: usize = 5;
pub(crate) const HK_ADD_NOTIFY: usize = 6;
pub(crate) const HK_POLL: usize = 7;
/// `EAC_BLIND=1`: local verdict suppression (see [`hook_add_integrity`]).
pub(crate) const HK_INTEGRITY: usize = 8;
pub(crate) const HK_BUILD_ID: usize = 9;
pub(crate) const HK_SYSREQ: usize = 10;
pub(crate) const HK_COUNT: usize = 11;

/// The exports actually hooked. The shim's other four only forward; they are
/// left untouched so the patch surface stays as small as the requirement.
pub(crate) const EXPORTS: [&str; HK_COUNT] = [
    "EOS_AntiCheatClient_BeginSession",
    "EOS_AntiCheatClient_EndSession",
    "EOS_AntiCheatClient_ReceiveMessageFromServer",
    "EOS_AntiCheatClient_ProtectMessage",
    "EOS_AntiCheatClient_UnprotectMessage",
    "EOS_AntiCheatClient_GetProtectMessageOutputLength",
    "EOS_AntiCheatClient_AddNotifyMessageToServer",
    "EOS_AntiCheatClient_PollStatus",
    // `EAC_BLIND=1` only; installed (and counted) in every mode so a live run
    // can show what the game asked for even when suppression is off.
    "EOS_AntiCheatClient_AddNotifyClientIntegrityViolated",
    "EOS_AntiCheatClient_GetModuleBuildId",
    "EOS_AntiCheatClient_GetSystemRequirementStatus",
];

/// The exports [`Hooks`] patches, in slot order (`index` == [`Counters::calls`]).
///
/// Public so the Windows stub test can ask which of them the stub actually
/// provides instead of hard-coding a count.
pub fn export_names() -> &'static [&'static str] {
    &EXPORTS
}

pub(crate) const MAX_BINDS: usize = 4;
pub(crate) const MAX_EVENTS: usize = 4096;
/// Log lines printed in full before switching to every-Nth sampling.
pub(crate) const CALLS_FULL: u64 = 200;
/// `EACSHIM_DROP_AFTER_MS` fallback when the variable is present but unparsable.
///
/// Zero means "no cut". A non-numeric value used to fall back to 20 s, which
/// silently armed the cut whenever a stale `EACSHIM_DROP_AFTER_MS=cmd` entry
/// from the C shim was left in `~/.hromatite-mode` (observed 2026-11-17: the
/// payload dropped every outbound frame 20 s into each session and the game
/// churned BeginSession/EndSession every 24-30 s).
pub(crate) const DEFAULT_DROP_MS: u64 = 0;

// ---------------------------------------------------------------- state

/// Trampoline per hook slot; written once at install, read by every handler.
pub(crate) static TRAMPS: [AtomicUsize; HK_COUNT] = [const { AtomicUsize::new(0) }; HK_COUNT];
/// Original export address recovered from a caller's IAT slot.
///
/// The IAT walk records what used to sit in the slot, so a handler still reaches
/// the real function when the inline patch of the export entry never landed (or
/// landed after the game had already cached the pointer).
pub(crate) static IAT_ORIG: [AtomicUsize; HK_COUNT] = [const { AtomicUsize::new(0) }; HK_COUNT];
/// Slots the IAT walk patched, kept for a repeated [`install_iat`] call.
pub(crate) static IAT_SLOTS: AtomicUsize = AtomicUsize::new(0);
/// Set once the module walk ran, so a second caller does not re-walk or re-log.
pub(crate) static IAT_TRIED: AtomicBool = AtomicBool::new(false);
/// Calls seen per hook slot.
pub(crate) static CALLS: [AtomicU64; HK_COUNT] = [const { AtomicU64::new(0) }; HK_COUNT];
pub(crate) static OUT_MESSAGES: AtomicU64 = AtomicU64::new(0);
pub(crate) static IN_MESSAGES: AtomicU64 = AtomicU64::new(0);
pub(crate) static FORWARDED: AtomicU64 = AtomicU64::new(0);
pub(crate) static DROPPED: AtomicU64 = AtomicU64::new(0);
pub(crate) static REPLAYED: AtomicU64 = AtomicU64::new(0);
pub(crate) static PROTECT_IN: AtomicU64 = AtomicU64::new(0);
pub(crate) static PROTECT_OUT: AtomicU64 = AtomicU64::new(0);
pub(crate) static PROTECT_SAMPLES: AtomicU64 = AtomicU64::new(0);
pub(crate) static PROTECT_22: AtomicU64 = AtomicU64::new(0);
pub(crate) static OVERHEAD_MIN: AtomicI32 = AtomicI32::new(i32::MAX);
pub(crate) static OVERHEAD_MAX: AtomicI32 = AtomicI32::new(0);
pub(crate) static LAST_OUT_MS: AtomicU64 = AtomicU64::new(0);
pub(crate) static SESSION_START_MS: AtomicU64 = AtomicU64::new(0);
pub(crate) static DROP_AFTER_MS: AtomicU64 = AtomicU64::new(0);
pub(crate) static DROP_IN: AtomicBool = AtomicBool::new(false);
pub(crate) static REPLAY: AtomicBool = AtomicBool::new(false);
pub(crate) static LAST_MESSAGE_LOG: AtomicU64 = AtomicU64::new(0);

// ------------------------------------------------------------------- forge
//
// `EAC_FORGE=1`: the configuration is copied into atomics at install time (a
// handler must never read the environment), the template is the last live
// 512-byte frame and is kept under its own mutex so a forge tick never waits
// behind the capture sink.
pub(crate) static FORGE_AFTER_MS: AtomicU64 = AtomicU64::new(0);
pub(crate) static FORGE_MIN_FRAMES: AtomicU64 = AtomicU64::new(0);
pub(crate) static FORGE_INTERVAL_MS: AtomicU64 = AtomicU64::new(forge::INTERVAL_MS);
pub(crate) static FORGE_BUMP: AtomicBool = AtomicBool::new(false);
/// `EAC_FORGE_KEEP_SMALL`: forward live frames that are not the 512-byte
/// heartbeat (ACKs, handshake fragments) instead of replacing them too.
pub(crate) static FORGE_KEEP_SMALL: AtomicBool = AtomicBool::new(false);
/// `EAC_FORGE_TEMPLATE=start`: keep only a message-start frame as template.
pub(crate) static FORGE_PREFER_START: AtomicBool = AtomicBool::new(false);
/// Set once the forge has replaced the live stream; from then on the wrapper
/// drops live frames and the generator thread owns the cadence.
pub(crate) static FORGE_STARTED: AtomicBool = AtomicBool::new(false);
/// Live 512-byte frames learned in this epoch (the readiness counter).
pub(crate) static FORGE_SEEN: AtomicU64 = AtomicU64::new(0);
/// Frames the generator handed to the game callback.
pub(crate) static FORGE_EMITTED: AtomicU64 = AtomicU64::new(0);
/// Next `Sequence` the generator will put in a frame.
pub(crate) static FORGE_NEXT_SEQ: AtomicU64 = AtomicU64::new(0);
/// Timestamp of the previous emission (for the capture file's `dt_ms`).
pub(crate) static FORGE_LAST_MS: AtomicU64 = AtomicU64::new(0);
/// Set once so a double install cannot start two generators.
pub(crate) static FORGE_THREAD: AtomicBool = AtomicBool::new(false);

/// Epoch gate: the emitter may only send frames of the session the anti-cheat
/// module is *currently* in (see [`crate::epoch`]). Mutex because the decision
/// needs two observations (armed identity, live identity) to be read together;
/// hook threads use `try_lock` and the generator a blocking one.
pub(crate) static EPOCH: Mutex<epoch::Gate> = Mutex::new(epoch::Gate::new(epoch::DEFAULT_STALE_MS));
/// Rate limiter for "waiting for fresh evidence" lines, which can otherwise be
/// printed on every 50 ms generator tick.
pub(crate) static EPOCH_WAIT_MS: AtomicU64 = AtomicU64::new(0);

// ------------------------------------------------------------------- blind
//
// `EAC_BLIND=1` suppresses the anti-cheat's *local* verdicts: the game is never
// registered for an integrity violation, `PollStatus` reports "no violation",
// and the two read-only requirement queries answer with managed values. Every
// switch defaults to off, and every handler falls back to the real function
// when its arguments do not support the suppression (fail-closed).
pub(crate) static BLIND: AtomicBool = AtomicBool::new(false);
/// Fake `EOS_NotificationId` base; the low bits count registrations so a log
/// line can match a later `RemoveNotify` call.
pub(crate) const FAKE_NOTIFICATION_BASE: u64 = 0xB11D_0000_0000_0000;
/// `EOS_NotFound`: the documented answer of `PollStatus` when no violation
/// occurred since the previous call.
pub(crate) const EOS_NOT_FOUND: u64 = 18;
/// `EOS_ACCSRS_FullyMet`.
pub(crate) const EOS_ACCSRS_FULLY_MET: u32 = 2;
pub(crate) static BLIND_NOTIFY: AtomicU64 = AtomicU64::new(0);
pub(crate) static BLIND_NOTIFY_ID: AtomicU64 = AtomicU64::new(0);
pub(crate) static BLIND_POLL: AtomicU64 = AtomicU64::new(0);
pub(crate) static BLIND_BUILD: AtomicU64 = AtomicU64::new(0);
pub(crate) static BLIND_SYSREQ: AtomicU64 = AtomicU64::new(0);
/// `EAC_BLIND_BUILD_ID`: value reported instead of the real build id.
pub(crate) static BLIND_BUILD_ID: AtomicU32 = AtomicU32::new(0);
pub(crate) static BLIND_BUILD_SET: AtomicBool = AtomicBool::new(false);
/// `EAC_BLIND_SYSREQ`: value reported by `GetSystemRequirementStatus`.
pub(crate) static BLIND_SYSREQ_VALUE: AtomicU32 = AtomicU32::new(EOS_ACCSRS_FULLY_MET);
/// The learned template *and* the body mode it must be emitted with. Both
/// live under the same lock because they belong together: the image that
/// learns the frame also decides how its body is refreshed, and the counter
/// must advance across emissions, not restart from the learned value.
pub(crate) static FORGE_TEMPLATE: Mutex<Option<Learned>> = Mutex::new(None);
pub(crate) static FORGE_CAPTURE: Mutex<Option<CaptureFile>> = Mutex::new(None);

// ------------------------------------------------------------------ report
//
// `EAC_REPLAY_REPORT=<path>`: the same generator thread, but the frame it emits
// comes from a recorded report instead of the template. The report and its
// cursor live under their own mutex, so a slow reader never blocks the SDK
// thread's template learning.
/// True when a recorded report is being replayed instead of building frames.
pub(crate) static REPORT_ENABLED: AtomicBool = AtomicBool::new(false);
/// `EAC_REPLAY_FROM_WRAPPER=1`: deliver from the SDK callback thread instead
/// of a generator thread (the latter deadlocks the game's message pump).
pub(crate) static REPORT_FROM_WRAPPER: AtomicBool = AtomicBool::new(false);
/// `EAC_REPLAY_SELF=1`: at takeover, replay the *current* session's own stream
/// from its last message start, taken out of the live capture file. This is the
/// ASLR hypothesis: if the 16-byte records hash memory addresses, only frames
/// of the same process can validate.
pub(crate) static REPORT_SELF: AtomicBool = AtomicBool::new(false);
/// Capture path of the running epoch, so `EAC_REPLAY_SELF` can read it back.
pub(crate) static CAPTURE_PATH: Mutex<Option<PathBuf>> = Mutex::new(None);
/// In-memory copy of the live 512-byte frames of this epoch.
///
/// `EAC_REPLAY_SELF` cannot read the capture file back: the writer holds it open
/// under Wine and a second read only ever saw one frame (`replayself_0403`).
/// Keeping the last `SELF_RING_MAX` frames in memory costs ~250 KiB and is the
/// same data the stream needs.
pub(crate) static SELF_RING: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
/// Last time the "waiting for a complete message" line was printed.
///
/// [`forge_begin`] retries every 50 ms, so an unthrottled line would bury the
/// log while the module is still producing its first report.
pub(crate) static SELF_WAIT_MS: AtomicU64 = AtomicU64::new(0);
/// How many live frames the ring keeps (≈ 17 minutes at 2007 ms).
pub(crate) const SELF_RING_MAX: usize = 512;
/// `EAC_REPLAY_FLAGS=live`: `Flags` come from the live template, not the file.
pub(crate) static REPORT_FLAGS_LIVE: AtomicBool = AtomicBool::new(false);
/// The report being replayed, or `None` outside `EAC_REPLAY_REPORT` runs.
pub(crate) static REPORT_STREAM: Mutex<Option<report::Stream>> = Mutex::new(None);
/// Set once the report ran out (`EAC_REPLAY_REPORT_LOOP` off), so the
/// exhaustion line reaches the log exactly once per epoch.
pub(crate) static REPORT_STOPPED: AtomicBool = AtomicBool::new(false);

/// One `(ClientData, callback)` pair registered by the game.
pub(crate) struct Bind {
    pub(crate) client_data: AtomicUsize,
    pub(crate) function: AtomicUsize,
}

impl Bind {
    const fn new() -> Self {
        Self {
            client_data: AtomicUsize::new(0),
            function: AtomicUsize::new(0),
        }
    }
}

pub(crate) static BINDS: [Bind; MAX_BINDS] = [const { Bind::new() }; MAX_BINDS];
pub(crate) static BIND_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Capture + hook log, created at install time.
pub(crate) struct Sink {
    pub(crate) capture: Option<CaptureFile>,
    pub(crate) log: Option<LogFile>,
    pub(crate) replay: Vec<Frame>,
    pub(crate) replay_cursor: usize,
    pub(crate) replay_base_ms: u64,
    pub(crate) replay_due_ms: u64,
}

impl Sink {
    pub(crate) fn log_line(&mut self, line: &str) {
        if let Some(log) = self.log.as_mut() {
            log.write_line(line);
        }
    }
}

pub(crate) static SINK: Mutex<Option<Sink>> = Mutex::new(None);

/// Serializes `reset_state` with the tests that depend on the global counters:
/// the hook state is process-global and the harness runs tests in parallel.
pub(crate) static STATE_LOCK: Mutex<()> = Mutex::new(());

/// Bounded observation queue drained by the managed tick.
pub(crate) static EVENTS: Mutex<VecDeque<HookEvent>> = Mutex::new(VecDeque::new());

/// Hook log with 32 MiB rotation (`*.1` archive).
pub(crate) struct LogFile {
    path: PathBuf,
    file: File,
    bytes: u64,
}

impl LogFile {
    pub(crate) fn open(path: PathBuf) -> Option<Self> {
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
pub(crate) fn hook_log_blocking(line: &str) {
    if let Ok(mut sink) = SINK.lock()
        && let Some(sink) = sink.as_mut()
    {
        sink.log_line(line);
    }
}

pub(crate) fn push_event(event: HookEvent) {
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
    /// Integrity-violation registrations refused (`EAC_BLIND`).
    pub blind_notify: u64,
    /// `PollStatus` calls answered with "no violation".
    pub blind_poll: u64,
    /// `GetModuleBuildId` answers overridden.
    pub blind_build: u64,
    /// `GetSystemRequirementStatus` answers overridden.
    pub blind_sysreq: u64,
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
            blind_notify: BLIND_NOTIFY.load(Ordering::Relaxed),
            blind_poll: BLIND_POLL.load(Ordering::Relaxed),
            blind_build: BLIND_BUILD.load(Ordering::Relaxed),
            blind_sysreq: BLIND_SYSREQ.load(Ordering::Relaxed),
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
        #[cfg(feature = "runtime")]
        lines.push(hromatite_hook::iat::status());
        lines.push(epoch_line());
        lines.push(format!(
            "eac.blind: armed={} notify_suppressed={} poll_no_violation={} build_id={} sysreq={} \
             (build_override={} sysreq_value={})",
            BLIND.load(Ordering::Relaxed),
            counters.blind_notify,
            counters.blind_poll,
            counters.blind_build,
            counters.blind_sysreq,
            if BLIND_BUILD_SET.load(Ordering::Relaxed) {
                BLIND_BUILD_ID.load(Ordering::Relaxed).to_string()
            } else {
                "passthrough".to_string()
            },
            BLIND_SYSREQ_VALUE.load(Ordering::Relaxed),
        ));
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
        // The EVP counters belong to the runtime half; without it there is no
        // probe to report.
        #[cfg(feature = "runtime")]
        if let Some(line) = super::hooks::evp_status_line() {
            lines.push(line);
        }
        lines
    }
}

// ---------------------------------------------------------------- epoch gate

/// Gate access that never blocks a caller.
///
/// The gate is touched from two threads that must never wait on each other: the
/// SDK thread (inside `BeginSession` and the message wrapper) and the emitter.
/// A panic-longjmp does not run `Drop`, so a leaked guard could not be recovered
/// — the policy is therefore "skip the observation, keep the game". A missed
/// observation costs one cadence; a missed stop is caught on the next slot.
pub(super) fn epoch_try<R>(f: impl FnOnce(&mut epoch::Gate) -> R) -> Option<R> {
    EPOCH.try_lock().ok().map(|mut gate| f(&mut gate))
}

/// Blocking lock for tests only.
///
/// Production callers never wait on the gate: it is touched from the SDK thread
/// (inside `BeginSession` and the message wrapper) and from the emitter, and a
/// panic-longjmp does not run `Drop`, so a leaked guard would hang the game.
#[cfg(test)]
pub(super) fn epoch_lock() -> std::sync::MutexGuard<'static, epoch::Gate> {
    EPOCH.lock().unwrap_or_else(|error| error.into_inner())
}

/// One `eac.hooks` / `eac.status` line.
pub(super) fn epoch_line() -> String {
    epoch_try(|gate| gate.line()).unwrap_or_else(|| "gate busy".to_string())
}

/// No OS hook path in this build: either the `runtime` feature is off (the pure
/// core links neither `hromatite-win` nor `hromatite-hook`), or the target is
/// the host. There is nothing to patch and nothing to probe, so the install
/// step says so instead of not compiling.
#[cfg(not(all(windows, feature = "runtime")))]
impl Hooks {
    fn install(&mut self) -> Result<Vec<Report>, String> {
        Err(NO_RUNTIME_REASON.to_string())
    }
}

/// Why [`Hooks::install`] cannot do anything in a build without the OS hook
/// path; `Hooks::poll` surfaces it verbatim.
#[cfg(not(all(windows, feature = "runtime")))]
pub const NO_RUNTIME_REASON: &str = "no OS hook path (needs the `runtime` feature and \
     the x86_64-pc-windows-gnu target)";

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract `hromatite-features` links with `default-features = false`:
    /// the observation API exists and its counters/status lines work with no OS
    /// layer behind them.
    ///
    /// The event queue itself is process-global and other tests push into it, so
    /// this only names the functions (rather than racing a `drain` against them)
    /// — that is exactly the surface a feature image calls.
    #[test]
    fn the_observation_contract_works_without_the_os_layer() {
        let _observe: fn(HookEvent) = Hooks::observe;
        let _drain: fn(&Hooks) -> Vec<HookEvent> = Hooks::drain_events;
        let hooks = Hooks::new();
        let counters = hooks.counters();
        assert_eq!(
            counters.calls.len(),
            EXPORTS.len(),
            "one call counter per export slot"
        );
        assert_eq!(
            hooks.reports().len(),
            0,
            "nothing installed, nothing to report"
        );
        assert!(!hooks.installed());
        let lines = hooks.status_lines();
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("eac.hooks: installed=false")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.starts_with("eac.blind:")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.starts_with("eac.hooks: out=")),
            "{lines:?}"
        );
    }

    /// Without the OS hook path there is no detour to install, and the answer
    /// says so instead of failing to compile.
    #[cfg(not(all(windows, feature = "runtime")))]
    #[test]
    fn install_without_the_os_layer_reports_why() {
        let mut hooks = Hooks::new();
        let error = hooks.install().expect_err("no OS hook path to patch");
        assert_eq!(error, NO_RUNTIME_REASON);
        assert!(error.contains("x86_64-pc-windows-gnu"), "{error}");
    }
}
