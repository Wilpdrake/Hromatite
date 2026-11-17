//! Command catalogue shared by completion, the interactive palette and the
//! risk gate.
//!
//! The table itself lives in `hromatite_commands`, the same crate the resident
//! runtime and the hot-reloaded feature image build their in-game `help` from.
//! Nothing is copied by hand here: adding a command to that table updates the
//! payload help, the palette, Tab completion and the confirmation rules at once.

pub use hromatite_commands::{Risk, SPECS, Scope, Spec};

/// Historical name for the palette row type; the data now comes from the
/// payload's own table.
pub type CommandSpec = Spec;

/// Resolve a typed name (or alias) to its declaration.
pub fn command(name: &str) -> Option<&'static Spec> {
    hromatite_commands::find(name)
}

/// Every accepted spelling, for completion.
pub fn names() -> Vec<&'static str> {
    hromatite_commands::names()
}

#[cfg(test)]
mod tests {
    use super::{Risk, SPECS, command, names};

    #[test]
    fn every_listed_command_resolves_and_keeps_its_risk() {
        for spec in SPECS {
            assert_eq!(command(spec.name).unwrap().name, spec.name);
            assert!(!spec.summary.is_empty(), "{} has no summary", spec.name);
            assert!(!spec.usage.is_empty(), "{} has no usage", spec.name);
        }
        assert_eq!(command("pos").unwrap().risk, Risk::ReadOnly);
        assert_eq!(command("q").unwrap().risk, Risk::Advanced);
        assert_eq!(command("wq").unwrap().risk, Risk::Dangerous);
        assert_eq!(command("call").unwrap().risk, Risk::Dangerous);
        assert_eq!(command("?").unwrap().name, "help");
        assert_eq!(command("qword").unwrap().name, "q");
        assert!(command("no-such-command").is_none());
    }

    #[test]
    fn completion_and_palette_see_the_same_generated_names() {
        let listed = names();
        for spec in SPECS {
            assert!(
                listed.contains(&spec.name),
                "{} missing from names",
                spec.name
            );
            for alias in spec.aliases {
                assert!(listed.contains(alias), "alias {alias} missing from names");
                assert_eq!(command(alias).unwrap().name, spec.name);
            }
        }
        // Every name the shell offers must be a palette entry or its alias, so
        // Tab can never complete something the palette cannot describe.
        for name in listed {
            assert!(command(name).is_some(), "completion offers unknown {name}");
        }
    }

    #[test]
    fn the_console_palette_covers_feature_only_commands_too() {
        // These used to be missing because the palette was typed by hand.
        for name in ["feature", "eac.status", "eac.hooks"] {
            assert!(command(name).is_some(), "{name} is not in the palette");
        }
    }
}
