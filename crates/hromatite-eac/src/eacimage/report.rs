//! The `eac.module` command: pick a view of the address space and turn what it
//! finds into log lines.
//!
//! The command is a diagnostic, so every branch answers with text: a view that
//! finds nothing says so, and the Windows view is only a fallback for the case
//! where `maps` cannot name the image at all.

use super::*;

/// `eac.module [pages] [read=<bytes>] [imports=<n>]`: locate the in-process
/// ELF image(s) and describe them. Read-only; never writes a byte.
#[cfg(feature = "runtime")]
pub fn command(arg: &str) -> Vec<String> {
    let probe = Probe::parse(arg);
    #[cfg(windows)]
    {
        // `win` = the Windows view (VirtualQuery, blind to native mappings);
        // `self` = the `proc/self` view (which names the wineserver, see above);
        // default = pid + `maps` + direct reads, the only view that finds it.
        if arg.split_whitespace().any(|token| token == "win") {
            return live::probe_command(&probe);
        }
        if arg.split_whitespace().any(|token| token == "self") {
            return locate_unix(&probe);
        }
        let mut lines = locate_native(&probe);
        if !lines.iter().any(|line| line.contains("native ELF image")) {
            lines.push("eac.module: falling back to the Windows view (VirtualQuery)".to_string());
            lines.extend(live::probe_command(&probe));
        }
        lines
    }
    #[cfg(not(windows))]
    {
        let _ = probe;
        vec!["eac.module: host target has no address space to walk (build for x86_64-pc-windows-gnu)".to_string()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "runtime")]
    #[test]
    fn the_module_command_always_answers_and_never_touches_memory_on_the_host() {
        let lines = command("pages read=1m typo");
        assert!(!lines.is_empty(), "the command must print something");
        #[cfg(not(windows))]
        assert!(
            lines[0].contains("host target"),
            "host builds cannot walk an address space: {lines:?}"
        );
    }
}
