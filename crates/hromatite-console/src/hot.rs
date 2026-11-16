//! Parser for the resident runtime's hot-reload state records.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotState {
    Idle,
    Loading,
    Draining,
    Switching,
    Active,
    RollingBack,
    Retiring,
    Error,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotStatus {
    pub state: HotState,
    pub state_name: String,
    pub generation: u64,
    pub previous: Option<u64>,
    pub pending: Option<u64>,
    pub sha: Option<String>,
    pub flight: u64,
    pub error: Option<String>,
}

pub fn parse_hot_status(input: &str) -> Option<HotStatus> {
    let rest = if let Some(rest) = input.strip_prefix("[hot] ") {
        rest
    } else {
        let timestamped = input.strip_prefix('[')?;
        let (timestamp, tagged) = timestamped.split_once(']')?;
        timestamp.parse::<u64>().ok()?;
        tagged.trim_start().strip_prefix("[hot] ")?
    };
    let (fields, error) = rest.split_once(" error=")?;
    let error = match error {
        "none" => None,
        "" => return None,
        value => Some(value.to_string()),
    };
    let mut tokens = fields.split_whitespace();
    let state_name = keyed(&mut tokens, "state=")?.to_string();
    if state_name.is_empty()
        || !state_name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
    {
        return None;
    }
    let state = match state_name.as_str() {
        "idle" | "empty" | "ready" => HotState::Idle,
        "loading" | "reloading" => HotState::Loading,
        "draining" => HotState::Draining,
        "switching" => HotState::Switching,
        "active" => HotState::Active,
        "rolling_back" | "rollback" => HotState::RollingBack,
        "retiring" => HotState::Retiring,
        "error" | "failed" => HotState::Error,
        _ => HotState::Other,
    };
    let generation = keyed(&mut tokens, "generation=")?.parse().ok()?;
    let previous = optional_u64(keyed(&mut tokens, "previous=")?)?;
    let pending = optional_u64(keyed(&mut tokens, "pending=")?)?;
    let sha = match keyed(&mut tokens, "sha=")? {
        "none" => None,
        value if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
            Some(value.to_string())
        }
        _ => return None,
    };
    let flight = keyed(&mut tokens, "flight=")?.parse().ok()?;
    if tokens.next().is_some() {
        return None;
    }
    Some(HotStatus {
        state,
        state_name,
        generation,
        previous,
        pending,
        sha,
        flight,
        error,
    })
}

fn keyed<'a>(tokens: &mut impl Iterator<Item = &'a str>, prefix: &str) -> Option<&'a str> {
    tokens.next()?.strip_prefix(prefix)
}

fn optional_u64(value: &str) -> Option<Option<u64>> {
    match value {
        "none" => Some(None),
        value => value.parse().ok().map(Some),
    }
}
