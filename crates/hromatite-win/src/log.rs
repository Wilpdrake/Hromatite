//! File logging. The game process has no console under Proton, so every stage
//! of the pipeline writes to one predictable file inside the wine prefix.

#![forbid(unsafe_code)]

use std::fs::OpenOptions;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn path() -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let base = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:".to_string());
        std::path::PathBuf::from(base).join("hromatite.log")
    }
    #[cfg(not(windows))]
    {
        // Host builds have no `%USERPROFILE%`. The old default produced a file
        // literally named `C:\hromatite.log` in the working directory, which
        // every `cargo test` run left inside the crate that was logging.
        std::env::temp_dir().join("hromatite.log")
    }
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
    #[cfg(windows)]
    let candidates = [path(), std::path::PathBuf::from("C:\\hromatite.log")];
    #[cfg(not(windows))]
    let candidates = [path()];
    for p in candidates {
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

/// Panic hook (installed once per process, exe **and** dll).
///
/// A heap corruption bug in the payload used to surface as "Fatal error in GC"
/// with no trace of where it came from; with `panic = "unwind"` and this hook
/// every Rust panic leaves a `[dll] PANIC at file:line: message` line before it
/// is caught. The hook itself must never panic (a panic inside a panic hook
/// aborts the process), so it only formats and appends to the log.
pub fn install_panic_hook() {
    use std::sync::Once;
    static HOOK: Once = Once::new();
    HOOK.call_once(|| {
        // Drop the default hook: under Proton stderr is usually gone, and its
        // writes are the one thing we cannot afford to fail.
        let _ = std::panic::take_hook();
        std::panic::set_hook(Box::new(|info| {
            let loc = match info.location() {
                Some(l) => format!("{}:{}", l.file(), l.line()),
                None => "<unknown>".to_string(),
            };
            let payload = info.payload();
            let msg = if let Some(s) = payload.downcast_ref::<&str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "<non-string panic payload>".to_string()
            };
            log_tag("dll", &format!("PANIC at {loc}: {msg}"));
        }));
    });
}

/// Run `f`, turning a Rust panic into a log line instead of a dead game.
///
/// Not `catch_unwind`: the payload is a manually mapped image with erased PE
/// headers, so the OS unwinder cannot find its `.pdata` and any real unwind
/// takes the process down (measured live — `PANIC` line, then the game is gone).
/// `veh::call_guarded` catches the panic's SEH raise (`0x20474343`) and longjmps
/// back to this frame, which needs no unwind tables at all.
///
/// Trade-off: locals of `f` are leaked on the panic path (no destructors). That
/// is the right price for a debug command that panics.
pub fn guard<T>(label: &str, f: impl FnOnce() -> T) -> Option<T> {
    match crate::veh::call_guarded(f) {
        Ok(v) => Some(v),
        Err(fault) => {
            // The hook has already logged location + message (it runs before the
            // raise); this line names the call site so the log reads top-down.
            log_tag(
                "dll",
                &format!("guard[{label}]: {fault} caught, continuing"),
            );
            None
        }
    }
}
