//! `hot-smoke <rounds> <dll-a> <dll-b> [...]` - mapper/ABI-only A/B loop.
//!
//! Runs the real local-map / canary / retire cycle of the resident hot-reload
//! router without Unity: the only invoked feature command is the real `feature`
//! canary, so nothing about the game is faked. Windows-only; it exists as its
//! own binary because the injector (`Hromatite.exe`) must not link the runtime.

use hromatite_win::log;

fn usage() {
    log::log_echo("usage: hot-smoke <rounds> <dll-a> <dll-b> [...]");
}

fn main() {
    log::install_panic_hook();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rounds = args.first().and_then(|v| v.parse::<usize>().ok());
    let Some(rounds) = rounds else {
        usage();
        std::process::exit(1);
    };
    if args.len() < 3 {
        usage();
        std::process::exit(1);
    }
    let paths = args[1..].to_vec();
    let code = match hromatite_runtime::hot::router::smoke(&paths, rounds) {
        Ok(lines) => {
            for line in lines {
                log::log_echo(&line);
            }
            0
        }
        Err(error) => {
            log::log_echo(&format!("hot-smoke: {error}"));
            2
        }
    };
    use std::io::Write;
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}
