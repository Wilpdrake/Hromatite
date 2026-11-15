//! Keyboard decoding, command history and completion over the payload's help set.

use std::collections::VecDeque;

pub const COMMANDS: &[&str] = &[
    "help",
    "?",
    "q",
    "qword",
    "b",
    "bytes",
    "s",
    "ws",
    "gct",
    "threads",
    "gctfix",
    "gctresume",
    "call",
    "wq",
    "wb",
    "guard",
    "m3f",
    "m3c",
    "m3l",
    "m3s",
    "m3i",
    "latec",
    "latehook",
    "late",
    "pos",
    "who",
    "esp",
    "panic",
    "av",
    "seal",
    "quit",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Backspace,
    Delete,
    Tab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PgUp,
    PgDn,
    F(u8),
    CtrlR,
    CtrlC,
    Esc,
    Ignore,
}

#[derive(Default)]
pub struct KeyDecoder {
    pending: Vec<u8>,
}

impl KeyDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Key> {
        self.pending.extend_from_slice(bytes);
        let mut keys = Vec::new();
        let mut at = 0;
        while at < self.pending.len() {
            let rest = &self.pending[at..];
            let (key, used) = match rest[0] {
                0x1b => match decode_escape(rest) {
                    Some(pair) => pair,
                    None => break,
                },
                b'\r' | b'\n' => (Key::Enter, 1),
                0x7f | 0x08 => (Key::Backspace, 1),
                b'\t' => (Key::Tab, 1),
                0x12 => (Key::CtrlR, 1),
                0x03 => (Key::CtrlC, 1),
                byte if byte < 0x20 => {
                    at += 1;
                    continue;
                }
                _ => {
                    let text = match std::str::from_utf8(rest) {
                        Ok(text) => text,
                        Err(error) if error.error_len().is_none() => break,
                        Err(error) => {
                            let valid = error.valid_up_to();
                            // SAFETY-free fallback: an invalid byte is input noise, not a command.
                            at += valid.max(1);
                            continue;
                        }
                    };
                    let ch = text.chars().next().expect("nonempty input");
                    (Key::Char(ch), ch.len_utf8())
                }
            };
            keys.push(key);
            at += used;
        }
        self.pending.drain(..at);
        keys
    }

    /// Resolve a lone Escape after the terminal read timeout. Longer split
    /// sequences stay pending so a slow read cannot turn an arrow into Exit.
    pub fn timeout(&mut self) -> Vec<Key> {
        if self.pending.as_slice() == b"\x1b" {
            self.pending.clear();
            vec![Key::Esc]
        } else {
            Vec::new()
        }
    }
}

/// Return `None` while an Escape, CSI or SS3 sequence may be incomplete.
fn decode_escape(bytes: &[u8]) -> Option<(Key, usize)> {
    if bytes.len() == 1 {
        return None;
    }
    if bytes[1] == b'O' {
        if bytes.len() < 3 {
            return None;
        }
        let key = match bytes[2] {
            b'A' => Key::Up,
            b'B' => Key::Down,
            b'C' => Key::Right,
            b'D' => Key::Left,
            b'H' => Key::Home,
            b'F' => Key::End,
            b'P' => Key::F(1),
            b'Q' => Key::F(2),
            b'R' => Key::F(3),
            b'S' => Key::F(4),
            _ => Key::Ignore,
        };
        return Some((key, 3));
    }
    if bytes[1] != b'[' {
        return Some((Key::Esc, 1));
    }
    // Position 1 is `[`, also in the final-byte range, so locate from position 2.
    let end = bytes[2..]
        .iter()
        .position(|b| (0x40..=0x7e).contains(b))
        .map(|p| p + 2)?;
    let body = std::str::from_utf8(&bytes[2..end]).ok()?;
    let key = match (body, bytes[end]) {
        ("", b'A') => Key::Up,
        ("", b'B') => Key::Down,
        ("", b'C') => Key::Right,
        ("", b'D') => Key::Left,
        ("", b'H') | ("1", b'~') | ("7", b'~') => Key::Home,
        ("", b'F') | ("4", b'~') | ("8", b'~') => Key::End,
        ("3", b'~') => Key::Delete,
        ("5", b'~') => Key::PgUp,
        ("6", b'~') => Key::PgDn,
        ("11", b'~') => Key::F(1),
        ("12", b'~') => Key::F(2),
        ("13", b'~') => Key::F(3),
        ("14", b'~') => Key::F(4),
        ("15", b'~') => Key::F(5),
        ("17", b'~') => Key::F(6),
        ("18", b'~') => Key::F(7),
        ("19", b'~') => Key::F(8),
        _ => Key::Ignore,
    };
    Some((key, end + 1))
}

pub struct History {
    entries: VecDeque<String>,
    cursor: Option<usize>,
    draft: String,
    capacity: usize,
}

impl Default for History {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            cursor: None,
            draft: String::new(),
            capacity: 256,
        }
    }
}

impl History {
    pub fn push(&mut self, command: &str) {
        let command = command.trim();
        if command.is_empty() {
            return;
        }
        if self.entries.back().is_none_or(|last| last != command) {
            self.entries.push_back(command.to_string());
            while self.entries.len() > self.capacity {
                self.entries.pop_front();
            }
        }
        self.cursor = None;
        self.draft.clear();
    }

    pub fn previous(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        let index = match self.cursor {
            None => {
                self.draft = current.to_string();
                self.entries.len() - 1
            }
            Some(index) => index.saturating_sub(1),
        };
        self.cursor = Some(index);
        self.entries.get(index).cloned()
    }

    pub fn next_newer(&mut self) -> Option<String> {
        let index = self.cursor?;
        if index + 1 < self.entries.len() {
            self.cursor = Some(index + 1);
            self.entries.get(index + 1).cloned()
        } else {
            self.cursor = None;
            Some(self.draft.clone())
        }
    }

    pub fn reset_navigation(&mut self) {
        self.cursor = None;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    pub text: String,
    pub matches: Vec<&'static str>,
}

pub fn complete(input: &str) -> Completion {
    let prefix = input.split_whitespace().next().unwrap_or("");
    if prefix.is_empty() || input.len() != prefix.len() {
        return Completion {
            text: input.to_string(),
            matches: Vec::new(),
        };
    }
    let matches: Vec<_> = COMMANDS
        .iter()
        .copied()
        .filter(|candidate| candidate.starts_with(prefix))
        .collect();
    let text = match matches.as_slice() {
        [] => input.to_string(),
        [only] => format!("{only} "),
        many => {
            let mut common = many[0].to_string();
            for candidate in &many[1..] {
                let bytes = common
                    .bytes()
                    .zip(candidate.bytes())
                    .take_while(|(a, b)| a == b)
                    .count();
                common.truncate(bytes);
            }
            common
        }
    };
    Completion { text, matches }
}

#[cfg(test)]
mod tests {
    use super::{COMMANDS, Completion, History, Key, KeyDecoder, complete};

    #[test]
    fn history_preserves_draft_and_deduplicates_commands() {
        let mut history = History::default();
        history.push("pos");
        history.push("who");
        history.push("pos");
        assert_eq!(history.previous("late"), Some("pos".into()));
        assert_eq!(history.previous(""), Some("who".into()));
        assert_eq!(history.previous(""), Some("pos".into()));
        assert_eq!(history.next_newer(), Some("who".into()));
        assert_eq!(history.next_newer(), Some("pos".into()));
        assert_eq!(history.next_newer(), Some("late".into()));
    }

    #[test]
    fn completion_handles_unique_and_ambiguous_prefixes() {
        let Completion { text, matches } = complete("lat");
        assert_eq!(text, "late");
        assert!(matches.contains(&"late"));
        assert!(matches.contains(&"latehook"));
        let unique = complete("wh");
        assert_eq!(unique.text, "who ");
        assert_eq!(unique.matches, vec!["who"]);
        let with_arg = complete("latehook VRC");
        assert_eq!(with_arg.text, "latehook VRC");
    }

    #[test]
    fn completion_matches_current_raw_console_help() {
        assert_eq!(
            COMMANDS,
            &[
                "help",
                "?",
                "q",
                "qword",
                "b",
                "bytes",
                "s",
                "ws",
                "gct",
                "threads",
                "gctfix",
                "gctresume",
                "call",
                "wq",
                "wb",
                "guard",
                "m3f",
                "m3c",
                "m3l",
                "m3s",
                "m3i",
                "latec",
                "latehook",
                "late",
                "pos",
                "who",
                "esp",
                "panic",
                "av",
                "seal",
                "quit",
            ]
        );
        assert_eq!(complete("m3i").text, "m3i ");
        assert!(complete("find").matches.is_empty());
    }

    #[test]
    fn decoder_handles_csi_function_keys_utf8_and_controls() {
        let mut decoder = KeyDecoder::default();
        assert_eq!(
            decoder.feed(b"\x1b[A\x1b[5~\x1bOQ\x1b[19~"),
            vec![Key::Up, Key::PgUp, Key::F(2), Key::F(8)]
        );
        assert_eq!(decoder.feed(&[18, 3]), vec![Key::CtrlR, Key::CtrlC]);
        assert_eq!(decoder.feed("й".as_bytes()), vec![Key::Char('й')]);
        assert!(decoder.feed(b"\x1b").is_empty());
    }

    #[test]
    fn decoder_distinguishes_split_arrows_from_single_escape() {
        let mut decoder = KeyDecoder::default();
        assert!(decoder.feed(b"\x1b").is_empty());
        assert_eq!(decoder.feed(b"[A"), vec![Key::Up]);
        assert!(decoder.feed(b"\x1b").is_empty());
        assert_eq!(decoder.timeout(), vec![Key::Esc]);
        assert!(decoder.timeout().is_empty());
    }

    #[test]
    fn decoder_ignores_unknown_csi_and_ss3_sequences() {
        let mut decoder = KeyDecoder::default();
        assert_eq!(
            decoder.feed(b"\x1b[99~\x1bOZ"),
            vec![Key::Ignore, Key::Ignore]
        );
        assert_eq!(decoder.feed(b"x"), vec![Key::Char('x')]);
    }

    #[test]
    fn decoder_keeps_split_utf8_and_escape_sequences() {
        let mut decoder = KeyDecoder::default();
        assert!(decoder.feed(b"\x1b[").is_empty());
        assert_eq!(decoder.feed(b"A"), vec![Key::Up]);
        assert!(decoder.feed(&[0xd0]).is_empty());
        assert_eq!(decoder.feed(&[0xb9]), vec![Key::Char('й')]);
    }
}
