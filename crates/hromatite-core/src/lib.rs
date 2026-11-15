//! Hromatite core: Win32 plumbing, manual mapper, il2cpp reconnaissance.
//!
//! Everything here targets `x86_64-pc-windows-gnu` — both the injector exe and
//! the injected dll are Windows binaries, even though the host is Linux.

#![deny(unsafe_code)]

pub mod api_symbols;
pub mod crypto;
pub mod gcthreads;
pub mod log;
pub mod m3;
pub mod manualmap;
pub mod meta;
pub mod metadata;
pub mod pe;
#[allow(unsafe_code)]
mod raw;
pub mod runtime;
pub mod symbols;

// Keep the established public API without exposing the raw implementation
// hierarchy as an additional compatibility contract.
pub use raw::api as api;
pub use raw::console as console;
pub use raw::context as veh;
pub use raw::ffi as ffi;
pub use raw::hook as hook;
pub use raw::il2cpp as il2cpp;
pub use raw::memory as safe;
pub use raw::procs as procs;
pub use raw::remote as remote;

pub const GAME_EXE: &str = "VRChat.exe";
pub const GAME_ASSEMBLY: &str = "GameAssembly.dll";
pub const UNITY_PLAYER: &str = "UnityPlayer.dll";
