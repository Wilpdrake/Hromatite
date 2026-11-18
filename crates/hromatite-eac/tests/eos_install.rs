//! Install the eight hooks against a mingw-built EOSSDK stub and drive them.
//!
//! The stub (`EAC-RE/shim/eossdkstub.c`, built into
//! `target/<triple>/debug/stub/EOSSDK-Win64-Shipping.dll`) exports every
//! `EOS_AntiCheatClient_*` entry the real SDK does, each returning a distinct
//! magic value. Loading it under its real base name is exactly what the payload
//! sees inside VRChat, so this test proves:
//!
//! * export-directory resolution and eight fresh patches (`mode == 1`);
//! * `BeginSession` / `EndSession` reach the original code through the
//!   trampoline and return the stub's magic (epoch opening is observed);
//! * `AddNotifyMessageToServer` substitutes our wrapper: the stub's own thread
//!   then pulses a frame through it, our capture file grows, and the game
//!   callback still receives every frame;
//! * `ProtectMessage` / `GetProtectMessageOutputLength` sizes are counted;
//! * `PollStatus` uses the stub's deliberately rel8-`je` prologue, so the
//!   inverse-`jcc`+absolute-jump relocation is exercised end to end.

#![cfg(windows)]
#![allow(unsafe_code)]

use std::sync::atomic::{AtomicUsize, Ordering};

use hromatite_eac::{HookEvent, Hooks};

unsafe extern "system" {
    fn LoadLibraryA(name: *const u8) -> isize;
    fn GetProcAddress(module: isize, name: *const u8) -> usize;
    fn FreeLibrary(module: isize) -> i32;
    fn Sleep(ms: u32);
}

/// The SDK's own `{ClientData, MessageData, SizeBytes}` callback struct.
#[repr(C)]
struct MessageInfo {
    client_data: *mut std::ffi::c_void,
    data: *const u8,
    size: u32,
}

static GAME_CALLBACKS: AtomicUsize = AtomicUsize::new(0);

/// What the game would register: receives every frame the SDK produces.
unsafe extern "system" fn game_callback(info: *const MessageInfo) {
    GAME_CALLBACKS.fetch_add(1, Ordering::SeqCst);
    if !info.is_null() {
        // SAFETY: the SDK builds this struct for the duration of the call.
        let info = unsafe { &*info };
        assert_eq!(info.size, 24, "stub frames are 24 bytes");
        // SAFETY: the stub's message buffer is 24 readable bytes.
        let data = unsafe { std::slice::from_raw_parts(info.data, info.size as usize) };
        assert_eq!(data[0], 0xB0);
    }
}

fn export(module: isize, name: &[u8]) -> usize {
    assert_eq!(name.last(), Some(&0), "name must be NUL-terminated");
    // SAFETY: the module handle came from LoadLibraryA and `name` is a literal.
    unsafe { GetProcAddress(module, name.as_ptr()) }
}

#[test]
fn hooks_resolve_patch_and_observe_the_stub_traffic() {
    let exe = std::env::current_exe().expect("current exe");
    let stub = exe
        .parent()
        .and_then(|deps| deps.parent())
        .expect("target dir")
        .join("stub")
        .join("EOSSDK-Win64-Shipping.dll");
    assert!(stub.is_file(), "stub missing at {}", stub.display());

    let mut name = stub.to_string_lossy().replace('/', "\\").into_bytes();
    name.push(0);
    // SAFETY: `name` is a NUL-terminated ASCII path that lives for the call.
    let module = unsafe { LoadLibraryA(name.as_ptr()) };
    assert_ne!(module, 0, "LoadLibraryA failed for {}", stub.display());

    let mut hooks = Hooks::new();
    let line = hooks.install_now();
    println!("{line}");
    assert!(hooks.installed(), "no hook installed: {line}");
    let ok = hooks.reports().iter().filter(|report| report.ok).count();
    // The stub is an artifact of the reverse-engineering tree, so it may predate
    // an export: count the ones it really provides instead of hard-coding 8.
    let mut expected = 0usize;
    for name in hromatite_eac::eos::export_names() {
        let mut terminated = name.as_bytes().to_vec();
        terminated.push(0);
        if export(module, &terminated) != 0 {
            expected += 1;
        } else {
            println!("stub does not export {name}");
        }
    }
    assert_eq!(
        ok,
        expected,
        "every export the stub provides must be hooked: {:?}",
        hooks.reports()
    );
    assert!(
        hooks
            .reports()
            .iter()
            .filter(|report| report.ok)
            .all(|report| report.mode == 1 && report.tramp != 0),
        "fresh patches with trampolines: {:?}",
        hooks.reports()
    );

    type Fn4 = unsafe extern "system" fn(usize, usize, usize, usize) -> i32;
    let begin = export(module, b"EOS_AntiCheatClient_BeginSession\0");
    let end = export(module, b"EOS_AntiCheatClient_EndSession\0");
    let add_notify = export(module, b"EOS_AntiCheatClient_AddNotifyMessageToServer\0");
    let protect = export(module, b"EOS_AntiCheatClient_ProtectMessage\0");
    let out_len = export(
        module,
        b"EOS_AntiCheatClient_GetProtectMessageOutputLength\0",
    );
    let poll = export(module, b"EOS_AntiCheatClient_PollStatus\0");
    assert!(begin != 0 && end != 0 && add_notify != 0 && protect != 0 && out_len != 0 && poll != 0);

    let mut begin_options = [1i32, 1i32]; // ApiVersion 1, Mode 1 (VRChat)
    // SAFETY: every call below goes through a hook into the stub's `__stdcall`
    // entry, whose only contract is the four integer slots.
    unsafe {
        let begin: Fn4 = std::mem::transmute(begin);
        assert_eq!(
            begin(0x1234, begin_options.as_mut_ptr() as usize, 0, 0),
            0x5E55_0001,
            "BeginSession must reach the stub through the trampoline"
        );
        let end: Fn4 = std::mem::transmute(end);
        assert_eq!(end(0x1234, 0, 0, 0), 0x5E55_0002);
        let add_notify: Fn4 = std::mem::transmute(add_notify);
        assert_eq!(
            add_notify(
                0x1234,
                begin_options.as_mut_ptr() as usize,
                0xCAFE,
                game_callback as *const () as usize
            ),
            0x5E55_0009,
            "the game callback must have been replaced by our wrapper"
        );
        let poll: Fn4 = std::mem::transmute(poll);
        assert_eq!(
            poll(0x1234, 1, 1, 1),
            0x5E55_000B,
            "rel8-je prologue survives the trampoline (both operands non-null)"
        );
        assert_eq!(
            poll(0, 1, 1, 1),
            0xDEAD_0001u32 as i32,
            "relocated early exit 1"
        );
        assert_eq!(
            poll(1, 0, 1, 1),
            0xDEAD_0002u32 as i32,
            "relocated early exit 2"
        );
    }

    // The stub's thread pulses a 24-byte frame every 200 ms through the
    // registered callback — i.e. through our wrapper.
    // SAFETY: plain Win32 sleep.
    unsafe { Sleep(550) };
    let counters = hooks.counters();
    assert!(
        counters.out_messages >= 2,
        "wrapper saw no frames: {counters:?}"
    );
    assert!(
        counters.forwarded >= 2,
        "frames were not forwarded: {counters:?}"
    );
    assert!(
        GAME_CALLBACKS.load(Ordering::SeqCst) >= 2,
        "the game callback did not receive the frames"
    );
    assert_eq!(counters.calls[0], 1, "BeginSession hook counted");
    assert_eq!(counters.calls[1], 1, "EndSession hook counted");
    assert_eq!(counters.calls[6], 1, "AddNotify hook counted");
    assert!(counters.calls[7] >= 3, "PollStatus hooks counted");

    // ProtectMessage: 8 in, stub writes 8 out.
    let mut input = [0xAAu8; 8];
    let mut output = [0u8; 64];
    let mut written = 0u32;
    // Exact ABI: {i32 ApiVersion, u32 DataLengthBytes, const void* Data, u32 OutCap}.
    let mut protect_options = [0u8; 24];
    // SAFETY: the offsets match the documented options struct.
    unsafe {
        std::ptr::write_unaligned(protect_options.as_mut_ptr() as *mut i32, 1);
        std::ptr::write_unaligned(
            protect_options.as_mut_ptr().add(4) as *mut u32,
            input.len() as u32,
        );
        std::ptr::write_unaligned(
            protect_options.as_mut_ptr().add(8) as *mut usize,
            input.as_mut_ptr() as usize,
        );
        std::ptr::write_unaligned(
            protect_options.as_mut_ptr().add(16) as *mut u32,
            output.len() as u32,
        );
    }
    // SAFETY: the stub only touches the buffers described by the options.
    unsafe {
        let protect: Fn4 = std::mem::transmute(protect);
        assert_eq!(
            protect(
                0x1234,
                protect_options.as_mut_ptr() as usize,
                output.as_mut_ptr() as usize,
                &mut written as *mut u32 as usize
            ),
            0x5E55_0006
        );
    }
    assert_eq!(written, 8);

    // GetProtectMessageOutputLength: the stub reports `len + 16`; the hook must
    // record that delta, which is how the live 22-byte proof is counted.
    let mut needed = 0u32;
    // {i32 ApiVersion, u32 DataLengthBytes, u32* Out}.
    let mut outlen_options = [0u8; 16];
    // SAFETY: the offsets match the documented options struct.
    unsafe {
        std::ptr::write_unaligned(outlen_options.as_mut_ptr() as *mut i32, 1);
        std::ptr::write_unaligned(outlen_options.as_mut_ptr().add(4) as *mut u32, 8);
        std::ptr::write_unaligned(
            outlen_options.as_mut_ptr().add(8) as *mut usize,
            &mut needed as *mut u32 as usize,
        );
    }
    // SAFETY: the stub writes one u32 through `Out`.
    unsafe {
        let out_len: Fn4 = std::mem::transmute(out_len);
        assert_eq!(
            out_len(0x1234, outlen_options.as_mut_ptr() as usize, 0, 0),
            0x5E55_0008
        );
    }
    assert_eq!(needed, 24);
    let counters = hooks.counters();
    // Two samples: ProtectMessage (8 -> 8, delta 0) and OutLength (8 -> 24).
    assert_eq!(counters.samples, 2, "both length calls counted");
    assert_eq!(counters.overhead_min, 0);
    assert_eq!(counters.overhead_max, 16);

    // The capture file must contain at least two complete records.
    let capture = hooks.capture_path().expect("capture sink");
    let bytes = std::fs::metadata(&capture)
        .map(|meta| meta.len())
        .unwrap_or(0);
    assert!(
        bytes >= 2 * (8 + 24),
        "capture {} has {bytes} bytes",
        capture.display()
    );

    let events = hooks.drain_events();
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HookEvent::SessionBegin { mode: 1, .. })),
        "epoch event missing: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, HookEvent::OutMessage { len: 24, .. })),
        "outgoing frame event missing: {events:?}"
    );

    // SAFETY: the module handle came from LoadLibraryA above.
    unsafe { FreeLibrary(module) };
}
