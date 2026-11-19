//! Implementation boundary for the domain layer.
//!
//! Every module here performs foreign calls, arbitrary-memory access, manual
//! machine-code patching or context restoration; the crate root denies
//! `unsafe_code` and this module is the single, explicit exception.
//!
//! Safe does not mean infallible: a `VirtualQuery` validates a point in time,
//! while another thread may still unmap or reprotect a live region. In-process
//! callers therefore retain the VEH guard around scans. `Option` and `Result`
//! remain the only contract for rejected addresses and OS failures.

pub mod discover;
pub mod gcguard;
pub mod hook;
pub mod il2cpp;
pub mod photon;
pub mod playerloop;

// Win32 primitives the domain modules reach for (`crate::raw::sys`,
// `crate::raw::memscan`), kept at the same paths they had in `hromatite-core`.
// Reached from Windows-only call sites (Photon resolution, thread ids).
#[cfg_attr(not(target_os = "windows"), allow(unused_imports))]
pub(crate) use hromatite_win::{memscan, sys};
