//! Resident Hromatite runtime.
//!
//! This is the payload that lives inside the game process. It is not a library
//! other parts of the project link for utilities: it owns the mutable, running
//! state - the command reader, the hot-reload router, the feature images - and
//! the startup decision tree that arms the managed detours in `hromatite-core`.
//!
//! Layout:
//!
//! * [`startup`] - everything `DllMain` does after the OS calls us: PEB unlink,
//!   waiting for Unity, EAC hooks, the tick target, GC guard, LateUpdate /
//!   PlayerLoop installation. `hromatite-dll` is a three-line shim over it.
//! * [`tick`] - the managed-tick implementation published into
//!   `hromatite_il2cpp::hook`: per-frame feature entry, throttled work (GC
//!   reports, hot reload, command dispatch).
//! * [`hot`] - the generation state machine ([`hot`]) and its runtime router
//!   ([`hot::router`]: mapping, canary, retirement, reload commands).
//! * [`localmap`] - in-process loader for the versioned feature images.
//! * [`hostapi`] - the `HostApiV1` table those images receive.
//! * [`console`] - the file-based command channel (`hromatite-cmd.txt`).
//!
//! # Why the tick is published, not called
//!
//! `hromatite-core` owns the detours and must not depend on this crate (this
//! crate depends on it). The detour therefore calls through
//! [`hromatite_il2cpp::hook::TickTarget`], which [`tick::install`] publishes once
//! at startup. Before that call the tick only counts frames.

#![deny(unsafe_code)]

#[allow(unsafe_code)]
pub mod console;
#[allow(unsafe_code)]
pub mod hostapi;
#[allow(unsafe_code)]
pub mod hot;
#[allow(unsafe_code)]
pub mod localmap;
pub mod startup;
pub mod tick;

pub use startup::dll_main;
