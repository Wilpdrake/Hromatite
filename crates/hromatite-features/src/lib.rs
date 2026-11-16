//! Replaceable feature-image scaffold. Real commands are implemented in M1.
#![allow(unsafe_code)]

use hromatite_abi::{
    ABI_VERSION, FeatureApiV1, HostApiV1, STATUS_ERROR, STATUS_NOT_READY, STATUS_OK,
};

extern "system" fn init(_host: *const HostApiV1, _generation: u64) -> i32 {
    STATUS_OK
}

extern "system" fn dispatch(_command: *const i8, _command_len: usize) -> i32 {
    STATUS_ERROR
}

extern "system" fn configure(
    _key: *const i8,
    _key_len: usize,
    _value: *const i8,
    _value_len: usize,
) -> i32 {
    STATUS_NOT_READY
}

extern "system" fn tick() {}
extern "system" fn shutdown() {}

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
