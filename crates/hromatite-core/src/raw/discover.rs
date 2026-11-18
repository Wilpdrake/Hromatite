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

use hromatite_abi::build::{GcAnchors, Il2cppAnchors};

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
/// The stop-the-world critical section sits this far before `GC_threads[]`
/// (`BUCKETS * 8` bytes of bucket heads follow it).
const STW_CS_DISTANCE: u32 = 0x40;

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
    pub export_dir: u32,
    pub export_dir_size: u32,
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
    let mut found = [None; 3];
    for index in 0..sections {
        let entry = optional + optional_size + index * 40;
        let name = headers.get(entry..entry + 8)?;
        let vsize = u32_at(&headers, entry + 8)?;
        let rva = u32_at(&headers, entry + 12)?;
        let slot = match &name[..name.iter().position(|b| *b == 0).unwrap_or(8)] {
            b".text" => 0,
            b".rdata" => 1,
            b".data" => 2,
            _ => continue,
        };
        let limit = size.saturating_sub(rva as usize) as u32;
        found[slot] = Some(Section { rva, size: vsize.min(limit) });
    }
    Some(Image {
        text: found[0]?,
        rdata: found[1]?,
        data: found[2]?,
        export_dir,
        export_dir_size,
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
    let stw = threads.wrapping_sub(STW_CS_DISTANCE);
    let list_cs = discover_list_cs(&text, image.text.rva, threads, stw).unwrap_or(0);
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

/// The thread-list critical section is the most-referenced lea target in the
/// window before the table, excluding the stop-the-world lock itself.
fn discover_list_cs(text: &[u8], text_rva: u32, threads: u32, stw: u32) -> Option<u32> {
    let mut best: Option<(u32, usize)> = None;
    let low = threads.wrapping_sub(0x200);
    for offset in 0..text.len().saturating_sub(7) {
        if text.get(offset..offset + 2) != Some(&[0x48, 0x8D]) {
            continue;
        }
        let Some(modrm) = text.get(offset + 2).copied() else { continue };
        if modrm & 0xC7 != 0x05 {
            continue;
        }
        let Some(disp) = i32_at(text, offset + 3) else { continue };
        let target = text_rva
            .wrapping_add(offset as u32)
            .wrapping_add(7)
            .wrapping_add(disp as u32);
        if target < low || target >= threads || target == stw {
            continue;
        }
        match &mut best {
            Some((_, count)) if *count >= 2 => *count += 1,
            Some((name, count)) if *name == target => *count += 1,
            Some(_) => {}
            None => best = Some((target, 1)),
        }
    }
    best.map(|(target, _)| target)
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
        class_table,
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
}
