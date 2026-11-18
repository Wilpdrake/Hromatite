//! `EAC_M3_PROBE=1` — locate the il2cpp type table in the *current* build.
//!
//! `m3` carries the offsets measured on the 2026-11-14 build
//! (`CLASS_TABLE_RVA = 0xe66c870`). VRChat re-randomises its layout on every
//! patch, so a game update silently turns every managed hook into "class not
//! found". This probe answers three questions in one live run without any
//! il2cpp API call:
//!
//! 1. does the recorded table RVA still resolve to `Il2CppClass` records,
//! 2. if not, at which qword-aligned offset in `[-8 MiB, +8 MiB)` around it
//!    does a plausible `Il2CppClass**` array live (entries point into the
//!    image and carry an ASCII `namespaze`/`name` pair), and
//! 3. what do the first records look like, so the offsets can be re-measured.
//!
//! It only reads memory that `safe::readable_at` vouches for.

use hromatite_core::m3;
use hromatite_win::{GAME_ASSEMBLY, memory as safe, procs};

/// The table RVA measured on the 2026-11-17 build; the search is centred on it.
const LEGACY_TABLE_RVA: usize = 0x0e66_f0d0;
/// How far around the legacy RVA the scan looks (±8 MiB).
const SEARCH_RADIUS: usize = 8 * 1024 * 1024;
/// `Il2CppClass` fields the probe validates (2026-11-17 layout).
const CLASS_NS_OFF: usize = 0x18;
const CLASS_NAME_OFF: usize = 0x10;
/// How many consecutive entries must look like classes before a slot counts.
const MIN_SCORE: usize = 24;
/// How many entries are scored per candidate slot.
const SCORE_SPAN: usize = 32;

fn ascii_at(addr: usize) -> Option<String> {
    if addr < 0x1_0000 {
        return None;
    }
    let bytes = safe::read_bytes(addr, 64)?;
    let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(bytes.len());
    if end == 0 || end > 48 {
        return None;
    }
    let text = &bytes[..end];
    if !text
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'`' | b'<' | b'>'))
    {
        return None;
    }
    Some(String::from_utf8_lossy(text).into_owned())
}

fn looks_like_class(entry: usize, base: usize, size: usize) -> bool {
    if entry < base || entry >= base + size || !safe::readable_at(entry) {
        return false;
    }
    let Some(ns_ptr) = safe::read_usize(entry + CLASS_NS_OFF) else {
        return false;
    };
    let Some(name_ptr) = safe::read_usize(entry + CLASS_NAME_OFF) else {
        return false;
    };
    ascii_at(ns_ptr).is_some() || ascii_at(name_ptr).is_some()
}

fn score(table: usize, base: usize, size: usize) -> usize {
    (0..SCORE_SPAN)
        .filter(|index| {
            safe::read_usize(table + index * 8)
                .map(|entry| looks_like_class(entry, base, size))
                .unwrap_or(false)
        })
        .count()
}

/// Run the probe. Returns log lines; never panics.
pub fn probe() -> Vec<String> {
    let mut out = Vec::new();
    let Some((base, size)) = procs::own_module(GAME_ASSEMBLY) else {
        out.push("[m3] GameAssembly is not loaded".to_string());
        return out;
    };
    out.push(format!(
        "[m3] base={base:#x} size={size:#x} legacy_table_rva={LEGACY_TABLE_RVA:#x}"
    ));

    // 1. Does the recorded RVA still work?
    let legacy = base + LEGACY_TABLE_RVA;
    let legacy_score = score(legacy, base, size);
    out.push(format!(
        "[m3] legacy table={legacy:#x} score={legacy_score}/{SCORE_SPAN}"
    ));
    if legacy_score >= MIN_SCORE {
        for index in 0..4 {
            let class = m3::class_at(index).unwrap_or(0);
            out.push(format!(
                "[m3]   [{index}] class={class:#x} ns={:?} name={:?}",
                if class != 0 { m3::class_namespace(class) } else { None },
                if class != 0 { m3::class_name(class) } else { None }
            ));
        }
        return out;
    }

    // 2. Search around the legacy slot, coarse to fine (64 KiB then 8 B).
    let from = legacy.saturating_sub(SEARCH_RADIUS) & !7;
    let to = legacy.saturating_add(SEARCH_RADIUS);
    let mut best = (0usize, 0usize);
    let mut scanned = 0u64;
    let mut step = 0x1_0000;
    let mut coarse = from;
    while coarse < to {
        scanned += 1;
        let candidate = coarse & !7;
        let value = score(candidate, base, size);
        if value > best.0 {
            best = (value, candidate);
        }
        coarse += step;
    }
    if best.0 >= MIN_SCORE / 2 {
        // Refine around the best coarse slot.
        let inner_from = best.1.saturating_sub(0x2_0000);
        let inner_to = best.1.saturating_add(0x2_0000);
        let mut fine = inner_from & !7;
        while fine < inner_to {
            let value = score(fine, base, size);
            if value > best.0 {
                best = (value, fine);
            }
            fine += 8;
        }
        step = 8;
    }
    out.push(format!(
        "[m3] search: coarse_slots={scanned} step={step} best={}/{} at {:#x} (rva {:#x})",
        best.0,
        SCORE_SPAN,
        best.1,
        best.1.saturating_sub(base)
    ));
    if best.0 >= MIN_SCORE {
        for index in 0..6 {
            let entry = safe::read_usize(best.1 + index * 8).unwrap_or(0);
            let ns = safe::read_usize(entry + CLASS_NS_OFF)
                .and_then(ascii_at)
                .unwrap_or_default();
            let name = safe::read_usize(entry + CLASS_NAME_OFF)
                .and_then(ascii_at)
                .unwrap_or_default();
            out.push(format!(
                "[m3]   [{index}] entry={entry:#x} ns={ns:?} name={name:?}"
            ));
        }
    }
    out
}
