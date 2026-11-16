//! End-to-end test of the inline-hook engine on real machine code.
//!
//! Built and run on the Windows target (under Proton's wine when the host is
//! Linux). It hooks a real exported Rust function, calls it through the patch,
//! and proves three things at once:
//!
//! * the 12-byte `mov rax,imm64 ; jmp rax` patch redirects the entry;
//! * the trampoline reproduces the original function (the value is unchanged);
//! * a second install reuses the shared table instead of re-patching.
//!
//! `hooked_add` is `#[inline(never)]`, so its body really exists in `.text`;
//! its prologue is whatever rustc emitted (`lea`/`add`/`ret` plus padding), which
//! exercises [`hromatite_eac::hook::prologue_len`] on live bytes.

#![cfg(windows)]
#![allow(unsafe_code)]

use std::sync::atomic::{AtomicUsize, Ordering};

use hromatite_eac::hook;

/// Trampoline published by the detour before any call can enter it.
static TRAMPOLINE: AtomicUsize = AtomicUsize::new(0);
static DETOUR_CALLS: AtomicUsize = AtomicUsize::new(0);

/// Original function under test: `a * 3 + b`.
#[inline(never)]
#[unsafe(no_mangle)]
pub extern "system" fn hooked_add(a: u64, b: u64) -> u64 {
    a.wrapping_mul(3).wrapping_add(b)
}

/// Detour with the win64 integer shape; forwards everything to the trampoline.
unsafe extern "system" fn detour(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64) -> u64 {
    DETOUR_CALLS.fetch_add(1, Ordering::SeqCst);
    let trampoline = TRAMPOLINE.load(Ordering::Acquire);
    assert_ne!(
        trampoline, 0,
        "detour ran before the trampoline was published"
    );
    // SAFETY: the trampoline is the relocated prologue of `hooked_add` and ends
    // in an absolute jump back into its body, so it has the same ABI.
    let original: unsafe extern "system" fn(u64, u64, u64, u64, u64, u64) -> u64 =
        unsafe { std::mem::transmute(trampoline) };
    // SAFETY: same ABI, same argument registers as the interrupted call.
    unsafe { original(a, b, c, d, e, f) }
}

#[test]
fn inline_hook_redirects_calls_and_the_trampoline_preserves_the_result() {
    assert_eq!(hooked_add(2, 5), 11, "sanity before hooking");

    // SAFETY: `hooked_add` is a real function entry in this image and `detour`
    // is an `extern "system"` function with the same return type; both live for
    // the whole test process.
    let installed = unsafe {
        hook::install(
            hooked_add as *const () as usize,
            detour as *const () as usize,
            "hooked_add",
        )
    }
    .expect("hook installs");
    assert_eq!(installed.mode, 1, "fresh patch");
    assert!(installed.copied >= hook::PATCH_LEN);
    assert_ne!(installed.tramp, 0);

    // Publish before calling: the entry is already redirected at this point.
    TRAMPOLINE.store(installed.tramp, Ordering::Release);
    // SAFETY: `hooked_add`'s entry is a readable 12-byte prologue in .text.
    let entry = unsafe { std::slice::from_raw_parts(hooked_add as *const u8, 12) };
    assert_eq!(
        hook::looks_patched(entry),
        Some(detour as *const () as usize),
        "the entry must hold our 12-byte patch"
    );

    let before = DETOUR_CALLS.load(Ordering::SeqCst);
    assert_eq!(hooked_add(2, 5), 11, "trampoline must preserve the result");
    assert_eq!(hooked_add(10, 1), 31);
    assert_eq!(
        DETOUR_CALLS.load(Ordering::SeqCst),
        before + 2,
        "both calls went through the detour"
    );

    // A second install into the same process must reuse the trampoline from the
    // named shared table (mode 2 == same handler) instead of patching twice.
    // SAFETY: same entry, same handler, still alive.
    let again = unsafe {
        hook::install(
            hooked_add as *const () as usize,
            detour as *const () as usize,
            "hooked_add",
        )
    }
    .expect("second install");
    assert_eq!(again.mode, 2, "reused through the shared table");
    assert_eq!(again.tramp, installed.tramp);
}
