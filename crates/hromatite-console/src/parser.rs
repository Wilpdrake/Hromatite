//! Parser for the payload's line-oriented log format.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    Info,
    Error,
    Gc,
    Late,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub raw: String,
    pub timestamp: Option<u64>,
    pub tag: String,
    pub message: String,
    pub kind: LineKind,
}

impl LogLine {
    pub fn new(input: &str) -> Self {
        let raw = input.trim_end_matches(['\r', '\n']).to_string();
        let lower = raw.to_ascii_lowercase();
        let kind = if contains_error(&lower) {
            LineKind::Error
        } else {
            LineKind::Info
        };
        Self {
            tag: "raw".to_string(),
            message: raw.clone(),
            raw,
            timestamp: None,
            kind,
        }
    }
}

pub fn parse_line(input: &str) -> LogLine {
    let raw = input.trim_end_matches(['\r', '\n']).to_string();
    let mut base = LogLine::new(input);
    let Some(rest) = raw.strip_prefix('[') else {
        return base;
    };
    let Some(ts_end) = rest.find(']') else {
        return base;
    };
    let Ok(timestamp) = rest[..ts_end].parse::<u64>() else {
        return base;
    };
    let after_ts = rest[ts_end + 1..].trim_start();
    if !after_ts.starts_with('[') {
        base.timestamp = Some(timestamp);
        base.message = after_ts.to_string();
        return base;
    }
    let Some(tag_end) = after_ts.find(']') else {
        return base;
    };
    let tag = after_ts[1..tag_end].to_ascii_lowercase();
    let message = after_ts[tag_end + 1..].trim_start().to_string();
    let lower = format!("{} {}", tag, message).to_ascii_lowercase();
    let kind = if contains_error(&lower) {
        LineKind::Error
    } else if tag == "gct" || lower.contains("fatal error in gc") {
        LineKind::Gc
    } else if tag == "late" || message.contains("LateUpdate") {
        LineKind::Late
    } else {
        LineKind::Info
    };
    base.timestamp = Some(timestamp);
    base.tag = tag;
    base.message = message;
    base.kind = kind;
    base
}

fn contains_error(lower: &str) -> bool {
    [
        "error",
        "failed",
        "fatal",
        "panic",
        "unresolved",
        "not found",
        "faulted",
        "access_violation",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}
