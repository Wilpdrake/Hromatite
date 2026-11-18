//! Hromatite core: the verified IL2CPP domain (symbols, per-build descriptors,
//! class/method access, GC interop, LateUpdate/PlayerLoop detours, Photon).
//!
//! Everything here targets `x86_64-pc-windows-gnu` — both the injector exe and
//! the injected dll are Windows binaries, even though the host is Linux.

#![deny(unsafe_code)]

pub mod buildinfo;
pub mod crc;
pub mod gamebuild;
pub mod gcthreads;
pub mod lateupdate;
pub mod m3;
#[allow(unsafe_code)]
mod raw;
pub mod symbols;

pub use hromatite_hook::{hookset, iat, midhook, vmt};
pub use hromatite_win::{
    GAME_ASSEMBLY, GAME_EXE, UNITY_PLAYER, ffi, log, memory as safe, pe, procs, remote, sha256,
    veh,
};

// Keep the established public API without exposing the raw implementation
// hierarchy as an additional compatibility contract.
pub use raw::gcguard;
pub use raw::hook;
pub use raw::il2cpp;
pub use raw::photon;
pub use raw::playerloop;
// Rendering lives in hromatite-render; core re-exports it until the runtime
// crate owns the install/toggle wiring.
pub use hromatite_render::{kiero, ui};
