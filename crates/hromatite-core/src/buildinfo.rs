//! Which build are we attached to, and what are its numbers?
//!
//! Every per-build address and struct offset lives in
//! [`hromatite_abi::build`], keyed by the SHA-256 of `GameAssembly.dll`.
//! This module answers "which entry applies here" and provides the dynamic
//! fallback: when the loaded module is a build the table does not know, the
//! anchors are re-derived from the image itself (`raw::discover`) and the result
//! is written next to the log as `hromatite-build-<sha12>.json`, so
//! `tools/buildgen.py` can promote it into a descriptor without re-deriving
//! anything by hand.
//!
//! The compiled SHA in `symbols.rs` is only a *claim*: after a game update the
//! payload is usually older than the module it is mapped into.  Trusting that
//! claim would hand out the previous build's RVAs, so the descriptor is first
//! validated against the four byte shapes that the GC guard itself relies on
//! (cheap, no hashing); only when they do not match is the module hashed and
//! looked up in the table.  Callers must treat `None` as "do not install": a
//! wrong address in the GC guard or the il2cpp accessors is worse than a
//! disabled feature.

#[cfg(target_os = "windows")]
use std::sync::OnceLock;

use hromatite_abi::build::{self, Build, GcAnchors, Il2cppAnchors, Layout};

use crate::raw::discover;

/// SHA-256 of the build `symbols.rs` was generated from. After a game update
/// without a rebuild this is stale, which is why [`resolve`] validates the
/// loaded module before trusting it.
pub fn compiled_sha() -> &'static str {
    crate::symbols::GAMEASSEMBLY_SHA256
}

/// Descriptor for the compiled-in build, when the table knows it.  This is a
/// claim about the *source tree*, not about the running process; use
/// [`resolve`] when a decision depends on the loaded module.
pub fn selected() -> Option<&'static Build> {
    build::select(compiled_sha())
}

/// True when this binary carries numbers for the build it was compiled for.
pub fn compiled_build_supported() -> bool {
    selected().is_some()
}

// ---------------------------------------------------------------------------
// loaded-module identity
// ---------------------------------------------------------------------------

/// Byte shapes the GC guard checks before it patches anything; a module that
/// fails any of them is not the build this descriptor describes.
const SUSPEND_ENTRY_LEN: usize = 15;
const SKIP_CLOSE_LEN: usize = 6;
const AFTER_UNLINK_LEN: usize = 7;
const CONTEXT_CALL_LEN: usize = 14;

fn valid_suspend_entry(bytes: &[u8]) -> bool {
    bytes.len() == SUSPEND_ENTRY_LEN
        && bytes[0..2] == [0x48, 0x89]
        && bytes[3] == 0x24
        && bytes[5..7] == [0x48, 0x89]
        && bytes[8] == 0x24
        && bytes[10..12] == [0x48, 0x89]
        && bytes[13] == 0x24
}

fn valid_skip_close(bytes: &[u8]) -> bool {
    bytes.len() == SKIP_CLOSE_LEN && bytes[0] == 0xFF && bytes[1] == 0x15
}

fn valid_after_unlink(bytes: &[u8]) -> bool {
    bytes.len() == AFTER_UNLINK_LEN && bytes[0..3] == [0x48, 0x8D, 0x05]
}

fn valid_context_call(bytes: &[u8]) -> bool {
    bytes.len() == CONTEXT_CALL_LEN && bytes[0] == 0xFF && bytes[1] == 0x15
}

/// `il2cpp_get_corlib` is always `mov rax,[rip+d]; ret`.
fn valid_corlib_getter(bytes: &[u8]) -> bool {
    bytes.len() == 8 && bytes[0..3] == [0x48, 0x8B, 0x05] && bytes[7] == 0xC3
}

/// Do the descriptor's own anchors describe the module at `base`?
///
/// This is the cheap identity test: no hashing, four independent signatures.
/// It is written as a reader closure so the rule itself is testable without a
/// live process.
pub fn anchors_match<F>(build: &Build, mut read: F) -> bool
where
    F: FnMut(u32, usize) -> Option<Vec<u8>>,
{
    let gc = &build.gc;
    let checks = [
        (gc.suspend_entry, SUSPEND_ENTRY_LEN, valid_suspend_entry as fn(&[u8]) -> bool),
        (gc.skip_close, SKIP_CLOSE_LEN, valid_skip_close as fn(&[u8]) -> bool),
        (gc.after_unlink, AFTER_UNLINK_LEN, valid_after_unlink as fn(&[u8]) -> bool),
        (gc.context_call, CONTEXT_CALL_LEN, valid_context_call as fn(&[u8]) -> bool),
        (build.il2cpp.get_corlib, 8, valid_corlib_getter as fn(&[u8]) -> bool),
    ];
    for (rva, length, check) in checks {
        let Some(bytes) = read(rva, length) else {
            return false;
        };
        if !check(&bytes) {
            return false;
        }
    }
    true
}

#[cfg(target_os = "windows")]
fn module_anchor_match(base: usize, build: &Build) -> bool {
    anchors_match(build, |rva, length| {
        crate::safe::read_bytes(base.wrapping_add(rva as usize), length)
    })
}

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

/// Decode 64 lowercase hex characters; `None` on any malformed input.
pub fn decode_sha_hex(text: &str) -> Option<[u8; 32]> {
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, slot) in out.iter_mut().enumerate() {
        let high = hex_nibble(bytes[index * 2])?;
        let low = hex_nibble(bytes[index * 2 + 1])?;
        *slot = (high << 4) | low;
    }
    Some(out)
}

/// Hex SHA-256 of the loaded `GameAssembly.dll`, memoised for the process.
///
/// Fast path: when the compiled descriptor's own anchors still describe the
/// module, the compiled SHA is the answer and nothing is hashed.  Only after a
/// game update (anchors no longer match) is the image hashed, once.
#[cfg(target_os = "windows")]
pub fn loaded_sha_hex() -> Option<&'static str> {
    static LOADED: OnceLock<Option<String>> = OnceLock::new();
    LOADED
        .get_or_init(|| {
            let (base, size) = crate::procs::own_module(crate::GAME_ASSEMBLY)?;
            if let Some(build) = selected() {
                if module_anchor_match(base, build) {
                    return Some(compiled_sha().to_string());
                }
                crate::log::log_tag(
                    "buildgen",
                    &format!(
                        "loaded GameAssembly does not match the compiled descriptor {} \
                         (anchors moved); identifying it by hash",
                        &compiled_sha()[..compiled_sha().len().min(12)]
                    ),
                );
            }
            // The descriptor table is keyed by the *file* hash, and a mapped
            // image is relocated (imports, .data pointers), so hashing the
            // pages would never reproduce it.  Hash the file the module was
            // loaded from instead; `base`/`size` are only used above.
            let _ = (base, size);
            let path = crate::procs::own_module_path(crate::GAME_ASSEMBLY)?;
            let started = std::time::Instant::now();
            let hex = crate::sha256::hex_digest_file(&path).ok()?;
            crate::log::log_tag(
                "buildgen",
                &format!(
                    "loaded GameAssembly sha256={} (hashed {} in {} ms)",
                    &hex[..12],
                    path.display(),
                    started.elapsed().as_millis()
                ),
            );
            Some(hex)
        })
        .as_deref()
}

/// The loaded module's SHA-256; host builds have no module to identify.
#[cfg(not(target_os = "windows"))]
pub fn loaded_sha_hex() -> Option<&'static str> {
    None
}

/// The descriptor that applies to the module at `base`, if one can be trusted.
///
/// Order: the compiled descriptor when its anchors still match; otherwise the
/// table entry for the loaded SHA; otherwise `None` (callers fall back to
/// `raw::discover` or refuse to patch).
pub fn resolve(base: usize, size: usize) -> Option<&'static Build> {
    #[cfg(target_os = "windows")]
    {
        let _ = size;
        if let Some(build) = selected()
            && module_anchor_match(base, build)
        {
            return Some(build);
        }
    }
    let _ = (base, size);
    build::select(loaded_sha_hex()?)
}

/// GC anchors: from the descriptor that matches the loaded module, otherwise
/// derived from the image. `None` means "no reliable anchors — do not patch".
pub fn gc_anchors(module_base: usize, module_size: usize) -> Option<GcAnchors> {
    if let Some(build) = resolve(module_base, module_size) {
        return Some(build.gc);
    }
    let anchors = discover::gc_anchors(module_base, module_size)?;
    write_discovery_cache(module_base, module_size);
    Some(anchors)
}

/// il2cpp anchors, same policy as [`gc_anchors`].
pub fn il2cpp_anchors(module_base: usize, module_size: usize) -> Option<Il2cppAnchors> {
    if let Some(build) = resolve(module_base, module_size) {
        return Some(build.il2cpp);
    }
    let anchors = discover::il2cpp_anchors(module_base, module_size)?;
    write_discovery_cache(module_base, module_size);
    Some(anchors)
}

/// Struct offsets for the loaded build (`None` = unknown build, features must
/// refuse rather than guess).
pub fn layout_for(module_base: usize, module_size: usize) -> Option<Layout> {
    resolve(module_base, module_size).map(|build| build.layout)
}

/// Struct offsets for the *compiled* build; kept for callers that only need to
/// know what the source tree carries.
pub fn layout() -> Option<Layout> {
    selected().map(|build| build.layout)
}

// ---------------------------------------------------------------------------
// discovery cache
// ---------------------------------------------------------------------------

/// Where `tools/buildgen.py` expects the runtime artefact.
pub fn cache_path(sha12: &str) -> std::path::PathBuf {
    let base = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:".to_string());
    std::path::PathBuf::from(base).join(format!("hromatite-build-{sha12}.json"))
}

fn hex_address(value: u32) -> String {
    format!("{value:#x}")
}

/// Same schema as `tools/data/build-<sha12>.json`, minus the layouts that only
/// the offline generator can prove.
pub fn render_discovery_cache(
    sha: &str,
    gc: &GcAnchors,
    il2cpp: Option<&Il2cppAnchors>,
) -> String {
    let gc_json = format!(
        "{{\"remove_thread\": \"{}\", \"skip_close\": \"{}\", \"unlinked_next\": \"{}\", \
         \"after_unlink\": \"{}\", \"suspend_entry\": \"{}\", \"context_call\": \"{}\", \
         \"process_record\": \"{}\", \"resume_abort\": \"{}\", \"suspend_abort\": \"{}\", \
         \"advance_bucket\": \"{}\", \"threads\": \"{}\", \"thread_list_cs\": \"{}\"}}",
        hex_address(gc.remove_thread),
        hex_address(gc.skip_close),
        hex_address(gc.unlinked_next),
        hex_address(gc.after_unlink),
        hex_address(gc.suspend_entry),
        hex_address(gc.context_call),
        hex_address(gc.process_record),
        hex_address(gc.resume_abort),
        hex_address(gc.suspend_abort),
        hex_address(gc.advance_bucket),
        hex_address(gc.threads),
        hex_address(gc.thread_list_cs),
    );
    let il2cpp_json = match il2cpp {
        Some(anchors) => format!(
            "{{\"get_corlib\": \"{}\", \"image_get_class\": \"{}\", \"class_table\": \"{}\"}}",
            hex_address(anchors.get_corlib),
            hex_address(anchors.image_get_class),
            hex_address(anchors.class_table),
        ),
        None => "{}".to_string(),
    };
    format!(
        "{{\"schema\": \"hromatite/build/1\", \"source\": \"runtime raw::discover\", \
         \"sha256\": \"{sha}\", \"unity\": \"unknown\", \"gc\": {gc_json}, \
         \"il2cpp\": {il2cpp_json}, \"layout\": {{}}, \
         \"note\": \"derived at runtime; struct layouts are not discoverable, \
         run tools/buildgen.py to promote this into a descriptor\"}}"
    )
}

/// Write the cache once per process when the loaded build is unknown.  Failure
/// is logged, never fatal: the anchors already installed stay valid.
#[cfg(target_os = "windows")]
fn write_discovery_cache(module_base: usize, module_size: usize) {
    static ONCE: OnceLock<()> = OnceLock::new();
    let first = ONCE.set(()).is_ok();
    let Some(sha) = loaded_sha_hex() else {
        return;
    };
    if !first || build::select(sha).is_some() {
        return;
    }
    let gc = discover::gc_anchors(module_base, module_size);
    let il2cpp = discover::il2cpp_anchors(module_base, module_size);
    let Some(gc) = gc else {
        crate::log::log_tag(
            "buildgen",
            "unknown build: GC anchors were not discovered, nothing to cache",
        );
        return;
    };
    let path = cache_path(&sha[..sha.len().min(12)]);
    let json = render_discovery_cache(sha, &gc, il2cpp.as_ref());
    match std::fs::write(&path, json) {
        Ok(()) => crate::log::log_tag(
            "buildgen",
            &format!("unknown build {sha}: discovery cached at {}", path.display()),
        ),
        Err(error) => crate::log::log_tag(
            "buildgen",
            &format!("unknown build {sha}: cannot write {} ({error})", path.display()),
        ),
    }
}

/// Host builds never discover anchors, so there is nothing to cache.
#[cfg(not(target_os = "windows"))]
fn write_discovery_cache(_module_base: usize, _module_size: usize) {}

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

    fn descriptor() -> &'static Build {
        build::newest()
    }

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

    fn shapes(build: &Build, rva: u32, length: usize, good_entry: bool) -> Option<Vec<u8>> {
        let gc = &build.gc;
        let mut bytes: Vec<u8> = match rva {
            r if r == gc.suspend_entry => {
                let mut entry = vec![0x48, 0x89, 0x5C, 0x24, 0x08];
                entry.extend_from_slice(&[0x48, 0x89, 0x5C, 0x24, 0x10]);
                entry.extend_from_slice(&[0x48, 0x89, 0x5C, 0x24, 0x18]);
                if !good_entry {
                    entry[1] = 0x90;
                }
                entry
            }
            r if r == gc.skip_close => vec![0xFF, 0x15, 0, 0, 0, 0],
            r if r == gc.after_unlink => vec![0x48, 0x8D, 0x05, 0, 0, 0, 0],
            r if r == gc.context_call => vec![
                0xFF, 0x15, 0, 0, 0, 0, 0x85, 0xC0, 0x0F, 0x85, 0, 0, 0, 0,
            ],
            r if r == build.il2cpp.get_corlib => vec![0x48, 0x8B, 0x05, 0, 0, 0, 0, 0xC3],
            _ => return None,
        };
        bytes.resize(length, 0);
        Some(bytes)
    }

    #[test]
    fn anchors_match_accepts_the_described_shapes() {
        let build = descriptor();
        assert!(anchors_match(build, |rva, length| shapes(build, rva, length, true)));
    }

    #[test]
    fn anchors_match_rejects_a_moved_anchor() {
        let build = descriptor();
        // Every shape right except the collector entry: this is what a new
        // build looks like -- one relocated function is enough to reject.
        assert!(!anchors_match(build, |rva, length| shapes(build, rva, length, false)));
    }

    #[test]
    fn unreadable_pages_mean_no_match() {
        assert!(!anchors_match(descriptor(), |_, _| None));
    }

    #[test]
    fn decode_sha_hex_round_trips_and_rejects_garbage() {
        let decoded = decode_sha_hex(&"0123456789abcdef".repeat(4)).expect("valid hex");
        assert_eq!(&decoded[..8], &[0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef]);
        assert!(decode_sha_hex(&"0123456789abcdef".repeat(4)[..63]).is_none());
        assert!(decode_sha_hex(&("zz".repeat(32))).is_none());
    }

    #[test]
    fn cache_path_uses_the_profile_and_the_short_sha() {
        let path = cache_path("245768f25214");
        assert!(path.to_string_lossy().ends_with("hromatite-build-245768f25214.json"));
    }

    #[test]
    fn discovery_cache_is_valid_json_with_the_expected_sections() {
        let build = descriptor();
        let json = render_discovery_cache(build.sha256, &build.gc, Some(&build.il2cpp));
        assert!(json.contains("\"schema\": \"hromatite/build/1\""));
        assert!(json.contains("\"source\": \"runtime raw::discover\""));
        assert!(json.contains("\"remove_thread\""));
        assert!(json.contains("\"class_table\""));
        assert!(json.contains("\"layout\": {}"));
        assert!(json.contains(build.sha256));
    }
}
