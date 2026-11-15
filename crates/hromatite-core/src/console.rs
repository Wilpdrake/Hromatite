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

/// Diagnostic: where the reader stands, plus the current file length.
pub fn cmd_pos() -> String {
    match std::fs::metadata(cmd_path()) {
        Ok(m) => format!("{} of {}", std::fs::metadata(cmd_path()).map(|x| x.len()).unwrap_or(0), m.len()),
        Err(e) => format!("<{e}>"),
    }
}

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

    /// Current read position (diagnostics).
    pub fn pos(&self) -> u64 {
        self.pos
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

/// How much of the address space a scan may touch. A full pass (1.5 GB) keeps the
/// injected thread busy long enough for Unity's GC `SuspendThread` to fail and the
/// game to freeze; 512 MB is enough for the il2cpp metadata and heap we care about.
/// Regions in an order that finds il2cpp structures inside the budget: the game's
/// own modules first (`.data`/`.rdata` hold the class tables), the heap last. A
/// flat walk spends the whole budget on the heap and never reaches them.
fn ordered_regions() -> Vec<procs::Region> {
    let mut all: Vec<procs::Region> = procs::regions(200_000)
        .into_iter()
        .filter(|r| r.readable())
        .collect();
    let mods: Vec<(usize, usize)> = [crate::GAME_ASSEMBLY, crate::UNITY_PLAYER]
        .iter()
        .filter_map(|m| procs::own_module(m))
        .collect();
    all.sort_by_key(|r| {
        let in_mod = mods
            .iter()
            .any(|(b, s)| r.base < b + s && r.base + r.size > *b);
        if in_mod { 0 } else { 1 }
    });
    all
}

fn scan_budget() -> usize {
    std::env::var("HROMATITE_SCAN_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(512)
        * 1024
        * 1024
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
    let mut spent = 0usize;
    let budget = scan_budget();
    for r in ordered_regions() {
        spent += r.size;
        if spent > budget { break; }
        if out.len() >= max || r.size < need + 1 {
            continue;
        }
        crate::safe::scan_yield(); // let Unity's GC suspend us
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
    let mut spent = 0usize;
    let budget = scan_budget();
    for r in ordered_regions() {
        spent += r.size;
        if spent > budget { break; }
        // без проверки размера `base + size - 8` уходит в минус на крошечных
        // регионах, и цикл читает неотображённую память (это убивало игру)
        if out.len() >= max || r.size < 16 {
            continue;
        }
        crate::safe::scan_yield();
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

/// The image every Unity type lives in (fallback: mscorlib).
fn core_image() -> Option<usize> {
    crate::api::image_of("UnityEngine.CoreModule").or_else(|| crate::api::image_of("mscorlib"))
}

/// Class lookup that does not depend on the image walk: Unity 6 keeps no class
/// table in the codegen module, so fall back to the global class arrays.
fn lookup_class(ns: &str, name: &str) -> Option<crate::meta::Class> {
    if let Some(img) = core_image() {
        if let Some(c) = crate::meta::find_class(img, ns, name) {
            return Some(c);
        }
    }
    if let Some(c) = crate::meta::find_class_global(ns, name) {
        return Some(c);
    }
    // Last resort (Unity 6): locate the record through its name string.
    crate::meta::class_by_name_string(ns, name)
}

/// Vector3 out of three little-endian floats.
fn vec3(bits: [u64; 2]) -> (f32, f32, f32) {
    (
        f32::from_bits(bits[0] as u32),
        f32::from_bits((bits[0] >> 32) as u32),
        f32::from_bits(bits[1] as u32),
    )
}

/// M3 end-to-end proof: `Camera.main` → `transform` → `position`.
///
/// Nothing here needs an identified il2cpp export: the classes come from the
/// image's own type table, and a method is called through the entry point that
/// sits at `MethodInfo+0x00`. The Vector3 comes back through the hidden return
/// pointer that Win64 uses for structs larger than 8 bytes (`rcx` = buffer,
/// `rdx` = this).
fn pos_probe() -> Vec<String> {
    let mut out = Vec::new();
    let image = match core_image() {
        Some(i) => i,
        None => return vec!["pos: no UnityEngine.CoreModule image".into()],
    };
    let cam = match lookup_class("UnityEngine", "Camera") {
        Some(c) => c,
        None => return vec![format!("pos: Camera not found in image {image:#x}")],
    };
    out.push(format!(
        "pos: Camera class {:#x} ({}), {} field(s)",
        cam.addr,
        crate::meta::layout_of(cam.addr),
        crate::meta::fields_of(cam.addr).len()
    ));

    // 1. static Camera.main
    let main = match crate::meta::method_entry(cam.addr, "get_main") {
        Some(f) => f,
        None => return {
            out.push("pos: no Camera.get_main".into());
            out
        },
    };
    let cam_inst = match crate::veh::call0(main) {
        Ok(v) => v,
        Err(e) => {
            out.push(format!("pos: Camera.main faulted: {e}"));
            return out;
        }
    };
    out.push(format!("pos: Camera.main = {cam_inst:#x}"));
    if cam_inst < 0x10000 {
        out.push("pos: no main camera in this world".into());
        return out;
    }

    // 2. instance transform — Camera overrides it, Component declares it
    let mut get_transform = crate::meta::method_entry(cam.addr, "get_transform");
    if get_transform.is_none() {
        for (ns, name) in [("UnityEngine", "Component"), ("UnityEngine", "Transform")] {
            if let Some(c) = lookup_class(ns, name) {
                get_transform = crate::meta::method_entry(c.addr, "get_transform");
                if get_transform.is_some() {
                    break;
                }
            }
        }
    }
    let get_transform = match get_transform {
        Some(f) => f,
        None => {
            out.push("pos: no get_transform anywhere".into());
            return out;
        }
    };
    let tr = match crate::veh::call1(get_transform, cam_inst) {
        Ok(v) => v,
        Err(e) => {
            out.push(format!("pos: get_transform faulted: {e}"));
            return out;
        }
    };
    out.push(format!("pos: transform = {tr:#x}"));
    if tr < 0x10000 {
        return out;
    }

    // 3. `Transform.position` (struct return: hidden buffer in rcx)
    let trc = match lookup_class("UnityEngine", "Transform") {
        Some(c) => c,
        None => {
            out.push("pos: Transform class not found".into());
            return out;
        }
    };
    out.push(format!(
        "pos: Transform class {:#x}, {} method(s)",
        trc.addr,
        crate::meta::methods_of(trc.addr).len()
    ));
    let get_pos = match crate::meta::method_entry(trc.addr, "get_position") {
        Some(f) => f,
        None => {
            out.push("pos: no Transform.get_position".into());
            return out;
        }
    };
    let mut buf = [0u64; 2];
    match crate::veh::call2(get_pos, buf.as_mut_ptr() as usize, tr) {
        Ok(_) => {
            let (x, y, z) = vec3(buf);
            out.push(format!(
                "pos: world position = ({x:.3}, {y:.3}, {z:.3})  [raw {:#x} {:#x}]",
                buf[0], buf[1]
            ));
        }
        Err(e) => out.push(format!("pos: get_position faulted: {e}")),
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
                "findb <text> / findw <t>  — поиск ascii / utf-16 строки".into(),
                "asm [фильтр]              — сборки (имя + адрес записи)".into(),
                "entry <имя>               — поля записи сборки".into(),
                "domain                    — домен и его геттер".into(),
                "diag                      — полный recon заново".into(),
                "gct [rva]                 — таблица потоков Boehm (живые/мёртвые записи)".into(),
                "threads                   — все потоки процесса: старт-адрес и есть ли запись в GC".into(),
                "gctfix                    — вычеркнуть мёртвые записи из таблицы Boehm".into(),
                "gctresume                 — ResumeThread по всем записям (снять утёкший suspend)".into(),
                "call <addr> [a1..a3]      — вызов функции под VEH".into(),
                "wq <addr> <hex>           — записать qword".into(),
                "wb <addr> <hexbytes>      — записать байты".into(),
                "guard <addr>              — вызов функции под VEH (fault -> Err, не смерть)".into(),
                "api [1|2|reset]           — идентифицировать il2cpp API (1=assemblies, 2=class)".into(),
                "cls2 <ns> <name> [asm]    — класс через il2cpp_class_from_name".into(),
                "cam                       — короткий тест: UnityEngine.Camera".into(),
                "types [asm] [max]         — классы образа через typeInfoTable (без API-вызовов)".into(),
                "tfind <ns> <name> [asm]   — найти класс по имени (без asm — по всем образам)".into(),
                "fields <class>            — поля класса и их смещения".into(),
                "methods <class> [фильтр]   — методы класса: имя, entry point, RVA".into(),
                "mcall <class> <method> [a1..a3] — вызов метода по entry point под VEH".into(),
                "pos                       — Camera.main -> transform -> position (M3)".into(),
                "cam2                      — Camera через таблицу типов (без API)".into(),
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
            let mut spent = 0usize;
            let budget = scan_budget();
            for r in ordered_regions() {
                spent += r.size;
                if spent > budget { break; }
                if total >= 64 {
                    break;
                }
                crate::safe::scan_yield();
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
            let mut spent = 0usize;
            let budget = scan_budget();
            for r in ordered_regions() {
                spent += r.size;
                if spent > budget { break; }
                if hits.len() >= 24 {
                    break;
                }
                crate::safe::scan_yield();
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
        // Boehm's own thread table vs. the threads the process really has.
        // `gct` alone = detection report + full dump; `gct <rva>` forces a base.
        "gct" => {
            let forced = parse_addr(a1).map(|v| v as u32);
            let mut out = crate::gcthreads::probe(forced);
            match crate::gcthreads::snapshot(forced) {
                Some(t) => out.extend(t.lines()),
                None => out.push("gct: table not found (see candidate scores above)".into()),
            }
            (out, Action::None)
        }
        // Writes. Deliberate, single pokes — the payload has no other way to
        // change game state, and a dead Boehm record has to be unlinked by hand
        // when the automatic repair is off.
        "wq" => match (parse_addr(a1), parse_addr(a2)) {
            (Some(addr), Some(v)) => {
                let ok = crate::safe::write_usize(addr, v);
                (
                    vec![format!("wq {addr:#x} <- {v:#x}: {ok} (now {:#x})", crate::safe::read_usize(addr).unwrap_or(0))],
                    Action::None,
                )
            }
            _ => (vec!["usage: wq <addr> <hex value>".into()], Action::None),
        },
        "wb" => match (parse_addr(a1), rest.trim()) {
            (Some(addr), hex) if !hex.is_empty() => {
                let cleaned: String = hex.chars().filter(|c| !c.is_whitespace() && *c != '_').collect();
                let bytes: Option<Vec<u8>> = (cleaned.len() % 2 == 0)
                    .then(|| {
                        (0..cleaned.len())
                            .step_by(2)
                            .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).ok())
                            .collect::<Option<Vec<u8>>>()
                    })
                    .flatten();
                match bytes {
                    Some(b) => {
                        let ok = crate::safe::write_bytes(addr, &b);
                        (vec![format!("wb {addr:#x} <- {} byte(s): {ok}", b.len())], Action::None)
                    }
                    None => (vec!["usage: wb <addr> <hex bytes, e.g. 0100ff>".into()], Action::None),
                }
            }
            _ => (vec!["usage: wb <addr> <hex bytes>".into()], Action::None),
        },
        // Resume every thread Boehm knows about: if a collection died half-way,
        // the world is left suspended and the game looks frozen while we still
        // run. Harmless when nothing is suspended (a resume of a running thread
        // only decrements a count that is already 0... and returns 0/-1).
        "gctresume" => match crate::gcthreads::snapshot(None) {
            Some(t) => (crate::gcthreads::resume_all(&t), Action::None),
            None => (vec!["gctresume: table not found".into()], Action::None),
        },
        // Raw guarded call: `call <addr> [a1] [a2] [a3]`.
        "call" => {
            let args: Vec<usize> = line
                .split_whitespace()
                .skip(1)
                .map(|s| parse_addr(s).unwrap_or(0))
                .collect();
            let f = args.first().copied().unwrap_or(0);
            if f == 0 {
                return (vec!["usage: call <addr> [a1] [a2] [a3]".into()], Action::None);
            }
            let g = |i: usize| args.get(i + 1).copied().unwrap_or(0);
            let mut out = vec![format!(
                "call {f:#x}({:#x}, {:#x}, {:#x})",
                g(0),
                g(1),
                g(2)
            )];
            match crate::veh::call3(f, g(0), g(1), g(2)) {
                Ok(v) => out.push(format!("  -> {v:#x}")),
                Err(e) => out.push(format!("  fault: {e}")),
            }
            (out, Action::None)
        }
        // Unlink every dead record from Boehm's table right now.
        "gctfix" => {
            let mut out = Vec::new();
            match crate::gcthreads::snapshot(None) {
                Some(t) => {
                    out.push(t.summary());
                    if t.dead == 0 {
                        out.push("nothing to repair".into());
                    } else {
                        out.extend(crate::gcthreads::repair(&t));
                        if let Some(t2) = crate::gcthreads::snapshot(None) {
                            out.push(format!("after repair: {}", t2.summary()));
                        }
                    }
                }
                None => out.push("gctfix: table not found".into()),
            }
            (out, Action::None)
        }
        // All threads with their start address (which function created them) and
        // whether Boehm has a record for them: a thread without a record, or a
        // record whose thread is gone, is what aborts the collector.
        "threads" => {
            let table = crate::gcthreads::snapshot(None);
            (
                crate::gcthreads::thread_rows(table.as_ref()),
                Action::None,
            )
        }
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
            // `api 1` / `api 2` run one phase only: the two phases have very
            // different risk (pointers vs three pointer args), and a memory
            // watchdog aborts a phase that starts allocating.
            let phase = match a1 {
                "1" => 1u8,
                "2" => 2u8,
                _ => 0u8,
            };
            let rep = if phase == 0 {
                crate::api::identify()
            } else {
                crate::api::identify_only(phase)
            };
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
        // `ustub <fileoff> [a1] [a2] [a3]` — call a lazily-resolving il2cpp stub
        // inside UnityPlayer.dll by its *file offset* (found offline by signature).
        // Each stub stores the resolved pointer in a global and forwards, so the
        // call works even though every export name is obfuscated.
        "ustub" => {
            let args: Vec<&str> = line.split_whitespace().skip(1).collect();
            let off = match args.first().and_then(|s| parse_addr(s)) {
                Some(v) => v,
                None => return (vec!["usage: ustub <fileoff> [a1] [a2] [a3]".into()], Action::None),
            };
            let a: Vec<usize> = args.iter().skip(1).map(|s| parse_addr(s).unwrap_or(0)).collect();
            let get = |i: usize| a.get(i).copied().unwrap_or(0);
            let base = match procs::own_module(crate::UNITY_PLAYER) {
                Some((b, _)) => b,
                None => return (vec!["UnityPlayer not loaded".into()], Action::None),
            };
            let target = base + off;
            let mut out = vec![format!(
                "ustub {off:#x} -> {target:#x} ({:#x}, {:#x}, {:#x})",
                get(0), get(1), get(2)
            )];
            match crate::veh::call3(target, get(0), get(1), get(2)) {
                Ok(r) => {
                    out.push(format!("  -> {r:#x}"));
                    if r >= 0x10000 && crate::safe::readable_at(r) {
                        for d in (0..0x20).step_by(8) {
                            if let Some(p) = crate::safe::read_u64(r + d) {
                                let tag = crate::safe::ascii_at(p as usize, 40)
                                    .map(|s| format!(" '{s}'"))
                                    .unwrap_or_default();
                                out.push(format!("     [{d:#x}] {p:#x}{tag}"));
                            }
                        }
                    }
                }
                Err(e) => out.push(format!("  fault: {e}")),
            }
            (out, Action::None)
        }
        // `c3 <fnaddr> <imageaddr> <ns> <name>` — call a candidate as
        // `class_from_name(image, ns, name)` with an *explicit* image pointer.
        // Needed because the assembly record offers four candidate pointers and
        // only one of them is the real `Il2CppImage` (`+0x28` is CodeGenModule).
        "c3" => {
            let args: Vec<&str> = line.split_whitespace().skip(1).collect();
            if args.len() < 4 {
                return (vec!["usage: c3 <fn> <image> <ns> <name>".into()], Action::None);
            }
            let (f, image) = match (parse_addr(args[0]), parse_addr(args[1])) {
                (Some(f), Some(i)) => (f, i),
                _ => return (vec!["usage: c3 <fn> <image> <ns> <name>".into()], Action::None),
            };
            let (ns, name) = (args[2], args[3]);
            let ns_c = crate::ffi::cstr(ns);
            let nm_c = crate::ffi::cstr(name);
            let mut out = vec![format!("c3 {f:#x}({image:#x}, '{ns}', '{name}')")];
            match crate::veh::call3(f, image, ns_c.as_ptr() as usize, nm_c.as_ptr() as usize) {
                Ok(r) => {
                    out.push(format!("  -> {r:#x}"));
                    if r >= 0x10000 && crate::safe::readable_at(r) {
                        for d in (0..0x40).step_by(8) {
                            if let Some(p) = crate::safe::read_u64(r + d) {
                                if let Some(s) = crate::safe::ascii_at(p as usize, 48) {
                                    out.push(format!("     [{d:#x}] -> '{s}'"));
                                }
                            }
                        }
                    }
                }
                Err(er) => out.push(format!("  fault: {er}")),
            }
            (out, Action::None)
        }
        // `cls3 <fnaddr> <ns> <name>` — call ONE candidate export as
        // class_from_name(image, ns, name) under VEH and inspect the result.
        // Cheaper and safer than the brute-force phase: the offline shortlist
        // (prologue reads both rdx and r8) narrows 251 exports to 16, and
        // passing `image` to an allocator costs gigabytes.
        "cls3" => {
            let (f, ns, name) = match (parse_addr(a1), a2, rest.trim()) {
                (Some(f), ns, nm) if !ns.is_empty() && !nm.is_empty() => (f, ns, nm),
                _ => return (vec!["usage: cls3 <fnaddr> <ns> <name>".into()], Action::None),
            };
            let image = match crate::api::image_of("UnityEngine.CoreModule")
                .or_else(|| crate::api::image_of("mscorlib"))
            {
                Some(i) => i,
                None => return (vec!["no image (assemble lookup failed)".into()], Action::None),
            };
            let ns_c = crate::ffi::cstr(ns);
            let nm_c = crate::ffi::cstr(name);
            let mut out = vec![format!(
                "cls3: {f:#x}(image {image:#x}, '{ns}', '{name}')"
            )];
            let called = crate::veh::call3(f, image, ns_c.as_ptr() as usize, nm_c.as_ptr() as usize);
            match called {
                Ok(r) => {
                    out.push(format!("  -> {r:#x}"));
                    if r >= 0x10000 && crate::safe::readable_at(r) {
                        match crate::api::class_probe(r) {
                            Some(l) => out.push(format!(
                                "  class layout: name@+{:#x} ns@+{:#x}",
                                l.name_off, l.ns_off
                            )),
                            None => out.push("  no name/ns pair in +/-0x40".into()),
                        }
                        out.extend(dump_qwords(r.saturating_sub(0x20), 10));
                    }
                }
                Err(er) => out.push(format!("  fault: {er}")),
            }
            (out, Action::None)
        }
        // `imgc <fnaddr> <index>` — call a 1/2-arg image function under VEH and
        // dump what it returns. 0x4cbd70 is `mov rax,[rcx+8]; ret`, i.e. the
        // image's typeCount (0x1dc5); the same shape with an index argument is
        // `il2cpp_image_get_class`, which is enough to enumerate classes without
        // ever guessing `class_from_name`.
        "imgc" => {
            let (f, idx) = match (parse_addr(a1), parse_addr(a2)) {
                (Some(f), Some(i)) => (f, i),
                _ => return (vec!["usage: imgc <fnaddr> <index>".into()], Action::None),
            };
            let image = match crate::api::image_of("UnityEngine.CoreModule") {
                Some(i) => i,
                None => return (vec!["no image".into()], Action::None),
            };
            let mut out = vec![format!("imgc: {f:#x}(image {image:#x}, {idx:#x})")];
            match crate::veh::call2(f, image, idx) {
                Ok(r) => {
                    out.push(format!("  -> {r:#x}"));
                    if r >= 0x10000 && crate::safe::readable_at(r) {
                        out.extend(dump_qwords(r, 8));
                    }
                }
                Err(er) => out.push(format!("  fault: {er}")),
            }
            (out, Action::None)
        }
        // `imgfind <fnaddr> <max> <name>` — enumerate the image's classes with
        // `il2cpp_image_get_class(image, i)` and report every index whose object
        // holds a pointer to a string equal to <name> in its first 0x60 bytes.
        // Layout-agnostic: it also prints the offset where the name was found.
        "imgfind" => {
            let (f, max) = match (parse_addr(a1), parse_addr(a2)) {
                (Some(f), Some(m)) => (f, m),
                _ => return (vec!["usage: imgfind <fnaddr> <max> <name>".into()], Action::None),
            };
            let want = rest.trim();
            if want.is_empty() {
                return (vec!["usage: imgfind <fnaddr> <max> <name>".into()], Action::None);
            }
            let image = match crate::api::image_of("UnityEngine.CoreModule") {
                Some(i) => i,
                None => return (vec!["no image".into()], Action::None),
            };
            let mut out = vec![format!("imgfind: {f:#x} image {image:#x}, 0..{max}, '{want}'")];
            let mut hits = 0usize;
            let mut faults = 0usize;
            for i in 0..max {
                match crate::veh::call2(f, image, i) {
                    Ok(r) if r >= 0x10000 && crate::safe::readable_at(r) => {
                        for d in (0..0x60).step_by(8) {
                            if let Some(p) = crate::safe::read_u64(r + d) {
                                if let Some(s) = crate::safe::ascii_at(p as usize, 64) {
                                    if s == want {
                                        hits += 1;
                                        out.push(format!(
                                            "  [{i}] class {r:#x} name@+{d:#x} ns@{:?}",
                                            {
                                                // рядом обычно лежит namespace
                                                crate::safe::read_u64(r + d - 8)
                                                    .and_then(|q| crate::safe::ascii_at(q as usize, 64))
                                            }
                                        ));
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(_) => {
                        faults += 1;
                        if faults > 8 {
                            out.push(format!("  stopped at {i}: too many faults"));
                            break;
                        }
                    }
                }
                if hits >= 8 {
                    break;
                }
            }
            out.push(format!("  hits {hits}, faults {faults}"));
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
        // ---- M3: memory-only metadata walk --------------------------------
        // `types [assembly] [max]` — enumerate the image's classes through its
        // own codegen module (`typeInfoTable`). This needs *no* identified
        // il2cpp export: the whole class table is plain pointers in memory.
        "types" => {
            let asm = if a1.is_empty() { "UnityEngine.CoreModule" } else { a1 };
            let max = parse_addr(a2).unwrap_or(40).min(500);
            let image = match crate::api::image_of(asm) {
                Some(i) => i,
                None => return (vec![format!("no image for '{asm}'")], Action::None),
            };
            let (cofs, table, list) = crate::meta::classes_of_image(image, 40000);
            let mut out = vec![format!(
                "types: {asm} image {image:#x}, typeInfoTable {table:#x} (@+{cofs:#x}), {} class(es)",
                list.len()
            )];
            for c in list.iter().take(max) {
                out.push(format!("  {:#x}  {}", c.addr, c.full()));
            }
            (out, Action::None)
        }
        // `tfind <ns> <name> [assembly]` — exact class lookup; without an
        // assembly it walks every loaded image (VRChat types live in
        // Assembly-CSharp, Unity types in UnityEngine.CoreModule).
        "tfind" => {
            if a1.is_empty() || a2.is_empty() {
                return (vec!["usage: tfind <ns> <name> [assembly]".into()], Action::None);
            }
            let (ns, name) = (a1, a2);
            let mut out = Vec::new();
            if rest.trim().is_empty() {
                let hits = crate::meta::find_class_anywhere(ns, name);
                if hits.is_empty() {
                    out.push(format!("{ns}.{name}: not found in any image"));
                }
                for (asm, c) in hits {
                    out.push(format!(
                        "{ns}.{name} = {:#x} (assembly {asm})  {}",
                        c.addr,
                        crate::meta::layout_of(c.addr)
                    ));
                    for (n, off) in crate::meta::fields_of(c.addr).iter().take(12) {
                        out.push(format!("     field {n} @ +{off:#x}"));
                    }
                }
            } else {
                let asm = rest.trim();
                match crate::api::image_of(asm).and_then(|i| {
                    crate::meta::find_class(i, ns, name).map(|c| (i, c))
                }) {
                    Some((img, c)) => {
                        out.push(format!(
                            "{ns}.{name} = {:#x} (assembly {asm}, image {img:#x})  {}",
                            c.addr,
                            crate::meta::layout_of(c.addr)
                        ));
                        for (n, off) in crate::meta::fields_of(c.addr).iter().take(24) {
                            out.push(format!("     field {n} @ +{off:#x}"));
                        }
                    }
                    None => out.push(format!("{ns}.{name}: not found in {asm}")),
                }
            }
            (out, Action::None)
        }
        // `fields <class>` — every field with its instance offset.
        "fields" => {
            let c = match parse_addr(a1) {
                Some(c) => c,
                None => return (vec!["usage: fields <classaddr>".into()], Action::None),
            };
            let f = crate::meta::fields_of(c);
            let mut out = vec![format!(
                "{}: {} field(s) via {}",
                crate::meta::class_name(c).unwrap_or_else(|| "?".into()),
                f.len(),
                crate::meta::fields_ptr(c)
                    .map(|(o, p)| format!("+{o:#x} -> {p:#x}"))
                    .unwrap_or_else(|| "no field table".into())
            )];
            for (n, off) in f {
                out.push(format!("  {n:32} @ +{off:#x}"));
            }
            (out, Action::None)
        }
        // `methods <class> [substr]` — method names with their compiled entry
        // points (GameAssembly RVA included, so they can be called directly).
        "methods" => {
            let c = match parse_addr(a1) {
                Some(c) => c,
                None => return (vec!["usage: methods <classaddr> [filter]".into()], Action::None),
            };
            let filter = rest.trim().to_string();
            let ga = procs::own_module(crate::GAME_ASSEMBLY);
            let m = crate::meta::methods_of(c);
            let mut out = vec![format!(
                "{}: {} method(s) via {}",
                crate::meta::class_name(c).unwrap_or_else(|| "?".into()),
                m.len(),
                crate::meta::methods_ptr(c)
                    .map(|(o, p)| format!("+{o:#x} -> {p:#x}"))
                    .unwrap_or_else(|| "no method table".into())
            )];
            for (n, fp, _) in m.iter() {
                if !filter.is_empty() && !n.contains(&filter) {
                    continue;
                }
                let tag = match ga {
                    Some((b, _)) if *fp >= b => format!("GA+{:#x}", fp - b),
                    _ => tag_of(*fp).to_string(),
                };
                out.push(format!("  {n:44} {fp:#x} ({tag})"));
                if out.len() > 200 {
                    out.push("  … (truncated)".into());
                    break;
                }
            }
            (out, Action::None)
        }
        // `mcall <class> <method> [a1..a3]` — call a method's compiled entry
        // point under the VEH. Static getters need no args, instance methods
        // take `this` as a1; a struct return would need a hidden pointer, so
        // `pos` below is the ready-made example of that case.
        "mcall" => {
            let (c, mname) = match (parse_addr(a1), a2) {
                (Some(c), m) if !m.is_empty() => (c, m),
                _ => return (vec!["usage: mcall <classaddr> <method> [a1..a3]".into()], Action::None),
            };
            let entry = match crate::meta::method_entry(c, mname) {
                Some(e) => e,
                None => return (vec![format!("no method '{mname}' in {c:#x}")], Action::None),
            };
            let a: Vec<usize> = rest.split_whitespace().filter_map(parse_addr).collect();
            let g = |i: usize| a.get(i).copied().unwrap_or(0);
            let mut out = vec![format!("mcall {mname} @ {entry:#x}({:#x}, {:#x}, {:#x})", g(0), g(1), g(2))];
            match crate::veh::call3(entry, g(0), g(1), g(2)) {
                Ok(r) => {
                    out.push(format!("  -> {r:#x}"));
                    if r >= 0x10000 && crate::safe::readable_at(r) {
                        out.extend(dump_qwords(r, 6));
                    }
                }
                Err(e) => out.push(format!("  fault: {e}")),
            }
            (out, Action::None)
        }
        // `pos` — the M3 proof: Camera.main -> transform -> position, i.e. a
        // real Vector3 read out of the running game with plain memory reads
        // plus three guarded method calls.
        "pos" => (pos_probe(), Action::None),
        // `cam2` — Camera class found through the type table (not through any
        // il2cpp export), as a sanity check for the whole metadata walk.
        "cam2" => {
            let mut out = Vec::new();
            match core_image() {
                Some(img) => match crate::meta::find_class(img, "UnityEngine", "Camera") {
                    Some(c) => {
                        out.push(format!("Camera = {:#x} (image {img:#x})", c.addr));
                        out.push(format!("  {}", crate::meta::layout_of(c.addr)));
                        for (n, off) in crate::meta::fields_of(c.addr) {
                            out.push(format!("  field {n} @ +{off:#x}"));
                        }
                    }
                    None => out.push(format!("Camera not found in image {img:#x}")),
                },
                None => out.push("no image".into()),
            }
            (out, Action::None)
        }
        // `tbl <module> [maxofs]` — layout vote in the open: every qword slot of
        // a codegen module that holds a pointer, with how many of the entries it
        // points at look like `Il2CppClass`. Empty `types` is then explained.
        "tbl" => {
            let m = match parse_addr(a1) {
                Some(m) => m,
                None => return (vec!["usage: tbl <moduleaddr> [maxofs]".into()], Action::None),
            };
            let max = parse_addr(a2).unwrap_or(0x400).min(0x4000);
            let mut out = vec![format!("tbl {m:#x} .. +{max:#x}")];
            out.extend(crate::meta::probe_module(m, max));
            (out, Action::None)
        }
        // `ccls <ns> <name>` — class lookup through the *name string* in memory
        // (works when no class table can be found at all).
        "ccls" => {
            if a1.is_empty() || a2.is_empty() {
                return (vec!["usage: ccls <ns> <name>".into()], Action::None);
            }
            match crate::meta::class_by_name_string(a1, a2) {
                Some(c) => (
                    vec![format!(
                        "{}.{} = {:#x}  {}",
                        a1, a2, c.addr,
                        crate::meta::layout_of(c.addr)
                    )],
                    Action::None,
                ),
                None => (vec![format!("{}.{}: not found", a1, a2)], Action::None),
            }
        }
        // `ctab` — find the runtime's global class arrays (Unity 6 keeps them
        // outside the codegen module) and report what was found.
        "ctab" => {
            let t = crate::meta::class_tables();
            let mut out = vec![format!("ctab: {} table(s)", t.len())];
            for (a, n) in t.iter().take(20) {
                out.push(format!("  {a:#x}  {n} entries"));
            }
            let classes = crate::meta::global_classes();
            out.push(format!("ctab: {} class(es) readable", classes.len()));
            for c in classes.iter().take(parse_addr(a1).unwrap_or(12)) {
                out.push(format!("  {:#x}  {}", c.addr, c.full()));
            }
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
