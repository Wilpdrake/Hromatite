//! Unified x64 hooking engine for Hromatite.
//!
//! Layers:
//! * [`decode`] — x64 instruction length and operand-offset decoder (pure Rust).
//! * [`tramp`] — 2-pass trampoline planner and relocator (pure Rust).
//! * [`mem`] — near-target allocation, protection brackets, cache flush, and
//!   atomic 8B / 16B (`lock cmpxchg16b`) window stores.
//! * [`table`] — process-wide named shared section (`Local\hromatite_rust_hooktable_v1`).
//! * [`inline`] — 5-byte `jmp rel32` + relay page entry hook with `unhook()`.
//! * [`abs`] — 12-byte `mov rax, imm64 ; jmp rax` entry hook with shared table reuse.
//! * [`midhook`] — mid-function 5-byte `jmp rel32` + relay page hook.
//! * [`hookset`] — process-wide registry for entry and mid-function hooks.
//! * [`vmt`] — virtual-method-table slot swapper.
//! * [`iat`] — import-address-table reader and slot patcher.
//!
//! Zero dependencies on Unity, IL2CPP, `LateUpdate`, `EOSSDK`, `console`, or `hot`.

#![deny(unsafe_code)]

#[allow(unsafe_code)]
pub mod abs;
pub mod decode;
pub mod hookset;
#[allow(unsafe_code)]
pub mod iat;
#[allow(unsafe_code)]
pub mod inline;
#[allow(unsafe_code)]
pub mod mem;
#[allow(unsafe_code)]
pub mod midhook;
#[allow(unsafe_code)]
pub mod table;
pub mod tramp;
#[allow(unsafe_code)]
pub mod vmt;

pub use abs::{Hook as AbsHook, PATCH_LEN as ABS_PATCH_LEN, install as install_abs};
pub use decode::{Insn, decode, decode_one, decode_span, prologue_len};
pub use hookset::{HookError, HookSet, Kind as HookKind};
pub use inline::{Hook as InlineHook, Prep, RELAY_OFF, hook_inline, patch, prepare, prepare_span};
pub use midhook::{MidHook, install as install_mid};
pub use vmt::{Live as VmtLive, TableMem, Vmt, VmtHook};
