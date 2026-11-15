//! Incremental file tail and append-only command channel.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

const REPLAY_BYTES: u64 = 128 * 1024;
const MAX_CARRY: usize = 1024 * 1024;

pub struct LogTail {
    path: PathBuf,
    position: u64,
    identity: Option<(u64, u64)>,
    carry: Vec<u8>,
}

impl LogTail {
    /// Open a tail that survives a missing file, truncation and recreation.
    /// With `replay_existing`, only the newest complete lines are loaded.
    pub fn open(path: &Path, replay_existing: bool) -> io::Result<Self> {
        let mut position = 0;
        let mut identity = None;
        match std::fs::metadata(path) {
            Ok(metadata) => {
                identity = Some((metadata.dev(), metadata.ino()));
                if replay_existing {
                    let mut file = File::open(path)?;
                    let len = metadata.len();
                    position = len.saturating_sub(REPLAY_BYTES);
                    if position > 0 {
                        file.seek(SeekFrom::Start(position))?;
                        let mut byte = [0u8; 1];
                        loop {
                            match file.read(&mut byte)? {
                                0 => break,
                                1 if byte[0] == b'\n' => {
                                    position += 1;
                                    break;
                                }
                                _ => position += 1,
                            }
                        }
                    }
                } else {
                    position = metadata.len();
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        Ok(Self {
            path: path.to_path_buf(),
            position,
            identity,
            carry: Vec::new(),
        })
    }

    pub fn poll(&mut self) -> io::Result<Vec<String>> {
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        let identity = (metadata.dev(), metadata.ino());
        if self.identity.is_some_and(|old| old != identity) || metadata.len() < self.position {
            self.position = 0;
            self.carry.clear();
        }
        self.identity = Some(identity);
        if metadata.len() == self.position {
            return Ok(Vec::new());
        }
        file.seek(SeekFrom::Start(self.position))?;
        let mut fresh = Vec::new();
        file.read_to_end(&mut fresh)?;
        self.position = self.position.saturating_add(fresh.len() as u64);
        self.carry.extend_from_slice(&fresh);
        let mut lines = Vec::new();
        while let Some(end) = self.carry.iter().position(|byte| *byte == b'\n') {
            let mut bytes: Vec<u8> = self.carry.drain(..=end).collect();
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            lines.push(String::from_utf8_lossy(&bytes).into_owned());
        }
        if self.carry.len() > MAX_CARRY {
            // A megabyte without newline is not useful UI content; never let a
            // broken log make memory grow without bound.
            self.carry.clear();
        }
        Ok(lines)
    }
}

pub struct CommandWriter {
    path: PathBuf,
}

impl CommandWriter {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn send(&self, command: &str) -> io::Result<()> {
        self.send_batch([command])
    }

    pub fn send_batch(
        &self,
        commands: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> io::Result<()> {
        let mut payload = String::new();
        for command in commands {
            let command = command.as_ref().trim();
            if command.is_empty() || command.contains(['\r', '\n']) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "command must be one non-empty line",
                ));
            }
            payload.push_str(command);
            payload.push('\n');
        }
        if payload.is_empty() {
            return Ok(());
        }
        // Same existing channel as the old console: create+append, never truncate.
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(payload.as_bytes())?;
        file.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::{CommandWriter, LogTail};
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);
    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "hromatite-console-test-{}-{}",
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

    #[test]
    fn tail_keeps_partial_lines_and_detects_truncation() {
        let dir = TestDir::new();
        let path = dir.0.join("game.log");
        fs::write(&path, "[1] [late] old\n").unwrap();
        let mut tail = LogTail::open(&path, true).unwrap();
        assert_eq!(tail.poll().unwrap(), vec!["[1] [late] old"]);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        write!(file, "[2] [gct] par").unwrap();
        file.flush().unwrap();
        assert!(tail.poll().unwrap().is_empty());
        write!(file, "tial\r\n").unwrap();
        file.flush().unwrap();
        assert_eq!(tail.poll().unwrap(), vec!["[2] [gct] partial"]);
        fs::write(&path, "[3] [dll] after truncate\n").unwrap();
        assert_eq!(tail.poll().unwrap(), vec!["[3] [dll] after truncate"]);
    }

    #[test]
    fn live_tail_starts_at_eof_and_waits_for_creation() {
        let dir = TestDir::new();
        let path = dir.0.join("late.log");
        let mut tail = LogTail::open(&path, false).unwrap();
        assert!(tail.poll().unwrap().is_empty());
        fs::write(&path, "first written after startup\n").unwrap();
        assert_eq!(tail.poll().unwrap(), vec!["first written after startup"]);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "appended").unwrap();
        assert_eq!(tail.poll().unwrap(), vec!["appended"]);
    }

    #[test]
    fn command_writer_uses_append_only_newline_protocol() {
        let dir = TestDir::new();
        let path = dir.0.join("hromatite-cmd.txt");
        fs::write(&path, "already\n").unwrap();
        let writer = CommandWriter::new(path.clone());
        writer.send("pos").unwrap();
        writer.send_batch(["who", "late"]).unwrap();
        assert_eq!(
            fs::read_to_string(path).unwrap(),
            "already\npos\nwho\nlate\n"
        );
        assert!(writer.send("pos\nquit").is_err());
        assert!(writer.send(" \r\n").is_err());
    }
}
