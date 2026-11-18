//! Win32/x64 OS boundary: hand-written Win32 FFI, safe memory/VirtualQuery
//! readers and writers, VEH context guard, PE parser, Toolhelp32/PEB helpers,
//! cross-process remote handle, SHA-256 and file logging.
//!
//! No dependencies on Unity, IL2CPP, EOS, Nuklear, D3D11, or inline hook engines.

#![deny(unsafe_code)]

#[allow(unsafe_code)]
pub mod ffi;
pub mod log;
#[allow(unsafe_code)]
pub mod memory;
#[allow(unsafe_code)]
pub mod memscan;
pub mod pe;
#[allow(unsafe_code)]
pub mod procs;
#[cfg(feature = "remote")]
#[allow(unsafe_code)]
pub mod remote;
pub mod sha256;
#[allow(unsafe_code)]
pub mod sys;
#[allow(unsafe_code)]
pub mod veh;

pub use memory as safe;

pub const GAME_EXE: &str = "VRChat.exe";
pub const GAME_ASSEMBLY: &str = "GameAssembly.dll";
pub const UNITY_PLAYER: &str = "UnityPlayer.dll";
