//! Injected payload: one managed LateUpdate runtime, no payload-owned thread.
//!
//! Startup is deliberately linear: wait for Unity, install the inline GC guard,
//! optionally install GC tracing, then hook the selected managed LateUpdate.
//! A failed prerequisite never falls back to another tick source or recon loop.

#![deny(unsafe_code)]

use hromatite_core::{GAME_ASSEMBLY, UNITY_PLAYER, hook, log, procs};

const DLL_PROCESS_ATTACH: u32 = 1;

fn wait_for_modules(max_secs: u32) -> bool {
    let mut waited = 0u32;
    loop {
        let ga = procs::own_module(GAME_ASSEMBLY);
        let up = procs::own_module(UNITY_PLAYER);
        if ga.is_some() && up.is_some() {
            log::log(&format!("modules ready after {waited}s"));
            return true;
        }
        if waited >= max_secs {
            log::log(&format!(
                "gave up waiting: GameAssembly={:?} UnityPlayer={:?}",
                ga.is_some(),
                up.is_some()
            ));
            return false;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        waited += 1;
    }
}

fn select_late_update(value: Option<&str>) -> Result<hromatite_core::lateupdate::Selector, String> {
    use hromatite_core::lateupdate::Selector;
    let Some(value) = value else {
        return Ok(Selector::Auto);
    };
    let value = value.trim();
    hromatite_core::lateupdate::parse_selector(value).ok_or_else(|| {
        format!("HROMATITE_LATE_UPDATE={value:?} is invalid; managed LateUpdate is required")
    })
}

fn player_loop_requested(value: Option<&str>) -> bool {
    value
        .map(str::trim)
        .is_some_and(|v| !v.is_empty() && v != "0")
}

/// `EAC_IAT=1` (off by default): also patch the callers' EOSSDK import slots.
///
/// Off by default because the IAT walk touches other modules' pages; when it is
/// on, the resident thread runs it as soon as EOSSDK is loaded, before any
/// managed tick exists.
fn iat_requested(value: Option<&str>) -> bool {
    value
        .map(str::trim)
        .is_some_and(|v| !v.is_empty() && v != "0")
}

fn validate_modes(frame_hook: &str, live: &str, seal: &str, l2b: &str) -> Result<(), String> {
    if frame_hook != "1" {
        return Err(
            "HROMATITE_FRAME_HOOK=0 is unsupported: managed LateUpdate is the only runtime".into(),
        );
    }
    if live != "0" {
        return Err("HROMATITE_LIVE stub console was removed; set HROMATITE_LIVE=0".into());
    }
    if seal != "0" || l2b != "0" {
        return Err(
            "HROMATITE_SEAL/HROMATITE_L2B are incompatible with the in-image LateUpdate detour"
                .into(),
        );
    }
    Ok(())
}

fn require_gc_guard(installed: bool) -> Result<(), &'static str> {
    if installed {
        return Ok(());
    }
    // `HROMATITE_GCT_ENTRY=0` is the deliberate A/B: Boehm runs stock and the
    // startup is allowed to continue, loudly. Without it the guard stays
    // mandatory, because an unguarded inline hook next to a leaked Boehm record
    // is what turns a normal worker exit into `SuspendThread loop failed`.
    if std::env::var("HROMATITE_GCT_ENTRY")
        .map(|value| value == "0")
        .unwrap_or(false)
    {
        log::log_tag(
            "gct",
            "startup continues WITHOUT the GC guard: HROMATITE_GCT_ENTRY=0 is an experiment switch",
        );
        return Ok(());
    }
    Err("refusing to install managed LateUpdate without the inline GC GetThreadContext guard")
}

fn install_late_update_with_retry(
    selector: hromatite_core::lateupdate::Selector,
) -> Result<hromatite_core::lateupdate::Candidate, String> {
    let wait_ms = std::env::var("HROMATITE_LATE_UPDATE_WAIT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(30_000)
        .min(120_000);
    let wait = std::time::Duration::from_millis(wait_ms);
    let started = std::time::Instant::now();
    hromatite_core::lateupdate::retry_for(wait, std::time::Duration::from_millis(250), |attempt| {
        if !hook::materialise_late_class(selector) {
            if attempt % 20 == 0 {
                log::log_tag(
                    "late",
                    &format!(
                        "waiting for class: attempt {attempt}, elapsed {}ms/{wait_ms}ms",
                        started.elapsed().as_millis()
                    ),
                );
            }
            return None;
        }
        Some(hook::install_late_update(selector))
    })
    .unwrap_or_else(|| {
        Err(format!(
            "class/method did not become available within {wait_ms}ms"
        ))
    })
}

fn install_player_loop(selector: &str) -> Result<hromatite_core::playerloop::Entry, String> {
    hromatite_core::playerloop::install(selector)
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Install the EOS anti-cheat hooks from the resident startup thread.
///
/// `AddNotifyMessageToServer` is registered by VRChat within a few hundred
/// milliseconds of `EOSSDK-Win64-Shipping.dll` loading — long before the first
/// managed tick. Waiting for the module here (50 ms poll, bounded) and patching
/// immediately is what makes the game's callback interception possible; the
/// feature image's own `Hooks::poll` then sees the entries already patched and
/// reuses their trampolines through the shared table.
fn install_ac_hooks() {
    const EOSSDK: &str = "EOSSDK-Win64-Shipping.dll";
    let started = std::time::Instant::now();
    let budget = std::time::Duration::from_secs(20);
    while started.elapsed() < budget {
        if procs::own_module(EOSSDK).is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if procs::own_module(EOSSDK).is_none() {
        log::log_tag(
            "eac",
            "EOSSDK not loaded yet; hooks stay pending on the tick",
        );
        return;
    }
    // `EAC_IAT=1`: rewrite the callers' import slots *first*. The game caches the
    // export pointer it resolved ~20 s in, so an inline patch of the export entry
    // may already be too late; the caller's IAT slot is re-read on every call.
    if iat_requested(std::env::var("EAC_IAT").ok().as_deref()) {
        let slots = hromatite_eac::eos::install_iat();
        log::log_tag("iat", &format!("patched {slots} slots"));
    }
    let mut hooks = hromatite_eac::Hooks::new();
    let line = hooks.install_now();
    log::log_tag("eac", &line);
    if let Some(status) = hooks.status_lines().first() {
        log::log_tag("eac", status);
    }
}

/// `EAC_EVENTHOOK=1`: first (early, likely too early) attempt at the managed
/// Photon hook. Photon types are materialised later, so failure here is normal —
/// `hook::tick` retries through `photon::poll` once the managed runtime is up.
fn install_photon_hook() {
    use hromatite_core::photon;
    if !photon::enabled_outgoing() && !photon::enabled_incoming() {
        return;
    }
    if photon::enabled_outgoing() {
        match photon::install_outgoing() {
            Ok(target) => log::log_tag(
                "photon",
                &format!(
                    "early send hook: entry={:#x} MethodInfo={:#x} params={}",
                    target.entry, target.info, target.parameters
                ),
            ),
            Err(reason) => log::log_tag("photon", &format!("early send hook deferred: {reason}")),
        }
    }
    if photon::enabled_incoming() {
        match photon::install_incoming() {
            Ok(target) => log::log_tag(
                "photon",
                &format!("early incoming hook: entry={:#x}", target.entry),
            ),
            Err(reason) => {
                log::log_tag("photon", &format!("early incoming hook deferred: {reason}"))
            }
        }
    }
}

fn attach(hinst: *mut core::ffi::c_void) {
    log::log_tag("dll", "DllMain(PROCESS_ATTACH), managed LateUpdate runtime");

    let unlinked =
        log::guard("peb-unlink", || procs::unlink_from_peb(hinst as usize)).unwrap_or(false);
    log::log_tag("dll", &format!("peb unlink: {unlinked}"));

    let budget = std::env::var("HROMATITE_WAIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    if !log::guard("wait_for_modules", || wait_for_modules(budget)).unwrap_or(false) {
        log::log_tag("dll", "startup aborted: Unity modules are unavailable");
        return;
    }

    // Anti-cheat hooks before any managed tick exists: VRChat registers its
    // message-to-server callback early and only once.
    log::guard("install_ac_hooks", install_ac_hooks);
    log::guard("install_photon_hook", install_photon_hook);

    // In-game nuklear UI: find the game's swapchain and hook Present. Off unless
    // `HROMATITE_UI=1` — a Present hook is the most visible thing this payload
    // can install, so it stays opt-in like the other live hooks.
    if hromatite_core::ui::enabled() {
        let installed = log::guard("install_ui", hromatite_core::ui::install).unwrap_or(false);
        log::log_tag("ui", &format!("install: hooked={installed}"));
    }

    if let Err(error) = validate_modes(
        &env_or("HROMATITE_FRAME_HOOK", "1"),
        &env_or("HROMATITE_LIVE", "0"),
        &env_or("HROMATITE_SEAL", "0"),
        &env_or("HROMATITE_L2B", "0"),
    ) {
        log::log_tag("dll", &format!("startup rejected: {error}"));
        return;
    }
    let selector = match select_late_update(std::env::var("HROMATITE_LATE_UPDATE").ok().as_deref())
    {
        Ok(selector) => selector,
        Err(error) => {
            log::log_tag("late", &format!("startup rejected: {error}"));
            return;
        }
    };

    let gc_guard =
        log::guard("install_gc_context_guard", hook::install_gc_context_guard).unwrap_or(false);
    if let Err(error) = require_gc_guard(gc_guard) {
        log::log_tag("dll", &format!("startup aborted: {error}"));
        return;
    }

    let trace = std::env::var("HROMATITE_GCT_TRACE")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);
    if trace {
        let traced =
            log::guard("install_gc_worker_trace", hook::install_gc_worker_trace).unwrap_or(false);
        log::log_tag("dll", &format!("GC worker trace installed={traced}"));
    }

    if player_loop_requested(std::env::var("HROMATITE_PLAYER_LOOP").ok().as_deref()) {
        let target = env_or("HROMATITE_PLAYER_LOOP_TARGET", "Update");
        match log::guard("install_player_loop", || install_player_loop(&target)) {
            Some(Ok(entry)) => log::log_tag(
                "playerloop",
                &format!(
                    "PlayerLoop runtime ready: hooked entry {:#x}, inline GC guard installed",
                    entry.update_function
                ),
            ),
            Some(Err(error)) => log::log_tag(
                "playerloop",
                &format!("PlayerLoop installation failed: {error}; no late fallback was started"),
            ),
            None => log::log_tag(
                "playerloop",
                "PlayerLoop installer panicked; no late fallback was started",
            ),
        }
        return;
    }

    match log::guard("install_late_update_with_retry", || {
        install_late_update_with_retry(selector)
    }) {
        Some(Ok(candidate)) => {
            log::log_tag(
                "late",
                &format!(
                    "LateUpdate runtime ready: hooked entry {:#x}, inline GC guard installed",
                    candidate.entry
                ),
            );
        }
        Some(Err(error)) => {
            log::log_tag(
                "late",
                &format!(
                    "LateUpdate installation failed: {error}; no alternative tick source was started"
                ),
            );
        }
        None => log::log_tag(
            "late",
            "LateUpdate installer panicked; no alternative tick source was started",
        ),
    }
}

#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "system" fn DllMain(
    hinst: *mut core::ffi::c_void,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        log::install_panic_hook();
        // A launcher that retries the injection maps a fresh copy each time;
        // only the first one may install hooks, guards and a VEH handler.
        if !procs::claim_single_instance("Local\\hromatite_payload_v1") {
            log::log_tag(
                "dll",
                "another payload instance is already resident; this copy stays dormant",
            );
            return 1;
        }
        hromatite_core::veh::install();
        if log::guard("dllmain", || attach(hinst)).is_none() {
            log::log_tag(
                "dll",
                "DllMain: panic escaped the attach phase, returning 1 anyway",
            );
        }
    }
    1
}

#[cfg(test)]
mod tests {
    use super::{
        iat_requested, player_loop_requested, require_gc_guard, select_late_update, validate_modes,
    };
    use hromatite_core::lateupdate::Selector;

    #[test]
    fn player_loop_is_explicit_opt_in_and_late_update_stays_the_default() {
        assert!(!player_loop_requested(None));
        assert!(!player_loop_requested(Some("")));
        assert!(!player_loop_requested(Some("0")));
        assert!(player_loop_requested(Some("1")));
        assert!(player_loop_requested(Some("true")));
    }

    #[test]
    fn the_iat_walk_is_opt_in_only() {
        assert!(!iat_requested(None));
        assert!(!iat_requested(Some("")));
        assert!(!iat_requested(Some("  ")));
        assert!(!iat_requested(Some("0")));
        assert!(iat_requested(Some("1")));
        assert!(iat_requested(Some(" 1 ")));
    }

    #[test]
    fn late_update_auto_is_the_only_default_tick_source() {
        assert_eq!(select_late_update(None), Ok(Selector::Auto));
        assert_eq!(select_late_update(Some("auto")), Ok(Selector::Auto));
        assert_eq!(select_late_update(Some(" AUTO ")), Ok(Selector::Auto));
    }

    #[test]
    fn explicit_late_update_selection_remains_available() {
        assert_eq!(
            select_late_update(Some("#31241")),
            Ok(Selector::Index(31_241))
        );
        assert_eq!(
            select_late_update(Some("0x3238aca0")),
            Ok(Selector::Address(0x3238aca0))
        );
        assert!(select_late_update(Some("0")).is_err());
        assert!(select_late_update(Some("not-a-class")).is_err());
    }

    #[test]
    fn removed_qpc_stub_and_sealing_modes_fail_fast() {
        assert!(validate_modes("1", "0", "0", "0").is_ok());
        for modes in [
            ("0", "0", "0", "0"),
            ("1", "1800", "0", "0"),
            ("1", "0", "1", "0"),
            ("1", "0", "0", "1"),
        ] {
            assert!(
                validate_modes(modes.0, modes.1, modes.2, modes.3).is_err(),
                "accepted removed mode {modes:?}"
            );
        }
    }

    #[test]
    fn managed_tick_requires_a_successful_gc_guard() {
        assert!(require_gc_guard(false).is_err());
        assert!(require_gc_guard(true).is_ok());
    }
}
