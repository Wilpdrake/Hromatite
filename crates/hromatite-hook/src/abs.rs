//! 12-byte position-independent inline-hook engine (`mov rax, imm64 ; jmp rax`)
//! with process-wide shared hook-table coordination (`Local\hromatite_rust_hooktable_v1`).
//!
//! Ported from `EAC-RE/shim/shim.c` and unified over [`crate::decode`],
//! [`crate::tramp`], [`crate::mem`], and [`crate::table`].

#![cfg_attr(not(windows), allow(dead_code))]

use hromatite_win::{ffi, log, memory as safe};

use crate::decode::prologue_span_bounded;
use crate::mem::{NEAR_SPAN, PAGE_SIZE, alloc_near_within, cas16, flush};
pub use crate::tramp::{ABS_CALL, ABS_JUMP, Built, Reloc, build_trampoline};

/// Bytes overwritten at the target entry (`48 B8 <imm64> FF E0`).
pub const PATCH_LEN: usize = 12;

/// Length of a prologue covering at least [`PATCH_LEN`] whole instructions, and
/// how many they are (`<= 8` instructions, `<= 24` bytes).
pub fn prologue_span(code: &[u8]) -> Result<(usize, usize), String> {
    prologue_span_bounded(code, PATCH_LEN, 8, 24)
}

/// Length of a prologue covering at least [`PATCH_LEN`] bytes. `None` when an
/// unknown opcode is hit or the span exceeds 24 bytes.
pub fn prologue_len(code: &[u8]) -> Option<usize> {
    prologue_span(code).ok().map(|(len, _)| len)
}

/// Encode `mov rax, imm64 ; jmp rax`.
pub fn abs_jmp(addr: usize) -> [u8; PATCH_LEN] {
    let mut out = [0u8; PATCH_LEN];
    out[0] = 0x48;
    out[1] = 0xB8;
    out[2..10].copy_from_slice(&(addr as u64).to_le_bytes());
    out[10] = 0xFF;
    out[11] = 0xE0;
    out
}

/// If `code` already starts with the 12-byte patch, return its handler address.
pub fn looks_patched(code: &[u8]) -> Option<usize> {
    if code.len() >= PATCH_LEN
        && code[0] == 0x48
        && code[1] == 0xB8
        && code[10] == 0xFF
        && code[11] == 0xE0
    {
        let mut imm = [0u8; 8];
        imm.copy_from_slice(&code[2..10]);
        Some(u64::from_le_bytes(imm) as usize)
    } else {
        None
    }
}

/// A freshly installed 12-byte hook. The trampoline page is kept alive for the
/// process lifetime and is what the detour must tail-call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hook {
    /// Original entry (now holding the 12-byte patch).
    pub target: usize,
    /// Relocated prologue; call this, never `target`.
    pub tramp: usize,
    /// Bytes of prologue relocated.
    pub copied: usize,
    /// Every relocation applied to the copy, as `kind+offset->addr` entries.
    /// Empty when the entry was reused (`copied == 0`).
    pub note: String,
    /// How many instructions the copied prologue holds (0 when reused).
    pub insns: usize,
    /// How many of those were re-encoded (0 when reused).
    pub relocs: usize,
    /// 1 = fresh patch, 2 = reused a trampoline from the shared table,
    /// 3 = the entry was patched by a foreign image (forwarded).
    pub mode: u8,
}

impl Hook {
    /// One log line naming what the trampoline contains: prologue length and
    /// instruction count, how many fields moved and in which form, the
    /// trampoline address and its distance from the target.
    pub fn describe(&self) -> String {
        let delta = self.tramp as i64 - self.target as i64;
        if self.copied == 0 {
            return format!(
                "mode={} target={:#x} tramp={:#x} ({delta:+#x}, reused) {}",
                self.mode, self.target, self.tramp, self.note
            );
        }
        format!(
            "prologue={}B/{} insn, reloc={}/{} [{}], target={:#x} -> tramp={:#x} ({delta:+#x}, {:.1} MB), mode={}",
            self.copied,
            self.insns,
            self.relocs,
            self.insns,
            if self.note.is_empty() {
                "verbatim"
            } else {
                &self.note
            },
            self.target,
            self.tramp,
            delta as f64 / (1024.0 * 1024.0),
            self.mode
        )
    }
}

/// Write `patch` at `target` (12 bytes) and flush the instruction cache.
///
/// # Safety
/// `target` must point at an executable, readable function entry and `patch`
/// must be exactly [`PATCH_LEN`] bytes.
pub unsafe fn patch_entry(target: usize, patch: &[u8; PATCH_LEN]) -> Result<(), String> {
    let mut old_protect = 0u32;
    // SAFETY: target is a function entry; the page is made writable for the
    // duration of the patch and restored afterwards.
    let ok = unsafe {
        ffi::VirtualProtect(
            target as *mut u8,
            PATCH_LEN + 4,
            ffi::PAGE_EXECUTE_READWRITE,
            &mut old_protect,
        )
    };
    if ok == 0 {
        return Err(format!("VirtualProtect failed at {target:#x}"));
    }

    let mut done = false;
    if target % 16 <= 4 {
        let base16 = target & !0xF;
        let mut window = [0u8; 16];
        // SAFETY: the block around the entry is inside the same page that was
        // just made writable (entry alignment is checked above).
        unsafe {
            core::ptr::copy_nonoverlapping(base16 as *const u8, window.as_mut_ptr(), 16);
        }
        let offset = target - base16;
        window[offset..offset + PATCH_LEN].copy_from_slice(patch);
        // SAFETY: `base16` is 16-byte aligned and writable.
        done = unsafe { cas16(base16, &window) };
    }
    if !done {
        // SAFETY: destination is patched after VirtualProtect; source is a
        // local 12-byte array.
        unsafe {
            core::ptr::copy_nonoverlapping(patch.as_ptr(), target as *mut u8, PATCH_LEN);
        }
    }

    // SAFETY: restore the original protection; the value came from
    // VirtualProtect above.
    unsafe {
        ffi::VirtualProtect(
            target as *mut u8,
            PATCH_LEN + 4,
            old_protect,
            &mut old_protect,
        );
    }
    flush(target, PATCH_LEN);
    Ok(())
}

/// Install one 12-byte hook. Returns the trampoline the detour must tail-call.
///
/// # Safety
/// `target` must be the entry of an executable function whose first
/// [`PATCH_LEN`]-covering prologue is decodable, and `handler` must be a
/// `extern "system"` function taking the same arguments as `target` and
/// returning the same type. Both addresses must stay valid for the process
/// lifetime.
pub unsafe fn install(target: usize, handler: usize, name: &str) -> Result<Hook, String> {
    if target == 0 {
        return Err("no export".to_string());
    }
    if !safe::readable_range(target, 32) {
        return Err("target unreadable".to_string());
    }
    let mut original = [0u8; 32];
    // SAFETY: readable_range() above proved 32 readable bytes at the entry.
    unsafe {
        core::ptr::copy_nonoverlapping(target as *const u8, original.as_mut_ptr(), 32);
    }

    if let Some(imm) = looks_patched(&original) {
        if let Some(entry) = crate::table::lookup(target) {
            let tramp = entry.tramp as usize;
            let mode = if entry.handler == handler as u64 {
                2
            } else {
                3
            };
            if mode == 3 {
                // Take ownership of the existing trampoline: patch the jump to
                // our handler so this (live) image keeps counting the calls.
                // SAFETY: the entry is patched already, so a 12-byte atomic
                // rewrite of the same window is safe.
                unsafe { patch_entry(target, &abs_jmp(handler))? };
                crate::table::store(target, tramp, handler, name);
            }
            return Ok(Hook {
                target,
                tramp,
                copied: 0,
                note: String::new(),
                insns: 0,
                relocs: 0,
                mode,
            });
        }
        // Someone else patched it and we know nothing about the trampoline:
        // forward to the handler it points at.
        return Ok(Hook {
            target,
            tramp: imm,
            copied: 0,
            note: String::new(),
            insns: 0,
            relocs: 0,
            mode: 3,
        });
    }

    let (total, insns) =
        prologue_span(&original).map_err(|reason| format!("prologue at {target:#x}: {reason}"))?;
    if total < PATCH_LEN {
        return Err(format!("prologue {total} < {PATCH_LEN}"));
    }

    let page = alloc_near_within(target, PAGE_SIZE, NEAR_SPAN, true, ffi::PAGE_EXECUTE_READWRITE)
        .ok_or_else(|| "VirtualAlloc failed".to_string())?;
    // SAFETY: the page is a fresh, writable PAGE_SIZE-byte allocation.
    let tramp = unsafe { core::slice::from_raw_parts_mut(page as *mut u8, PAGE_SIZE) };
    let built = build_trampoline(tramp, target, &original, total)
        .map_err(|reason| format!("trampoline relocation at {target:#x}: {reason}"))?;

    // Publish the trampoline before patching the target, so a thread that
    // enters the hook immediately always finds a complete trampoline.
    flush(page, built.len);

    // SAFETY: patch the validated entry with an absolute jump to the handler.
    unsafe { patch_entry(target, &abs_jmp(handler))? };

    let mut check = [0u8; 2];
    // SAFETY: read back the two opcode bytes just written.
    unsafe {
        core::ptr::copy_nonoverlapping(target as *const u8, check.as_mut_ptr(), 2);
    }
    if check != [0x48, 0xB8] {
        return Err("readback mismatch after patch".to_string());
    }

    crate::table::store(target, page, handler, name);
    let hook = Hook {
        target,
        tramp: page,
        copied: total,
        note: built.note,
        insns,
        relocs: built.relocs.len(),
        mode: 1,
    };
    log::log_tag("hook", &format!("{name}: {}", hook.describe()));
    Ok(hook)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decode;

    /// The exact prologue observed live in `eacshim.log`:
    /// `48 83 ec 28 | 48 85 c9 | 74 13 | 48 85 d2` (4+3+2+3 = 12 bytes).
    const LIVE_PROLOGUE: [u8; 16] = [
        0x48, 0x83, 0xEC, 0x28, 0x48, 0x85, 0xC9, 0x74, 0x13, 0x48, 0x85, 0xD2, 0x48, 0x8B, 0x01,
        0xC3,
    ];

    #[test]
    fn decode_matches_the_measured_prologue() {
        let first = decode(&LIVE_PROLOGUE).expect("sub rsp");
        assert_eq!(first.len, 4);
        let second = decode(&LIVE_PROLOGUE[4..]).expect("test rcx");
        assert_eq!(second.len, 3);
        let third = decode(&LIVE_PROLOGUE[7..]).expect("jz rel8");
        assert_eq!(third.len, 2);
        assert!(third.rel);
        assert_eq!(third.rel_size, 1);
        let fourth = decode(&LIVE_PROLOGUE[9..]).expect("test rdx");
        assert_eq!(fourth.len, 3);
    }

    #[test]
    fn prologue_len_is_twelve_for_the_live_export() {
        assert_eq!(prologue_len(&LIVE_PROLOGUE), Some(12));
        let padding = [
            0x0F, 0x1F, 0x00, 0x0F, 0x1F, 0x00, 0x0F, 0x1F, 0x00, 0x0F, 0x1F, 0x00,
        ];
        assert_eq!(prologue_len(&padding), Some(12));
        assert_eq!(prologue_len(&[0x06]), None, "unknown opcode");
    }

    fn abs_jump_target(page: &[u8], at: usize) -> usize {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&page[at + 6..at + 14]);
        u64::from_le_bytes(raw) as usize
    }

    #[test]
    fn rel8_jcc_becomes_a_cond_short_over_an_absolute_jump() {
        let mut page = vec![0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x800;
        let built = build_trampoline(&mut page, base, &LIVE_PROLOGUE, 12).expect("relocate");
        assert!(built.note.starts_with("jcc-abs+7->"), "{}", built.note);
        assert_eq!(built.len, 40);
        assert_eq!(built.insns, 4);
        assert_eq!(built.relocs.len(), 1);

        assert_eq!(&page[0..4], &[0x48, 0x83, 0xEC, 0x28]);
        assert_eq!(&page[4..7], &[0x48, 0x85, 0xC9]);
        assert_eq!(
            &page[7..9],
            &[0x75, ABS_JUMP as u8],
            "jnz over the abs jump"
        );
        assert_eq!(&page[9..11], &[0xFF, 0x25]);
        assert_eq!(abs_jump_target(&page, 9), base + 0x1C);
        assert_eq!(&page[23..26], &[0x48, 0x85, 0xD2]);
        assert_eq!(&page[26..28], &[0xFF, 0x25]);
        assert_eq!(abs_jump_target(&page, 26), base + 12);
    }

    #[test]
    fn jrcxz_and_loop_rel8_branches_are_relocated_too() {
        let code = [
            0x48, 0x83, 0xEC, 0x28, // sub rsp,0x28
            0xE3, 0x06, // jrcxz +6 -> base + 12
            0x48, 0x85, 0xC9, // test rcx,rcx
            0x90, 0x90, 0x90, // padding to 12 bytes
        ];
        assert_eq!(prologue_len(&code), Some(12));
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x800;
        let built = build_trampoline(&mut page, base, &code, 12).expect("relocate");
        assert_eq!(built.relocs[0].kind, "jcc-abs");
        assert_eq!(built.relocs[0].at, 4);
        assert_eq!(&page[4..8], &[0xE3, 0x02, 0xEB, ABS_JUMP as u8]);
        assert_eq!(abs_jump_target(&page, 8), base + 12);
        assert_eq!(&page[22..25], &[0x48, 0x85, 0xC9]);
    }

    #[test]
    fn rip_relative_operand_is_retargeted_to_the_copy() {
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x800;
        let code = [
            0x48, 0x8B, 0x05, 0x00, 0x01, 0x00, 0x00, 0xC3, 0xCC, 0xCC, 0xCC, 0xCC,
        ];
        let built = build_trampoline(&mut page, base, &code, 12).expect("relocate");
        assert!(built.note.starts_with("rip-rel+0"), "{}", built.note);
        let disp = i32::from_le_bytes(page[3..7].try_into().unwrap());
        let resolved = (page_addr + 7) as i64 + i64::from(disp);
        assert_eq!(
            resolved,
            (base + 0x107) as i64,
            "disp must still resolve the original target"
        );
    }

    #[test]
    fn call_rel32_in_prologue_targets_the_original_callee() {
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x800;
        let callee = base + 5 + 5 + 0x40;

        let mut code = [0x90u8; 14];
        code[0..5].copy_from_slice(&[0xB8, 0x28, 0x00, 0x00, 0x00]);
        code[5] = 0xE8;
        code[6..10].copy_from_slice(&(callee as i64 - (base + 10) as i64).to_le_bytes()[..4]);
        code[10..14].copy_from_slice(&[0x48, 0x83, 0xEC, 0x28]);

        let built = build_trampoline(&mut page, base, &code, 14).expect("relocate");
        assert!(built.note.starts_with("call-rel32+5->"), "{}", built.note);
        assert_eq!(&page[5], &0xE8);
        let disp = i32::from_le_bytes(page[6..10].try_into().unwrap());
        assert_eq!(
            (page_addr + 5 + 5) as i64 + i64::from(disp),
            callee as i64,
            "the relocated call must still reach the original callee"
        );
    }

    #[test]
    fn call_rel32_beyond_reach_goes_through_an_absolute_slot() {
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x2_0000_0000;
        let callee = base + 0x100;
        let mut code = [0x90u8; 14];
        code[0..5].copy_from_slice(&[0xB8, 0x28, 0, 0, 0]);
        code[5] = 0xE8;
        code[6..10].copy_from_slice(&((callee as i64 - (base + 10) as i64) as i32).to_le_bytes());
        code[10..14].copy_from_slice(&[0x48, 0x83, 0xEC, 0x28]);

        let built = build_trampoline(&mut page, base, &code, 14).expect("relocate");
        assert_eq!(built.relocs[0].kind, "call-abs");
        assert_eq!(&page[5..11], &[0xFF, 0x15, 0x02, 0, 0, 0]);
        assert_eq!(&page[11..13], &[0xEB, (ABS_CALL - 8) as u8]);
        let mut slot = [0u8; 8];
        slot.copy_from_slice(&page[13..21]);
        assert_eq!(u64::from_le_bytes(slot) as usize, callee);
        assert_eq!(&page[21..25], &[0x48, 0x83, 0xEC, 0x28]);
    }

    #[test]
    fn jcc_rel32_out_of_reach_becomes_cond_short_plus_absolute() {
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x2_0000_0000;
        let mut code = [0x90u8; 12];
        code[0..4].copy_from_slice(&[0x48, 0x83, 0xEC, 0x28]);
        code[4..6].copy_from_slice(&[0x0F, 0x85]);
        code[6..10]
            .copy_from_slice(&(((base + 12) as i64 - (base + 10) as i64) as i32).to_le_bytes());

        let built = build_trampoline(&mut page, base, &code, 12).expect("relocate");
        assert_eq!(built.relocs[0].kind, "jcc-abs");
        assert_eq!(&page[4..6], &[0x0F, 0x84]);
        assert_eq!(
            i32::from_le_bytes(page[6..10].try_into().unwrap()),
            ABS_JUMP as i32
        );
        assert_eq!(abs_jump_target(&page, 10), base + 12);
    }

    #[test]
    fn a_branch_into_the_prologue_follows_the_copy() {
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x800;
        let mut code = [0x90u8; 12];
        code[0..3].copy_from_slice(&[0x48, 0x85, 0xC9]);
        code[3..8].copy_from_slice(&[0xE8, 0xFB, 0xFF, 0xFF, 0xFF]); // call +3

        let built = build_trampoline(&mut page, base, &code, 12).expect("relocate");
        assert_eq!(built.relocs[0].kind, "call-rel32");
        assert_eq!(
            built.relocs[0].target as usize,
            page_addr + 3,
            "the self-call must land in the copy, not on the patched original"
        );
        assert_eq!(i32::from_le_bytes(page[4..8].try_into().unwrap()), -5);
    }

    #[test]
    fn a_branch_into_the_middle_of_an_instruction_is_refused() {
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x800;
        let mut code = [0x90u8; 12];
        code[0..3].copy_from_slice(&[0x48, 0x85, 0xC9]);
        code[3..8].copy_from_slice(&[0xE9, 0xF9, 0xFF, 0xFF, 0xFF]); // jmp +1
        let err = build_trampoline(&mut page, base, &code, 12).expect_err("mid-instruction target");
        assert!(err.contains("middle of an instruction"), "{err}");
    }

    #[test]
    fn prologue_span_names_the_offending_byte() {
        assert_eq!(prologue_span(&LIVE_PROLOGUE), Ok((12, 4)));
        let err = prologue_span(&[0x48, 0x83, 0xEC, 0x28, 0x06]).expect_err("0x06 is not decoded");
        assert!(err.contains("undecodable opcode 0x06 at +4"), "{err}");
        assert!(prologue_span(&[0x48, 0x81, 0xEC, 0x28]).is_err());
    }

    #[test]
    fn the_real_boring_ssl_evp_prologues_relocate_their_chkstk_call() {
        const RVAS: [(usize, [u8; 16]); 5] = [
            (
                0xcd_b040,
                [
                    0xB8, 0x28, 0x00, 0x00, 0x00, 0xE8, 0xE6, 0xFF, 0x18, 0x00, 0x48, 0x2B, 0xE0,
                    0x44, 0x8D, 0x40,
                ],
            ),
            (
                0xcd_bcb0,
                [
                    0xB8, 0x38, 0x00, 0x00, 0x00, 0xE8, 0x76, 0xF3, 0x18, 0x00, 0x48, 0x2B, 0xE0,
                    0x48, 0x8B, 0x44,
                ],
            ),
            (
                0xcd_bce0,
                [
                    0xB8, 0x38, 0x00, 0x00, 0x00, 0xE8, 0x46, 0xF3, 0x18, 0x00, 0x48, 0x2B, 0xE0,
                    0x83, 0x79, 0x10,
                ],
            ),
            (
                0xcd_af90,
                [
                    0xB8, 0x38, 0x00, 0x00, 0x00, 0xE8, 0x96, 0x00, 0x19, 0x00, 0x48, 0x2B, 0xE0,
                    0x48, 0x8B, 0x01,
                ],
            ),
            (
                0xcd_bb60,
                [
                    0x48, 0x89, 0x5C, 0x24, 0x18, 0x48, 0x89, 0x6C, 0x24, 0x20, 0x41, 0x56, 0xB8,
                    0x30, 0x00, 0x00,
                ],
            ),
        ];
        const CHKSTK_RVA: i64 = 0xE6_B030;
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        for (rva, bytes) in RVAS {
            let base = page_addr + 0x800;
            let added = base as i64 - rva as i64;
            assert_eq!(
                prologue_len(&bytes),
                Some(if rva == 0xcd_bb60 { 12 } else { 13 }),
                "{rva:#x}"
            );
            let built = build_trampoline(
                &mut page,
                base,
                &bytes,
                if rva == 0xcd_bb60 { 12 } else { 13 },
            )
            .expect("relocate");
            if rva == 0xcd_bb60 {
                assert!(built.relocs.is_empty(), "{rva:#x}: {}", built.note);
                assert_eq!(built.note, "verbatim");
            } else {
                assert_eq!(built.relocs.len(), 1, "{rva:#x}: {}", built.note);
                assert_eq!(built.relocs[0].kind, "call-rel32");
                assert_eq!(built.relocs[0].at, 5);
                assert_eq!(
                    built.relocs[0].target - added,
                    CHKSTK_RVA,
                    "{rva:#x} must call __chkstk"
                );
            }
            assert_eq!(
                &page[built.len - ABS_JUMP..built.len - ABS_JUMP + 2],
                &[0xFF, 0x25]
            );
        }
    }

    #[test]
    fn describe_reports_the_relocations_of_a_installed_hook() {
        let hook = Hook {
            target: 0x6ffe_0000,
            tramp: 0x6ff0_0000,
            copied: 13,
            note: "call-rel32+5->0x6ffe3b020(+0x3b020)".to_string(),
            insns: 3,
            relocs: 1,
            mode: 1,
        };
        let line = hook.describe();
        assert!(line.contains("prologue=13B/3 insn"), "{line}");
        assert!(line.contains("reloc=1/3"), "{line}");
        assert!(line.contains("call-rel32+5->"), "{line}");
        assert!(line.contains("tramp=0x6ff00000"), "{line}");

        let reused = Hook {
            copied: 0,
            insns: 0,
            relocs: 0,
            mode: 2,
            note: String::new(),
            target: 0x1000,
            tramp: 0x2000,
        };
        assert!(
            reused.describe().contains("reused"),
            "{}",
            reused.describe()
        );
    }

    #[test]
    fn lea_rip_relative_in_prologue_is_retargeted() {
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x800;
        let mut code = [0xCCu8; 17];
        code[0..3].copy_from_slice(&[0x48, 0x8D, 0x0D]);
        code[3..7].copy_from_slice(&0x400i32.to_le_bytes());
        code[7..9].copy_from_slice(&[0x8B, 0x05]);
        code[9..13].copy_from_slice(&0x100i32.to_le_bytes());
        code[13] = 0xC3;

        let built = build_trampoline(&mut page, base, &code, 14).expect("relocate");
        assert!(built.note.starts_with("rip-rel+0"), "{}", built.note);
        assert!(built.note.contains("rip-rel+7"), "{}", built.note);
        let lea = i32::from_le_bytes(page[3..7].try_into().unwrap());
        assert_eq!(
            (page_addr + 7) as i64 + i64::from(lea),
            (base + 7 + 0x400) as i64
        );
        let mov = i32::from_le_bytes(page[9..13].try_into().unwrap());
        assert_eq!(
            (page_addr + 13) as i64 + i64::from(mov),
            (base + 13 + 0x100) as i64
        );
    }

    #[test]
    fn every_relocation_is_reported_not_just_the_first() {
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x800;
        let callee = base + 0x100;
        let mut code = [0xCCu8; 19];
        code[0..5].copy_from_slice(&[0xB8, 0x28, 0x00, 0x00, 0x00]);
        code[5] = 0xE8;
        code[6..10].copy_from_slice(&((callee as i64 - (base + 10) as i64) as i32).to_le_bytes());
        code[10..13].copy_from_slice(&[0x48, 0x8D, 0x15]);
        code[13..17].copy_from_slice(&0x200i32.to_le_bytes());
        code[17] = 0xC3;

        let built = build_trampoline(&mut page, base, &code, 18).expect("relocate");
        assert!(built.note.contains("call-rel32+5"), "{}", built.note);
        assert!(built.note.contains("rip-rel+10"), "{}", built.note);
        assert_eq!(built.insns, 4);
        assert_eq!(built.relocs.len(), 2);
    }

    #[test]
    fn prologue_len_never_splits_an_instruction() {
        let chkstk = [
            0xB8, 0x28, 0x00, 0x00, 0x00, // mov eax,0x28
            0xE8, 0x10, 0x00, 0x00, 0x00, // call rel32
            0x48, 0x83, 0xEC, 0x28, // sub rsp,0x28
        ];
        assert_eq!(prologue_len(&chkstk), Some(14));
        assert_eq!(prologue_len(&chkstk[..12]), None);
    }

    #[test]
    fn abs_jmp_round_trips_through_looks_patched() {
        let bytes = abs_jmp(0x1234_5678_9ABC_DEF0);
        assert_eq!(looks_patched(&bytes), Some(0x1234_5678_9ABC_DEF0));
        assert_eq!(looks_patched(&[0x90; 12]), None);
    }
}
