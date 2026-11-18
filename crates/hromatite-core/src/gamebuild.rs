//! The one GameAssembly build whose constants this payload understands.
//!
//! Every RVA and byte signature under `raw/` and in [`crate::m3`] was measured
//! against a specific `GameAssembly.dll`. VRChat re-randomises the image on each
//! patch, so a game update must fail closed instead of installing a hook at an
//! address that now holds different code.
//!
//! Re-deriving the map (Ghidra disassembly plus one live read of the table /
//! class record) and bumping this string is a deliberate single edit: the
//! constants are never kept for two builds at once, because a half-updated map
//! is what produced "entry guard disabled: unsupported GameAssembly build"
//! followed by hooks that silently did nothing.

/// sha256 of `GameAssembly.dll` the constants in this crate belong to.
///
/// Measured 2026-11-17 (`build_26715337`); the previous build was
/// `648f61b13e4f6daf6b144d03ff0fcdd0b294ca4b812ade90b7f7f9fcbd25903b`.
pub const SUPPORTED_SHA256: &str = "245768f252146ef463b539e6c613b4955b7e829a4afd4dc0a2a2939aef3baadb";

/// Is the loaded game the build these constants were re-derived from?
pub fn is_supported() -> bool {
    crate::symbols::GAMEASSEMBLY_SHA256 == SUPPORTED_SHA256
}
