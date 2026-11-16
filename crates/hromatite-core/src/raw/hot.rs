//! Resident generation router. Runtime mapping lives behind the existing tick;
//! immutable artifact parsing and configuration remain independently testable.

use std::sync::atomic::{AtomicU64, Ordering};

const DEFAULT_TICK_MS: u64 = 250;
static TICK_INTERVAL_MS: AtomicU64 = AtomicU64::new(0);

#[cfg(any(target_os = "windows", test))]
#[derive(Clone, Debug, Eq, PartialEq)]
struct VersionedName {
    generation: u64,
    short_sha: String,
}

#[cfg(any(target_os = "windows", test))]
fn parse_versioned_name(name: &str) -> Option<VersionedName> {
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
    let generation = generation.parse::<u64>().ok()?;
    (generation != 0).then(|| VersionedName {
        generation,
        short_sha: short_sha.to_string(),
    })
}

#[cfg(any(target_os = "windows", test))]
fn config_text(config: crate::hot::HotConfig) -> String {
    format!(
        "TICK_MS={}\nGRACE_MS={}\nTELEMETRY={}\nMEMORY={}\nM3={}\n",
        config.tick_ms,
        config.grace_ms,
        u8::from(config.telemetry),
        u8::from(config.memory),
        u8::from(config.m3),
    )
}

#[cfg(any(target_os = "windows", test))]
fn replace_config(
    original: crate::hot::HotConfig,
    key: &str,
    value: &str,
) -> Result<crate::hot::HotConfig, crate::hot::ConfigError> {
    let mut lines: Vec<String> = config_text(original).lines().map(str::to_string).collect();
    let prefix = format!("{key}=");
    if let Some(line) = lines.iter_mut().find(|line| line.starts_with(&prefix)) {
        *line = format!("{prefix}{value}");
    } else {
        lines.push(format!("{prefix}{value}"));
    }
    crate::hot::parse_config(&lines.join("\n"))
}

#[cfg(target_os = "windows")]
#[derive(Clone, Debug, Eq, PartialEq)]
struct FeatureArtifact {
    generation: u64,
    sha256: String,
    path: std::path::PathBuf,
}

pub fn tick_interval_ms() -> u64 {
    let mut value = TICK_INTERVAL_MS.load(Ordering::Relaxed);
    if value == 0 {
        value = std::env::var("HROMATITE_TICK_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|value| (1..=10_000).contains(value))
            .unwrap_or(DEFAULT_TICK_MS);
        TICK_INTERVAL_MS.store(value, Ordering::Relaxed);
    }
    value
}

#[cfg(target_os = "windows")]
mod imp {
    use super::{FeatureArtifact, config_text, parse_versioned_name, replace_config};
    use crate::hot::{Fault, HotConfig, HotReload, Phase, Tick};
    use crate::raw::localmap::FeatureImage;
    use crate::symbols;
    use hromatite_abi::{HostApiV1, STATUS_OK};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
    static HOST_POINTER: AtomicUsize = AtomicUsize::new(0);

    struct ImageGeneration {
        image: FeatureImage,
        host: *const HostApiV1,
        sha256: String,
    }

    struct Pending {
        artifact: FeatureArtifact,
        image: Option<ImageGeneration>,
    }

    struct Runtime {
        model: HotReload,
        config: HotConfig,
        active: Option<ImageGeneration>,
        previous: Option<ImageGeneration>,
        pending: Option<Pending>,
        failed: Option<ImageGeneration>,
        last_error: Option<String>,
        bootstrap_attempted: bool,
        dirty: bool,
    }

    impl Runtime {
        fn new() -> Self {
            let mut config = HotConfig::default();
            let mut error = None;
            match read_config_file() {
                Ok(text) if text.trim().is_empty() => {}
                Ok(text) => match crate::hot::parse_config(&text) {
                    Ok(value) => config = value,
                    Err(problem) => error = Some(format!("initial config: {problem:?}")),
                },
                Err(problem) => error = Some(problem),
            }
            super::TICK_INTERVAL_MS.store(config.tick_ms, Ordering::Relaxed);
            Self {
                model: HotReload::new(config),
                config,
                active: None,
                previous: None,
                pending: None,
                failed: None,
                last_error: error,
                bootstrap_attempted: false,
                dirty: true,
            }
        }

        fn status(&self) -> String {
            let optional =
                |value: Option<u64>| value.map_or_else(|| "none".to_string(), |v| v.to_string());
            let flights = self.model.in_flight();
            let generation = self.model.active_version().unwrap_or(0);
            let sha = self
                .active
                .as_ref()
                .map_or("none", |generation| generation.sha256.as_str());
            format!(
                "state={} generation={generation} previous={} pending={} sha={} flight={} error={}",
                phase_name(self.model.phase()),
                optional(self.model.previous_version()),
                optional(self.model.pending_version()),
                sha,
                flights.active + flights.previous + flights.pending + flights.failed,
                self.last_error.as_deref().unwrap_or("none"),
            )
        }

        fn report(&mut self) {
            if self.dirty {
                crate::log::log_tag("hot", &self.status());
                self.dirty = false;
            }
        }

        fn fail_pending(&mut self, reason: String) {
            match self.model.fail_pending() {
                Ok(version) => {
                    self.failed = self.pending.take().and_then(|pending| pending.image);
                    self.last_error = Some(reason.clone());
                    crate::log::log_tag(
                        "hot",
                        &format!("generation {version} not activated: {reason}"),
                    );
                }
                Err(problem) => {
                    self.last_error = Some(format!("state error {problem:?}: {reason}"));
                    crate::log::log_tag("hot", self.last_error.as_deref().unwrap_or("state error"));
                }
            }
            self.dirty = true;
        }

        fn rollback_current(&mut self, reason: &str) -> Result<String, String> {
            let failed = match self.model.fail_current(now_ms()) {
                Ok(Fault::RolledBack { failed: _, active }) => {
                    self.failed = self.active.take();
                    self.active = self.previous.take();
                    self.dirty = true;
                    self.last_error = Some(format!("{reason}; rolled back to {active}"));
                    return Err(self.last_error.clone().unwrap_or_else(|| "rollback".into()));
                }
                Ok(Fault::Unrecoverable { failed }) => failed,
                Err(problem) => return Err(format!("cannot roll back: {problem:?}")),
            };
            self.failed = self.active.take();
            self.active = None;
            self.last_error = Some(format!("{reason}; built-in console remains active"));
            self.dirty = true;
            Ok(format!(
                "generation {failed} failed; built-in console active"
            ))
        }

        fn schedule(&mut self, artifact: FeatureArtifact) -> Result<String, String> {
            let generation = artifact.generation;
            self.model
                .request(generation, now_ms())
                .map_err(|e| format!("reload rejected: {e:?}"))?;
            self.pending = Some(Pending {
                artifact: artifact.clone(),
                image: None,
            });
            self.last_error = None;
            self.dirty = true;
            Ok(format!(
                "reload scheduled: generation {generation}, {}",
                artifact.path.display()
            ))
        }

        fn load_pending(&mut self) {
            let Some(artifact) = self
                .pending
                .as_ref()
                .map(|pending| pending.artifact.clone())
            else {
                return;
            };
            let bytes = match read_artifact(&artifact.path) {
                Ok(bytes) => bytes,
                Err(reason) => return self.fail_pending(reason),
            };
            let actual_sha = crate::sha256::hex_digest(&bytes);
            if !actual_sha.starts_with(&artifact.sha256) {
                return self.fail_pending(format!(
                    "SHA mismatch: expected {}, got {actual_sha}",
                    artifact.sha256
                ));
            }
            if !game_hash_matches() {
                return self
                    .fail_pending("GameAssembly build hash differs from resident symbols".into());
            }
            let path = match artifact.path.to_str() {
                Some(path) => path.to_string(),
                None => return self.fail_pending("feature path is not UTF-8".into()),
            };
            match self.model.begin_canary(now_ms()) {
                Ok(()) => {}
                Err(problem) => return self.fail_pending(format!("begin canary: {problem:?}")),
            }
            let loaded = crate::log::guard("hot::FeatureImage::load", || FeatureImage::load(&path));
            let image = match loaded {
                Some(Ok(image)) => image,
                Some(Err(error)) => return self.fail_pending(format!("local map: {error}")),
                None => {
                    return self
                        .fail_pending("local map faulted; partial image is quarantined".into());
                }
            };
            let host = match host_pointer() {
                Some(host) => host,
                None => {
                    // The successfully loaded image must remain registered even if
                    // host discovery fails: its shutdown is attempted during retire.
                    let image = ImageGeneration {
                        image,
                        host: core::ptr::null(),
                        sha256: actual_sha,
                    };
                    if let Some(pending) = self.pending.as_mut() {
                        pending.image = Some(image);
                    }
                    return self
                        .fail_pending("GameAssembly/UnityPlayer host modules unavailable".into());
                }
            };
            let base = image.base();
            let size = image.size();
            if let Some(pending) = self.pending.as_mut() {
                pending.image = Some(ImageGeneration {
                    image,
                    host,
                    sha256: actual_sha,
                });
            }
            let Some(generation) = self
                .pending
                .as_ref()
                .and_then(|pending| pending.image.as_ref())
            else {
                return self.fail_pending("loaded generation ownership was lost".into());
            };
            let api = generation.image.api();
            let version = artifact.generation;
            if let Err(problem) = self.model.enter(version) {
                return self.fail_pending(format!("acquire pending canary: {problem:?}"));
            }
            // SAFETY: localmap validated this table inside the still-owned image;
            // host storage intentionally lives for the whole process.
            let initialized = crate::log::guard("hot::feature::init", || unsafe {
                ((*api).init)(generation.host, version)
            });
            let init_status = initialized.unwrap_or(hromatite_abi::STATUS_ERROR);
            let mut config_status = if init_status == STATUS_OK {
                STATUS_OK
            } else {
                hromatite_abi::STATUS_ERROR
            };
            if config_status == STATUS_OK {
                for setting in self.config.feature_settings() {
                    let enabled = if setting.enabled { "1" } else { "0" };
                    // SAFETY: the byte slices remain live for this synchronous callback.
                    let status = crate::log::guard("hot::feature::configure", || unsafe {
                        ((*api).configure)(
                            setting.key.as_ptr() as *const i8,
                            setting.key.len(),
                            enabled.as_ptr() as *const i8,
                            enabled.len(),
                        )
                    })
                    .unwrap_or(hromatite_abi::STATUS_ERROR);
                    if status != STATUS_OK {
                        config_status = status;
                        break;
                    }
                }
            }
            let mut canary_status = config_status;
            if canary_status == STATUS_OK {
                let command = b"feature";
                // SAFETY: the command is valid borrowed UTF-8 and the loaded
                // generation stays owned by `pending` throughout this call.
                canary_status = crate::log::guard("hot::feature::canary", || unsafe {
                    ((*api).dispatch)(command.as_ptr() as *const i8, command.len())
                })
                .unwrap_or(hromatite_abi::STATUS_ERROR);
            }
            if let Err(problem) = self.model.leave(version) {
                return self.fail_pending(format!("release pending canary: {problem:?}"));
            }
            if canary_status != STATUS_OK {
                return self.fail_pending(format!(
                    "generation {version} init/canary failed with status {canary_status} at {base:#x}+{size:#x}"
                ));
            }
            crate::log::log_tag("hot", &format!("generation {version} canary OK"));
            if self.model.previous_version().is_some() {
                match self.model.retire_previous() {
                    Ok(_) => {
                        if let Some(previous) = self.previous.take()
                            && let Err(error) = shutdown_retire(previous)
                        {
                            self.last_error = Some(error);
                        }
                    }
                    Err(problem) => {
                        crate::log::log_tag(
                            "hot",
                            &format!("previous generation is not drained: {problem:?}"),
                        );
                        return;
                    }
                }
            }
            match self.model.canary_succeeded(now_ms()) {
                Ok(()) => {
                    let generation = self.pending.take().and_then(|pending| pending.image);
                    self.previous = self.active.take();
                    self.active = generation;
                    self.last_error = None;
                    self.dirty = true;
                    crate::log::log_tag(
                        "hot",
                        &format!("generation {version} is active; previous available for rollback"),
                    );
                }
                Err(problem) => self.fail_pending(format!("publish generation: {problem:?}")),
            }
        }

        fn retire_failed(&mut self) {
            match self.model.retire_failed() {
                Ok(version) => {
                    if let Some(failed) = self.failed.take()
                        && let Err(error) = shutdown_retire(failed)
                    {
                        self.last_error = Some(format!(
                            "retire {version}: {error}; image stays quarantined"
                        ));
                    }
                    self.dirty = true;
                }
                Err(crate::hot::HotError::CallsInFlight) => {}
                Err(problem) => self.last_error = Some(format!("retire check: {problem:?}")),
            }
        }

        fn advance(&mut self) {
            match self.model.poll(now_ms()) {
                Ok(Tick::PendingFailed { failed }) => {
                    self.failed = self.pending.take().and_then(|pending| pending.image);
                    self.last_error = Some(format!(
                        "generation {failed} exceeded its load/drain budget"
                    ));
                    self.dirty = true;
                }
                Ok(Tick::GraceComplete) | Ok(Tick::RollbackComplete { .. }) => self.dirty = true,
                Ok(Tick::Unchanged) => {}
                Err(problem) => self.last_error = Some(format!("deadline: {problem:?}")),
            }

            if self.model.failed_version().is_some() {
                self.retire_failed();
                self.report();
                return;
            }
            if let Some(pending) = self.pending.as_ref()
                && pending.artifact.generation != 0
                && matches!(self.model.phase(), Phase::Requested | Phase::Draining)
            {
                if self.model.active_version().is_some()
                    && self.model.phase() == Phase::Requested
                    && let Err(problem) = self.model.begin_drain(now_ms())
                {
                    return self.fail_pending(format!("begin drain: {problem:?}"));
                }
                if self.model.phase() == Phase::Draining {
                    match self.model.begin_loading(now_ms()) {
                        Ok(()) => {}
                        Err(crate::hot::HotError::CallsInFlight) => return,
                        Err(problem) => return self.fail_pending(format!("drain: {problem:?}")),
                    }
                }
            }
            if self.model.pending_version().is_some()
                && self.model.phase() == Phase::Requested
                && let Err(problem) = self.model.begin_loading(now_ms())
            {
                return self.fail_pending(format!("begin load: {problem:?}"));
            }
            if self.model.pending_version().is_some() && self.model.phase() == Phase::Loading {
                self.load_pending();
            }
        }

        fn dispatch(&mut self, command: &str) -> bool {
            let Some(version) = self.model.active_version() else {
                return false;
            };
            if self.model.enter(version).is_err() || self.active.is_none() {
                return false;
            }
            let api = self
                .active
                .as_ref()
                .map(|generation| generation.image.api());
            let result = api.map(|api| {
                crate::log::guard("hot::feature::dispatch", || {
                    // SAFETY: active image is retained until after model.leave, and
                    // command bytes are borrowed only for this synchronous dispatch.
                    unsafe { ((*api).dispatch)(command.as_ptr() as *const i8, command.len()) }
                })
            });
            let leave = self.model.leave(version);
            if leave.is_err() || result.is_none() || result.is_some_and(|v| v.is_none()) {
                let _ = self.rollback_current("feature dispatch fault");
                return true;
            }
            matches!(result, Some(Some(_)))
        }

        fn feature_tick(&mut self) {
            let Some(version) = self.model.active_version() else {
                return;
            };
            if self.model.enter(version).is_err() {
                return;
            }
            let api = self
                .active
                .as_ref()
                .map(|generation| generation.image.api());
            let result = api.map(|api| {
                crate::log::guard("hot::feature::tick", || {
                    // SAFETY: active generation cannot retire during this guarded tick.
                    unsafe { ((*api).tick)() }
                })
            });
            let leave = self.model.leave(version);
            if leave.is_err() || result.is_none() || result.is_some_and(|v| v.is_none()) {
                let _ = self.rollback_current("feature tick fault");
            }
        }

        fn configure(&mut self, config: HotConfig) {
            self.config = config;
            self.model.reconfigure(config);
            super::TICK_INTERVAL_MS.store(config.tick_ms, Ordering::Relaxed);
            for generation in [&self.active, &self.previous].into_iter().flatten() {
                let api = generation.image.api();
                for setting in config.feature_settings() {
                    let value = if setting.enabled { "1" } else { "0" };
                    let status = crate::log::guard("hot::configure:active", || {
                        // SAFETY: both generation values stay mapped throughout this call.
                        unsafe {
                            ((*api).configure)(
                                setting.key.as_ptr() as *const i8,
                                setting.key.len(),
                                value.as_ptr() as *const i8,
                                value.len(),
                            )
                        }
                    })
                    .unwrap_or(hromatite_abi::STATUS_ERROR);
                    if status != STATUS_OK {
                        self.last_error =
                            Some(format!("configure {} failed ({status})", setting.key));
                    }
                }
            }
            self.dirty = true;
        }
    }

    static mut RUNTIME: Option<Runtime> = None;

    unsafe fn runtime() -> *mut Runtime {
        // SAFETY: hook::work is serialized by BUSY on the established game thread.
        // No new polling thread is created by this module. A raw pointer is used
        // so Rust 2024 never creates a long-lived reference to a mutable static.
        unsafe { (&raw mut RUNTIME).as_mut().unwrap() }.get_or_insert_with(Runtime::new)
            as *mut Runtime
    }

    fn now_ms() -> u64 {
        // SAFETY: parameterless Win32 millisecond counter.
        unsafe { crate::ffi::GetTickCount64() }
    }

    fn phase_name(phase: Phase) -> &'static str {
        match phase {
            Phase::Idle => "idle",
            Phase::Stable => "stable",
            Phase::Requested => "requested",
            Phase::Draining => "draining",
            Phase::Loading => "loading",
            Phase::Canary => "canary",
            Phase::ActiveGrace => "active",
            Phase::RollingBack => "rolling_back",
            Phase::Failed => "failed",
        }
    }

    fn host_pointer() -> Option<*const HostApiV1> {
        let existing = HOST_POINTER.load(Ordering::Acquire);
        if existing != 0 {
            return Some(existing as *const HostApiV1);
        }
        let host = Box::new(crate::raw::hostapi::current_host_api()?);
        let pointer = Box::into_raw(host);
        match HOST_POINTER.compare_exchange(
            0,
            pointer as usize,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Some(pointer),
            Err(winner) => {
                // SAFETY: this allocation did not become shared after losing CAS.
                drop(unsafe { Box::from_raw(pointer) });
                Some(winner as *const HostApiV1)
            }
        }
    }

    fn module_dir() -> PathBuf {
        std::env::var_os("HROMATITE_FEATURE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("USERPROFILE")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("C:"))
                    .join("hromatite-modules")
            })
    }

    fn choose_artifact(requested: Option<u64>) -> Result<FeatureArtifact, String> {
        let directory = module_dir();
        let entries = std::fs::read_dir(&directory)
            .map_err(|e| format!("feature directory {}: {e}", directory.display()))?;
        let mut candidates = Vec::new();
        for entry in entries.take(512) {
            let entry = entry.map_err(|e| format!("feature directory entry: {e}"))?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(parsed) = parse_versioned_name(name) else {
                continue;
            };
            if requested.is_some_and(|requested| requested != parsed.generation) {
                continue;
            }
            candidates.push(FeatureArtifact {
                generation: parsed.generation,
                sha256: parsed.short_sha,
                path: entry.path(),
            });
        }
        if requested.is_none() {
            candidates.sort_by_key(|candidate| candidate.generation);
        }
        candidates.pop().ok_or_else(|| {
            requested.map_or_else(
                || {
                    format!(
                        "no features-<generation>-<sha12>.dll in {}",
                        directory.display()
                    )
                },
                |generation| {
                    format!(
                        "generation {generation} not found in {}",
                        directory.display()
                    )
                },
            )
        })
    }

    fn read_artifact(path: &Path) -> Result<Vec<u8>, String> {
        let metadata = std::fs::symlink_metadata(path)
            .map_err(|e| format!("artifact {}: {e}", path.display()))?;
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err("artifact must be a regular immutable file, not a symlink".into());
        }
        if metadata.len() == 0 || metadata.len() > MAX_ARTIFACT_BYTES {
            return Err(format!(
                "artifact size {} is outside 1..{MAX_ARTIFACT_BYTES}",
                metadata.len()
            ));
        }
        std::fs::read(path).map_err(|e| format!("read artifact {}: {e}", path.display()))
    }

    fn game_hash_matches() -> bool {
        crate::procs::own_module(crate::GAME_ASSEMBLY).is_some()
            && crate::procs::own_module(crate::UNITY_PLAYER).is_some()
            && symbols::GAMEASSEMBLY_SHA256.len() == 64
            && symbols::GAMEASSEMBLY_SHA256 == crate::lateupdate::AUTO_BUILD_SHA256
    }

    fn config_path() -> PathBuf {
        std::env::var_os("HROMATITE_HOT_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("USERPROFILE")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("C:"))
                    .join("hromatite-live.conf")
            })
    }

    fn read_config_file() -> Result<String, String> {
        let path = config_path();
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.len() > 64 * 1024 => Err("hot config exceeds 64 KiB".into()),
            Ok(_) => std::fs::read_to_string(&path)
                .map_err(|e| format!("hot config {}: {e}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(format!("hot config {}: {error}", path.display())),
        }
    }

    fn shutdown_retire(generation: ImageGeneration) -> Result<(), String> {
        let api = generation.image.api();
        // SAFETY: the model has rejected all new entries and in_flight==0.
        let shutdown =
            crate::log::guard("hot::feature::shutdown", || unsafe { ((*api).shutdown)() });
        if shutdown.is_none() {
            return Err("feature shutdown faulted".into());
        }
        // SAFETY: shutdown returned and migration no longer dispatches to this image.
        crate::log::guard("hot::feature::retire", || unsafe {
            generation.image.retire()
        })
        .unwrap_or_else(|| Err("feature image detach faulted".into()))
    }

    pub(super) fn tick_impl() {
        // SAFETY: the existing tick serializes all runtime state transitions.
        let runtime = unsafe { &mut *runtime() };
        if !runtime.bootstrap_attempted {
            runtime.bootstrap_attempted = true;
            if let Ok(path) = std::env::var("HROMATITE_FEATURE_DLL")
                && !path.trim().is_empty()
            {
                let artifact = FeatureArtifact {
                    generation: 0,
                    sha256: String::new(),
                    path: PathBuf::from(&path),
                };
                if let Err(error) = runtime.schedule(artifact) {
                    crate::log::log_tag("hot", &error);
                }
            }
        }
        runtime.advance();
        runtime.feature_tick();
        runtime.report();
    }

    pub(super) fn dispatch_impl(line: &str) -> bool {
        // SAFETY: the caller owns the same serialized hook::work context.
        unsafe { &mut *runtime() }.dispatch(line)
    }

    pub(super) fn command_impl(line: &str) -> Option<Vec<String>> {
        // SAFETY: command dispatch runs only from the established tick context.
        let runtime = unsafe { &mut *runtime() };
        let mut parts = line.split_whitespace();
        let command = parts.next()?.to_ascii_lowercase();
        match command.as_str() {
            "reload" => {
                let action = parts.next().unwrap_or("latest");
                if parts.next().is_some() {
                    return Some(vec!["usage: reload [generation|status]".into()]);
                }
                if action == "status" {
                    return Some(vec![runtime.status()]);
                }
                let requested = if action == "latest" {
                    None
                } else {
                    match action.parse::<u64>() {
                        Ok(value) if value != 0 => Some(value),
                        _ => {
                            return Some(vec![
                                "reload generation must be a positive decimal integer".into(),
                            ]);
                        }
                    }
                };
                let result =
                    choose_artifact(requested).and_then(|artifact| runtime.schedule(artifact));
                if let Err(error) = &result {
                    runtime.last_error = Some(error.clone());
                }
                Some(vec![result.unwrap_or_else(|error| error)])
            }
            "rollback" => {
                let response =
                    runtime
                        .model
                        .request_rollback(now_ms())
                        .map(|result| match result {
                            Fault::RolledBack { failed, active } => {
                                runtime.failed = runtime.active.take();
                                runtime.active = runtime.previous.take();
                                runtime.dirty = true;
                                format!("rolled back generation {failed} -> {active}")
                            }
                            Fault::Unrecoverable { failed } => {
                                format!("unrecoverable generation {failed}")
                            }
                        });
                Some(vec![response.unwrap_or_else(|error| {
                    format!("rollback unavailable: {error:?}")
                })])
            }
            "config" => {
                let action = parts.next().unwrap_or("show").to_ascii_lowercase();
                let response = match action.as_str() {
                    "show" if parts.next().is_none() => config_text(runtime.config),
                    "reload" if parts.next().is_none() => match read_config_file() {
                        Ok(text) => match crate::hot::parse_config(&text) {
                            Ok(config) => {
                                runtime.configure(config);
                                config_text(config)
                            }
                            Err(error) => format!("config rejected without changes: {error:?}"),
                        },
                        Err(error) => error,
                    },
                    "set" => {
                        let key = parts.next().unwrap_or("");
                        let value = parts.next().unwrap_or("");
                        if parts.next().is_some() {
                            "usage: config set <KEY> <value>".into()
                        } else {
                            match replace_config(runtime.config, &key.to_ascii_uppercase(), value) {
                                Ok(config) => {
                                    runtime.configure(config);
                                    config_text(config)
                                }
                                Err(error) => format!("config rejected without changes: {error:?}"),
                            }
                        }
                    }
                    _ => "usage: config show | config reload | config set <KEY> <value>".into(),
                };
                Some(response.lines().map(str::to_string).collect())
            }
            _ => None,
        }
    }

    pub(super) fn smoke_impl(paths: &[String], rounds: usize) -> Result<Vec<String>, String> {
        if paths.len() < 2 || rounds == 0 || rounds > 64 {
            return Err("hot-smoke requires at least two DLL paths and 1..64 rounds".into());
        }
        let host = crate::raw::hostapi::smoke_host_api();
        let mut previous: Option<ImageGeneration> = None;
        let mut output = Vec::new();
        for generation in 0..rounds {
            let path = &paths[generation % paths.len()];
            let image = FeatureImage::load(path)
                .map_err(|error| format!("smoke generation {generation}: {error}"))?;
            let api = image.api();
            let base = image.base();
            // SAFETY: the mapper returned a checked v1 table in its owned image.
            let status = unsafe { ((*api).init)(&host, generation as u64) };
            if status != STATUS_OK {
                let error = format!("smoke generation {generation}: init status {status}");
                let _ = shutdown_retire(ImageGeneration {
                    image,
                    host: &host,
                    sha256: String::new(),
                });
                return Err(error);
            }
            let command = b"feature";
            // SAFETY: canary takes a borrowed read-only command and does not
            // replace the mapper-owned image during this invocation.
            let marker = unsafe { ((*api).dispatch)(command.as_ptr() as *const i8, command.len()) };
            if marker != STATUS_OK {
                let error = format!("smoke generation {generation}: marker status {marker}");
                let _ = shutdown_retire(ImageGeneration {
                    image,
                    host: &host,
                    sha256: String::new(),
                });
                return Err(error);
            }
            if let Some(old) = previous.take() {
                let old_version = generation - 1;
                shutdown_retire(old)
                    .map_err(|error| format!("retire smoke generation {old_version}: {error}"))?;
                output.push(format!("smoke: retired generation {old_version}"));
            }
            output.push(format!(
                "smoke: generation {generation} active at {base:#x}"
            ));
            previous = Some(ImageGeneration {
                image,
                host: &host,
                sha256: String::new(),
            });
        }
        if previous.is_some() {
            output.push("smoke: final shutdown/retire".into());
            shutdown_retire(previous.take().expect("checked above"))?;
        }
        Ok(output)
    }
}

pub fn tick() {
    #[cfg(target_os = "windows")]
    imp::tick_impl();
}

/// Mapper/ABI-only A/B test with no Unity dependencies. The only invoked
/// feature command is the real `feature` canary; no native game probe is faked.
pub fn smoke(paths: &[String], rounds: usize) -> Result<Vec<String>, String> {
    #[cfg(target_os = "windows")]
    return imp::smoke_impl(paths, rounds);
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (paths, rounds);
        Err("hot-smoke requires Windows".into())
    }
}

/// Route one non-resident command through the active generation. `false` means
/// there is no replaceable feature available and the established built-in
/// console should execute the command as before.
pub fn resident_command(command: &str) -> bool {
    matches!(
        command
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "gct"
            | "threads"
            | "gctfix"
            | "gctresume"
            | "loopc"
            | "loop"
            | "latec"
            | "latehook"
            | "late"
            | "panic"
            | "av"
            | "seal"
            | "quit"
    )
}

pub fn dispatch(line: &str) -> bool {
    #[cfg(target_os = "windows")]
    return imp::dispatch_impl(line);
    #[cfg(not(target_os = "windows"))]
    {
        let _ = line;
        false
    }
}

/// Resident reload/config commands are never forwarded to a feature module.
pub fn command(line: &str) -> Option<Vec<String>> {
    #[cfg(target_os = "windows")]
    return imp::command_impl(line);
    #[cfg(not(target_os = "windows"))]
    {
        let _ = line;
        None
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn resident_commands_never_fallback_to_a_feature_image() {
        for command in [
            "gct",
            "threads",
            "gctfix",
            "gctresume",
            "loopc",
            "loop",
            "latec",
            "latehook",
            "late",
            "panic",
            "av",
            "seal",
            "quit",
        ] {
            assert!(
                super::resident_command(command),
                "feature can steal {command}"
            );
        }
        for command in [
            "feature", "pos", "who", "esp", "m3c", "q", "guard", "unknown",
        ] {
            assert!(!super::resident_command(command), "feature loses {command}");
        }
    }

    #[test]
    fn versioned_feature_names_are_strict_and_do_not_accept_traversal() {
        let parsed = super::parse_versioned_name("features-42-0123456789ab.dll").unwrap();
        assert_eq!(parsed.generation, 42);
        assert_eq!(parsed.short_sha, "0123456789ab");
        for bad in [
            "../features-42-0123456789ab.dll",
            "features--1-0123456789ab.dll",
            "features-0-0123456789ab.dll",
            "features-42-0123456789a.dll",
            "features-42-0123456789az.dll",
            "features-42-0123456789ab.dll.exe",
            "features-42-0123456789ab/dll",
        ] {
            assert!(super::parse_versioned_name(bad).is_none(), "accepted {bad}");
        }
    }

    #[test]
    fn config_set_preserves_unrelated_values_and_rejects_the_whole_bad_update() {
        let original = crate::hot::HotConfig {
            tick_ms: 123,
            telemetry: false,
            ..Default::default()
        };
        let updated = super::replace_config(original, "M3", "0").unwrap();
        assert_eq!(updated.tick_ms, 123);
        assert!(!updated.telemetry);
        assert!(!updated.m3);
        assert!(super::replace_config(original, "UNKNOWN", "1").is_err());
        assert_eq!(original.tick_ms, 123);
    }
}
