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
