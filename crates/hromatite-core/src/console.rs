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

// Every read below goes through `safe` — the console's job is exploring memory
// whose shape is unknown, so an unguarded deref here is an AV in the stub
// thread, i.e. a dead game instead of a bad answer line.

fn qword(addr: usize) -> Option<u64> {
    crate::safe::read_u64(addr)
}

fn bytes(addr: usize, len: usize) -> Vec<u8> {
    // partial read on purpose: `b <addr> n` should show what is there
    crate::safe::read_upto(addr, len)
}

fn ascii(addr: usize, max: usize) -> String {
    crate::safe::ascii_lossy(addr, max)
}

fn utf16(addr: usize, max: usize) -> String {
    crate::safe::utf16_lossy(addr, max)
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

/// Exact NUL-terminated string occurrences in readable memory (capped).
fn find_string(text: &str, max: usize) -> Vec<usize> {
    let pat = text.as_bytes();
    let mut out = Vec::new();
    let need = pat.len() + 1;
    if pat.is_empty() {
        return out;
    }
    for r in procs::regions(200_000).into_iter().filter(|r| r.readable()) {
        if out.len() >= max || r.size < need + 1 {
            continue;
        }
        let mut a = r.base;
        // last start such that a+pat.len() (the NUL) is still inside the region
        let last = r.base + r.size - need;
        while a <= last && out.len() < max {
            if unsafe { *(a as *const u8) } == pat[0]
                && unsafe { core::slice::from_raw_parts(a as *const u8, pat.len()) } == pat
                && unsafe { *((a + pat.len()) as *const u8) } == 0
            {
                out.push(a);
                a += pat.len();
                continue;
            }
            a += 1;
        }
    }
    out
}

/// Slots (addresses) whose 8-byte word equals `value`.
fn find_qword_slots(value: usize, max: usize) -> Vec<usize> {
    let want = value as u64;
    let mut out = Vec::new();
    for r in procs::regions(200_000).into_iter().filter(|r| r.readable()) {
        // без проверки размера `base + size - 8` уходит в минус на крошечных
        // регионах, и цикл читает неотображённую память (это убивало игру)
        if out.len() >= max || r.size < 16 {
            continue;
        }
        // регион уже проверен как committed+readable → прямой обход внутри
        // его границ (per-word VirtualQuery здесь стоил бы минуты)
        let span = crate::safe::Span { base: r.base, size: r.size, protect: r.protect };
        crate::safe::find_qword_in_span(&span, want, max, &mut out);
    }
    out
}

/// domain → assembly record → `Il2CppImage` (+0x28) → `il2cpp_class_from_name`.
///
/// This is the *real* lookup path (the older `cls` command brute-forces memory);
/// it needs `api::identify()` to have found `class_from_name`, which it does on
/// first use. The assembly is guessed from the namespace and then verified by
/// the returned class name, so a wrong guess costs a fault-tolerant call.
fn cls2_lookup(ns: &str, name: &str, asm_hint: &str) -> Vec<String> {
    let mut out = Vec::new();
    let d = match crate::api::domain() {
        Some(d) => d,
        None => return vec!["domain getter not found".into()],
    };
    let rep = crate::api::identify();
    if rep.class_fn.is_none() {
        out.push("class_from_name not identified — see `api` (retry with `api reset`)".into());
    }

    let mut cands: Vec<&str> = Vec::new();
    if !asm_hint.is_empty() {
        cands.push(asm_hint);
    }
    if ns.starts_with("UnityEngine") {
        cands.push("UnityEngine.CoreModule");
    }
    if ns.starts_with("System") || ns.starts_with("Microsoft") {
        cands.push("mscorlib");
    }
    cands.push("Assembly-CSharp");
    cands.push("mscorlib");
    cands.push("UnityEngine.CoreModule");
    let mut seen: Vec<String> = Vec::new();
    cands.retain(|c| {
        let k = c.to_ascii_lowercase();
        let fresh = !seen.contains(&k);
        seen.push(k);
        fresh
    });

    for asm in cands {
        let img = match crate::api::image_of_domain(d, asm) {
            Some(i) => i,
            None => continue,
        };
        if let Some(c) = crate::api::class_from_name(img, ns, name) {
            out.push(format!(
                "class {ns}.{name} = {c:#x} (assembly {asm}, image {img:#x})"
            ));
            if let Some(n) = crate::api::class_name(c) {
                let ns2 = crate::api::class_namespace(c).unwrap_or_default();
                out.push(format!("  name: '{n}' namespace: '{ns2}'"));
            } else {
                out.push("  name: (unreadable)".into());
            }
            return out;
        }
    }
    out.push(format!("class {ns}.{name}: not found (domain {d:#x})"));
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
                "findb <text> / findw <t>  — поиск ascii / utf-16 строки".into(),
                "asm [фильтр]              — сборки (имя + адрес записи)".into(),
                "entry <имя>               — поля записи сборки".into(),
                "domain                    — домен и его геттер".into(),
                "diag                      — полный recon заново".into(),
                "guard <addr>              — вызов функции под VEH (fault -> Err, не смерть)".into(),
                "api [reset]               — идентифицировать il2cpp API среди экспортов".into(),
                "cls2 <ns> <name> [asm]    — класс через il2cpp_class_from_name".into(),
                "cam                       — короткий тест: UnityEngine.Camera".into(),
                "panic [msg]               — намеренная паника (проверка catch_unwind)".into(),
                "av [addr]                 — safe-чтение + сырое разыменование (проверка VEH)".into(),
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
        "findb" | "findw" => {
            // сырой остаток строки — чтобы искать пробелы и точки
            let rest = line.splitn(2, char::is_whitespace).nth(1).unwrap_or("");
            if rest.is_empty() {
                return (vec!["usage: findb <ascii text>".into()], Action::None);
            }
            let pat: Vec<u8> = if cmd == "findw" {
                rest.encode_utf16().flat_map(|c| c.to_le_bytes()).collect()
            } else {
                rest.as_bytes().to_vec()
            };
            let mut hits = Vec::new();
            let mut total = 0usize;
            for r in procs::regions(200_000).into_iter().filter(|r| r.readable()) {
                if total >= 64 {
                    break;
                }
                let mut a = r.base;
                let end = r.base + r.size;
                let n = pat.len();
                // регион уже проверен как committed+readable, поэтому читаем
                // напрямую: VirtualQuery на каждый байт — это минуты
                while a + n <= end && total < 64 {
                    let b0 = unsafe { *(a as *const u8) };
                    if b0 == pat[0] {
                        let window = unsafe { core::slice::from_raw_parts(a as *const u8, n) };
                        if window == &pat[..] {
                            total += 1;
                            hits.push(format!("  {a:#x}"));
                            a += n;
                            continue;
                        }
                    }
                    a += 1;
                }
            }
            hits.insert(
                0,
                format!("{} '{}': {total} hit(s){}", cmd, rest, if total >= 64 { " (capped)" } else { "" }),
            );
            (hits, Action::None)
        }
        "cls" => {
            // layout-agnostic поиск Il2CppClass: ищем указатель на строку имени
            // и рядом (в пределах ±0x40) указатель на строку namespace
            let ns = a1;
            let name = a2;
            if ns.is_empty() || name.is_empty() {
                return (vec!["usage: cls <namespace> <name>".into()], Action::None);
            }
            let ns_a = find_string(ns, 4);
            let nm_a = find_string(name, 6);
            let mut out = vec![
                format!("ns '{ns}' at {:?}", ns_a.iter().map(|a| format!("{a:#x}")).collect::<Vec<_>>()),
                format!("name '{name}' at {:?}", nm_a.iter().map(|a| format!("{a:#x}")).collect::<Vec<_>>()),
            ];
            let mut found = 0usize;
            for na in &nm_a {
                for slot in find_qword_slots(*na, 24) {
                    let mut d = -0x40i64;
                    while d <= 0x40 {
                        let p = slot as i64 + d;
                        // окно может выйти за границу страницы: без проверки
                        // здесь был AV, который уронил игру
                        if p <= 0 {
                            d += 8;
                            continue;
                        }
                        let word = match qword(p as usize) {
                            Some(v) => v as usize,
                            None => {
                                d += 8;
                                continue;
                            }
                        };
                        if ns_a.contains(&word) {
                            found += 1;
                            out.push(format!(
                                "  [{found}] name@{slot:#x} ns@{p:#x} (delta {d:+#x})"
                            ));
                            out.push(format!("     struct base ~{:#x}, window:", slot.wrapping_sub(0x20)));
                            out.extend(dump_qwords(slot.saturating_sub(0x20), 10));
                        }
                        d += 8;
                    }
                }
            }
            if found == 0 {
                out.push("  no struct with name+namespace pair found".into());
            }
            (out, Action::None)
        }
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
                // `size < 16`: иначе `base + size - 8` уходит ниже начала региона
                if r.size < 16 {
                    continue;
                }
                let span = crate::safe::Span { base: r.base, size: r.size, protect: r.protect };
                let mut slots = Vec::new();
                crate::safe::find_qword_in_span(&span, want, 24 - hits.len(), &mut slots);
                for a in slots {
                    hits.push(format!("  {a:#x}"));
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
        // ---- plan 2: guarded calls + behavioural API identification -------
        // `guard <addr>` calls a raw GameAssembly RVA-relative function through
        // the vectored handler: the answer is either a return value or a fault
        // line, never a dead game (that is the whole point of `veh`).
        "guard" => match parse_addr(a1) {
            Some(addr) => {
                crate::log::log_tag("con", &format!("guard {addr:#x}: calling under VEH"));
                match crate::veh::call0(addr) {
                    Ok(v) => (vec![format!("guard {addr:#x}: returned {v:#x}")], Action::None),
                    Err(f) => (vec![format!("guard {addr:#x}: {f}")], Action::None),
                }
            }
            None => (vec!["usage: guard <addr>".into()], Action::None),
        },
        // `api` runs (or reports) the identification of domain_get_assemblies /
        // class_from_name among the 251 obfuscated exports.
        "api" => {
            if a1.eq_ignore_ascii_case("reset") {
                crate::api::reset();
                crate::log::log_tag("con", "api cache reset");
            }
            let rep = crate::api::identify();
            let mut out = vec![format!(
                "domain {:#x}, image {:#x}, tried {}, faulted {}",
                rep.domain, rep.image, rep.tried, rep.faulted
            )];
            match rep.assemblies_fn {
                Some(f) => out.push(format!(
                    "domain_get_assemblies: {} rva={:#x} addr={:#x}",
                    f.name, f.rva, f.addr
                )),
                None => out.push("domain_get_assemblies: NOT identified".into()),
            }
            match rep.class_fn {
                Some(f) => out.push(format!(
                    "class_from_name: {} rva={:#x} addr={:#x}",
                    f.name, f.rva, f.addr
                )),
                None => out.push("class_from_name: NOT identified".into()),
            }
            if let Some(l) = rep.layout {
                out.push(format!(
                    "class layout: name@+{:#x} ns@+{:#x}",
                    l.name_off, l.ns_off
                ));
            }
            out.extend(rep.notes.iter().cloned());
            (out, Action::None)
        }
        // `cls2 <ns> <name> [assembly]` — the real il2cpp lookup path:
        // domain -> assembly -> Il2CppImage (+0x28) -> class_from_name.
        "cls2" => {
            if a1.is_empty() || a2.is_empty() {
                return (vec!["usage: cls2 <namespace> <name> [assembly]".into()], Action::None);
            }
            (cls2_lookup(a1, a2, rest.trim()), Action::None)
        }
        // `cam` — the shortest end-to-end proof: UnityEngine.Camera must resolve.
        "cam" => {
            let mut out = vec!["probe: UnityEngine.Camera".to_string()];
            out.extend(cls2_lookup("UnityEngine", "Camera", ""));
            (out, Action::None)
        }
        // ---- plan 5 self-tests: prove guard()/VEH actually hold -----------
        // `panic` must produce a `[dll] PANIC at file:line:` line and *not*
        // kill the game; the session continues with the next command.
        "panic" => {
            let msg = line
                .splitn(2, char::is_whitespace)
                .nth(1)
                .unwrap_or("console panic test")
                .trim();
            let msg = if msg.is_empty() { "console panic test" } else { msg };
            crate::log::log_tag("con", &format!("panic test: about to panic with '{msg}'"));
            panic!("console panic test: {msg}");
        }
        // `av <addr>` shows the difference between the two failure classes:
        // the safe read survives (returns None), the raw deref is exactly what
        // the VEH from plan 2 has to catch. Default 0x1 — never mapped.
        "av" => {
            let addr = parse_addr(a1).unwrap_or(1);
            let guarded = match crate::safe::read_u64(addr) {
                Some(v) => format!("{v:#016x}"),
                None => "(unreadable, survived)".to_string(),
            };
            let head = format!(
                "av @{addr:#x}: readable_at={}, safe::read_u64={guarded}",
                crate::safe::readable_at(addr)
            );
            // log before the raw deref: if the process dies here, this line is
            // the only witness we get.
            crate::log::log_tag("con", &head);
            let raw = unsafe { *(addr as *const u64) };
            (
                vec![
                    head,
                    format!("raw deref @{addr:#x} = {raw:#016x} (VEH did not run / addr was mapped)"),
                ],
                Action::None,
            )
        }
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
        other => {
            // Extension point for modules that own their own commands (hook,
            // api); keeps this file from being a merge conflict every wave.
            if let Some(out) = crate::hook::command(line) {
                return (out, Action::None);
            }
            (vec![format!("unknown command: {other} (try help)")], Action::None)
        }
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
