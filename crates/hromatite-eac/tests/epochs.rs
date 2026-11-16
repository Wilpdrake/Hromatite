//! Host-runnable end-to-end check: two anti-cheat epochs separated by a world
//! load, written to a real JSONL file, read back and asserted field by field.
//!
//! This is the test the plan promises: `eac.tick`-equivalent calls only, no
//! game process, no FFI.

use std::fs;
use std::path::PathBuf;

use hromatite_eac::eventlog::{Event, JsonlLog};
use hromatite_eac::session::EacSession;

fn temp_log() -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "hromatite-eac-epochs-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("eac.jsonl");
    (dir, path)
}

/// Extract `"key":<raw>` where raw stops at `,` or `}`.
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("\"{key}\":");
    let start = line.find(&needle)? + needle.len();
    let rest = &line[start..];
    let end = rest.find([',', '}']).unwrap_or(rest.len());
    Some(&rest[..end])
}

#[test]
fn two_epochs_and_a_world_load_gap_are_written_as_jsonl_records() {
    let (dir, path) = temp_log();
    let mut log = JsonlLog::with_limits(&path, 1 << 20, 2).expect("log");
    let mut session = EacSession::new(5_000);

    // ---- epoch 1: a room with two steady heartbeats and one late one ----
    let begin = session.begin(1_763_000_000_000, Some(0x1122_3344_5566_7788));
    log.write(
        &Event::at(begin.timestamp_ms, "session_begin")
            .num("epoch", begin.epoch)
            .text("sid", format!("0x{:016x}", begin.session_id))
            .num("interval_ms", session.interval_ms()),
    )
    .expect("session_begin");

    let mut heartbeats = 0;
    for at in [1_763_000_000_500u64, 1_763_000_005_500, 1_763_000_010_500] {
        let beat = session.beat(at);
        heartbeats += 1;
        log.write(
            &Event::at(beat.timestamp_ms, "heartbeat")
                .num("epoch", beat.epoch)
                .text("sid", format!("0x{:016x}", beat.session_id))
                .num("seq", beat.seq)
                .num("hb_in_epoch", u64::from(beat.hb_in_epoch))
                .opt_num("dt_ms", beat.dt_ms)
                .num("frame_len", 144),
        )
        .expect("heartbeat");
        if beat.gap {
            log.write(
                &Event::at(beat.timestamp_ms, "gap")
                    .num("epoch", beat.epoch)
                    .num("dt_ms", beat.dt_ms.unwrap_or(0))
                    .num("expected_ms", session.interval_ms())
                    .num("hb_in_epoch", u64::from(beat.hb_in_epoch))
                    .num("seq", beat.seq),
            )
            .expect("gap");
        }
    }

    let end = session.end(1_763_000_011_000).expect("epoch 1 was running");
    assert_eq!(end.heartbeats, 3);
    log.write(
        &Event::at(end.timestamp_ms, "session_end")
            .num("epoch", end.epoch)
            .num("heartbeats", u64::from(end.heartbeats))
            .text("reason", "OnLeftRoom"),
    )
    .expect("session_end");

    // ---- the world load: 20 s of silence, then a new epoch ----
    let begin = session.begin(1_763_000_031_000, Some(0x99aa_bbcc_ddee_ff00));
    log.write(
        &Event::at(begin.timestamp_ms, "session_begin")
            .num("epoch", begin.epoch)
            .text("sid", format!("0x{:016x}", begin.session_id)),
    )
    .expect("session_begin 2");

    let beat = session.beat(1_763_000_031_500);
    assert_eq!(beat.epoch, 2);
    assert_eq!(beat.seq, 1, "per-epoch sequence restarts");
    assert_eq!(beat.dt_ms, Some(21_000));
    assert!(beat.gap);
    log.write(
        &Event::at(beat.timestamp_ms, "heartbeat")
            .num("epoch", beat.epoch)
            .text("sid", format!("0x{:016x}", beat.session_id))
            .num("seq", beat.seq)
            .num("hb_in_epoch", u64::from(beat.hb_in_epoch))
            .opt_num("dt_ms", beat.dt_ms)
            .num("frame_len", 144),
    )
    .expect("heartbeat 2");
    log.write(
        &Event::at(beat.timestamp_ms, "gap")
            .num("epoch", beat.epoch)
            .num("dt_ms", beat.dt_ms.unwrap_or(0))
            .num("expected_ms", session.interval_ms())
            .num("hb_in_epoch", u64::from(beat.hb_in_epoch))
            .num("seq", beat.seq),
    )
    .expect("gap 2");

    // ---- read back and verify the on-disk record stream ----
    let text = fs::read_to_string(&path).expect("read log");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 8, "records: {text}");
    assert!(lines.iter().all(|line| line.starts_with("{\"ts_ms\":")));

    let events: Vec<&str> = lines
        .iter()
        .map(|line| field(line, "ev").expect("ev field"))
        .collect();
    assert_eq!(
        events,
        vec![
            "\"session_begin\"",
            "\"heartbeat\"",
            "\"heartbeat\"",
            "\"heartbeat\"",
            "\"session_end\"",
            "\"session_begin\"",
            "\"heartbeat\"",
            "\"gap\"",
        ]
    );
    assert_eq!(heartbeats, 3);

    // First heartbeat: no dt_ms at all (omitted, not null).
    assert_eq!(field(lines[1], "dt_ms"), None, "{}", lines[1]);
    assert_eq!(field(lines[1], "seq"), Some("1"));
    assert_eq!(field(lines[1], "hb_in_epoch"), Some("1"));
    assert_eq!(field(lines[1], "epoch"), Some("1"));
    // Second heartbeat: exactly one interval.
    assert_eq!(field(lines[2], "dt_ms"), Some("5000"));
    // The session_end carries the per-epoch heartbeat count.
    assert_eq!(field(lines[4], "heartbeats"), Some("3"));
    assert_eq!(field(lines[4], "reason"), Some("\"OnLeftRoom\""));
    // The world load shows up as a 20.5 s gap in epoch 2.
    let gap = lines[7];
    assert_eq!(field(gap, "dt_ms"), Some("21000"), "{gap}");
    assert_eq!(field(gap, "expected_ms"), Some("5000"), "{gap}");
    assert_eq!(field(gap, "epoch"), Some("2"), "{gap}");
    assert_eq!(field(lines[6], "dt_ms"), Some("21000"), "{}", lines[6]);
    assert_eq!(field(lines[6], "sid"), Some("\"0x99aabbccddeeff00\""));

    fs::remove_dir_all(dir).ok();
}
