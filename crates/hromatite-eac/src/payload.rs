//! The 120-byte heartbeat payload.
//!
//! Layout ported from `eacheartbeat/heartbeat.py` (verified reference, 98/98
//! self-test), which is itself a **HYPOTHESIS** about the real blob:
//!
//! ```text
//! offset  size  field
//!      0     4  seq           u32 LE
//!      4     8  timestamp_ms  u64 LE
//!     12     8  nonce         u64 LE
//!     20     4  flags         u32 LE
//!     24    32  integrity     sha256 over the catalog of signed files
//!     56    32  modules       sha256 over the sorted loaded-module list
//!     88    32  mac           HMAC-SHA256(key, bytes[0..88])
//! ```
//!
//! The digest builders below reproduce the Python canonicalisation byte for
//! byte: sorted entries, `u32` little-endian length prefix, `/`-normalised
//! paths, lower-cased module basenames.

use crate::digest::{hmac_sha256, sha256};

/// Payload size on the wire.
pub const PAYLOAD_LEN: usize = 120;
/// Bytes covered by the trailing MAC.
pub const SIGNED_LEN: usize = 88;

/// `integrity_ok` — the only flag the model sets by default.
pub const FLAG_INTEGRITY_OK: u32 = 0x0001;
/// `integrity_fail`.
pub const FLAG_INTEGRITY_FAIL: u32 = 0x0002;
/// A module report is attached.
pub const FLAG_HAS_MODULE_REPORT: u32 = 0x0004;
/// Answers a server challenge nonce.
pub const FLAG_CHALLENGE_RESPONSE: u32 = 0x0008;

/// One heartbeat body, MAC included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeartbeatPayload {
    pub seq: u32,
    pub timestamp_ms: u64,
    pub nonce: u64,
    pub flags: u32,
    pub integrity_digest: [u8; 32],
    pub module_list_hash: [u8; 32],
    pub mac: [u8; 32],
}

/// Why a payload could not be decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadError {
    /// Not exactly 120 bytes.
    Length(usize),
    /// HMAC did not verify.
    MacMismatch,
}

impl HeartbeatPayload {
    /// The 88 bytes the MAC covers.
    pub fn signing_bytes(&self) -> [u8; SIGNED_LEN] {
        let mut out = [0u8; SIGNED_LEN];
        out[0..4].copy_from_slice(&self.seq.to_le_bytes());
        out[4..12].copy_from_slice(&self.timestamp_ms.to_le_bytes());
        out[12..20].copy_from_slice(&self.nonce.to_le_bytes());
        out[20..24].copy_from_slice(&self.flags.to_le_bytes());
        out[24..56].copy_from_slice(&self.integrity_digest);
        out[56..88].copy_from_slice(&self.module_list_hash);
        out
    }

    /// Serialize, computing the trailing MAC over the first 88 bytes.
    pub fn encode(&self, mac_key: &[u8]) -> [u8; PAYLOAD_LEN] {
        let signed = self.signing_bytes();
        let mac = hmac_sha256(mac_key, &signed);
        let mut out = [0u8; PAYLOAD_LEN];
        out[..SIGNED_LEN].copy_from_slice(&signed);
        out[SIGNED_LEN..].copy_from_slice(&mac);
        out
    }

    /// Parse and (optionally) verify a 120-byte payload.
    pub fn parse(buf: &[u8], mac_key: Option<&[u8]>) -> Result<Self, PayloadError> {
        let bytes: &[u8; PAYLOAD_LEN] = buf
            .try_into()
            .map_err(|_| PayloadError::Length(buf.len()))?;
        let mut integrity_digest = [0u8; 32];
        integrity_digest.copy_from_slice(&bytes[24..56]);
        let mut module_list_hash = [0u8; 32];
        module_list_hash.copy_from_slice(&bytes[56..88]);
        let mut mac = [0u8; 32];
        mac.copy_from_slice(&bytes[88..]);
        let payload = Self {
            seq: u32::from_le_bytes(bytes[0..4].try_into().unwrap_or_default()),
            timestamp_ms: u64::from_le_bytes(bytes[4..12].try_into().unwrap_or_default()),
            nonce: u64::from_le_bytes(bytes[12..20].try_into().unwrap_or_default()),
            flags: u32::from_le_bytes(bytes[20..24].try_into().unwrap_or_default()),
            integrity_digest,
            module_list_hash,
            mac,
        };
        if let Some(key) = mac_key
            && hmac_sha256(key, &payload.signing_bytes()) != mac
        {
            return Err(PayloadError::MacMismatch);
        }
        Ok(payload)
    }
}

/// SHA-256 over the canonical catalog of signed files.
///
/// `files` is `(relative path, already-computed sha256)`. Entries are sorted by
/// path; each line is `path \\0 digest` preceded by a LE `u32` length.
pub fn integrity_digest<'a>(files: impl IntoIterator<Item = (&'a str, [u8; 32])>) -> [u8; 32] {
    let mut lines: Vec<(String, [u8; 32])> = files
        .into_iter()
        .map(|(path, digest)| (path.replace('\\', "/"), digest))
        .collect();
    lines.sort_by(|left, right| left.0.cmp(&right.0));
    let mut catalog = Vec::new();
    for (path, digest) in lines {
        let mut line = Vec::with_capacity(path.len() + 1 + 32);
        line.extend_from_slice(path.as_bytes());
        line.push(0);
        line.extend_from_slice(&digest);
        catalog.extend_from_slice(&(line.len() as u32).to_le_bytes());
        catalog.extend_from_slice(&line);
    }
    sha256(&catalog)
}

/// SHA-256 over the sorted loaded-module list (`name` + optional `size`).
pub fn module_list_hash<'a>(modules: impl IntoIterator<Item = (&'a str, Option<u64>)>) -> [u8; 32] {
    let mut entries: Vec<Vec<u8>> = modules
        .into_iter()
        .map(|(name, size)| {
            let base = name
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(name)
                .to_ascii_lowercase();
            let mut record = base.into_bytes();
            if let Some(size) = size {
                record.push(0);
                record.extend_from_slice(&size.to_le_bytes());
            }
            record
        })
        .collect();
    entries.sort();
    let mut catalog = Vec::new();
    for record in entries {
        catalog.extend_from_slice(&(record.len() as u32).to_le_bytes());
        catalog.extend_from_slice(&record);
    }
    sha256(&catalog)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::hex;

    fn sample() -> HeartbeatPayload {
        let mut integrity_digest = [0u8; 32];
        integrity_digest.copy_from_slice(&(0u8..32).collect::<Vec<_>>());
        let mut module_list_hash = [0u8; 32];
        module_list_hash.copy_from_slice(&(32u8..64).collect::<Vec<_>>());
        HeartbeatPayload {
            seq: 7,
            timestamp_ms: 1_700_000_000_000,
            nonce: 0x0123_4567_89ab_cdef,
            flags: FLAG_INTEGRITY_OK,
            integrity_digest,
            module_list_hash,
            mac: [0u8; 32],
        }
    }

    /// Vector generated by the Python reference:
    /// `Heartbeat(seq=7, ts=1700000000000, nonce=0x0123456789abcdef,
    /// integrity=bytes(range(32)), modules=bytes(range(32,64)), flags=1)`
    /// with a 32-byte zero MAC key.
    const REFERENCE_PAYLOAD: &str = "070000000068e5cf8b010000efcdab896745230101000000000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3fed5e8e529338a54bb3120d49732ec25164d3bc765b0fb767cf78a46c5b431644";

    #[test]
    fn payload_matches_the_python_reference_vector_exactly() {
        let encoded = sample().encode(&[0u8; 32]);
        assert_eq!(hex(&encoded), REFERENCE_PAYLOAD);
    }

    #[test]
    fn payload_round_trips_and_mac_verification_detects_tampering() {
        let key = [0u8; 32];
        let encoded = sample().encode(&key);
        let parsed = HeartbeatPayload::parse(&encoded, Some(&key)).expect("valid payload");
        assert_eq!(parsed.seq, 7);
        assert_eq!(parsed.timestamp_ms, 1_700_000_000_000);
        assert_eq!(parsed.nonce, 0x0123_4567_89ab_cdef);
        assert_eq!(parsed.flags, FLAG_INTEGRITY_OK);
        assert_eq!(parsed.integrity_digest, sample().integrity_digest);

        let mut broken = encoded;
        broken[10] ^= 1;
        assert_eq!(
            HeartbeatPayload::parse(&broken, Some(&key)),
            Err(PayloadError::MacMismatch)
        );
        assert_eq!(
            HeartbeatPayload::parse(&encoded[..119], None),
            Err(PayloadError::Length(119))
        );
    }

    #[test]
    fn digest_builders_are_order_independent_and_path_normalised() {
        let a = [1u8; 32];
        let b = [2u8; 32];
        assert_eq!(
            integrity_digest([("b.dll", b), ("a.dll", a)]),
            integrity_digest([("a.dll", a), ("b.dll", b)])
        );
        assert_ne!(
            integrity_digest([("a.dll", a)]),
            integrity_digest([("a.dll", b)])
        );
        assert_eq!(
            module_list_hash([("C:\\Games\\VRChat.exe", Some(7))]),
            module_list_hash([("VRChat.exe", Some(7))])
        );
        assert_eq!(
            module_list_hash([("B.dll", None), ("a.dll", None)]),
            module_list_hash([("a.dll", None), ("B.dll", None)])
        );
    }
}
