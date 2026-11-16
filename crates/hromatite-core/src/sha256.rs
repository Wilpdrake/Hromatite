//! SHA-256 for immutable feature artifacts; no external runtime dependency.
#![forbid(unsafe_code)]

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

pub fn digest(data: &[u8]) -> [u8; 32] {
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

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn hex_digest(data: &[u8]) -> String {
    hex(&digest(data))
}

fn finish(mut state: [u32; 8], remainder: &[u8], bit_len: u64) -> [u8; 32] {
    let final_len = if remainder.len() < 56 { 64 } else { 128 };
    let mut final_blocks = [0u8; 128];
    final_blocks[..remainder.len()].copy_from_slice(remainder);
    final_blocks[remainder.len()] = 0x80;
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

/// Hash a potentially large stream with bounded memory. Only one 64 KiB read
/// buffer plus at most one partial digest block is ever live at once.
pub fn digest_reader<R: std::io::Read>(mut reader: R) -> std::io::Result<[u8; 32]> {
    let mut state = INITIAL;
    let mut buffer = [0u8; 64 * 1024];
    let mut trailing = Vec::new();
    let mut total_len = 0u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        total_len = total_len.checked_add(read as u64).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "input exceeds SHA-256 length",
            )
        })?;
        trailing.extend_from_slice(&buffer[..read]);
        let (blocks, remainder) = trailing.as_chunks::<64>();
        for block in blocks {
            compress(&mut state, block);
        }
        trailing = remainder.to_vec();
    }
    Ok(finish(state, &trailing, total_len.wrapping_mul(8)))
}

pub fn hex_digest_file(path: impl AsRef<std::path::Path>) -> std::io::Result<String> {
    let file = std::fs::File::open(path)?;
    digest_reader(file).map(|digest| hex(&digest))
}

#[cfg(test)]
mod tests {
    #[test]
    fn standard_vectors_and_streamed_input_match_the_system_hasher() {
        let vectors: &[(&[u8], &str)] = &[
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
        for (input, expected) in vectors {
            assert_eq!(super::hex_digest(input), *expected);
        }
    }

    #[test]
    fn padding_boundaries_match_independently_computed_vectors() {
        let expected = [
            (
                55,
                "463eb28e72f82e0a96c0a4cc53690c571281131f672aa229e0d45ae59b598b59",
            ),
            (
                56,
                "da2ae4d6b36748f2a318f23e7ab1dfdf45acdc9d049bd80e59de82a60895f562",
            ),
            (
                63,
                "29af2686fd53374a36b0846694cc342177e428d1647515f078784d69cdb9e488",
            ),
            (
                64,
                "fdeab9acf3710362bd2658cdc9a29e8f9c757fcf9811603a8c447cd1d9151108",
            ),
            (
                65,
                "4bfd2c8b6f1eec7a2afeb48b934ee4b2694182027e6d0fc075074f2fabb31781",
            ),
        ];
        for (length, expected) in expected {
            let input: Vec<u8> = (0..length).map(|value| (value % 251) as u8).collect();
            assert_eq!(super::hex_digest(&input), expected, "length {length}");
        }
    }

    #[test]
    fn streamed_and_file_hashes_match_the_whole_input_digest() {
        let data: Vec<u8> = (0..=255).cycle().take(64 * 1024 + 37).collect();
        let expected = super::hex_digest(&data);
        assert_eq!(
            super::digest_reader(&data[..])
                .map(|digest| super::hex(&digest))
                .unwrap(),
            expected.clone()
        );

        let path = std::env::temp_dir().join(format!("hromatite-sha-{}.bin", std::process::id()));
        std::fs::write(&path, &data).unwrap();
        assert_eq!(super::hex_digest_file(&path).unwrap(), expected);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn one_byte_change_is_a_different_digest() {
        assert_ne!(
            super::digest(b"generation-1"),
            super::digest(b"generation-2")
        );
    }
}
