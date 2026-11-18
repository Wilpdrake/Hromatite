//! hromatite-nuklear - immediate-mode in-game UI built on vendored nuklear.
//!
//! The crate is deliberately independent of the graphics hook and of
//! `hromatite-core`: it owns a nuklear context, lays out the panels and hands
//! the caller plain draw commands. Rendering them is somebody else's job.
//!
//! # Safety posture
//!
//! All `unsafe` lives in the [`sys`] module, which is the only place that
//! touches the C shim. Everything above it is safe Rust.
#![deny(unsafe_code)]

#[allow(unsafe_code)]
mod sys;

// The safe wrapper (`Nk`, `Ui`, `Input`, `Frame`, `DrawCommand`, `Vertex`) is
// added next; this stub pins the lint posture and the module layout.
