//! EAC/EOS heartbeat model for the Hromatite payload — pure `std`, no external
//! crates, no FFI. Everything here is portable so it can be unit-tested on the
//! host (`--target x86_64-unknown-linux-gnu`) and still compiled into the
//! Windows feature image.
//!
//! Layers:
//!
//! * [`codec`] — the 24-byte wire header. The checksum is **measured**, not
//!   guessed: raw CRC-32C (`init=0, xorout=0`), reproduced on every captured
//!   frame.
//! * [`payload`] — the 120-byte *model* heartbeat body, layout ported from the
//!   verified Python reference (`eacheartbeat/heartbeat.py`). The real
//!   steady-state frame is 512 bytes of a report stream (see [`codec`]).
//! * [`session`] — epoch model: one anti-cheat session per VRChat room, with
//!   `dt_ms` measured across epochs so a world load shows up as a `gap`, and a
//!   measured 2007 ms cadence that [`session::EacSession::record_observed`]
//!   re-learns from the wire.
//! * [`eventlog`] — JSONL records with 32 MiB / 2-file rotation.
//! * [`tailer`] — polling tail over the VRChat `output_log` for room markers.
//! * [`hook`] — x64 inline-hook engine (trampoline, ±512 MB allocation,
//!   12-byte `mov rax,imm64; jmp rax` patch), ported from the C shim.
//! * [`eos`] — the twelve `EOS_AntiCheatClient_*` exports hooked with it:
//!   epochs come from `BeginSession`/`EndSession`, every outgoing frame is
//!   captured, and `ProtectMessage` lengths are counted (the 22-byte proof).
//! * [`capture`] — the binary capture/replay format (`u32 len, u32 dt_ms, bytes`).
//! * [`tap`] — incremental reader of that capture, so a feature image can mirror
//!   the real frames even when the resident owns the hooks.
//! * [`forge`] — `EAC_FORGE`: 512-byte heartbeats rebuilt from a live template.
//! * [`report`] — `EAC_REPLAY_REPORT`: replay a whole recorded report instead,
//!   retargeted to the live `SessionId`/`Sequence` with a fresh CRC.
//!
//! Only `hook`/`eos` contain `unsafe`; every block carries a `// SAFETY:`
//! justification and stays inside the two `#[allow(unsafe_code)]` modules.

#![deny(unsafe_code)]

pub mod capture;
pub mod clock;
pub mod codec;
pub mod digest;
#[allow(unsafe_code)]
pub mod eacimage;
#[allow(unsafe_code)]
pub mod eos;
pub mod epoch;
pub mod eventlog;
#[allow(unsafe_code)]
pub mod evp;
pub mod forge;
#[allow(unsafe_code)]
pub mod m3probe;
#[allow(unsafe_code)]
pub mod hook;
pub mod payload;
pub mod report;
pub mod session;
pub mod tailer;
pub mod tap;

pub use capture::{CaptureFile, Frame as CapturedFrame};
pub use codec::{ChecksumKind, CodecError, Frame, Header, WIRE_BODY_LEN, WIRE_HEARTBEAT_LEN};
pub use eos::{Counters as HookCounters, HookEvent, Hooks};
pub use eventlog::{Event, JsonlLog};
pub use hook::Hook;
pub use payload::{HeartbeatPayload, PAYLOAD_LEN};
pub use session::{Beat, EacSession};
pub use tap::CaptureTap;

/// Encode one complete heartbeat frame (24-byte header + 120-byte payload).
///
/// Header `sequence` is the per-epoch frame sequence from [`Beat::seq`]; the
/// payload carries [`Beat::hb_in_epoch`], the timestamp and the nonce.
pub fn encode_beat(
    beat: &Beat,
    flags: u16,
    integrity_digest: [u8; 32],
    module_list_hash: [u8; 32],
    mac_key: &[u8],
    kind: ChecksumKind,
) -> (Vec<u8>, HeartbeatPayload) {
    let body = HeartbeatPayload {
        seq: beat.hb_in_epoch,
        timestamp_ms: beat.timestamp_ms,
        nonce: beat.nonce,
        flags: payload::FLAG_INTEGRITY_OK,
        integrity_digest,
        module_list_hash,
        mac: [0u8; 32],
    };
    let encoded = body.encode(mac_key);
    let header = Header {
        version: codec::VERSION,
        flags,
        session_id: beat.session_id,
        sequence: beat.seq,
        checksum: 0,
    };
    let frame = Frame {
        header,
        payload: &encoded,
    };
    (frame.encode(kind), body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_encoded_beat_is_a_144_byte_frame_that_parses_back() {
        let mut session = EacSession::new(5_000);
        session.begin(1_000, Some(0x1122_3344_5566_7788));
        let beat = session.beat(1_500);
        let (bytes, body) = encode_beat(
            &beat,
            0,
            [3u8; 32],
            [4u8; 32],
            &[0u8; 32],
            ChecksumKind::Crc32c,
        );
        assert_eq!(bytes.len(), codec::FRAME_LEN);
        assert_eq!(bytes.len(), 144);
        let parsed = Frame::parse(&bytes, ChecksumKind::Crc32c).expect("own frame");
        assert_eq!(parsed.header.session_id, beat.session_id);
        assert_eq!(parsed.header.sequence, 1);
        assert_eq!(parsed.payload.len(), PAYLOAD_LEN);
        let decoded = HeartbeatPayload::parse(parsed.payload, Some(&[0u8; 32])).expect("payload");
        assert_eq!(decoded.seq, 1);
        assert_eq!(decoded.timestamp_ms, 1_500);
        assert_eq!(decoded.nonce, beat.nonce);
        assert_eq!(decoded.mac, body.encode(&[0u8; 32])[88..]);
    }
}
