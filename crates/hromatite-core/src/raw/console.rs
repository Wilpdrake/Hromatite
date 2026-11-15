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

#![deny(unsafe_op_in_unsafe_fn)]

use crate::il2cpp;
use crate::procs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

/// `imgset` override: the assembly record's `+0x28` is an Il2CppCodeGenModule
/// on this build, so image-taking API calls need an explicitly chosen image.
static IMAGE_OVERRIDE: AtomicUsize = AtomicUsize::new(0);

/// Image argument for `imgc`/`imgfind`: explicit override wins, otherwise the
/// assembly-record default (CoreModule, then mscorlib).
fn image_arg() -> Option<usize> {
    let over = IMAGE_OVERRIDE.load(Ordering::Relaxed);
    if over != 0 {
        return Some(over);
    }
    crate::api::image_of("UnityEngine.CoreModule").or_else(|| crate::api::image_of("mscorlib"))
}

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

impl Default for Reader {
    fn default() -> Self {
        Self::new()
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
    if let Some((b, s)) = ga
        && v >= b
        && v < b + s
    {
        return "GA";
    }
    if let Some((b, s)) = up
        && v >= b
        && v < b + s
    {
        return "UP";
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
        let mut chunks = crate::safe::ScanChunks::new(r.base, r.size, r.protect, 64 * 1024, need);
        while chunks.next_chunk() {
            let span = chunks.span();
            // last start such that a+pat.len() (the NUL) is still inside this chunk
            let last = chunks.candidate_end().saturating_sub(need);
            let mut a = span.base();
            while a <= last && out.len() < max {
                if crate::safe::matches_in_span(span, a, pat)
                    && crate::safe::read_u8_in_span(span, a + pat.len()) == Some(0)
                {
                    out.push(a);
                    a += pat.len();
                    continue;
                }
                a += 1;
            }
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
        let mut chunks = crate::safe::ScanChunks::new(r.base, r.size, r.protect, 64 * 1024, 0);
        while chunks.next_chunk() {
            crate::safe::find_qword_in_span(chunks.span(), want, max, &mut out);
        }
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
    if let Some(img) = core_image()
        && let Some(c) = crate::meta::find_class(img, ns, name)
    {
        return Some(c);
    }
    if let Some(c) = crate::meta::find_class_global(ns, name) {
        return Some(c);
    }
    // Last resort (Unity 6): a linear pass over the heap looking for a record
    // whose `+0x10` is exactly this name.
    if let Some((addr, cns)) = crate::meta::find_class_by_scan(ns, name).first() {
        return Some(crate::meta::Class {
            addr: *addr,
            name: name.to_string(),
            ns: cns.clone(),
        });
    }
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
    // Everything below uses the measured Unity 6 layout (crate::m3), not the
    // older `meta` guesses: name@+0x98, methods@+0x50, parent@+0x10.
    let cam = match crate::m3::find("UnityEngine", "Camera", 120_000).first() {
        Some((i, c)) => {
            out.push(format!(
                "pos: Camera class {c:#x} ({}), #{i}",
                crate::m3::class_full_name(*c)
            ));
            *c
        }
        None => return vec!["pos: Camera class not found (class table)".into()],
    };

    // 1. static Camera.main
    let main = match crate::m3::methods(cam).into_iter().find(|m| m.name == "get_main") {
        Some(m) => m.entry,
        None => {
            out.push("pos: no Camera.get_main".into());
            return out;
        }
    };
    // SAFETY: get_main is Camera's static accessor (no arguments) taken from
    // the live MethodInfo table; VEH contains a bad target.
    let cam_inst = match unsafe { crate::veh::call0(main) } {
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

    // 2. `Component.transform` — found through the parent chain
    //    (Camera → Behaviour → Component), never in Camera's own method list.
    let (owner, get_transform) = match crate::m3::method_in_chain(cam, "get_transform") {
        Some(v) => v,
        None => {
            out.push("pos: no get_transform in the class chain".into());
            return out;
        }
    };
    out.push(format!(
        "pos: get_transform from {}",
        crate::m3::class_full_name(owner)
    ));
    // SAFETY: instance getter, exactly one `this` pointer, from a live MethodInfo.
    let tr = match unsafe { crate::veh::call1(get_transform, cam_inst) } {
        Ok(v) => v,
        Err(e) => {
            out.push(format!("pos: get_transform faulted: {e}"));
            return out;
        }
    };
    out.push(format!(
        "pos: transform = {tr:#x} ({})",
        crate::m3::class_full_name(crate::safe::read_usize(tr).unwrap_or(0))
    ));
    if tr < 0x10000 {
        return out;
    }

    // 3. `Transform.position` and `localPosition` (struct return: hidden buffer
    //    in rcx, `this` in rdx — measured, not assumed).
    let trc = match crate::m3::find("UnityEngine", "Transform", 120_000).first() {
        Some((_, c)) => *c,
        None => {
            out.push("pos: Transform class not found".into());
            return out;
        }
    };
    for name in ["get_position", "get_localPosition"] {
        let f = match crate::m3::methods(trc).into_iter().find(|m| m.name == name) {
            Some(m) => m.entry,
            None => {
                out.push(format!("pos: no Transform.{name}"));
                continue;
            }
        };
        let mut buf = [0u64; 2];
        // SAFETY: get_*Position returns a 12-byte Vector3 through a hidden
        // output pointer (first argument) and takes `this` second; `buf` is a
        // live two-qword writable buffer on our own stack.
        match unsafe { crate::veh::call2(f, buf.as_mut_ptr() as usize, tr) } {
            Ok(_) => {
                let (x, y, z) = vec3(buf);
                out.push(format!(
                    "pos: {name} = ({x:.3}, {y:.3}, {z:.3})  [raw {:#x} {:#x}]",
                    buf[0], buf[1]
                ));
            }
            Err(e) => out.push(format!("pos: {name} faulted: {e}")),
        }
    }
    out
}

/// `Il2CppString*` → Rust string (length at +0x10, UTF-16 chars at +0x14).
fn il2cpp_string(p: usize) -> Option<String> {
    if p < 0x10000 {
        return None;
    }
    let len = crate::safe::read_u32(p + 0x10)? as usize;
    if len > 4096 {
        return None;
    }
    let bytes = crate::safe::read_bytes(p + 0x14, len.checked_mul(2)?)?;
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// `who` — every player in the instance, through the public SDK API
/// (`VRC.SDKBase.VRCPlayerApi`): the list comes from `get_AllPlayers`
/// (`List<VRCPlayerApi>`), id from `GetPlayerId(player)`, position from
/// `GetPosition(player)` (Vector3 through a hidden output pointer).
///
/// This deliberately avoids VRChat's own obfuscated classes: the SDK surface is
/// stable across builds and is what world scripts themselves use.
/// Class by namespace/name; if the class table has not materialised it yet,
/// walk the assembly's `image_get_class(image, i)` (which builds it on demand).
fn class_or_materialise(ns: &str, name: &str, assembly: &str) -> Option<usize> {
    if let Some((_, c)) = crate::m3::find(ns, name, 120_000).first() {
        return Some(*c);
    }
    let (ga, gasize) = crate::procs::own_module(crate::GAME_ASSEMBLY)?;
    let f = ga + crate::m3::IMAGE_GET_CLASS_RVA as usize;
    if f + 8 > ga + gasize {
        return None;
    }
    let first = crate::il2cpp::find_domain()?;
    let image = crate::m3::image_of(first, assembly)?;
    for i in 0..40_000usize {
        // SAFETY: image_get_class(image, i) with the measured ABI; VEH guards it.
        let cls = unsafe { crate::veh::call2(f, image, i) }.ok()?;
        if cls < 0x10000 || !crate::safe::readable_at(cls) {
            continue;
        }
        if crate::m3::class_name(cls).as_deref() == Some(name)
            && crate::m3::class_namespace(cls).as_deref() == Some(ns)
        {
            return Some(cls);
        }
    }
    None
}

/// Method entries and classes that `who`/`esp` need, resolved once per process.
///
/// Resolving them walks the whole type table (tens of milliseconds) and `esp`
/// runs four to five times per second, so the records are `Box::leak`ed instead
/// of cached under a `Mutex`: a panic inside the guard longjmps past
/// destructors, and a leaked `MutexGuard` would hang every later command.
struct Sdk {
    api_class: usize,
    get_all: usize,
    get_id: usize,
    get_pos: usize,
    get_objects: usize,
    obj_name: usize,
}

struct CamApi {
    get_main: usize,
    get_transform: usize,
    tr_position: usize,
    tr_forward: usize,
    get_proj: usize,
    get_view: usize,
    get_pixw: usize,
    get_pixh: usize,
}

static SDK: AtomicUsize = AtomicUsize::new(0);
static CAM: AtomicUsize = AtomicUsize::new(0);

fn method_entry(ms: &[crate::m3::Method], name: &str) -> usize {
    ms.iter().find(|m| m.name == name).map_or(0, |m| m.entry)
}

fn sdk() -> Option<&'static Sdk> {
    let cached = SDK.load(Ordering::Relaxed);
    if cached != 0 {
        // SAFETY: `cached` is a `Box::into_raw` pointer that is never freed.
        return Some(unsafe { &*(cached as *const Sdk) });
    }
    let api_class = class_or_materialise("VRC.SDKBase", "VRCPlayerApi", "VRCSDKBase")?;
    let ms = crate::m3::methods(api_class);
    let get_all = method_entry(&ms, "get_AllPlayers");
    let get_id = method_entry(&ms, "GetPlayerId");
    if get_all == 0 || get_id == 0 {
        return None;
    }
    let obj_name = crate::m3::find("UnityEngine", "Object", 120_000)
        .first()
        .and_then(|(_, c)| {
            crate::m3::methods(*c)
                .into_iter()
                .find(|m| m.name == "get_name")
                .map(|m| m.entry)
        })
        .unwrap_or(0);
    let rec = Sdk {
        api_class,
        get_all,
        get_id,
        get_pos: method_entry(&ms, "GetPosition"),
        get_objects: method_entry(&ms, "GetPlayerObjects"),
        obj_name,
    };
    let ptr = Box::into_raw(Box::new(rec));
    SDK.store(ptr as usize, Ordering::Relaxed);
    // SAFETY: freshly boxed and leaked on purpose; valid for the process.
    Some(unsafe { &*ptr })
}

fn cam_api() -> Option<&'static CamApi> {
    let cached = CAM.load(Ordering::Relaxed);
    if cached != 0 {
        // SAFETY: `cached` is a `Box::into_raw` pointer that is never freed.
        return Some(unsafe { &*(cached as *const CamApi) });
    }
    let cam_class = crate::m3::find("UnityEngine", "Camera", 120_000).first()?.1;
    let (trc, get_transform) = crate::m3::method_in_chain(cam_class, "get_transform")?;
    let trms = crate::m3::methods(trc);
    let ms = crate::m3::methods(cam_class);
    let get_proj = method_entry(&ms, "get_projectionMatrix");
    let get_view = method_entry(&ms, "get_worldToCameraMatrix");
    let get_main = method_entry(&ms, "get_main");
    if get_proj == 0 || get_view == 0 || get_main == 0 {
        return None;
    }
    let rec = CamApi {
        get_main,
        get_transform,
        tr_position: method_entry(&trms, "get_position"),
        tr_forward: method_entry(&trms, "get_forward"),
        get_proj,
        get_view,
        get_pixw: method_entry(&ms, "get_pixelWidth"),
        get_pixh: method_entry(&ms, "get_pixelHeight"),
    };
    let ptr = Box::into_raw(Box::new(rec));
    CAM.store(ptr as usize, Ordering::Relaxed);
    // SAFETY: freshly boxed and leaked on purpose; valid for the process.
    Some(unsafe { &*ptr })
}

/// One player as the SDK sees it.
struct Pl {
    api: usize,
    id: usize,
    pos: Option<(f32, f32, f32)>,
    name: Option<String>,
}

struct Players {
    list: usize,
    size: usize,
    items: usize,
    rows: Vec<Pl>,
}

/// Walk `List<VRCPlayerApi>` into plain Rust data. Positions come from
/// `GetPosition(player)` (Vector3 through a hidden output pointer).
fn enumerate_players(sdk: &Sdk, with_name: bool) -> Result<Players, String> {
    // SAFETY: get_AllPlayers is the SDK's static accessor (no arguments).
    let list = match unsafe { crate::veh::call0(sdk.get_all) } {
        Ok(v) if v >= 0x10000 => v,
        Ok(_) => {
            return Ok(Players {
                list: 0,
                size: 0,
                items: 0,
                rows: Vec::new(),
            });
        }
        Err(e) => return Err(format!("get_AllPlayers faulted: {e}")),
    };
    // List<T>: _items @+0x10, _size @+0x18; Il2CppArray: length @+0x18, data @+0x20.
    let items = crate::safe::read_usize(list + 0x10).unwrap_or(0);
    let size = crate::safe::read_u32(list + 0x18).unwrap_or(0) as usize;
    let maxlen = crate::safe::read_usize(items + 0x18).unwrap_or(0) as usize;
    let mut rows = Vec::new();
    for i in 0..size.min(maxlen).min(256) {
        let p = crate::safe::read_usize(items + 0x20 + i * 8).unwrap_or(0);
        if p < 0x10000 {
            continue;
        }
        // SAFETY: GetPlayerId(VRCPlayerApi*) is a pure accessor on a live instance.
        let id = unsafe { crate::veh::call1(sdk.get_id, p) }.unwrap_or(usize::MAX);
        let mut pos = None;
        if sdk.get_pos != 0 {
            let mut buf = [0u64; 2];
            // SAFETY: GetPosition returns Vector3 through a hidden output
            // pointer (rcx) with `this` in rdx; buf is a live stack buffer.
            if unsafe { crate::veh::call2(sdk.get_pos, buf.as_mut_ptr() as usize, p) }.is_ok() {
                pos = Some(vec3(buf));
            }
        }
        // The player's GameObject name (avatar/root name) — handy for a human
        // and read through UnityEngine.Object.get_name, nothing VRChat-specific.
        let mut name = None;
        if with_name
            && sdk.get_objects != 0
            && sdk.obj_name != 0
            && let Ok(arr) = unsafe { crate::veh::call1(sdk.get_objects, p) }
            && arr >= 0x10000
        {
            let len = crate::safe::read_usize(arr + 0x18).unwrap_or(0) as usize;
            let first = crate::safe::read_usize(arr + 0x20).unwrap_or(0);
            if len > 0 && first >= 0x10000 {
                // SAFETY: Object.get_name on a live UnityEngine.Object.
                if let Ok(s) = unsafe { crate::veh::call1(sdk.obj_name, first) } {
                    name = il2cpp_string(s).filter(|s| !s.is_empty());
                }
            }
        }
        rows.push(Pl { api: p, id, pos, name });
    }
    Ok(Players {
        list,
        size,
        items,
        rows,
    })
}

fn who_probe() -> Vec<String> {
    let mut out = Vec::new();
    let Some(sdk) = sdk() else {
        return vec!["who: VRCPlayerApi not resolved (no get_AllPlayers/GetPlayerId)".into()];
    };
    out.push(format!(
        "who: VRCPlayerApi {:#x}, get_pos {}",
        sdk.api_class,
        sdk.get_pos != 0
    ));
    let ps = match enumerate_players(sdk, true) {
        Ok(v) => v,
        Err(e) => {
            out.push(format!("who: {e}"));
            return out;
        }
    };
    out.push(format!(
        "who: list {:#x}, size {}, items {:#x}, cap {}",
        ps.list,
        ps.size,
        ps.items,
        ps.rows.len()
    ));
    for (i, p) in ps.rows.iter().enumerate() {
        let pos = match p.pos {
            Some((x, y, z)) => format!("pos=({x:.2}, {y:.2}, {z:.2})"),
            None => "pos=?".to_string(),
        };
        let label = match &p.name {
            Some(n) => format!(" go=\"{n}\""),
            None => String::new(),
        };
        out.push(format!("  player #{i}: api {:#x} id={} {pos}{label}", p.api, p.id));
    }
    out.push(format!("who: {} player(s)", ps.rows.len()));
    out
}

// ---- esp: players projected to screen pixels (M4) --------------------------

/// `Matrix4x4` (64 B) returned through the hidden output pointer in `rcx`,
/// `this` in `rdx` — the same ABI the Vector3 getters use, only bigger.
fn call_matrix(f: usize, this: usize) -> Option<[f32; 16]> {
    let mut buf = [0u64; 8];
    // SAFETY: get_projectionMatrix/get_worldToCameraMatrix are pure getters; the
    // buffer is a live 64-byte stack array and the VEH guards the call.
    unsafe { crate::veh::call2(f, buf.as_mut_ptr() as usize, this) }.ok()?;
    let mut out = [0f32; 16];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = f32::from_bits((buf[i / 2] >> ((i % 2) * 32)) as u32);
    }
    Some(out)
}

/// `Transform.get_position` / `get_forward` (Vector3 through a hidden pointer).
fn transform_vec3(entry: usize, this: usize) -> Option<(f32, f32, f32)> {
    if entry == 0 || this < 0x10000 {
        return None;
    }
    let mut buf = [0u64; 2];
    // SAFETY: instance getter, hidden Vector3 return pointer in rcx and `this`
    // in rdx; buf is a live stack buffer, VEH guards the call.
    unsafe { crate::veh::call2(entry, buf.as_mut_ptr() as usize, this) }.ok()?;
    Some(vec3(buf))
}

/// Column-major 4x4 multiply (`Matrix4x4` storage order in Unity).
fn mul4(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut r = [0f32; 16];
    for col in 0..4 {
        for row in 0..4 {
            let mut s = 0f32;
            for k in 0..4 {
                s += a[row + k * 4] * b[k + col * 4];
            }
            r[row + col * 4] = s;
        }
    }
    r
}

fn mul_vec4(m: &[f32; 16], v: [f32; 4]) -> [f32; 4] {
    let mut r = [0f32; 4];
    for row in 0..4 {
        r[row] = m[row] * v[0] + m[row + 4] * v[1] + m[row + 8] * v[2] + m[row + 12] * v[3];
    }
    r
}

/// World → pixel projection. Screen coordinates are top-left origin, because
/// that is what the X11 overlay draws in; Unity works bottom-left.
fn project(pv: &[f32; 16], w: f32, h: f32, p: (f32, f32, f32)) -> Option<(f32, f32)> {
    let c = mul_vec4(pv, [p.0, p.1, p.2, 1.0]);
    if c[3] <= 0.0001 {
        return None; // behind the camera
    }
    let ndc_x = c[0] / c[3];
    let ndc_y = c[1] / c[3];
    Some(((ndc_x * 0.5 + 0.5) * w, (0.5 - ndc_y * 0.5) * h))
}

/// `esp` — every player's screen box, plus the camera rig it was computed from.
///
/// `projectionMatrix * worldToCameraMatrix` is read through the same guarded
/// struct-return path the Vector3 getters use, and the multiplication happens in
/// Rust, so no unknown ABI for struct *arguments* is involved (that is what
/// makes `WorldToScreenPoint` risky and this safe).
///
/// Self-check in the header: the main camera's own position must project to the
/// centre of the viewport, so `err` (pixels off centre) is 0 in a sane world.
fn esp_probe() -> Vec<String> {
    let mut out = Vec::new();
    let (Some(cam), Some(sdk)) = (cam_api(), sdk()) else {
        return vec![format!(
            "esp: unresolved (camera {}, players {})",
            cam_api().is_some(),
            sdk().is_some()
        )];
    };
    // SAFETY: Camera.get_main is the static accessor.
    let inst = match unsafe { crate::veh::call0(cam.get_main) } {
        Ok(v) if v >= 0x10000 => v,
        Ok(v) => {
            out.push(format!("esp: no main camera ({v:#x})"));
            return out;
        }
        Err(e) => {
            out.push(format!("esp: Camera.main faulted: {e}"));
            return out;
        }
    };
    let mut pw = if cam.get_pixw != 0 {
        // SAFETY: Camera.pixelWidth is a constant instance getter.
        unsafe { crate::veh::call1(cam.get_pixw, inst) }.unwrap_or(0) as u32
    } else {
        0
    };
    let mut ph = if cam.get_pixh != 0 {
        // SAFETY: Camera.pixelHeight is a constant instance getter.
        unsafe { crate::veh::call1(cam.get_pixh, inst) }.unwrap_or(0) as u32
    } else {
        0
    };
    if pw == 0 || ph == 0 {
        pw = 1920;
        ph = 1080;
    }
    // SAFETY: Component.get_transform on a live Camera instance.
    let tr = unsafe { crate::veh::call1(cam.get_transform, inst) }.unwrap_or(0);
    let cam_pos = transform_vec3(cam.tr_position, tr);
    let fwd = transform_vec3(cam.tr_forward, tr);
    let (Some(proj), Some(view)) = (
        call_matrix(cam.get_proj, inst),
        call_matrix(cam.get_view, inst),
    ) else {
        out.push("esp: projection/worldToCamera matrix faulted".into());
        return out;
    };
    let pv = mul4(&proj, &view);
    let (fw, fh) = (pw as f32, ph as f32);
    let err = cam_pos
        .and_then(|p| project(&pv, fw, fh, p))
        .map(|(x, y)| (x - fw / 2.0).abs().max((y - fh / 2.0).abs()))
        .unwrap_or(-1.0);
    let players = match enumerate_players(sdk, true) {
        Ok(v) => v.rows,
        Err(e) => {
            out.push(format!("esp: {e}"));
            return out;
        }
    };
    let head = |p: (f32, f32, f32)| (p.0, p.1 + 1.70, p.2);
    out.push(format!(
        "esp: head w={pw} h={ph} err={err:.1} cam=({:.2}, {:.2}, {:.2}) fwd=({:.2}, {:.2}, {:.2})",
        cam_pos.map_or(0.0, |p| p.0),
        cam_pos.map_or(0.0, |p| p.1),
        cam_pos.map_or(0.0, |p| p.2),
        fwd.map_or(0.0, |p| p.0),
        fwd.map_or(0.0, |p| p.1),
        fwd.map_or(0.0, |p| p.2)
    ));
    for p in players {
        let Some(pos) = p.pos else { continue };
        let Some((sx, sy)) = project(&pv, fw, fh, pos) else {
            continue;
        };
        let Some((_, hy)) = project(&pv, fw, fh, head(pos)) else {
            continue;
        };
        let dist = match cam_pos {
            Some(c) => {
                let (dx, dy, dz) = (pos.0 - c.0, pos.1 - c.1, pos.2 - c.2);
                (dx * dx + dy * dy + dz * dz).sqrt()
            }
            None => -1.0,
        };
        // `w=` carries the world position too: the host overlay uses it for the
        // radar, and it costs nothing here.
        out.push(format!(
            "esp: id={} d={dist:.2} x={sx:.1} y={sy:.1} h={hy:.1} w={:.2},{:.2},{:.2} name={}",
            p.id,
            pos.0,
            pos.1,
            pos.2,
            p.name.unwrap_or_default()
        ));
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
                "esp                       — игроки в пикселях экрана (для оверлея)".into(),
                "cam2                      — Camera через таблицу типов (без API)".into(),
                "meta                      — version/счётчики расшифрованной metadata".into(),
                "typelist <filter> [max]    — каталог типов из metadata".into(),
                "typedump <ns.name>        — поля (тип+offset) и методы (RVA)".into(),
                "meths / fields2 <ns.name> — только методы / только поля".into(),
                "m3f <ns> [name]           — классы из таблицы классов (без вызовов)".into(),
                "m3c <class>               — поля и методы класса (Unity 6 layout)".into(),
                "m3l <asm> [substr] [max]  — перечислить классы образа (создаёт их)".into(),
                "m3s <substr>              — поиск по уже созданным классам".into(),
                "latec                      — все материализованные классы с LateUpdate".into(),
                "latehook #idx|<class>      — hook конкретного LateUpdate (потом late)".into(),
                "late                       — hits/частота/tid LateUpdate-хука".into(),
                "pos                       — Camera.main -> transform -> position".into(),
                "imgset <image|reset>      — какой Il2CppImage* отдавать imgc/imgfind".into(),
                "dumpmeta <Z:\\path>        — сохранить расшифрованный metadata blob".into(),
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
            let rest = line.split_once(char::is_whitespace).map_or("", |x| x.1);
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
                let n = pat.len();
                let mut chunks = crate::safe::ScanChunks::new(r.base, r.size, r.protect, 64 * 1024, n);
                while chunks.next_chunk() {
                    let span = chunks.span();
                    let mut a = span.base();
                    let end = chunks.candidate_end();
                    // регион уже проверен как committed+readable, поэтому читаем
                    // через snapshot-границу safe: VirtualQuery на каждый байт — минуты
                    while a + n <= end && total < 64 {
                        if crate::safe::matches_in_span(span, a, &pat) {
                            total += 1;
                            hits.push(format!("  {a:#x}"));
                            a += n;
                            continue;
                        }
                        a += 1;
                    }
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
                let mut slots = Vec::new();
                let mut chunks = crate::safe::ScanChunks::new(r.base, r.size, r.protect, 64 * 1024, 0);
                while chunks.next_chunk() {
                    crate::safe::find_qword_in_span(chunks.span(), want, 24 - hits.len(), &mut slots);
                }
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
                let bytes: Option<Vec<u8>> = cleaned.len().is_multiple_of(2).then(|| {
                    (0..cleaned.len())
                        .step_by(2)
                        .map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).ok())
                        .collect::<Option<Vec<u8>>>()
                }).flatten();
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
            // SAFETY: this is the explicit low-level `call` command: the user
            // supplied the Win64 address and all opaque pointer arguments knowingly.
            match unsafe { crate::veh::call3(f, g(0), g(1), g(2)) } {
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
                // SAFETY: the explicit guard command is a delegated raw Win64
                // probe; VEH captures the calling thread before dereferencing code.
                match unsafe { crate::veh::call0(addr) } {
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
            // SAFETY: target is computed from UnityPlayer base + the user supplied
            // lazy stub offset; arguments are forwarded opaque integers/pointers.
            match unsafe { crate::veh::call3(target, get(0), get(1), get(2)) } {
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
            // SAFETY: explicit c3 probe with live image and NUL-terminated names.
            match unsafe { crate::veh::call3(f, image, ns_c.as_ptr() as usize, nm_c.as_ptr() as usize) } {
                Ok(r) => {
                    out.push(format!("  -> {r:#x}"));
                    if r >= 0x10000 && crate::safe::readable_at(r) {
                        for d in (0..0x40).step_by(8) {
                            if let Some(p) = crate::safe::read_u64(r + d)
                                && let Some(s) = crate::safe::ascii_at(p as usize, 48)
                            {
                                out.push(format!("     [{d:#x}] -> '{s}'"));
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
            // SAFETY: explicit cls3 candidate with a live image and valid C strings.
            let called = unsafe {
                crate::veh::call3(f, image, ns_c.as_ptr() as usize, nm_c.as_ptr() as usize)
            };
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
        // `imgset [addr|reset]` — choose the `Il2CppImage*` handed to `imgc` and
        // `imgfind`. Needed on this build: the assembly record's `+0x28` is an
        // `Il2CppCodeGenModule`, and calling `image_get_class` with it returns
        // structures that carry no name. `q <assembly-record>` lists the
        // candidate pointers; try them one by one.
        "imgset" => {
            if a1.is_empty() || a1 == "reset" || a1 == "0" {
                IMAGE_OVERRIDE.store(0, Ordering::Relaxed);
                (vec![format!("imgset: reset (assembly record default)")], Action::None)
            } else {
                match parse_addr(a1) {
                    Some(v) if v >= 0x10000 => {
                        IMAGE_OVERRIDE.store(v, Ordering::Relaxed);
                        let head = crate::safe::read_u64(v)
                            .and_then(|p| crate::safe::ascii_at(p as usize, 48))
                            .unwrap_or_else(|| "?"
                                .to_string());
                        (vec![format!("imgset: image {v:#x} (name ptr -> '{head}')")], Action::None)
                    }
                    _ => (vec!["usage: imgset <imageaddr>|reset".into()], Action::None),
                }
            }
        }
        // ---- Unity 6 layout (verified live 2026-11-15, see m3.rs) ---------
        // `m3f <ns> [name]` — search the class table directly (no API call).
        "m3f" => {
            let ns = a1;
            let name = a2;
            let hits = crate::m3::find(ns, name, 120_000);
            let mut out = vec![format!(
                "m3f {ns:?} {name:?}: {} class(es), table {:#x}",
                hits.len(),
                crate::m3::class_table().unwrap_or(0)
            )];
            for (i, p) in hits.iter().take(32) {
                out.push(format!(
                    "  #{i} {p:#x}  {}",
                    crate::m3::class_full_name(*p)
                ));
            }
            (out, Action::None)
        }
        // `m3s <substr>` — search only the *materialised* class table (no calls).
        "m3s" => {
            let want = a1.to_ascii_lowercase();
            let mut out = vec![format!("m3s {want:?}")];
            let mut n = 0usize;
            for (i, p) in crate::m3::classes(120_000) {
                let nm = crate::m3::class_name(p).unwrap_or_default();
                if want.is_empty() || nm.to_ascii_lowercase().contains(&want) {
                    n += 1;
                    if n <= 40 {
                        out.push(format!(
                            "  #{i} {p:#x}  {}",
                            crate::m3::class_full_name(p)
                        ));
                    }
                }
            }
            out.insert(1, format!("  {n} match(es)"));
            (out, Action::None)
        }
        // `m3l <assembly> [substr] [max]` — enumerate an image's classes through
        // `il2cpp_image_get_class`, which also *materialises* lazily-built
        // classes (VRChat's own assembly is not in the table until touched).
        "m3l" => {
            let asm = if a1.is_empty() { "Assembly-CSharp" } else { a1 };
            let filt = a2.to_ascii_lowercase();
            let max = parse_addr(rest.trim()).unwrap_or(30_000).min(60_000);
            let (ga, gasize) = match crate::procs::own_module(crate::GAME_ASSEMBLY) {
                Some(v) => v,
                None => return (vec!["m3l: GameAssembly.dll not loaded".into()], Action::None),
            };
            let f = ga + crate::m3::IMAGE_GET_CLASS_RVA as usize;
            if f + 8 > ga + gasize {
                return (vec!["m3l: image_get_class outside the module".into()], Action::None);
            }
            let first = match crate::il2cpp::find_domain() {
                Some(v) => v,
                None => return (vec!["m3l: get_corlib getter not found".into()], Action::None),
            };
            let image = match crate::m3::image_of(first, asm) {
                Some(i) => i,
                None => return (vec![format!("m3l: no image for '{asm}'")], Action::None),
            };
            let mut out = vec![format!(
                "m3l {asm}: image {image:#x}, fn {f:#x}, filter {filt:?}, max {max}"
            )];
            let mut seen = 0usize;
            let mut shown = 0usize;
            let mut faults = 0usize;
            for i in 0..max {
                // SAFETY: image_get_class(image, i) is the measured (image,index)
                // ABI; i stays inside the caller's bound and VEH contains faults.
                let cls = match unsafe { crate::veh::call2(f, image, i) } {
                    Ok(v) => v,
                    Err(_) => {
                        faults += 1;
                        if faults > 32 {
                            out.push(format!("  stopped at {i}: too many faults"));
                            break;
                        }
                        continue;
                    }
                };
                if cls < 0x10000 || !crate::safe::readable_at(cls) {
                    continue;
                }
                seen += 1;
                let nm = crate::m3::class_name(cls).unwrap_or_default();
                if nm.is_empty() {
                    continue;
                }
                if filt.is_empty() || nm.to_ascii_lowercase().contains(&filt) {
                    shown += 1;
                    if shown <= 60 {
                        out.push(format!(
                            "  #{i} {cls:#x}  {}.{nm}",
                            crate::m3::class_namespace(cls).unwrap_or_default()
                        ));
                    }
                }
            }
            out.push(format!("  {seen} class(es) seen, {shown} match(es), {faults} fault(s)"));
            (out, Action::None)
        }
        // `m3c <class>` — fields and methods through the measured layout.
        "m3c" => match parse_addr(a1) {
            Some(c) if c >= 0x10000 => {
                let mut out = vec![format!(
                    "m3c {c:#x}  {}  (image {:#x})",
                    crate::m3::class_full_name(c),
                    crate::safe::read_usize(c).unwrap_or(0)
                )];
                let fld = crate::m3::fields(c);
                out.push(format!("  fields: {}", fld.len()));
                for f in fld.iter().take(40) {
                    out.push(format!(
                        "    +{:#06x}? {:<28} type {:#x} parent {:#x}",
                        f.offset_or_index, f.name, f.ty, f.parent
                    ));
                }
                let ms = crate::m3::methods(c);
                out.push(format!("  methods: {}", ms.len()));
                for m in ms.iter().take(60) {
                    out.push(format!("    {:<36} entry {:#x}", m.name, m.entry));
                }
                (out, Action::None)
            }
            _ => (vec!["usage: m3c <classaddr>".into()], Action::None),
        },
        // `m3i <assembly>` — Il2CppImage* (images are getter+i*0x48).
        "m3i" => match crate::il2cpp::find_domain() {
            Some(first) => {
                let mut out = vec![format!("m3i: corlib image {first:#x}")];
                let want = if a1.is_empty() { "UnityEngine.CoreModule" } else { a1 };
                match crate::m3::image_of(first, want) {
                    Some(img) => {
                        out.push(format!(
                            "  {want}: image {img:#x}, il2cpp_image_get_class {:#x}",
                            crate::m3::IMAGE_GET_CLASS_RVA
                        ));
                    }
                    None => out.push(format!("  {want}: not found")),
                }
                (out, Action::None)
            }
            None => (vec!["m3i: get_corlib getter not found".into()], Action::None),
        },
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
            let image = match image_arg() {
                Some(i) => i,
                None => return (vec!["no image".into()], Action::None),
            };
            let mut out = vec![format!("imgc: {f:#x}(image {image:#x}, {idx:#x})")];
            // SAFETY: imgc probes image_get_class(image,index) with a live image.
            match unsafe { crate::veh::call2(f, image, idx) } {
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
            let image = match image_arg() {
                Some(i) => i,
                None => return (vec!["no image".into()], Action::None),
            };
            let mut out = vec![format!("imgfind: {f:#x} image {image:#x}, 0..{max}, '{want}'")];
            let mut hits = 0usize;
            let mut faults = 0usize;
            for i in 0..max {
                // SAFETY: imgfind repeats the same image/index ABI and never
                // transcends an index derived from the user's bounded max.
                match unsafe { crate::veh::call2(f, image, i) } {
                    Ok(r) if r >= 0x10000 && crate::safe::readable_at(r) => {
                        for d in (0..0x100).step_by(8) {
                            if let Some(p) = crate::safe::read_u64(r + d)
                                && let Some(s) = crate::safe::ascii_at(p as usize, 64)
                                && s == want
                            {
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
            // SAFETY: entry comes from MethodInfo and its three opaque arguments
            // are supplied explicitly by the user of the low-level mcall command.
            match unsafe { crate::veh::call3(entry, g(0), g(1), g(2)) } {
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
        // `who` — players in the instance through the public SDK surface:
        // VRCPlayerApi.get_AllPlayers → List<VRCPlayerApi> → GetPlayerId /
        // GetPosition. Obfuscated VRChat classes are not needed at all.
        "who" => (who_probe(), Action::None),
        // `esp` — the same player list projected to *pixels*, so the host
        // overlay can draw boxes over them. Costs four guarded calls per line of
        // the camera rig (main, transform, both matrices, pixel size), which is
        // why the method entries are cached in `CamApi`.
        "esp" => (esp_probe(), Action::None),
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
        // `cscan <ns> <name>` — linear heap pass for a class record by its name
        // (the fallback that works when no table layout is available).
        "cscan" => {
            if a1.is_empty() || a2.is_empty() {
                return (vec!["usage: cscan <ns> <name>".into()], Action::None);
            }
            let hits = crate::meta::find_class_by_scan(a1, a2);
            let mut seen: Vec<usize> = Vec::new();
            let mut out = vec![format!("cscan {}.{}: {} hit(s)", a1, a2, hits.len())];
            let mut best: Option<usize> = None;
            for (addr, cns) in hits.iter() {
                if seen.contains(addr) {
                    continue;
                }
                seen.push(*addr);
                let nf = crate::meta::fields_of(*addr).len();
                let nm = crate::meta::methods_of(*addr).len();
                out.push(format!(
                    "  {addr:#x} ns='{cns}' fields={nf} methods={nm}  {}",
                    crate::meta::field_offset(*addr, "m_LocalPosition")
                        .map(|o| format!("m_LocalPosition@+{o:#x}"))
                        .unwrap_or_default()
                ));
                if nm > 0 && best.is_none() {
                    best = Some(*addr);
                }
                // layout probe on the first few candidates: this is what turns a
                // "record named Camera" into a usable class
                if seen.len() <= 4 {
                    let shapes = crate::meta::probe_layout(*addr);
                    if shapes.is_empty() {
                        out.push(format!("    {addr:#x}: no field/method shape"));
                    }
                    for sh in shapes.iter().take(4) {
                        out.push(format!("    {addr:#x}:{sh}"));
                    }
                }
                if out.len() > 24 {
                    break;
                }
            }
            if let Some(b) = best {
                out.push(format!("cscan: class with a method table = {b:#x}"));
                for (m, fp, _) in crate::meta::methods_of(b).iter().take(12) {
                    out.push(format!("   method {m} @ {fp:#x}"));
                }
                for (f, o) in crate::meta::fields_of(b).iter().take(12) {
                    out.push(format!("   field {f} @ +{o:#x}"));
                }
            }
            (out, Action::None)
        }
        // `mfind <method-name>` — locate a MethodInfo through its name string and
        // walk to the declaring class (layout-free route to `Il2CppClass`).
        "mfind" => {
            if a1.is_empty() {
                return (vec!["usage: mfind <method-name>".into()], Action::None);
            }
            let hits = crate::meta::find_method_by_name(a1);
            let mut out = vec![format!("mfind '{a1}': {} hit(s)", hits.len())];
            let mut seen: Vec<usize> = Vec::new();
            for (mi, fp, klass) in hits.iter() {
                out.push(format!(
                    "  MethodInfo {mi:#x} fn {fp:#x} klass {klass:#x} ({})",
                    crate::meta::class_name(*klass).unwrap_or_else(|| "?".into())
                ));
                if seen.contains(klass) {
                    continue;
                }
                seen.push(*klass);
                let f = crate::meta::fields_of(*klass);
                let m = crate::meta::methods_of(*klass);
                out.push(format!(
                    "    class {klass:#x}: {} field(s), {} method(s)",
                    f.len(),
                    m.len()
                ));
                for (n, off) in f.iter().take(10) {
                    out.push(format!("      field {n} @ +{off:#x}"));
                }
                for (n, e, _) in m.iter().take(10) {
                    out.push(format!("      method {n} @ {e:#x}"));
                }
            }
            (out, Action::None)
        }
        // `lay <class>` — probe every slot of a candidate class for field/method
        // table shapes and report the ones that fit (layout discovery).
        "lay" => {
            let c = match parse_addr(a1) {
                Some(c) => c,
                None => return (vec!["usage: lay <classaddr>".into()], Action::None),
            };
            let mut out = vec![format!(
                "lay {c:#x} ({}):",
                crate::meta::class_name(c).unwrap_or_else(|| "?".into())
            )];
            let hits = crate::meta::probe_layout(c);
            if hits.is_empty() {
                out.push("  no field/method table shape found in +0x10..+0x300".into());
            }
            out.extend(hits);
            (out, Action::None)
        }
        // `iscan` — class records recognised by their image back-pointer
        // (`class+0x00` → Il2CppImage whose name ends in ".dll"), which works even
        // when `Il2CppClass.name` is not materialized.
        "iscan" => {
            let max = parse_addr(a2).unwrap_or(64).min(2000);
            let hits = crate::meta::scan_classes_by_image(max);
            let mut out = vec![format!("iscan: {} record(s)", hits.len())];
            let mut by_img: Vec<(String, usize)> = Vec::new();
            for (_, img, _) in hits.iter() {
                match by_img.iter_mut().find(|(n, _)| n == img) {
                    Some(e) => e.1 += 1,
                    None => by_img.push((img.clone(), 1)),
                }
            }
            for (n, c) in by_img.iter().take(12) {
                out.push(format!("  image {n}: {c} class record(s)"));
            }
            let want = if a1.is_empty() { "" } else { a1 };
            for (addr, img, nm) in hits.iter().filter(|(_, _, nm)| {
                want.is_empty() || nm.eq_ignore_ascii_case(want)
            }) {
                out.push(format!("  {addr:#x} image {img} name '{nm}'"));
                if out.len() > 40 {
                    break;
                }
            }
            (out, Action::None)
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
        // ---- immutable IL2CPP metadata and runtime catalog ---------------
        "meta" | "typelist" | "typedump" | "meths" | "fields2" | "dumpmeta" => {
            (crate::runtime::console(cmd.as_str(), a1, a2, rest).unwrap_or_default(), Action::None)
        }
        // ---- plan 5 self-tests: prove guard()/VEH actually hold -----------
        // `panic` must produce a `[dll] PANIC at file:line:` line and *not*
        // kill the game; the session continues with the next command.
        "panic" => {
            let msg = line
                .split_once(char::is_whitespace)
                .map_or("console panic test", |x| x.1)
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
            // SAFETY: this command deliberately performs an unaligned probe of
            // an arbitrary address under VEH, as documented by diagnostic_raw_read.
            let raw = match unsafe { crate::safe::diagnostic_raw_read(addr) } {
                Ok(v) => format!("raw deref @{addr:#x} = {v:#016x} (address was mapped)"),
                Err(e) => format!("raw deref @{addr:#x}: {e} (VEH caught it)"),
            };
            (vec![head, raw], Action::None)
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

#[cfg(test)]
mod project_tests {
    //! The projection is the one part of `esp` that can be checked without the
    //! game: `projectionMatrix * worldToCameraMatrix` follows Unity's column-major
    //! `Matrix4x4` storage, and a point straight ahead must land dead centre.

    use super::{mul4, mul_vec4, project};

    fn perspective(fov_y_deg: f32, aspect: f32) -> [f32; 16] {
        let f = 1.0 / (fov_y_deg.to_radians() / 2.0).tan();
        let mut m = [0f32; 16];
        m[0] = f / aspect;
        m[5] = f;
        m[10] = -1.0002;
        m[11] = -1.0;
        m[14] = -0.20002;
        m
    }

    fn identity() -> [f32; 16] {
        let mut m = [0f32; 16];
        for i in 0..4 {
            m[i * 5] = 1.0;
        }
        m
    }

    #[test]
    fn centre_of_view_projects_to_centre_of_screen() {
        let pv = mul4(&perspective(60.0, 16.0 / 9.0), &identity());
        let (x, y) = project(&pv, 1920.0, 1080.0, (0.0, 0.0, -5.0)).unwrap();
        assert!((x - 960.0).abs() < 0.5, "x = {x}");
        assert!((y - 540.0).abs() < 0.5, "y = {y}");
    }

    #[test]
    fn behind_the_camera_is_none() {
        let pv = mul4(&perspective(60.0, 16.0 / 9.0), &identity());
        assert!(project(&pv, 1920.0, 1080.0, (0.0, 0.0, 5.0)).is_none());
    }

    #[test]
    fn right_of_centre_lands_right_of_centre() {
        let pv = mul4(&perspective(60.0, 16.0 / 9.0), &identity());
        let (x, _) = project(&pv, 1920.0, 1080.0, (1.0, 0.0, -5.0)).unwrap();
        assert!(x > 960.0, "x = {x}");
    }

    #[test]
    fn column_major_multiply_matches_hand_math() {
        let mut a = identity();
        a[12] = 3.0; // translation on x, stored in the last column
        let v = mul_vec4(&a, [1.0, 0.0, 0.0, 1.0]);
        assert_eq!(v[0], 4.0);
        assert_eq!(v[1], 0.0);
    }
}
