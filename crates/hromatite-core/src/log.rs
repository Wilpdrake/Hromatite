//! File logging. The game process has no console under Proton, so every stage
//! of the pipeline writes to one predictable file inside the wine prefix.

use std::fs::OpenOptions;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn path() -> std::path::PathBuf {
    let base = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:".to_string());
    std::path::PathBuf::from(base).join("hromatite.log")
}

fn stamps(secs: u64, tag: &str, msg: &str) -> String {
    format!("[{secs}] [{tag}] {msg}\r\n")
}

pub fn log_tag(tag: &str, msg: &str) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = stamps(secs, tag, msg);
    for p in [path(), std::path::PathBuf::from("C:\\hromatite.log")] {
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&p) {
            let _ = f.write_all(line.as_bytes());
            return;
        }
    }
}

pub fn log(msg: &str) {
    log_tag("hromatite", msg);
}

/// Log to file *and* stdout (only the injector exe has a stdout).
pub fn log_echo(msg: &str) {
    log(msg);
    println!("{msg}");
}

#[macro_export]
macro_rules! hlog {
    ($($arg:tt)*) => { $crate::log::log(&format!($($arg)*)) };
}
