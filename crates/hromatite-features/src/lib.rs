//! Dynamically replaceable feature commands over the frozen Hromatite C ABI.
//!
//! Every game-memory access and native call is delegated to the resident host.
//! No core runtime, VEH handler, panic hook or leaked cache lives in this image.
// `unsafe` is allowed per item, not for the crate: the ABI boundary is the only
// place that touches raw pointers, and each such site carries its own SAFETY note.
#![deny(unsafe_code)]

mod eac;
mod feature;
mod host;
mod m3;
mod render;
mod telemetry;

use feature::Feature;
use hromatite_abi::{
    ABI_VERSION, FeatureApiV1, HostApiV1, STATUS_BAD_ABI, STATUS_BAD_ARGUMENT, STATUS_NOT_READY,
    STATUS_OK, host_header_valid,
};
use std::cell::UnsafeCell;

/// The resident serializes every lifecycle call on the established game thread;
/// its hook BUSY admission also prevents host callbacks from re-entering us.
/// Recovery may abandon feature frames, so this cell deliberately owns no
/// acquisition state that could survive in an acquired or poisoned condition.
struct Lifecycle(UnsafeCell<Option<Feature>>);

// SAFETY: all access is externally serialized by the resident as described
// above. No reference or pointer escapes a synchronous ABI invocation.
#[allow(unsafe_code)]
unsafe impl Sync for Lifecycle {}

static STATE: Lifecycle = Lifecycle(UnsafeCell::new(None));

fn state() -> *mut Option<Feature> {
    STATE.0.get()
}

#[allow(unsafe_code)]
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
    // SAFETY: the resident exclusively serializes this synchronous lifecycle
    // access. Validation is complete before replacing the previous generation.
    unsafe { *state() = Some(Feature::new(host, generation)) };
    STATUS_OK
}

extern "system" fn init(host: *const HostApiV1, generation: u64) -> i32 {
    init_impl(host, generation)
}

#[allow(unsafe_code)]
fn dispatch_impl(command: *const i8, command_len: usize) -> i32 {
    // SAFETY: the resident exclusively serializes this synchronous invocation;
    // BUSY admission prevents a host callback from creating a second borrow.
    let state = unsafe { &mut *state() };
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
    dispatch_impl(command, command_len)
}

#[allow(unsafe_code)]
fn configure_impl(key: *const i8, key_len: usize, value: *const i8, value_len: usize) -> i32 {
    // SAFETY: the resident exclusively serializes this synchronous invocation;
    // BUSY admission prevents a host callback from creating a second borrow.
    let state = unsafe { &mut *state() };
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
    configure_impl(key, key_len, value, value_len)
}

/// Managed tick (LateUpdate / native PlayerLoop). Telemetry stays request-driven;
/// only anti-cheat housekeeping runs here: heartbeat cadence and the game-log
/// mirror. Bounded file I/O, no game-memory access, no native calls.
#[allow(unsafe_code)]
extern "system" fn tick() {
    // SAFETY: the resident exclusively serializes this synchronous invocation on
    // the established game thread; BUSY admission prevents re-entry from a host
    // callback, and no reference escapes this call.
    let state = unsafe { &mut *state() };
    if let Some(feature) = state.as_mut() {
        feature.tick();
    }
}

#[allow(unsafe_code)]
extern "system" fn shutdown() {
    // Dropping the owned state invalidates our copied service table and every
    // resolved-pointer cache. A fresh generation is built only by `init`.
    // SAFETY: shutdown is serialized against every other lifecycle entry by
    // the resident, and no feature reference escapes an earlier invocation.
    unsafe { *state() = None }
}

/// Per-frame entry: the resident calls this once per game frame, outside the
/// throttled tick. It must stay bounded and free of file or console I/O.
#[allow(unsafe_code)]
extern "system" fn frame() {
    // SAFETY: the resident exclusively serializes this synchronous invocation on
    // the established game thread; BUSY admission prevents re-entry from a host
    // callback, and no reference escapes this call.
    let state = unsafe { &mut *state() };
    if let Some(feature) = state.as_mut() {
        feature.frame();
    }
}

static FEATURE_API: FeatureApiV1 = FeatureApiV1 {
    struct_size: core::mem::size_of::<FeatureApiV1>() as u32,
    abi_version: ABI_VERSION,
    init,
    dispatch,
    tick,
    frame,
    configure,
    shutdown,
};

/// Exported ABI entry. The resident loader keeps the returned table for the
/// complete module generation and never caches addresses in another image.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "system" fn hromatite_feature_v1() -> *const FeatureApiV1 {
    &FEATURE_API
}

/// Manual mapping can call this entry, but reload setup belongs to `init`.
#[allow(unsafe_code)]
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
#[allow(unsafe_code)]
mod test_support;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support as fake;
    use hromatite_abi::{STATUS_BAD_ARGUMENT, STATUS_NOT_READY};

    fn production_lifecycle_source() -> &'static str {
        include_str!("lib.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production lifecycle source")
    }

    #[test]
    fn feature_abi_leaves_panic_containment_to_the_resident() {
        let source = production_lifecycle_source();
        assert!(
            !source.contains("catch_unwind") && !source.contains("AssertUnwindSafe"),
            "manual-mapped feature images cannot unwind through erased PE metadata"
        );
    }

    #[test]
    fn feature_lifecycle_has_no_lock_to_survive_host_recovery() {
        let source = production_lifecycle_source();
        assert!(
            !source.contains("Mutex") && !source.contains(".lock()"),
            "a host guard recovery must not strand a lifecycle lock in its locked state"
        );
    }

    #[allow(unsafe_code)]
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
        assert!(
            fake::message_log()
                .contains(&format!("feature: generation=42 abi={ABI_VERSION}"))
        );

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

    /// Overload selection reads the signature, because `Mesh.SetVertices` and
    /// `SetTriangles` exist for both `T[]` and `List<T>` with the same arity:
    /// an array passed where a list is expected makes the callee read fields the
    /// object does not have.
    #[test]
    fn parameter_types_come_from_the_signature_not_from_the_name() {
        let _serial = fake::serial();
        start(41);
        let host = crate::host::Host::new(fake::api());
        let mut cache = crate::m3::Cache::default();
        let Some(metadata) = crate::m3::Metadata::new(&host, &mut cache) else {
            panic!("the fixture build must resolve a descriptor");
        };

        // MethodInfo: parameters pointer at the layout's own offset.
        let layout = hromatite_abi::build::newest().layout;
        let info = 0x0000_0010_0000_0000u64;
        let params = info + 0x100;
        let vector3_array = info + 0x200;
        let array_type = info + 0x280;
        let element = info + 0x300;
        let vector3 = 0x0000_0010_0000_4000u64;
        let int32 = 0x0000_0010_0000_5000u64;
        fake::map_zeroed(info, 0x800);
        fake::put_u64(info + u64::from(layout.method_parameters), params);
        fake::put_u64(params, vector3_array);
        fake::put_u64(params + 8, int32);
        // SZARRAY: `data` points at an Il2CppArrayType whose first field is the
        // element type.
        fake::put_u64(vector3_array, array_type);
        fake::put_u32(vector3_array + 8, 0x1d << 16);
        fake::put_u64(array_type, element);
        fake::put_u32(element + 8, 0x11 << 16);
        // System.Int32 is a plain I4, not a class.
        fake::put_u32(int32 + 8, 0x08 << 16);

        // `Il2CppType` does not carry a class pointer, so the only way to a class
        // is `il2cpp_class_from_il2cpp_type`; the mock answers for Vector3 only.
        let class_from_type = fake::api().game_assembly_base
            + u64::from(hromatite_abi::build::newest().il2cpp.class_from_type);
        fake::on_call(
            class_from_type,
            Box::new(move |args| match args.first().copied() {
                Some(address) if address == element => (0, vector3),
                _ => (0, 0),
            }),
        );

        assert!(
            metadata.parameter_is_array(info as usize, 0),
            "Vector3[] must be seen as an array"
        );
        assert_eq!(
            metadata.parameter_array_element_class(info as usize, 0),
            Some(vector3 as usize)
        );
        assert_eq!(metadata.parameter_kind(info as usize, 1), Some(0x08));
        assert_eq!(
            metadata.parameter_class(info as usize, 1),
            None,
            "Int32 is not an object"
        );
        assert_eq!(metadata.parameter_kind(info as usize, 2), None, "no third parameter");
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

    /// Resident-only commands are refused from the shared table *before* the
    /// `eac` family branch, so a forwarded `eac.module` (resident-scoped since
    /// it reads EAC's own modules) is rejected here instead of falling through
    /// to `eac.*` dispatch.
    #[test]
    fn resident_only_commands_are_refused_before_the_eac_family_branch() {
        let _serial = fake::serial();
        start(50);
        assert_eq!(send("eac.module"), STATUS_NOT_READY);
        assert_eq!(send("gct"), STATUS_NOT_READY);
        assert_eq!(send("quit"), STATUS_NOT_READY);
        let log = fake::message_log();
        assert!(
            log.contains("eac.module: resident-only command (feature image did not execute it)"),
            "{log}"
        );
        assert!(log.contains("quit: resident-only command"), "{log}");
        assert!(
            !log.contains("unknown eac command"),
            "eac.module must not reach eac dispatch: {log}"
        );
    }

    /// `draw` is the in-game overlay switch: it must answer from the feature
    /// image, stay off by default and never invent an unknown-command error.
    #[test]
    fn the_in_game_renderer_is_switched_by_the_draw_command() {
        let _serial = fake::serial();
        start(40);
        assert_eq!(send("draw"), STATUS_OK);
        assert_eq!(send("draw status"), STATUS_OK);
        let initial = fake::message_log();
        assert!(initial.contains("draw: off mode segments=0 players=0"), "{initial}");

        assert_eq!(send("draw on"), STATUS_OK);
        assert_eq!(send("draw status"), STATUS_OK);
        assert!(
            fake::message_log().contains("draw: on mode"),
            "{}",
            fake::message_log()
        );

        assert_eq!(send("draw off"), STATUS_OK);
        assert_eq!(send("draw sideways"), STATUS_BAD_ARGUMENT);
        assert!(
            fake::message_log().contains("usage: draw [on|off|status|probe]"),
            "{}",
            fake::message_log()
        );
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
        assert!(
            fake::message_log()
                .contains(&format!("feature: generation=12 abi={ABI_VERSION}"))
        );
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

    #[test]
    fn eac_commands_write_epochs_heartbeats_and_a_session_end_to_jsonl() {
        let _serial = fake::serial();
        start(30);
        let dir = std::env::temp_dir().join(format!(
            "hromatite-features-eac-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("eac.jsonl");

        assert_eq!(configure("eac_interval_ms", "5000"), STATUS_OK);
        assert_eq!(configure("eac_interval_ms", "nope"), STATUS_NOT_READY);
        assert_eq!(send(&format!("eac.log {}", path.display())), STATUS_OK);
        assert_eq!(send("eac.begin"), STATUS_OK);
        assert_eq!(send("eac.tick"), STATUS_OK);
        assert_eq!(send("eac.tick"), STATUS_OK);
        assert_eq!(send("eac.status"), STATUS_OK);
        assert_eq!(send("eac.end OnLeftRoom"), STATUS_OK);
        assert_eq!(send("eac.tick"), STATUS_NOT_READY, "epoch is closed");

        assert_eq!(configure("eac", "0"), STATUS_OK);
        assert_eq!(send("eac.status"), STATUS_NOT_READY, "family disabled");
        assert_eq!(configure("eac", "1"), STATUS_OK);

        let text = std::fs::read_to_string(&path).expect("jsonl log");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "records: {text}");
        assert!(lines[0].contains("\"ev\":\"session_begin\""));
        assert_eq!(lines[1].matches("\"ev\":\"heartbeat\"").count(), 1);
        assert!(lines[1].contains("\"frame_len\":144"), "{}", lines[1]);
        assert!(
            lines[1].contains("\"source\":\"model\""),
            "a synthesized beat must say so: {}",
            lines[1]
        );
        assert!(lines[2].contains("\"ev\":\"heartbeat\""));
        assert!(lines[3].contains("\"ev\":\"session_end\""));
        assert!(lines[3].contains("\"heartbeats\":2"), "{}", lines[3]);
        assert!(lines[3].contains("\"reason\":\"OnLeftRoom\""));

        let console = fake::message_log();
        assert!(console.contains("eac.tick: epoch=1 seq=1 hb_in_epoch=1"));
        assert!(console.contains("eac.status: epoch=1"), "{console}");
        assert!(console.contains("checksum=crc32c"), "{console}");
        assert!(
            console.contains("eac.status: disabled by configuration"),
            "{console}"
        );

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn hook_observations_drive_epochs_and_the_jsonl_log() {
        let _serial = fake::serial();
        start(31);
        let dir = std::env::temp_dir().join(format!(
            "hromatite-features-hooks-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("hooks.jsonl");
        assert_eq!(configure("eac_auto", "0"), STATUS_OK);
        assert_eq!(send(&format!("eac.log {}", path.display())), STATUS_OK);

        // BeginSession(handle, mode=1) observed by the Rust hook handler.
        hromatite_eac::Hooks::observe(hromatite_eac::HookEvent::SessionBegin {
            mode: 1,
            ts_ms: 1_000,
        });
        (api().tick)();
        hromatite_eac::Hooks::observe(hromatite_eac::HookEvent::OutMessage {
            len: 144,
            dt_ms: 0,
            ts_ms: 1_100,
        });
        hromatite_eac::Hooks::observe(hromatite_eac::HookEvent::Protect {
            in_len: 122,
            out_len: 144,
        });
        hromatite_eac::Hooks::observe(hromatite_eac::HookEvent::SessionEnd { ts_ms: 2_000 });
        (api().tick)();
        assert_eq!(send("eac.hooks"), STATUS_OK);
        assert_eq!(send("eac.hooks nope"), STATUS_BAD_ARGUMENT);
        assert_eq!(configure("eac_hooks", "0"), STATUS_OK);
        assert_eq!(configure("eac_hooks", "1"), STATUS_OK);

        let text = std::fs::read_to_string(&path).expect("jsonl log");
        assert!(text.contains("\"ev\":\"session_begin\""));
        assert!(text.contains("\"source\":\"hook\""));
        assert!(text.contains("\"ev\":\"ac_out\""));
        assert!(text.contains("\"ev\":\"ac_protect\""));
        assert!(text.contains("\"overhead\":22"), "{text}");
        assert!(text.contains("\"reason\":\"hook\""));

        let console = fake::message_log();
        assert!(console.contains("BeginSession epoch=1 mode=1"), "{console}");
        assert!(
            console.contains("EndSession epoch=1 heartbeats=0"),
            "{console}"
        );
        assert!(console.contains("eac.hooks: installed=false"), "{console}");

        std::fs::remove_dir_all(dir).ok();
    }

    /// One capture record: `u32 len, u32 dt_ms, bytes`.
    fn record(dt_ms: u32, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + data.len());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&dt_ms.to_le_bytes());
        out.extend_from_slice(data);
        out
    }

    fn wire_frame(flags: u16, session_id: u64, sequence: u64, body: &[u8]) -> Vec<u8> {
        hromatite_eac::Frame {
            header: hromatite_eac::Header {
                version: hromatite_eac::codec::VERSION,
                flags,
                session_id,
                sequence,
                checksum: 0,
            },
            payload: body,
        }
        .encode(hromatite_eac::ChecksumKind::Crc32c)
    }

    fn wire_heartbeat(session_id: u64, sequence: u64) -> Vec<u8> {
        let body = vec![0u8; hromatite_eac::codec::WIRE_BODY_LEN];
        wire_frame(1, session_id, sequence, &body)
    }

    /// When the resident owns the outbound wrapper this image sees no frames at
    /// all, so the only truth available is the capture file. The tap must mirror
    /// it: real `SessionId`/`Sequence`, the transport's `dt`, the real 512-byte
    /// size, a verified CRC — and it must silence the synthetic timer.
    #[test]
    fn the_capture_tap_mirrors_real_wire_heartbeats() {
        let _serial = fake::serial();
        start(32);
        let dir = std::env::temp_dir().join(format!(
            "hromatite-features-tap-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let log = dir.join("tap.jsonl");
        let capture = dir.join("capture.bin");
        std::fs::write(&capture, b"").expect("empty capture");

        // `eac_auto=1` is the default: without a tap it would synthesize beats
        // on its own timer, which is exactly what must not happen here.
        assert_eq!(configure("eac_auto", "1"), STATUS_OK);
        assert_eq!(send(&format!("eac.log {}", log.display())), STATUS_OK);
        assert_eq!(send(&format!("eac.tap {}", capture.display())), STATUS_OK);

        let mut bytes = Vec::new();
        bytes.extend(record(0, &wire_frame(0x0013, 0x158b, 9, &[])));
        bytes.extend(record(0, &wire_heartbeat(0x158b, 1)));
        bytes.extend(record(2_007, &wire_heartbeat(0x158b, 2)));
        std::fs::write(&capture, &bytes).expect("capture");
        (api().tick)();
        assert_eq!(send("eac.status"), STATUS_OK);

        let text = std::fs::read_to_string(&log).expect("jsonl");
        let beats: Vec<&str> = text
            .lines()
            .filter(|line| line.contains("\"ev\":\"heartbeat\""))
            .collect();
        assert_eq!(beats.len(), 2, "records: {text}");
        assert!(beats[0].contains("\"frame_len\":512"), "{}", beats[0]);
        assert!(beats[0].contains("\"source\":\"wire\""), "{}", beats[0]);
        assert!(
            beats[0].contains("\"sid\":\"0x000000000000158b\""),
            "{}",
            beats[0]
        );
        assert!(beats[0].contains("\"seq\":1"), "{}", beats[0]);
        assert!(beats[1].contains("\"seq\":2"), "{}", beats[1]);
        assert!(beats[1].contains("\"dt_ms\":2007"), "{}", beats[1]);
        assert!(
            !text.contains("\"source\":\"model\""),
            "the tap must suppress synthetic beats: {text}"
        );

        let console = fake::message_log();
        assert!(console.contains("source=wire"), "{console}");
        assert!(console.contains("observed=2"), "{console}");
        assert!(console.contains("interval_ms=2007"), "{console}");
        assert!(console.contains("crc_ok=3"), "{console}");
        assert!(console.contains("crc_bad=0"), "{console}");

        assert_eq!(send("eac.tap off"), STATUS_OK);
        assert_eq!(send("eac.status"), STATUS_OK);
        assert!(
            fake::message_log().contains("source=model"),
            "detaching must return the model"
        );

        std::fs::remove_dir_all(dir).ok();
    }
}
