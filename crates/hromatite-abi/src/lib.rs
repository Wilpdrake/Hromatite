//! Stable C ABI between the resident runtime and replaceable feature images.
//!
//! No Rust-owned value crosses this interface. Buffers are borrowed only for
//! the duration of a callback and all strings are UTF-8 byte slices with an
//! explicit length. Function pointers use the Windows x64 `system` ABI.
#![forbid(unsafe_code)]

use core::ffi::c_char;

pub mod build;

pub const ABI_VERSION: u32 = 2;
pub const FEATURE_ENTRY_EXPORT: &[u8] = b"hromatite_feature_v1\0";
pub const STATUS_OK: i32 = 0;
pub const STATUS_BAD_ABI: i32 = -1;
pub const STATUS_BAD_ARGUMENT: i32 = -2;
pub const STATUS_NOT_READY: i32 = -3;
pub const STATUS_ERROR: i32 = -4;
/// Largest number of register arguments a single delegated call may carry.
///
/// Win64 passes the first four integer/pointer arguments in RCX, RDX, R8, R9,
/// and the callers here never need a fifth: `Graphics.DrawMesh(mesh, matrix,
/// material, layer)` is the widest shape the overlay issues.
pub const MAX_CALL_ARGS: usize = 4;

/// Resident logging service. Both slices are borrowed UTF-8 data.
pub type LogFn = extern "system" fn(
    tag: *const c_char,
    tag_len: usize,
    message: *const c_char,
    message_len: usize,
);
/// Copy process bytes into `out`. Returns the number of bytes actually read.
pub type ReadFn = extern "system" fn(address: u64, out: *mut u8, out_len: usize) -> usize;
/// Copy bytes into process memory. Returns the number of bytes written.
pub type WriteFn = extern "system" fn(address: u64, data: *const u8, data_len: usize) -> usize;
/// Invoke a native address through the resident VEH boundary.
/// `result == 0` means success; otherwise the resident fault status is returned.
pub type GuardedCallFn = extern "system" fn(
    address: u64,
    args: *const u64,
    arg_count: usize,
    result: *mut u64,
) -> i32;

/// Services and immutable process facts supplied by the resident runtime.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct HostApiV1 {
    pub struct_size: u32,
    pub abi_version: u32,
    pub game_assembly_base: u64,
    pub game_assembly_size: u64,
    pub unity_player_base: u64,
    pub unity_player_size: u64,
    pub game_assembly_sha256: [u8; 32],
    pub log: LogFn,
    pub read: ReadFn,
    pub write: WriteFn,
    pub guarded_call: GuardedCallFn,
}

/// Replaceable feature image lifecycle.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct FeatureApiV1 {
    pub struct_size: u32,
    pub abi_version: u32,
    /// Must validate and retain `host` before returning `STATUS_OK`.
    pub init: extern "system" fn(host: *const HostApiV1, generation: u64) -> i32,
    /// Execute one complete console command. Answers are emitted through `log`.
    pub dispatch: extern "system" fn(command: *const c_char, command_len: usize) -> i32,
    /// Optional bounded feature work on the already-established game thread.
    /// Called at the throttled tick rate (command polling, anti-cheat cadence).
    pub tick: extern "system" fn(),
    /// Optional per-frame work on the same game thread, called once per game
    /// frame with no file or command I/O of its own.
    ///
    /// Separate from `tick` on purpose: `Graphics.DrawMesh` applies to exactly
    /// one frame, so a 4 Hz tick would flash the overlay instead of drawing it,
    /// while raising the tick rate would put the command file and the anti-cheat
    /// cadence on the frame path.
    pub frame: extern "system" fn(),
    /// Apply a UTF-8 key/value pair. Unknown keys must not crash the module.
    pub configure: extern "system" fn(
        key: *const c_char,
        key_len: usize,
        value: *const c_char,
        value_len: usize,
    ) -> i32,
    /// Stop accepting work and release feature-owned resources.
    pub shutdown: extern "system" fn(),
}

/// The sole export resolved by the local mapper in a new feature generation.
pub type FeatureEntryFn = extern "system" fn() -> *const FeatureApiV1;

/// Validate fixed headers before following any lifecycle function pointer.
pub fn host_header_valid(host: Option<&HostApiV1>) -> bool {
    host.is_some_and(|host| {
        host.abi_version == ABI_VERSION
            && host.struct_size as usize >= core::mem::size_of::<HostApiV1>()
    })
}

/// This check is meaningful once `api` is known to point at a readable header.
pub fn feature_header_valid(api: &FeatureApiV1) -> bool {
    api.abi_version == ABI_VERSION && api.struct_size as usize >= core::mem::size_of::<FeatureApiV1>()
}

#[cfg(test)]
mod tests {
    use super::*;

    extern "system" fn test_init(_: *const HostApiV1, _: u64) -> i32 { STATUS_ERROR }
    extern "system" fn test_dispatch(_: *const c_char, _: usize) -> i32 { STATUS_ERROR }
    extern "system" fn test_tick() {}
    extern "system" fn test_frame() {}
    extern "system" fn test_configure(_: *const c_char, _: usize, _: *const c_char, _: usize) -> i32 { STATUS_ERROR }
    extern "system" fn test_shutdown() {}

    #[test]
    fn abi_is_explicitly_versioned_and_entry_name_is_nul_terminated() {
        assert_eq!(ABI_VERSION, 2);
        assert_eq!(FEATURE_ENTRY_EXPORT.last(), Some(&0));
        assert_eq!(FEATURE_ENTRY_EXPORT, b"hromatite_feature_v1\0");
    }

    #[test]
    fn host_header_rejects_null_and_feature_header_checks_exact_version() {
        assert!(!host_header_valid(None));
        let api = FeatureApiV1 {
            struct_size: core::mem::size_of::<FeatureApiV1>() as u32,
            abi_version: ABI_VERSION + 1,
            init: test_init,
            dispatch: test_dispatch,
            tick: test_tick,
            frame: test_frame,
            configure: test_configure,
            shutdown: test_shutdown,
        };
        assert!(!feature_header_valid(&api));
    }

    #[test]
    fn no_rust_owned_containers_appear_in_the_abi() {
        let host_fields = core::mem::size_of::<HostApiV1>();
        let feature_fields = core::mem::size_of::<FeatureApiV1>();
        assert!(host_fields <= 128);
        // Two u32 header words plus six lifecycle pointers, eight-byte aligned.
        assert!(feature_fields <= 64);
        assert_eq!(core::mem::size_of::<*const core::ffi::c_void>(), 8);
    }
}
