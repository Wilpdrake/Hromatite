//! Offline memory dump of the il2cpp class table (`EAC_M3_DUMP=<dir>`).
//!
//! When the game updates, [`crate::m3`] stops finding the right array and the
//! only symptom is a log line full of one-character "class names" (`n`, `>`,
//! `ISE`, `|#`). Guessing the new layout from that is hopeless, so this module
//! writes the *raw bytes* of every plausible table into a directory and a host
//! side script (`~/EAC-RE/tools/m3dump_analyze.py`) turns them into facts:
//!
//! ```text
//! candidates.txt      every candidate table: address, score, first 32 entries
//!                     with the first 0x120 bytes of each pointed record
//! classes_<addr>.bin  self-describing container: 64 KiB window around the
//!                     candidate + the raw 0x120-byte records it points at
//! strings.txt         every printable run (4..=64 B) found in a record or
//!                     reachable from one qword of a record, with its offset
//! layout_probe.txt    first 16 records, every 8-byte slot: is the value a
//!                     readable pointer, and if so which ASCII string is at it
//! ```
//!
//! Nothing here runs unless `EAC_M3_DUMP` is set, and it runs at most once per
//! session (one pass costs a module scan plus ≤ 8 × 64 KiB of reads). It is
//! called from the already-established game tick, so it must stay quick.
//!
//! Container format of `classes_<addr>.bin` (parsed by the analyzer):
//!
//! ```text
//! M3DUMP1\n
//! table=0x...\n
//! span=0x... len=0x...\n
//! records=<n> rec_len=0x120\n
//! --\n
//! <span bytes>          64 KiB window as read from the process
//! n × (<addr: u64 LE> + <rec_len bytes>)
//! ```

#![cfg_attr(not(windows), allow(dead_code))]

use core::sync::atomic::{AtomicU8, Ordering};

/// Log tag for every line of this module (`[m3dump] …`).
pub const TAG: &str = "m3dump";
/// Environment variable holding the output directory.
pub const ENV_DIR: &str = "EAC_M3_DUMP";

/// At most this many candidate tables are dumped.
const MAX_CANDIDATES: usize = 8;
/// Entries listed per candidate in `candidates.txt`.
const MAX_ENTRIES: usize = 32;
/// Bytes of every record dumped (one `Il2CppClass` head).
const RECORD_LEN: usize = 0x120;
/// Records embedded in a container (enough to measure offsets statistically).
const EMBEDDED_RECORDS: usize = 64;
/// Raw window around a candidate table.
const WINDOW: usize = 0x1_0000;
/// Records probed slot by slot in `layout_probe.txt`.
const PROBE_RECORDS: usize = 16;
/// Shortest / longest printable run worth reporting.
const STRING_MIN: usize = 4;
const STRING_MAX: usize = 64;
/// Coarse scan stride and the number of entries scored per candidate.
const COARSE_STRIDE: usize = 0x1_0000;
const SCORE_SPAN: usize = 32;
/// A coarse slot is only considered when this many of its entries look real.
const SCORE_MIN_VALID: usize = 8;

/// One candidate class table and the score that put it on the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub table: usize,
    /// Entries whose `+name_off` qword points at a printable non-empty string.
    pub valid: usize,
    /// Entries strictly larger than their predecessor (il2cpp is monotonic).
    pub increasing: usize,
    /// Entries that are readable pointers at all.
    pub filled: usize,
}

/// Serialise `classes_<addr>.bin` (format documented in the module docs).
///
/// Pure and host-testable on purpose: the analyzer depends on this exact byte
/// layout, so a mistake here would only ever be noticed after a live run.
pub fn container(
    table: usize,
    span: usize,
    span_bytes: &[u8],
    records: &[(usize, Vec<u8>)],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(span_bytes.len() + records.len() * (8 + RECORD_LEN) + 96);
    out.extend_from_slice(
        format!(
            "M3DUMP1\ntable={table:#x}\nspan={span:#x} len={:#x}\nrecords={} rec_len={RECORD_LEN:#x}\n--\n",
            span_bytes.len(),
            records.len()
        )
        .as_bytes(),
    );
    out.extend_from_slice(span_bytes);
    for (address, bytes) in records {
        out.extend_from_slice(&(*address as u64).to_le_bytes());
        out.extend_from_slice(bytes);
        // Keep the layout fixed even if a record ran into an unreadable page.
        out.resize(out.len() + RECORD_LEN - bytes.len(), 0);
    }
    out
}

impl Candidate {
    fn score(&self) -> usize {
        self.valid + self.increasing
    }
}

/// Dump once, if `EAC_M3_DUMP` says where. Cheap when disabled: one atomic swap.
pub fn tick() {
    static STATE: AtomicU8 = AtomicU8::new(0);
    if STATE.swap(1, Ordering::Relaxed) != 0 {
        return;
    }
    let Some(dir) = std::env::var(ENV_DIR).ok().filter(|d| !d.trim().is_empty()) else {
        return;
    };
    #[cfg(windows)]
    dump(&dir);
    #[cfg(not(windows))]
    let _ = dir;
}

/// Space separated lowercase hex, capped at `max` bytes.
pub fn hex_bytes(bytes: &[u8], max: usize) -> String {
    let mut out = String::with_capacity(max.min(bytes.len()) * 3);
    for byte in bytes.iter().take(max) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Printable ASCII runs of `min..=max` bytes as `(offset, text)`.
///
/// A run longer than `max` is reported truncated at `max`: an over-long
/// "string" is usually several fields glued together, not a name.
pub fn ascii_runs(bytes: &[u8], min: usize, max: usize) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut run = String::new();
    for (index, byte) in bytes.iter().copied().enumerate() {
        if (0x20..0x7f).contains(&byte) {
            if run.is_empty() {
                start = index;
            }
            if run.len() < max {
                run.push(byte as char);
            }
            continue;
        }
        if run.len() >= min {
            out.push((start, core::mem::take(&mut run)));
        } else {
            run.clear();
        }
    }
    if run.len() >= min {
        out.push((start, run));
    }
    out
}

/// Does this text look like a managed identifier (`PhotonPeer`, `get_Count`,
/// ``Dictionary`2``)? Used to tell "a string is here" from "a *name* is here".
pub fn identifier_like(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= STRING_MAX
        && !text.contains(' ')
        && text
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
}

#[cfg(windows)]
mod win {
    use std::path::{Path, PathBuf};

    use super::{
        COARSE_STRIDE, Candidate, EMBEDDED_RECORDS, MAX_CANDIDATES, MAX_ENTRIES, PROBE_RECORDS,
        RECORD_LEN, SCORE_MIN_VALID, SCORE_SPAN, STRING_MAX, STRING_MIN, TAG, WINDOW, ascii_runs,
        container, hex_bytes, identifier_like,
    };
    use crate::{log, m3, procs, safe};

    /// Score `span` entries starting at `table`.
    pub(super) fn score(table: usize, span: usize) -> Candidate {
        let mut candidate = Candidate {
            table,
            valid: 0,
            increasing: 0,
            filled: 0,
        };
        let name_offset = m3::layout().map(|layout| layout.class_name as usize);
        let mut previous = 0usize;
        for index in 0..span {
            let Some(entry) = safe::read_usize(table + index * 8) else {
                break;
            };
            if entry > previous {
                candidate.increasing += 1;
            }
            previous = entry;
            if entry >= 0x1_0000 && safe::readable_at(entry) {
                candidate.filled += 1;
                // The `name` slot is per build: without a descriptor there is
                // nothing to probe for, so the candidate is only scored on shape.
                if let Some(name_offset) = name_offset
                    && let Some(name) = safe::read_usize(entry + name_offset)
                    && name >= 0x1_0000
                    && safe::ascii_at(name, 128).is_some_and(|text| !text.is_empty())
                {
                    candidate.valid += 1;
                }
            }
        }
        candidate
    }

    fn push(out: &mut Vec<Candidate>, candidate: Candidate) {
        if candidate.table == 0 || out.iter().any(|c| c.table == candidate.table) {
            return;
        }
        out.push(candidate);
    }

    /// Candidate tables: the recorded RVA (either shape), whatever [`m3`]
    /// resolved, and the best coarse-scan slots, best first.
    pub(super) fn candidates(base: usize, size: usize) -> Vec<Candidate> {
        let mut out: Vec<Candidate> = Vec::new();

        // `s_TypeInfoDefinitionTable` slot of the loaded build, when known.
        if let Some(rva) = m3::anchors().map(|anchors| anchors.class_table as usize)
            && rva != 0
        {
            let legacy = base.saturating_add(rva);
            if let Some(slot) = safe::read_usize(legacy)
                && slot >= 0x1_0000
            {
                push(&mut out, score(slot, SCORE_SPAN));
            }
            push(&mut out, score(legacy, SCORE_SPAN));
        }
        if let Some(table) = m3::class_table() {
            push(&mut out, score(table, SCORE_SPAN));
        }

        // Coarse sweep of the image, then a fine pass inside each winning
        // 64 KiB window: the same shape as `m3::locate_table`, but keeping the
        // runners-up so the dump can show *why* the winner won.
        let end = base.saturating_add(size).saturating_sub(8);
        let mut coarse = Vec::new();
        let mut at = base;
        while at < end {
            let candidate = score(at, SCORE_SPAN);
            if candidate.valid >= SCORE_MIN_VALID {
                coarse.push(candidate);
            }
            at += COARSE_STRIDE;
        }
        coarse.sort_by_key(|c| core::cmp::Reverse(c.score()));
        for window in coarse.into_iter().take(MAX_CANDIDATES) {
            let from = window.table.saturating_sub(COARSE_STRIDE);
            let to = window.table.saturating_add(COARSE_STRIDE);
            let mut best = window;
            let mut fine = from & !7;
            while fine < to {
                let candidate = score(fine, SCORE_SPAN);
                if candidate.score() >= best.score() {
                    best = candidate;
                }
                fine += 8;
            }
            push(&mut out, best);
        }

        out.sort_by_key(|c| core::cmp::Reverse(c.score()));
        out.truncate(MAX_CANDIDATES);
        out
    }

    fn write(path: &Path, data: &[u8]) {
        match std::fs::write(path, data) {
            Ok(()) => log::log_tag(TAG, &format!("wrote {} ({} B)", path.display(), data.len())),
            Err(err) => log::log_tag(TAG, &format!("write {} failed: {err}", path.display())),
        }
    }

    fn text(path: &Path, body: String) {
        write(path, body.as_bytes());
    }

    /// Entry pointers of the table, in index order, only readable ones.
    fn entries(table: usize, count: usize) -> Vec<(usize, usize)> {
        (0..count)
            .filter_map(|index| {
                let slot = table + index * 8;
                let entry = safe::read_usize(slot)?;
                (entry >= 0x1_0000 && safe::readable_at(entry)).then_some((slot, entry))
            })
            .collect()
    }

    fn string_at(pointer: usize) -> Option<String> {
        if pointer < 0x1_0000 || !safe::readable_at(pointer) {
            return None;
        }
        safe::ascii_at(pointer, STRING_MAX).filter(|text| text.len() >= 2)
    }

    #[cfg(windows)]
    pub(super) fn dump(dir: &str) {
        let Some((base, size)) = procs::own_module(crate::GAME_ASSEMBLY) else {
            log::log_tag(TAG, "own_module(GameAssembly) failed, nothing dumped");
            return;
        };
        let dir = PathBuf::from(dir);
        if let Err(err) = std::fs::create_dir_all(&dir) {
            log::log_tag(
                TAG,
                &format!("create_dir_all {} failed: {err}", dir.display()),
            );
            return;
        }

        let candidates = candidates(base, size);
        log::log_tag(
            TAG,
            &format!(
                "base={base:#x} size={size:#x} candidates={} dir={}",
                candidates.len(),
                dir.display()
            ),
        );

        let mut cand_lines = format!(
            "# m3dump v1  GameAssembly base={base:#x} size={size:#x}\n\
             # candidate <table> score=<valid+increasing> valid=<n> increasing=<n> filled=<n>\n\
             # rec lines: <record address> then the first {RECORD_LEN:#x} bytes\n"
        );
        let mut string_lines = String::from("# m3dump v1 strings (offset is inside the record)\n");
        let mut layout_lines =
            format!("# m3dump v1 layout probe (first {PROBE_RECORDS} records, step 8)\n");

        for candidate in &candidates {
            let table = candidate.table;
            let span_from = table.saturating_sub(WINDOW / 2);
            let span_bytes = safe::read_upto(span_from, WINDOW);
            let pointers = entries(table, EMBEDDED_RECORDS);
            let records: Vec<(usize, Vec<u8>)> = pointers
                .iter()
                .map(|(_, entry)| (*entry, safe::read_upto(*entry, RECORD_LEN)))
                .collect();

            cand_lines.push_str(&format!(
                "candidate={table:#x} score={} valid={} increasing={} filled={}\n",
                candidate.score(),
                candidate.valid,
                candidate.increasing,
                candidate.filled
            ));
            for index in 0..MAX_ENTRIES {
                let slot = table + index * 8;
                match safe::read_usize(slot) {
                    Some(entry) if entry >= 0x1_0000 => {
                        let bytes = safe::read_upto(entry, RECORD_LEN);
                        cand_lines.push_str(&format!(
                            "entry[{index}] slot={slot:#x} rec={entry:#x} {}\n",
                            hex_bytes(&bytes, RECORD_LEN)
                        ));
                    }
                    Some(entry) => cand_lines.push_str(&format!(
                        "entry[{index}] slot={slot:#x} rec={entry:#x} null\n"
                    )),
                    None => break,
                }
            }

            for (index, (entry, bytes)) in records.iter().enumerate() {
                let entry = *entry;
                string_lines.push_str(&format!("cand={table:#x} rec[{index}]={entry:#x}\n"));
                // Strings written inline into the record (a name inside the
                // record instead of behind a pointer) and strings behind each
                // qword, with the slot that reached them.
                for (offset, text) in ascii_runs(bytes, STRING_MIN, STRING_MAX) {
                    string_lines.push_str(&format!("  inline +{offset:#05x} {text:?}\n"));
                }
                for slot in 0..RECORD_LEN / 8 {
                    let offset = slot * 8;
                    let Some(value) = safe::read_usize(entry + offset) else {
                        continue;
                    };
                    let Some(text) = string_at(value) else {
                        continue;
                    };
                    string_lines
                        .push_str(&format!("  ptr +{offset:#05x} value={value:#x} {text:?}\n"));
                }
            }

            for (index, (entry, _)) in records.iter().take(PROBE_RECORDS).enumerate() {
                let entry = *entry;
                layout_lines.push_str(&format!("cand={table:#x} rec[{index}]={entry:#x}\n"));
                for slot in 0..RECORD_LEN / 8 {
                    let offset = slot * 8;
                    let Some(value) = safe::read_usize(entry + offset) else {
                        continue;
                    };
                    let readable = value >= 0x1_0000 && safe::readable_at(value);
                    let text = readable.then(|| string_at(value)).flatten();
                    match text {
                        Some(text) if identifier_like(&text) => layout_lines.push_str(&format!(
                            "  +{offset:#05x} {value:#014x} ptr=yes text={text:?}\n"
                        )),
                        Some(text) => layout_lines.push_str(&format!(
                            "  +{offset:#05x} {value:#014x} ptr=yes raw={text:?}\n"
                        )),
                        None => layout_lines.push_str(&format!(
                            "  +{offset:#05x} {value:#014x} ptr={}\n",
                            if readable { "yes" } else { "no" }
                        )),
                    }
                    // A slot that points at an array of `MethodInfo*` (each of
                    // which has its name at `m3::METHOD_NAME_OFF`) is worth
                    // naming out loud: that is how the offline analyzer
                    // re-finds `methods_off`.
                    if readable {
                        let names: Vec<String> = (0..3)
                            .filter_map(|i| {
                                let pointer = safe::read_usize(value + i * 8)?;
                                // `Il2CppClass.methods` → `MethodInfo.name`; the
                                // slot comes from the build descriptor.
                                let name_offset = m3::layout()?.method_name as usize;
                                string_at(pointer.checked_add(name_offset)?)
                                    .filter(|name| name.len() >= 3 && identifier_like(name))
                            })
                            .collect();
                        if names.len() >= 2 {
                            layout_lines.push_str(&format!(
                                "  methods +{offset:#05x} array={value:#x} first={names:?}\n"
                            ));
                        }
                    }
                }
            }

            let path = dir.join(format!("classes_{table:x}.bin"));
            write(&path, &container(table, span_from, &span_bytes, &records));
        }

        text(&dir.join("candidates.txt"), cand_lines);
        text(&dir.join("strings.txt"), string_lines);
        text(&dir.join("layout_probe.txt"), layout_lines);
    }
}

#[cfg(windows)]
fn dump(dir: &str) {
    win::dump(dir);
}

#[cfg(test)]
mod tests {
    use super::{RECORD_LEN, ascii_runs, container, hex_bytes, identifier_like};

    #[test]
    fn hex_is_space_separated_lowercase_and_capped() {
        assert_eq!(hex_bytes(&[0xde, 0xad, 0x00, 0xff], 16), "de ad 00 ff");
        assert_eq!(hex_bytes(&[0xde, 0xad, 0x00, 0xff], 2), "de ad");
        assert_eq!(hex_bytes(&[], 8), "");
    }

    #[test]
    fn ascii_runs_keep_offsets_and_drop_short_runs() {
        let bytes = b"\x00abc\x01PhotonPeer\x00xy\x00get_Count!";
        let runs = ascii_runs(bytes, 4, 64);
        let texts: Vec<&str> = runs.iter().map(|(_, t)| t.as_str()).collect();
        // `abc` is three bytes, below the minimum run length.
        assert_eq!(texts, vec!["PhotonPeer", "get_Count!"]);
        assert_eq!(runs[0].0, 5);
        assert_eq!(runs[1].0, 19);
    }

    #[test]
    fn long_runs_are_truncated_at_the_limit() {
        let bytes = [b'A'; 80];
        let runs = ascii_runs(&bytes, 4, 8);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].1, "AAAAAAAA");
    }

    #[test]
    fn identifier_filter_rejects_noise() {
        assert!(identifier_like("PhotonPeer"));
        assert!(identifier_like("Dictionary`2"));
        assert!(identifier_like("_private"));
        assert!(!identifier_like(""));
        assert!(!identifier_like("two words"));
        assert!(!identifier_like(">"));
        assert!(!identifier_like("|#"));
    }

    /// The Python analyzer parses this byte layout; a change here breaks it.
    #[test]
    fn container_header_matches_the_documented_format() {
        let span = vec![0xaau8; 16];
        let records = vec![(0x1000usize, vec![1u8; RECORD_LEN]), (0x2000, vec![2u8; 4])];
        let blob = container(0xdead_beef, 0x1000, &span, &records);
        let head_end = blob.windows(3).position(|w| w == b"--\n").expect("header") + 3;
        let head = core::str::from_utf8(&blob[..head_end]).expect("ascii header");
        let lines: Vec<&str> = head.lines().collect();
        assert_eq!(lines[0], "M3DUMP1");
        assert_eq!(lines[1], "table=0xdeadbeef");
        assert_eq!(lines[2], "span=0x1000 len=0x10");
        assert_eq!(lines[3], "records=2 rec_len=0x120");
        assert_eq!(lines[4], "--");
        assert_eq!(&blob[head_end..head_end + 16], &span[..]);
        let rec0 = head_end + 16;
        assert_eq!(
            u64::from_le_bytes(blob[rec0..rec0 + 8].try_into().unwrap()),
            0x1000
        );
        let rec1 = rec0 + 8 + RECORD_LEN;
        assert_eq!(
            u64::from_le_bytes(blob[rec1..rec1 + 8].try_into().unwrap()),
            0x2000
        );
        // A short record is zero padded to the fixed stride, so every record
        // after it keeps its offset.
        assert_eq!(blob[rec1 + 8 + RECORD_LEN - 1], 0);
        assert_eq!(blob.len(), rec1 + 8 + RECORD_LEN);
    }

    #[test]
    fn trailing_run_at_eof_is_reported() {
        let runs = ascii_runs(b"\x00\x00alpha", 4, 64);
        assert_eq!(runs, vec![(2, "alpha".to_string())]);
    }
}
