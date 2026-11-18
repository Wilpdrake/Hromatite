//! The 24-byte EAC message header and the (now measured) checksum.
//!
//! Wire layout (packed, little-endian, from the public EAC handshake RE and
//! confirmed byte-for-byte on live captures):
//!
//! ```text
//! offset  size  field
//!      0     2  Version   (observed 2 in every message)
//!      2     2  Flags     (bit0 StartData, bit1 EndData; short frames use 0x0007/0x0013/0x0047)
//!      4     8  SessionId (constant inside one anti-cheat session)
//!     12     8  Sequence  (monotonic frame counter, +1..+6 per frame)
//!     20     4  Checksum  CRC-32C raw (init=0, xorout=0) over the whole frame
//!                 with these four bytes replaced by zeros
//! ```
//!
//! A "heartbeat" is not a distinct message type: the client streams one report
//! and the transport slices it into frames. The steady-state frame is 512 bytes
//! (`24 + 488`) carrying the next 488 bytes of that report, one every ~2 s;
//! short frames (24 bytes and up) are acks, handshakes and frame tails.
//!
//! `PAYLOAD_LEN`/`FRAME_LEN` therefore describe the **model** frame this crate
//! synthesizes (a 120-byte body from [`crate::payload`]); [`WIRE_HEARTBEAT_LEN`]
//! is the real steady-state size used to recognise a heartbeat in a capture.
//! `parse` is heuristic (no magic bytes): it validates the version range and
//! length, exactly like `framing.EacMessage.try_parse`.

use crate::digest::{crc32, crc32c, crc32c_eac, fnv1a32};

/// Fixed header size on the wire (packed, no padding).
pub const HEADER_LEN: usize = 24;
/// Size of the heartbeat payload this crate emits.
pub const PAYLOAD_LEN: usize = 120;
/// Header + payload for one synthesized heartbeat.
pub const FRAME_LEN: usize = HEADER_LEN + PAYLOAD_LEN;
/// Body length of a real steady-state heartbeat frame (512 - 24).
pub const WIRE_BODY_LEN: usize = 488;
/// Total length of a real steady-state heartbeat frame.
pub const WIRE_HEARTBEAT_LEN: usize = HEADER_LEN + WIRE_BODY_LEN;
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
    /// CRC-32C (Castagnoli) with `init=0, xorout=0` — **measured** on real
    /// frames, the default.
    #[default]
    Crc32c,
    /// CRC-32C with the usual `init=xorout=0xFFFF_FFFF`; kept because earlier
    /// captures were analysed with it, it matches no real frame.
    Crc32cStd,
    /// CRC-32/ISO-HDLC, the zlib/Python `zlib.crc32` variant.
    Crc32,
    /// FNV-1a 32-bit, cheap fourth candidate.
    Fnv1a32,
}

impl ChecksumKind {
    /// Compute the checksum over `header[0..20] || payload`.
    pub fn compute(self, region: &[u8]) -> u32 {
        match self {
            Self::Crc32c => crc32c_eac(region),
            Self::Crc32cStd => crc32c(region),
            Self::Crc32 => crc32(region),
            Self::Fnv1a32 => fnv1a32(region),
        }
    }

    /// Compute the header checksum the way the module does: over the **whole
    /// frame** with the four checksum bytes replaced by zeros.
    ///
    /// The zero-filled field is part of the CRC input — leaving it out (the
    /// obvious-looking `header[0..20] || body`) reproduces no real frame.
    pub fn compute_frame(self, frame: &[u8]) -> u32 {
        let mut region = frame.to_vec();
        if region.len() >= HEADER_LEN {
            region[CHECKSUM_OFFSET..HEADER_LEN].fill(0);
        }
        self.compute(&region)
    }

    /// Stable name for logs.
    pub fn name(self) -> &'static str {
        match self {
            Self::Crc32c => "crc32c",
            Self::Crc32cStd => "crc32c-std",
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
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.extend_from_slice(&prefix);
        out.extend_from_slice(&[0u8; 4]);
        out.extend_from_slice(self.payload);
        let checksum = kind.compute_frame(&out);
        out[CHECKSUM_OFFSET..HEADER_LEN].copy_from_slice(&checksum.to_le_bytes());
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
            checksum: u32::from_le_bytes(
                buf[CHECKSUM_OFFSET..HEADER_LEN]
                    .try_into()
                    .unwrap_or_default(),
            ),
        };
        let payload = &buf[HEADER_LEN..];
        let want = kind.compute_frame(buf);
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
        assert_eq!(
            Frame::parse(&[0u8; 23], ChecksumKind::Crc32c),
            Err(CodecError::TooShort(23))
        );
        let mut bytes = [0u8; 24];
        bytes[0..2].copy_from_slice(&9u16.to_le_bytes());
        assert_eq!(
            Frame::parse(&bytes, ChecksumKind::Crc32c),
            Err(CodecError::BadVersion(9))
        );
    }

    #[test]
    fn the_checksum_covers_the_zeroed_field_itself() {
        let payload = [0x5au8; 16];
        let frame = Frame {
            header: Header {
                version: VERSION,
                flags: 0,
                session_id: 7,
                sequence: 8,
                checksum: 0,
            },
            payload: &payload,
        };
        let encoded = frame.encode(ChecksumKind::Crc32c);
        let zeroed = ChecksumKind::Crc32c.compute_frame(&encoded);
        // Feeding the same bytes is idempotent: parse() recomputes over zeros.
        assert_eq!(
            Frame::parse(&encoded, ChecksumKind::Crc32c)
                .expect("self-encoded")
                .header
                .checksum,
            zeroed
        );
        // Skipping the four zero bytes changes the value, which is exactly the
        // mistake the first implementation made.
        let mut skipped = Vec::new();
        skipped.extend_from_slice(&encoded[..CHECKSUM_OFFSET]);
        skipped.extend_from_slice(&encoded[HEADER_LEN..]);
        assert_ne!(ChecksumKind::Crc32c.compute(&skipped), zeroed);
        // Any change inside the field itself must not change the checksum.
        let mut rewritten = encoded.clone();
        rewritten[CHECKSUM_OFFSET..HEADER_LEN].copy_from_slice(&0xdead_beefu32.to_le_bytes());
        assert_eq!(ChecksumKind::Crc32c.compute_frame(&rewritten), zeroed);
    }

    #[test]
    fn raw_crc32c_is_the_wire_variant_and_the_standard_one_is_not() {
        assert_eq!(crate::digest::crc32c_eac(b"123456789"), 0x58e3_fa20);
        assert_eq!(crate::digest::crc32c(b"123456789"), 0xe306_9283);
        assert_ne!(
            ChecksumKind::Crc32c.compute(b"123456789"),
            ChecksumKind::Crc32cStd.compute(b"123456789")
        );
        assert_eq!(ChecksumKind::Crc32c.name(), "crc32c");
        assert_eq!(ChecksumKind::Crc32cStd.name(), "crc32c-std");
    }

    #[test]
    fn wire_constants_describe_the_measured_heartbeat() {
        assert_eq!(WIRE_BODY_LEN, 488);
        assert_eq!(WIRE_HEARTBEAT_LEN, 512);
        assert_eq!(WIRE_HEARTBEAT_LEN, HEADER_LEN + WIRE_BODY_LEN);
        assert_ne!(FRAME_LEN, WIRE_HEARTBEAT_LEN, "model frame is not the wire frame");
    }

    /// Reproduces the header checksum on real captured frames. Runs only when a
    /// capture is supplied, so no protocol bytes have to live in the repository:
    ///
    /// ```sh
    /// HROMATITE_EAC_CAPTURE=~/EAC-RE/evidence/control_0009/capture.bin \
    ///     cargo test -p hromatite-eac --target x86_64-unknown-linux-gnu crc
    /// ```
    #[test]
    fn crc_reproduces_every_frame_of_a_real_capture() {
        let Ok(path) = std::env::var("HROMATITE_EAC_CAPTURE") else {
            return;
        };
        let frames = crate::capture::read_frames(&path).expect("capture");
        let mut shown = 0usize;
        let mut ok = 0usize;
        let mut bad = 0usize;
        for frame in &frames {
            if frame.data.len() < HEADER_LEN {
                continue;
            }
            match Frame::parse(&frame.data, ChecksumKind::Crc32c) {
                Ok(parsed) => {
                    ok += 1;
                    // The first heartbeat of a session starts a message.
                    if frame.data.len() >= WIRE_HEARTBEAT_LEN && shown < 3 {
                        shown += 1;
                        assert_eq!(parsed.header.version, VERSION);
                        assert!(parsed.header.session_id != 0);
                    }
                }
                Err(CodecError::ChecksumMismatch { got, want }) => {
                    bad += 1;
                    if bad < 4 {
                        eprintln!(
                            "checksum mismatch on {} bytes: got {got:#010x} want {want:#010x}",
                            frame.data.len()
                        );
                    }
                }
                Err(_) => {}
            }
        }
        assert!(frames.len() > 10, "capture {path} is empty");
        assert_eq!(bad, 0, "{bad} frame(s) failed the raw CRC-32C check");
        assert!(ok > 10, "only {ok} frames verified");
        eprintln!("crc32c_eac verified {ok}/{} frames from {path}", frames.len());
    }
}
