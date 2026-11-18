//! Stable C-string stashing and VEH-guarded 3-argument Win64 calls.
//!
//! [`stash_cstr`] hands out process-lifetime NUL-terminated ASCII pointers so
//! foreign C functions that take `const char*` never observe a moved buffer.
//! [`call3`] invokes a verified 3-argument function pointer under [`crate::veh`].

use std::sync::{Mutex, PoisonError};

/// Stable NUL-terminated ASCII copies for functions that take `const char*`.
///
/// il2cpp's `class_from_name` only reads the bytes during the call, so a leaked
/// 256-byte box per lookup is cheaper (and far simpler) than an allocation the
/// game could move.
pub fn stash_cstr(text: &str) -> Option<usize> {
    const CAP: usize = 256;
    // Boxed so that `Vec` growth never moves a buffer il2cpp may still hold;
    // the addresses handed out must stay valid for the process lifetime.
    #[allow(clippy::vec_box)]
    static POOL: Mutex<Vec<Box<[u8; CAP]>>> = Mutex::new(Vec::new());
    let bytes = text.as_bytes();
    if bytes.len() >= CAP {
        return None;
    }
    let mut buffer = Box::new([0u8; CAP]);
    buffer[..bytes.len()].copy_from_slice(bytes);
    let address = buffer.as_ptr() as usize;
    let mut pool = POOL.lock().unwrap_or_else(PoisonError::into_inner);
    pool.push(buffer);
    Some(address)
}

/// Call a verified three-argument export under the VEH guard.
#[cfg(target_os = "windows")]
pub fn call3(f: usize, a: usize, b: usize, c: usize) -> Option<usize> {
    if f == 0 {
        return None;
    }
    // SAFETY: callers pass a verified export address with this Win64 signature;
    // VEH turns a bad call into `Err`.
    unsafe { crate::veh::call3(f, a, b, c) }.ok()
}

#[cfg(not(target_os = "windows"))]
pub fn call3(_f: usize, _a: usize, _b: usize, _c: usize) -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::stash_cstr;

    #[test]
    fn stash_cstr_returns_distinct_stable_nul_terminated_copies() {
        let first = stash_cstr("PhotonPeer").expect("short string fits");
        let second = stash_cstr("PhotonPeer").expect("short string fits");
        assert_ne!(first, second, "each lookup gets its own copy");
        for address in [first, second] {
            // SAFETY: the pool owns each 256-byte box for the process lifetime.
            let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, 16) };
            assert_eq!(&bytes[..11], b"PhotonPeer\0");
        }
        assert!(stash_cstr(&"x".repeat(256)).is_none());
    }
}
