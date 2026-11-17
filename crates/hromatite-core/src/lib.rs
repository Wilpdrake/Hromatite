//! Hromatite core: Win32 plumbing, manual mapper and verified IL2CPP runtime access.
//!
//! Everything here targets `x86_64-pc-windows-gnu` — both the injector exe and
//! the injected dll are Windows binaries, even though the host is Linux.

#![deny(unsafe_code)]

pub mod crc;
pub mod crypto;
pub mod gcthreads;
pub mod hot;
pub mod lateupdate;
pub mod log;
pub mod m3;
pub mod manualmap;
pub mod pe;
#[allow(unsafe_code)]
mod raw;
pub mod sha256;
pub mod symbols;

// Keep the established public API without exposing the raw implementation
// hierarchy as an additional compatibility contract.
pub use raw::console;
pub use raw::context as veh;
pub use raw::ffi;
pub use raw::hook;
pub use raw::hot as hot_runtime;
pub use raw::il2cpp;
pub use raw::memory as safe;
pub use raw::photon;
pub use raw::playerloop;
pub use raw::procs;
pub use raw::remote;

pub const GAME_EXE: &str = "VRChat.exe";
pub const GAME_ASSEMBLY: &str = "GameAssembly.dll";
pub const UNITY_PLAYER: &str = "UnityPlayer.dll";
