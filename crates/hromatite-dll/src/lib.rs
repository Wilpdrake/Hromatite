//! The injected payload.
//!
//! Stealth constraint: the game must not gain extra threads or processes, so
//! everything runs inline on the stub thread the injector already started.
//! Waiting for il2cpp there is fine — that thread is ours, not the game's.
//!
//! M1/M2 scope: prove we are inside VRChat's wine process, that
//! GameAssembly/UnityPlayer are there, and resolve UnityPlayer's private export
//! table. No game function is called except the ones we have identified.

use hromatite_core::{il2cpp, log, procs, GAME_ASSEMBLY, UNITY_PLAYER};

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

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(
    _hinst: *mut core::ffi::c_void,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        // Boot marker: proves execution even if the logger itself is at fault.
        let _ = std::fs::write("C:\\dllmain-probe.txt", b"reached DllMain\n");
        log::log_tag("dll", "DllMain(PROCESS_ATTACH), inline (no extra thread)");
        if wait_for_modules(120) {
            il2cpp::run_once();
        }
        log::log_tag("dll", "recon done, returning from DllMain");
    }
    1
}
