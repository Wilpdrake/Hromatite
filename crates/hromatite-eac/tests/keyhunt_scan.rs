//! End-to-end dry run of `EAC_KEYHUNT=1` against this very process.
//!
//! The scan path is the same code the payload runs inside VRChat: region map
//! from `/proc/self/maps`, budgeted chunked scan, hit logging, window dumps.
//! The "module buffer" is a local `Vec` that is made to look like a 512-byte AC
//! frame (12-byte stable prefix) with the seed key planted right behind it, so a
//! correct scan must find both the frame prefix and the seed.

use std::sync::Mutex;

use hromatite_eac::keyhunt::{self, Hunter, Region, SEED_KEY};

/// Environment variables are process-global; every test in this binary shares
/// the flag.
static GUARD: Mutex<()> = Mutex::new(());

fn fake_frame() -> Vec<u8> {
    let mut frame = vec![0u8; 512 + 1024];
    // Header the live module emits: 02 00 xx 00 | SessionId | 0 | counter.
    frame[0..12].copy_from_slice(&[0x02, 0x00, 0x00, 0x00, 0x8b, 0x15, 0x00, 0x00, 0, 0, 0, 0]);
    frame[12..16].copy_from_slice(&7u32.to_le_bytes());
    for (index, byte) in frame[16..512].iter_mut().enumerate() {
        *byte = (index as u8).wrapping_mul(37).wrapping_add(11);
    }
    // "key material" 200 bytes past the frame in the same allocation
    frame[712..744].copy_from_slice(&SEED_KEY);
    frame
}

#[test]
fn dry_run_finds_the_seed_key_the_frame_prefix_and_dumps_windows() {
    let _guard = GUARD.lock().unwrap_or_else(|poison| poison.into_inner());
    let dir = std::env::temp_dir().join(format!("hromatite-keyhunt-dry-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    // SAFETY: single-threaded setup for this binary's tests, before any read.
    unsafe {
        std::env::set_var("EAC_KEYHUNT", "1");
        std::env::set_var("EAC_KEYHUNT_DIR", &dir);
        std::env::set_var("EAC_KEYHUNT_CHUNK_MB", "256");
        std::env::set_var("EAC_KEYHUNT_DELAY_MS", "0");
        std::env::set_var("EAC_KEYHUNT_GAP_MS", "0");
    }

    let frame = fake_frame();
    let ptr = frame.as_ptr() as usize;
    keyhunt::record_frame(ptr, 512, &frame[..16], 0);
    assert!(keyhunt::probe().is_some(), "frame probe recorded");

    let mut hunter = Hunter::new();
    assert!(hunter.enabled());
    let mut lines = Vec::new();
    let mut guard = 0;
    // The host has a few GB of maps; 256 MiB per tick bounds each call.
    while guard < 200 {
        lines.extend(hunter.tick());
        if lines.iter().any(|line| line.contains("pass 2 done")) {
            break;
        }
        guard += 1;
    }
    let log = std::fs::read_to_string(dir.join("keyhunt.log")).expect("keyhunt log");
    if std::env::var_os("EAC_KEYHUNT_KEEP").is_none() {
        let _ = std::fs::remove_dir_all(&dir);
    } else {
        eprintln!("kept keyhunt dir: {}", dir.display());
    }

    assert!(guard < 200, "two passes completed");
    assert!(
        log.contains("keyhunt: frame-buffer"),
        "buffer probed: {log}"
    );
    assert!(log.contains("needle=seed "), "seed key found: {log}");
    assert!(
        log.contains("needle=frame-head"),
        "frame prefix found: {log}"
    );
    assert!(log.contains("pass 1 done"), "pass summary written");
    let console = lines.join("\n");
    assert!(console.contains("eac: keyhunt: pass 1 start"), "{console}");
}

#[test]
fn region_parser_covers_the_live_map_and_the_frame_probe_region_is_readable() {
    let _guard = GUARD.lock().unwrap_or_else(|poison| poison.into_inner());
    let text = std::fs::read_to_string("/proc/self/maps").expect("maps");
    let regions = keyhunt::parse_maps(&text);
    assert!(regions.len() > 8);
    assert!(regions.iter().any(|region| region.readable()));
    // Our own buffer must be inside a readable region.
    let buffer = [1u8; 64];
    let addr = buffer.as_ptr() as usize;
    let region = regions
        .iter()
        .find(|region| addr >= region.start && addr < region.end)
        .expect("buffer mapped");
    assert!(region.readable());
    let hits = keyhunt::find_in_region(region, &[1u8; 32]);
    assert!(
        hits.iter().any(|hit| *hit <= addr && *hit + 32 > addr),
        "hit inside buffer"
    );
}

#[test]
fn an_unreadable_region_is_skipped_without_faulting() {
    let region = Region {
        start: 0x1000,
        end: 0x2000,
        perms: "---p".to_string(),
        path: "[anon]".to_string(),
    };
    assert!(keyhunt::find_in_region(&region, &[0u8; 32]).is_empty());
}
