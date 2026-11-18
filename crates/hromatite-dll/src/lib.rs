//! `hromatite.dll`: the injected payload's OS entry point.
//!
//! This crate exists for exactly one reason - Windows needs a `cdylib` with a
//! `DllMain` export. Every decision lives in `hromatite-runtime`
//! ([`hromatite_runtime::dll_main`]): the PEB unlink, waiting for Unity, the
//! EOS/Photon hooks, the tick publication and the managed detours.
//!
//! Keeping the entry this thin is what guarantees the resident never links the
//! injector's manual mapper or the cross-process remote layer.

#![deny(unsafe_code)]

const DLL_PROCESS_ATTACH: u32 = 1;

/// `DllMain` returns 1 unconditionally: the payload must never let the loader
/// unload (or fail) the module because of its own error path, and a panic must
/// not cross the `extern "system"` boundary.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "system" fn DllMain(
    hinst: *mut core::ffi::c_void,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> i32 {
    if reason == DLL_PROCESS_ATTACH {
        return hromatite_runtime::dll_main(hinst, reason);
    }
    1
}
