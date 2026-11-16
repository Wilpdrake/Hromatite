//! Command routing and feature-owned generation state.

use crate::host::Host;
use crate::m3::{Cache as MetadataCache, Metadata};
use crate::memory::{bytes, decode_hex, parse_addr, qwords};
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
}

impl Feature {
    pub fn new(host: HostApiV1, generation: u64) -> Self {
        Self {
            host: Host::new(host),
            generation,
            telemetry: true,
            memory: true,
            m3: true,
            metadata_cache: MetadataCache::default(),
        }
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
        if M3_COMMANDS.contains(&command.as_str()) {
            let (lines, status) = {
                let mut metadata = Metadata::new(&self.host, &mut self.metadata_cache);
                crate::m3::dispatch(&mut metadata, &command, a1, a2, rest)
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

fn help_lines() -> Vec<String> {
    [
        "feature                        — feature generation / ABI canary",
        "pos                            — Camera.main -> transform -> position",
        "who                            — players through VRCPlayerApi",
        "esp                            — projected players (overlay contract)",
        "q <addr> [n]                   — qwords with ascii and module tags",
        "b <addr> [n]                   — bytes in hex",
        "s <addr> / ws <addr>           — ASCII / UTF-16 strings",
        "m3f <ns> [name]                — classes in the verified class table",
        "m3s <substr>                   — materialised class-name search",
        "m3l <asm> [substr] [max]       — classes in an IL2CPP image",
        "m3c <class>                    — class fields and methods",
        "m3i <assembly>                 — resolve an IL2CPP image",
        "call <addr> [a1..a3]           — native call through the host guard",
        "guard <addr>                   — no-argument host-guarded probe",
        "wq <addr> <hex> / wb <addr> <hexbytes> — host-mediated writes",
        "gct/threads/loop/late/seal/quit — resident runtime commands",
    ]
    .iter()
    .map(|line| (*line).to_string())
    .collect()
}
