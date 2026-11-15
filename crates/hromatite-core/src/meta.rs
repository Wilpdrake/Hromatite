//! Memory-only walk of il2cpp runtime metadata.
//!
//! The obfuscated exports make every `il2cpp_*` API call a guessing game, but
//! the runtime keeps everything reachable from pointers we already have:
//!
//! ```text
//! assembly record +0x28 → Il2CppCodeGenModule
//!     ... typeInfoTable  → Il2CppClass*[]        (one entry per type)
//! Il2CppClass +0x10      → const char* name
//!            +0x18      → const char* namespaze
//!            +0x80      → FieldInfo[]           (name@+0x00, offset@+0x18)
//!            +0x98      → MethodInfo*[]         (ptr@+0x00, name@+0x10)
//! ```
//!
//! Field/method array offsets are *discovered*, not assumed: a candidate is
//! accepted only when every record passes an invariant (a field's `parent`
//! equals the class; a method's `methodPointer` points into GameAssembly and
//! its name is printable). That is why this module survives layout drift
//! between Unity versions — and why it needs no guarded calls at all: it only
//! reads memory that a previous validation already proved readable.

use crate::log;
use crate::procs;
use crate::safe;

/// `const char* name` inside `Il2CppClass`.
pub const CLASS_NAME_OFF: usize = 0x10;
/// `const char* namespaze`
pub const CLASS_NS_OFF: usize = 0x18;
/// `const Il2CppImage* image` — the invariant used to recognise a class at all.
pub const CLASS_IMAGE_OFF: usize = 0x00;

/// Where a `FieldInfo` keeps its data (il2cpp has kept this shape for years).
const FIELD_NAME_OFF: usize = 0x00;
const FIELD_PARENT_OFF: usize = 0x10;
const FIELD_OFFSET_OFF: usize = 0x18;
/// Where a runtime `MethodInfo` keeps its compiled entry point and its name.
/// Unity 6 inserted `virtualMethodPointer` after `methodPointer`, so the name
/// moved from `+0x10` to `+0x18` (`il2cpp-class-internals.h`, 6000.0.x):
/// `methodPointer@0x00, virtualMethodPointer@0x08, invoker_method@0x10,
/// name@0x18, klass@0x20, return_type@0x28, parameters@0x30`.
const METHOD_PTR_OFF: usize = 0x00;
const METHOD_NAME_OFF: usize = 0x18;

#[derive(Clone, Debug)]
pub struct Class {
    pub addr: usize,
    pub name: String,
    pub ns: String,
}

impl Class {
    pub fn full(&self) -> String {
        if self.ns.is_empty() {
            self.name.clone()
        } else {
            format!("{}.{}", self.ns, self.name)
        }
    }
}

fn ident_like(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 80
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '<' | '>' | '`' | '+' | '=' | ',' | '@'))
}

/// Structural test for an `Il2CppClass`: `name` at `+0x10` is an identifier and
/// `namespaze` at `+0x18` is either NULL or an identifier. Deliberately does not
/// compare the image pointer — the caller usually cannot know it (the pointer we
/// reach from an assembly record is the *codegen module*, not the image).
pub fn class_like(p: usize) -> bool {
    if p < 0x10000 || !safe::readable_at(p) {
        return false;
    }
    let name_ptr = match safe::read_usize(p + CLASS_NAME_OFF) {
        Some(v) => v,
        None => return false,
    };
    match safe::ascii_at(name_ptr, 96) {
        Some(s) if ident_like(&s) => {}
        _ => return false,
    }
    match safe::read_usize(p + CLASS_NS_OFF) {
        Some(0) => true,
        Some(ns) => matches!(safe::ascii_at(ns, 96), Some(s) if ident_like(&s)),
        None => false,
    }
}

/// A class pointer is trusted when its first word is the image we asked for and
/// its name is a real identifier: heap garbage passes neither test.
pub fn is_class_of(p: usize, image: usize) -> bool {
    if !class_like(p) {
        return false;
    }
    matches!(safe::read_usize(p + CLASS_IMAGE_OFF), Some(img) if img == image)
}

pub fn class_name(p: usize) -> Option<String> {
    let ptr = safe::read_usize(p + CLASS_NAME_OFF)?;
    safe::ascii_at(ptr, 256)
}

pub fn class_namespace(p: usize) -> Option<String> {
    let ptr = safe::read_usize(p + CLASS_NS_OFF)?;
    if ptr < 0x10000 {
        return Some(String::new());
    }
    safe::ascii_at(ptr, 256).or(Some(String::new()))
}

/// Walk `image`'s `typeInfoTable` and return the classes it holds.
///
/// The table is found by voting: every 8-byte slot of the codegen module is a
/// candidate, and the winner is the pointer whose first entries are all
/// `Il2CppClass*` of *this* image. Reported so the caller can log the layout.
pub fn classes_of_image(image: usize, max: usize) -> (usize, usize, Vec<Class>) {
    let module = image;
    let mut best: Option<(usize, usize, usize)> = None; // (class_ofs, table, votes)
    let mut ofs = 0x00usize;
    while ofs <= 0x1000 {
        let slot = match safe::read_usize(module + ofs) {
            Some(v) => v,
            None => break,
        };
        if slot >= 0x10000 && safe::readable_at(slot) {
            let mut votes = 0usize;
            for i in 0..12 {
                let p = match safe::read_usize(slot + i * 8) {
                    Some(p) => p,
                    None => break,
                };
                // prefer exact image match, accept structural match
                if is_class_of(p, image) || class_like(p) {
                    votes += 1;
                } else {
                    break;
                }
            }
            if votes >= 6 && best.map(|b| votes > b.2).unwrap_or(true) {
                best = Some((ofs, slot, votes));
            }
        }
        ofs += 8;
    }
    let (class_ofs, table, _) = match best {
        Some(b) => b,
        None => return (0, 0, Vec::new()),
    };
    let mut out = Vec::new();
    let mut i = 0usize;
    while out.len() < max {
        let p = match safe::read_usize(table + i * 8) {
            Some(p) => p,
            None => break,
        };
        if !class_like(p) {
            break;
        }
        let name = match class_name(p) {
            Some(n) => n,
            None => break,
        };
        let ns = class_namespace(p).unwrap_or_default();
        out.push(Class { addr: p, name, ns });
        i += 1;
    }
    (class_ofs, table, out)
}

/// Diagnostic dump of every qword slot in `module` that holds a readable
/// pointer, annotated with how many of its entries look like classes. Used when
/// the layout vote finds nothing: it shows *which* offset failed and what it
/// points at.
pub fn probe_module(module: usize, max_ofs: usize) -> Vec<String> {
    let ga = procs::own_module(crate::GAME_ASSEMBLY);
    let mut out = Vec::new();
    let mut ofs = 0usize;
    while ofs <= max_ofs {
        let v = match safe::read_usize(module + ofs) {
            Some(v) => v,
            None => break,
        };
        if v >= 0x10000 && safe::readable_at(v) {
            let in_ga = ga.map(|(b, s)| v >= b && v < b + s).unwrap_or(false);
            let mut votes = 0usize;
            for i in 0..8 {
                let p = match safe::read_usize(v + i * 8) {
                    Some(p) => p,
                    None => break,
                };
                if class_like(p) {
                    votes += 1;
                } else {
                    break;
                }
            }
            let first = safe::read_usize(v).unwrap_or(0);
            let tag = safe::ascii_at(first, 40)
                .map(|s| format!(" '{s}'"))
                .unwrap_or_default();
            if votes > 0 || in_ga {
                out.push(format!(
                    "  +{ofs:#04x} = {v:#x}{} votes={votes}{tag}",
                    if in_ga { " GA" } else { "" }
                ));
            }
        }
        ofs += 8;
    }
    out
}

/// `classes_of_image` behind a cache: a console session re-reads the same image
/// dozens of times, and each walk costs tens of thousands of VirtualQuery calls.
pub fn classes_cached(image: usize) -> Vec<Class> {
    use std::sync::Mutex;
    static CACHE: Mutex<Vec<(usize, Vec<Class>)>> = Mutex::new(Vec::new());
    let mut c = match CACHE.lock() {
        Ok(c) => c,
        Err(p) => p.into_inner(),
    };
    if let Some((_, list)) = c.iter().find(|(i, _)| *i == image) {
        return list.clone();
    }
    let (_, _, list) = classes_of_image(image, 40000);
    c.push((image, list.clone()));
    list
}

/// Find one class by exact namespace + name (empty namespace allowed).
pub fn find_class(image: usize, ns: &str, name: &str) -> Option<Class> {
    classes_cached(image)
        .into_iter()
        .find(|c| c.name == name && (ns.is_empty() || c.ns == ns))
}

/// Find a class by name in any assembly; `want_ns` may be empty. Returns the
/// class plus the assembly name it came from.
pub fn find_class_anywhere(want_ns: &str, name: &str) -> Vec<(String, Class)> {
    let d = match crate::api::domain() {
        Some(d) => d,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for (asm, entry) in crate::il2cpp::assemblies(d) {
        let image = match safe::read_usize(entry + 0x28) {
            Some(i) if i > 0x10000 => i,
            _ => continue,
        };
        if let Some(c) = find_class(image, want_ns, name) {
            out.push((asm, c));
            if out.len() >= 8 {
                break;
            }
        }
    }
    if out.is_empty() {
        // Unity 6: the per-image walk has no table to read, so fall back to the
        // global class arrays found by scanning GameAssembly's `.data`.
        if let Some(c) = find_class_global(want_ns, name) {
            out.push(("(global table)".to_string(), c));
        }
    }
    out
}

/// Locate the `FieldInfo*` field of a class: first pointer in the class body
/// whose records all name a field **and** point back at this class.
pub fn fields_ptr(class: usize) -> Option<(usize, usize)> {
    let mut ofs = 0x60usize;
    while ofs <= 0xb0 {
        let p = match safe::read_usize(class + ofs) {
            Some(v) => v,
            None => break,
        };
        if p >= 0x10000 && safe::readable_at(p) && reads_as_fields(p, class) {
            return Some((ofs, p));
        }
        ofs += 8;
    }
    None
}

fn reads_as_fields(p: usize, class: usize) -> bool {
    let mut ok = 0usize;
    for i in 0..3 {
        let e = p + i * 0x20;
        let name = match safe::read_usize(e + FIELD_NAME_OFF) {
            Some(v) => v,
            None => return false,
        };
        let parent = match safe::read_usize(e + FIELD_PARENT_OFF) {
            Some(v) => v,
            None => return false,
        };
        if parent != class {
            return false;
        }
        if !matches!(safe::ascii_at(name, 64), Some(s) if !s.is_empty()) {
            return false;
        }
        ok += 1;
    }
    ok == 3
}

/// `(name, offset)` for every field of the class that can be read safely.
pub fn fields_of(class: usize) -> Vec<(String, i32)> {
    let mut out = Vec::new();
    let p = match fields_ptr(class) {
        Some((_, p)) => p,
        None => return out,
    };
    for i in 0..512 {
        let e = p + i * 0x20;
        let name = match safe::read_usize(e + FIELD_NAME_OFF) {
            Some(v) => v,
            None => break,
        };
        let parent = match safe::read_usize(e + FIELD_PARENT_OFF) {
            Some(v) => v,
            None => break,
        };
        if parent != class {
            break;
        }
        let s = match safe::ascii_at(name, 128) {
            Some(s) if !s.is_empty() => s,
            _ => break,
        };
        let off = safe::read_u32(e + FIELD_OFFSET_OFF).unwrap_or(0) as i32;
        out.push((s, off));
    }
    out
}

/// Locate the `MethodInfo**` field of a class. Invariant: `methodPointer` sits
/// inside GameAssembly and the name at `+0x10` is a printable string.
pub fn methods_ptr(class: usize) -> Option<(usize, usize)> {
    let ga = procs::own_module(crate::GAME_ASSEMBLY);
    let mut ofs = 0x60usize;
    while ofs <= 0xc0 {
        let p = match safe::read_usize(class + ofs) {
            Some(v) => v,
            None => break,
        };
        if p >= 0x10000 && safe::readable_at(p) && reads_as_methods(p, ga) {
            return Some((ofs, p));
        }
        ofs += 8;
    }
    None
}

fn reads_as_methods(p: usize, ga: Option<(usize, usize)>) -> bool {
    for i in 0..3 {
        let mi = match safe::read_usize(p + i * 8) {
            Some(v) => v,
            None => return false,
        };
        if mi < 0x10000 || !safe::readable_at(mi) {
            return false;
        }
        let fp = match safe::read_usize(mi + METHOD_PTR_OFF) {
            Some(v) => v,
            None => return false,
        };
        if let Some((base, size)) = ga {
            if fp < base || fp >= base + size {
                return false;
            }
        }
        let name = match safe::read_usize(mi + METHOD_NAME_OFF) {
            Some(v) => v,
            None => return false,
        };
        if !matches!(safe::ascii_at(name, 96), Some(s) if !s.is_empty()) {
            return false;
        }
    }
    true
}

/// Every method of the class as `(name, compiled entry point, MethodInfo*)`.
pub fn methods_of(class: usize) -> Vec<(String, usize, usize)> {
    let mut out = Vec::new();
    let p = match methods_ptr(class) {
        Some((_, p)) => p,
        None => return out,
    };
    for i in 0..4096 {
        let mi = match safe::read_usize(p + i * 8) {
            Some(v) => v,
            None => break,
        };
        if mi < 0x10000 || !safe::readable_at(mi) {
            break;
        }
        let fp = match safe::read_usize(mi + METHOD_PTR_OFF) {
            Some(v) => v,
            None => break,
        };
        let name_ptr = match safe::read_usize(mi + METHOD_NAME_OFF) {
            Some(v) => v,
            None => break,
        };
        let name = match safe::ascii_at(name_ptr, 160) {
            Some(s) if !s.is_empty() => s,
            _ => break,
        };
        if fp < 0x10000 {
            break;
        }
        out.push((name, fp, mi));
    }
    out
}

/// Entry point of the method with this exact name.
pub fn method_entry(class: usize, name: &str) -> Option<usize> {
    methods_of(class)
        .into_iter()
        .find(|(n, _, _)| n == name)
        .map(|(_, fp, _)| fp)
}

/// Class field by name → instance byte offset.
pub fn field_offset(class: usize, name: &str) -> Option<i32> {
    fields_of(class)
        .into_iter()
        .find(|(n, _)| n == name)
        .map(|(_, off)| off)
}

// ---- global class tables ---------------------------------------------------
//
// Unity 6 has *no* `typeInfoTable` inside `Il2CppCodeGenModule` (see
// `il2cpp-class-internals.h`, 6000.0.x: the module ends with
// `metadataRegistration`/`codeRegistaration`). `Il2CppClass` objects are kept
// by the runtime in a global array indexed by type definition, so the only
// layout-agnostic way in is to *find* that array: a dense stretch of aligned
// pointers inside GameAssembly's own `.data`, most of which lead to records
// whose `+0x10` is an identifier. Holes are expected (unloaded types are NULL),
// so the test is a *density* window, not a strict run.

fn readable_protect(p: u32) -> bool {
    matches!(p & 0xff, 2 | 4 | 8 | 0x20 | 0x40 | 0x80)
}

/// Cheap readable-range index: the scan touches millions of words, and a
/// `VirtualQuery` per word would take minutes. Regions come from one snapshot.
struct Ranges(Vec<(usize, usize)>);

impl Ranges {
    fn new() -> Self {
        let mut v: Vec<(usize, usize)> = procs::regions(1 << 20)
            .into_iter()
            // PAGE_GUARD pages raise 0xC00000FD when touched — the scan runs on
            // the game thread, so a "readable" stack guard page must be skipped.
            .filter(|r| readable_protect(r.protect) && r.protect & 0x100 == 0)
            .map(|r| (r.base, r.base + r.size))
            .collect();
        v.sort_unstable();
        Self(v)
    }

    fn readable(&self, a: usize, len: usize) -> bool {
        let i = match self.0.binary_search_by(|(base, _)| base.cmp(&a)) {
            Ok(i) => i,
            Err(0) => return false,
            Err(i) => i - 1,
        };
        a.saturating_add(len) <= self.0[i].1
    }

    fn u64(&self, a: usize) -> Option<u64> {
        self.readable(a, 8).then(|| unsafe { *(a as *const u64) })
    }

    fn cstr(&self, a: usize, max: usize) -> Option<String> {
        if a < 0x10000 || !self.readable(a, 1) {
            return None;
        }
        let mut s = String::with_capacity(32);
        for i in 0..max {
            if !self.readable(a + i, 1) {
                return None;
            }
            let b = unsafe { *((a + i) as *const u8) };
            if b == 0 {
                return (!s.is_empty()).then_some(s);
            }
            if !(33..127).contains(&b) {
                return None;
            }
            s.push(b as char);
        }
        None
    }
}

static STAT_RD: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static STAT_NAME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn class_like_in(r: &Ranges, p: usize) -> bool {
    if p < 0x10000 || !r.readable(p, 0x20) {
        return false;
    }
    STAT_RD.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let name_ptr = match r.u64(p + CLASS_NAME_OFF) {
        Some(v) => v as usize,
        None => return false,
    };
    match r.cstr(name_ptr, 96) {
        Some(s) if ident_like(&s) => {
            STAT_NAME.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        _ => return false,
    }
    // `namespaze` may legitimately be NULL (global namespace) — and, in this
    // build, sometimes points at a metadata handle rather than a C string, so a
    // readable pointer is accepted here; callers filter by exact name anyway.
    let ns = r.u64(p + CLASS_NS_OFF).unwrap_or(0) as usize;
    ns == 0 || r.readable(ns, 1)
}

/// Sparse-table scan: `s_TypeInfoTable` is `IL2CPP_CALLOC(typesCount * 8)`, so
/// it is a long stretch of **zeros** with class pointers scattered where types
/// were lazily initialized. A density window misses it; "zero or class-like" run
/// detection is what matches (`GlobalMetadata.cpp:341`).
fn scan_region(start: usize, words: usize, ranges: &Ranges) -> (Vec<(usize, usize)>, usize) {
    let mut out: Vec<(usize, usize)> = Vec::new();
    let mut run_start = 0usize;
    let mut run_len = 0usize;
    let mut run_hits = 0usize;
    let mut best_hits = 0usize;
    for i in 0..words {
        let a = start + i * 8;
        let v = match ranges.u64(a) {
            Some(v) => v as usize,
            None => {
                v_break(&mut out, run_start, run_len, run_hits, &mut best_hits);
                run_len = 0;
                run_hits = 0;
                continue;
            }
        };
        if v == 0 {
            if run_len == 0 {
                run_start = a;
            }
            run_len += 1;
            continue;
        }
        if v >= 0x10000 && class_like_in(ranges, v) {
            if run_len == 0 {
                run_start = a;
            }
            run_len += 1;
            run_hits += 1;
            continue;
        }
        v_break(&mut out, run_start, run_len, run_hits, &mut best_hits);
        run_len = 0;
        run_hits = 0;
    }
    v_break(&mut out, run_start, run_len, run_hits, &mut best_hits);
    (out, best_hits)
}

#[inline]
fn v_break(out: &mut Vec<(usize, usize)>, start: usize, len: usize, hits: usize, best: &mut usize) {
    // `best` records the strongest run even when it is below the acceptance
    // threshold — without that, "no tables" cannot be told apart from "the
    // detector never fired".
    if len >= 256 && hits > *best {
        *best = hits;
    }
    if len >= 256 && hits >= 16 {
        out.push((start, len));
    }
}

pub fn class_tables() -> Vec<(usize, usize)> {
    use std::sync::Mutex;
    static CACHE: Mutex<Vec<(usize, usize)>> = Mutex::new(Vec::new());
    let mut c = match CACHE.lock() {
        Ok(c) => c,
        Err(p) => p.into_inner(),
    };
    if !c.is_empty() {
        return c.clone();
    }
    let ranges = Ranges::new();
    let mut scanned_words = 0usize;
    let mut scanned_regions = 0usize;
    // Unity 6 allocates the class table with `IL2CPP_CALLOC`, so it lives on the
    // **heap**, not in GameAssembly's `.data` (`GlobalMetadata.cpp: s_TypeInfoTable`).
    for r in procs::regions(65536) {
        // PAGE_GUARD (0x100) has to be excluded: touching such a page raises
        // 0xC00000FD on the game thread, which our VEH reports as a fault.
        let writable = matches!(r.protect & 0xff, 4 | 8 | 0x40 | 0x80) && r.protect & 0x100 == 0;
        if !writable || r.size < 0x10000 {
            continue;
        }
        let start = (r.base + 7) & !7usize;
        let words = (r.base + r.size).saturating_sub(start) / 8;
        if words < 1024 {
            continue;
        }
        scanned_words += words;
        scanned_regions += 1;
        if scanned_words > 80_000_000 {
            break;
        }
        match crate::veh::call_with(|| scan_region(start, words, &ranges)) {
            Ok((found, best)) => {
                if best >= 1 {
                    log::log_tag(
                        "meta",
                        &format!(
                            "  {start:#x}+{words}w: best {best} class words, {} run(s)",
                            found.len()
                        ),
                    );
                }
                c.extend(found);
            }
            Err(_) => log::log_tag("meta", &format!("  {start:#x}: fault")),
        }
    }
    c.sort_by_key(|(a, _)| *a);
    c.dedup();
    log::log_tag(
        "meta",
        &format!(
            "class_like_in: {} readable-class-pointer test(s), {} printable name(s)",
            STAT_RD.load(std::sync::atomic::Ordering::Relaxed),
            STAT_NAME.load(std::sync::atomic::Ordering::Relaxed)
        ),
    );
    log::log_tag(
        "meta",
        &format!(
            "class tables ({} region(s), {} words): {:?}",
            scanned_regions,
            scanned_words,
            c.iter()
                .take(24)
                .map(|(a, n)| format!("{a:#x}+{n}w"))
                .collect::<Vec<_>>()
        ),
    );
    c.clone()
}

/// Every class in every detected table.
pub fn global_classes() -> Vec<Class> {
    use std::sync::Mutex;
    static CACHE: Mutex<Vec<Class>> = Mutex::new(Vec::new());
    let mut c = match CACHE.lock() {
        Ok(c) => c,
        Err(p) => p.into_inner(),
    };
    if !c.is_empty() {
        return c.clone();
    }
    for (start, count) in class_tables() {
        for i in 0..count.min(300_000) {
            let p = match safe::read_usize(start + i * 8) {
                Some(p) => p,
                None => break,
            };
            if !class_like(p) {
                continue;
            }
            if let (Some(name), Some(ns)) = (class_name(p), class_namespace(p)) {
                c.push(Class { addr: p, name, ns });
            }
        }
    }
    log::log_tag("meta", &format!("global class cache: {} classes", c.len()));
    c.clone()
}

// ---- class by name string --------------------------------------------------
//
// When no dense table turns up, the *string* is the way in: an `Il2CppClass`
// holds `name` at `+0x10`, so the 8-byte word equal to the string's address sits
// at `class+0x10` and the record starts 0x10 bytes earlier. That is exactly the
// `find <string-addr>` trick, just packaged and verified (the candidate must
// pass `class_like` *and* re-read as the requested namespace/name).

fn scan_budget_bytes() -> usize {
    std::env::var("HROMATITE_SCAN_MB")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(512)
        * 1024
        * 1024
}

/// All readable, non-guard regions, modules first (their `.data` is warm).
fn scan_regions() -> Vec<(usize, usize, u32)> {
    let mut v: Vec<(usize, usize, u32)> = procs::regions(65536)
        .into_iter()
        .filter(|r| readable_protect(r.protect) && r.protect & 0x100 == 0 && r.size >= 0x1000)
        .map(|r| (r.base, r.size, 1u32))
        .collect();
    // mark module regions first (lower key = scanned earlier)
    let mods: Vec<(usize, usize)> = [crate::GAME_ASSEMBLY, crate::UNITY_PLAYER]
        .iter()
        .filter_map(|m| procs::own_module(m))
        .collect();
    for e in v.iter_mut() {
        if mods.iter().any(|(b, s)| e.0 < b + s && e.0 + e.1 > *b) {
            e.2 = 0;
        }
    }
    v.sort_by_key(|(base, _, prio)| (*prio, *base));
    v
}

/// Addresses of `pat` (NUL-terminated in memory) in readable memory.
pub fn find_string(pat: &[u8], max: usize) -> Vec<usize> {
    let mut out = Vec::new();
    if pat.is_empty() {
        return out;
    }
    let budget = scan_budget_bytes();
    let mut spent = 0usize;
    for (base, size, _) in scan_regions() {
        spent += size;
        if spent > budget || out.len() >= max {
            break;
        }
        if size < pat.len() + 1 {
            continue;
        }
        let last = base + size - pat.len() - 1;
        let mut a = base;
        while a <= last && out.len() < max {
            if unsafe { *(a as *const u8) } == pat[0] {
                let hit = unsafe { core::slice::from_raw_parts(a as *const u8, pat.len()) } == pat
                    && unsafe { *((a + pat.len()) as *const u8) } == 0;
                if hit {
                    out.push(a);
                }
            }
            a += 1;
        }
    }
    out
}

/// Addresses of the 8-byte word `want`.
pub fn find_qword(want: u64, max: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let budget = scan_budget_bytes();
    let mut spent = 0usize;
    for (base, size, _) in scan_regions() {
        spent += size;
        if spent > budget || out.len() >= max {
            break;
        }
        if size < 16 {
            continue;
        }
        let mut a = (base + 7) & !7usize;
        let end = base + size - 8;
        while a <= end && out.len() < max {
            if unsafe { *(a as *const u64) } == want {
                out.push(a);
            }
            a += 8;
        }
    }
    out
}

/// Directory scan for a class record: `+0x10` must be the exact name string.
///
/// Measured on this build: a full heap pass performs ~19 M `class_like_in` tests
/// and finds ~300 k class-shaped records, but they are **not** laid out as one
/// contiguous table (runs never form), so a table-shaped detector cannot see
/// them. A linear filter can, and it costs the same single pass.
pub fn find_class_by_scan(ns: &str, name: &str) -> Vec<(usize, String)> {
    let ranges = Ranges::new();
    let mut out = Vec::new();
    let mut words_total = 0usize;
    for r in procs::regions(65536) {
        let writable = matches!(r.protect & 0xff, 4 | 8 | 0x40 | 0x80) && r.protect & 0x100 == 0;
        if !writable || r.size < 0x1000 {
            continue;
        }
        let start = (r.base + 7) & !7usize;
        let words = (r.base + r.size).saturating_sub(start) / 8;
        if words < 16 {
            continue;
        }
        words_total += words;
        if words_total > 120_000_000 {
            break;
        }
        let found = crate::veh::call_with(|| {
            let mut hit: Vec<(usize, String)> = Vec::new();
            for i in 0..words {
                let v = match ranges.u64(start + i * 8) {
                    Some(v) => v as usize,
                    None => continue,
                };
                if v < 0x10000 || !class_like_in(&ranges, v) {
                    continue;
                }
                let np = match ranges.u64(v + CLASS_NAME_OFF) {
                    Some(p) => p as usize,
                    None => continue,
                };
                if let Some(s) = ranges.cstr(np, 96) {
                    if s == name {
                        let nsv = ranges
                            .u64(v + CLASS_NS_OFF)
                            .map(|q| ranges.cstr(q as usize, 96))
                            .flatten()
                            .unwrap_or_default();
                        hit.push((v, nsv));
                    }
                }
            }
            hit
        });
        if let Ok(mut h) = found {
            out.append(&mut h);
        }
        if out.len() >= 64 {
            break;
        }
    }
    log::log_tag(
        "meta",
        &format!("class scan for '{name}': {} candidate(s)", out.len()),
    );
    let want_ns = ns.to_string();
    if !want_ns.is_empty() {
        out.retain(|(_, cns)| cns.is_empty() || *cns == want_ns);
    }
    out
}

/// `Il2CppClass*` for `ns.name`, found through the class-name string.
pub fn class_by_name_string(ns: &str, name: &str) -> Option<Class> {
    let ptrs = find_string(name.as_bytes(), 16);
    log::log_tag(
        "meta",
        &format!("'{}' string: {} occurrence(s)", name, ptrs.len()),
    );
    for sp in ptrs {
        let holders = find_qword(sp as u64, 256);
        log::log_tag(
            "meta",
            &format!(
                "  str {sp:#x}: {} holder(s){}",
                holders.len(),
                holders
                    .iter()
                    .take(3)
                    .map(|h| format!(" {h:#x}"))
                    .collect::<Vec<_>>()
                    .join("")
            ),
        );
        for slot in holders {
            // `name` lives at class+0x10
            let c = slot.wrapping_sub(CLASS_NAME_OFF);
            if !class_like(c) {
                log::log_tag(
                    "meta",
                    &format!(
                        "    slot {slot:#x} -> {c:#x}: not class-like (name@+0x10={:?})",
                        class_name(c)
                    ),
                );
                continue;
            }
            let n = class_name(c)?;
            let cns = class_namespace(c).unwrap_or_default();
            if n == name && (ns.is_empty() || cns == ns) {
                log::log_tag(
                    "meta",
                    &format!("class {cns}.{n} = {c:#x} (via string {sp:#x}, slot {slot:#x})"),
                );
                return Some(Class {
                    addr: c,
                    name: n,
                    ns: cns,
                });
            }
        }
    }
    None
}

/// Structural class lookup that ignores images (used when the per-image walk is
/// unavailable — Unity 6 keeps no class table in the codegen module).
pub fn find_class_global(ns: &str, name: &str) -> Option<Class> {
    global_classes()
        .into_iter()
        .find(|c| c.name == name && (ns.is_empty() || c.ns == ns))
}

/// One-line summary of the discovered layout, for the console.
pub fn layout_of(class: usize) -> String {
    let f = fields_ptr(class).map(|(o, p)| format!("fields@+{o:#x} ({p:#x}, {} entries)", fields_of(class).len()));
    let m = methods_ptr(class)
        .map(|(o, p)| format!("methods@+{o:#x} ({p:#x}, {} entries)", methods_of(class).len()));
    format!(
        "{}.{}, {}",
        class_namespace(class).unwrap_or_default(),
        class_name(class).unwrap_or_default(),
        [f, m]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ")
    )
}
