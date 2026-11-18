//! The EAC frame checksum: CRC-32C (Castagnoli) with `init = 0` and
//! `xorout = 0`.
//!
//! Verified offline on 451/451 captured 24-byte-headered frames
//! (`~/EAC-RE/findings/frame_structure.md` §3), so this is not a hypothesis: the
//! four bytes at `[20:24]` of every heartbeat frame are exactly this value over
//! the frame with those four bytes zeroed.
//!
//! Same reflected polynomial as the standard CRC-32C (`0x82F6_3B78`, i.e. the
//! reversed Castagnoli polynomial); the only difference is that EAC does not
//! invert the register on the way in or out. The reference implementation lives
//! in the `hromatite-eac` crate (`digest::crc32c_eac`) next to the report
//! replayer; the payload needs it here because the Photon transport
//! (`raw::photon::tick`) rebuilds frames without depending on that crate.

#![forbid(unsafe_code)]

/// Reflected Castagnoli polynomial (`0x1EDC6F41` bit-reversed).
pub const POLY: u32 = 0x82F6_3B78;

/// CRC-32C with `init = 0`, `xorout = 0` — the EAC frame checksum.
pub fn crc32c_eac(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ POLY
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// Store the checksum of `frame` into `frame[at..at+4]` after zeroing that
/// window — the exact recipe every live frame follows.
///
/// Returns `false` when `frame` is too short to hold a checksum at `at`.
pub fn seal_checksum(frame: &mut [u8], at: usize) -> bool {
    let Some(end) = at.checked_add(4) else {
        return false;
    };
    if frame.len() < end {
        return false;
    }
    frame[at..end].fill(0);
    let crc = crc32c_eac(frame);
    frame[at..end].copy_from_slice(&crc.to_le_bytes());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_reference_vector() {
        // Same check value as `hromatite_eac::digest::crc32c_eac`, cross-checked
        // against `~/EAC-RE/tools/frame_analyze.py::eac_crc32c`.
        assert_eq!(crc32c_eac(b"123456789"), 0x58e3_fa20);
        assert_eq!(crc32c_eac(b""), 0);
    }

    #[test]
    fn sealing_writes_a_checksum_that_verifies_over_the_zeroed_window() {
        let mut frame = vec![0xABu8; 512];
        frame[20..24].copy_from_slice(&[0x11, 0x22, 0x33, 0x44]);
        assert!(seal_checksum(&mut frame, 20));
        let stored = u32::from_le_bytes(frame[20..24].try_into().expect("4 bytes"));
        let mut zeroed = frame.clone();
        zeroed[20..24].fill(0);
        assert_eq!(stored, crc32c_eac(&zeroed));
        assert!(seal_checksum(&mut frame, 20), "idempotent");
        let mut short = [0u8; 3];
        assert!(!seal_checksum(&mut short, 20));
    }
}
