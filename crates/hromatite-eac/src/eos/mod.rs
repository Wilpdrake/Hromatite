//! EOSSDK anti-cheat export hooks.
//!
//! The module is split so the pure protocol core can be built (and linked by a
//! hot-reloadable feature image) with `default-features = false`:
//!
//! * [`state`] — always compiled: [`HookEvent`], [`Counters`], [`Report`],
//!   [`Hooks`] and the global atomics `Hooks::counters` / `Hooks::status_lines`
//!   read. No Win32, no `hromatite-win`, no `hromatite-hook`.
//! * `hooks` — behind the `runtime` feature (on by default): the install path,
//!   the twelve detours, the IAT walk and the BoringSSL EVP probe.
//!
//! `Hooks::install` therefore has two faces: the real one in `hooks` on
//! `windows` + `runtime`, and the `Err(..)` sibling in [`state`] everywhere
//! else, so `Hooks::poll` has a well-defined answer in every build.

mod state;

pub use state::*;

#[cfg(feature = "runtime")]
mod hooks;

// The runtime half's public entry points are re-exported here so a caller keeps
// writing `hromatite_eac::eos::install_iat` / `...::module_report`; with the
// feature off they do not exist (the pure build has no OS layer to probe).
#[cfg(feature = "runtime")]
pub use hooks::{install_iat, module_report};
