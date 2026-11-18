//! Log sources: which files the console follows and how a path is resolved.
//!
//! Everything here is read-only host-side file access. Nothing is created inside
//! the game process: the console only observes files that the game, Proton, EAC
//! and the payload already write. Several of those get a timestamped file name
//! per session (`output_log_<ts>.txt`, `game-<ts>.log`, `anticheatlauncher.log`
//! plus its backups), so a source is a pattern that is re-resolved while it runs
//! and follows the newest matching file.

use crate::config::Config;
use crate::fileio::LogTail;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Check every wildcard source this often (in polls) instead of on each frame.
const RESOLVE_EVERY: u64 = 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSpec {
    /// Short name used by the tab strip and the `log` command.
    pub label: &'static str,
    /// Absolute file path, or a path with `*` in one or more components.
    pub pattern: PathBuf,
}

/// The sources the console follows by default, derived from the Proton prefix.
///
/// The first entry is always the payload log: it carries the command channel,
/// the tick and the telemetry the status bar shows.
pub fn plan(config: &Config) -> Vec<SourceSpec> {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"));
    let user_dir = config
        .prefix
        .join("drive_c")
        .join("users")
        .join("steamuser");
    let dbg_dir = std::env::var_os("HROMATITE_DBG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".hromatite-logs"));
    let game_dir = game_dir(&config.prefix);

    let mut sources = vec![SourceSpec {
        label: "payload",
        pattern: config.log_path.clone(),
    }];
    if game_dir.is_some() {
        sources.push(SourceSpec {
            label: "game",
            // Unity writes the player log under LocalLow, not next to the exe.
            pattern: user_dir
                .join("AppData/LocalLow/VRChat/VRChat")
                .join("output_log_*.txt"),
        });
    }
    sources.push(SourceSpec {
        label: "eac",
        pattern: user_dir
            .join("AppData/Roaming/EasyAntiCheat")
            .join("*/*/anticheatlauncher.log"),
    });
    let hooks = std::env::var_os("HROMATITE_EAC_HOOKS_LOG")
        .map(PathBuf::from)
        .unwrap_or_else(|| user_dir.join("hromatite_eac_hooks.log"));
    sources.push(SourceSpec {
        label: "hooks",
        pattern: hooks,
    });
    let gc = std::env::var_os("HROMATITE_GCLOG")
        .map(PathBuf::from)
        .or_else(|| game_dir.map(|dir| dir.join("VRChat.gc.log")));
    if let Some(path) = gc {
        sources.push(SourceSpec {
            label: "gc",
            pattern: path,
        });
    }
    sources.push(SourceSpec {
        label: "stdout",
        pattern: dbg_dir.join("game-*.log"),
    });
    sources.push(SourceSpec {
        label: "launch",
        pattern: home.join(".hromatite-launch.log"),
    });
    sources
}

/// `.../steamapps/compatdata/438100/pfx` → `.../steamapps/common/VRChat`.
fn game_dir(prefix: &Path) -> Option<PathBuf> {
    let steamapps = prefix
        .ancestors()
        .find(|path| path.file_name().is_some_and(|name| name == "steamapps"))?;
    Some(steamapps.join("common").join("VRChat"))
}

/// Every existing file matching `pattern`, in directory order.
///
/// A path component may contain `*` (`output_log_*.txt`), and a whole component
/// may be `*` (`EasyAntiCheat/*/*/anticheatlauncher.log`). A pattern with no
/// wildcard returns at most one path.
pub fn expand(pattern: &Path) -> Vec<PathBuf> {
    let text = pattern.to_string_lossy();
    let mut current = vec![PathBuf::from("/")];
    for part in text.trim_start_matches('/').split('/') {
        if part.is_empty() {
            continue;
        }
        let mut next = Vec::new();
        for base in &current {
            if part.contains('*') {
                if let Ok(entries) = std::fs::read_dir(base) {
                    for entry in entries.flatten() {
                        let name = entry.file_name();
                        if component_matches(part, &name.to_string_lossy()) {
                            next.push(entry.path());
                        }
                    }
                }
            } else {
                let candidate = base.join(part);
                if candidate.exists() {
                    next.push(candidate);
                }
            }
        }
        current = next;
        if current.is_empty() {
            return Vec::new();
        }
    }
    current.retain(|path| path.is_file());
    current
}

/// `*` inside one path component, without pulling in a glob dependency.
fn component_matches(pattern: &str, name: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == name;
    }
    let mut pieces = pattern.split('*');
    let head = pieces.next().unwrap_or_default();
    if !name.starts_with(head) {
        return false;
    }
    let mut rest = &name[head.len()..];
    let tail = pieces.next_back();
    for piece in pieces {
        if piece.is_empty() {
            continue;
        }
        match rest.find(piece) {
            Some(at) => rest = &rest[at + piece.len()..],
            None => return false,
        }
    }
    match tail {
        Some(tail) if !tail.is_empty() => rest.len() >= tail.len() && rest.ends_with(tail),
        _ => true,
    }
}

/// Newest (by modification time) file matching the pattern.
pub fn newest(pattern: &Path) -> Option<PathBuf> {
    expand(pattern).into_iter().max_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    })
}

pub struct Polled {
    pub lines: Vec<String>,
    /// The file actually being read, or `None` while the pattern matches nothing.
    pub resolved: Option<PathBuf>,
    /// True when the pattern started pointing at a different file (game restart,
    /// log rotation), so the view was replayed from the new file.
    pub retargeted: bool,
}

/// One followed source: resolves the newest match, then tails it.
pub struct SourceTail {
    pattern: PathBuf,
    path: Option<PathBuf>,
    tail: Option<LogTail>,
    opened: bool,
    polls: u64,
}

impl SourceTail {
    pub fn new(pattern: PathBuf) -> Self {
        Self {
            pattern,
            path: None,
            tail: None,
            opened: false,
            polls: 0,
        }
    }

    /// What to show in the UI: the open file, else the pattern with its `*`.
    pub fn display(&self) -> String {
        self.path
            .as_ref()
            .unwrap_or(&self.pattern)
            .display()
            .to_string()
    }

    pub fn resolved(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn poll(&mut self) -> io::Result<Polled> {
        // Re-resolving a wildcard costs a directory listing; a new game log
        // appearing two seconds late is harmless, 140 listings per second are not.
        self.polls = self.polls.wrapping_add(1);
        if self.path.is_none() || self.polls.is_multiple_of(RESOLVE_EVERY) {
            let newest = newest(&self.pattern);
            if newest != self.path {
                self.path = newest;
                self.tail = None;
            }
        }
        let mut retargeted = false;
        if self.tail.is_none()
            && let Some(path) = &self.path
        {
            self.tail = Some(LogTail::open(path, true)?);
            retargeted = self.opened;
            self.opened = true;
        }
        let lines = match self.tail.as_mut() {
            Some(tail) => tail.poll()?,
            None => Vec::new(),
        };
        Ok(Polled {
            lines,
            resolved: self.path.clone(),
            retargeted,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{SourceTail, expand, newest, plan};
    use crate::config::Config;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, SystemTime};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "hromatite-sources-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write(path: &std::path::Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, text).unwrap();
    }

    fn set_age(path: &std::path::Path, secs: u64) {
        let when = SystemTime::now() - Duration::from_secs(secs);
        let file = fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(when).unwrap();
    }

    #[test]
    fn wildcards_expand_one_component_and_pick_the_newest_file() {
        let dir = TestDir::new();
        let old = dir.0.join("game-1.log");
        let new = dir.0.join("game-2.log");
        write(&old, "old\n");
        write(&new, "new\n");
        set_age(&old, 600);
        set_age(&new, 5);
        write(&dir.0.join("other.txt"), "ignored\n");

        let mut found = expand(&dir.0.join("*.txt"));
        assert_eq!(found.len(), 1, "{found:?}");
        found = expand(&dir.0.join("game-*.log"));
        assert_eq!(found.len(), 2);
        assert_eq!(newest(&dir.0.join("game-*.log")), Some(new.clone()));
        assert_eq!(newest(&dir.0.join("missing-*.log")), None);
        // A literal path (no wildcard) resolves to itself only when it exists.
        assert_eq!(newest(&new), Some(new.clone()));
        assert_eq!(newest(&dir.0.join("absent.log")), None);
    }

    #[test]
    fn nested_wildcards_reach_two_hash_levels_like_the_eac_directory() {
        let dir = TestDir::new();
        let log = dir.0.join("EasyAntiCheat/aaaa/bbbb/anticheatlauncher.log");
        write(&log, "launcher ready\n");
        write(
            &dir.0
                .join("EasyAntiCheat/aaaa/bbbb/anticheatlauncher-backup-1.log"),
            "backup\n",
        );
        assert_eq!(
            newest(&dir.0.join("EasyAntiCheat/*/*/anticheatlauncher.log")),
            Some(log)
        );
    }

    #[test]
    fn a_source_replays_initially_and_switches_when_a_newer_file_appears() {
        let dir = TestDir::new();
        let first = dir.0.join("output_log_1.txt");
        write(&first, "first session\n");
        let mut source = SourceTail::new(dir.0.join("output_log_*.txt"));
        let opened = source.poll().unwrap();
        assert!(!opened.retargeted, "the first open is not a retarget");
        assert_eq!(opened.lines, vec!["first session"]);
        assert_eq!(source.display(), first.display().to_string());

        let second = dir.0.join("output_log_2.txt");
        write(&second, "second session\n");
        set_age(&first, 600);
        // Resolution is throttled; poll until the newest match is noticed.
        let mut moved = None;
        for _ in 0..40 {
            let polled = source.poll().unwrap();
            if polled.retargeted {
                moved = Some(polled);
                break;
            }
        }
        let moved = moved.expect("the source must follow the newer log");
        assert_eq!(moved.lines, vec!["second session"]);
        assert_eq!(moved.resolved, Some(second));
    }

    #[test]
    fn a_source_without_a_file_reports_the_pattern_and_reads_nothing() {
        let dir = TestDir::new();
        let mut source = SourceTail::new(dir.0.join("game-*.log"));
        let polled = source.poll().unwrap();
        assert!(polled.lines.is_empty());
        assert_eq!(polled.resolved, None);
        assert!(source.display().contains("game-*.log"));

        write(&dir.0.join("game-9.log"), "late arrival\n");
        let mut lines = Vec::new();
        for _ in 0..3 {
            lines.extend(source.poll().unwrap().lines);
        }
        assert_eq!(lines, vec!["late arrival"]);
    }

    #[test]
    fn the_plan_starts_with_the_payload_and_derives_the_steam_paths() {
        let base = Config::parse([
            "console",
            "--prefix",
            "/steam/steamapps/compatdata/438100/pfx",
        ])
        .unwrap();
        let sources = plan(&base);
        assert_eq!(sources[0].label, "payload");
        assert!(sources[0].pattern.ends_with("hromatite.log"));
        let labels: Vec<_> = sources.iter().map(|source| source.label).collect();
        assert!(labels.contains(&"game"), "{labels:?}");
        assert!(labels.contains(&"eac"), "{labels:?}");
        let game = sources
            .iter()
            .find(|source| source.label == "game")
            .unwrap();
        assert_eq!(
            game.pattern,
            PathBuf::from(
                "/steam/steamapps/compatdata/438100/pfx/drive_c/users/steamuser/\
                 AppData/LocalLow/VRChat/VRChat/output_log_*.txt"
            )
        );
        let eac = sources.iter().find(|source| source.label == "eac").unwrap();
        assert!(
            eac.pattern
                .ends_with("EasyAntiCheat/*/*/anticheatlauncher.log")
        );
        // An explicit --log replaces the payload source, it does not add one.
        let custom = Config::parse(["console", "--log", "/tmp/live.log"]).unwrap();
        assert_eq!(plan(&custom)[0].pattern, PathBuf::from("/tmp/live.log"));
    }
}
