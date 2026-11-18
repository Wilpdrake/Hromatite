//! SHA-256, HMAC-SHA256 and the two CRC-32 variants the EAC checksum may use.
//!
//! The SHA-256 core is ported verbatim from `crates/hromatite-core/src/sha256.rs`
//! (attribution: same project). It is copied rather than depended upon because
//! `hromatite-core` is Win32-specific and a feature image should not link it.
//! Only `digest`/`hex_digest` are reproduced; streaming and file hashing stay in
//! core.

const INITIAL: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

fn compress(state: &mut [u32; 8], block: &[u8]) {
    debug_assert_eq!(block.len(), 64);
    let mut words = [0u32; 64];
    for (index, bytes) in block.as_chunks::<4>().0.iter().enumerate() {
        words[index] = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    }
    for index in 16..64 {
        let x = words[index - 15];
        let y = words[index - 2];
        let small_sigma0 = x.rotate_right(7) ^ x.rotate_right(18) ^ (x >> 3);
        let small_sigma1 = y.rotate_right(17) ^ y.rotate_right(19) ^ (y >> 10);
        words[index] = words[index - 16]
            .wrapping_add(small_sigma0)
            .wrapping_add(words[index - 7])
            .wrapping_add(small_sigma1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = *state;
    for index in 0..64 {
        let big_sigma1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let choice = (e & f) ^ ((!e) & g);
        let temp1 = h
            .wrapping_add(big_sigma1)
            .wrapping_add(choice)
            .wrapping_add(K[index])
            .wrapping_add(words[index]);
        let big_sigma0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let majority = (a & b) ^ (a & c) ^ (b & c);
        let temp2 = big_sigma0.wrapping_add(majority);
        h = g;
        g = f;
        f = e;
        e = d.wrapping_add(temp1);
        d = c;
        c = b;
        b = a;
        a = temp1.wrapping_add(temp2);
    }
    for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
        *slot = slot.wrapping_add(value);
    }
}

/// SHA-256 of `data`.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state = INITIAL;
    let (blocks, remainder) = data.as_chunks::<64>();
    for block in blocks {
        compress(&mut state, block);
    }
    let final_len = if remainder.len() < 56 { 64 } else { 128 };
    let mut final_blocks = [0u8; 128];
    final_blocks[..remainder.len()].copy_from_slice(remainder);
    final_blocks[remainder.len()] = 0x80;
    let bit_len = (data.len() as u64).wrapping_mul(8);
    final_blocks[final_len - 8..final_len].copy_from_slice(&bit_len.to_be_bytes());
    for block in final_blocks[..final_len].as_chunks::<64>().0 {
        compress(&mut state, block);
    }
    let mut out = [0u8; 32];
    for (chunk, value) in out.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        chunk.copy_from_slice(&value.to_be_bytes());
    }
    out
}

/// Lower-case hex rendering of a digest.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// HMAC-SHA256 with the RFC 2104 construction (block size 64).
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; 64];
    if key.len() > 64 {
        block[..32].copy_from_slice(&sha256(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner_pad = [0x36u8; 64];
    let mut outer_pad = [0x5cu8; 64];
    for index in 0..64 {
        inner_pad[index] ^= block[index];
        outer_pad[index] ^= block[index];
    }
    let mut inner_input = Vec::with_capacity(64 + message.len());
    inner_input.extend_from_slice(&inner_pad);
    inner_input.extend_from_slice(message);
    let inner = sha256(&inner_input);
    let mut outer_input = [0u8; 96];
    outer_input[..64].copy_from_slice(&outer_pad);
    outer_input[64..].copy_from_slice(&inner);
    sha256(&outer_input)
}

/// CRC-32/ISO-HDLC (zlib polynomial), the variant Python's `zlib.crc32` emits.
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// CRC-32C (Castagnoli), the standard variant: register starts at
/// `0xFFFF_FFFF` and is inverted on the way out. Kept for capture analysis;
/// the anti-cheat module does **not** use it (see [`crc32c_eac`]).
pub fn crc32c(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x82f6_3b78
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// CRC-32C (Castagnoli) with a **zero** initial value and **no** final XOR —
/// the variant the anti-cheat module writes into the 24-byte header.
///
/// OBSERVED, not guessed any more: the raw Castagnoli variant reproduces the
/// header checksum on 451 frames from five sessions (offline analysis,
/// 2026-11-17) and on the control capture used by the live runs — 121/121
/// heartbeat frames and 18/18 short frames — while the standard variant
/// (`crc32c`) reproduces none of them.
/// Check value: `crc32c_eac(b"123456789") == 0x58e3_FA20`.
pub fn crc32c_eac(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0x82f6_3b78
            } else {
                crc >> 1
            };
        }
    }
    crc
}

/// 32-bit FNV-1a, kept only as a third cheap candidate during capture analysis.
pub fn fnv1a32(data: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for &byte in data {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    const VECTORS: &[(&[u8], &str)] = &[
        (
            b"",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
        ),
        (
            b"abc",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ),
        (
            b"hromatite-feature-v1",
            "916bc79f3ad57db78640c3edaf9e8064cb498779331fd112eee23d892ba4414a",
        ),
    ];

    #[test]
    fn sha256_matches_standard_vectors_and_padding_boundaries() {
        for (input, expected) in VECTORS {
            assert_eq!(hex(&sha256(input)), *expected, "input {input:?}");
        }
        let input: Vec<u8> = (0..64).map(|value| (value % 251) as u8).collect();
        assert_eq!(
            hex(&sha256(&input)),
            "fdeab9acf3710362bd2658cdc9a29e8f9c757fcf9811603a8c447cd1d9151108"
        );
    }

    #[test]
    fn hmac_sha256_matches_rfc_4231_case_1_and_case_2() {
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn crc_variants_match_their_check_values() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
        assert_eq!(fnv1a32(b""), 0x811c_9dc5);
    }
}
