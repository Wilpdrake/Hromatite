//! Host-level invariants of the captured EAC heartbeat frames: the CRC-32C
//! checksum, `retarget_frame` (field offsets, reseal, idempotence) and the
//! capture-record parser.
//!
//! The tests run against the **real** captures in `~/EAC-RE/evidence` (format
//! `u32 len | u32 dt_ms | bytes`, see `frame_analyze.py`), so they are only as
//! good as the files that happen to be on the box:
//!
//! * `HROMATITE_EAC_EVIDENCE` overrides the evidence root;
//! * otherwise `$HOME/EAC-RE/evidence` is used;
//! * when neither exists (CI) every test prints `skip:` and returns — a missing
//!   artifact must not turn the suite red.
//!
//! Only the public API is used: `hromatite_abi::crc::{crc32c_eac,
//! seal_checksum}` and `hromatite_il2cpp::photon::{parse_payload_records,
//! retarget_frame, EAC_CHECKSUM_OFF, EAC_FRAME_LEN}`. Nothing in `src/` is
//! touched, so the offsets `SessionId = 4` and `Sequence = 12` (private consts
//! in `raw::photon`) are restated here from the verified wire layout
//! (`findings/frame_structure.md` §2).

#![forbid(unsafe_code)]

use std::path::PathBuf;

use hromatite_abi::crc::{crc32c_eac, seal_checksum};
use hromatite_il2cpp::photon::{EAC_CHECKSUM_OFF, EAC_FRAME_LEN, parse_payload_records, retarget_frame};

/// Wire header: `u16 Version, u16 Flags, u64 SessionId, u64 Sequence, u32 CRC`.
const HEADER_LEN: usize = 24;
/// Zero-based offset of `SessionId` (private in `raw::photon`).
const SESSION_OFF: usize = 4;
/// Zero-based offset of `Sequence` (private in `raw::photon`).
const SEQUENCE_OFF: usize = 12;
/// Reference vector of CRC-32C/EAC, cross-checked with `frame_analyze.py`.
const REFERENCE: &[u8] = b"123456789";
const REFERENCE_CK: u32 = 0x58e3_fa20;

/// Artifacts worth checking, in order of preference. The main capture holds
/// 170 records (121 of them 512-byte frames, one session) and 100 % of its
/// frames verify; the report extraction is 117 frames of one message.
const CRC_ARTIFACTS: [&str; 3] = [
    "control_0009/capture.bin",
    "keyhunt_0105/live_capture.bin",
    "report_full/report_a_sid158b.bin",
];

fn evidence_root() -> PathBuf {
    std::env::var_os("HROMATITE_EAC_EVIDENCE")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join("EAC-RE/evidence")))
        .unwrap_or_else(|| PathBuf::from("EAC-RE/evidence"))
}

/// Read one artifact, or print `skip:` and return `None`.
fn artifact(rel: &str) -> Option<Vec<u8>> {
    let path = evidence_root().join(rel);
    match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(err) => {
            println!("skip: {} ({err})", path.display());
            None
        }
    }
}

/// Every 512-byte frame of a capture, in file order.
fn frames_512(bytes: &[u8]) -> Vec<Vec<u8>> {
    parse_payload_records(bytes)
        .unwrap_or_default()
        .into_iter()
        .filter(|payload| payload.len() == EAC_FRAME_LEN)
        .collect()
}

fn stored_checksum(frame: &[u8]) -> u32 {
    u32::from_le_bytes(
        frame[EAC_CHECKSUM_OFF..EAC_CHECKSUM_OFF + 4]
            .try_into()
            .expect("4-byte checksum field"),
    )
}

fn session_of(frame: &[u8]) -> u64 {
    u64::from_le_bytes(frame[SESSION_OFF..SESSION_OFF + 8].try_into().expect("8 bytes"))
}

fn sequence_of(frame: &[u8]) -> u64 {
    u64::from_le_bytes(frame[SEQUENCE_OFF..SEQUENCE_OFF + 8].try_into().expect("8 bytes"))
}

/// The checksum of a frame is taken over the frame with its own field zeroed.
fn verify(frame: &[u8]) -> bool {
    let mut zeroed = frame.to_vec();
    zeroed[EAC_CHECKSUM_OFF..EAC_CHECKSUM_OFF + 4].fill(0);
    stored_checksum(frame) == crc32c_eac(&zeroed)
}

#[test]
fn crc_reference_vector_is_crc32c_with_init_zero() {
    assert_eq!(crc32c_eac(REFERENCE), REFERENCE_CK);
    assert_eq!(crc32c_eac(b""), 0);
    // Not the standard CRC-32C (init/xorout = 0xFFFFFFFF): EAC uses 0/0.
    assert_ne!(crc32c_eac(REFERENCE), 0xE306_9283);
    let mut buf = [0u8; 8];
    assert!(seal_checksum(&mut buf, 4));
    assert_eq!(
        u32::from_le_bytes(buf[4..8].try_into().expect("4 bytes")),
        crc32c_eac(&[0, 0, 0, 0, 0, 0, 0, 0])
    );
}

#[test]
fn every_captured_frame_checksum_matches() {
    let mut checked = 0usize;
    let mut artifacts = 0usize;
    for rel in CRC_ARTIFACTS {
        let Some(bytes) = artifact(rel) else {
            continue;
        };
        let frames = frames_512(&bytes);
        if frames.is_empty() {
            println!("skip: {rel} holds no 512-byte frame");
            continue;
        }
        let mut bad = Vec::new();
        for (index, frame) in frames.iter().enumerate() {
            if !verify(frame) {
                bad.push((index, stored_checksum(frame), {
                    let mut zeroed = frame.to_vec();
                    zeroed[EAC_CHECKSUM_OFF..EAC_CHECKSUM_OFF + 4].fill(0);
                    crc32c_eac(&zeroed)
                }));
            }
        }
        println!(
            "{rel}: {}/{} 512-byte frames verify (sid=0x{:x})",
            frames.len() - bad.len(),
            frames.len(),
            session_of(&frames[0])
        );
        assert!(
            bad.is_empty(),
            "{rel}: {} frame(s) disagree with CRC-32C/EAC, first {:?}",
            bad.len(),
            bad.first()
        );
        checked += frames.len();
        artifacts += 1;
    }
    println!("verified {checked} frames across {artifacts} artifact(s)");
    if artifacts == 0 {
        println!("skip: no evidence artifact found; nothing was verified");
        return;
    }
    assert!(checked >= 100, "only {checked} frames were available");
}

#[test]
fn retarget_rewrites_session_sequence_and_reseals() {
    let Some(bytes) = artifact(CRC_ARTIFACTS[0]) else {
        return;
    };
    let frames = frames_512(&bytes);
    let Some(recorded) = frames.first().cloned() else {
        println!("skip: no 512-byte frame to retarget");
        return;
    };
    let old_session = session_of(&recorded);
    let old_sequence = sequence_of(&recorded);
    let new_session = 0xDEAD_BEEF_0102_0304u64;
    let new_sequence = 0x0102_0304_0506_0708u64;

    let retargeted = retarget_frame(&recorded, Some(new_session), new_sequence);
    assert_eq!(retargeted.len(), EAC_FRAME_LEN, "frame size is preserved");
    assert_eq!(session_of(&retargeted), new_session, "SessionId at [4:12]");
    assert_eq!(sequence_of(&retargeted), new_sequence, "Sequence at [12:20]");
    assert_ne!(session_of(&retargeted), old_session);
    assert_ne!(sequence_of(&retargeted), old_sequence);
    assert!(verify(&retargeted), "the rewritten frame must carry a fresh CRC");

    // Everything outside the three rewritten fields is the recorded bytes: the
    // body is what the live session could not produce.
    for (index, (old, new)) in recorded.iter().zip(retargeted.iter()).enumerate() {
        let rewritten = (SESSION_OFF..SESSION_OFF + 8).contains(&index)
            || (SEQUENCE_OFF..SEQUENCE_OFF + 8).contains(&index)
            || (EAC_CHECKSUM_OFF..EAC_CHECKSUM_OFF + 4).contains(&index);
        if !rewritten {
            assert_eq!(old, new, "byte {index} outside the retargeted fields");
        }
    }
    assert_eq!(&retargeted[HEADER_LEN..], &recorded[HEADER_LEN..], "body");

    // Re-applying the same sequence number must be byte-identical: the seal is
    // over a zeroed window, so it cannot feed back into itself.
    let again = retarget_frame(&retargeted, Some(new_session), new_sequence);
    assert_eq!(again, retargeted, "retargeting at the same seq is idempotent");
    println!(
        "retarget: sid 0x{old_session:x}->0x{new_session:x}, seq {old_sequence}->{new_sequence}, crc 0x{:08x}",
        stored_checksum(&retargeted)
    );
}

#[test]
fn retarget_ignores_frames_that_are_not_512_bytes() {
    let Some(bytes) = artifact(CRC_ARTIFACTS[0]) else {
        return;
    };
    let payloads = parse_payload_records(&bytes).expect("record stream");
    let short: Vec<Vec<u8>> = payloads
        .into_iter()
        .filter(|payload| payload.len() != EAC_FRAME_LEN)
        .collect();
    assert!(!short.is_empty(), "the capture should hold control frames too");
    for payload in short {
        let expected = payload.clone();
        assert_eq!(retarget_frame(&payload, Some(0x1234), 77), expected);
    }
    // A capture may also hold a 100-byte (116/120) control frame, but a frame
    // one byte off must still be passed through untouched.
    let almost = vec![0x5Au8; EAC_FRAME_LEN - 1];
    assert_eq!(retarget_frame(&almost, Some(1), 2), almost);
    println!("non-512-byte payloads are passed through unchanged");
}

#[test]
fn parse_payload_records_accounts_for_the_whole_file() {
    let Some(bytes) = artifact(CRC_ARTIFACTS[0]) else {
        return;
    };
    let payloads = parse_payload_records(&bytes).expect("control_0009 is a record stream");
    assert!(!payloads.is_empty());
    let mut consumed = 0usize;
    for payload in &payloads {
        assert!(!payload.is_empty(), "zero-length record");
        assert!(
            payload.len() <= bytes.len() - 8,
            "record length {} exceeds the file ({} B)",
            payload.len(),
            bytes.len()
        );
        consumed = consumed
            .checked_add(8 + payload.len())
            .expect("record offset fits in usize");
    }
    assert_eq!(
        consumed,
        bytes.len(),
        "the records must account for every byte of the capture"
    );
    let n512 = payloads
        .iter()
        .filter(|payload| payload.len() == EAC_FRAME_LEN)
        .count();
    println!(
        "parsed {} records ({n512} of them 512-byte frames) from {} B",
        payloads.len(),
        bytes.len()
    );
}

#[test]
fn parse_payload_records_survives_a_cut_tail() {
    let Some(bytes) = artifact(CRC_ARTIFACTS[0]) else {
        return;
    };
    let full = parse_payload_records(&bytes).expect("record stream");
    // Cutting inside the last record loses exactly that record; cutting inside
    // a header keeps the records before it. Neither may panic.
    let cuts = [
        0usize,
        1,
        7,
        8,
        9,
        bytes.len() / 2 + 3,
        bytes.len() - 1,
        bytes.len() - 7,
        bytes.len() - 9,
    ];
    for cut in cuts {
        let sliced = &bytes[..cut.min(bytes.len())];
        let parsed = parse_payload_records(sliced);
        match &parsed {
            None => println!("cut at {cut}: no complete record"),
            Some(payloads) => {
                assert!(
                    payloads.len() <= full.len(),
                    "a shorter buffer cannot yield more records"
                );
                assert_eq!(
                    payloads.as_slice(),
                    &full[..payloads.len()],
                    "a cut tail must only drop records, not corrupt them"
                );
                println!("cut at {cut}: {} intact record(s)", payloads.len());
            }
        }
    }
    // A bare payload (a single raw frame) is not a record stream.
    let raw = vec![0x5Au8; EAC_FRAME_LEN];
    assert_eq!(parse_payload_records(&raw), None);
    assert_eq!(parse_payload_records(&[]), None);
    assert_eq!(
        parse_payload_records(&bytes[..8]),
        None,
        "a lone header without payload is empty"
    );
}

#[test]
fn every_frame_of_a_session_shares_one_fresh_checksum() {
    let Some(bytes) = artifact(CRC_ARTIFACTS[0]) else {
        return;
    };
    let frames = frames_512(&bytes);
    assert!(frames.len() > 1);
    // Stored checksums are effectively unique — the body is fresh every 2 s —
    // so a "recomputed all four bytes correctly" claim is not one value reused.
    let mut stored: Vec<u32> = frames.iter().map(|frame| stored_checksum(frame)).collect();
    stored.sort_unstable();
    stored.dedup();
    println!(
        "{}/{} distinct checksums",
        stored.len(),
        frames.len()
    );
    assert!(
        stored.len() * 4 > frames.len() * 3,
        "checksums look reused: {} distinct of {}",
        stored.len(),
        frames.len()
    );
}
