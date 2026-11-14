//! The injected payload.
//!
//! Stealth constraint: the game must not gain extra threads or processes, so
//! everything runs inline on the stub thread the injector already started.
//! Waiting for il2cpp there is fine — that thread is ours, not the game's.
//!
//! Failure containment (plan 5): a Rust panic must never escape `DllMain`.
//! Unwinding through an `extern "system"` frame is UB, and under Proton it shows
//! up as "Fatal error in GC" rather than a message — so every stage is run
//! through `log::guard`, and the whole body again at the entry point.
//! (Access violations are *not* caught here: that is the VEH, plan 2.)

use hromatite_core::{console, hook, il2cpp, log, procs, GAME_ASSEMBLY, UNITY_PLAYER};

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

/// Poll `hromatite-cmd.txt` and log the answers. Same single stub thread — no
/// new thread, no socket. Returns true when the host asked to finish (seal/quit).
fn console_loop(secs: u32) -> bool {
    let mut reader = console::Reader::new();
    log::log_tag(
        "con",
        &format!(
            "console ready for {secs}s; file {}, 'help' for commands",
            console::cmd_path().display()
        ),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs as u64);
    loop {
        for line in reader.poll() {
            log::log_tag("con", &format!("> {line}"));
            // One bad command (a panic in a scan loop) must not end the session.
            let answered = log::guard("console::dispatch", || console::dispatch(&line));
            let (out, action) = match answered {
                Some(v) => v,
                None => (vec!["command panicked (see PANIC line above)".to_string()], console::Action::None),
            };
            for l in out {
                log::log_tag("con", &l);
            }
            match action {
                console::Action::Seal | console::Action::Quit => {
                    log::log_tag("con", "leaving interactive mode");
                    return true;
                }
                console::Action::None => {}
            }
        }
        if std::time::Instant::now() >= deadline {
            log::log_tag("con", "console window closed");
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
}

fn attach(hinst: *mut core::ffi::c_void) {
    // Boot marker: proves execution even if the logger itself is at fault.
    let _ = std::fs::write("C:\\dllmain-probe.txt", b"reached DllMain\n");
    log::log_tag("dll", "DllMain(PROCESS_ATTACH), inline (no extra thread)");

    // LoadLibrary-style delivery leaves us in the PEB module list; drop the
    // entry so a module sweep sees only the game's own files.
    let unlinked = log::guard("peb-unlink", || procs::unlink_from_peb(hinst as usize))
        .unwrap_or(false);
    log::log_tag("dll", &format!("peb unlink: {unlinked}"));

    // Tests set HROMATITE_WAIT=2 so the stub thread returns quickly.
    let budget: u32 = std::env::var("HROMATITE_WAIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120);
    let ready = log::guard("wait_for_modules", || wait_for_modules(budget)).unwrap_or(false);
    if ready {
        // The recon walks unknown structures; a panic (index/Option arithmetic)
        // is expected there sooner or later and must stay contained.
        let _ = log::guard("il2cpp::run_once", il2cpp::run_once);
    }
    // Plan 3: HROMATITE_FRAME_HOOK=1 replaces the long-lived stub thread with a
    // tick inside a function the game already calls. DllMain returns right away,
    // so there is no thread of ours left and the injector stops waiting for us.
    // (Trade-off: `--seal` XORs the image the moment we return, which also
    // destroys the detour — frame hook and seal are exclusive for now.)
    let frame_hook = std::env::var("HROMATITE_FRAME_HOOK")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);
    if frame_hook {
        let ok = log::guard("install_tick", hook::install_tick).unwrap_or(false);
        log::log_tag(
            "dll",
            &format!("FRAME_HOOK: tick hook installed={ok}, returning from DllMain (no console_loop)"),
        );
        log::log_tag("dll", "recon done, returning from DllMain");
        return;
    }
    // HROMATITE_LIVE=0 (default) keeps the one-shot stealth behaviour.
    let live: u32 = std::env::var("HROMATITE_LIVE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if live > 0 {
        let _ = log::guard("console_loop", || console_loop(live));
    }
    log::log_tag("dll", "recon done, returning from DllMain");
}

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(
    hinst: *mut core::ffi::c_void,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        // Install before anything can panic, then run the body under a guard:
        // unwinding out of `extern "system"` is UB, returning 1 always is not.
        log::install_panic_hook();
        if log::guard("dllmain", || attach(hinst)).is_none() {
            log::log_tag("dll", "DllMain: panic escaped the attach phase, returning 1 anyway");
        }
    }
    1
}
