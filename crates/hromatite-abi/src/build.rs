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
    /// `il2cpp_class_from_name(image, namespace, name)` — one call instead of
    /// enumerating the image's classes when the pinned type index no longer
    /// applies to a newer build.
    pub class_from_name: u32,
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
    /// `Il2CppImage` record stride in the contiguous image table.
    ///
    /// The table itself is contiguous and `il2cpp_get_corlib` returns one
    /// record inside it, so the stride is the only way to walk to another
    /// assembly: `0x48` on `648f61b1…`, `0x50` on `245768f2…` (measured live).
    pub image_stride: u16,
    /// Offset of the short assembly name (`char*`) inside one image record.
    pub image_name: u16,
    /// Offset of the file name (`char*`, `*.dll`) inside one image record;
    /// `0` when the build carries it elsewhere.
    pub image_filename: u16,
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
    /// True when every value the runtime needs is present and was produced by a
    /// shape rule proved on a reference binary; any value that is inferred
    /// rather than read out of the code is named in `note`.
    pub verified: bool,
    /// Human note about what is *not* proven, if anything.
    pub note: &'static str,
}

// --- buildgen:begin (kept in sync by tools/buildgen.py) ---
/// Build `648f61b13e4f` (Unity 6000.0.67f1), generated by `tools/buildgen.py`.
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
        class_from_name: 0x00c2_8050,
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
        image_stride: 0x48,
        image_name: 0x08,
        image_filename: 0x40,
    },
    verified: true,
    note: "method_return_type: 0x20 (heuristic: name+8; no function reads it in this build); gc.thread_list_cs: most-referenced 8-byte-aligned lea target before the table (heuristic)",
};

/// Build `245768f25214` (Unity 6000.0.67f1-DWR), generated by `tools/buildgen.py`.
pub const BUILD_245768F2: Build = Build {
    sha256: "245768f252146ef463b539e6c613b4955b7e829a4afd4dc0a2a2939aef3baadb",
    unity: "6000.0.67f1-DWR",
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
        class_from_name: 0x00c2_9dc0,
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
        image_stride: 0x50,
        image_name: 0x08,
        image_filename: 0x18,
    },
    verified: true,
    note: "field_parent: 0 (no il2cpp_field_get_parent export on this build); method_return_type: 0x28 (heuristic: name+8; no function reads it in this build); image_stride: 0x50 and image_filename: 0x18 measured live on pid 1071305 (the 0x48/+0x40 layout of the previous build breaks image_of); gc.thread_list_cs: most-referenced 8-byte-aligned lea target before the table (heuristic)",
};

pub static KNOWN: &[Build] = &[
    BUILD_648F61B1,
    BUILD_245768F2,
];
// --- buildgen:end ---

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

    /// The oracle `tools/test_buildgen.py` also checks: every number below was
    /// derived by hand (Ghidra + a live VRChat session) before the generator
    /// existed.  A descriptor edit that changes one of them without new
    /// evidence fails here, not in the game.
    #[test]
    fn reference_builds_match_the_hand_derived_oracle() {
        let old = select("648f61b13e4f6daf6b144d03ff0fcdd0b294ca4b812ade90b7f7f9fcbd25903b")
            .expect("previous build");
        assert_eq!(old.gc.remove_thread, 0xc4_17b0);
        assert_eq!(old.gc.skip_close, 0xc4_17f0);
        assert_eq!(old.gc.unlinked_next, 0xc4_17f6);
        assert_eq!(old.gc.after_unlink, 0xc4_1818);
        assert_eq!(old.gc.suspend_entry, 0xc4_1a20);
        assert_eq!(old.gc.context_call, 0xc4_1ad2);
        assert_eq!(old.gc.process_record, 0xc4_1a90);
        assert_eq!(old.gc.resume_abort, 0xc4_1afb);
        assert_eq!(old.gc.suspend_abort, 0xc4_1b65);
        assert_eq!(old.gc.advance_bucket, 0xc4_1c6a);
        assert_eq!(old.gc.threads, 0xe6b_b180);
        assert_eq!(old.gc.thread_list_cs, 0xe6b_b100);
        assert_eq!(old.il2cpp.get_corlib, 0xc2_7f00);
        assert_eq!(old.il2cpp.image_get_class, 0xc2_a330);
        assert_eq!(old.il2cpp.class_from_name, 0xc2_8050);
        assert_eq!(old.il2cpp.class_table, 0xe66_c870);
        assert_eq!(old.layout.class_parent, 0x10);
        assert_eq!(old.layout.class_methods, 0x50);
        assert_eq!(old.layout.class_fields, 0x80);
        assert_eq!(old.layout.field_stride, 0x28);
        assert_eq!(old.layout.field_name, 0x10);
        assert_eq!(old.layout.field_type, 0x00);
        assert_eq!(old.layout.field_offset, 0x08);
        assert_eq!(old.layout.field_parent, 0x20);
        assert_eq!(old.layout.method_name, 0x18);
        assert_eq!(old.layout.method_return_type, 0x20);
        assert_eq!(old.layout.image_stride, 0x48);
        assert_eq!(old.layout.image_name, 0x08);
        assert_eq!(old.layout.image_filename, 0x40);

        let new = select("245768f252146ef463b539e6c613b4955b7e829a4afd4dc0a2a2939aef3baadb")
            .expect("current build");
        assert_eq!(new.gc.remove_thread, 0xc4_3520);
        assert_eq!(new.gc.skip_close, 0xc4_3560);
        assert_eq!(new.gc.unlinked_next, 0xc4_3566);
        assert_eq!(new.gc.after_unlink, 0xc4_3588);
        assert_eq!(new.gc.suspend_entry, 0xc4_3790);
        assert_eq!(new.gc.context_call, 0xc4_3842);
        assert_eq!(new.gc.process_record, 0xc4_3800);
        assert_eq!(new.gc.resume_abort, 0xc4_386b);
        assert_eq!(new.gc.suspend_abort, 0xc4_38d5);
        assert_eq!(new.gc.advance_bucket, 0xc4_39da);
        assert_eq!(new.gc.threads, 0xe6b_d8a0);
        assert_eq!(new.gc.thread_list_cs, 0xe6b_d800);
        assert_eq!(new.il2cpp.get_corlib, 0xc2_9c70);
        assert_eq!(new.il2cpp.image_get_class, 0xc2_c0a0);
        assert_eq!(new.il2cpp.class_from_name, 0xc2_9dc0);
        assert_eq!(new.il2cpp.class_table, 0xe66_f0d0);
        assert_eq!(new.layout.class_parent, 0xa0);
        assert_eq!(new.layout.class_methods, 0x58);
        assert_eq!(new.layout.class_fields, 0x88);
        assert_eq!(new.layout.field_stride, 0x20);
        assert_eq!(new.layout.field_name, 0x00);
        assert_eq!(new.layout.field_type, 0x10);
        assert_eq!(new.layout.field_offset, 0x08);
        assert_eq!(new.layout.field_parent, 0x00);
        assert_eq!(new.layout.method_name, 0x20);
        assert_eq!(new.layout.method_return_type, 0x28);
        assert_eq!(new.layout.image_stride, 0x50);
        assert_eq!(new.layout.image_name, 0x08);
        assert_eq!(new.layout.image_filename, 0x18);
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
