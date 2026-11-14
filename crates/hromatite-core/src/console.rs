//! File-based command channel — "remote console" without any kernel object.
//!
//! The host appends lines to `%USERPROFILE%\hromatite-cmd.txt`; the payload
//! polls that file between log flushes and answers by writing to its normal log
//! file. No socket, no named pipe, no extra thread: from the anti-cheat's point
//! of view the game only touches two ordinary files.
//!
//! The point of the command set is *exploration without re-injection*: `q`,
//! `b`, `s`, `find`, `asm` let the host walk the il2cpp structures live, which
//! turns a 2-minute rebuild+restart cycle into a single command.

use crate::il2cpp;
use crate::procs;
use std::io::{Read, Seek, SeekFrom, Write};

pub const CMD_FILE: &str = "hromatite-cmd.txt";

pub fn cmd_path() -> std::path::PathBuf {
    let base = std::env::var("USERPROFILE").unwrap_or_else(|_| "C:".to_string());
    std::path::PathBuf::from(base).join(CMD_FILE)
}

pub struct Reader {
    path: std::path::PathBuf,
    pos: u64,
}

impl Reader {
    /// Start at the current end of file so that commands sent before injection
    /// are not replayed.
    pub fn new() -> Self {
        let path = cmd_path();
        let pos = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Self { path, pos }
    }

    /// Complete lines appended since the last call (a partial tail is kept for
    /// the next poll).
    pub fn poll(&mut self) -> Vec<String> {
        let mut out = Vec::new();
        let mut f = match std::fs::OpenOptions::new().read(true).open(&self.path) {
            Ok(f) => f,
            Err(_) => return out,
        };
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        if len < self.pos {
            // host truncated/recreated the file
            self.pos = 0;
        }
        if len == self.pos {
            return out;
        }
        if f.seek(SeekFrom::Start(self.pos)).is_err() {
            return out;
        }
        let mut buf = String::new();
        if f.read_to_string(&mut buf).is_err() {
            return out;
        }
        self.pos = len;
        for line in buf.lines() {
            let l = line.trim();
            if !l.is_empty() {
                out.push(l.to_string());
            }
        }
        out
    }
}

#[derive(PartialEq)]
pub enum Action {
    None,
    Quit,
    Seal,
}

fn parse_addr(s: &str) -> Option<usize> {
    let t = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    usize::from_str_radix(t, 16).ok()
}

fn readable(addr: usize) -> bool {
    unsafe {
        let mut mbi = crate::ffi::MEMORY_BASIC_INFORMATION::default();
        let n = crate::ffi::VirtualQuery(
            addr as *const core::ffi::c_void,
            &mut mbi,
            core::mem::size_of::<crate::ffi::MEMORY_BASIC_INFORMATION>(),
        );
        if n == 0 || mbi.State != crate::ffi::MEM_COMMIT {
            return false;
        }
        let p = mbi.Protect & 0xFF;
        p != crate::ffi::PAGE_NOACCESS && p != 0 && (mbi.Protect & crate::ffi::PAGE_GUARD) == 0
    }
}

fn qword(addr: usize) -> Option<u64> {
    if !readable(addr) {
        return None;
    }
    Some(unsafe { *(addr as *const u64) })
}

fn bytes(addr: usize, len: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(len);
    for i in 0..len {
        if !readable(addr + i) {
            break;
        }
        v.push(unsafe { *((addr + i) as *const u8) });
    }
    v
}

fn ascii(addr: usize, max: usize) -> String {
    let mut s = String::new();
    for b in bytes(addr, max) {
        if b == 0 {
            break;
        }
        s.push(if (32..127).contains(&b) { b as char } else { '.' });
    }
    s
}

fn utf16(addr: usize, max: usize) -> String {
    let mut out = Vec::new();
    for i in 0..max {
        if !readable(addr + i * 2) {
            break;
        }
        let c = unsafe { *((addr + i * 2) as *const u16) };
        if c == 0 {
            break;
        }
        out.push(c);
    }
    String::from_utf16_lossy(&out)
}

fn tag_of(v: usize) -> &'static str {
    let ga = procs::own_module(crate::GAME_ASSEMBLY);
    let up = procs::own_module(crate::UNITY_PLAYER);
    if let Some((b, s)) = ga {
        if v >= b && v < b + s {
            return "GA";
        }
    }
    if let Some((b, s)) = up {
        if v >= b && v < b + s {
            return "UP";
        }
    }
    ""
}

fn dump_qwords(addr: usize, n: usize) -> Vec<String> {
    let mut out = Vec::new();
    for i in 0..n {
        let a = addr + i * 8;
        match qword(a) {
            Some(v) => {
                let asc = ascii(v as usize, 24);
                let asc = if asc.is_empty() {
                    String::new()
                } else {
                    format!(" '{asc}'")
                };
                out.push(format!("  {a:#x}: {v:#016x}{asc} {}", tag_of(v as usize)));
            }
            None => {
                out.push(format!("  {a:#x}: (unmapped)"));
                break;
            }
        }
    }
    out
}

/// Execute one command. Returns the answer lines and a control action.
pub fn dispatch(line: &str) -> (Vec<String>, Action) {
    let mut it = line.splitn(4, char::is_whitespace);
    let cmd = it.next().unwrap_or("").to_ascii_lowercase();
    let a1 = it.next().unwrap_or("");
    let a2 = it.next().unwrap_or("");
    let rest = it.next().unwrap_or("");

    match cmd.as_str() {
        "help" | "?" => (
            vec![
                "help                      — этот список".into(),
                "q <addr> [n]              — n qword'ов с ascii и меткой модуля".into(),
                "b <addr> [n]              — n байт в hex".into(),
                "s <addr>                  — ascii-строка".into(),
                "ws <addr>                 — utf-16 строка".into(),
                "find <hex64>              — поиск значения по всей читаемой памяти".into(),
                "asm [фильтр]              — сборки (имя + адрес записи)".into(),
                "entry <имя>               — поля записи сборки".into(),
                "domain                    — домен и его геттер".into(),
                "diag                      — полный recon заново".into(),
                "seal | quit               — выйти из интерактива (инжектор запечатает образ)".into(),
            ],
            Action::None,
        ),
        "q" | "qword" => match parse_addr(a1) {
            Some(addr) => {
                let n = a2.parse::<usize>().unwrap_or(8).min(64);
                (dump_qwords(addr, n), Action::None)
            }
            None => (vec!["usage: q <addr> [n]".into()], Action::None),
        },
        "b" | "bytes" => match parse_addr(a1) {
            Some(addr) => {
                let n = a2.parse::<usize>().unwrap_or(32).min(512);
                let bs = bytes(addr, n);
                let hex: Vec<String> = bs.iter().map(|b| format!("{b:02x}")).collect();
                (
                    vec![format!("{addr:#x}: {} ({} bytes)", hex.join(" "), bs.len())],
                    Action::None,
                )
            }
            None => (vec!["usage: b <addr> [n]".into()], Action::None),
        },
        "s" => match parse_addr(a1) {
            Some(addr) => (vec![format!("{addr:#x}: '{}'", ascii(addr, 128))], Action::None),
            None => (vec!["usage: s <addr>".into()], Action::None),
        },
        "ws" => match parse_addr(a1) {
            Some(addr) => (
                vec![format!("{addr:#x}: '{}'", utf16(addr, 128))],
                Action::None,
            ),
            None => (vec!["usage: ws <addr>".into()], Action::None),
        },
        "find" => {
            let want = match parse_addr(a1) {
                Some(v) => v as u64,
                None => return (vec!["usage: find <hex64>".into()], Action::None),
            };
            let mut hits = Vec::new();
            for r in procs::regions(200_000).into_iter().filter(|r| r.readable()) {
                if hits.len() >= 24 {
                    break;
                }
                let mut a = (r.base + 7) & !7usize;
                let end = r.base + r.size - 8;
                while a <= end && hits.len() < 24 {
                    if unsafe { *(a as *const u64) } == want {
                        hits.push(format!("  {a:#x}"));
                    }
                    a += 8;
                }
            }
            hits.insert(0, format!("find {want:#x}: {} hit(s)", hits.len()));
            (hits, Action::None)
        }
        "asm" => match il2cpp::find_domain() {
            Some(d) => {
                let list = il2cpp::assemblies(d);
                let filt = a1.to_ascii_lowercase();
                let mut out = vec![format!("domain {d:#x}, {} assemblies", list.len())];
                for (name, entry) in list {
                    if filt.is_empty() || name.to_ascii_lowercase().contains(&filt) {
                        out.push(format!("  {entry:#x}  {name}"));
                    }
                }
                (out, Action::None)
            }
            None => (vec!["domain getter not found".into()], Action::None),
        },
        "entry" => match il2cpp::find_domain() {
            Some(d) => match il2cpp::assembly(d, if a1.is_empty() { "mscorlib" } else { a1 }) {
                Some((name, entry)) => {
                    let mut out = vec![format!("assembly {name} @{entry:#x}")];
                    out.extend(dump_qwords(entry, 9));
                    (out, Action::None)
                }
                None => (vec![format!("assembly '{}' not found", a1)], Action::None),
            },
            None => (vec!["domain getter not found".into()], Action::None),
        },
        "domain" => match il2cpp::find_domain() {
            Some(d) => {
                let mut out = vec![format!("domain {d:#x}")];
                out.extend(dump_qwords(d, 16));
                (out, Action::None)
            }
            None => (vec!["domain getter not found".into()], Action::None),
        },
        "diag" => (
            il2cpp::diagnose().lines().map(|s| s.to_string()).collect(),
            Action::None,
        ),
        "seal" => (vec!["seal requested, returning from DllMain".into()], Action::Seal),
        "quit" | "exit" => (vec!["bye".into()], Action::Quit),
        "echo" => {
            let msg = if rest.is_empty() {
                format!("{a1} {a2}")
            } else {
                format!("{a1} {a2} {rest}")
            };
            (vec![msg.trim().to_string()], Action::None)
        }
        "" => (Vec::new(), Action::None),
        other => (vec![format!("unknown command: {other} (try help)")], Action::None),
    }
}

/// Append one line to the command file — used by in-process tests/payload.
pub fn send(line: &str) -> bool {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cmd_path())
        .and_then(|mut f| writeln!(f, "{line}"))
        .is_ok()
}
