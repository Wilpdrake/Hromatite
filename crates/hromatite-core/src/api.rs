//! Behavioural identification of the il2cpp C API among the 251 obfuscated
//! GameAssembly exports, and safe wrappers built on top of `veh`.
//!
//! The names in `symbols.rs` are Beebyte per-build garbage, and the export
//! order does not follow `il2cpp-api-functions.h`, so the only oracle available
//! is *behaviour*: call a candidate with arguments whose result we can predict
//! and check the answer.
//!
//! Two functions are found this way:
//!   * `il2cpp_domain_get_assemblies(domain, &count)` — the count it writes must
//!     equal the number of assemblies we can already walk by hand (`il2cpp`);
//!   * `il2cpp_class_from_name(image, ns, name)` — for
//!     `("UnityEngine", "Camera")` the returned `Il2CppClass*` must hold string
//!     pointers to exactly those two names in its first 0x80 bytes; the offsets
//!     found are the class layout we then reuse.
//!
//! Every call goes through `veh`, so a wrong candidate (or a wrong argument
//! shape) costs an `Err(Fault)`, not the game. A candidate that faulted is
//! never called again in the same phase.

use std::sync::Mutex;

use crate::ffi;
use crate::il2cpp;
use crate::log;
use crate::procs;
use crate::safe;
use crate::symbols;
use crate::veh;

/// Value the callee is supposed to overwrite; if it survives, the export is not
/// the one we are looking for.
const SENTINEL: usize = 0xDEAD_BEEF;
/// Per-phase budget: a wrong function can be expensive, so cap the search.
const MAX_TRIED: usize = 120;
/// How many fault lines a phase may write to the log.
const MAX_FAULT_LOGS: usize = 5;

#[derive(Clone, Copy, Debug)]
pub struct Func {
    pub name: &'static str,
    pub rva: u32,
    pub addr: usize,
}

#[derive(Clone, Copy, Debug)]
pub struct ClassLayout {
    /// offset of the `const char* name` field inside `Il2CppClass`
    pub name_off: i64,
    /// offset of the `const char* namespaze` field
    pub ns_off: i64,
}

#[derive(Clone, Debug, Default)]
pub struct Report {
    pub domain: usize,
    pub assemblies_fn: Option<Func>,
    pub class_fn: Option<Func>,
    pub field_fn: Option<Func>,
    pub layout: Option<ClassLayout>,
    pub image: usize,
    pub tried: usize,
    pub faulted: usize,
    /// human-readable summary lines for the console (`api` command)
    pub notes: Vec<String>,
}

impl Report {
    fn note(&mut self, s: impl Into<String>) {
        let s = s.into();
        log::log_tag("api", &s);
        self.notes.push(s);
    }
}

static REPORT: Mutex<Option<Report>> = Mutex::new(None);

/// Identify the API once and cache it. A cached report missing either function
/// is retried (the previous attempt may have run before the game was ready).
pub fn identify() -> Report {
    let mut slot = REPORT.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(r) = slot.as_ref() {
        if r.assemblies_fn.is_some() && r.class_fn.is_some() {
            return r.clone();
        }
    }
    let r = run_identify();
    *slot = Some(r.clone());
    r
}

/// Forget the cache (console: retry after the game finished loading).
pub fn reset() {
    *REPORT.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn ga_addr(rva: u32) -> Option<usize> {
    let (base, size) = procs::own_module(crate::GAME_ASSEMBLY)?;
    let a = base.checked_add(rva as usize)?;
    (a < base + size).then_some(a)
}

/// Is this export address callable-looking? Getter-shaped exports take no
/// arguments and return a singleton; they are useless as 2/3-arg candidates.
fn callable(addr: usize) -> bool {
    if addr < 0x10000 || !safe::readable_at(addr) {
        return false;
    }
    !il2cpp::is_getter(addr)
}

fn run_identify() -> Report {
    let mut rep = Report::default();
    let (ga_base, ga_size) = match procs::own_module(crate::GAME_ASSEMBLY) {
        Some(v) => v,
        None => {
            rep.note("GameAssembly.dll not loaded");
            return rep;
        }
    };
    let domain = match il2cpp::find_domain() {
        Some(d) => d,
        None => {
            rep.note("domain not found — run `domain` / `asm` first");
            return rep;
        }
    };
    rep.domain = domain;
    rep.image = image_of_in(domain, "UnityEngine.CoreModule")
        .or_else(|| image_of_in(domain, "mscorlib"))
        .unwrap_or(0);
    log::log_tag(
        "api",
        &format!(
            "identify: domain {domain:#x}, GameAssembly {ga_base:#x}+{ga_size:#x}, image {:#x}",
            rep.image
        ),
    );

    let want = il2cpp::assemblies(domain).len();
    let mut tried_addrs: Vec<usize> = Vec::new();

    // ---- phase 1: il2cpp_domain_get_assemblies(domain, &count) ------------
    if want >= 2 {
        let mut faults = 0usize;
        let mut logged = 0usize;
        'outer: for (name, rva) in symbols::EXPORTS.iter() {
            if tried_addrs.len() >= MAX_TRIED {
                break;
            }
            let addr = match ga_addr(*rva) {
                Some(a) => a,
                None => continue,
            };
            if tried_addrs.contains(&addr) || !callable(addr) {
                continue;
            }
            tried_addrs.push(addr);
            rep.tried += 1;

            let mut count = SENTINEL;
            let p1 = veh::call2(addr, domain, &mut count as *mut usize as usize);
            match p1 {
                Ok(ptr) => {
                    if count != want {
                        continue;
                    }
                    // Same function, same answer twice: a count match is already
                    // strong, a repeat guards against a stale stack slot.
                    let mut count2 = SENTINEL;
                    match veh::call2(addr, domain, &mut count2 as *mut usize as usize) {
                        Ok(ptr2) if count2 == want => {
                            rep.assemblies_fn = Some(Func { name, rva: *rva, addr });
                            rep.note(format!(
                                "found: domain_get_assemblies = {name} rva={rva:#x} addr={addr:#x} \
                                 count={want} ptr={ptr:#x} (repeat ptr={ptr2:#x})"
                            ));
                            break 'outer;
                        }
                        Ok(_) => continue,
                        Err(f) => {
                            faults += 1;
                            if logged < MAX_FAULT_LOGS {
                                logged += 1;
                                rep.note(format!(
                                    "domain_get_assemblies: fault on repeat {name} rva={rva:#x}: {f}"
                                ));
                            }
                            continue;
                        }
                    }
                }
                Err(f) => {
                    faults += 1;
                    if logged < MAX_FAULT_LOGS {
                        logged += 1;
                        rep.note(format!(
                            "domain_get_assemblies: fault {name} rva={rva:#x}: {f}"
                        ));
                    }
                }
            }
        }
        rep.faulted += faults;
        if rep.assemblies_fn.is_none() {
            rep.note(format!(
                "domain_get_assemblies: NOT found (tried {}, faulted {faults}, expected count {want})",
                rep.tried
            ));
        }
    } else {
        rep.note(format!("too few assemblies ({want}) to identify domain_get_assemblies"));
    }

    // ---- phase 2: il2cpp_class_from_name(image, ns, name) ------------------
    if rep.image != 0 {
        let ns = ffi::cstr("UnityEngine");
        let nm = ffi::cstr("Camera");
        let skip = rep.assemblies_fn.map(|f| f.addr).unwrap_or(0);
        let mut tried = 0usize;
        let mut faults = 0usize;
        let mut logged = 0usize;
        let mut tried_addrs2: Vec<usize> = Vec::new();
        'class: for (name, rva) in symbols::EXPORTS.iter() {
            if tried >= MAX_TRIED {
                break;
            }
            let addr = match ga_addr(*rva) {
                Some(a) => a,
                None => continue,
            };
            if addr == skip || tried_addrs2.contains(&addr) || !callable(addr) {
                continue;
            }
            tried_addrs2.push(addr);
            tried += 1;

            let r = match veh::call3(
                addr,
                rep.image,
                ns.as_ptr() as usize,
                nm.as_ptr() as usize,
            ) {
                Ok(r) => r,
                Err(f) => {
                    faults += 1;
                    if logged < MAX_FAULT_LOGS {
                        logged += 1;
                        rep.note(format!("class_from_name: fault {name} rva={rva:#x}: {f}"));
                    }
                    continue;
                }
            };
            if r < 0x10000 || !safe::readable_at(r) {
                continue;
            }
            let layout = match class_probe(r) {
                Some(l) => l,
                None => continue,
            };
            // Same answer twice, or it is not a lookup function.
            let r2 = match veh::call3(
                addr,
                rep.image,
                ns.as_ptr() as usize,
                nm.as_ptr() as usize,
            ) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if r2 != r {
                continue;
            }
            rep.class_fn = Some(Func { name, rva: *rva, addr });
            rep.layout = Some(layout);
            rep.note(format!(
                "found: class_from_name = {name} rva={rva:#x} addr={addr:#x} \
                 camera_class={r:#x} (repeat ok)"
            ));
            rep.note(format!(
                "class layout: name@+{:#x} ns@+{:#x} (window +/-0x40)",
                layout.name_off, layout.ns_off
            ));
            log::log_tag(
                "api",
                &format!(
                    "class probe: {:#x} name@+{:#x} ns@+{:#x}",
                    r, layout.name_off, layout.ns_off
                ),
            );
            break 'class;
        }
        rep.faulted += faults;
        if rep.class_fn.is_none() {
            rep.note(format!(
                "class_from_name: NOT found (tried {tried}, faulted {faults})"
            ));
        }
    } else {
        rep.note("no UnityEngine.CoreModule/mscorlib image — class_from_name skipped");
    }

    rep.note(format!(
        "identify done: tried={} faulted={} assemblies={} class={} field={}",
        rep.tried,
        rep.faulted,
        rep.assemblies_fn.is_some(),
        rep.class_fn.is_some(),
        rep.field_fn.is_some()
    ));
    rep
}

/// Look for the two name pointers in the first 0x80 bytes of a returned class.
/// Only exact matches count: `ascii_at` rejects any non-printable byte, so a
/// number or a pointer to something else cannot masquerade as "Camera".
fn class_probe(r: usize) -> Option<ClassLayout> {
    if r < 0x10000 || !safe::readable_at(r) {
        return None;
    }
    let mut name_off: Option<i64> = None;
    let mut ns_off: Option<i64> = None;
    let mut d: i64 = -0x40;
    while d <= 0x40 {
        if let Some(p) = (r as i64).checked_add(d) {
            if p > 0 {
                if let Some(ptr) = safe::read_u64(p as usize) {
                    if let Some(s) = safe::ascii_at(ptr as usize, 32) {
                        if s == "Camera" && name_off.is_none() {
                            name_off = Some(d);
                        } else if s == "UnityEngine" && ns_off.is_none() {
                            ns_off = Some(d);
                        }
                    }
                }
            }
        }
        d += 8;
    }
    match (name_off, ns_off) {
        (Some(n), Some(s)) => Some(ClassLayout { name_off: n, ns_off: s }),
        _ => None,
    }
}

// ---- public wrappers for M3 ------------------------------------------------

/// The il2cpp domain, or `None` before the runtime is up.
pub fn domain() -> Option<usize> {
    il2cpp::find_domain()
}

fn image_of_in(domain: usize, assembly: &str) -> Option<usize> {
    let (_, entry) = il2cpp::assembly(domain, assembly)?;
    let img = safe::read_u64(entry + 0x28)? as usize;
    (img > 0x10000).then_some(img)
}

/// `Il2CppImage*` of an assembly ("UnityEngine.CoreModule", "mscorlib", …).
/// Read through the assembly record: `+0x28` is the image, measured live.
pub fn image_of(assembly: &str) -> Option<usize> {
    image_of_in(domain()?, assembly)
}

/// `Il2CppClass*` by namespace/name. Needs `class_from_name` identified; runs
/// `identify()` on first use.
pub fn class_from_name(image: usize, ns: &str, name: &str) -> Option<usize> {
    let rep = identify();
    let f = rep.class_fn?;
    let ns_c = ffi::cstr(ns);
    let nm_c = ffi::cstr(name);
    let r = veh::call3(f.addr, image, ns_c.as_ptr() as usize, nm_c.as_ptr() as usize).ok()?;
    if r < 0x10000 || !safe::readable_at(r) {
        return None;
    }
    // If we know the layout, the returned object has to agree with the name we
    // asked for — otherwise the "lookup" returned something unrelated.
    if let Some(l) = rep.layout {
        let p = match (r as i64).checked_add(l.name_off) {
            Some(p) if p > 0 => p as usize,
            _ => return None,
        };
        let ok = safe::read_u64(p)
            .and_then(|v| safe::ascii_at(v as usize, 256))
            .map(|s| s == name)
            .unwrap_or(false);
        if !ok {
            return None;
        }
    }
    Some(r)
}

/// Class name via the layout learned during identification.
pub fn class_name(class: usize) -> Option<String> {
    let rep = identify();
    let l = rep.layout?;
    let p = (class as i64).checked_add(l.name_off)?;
    if p <= 0 {
        return None;
    }
    let ptr = safe::read_u64(p as usize)? as usize;
    safe::ascii_at(ptr, 256)
}

/// Class namespace, same mechanism (useful when debugging a candidate class).
pub fn class_namespace(class: usize) -> Option<String> {
    let rep = identify();
    let l = rep.layout?;
    let p = (class as i64).checked_add(l.ns_off)?;
    if p <= 0 {
        return None;
    }
    let ptr = safe::read_u64(p as usize)? as usize;
    safe::ascii_at(ptr, 256)
}

/// `Il2CppDomain::assembly_count`-free variant of `image_of` for callers that
/// already hold the domain.
pub fn image_of_domain(domain: usize, assembly: &str) -> Option<usize> {
    image_of_in(domain, assembly)
}
