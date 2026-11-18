//! Build the vendored nuklear implementation plus the thin C boundary that the
//! Rust side talks to.
//!
//! `nuklear.h` is a single-header library: exactly one translation unit defines
//! `NK_IMPLEMENTATION`, and that is `nuklear_shim.c`. Nothing else in the crate
//! (or in the payload) includes it.

use std::env;
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo"));
    let shim = manifest.join("nuklear_shim.c");
    let vendor = manifest.join("third_party").join("nuklear");

    cc::Build::new()
        .file(&shim)
        .include(&vendor)
        .std("c99")
        .compile("nuklear_shim");

    // nuklear.h is ~31k lines; rebuilding it on every cargo invocation would be
    // wasteful, but it must not be missed either.
    println!("cargo:rerun-if-changed={}", shim.display());
    println!("cargo:rerun-if-changed={}", vendor.join("nuklear.h").display());
}
