//! The injected payload.
//!
//! Stealth constraint: the game must not gain extra threads or processes, so
//! everything runs inline on the stub thread the injector already started.
//! Waiting for il2cpp there is fine — that thread is ours, not the game's.
//!
//! M1/M2 scope: prove we are inside VRChat's wine process, that
//! GameAssembly/UnityPlayer are there, and resolve UnityPlayer's private export
//! table. No game function is called except the ones we have identified.

use hromatite_core::{console, il2cpp, log, procs, GAME_ASSEMBLY, UNITY_PLAYER};

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
            let (out, action) = console::dispatch(&line);
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

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(
    hinst: *mut core::ffi::c_void,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        // Boot marker: proves execution even if the logger itself is at fault.
        let _ = std::fs::write("C:\\dllmain-probe.txt", b"reached DllMain\n");
        log::log_tag("dll", "DllMain(PROCESS_ATTACH), inline (no extra thread)");

        // LoadLibrary-style delivery leaves us in the PEB module list; drop the
        // entry so a module sweep sees only the game's own files.
        let unlinked = procs::unlink_from_peb(hinst as usize);
        log::log_tag("dll", &format!("peb unlink: {unlinked}"));

        // Tests set HROMATITE_WAIT=2 so the stub thread returns quickly.
        let budget: u32 = std::env::var("HROMATITE_WAIT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120);
        if wait_for_modules(budget) {
            il2cpp::run_once();
        }
        // HROMATITE_LIVE=0 (default) keeps the one-shot stealth behaviour.
        let live: u32 = std::env::var("HROMATITE_LIVE")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        if live > 0 {
            console_loop(live);
        }
        log::log_tag("dll", "recon done, returning from DllMain");
    }
    1
}
