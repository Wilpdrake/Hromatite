//! Hromatite core: Win32 plumbing, manual mapper and verified IL2CPP runtime access.
//!
//! Everything here targets `x86_64-pc-windows-gnu` — both the injector exe and
//! the injected dll are Windows binaries, even though the host is Linux.

#![deny(unsafe_code)]

pub mod buildinfo;
pub mod crc;
pub mod crypto;
pub mod gamebuild;
pub mod gcthreads;
pub mod hot;
pub mod lateupdate;
pub mod m3;
pub mod manualmap;
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
pub use raw::console;
pub use raw::d3d11;
pub use raw::hook;
pub use raw::hot as hot_runtime;
pub use raw::il2cpp;
pub use raw::kiero;
pub use raw::photon;
pub use raw::playerloop;
pub use raw::ui;
