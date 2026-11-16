//! Polling tailer for the VRChat `output_log_*.txt` file.
//!
//! No threads, no notify crate: the payload tick calls `poll()` and receives the
//! complete lines appended since the previous call. Partial lines are buffered
//! until their newline arrives; truncation (game restart, log rolled) resets the
//! offset and discards the partial buffer.
//!
//! Only the markers that correlate with anti-cheat session lifetime are
//! mirrored: `Entering Room:`, `Joining wrld_`, `OnLeftRoom`, `OnDisconnected`
//! and anything tagged `[AntiCheatClient]`.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Substrings that make a game log line interesting to the EAC model.
pub const GAME_MARKERS: &[&str] = &[
    "Entering Room:",
    "Joining wrld_",
    "OnLeftRoom",
    "OnDisconnected",
    "[AntiCheatClient]",
];

/// Largest chunk read per poll; a bigger append is picked up on the next poll.
const MAX_READ: u64 = 256 * 1024;

/// True when `line` contains one of [`GAME_MARKERS`].
pub fn is_game_line(line: &str) -> bool {
    GAME_MARKERS.iter().any(|marker| line.contains(marker))
}

/// True when a `[AntiCheatClient]` line announces a session start.
pub fn announces_begin(line: &str) -> bool {
    line.contains("[AntiCheatClient]") && line.contains("BeginSession")
}

/// True when a `[AntiCheatClient]` line announces a session end.
pub fn announces_end(line: &str) -> bool {
    line.contains("[AntiCheatClient]") && line.contains("EndSession")
}

/// Incremental reader over one game log file.
pub struct Tailer {
    path: PathBuf,
    file: Option<File>,
    offset: u64,
    partial: Vec<u8>,
    lines_seen: u64,
}

impl Tailer {
    /// Remember `path`; the file is opened on the first `poll`.
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            file: None,
            offset: 0,
            partial: Vec::new(),
            lines_seen: 0,
        }
    }

    /// Open `path` immediately (surfaces a missing file to the caller).
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let mut tailer = Self::new(path);
        tailer.ensure_open()?;
        Ok(tailer)
    }

    /// Path being tailed.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Lines returned so far, matching markers included.
    pub fn lines_seen(&self) -> u64 {
        self.lines_seen
    }

    /// Byte offset already consumed.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Read everything appended since the last poll; return complete lines.
    ///
    /// Only marker lines are returned, but every consumed line advances
    /// `lines_seen` so `eac.status` can show the tailer is alive.
    pub fn poll(&mut self) -> io::Result<Vec<String>> {
        self.ensure_open()?;
        let Some(file) = self.file.as_mut() else {
            return Ok(Vec::new());
        };
        let length = file.metadata()?.len();
        if length < self.offset {
            // Truncated or replaced: start over.
            file.seek(SeekFrom::Start(0))?;
            self.offset = 0;
            self.partial.clear();
        } else if length == self.offset {
            return Ok(Vec::new());
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buffer = Vec::new();
        (&mut *file).take(MAX_READ).read_to_end(&mut buffer)?;
        if buffer.is_empty() {
            return Ok(Vec::new());
        }
        self.offset = self.offset.saturating_add(buffer.len() as u64);

        let mut lines = Vec::new();
        if !self.partial.is_empty() {
            if let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
                let mut head = std::mem::take(&mut self.partial);
                head.extend_from_slice(&buffer[..newline]);
                self.lines_seen = self.lines_seen.saturating_add(1);
                push_line(&mut lines, &head);
                buffer.drain(..=newline);
            } else {
                self.partial.extend_from_slice(&buffer);
                return Ok(lines);
            }
        }
        let mut rest = &buffer[..];
        while let Some(newline) = rest.iter().position(|byte| *byte == b'\n') {
            let (line, tail) = rest.split_at(newline);
            self.lines_seen = self.lines_seen.saturating_add(1);
            push_line(&mut lines, line);
            rest = &tail[1..];
        }
        self.partial.extend_from_slice(rest);
        Ok(lines)
    }

    fn ensure_open(&mut self) -> io::Result<()> {
        if self.file.is_none() {
            let mut file = OpenOptions::new().read(true).open(&self.path)?;
            self.offset = file.seek(SeekFrom::End(0))?;
            self.file = Some(file);
        }
        Ok(())
    }
}

fn push_line(lines: &mut Vec<String>, raw: &[u8]) {
    let text = String::from_utf8_lossy(raw);
    let text = text.trim_end_matches('\r');
    if is_game_line(text) {
        lines.push(text.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn only_marker_lines_are_returned_and_partial_lines_are_buffered() {
        let dir = std::env::temp_dir().join(format!("hromatite-eac-tail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("output_log.txt");
        let mut file = File::create(&path).expect("create");
        writeln!(file, "noise before the interesting part").expect("w");
        writeln!(file, "[Behaviour] Entering Room: Puddle").expect("w");
        file.flush().expect("flush");

        let mut tailer = Tailer::open(&path).expect("tailer");
        // Open positions at end-of-file: new appends only.
        assert!(tailer.poll().expect("poll").is_empty());

        let mut append = OpenOptions::new().append(true).open(&path).expect("append");
        write!(append, "Joining wrld_1234-5678:0001 fault").expect("w");
        append.flush().expect("flush");
        assert!(tailer.poll().expect("poll").is_empty(), "partial line waits");

        writeln!(append, " but not really").expect("w");
        append.flush().expect("flush");
        let lines = tailer.poll().expect("poll");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("Joining wrld_1234-5678:0001"));

        writeln!(append, "[AntiCheatClient] OnLeftRoom then EndSession").expect("w");
        writeln!(append, "OnDisconnected (0)").expect("w");
        append.flush().expect("flush");
        let lines = tailer.poll().expect("poll");
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(announces_end(&lines[0]));
        assert!(!announces_begin(&lines[0]));

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn truncation_restarts_from_the_beginning() {
        let dir = std::env::temp_dir().join(format!("hromatite-eac-trunc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("output_log.txt");
        std::fs::write(&path, "Entering Room: one\nEntering Room: two\n").expect("write");
        let mut tailer = Tailer::open(&path).expect("tailer");
        std::fs::write(&path, "OnLeftRoom\n").expect("rewrite");
        let lines = tailer.poll().expect("poll");
        assert_eq!(lines, vec!["OnLeftRoom".to_string()]);
        std::fs::remove_dir_all(dir).ok();
    }
}
