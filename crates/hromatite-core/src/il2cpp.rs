//! Runtime il2cpp reconnaissance, in-process.
//!
//! Two facts drive the design:
//!   * GameAssembly's export names are Beebyte-renamed per build, so we carry a
//!     generated name list (`symbols.rs`) and never an RVA;
//!   * names alone are not enough — we also need to know *which* obfuscated
//!     export is `il2cpp_domain_get`. The published `il2cpp-api-functions.h`
//!     order (Unity 2020) does NOT match the Unity 6 table, so ordering is
//!     useless as an oracle.
//!
//! Measured on the shipping build (2026-11-14): the 235 obfuscated export names
//! sit in `UnityPlayer.dll:.rdata` as a zero-padded string pool, each name on
//! its own, 0x38..0x80 bytes apart, and **no 8-byte value anywhere in the image
//! points at them** (checked offline over all 4.2 M aligned words). So the
//! name → resolved-address table is built at runtime somewhere else, most
//! likely on the heap. That is why the scan below walks every readable region
//! of the process instead of only UnityPlayer's image.
//!
//! Two independent oracles are collected in one pass:
//!   1. name pointers — any word equal to the VA of a known name string; the
//!      neighbouring field that lands inside GameAssembly is the export;
//!   2. pointer runs — long contiguous arrays of pointers into GameAssembly,
//!      which is what a `void* api[]` table looks like; if such a run exists,
//!      its index order should follow the .rdata pool order.

use crate::log;
use crate::procs;
use crate::symbols;
use std::collections::HashMap;

fn slice(base: usize, size: usize) -> &'static [u8] {
    unsafe { core::slice::from_raw_parts(base as *const u8, size) }
}

pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let first = needle[0];
    let mut i = 0usize;
    while i + needle.len() <= hay.len() {
        match hay[i..hay.len() - needle.len() + 1]
            .iter()
            .position(|&b| b == first)
        {
            Some(p) => {
                let at = i + p;
                if &hay[at..at + needle.len()] == needle {
                    return Some(at);
                }
                i = at + 1;
            }
            None => return None,
        }
    }
    None
}

/// Single pass over an image collecting every occurrence of every known export
/// name (capped per name: the pool has duplicates and the first copy is not
/// necessarily the interesting one).
fn locate_names(mem: &[u8], cap: usize) -> Vec<(&'static str, usize)> {
    let wanted: HashMap<&'static str, usize> =
        symbols::EXPORTS.iter().map(|(n, _)| (*n, 0usize)).collect();
    let mut out: Vec<(&'static str, usize)> = Vec::new();
    let mut count: HashMap<&'static str, usize> = HashMap::with_capacity(wanted.len());

    let is_name_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = 0usize;
    while i < mem.len() {
        if !is_name_byte(mem[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < mem.len() && is_name_byte(mem[i]) {
            i += 1;
        }
        let len = i - start;
        if (4..=32).contains(&len) {
            if let Ok(s) = core::str::from_utf8(&mem[start..i]) {
                if let Some((k, _)) = wanted.get_key_value(s) {
                    let c = count.entry(k).or_insert(0);
                    if *c < cap {
                        *c += 1;
                        out.push((k, start));
                    }
                }
            }
        }
    }
    out
}

#[derive(Clone, Copy)]
pub struct Slot {
    pub name: &'static str,
    /// image offset of the name string inside UnityPlayer
    pub name_off: usize,
    /// absolute address of the slot that yields `addr` (abs for oracle 1,
    /// image offset for oracle-free bookkeeping)
    pub addr_off: usize,
    pub addr: usize,
}

/// Offsets (relative to the name-string pointer slot) where the resolved
/// function pointer may sit, `-0x80..+0x80` in 8-byte steps. Measured on the
/// live build 2026-11-14: the name pointers *were* found on the heap (236 of
/// them) but none of `+8/+16/+24/-8/+32/+40` held a GameAssembly address, so
/// the table is not a packed `{name, fn}` struct — hence the wider window and
/// the per-hit dumps below.
const DELTAS: [i64; 33] = [
    -0x80, -0x78, -0x70, -0x68, -0x60, -0x58, -0x50, -0x48, -0x40, -0x38, -0x30, -0x28, -0x20,
    -0x18, -0x10, -8, 0, 8, 0x10, 0x18, 0x20, 0x28, 0x30, 0x38, 0x40, 0x48, 0x50, 0x58, 0x60,
    0x68, 0x70, 0x78, 0x80,
];

#[derive(Default, Clone)]
pub struct Stats {
    pub names: usize,
    pub occ: usize,
    pub regions: usize,
    pub scanned_mb: usize,
    pub name_ptrs: usize,
    pub votes: Vec<usize>,
    /// (absolute address of run start, word count, first word)
    pub runs: Vec<(usize, usize, usize)>,
    pub secs: f32,
}

pub struct Resolved {
    pub slots: Vec<Slot>,
    pub order: Vec<Slot>,
    pub delta: i64,
    pub stats: Stats,
    /// absolute address + name of every slot found holding a name pointer
    pub hits: Vec<(usize, &'static str)>,
    /// absolute base of the run oracle 2 used, if any
    pub run_base: usize,
}

fn is_readable(r: &procs::Region) -> bool {
    r.readable() && !r.executable()
}

/// One pass over the whole readable address space: collect name-pointer slots
/// and contiguous GameAssembly pointer runs at the same time.
fn scan_process(
    up_base: usize,
    up_size: usize,
    targets: &HashMap<u64, &'static str>,
    ga_lo: usize,
    ga_hi: usize,
    budget_mb: usize,
) -> (Vec<(usize, &'static str)>, Stats) {
    let mut stats = Stats {
        votes: vec![0usize; DELTAS.len()],
        ..Default::default()
    };
    let mut hits: Vec<(usize, &'static str)> = Vec::new();
    let mut runs: Vec<(usize, usize, usize)> = Vec::new();

    let regions = procs::regions(200_000);
    stats.regions = regions.len();
    let budget = budget_mb * 1024 * 1024;
    let mut spent = 0usize;

    for reg in &regions {
        if !is_readable(reg) || reg.size == 0 {
            continue;
        }
        if spent >= budget {
            break;
        }
        let size = reg.size.min(budget - spent);
        spent += size;
        stats.scanned_mb = spent / (1024 * 1024);

        // align to 8 bytes, walk words
        let mut addr = (reg.base + 7) & !7usize;
        let end = reg.base + size - 8;
        let mut run_start = 0usize;
        let mut run_len = 0usize;
        let mut run_first = 0usize;

        while addr <= end {
            let w = unsafe { *(addr as *const u64) } as usize;
            let in_ga = w >= ga_lo && w < ga_hi;

            if in_ga {
                if run_len == 0 {
                    run_start = addr;
                    run_first = w;
                }
                run_len += 1;
            } else if run_len > 0 {
                if run_len >= 32 {
                    runs.push((run_start, run_len, run_first));
                }
                run_len = 0;
            }

            // name string lives inside UnityPlayer's image, so a cheap range
            // test removes almost every word before the hash lookup
            if w >= up_base && w < up_base + up_size {
                if let Some(name) = targets.get(&(w as u64)) {
                    hits.push((addr, name));
                    stats.name_ptrs += 1;
                    for (idx, d) in DELTAS.iter().enumerate() {
                        let cand = addr as i64 + d;
                        if cand < 0 {
                            continue;
                        }
                        let v = unsafe { *(cand as usize as *const u64) } as usize;
                        if v >= ga_lo && v < ga_hi {
                            stats.votes[idx] += 1;
                        }
                    }
                }
            }
            addr += 8;
        }
        if run_len >= 32 {
            runs.push((run_start, run_len, run_first));
        }
    }

    runs.sort_by_key(|r| core::cmp::Reverse(r.1));
    runs.truncate(8);
    stats.runs = runs;
    (hits, stats)
}

/// Best-effort resolution using both oracles. `delta` is the winning struct
/// offset for oracle 1 (0 when nothing was found there).
pub fn resolve(budget_mb: usize) -> Option<Resolved> {
    let t0 = std::time::Instant::now();
    let (up_base, up_size) = procs::own_module("UnityPlayer.dll")?;
    let (ga_base, ga_size) = procs::own_module("GameAssembly.dll")?;
    let mem = slice(up_base, up_size);

    let found = locate_names(mem, 4);
    let mut stats = Stats {
        names: {
            let mut u: Vec<&str> = found.iter().map(|(n, _)| *n).collect();
            u.sort_unstable();
            u.dedup();
            u.len()
        },
        occ: found.len(),
        ..Default::default()
    };
    if found.is_empty() {
        stats.secs = t0.elapsed().as_secs_f32();
        return Some(Resolved {
            slots: Vec::new(),
            order: Vec::new(),
            delta: 0,
            stats,
            hits: Vec::new(),
            run_base: 0,
        });
    }

    let mut targets: HashMap<u64, &'static str> = HashMap::with_capacity(found.len());
    for (name, off) in &found {
        targets.insert((up_base + off) as u64, name);
    }

    let (hits, mut scan) = scan_process(up_base, up_size, &targets, ga_base, ga_base + ga_size, budget_mb);
    scan.names = stats.names;
    scan.occ = stats.occ;

    let (best_idx, _) = scan
        .votes
        .iter()
        .enumerate()
        .max_by_key(|(_, v)| **v)
        .unwrap_or((0, &0));
    let delta = DELTAS[best_idx];
    let mut slots: Vec<Slot> = Vec::new();
    for (addr, name) in &hits {
        let cand = *addr as i64 + delta;
        if cand < 0 {
            continue;
        }
        let v = unsafe { *(cand as usize as *const u64) } as usize;
        if v >= ga_base && v < ga_base + ga_size {
            slots.push(Slot {
                name,
                name_off: (*addr).wrapping_sub(up_base),
                addr_off: cand as usize,
                addr: v,
            });
        }
    }

    // oracle 2: if the process holds a contiguous array of GameAssembly
    // pointers, its index order should follow the .rdata pool order. Long runs
    // are useless here (il2cpp's own metadata pages are full of pointers into
    // GameAssembly: the best run measured 430 976 words), so only a run whose
    // length is in the same ballpark as the export count is a table candidate.
    let n_names = {
        let mut u: Vec<&str> = found.iter().map(|(n, _)| *n).collect();
        u.sort_unstable();
        u.dedup();
        u.len()
    };
    let cand_run = scan
        .runs
        .iter()
        .filter(|(_, words, _)| *words >= n_names / 2 && *words <= n_names * 4)
        .min_by_key(|(_, words, _)| words.abs_diff(n_names))
        .copied();
    let mut order: Vec<Slot> = Vec::new();
    if let Some((run_addr, run_words, _)) = cand_run {
        let mut by_off: Vec<(&'static str, usize)> = found.clone();
        by_off.sort_by_key(|(_, o)| *o);
        by_off.dedup_by_key(|(n, _)| *n);
        if run_words >= by_off.len() {
            for (i, (name, name_off)) in by_off.iter().enumerate() {
                let slot = run_addr + i * 8;
                let addr = unsafe { *(slot as *const u64) } as usize;
                order.push(Slot {
                    name,
                    name_off: *name_off,
                    addr_off: slot,
                    addr,
                });
            }
        }
    }

    scan.secs = t0.elapsed().as_secs_f32();
    let run_base = cand_run.map(|r| r.0).unwrap_or(0);
    Some(Resolved {
        slots,
        order,
        delta,
        stats: scan,
        hits,
        run_base,
    })
}

/// 8-byte words around `at` for the report: `addr [hex] as_u64 (ga|up|…)`.
fn window(at: usize, before: usize, after: usize, ga: (usize, usize), up: (usize, usize)) -> String {
    let mut out = String::new();
    let mut p = at - before;
    let end = at + after;
    while p < end {
        let v = unsafe { *(p as *const u64) };
        let tag = if v as usize >= ga.0 && (v as usize) < ga.0 + ga.1 {
            "GA"
        } else if v as usize >= up.0 && (v as usize) < up.0 + up.1 {
            "UP"
        } else if v < 0x10000 {
            "small"
        } else {
            ""
        };
        let mark = if p == at { "<<" } else { "  " };
        out.push_str(&format!("  {mark}{p:#x}: {v:#018x} {tag}\n"));
        p += 8;
    }
    out
}

/// Tiny getters (`mov rax,[rip+d]; ret` / `lea rax,[rip+d]; ret`) are how il2cpp
/// exposes its singletons — including the domain. Shortlisting them lets the
/// next stage pick candidates without calling random functions.
pub fn is_getter(addr: usize) -> bool {
    let mut buf = [0u8; 8];
    unsafe {
        core::ptr::copy_nonoverlapping(addr as *const u8, buf.as_mut_ptr(), 8);
    }
    let is_mov = buf[0] == 0x48 && buf[1] == 0x8B && buf[2] == 0x05 && buf[7] == 0xC3;
    let is_lea = buf[0] == 0x48 && buf[1] == 0x8D && buf[2] == 0x05 && buf[7] == 0xC3;
    is_mov || is_lea
}

fn describe(slots: &[Slot], tag: &str) -> String {
    let getters: Vec<&Slot> = slots.iter().filter(|s| is_getter(s.addr)).collect();
    let mut out = format!("{tag}: {} entries, {} singleton getters\n", slots.len(), getters.len());
    for g in getters.iter().take(10) {
        out.push_str(&format!(
            "  getter {} @copy+{:#x} -> {:#x}\n",
            g.name, g.name_off, g.addr
        ));
    }
    if let Some(s) = slots.first() {
        out.push_str(&format!(
            "  first: {} name@up+{:#x} slot@{:#x} -> {:#x}\n",
            s.name, s.name_off, s.addr_off, s.addr
        ));
    }
    out
}

pub fn diagnose() -> String {
    let mut out = String::new();
    let ga = procs::own_module("GameAssembly.dll");
    let up = procs::own_module("UnityPlayer.dll");
    out.push_str(&format!("GameAssembly.dll: {ga:?}\nUnityPlayer.dll: {up:?}\n"));

    let (ga_base, ga_size) = match ga {
        Some(v) => v,
        None => return out + "GameAssembly.dll not loaded yet\n",
    };
    if up.is_none() {
        return out + "UnityPlayer.dll not loaded yet\n";
    }

    let budget: usize = std::env::var("HROMATITE_SCAN_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2048);

    let res = match resolve(budget) {
        Some(r) => r,
        None => return out + "resolve() found no modules\n",
    };

    out.push_str(&format!(
        "build {} [{}]\n",
        symbols::UNITY_VERSION,
        &symbols::GAMEASSEMBLY_SHA256[..12]
    ));
    out.push_str(&format!(
        "names: {}/{} ({} occurrences), regions {}, scanned {} MB in {:.1}s\n",
        res.stats.names,
        symbols::EXPORTS.len(),
        res.stats.occ,
        res.stats.regions,
        res.stats.scanned_mb,
        res.stats.secs
    ));
    out.push_str(&format!(
        "name pointers: {}, delta votes {:?} -> delta {}\n",
        res.stats.name_ptrs, res.stats.votes, res.delta
    ));
    out.push_str(&format!(
        "export table: {} slots resolved (oracle 1)\n",
        res.slots.len()
    ));
    if !res.slots.is_empty() {
        out.push_str(&describe(&res.slots, "oracle1(name-ptr)"));
        out.push_str(&format!(
            "  getter sample decoded: {} -> {:#x}\n",
            res.slots[0].name, res.slots[0].addr
        ));
    }

    // How many name-pointer slots have a GameAssembly address anywhere close?
    // If this is 0 the table does not store the resolved pointer next to the
    // name at all, and the dump below shows what it does store.
    let upm = procs::own_module("UnityPlayer.dll").unwrap_or((0, 0));
    let mut near = 0usize;
    for (addr, _) in res.hits.iter().take(4000) {
        let mut p = addr.saturating_sub(0x80);
        let mut found = false;
        while p <= addr + 0x80 {
            let v = unsafe { *(p as *const u64) } as usize;
            if v >= ga_base && v < ga_base + ga_size {
                found = true;
                break;
            }
            p += 8;
        }
        if found {
            near += 1;
        }
    }
    out.push_str(&format!(
        "hits with a GA pointer within +/-0x80: {near}/{} (UnityPlayer {:#x}+{:#x})\n",
        res.hits.len().min(4000),
        upm.0,
        upm.1
    ));

    for (addr, words, first) in &res.stats.runs {
        out.push_str(&format!(
            "run @{addr:#x}: {words} words, first -> {first:#x} (ga {ga_base:#x}+{ga_size:#x})\n"
        ));
    }
    out.push_str(&format!("run used by oracle 2: {:#x}\n", res.run_base));
    if !res.order.is_empty() {
        out.push_str(&describe(&res.order, "oracle2(pool-order)"));
    }

    // Raw neighbourhood of the first few name-pointer slots: the layout of the
    // runtime table is what the next stage has to mimic.
    for (addr, name) in res.hits.iter().take(3) {
        out.push_str(&format!("window around {name} @{addr:#x}:\n"));
        out.push_str(&window(*addr, 0x20, 0x40, (ga_base, ga_size), upm));
    }
    out
}

pub fn run_once() {
    let report = diagnose();
    for line in report.lines() {
        log::log(&format!("diag: {line}"));
    }
}
