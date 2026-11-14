//! Hromatite injector CLI. Runs *inside* the Proton prefix:
//!
//!   protontricks-launch --appid 438100 Hromatite.exe bridge
//!   protontricks-launch --appid 438100 Hromatite.exe inject --dll 'Z:\path\hromatite.dll'
//!
//! `bridge` proves cross-process access works at all (M0b); `inject` is the
//! manual mapper (M1).

use hromatite_core::{log, manualmap, procs, remote::Remote, GAME_ASSEMBLY, GAME_EXE, UNITY_PLAYER};

fn usage() {
    println!(
        "Hromatite - VRChat/il2cpp toolkit\n\n\
         commands:\n\
         \x20 bridge                 find {GAME_EXE} and report module bases (M0b)\n\
         \x20 inject --dll <path>     manual-map a DLL into the game (M1)\n\
         \x20 procs                  list wine processes"
    );
}

fn wait_pid(secs: u64, name: &str) -> Option<u32> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        if let Some(pid) = procs::find_pid(name) {
            return Some(pid);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

fn bridge(target: &str) -> i32 {
    log::log_echo("=== hromatite bridge (M0b) ===");
    log::log_echo(&format!("log file: {}", log::path().display()));
    let pid = match wait_pid(30, target) {
        Some(p) => p,
        None => {
            log::log_echo(&format!("{target} not found; is the game running?"));
            return 1;
        }
    };
    log::log_echo(&format!("found {target}: pid {pid}"));

    let mods = procs::modules(pid);
    log::log_echo(&format!("modules visible: {}", mods.len()));
    for want in [GAME_ASSEMBLY, UNITY_PLAYER] {
        match mods.iter().find(|(n, _, _)| n.eq_ignore_ascii_case(want)) {
            Some((n, base, size)) => log::log_echo(&format!("  {n}: base {base:#x} size {size:#x}")),
            None => log::log_echo(&format!("  {want}: MISSING")),
        }
    }

    let r = match Remote::open(pid) {
        Ok(r) => r,
        Err(e) => {
            log::log_echo(&format!("OpenProcess failed: {e}"));
            return 2;
        }
    };
    log::log_echo("OpenProcess ok");

    let ga_base = match mods.iter().find(|(n, _, _)| n.eq_ignore_ascii_case(GAME_ASSEMBLY)) {
        Some((_, base, _)) => *base,
        None => return 4,
    };
    match r.read_vec(ga_base, 0x40) {
        Some(b) => log::log_echo(&format!(
            "GameAssembly header: MZ={} e_lfanew={}",
            &b[0..2] == b"MZ",
            u32::from_le_bytes([b[0x3C], b[0x3D], b[0x3E], b[0x3F]])
        )),
        None => {
            log::log_echo("ReadProcessMemory(GameAssembly header) FAILED - injection path is broken");
            return 3;
        }
    }
    log::log_echo("bridge OK");
    0
}

fn inject(dll: &str, target: &str, opts: &manualmap::Options) -> i32 {
    log::log_echo("=== hromatite inject (manual map) ===");
    let pid = match wait_pid(30, target) {
        Some(p) => p,
        None => {
            log::log_echo(&format!("{target} not found"));
            return 1;
        }
    };
    log::log_echo(&format!(
        "target pid {pid}, dll {dll}, erase_headers={}, stomp={:?}",
        opts.erase_headers, opts.stomp
    ));
    match manualmap::inject(pid, dll, opts) {
        Ok(base) => {
            log::log_echo(&format!("mapped at {base:#x}"));
            0
        }
        Err(e) => {
            log::log_echo(&format!("inject failed: {e}"));
            2
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut dll: Option<String> = None;
    let mut target = String::from(GAME_EXE);
    // Stealth defaults: no PE headers in memory, fresh RX region.
    let mut opts = manualmap::Options {
        erase_headers: true,
        stomp: None,
    };
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--dll" && i + 1 < args.len() {
            dll = Some(args[i + 1].clone());
            i += 2;
        } else if args[i] == "--proc" && i + 1 < args.len() {
            target = args[i + 1].clone();
            i += 2;
        } else if args[i] == "--stomp" && i + 1 < args.len() {
            opts.stomp = Some(args[i + 1].clone());
            i += 2;
        } else if args[i] == "--keep-headers" {
            opts.erase_headers = false;
            i += 1;
        } else {
            i += 1;
        }
    }
    let cmd = args.first().map(|s| s.as_str()).unwrap_or("bridge");
    let code = match cmd {
        "bridge" => bridge(&target),
        "inject" => match dll {
            Some(d) => inject(&d, &target, &opts),
            None => {
                log::log_echo("inject needs --dll <path>");
                1
            }
        },
        "procs" => {
            for (pid, name) in procs::all_pids() {
                println!("{pid:>8}  {name}");
            }
            0
        }
        _ => {
            usage();
            1
        }
    };
    use std::io::Write;
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}
