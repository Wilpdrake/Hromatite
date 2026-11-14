//! Hromatite core: Win32 plumbing, manual mapper, il2cpp reconnaissance.
//!
//! Everything here targets `x86_64-pc-windows-gnu` — both the injector exe and
//! the injected dll are Windows binaries, even though the host is Linux.

pub mod api;
pub mod console;
pub mod crypto;
pub mod ffi;
pub mod gcthreads;
pub mod hook;
pub mod il2cpp;
pub mod log;
pub mod manualmap;
pub mod pe;
pub mod procs;
pub mod remote;
pub mod safe;
pub mod symbols;
pub mod veh;

pub const GAME_EXE: &str = "VRChat.exe";
pub const GAME_ASSEMBLY: &str = "GameAssembly.dll";
pub const UNITY_PLAYER: &str = "UnityPlayer.dll";
