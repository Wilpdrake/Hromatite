//! Derive per-build anchors from the loaded image instead of trusting a table.
//!
//! Every rule here was validated on two consecutive VRChat builds: run against
//! the previous `GameAssembly.dll` it reproduces the addresses that were derived
//! by hand in Ghidra (`tools/export_transfer.py` does the same for export
//! names). Nothing in this module assumes a specific RVA — only code shapes:
//!
//! * `GC_threads[]` is the global loaded by `lea r14,[rip+d]` right before
//!   `mov rbx,[r14+rsi*8]` inside the thread-removal function;
//! * `GC_suspend_all` contains the `lea` of the `"SuspendThread loop failed"`
//!   message at a fixed distance from its entry;
//! * `s_TypeInfoDefinitionTable` is the global of `mov rax,[rip+d];
//!   lea rsi,[rax+rdi*8]` in the metadata cache;
//! * `il2cpp_image_get_class` has a distinctive ten-instruction body;
//! * `il2cpp_get_corlib` is the export that returns a global whose `+0x8` is
//!   the ASCII short name `"mscorlib"`.

#![forbid(unsafe_code)]

use hromatite_abi::build::{GcAnchors, Il2cppAnchors, RuntimeAnchors};

use crate::{log, safe};

/// Distances inside `GC_suspend_all` that are unchanged between builds: the two
/// `lea` references to the abort message, the per-record label, the two `int3`
/// aborts and the bucket advance.
const SUSPEND_MSG_XREF_FIRST: u32 = 0x128;
/// `call [GetThreadContext]` inside `GC_suspend_all`.
const SUSPEND_CONTEXT_CALL: u32 = 0xb2;
const SUSPEND_MSG_XREF_SECOND: u32 = 0x139;
const SUSPEND_PROCESS_RECORD: u32 = 0x70;
const SUSPEND_RESUME_ABORT: u32 = 0xdb;
const SUSPEND_ABORT: u32 = 0x145;
const SUSPEND_ADVANCE_BUCKET: u32 = 0x24a;
/// Distances inside `GC_remove_thread`.
const REMOVE_SKIP_CLOSE: u32 = 0x40;
const REMOVE_UNLINKED_NEXT: u32 = 0x46;
const REMOVE_AFTER_UNLINK: u32 = 0x68;

#[derive(Clone, Copy, Debug)]
pub struct Section {
    pub rva: u32,
    pub size: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct Image {
    pub text: Section,
    pub rdata: Section,
    /// `.data` bounds; kept for heap/table scanning callers.
    #[allow(dead_code)]
    pub data: Section,
    /// IL2CPP's own code section (`il2cpp`, no leading dot). Managed methods
    /// live here, not in `.text`: a `.text`-only scan finds no PlayerLoop.
    pub il2cpp: Option<Section>,
    /// `.pdata` (exception directory payload): the only exact source of
    /// function boundaries — byte walks stop on `0xCC` bytes inside rel32
    /// displacements and C++ EH scopes add entries *inside* one function.
    pub pdata: Section,
    pub export_dir: u32,
    pub export_dir_size: u32,
    pub import_dir: u32,
    pub import_dir_size: u32,
}

fn u16_at(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_le_bytes(bytes.get(offset..offset + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?))
}

fn i32_at(bytes: &[u8], offset: usize) -> Option<i32> {
    Some(i32::from_le_bytes(bytes.get(offset..offset + 4)?.try_into().ok()?))
}

/// Read the section table of an already-mapped PE image.
pub fn image(base: usize, size: usize) -> Option<Image> {
    let headers = safe::read_bytes(base, 0x1000)?;
    if headers.get(..2)? != b"MZ" {
        return None;
    }
    let pe_offset = u32_at(&headers, 0x3C)? as usize;
    if headers.get(pe_offset..pe_offset + 4)? != b"PE\0\0" {
        return None;
    }
    let coff = pe_offset + 4;
    let sections = u16_at(&headers, coff + 2)? as usize;
    let optional_size = u16_at(&headers, coff + 16)? as usize;
    let optional = coff + 20;
    let dirs = optional + 112;
    let export_dir = u32_at(&headers, dirs)?;
    let export_dir_size = u32_at(&headers, dirs + 4)?;
    let exception = Section {
        rva: u32_at(&headers, dirs + 3 * 8)?,
        size: u32_at(&headers, dirs + 3 * 8 + 4)?,
    };
    let import_dir = u32_at(&headers, dirs + 8 * 8)?;
    let import_dir_size = u32_at(&headers, dirs + 8 * 8 + 4)?;
    let mut found = [None; 4];
    for index in 0..sections {
        let entry = optional + optional_size + index * 40;
        let name = headers.get(entry..entry + 8)?;
        let vsize = u32_at(&headers, entry + 8)?;
        let rva = u32_at(&headers, entry + 12)?;
        let slot = match &name[..name.iter().position(|b| *b == 0).unwrap_or(8)] {
            b".text" => 0,
            b".rdata" => 1,
            b".data" => 2,
            b"il2cpp" => 3,
            _ => continue,
        };
        let limit = size.saturating_sub(rva as usize) as u32;
        found[slot] = Some(Section { rva, size: vsize.min(limit) });
    }
    Some(Image {
        text: found[0]?,
        rdata: found[1]?,
        data: found[2]?,
        il2cpp: found[3],
        pdata: exception,
        export_dir,
        export_dir_size,
        import_dir,
        import_dir_size,
    })
}

// ---------------------------------------------------------------------------
// pure scan helpers (unit-tested on synthetic buffers)
// ---------------------------------------------------------------------------

/// All start offsets of `needle` inside `hay`.
pub(crate) fn find_all(hay: &[u8], needle: &[u8]) -> Vec<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut index = 0;
    while index + needle.len() <= hay.len() {
        if hay[index..].starts_with(needle) {
            out.push(index);
        }
        index += 1;
    }
    out
}

/// Every `lea reg,[rip+disp32]` whose target equals `target`.
pub(crate) fn lea_targets(hay: &[u8], hay_rva: u32, target: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for site in find_all(hay, &[0x48, 0x8D]) {
        let Some(modrm) = hay.get(site + 2).copied() else { continue };
        if modrm & 0xC7 != 0x05 {
            continue;
        }
        let Some(disp) = i32_at(hay, site + 3) else { continue };
        let rva = hay_rva.wrapping_add(site as u32).wrapping_add(7);
        if rva.wrapping_add(disp as u32) == target {
            out.push(hay_rva.wrapping_add(site as u32));
        }
    }
    out
}

/// `lea r14,[rip+d]` … `mov rbx,[r14+rsi*8]` — the thread table and the
/// function that walks it.
pub(crate) fn remove_thread_and_table(hay: &[u8], hay_rva: u32) -> Option<(u32, u32)> {
    let walk = *find_all(hay, &[0x49, 0x8B, 0x1C, 0xF6]).first()?;
    let mut lea_site = None;
    let start = walk.saturating_sub(0x40);
    let mut index = start;
    while index < walk {
        if hay.get(index..index + 3) == Some(&[0x4C, 0x8D, 0x35]) {
            lea_site = Some(index);
        }
        index += 1;
    }
    let lea_site = lea_site?;
    let disp = i32_at(hay, lea_site + 3)?;
    let table = hay_rva
        .wrapping_add(lea_site as u32)
        .wrapping_add(7)
        .wrapping_add(disp as u32);
    // The function starts after the last int3 padding byte.
    let mut entry = lea_site;
    while entry > 0 && hay.get(entry - 1).copied() != Some(0xCC) {
        entry -= 1;
        if lea_site - entry > 0x100 {
            return None;
        }
    }
    Some((hay_rva.wrapping_add(entry as u32), table))
}

/// `mov rax,[rip+d]; lea rsi,[rax+rdi*8]` — the class table descriptor slot.
pub(crate) fn class_table_global(hay: &[u8], hay_rva: u32) -> Option<u32> {
    for site in find_all(hay, &[0x48, 0x8B, 0x05]) {
        // `mov rax,[rip+d]` followed by `lea rsi,[rax+rdi*8]`
        if hay.get(site + 7..site + 11) != Some(&[0x48, 0x8D, 0x34, 0xF8]) {
            continue;
        }
        let disp = i32_at(hay, site + 3)?;
        return Some(hay_rva.wrapping_add(site as u32).wrapping_add(7).wrapping_add(disp as u32));
    }
    None
}

/// `il2cpp_image_get_class`: reads the per-image metadata handle, adds the index
/// and scales by `sizeof(Il2CppTypeDefinition)`.
pub(crate) fn image_get_class_site(hay: &[u8], hay_rva: u32) -> Option<u32> {
    let pattern = [0x44, 0x8B, 0x00, 0x44, 0x03, 0xC2, 0x41, 0x83, 0xF8, 0xFF, 0x75];
    for site in find_all(hay, &[0x48, 0x8B, 0x41]) {
        if hay.get(site + 4..site + 4 + pattern.len()) == Some(&pattern[..]) {
            return Some(hay_rva.wrapping_add(site as u32));
        }
    }
    None
}

/// `mov rax,[rip+d]; ret` — a global getter export.
pub(crate) fn naked_getter_global(hay: &[u8], hay_rva: u32, at: usize) -> Option<u32> {
    if hay.get(at..at + 3) != Some(&[0x48, 0x8B, 0x05]) || hay.get(at + 7) != Some(&0xC3) {
        return None;
    }
    let disp = i32_at(hay, at + 3)?;
    Some(hay_rva.wrapping_add(at as u32).wrapping_add(7).wrapping_add(disp as u32))
}

// ---------------------------------------------------------------------------
// image-backed discovery
// ---------------------------------------------------------------------------

/// Every occurrence of `needle` in a section, as RVAs (bounded to `limit`).
fn find_in_section(base: usize, section: Section, needle: &[u8], limit: usize) -> Vec<u32> {
    const WINDOW: usize = 4 * 1024 * 1024;
    const OVERLAP: usize = 64;
    let mut out = Vec::new();
    let mut offset = 0usize;
    while offset < section.size as usize && out.len() < limit {
        let take = WINDOW.min(section.size as usize - offset);
        let Some(bytes) = safe::read_bytes(base + section.rva as usize + offset, take) else {
            break;
        };
        let mut cursor = 0usize;
        while cursor + needle.len() <= bytes.len() && out.len() < limit {
            match bytes[cursor..].windows(needle.len()).position(|window| window == needle) {
                Some(hit) => {
                    out.push(section.rva + (offset + cursor + hit) as u32);
                    cursor += hit + needle.len();
                }
                None => break,
            }
        }
        if take == WINDOW {
            offset += WINDOW - OVERLAP;
        } else {
            break;
        }
    }
    out
}

fn read_text(base: usize, image: Image) -> Option<Vec<u8>> {
    safe::read_bytes(base + image.text.rva as usize, image.text.size as usize)
}

/// Export directory of the loaded image: `(name, rva)` pairs.
pub fn exports(base: usize, image: Image) -> Vec<(String, u32)> {
    let Some(dir) = safe::read_bytes(base + image.export_dir as usize, image.export_dir_size as usize)
    else {
        return Vec::new();
    };
    let (Some(n_names), Some(names_rva), Some(ords_rva), Some(funcs_rva)) = (
        u32_at(&dir, 24),
        u32_at(&dir, 32),
        u32_at(&dir, 36),
        u32_at(&dir, 28),
    ) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for index in 0..n_names as usize {
        let Some(name_rva) = u32_at(&dir, names_rva as usize + index * 4 - image.export_dir as usize)
        else {
            continue;
        };
        let Some(ordinal) = u16_at(&dir, ords_rva as usize + index * 2 - image.export_dir as usize)
        else {
            continue;
        };
        let name = safe::read_upto(base + name_rva as usize, 64)
            .split(|byte| *byte == 0)
            .next()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default();
        let Some(func) = u32_at(&dir, funcs_rva as usize + ordinal as usize * 4 - image.export_dir as usize)
        else {
            continue;
        };
        out.push((name, func));
    }
    out
}

/// `il2cpp_get_corlib`: the global getter whose value is an image whose `+0x8`
/// is the string `"mscorlib"`.
fn corlib_getter(base: usize, image: Image, text: &[u8], exports: &[(String, u32)]) -> Option<u32> {
    for (_, rva) in exports {
        let Some(global) = naked_getter_global(text, image.text.rva, *rva as usize - image.text.rva as usize)
        else {
            continue;
        };
        let Some(slot) = safe::read_usize(base + global as usize) else {
            continue;
        };
        let Some(name) = safe::read_upto(slot + 8, 16)
            .split(|byte| *byte == 0)
            .next()
            .map(|bytes| bytes.to_vec())
        else {
            continue;
        };
        if name == b"mscorlib" {
            return Some(*rva);
        }
    }
    None
}

/// GC anchors derived from the image alone.
pub fn gc_anchors(base: usize, size: usize) -> Option<GcAnchors> {
    let image = image(base, size)?;
    let text = read_text(base, image)?;
    // Both spellings of the abort message live in .rdata; their code xrefs are at
    // entry+0x128 and entry+0x139, so the earliest xref fixes the entry.
    let messages = find_in_section(base, image.rdata, b"SuspendThread loop failed", 8);
    let mut sites = Vec::new();
    for message in messages {
        sites.extend(lea_targets(&text, image.text.rva, message));
    }
    let first = sites.iter().copied().min()?;
    let entry = first.checked_sub(SUSPEND_MSG_XREF_FIRST)?;
    let second = entry.wrapping_add(SUSPEND_MSG_XREF_SECOND);
    let (remove_thread, threads) = remove_thread_and_table(&text, image.text.rva)?;
    if !sites.contains(&second) {
        log::log_tag("gct", "discover: abort message xrefs do not match the expected pair");
        return None;
    }
    let anchor = |delta: u32| entry.wrapping_add(delta);
    let suspend_abort = anchor(SUSPEND_ABORT);
    let resume_abort = anchor(SUSPEND_RESUME_ABORT);
    for (label, rva) in [("suspend", suspend_abort), ("resume", resume_abort)] {
        let offset = (rva - image.text.rva) as usize;
        if text.get(offset) != Some(&0xCC) {
            log::log_tag("gct", &format!("discover: {label} abort site is not int3"));
            return None;
        }
    }
    let list_cs = collector_cs(base, image, &text, entry).unwrap_or(0);
    if list_cs == 0 {
        log::log_tag(
            "gct",
            "discover: the collector's critical section was not derived; sanitisation stays off",
        );
    }
    let anchors = GcAnchors {
        remove_thread,
        skip_close: remove_thread.wrapping_add(REMOVE_SKIP_CLOSE),
        unlinked_next: remove_thread.wrapping_add(REMOVE_UNLINKED_NEXT),
        after_unlink: remove_thread.wrapping_add(REMOVE_AFTER_UNLINK),
        suspend_entry: entry,
        context_call: anchor(SUSPEND_CONTEXT_CALL),
        process_record: anchor(SUSPEND_PROCESS_RECORD),
        resume_abort,
        suspend_abort,
        advance_bucket: anchor(SUSPEND_ADVANCE_BUCKET),
        threads,
        thread_list_cs: list_cs,
    };
    log::log_tag(
        "gct",
        &format!(
            "discover: gc anchors remove={:#x} suspend={:#x} threads={:#x} list_cs={:#x}",
            anchors.remove_thread, anchors.suspend_entry, anchors.threads, anchors.thread_list_cs
        ),
    );
    Some(anchors)
}

/// The critical section `GC_suspend_all` enters — Boehm's collector lock.
///
/// The old heuristic ("most-referenced lea target in the 0x200 bytes before the
/// table") picked the string `"CP1252"` on the next build, and
/// `EnterCriticalSection` on a string never returns: the game hung. The lock is
/// named by one instruction instead — the `lea rcx` that feeds
/// `call [EnterCriticalSection]` inside the stop-the-world pass.
fn collector_cs(base: usize, image: Image, text: &[u8], entry: u32) -> Option<u32> {
    let offset = (entry.wrapping_sub(image.text.rva)) as usize;
    let body = text.get(offset..offset.checked_add(0x260)?)?;
    let body_rva = entry;
    let mut votes: Vec<u32> = Vec::new();
    for site in find_all(body, &[0xFF, 0x15]) {
        let Some(disp) = i32_at(body, site + 2) else { continue };
        let slot = body_rva.wrapping_add(site as u32).wrapping_add(6).wrapping_add(disp as u32);
        if iat_import_name(base, image, slot).as_deref() != Some("EnterCriticalSection") {
            continue;
        }
        let lowest = site.saturating_sub(18);
        for back in (lowest..site).rev() {
            if body.get(back..back + 3) != Some(&[0x48, 0x8D, 0x0D]) {
                continue;
            }
            let Some(disp) = i32_at(body, back + 3) else { continue };
            let target = body_rva
                .wrapping_add(back as u32)
                .wrapping_add(7)
                .wrapping_add(disp as u32);
            if !target.is_multiple_of(8) {
                continue;
            }
            votes.push(target);
            break;
        }
    }
    votes.sort_unstable();
    votes.dedup();
    match votes.as_slice() {
        [only] => Some(*only),
        other => {
            log::log_tag(
                "gct",
                &format!(
                    "discover: GC_suspend_all enters {} critical section(s); refusing to guess",
                    other.len()
                ),
            );
            None
        }
    }
}

/// Name of the import a thunk slot points at (`IMAGE_THUNK_DATA` -> hint/name).
fn iat_import_name(base: usize, image: Image, slot: u32) -> Option<String> {
    if image.import_dir == 0 {
        return None;
    }
    let mut cursor = 0usize;
    while cursor < image.import_dir_size as usize {
        let descriptor = safe::read_bytes(base + image.import_dir as usize + cursor, 20)?;
        if descriptor.iter().all(|byte| *byte == 0) {
            return None;
        }
        let first_thunk = u32_at(&descriptor, 16)?;
        let thunk_rva = u32_at(&descriptor, 0).filter(|value| *value != 0).unwrap_or(first_thunk);
        if slot >= first_thunk {
            let index = (slot - first_thunk) / 8;
            if index < 4096 {
                let value = safe::read_u64(base + thunk_rva as usize + index as usize * 8)?;
                if value != 0 && value >> 63 == 0 {
                    let name = safe::read_upto(base + (value & 0x7FFF_FFFF) as usize + 2, 64);
                    if name.is_empty() {
                        return None;
                    }
                    let bytes: &[u8] = name.split(|byte| *byte == 0).next().unwrap_or(&[]);
                    return Some(String::from_utf8_lossy(bytes).into_owned());
                }
            }
        }
        cursor += 20;
    }
    None
}

/// il2cpp entry points derived from the image alone.
pub fn il2cpp_anchors(base: usize, size: usize) -> Option<Il2cppAnchors> {
    let image = image(base, size)?;
    let text = read_text(base, image)?;
    let exports = exports(base, image);
    let class_table = class_table_global(&text, image.text.rva)?;
    let image_get_class = image_get_class_site(&text, image.text.rva)?;
    let get_corlib = corlib_getter(base, image, &text, &exports)?;
    log::log_tag(
        "m3",
        &format!(
            "discover: il2cpp anchors corlib={get_corlib:#x} image_get_class={image_get_class:#x} class_table={class_table:#x}"
        ),
    );
    Some(Il2cppAnchors {
        get_corlib,
        image_get_class,
        // `il2cpp_class_from_name` cannot be derived from shapes yet; a zero
        // anchor makes that path refuse instead of calling the module base.
        class_from_name: 0,
        class_from_type: 0,
        class_table,
        // The object/array/string factories have no shape of their own either
        // (they are ordinary exported functions with obfuscated names), so an
        // unknown build cannot build managed buffers: the in-game renderer
        // refuses instead of guessing.
        object_new: 0,
        array_new: 0,
        string_new: 0,
        class_init: 0,
    })
}

// ---------------------------------------------------------------------------
// non-export entry points (PlayerLoop getter, worker dispatcher, GC unregister)
// ---------------------------------------------------------------------------

/// Prologue of the compiled managed `PlayerLoop.GetCurrentPlayerLoop`: it zeroes
/// the 0x28-byte `PlayerLoopSystem` it was handed in RCX.
const PLAYER_LOOP_PROLOGUE: &[u8] = &[
    0x40, 0x53, 0x48, 0x83, 0xEC, 0x50, 0x33, 0xC0, 0xC7, 0x44, 0x24, 0x60, 0x00, 0x00, 0x00,
    0x00, 0x0F, 0x57, 0xC0, 0x48, 0x8B, 0xD9, 0x0F, 0x11, 0x01, 0x0F, 0x11, 0x41, 0x10, 0x48,
    0x89, 0x41, 0x20,
];

/// The icall the getter caches. A build that renames it fails closed here rather
/// than somewhere inside the collector.
const PLAYER_LOOP_ICALL: &[u8] = b"UnityEngine.LowLevel.PlayerLoop::GetCurrentPlayerLoopInternal()";

/// il2cpp's thread dispatcher: `RCX = {thread*, fn, arg}`, and it bumps the
/// thread's flags word before calling the shared trampoline.
const WORKER_DISPATCH_BODY: &[u8] = &[
    0x48, 0x89, 0x5C, 0x24, 0x08, 0x48, 0x89, 0x6C, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18,
    0x48, 0x89, 0x7C, 0x24, 0x20, 0x41, 0x56, 0x48, 0x83, 0xEC, 0x20, 0x48, 0x8B, 0xD9, 0x48,
    0x8B, 0x31, 0x48, 0x8B, 0x46, 0x08, 0x8B, 0x90, 0xF8, 0x00, 0x00, 0x00, 0x83, 0xE2, 0xFB,
    0x83, 0xFA, 0x02,
];

/// `lea reg,[rip+d]` sites in `body` that point at `target`.
pub(crate) fn lea_sites(body: &[u8], body_rva: u32, target: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for site in 0..body.len().saturating_sub(7) {
        if body[site..site + 2] != [0x48, 0x8D] && body[site..site + 2] != [0x4C, 0x8D] {
            continue;
        }
        if body[site + 2] & 0xC7 != 0x05 {
            continue;
        }
        let Some(disp) = i32_at(body, site + 3) else { continue };
        let rip = body_rva.wrapping_add(site as u32).wrapping_add(7);
        if rip.wrapping_add(disp as u32) == target {
            out.push(body_rva.wrapping_add(site as u32));
        }
    }
    out
}

/// `call rel32` sites in `body` that land on `target`.
pub(crate) fn call_sites(body: &[u8], body_rva: u32, target: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for site in find_all(body, &[0xE8]) {
        let Some(disp) = i32_at(body, site + 1) else { continue };
        let rip = body_rva.wrapping_add(site as u32).wrapping_add(5);
        if rip.wrapping_add(disp as u32) == target {
            out.push(body_rva.wrapping_add(site as u32));
        }
    }
    out
}

/// Function entry that contains `rva`, from the exception directory.
///
/// Entries are `{begin, end, unwind}` and sorted by `begin`; Windows emits one
/// entry per C++ EH scope, so the containing entry is usually a *fragment*. The
/// chained UNWIND_INFO points at its parent, and following that chain ends at
/// the primary entry — which is the address a detour must patch.
pub(crate) fn function_at(
    entries: &[u8],
    rva: u32,
    unwind_info: impl FnMut(u32) -> Option<Vec<u8>>,
) -> Option<u32> {
    let count = entries.len() / 12;
    if count == 0 {
        return None;
    }
    let (mut low, mut high) = (0usize, count);
    while low < high {
        let middle = (low + high) / 2;
        let begin = u32_at(entries, middle * 12)?;
        if begin <= rva {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    if low == 0 {
        return None;
    }
    let begin = u32_at(entries, (low - 1) * 12)?;
    let end = u32_at(entries, (low - 1) * 12 + 4)?;
    if !(begin <= rva && rva < end) {
        return None;
    }
    primary_from(entries, begin, unwind_info)
}

fn entries_index_of(entries: &[u8], begin: u32) -> Option<usize> {
    let count = entries.len() / 12;
    let (mut low, mut high) = (0usize, count);
    while low < high {
        let middle = (low + high) / 2;
        let value = u32_at(entries, middle * 12)?;
        match value.cmp(&begin) {
            core::cmp::Ordering::Less => low = middle + 1,
            core::cmp::Ordering::Greater => high = middle,
            core::cmp::Ordering::Equal => return Some(middle),
        }
    }
    None
}

fn primary_from(
    entries: &[u8],
    begin: u32,
    mut unwind_info: impl FnMut(u32) -> Option<Vec<u8>>,
) -> Option<u32> {
    let mut current = begin;
    for _ in 0..8 {
        let index = entries_index_of(entries, current)?;
        let unwind = u32_at(entries, index * 12 + 8)?;
        let info = unwind_info(unwind)?;
        if info.len() < 4 || info[0] >> 3 & 0x4 == 0 {
            return Some(current);
        }
        let codes = info[2] as usize;
        let offset = 4 + 2 * codes + if codes & 1 == 1 { 2 } else { 0 };
        let parent = info.get(offset..offset + 4)?;
        current = u32::from_le_bytes(parent.try_into().ok()?);
    }
    Some(current)
}

fn read_pdata(base: usize, image: Image) -> Vec<u8> {
    if image.pdata.size == 0 {
        return Vec::new();
    }
    safe::read_bytes(base + image.pdata.rva as usize, image.pdata.size as usize).unwrap_or_default()
}

fn unwind_reader(base: usize) -> impl FnMut(u32) -> Option<Vec<u8>> {
    move |rva: u32| {
        let bytes = safe::read_upto(base + rva as usize, 32);
        (bytes.len() >= 4).then_some(bytes)
    }
}

/// The compiled `PlayerLoop.GetCurrentPlayerLoop`, by prologue shape plus the
/// icall it caches. `.text` first, then the IL2CPP code section that actually
/// holds managed methods.
fn player_loop_getter(base: usize, image: Image) -> Option<u32> {
    let icall = *find_in_section(base, image.rdata, PLAYER_LOOP_ICALL, 1).first()?;
    let mut sections = vec![image.text];
    sections.extend(image.il2cpp);
    let mut proven = Vec::new();
    for section in sections {
        for start in find_in_section(base, section, PLAYER_LOOP_PROLOGUE, 8) {
            let Some(body) = safe::read_bytes(base + start as usize, 0x80) else {
                continue;
            };
            if !lea_sites(&body, start, icall).is_empty() {
                proven.push(start);
            }
        }
    }
    match proven.len() {
        1 => Some(proven[0]),
        _ => {
            log::log_tag(
                "playerloop",
                &format!(
                    "discover: {} PlayerLoop candidates resolve the icall; refusing to guess",
                    proven.len()
                ),
            );
            None
        }
    }
}

/// il2cpp's thread dispatcher: unique body, cross-checked by the `lea` its
/// creator uses to hand it to `CreateThread`.
fn worker_dispatch(text: &[u8], text_rva: u32) -> Option<u32> {
    let hits = find_all(text, WORKER_DISPATCH_BODY);
    if hits.len() != 1 {
        log::log_tag(
            "gct",
            &format!("discover: {} worker-dispatch bodies, refusing", hits.len()),
        );
        return None;
    }
    let dispatch = text_rva.wrapping_add(hits[0] as u32);
    if lea_sites(text, text_rva, dispatch).is_empty() {
        log::log_tag("gct", "discover: nothing refers to the worker dispatcher");
        return None;
    }
    Some(dispatch)
}

/// Runtime anchors derived from the image alone (`gc` supplies the one address
/// this rule starts from: `GC_remove_thread`).
pub fn runtime_anchors(base: usize, size: usize, gc: &GcAnchors) -> Option<RuntimeAnchors> {
    let image = image(base, size)?;
    let text = read_text(base, image)?;
    let getter = player_loop_getter(base, image)?;
    let dispatch = worker_dispatch(&text, image.text.rva)?;
    let entries = read_pdata(base, image);
    let mut callers = Vec::new();
    for site in call_sites(&text, image.text.rva, gc.remove_thread) {
        let mut reader = unwind_reader(base);
        if let Some(entry) = function_at(&entries, site, &mut reader) {
            callers.push(entry);
        }
    }
    callers.sort_unstable();
    callers.dedup();
    let [unregister] = callers.as_slice() else {
        log::log_tag(
            "gct",
            &format!(
                "discover: {} function(s) call GC_remove_thread, refusing",
                callers.len()
            ),
        );
        return None;
    };
    log::log_tag(
        "playerloop",
        &format!(
            "discover: player_loop={getter:#x} worker_dispatch={dispatch:#x} gc_unregister={unregister:#x}"
        ),
    );
    Some(RuntimeAnchors {
        get_current_player_loop: getter,
        worker_dispatch: dispatch,
        gc_unregister: *unregister,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lea(reg_opcode: u8, from: u32, to: u32) -> [u8; 7] {
        let disp = (to as i64 - (from as i64 + 7)) as i32;
        let mut out = [0x48, 0x8D, reg_opcode, 0, 0, 0, 0];
        out[3..7].copy_from_slice(&disp.to_le_bytes());
        out
    }

    #[test]
    fn finds_lea_targets_by_rip_relative_displacement() {
        let mut text = vec![0x90; 32];
        let target = 0x5000;
        let bytes = lea(0x0D, 0x1010, target);
        text[0x10..0x17].copy_from_slice(&bytes);
        let hits = lea_targets(&text, 0x1000, target);
        assert_eq!(hits, vec![0x1010]);
        assert!(lea_targets(&text, 0x1000, target + 1).is_empty());
    }

    #[test]
    fn locates_the_thread_table_and_entry_from_the_bucket_walk() {
        let mut text = vec![0x90; 0x80];
        text[..0x20].fill(0xCC);
        let entry = 0x20usize;
        text[entry..entry + 4].copy_from_slice(&[0x48, 0x89, 0x5C, 0x24]);
        let lea_site = 0x37usize;
        // `lea r14,[rip+d]` carries REX.R, i.e. 4C 8D 35.
        let bytes = lea(0x35, 0x1000 + lea_site as u32, 0x2_0000);
        text[lea_site..lea_site + 7].copy_from_slice(&bytes);
        text[lea_site] = 0x4C;
        let walk = 0x48usize;
        text[walk..walk + 4].copy_from_slice(&[0x49, 0x8B, 0x1C, 0xF6]);
        let (found_entry, table) = remove_thread_and_table(&text, 0x1000).expect("pattern");
        assert_eq!(found_entry, 0x1000 + entry as u32);
        assert_eq!(table, 0x2_0000);
    }

    #[test]
    fn finds_the_class_table_slot() {
        let mut text = vec![0x90; 32];
        let slot = 0x2000u32;
        let disp = slot as i32 - (0x1004 + 7);
        let mut bytes = vec![0x48, 0x8B, 0x05];
        bytes.extend_from_slice(&disp.to_le_bytes());
        bytes.extend_from_slice(&[0x48, 0x8D, 0x34, 0xF8]);
        text[4..4 + bytes.len()].copy_from_slice(&bytes);
        assert_eq!(class_table_global(&text, 0x1000), Some(slot));
    }

    #[test]
    fn recognises_the_image_get_class_body() {
        let mut text = vec![0x90; 32];
        let body = [
            0x48, 0x8B, 0x41, 0x28, 0x44, 0x8B, 0x00, 0x44, 0x03, 0xC2, 0x41, 0x83, 0xF8, 0xFF,
            0x75, 0x07,
        ];
        text[8..8 + body.len()].copy_from_slice(&body);
        assert_eq!(image_get_class_site(&text, 0x1000), Some(0x1008));
    }

    #[test]
    fn naked_getter_global_reads_the_displacement() {
        let mut text = vec![0x90; 16];
        let disp: i32 = 0x100 - (0x1000 + 7);
        text[0..3].copy_from_slice(&[0x48, 0x8B, 0x05]);
        text[3..7].copy_from_slice(&disp.to_le_bytes());
        text[7] = 0xC3;
        assert_eq!(naked_getter_global(&text, 0x1000, 0), Some(0x100));
        text[7] = 0x90;
        assert_eq!(naked_getter_global(&text, 0x1000, 0), None);
    }

    #[test]
    fn lea_sites_find_rip_relative_references() {
        let mut body = vec![0x90; 0x20];
        let target = 0x9000u32;
        let disp = (target as i64 - (0x1004 + 7)) as i32;
        body[4..7].copy_from_slice(&[0x48, 0x8D, 0x0D]);
        body[7..11].copy_from_slice(&disp.to_le_bytes());
        assert_eq!(lea_sites(&body, 0x1000, target), vec![0x1004]);
        assert!(lea_sites(&body, 0x1000, target + 1).is_empty());
    }

    #[test]
    fn call_sites_find_rel32_callers() {
        let mut body = vec![0x90; 16];
        let target = 0x8000u32;
        let disp = (target as i64 - (0x1004 + 5)) as i32;
        body[4] = 0xE8;
        body[5..9].copy_from_slice(&disp.to_le_bytes());
        assert_eq!(call_sites(&body, 0x1000, target), vec![0x1004]);
        assert!(call_sites(&body, 0x1000, target + 1).is_empty());
    }

    fn pdata(entries: &[(u32, u32, u32)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (begin, end, unwind) in entries {
            out.extend_from_slice(&begin.to_le_bytes());
            out.extend_from_slice(&end.to_le_bytes());
            out.extend_from_slice(&unwind.to_le_bytes());
        }
        out
    }

    /// A chained UNWIND_INFO points at its parent; the fragment is *inside* the
    /// function, so the byte walk would land in the middle of it.
    #[test]
    fn function_at_follows_the_eh_chain_to_the_primary_entry() {
        // 0x4000 = primary (no chained flag), 0x4020 = fragment chained to it.
        let entries = pdata(&[(0x4000, 0x4004, 0x5000), (0x4020, 0x4100, 0x5010)]);
        let unwind = |rva: u32| -> Option<Vec<u8>> {
            match rva {
                0x5000 => Some(vec![0x01, 0x00, 0x00, 0x00]),
                // version 1, flags 4 (UNW_FLAG_CHAININFO), 0 codes, parent 0x4000
                0x5010 => Some(vec![0x21, 0x00, 0x00, 0x00, 0x00, 0x40, 0x00, 0x00]),
                _ => None,
            }
        };
        assert_eq!(function_at(&entries, 0x4030, unwind), Some(0x4000));
        assert_eq!(function_at(&entries, 0x4002, |_| Some(vec![1, 0, 0, 0])), Some(0x4000));
        // Outside every entry: refuse rather than guess.
        assert_eq!(function_at(&entries, 0x4200, unwind), None);
        assert_eq!(function_at(&[], 0x4000, unwind), None);
    }

    #[test]
    fn player_loop_prologue_and_dispatcher_bodies_are_intact() {
        // Guard against a silent edit: both are byte-for-byte copies of the two
        // reference builds, so their length and edges matter.
        assert_eq!(PLAYER_LOOP_PROLOGUE.len(), 0x21);
        assert_eq!(PLAYER_LOOP_PROLOGUE[..7], [0x40, 0x53, 0x48, 0x83, 0xEC, 0x50, 0x33]);
        assert_eq!(WORKER_DISPATCH_BODY.len(), 0x30);
        assert_eq!(WORKER_DISPATCH_BODY[..4], [0x48, 0x89, 0x5C, 0x24]);
        assert!(PLAYER_LOOP_ICALL.starts_with(b"UnityEngine.LowLevel.PlayerLoop::"));
    }
}
