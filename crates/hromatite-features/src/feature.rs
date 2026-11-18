//! Command routing and feature-owned generation state.

use crate::eac::{EacFeature, INTERVAL_RANGE};
use crate::host::Host;
use crate::m3::{Cache as MetadataCache, Metadata};
use crate::memory::{bytes, decode_hex, parse_addr, qwords};
use crate::render::Renderer;
use crate::telemetry::Cache as TelemetryCache;
use hromatite_abi::{
    ABI_VERSION, HostApiV1, STATUS_BAD_ARGUMENT, STATUS_ERROR, STATUS_NOT_READY, STATUS_OK,
};

pub struct Feature {
    pub host: Host,
    pub generation: u64,
    telemetry: bool,
    memory: bool,
    m3: bool,
    metadata_cache: MetadataCache,
    telemetry_cache: TelemetryCache,
    eac: EacFeature,
    render: Renderer,
}

impl Feature {
    pub fn new(host: HostApiV1, generation: u64) -> Self {
        let mut render = Renderer::default();
        // The wrapper exports this from `~/.hromatite-mode`, so the overlay can
        // come up with the game instead of waiting for a console command.
        if std::env::var("HROMATITE_DRAW").is_ok_and(|value| value.trim() == "1") {
            render.set(true);
        }
        Self {
            host: Host::new(host),
            generation,
            telemetry: true,
            memory: true,
            m3: true,
            metadata_cache: MetadataCache::default(),
            telemetry_cache: TelemetryCache::default(),
            eac: EacFeature::new(),
            render,
        }
    }

    /// Throttled managed tick: anti-cheat housekeeping (heartbeat cadence, the
    /// game-log mirror) and command routing. Bounded file I/O lives here and
    /// never on the per-frame path.
    pub fn tick(&mut self) {
        for line in self.eac.tick() {
            self.host.log(&line);
        }
    }

    /// Per-frame managed work on the same game thread: one draw call for the
    /// in-game overlay. No file I/O, no command routing, bounded game calls.
    pub fn frame(&mut self) {
        if !self.render.enabled() {
            return;
        }
        let Some(mut metadata) = Metadata::new(&self.host, &mut self.metadata_cache) else {
            return;
        };
        self.render.frame(&mut metadata, &mut self.telemetry_cache);
    }

    pub fn respond(&self, lines: impl IntoIterator<Item = String>, status: i32) -> i32 {
        for line in lines {
            self.host.log(&line);
        }
        status
    }

    fn error(&self, message: String) -> i32 {
        self.respond([message], STATUS_BAD_ARGUMENT)
    }

    fn disabled(&self, command: &str) -> i32 {
        self.respond(
            [format!("{command}: disabled by configuration")],
            STATUS_NOT_READY,
        )
    }

    pub fn configure(&mut self, key: &str, value: &str) -> i32 {
        // Value-carrying anti-cheat keys are handled before the 0/1 switch so a
        // path or an interval never has to pretend to be a boolean.
        match key {
            "eac_log" => {
                return match self.eac.set_log(value) {
                    Ok(path) => self.respond(
                        [format!("configure: eac_log={}", path.display())],
                        STATUS_OK,
                    ),
                    Err(error) => {
                        self.respond([format!("configure: eac_log: {error}")], STATUS_NOT_READY)
                    }
                };
            }
            "eac_tail" => {
                return match self.eac.set_tail(value) {
                    Ok(path) => self.respond(
                        [format!("configure: eac_tail={}", path.display())],
                        STATUS_OK,
                    ),
                    Err(error) => {
                        self.respond([format!("configure: eac_tail: {error}")], STATUS_NOT_READY)
                    }
                };
            }
            "eac_interval_ms" => {
                let Ok(requested) = value.trim().parse::<u64>() else {
                    return self.respond(
                        [format!(
                            "configure: eac_interval_ms expects milliseconds, got '{value}'"
                        )],
                        STATUS_NOT_READY,
                    );
                };
                let applied = self.eac.set_interval_ms(requested);
                let note = if applied == requested {
                    String::new()
                } else {
                    format!(" (clamped to {}-{})", INTERVAL_RANGE.0, INTERVAL_RANGE.1)
                };
                return self.respond(
                    [format!("configure: eac_interval_ms={applied}{note}")],
                    STATUS_OK,
                );
            }
            _ => {}
        }

        let enabled = match value {
            "0" => false,
            "1" => true,
            _ => {
                return self.respond(
                    [format!("configure: {key} expects 0 or 1")],
                    STATUS_NOT_READY,
                );
            }
        };
        match key {
            "telemetry" => self.telemetry = enabled,
            "memory" => self.memory = enabled,
            "m3" => self.m3 = enabled,
            "eac" => self.eac.set_enabled(enabled),
            "draw" => self.render.set(enabled),
            "eac_auto" => self.eac.set_auto(enabled),
            "eac_autosession" => self.eac.set_autosession(enabled),
            "eac_hooks" => self.eac.set_hooks_enabled(enabled),
            _ => {
                return self.respond(
                    [format!("configure: unknown key '{key}'")],
                    STATUS_NOT_READY,
                );
            }
        }
        self.respond([format!("configure: {key}={enabled}")], STATUS_OK)
    }

    pub fn execute(&mut self, line: &str) -> i32 {
        let line = line.trim();
        let mut parts = line.splitn(4, char::is_whitespace);
        let command = parts.next().unwrap_or("").to_ascii_lowercase();
        let a1 = parts.next().unwrap_or("");
        let a2 = parts.next().unwrap_or("");
        let rest = parts.next().unwrap_or("");

        // One token (`eac.status`), never a bare prefix match on `eac`.
        if command == "eac" || command.starts_with("eac.") {
            if command != "eac" && !self.eac.enabled() {
                return self.disabled(&command);
            }
            let arg = [a1, a2, rest]
                .iter()
                .filter(|part| !part.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(" ");
            let (lines, status) = self.eac.dispatch(&command, arg.trim());
            return self.respond(lines, status);
        }

        const MEMORY_COMMANDS: &[&str] = &[
            "q", "qword", "b", "bytes", "s", "ws", "wq", "wb", "guard", "call",
        ];
        const M3_COMMANDS: &[&str] = &["m3f", "m3s", "m3l", "m3c", "m3i"];
        if MEMORY_COMMANDS.contains(&command.as_str()) && !self.memory {
            return self.disabled(&command);
        }
        if M3_COMMANDS.contains(&command.as_str()) && !self.m3 {
            return self.disabled(&command);
        }
        if matches!(command.as_str(), "pos" | "who" | "esp") && !self.telemetry {
            return self.disabled(&command);
        }
        if M3_COMMANDS.contains(&command.as_str())
            || matches!(command.as_str(), "pos" | "who" | "esp")
        {
            let Some(mut metadata) = Metadata::new(&self.host, &mut self.metadata_cache) else {
                return self.respond(
                    [format!(
                        "{command}: no metadata layout for GameAssembly {}",
                        self.host.game_sha_hex()
                    )],
                    STATUS_NOT_READY,
                );
            };
            let (lines, status) = if M3_COMMANDS.contains(&command.as_str()) {
                if matches!(command.as_str(), "pos" | "who" | "esp") && !self.telemetry {
                    return self.disabled(&command);
                }
                crate::m3::dispatch(&mut metadata, &command, a1, a2, rest)
            } else {
                crate::telemetry::dispatch(&mut metadata, &mut self.telemetry_cache, &command)
            };
            return self.respond(lines, status);
        }

        match command.as_str() {
            "" => STATUS_OK,
            "feature" => self.respond(
                [format!(
                    "feature: generation={} abi={ABI_VERSION}",
                    self.generation
                )],
                STATUS_OK,
            ),
            "help" | "?" => self.respond(help_lines(), STATUS_OK),
            // In-game ESP: the game's own renderer draws it, see `render.rs`.
            "draw" => match a1.to_ascii_lowercase().as_str() {
                "" | "status" => self.respond(self.render.status(), STATUS_OK),
                "probe" => {
                    let lines = {
                        let Some(mut metadata) = Metadata::new(&self.host, &mut self.metadata_cache)
                        else {
                            return self.respond(
                                [format!(
                                    "draw probe: no metadata layout for GameAssembly {}",
                                    self.host.game_sha_hex()
                                )],
                                STATUS_NOT_READY,
                            );
                        };
                        self.render.probe(&mut metadata)
                    };
                    self.respond(lines, STATUS_OK)
                }
                "on" | "1" => {
                    self.render.set(true);
                    self.respond(
                        ["draw: on — the mesh is built on the next game frame".to_string()],
                        STATUS_OK,
                    )
                }
                "off" | "0" => {
                    self.render.set(false);
                    self.respond(["draw: off".to_string()], STATUS_OK)
                }
                other => self.error(format!("usage: draw [on|off|status|probe] (got '{other}')")),
            },
            "q" | "qword" => match parse_addr(a1) {
                Some(address) => {
                    let count = a2.parse::<usize>().unwrap_or(8);
                    self.respond(qwords(&self.host, address, count), STATUS_OK)
                }
                None => self.error("usage: q <addr> [n]".into()),
            },
            "b" | "bytes" => match parse_addr(a1) {
                Some(address) => {
                    let count = a2.parse::<usize>().unwrap_or(32);
                    self.respond([bytes(&self.host, address, count)], STATUS_OK)
                }
                None => self.error("usage: b <addr> [n]".into()),
            },
            "s" => match parse_addr(a1) {
                Some(address) => self.respond(
                    [format!(
                        "{address:#x}: '{}'",
                        self.host.ascii_lossy(address, 128)
                    )],
                    STATUS_OK,
                ),
                None => self.error("usage: s <addr>".into()),
            },
            "ws" => match parse_addr(a1) {
                Some(address) => self.respond(
                    [format!(
                        "{address:#x}: '{}'",
                        self.host.utf16_lossy(address, 128)
                    )],
                    STATUS_OK,
                ),
                None => self.error("usage: ws <addr>".into()),
            },
            "wq" => match (parse_addr(a1), parse_addr(a2)) {
                (Some(address), Some(value)) => {
                    let ok = self.host.write_usize(address, value);
                    let now = self.host.read_usize(address).unwrap_or(0);
                    self.respond(
                        [format!(
                            "wq {address:#x} <- {value:#x}: {ok} (now {now:#x})"
                        )],
                        if ok { STATUS_OK } else { STATUS_ERROR },
                    )
                }
                _ => self.error("usage: wq <addr> <hex value>".into()),
            },
            "wb" => {
                let text = format!("{a2} {rest}");
                match (parse_addr(a1), decode_hex(text.trim())) {
                    (Some(address), Some(data)) => {
                        let len = data.len();
                        let ok = self.host.write_bytes(address, &data);
                        self.respond(
                            [format!("wb {address:#x} <- {len} byte(s): {ok}")],
                            if ok { STATUS_OK } else { STATUS_ERROR },
                        )
                    }
                    _ => self.error("usage: wb <addr> <hex bytes, e.g. 0100ff>".into()),
                }
            }
            "guard" => match parse_addr(a1).filter(|address| *address != 0) {
                Some(address) => match self.host.call(address, &[]) {
                    Ok(value) => self.respond(
                        [format!("guard {address:#x}: returned {value:#x}")],
                        STATUS_OK,
                    ),
                    Err(status) => self.respond(
                        [format!("guard {address:#x}: host fault status {status}")],
                        STATUS_ERROR,
                    ),
                },
                None => self.error("usage: guard <addr>".into()),
            },
            "call" => {
                let tokens: Vec<&str> = line.split_whitespace().skip(1).collect();
                let parsed = tokens
                    .iter()
                    .map(|token| parse_addr(token))
                    .collect::<Option<Vec<_>>>();
                match parsed.filter(|args| !args.is_empty() && args.len() <= 4) {
                    Some(args) => {
                        let function = args[0];
                        let native_args = &args[1..];
                        let value = self.host.call(function, native_args);
                        let shown = [
                            native_args.first().copied().unwrap_or(0),
                            native_args.get(1).copied().unwrap_or(0),
                            native_args.get(2).copied().unwrap_or(0),
                        ];
                        let mut lines = vec![format!(
                            "call {function:#x}({:#x}, {:#x}, {:#x})",
                            shown[0], shown[1], shown[2]
                        )];
                        match value {
                            Ok(value) => {
                                lines.push(format!("  -> {value:#x}"));
                                self.respond(lines, STATUS_OK)
                            }
                            Err(status) => {
                                lines.push(format!("  fault: host status {status}"));
                                self.respond(lines, STATUS_ERROR)
                            }
                        }
                    }
                    None => self.error("usage: call <addr> [a1] [a2] [a3]".into()),
                }
            }
            // Resident-only facilities are explicit rather than falling through
            // to an alarming unknown-command response in a routed console.
            "gct" | "threads" | "gctfix" | "gctresume" | "loopc" | "loop" | "latec"
            | "latehook" | "late" | "panic" | "av" | "seal" | "quit" => self.respond(
                [format!(
                    "{command}: resident-only command (feature image did not execute it)"
                )],
                STATUS_NOT_READY,
            ),
            other => self.error(format!("unknown command: {other} (try help)")),
        }
    }
}

/// The feature image's `help`, generated from the same shared table the resident
/// runtime and the host TUI use. Only commands this image implements itself are
/// listed; resident-only names are refused with an explicit message instead.
fn help_lines() -> Vec<String> {
    hromatite_commands::help_lines(hromatite_commands::Scope::Feature)
}
