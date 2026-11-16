//! Dynamically replaceable feature commands over the frozen Hromatite C ABI.
//!
//! Every game-memory access and native call is delegated to the resident host.
//! No core runtime, VEH handler, panic hook or leaked cache lives in this image.
#![allow(unsafe_code)]

mod feature;
mod host;
mod m3;
mod memory;
mod telemetry;

use feature::Feature;
use hromatite_abi::{
    ABI_VERSION, FeatureApiV1, HostApiV1, STATUS_BAD_ABI, STATUS_BAD_ARGUMENT, STATUS_ERROR,
    STATUS_NOT_READY, STATUS_OK, host_header_valid,
};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Mutex, MutexGuard};

static STATE: Mutex<Option<Feature>> = Mutex::new(None);

fn state() -> MutexGuard<'static, Option<Feature>> {
    STATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn init_impl(host: *const HostApiV1, generation: u64) -> i32 {
    if host.is_null() || !(host as usize).is_multiple_of(core::mem::align_of::<HostApiV1>()) {
        return STATUS_BAD_ARGUMENT;
    }
    // SAFETY: the fixed two-u32 header is the version/size discovery prefix;
    // the full table is copied only after that prefix authorises its layout.
    let (struct_size, abi_version) = unsafe {
        (
            core::ptr::addr_of!((*host).struct_size).read(),
            core::ptr::addr_of!((*host).abi_version).read(),
        )
    };
    if abi_version != ABI_VERSION {
        return STATUS_BAD_ABI;
    }
    if (struct_size as usize) < core::mem::size_of::<HostApiV1>() {
        return STATUS_BAD_ARGUMENT;
    }
    // SAFETY: validation above proves the resident supplied a complete v1 POD
    // table. Copying it avoids retaining a host pointer or Rust-owned object.
    let host = unsafe { *host };
    if !host_header_valid(Some(&host)) {
        return STATUS_BAD_ABI;
    }
    *state() = Some(Feature::new(host, generation));
    STATUS_OK
}

extern "system" fn init(host: *const HostApiV1, generation: u64) -> i32 {
    catch_unwind(AssertUnwindSafe(|| init_impl(host, generation))).unwrap_or(STATUS_ERROR)
}

fn dispatch_impl(command: *const i8, command_len: usize) -> i32 {
    let mut state = state();
    let Some(feature) = state.as_mut() else {
        return STATUS_NOT_READY;
    };
    if command_len > 64 * 1024 || (command.is_null() && command_len != 0) {
        return feature.respond(
            ["dispatch: invalid command buffer".to_string()],
            STATUS_BAD_ARGUMENT,
        );
    }
    // SAFETY: the ABI lends exactly `command_len` readable bytes for this call.
    // Empty commands may use a null pointer and are handled without a slice.
    let bytes = unsafe {
        if command_len == 0 {
            &[][..]
        } else {
            std::slice::from_raw_parts(command.cast::<u8>(), command_len)
        }
    };
    match std::str::from_utf8(bytes) {
        Ok(command) => feature.execute(command),
        Err(_) => feature.respond(
            ["dispatch: command is not valid UTF-8".to_string()],
            STATUS_BAD_ARGUMENT,
        ),
    }
}

extern "system" fn dispatch(command: *const i8, command_len: usize) -> i32 {
    match catch_unwind(AssertUnwindSafe(|| dispatch_impl(command, command_len))) {
        Ok(status) => status,
        Err(_) => {
            let state = state();
            if let Some(feature) = state.as_ref() {
                feature
                    .host
                    .log("dispatch: panic contained at feature ABI boundary");
            }
            STATUS_ERROR
        }
    }
}

fn configure_impl(key: *const i8, key_len: usize, value: *const i8, value_len: usize) -> i32 {
    let mut state = state();
    let Some(feature) = state.as_mut() else {
        return STATUS_NOT_READY;
    };
    let valid = key_len <= 256
        && value_len <= 4096
        && (!key.is_null() || key_len == 0)
        && (!value.is_null() || value_len == 0);
    if !valid {
        return feature.respond(
            ["configure: invalid key/value buffer".to_string()],
            STATUS_BAD_ARGUMENT,
        );
    }
    // SAFETY: both buffers are borrowed for this synchronous ABI invocation.
    // Null is accepted only for zero length and is never used to form a slice.
    let (key, value) = unsafe {
        (
            if key_len == 0 {
                &[][..]
            } else {
                std::slice::from_raw_parts(key.cast::<u8>(), key_len)
            },
            if value_len == 0 {
                &[][..]
            } else {
                std::slice::from_raw_parts(value.cast::<u8>(), value_len)
            },
        )
    };
    match (std::str::from_utf8(key), std::str::from_utf8(value)) {
        (Ok(key), Ok(value)) => feature.configure(key, value),
        _ => feature.respond(
            ["configure: key/value must be UTF-8".to_string()],
            STATUS_BAD_ARGUMENT,
        ),
    }
}

extern "system" fn configure(
    key: *const i8,
    key_len: usize,
    value: *const i8,
    value_len: usize,
) -> i32 {
    match catch_unwind(AssertUnwindSafe(|| {
        configure_impl(key, key_len, value, value_len)
    })) {
        Ok(status) => status,
        Err(_) => {
            let state = state();
            if let Some(feature) = state.as_ref() {
                feature
                    .host
                    .log("configure: panic contained at feature ABI boundary");
            }
            STATUS_ERROR
        }
    }
}

/// Telemetry is request-driven, preserving the existing throttled console.
extern "system" fn tick() {}

extern "system" fn shutdown() {
    // Dropping the owned state invalidates our copied service table and every
    // resolved-pointer cache. A fresh generation is built only by `init`.
    *state() = None;
}

static FEATURE_API: FeatureApiV1 = FeatureApiV1 {
    struct_size: core::mem::size_of::<FeatureApiV1>() as u32,
    abi_version: ABI_VERSION,
    init,
    dispatch,
    tick,
    configure,
    shutdown,
};

/// Exported ABI entry. The resident loader keeps the returned table for the
/// complete module generation and never caches addresses in another image.
#[unsafe(no_mangle)]
pub extern "system" fn hromatite_feature_v1() -> *const FeatureApiV1 {
    &FEATURE_API
}

/// Manual mapping can call this entry, but reload setup belongs to `init`.
#[unsafe(no_mangle)]
pub extern "system" fn DllMain(
    _module: *mut core::ffi::c_void,
    reason: u32,
    _reserved: *mut core::ffi::c_void,
) -> i32 {
    (reason == 1) as i32
}

#[cfg(test)]
mod game_fixture;
#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support as fake;
    use hromatite_abi::{STATUS_BAD_ARGUMENT, STATUS_NOT_READY};

    fn api() -> &'static FeatureApiV1 {
        // SAFETY: the exported table has static lifetime and a fixed C layout.
        unsafe { &*hromatite_feature_v1() }
    }

    fn start(generation: u64) {
        (api().shutdown)();
        fake::reset();
        let host = fake::api();
        assert_eq!((api().init)(&host, generation), STATUS_OK);
    }

    fn send(command: &str) -> i32 {
        (api().dispatch)(command.as_ptr().cast(), command.len())
    }

    fn configure(key: &str, value: &str) -> i32 {
        (api().configure)(
            key.as_ptr().cast(),
            key.len(),
            value.as_ptr().cast(),
            value.len(),
        )
    }

    #[test]
    fn feature_marker_configuration_and_shutdown_use_the_real_abi() {
        let _serial = fake::serial();
        start(42);
        assert_eq!(send("feature"), STATUS_OK);
        assert!(fake::message_log().contains("feature: generation=42 abi=1"));

        assert_eq!(configure("telemetry", "0"), STATUS_OK);
        assert_eq!(configure("memory", "0"), STATUS_OK);
        assert_eq!(send("q 1000"), STATUS_NOT_READY);
        assert_eq!(configure("memory", "1"), STATUS_OK);
        assert_eq!(configure("m3", "0"), STATUS_OK);
        assert_eq!(configure("unknown", "1"), STATUS_NOT_READY);
        assert_eq!(configure("telemetry", "yes"), STATUS_NOT_READY);
        assert_eq!(send("pos"), STATUS_NOT_READY);
        assert_eq!(send("m3f UnityEngine Camera"), STATUS_NOT_READY);
        let log = fake::message_log();
        assert!(log.contains("configure: unknown key 'unknown'"));
        assert!(log.contains("configure: telemetry expects 0 or 1"));
        assert!(log.contains("pos: disabled by configuration"));

        (api().shutdown)();
        assert_eq!(send("feature"), STATUS_NOT_READY);
        (api().tick)();
        assert_eq!(
            (api().init)(std::ptr::null(), 43),
            hromatite_abi::STATUS_BAD_ARGUMENT
        );
    }

    #[test]
    fn memory_commands_read_aliases_strings_and_qword_module_tags() {
        let _serial = fake::serial();
        start(1);
        fake::map_zeroed(0x1000, 0x200);
        fake::put_u64(0x1000, 0x1122_3344_5566_7788);
        fake::put_u64(0x1008, fake::api().game_assembly_base + 0x345);
        fake::put_ascii(0x11000, "hello");
        fake::put_utf16(0x12000, "wide");

        assert_eq!(send("q 1000 2"), STATUS_OK);
        assert_eq!(send("qword 1000 1"), STATUS_OK);
        assert_eq!(send("b 1000 4"), STATUS_OK);
        assert_eq!(send("bytes 1000 4"), STATUS_OK);
        assert_eq!(send("s 11000"), STATUS_OK);
        assert_eq!(send("ws 12000"), STATUS_OK);
        let log = fake::message_log();
        assert!(log.contains("0x1000: 0x1122334455667788"), "{log}");
        assert!(log.contains("GameAssembly.dll+0x345"), "{log}");
        assert!(log.contains("0x1000: 88 77 66 55 (4 bytes)"), "{log}");
        assert!(log.contains("0x11000: 'hello'"), "{log}");
        assert!(log.contains("0x12000: 'wide'"), "{log}");
        assert!(fake::logs().iter().all(|(tag, _)| tag == "con"));
    }

    #[test]
    fn native_calls_and_writes_stay_behind_host_services() {
        let _serial = fake::serial();
        start(2);
        fake::map_zeroed(0x2000, 0x100);
        fake::put_u64(0x2000, 7);
        fake::on_call(0x3000, fake::returning(0x1234));
        fake::on_call(0x3001, fake::returning(0x5678));

        assert_eq!(send("guard 3000"), STATUS_OK);
        assert_eq!(send("call 3001 11 22 33"), STATUS_OK);
        assert_eq!(send("wq 2000 deadbeef"), STATUS_OK);
        assert_eq!(send("wb 2008 01_00 ff"), STATUS_OK);
        let log = fake::message_log();
        assert!(log.contains("guard 0x3000: returned 0x1234"), "{log}");
        assert!(log.contains("-> 0x5678"), "{log}");
        assert!(log.contains("wq 0x2000 <- 0xdeadbeef: true"), "{log}");
        assert!(log.contains("wb 0x2008 <- 3 byte(s): true"), "{log}");
        assert_eq!(fake::calls()[1].1, vec![0x11, 0x22, 0x33]);
        assert_eq!(fake::bytes(0x2000, 12)[..8], 0xdeadbeefu64.to_le_bytes());
        assert_eq!(&fake::bytes(0x2008, 3)[..], &[1, 0, 0xff]);
    }

    #[test]
    fn unknown_commands_and_usage_errors_have_explicit_responses() {
        let _serial = fake::serial();
        start(3);
        assert_eq!(send("not-a-command"), STATUS_BAD_ARGUMENT);
        assert_eq!(send("q nope"), STATUS_BAD_ARGUMENT);
        assert_eq!(send("call"), STATUS_BAD_ARGUMENT);
        let log = fake::message_log();
        assert!(log.contains("unknown command: not-a-command (try help)"));
        assert!(log.contains("usage: q <addr> [n]"));
        assert!(log.contains("usage: call <addr> [a1] [a2] [a3]"));
    }

    #[test]
    fn m3_commands_use_the_measured_unity6_layout_and_host_guard() {
        let _serial = fake::serial();
        start(10);
        crate::game_fixture::install();
        use crate::game_fixture as fixture;

        assert_eq!(send("m3f UnityEngine Camera"), STATUS_OK);
        assert_eq!(send("m3s cam"), STATUS_OK);
        assert_eq!(send(&format!("m3c {:x}", fixture::CAMERA_CLASS)), STATUS_OK);
        assert_eq!(send("m3i UnityEngine.CoreModule"), STATUS_OK);
        assert_eq!(send("m3l Assembly-CSharp cam 2"), STATUS_OK);
        let log = fake::message_log();
        assert!(log.contains("#167"), "{log}");
        assert!(
            log.contains(&format!("table {:#x}", fixture::CLASS_TABLE)),
            "{log}"
        );
        assert!(log.contains("UnityEngine.Camera"), "{log}");
        assert!(log.contains("+0x0040 m_FieldOfView"), "{log}");
        assert!(log.contains("get_main"), "{log}");
        assert!(log.contains("return_kind=1"), "{log}");
        assert!(
            log.contains(&format!("corlib image {:#x}", fixture::CORLIB_IMAGE)),
            "{log}"
        );
        assert!(
            log.contains("2 class(es) seen, 1 match(es), 0 fault(s)"),
            "{log}"
        );
        let ga = fake::api().game_assembly_base;
        assert!(fake::calls().iter().any(|(address, args)| {
            *address == ga + fixture::IMAGE_GET_CLASS_RVA && args.len() == 2
        }));
    }

    #[test]
    fn a_new_generation_does_not_reuse_the_previous_class_table_cache() {
        let _serial = fake::serial();
        start(11);
        crate::game_fixture::install();
        use crate::game_fixture as fixture;
        assert_eq!(send("m3f UnityEngine Camera"), STATUS_OK);

        let host = fake::api();
        assert_eq!((api().init)(&host, 12), STATUS_OK);
        let replacement = fixture::CLASS_TABLE + 0x1000;
        fake::put_u64(
            fake::api().game_assembly_base + fixture::CLASS_TABLE_RVA,
            replacement,
        );
        assert_eq!(send("m3f UnityEngine Camera"), STATUS_OK);
        assert!(fake::message_log().contains(&format!("table {replacement:#x}")));
        assert_eq!(send("feature"), STATUS_OK);
        assert!(fake::message_log().contains("feature: generation=12 abi=1"));
    }

    #[test]
    fn telemetry_preserves_camera_players_and_overlay_projection_contract() {
        let _serial = fake::serial();
        start(20);
        crate::game_fixture::install();
        crate::game_fixture::install_telemetry();
        use crate::game_fixture as fixture;

        assert_eq!(send("pos"), STATUS_OK);
        assert_eq!(send("who"), STATUS_OK);
        assert_eq!(send("esp"), STATUS_OK);
        let log = fake::message_log();
        assert!(log.contains("pos: Camera.main ="), "{log}");
        assert!(
            log.contains("pos: get_position = (0.000, 0.000, 0.000)"),
            "{log}"
        );
        assert!(
            log.contains("pos: get_localPosition = (1.000, 2.000, 3.000)"),
            "{log}"
        );
        assert!(log.contains("who: 2 player(s)"), "{log}");
        assert!(
            log.contains("id=11 pos=(0.00, 0.00, 0.00) go=\"AvatarOne\""),
            "{log}"
        );
        assert!(
            log.contains("id=12 pos=(1.00, 0.00, 0.00) go=\"AvatarTwo\""),
            "{log}"
        );
        assert!(log.contains("esp: head w=1920 h=1080 err=0.0"), "{log}");
        assert!(log.contains("esp: id=11 d=0.00 x=960.0 y=540.0"), "{log}");
        assert!(log.contains("esp: id=12 d=1.00 x=1440.0 y=540.0"), "{log}");
        assert!(log.contains("w=1.00,0.00,0.00 name=AvatarTwo"), "{log}");

        let vector_calls: Vec<_> = fake::calls()
            .into_iter()
            .filter(|(address, _)| *address == fixture::GET_POSITION)
            .collect();
        assert!(
            vector_calls.iter().all(|(_, args)| args.len() == 2),
            "{vector_calls:?}"
        );
        assert!(
            vector_calls
                .iter()
                .any(|(_, args)| args[1] == fixture::TRANSFORM_INSTANCE)
        );
        for function in [fixture::GET_PROJECTION, fixture::GET_VIEW] {
            assert!(fake::calls().iter().any(|(address, args)| {
                *address == function && args.len() == 2 && args[1] == fixture::CAMERA_INSTANCE
            }));
        }
    }

    #[test]
    fn telemetry_faults_return_answer_lines_instead_of_direct_native_calls() {
        let _serial = fake::serial();
        start(21);
        crate::game_fixture::install();
        crate::game_fixture::install_telemetry();
        fake::on_call(crate::game_fixture::GET_MAIN, fake::fault(0x0000_0005));

        assert_eq!(send("pos"), STATUS_OK);
        let log = fake::message_log();
        assert!(
            log.contains("pos: Camera.main faulted: host status 5"),
            "{log}"
        );
        assert!(fake::calls().iter().any(|(address, args)| {
            *address == crate::game_fixture::GET_MAIN && args.is_empty()
        }));
    }
}
