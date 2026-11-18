//! In-game rendering: nuklear panels drawn by the game's own D3D11 device.
//!
//! Everything graphics-facing lives behind this crate boundary:
//!
//! * [`nk`] - the vendored nuklear immediate-mode UI (layout, input model,
//!   draw commands) plus the C shim it talks to ([`sys`]);
//! * [`kiero`] - the `IDXGISwapChain::Present` VMT hook and the swapchain
//!   locator, so a frame can be drawn once per presented frame;
//! * [`d3d11`] - the D3D11 backend that turns one [`nk::Frame`] into
//!   `DrawIndexed` calls and restores the game's pipeline state afterwards;
//! * [`ui`] - the resident-side install/toggle/status surface.
//!
//! No other crate in the workspace links a graphics library. `hromatite-runtime`
//! only calls [`ui::install`], [`ui::enabled`] and [`ui::status`].
//!
//! # Safety posture
//!
//! The crate root denies `unsafe_code`; the modules that must call into COM or
//! the C shim are the explicit, documented exceptions. Every unsafe block
//! carries a `// SAFETY:` justification for the value and lifetime it assumes.

#![deny(unsafe_code)]

#[allow(unsafe_code)]
pub mod d3d11;
// Only the Windows D3D11 path consumes the precompiled shader blobs.
#[allow(unsafe_code)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) mod d3d11_shaders;
#[allow(unsafe_code)]
pub mod kiero;
#[allow(unsafe_code)]
pub mod nk;
// The C shim is a boundary: it covers the whole widget set the panels may need,
// not only what today's layout happens to call, so unused entries are expected.
#[allow(unsafe_code)]
#[allow(dead_code)]
pub(crate) mod sys;
#[allow(unsafe_code)]
pub mod ui;

pub use nk::{DrawCommand, Frame, FrameStats, Input, Keys, Nk, Ui, Vertex};
pub use ui::{enabled as ui_enabled, install as install_ui, status as ui_status};
