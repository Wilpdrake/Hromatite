//! Incremental reader of the resident's outbound capture file.
//!
//! The hooks that own the client→server wrapper live in the resident image, so
//! a feature generation cannot read their counters or their frames directly.
//! What it *can* read is the capture the resident writes
//! (`%TEMP%\hromatite_eac_capture.bin`, format `u32 len, u32 dt_ms, bytes`).
//! This tap turns that file into a stream of complete frames without ever
//! re-reading it in full:
//!
//! * `offset` is the file position already read, so a poll costs only the new
//!   bytes;
//! * `pending` keeps a trailing partial record until the rest of it lands;
//! * a shorter file than `offset` means rotation or truncation, and the tap
//!   simply restarts from zero.
//!
//! `skip_to_end` is the live-attach behaviour: start from the *next* frame
//! instead of replaying a session's whole history into the JSONL at once.

use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::capture::Frame;

/// Largest single read; a capture poll runs on the game thread.
const MAX_READ: usize = 4 * 1024 * 1024;

/// Tail-reader over one capture file.
#[derive(Clone, Debug)]
pub struct CaptureTap {
    path: PathBuf,
    offset: u64,
    pending: Vec<u8>,
    frames: u64,
}

impl CaptureTap {
    /// Start at the beginning of `path` (history is replayed on the first poll).
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            offset: 0,
            pending: Vec::new(),
            frames: 0,
        }
    }

    /// Start at the current end of the file: only frames written from now on
    /// are reported. Used by `eac.tap` when attaching to a live session.
    pub fn skip_to_end(mut self) -> Self {
        if let Ok(metadata) = std::fs::metadata(&self.path) {
            self.offset = metadata.len();
        }
        self.pending.clear();
        self
    }

    /// Path being tapped.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// File position already consumed.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// Complete frames delivered so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Bytes held back because the record is not complete yet.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Read whatever is new and return the complete records in it.
    ///
    /// A missing file is not an error (the resident opens it lazily); it simply
    /// yields nothing until it appears.
    pub fn poll(&mut self) -> io::Result<Vec<Frame>> {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let len = file.metadata()?.len();
        if len < self.offset {
            // Rotated (`*.1` plus a fresh file) or truncated: start over.
            self.offset = 0;
            self.pending.clear();
        }
        if len == self.offset {
            return Ok(Vec::new());
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let want = (len - self.offset).min(MAX_READ as u64) as usize;
        let mut buf = vec![0u8; want];
        let read = file.read(&mut buf)?;
        buf.truncate(read);
        if buf.is_empty() {
            return Ok(Vec::new());
        }
        self.offset += read as u64;
        self.pending.extend_from_slice(&buf);

        let frames = parse_complete(&self.pending);
        let consumed = frames.last().map(|(_, end)| *end).unwrap_or(0);
        if consumed > 0 {
            self.pending.drain(..consumed);
        }
        let frames: Vec<Frame> = frames.into_iter().map(|(frame, _)| frame).collect();
        self.frames = self.frames.saturating_add(frames.len() as u64);
        Ok(frames)
    }
}

/// Parse every complete record from `bytes`, returning each frame with the
/// offset just past it. A truncated tail stays unconsumed.
fn parse_complete(bytes: &[u8]) -> Vec<(Frame, usize)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 8 <= bytes.len() {
        let len = u32::from_le_bytes(match bytes[at..at + 4].try_into() {
            Ok(value) => value,
            Err(_) => break,
        }) as usize;
        let dt_ms = u32::from_le_bytes(match bytes[at + 4..at + 8].try_into() {
            Ok(value) => value,
            Err(_) => break,
        });
        let start = at + 8;
        let Some(end) = start.checked_add(len).filter(|end| *end <= bytes.len()) else {
            break;
        };
        out.push((
            Frame {
                dt_ms,
                data: bytes[start..end].to_vec(),
            },
            end,
        ));
        at = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_path(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hromatite-eac-tap-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("capture.bin")
    }

    fn record(len: usize, dt_ms: u32, fill: u8) -> Vec<u8> {
        let mut out = Vec::with_capacity(8 + len);
        out.extend_from_slice(&(len as u32).to_le_bytes());
        out.extend_from_slice(&dt_ms.to_le_bytes());
        out.extend(std::iter::repeat_n(fill, len));
        out
    }

    fn append(path: &Path, bytes: &[u8]) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open");
        file.write_all(bytes).expect("write");
        file.flush().expect("flush");
    }

    #[test]
    fn polls_only_new_records() {
        let path = temp_path("incremental");
        let mut tap = CaptureTap::new(&path);
        assert!(tap.poll().expect("empty poll").is_empty(), "missing file");

        append(&path, &record(24, 0, 0xaa));
        append(&path, &record(512, 2_007, 0xbb));
        let frames = tap.poll().expect("first poll");
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].data.len(), 24);
        assert_eq!(frames[1].data.len(), 512);
        assert_eq!(frames[1].dt_ms, 2_007);
        assert_eq!(frames[1].data[0], 0xbb);
        assert!(tap.poll().expect("second poll").is_empty(), "no duplicates");

        append(&path, &record(48, 91, 0xcc));
        let frames = tap.poll().expect("third poll");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data.len(), 48);
        assert_eq!(tap.frames(), 3);
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn a_partial_record_waits_for_the_rest() {
        let path = temp_path("partial");
        let mut tap = CaptureTap::new(&path);
        let full = record(512, 2_007, 0x11);
        append(&path, &full[..300]);
        assert!(tap.poll().expect("partial poll").is_empty());
        assert_eq!(tap.pending_len(), 300);
        append(&path, &full[300..]);
        let frames = tap.poll().expect("completed poll");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data.len(), 512);
        assert_eq!(tap.pending_len(), 0);
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn rotation_and_truncation_restart_the_tap() {
        let path = temp_path("rotate");
        let mut tap = CaptureTap::new(&path);
        append(&path, &record(512, 0, 1));
        append(&path, &record(512, 2_007, 2));
        assert_eq!(tap.poll().expect("first").len(), 2);

        // Rotation: the active file is replaced by a shorter one.
        std::fs::write(&path, record(24, 3_000, 3)).expect("rewrite");
        let frames = tap.poll().expect("after rotation");
        assert_eq!(frames.len(), 1, "the fresh file is read from zero");
        assert_eq!(frames[0].data.len(), 24);
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }

    #[test]
    fn skip_to_end_ignores_history() {
        let path = temp_path("skip");
        append(&path, &record(512, 0, 1));
        append(&path, &record(512, 2_007, 2));
        let mut tap = CaptureTap::new(&path).skip_to_end();
        assert!(tap.poll().expect("empty").is_empty());
        append(&path, &record(512, 2_010, 3));
        let frames = tap.poll().expect("after append");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].data[0], 3);
        let _ = std::fs::remove_dir_all(path.parent().expect("parent"));
    }
}
