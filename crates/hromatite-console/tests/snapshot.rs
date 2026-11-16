use std::process::Command;

#[test]
fn demo_snapshot_is_plain_headless_and_explicit() {
    let output = Command::new(env!("CARGO_BIN_EXE_hromatite-console"))
        .args(["--demo", "--snapshot", "--size", "100x24"])
        .output()
        .expect("run snapshot binary");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains('\u{1b}'));
    assert_eq!(text.lines().count(), 24);
    assert!(text.contains("DEMO // SYNTHETIC"));
    assert!(text.contains("legacy unframed compatibility line"));
}

#[test]
fn live_log_snapshot_surfaces_hot_generation() {
    let log = std::env::temp_dir().join(format!(
        "hromatite-console-hot-snapshot-{}.log",
        std::process::id()
    ));
    std::fs::write(
        &log,
        "[10] [con] feature: generation=3 abi=1\n\
         [11] [hot] state=active generation=3 previous=2 pending=none sha=0123456789ab flight=0 error=none\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hromatite-console"))
        .arg("--log")
        .arg(&log)
        .args(["--snapshot", "--size", "140x18"])
        .output()
        .expect("run hot snapshot binary");
    let _ = std::fs::remove_file(&log);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(!text.contains('\u{1b}'));
    assert_eq!(text.lines().count(), 18);
    assert!(text.contains("hot active gen=3 prev=2 pending=-- flight=0 sha=0123456789ab"));
    assert!(text.contains("feature gen=3 abi=1"));
}

#[test]
fn help_is_available_without_a_tty_or_logfile() {
    let output = Command::new(env!("CARGO_BIN_EXE_hromatite-console"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("F5 refresh once"));
    assert!(text.contains("F6 auto telemetry"));
    assert!(text.contains("F1 field manual"));
    assert!(text.contains("--demo"));
}
