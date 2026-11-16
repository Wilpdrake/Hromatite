//! Binary capture of client→server anti-cheat frames.
//!
//! Record layout (little endian, identical to the shim's `eacshim_out.bin`):
//!
//! ```text
//! u32 len        payload length in bytes
//! u32 dt_ms      milliseconds since the previous frame (0 for the first)
//! u8  data[len]  the frame as the module handed it to the game callback
//! ```
//!
//! The file rotates at 32 MiB keeping the active file plus one archive
//! (`*.1`), so a long session cannot fill the prefix. Reading tolerates a
//! truncated tail: a crash mid-write loses at most the last frame, and replay
//! simply stops at the last complete record.
//!
//! This module is pure `std`, so the format is unit-tested on the host.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Rotation threshold: 32 MiB (the project-wide log budget).
pub const DEFAULT_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// One captured frame plus its original inter-arrival time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// Milliseconds since the previous captured frame.
    pub dt_ms: u32,
    /// Raw frame bytes.
    pub data: Vec<u8>,
}

/// Append-only capture writer with rotation.
#[derive(Debug)]
pub struct CaptureFile {
    path: PathBuf,
    file: File,
    bytes: u64,
    max_bytes: u64,
}

impl CaptureFile {
    /// Open (or create) `path` for appending, starting at its current size.
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            let _ = fs::create_dir_all(parent);
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        let bytes = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Ok(Self {
            path,
            file,
            bytes,
            max_bytes: DEFAULT_MAX_BYTES,
        })
    }

    /// Path of the active capture file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Bytes written to the active file.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// Append one frame; rotates first when the record would cross the cap.
    pub fn append(&mut self, dt_ms: u32, data: &[u8]) -> io::Result<()> {
        let needed = 8 + data.len() as u64;
        if self.bytes > 0 && self.bytes + needed > self.max_bytes {
            self.rotate();
        }
        let mut header = [0u8; 8];
        header[..4].copy_from_slice(&(data.len() as u32).to_le_bytes());
        header[4..].copy_from_slice(&dt_ms.to_le_bytes());
        self.file.write_all(&header)?;
        self.file.write_all(data)?;
        self.bytes += needed;
        Ok(())
    }

    /// Rename the active file to `<path>.1` (replacing an older archive) and
    /// start a fresh one. Failure degrades to appending to the old file.
    fn rotate(&mut self) {
        let archive = self.path.with_extension(format!(
            "{}.1",
            self.path
                .extension()
                .and_then(|ext| ext.to_str())
                .unwrap_or("")
        ));
        if fs::rename(&self.path, &archive).is_ok()
            && let Ok(file) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
        {
            self.file = file;
            self.bytes = 0;
        }
    }
}

/// Read every complete frame from a capture file.
///
/// A partial header or a short payload stops the scan without an error: that is
/// exactly the state a crash leaves behind.
pub fn read_frames(path: impl AsRef<Path>) -> io::Result<Vec<Frame>> {
    let mut file = File::open(path)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(parse(&bytes))
}

/// Parse complete records out of `bytes` (public for tests and replay).
pub fn parse(bytes: &[u8]) -> Vec<Frame> {
    let mut frames = Vec::new();
    let mut at = 0usize;
    while at + 8 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes")) as usize;
        let dt_ms = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().expect("4 bytes"));
        let start = at + 8;
        let Some(end) = start.checked_add(len).filter(|end| *end <= bytes.len()) else {
            break;
        };
        frames.push(Frame {
            dt_ms,
            data: bytes[start..end].to_vec(),
        });
        at = end;
    }
    frames
}

/// Render the first `limit` bytes as lowercase hex (the hook log's `hex=`).
pub fn hex_prefix(data: &[u8], limit: usize) -> String {
    let take = data.len().min(limit);
    let mut out = String::with_capacity(take * 2);
    for byte in &data[..take] {
        out.push_str(&format!("{byte:02x}"));
    }
    if take == 0 { "-".to_string() } else { out }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hromatite-eac-capture-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn frames_round_trip_through_a_length_prefixed_file() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("out.bin");
        {
            let mut capture = CaptureFile::open(&path).expect("open");
            capture.append(0, &[0xAA; 24]).expect("append");
            capture.append(5_000, &[0xBB; 144]).expect("append");
            capture.append(5_010, &[0xCC; 512]).expect("append");
        }
        let frames = read_frames(&path).expect("read");
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].dt_ms, 0);
        assert_eq!(frames[0].data.len(), 24);
        assert_eq!(frames[1].dt_ms, 5_000);
        assert_eq!(frames[1].data.len(), 144);
        assert_eq!(frames[2].data.len(), 512);
        assert_eq!(&frames[2].data[..4], &[0xCC; 4]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_tail_is_ignored() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&7u32.to_le_bytes());
        bytes.extend_from_slice(&[1, 2, 3]);
        bytes.extend_from_slice(&9u32.to_le_bytes()); // header only
        let frames = parse(&bytes);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].dt_ms, 7);
        assert_eq!(frames[0].data, vec![1, 2, 3]);
    }

    #[test]
    fn rotation_keeps_one_archive_and_a_fresh_active_file() {
        let dir = temp_dir("rotate");
        let path = dir.join("out.bin");
        let mut capture = CaptureFile::open(&path).expect("open");
        capture.max_bytes = 32;
        for _ in 0..4 {
            capture.append(1_000, &[0x11; 16]).expect("append");
        }
        assert!(capture.bytes() <= 32);
        let archived = fs::read_dir(&dir)
            .expect("dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.path() != path)
            .count();
        assert_eq!(archived, 1, "exactly one rotated archive");
        assert!(!read_frames(&path).expect("read").is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hex_prefix_bounds_and_zero_length() {
        assert_eq!(hex_prefix(&[0xDE, 0xAD, 0xBE, 0xEF], 2), "dead");
        assert_eq!(hex_prefix(&[0xDE, 0xAD], 8), "dead");
        assert_eq!(hex_prefix(&[], 8), "-");
    }
}
