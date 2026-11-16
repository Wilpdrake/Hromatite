//! JSONL event log with hard rotation, field names matching `eventlog.py`.
//!
//! One record per line, flushed immediately so a crash still leaves the last
//! heartbeat on disk:
//!
//! ```text
//! {"ts_ms":1763...,"ev":"session_begin","epoch":1,"sid":"0x..."}
//! {"ts_ms":1763...,"ev":"heartbeat","epoch":1,"sid":"0x...","seq":1,
//!  "hb_in_epoch":1,"dt_ms":5000,"frame_len":144}
//! {"ts_ms":1763...,"ev":"gap","epoch":1,"dt_ms":3120,"expected_ms":5000,
//!  "hb_in_epoch":3,"seq":3}
//! {"ts_ms":1763...,"ev":"session_end","epoch":1,"heartbeats":12,"reason":"close()"}
//! {"ts_ms":1763...,"ev":"game","epoch":1,"line":"Entering Room: ..."}
//! ```
//!
//! Absent fields are omitted (never written as `null`), exactly like the Python
//! writer — the first heartbeat of an epoch therefore has no `dt_ms`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use crate::clock::now_ms;

/// Rotation threshold used by the payload (32 MiB).
pub const DEFAULT_MAX_BYTES: u64 = 32 * 1024 * 1024;
/// Files kept on disk: the active log plus one archive.
pub const DEFAULT_KEEP: usize = 2;

/// A typed JSON field value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Num(u64),
    Str(String),
}

impl Value {
    fn render(&self) -> String {
        match self {
            Self::Num(number) => number.to_string(),
            Self::Str(text) => quote(text),
        }
    }
}

/// One log record under construction. Field order is insertion order.
#[derive(Clone, Debug)]
pub struct Event {
    pub ts_ms: u64,
    pub ev: &'static str,
    fields: Vec<(&'static str, Value)>,
}

impl Event {
    /// Record stamped with the current wall clock.
    pub fn new(ev: &'static str) -> Self {
        Self::at(now_ms(), ev)
    }

    /// Record with an explicit timestamp (tests, replayed lines).
    pub fn at(ts_ms: u64, ev: &'static str) -> Self {
        Self {
            ts_ms,
            ev,
            fields: Vec::new(),
        }
    }

    /// Add a numeric field.
    pub fn num(mut self, key: &'static str, value: u64) -> Self {
        self.fields.push((key, Value::Num(value)));
        self
    }

    /// Add a numeric field only when a value exists.
    pub fn opt_num(self, key: &'static str, value: Option<u64>) -> Self {
        match value {
            Some(value) => self.num(key, value),
            None => self,
        }
    }

    /// Add a string field.
    pub fn text(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.fields.push((key, Value::Str(value.into())));
        self
    }

    /// Serialize to one compact JSON object (no trailing newline).
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(96);
        out.push_str("{\"ts_ms\":");
        out.push_str(&self.ts_ms.to_string());
        out.push_str(",\"ev\":");
        out.push_str(&quote(self.ev));
        for (key, value) in &self.fields {
            out.push(',');
            out.push_str(&quote(key));
            out.push(':');
            out.push_str(&value.render());
        }
        out.push('}');
        out
    }
}

fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Append-only JSONL writer with size-based rotation.
pub struct JsonlLog {
    path: PathBuf,
    file: File,
    written: u64,
    max_bytes: u64,
    keep: usize,
}

impl JsonlLog {
    /// Open (or create) `path` with the payload defaults: 32 MiB, 2 files.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Self::with_limits(path, DEFAULT_MAX_BYTES, DEFAULT_KEEP)
    }

    /// Open (or create) `path`, rotating at `max_bytes` and keeping `keep` files.
    pub fn with_limits(path: impl AsRef<Path>, max_bytes: u64, keep: usize) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let file = open_append(&path)?;
        let mut log = Self {
            path,
            file,
            written: 0,
            max_bytes: max_bytes.max(1),
            keep: keep.max(1),
        };
        log.written = log.file.metadata().map(|meta| meta.len()).unwrap_or(0);
        if log.written >= log.max_bytes {
            log.rotate()?;
        }
        Ok(log)
    }

    /// Path of the active file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes written to the active file since it was opened.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Append one record and flush it.
    pub fn write(&mut self, event: &Event) -> io::Result<()> {
        self.write_line(&event.to_json())
    }

    /// Append a raw line (newline added).
    pub fn write_line(&mut self, line: &str) -> io::Result<()> {
        let bytes = line.len() as u64 + 1;
        if self.written > 0 && self.written.saturating_add(bytes) > self.max_bytes {
            self.rotate()?;
        }
        self.file.write_all(line.as_bytes())?;
        self.file.write_all(b"\n")?;
        self.file.flush()?;
        self.written = self.written.saturating_add(bytes);
        Ok(())
    }

    /// Rotate: `path` becomes `path.1`, older archives shift, `path` is recreated.
    fn rotate(&mut self) -> io::Result<()> {
        if self.keep > 1 {
            for index in (1..self.keep).rev() {
                let from = if index == 1 {
                    self.path.clone()
                } else {
                    archive_path(&self.path, index - 1)
                };
                let to = archive_path(&self.path, index);
                if to.exists() {
                    fs::remove_file(&to)?;
                }
                if from.exists() {
                    fs::rename(&from, &to)?;
                }
            }
        } else if self.path.exists() {
            fs::remove_file(&self.path)?;
        }
        self.file = open_append(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

fn archive_path(path: &Path, index: usize) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{index}"));
    PathBuf::from(name)
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hromatite-eac-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn records_are_one_compact_line_with_omitted_optional_fields() {
        let event = Event::at(1_700_000_000_000, "heartbeat")
            .num("epoch", 1)
            .text("sid", "0x0000000000000001")
            .num("seq", 1)
            .num("hb_in_epoch", 1)
            .opt_num("dt_ms", None)
            .num("frame_len", 144);
        assert_eq!(
            event.to_json(),
            "{\"ts_ms\":1700000000000,\"ev\":\"heartbeat\",\"epoch\":1,\
             \"sid\":\"0x0000000000000001\",\"seq\":1,\"hb_in_epoch\":1,\"frame_len\":144}"
        );
        assert!(!event.to_json().contains('\n'));
    }

    #[test]
    fn strings_are_escaped_so_a_game_line_stays_one_record() {
        let event = Event::at(5, "game").text("line", "a \"quoted\" \\ path\nline2");
        let json = event.to_json();
        assert!(json.contains("\\\"quoted\\\""), "{json}");
        assert!(json.contains("\\\\ path\\nline2"), "{json}");
        assert_eq!(json.matches('\n').count(), 0);
    }

    #[test]
    fn the_active_log_rotates_and_keeps_exactly_two_files() {
        let dir = temp_dir("rotate");
        let path = dir.join("eac.jsonl");
        let mut log = JsonlLog::with_limits(&path, 64, 2).expect("open");
        for index in 0..12u64 {
            log.write(&Event::at(index, "heartbeat").num("seq", index))
                .expect("write");
        }
        assert!(path.exists());
        assert!(archive_path(&path, 1).exists());
        assert!(!archive_path(&path, 2).exists());
        let active = fs::read_to_string(&path).expect("active");
        let archived = fs::read_to_string(archive_path(&path, 1)).expect("archive");
        assert!(active.len() <= 64, "active log exceeded the rotation limit");
        assert!(archived.len() <= 64, "archive exceeded the rotation limit");
        assert!(active.contains("\"ev\":\"heartbeat\""));
        fs::remove_dir_all(dir).ok();
    }
}
