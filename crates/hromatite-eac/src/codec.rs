//! The 24-byte EAC message header and the `HYPOTHESIS` checksum choice.
//!
//! Wire layout (packed, little-endian, from the public EAC handshake RE):
//!
//! ```text
//! offset  size  field
//!      0     2  Version   (observed 2)
//!      2     2  Flags
//!      4     8  SessionId
//!     12     8  Sequence
//!     20     4  Checksum  (over header[0..20] || payload)
//! ```
//!
//! **HYPOTHESIS.** Version/Flags/SessionId/Sequence and the 24-byte size are
//! verified from published RE + the live Python model; the checksum *algorithm*
//! is not (it is computed inside a bytecode VM). `ChecksumKind::Crc32c` is the
//! default guess, and the field is pluggable so a capture can falsify it without
//! touching the session model. There are no magic bytes in this format, so
//! `parse` is heuristic: it validates version range and length, exactly like
//! `framing.EacMessage.try_parse`.

use crate::digest::{crc32, crc32c, fnv1a32};

/// Fixed header size on the wire (packed, no padding).
pub const HEADER_LEN: usize = 24;
/// Size of the heartbeat payload this crate emits.
pub const PAYLOAD_LEN: usize = 120;
/// Header + payload for one heartbeat.
pub const FRAME_LEN: usize = HEADER_LEN + PAYLOAD_LEN;
/// Version observed in every published message.
pub const VERSION: u16 = 2;
/// Offset of the checksum inside the header.
pub const CHECKSUM_OFFSET: usize = 20;
/// Highest plausible version; anything else is not an EAC message.
const MAX_VERSION: u16 = 4;

/// Candidate checksum algorithms for the header field.
///
/// The enum is on the *model* side only; nothing on the wire selects it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChecksumKind {
    /// CRC-32C (Castagnoli). Default, unverified guess.
    #[default]
    Crc32c,
    /// CRC-32/ISO-HDLC, the zlib/Python `zlib.crc32` variant.
    Crc32,
    /// FNV-1a 32-bit, cheap third candidate.
    Fnv1a32,
}

impl ChecksumKind {
    /// Compute the checksum over `header[0..20] || payload`.
    pub fn compute(self, region: &[u8]) -> u32 {
        match self {
            Self::Crc32c => crc32c(region),
            Self::Crc32 => crc32(region),
            Self::Fnv1a32 => fnv1a32(region),
        }
    }

    /// Stable name for logs.
    pub fn name(self) -> &'static str {
        match self {
            Self::Crc32c => "crc32c",
            Self::Crc32 => "crc32",
            Self::Fnv1a32 => "fnv1a32",
        }
    }
}

/// Decoded header fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Header {
    pub version: u16,
    pub flags: u16,
    pub session_id: u64,
    pub sequence: u64,
    pub checksum: u32,
}

impl Header {
    /// Serialize the 20 bytes that precede the checksum.
    fn prefix(&self) -> [u8; CHECKSUM_OFFSET] {
        let mut out = [0u8; CHECKSUM_OFFSET];
        out[0..2].copy_from_slice(&self.version.to_le_bytes());
        out[2..4].copy_from_slice(&self.flags.to_le_bytes());
        out[4..12].copy_from_slice(&self.session_id.to_le_bytes());
        out[12..20].copy_from_slice(&self.sequence.to_le_bytes());
        out
    }
}

/// One framed EAC message: header fields plus a borrowed payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub header: Header,
    pub payload: &'a [u8],
}

/// Why a buffer could not be read as an EAC message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CodecError {
    /// Shorter than the 24-byte header.
    TooShort(usize),
    /// `Version` outside 1..=4.
    BadVersion(u16),
    /// Header checksum does not reproduce.
    ChecksumMismatch { got: u32, want: u32 },
}

impl Frame<'_> {
    /// Total wire size.
    pub fn len(&self) -> usize {
        HEADER_LEN + self.payload.len()
    }

    /// True when the frame carries no payload.
    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }

    /// Encode header + payload, computing the checksum with `kind`.
    pub fn encode(&self, kind: ChecksumKind) -> Vec<u8> {
        let prefix = self.header.prefix();
        let mut region = Vec::with_capacity(prefix.len() + self.payload.len());
        region.extend_from_slice(&prefix);
        region.extend_from_slice(self.payload);
        let checksum = kind.compute(&region);
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.extend_from_slice(&prefix);
        out.extend_from_slice(&checksum.to_le_bytes());
        out.extend_from_slice(self.payload);
        out
    }

    /// Parse `buf` as one complete message and verify its checksum.
    pub fn parse(buf: &[u8], kind: ChecksumKind) -> Result<Frame<'_>, CodecError> {
        if buf.len() < HEADER_LEN {
            return Err(CodecError::TooShort(buf.len()));
        }
        let version = u16::from_le_bytes([buf[0], buf[1]]);
        if !(1..=MAX_VERSION).contains(&version) {
            return Err(CodecError::BadVersion(version));
        }
        let header = Header {
            version,
            flags: u16::from_le_bytes([buf[2], buf[3]]),
            session_id: u64::from_le_bytes(buf[4..12].try_into().unwrap_or_default()),
            sequence: u64::from_le_bytes(buf[12..20].try_into().unwrap_or_default()),
            checksum: u32::from_le_bytes(buf[CHECKSUM_OFFSET..HEADER_LEN].try_into().unwrap_or_default()),
        };
        let payload = &buf[HEADER_LEN..];
        let mut region = Vec::with_capacity(CHECKSUM_OFFSET + payload.len());
        region.extend_from_slice(&buf[..CHECKSUM_OFFSET]);
        region.extend_from_slice(payload);
        let want = kind.compute(&region);
        if want != header.checksum {
            return Err(CodecError::ChecksumMismatch {
                got: header.checksum,
                want,
            });
        }
        Ok(Frame { header, payload })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_exactly_twenty_four_bytes_on_the_wire() {
        let frame = Frame {
            header: Header {
                version: VERSION,
                flags: 0,
                session_id: 0x1122_3344_5566_7788,
                sequence: 9,
                checksum: 0,
            },
            payload: &[0u8; PAYLOAD_LEN],
        };
        assert_eq!(frame.len(), FRAME_LEN);
        assert_eq!(frame.encode(ChecksumKind::Crc32c).len(), 24 + 120);
    }

    #[test]
    fn round_trip_preserves_fields_and_rejects_a_corrupted_byte() {
        let payload = [7u8; PAYLOAD_LEN];
        let frame = Frame {
            header: Header {
                version: VERSION,
                flags: 0x0004,
                session_id: 0xabcd_ef01_2345_6789,
                sequence: 42,
                checksum: 0,
            },
            payload: &payload,
        };
        let bytes = frame.encode(ChecksumKind::Crc32c);
        let parsed = Frame::parse(&bytes, ChecksumKind::Crc32c).expect("self-encoded frame");
        assert_eq!(parsed.header.session_id, frame.header.session_id);
        assert_eq!(parsed.header.sequence, 42);
        assert_eq!(parsed.header.flags, 0x0004);
        assert_eq!(parsed.payload, &payload);

        let mut broken = bytes.clone();
        broken[40] ^= 0xff;
        assert!(matches!(
            Frame::parse(&broken, ChecksumKind::Crc32c),
            Err(CodecError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn short_and_implausible_buffers_are_rejected_without_panicking() {
        assert_eq!(Frame::parse(&[0u8; 23], ChecksumKind::Crc32c), Err(CodecError::TooShort(23)));
        let mut bytes = [0u8; 24];
        bytes[0..2].copy_from_slice(&9u16.to_le_bytes());
        assert_eq!(
            Frame::parse(&bytes, ChecksumKind::Crc32c),
            Err(CodecError::BadVersion(9))
        );
    }
}
