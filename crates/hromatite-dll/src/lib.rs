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
        Ok(())
    } else {
        Err("refusing to install managed LateUpdate without the inline GC GetThreadContext guard")
    }
}

fn install_late_update_with_retry(
    selector: hromatite_core::lateupdate::Selector,
) -> Result<hromatite_core::lateupdate::Candidate, String> {
    let wait_ms = std::env::var("HROMATITE_LATE_UPDATE_WAIT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(30_000)
        .min(120_000);
    let attempts = (wait_ms / 250).max(1) as u32;
    hromatite_core::lateupdate::retry_bounded(
        attempts,
        std::time::Duration::from_millis(250),
        |attempt| {
            if !hook::materialise_late_class(selector) {
                if attempt % 20 == 0 {
                    log::log_tag(
                        "late",
                        &format!("waiting for class: attempt {attempt}/{attempts}"),
                    );
                }
                return None;
            }
            Some(hook::install_late_update(selector))
        },
    )
    .unwrap_or_else(|| {
        Err(format!(
            "class/method did not become available within {wait_ms}ms"
        ))
    })
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
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
    use super::{require_gc_guard, select_late_update, validate_modes};
    use hromatite_core::lateupdate::Selector;

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
