//! Low-level implementation boundary.
//!
//! Every module that knowingly performs foreign calls, arbitrary-memory access,
//! manual machine-code patching or context restoration lives here. Higher-level
//! modules consume safe facades and do not contain `unsafe` blocks.
//!
//! Safe does not mean infallible: a `VirtualQuery` validates a point in time,
//! while another thread may still unmap or reprotect a live region. In-process
//! callers therefore retain the existing VEH guard around scans. `Option` and
//! `Result` remain the only contract for rejected addresses and OS failures.

pub mod console;
pub mod context;
pub mod ffi;
pub mod gcguard;
pub mod hook;
pub mod hostapi;
pub mod hot;
pub mod il2cpp;
pub mod localmap;
pub mod m3dump;
pub mod memory;
pub mod photon;
pub mod playerloop;
pub mod procs;
pub mod remote;
pub mod sys;
