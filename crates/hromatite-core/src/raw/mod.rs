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
pub mod discover;
pub mod context;
pub mod ffi;
pub mod gcguard;
pub mod hook;
pub mod hostapi;
pub mod hot;
pub mod iat;
pub mod il2cpp;
pub mod localmap;
// EAC_M3_DUMP diagnostic (raw byte dumps of the class table for
// `~/EAC-RE/tools/m3dump_analyze.py`). Kept from `eac-cut`; the env-gated call
// site is not wired on master, so nothing references it yet.
#[allow(dead_code)]
pub mod m3dump;
pub mod memory;
pub mod memscan;
pub mod photon;
pub mod playerloop;
pub mod procs;
pub mod remote;
pub mod sys;
