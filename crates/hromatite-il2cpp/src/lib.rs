//! Unity IL2CPP domain for the Hromatite payload.
//!
//! Everything that knows about *this game build* lives here: the generated
//! symbol table, the per-build descriptor (`buildinfo`), the IL2CPP type system
//! (`m3`), the Boehm GC interop (`gcthreads`, `gcguard`), the managed/native
//! tick sources (`lateupdate`, `playerloop`) and Photon event interception
//! (`photon`).
//!
//! # Layering
//!
//! * `hromatite-win` - Win32/x64 OS boundary (memory, PE, procs, VEH, log).
//! * `hromatite-hook` - the inline hook engine and its satellites.
//! * **this crate** - what those primitives are pointed at: IL2CPP symbols,
//!   structures and detours.
//! * `hromatite-runtime` - the resident that decides *when* to arm any of it.
//!
//! No module here may call the console, the feature router or the hot-reload
//! runtime. Tick sources call `raw::hook::tick`, which dispatches through the
//! callback slot the runtime publishes ([`raw::hook::TickTarget`]).
//!
//! # Verified, not guessed
//!
//! Every offset and RVA in this crate comes from a live run against a specific
//! `GameAssembly.dll` (recorded in `tools/data/build-*.json`) and is validated
//! before use: `buildinfo` refuses a module whose anchors moved, and `m3` reads
//! structures only through checked readers.

#![deny(unsafe_code)]

pub mod buildinfo;
pub mod gamebuild;
pub mod gcthreads;
pub mod lateupdate;
pub mod m3;
pub mod symbols;
#[allow(unsafe_code)]
pub mod raw;

// OS-layer re-exports: the domain modules address Win32 primitives through
// these names (`crate::safe`, `crate::procs`, ...), exactly as they did when
// they lived in `hromatite-core`.
pub use hromatite_win::{
    GAME_ASSEMBLY, GAME_EXE, UNITY_PLAYER, ffi, log, memory as safe, memscan, pe, procs, sha256,
    sys, veh,
};

pub use raw::{discover, gcguard, hook, il2cpp, photon, playerloop};
