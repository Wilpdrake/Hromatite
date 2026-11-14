//! The injected payload. M1 scope: prove we are inside VRChat's wine process,
//! that GameAssembly/UnityPlayer are there, and that the obfuscated export
//! names really do live in UnityPlayer's image.
//!
//! No il2cpp function is called yet — nothing here can crash the game beyond
//! our own thread.

use hromatite_core::{ffi, il2cpp, log, procs, GAME_ASSEMBLY, UNITY_PLAYER};

const DLL_PROCESS_ATTACH: u32 = 1;

extern "system" fn worker(_param: *mut core::ffi::c_void) -> u32 {
    log::log_tag("dll", "payload thread started");

    // GameAssembly appears only after Unity boots il2cpp; wait for it.
    let mut waited = 0u32;
    loop {
        let ga = procs::own_module(GAME_ASSEMBLY);
        let up = procs::own_module(UNITY_PLAYER);
        if ga.is_some() && up.is_some() {
            log::log(&format!("modules ready after {waited}s"));
            break;
        }
        if waited > 180 {
            log::log(&format!(
                "gave up waiting: GameAssembly={:?} UnityPlayer={:?}",
                ga.is_some(),
                up.is_some()
            ));
            return 0;
        }
        std::thread::sleep(std::time::Duration::from_secs(1));
        waited += 1;
    }

    il2cpp::run_once();
    log::log_tag("dll", "M1 diagnostics done");
    0
}

#[unsafe(no_mangle)]
pub extern "system" fn DllMain(_hinst: *mut core::ffi::c_void, reason: u32, _reserved: *mut core::ffi::c_void) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        // Boot marker: std-free, so it also proves the thread reached our code
        // when the logger itself is the thing under suspicion.
        let _ = std::fs::write("C:\\dllmain-probe.txt", b"reached DllMain\n");
        log::log_tag("dll", "DllMain(PROCESS_ATTACH)");
        // A fresh thread, never work inside the loader lock.
        let mut tid = 0u32;
        unsafe {
            let h = ffi::CreateThread(
                core::ptr::null_mut(),
                0,
                worker as *const () as usize,
                core::ptr::null_mut(),
                0,
                &mut tid,
            );
            log::log_tag("dll", &format!("worker thread handle {:?} tid {tid}", h));
        }
    }
    1
}
