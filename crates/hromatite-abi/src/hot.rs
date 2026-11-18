//! Pure generation and hot-configuration contracts, artifact/manifest parsers,
//! and hot-reload status wire records.

#![forbid(unsafe_code)]

use std::path::PathBuf;

/// Host-owned settings. Feature flags are sent over `configure` using the
/// lowercase names returned by [`HotConfig::feature_settings`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HotConfig {
    pub tick_ms: u64,
    pub grace_ms: u64,
    pub telemetry: bool,
    pub memory: bool,
    pub m3: bool,
}

impl Default for HotConfig {
    fn default() -> Self {
        Self {
            tick_ms: 250,
            grace_ms: 3_000,
            telemetry: true,
            memory: true,
            m3: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeatureSetting {
    pub key: &'static str,
    pub enabled: bool,
}

impl HotConfig {
    pub const fn feature_settings(self) -> [FeatureSetting; 3] {
        [
            FeatureSetting {
                key: "telemetry",
                enabled: self.telemetry,
            },
            FeatureSetting {
                key: "memory",
                enabled: self.memory,
            },
            FeatureSetting {
                key: "m3",
                enabled: self.m3,
            },
        ]
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigError {
    Syntax { line: usize },
    UnknownKey { line: usize },
    InvalidValue { line: usize },
    OutOfRange { line: usize },
}

/// Parse a complete `KEY=VALUE` snapshot. No caller-owned value is mutated on
/// error; unknown keys, malformed lines and out-of-range values reject all of
/// the input.
pub fn parse_config(input: &str) -> Result<HotConfig, ConfigError> {
    let mut parsed = HotConfig::default();

    for (index, raw_line) in input.lines().enumerate() {
        let line_number = index + 1;
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or(ConfigError::Syntax { line: line_number })?;
        let key = key.trim();
        let value = value.trim();

        match key {
            "TICK_MS" => {
                parsed.tick_ms = parse_bounded(value, 1, 10_000, line_number)?;
            }
            "GRACE_MS" => {
                parsed.grace_ms = parse_bounded(value, 100, 60_000, line_number)?;
            }
            "TELEMETRY" => parsed.telemetry = parse_bool(value, line_number)?,
            "MEMORY" => parsed.memory = parse_bool(value, line_number)?,
            "M3" => parsed.m3 = parse_bool(value, line_number)?,
            _ => return Err(ConfigError::UnknownKey { line: line_number }),
        }
    }

    Ok(parsed)
}

fn parse_bounded(value: &str, minimum: u64, maximum: u64, line: usize) -> Result<u64, ConfigError> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| ConfigError::InvalidValue { line })?;
    if parsed < minimum || parsed > maximum {
        return Err(ConfigError::OutOfRange { line });
    }
    Ok(parsed)
}

fn parse_bool(value: &str, line: usize) -> Result<bool, ConfigError> {
    match value.to_ascii_lowercase().as_str() {
        "0" | "false" | "off" => Ok(false),
        "1" | "true" | "on" => Ok(true),
        _ => Err(ConfigError::InvalidValue { line }),
    }
}

pub fn config_text(config: HotConfig) -> String {
    format!(
        "TICK_MS={}\nGRACE_MS={}\nTELEMETRY={}\nMEMORY={}\nM3={}\n",
        config.tick_ms,
        config.grace_ms,
        u8::from(config.telemetry),
        u8::from(config.memory),
        u8::from(config.m3),
    )
}

pub fn replace_config(
    original: HotConfig,
    key: &str,
    value: &str,
) -> Result<HotConfig, ConfigError> {
    let mut lines: Vec<String> = config_text(original).lines().map(str::to_string).collect();
    let prefix = format!("{key}=");
    if let Some(line) = lines.iter_mut().find(|line| line.starts_with(&prefix)) {
        *line = format!("{prefix}{value}");
    } else {
        lines.push(format!("{prefix}{value}"));
    }
    parse_config(&lines.join("\n"))
}

pub fn canonical_generation(value: &str) -> Option<u64> {
    if value.is_empty()
        || value.starts_with('0')
        || !value.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    value.parse::<u64>().ok()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReloadSelection {
    Latest,
    Generation(u64),
    Status,
}

pub fn parse_reload_request(line: &str) -> Result<ReloadSelection, String> {
    let mut parts = line.split_whitespace();
    if parts.next() != Some("reload") {
        return Err("usage: reload [gen [generation]] | reload status".into());
    }
    let action = parts.next().unwrap_or("latest");
    match action {
        "status" if parts.next().is_none() => Ok(ReloadSelection::Status),
        "latest" if parts.next().is_none() => Ok(ReloadSelection::Latest),
        "gen" => {
            let generation = parts
                .next()
                .ok_or("reload gen requires a positive decimal id")?;
            if parts.next().is_some() {
                return Err("usage: reload gen <positive-id>".into());
            }
            let generation = canonical_generation(generation)
                .ok_or("reload gen requires a positive canonical decimal id")?;
            Ok(ReloadSelection::Generation(generation))
        }
        "status" | "latest" => Err("usage: reload status (no generation argument)".into()),
        generation => {
            if parts.next().is_some() {
                return Err("usage: reload [gen [generation]] | reload status".into());
            }
            let generation = canonical_generation(generation)
                .ok_or("reload generation must be a positive canonical decimal integer")?;
            Ok(ReloadSelection::Generation(generation))
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VersionedName {
    pub generation: u64,
    pub short_sha: String,
}

pub fn parse_versioned_name(name: &str) -> Option<VersionedName> {
    if name.contains(['/', '\\']) {
        return None;
    }
    let rest = name.strip_prefix("features-")?.strip_suffix(".dll")?;
    let (generation, short_sha) = rest.rsplit_once('-')?;
    if generation.is_empty()
        || !generation.bytes().all(|byte| byte.is_ascii_digit())
        || short_sha.len() != 12
        || !short_sha
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    let generation = canonical_generation(generation)?;
    Some(VersionedName {
        generation,
        short_sha: short_sha.to_string(),
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeatureArtifact {
    pub generation: u64,
    pub sha256: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FeatureManifest {
    pub artifact: FeatureArtifact,
    pub game_sha256: String,
}

pub fn windows_artifact_path(host_path: &str) -> Result<PathBuf, String> {
    if !host_path.starts_with('/')
        || host_path.contains('\\')
        || host_path.split('/').any(|component| component == "..")
        || !host_path.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'_' | b'/' | b'.' | b':' | b'+' | b',' | b'=' | b'@' | b'%' | b'-'
                )
        })
    {
        return Err("manifest FILE is not a safe absolute Linux path".into());
    }
    Ok(PathBuf::from(format!(
        "Z:{}",
        host_path.replace('/', "\\")
    )))
}

pub fn parse_feature_manifest(text: &str) -> Result<FeatureManifest, String> {
    if text.len() > 4096 {
        return Err("current.manifest exceeds 4096 bytes".into());
    }
    const KEYS: [&str; 5] = ["GENERATION", "SHA256", "FILE", "GAME_SHA256", "ABI"];
    let lines: Vec<_> = text.lines().collect();
    if lines.len() != KEYS.len() {
        return Err("current.manifest must have five KEY=VALUE lines".into());
    }
    let mut values = [""; 5];
    for ((key, slot), line) in KEYS.into_iter().zip(&mut values).zip(lines) {
        let (name, value) = line
            .split_once('=')
            .ok_or_else(|| format!("malformed manifest line: {line:?}"))?;
        if name != key {
            return Err(format!("expected {key}, got {name}"));
        }
        *slot = value;
    }
    let generation = canonical_generation(values[0])
        .ok_or("manifest GENERATION must be positive and canonical")?;
    for sha in [&values[1], &values[3]] {
        if sha.len() != 64
            || !sha
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err("manifest hash must contain 64 lowercase hex characters".into());
        }
    }
    if values[4] != crate::ABI_VERSION.to_string() {
        return Err(format!("unsupported manifest ABI {}", values[4]));
    }
    let filename = values[2].rsplit('/').next().unwrap_or_default();
    let parsed = parse_versioned_name(filename)
        .ok_or("manifest FILE has no valid features-N-SHA12.dll name")?;
    if parsed.generation != generation || parsed.short_sha != values[1][..12] {
        return Err("manifest FILE differs from GENERATION/SHA256".into());
    }
    Ok(FeatureManifest {
        artifact: FeatureArtifact {
            generation,
            sha256: values[1].to_string(),
            path: windows_artifact_path(values[2])?,
        },
        game_sha256: values[3].to_string(),
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Idle,
    Stable,
    Requested,
    Draining,
    Loading,
    Canary,
    ActiveGrace,
    RollingBack,
    Failed,
}

impl Phase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Stable => "stable",
            Self::Requested => "requested",
            Self::Draining => "draining",
            Self::Loading => "loading",
            Self::Canary => "canary",
            Self::ActiveGrace => "active",
            Self::RollingBack => "rolling_back",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotState {
    Idle,
    Stable,
    Requested,
    Loading,
    Draining,
    Canary,
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

pub fn format_hot_status(
    phase: Phase,
    generation: u64,
    previous: Option<u64>,
    pending: Option<u64>,
    sha: Option<&str>,
    flight: u64,
    error: Option<&str>,
) -> String {
    let optional = |value: Option<u64>| value.map_or_else(|| "none".to_string(), |v| v.to_string());
    let sha = match sha {
        Some(value) if !value.is_empty() => value,
        _ => "none",
    };
    let error = match error {
        Some(value) if !value.is_empty() => value,
        _ => "none",
    };
    format!(
        "state={} generation={generation} previous={} pending={} sha={sha} flight={flight} error={error}",
        phase.as_str(),
        optional(previous),
        optional(pending),
    )
}

pub fn parse_hot_status(input: &str) -> Option<HotStatus> {
    let rest = if let Some(rest) = input.strip_prefix("[hot] ") {
        rest
    } else if input.starts_with("state=") {
        input
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
        "stable" => HotState::Stable,
        "requested" => HotState::Requested,
        "loading" | "reloading" => HotState::Loading,
        "draining" => HotState::Draining,
        "canary" => HotState::Canary,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GenerationRole {
    Active,
    Previous,
    Pending,
    Failed,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InFlight {
    pub active: u32,
    pub previous: u32,
    pub pending: u32,
    pub failed: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HotError {
    InvalidTransition,
    DuplicateRequest,
    RequestInProgress,
    NoPrevious,
    PreviousResident,
    CallsInFlight,
    NoCallsInFlight,
    CounterOverflow,
    UnknownGeneration,
    DeadlineOverflow,
    DeadlineExpired,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Tick {
    Unchanged,
    GraceComplete,
    RollbackComplete { active: u64 },
    PendingFailed { failed: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fault {
    RolledBack { failed: u64, active: u64 },
    Unrecoverable { failed: u64 },
}

#[derive(Clone, Copy, Debug)]
struct Generation {
    version: u64,
    in_flight: u32,
}

impl Generation {
    const fn new(version: u64) -> Self {
        Self {
            version,
            in_flight: 0,
        }
    }
}

/// Pure ownership model for one resident active generation, its rollback slot,
/// one pending load and at most one failed generation awaiting quarantine drain.
/// Transition methods route/count calls only; a successful retirement returns
/// the version whose code the runtime may then unload. Callers supply monotonic
/// milliseconds and perform actual load, switch and unload operations.
#[derive(Clone, Debug)]
pub struct HotReload {
    config: HotConfig,
    phase: Phase,
    active: Option<Generation>,
    previous: Option<Generation>,
    pending: Option<Generation>,
    failed: Option<Generation>,
    deadline_ms: Option<u64>,
}

impl HotReload {
    pub const fn new(config: HotConfig) -> Self {
        Self {
            config,
            phase: Phase::Idle,
            active: None,
            previous: None,
            pending: None,
            failed: None,
            deadline_ms: None,
        }
    }

    pub const fn phase(&self) -> Phase {
        self.phase
    }

    pub const fn active_version(&self) -> Option<u64> {
        match self.active {
            Some(generation) => Some(generation.version),
            None => None,
        }
    }

    pub const fn previous_version(&self) -> Option<u64> {
        match self.previous {
            Some(generation) => Some(generation.version),
            None => None,
        }
    }

    pub const fn pending_version(&self) -> Option<u64> {
        match self.pending {
            Some(generation) => Some(generation.version),
            None => None,
        }
    }

    pub const fn failed_version(&self) -> Option<u64> {
        match self.failed {
            Some(generation) => Some(generation.version),
            None => None,
        }
    }

    pub const fn deadline_ms(&self) -> Option<u64> {
        self.deadline_ms
    }

    pub const fn grace_ms(&self) -> u64 {
        self.config.grace_ms
    }

    /// Apply hot settings atomically. An already-established grace/deadline is
    /// never extended or shortened retroactively; new timed phases use `config`.
    pub fn reconfigure(&mut self, config: HotConfig) {
        self.config = config;
    }

    pub fn request(&mut self, version: u64, now_ms: u64) -> Result<(), HotError> {
        if self
            .active
            .is_some_and(|generation| generation.version == version)
        {
            return Err(HotError::DuplicateRequest);
        }
        if self
            .previous
            .is_some_and(|generation| generation.version == version)
            || self
                .failed
                .is_some_and(|generation| generation.version == version)
        {
            return Err(HotError::DuplicateRequest);
        }
        if let Some(pending) = self.pending {
            return Err(if pending.version == version {
                HotError::DuplicateRequest
            } else {
                HotError::RequestInProgress
            });
        }
        if self.failed.is_some() {
            return Err(HotError::InvalidTransition);
        }
        if !matches!(self.phase, Phase::Idle | Phase::Stable) {
            return Err(HotError::InvalidTransition);
        }
        self.set_timed_phase(Phase::Requested, now_ms)?;
        self.pending = Some(Generation::new(version));
        Ok(())
    }

    pub fn begin_drain(&mut self, now_ms: u64) -> Result<(), HotError> {
        if self.phase != Phase::Requested || self.active.is_none() {
            return Err(HotError::InvalidTransition);
        }
        self.set_timed_phase(Phase::Draining, now_ms)
    }

    pub fn begin_loading(&mut self, now_ms: u64) -> Result<(), HotError> {
        let valid = match self.phase {
            Phase::Requested => self.active.is_none(),
            Phase::Draining => self.active.is_some(),
            _ => false,
        };
        if !valid {
            return Err(HotError::InvalidTransition);
        }
        if self.phase == Phase::Draining
            && self
                .active
                .is_some_and(|generation| generation.in_flight != 0)
        {
            return Err(HotError::CallsInFlight);
        }
        self.set_timed_phase(Phase::Loading, now_ms)
    }

    pub fn begin_canary(&mut self, now_ms: u64) -> Result<(), HotError> {
        if self.phase != Phase::Loading {
            return Err(HotError::InvalidTransition);
        }
        self.set_timed_phase(Phase::Canary, now_ms)
    }

    pub fn canary_succeeded(&mut self, now_ms: u64) -> Result<(), HotError> {
        if self.phase != Phase::Canary {
            return Err(HotError::InvalidTransition);
        }
        if now_ms.checked_add(self.config.grace_ms).is_none() {
            return Err(HotError::DeadlineOverflow);
        }
        if self.deadline_ms.is_some_and(|deadline| now_ms >= deadline) {
            return Err(HotError::DeadlineExpired);
        }
        if self.previous.is_some() {
            return Err(HotError::PreviousResident);
        }
        self.set_timed_phase(Phase::ActiveGrace, now_ms)?;
        let pending = self.pending.take().ok_or(HotError::InvalidTransition)?;
        self.previous = self.active.take();
        self.active = Some(pending);
        Ok(())
    }

    pub fn fail_pending(&mut self) -> Result<u64, HotError> {
        if !matches!(
            self.phase,
            Phase::Requested | Phase::Draining | Phase::Loading | Phase::Canary
        ) || self.failed.is_some()
        {
            return Err(HotError::InvalidTransition);
        }
        let failed = self.pending.take().ok_or(HotError::InvalidTransition)?;
        let version = failed.version;
        self.failed = Some(failed);
        self.phase = if self.active.is_some() {
            Phase::Stable
        } else {
            Phase::Failed
        };
        self.deadline_ms = None;
        Ok(version)
    }

    pub fn request_rollback(&mut self, now_ms: u64) -> Result<Fault, HotError> {
        if self.previous.is_none() {
            return Err(HotError::NoPrevious);
        }
        self.fail_current(now_ms)
    }

    pub fn fail_current(&mut self, now_ms: u64) -> Result<Fault, HotError> {
        if !matches!(self.phase, Phase::Stable | Phase::ActiveGrace) || self.failed.is_some() {
            return Err(HotError::InvalidTransition);
        }
        let current = self.active.ok_or(HotError::InvalidTransition)?;
        let failed = current.version;

        if let Some(active) = self.previous {
            self.set_timed_phase(Phase::RollingBack, now_ms)?;
            self.previous = None;
            self.active = Some(active);
            self.failed = Some(current);
            Ok(Fault::RolledBack {
                failed,
                active: active.version,
            })
        } else {
            self.active = None;
            self.failed = Some(current);
            self.phase = Phase::Failed;
            self.deadline_ms = None;
            Ok(Fault::Unrecoverable { failed })
        }
    }

    pub fn retire_failed(&mut self) -> Result<u64, HotError> {
        let failed = self.failed.ok_or(HotError::InvalidTransition)?;
        if failed.in_flight != 0 {
            return Err(HotError::CallsInFlight);
        }
        self.failed = None;
        if self.phase == Phase::Failed {
            self.phase = Phase::Idle;
        }
        Ok(failed.version)
    }

    pub fn in_flight(&self) -> InFlight {
        InFlight {
            active: self.active.map_or(0, |generation| generation.in_flight),
            previous: self.previous.map_or(0, |generation| generation.in_flight),
            pending: self.pending.map_or(0, |generation| generation.in_flight),
            failed: self.failed.map_or(0, |generation| generation.in_flight),
        }
    }

    pub fn enter(&mut self, version: u64) -> Result<GenerationRole, HotError> {
        if let Some(mut active) = self.active
            && active.version == version
        {
            if !matches!(
                self.phase,
                Phase::Stable
                    | Phase::Requested
                    | Phase::Canary
                    | Phase::ActiveGrace
                    | Phase::RollingBack
            ) {
                return Err(HotError::InvalidTransition);
            }
            active.in_flight = active
                .in_flight
                .checked_add(1)
                .ok_or(HotError::CounterOverflow)?;
            self.active = Some(active);
            return Ok(GenerationRole::Active);
        }
        if let Some(mut pending) = self.pending
            && pending.version == version
        {
            if self.phase != Phase::Canary {
                return Err(HotError::InvalidTransition);
            }
            pending.in_flight = pending
                .in_flight
                .checked_add(1)
                .ok_or(HotError::CounterOverflow)?;
            self.pending = Some(pending);
            return Ok(GenerationRole::Pending);
        }
        if self
            .previous
            .is_some_and(|generation| generation.version == version)
            || self
                .failed
                .is_some_and(|generation| generation.version == version)
        {
            return Err(HotError::InvalidTransition);
        }
        Err(HotError::UnknownGeneration)
    }

    pub fn leave(&mut self, version: u64) -> Result<GenerationRole, HotError> {
        for (slot, role) in [
            (&mut self.active, GenerationRole::Active),
            (&mut self.previous, GenerationRole::Previous),
            (&mut self.pending, GenerationRole::Pending),
            (&mut self.failed, GenerationRole::Failed),
        ] {
            if let Some(generation) = slot
                && generation.version == version
            {
                generation.in_flight = generation
                    .in_flight
                    .checked_sub(1)
                    .ok_or(HotError::NoCallsInFlight)?;
                return Ok(role);
            }
        }
        Err(HotError::UnknownGeneration)
    }

    pub fn poll(&mut self, now_ms: u64) -> Result<Tick, HotError> {
        if !self.deadline_ms.is_some_and(|deadline| now_ms >= deadline) {
            return Ok(Tick::Unchanged);
        }
        match self.phase {
            Phase::ActiveGrace => {
                self.phase = Phase::Stable;
                self.deadline_ms = None;
                Ok(Tick::GraceComplete)
            }
            Phase::RollingBack => {
                let active = self.active.ok_or(HotError::InvalidTransition)?.version;
                self.phase = Phase::Stable;
                self.deadline_ms = None;
                Ok(Tick::RollbackComplete { active })
            }
            Phase::Requested | Phase::Draining | Phase::Loading | Phase::Canary => {
                let failed = self.fail_pending()?;
                Ok(Tick::PendingFailed { failed })
            }
            _ => Ok(Tick::Unchanged),
        }
    }

    pub fn retire_previous(&mut self) -> Result<u64, HotError> {
        if !matches!(
            self.phase,
            Phase::Stable | Phase::Requested | Phase::Draining | Phase::Loading | Phase::Canary
        ) {
            return Err(HotError::InvalidTransition);
        }
        let previous = self.previous.ok_or(HotError::InvalidTransition)?;
        if previous.in_flight != 0 {
            return Err(HotError::CallsInFlight);
        }
        self.previous = None;
        Ok(previous.version)
    }

    fn set_timed_phase(&mut self, phase: Phase, now_ms: u64) -> Result<(), HotError> {
        let deadline = now_ms
            .checked_add(self.config.grace_ms)
            .ok_or(HotError::DeadlineOverflow)?;
        self.phase = phase;
        self.deadline_ms = Some(deadline);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_and_complete_parse() {
        assert_eq!(
            HotConfig::default(),
            HotConfig {
                tick_ms: 250,
                grace_ms: 3_000,
                telemetry: true,
                memory: true,
                m3: true,
            }
        );

        assert_eq!(
            parse_config("TICK_MS=1\nGRACE_MS=60000\nTELEMETRY=0\nMEMORY=0\nM3=1\n"),
            Ok(HotConfig {
                tick_ms: 1,
                grace_ms: 60_000,
                telemetry: false,
                memory: false,
                m3: true,
            })
        );
    }

    #[test]
    fn config_rejects_bad_input_without_partial_application() {
        let original = HotConfig::default();
        let mut applied = original;
        let result = parse_config("TICK_MS=10\nUNKNOWN=1\nGRACE_MS=100");
        assert_eq!(result, Err(ConfigError::UnknownKey { line: 2 }));
        if let Ok(config) = result {
            applied = config;
        }
        assert_eq!(applied, original);

        for input in [
            "TICK_MS=0",
            "TICK_MS=10001",
            "GRACE_MS=99",
            "GRACE_MS=60001",
            "TICK_MS=18446744073709551616",
            "TELEMETRY=maybe",
            "MEMORY=",
            "M3=2",
            "TICK_MS",
            "telemetry=0",
        ] {
            assert!(parse_config(input).is_err(), "accepted {input:?}");
        }

        assert_eq!(
            parse_config("TICK_MS=10000\nGRACE_MS=100"),
            Ok(HotConfig {
                tick_ms: 10_000,
                grace_ms: 100,
                ..HotConfig::default()
            })
        );
    }

    #[test]
    fn reconfiguration_changes_only_future_phase_grace_periods() {
        let mut hot = HotReload::new(HotConfig::default());
        hot.request(1, 0).unwrap();
        hot.begin_loading(0).unwrap();
        hot.begin_canary(0).unwrap();
        hot.canary_succeeded(0).unwrap();
        let original_deadline = hot.deadline_ms();
        hot.reconfigure(HotConfig {
            grace_ms: 123,
            ..HotConfig::default()
        });
        assert_eq!(hot.deadline_ms(), original_deadline);
        assert_eq!(hot.grace_ms(), 123);
        hot.poll(original_deadline.unwrap()).unwrap();
        hot.request(2, 400).unwrap();
        assert_eq!(hot.deadline_ms(), Some(523));
    }

    #[test]
    fn feature_configure_keys_are_lowercase() {
        let settings = HotConfig {
            telemetry: false,
            memory: true,
            m3: false,
            ..HotConfig::default()
        }
        .feature_settings();
        assert_eq!(
            settings.map(|setting| setting.key),
            ["telemetry", "memory", "m3"]
        );
        assert_eq!(
            settings.map(|setting| setting.enabled),
            [false, true, false]
        );
    }

    #[test]
    fn full_swap_sequence_retains_one_previous_until_explicit_retirement() {
        let mut hot = HotReload::new(HotConfig::default());
        assert_eq!(hot.phase(), Phase::Idle);

        hot.request(1, 0).unwrap();
        assert_eq!(hot.phase(), Phase::Requested);
        hot.begin_loading(1).unwrap();
        hot.begin_canary(2).unwrap();
        assert_eq!(hot.active_version(), None);
        hot.canary_succeeded(3).unwrap();
        assert_eq!(hot.active_version(), Some(1));
        assert_eq!(hot.phase(), Phase::ActiveGrace);
        assert_eq!(hot.poll(3_003), Ok(Tick::GraceComplete));
        assert_eq!(hot.phase(), Phase::Stable);

        hot.request(2, 200).unwrap();
        hot.begin_drain(201).unwrap();
        assert_eq!(hot.active_version(), Some(1));
        hot.begin_loading(202).unwrap();
        hot.begin_canary(203).unwrap();
        assert_eq!(hot.phase(), Phase::Canary);
        assert_eq!(hot.active_version(), Some(1));
        hot.canary_succeeded(204).unwrap();

        assert_eq!(hot.active_version(), Some(2));
        assert_eq!(hot.previous_version(), Some(1));
        assert_eq!(hot.pending_version(), None);
        assert_eq!(hot.phase(), Phase::ActiveGrace);
        assert_eq!(hot.deadline_ms(), Some(204 + hot.grace_ms()));
        assert_eq!(hot.retire_previous(), Err(HotError::InvalidTransition));
        assert_eq!(
            hot.poll(hot.deadline_ms().unwrap()),
            Ok(Tick::GraceComplete)
        );
        assert_eq!(hot.retire_previous(), Ok(1));
        assert_eq!(hot.previous_version(), None);
    }

    #[test]
    fn configuration_accepts_human_boolean_spellings() {
        assert_eq!(
            parse_config("TELEMETRY=on\nMEMORY=off\nM3=false"),
            Ok(HotConfig {
                telemetry: true,
                memory: false,
                m3: false,
                ..HotConfig::default()
            })
        );
    }

    #[test]
    fn requests_are_deduplicated_and_deadline_overflow_is_atomic() {
        let mut hot = HotReload::new(HotConfig::default());
        assert_eq!(hot.request(1, u64::MAX), Err(HotError::DeadlineOverflow));
        assert_eq!(hot.phase(), Phase::Idle);
        assert_eq!(hot.pending_version(), None);

        hot.request(1, 0).unwrap();
        assert_eq!(hot.request(1, 1), Err(HotError::DuplicateRequest));
        assert_eq!(hot.request(2, 1), Err(HotError::RequestInProgress));
        assert_eq!(hot.pending_version(), Some(1));
        assert_eq!(hot.deadline_ms(), Some(3_000));

        hot.begin_loading(2).unwrap();
        hot.begin_canary(3).unwrap();
        hot.canary_succeeded(4).unwrap();
        hot.poll(3_004).unwrap();
        assert_eq!(hot.request(1, 3_005), Err(HotError::DuplicateRequest));
        assert_eq!(hot.active_version(), Some(1));
        assert_eq!(hot.phase(), Phase::Stable);

        hot.request(2, 4_000).unwrap();
        hot.begin_drain(4_000).unwrap();
        hot.begin_loading(4_000).unwrap();
        hot.begin_canary(4_000).unwrap();
        assert_eq!(
            hot.canary_succeeded(u64::MAX),
            Err(HotError::DeadlineOverflow)
        );
        assert_eq!(hot.active_version(), Some(1));
        assert_eq!(hot.pending_version(), Some(2));
        assert_eq!(hot.phase(), Phase::Canary);
    }

    #[test]
    fn in_flight_calls_control_draining_canary_and_retirement() {
        let config = HotConfig {
            grace_ms: 100,
            ..HotConfig::default()
        };
        let mut hot = HotReload::new(config);
        hot.request(1, 0).unwrap();
        hot.begin_loading(0).unwrap();
        hot.begin_canary(0).unwrap();
        hot.canary_succeeded(0).unwrap();
        hot.poll(100).unwrap();

        assert_eq!(hot.enter(1), Ok(GenerationRole::Active));
        assert_eq!(hot.enter(1), Ok(GenerationRole::Active));
        assert_eq!(hot.in_flight().active, 2);
        hot.request(2, 101).unwrap();
        hot.begin_drain(101).unwrap();
        assert_eq!(hot.enter(1), Err(HotError::InvalidTransition));
        assert_eq!(hot.begin_loading(101), Err(HotError::CallsInFlight));
        assert_eq!(hot.leave(1), Ok(GenerationRole::Active));
        assert_eq!(hot.leave(1), Ok(GenerationRole::Active));
        assert_eq!(hot.leave(1), Err(HotError::NoCallsInFlight));

        hot.begin_loading(102).unwrap();
        hot.begin_canary(102).unwrap();
        assert_eq!(hot.enter(2), Ok(GenerationRole::Pending));
        assert_eq!(hot.enter(1), Ok(GenerationRole::Active));
        assert_eq!(
            hot.in_flight(),
            InFlight {
                active: 1,
                previous: 0,
                pending: 1,
                failed: 0,
            }
        );
        hot.canary_succeeded(103).unwrap();
        assert_eq!(hot.previous_version(), Some(1));
        assert_eq!(hot.in_flight().active, 1);
        assert_eq!(hot.in_flight().previous, 1);
        assert_eq!(hot.poll(203), Ok(Tick::GraceComplete));
        assert_eq!(hot.retire_previous(), Err(HotError::CallsInFlight));
        assert_eq!(hot.leave(1), Ok(GenerationRole::Previous));
        assert_eq!(hot.leave(2), Ok(GenerationRole::Active));
        assert_eq!(hot.retire_previous(), Ok(1));
        assert_eq!(hot.leave(1), Err(HotError::UnknownGeneration));
    }

    #[test]
    fn failed_canary_preserves_active_and_failed_code_waits_for_calls() {
        let config = HotConfig {
            grace_ms: 100,
            ..HotConfig::default()
        };
        let mut hot = HotReload::new(config);
        hot.request(1, 0).unwrap();
        hot.begin_loading(0).unwrap();
        hot.begin_canary(0).unwrap();
        hot.canary_succeeded(0).unwrap();
        hot.poll(100).unwrap();
        hot.request(2, 101).unwrap();
        hot.begin_drain(101).unwrap();
        hot.begin_loading(101).unwrap();
        hot.begin_canary(101).unwrap();
        hot.enter(2).unwrap();

        assert_eq!(hot.fail_pending(), Ok(2));
        assert_eq!(hot.phase(), Phase::Stable);
        assert_eq!(hot.active_version(), Some(1));
        assert_eq!(hot.pending_version(), None);
        assert_eq!(hot.failed_version(), Some(2));
        assert_eq!(hot.in_flight().failed, 1);
        assert_eq!(hot.retire_failed(), Err(HotError::CallsInFlight));
        assert_eq!(hot.leave(2), Ok(GenerationRole::Failed));
        assert_eq!(hot.retire_failed(), Ok(2));

        assert_eq!(hot.request_rollback(102), Err(HotError::NoPrevious));
        assert_eq!(
            hot.fail_current(102),
            Ok(Fault::Unrecoverable { failed: 1 })
        );
        assert_eq!(hot.phase(), Phase::Failed);
        assert_eq!(hot.active_version(), None);
        assert_eq!(hot.failed_version(), Some(1));
        assert_eq!(hot.poll(1_000), Ok(Tick::Unchanged));
        assert_eq!(hot.retire_failed(), Ok(1));
        assert_eq!(hot.phase(), Phase::Idle);
    }

    #[test]
    fn current_fault_selects_previous_then_completes_bounded_rollback() {
        let config = HotConfig {
            grace_ms: 100,
            ..HotConfig::default()
        };
        let mut hot = HotReload::new(config);
        hot.request(1, 0).unwrap();
        hot.begin_loading(0).unwrap();
        hot.begin_canary(0).unwrap();
        hot.canary_succeeded(0).unwrap();
        hot.poll(100).unwrap();
        hot.request(2, 100).unwrap();
        hot.begin_drain(100).unwrap();
        hot.begin_loading(100).unwrap();
        hot.begin_canary(100).unwrap();
        hot.canary_succeeded(100).unwrap();
        hot.enter(2).unwrap();

        assert_eq!(
            hot.fail_current(101),
            Ok(Fault::RolledBack {
                failed: 2,
                active: 1
            })
        );
        assert_eq!(hot.phase(), Phase::RollingBack);
        assert_eq!(hot.active_version(), Some(1));
        assert_eq!(hot.previous_version(), None);
        assert_eq!(hot.failed_version(), Some(2));
        assert_eq!(hot.retire_failed(), Err(HotError::CallsInFlight));
        assert_eq!(hot.enter(1), Ok(GenerationRole::Active));
        assert_eq!(hot.poll(200), Ok(Tick::Unchanged));
        assert_eq!(hot.poll(201), Ok(Tick::RollbackComplete { active: 1 }));
        assert_eq!(hot.phase(), Phase::Stable);
        assert_eq!(hot.leave(2), Ok(GenerationRole::Failed));
        assert_eq!(hot.retire_failed(), Ok(2));
        assert_eq!(hot.retire_failed(), Err(HotError::InvalidTransition));
    }

    #[test]
    fn previous_pending_and_active_do_not_overwrite_retention_slots() {
        let config = HotConfig {
            grace_ms: 100,
            ..HotConfig::default()
        };
        let mut hot = HotReload::new(config);
        for version in [1, 2] {
            hot.request(version, version * 200).unwrap();
            if version == 2 {
                hot.begin_drain(version * 200).unwrap();
            }
            hot.begin_loading(version * 200).unwrap();
            hot.begin_canary(version * 200).unwrap();
            hot.canary_succeeded(version * 200).unwrap();
            hot.poll(version * 200 + 100).unwrap();
        }
        assert_eq!(hot.active_version(), Some(2));
        assert_eq!(hot.previous_version(), Some(1));

        hot.request(3, 500).unwrap();
        hot.begin_drain(500).unwrap();
        hot.begin_loading(500).unwrap();
        hot.begin_canary(500).unwrap();
        let deadline = hot.deadline_ms();
        assert_eq!(hot.canary_succeeded(501), Err(HotError::PreviousResident));
        assert_eq!(hot.active_version(), Some(2));
        assert_eq!(hot.previous_version(), Some(1));
        assert_eq!(hot.pending_version(), Some(3));
        assert_eq!(hot.phase(), Phase::Canary);
        assert_eq!(hot.deadline_ms(), deadline);
        assert_eq!(hot.retire_previous(), Ok(1));
        hot.canary_succeeded(502).unwrap();
        assert_eq!(hot.active_version(), Some(3));
        assert_eq!(hot.previous_version(), Some(2));
    }

    #[test]
    fn expired_completed_canary_is_never_published() {
        let mut hot = HotReload::new(HotConfig {
            grace_ms: 100,
            ..HotConfig::default()
        });
        hot.request(1, 0).unwrap();
        hot.begin_loading(0).unwrap();
        hot.begin_canary(0).unwrap();
        hot.canary_succeeded(0).unwrap();
        hot.poll(100).unwrap();

        hot.request(2, 101).unwrap();
        hot.begin_drain(101).unwrap();
        hot.begin_loading(101).unwrap();
        hot.begin_canary(195).unwrap();
        assert_eq!(hot.canary_succeeded(296), Err(HotError::DeadlineExpired));
        assert_eq!(hot.phase(), Phase::Canary);
        assert_eq!(hot.active_version(), Some(1));
        assert_eq!(hot.pending_version(), Some(2));
        assert_eq!(hot.fail_pending(), Ok(2));
        assert_eq!(hot.active_version(), Some(1));
    }

    #[test]
    fn pending_deadline_fails_only_pending_and_counter_never_wraps() {
        let config = HotConfig {
            grace_ms: 100,
            ..HotConfig::default()
        };
        let mut hot = HotReload::new(config);
        hot.request(1, 0).unwrap();
        hot.begin_loading(0).unwrap();
        hot.begin_canary(0).unwrap();
        hot.canary_succeeded(0).unwrap();
        hot.poll(100).unwrap();
        hot.enter(1).unwrap();
        hot.request(2, 101).unwrap();
        hot.begin_drain(101).unwrap();
        assert_eq!(hot.poll(200), Ok(Tick::Unchanged));
        assert_eq!(hot.poll(201), Ok(Tick::PendingFailed { failed: 2 }));
        assert_eq!(hot.active_version(), Some(1));
        assert_eq!(hot.pending_version(), None);
        assert_eq!(hot.failed_version(), Some(2));
        assert_eq!(hot.in_flight().active, 1);
        assert_eq!(hot.leave(1), Ok(GenerationRole::Active));

        hot.active.as_mut().unwrap().in_flight = u32::MAX;
        assert_eq!(hot.enter(1), Err(HotError::CounterOverflow));
        assert_eq!(hot.in_flight().active, u32::MAX);
    }

    #[test]
    fn manifest_drives_latest_and_preserves_the_artifact_identity() {
        let expected = "245768f2c2764ca43299e32a117021bc7fb57f73ca7a03cc93fb8e31c3bf9708";
        let sha = "a".repeat(64);
        let valid = format!(
            "GENERATION=4\nSHA256={sha}\nFILE=/tmp/features-4-aaaaaaaaaaaa.dll\nGAME_SHA256={expected}\nABI=2\n"
        );
        let manifest = parse_feature_manifest(&valid).unwrap();
        assert_eq!(manifest.artifact.generation, 4);
        assert_eq!(manifest.artifact.sha256, sha);
        assert_eq!(manifest.game_sha256, expected);
        let windows = windows_artifact_path("/tmp/features-4-aaaaaaaaaaaa.dll").unwrap();
        assert_eq!(manifest.artifact.path, windows);
        assert!(parse_feature_manifest(&valid.replace("ABI=2", "ABI=1")).is_err());
        assert!(
            parse_feature_manifest(&valid.replace("aaaaaaaaaaaa.dll", "bbbbbbbbbbbb.dll")).is_err()
        );
        assert!(
            parse_feature_manifest(&valid.replace("/tmp/features-4", "/tmp/../features-4"))
                .is_err()
        );
    }

    #[test]
    fn reload_parser_supports_generation_shorthand_and_rejects_bad_arity() {
        assert_eq!(parse_reload_request("reload"), Ok(ReloadSelection::Latest));
        assert_eq!(
            parse_reload_request("reload 7"),
            Ok(ReloadSelection::Generation(7))
        );
        assert_eq!(
            parse_reload_request("reload gen 8"),
            Ok(ReloadSelection::Generation(8))
        );
        assert_eq!(
            parse_reload_request("reload status"),
            Ok(ReloadSelection::Status)
        );
        for bad in [
            "reload gen",
            "reload gen 0",
            "reload gen 09",
            "reload gen 3 extra",
            "reload latest 3",
            "reload 0",
            "reload 00",
        ] {
            assert!(parse_reload_request(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn versioned_feature_names_are_strict_and_do_not_accept_traversal() {
        let parsed = parse_versioned_name("features-42-0123456789ab.dll").unwrap();
        assert_eq!(parsed.generation, 42);
        assert_eq!(parsed.short_sha, "0123456789ab");
        for bad in [
            "../features-42-0123456789ab.dll",
            "features-09-0123456789ab.dll",
            "features--1-0123456789ab.dll",
            "features-0-0123456789ab.dll",
            "features-42-0123456789a.dll",
            "features-42-0123456789az.dll",
            "features-42-0123456789ab.dll.exe",
            "features-42-0123456789ab/dll",
        ] {
            assert!(parse_versioned_name(bad).is_none(), "accepted {bad}");
        }
    }

    #[test]
    fn config_set_preserves_unrelated_values_and_rejects_the_whole_bad_update() {
        let original = HotConfig {
            tick_ms: 123,
            telemetry: false,
            ..Default::default()
        };
        let updated = replace_config(original, "M3", "0").unwrap();
        assert_eq!(updated.tick_ms, 123);
        assert!(!updated.telemetry);
        assert!(!updated.m3);
        assert!(replace_config(original, "UNKNOWN", "1").is_err());
        assert_eq!(original.tick_ms, 123);
    }

    #[test]
    fn format_and_parse_hot_status_roundtrip_all_phases() {
        let cases = [
            (Phase::Idle, HotState::Idle, "idle"),
            (Phase::Stable, HotState::Stable, "stable"),
            (Phase::Requested, HotState::Requested, "requested"),
            (Phase::Draining, HotState::Draining, "draining"),
            (Phase::Loading, HotState::Loading, "loading"),
            (Phase::Canary, HotState::Canary, "canary"),
            (Phase::ActiveGrace, HotState::Active, "active"),
            (Phase::RollingBack, HotState::RollingBack, "rolling_back"),
            (Phase::Failed, HotState::Error, "failed"),
        ];
        for (phase, expected_state, expected_name) in cases {
            let line = format_hot_status(
                phase,
                3,
                Some(2),
                Some(4),
                Some("0123456789ab"),
                1,
                None,
            );
            let tagged = format!("[42] [hot] {line}");
            let parsed = parse_hot_status(&tagged).expect("roundtrip must parse");
            assert_eq!(parsed.state, expected_state);
            assert_eq!(parsed.state_name, expected_name);
            assert_eq!(parsed.generation, 3);
            assert_eq!(parsed.previous, Some(2));
            assert_eq!(parsed.pending, Some(4));
            assert_eq!(parsed.sha.as_deref(), Some("0123456789ab"));
            assert_eq!(parsed.flight, 1);
            assert_eq!(parsed.error, None);
        }
    }
}
