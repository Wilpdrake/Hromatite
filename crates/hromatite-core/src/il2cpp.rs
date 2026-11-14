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
//! So addresses are resolved empirically: UnityPlayer.dll resolves GameAssembly's
//! exports itself at startup and keeps the results next to the name strings it
//! feeds to `GetProcAddress`. We locate each name string, then find a slot that
//! points at it and whose neighbour points into GameAssembly — that neighbour is
//! the resolved export address.

use crate::log;
use crate::procs;
use crate::symbols;

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
        match hay[i..hay.len() - needle.len() + 1].iter().position(|&b| b == first) {
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

fn u64_at(mem: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(mem[off..off + 8].try_into().unwrap())
}

#[derive(Clone, Copy)]
pub struct Slot {
    pub name: &'static str,
    /// image offset of the name string inside UnityPlayer
    pub name_off: usize,
    /// image offset of the slot holding the resolved function pointer
    pub addr_off: usize,
    pub addr: usize,
}

/// Offsets (relative to the name-string pointer field) where the resolved
/// function pointer may sit. The struct is private, so every layout is tried
/// and the one with the most plausible hits wins.
const DELTAS: [i64; 6] = [8, 16, 24, -8, 32, 40];

/// Find the GameAssembly exports UnityPlayer resolved at startup.
pub fn resolve_table() -> (Vec<Slot>, i64) {
    let (up_base, up_size) = match procs::own_module("UnityPlayer.dll") {
        Some(v) => v,
        None => return (Vec::new(), 0),
    };
    let (ga_base, ga_size) = match procs::own_module("GameAssembly.dll") {
        Some(v) => v,
        None => return (Vec::new(), 0),
    };
    let mem = slice(up_base, up_size);
    let ga_lo = ga_base;
    let ga_hi = ga_base + ga_size;

    // 1. locate every export name inside UnityPlayer's image
    let mut found: Vec<(&'static str, usize)> = Vec::new();
    for (name, _rva) in symbols::EXPORTS {
        if let Some(off) = find(mem, name.as_bytes()) {
            found.push((name, off));
        }
    }
    if found.is_empty() {
        return (Vec::new(), 0);
    }

    // 2. for each name, find slots holding a pointer to it, then look around the
    //    slot for a value inside GameAssembly — that is the resolved export.
    let mut votes = [0usize; DELTAS.len()];
    let mut plain: Vec<(&'static str, usize, usize, usize)> = Vec::new();
    for (name, name_off) in &found {
        let needle = ((up_base + name_off) as u64).to_le_bytes();
        let mut i = 0usize;
        while i + 8 <= mem.len() {
            let rel = match mem[i..].windows(8).position(|w| w == needle) {
                Some(p) => p,
                None => break,
            };
            let slot_off = i + rel;
            let mut best: Option<usize> = None;
            for (idx, d) in DELTAS.iter().enumerate() {
                let cand = slot_off as i64 + d;
                if cand < 0 || (cand as usize) + 8 > mem.len() {
                    continue;
                }
                let v = u64_at(mem, cand as usize) as usize;
                if v >= ga_lo && v < ga_hi {
                    votes[idx] += 1;
                    if best.is_none() {
                        best = Some(v);
                    }
                }
            }
            if best.is_some() {
                plain.push((name, *name_off, slot_off, slot_off));
            }
            i = slot_off + 8;
        }
    }
    if plain.is_empty() {
        return (Vec::new(), 0);
    }

    // 3. the delta with the most GameAssembly hits is the real slot layout
    let (best_idx, _) = votes
        .iter()
        .enumerate()
        .max_by_key(|(_, v)| **v)
        .unwrap_or((0, &0));
    let delta = DELTAS[best_idx];

    let slots: Vec<Slot> = plain
        .iter()
        .filter_map(|(name, name_off, slot_off, _)| {
            let cand = *slot_off as i64 + delta;
            if cand < 0 || (cand as usize) + 8 > mem.len() {
                return None;
            }
            let addr_off = cand as usize;
            let addr = u64_at(mem, addr_off) as usize;
            (addr >= ga_lo && addr < ga_hi).then_some(Slot {
                name,
                name_off: *name_off,
                addr_off,
                addr,
            })
        })
        .collect();

    (slots, delta)
}

/// Tiny getters (`mov rax,[rip+d]; ret` / `lea rax,[rip+d]; ret`) are how il2cpp
/// exposes its singletons — including the domain. Shortlisting them lets the
/// next stage pick candidates without calling random functions.
pub fn getter_candidates(slots: &[Slot], ga_base: usize, ga_size: usize) -> Vec<Slot> {
    let mut out = Vec::new();
    for s in slots {
        if s.addr < ga_base || s.addr >= ga_base + ga_size {
            continue;
        }
        let mut buf = [0u8; 8];
        unsafe {
            core::ptr::copy_nonoverlapping(s.addr as *const u8, buf.as_mut_ptr(), 8);
        }
        let is_mov = buf[0] == 0x48 && buf[1] == 0x8B && buf[2] == 0x05 && buf[7] == 0xC3;
        let is_lea = buf[0] == 0x48 && buf[1] == 0x8D && buf[2] == 0x05 && buf[7] == 0xC3;
        if is_mov || is_lea {
            out.push(*s);
        }
    }
    out
}

fn hexdump(mem: &[u8], at: usize, len: usize) -> String {
    let lo = at.saturating_sub(16);
    let hi = (at + len + 16).min(mem.len());
    let mut out = String::new();
    let mut p = lo;
    while p < hi {
        let chunk_end = (p + 8).min(hi);
        let bytes = &mem[p..chunk_end];
        let as_u64 = if bytes.len() == 8 {
            u64::from_le_bytes(bytes.try_into().unwrap())
        } else {
            0
        };
        out.push_str(&format!("  {p:08x} {bytes:02x?}"));
        if bytes.len() == 8 {
            out.push_str(&format!("  as_u64={as_u64:#x}"));
        }
        out.push('\n');
        p += 8;
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
    let (up_base, up_size) = match up {
        Some(v) => v,
        None => return out + "UnityPlayer.dll not loaded yet\n",
    };

    let (slots, delta) = resolve_table();
    out.push_str(&format!(
        "export table: {} slots resolved (delta {delta}), build {} [{}]\n",
        slots.len(),
        symbols::UNITY_VERSION,
        &symbols::GAMEASSEMBLY_SHA256[..12]
    ));

    let cands = getter_candidates(&slots, ga_base, ga_size);
    out.push_str(&format!("singleton getters among them: {}\n", cands.len()));
    for c in cands.iter().take(8) {
        out.push_str(&format!(
            "  getter? {} slot+{:#x} -> {:#x}\n",
            c.name, c.addr_off, c.addr
        ));
    }

    let mem = slice(up_base, up_size);
    if let Some(s) = slots.first() {
        out.push_str(&format!("sample slot {} (name@up+{:#x}):\n", s.name, s.name_off));
        out.push_str(&hexdump(mem, s.addr_off, 8));
    }
    out
}

pub fn run_once() {
    let report = diagnose();
    for line in report.lines() {
        log::log(&format!("diag: {line}"));
    }
}
