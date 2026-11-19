//! Single source of truth for every payload command.
//!
//! The resident console, the feature image and the host TUI all render this
//! table, so a command is declared exactly once:
//!
//! * `help` inside the game (resident runtime and feature image),
//! * the host TUI command palette and its risk gate,
//! * the host TUI Tab completion.
//!
//! Adding an entry here is the only edit those four views need. Nothing may be
//! maintained by hand anywhere else — a second list is a list that goes stale.
#![forbid(unsafe_code)]

/// How much damage a command can do if it is mistyped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    ReadOnly,
    Advanced,
    Dangerous,
}

/// Which image implements the command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Resident runtime only; a hot-reloaded feature image refuses it.
    Resident,
    /// Resident runtime and feature image both implement it.
    Both,
    /// Feature image only; the resident runtime does not know the name.
    Feature,
}

/// One command as it is described everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spec {
    /// Primary name typed at the prompt.
    pub name: &'static str,
    /// Extra names that resolve to the same handler.
    pub aliases: &'static [&'static str],
    /// Argument template. `<...>` marks a required argument.
    pub usage: &'static str,
    /// Host-side description (English) used by the TUI palette search.
    pub summary: &'static str,
    /// In-game `help` text (Russian), appended after the usage column.
    pub note: &'static str,
    pub risk: Risk,
    pub scope: Scope,
}

impl Spec {
    /// Optional arguments do not need a template detour: selecting
    /// `reload [generation]` still follows the normal two-Enter risk flow.
    pub fn takes_arguments(self) -> bool {
        self.usage.contains('<')
    }

    /// Whether typing `name` at the prompt reaches this command.
    pub fn answers_to(self, name: &str) -> bool {
        self.name == name || self.aliases.contains(&name)
    }

    /// The `help` line for this command: usage column, then the note.
    pub fn help_line(self) -> String {
        format!("{:<30} — {}", self.usage, self.note)
    }
}

/// Every payload command, in the order the in-game `help` prints them.
pub const SPECS: &[Spec] = &[
    Spec {
        name: "help",
        aliases: &["?"],
        usage: "help",
        summary: "open the field manual",
        note: "этот список",
        risk: Risk::ReadOnly,
        scope: Scope::Both,
    },
    Spec {
        name: "pos",
        aliases: &[],
        usage: "pos",
        summary: "camera position and local transform",
        note: "Camera.main -> transform -> position",
        risk: Risk::ReadOnly,
        scope: Scope::Both,
    },
    Spec {
        name: "who",
        aliases: &[],
        usage: "who",
        summary: "players, ids and world positions",
        note: "игроки через VRCPlayerApi",
        risk: Risk::ReadOnly,
        scope: Scope::Both,
    },
    Spec {
        name: "esp",
        aliases: &[],
        usage: "esp",
        summary: "project players into screen coordinates",
        note: "игроки в пикселях экрана (overlay contract)",
        risk: Risk::ReadOnly,
        scope: Scope::Both,
    },
    Spec {
        name: "draw",
        aliases: &[],
        usage: "draw [on|off|status|probe]",
        summary: "in-game ESP drawn by the game's own renderer",
        note: "оверлей внутри кадра игры: сетка/боксы рисует сама Unity (без хука графики)",
        risk: Risk::ReadOnly,
        scope: Scope::Feature,
    },
    Spec {
        name: "q",
        aliases: &["qword"],
        usage: "q <addr> [count]",
        summary: "read qwords at an address",
        note: "n qword с ascii и меткой модуля",
        risk: Risk::Advanced,
        scope: Scope::Both,
    },
    Spec {
        name: "b",
        aliases: &["bytes"],
        usage: "b <addr> [count]",
        summary: "read bytes at an address",
        note: "n байт в hex",
        risk: Risk::Advanced,
        scope: Scope::Both,
    },
    Spec {
        name: "s",
        aliases: &[],
        usage: "s <addr>",
        summary: "read an ASCII string",
        note: "ascii-строка",
        risk: Risk::Advanced,
        scope: Scope::Both,
    },
    Spec {
        name: "ws",
        aliases: &[],
        usage: "ws <addr>",
        summary: "read a UTF-16 string",
        note: "utf-16 строка",
        risk: Risk::Advanced,
        scope: Scope::Both,
    },
    Spec {
        name: "gct",
        aliases: &[],
        usage: "gct [rva]",
        summary: "Boehm GC thread records",
        note: "таблица потоков Boehm",
        risk: Risk::ReadOnly,
        scope: Scope::Resident,
    },
    Spec {
        name: "threads",
        aliases: &[],
        usage: "threads",
        summary: "process threads and start addresses",
        note: "потоки процесса и записи GC",
        risk: Risk::ReadOnly,
        scope: Scope::Resident,
    },
    Spec {
        name: "ui",
        aliases: &[],
        usage: "ui [status]",
        summary: "in-game nuklear UI drawn through Present",
        note: "HROMATITE_UI=1 включает оверлей nuklear (хук Present)",
        risk: Risk::ReadOnly,
        scope: Scope::Resident,
    },
    Spec {
        name: "gctfix",
        aliases: &[],
        usage: "gctfix",
        summary: "repair stale Boehm records",
        note: "вычеркнуть мёртвые записи Boehm",
        risk: Risk::Dangerous,
        scope: Scope::Resident,
    },
    Spec {
        name: "gctresume",
        aliases: &[],
        usage: "gctresume",
        summary: "resume GC records after inspection",
        note: "ResumeThread по записям GC",
        risk: Risk::Dangerous,
        scope: Scope::Resident,
    },
    Spec {
        name: "call",
        aliases: &[],
        usage: "call <addr> [args]",
        summary: "invoke a native function",
        note: "вызов функции под VEH",
        risk: Risk::Dangerous,
        scope: Scope::Both,
    },
    Spec {
        name: "wq",
        aliases: &[],
        usage: "wq <addr> <hex64>",
        summary: "write one qword to game memory",
        note: "записать qword",
        risk: Risk::Dangerous,
        scope: Scope::Both,
    },
    Spec {
        name: "wb",
        aliases: &[],
        usage: "wb <addr> <hexbytes>",
        summary: "write bytes to game memory",
        note: "записать байты",
        risk: Risk::Dangerous,
        scope: Scope::Both,
    },
    Spec {
        name: "guard",
        aliases: &[],
        usage: "guard <addr>",
        summary: "probe guarded exception handling",
        note: "диагностический вызов под VEH",
        risk: Risk::Dangerous,
        scope: Scope::Both,
    },
    Spec {
        name: "m3f",
        aliases: &[],
        usage: "m3f <namespace> [name]",
        summary: "find materialized classes",
        note: "классы из verified class table",
        risk: Risk::Advanced,
        scope: Scope::Both,
    },
    Spec {
        name: "m3s",
        aliases: &[],
        usage: "m3s <filter>",
        summary: "search materialized class names",
        note: "поиск материализованных классов",
        risk: Risk::Advanced,
        scope: Scope::Both,
    },
    Spec {
        name: "m3l",
        aliases: &[],
        usage: "m3l <assembly> [filter] [max]",
        summary: "enumerate an assembly",
        note: "классы образа",
        risk: Risk::Advanced,
        scope: Scope::Both,
    },
    Spec {
        name: "m3c",
        aliases: &[],
        usage: "m3c <class>",
        summary: "inspect class fields and methods",
        note: "поля и методы класса",
        risk: Risk::Advanced,
        scope: Scope::Both,
    },
    Spec {
        name: "m3i",
        aliases: &[],
        usage: "m3i <assembly>",
        summary: "resolve an IL2CPP image",
        note: "Il2CppImage сборки",
        risk: Risk::Advanced,
        scope: Scope::Both,
    },
    Spec {
        name: "loopc",
        aliases: &[],
        usage: "loopc",
        summary: "native PlayerLoop root callbacks",
        note: "native PlayerLoop root callbacks",
        risk: Risk::ReadOnly,
        scope: Scope::Resident,
    },
    Spec {
        name: "loop",
        aliases: &[],
        usage: "loop",
        summary: "PlayerLoop hits, frequency and tid",
        note: "hits/частота/tid PlayerLoop",
        risk: Risk::ReadOnly,
        scope: Scope::Resident,
    },
    Spec {
        name: "latec",
        aliases: &[],
        usage: "latec",
        summary: "list valid LateUpdate candidates",
        note: "классы с LateUpdate",
        risk: Risk::Advanced,
        scope: Scope::Resident,
    },
    Spec {
        name: "latehook",
        aliases: &[],
        usage: "latehook <#index|class>",
        summary: "replace the managed tick anchor",
        note: "hook конкретного LateUpdate",
        risk: Risk::Dangerous,
        scope: Scope::Resident,
    },
    Spec {
        name: "late",
        aliases: &[],
        usage: "late",
        summary: "managed LateUpdate tick health",
        note: "hits/частота/tid LateUpdate",
        risk: Risk::ReadOnly,
        scope: Scope::Resident,
    },
    Spec {
        name: "feature",
        aliases: &[],
        usage: "feature",
        summary: "feature generation and ABI canary",
        note: "поколение фич и ABI canary",
        risk: Risk::ReadOnly,
        scope: Scope::Feature,
    },
    Spec {
        name: "eac.status",
        aliases: &[],
        usage: "eac.status",
        summary: "anti-cheat epoch, counters and paths",
        note: "эпоха, sid, счётчики, gap, пути",
        risk: Risk::ReadOnly,
        scope: Scope::Feature,
    },
    Spec {
        name: "eac.begin",
        aliases: &[],
        usage: "eac.begin [sid]",
        summary: "open an anti-cheat epoch per room",
        note: "открыть эпоху античита",
        risk: Risk::Advanced,
        scope: Scope::Feature,
    },
    Spec {
        name: "eac.tick",
        aliases: &[],
        usage: "eac.tick",
        summary: "emit one heartbeat now",
        note: "выпустить один heartbeat",
        risk: Risk::Advanced,
        scope: Scope::Feature,
    },
    Spec {
        name: "eac.end",
        aliases: &[],
        usage: "eac.end [reason]",
        summary: "close the epoch with a session_end record",
        note: "закрыть эпоху (session_end)",
        risk: Risk::Advanced,
        scope: Scope::Feature,
    },
    Spec {
        name: "eac.hooks",
        aliases: &[],
        usage: "eac.hooks [install|capture <path>|on|off]",
        summary: "EOSSDK hook status and capture",
        note: "статус и захват хуков EOSSDK",
        risk: Risk::Advanced,
        scope: Scope::Feature,
    },
    Spec {
        name: "eac.tap",
        aliases: &[],
        usage: "eac.tap [off|from <path>|<path>]",
        summary: "mirror the real heartbeat frames from the capture file",
        note: "зеркало реальных кадров из capture-файла",
        risk: Risk::ReadOnly,
        scope: Scope::Feature,
    },
    Spec {
        name: "eac.module",
        aliases: &[],
        usage: "eac.module [pages] [read=<bytes>]",
        summary: "locate the in-process ELF image of the anti-cheat client",
        note: "найти ELF-образ античита в процессе",
        risk: Risk::ReadOnly,
        scope: Scope::Resident,
    },
    Spec {
        name: "eac.log",
        aliases: &[],
        usage: "eac.log <path>",
        summary: "JSONL event log (32 MiB, two files)",
        note: "JSONL лог событий",
        risk: Risk::Advanced,
        scope: Scope::Feature,
    },
    Spec {
        name: "eac.tail",
        aliases: &[],
        usage: "eac.tail <path>",
        summary: "mirror output_log room markers",
        note: "зеркало output_log по комнатам",
        risk: Risk::Advanced,
        scope: Scope::Feature,
    },
    Spec {
        name: "eac.help",
        aliases: &["eac"],
        usage: "eac.help",
        summary: "anti-cheat command list",
        note: "список команд eac.*",
        risk: Risk::ReadOnly,
        scope: Scope::Feature,
    },
    Spec {
        name: "reload",
        aliases: &[],
        usage: "reload [generation]",
        summary: "activate latest or select an immutable feature generation",
        note: "атомарная смена feature DLL",
        risk: Risk::Dangerous,
        scope: Scope::Resident,
    },
    Spec {
        name: "rollback",
        aliases: &[],
        usage: "rollback",
        summary: "return to the previous feature generation",
        note: "вернуть предыдущее поколение",
        risk: Risk::Dangerous,
        scope: Scope::Resident,
    },
    Spec {
        name: "config",
        aliases: &[],
        usage: "config show|reload|set <key> <value>",
        summary: "inspect/reload config; config set requires confirmation",
        note: "hot-config без замены DLL",
        risk: Risk::Advanced,
        scope: Scope::Resident,
    },
    Spec {
        name: "panic",
        aliases: &[],
        usage: "panic [message]",
        summary: "intentionally panic the payload command",
        note: "проверка catch_unwind",
        risk: Risk::Dangerous,
        scope: Scope::Resident,
    },
    Spec {
        name: "av",
        aliases: &[],
        usage: "av [addr]",
        summary: "intentionally trigger an access violation",
        note: "проверка VEH",
        risk: Risk::Dangerous,
        scope: Scope::Resident,
    },
    Spec {
        name: "seal",
        aliases: &[],
        usage: "seal",
        summary: "stop the payload console (legacy name)",
        note: "выйти из интерактива",
        risk: Risk::Dangerous,
        scope: Scope::Resident,
    },
    Spec {
        name: "quit",
        aliases: &[],
        usage: "quit",
        summary: "stop the payload console",
        note: "выйти из интерактива",
        risk: Risk::Dangerous,
        scope: Scope::Resident,
    },
];

/// `help` lines for one image: the resident runtime lists everything it can
/// route; a feature image lists everything it implements itself.
pub fn help_lines(scope: Scope) -> Vec<String> {
    SPECS
        .iter()
        .filter(|spec| match scope {
            Scope::Resident => spec.scope != Scope::Feature,
            Scope::Feature => spec.scope != Scope::Resident,
            Scope::Both => true,
        })
        .map(|spec| spec.help_line())
        .collect()
}

/// Every accepted spelling: primary names first, in table order, then aliases.
pub fn names() -> Vec<&'static str> {
    let mut out = Vec::with_capacity(SPECS.len() + 8);
    out.extend(SPECS.iter().map(|spec| spec.name));
    for spec in SPECS {
        out.extend(spec.aliases.iter().copied());
    }
    out
}

/// Resolve a typed name (or one of its aliases) to its spec.
pub fn find(name: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|spec| spec.answers_to(name))
}

#[cfg(test)]
mod tests {
    use super::{Risk, SPECS, Scope, find, help_lines, names};
    use std::collections::BTreeSet;

    #[test]
    fn every_spelling_resolves_and_no_name_is_declared_twice() {
        let mut seen = BTreeSet::new();
        for name in names() {
            assert!(!name.is_empty());
            assert!(seen.insert(name), "duplicate command name: {name}");
            let spec = find(name).unwrap_or_else(|| panic!("unresolved name: {name}"));
            assert!(spec.answers_to(name));
        }
        assert_eq!(find("qword").unwrap().name, "q");
        assert_eq!(find("?").unwrap().name, "help");
        assert_eq!(find("eac").unwrap().name, "eac.help");
        assert!(find("no-such-command").is_none());
    }

    #[test]
    fn every_entry_is_described_for_both_audiences() {
        for spec in SPECS {
            assert!(!spec.usage.is_empty(), "{} has no usage", spec.name);
            assert!(!spec.summary.is_empty(), "{} has no summary", spec.name);
            assert!(!spec.note.is_empty(), "{} has no help note", spec.name);
            let line = spec.help_line();
            assert!(line.contains(spec.usage), "{} lost its usage", spec.name);
            assert!(line.contains("— "), "{} lost its separator", spec.name);
        }
    }

    /// `eac.module` reads EAC's own modules through live Win32 memory, so only
    /// the resident runtime may execute it; the feature image must not.
    #[test]
    fn module_report_is_a_resident_command() {
        let spec = find("eac.module").expect("eac.module must stay declared");
        assert_eq!(spec.scope, Scope::Resident);
        assert_eq!(spec.risk, Risk::ReadOnly);
        assert_eq!(spec.usage, "eac.module [pages] [read=<bytes>]");
        assert!(
            !help_lines(Scope::Feature)
                .iter()
                .any(|line| line.starts_with("eac.module ")),
            "the feature image must not advertise a resident-only command"
        );
        assert!(
            help_lines(Scope::Resident)
                .iter()
                .any(|line| line.starts_with("eac.module ")),
            "the resident runtime lost eac.module from its help"
        );
    }

    #[test]
    fn help_is_generated_per_image_and_hides_the_other_side() {
        let resident = help_lines(Scope::Resident);
        let feature = help_lines(Scope::Feature);
        assert!(
            resident.iter().any(|line| line.starts_with("reload ")),
            "resident help lost reload"
        );
        assert!(
            !resident.iter().any(|line| line.starts_with("eac.status")),
            "resident help must not advertise feature-only commands"
        );
        assert!(
            feature.iter().any(|line| line.starts_with("eac.status")),
            "feature help lost eac.status"
        );
        assert!(
            !feature.iter().any(|line| line.starts_with("reload ")),
            "feature help must not advertise resident-only commands"
        );
        // resident + feature − shared must equal the number of entries that
        // both images list, i.e. nothing may be dropped or duplicated.
        let shared = SPECS.iter().filter(|s| s.scope == Scope::Both).count();
        assert_eq!(
            resident.len() + feature.len() - shared,
            help_lines(Scope::Both).len()
        );
    }

    #[test]
    fn risk_levels_stay_conservative_for_writes_and_console_control() {
        assert_eq!(find("pos").unwrap().risk, Risk::ReadOnly);
        assert_eq!(find("q").unwrap().risk, Risk::Advanced);
        assert_eq!(find("wq").unwrap().risk, Risk::Dangerous);
        assert_eq!(find("call").unwrap().risk, Risk::Dangerous);
        assert_eq!(find("quit").unwrap().risk, Risk::Dangerous);
        for spec in SPECS {
            if spec.scope == Scope::Feature && matches!(spec.name, "eac.begin" | "eac.end") {
                assert_eq!(spec.risk, Risk::Advanced);
            }
        }
    }
}
