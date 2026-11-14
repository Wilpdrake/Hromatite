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
    // Our own thread id: the GC crash ("SuspendThread loop failed") is caused by
    // a Boehm thread record whose OS thread is gone, so the log has to say which
    // thread was ours before anything else can be blamed on it.
    log::log_tag(
        "con",
        &format!(
            "stub thread tid={:#x}, {} thread(s) in the process",
            hromatite_core::gcthreads::own_tid(),
            hromatite_core::gcthreads::live_tids().len()
        ),
    );
    // Baseline of Boehm's own thread table: one line, cheap (256 reads).
    log::log_tag(
        "gct",
        &log::guard("gct baseline", || {
            hromatite_core::gcthreads::snapshot(None)
                .map(|t| t.summary())
                .unwrap_or_else(|| "table not found".to_string())
        })
        .unwrap_or_else(|| "guard".to_string()),
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs as u64);
    let mut polls: u32 = 0;
    loop {
        // Every ~16 s: the state of the GC thread table. If the game dies of a
        // "SuspendThread loop failed", the last line before the end of the log
        // names the record that made the collector spin.
        polls += 1;
        // Every ~8 s: thread births/deaths plus the state of Boehm's own thread
        // table. The line we are hunting is "thread 0xNNN exited" immediately
        // followed by "Boehm STILL holds it" — that record is what makes
        // GC_suspend spin 1e6 times and abort the game a few seconds later.
        if polls % 20 == 0 {
            let lines = log::guard("gct watch", hromatite_core::gcthreads::watch)
                .unwrap_or_else(|| vec!["guard".to_string()]);
            for l in lines {
                log::log_tag("gct", &l);
            }
        }
        // `poll()` is the one call in this loop that is not a `dispatch`: an AV
        // there used to kill the payload thread silently (the log simply stops
        // right after "console ready" and every command goes unanswered).
        let lines = log::guard("reader.poll", || reader.poll()).unwrap_or_default();
        for line in lines {
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

fn attach(hinst: *mut core::ffi::c_void, reserved: *mut core::ffi::c_void) {
    // The injector's stub passes `r8 = &{base, size}`: the payload cannot read its
    // own PE header (erased) and `VirtualQuery` reports the section split, so L2b
    // needs the exact range the injector is going to seal.
    if !reserved.is_null() {
        unsafe {
            let p = reserved as *const usize;
            let base = *p;
            let size = *p.add(1);
            let text_lo = *p.add(2);
            let text_hi = *p.add(3);
            if base > 0x10000 && size > 0 {
                hook::set_image_range(base, size);
            }
            if text_lo > base && text_hi > text_lo {
                hook::set_keep_range(text_lo, text_hi);
            }
            let code_lo = *p.add(4);
            let code_hi = *p.add(5);
            if code_lo > base && code_hi > code_lo {
                hook::set_code_range(code_lo, code_hi);
            }
        }
    }
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
    // Plan 3: HROMATITE_FRAME_HOOK=1 replaces the long-lived stub thread with a
    // tick inside a function the game already calls. DllMain returns right away,
    // so there is no thread of ours left and the injector stops waiting for us.
    // (Trade-off: `--seal` XORs the image the moment we return, which also
    // destroys the detour — frame hook and seal are exclusive for now.)
    let frame_hook = std::env::var("HROMATITE_FRAME_HOOK")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);

    let ready = log::guard("wait_for_modules", || wait_for_modules(budget)).unwrap_or(false);
    if frame_hook {
        // Plan 4: same flow, no extra code path — `install_tick` sees
        // HROMATITE_SEAL/HROMATITE_L2B and puts the tick behind a shim page
        // that decrypts the image for the duration of the call. We still return
        // immediately, which is what lets the injector seal us.
        let seal = std::env::var("HROMATITE_SEAL").unwrap_or_else(|_| "-".into());
        let l2b = std::env::var("HROMATITE_L2B").unwrap_or_else(|_| "-".into());
        log::log_tag("dll", &format!("FRAME_HOOK: HROMATITE_SEAL={seal} HROMATITE_L2B={l2b}"));
        // Install the tick *before* any recon: the full diagnose() scan takes
        // seconds and must not delay getting off the stub thread. `diag` stays
        // available as a console command for the same reason.
        let ok = log::guard("install_tick", hook::install_tick).unwrap_or(false);
        log::log_tag(
            "dll",
            &format!("FRAME_HOOK: tick hook installed={ok}, returning from DllMain (no console_loop)"),
        );
        log::log_tag("dll", "recon done, returning from DllMain");
        return;
    }
    if ready {
        // The recon walks unknown structures; a panic (index/Option arithmetic)
        // is expected there sooner or later and must stay contained.
        let _ = log::guard("il2cpp::run_once", il2cpp::run_once);
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
    reserved: *mut core::ffi::c_void,
) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        // Install before anything can panic, then run the body under a guard:
        // unwinding out of `extern "system"` is UB, returning 1 always is not.
        log::install_panic_hook();
        // Plan 2: the vectored handler is process-wide and inert until a guard
        // is armed (`veh::call_*`), so installing it at attach costs nothing and
        // guarantees the very first guarded call is already covered.
        hromatite_core::veh::install();
        if log::guard("dllmain", || attach(hinst, reserved)).is_none() {
            log::log_tag("dll", "DllMain: panic escaped the attach phase, returning 1 anyway");
        }
    }
    1
}
