#![forbid(unsafe_code)]

pub mod config;
pub mod demo;
pub mod fileio;
pub mod input;
pub mod model;
pub mod parser;
pub mod render;

#[cfg(test)]
mod tests {
    use crate::parser::{LineKind, LogLine, parse_line};

    #[test]
    fn parses_stamped_tagged_lines_and_crlf() {
        let line = parse_line("[1777777777] [late] hits=42 hz=90.0 tid=0x77\r\n");
        assert_eq!(line.timestamp, Some(1_777_777_777));
        assert_eq!(line.tag, "late");
        assert_eq!(line.message, "hits=42 hz=90.0 tid=0x77");
        assert_eq!(line.kind, LineKind::Late);
        assert!(!line.raw.ends_with(['\r', '\n']));
    }

    #[test]
    fn legacy_and_error_lines_remain_visible() {
        let legacy = LogLine::new("old unframed line\n");
        assert_eq!(legacy.timestamp, None);
        assert_eq!(legacy.tag, "raw");
        assert_eq!(legacy.message, "old unframed line");
        let error = parse_line("[9] [gct] Fatal error in GC: SuspendThread loop failed");
        assert_eq!(error.kind, LineKind::Error);
        assert_eq!(error.timestamp, Some(9));
    }
}
