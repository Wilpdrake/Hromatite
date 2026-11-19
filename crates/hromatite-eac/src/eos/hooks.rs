//! The OS half of [`crate::eos`]: the twelve `EOS_AntiCheatClient_*` detours,
//! their install path, the IAT walk and the BoringSSL EVP probe.
//!
//! Compiled only with the `runtime` feature (the crate's default). The pure
//! types, the counters and the status lines they publish live in
//! [`super::state`], which every build compiles.

#![cfg_attr(not(windows), allow(dead_code, unused_imports))]

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crate::capture::{CaptureFile, hex_prefix, read_frames};
use crate::clock::now_ms;
use crate::epoch;
use crate::forge::{self, BodyMode, Template};
use crate::hook;
use crate::report;

use super::*;

/// Install every export hook ([`Hooks::poll`] drives this until it lands).
#[cfg(windows)]
impl Hooks {
    pub(crate) fn install(&mut self) -> Result<Vec<Report>, String> {
        let Some(module) = hromatite_win::sys::module_handle(EOSSDK) else {
            return Err(format!("{EOSSDK} is not loaded"));
        };
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

        // `EAC_BLIND=1`: local-verdict suppression. Off by default, and parsed
        // here (never in a handler: a handler must not read the environment).
        let blind = env_flag("EAC_BLIND");
        BLIND.store(blind, Ordering::Relaxed);
        BLIND_BUILD_SET.store(false, Ordering::Relaxed);
        BLIND_BUILD_ID.store(0, Ordering::Relaxed);
        BLIND_SYSREQ_VALUE.store(EOS_ACCSRS_FULLY_MET, Ordering::Relaxed);
        if blind {
            let build_id = std::env::var("EAC_BLIND_BUILD_ID")
                .ok()
                .and_then(|value| parse_u32(&value));
            if let Some(build_id) = build_id {
                BLIND_BUILD_ID.store(build_id, Ordering::Relaxed);
                BLIND_BUILD_SET.store(true, Ordering::Relaxed);
            }
            let sysreq = std::env::var("EAC_BLIND_SYSREQ")
                .ok()
                .and_then(|value| value.trim().parse::<u32>().ok())
                .filter(|value| *value <= EOS_ACCSRS_FULLY_MET)
                .unwrap_or(EOS_ACCSRS_FULLY_MET);
            BLIND_SYSREQ_VALUE.store(sysreq, Ordering::Relaxed);
            hook_log_blocking(&format!(
                "[blind] armed: integrity_callback=suppressed poll_status=not_found \
                 build_id={} sysreq={sysreq}",
                if BLIND_BUILD_SET.load(Ordering::Relaxed) {
                    build_id.unwrap_or(0).to_string()
                } else {
                    "passthrough".to_string()
                }
            ));
        }

        // Epoch gate budget: four live cadences, never below the model minimum.
        // `EAC_FORGE_STALE_MS` overrides it (tests and short experiments).
        let interval = FORGE_INTERVAL_MS.load(Ordering::Relaxed);
        let stale = std::env::var("EAC_FORGE_STALE_MS")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or_else(|| (interval.saturating_mul(4)).max(epoch::DEFAULT_STALE_MS));
        epoch_try(|gate| gate.set_stale_ms(stale));
        hook_log_blocking(&format!(
            "[epoch] gate armed stale_ms={stale} interval_ms={interval}"
        ));

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
            let target = hromatite_win::pe::resolve_live_export(module, name);
            match target {
                Some(target) => {
                    // SAFETY: `target` is the entry of an exported function and
                    // `handlers[index]` an `extern "system"` detour of matching
                    // arity for the slot.
                    match unsafe { hook::install(target, handlers[index], name) } {
                        Ok(installed) => {
                            if installed.mode == 1 {
                                hook_log(format!("[hook] {name}: {}", installed.describe()));
                            }
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

        // `EAC_KEYHUNT_EVP=1`: also probe the statically linked BoringSSL
        // entry points the RE located. Mode 1 is expected to leave them cold,
        // so this is a proof-of-absence as much as a key source.
        if env_flag("EAC_KEYHUNT_EVP") {
            let before = reports.len();
            install_evp(module, &mut reports);
            let ok = reports[before..].iter().filter(|report| report.ok).count();
            hook_log_blocking(&format!("[evp] installed {ok}/{}", EVP_SLOTS));
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
}

// ------------------------------------------------- BoringSSL EVP key probe

/// One EOSSDK entry point worth hooking for key material, addressed by the RE
/// offset rather than by a symbol: EOSSDK's BoringSSL is statically linked, so
/// nothing in the export table names it.
struct EvpTarget {
    name: &'static str,
    /// Offset from the image base (`0x180000000` in the RE listing).
    rva: usize,
    /// Exact prologue from the RE listing: a drift disables the hook instead of
    /// patching the middle of a function.
    signature: &'static [u8],
    kind: EvpKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EvpKind {
    /// `EVP_CIPHER_CTX_new` — count only, there is no key to read yet.
    CtxNew,
    /// `EVP_*Init_ex`-shaped: arg4 is the key pointer, arg5 the IV pointer.
    Init,
    /// Unknown arity: dump arg4 when it looks like a readable pointer.
    Generic,
}

/// The five addresses the RE identified in the protector's crypto path
/// (`findings/eossdk.md` §6). VRChat runs AC mode 1, where these are *expected*
/// to stay silent — the hook exists to prove or disprove that live.
const EVP_TARGETS: [EvpTarget; 5] = [
    EvpTarget {
        name: "evp@cdb040(ctx_new)",
        rva: 0xcdb040,
        signature: &[
            0xb8, 0x28, 0x00, 0x00, 0x00, 0xe8, 0xe6, 0xff, 0x18, 0x00, 0x48, 0x2b, 0xe0,
        ],
        kind: EvpKind::CtxNew,
    },
    EvpTarget {
        name: "evp@cdbcb0(cipher_init)",
        rva: 0xcdbcb0,
        signature: &[
            0xb8, 0x38, 0x00, 0x00, 0x00, 0xe8, 0x76, 0xf3, 0x18, 0x00, 0x48, 0x2b, 0xe0,
        ],
        kind: EvpKind::Init,
    },
    EvpTarget {
        name: "evp@cdbce0",
        rva: 0xcdbce0,
        signature: &[
            0xb8, 0x38, 0x00, 0x00, 0x00, 0xe8, 0x46, 0xf3, 0x18, 0x00, 0x48, 0x2b, 0xe0, 0x83,
            0x79, 0x10, 0x00,
        ],
        kind: EvpKind::Generic,
    },
    EvpTarget {
        name: "evp@cdbb60",
        rva: 0xcdbb60,
        signature: &[
            0x48, 0x89, 0x5c, 0x24, 0x18, 0x48, 0x89, 0x6c, 0x24, 0x20, 0x41, 0x56, 0xb8,
        ],
        kind: EvpKind::Generic,
    },
    EvpTarget {
        name: "evp@cdaf90",
        rva: 0xcdaf90,
        signature: &[
            0xb8, 0x38, 0x00, 0x00, 0x00, 0xe8, 0x96, 0x00, 0x19, 0x00, 0x48, 0x2b, 0xe0,
        ],
        kind: EvpKind::Generic,
    },
];

const EVP_SLOTS: usize = EVP_TARGETS.len();
static EVP_TRAMPS: [AtomicUsize; EVP_SLOTS] = [const { AtomicUsize::new(0) }; EVP_SLOTS];
static EVP_CALLS: [AtomicU64; EVP_SLOTS] = [const { AtomicU64::new(0) }; EVP_SLOTS];

/// Log `key`/`iv` (init-shaped hooks) or a small buffer dump (generic ones).
#[cfg(windows)]
fn log_evp(slot: usize, calls: u64, a1: u64, a2: u64, a4: u64, a5: u64, a6: u64) {
    use hromatite_win::memory as safe;
    let target = &EVP_TARGETS[slot];
    let mut line = format!(
        "[evp] {} #{calls} arg1={a1:#x} arg2={a2:#x} arg4={a4:#x} arg5={a5:#x} arg6={a6:#x}",
        target.name
    );
    match target.kind {
        EvpKind::CtxNew => {}
        EvpKind::Init => {
            if a4 != 0
                && let Some(key) = safe::read_bytes(a4 as usize, 32)
            {
                line.push_str(&format!(" key32={}", hex_prefix(&key, 32)));
            }
            if a5 != 0
                && let Some(iv) = safe::read_bytes(a5 as usize, 16)
            {
                line.push_str(&format!(
                    " iv12={} iv16={}",
                    hex_prefix(&iv[..12], 12),
                    hex_prefix(&iv, 16)
                ));
            }
        }
        EvpKind::Generic => {
            if a4 != 0
                && let Some(bytes) = safe::read_bytes(a4 as usize, 32)
            {
                line.push_str(&format!(" data32={}", hex_prefix(&bytes, 32)));
            }
            if a5 < 0x1_0000 {
                line.push_str(&format!(" arg5_u32={a5:#x}"));
            }
        }
    }
    hook_log(line);
}

macro_rules! evp_detour {
    ($name:ident, $slot:expr) => {
        /// Generic six-slot detour: log, then tail-call the trampoline.
        ///
        /// # Safety
        ///
        /// Called by the patched entry with the original argument registers.
        #[cfg(windows)]
        unsafe extern "system" fn $name(
            a1: u64,
            a2: u64,
            a3: u64,
            a4: u64,
            a5: u64,
            a6: u64,
        ) -> u64 {
            let calls = EVP_CALLS[$slot].fetch_add(1, Ordering::Relaxed) + 1;
            if calls <= 64 || calls.is_multiple_of(100) {
                let _ = std::panic::catch_unwind(|| {
                    log_evp($slot, calls, a1, a2, a4, a5, a6);
                });
            }
            let tramp = EVP_TRAMPS[$slot].load(Ordering::Acquire);
            if tramp == 0 {
                return 0;
            }
            // SAFETY: the trampoline is the relocated prologue of this very
            // function, ending in an absolute jump back into it.
            let function: Fn6 = unsafe { core::mem::transmute(tramp) };
            // SAFETY: same ABI and argument registers as the interrupted call.
            unsafe { function(a1, a2, a3, a4, a5, a6) }
        }
    };
}

evp_detour!(evp_slot0, 0);
evp_detour!(evp_slot1, 1);
evp_detour!(evp_slot2, 2);
evp_detour!(evp_slot3, 3);
evp_detour!(evp_slot4, 4);

/// Install the EVP probes. Every target must match its recorded prologue.
#[cfg(windows)]
fn install_evp(module: usize, reports: &mut Vec<Report>) {
    use hromatite_win::memory as safe;
    let handlers: [usize; EVP_SLOTS] = [
        evp_slot0 as *const () as usize,
        evp_slot1 as *const () as usize,
        evp_slot2 as *const () as usize,
        evp_slot3 as *const () as usize,
        evp_slot4 as *const () as usize,
    ];
    for (index, target) in EVP_TARGETS.iter().enumerate() {
        let address = module + target.rva;
        let matches = safe::read_bytes(address, target.signature.len())
            .is_some_and(|seen| seen.as_slice() == target.signature);
        if !matches {
            reports.push(Report {
                name: target.name,
                ok: false,
                mode: 0,
                detail: "prologue mismatch".to_string(),
                tramp: 0,
            });
            continue;
        }
        // SAFETY: `address` is a function entry whose prologue matches the RE
        // listing, and the handler has the same six-integer ABI.
        match unsafe { hook::install(address, handlers[index], target.name) } {
            Ok(installed) => {
                if installed.mode == 1 {
                    hook_log(format!("[hook] {}: {}", target.name, installed.describe()));
                }
                EVP_TRAMPS[index].store(installed.tramp, Ordering::Release);
                reports.push(Report {
                    name: target.name,
                    ok: true,
                    mode: installed.mode,
                    detail: format!("prologue={} copied", installed.copied),
                    tramp: installed.tramp,
                });
            }
            Err(reason) => reports.push(Report {
                name: target.name,
                ok: false,
                mode: 0,
                detail: reason,
                tramp: 0,
            }),
        }
    }
}

fn default_dir() -> PathBuf {
    std::env::temp_dir()
}

/// `EAC_KEYHUNT_EVP=1`: the per-target call counts of the BoringSSL probe.
///
/// `Hooks::status_lines` lives in the pure [`super::state`] half, so the EVP
/// counters (this module owns them) are handed back as one already-formatted
/// line instead of being read across the split.
pub(super) fn evp_status_line() -> Option<String> {
    if !env_flag("EAC_KEYHUNT_EVP") {
        return None;
    }
    let counts = EVP_CALLS
        .iter()
        .enumerate()
        .map(|(index, calls)| {
            format!(
                "{}={}",
                EVP_TARGETS[index].name,
                calls.load(Ordering::Relaxed)
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    Some(format!("eac.hooks: evp {counts}"))
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|value| !value.is_empty() && value != "0")
        .unwrap_or(false)
}

/// `42`, `0x2a` → `Some(42)`; anything else → `None` (fail-closed).
fn parse_u32(value: &str) -> Option<u32> {
    let value = value.trim();
    match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => value.parse::<u32>().ok(),
    }
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
        hook_add_integrity as *const () as usize,
        hook_build_id as *const () as usize,
        hook_sysreq as *const () as usize,
    ]
}

/// Emit one IAT line where a live run can find it: the hook log (next to the
/// frame evidence) and `hromatite.log`.
fn iat_line(line: &str) {
    hook_log_blocking(line);
    hromatite_win::log::log_tag("iat", line);
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

/// `eac.module [pages] [read=<bytes>] [imports=<n>]`: locate the in-process
/// ELF image of the anti-cheat client module and describe it.
///
/// The module is a memfd ELF, not a PE, so this is the only way to see it:
/// [`crate::eacimage`] walks committed regions, probes one magic per region and
/// parses the header / program headers / `.dynsym` of a match, with a hard byte
/// budget. Read-only; the lines are logged and returned for the console.
pub fn module_report(arg: &str) -> Vec<String> {
    let lines = crate::eacimage::command(arg);
    for line in &lines {
        hook_log_blocking(line);
    }
    lines
}

/// Windows body of [`install_iat`].
#[cfg(windows)]
fn iat_install() -> usize {
    use hromatite_hook::iat;
    let handlers = handlers();
    // The three game modules first, then every other module of the process: an
    // EOS plugin can live in a module we have never heard of, and the walk is
    // cheap (one header read per module).
    let mut names: Vec<String> = vec![
        hromatite_win::GAME_EXE.to_string(),
        hromatite_win::GAME_ASSEMBLY.to_string(),
        hromatite_win::UNITY_PLAYER.to_string(),
    ];
    for (name, _, _) in hromatite_win::procs::modules(std::process::id()) {
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
    BLIND_NOTIFY.store(0, Ordering::Relaxed);
    BLIND_NOTIFY_ID.store(0, Ordering::Relaxed);
    BLIND_POLL.store(0, Ordering::Relaxed);
    BLIND_BUILD.store(0, Ordering::Relaxed);
    BLIND_SYSREQ.store(0, Ordering::Relaxed);
    // A fresh install is a fresh epoch: nothing armed, nothing learned.
    epoch_reset();
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

        // `EAC_KEYHUNT=1`: remember where the module keeps a big frame so the
        // managed tick can probe (and scan around) that buffer. Cheap: one
        // `try_lock` and a 16-byte copy, and only until the first frame lands.
        crate::keyhunt::record_frame(message.data as usize, length as usize, data, ts);

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
                forge_learn(data, length as usize, ts);
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
                if heartbeat && let Some(sid) = forge::frame_session_id(data) {
                    // The takeover drops the live heartbeat but must keep
                    // watching *which session* it belongs to: this is the only
                    // evidence the epoch gate has that the stream is still ours.
                    epoch_note_live(sid, ts);
                }
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
                forge_learn(data, length as usize, ts);
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

// -------------------------------------------------------------- epoch gate
//
// See [`crate::epoch`] for why the invariant cannot rely on a reset *event*: the
// payload exists twice in the process (resident image + feature image), and each
// copy owns its own statics. The emitter therefore asks the live stream, every
// time it is about to send, whether the session it learned is still the one the
// anti-cheat module is in.

/// Record a live 512-byte frame. Called from the SDK thread on every heartbeat,
/// so it must not block: a skipped observation costs one cadence, not a decision.
fn epoch_note_live(sid: u64, now_ms: u64) {
    epoch_try(|gate| gate.note_live(sid, now_ms));
}

/// `BeginSession`: the previous session, its template and its evidence are gone.
///
/// Also non-blocking: it runs on the SDK thread, and the gate's own verdict stops
/// a dead session on the next slot even if this reset is skipped.
fn epoch_reset() {
    epoch_try(epoch::Gate::reset);
}

/// Try to take over `sid` at `now_ms`; refused unless the module is still there.
fn epoch_arm(sid: u64, now_ms: u64) -> bool {
    let armed = epoch_try(|gate| gate.arm(sid, now_ms)).unwrap_or(false);
    if !armed {
        let waited = now_ms.saturating_sub(EPOCH_WAIT_MS.load(Ordering::Relaxed));
        if waited >= 5_000 {
            EPOCH_WAIT_MS.store(now_ms, Ordering::Relaxed);
            let line = epoch_line();
            hook_log(format!(
                "[epoch] arm refused sid=0x{sid:x} — no fresh live frame of it; {line}"
            ));
        }
    }
    armed
}

/// True when a frame may be emitted at `now_ms`.
///
/// A `Stop` is latched and reported once, and the caller must also forget the
/// learned template: a re-arm has to come from the *new* session's frames.
fn epoch_allows(now_ms: u64) -> bool {
    // `Ok(true)` = emit, `Ok(false)` = stay idle, `Err(stop)` = stop for good.
    let verdict = epoch_try(|gate| match gate.verdict(now_ms) {
        epoch::Verdict::Emit => Ok(true),
        epoch::Verdict::Idle => Ok(false),
        epoch::Verdict::Stop(stop) => {
            if gate.stopped().is_some() {
                return Ok(false);
            }
            gate.latched(epoch::Verdict::Stop(stop));
            Err(stop)
        }
    });
    match verdict {
        Some(Ok(allowed)) => allowed,
        Some(Err(stop)) => {
            // Logging and forgetting happen with the lock released: neither is
            // safe to do while another thread may be waiting for the gate.
            hook_log_blocking(&format!(
                "[epoch] stop {} emitted={} — no frame of a dead epoch is sent",
                stop.line(),
                FORGE_EMITTED.load(Ordering::Relaxed)
            ));
            forge_forget();
            false
        }
        // Gate busy: skip this slot rather than block the caller. The next slot
        // asks again, and a dead session keeps being refused until then.
        None => false,
    }
}

/// Forget the learned session (template, sequence seed, readiness counters).
///
/// Shared by `BeginSession` and by a gate stop, so both leave the emitter in the
/// same state: "nothing learned, wait for a live frame of the current session".
fn forge_forget() {
    FORGE_STARTED.store(false, Ordering::Release);
    FORGE_SEEN.store(0, Ordering::Relaxed);
    FORGE_NEXT_SEQ.store(0, Ordering::Relaxed);
    if let Ok(mut slot) = FORGE_TEMPLATE.try_lock() {
        *slot = None;
    }
}

// ------------------------------------------------------------------- forge

/// Remember a live 512-byte frame as the template for the current epoch.
///
/// Called from the SDK thread, so it uses `try_lock`: a skipped learning step
/// only costs one frame, the next one arrives in ~2 s anyway.
fn forge_learn(data: &[u8], length: usize, now_ms: u64) {
    if length != forge::FRAME_LEN {
        return;
    }
    let Some(template) = Template::from_frame(data) else {
        return;
    };
    // Even when this frame is not stored as the template (a middle fragment in
    // `EAC_FORGE_TEMPLATE=start`), it still proves which session is live.
    epoch_note_live(template.session_id, now_ms);
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
    // The gate refuses a takeover of a session the module is no longer in. The
    // template may be seconds old by now (`EAC_FORGE_AFTER_MS`), and taking it
    // over after the session changed is exactly what produced the second kick.
    if !epoch_arm(template.session_id, now) {
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
    // The gate goes first: an epoch change must be able to cancel an emission
    // that is already in flight on another thread.
    epoch_reset();
    forge_forget();
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
                // The epoch gate is asked on every slot, not only at the
                // takeover: a session can die while the generator runs, and the
                // image that owns the emitter may never see that `BeginSession`.
                if !epoch_allows(now) {
                    due = 0;
                    continue;
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
    // Defence in depth: the wrapper can reach this directly when the report
    // stream is driven from the SDK callback, so the gate is asked here too.
    if !epoch_allows(now) {
        return;
    }
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
    // The wrapper drives this directly when `EAC_REPLAY_FROM_WRAPPER=1`, so it
    // carries the same epoch check as the template forge.
    if !epoch_allows(now) {
        return;
    }
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
    // `EAC_BLIND=1`: answer "no violation occurred since the last call", which
    // is the SDK's own wording for `EOS_NotFound` (`OutViolationType` is not
    // touched on that result, so writing `Invalid` is belt and braces).
    if BLIND.load(Ordering::Relaxed) {
        let suppressed = BLIND_POLL.fetch_add(1, Ordering::Relaxed) + 1;
        if a3 != 0 {
            let _ = hromatite_win::memory::write_bytes(a3 as usize, &0u32.to_le_bytes());
        }
        if suppressed <= 5 || suppressed.is_multiple_of(200) {
            hook_log(format!(
                "[blind] PollStatus #{calls} -> EOS_NotFound (no violation) suppressed={suppressed}"
            ));
        }
        return EOS_NOT_FOUND;
    }
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

/// `EOS_AntiCheatClient_AddNotifyClientIntegrityViolated`: the one export that
/// decides whether the game ever hears about a local violation.
///
/// With `EAC_BLIND=1` the game's callback is **never handed to the SDK**, so the
/// registration cannot fire `OnClientIntegrityViolated` even if the module keeps
/// finding violations internally. The call still looks successful: a synthetic
/// (non-zero, never-issued) `EOS_NotificationId` is returned, and a later
/// `RemoveNotifyClientIntegrityViolated` for it is answered by the real SDK with
/// `EOS_NotFound`, which the game has to tolerate anyway.
///
/// Fail-closed: blind off, or a null callback, forwards the untouched arguments.
#[cfg(windows)]
unsafe extern "system" fn hook_add_integrity(
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
    a6: u64,
) -> u64 {
    let calls = CALLS[HK_INTEGRITY].fetch_add(1, Ordering::Relaxed) + 1;
    if !BLIND.load(Ordering::Relaxed) || a4 == 0 {
        // SAFETY: untouched argument registers.
        let result = unsafe { call_tramp(HK_INTEGRITY, a1, a2, a3, a4, a5, a6) };
        if verbose(calls) {
            hook_log(format!(
                "[call] AddNotifyClientIntegrityViolated #{calls} blind={} game_fn={a4:#x} \
                 -> notification={result:#x}",
                BLIND.load(Ordering::Relaxed)
            ));
        }
        return result;
    }
    // A panic must not cross into the SDK: forward instead.
    match std::panic::catch_unwind(|| {
        let sequence = BLIND_NOTIFY.fetch_add(1, Ordering::Relaxed) + 1;
        let id = FAKE_NOTIFICATION_BASE | sequence;
        BLIND_NOTIFY_ID.store(id, Ordering::Relaxed);
        if sequence <= 5 || sequence.is_multiple_of(50) {
            hook_log(format!(
                "[blind] client-integrity callback NOT registered #{calls} handle={a1:#x} \
                 clientData={a3:#x} game_fn={a4:#x} -> notification={id:#x}"
            ));
        }
        id
    }) {
        Ok(id) => id,
        Err(_) => {
            // SAFETY: untouched argument registers.
            unsafe { call_tramp(HK_INTEGRITY, a1, a2, a3, a4, a5, a6) }
        }
    }
}

/// `EOS_AntiCheatClient_GetModuleBuildId`: read-only analytics query.
///
/// The real function always runs first, so the log carries the true build id;
/// with `EAC_BLIND_BUILD_ID=<n>` the caller then receives that managed value
/// instead. Without the override nothing is rewritten (a fabricated build id has
/// no protective value and would only corrupt the game's telemetry).
#[cfg(windows)]
unsafe extern "system" fn hook_build_id(
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
    a6: u64,
) -> u64 {
    let calls = CALLS[HK_BUILD_ID].fetch_add(1, Ordering::Relaxed) + 1;
    // SAFETY: untouched argument registers.
    let result = unsafe { call_tramp(HK_BUILD_ID, a1, a2, a3, a4, a5, a6) };
    if BLIND.load(Ordering::Relaxed) && a3 != 0 {
        let real = hromatite_win::memory::read_u32(a3 as usize).unwrap_or(0);
        if BLIND_BUILD_SET.load(Ordering::Relaxed) {
            let managed = BLIND_BUILD_ID.load(Ordering::Relaxed);
            let written = hromatite_win::memory::write_bytes(a3 as usize, &managed.to_le_bytes());
            let suppressed = BLIND_BUILD.fetch_add(1, Ordering::Relaxed) + 1;
            if suppressed <= 5 || suppressed.is_multiple_of(50) {
                hook_log(format!(
                    "[blind] GetModuleBuildId #{calls} real={real} managed={managed} \
                     written={written} -> EOS_Success"
                ));
            }
            return 0; // EOS_Success
        }
        if verbose(calls) {
            hook_log(format!(
                "[blind] GetModuleBuildId #{calls} real={real} (passthrough, no \
                 EAC_BLIND_BUILD_ID) ret={result:#x}"
            ));
        }
        return result;
    }
    if verbose(calls) {
        hook_log(format!(
            "[call] GetModuleBuildId #{calls} -> ret={result:#x}"
        ));
    }
    result
}

/// `EOS_AntiCheatClient_GetSystemRequirementStatus`: Secure Boot / TPM style
/// requirement query. Blind mode reports `EOS_ACCSRS_FullyMet` and
/// `EOS_Success`; the real status is logged first.
///
/// `OutReason` is deliberately left alone: its capacity lives in the options
/// struct, and guessing the field offset is how a "harmless" diagnostic ends up
/// scribbling over the game's memory. The caller's buffer is already initialised
/// by the game, and `FullyMet` makes the reason optional.
#[cfg(windows)]
unsafe extern "system" fn hook_sysreq(a1: u64, a2: u64, a3: u64, a4: u64, a5: u64, a6: u64) -> u64 {
    let calls = CALLS[HK_SYSREQ].fetch_add(1, Ordering::Relaxed) + 1;
    // SAFETY: untouched argument registers.
    let result = unsafe { call_tramp(HK_SYSREQ, a1, a2, a3, a4, a5, a6) };
    if BLIND.load(Ordering::Relaxed) && a3 != 0 {
        let real = hromatite_win::memory::read_u32(a3 as usize).unwrap_or(0);
        let managed = BLIND_SYSREQ_VALUE.load(Ordering::Relaxed);
        let written = hromatite_win::memory::write_bytes(a3 as usize, &managed.to_le_bytes());
        let suppressed = BLIND_SYSREQ.fetch_add(1, Ordering::Relaxed) + 1;
        if suppressed <= 5 || suppressed.is_multiple_of(50) {
            hook_log(format!(
                "[blind] GetSystemRequirementStatus #{calls} real={real} managed={managed} \
                 written={written} real_ret={result:#x} -> EOS_Success"
            ));
        }
        return 0; // EOS_Success
    }
    if verbose(calls) {
        hook_log(format!(
            "[call] GetSystemRequirementStatus #{calls} -> ret={result:#x}"
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
        // `reset_state` (the queue test) zeroes these same counters, and the
        // harness runs tests in parallel: hold the lock `reset_state` takes, so
        // the sample count cannot be cleared between the stores and the reads.
        let _serial = STATE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
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
        // The gate is emitter state too: leave it disarmed and back to its
        // default budget, so no test inherits another one's epoch.
        let mut gate = epoch_lock();
        gate.reset();
        gate.set_stale_ms(epoch::DEFAULT_STALE_MS);
    }

    /// Arm the epoch gate for a test template. Emission is refused otherwise —
    /// that is the invariant, not an inconvenience of the tests.
    fn arm_gate(sid: u64, now_ms: u64) {
        let mut gate = epoch_lock();
        gate.reset();
        gate.set_stale_ms(60_000);
        gate.note_live(sid, now_ms);
        assert!(gate.arm(sid, now_ms), "the test gate must arm");
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

        forge_learn(&forge_frame(0xAA)[..24], 24, 0);
        assert_eq!(FORGE_SEEN.load(Ordering::Relaxed), 0, "a 24-byte ACK");
        assert!(!forge_due(60_000), "no 512-byte frame learned yet");

        forge_learn(&forge_frame(0xAA), forge::FRAME_LEN, 1_000);
        assert_eq!(FORGE_SEEN.load(Ordering::Relaxed), 1);
        assert!(!forge_due(60_000), "the minimum frame count is not reached");

        forge_learn(&forge_frame(0xBB), forge::FRAME_LEN, 2_000);
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
        arm_gate(0x158b, 0);
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
        arm_gate(0x158b, 0);

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
        arm_gate(0x77aa, 0);

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
        arm_gate(0x9001, 0);

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

    /// A frame of an arbitrary session, for re-learning after the change.
    fn frame_of(sid: u64, tag: u8) -> Vec<u8> {
        let template = Template {
            session_id: sid,
            flags: 0,
            last_seq: 1,
            body: vec![tag; forge::BODY_LEN],
        };
        forge::build_frame(&template, 2, BodyMode::Stale)
    }

    #[test]
    fn the_emitter_stops_when_the_live_session_changes() {
        let _serial = STATE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        forge_state_reset();
        let path = capture_path("epoch");
        *FORGE_CAPTURE.lock().expect("capture lock") = CaptureFile::open(&path).ok();
        let counted = || -> usize {
            crate::capture::read_frames(&path)
                .map(|frames| frames.len())
                .unwrap_or(0)
        };

        // Takeover of 0x158b, backed by a live frame of that session.
        let template = Template::from_frame(&frame_of(0x158b, 0x11)).expect("template");
        *FORGE_TEMPLATE.lock().expect("template lock") = Some(Learned {
            template,
            bump: false,
            counter: 0,
        });
        FORGE_STARTED.store(true, Ordering::Release);
        FORGE_NEXT_SEQ.store(10, Ordering::Relaxed);
        arm_gate(0x158b, 1_000);

        emit_forge_frame(1_000);
        assert_eq!(counted(), 1, "the armed session emits");
        emit_forge_frame(3_000);
        assert_eq!(counted(), 2, "and keeps emitting while it is live");
        assert!(FORGE_STARTED.load(Ordering::Relaxed));

        // The module's next heartbeat belongs to a *new* session: this is the
        // live shape of the 2026-11-18 kick (81 stale-sid frames, second kick).
        epoch_note_live(0x7a1b, 4_000);
        emit_forge_frame(5_000);
        assert_eq!(counted(), 2, "no frame of the old sid after the change");
        assert!(
            !FORGE_STARTED.load(Ordering::Relaxed),
            "the emitter stopped itself"
        );
        assert!(
            FORGE_TEMPLATE.lock().expect("template lock").is_none(),
            "and forgot the dead template"
        );
        assert!(
            epoch_lock().stopped().is_some(),
            "the stop is sticky, so a single stale frame cannot resume it"
        );

        // Still stopped on the next ticks, and a stale template cannot re-arm.
        emit_forge_frame(6_000);
        emit_forge_frame(8_000);
        assert_eq!(counted(), 2);
        assert!(
            !epoch_arm(0x158b, 8_000),
            "no fresh evidence of the old sid"
        );

        // Only learning the new session and re-arming resumes — on the new sid.
        forge_learn(&frame_of(0x7a1b, 0x22), forge::FRAME_LEN, 9_000);
        FORGE_AFTER_MS.store(1, Ordering::Relaxed);
        FORGE_MIN_FRAMES.store(0, Ordering::Relaxed);
        SESSION_START_MS.store(1, Ordering::Relaxed);
        assert!(forge_begin(9_100), "the new session is taken over");
        emit_forge_frame(9_100);
        assert_eq!(counted(), 3, "emission resumed");
        let frames = crate::capture::read_frames(&path).expect("capture");
        assert_eq!(
            u64::from_le_bytes(frames[2].data[4..12].try_into().expect("8 bytes")),
            0x7a1b,
            "and carries the session that is live now"
        );

        *FORGE_CAPTURE.lock().expect("capture lock") = None;
        let _ = std::fs::remove_file(&path);
        forge_state_reset();
    }

    #[test]
    fn a_silent_stream_stops_the_emitter_after_the_budget() {
        let _serial = STATE_LOCK.lock().unwrap_or_else(|error| error.into_inner());
        forge_state_reset();
        let path = capture_path("silent");
        *FORGE_CAPTURE.lock().expect("capture lock") = CaptureFile::open(&path).ok();
        let template = Template::from_frame(&frame_of(0x4242, 0x33)).expect("template");
        *FORGE_TEMPLATE.lock().expect("template lock") = Some(Learned {
            template,
            bump: false,
            counter: 0,
        });
        FORGE_STARTED.store(true, Ordering::Release);
        FORGE_NEXT_SEQ.store(1, Ordering::Relaxed);
        {
            // 1 s budget: the module died, nothing confirms the session any more.
            let mut gate = epoch_lock();
            gate.reset();
            gate.set_stale_ms(1_000);
            gate.note_live(0x4242, 1_000);
            assert!(gate.arm(0x4242, 1_000));
        }
        emit_forge_frame(1_500);
        let after_first = crate::capture::read_frames(&path)
            .map(|f| f.len())
            .unwrap_or(0);
        assert_eq!(after_first, 1);

        emit_forge_frame(2_001);
        let after_budget = crate::capture::read_frames(&path)
            .map(|f| f.len())
            .unwrap_or(0);
        assert_eq!(after_budget, 1, "silence past the budget stops the emitter");
        assert!(!FORGE_STARTED.load(Ordering::Relaxed));

        *FORGE_CAPTURE.lock().expect("capture lock") = None;
        let _ = std::fs::remove_file(&path);
        forge_state_reset();
    }
}
