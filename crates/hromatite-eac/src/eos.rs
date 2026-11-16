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
/// `EACSHIM_DROP_AFTER_MS` default when the variable is present but unparsable.
const DEFAULT_DROP_MS: u64 = 20_000;

// ---------------------------------------------------------------- state

/// Trampoline per hook slot; written once at install, read by every handler.
static TRAMPS: [AtomicUsize; HK_COUNT] = [const { AtomicUsize::new(0) }; HK_COUNT];
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
fn hook_log(line: impl AsRef<str>) {
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
    last_probe_ms: u64,
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
            last_probe_ms: 0,
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

    /// Lazy, idempotent install. Called from the managed tick; probes the module
    /// at most every two seconds and never panics when EOSSDK is absent.
    ///
    /// Returns a console line the first time state changes (installed, or first
    /// "module absent" observation), `None` otherwise.
    pub fn poll(&mut self, now: u64) -> Option<String> {
        if !self.enabled || self.installed {
            return None;
        }
        if self.last_probe_ms != 0 && now.saturating_sub(self.last_probe_ms) < 2_000 {
            return None;
        }
        self.last_probe_ms = now;
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
        let mode = if REPLAY.load(Ordering::Relaxed) {
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
        lines.push(format!(
            "eac.hooks: out={} in={} forwarded={} dropped={} replayed={} begins={} ends={} \
             addnotify={} protect={} overhead22={}/{} min={} max={}",
            counters.out_messages,
            counters.in_messages,
            counters.forwarded,
            counters.dropped,
            counters.replayed,
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

        let drop_after = std::env::var("EACSHIM_DROP_AFTER_MS")
            .ok()
            .map(|value| value.trim().parse::<u64>().unwrap_or(DEFAULT_DROP_MS))
            .unwrap_or(0);
        DROP_AFTER_MS.store(drop_after, Ordering::Relaxed);
        DROP_IN.store(env_flag("EACSHIM_DROP_IN"), Ordering::Relaxed);

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

        let handlers: [usize; HK_COUNT] = [
            hook_begin as *const () as usize,
            hook_end as *const () as usize,
            hook_receive as *const () as usize,
            hook_protect as *const () as usize,
            hook_unprotect as *const () as usize,
            hook_outlen as *const () as usize,
            hook_add_notify as *const () as usize,
            hook_poll_status as *const () as usize,
        ];

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
                                    format!("prologue={} copied", installed.copied)
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

fn reset_state() {
    for tramp in &TRAMPS {
        tramp.store(0, Ordering::Relaxed);
    }
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
        pub _version: [u32; 2],
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
    let tramp = TRAMPS[index].load(Ordering::Acquire);
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
}
