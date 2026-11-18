//! Resident implementations of the versioned feature ABI. The returned POD
//! table contains no Rust references; feature images borrow output buffers only
//! for the duration of one callback.

// Router integration deliberately comes next; suppress scaffold-only dead code.
#![allow(dead_code)]

use core::ffi::c_char;
#[cfg(target_os = "windows")]
use hromatite_abi::STATUS_OK;
use hromatite_abi::{
    GuardedCallFn, HostApiV1, LogFn, MAX_CALL_ARGS, ReadFn, STATUS_BAD_ARGUMENT, STATUS_ERROR,
    WriteFn,
};

const MAX_IO_BYTES: usize = 1024 * 1024;
const MAX_TAG_BYTES: usize = 32;
const MAX_LOG_BYTES: usize = 64 * 1024;

fn hex_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

/// SHA-256 of the build the features are talking to.
///
/// It must describe the *loaded* module, not the source tree: after a game
/// update the compiled `symbols.rs` is stale, and a feature that keys its
/// struct layout on the old SHA would read the new binary with the old offsets.
/// `buildinfo` validates the compiled descriptor against the image first (no
/// hashing on the normal path) and falls back to hashing only when it moved.
pub(crate) fn game_sha_bytes() -> [u8; 32] {
    if let Some(hex) = hromatite_core::buildinfo::loaded_sha_hex()
        && let Some(bytes) = hromatite_core::buildinfo::decode_sha_hex(hex)
    {
        return bytes;
    }
    compiled_sha_bytes()
}

fn compiled_sha_bytes() -> [u8; 32] {
    let mut out = [0u8; 32];
    let text = hromatite_core::symbols::GAMEASSEMBLY_SHA256.as_bytes();
    for (index, slot) in out.iter_mut().enumerate() {
        let Some(high) = text.get(index * 2).and_then(|value| hex_nibble(*value)) else {
            return [0u8; 32];
        };
        let Some(low) = text.get(index * 2 + 1).and_then(|value| hex_nibble(*value)) else {
            return [0u8; 32];
        };
        *slot = (high << 4) | low;
    }
    out
}

fn borrowed_lossy(pointer: *const c_char, length: usize, cap: usize) -> String {
    if pointer.is_null() {
        return String::new();
    }
    let length = length.min(cap);
    // SAFETY: the feature ABI requires a buffer of at least the declared length,
    // capped above. The pointer is consumed synchronously and never retained.
    let bytes = unsafe { core::slice::from_raw_parts(pointer as *const u8, length) };
    String::from_utf8_lossy(bytes).replace([char::from(13), char::from(10)], " ")
}

extern "system" fn host_log(
    tag: *const c_char,
    tag_len: usize,
    message: *const c_char,
    message_len: usize,
) {
    let tag = borrowed_lossy(tag, tag_len, MAX_TAG_BYTES);
    let message = borrowed_lossy(message, message_len, MAX_LOG_BYTES);
    hromatite_core::log::log_tag(&tag, &message);
}

extern "system" fn host_read(address: u64, out: *mut u8, out_len: usize) -> usize {
    if out.is_null() || out_len == 0 || out_len > MAX_IO_BYTES {
        return 0;
    }
    let bytes = hromatite_core::safe::read_upto(address as usize, out_len);
    if bytes.is_empty() {
        return 0;
    }
    // SAFETY: the feature ABI supplies a writable output buffer of out_len bytes.
    // `bytes.len() <= out_len` and both buffers remain alive for this copy.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), out, bytes.len()) };
    bytes.len()
}

extern "system" fn host_write(address: u64, data: *const u8, data_len: usize) -> usize {
    if data.is_null() || data_len == 0 || data_len > MAX_IO_BYTES {
        return 0;
    }
    // SAFETY: the feature ABI supplies a readable buffer of data_len bytes; the
    // OS-mediated write below does not retain it after this callback returns.
    let bytes = unsafe { core::slice::from_raw_parts(data, data_len) };
    if hromatite_core::safe::write_bytes(address as usize, bytes) {
        data_len
    } else {
        0
    }
}

extern "system" fn host_guarded_call(
    address: u64,
    args: *const u64,
    arg_count: usize,
    result: *mut u64,
) -> i32 {
    if address == 0 || arg_count > MAX_CALL_ARGS || (arg_count != 0 && args.is_null()) {
        return STATUS_BAD_ARGUMENT;
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (args, result);
        STATUS_ERROR
    }
    #[cfg(target_os = "windows")]
    {
        let mut values = [0u64; MAX_CALL_ARGS];
        if arg_count != 0 {
            // SAFETY: the ABI requires arg_count u64 values. The cap above is three
            // and these arguments are copied, never retained by the resident.
            values[..arg_count]
                .copy_from_slice(unsafe { core::slice::from_raw_parts(args, arg_count) });
        }
        // SAFETY: the feature explicitly delegates this opaque native call to the
        // same checked VEH boundary used by the established console commands.
        let call = unsafe {
            match arg_count {
                0 => hromatite_core::veh::call0(address as usize),
                1 => hromatite_core::veh::call1(address as usize, values[0] as usize),
                2 => hromatite_core::veh::call2(address as usize, values[0] as usize, values[1] as usize),
                3 => hromatite_core::veh::call3(
                    address as usize,
                    values[0] as usize,
                    values[1] as usize,
                    values[2] as usize,
                ),
                4 => hromatite_core::veh::call4(
                    address as usize,
                    values[0] as usize,
                    values[1] as usize,
                    values[2] as usize,
                    values[3] as usize,
                ),
                5 => hromatite_core::veh::call5(
                    address as usize,
                    values[0] as usize,
                    values[1] as usize,
                    values[2] as usize,
                    values[3] as usize,
                    values[4] as usize,
                ),
                6 => hromatite_core::veh::call6(
                    address as usize,
                    values[0] as usize,
                    values[1] as usize,
                    values[2] as usize,
                    values[3] as usize,
                    values[4] as usize,
                    values[5] as usize,
                ),
                7 => hromatite_core::veh::call7(
                    address as usize,
                    values[0] as usize,
                    values[1] as usize,
                    values[2] as usize,
                    values[3] as usize,
                    values[4] as usize,
                    values[5] as usize,
                    values[6] as usize,
                ),
                8 => hromatite_core::veh::call8(
                    address as usize,
                    values[0] as usize,
                    values[1] as usize,
                    values[2] as usize,
                    values[3] as usize,
                    values[4] as usize,
                    values[5] as usize,
                    values[6] as usize,
                    values[7] as usize,
                ),
                9 => hromatite_core::veh::call9(
                    address as usize,
                    values[0] as usize,
                    values[1] as usize,
                    values[2] as usize,
                    values[3] as usize,
                    values[4] as usize,
                    values[5] as usize,
                    values[6] as usize,
                    values[7] as usize,
                    values[8] as usize,
                ),
                10 => hromatite_core::veh::call10(
                    address as usize,
                    values[0] as usize,
                    values[1] as usize,
                    values[2] as usize,
                    values[3] as usize,
                    values[4] as usize,
                    values[5] as usize,
                    values[6] as usize,
                    values[7] as usize,
                    values[8] as usize,
                    values[9] as usize,
                ),
                _ => unreachable!(),
            }
        };
        match call {
            Ok(value) => {
                if !result.is_null() {
                    // SAFETY: a non-null result is a writable u64 supplied by the
                    // feature for this synchronous call.
                    unsafe { *result = value as u64 };
                }
                STATUS_OK
            }
            Err(_) => STATUS_ERROR,
        }
    }
}

/// Build a stable-by-value table for a newly loaded generation. The caller puts
/// it in leaked host-owned storage before passing the pointer across the ABI.
pub(crate) fn current_host_api() -> Option<HostApiV1> {
    let (ga, ga_size) = hromatite_core::procs::own_module(hromatite_core::GAME_ASSEMBLY)?;
    let (up, up_size) = hromatite_core::procs::own_module(hromatite_core::UNITY_PLAYER)?;
    Some(HostApiV1 {
        struct_size: core::mem::size_of::<HostApiV1>() as u32,
        abi_version: hromatite_abi::ABI_VERSION,
        game_assembly_base: ga as u64,
        game_assembly_size: ga_size as u64,
        unity_player_base: up as u64,
        unity_player_size: up_size as u64,
        game_assembly_sha256: game_sha_bytes(),
        log: host_log as LogFn,
        read: host_read as ReadFn,
        write: host_write as WriteFn,
        guarded_call: host_guarded_call as GuardedCallFn,
    })
}

/// Explicit `hot-smoke` fixture for mapper-only tests without Unity modules.
/// Only the canary marker may be invoked with it; il2cpp/M3 is never simulated.
#[cfg(target_os = "windows")]
pub(crate) fn smoke_host_api() -> HostApiV1 {
    HostApiV1 {
        struct_size: core::mem::size_of::<HostApiV1>() as u32,
        abi_version: hromatite_abi::ABI_VERSION,
        game_assembly_base: 0x0001_8000_0000,
        game_assembly_size: 0x0f44_0000,
        unity_player_base: 0x0001_4000_0000,
        unity_player_size: 0x0215_3000,
        game_assembly_sha256: game_sha_bytes(),
        log: host_log as LogFn,
        read: host_read as ReadFn,
        write: host_write as WriteFn,
        guarded_call: host_guarded_call as GuardedCallFn,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn game_hash_is_decoded_not_empty() {
        let hash = super::game_sha_bytes();
        let expected = hromatite_core::symbols::GAMEASSEMBLY_SHA256;
        assert_eq!(hex(&hash), expected);
        // First four bytes must decode from the same string (build agnostic).
        assert_eq!(hex(&hash[..4]).len(), 8);
        assert!(expected.starts_with(&hex(&hash[..4])));
    }

    #[test]
    fn null_output_and_excess_call_arguments_are_rejected_without_dereferences() {
        assert_eq!(super::host_read(0x1000, core::ptr::null_mut(), 8), 0);
        let mut out = 0u64;
        let status = super::host_guarded_call(0x1000, core::ptr::null(), 4, &mut out);
        assert_eq!(status, hromatite_abi::STATUS_BAD_ARGUMENT);
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}
