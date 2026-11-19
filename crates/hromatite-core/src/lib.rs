//! Transitional facade over [`hromatite_il2cpp`].
//!
//! The IL2CPP domain moved to its own crate; this shim only exists so that the
//! `hromatite-eac` dependency can be repointed in a separate, reviewable step.
//! Nothing may add code here: the crate is scheduled for deletion.

#![deny(unsafe_code)]

pub use hromatite_il2cpp::*;
pub use hromatite_render::{kiero, ui};
