//! Per-build descriptors: every address and struct offset the runtime needs.
//!
//! The game re-obfuscates its exports and relinks `GameAssembly.dll` on every
//! update, so a number that is correct for one build is a crash for the next.
//! Instead of scattering constants through the code, each supported build is
//! described here once, keyed by the module's SHA-256, and the runtime picks the
//! entry that matches the module it actually loaded.  Unknown builds are
//! resolved dynamically (`hromatite-core::raw::discover`) and, if that fails,
//! the affected feature refuses to install instead of guessing.
//!
//! Fields marked `verified` were read out of the shipped binary (Ghidra
//! disassembly, cross-checked against the functions that must use them); an
//! entry with `verified: false` must not be trusted blindly.

/// Boehm GC anchors inside `GameAssembly.dll`, as RVAs.
///
/// The two intra-function sites (`skip_close`, `after_unlink`, `unlinked_next`)
/// and the two abort sites are offsets into functions whose code is unchanged
/// between builds, so they keep their distance from the function entry.
#[derive(Clone, Copy, Debug)]
pub struct GcAnchors {
    /// `GC_remove_thread(tid)` — bucket walk + CloseHandle + free.
    pub remove_thread: u32,
    /// `call [CloseHandle]` inside `remove_thread` (patched to skip the close).
    pub skip_close: u32,
    /// Instruction after that call: the resume point when the close is skipped.
    pub unlinked_next: u32,
    /// `lea rax,[sentinel]` after the unlink; the close runs here instead.
    pub after_unlink: u32,
    /// `GC_suspend_all` — walks `GC_threads[]` under the stop-the-world lock.
    pub suspend_entry: u32,
    /// `call [GetThreadContext]; test eax,eax; jne` inside `GC_suspend_all`.
    pub context_call: u32,
    /// Per-record processing label inside `GC_suspend_all`.
    pub process_record: u32,
    /// `int3` of `ABORT("ResumeThread failed")`.
    pub resume_abort: u32,
    /// `int3` of `ABORT("SuspendThread loop failed")`.
    pub suspend_abort: u32,
    /// Bucket advance (`add r15,8`) inside `GC_suspend_all`.
    pub advance_bucket: u32,
    /// `GC_threads[256]` — array of bucket heads.
    pub threads: u32,
    /// Boehm thread-list critical section.
    pub thread_list_cs: u32,
}

/// il2cpp entry points the runtime calls directly, as RVAs.
#[derive(Clone, Copy, Debug)]
pub struct Il2cppAnchors {
    /// `il2cpp_get_corlib` — returns the corlib `Il2CppImage*`.
    pub get_corlib: u32,
    /// `il2cpp_image_get_class(image, index)`.
    pub image_get_class: u32,
    /// `s_TypeInfoDefinitionTable` slot (holds `Il2CppClass**`).
    pub class_table: u32,
}

/// Struct offsets that differ between builds, validated against the functions
/// that must read them.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub class_parent: u16,
    pub class_namespace: u16,
    pub class_methods: u16,
    pub class_fields: u16,
    pub class_name: u16,
    pub class_fields_count: u16,
    pub field_stride: u16,
    /// Offset of `FieldInfo.name` inside one field record.
    pub field_name: u16,
    /// Offset of `FieldInfo.type`.
    pub field_type: u16,
    /// Offset of `FieldInfo.offset` (int32).
    pub field_offset: u16,
    /// Offset of `FieldInfo.parent`, `0` when the build no longer stores it.
    pub field_parent: u16,
    pub method_name: u16,
    pub method_return_type: u16,
    pub method_parameters: u16,
    pub method_flags: u16,
    pub method_param_count: u16,
}

/// One supported game build.
pub struct Build {
    /// Lowercase hex SHA-256 of `GameAssembly.dll`.
    pub sha256: &'static str,
    /// Unity version string, for the journal only.
    pub unity: &'static str,
    pub gc: GcAnchors,
    pub il2cpp: Il2cppAnchors,
    pub layout: Layout,
    /// True when every number below was proven against this exact binary.
    pub verified: bool,
    /// Human note about what is *not* proven, if anything.
    pub note: &'static str,
}

/// The build measured on 2026-11-14 (Unity 6000.0.67f1, names in
/// `tools/data/il2cpp-export-names.json`).
pub const BUILD_648F61B1: Build = Build {
    sha256: "648f61b13e4f6daf6b144d03ff0fcdd0b294ca4b812ade90b7f7f9fcbd25903b",
    unity: "6000.0.67f1",
    gc: GcAnchors {
        remove_thread: 0x00c4_17b0,
        skip_close: 0x00c4_17f0,
        unlinked_next: 0x00c4_17f6,
        after_unlink: 0x00c4_1818,
        suspend_entry: 0x00c4_1a20,
        context_call: 0x00c4_1ad2,
        process_record: 0x00c4_1a90,
        resume_abort: 0x00c4_1afb,
        suspend_abort: 0x00c4_1b65,
        advance_bucket: 0x00c4_1c6a,
        threads: 0x0e6b_b180,
        thread_list_cs: 0x0e6b_b100,
    },
    il2cpp: Il2cppAnchors {
        get_corlib: 0x00c2_7f00,
        image_get_class: 0x00c2_a330,
        class_table: 0x0e66_c870,
    },
    layout: Layout {
        class_parent: 0x10,
        class_namespace: 0x18,
        class_methods: 0x50,
        class_fields: 0x80,
        class_name: 0x98,
        class_fields_count: 0x124,
        field_stride: 0x28,
        field_name: 0x10,
        field_type: 0x00,
        field_offset: 0x08,
        field_parent: 0x20,
        method_name: 0x18,
        method_return_type: 0x20,
        method_parameters: 0x30,
        method_flags: 0x4c,
        method_param_count: 0x52,
    },
    verified: true,
    note: "",
};

/// The build shipped on 2026-11-17 (same Unity version, new link).
///
/// Addresses were derived from Ghidra (`/11.17/GameAssembly.dll`) and from the
/// finder in `hromatite-core::raw::discover`, which reproduces this build's
/// anchors from bare byte patterns.  `thread_list_cs` is the one number that is
/// inferred (table − 0xa0, the only critical-section-sized slot before the table
/// that the binary references) rather than proven.
pub const BUILD_245768F2: Build = Build {
    sha256: "245768f252146ef463b539e6c613b4955b7e829a4afd4dc0a2a2939aef3baadb",
    unity: "6000.0.67f1",
    gc: GcAnchors {
        remove_thread: 0x00c4_3520,
        skip_close: 0x00c4_3560,
        unlinked_next: 0x00c4_3566,
        after_unlink: 0x00c4_3588,
        suspend_entry: 0x00c4_3790,
        context_call: 0x00c4_3842,
        process_record: 0x00c4_3800,
        resume_abort: 0x00c4_386b,
        suspend_abort: 0x00c4_38d5,
        advance_bucket: 0x00c4_39da,
        threads: 0x0e6b_d8a0,
        thread_list_cs: 0x0e6b_d800,
    },
    il2cpp: Il2cppAnchors {
        get_corlib: 0x00c2_9c70,
        image_get_class: 0x00c2_c0a0,
        class_table: 0x0e66_f0d0,
    },
    layout: Layout {
        class_parent: 0xa0,
        class_namespace: 0x18,
        class_methods: 0x58,
        class_fields: 0x88,
        class_name: 0x98,
        class_fields_count: 0x124,
        field_stride: 0x20,
        field_name: 0x00,
        field_type: 0x10,
        field_offset: 0x08,
        field_parent: 0x00,
        method_name: 0x20,
        method_return_type: 0x28,
        method_parameters: 0x30,
        method_flags: 0x4c,
        method_param_count: 0x52,
    },
    verified: false,
    note: "gc.thread_list_cs inferred (table-0xa0); layout.method_return_type followed the +8 shift of name",
};

/// Every build the runtime knows statically, newest last.
pub static KNOWN: &[Build] = &[BUILD_648F61B1, BUILD_245768F2];

/// Pick the descriptor for a GameAssembly SHA-256 (lowercase hex).
pub fn select(sha256_hex: &str) -> Option<&'static Build> {
    KNOWN.iter().find(|build| build.sha256.eq_ignore_ascii_case(sha256_hex))
}

/// Newest known build, used only as a last-resort default for diagnostics.
pub fn newest() -> &'static Build {
    KNOWN.last().expect("KNOWN is never empty")
}

/// Decode a lowercase hex SHA-256 into its 32 bytes.
pub fn sha_bytes(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    let bytes = hex.as_bytes();
    for (index, slot) in out.iter_mut().enumerate() {
        let high = nibble(bytes.get(index * 2).copied().unwrap_or(b'0'));
        let low = nibble(bytes.get(index * 2 + 1).copied().unwrap_or(b'0'));
        *slot = (high << 4) | low;
    }
    out
}

fn nibble(value: u8) -> u8 {
    match value {
        b'0'..=b'9' => value - b'0',
        b'a'..=b'f' => value - b'a' + 10,
        b'A'..=b'F' => value - b'A' + 10,
        _ => 0,
    }
}

/// SHA-256 bytes of the newest known build (test fixtures and diagnostics).
pub fn newest_sha_bytes() -> [u8; 32] {
    sha_bytes(newest().sha256)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_each_known_build_case_insensitively() {
        for build in KNOWN {
            let upper = build.sha256.to_ascii_uppercase();
            let picked = select(&upper).expect("known build must resolve");
            assert_eq!(picked.sha256, build.sha256);
        }
    }

    #[test]
    fn rejects_unknown_sha() {
        assert!(select("0000000000000000000000000000000000000000000000000000000000000000").is_none());
    }

    #[test]
    fn gc_anchor_offsets_stay_consistent_inside_the_located_functions() {
        for build in KNOWN {
            let gc = &build.gc;
            assert_eq!(gc.skip_close - gc.remove_thread, 0x40, "skip_close");
            assert_eq!(gc.unlinked_next - gc.remove_thread, 0x46, "unlinked_next");
            assert_eq!(gc.after_unlink - gc.remove_thread, 0x68, "after_unlink");
            assert_eq!(gc.context_call - gc.suspend_entry, 0xb2, "context_call");
            assert_eq!(gc.process_record - gc.suspend_entry, 0x70, "process_record");
            assert_eq!(gc.resume_abort - gc.suspend_entry, 0xdb, "resume_abort");
            assert_eq!(gc.suspend_abort - gc.suspend_entry, 0x145, "suspend_abort");
            assert_eq!(gc.advance_bucket - gc.suspend_entry, 0x24a, "advance_bucket");
            assert!(gc.thread_list_cs < gc.threads);
        }
    }

    #[test]
    fn struct_offsets_are_word_aligned_and_ordered() {
        for build in KNOWN {
            let l = &build.layout;
            assert_eq!(l.class_namespace % 8, 0);
            assert!(l.class_fields < l.class_name);
            assert_ne!(l.class_name, l.class_parent);
            assert!(l.field_stride >= 0x20);
            assert!(l.field_name < l.field_stride && l.field_type < l.field_stride);
            assert!(l.field_offset < l.field_stride);
            assert!(l.method_name < l.method_return_type);
            assert!(l.method_return_type < l.method_parameters);
        }
    }
}
