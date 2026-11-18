//! L2 "cipher at rest": the one cipher shared by the injector and the payload.
//!
//! The injector XORs the mapped image page by page (`manualmap::seal_image`); the
//! payload's trampoline page XORs the *same* bytes back for the duration of a
//! tick and encrypts them again on the way out (`hook::seal_shim`). Both sides
//! therefore have to agree bit for bit, and the payload side is hand-written
//! machine code, so the cipher is deliberately primitive:
//!
//! * per page the key is `SEAL_KEY ^ page * 0x9E37_79B9_7F4A_7C15` — neighbouring
//!   pages never share a keystream, and the page index is just a counter;
//! * the page's keystream is one 8-byte block that **repeats** over the whole
//!   page. That block is built from the page key with the classic xorshift
//!   `k ^= k << 13; k ^= k >> 7; k ^= k << 17`, taking the low byte of eight
//!   successive states (`b[i] = step^i(page_key) & 0xff`).
//!
//! Why the low bytes of eight states instead of one stepped 64-bit word per
//! 8-byte block: the live seal marker the project has been reading out of the
//! game (`c6 2f ed 2c 76 06 ec 7b` for page 0) is exactly that byte stream, and
//! criterion (a) of plan 4 checks it. Stepping once per block would change the
//! marker. Building the block costs eight xorshift rounds per page — in the
//! payload that is a short loop over one register, and the payload's inner loop
//! is then a plain `xor [ptr], key; add ptr, 8`, the cheapest thing that can be
//! written in bare asm.
//!
//! `xor_page` is an involution (XOR), so decrypt == encrypt: no separate code
//! path, no state to keep, and `apply twice` is the unit test.

#![forbid(unsafe_code)]

/// Fixed ASCII key, expanded per page so neighbouring pages do not repeat.
pub const SEAL_KEY: u64 = 0x4852_4F4D_4154_4954; // "HROMATIT"

/// Odd 64-bit constant: page * GOLDEN spreads pages over the key space.
pub const GOLDEN: u64 = 0x9E37_79B9_7F4A_7C15;

/// Page granularity of the seal (matches the OS page size on x64).
pub const PAGE: usize = 4096;

/// One xorshift64 round, as the payload's asm performs it.
#[inline]
pub const fn step(mut k: u64) -> u64 {
    k ^= k << 13;
    k ^= k >> 7;
    k ^= k << 17;
    k
}

/// Per-page seed. The payload computes this with `imul` + `xor` (no lookup
/// tables, no constants in memory).
#[inline]
pub const fn page_key(page: u64) -> u64 {
    SEAL_KEY ^ page.wrapping_mul(GOLDEN)
}

/// The 8 bytes that repeat over one page (little-endian word, so
/// `to_le_bytes()` is the byte order seen on disk).
#[inline]
pub fn page_block(page: u64) -> u64 {
    let mut k = page_key(page);
    let mut bytes = [0u8; 8];
    for b in bytes.iter_mut() {
        k = step(k);
        *b = (k & 0xFF) as u8;
    }
    u64::from_le_bytes(bytes)
}

/// XOR one page (`buf` shorter than a page is fine: it is the tail of the
/// image). `page` is the page index inside the image, the same number the
/// injector used.
pub fn xor_page(page: u64, buf: &mut [u8]) {
    let kb = page_block(page).to_le_bytes();
    for (i, b) in buf.iter_mut().enumerate() {
        *b ^= kb[i & 7];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Criterion of the whole scheme: decrypt is the same operation.
    #[test]
    fn xor_page_is_an_involution() {
        let mut buf: Vec<u8> = (0..PAGE).map(|i| (i * 7 + 3) as u8).collect();
        let orig = buf.clone();
        xor_page(0, &mut buf);
        assert_ne!(buf, orig, "page 0 was not encrypted at all");
        xor_page(0, &mut buf);
        assert_eq!(buf, orig, "second pass did not restore the page");
    }

    /// The payload's asm must reproduce this exact stream: page 0 starts with
    /// the seal marker the live log has been showing (`c6 2f ed 2c 76 06 ec 7b`
    /// when the page begins with `MZ`, i.e. exactly the keystream).
    #[test]
    fn page0_matches_the_live_marker() {
        let zeros = [0u8; 8];
        let mut b = zeros;
        xor_page(0, &mut b);
        assert_eq!(
            b,
            [0xc6, 0x2f, 0xed, 0x2c, 0x76, 0x06, 0xec, 0x7b],
            "page 0 keystream changed — the live seal marker no longer matches"
        );
        // a real PE starts with MZ (4d 5a): the ciphertext must not look like one
        let mut mz = *b"MZ\x90\x00\x03\x00\x00\x00";
        xor_page(0, &mut mz);
        assert_ne!(&mz[..2], b"MZ");
    }

    #[test]
    fn pages_have_different_keystreams() {
        let mut a = [0u8; 64];
        let mut b = [0u8; 64];
        xor_page(0, &mut a);
        xor_page(1, &mut b);
        assert_ne!(a, b, "page 1 repeats the keystream of page 0");
        let mut c = [0u8; 64];
        xor_page(0x1000, &mut c);
        assert_ne!(a, c);
    }

    #[test]
    fn key_follows_the_documented_formula() {
        assert_eq!(page_key(0), SEAL_KEY);
        assert_eq!(page_key(1), SEAL_KEY ^ GOLDEN);
        assert_eq!(page_key(3), SEAL_KEY ^ (3u64).wrapping_mul(GOLDEN));
    }

    /// The block is 8 bytes and repeats across the page (that is what lets the
    /// payload use one `xor` per 8 bytes).
    #[test]
    fn block_repeats_over_the_page() {
        let mut buf = [0u8; 64];
        xor_page(7, &mut buf);
        for i in 0..8 {
            for j in 0..8 {
                assert_eq!(
                    buf[i * 8 + j],
                    buf[j],
                    "byte {i}*8+{j} broke the 8-byte period"
                );
            }
        }
    }
}
