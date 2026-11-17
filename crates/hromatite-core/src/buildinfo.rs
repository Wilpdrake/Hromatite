//! Which build are we attached to, and what are its numbers?
//!
//! Every per-build address and struct offset lives in
//! [`hromatite_abi::build`], keyed by the SHA-256 of `GameAssembly.dll`.
//! This module answers "which entry applies here" and provides the dynamic
//! fallback: when the loaded module is a build the table does not know, the
//! anchors are re-derived from the image itself (`raw::discover`).
//!
//! Callers must treat `None` as "do not install": a wrong address in the GC
//! guard or the il2cpp accessors is worse than a disabled feature.

use hromatite_abi::build::{self, Build, GcAnchors, Il2cppAnchors, Layout};

use crate::raw::discover;

/// SHA-256 of the build `symbols.rs` was generated from. After a game update
/// without a rebuild this is stale, which is why [`selected`] also re-checks the
/// loaded module before trusting it.
pub fn compiled_sha() -> &'static str {
    crate::symbols::GAMEASSEMBLY_SHA256
}

/// Descriptor for the compiled-in build, when the table knows it.
pub fn selected() -> Option<&'static Build> {
    build::select(compiled_sha())
}

/// True when this binary carries numbers for the build it was compiled for.
pub fn compiled_build_supported() -> bool {
    selected().is_some()
}

/// GC anchors: from the table when the build is known, otherwise derived from
/// the loaded image. `None` means "no reliable anchors — do not patch".
pub fn gc_anchors(module_base: usize, module_size: usize) -> Option<GcAnchors> {
    if let Some(build) = selected() {
        return Some(build.gc);
    }
    discover::gc_anchors(module_base, module_size)
}

/// il2cpp anchors, same policy as [`gc_anchors`].
pub fn il2cpp_anchors(module_base: usize, module_size: usize) -> Option<Il2cppAnchors> {
    if let Some(build) = selected() {
        return Some(build.il2cpp);
    }
    discover::il2cpp_anchors(module_base, module_size)
}

/// Struct offsets for the loaded build (`None` = unknown build).
pub fn layout() -> Option<Layout> {
    selected().map(|build| build.layout)
}

/// One line for the startup log: what we think we are attached to.
pub fn describe() -> String {
    match selected() {
        Some(build) => format!(
            "build {} (unity {}), verified={}{}",
            &build.sha256[..12],
            build.unity,
            build.verified,
            if build.note.is_empty() {
                String::new()
            } else {
                format!(", note: {}", build.note)
            }
        ),
        None => format!(
            "unknown build {} (no descriptor; anchors will be discovered at runtime)",
            &compiled_sha()[..compiled_sha().len().min(12)]
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_compiled_build_is_described_in_the_table() {
        // The repository always ships symbols.rs generated from one of the
        // builds in the table; a mismatch means the table was not updated
        // together with the generator output.
        assert!(
            compiled_build_supported(),
            "symbols.rs sha {} has no descriptor in hromatite_abi::build::KNOWN",
            compiled_sha()
        );
    }

    #[test]
    fn layout_matches_the_descriptor() {
        let build = selected().expect("supported build");
        let layout = layout().expect("layout");
        assert_eq!(layout.class_name, build.layout.class_name);
        assert_eq!(layout.method_name, build.layout.method_name);
    }

    #[test]
    fn description_never_panics_on_a_short_sha() {
        assert!(!describe().is_empty());
    }
}
