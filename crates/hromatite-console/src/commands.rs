//! Payload command catalogue shared by completion and the interactive palette.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    ReadOnly,
    Advanced,
    Dangerous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandSpec {
    pub name: &'static str,
    pub usage: &'static str,
    pub summary: &'static str,
    pub risk: Risk,
}

impl CommandSpec {
    pub fn takes_arguments(self) -> bool {
        self.usage.contains(' ')
    }
}

macro_rules! commands {
    ($(($name:literal, $usage:literal, $summary:literal, $risk:ident)),+ $(,)?) => {
        pub const CATALOG: &[CommandSpec] = &[
            $(CommandSpec { name: $name, usage: $usage, summary: $summary, risk: Risk::$risk }),+
        ];
    };
}

// Read-only operational commands stay at the top. Everything here describes an
// existing payload command; the host console adds no new in-game capability.
commands![
    (
        "pos",
        "pos",
        "camera position and local transform",
        ReadOnly
    ),
    (
        "who",
        "who",
        "list players, ids and world positions",
        ReadOnly
    ),
    (
        "esp",
        "esp",
        "project players into screen coordinates",
        ReadOnly
    ),
    ("late", "late", "managed LateUpdate tick health", ReadOnly),
    ("gct", "gct", "Boehm GC thread records", ReadOnly),
    (
        "threads",
        "threads",
        "process threads and start addresses",
        ReadOnly
    ),
    ("help", "help", "payload command reference", ReadOnly),
    ("?", "?", "alias for payload command reference", ReadOnly),
    (
        "q",
        "q <addr> [count]",
        "read qwords at an address",
        Advanced
    ),
    (
        "qword",
        "qword <addr> [count]",
        "alias for qword reads",
        Advanced
    ),
    (
        "b",
        "b <addr> [count]",
        "read bytes at an address",
        Advanced
    ),
    (
        "bytes",
        "bytes <addr> [count]",
        "alias for byte reads",
        Advanced
    ),
    ("s", "s <addr>", "read an ASCII string", Advanced),
    ("ws", "ws <addr>", "read a UTF-16 string", Advanced),
    (
        "m3f",
        "m3f <namespace> [name]",
        "find materialized classes",
        Advanced
    ),
    (
        "m3c",
        "m3c <class>",
        "inspect class fields and methods",
        Advanced
    ),
    (
        "m3l",
        "m3l <assembly> [filter] [max]",
        "enumerate an assembly",
        Advanced
    ),
    (
        "m3s",
        "m3s <filter>",
        "search materialized class names",
        Advanced
    ),
    ("m3i", "m3i <assembly>", "resolve an IL2CPP image", Advanced),
    (
        "latec",
        "latec",
        "list valid LateUpdate candidates",
        Advanced
    ),
    ("gctfix", "gctfix", "repair stale Boehm records", Dangerous),
    (
        "gctresume",
        "gctresume",
        "resume GC records after inspection",
        Dangerous
    ),
    (
        "latehook",
        "latehook <#index|class>",
        "replace the managed tick anchor",
        Dangerous
    ),
    (
        "guard",
        "guard <addr>",
        "probe guarded exception handling",
        Dangerous
    ),
    (
        "call",
        "call <addr> [args]",
        "invoke a native function",
        Dangerous
    ),
    (
        "wq",
        "wq <addr> <hex64>",
        "write one qword to game memory",
        Dangerous
    ),
    (
        "wb",
        "wb <addr> <hexbytes>",
        "write bytes to game memory",
        Dangerous
    ),
    (
        "panic",
        "panic [message]",
        "intentionally panic the payload command",
        Dangerous
    ),
    (
        "av",
        "av [addr]",
        "intentionally trigger an access violation",
        Dangerous
    ),
    ("seal", "seal", "request payload image sealing", Dangerous),
    ("quit", "quit", "stop the payload console", Dangerous),
];

pub fn command(name: &str) -> Option<&'static CommandSpec> {
    CATALOG.iter().find(|item| item.name == name)
}

#[cfg(test)]
mod tests {
    use super::{CATALOG, Risk, command};

    #[test]
    fn useful_commands_lead_and_destructive_commands_are_explicit() {
        assert_eq!(CATALOG[0].name, "pos");
        assert_eq!(CATALOG[1].name, "who");
        assert_eq!(command("who").unwrap().risk, Risk::ReadOnly);
        assert_eq!(command("q").unwrap().risk, Risk::Advanced);
        assert_eq!(command("wq").unwrap().risk, Risk::Dangerous);
        assert_eq!(command("call").unwrap().risk, Risk::Dangerous);
        assert!(CATALOG.iter().all(|item| !item.summary.is_empty()));
        for name in crate::input::COMMANDS {
            assert!(
                command(name).is_some(),
                "palette lost payload command {name}"
            );
        }
    }
}
