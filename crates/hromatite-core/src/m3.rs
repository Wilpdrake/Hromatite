//! Unity 6 (6000.0.x) il2cpp layout as measured on this build.
//!
//! Everything here was verified live (2026-11-15, sha256 `648f61b1…`):
//!
//! ```text
//! GameAssembly .data +0xe66c870  → Il2CppClass**  (s_TypeInfoDefinitionTable,
//!                                  indexed by typeDefinitionIndex, lazily filled)
//! Il2CppClass +0x00              → const Il2CppImage*
//!            +0x18              → const char* namespaze
//!            +0x50              → const MethodInfo**  (array of pointers)
//!            +0x80              → FieldInfo[]         (inline records)
//!            +0x98              → const char* name
//!            +0x124             → uint16 fieldsCount
//! FieldInfo  +0x00              → const Il2CppType*
//!            +0x08              → int32 offset
//!            +0x10              → const char* name
//!            +0x18              → uint32 token
//!            +0x20              → Il2CppClass* parent        (stride 0x28)
//! MethodInfo +0x00              → methodPointer
//!            +0x18              → const char* name
//!            +0x20              → const Il2CppType* return_type
//!            +0x28              → Il2CppClass* klass
//!            +0x30              → parameters
//! ```
//!
//! The image array is not the "domain": `il2cpp_get_corlib` (obfuscated
//! `wYRnnMQuvvH`, RVA 0xc27f00) returns the *corlib `Il2CppImage`*, and the
//! images of all 216 assemblies are laid out back to back with stride 0x48
//! (`image + i*0x48`); `image + 0x08` is the short name, `image + 0x30` the
//! `Il2CppCodeGenModule`.
//!
//! `il2cpp_image_get_class` is `GEsTfKaEfsh` at RVA 0xc2a330 (verified live:
//! index 167 of UnityEngine.CoreModule returns `Camera`). Reading the class
//! table directly needs no API call at all, which is why this module is safe
//! from the game's own thread.

#![forbid(unsafe_code)]

use crate::procs;
use crate::safe;

/// `Il2CppClass**` (s_TypeInfoDefinitionTable) inside GameAssembly `.data`.
pub const CLASS_TABLE_RVA: u32 = 0x0e66_c870;
/// Offset of `il2cpp_image_get_class` (GEsTfKaEfsh).
pub const IMAGE_GET_CLASS_RVA: u32 = 0x00c2_a330;
/// Offset of `il2cpp_class_get_fields` (verified by disassembly: reads
/// `class+0x80` and the u16 count at `class+0x124`).
pub const CLASS_GET_FIELDS_RVA: u32 = 0x00c2_8070;
/// Offset of `il2cpp_class_from_name` (thunk `jmp`).
pub const CLASS_FROM_NAME_RVA: u32 = 0x00c2_8050;

pub const CLASS_IMAGE_OFF: usize = 0x00;
pub const CLASS_NS_OFF: usize = 0x18;
pub const CLASS_METHODS_OFF: usize = 0x50;
pub const CLASS_FIELDS_OFF: usize = 0x80;
pub const CLASS_NAME_OFF: usize = 0x98;
pub const CLASS_FIELDS_COUNT_OFF: usize = 0x124;

pub const FIELD_STRIDE: usize = 0x28;
const FIELD_TYPE_OFF: usize = 0x00;
const FIELD_NAME_OFF: usize = 0x10;
// Verified by il2cpp_field_get_offset/value/set_value (tools/data/
// il2cpp-export-names.json): offset is int32 at +0x08; token is at +0x18.
const FIELD_OFFSET_OFF: usize = 0x08;
const FIELD_PARENT_OFF: usize = 0x20;

const METHOD_NAME_OFF: usize = 0x18;
const METHOD_RETURN_TYPE_OFF: usize = 0x20;

fn method_return_kind_with<R, U>(mi: usize, read_usize: R, read_u32: U) -> u32
where
    R: FnOnce(usize) -> Option<usize>,
    U: FnOnce(usize) -> Option<u32>,
{
    read_usize(mi + METHOD_RETURN_TYPE_OFF)
        .and_then(|ty| read_u32(ty + 0x08))
        .map(|bits| (bits >> 16) & 0xff)
        .unwrap_or(u32::MAX)
}

/// Entries scored when a candidate table is validated.
const CANDIDATE_SPAN: usize = 64;
/// Entries scored during the fine pass (cheap, alignment is what it looks for).
const REFINE_SPAN: usize = 32;
/// A candidate must look like a real `Il2CppClass**` on this many of the first
/// [`CANDIDATE_SPAN`] entries *and* grow on this many, otherwise it is noise.
const MIN_VALID: usize = 24;

/// How many of the first `span` entries point at a plausible `Il2CppClass`.
fn valid_entries(table: usize, span: usize) -> usize {
    (0..span)
        .filter(|index| {
            let Some(entry) = safe::read_usize(table + index * 8) else {
                return false;
            };
            entry >= 0x1_0000
                && safe::readable_at(entry)
                && safe::read_usize(entry + CLASS_NAME_OFF).is_some_and(|name| {
                    name >= 0x1_0000 && name_at(name).is_some_and(|text| !text.is_empty())
                })
        })
        .count()
}

/// How many entries are strictly larger than their predecessor. il2cpp allocates
/// `Il2CppClass` records in type-definition order, so a real table is monotonic;
/// a run of unrelated pointers (code, vtables) is not.
fn increasing_entries(table: usize, span: usize) -> usize {
    let mut count = 0usize;
    let mut previous = 0usize;
    for index in 0..span {
        let Some(entry) = safe::read_usize(table + index * 8) else {
            break;
        };
        if entry > previous {
            count += 1;
        }
        previous = entry;
    }
    count
}

fn score_table(table: usize, span: usize) -> (usize, usize) {
    (valid_entries(table, span), increasing_entries(table, span))
}

fn plausible(table: usize) -> bool {
    if table < 0x1_0000 || !safe::readable_at(table) {
        return false;
    }
    let (valid, increasing) = score_table(table, CANDIDATE_SPAN);
    valid >= MIN_VALID && increasing >= MIN_VALID
}

/// Find `s_TypeInfoDefinitionTable` in the *current* build.
///
/// The RVA measured on 2026-11-14 (`CLASS_TABLE_RVA`) moved on 2026-11-17 and
/// VRChat re-randomises it on every patch, so the recorded value is a hint, not
/// a fact: this walks the whole image coarse (64 KiB) then fine (8 B) and keeps
/// the best-scoring candidate. Costs a second or two once, in the payload's
/// release build; the answer is cached for the run.
fn locate_table(base: usize, size: usize) -> Option<usize> {
    let legacy = base.checked_add(CLASS_TABLE_RVA as usize)?;
    // The recorded slot dereferences to the array on the build it was measured
    // on; a bare array at that RVA is the other shape the same number can mean.
    if let Some(pointer) = safe::read_usize(legacy)
        && plausible(pointer)
    {
        crate::log::log_tag(
            "m3",
            &format!("class table: legacy slot {legacy:#x} -> {pointer:#x}"),
        );
        return Some(pointer);
    }
    if plausible(legacy) {
        crate::log::log_tag(
            "m3",
            &format!("class table: legacy RVA is the array itself ({legacy:#x})"),
        );
        return Some(legacy);
    }

    // Coarse pass over the image. Nothing outside `[base, base+size)` can be the
    // array, and the array is at least a few KiB long, so a 64 KiB stride cannot
    // step over it.
    let end = base.saturating_add(size).saturating_sub(8);
    let mut best = (0usize, 0usize, 0usize);
    let mut at = base;
    while at < end {
        let (valid, increasing) = score_table(at, REFINE_SPAN);
        let total = valid + increasing;
        if total > best.2 && valid >= MIN_VALID / 2 {
            best = (at, valid, total);
        }
        at += 0x1_0000;
    }
    if best.0 == 0 {
        crate::log::log_tag("m3", "class table: nothing scored in the image");
        return None;
    }
    // Fine pass: the coarse winner sits somewhere inside its 64 KiB window.
    let from = best.0.saturating_sub(0x1_0000);
    let to = best.0.saturating_add(0x1_0000);
    let mut found = 0usize;
    let mut found_score = 0usize;
    let mut fine = from & !7;
    while fine < to {
        let (valid, increasing) = score_table(fine, REFINE_SPAN);
        let total = valid + increasing;
        if total > found_score {
            found = fine;
            found_score = total;
        }
        fine += 8;
    }
    crate::log::log_tag(
        "m3",
        &format!(
            "class table: scanned {size:#x} B, coarse {:#x} ({}/{}), fine {found:#x} (score {found_score})",
            best.0, best.1, best.2
        ),
    );
    if found != 0 {
        // Say out loud what the winner actually contains: a plausible-looking
        // array of the wrong kind is the expensive failure mode here.
        let mut sample = Vec::new();
        for index in 0..6 {
            let Some(class) = safe::read_usize(found + index * 8) else {
                break;
            };
            if class < 0x1_0000 {
                sample.push(format!("[{index}] null"));
                continue;
            }
            sample.push(format!("[{index}] {}", class_full_name(class)));
        }
        let filled = (0..1024)
            .filter(|index| safe::read_usize(found + index * 8).is_some_and(|p| p >= 0x1_0000))
            .count();
        crate::log::log_tag(
            "m3",
            &format!(
                "class table: sample {} (filled {filled}/1024)",
                sample.join(", ")
            ),
        );
        // Which qwords of a record point at ASCII? That single dump re-measures
        // the `name`/`namespaze` offsets after a build change.
        for index in 0..2 {
            let Some(class) = safe::read_usize(found + index * 8) else {
                continue;
            };
            if class < 0x1_0000 {
                continue;
            }
            let map: Vec<String> = (0..0x110 / 8)
                .filter_map(|slot| {
                    let off = slot * 8;
                    let value = safe::read_usize(class + off)?;
                    let text = name_at(value)?;
                    (!text.is_empty()).then(|| format!("+{off:#x}={text:?}"))
                })
                .collect();
            crate::log::log_tag(
                "m3",
                &format!("class[{index}]={class:#x} strings {}", map.join(" ")),
            );
        }
    }
    (found != 0).then_some(found)
}

/// Resolved class table (cached: the pointer is stable for the run).
pub fn class_table() -> Option<usize> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CACHE: AtomicUsize = AtomicUsize::new(0);
    static TRIES: AtomicUsize = AtomicUsize::new(0);
    let cached = CACHE.load(Ordering::Relaxed);
    if cached != 0 {
        return Some(cached);
    }
    let (base, size) = procs::own_module(crate::GAME_ASSEMBLY)?;
    // The old EXE-only path: the recorded RVA holds a pointer to the array.
    let slot = base.checked_add(CLASS_TABLE_RVA as usize)?;
    if safe::readable_at(slot)
        && let Some(t) = safe::read_usize(slot)
        && t >= 0x1_0000
        && safe::readable_at(t)
        && plausible(t)
    {
        CACHE.store(t, Ordering::Relaxed);
        return Some(t);
    }
    // A full scan is no good on every tick: the table fills lazily, so a miss is
    // retried, but at most once every few calls.
    let tries = TRIES.fetch_add(1, Ordering::Relaxed) + 1;
    if tries > 1 && !tries.is_multiple_of(8) {
        return None;
    }
    let t = locate_table(base, size)?;
    CACHE.store(t, Ordering::Relaxed);
    Some(t)
}

fn name_at(ptr: usize) -> Option<String> {
    if ptr < 0x10000 {
        return Some(String::new());
    }
    safe::ascii_at(ptr, 256)
}

/// `Il2CppClass*` for a `typeDefinitionIndex`.
pub fn class_at(index: usize) -> Option<usize> {
    let table = class_table()?;
    let p = safe::read_usize(table + index * 8)?;
    (p >= 0x10000 && safe::readable_at(p)).then_some(p)
}

/// Parent class (`Il2CppClass.parent`, measured at `+0x10` on this build:
/// Camera → Behaviour → Component → Object → System.Object).
pub const CLASS_PARENT_OFF: usize = 0x10;

pub fn parent(class: usize) -> Option<usize> {
    let p = safe::read_usize(class + CLASS_PARENT_OFF)?;
    (p >= 0x10000 && safe::readable_at(p)).then_some(p)
}

/// Walk the inheritance chain, starting with `class` itself.
pub fn chain(class: usize) -> Vec<usize> {
    let mut out = vec![class];
    let mut cur = class;
    for _ in 0..32 {
        match parent(cur) {
            Some(p) => {
                out.push(p);
                cur = p;
            }
            None => break,
        }
    }
    out
}

/// First method with this name anywhere in the class hierarchy.
pub fn method_in_chain(class: usize, name: &str) -> Option<(usize, usize)> {
    for c in chain(class) {
        for m in methods(c) {
            if m.name == name {
                return Some((c, m.entry));
            }
        }
    }
    None
}

/// Class name/namespace (this build: name at +0x98, namespace at +0x18).
pub fn class_name(class: usize) -> Option<String> {
    name_at(safe::read_usize(class + CLASS_NAME_OFF)?)
}

pub fn class_namespace(class: usize) -> Option<String> {
    name_at(safe::read_usize(class + CLASS_NS_OFF)?)
}

pub fn class_full_name(class: usize) -> String {
    let n = class_name(class).unwrap_or_default();
    let ns = class_namespace(class).unwrap_or_default();
    if ns.is_empty() {
        n
    } else {
        format!("{ns}.{n}")
    }
}

/// Every materialised class, in table order (index = typeDefinitionIndex).
pub fn classes(max: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let Some(table) = class_table() else {
        return out;
    };
    for i in 0..max {
        let p = match safe::read_usize(table + i * 8) {
            Some(p) => p,
            None => break,
        };
        if p < 0x10000 || !safe::readable_at(p) {
            continue;
        }
        out.push((i, p));
    }
    out
}

/// Linear search by namespace/name. `name` empty = every class of that namespace.
pub fn find(ns: &str, name: &str, max: usize) -> Vec<(usize, usize)> {
    let mut hits = Vec::new();
    let Some(table) = class_table() else {
        return hits;
    };
    for i in 0..max {
        let p = match safe::read_usize(table + i * 8) {
            Some(p) => p,
            None => break,
        };
        if p < 0x10000 || !safe::readable_at(p) {
            continue;
        }
        let Some(cname) = class_name(p) else {
            continue;
        };
        if !name.is_empty() && cname != name {
            continue;
        }
        let cns = class_namespace(p).unwrap_or_default();
        if !ns.is_empty() && cns != ns {
            continue;
        }
        hits.push((i, p));
        if hits.len() >= 64 {
            break;
        }
    }
    hits
}

/// Diagnostics only: every class whose **name contains** `needle`, up to
/// `limit` hits, with its full `Namespace.Name`. Used when a hook cannot find
/// its type by exact name after a game update — it says what the table really
/// holds instead of leaving a bare "not found".
pub fn find_contains(needle: &str, max: usize, limit: usize) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let Some(table) = class_table() else {
        return out;
    };
    for i in 0..max {
        let Some(p) = safe::read_usize(table + i * 8) else {
            break;
        };
        if p < 0x10000 || !safe::readable_at(p) {
            continue;
        }
        let Some(name) = class_name(p) else {
            continue;
        };
        if name.is_empty() || !name.contains(needle) {
            continue;
        }
        out.push((i, class_full_name(p)));
        if out.len() >= limit {
            break;
        }
    }
    out
}

pub struct Field {
    pub name: String,
    pub ty: usize,
    pub offset_or_index: u32,
    pub parent: usize,
}

fn field_offset(record: &[u8]) -> Option<u32> {
    let bytes = record.get(FIELD_OFFSET_OFF..FIELD_OFFSET_OFF + size_of::<u32>())?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

/// Fields of a class through the verified Unity 6 `FieldInfo` record.
pub fn fields(class: usize) -> Vec<Field> {
    let mut out = Vec::new();
    let ptr = match safe::read_usize(class + CLASS_FIELDS_OFF) {
        Some(p) if p >= 0x10000 && safe::readable_at(p) => p,
        _ => return out,
    };
    let count = safe::read_u16(class + CLASS_FIELDS_COUNT_OFF).unwrap_or(0) as usize;
    for i in 0..count.min(512) {
        let e = ptr + i * FIELD_STRIDE;
        let ty = safe::read_usize(e + FIELD_TYPE_OFF).unwrap_or(0);
        let name = safe::read_usize(e + FIELD_NAME_OFF)
            .and_then(name_at)
            .unwrap_or_default();
        let off = safe::read_bytes(e, FIELD_STRIDE)
            .as_deref()
            .and_then(field_offset)
            .unwrap_or(0);
        let parent = safe::read_usize(e + FIELD_PARENT_OFF).unwrap_or(0);
        out.push(Field {
            name,
            ty,
            offset_or_index: off,
            parent,
        });
    }
    out
}

pub struct Method {
    pub name: String,
    pub entry: usize,
    pub info: usize,
    /// Il2CppType.type from the return descriptor (1 = IL2CPP_TYPE_VOID).
    pub return_kind: u32,
    pub parameters_count: u8,
    /// Mono method attributes; 0x10 = METHOD_ATTRIBUTE_STATIC.
    pub flags: u16,
}

/// Methods of a class (`class+0x50` is an array of `MethodInfo*`).
pub fn methods(class: usize) -> Vec<Method> {
    let mut out = Vec::new();
    let arr = match safe::read_usize(class + CLASS_METHODS_OFF) {
        Some(p) if p >= 0x10000 && safe::readable_at(p) => p,
        _ => return out,
    };
    for i in 0..4096 {
        let mi = match safe::read_usize(arr + i * 8) {
            Some(p) => p,
            None => break,
        };
        if mi < 0x10000 || !safe::readable_at(mi) {
            break;
        }
        let name = safe::read_usize(mi + METHOD_NAME_OFF)
            .and_then(name_at)
            .unwrap_or_default();
        if name.is_empty() {
            break;
        }
        let entry = safe::read_usize(mi).unwrap_or(0);
        let return_kind = method_return_kind_with(mi, safe::read_usize, safe::read_u32);
        let parameters_count = safe::read_u8(mi + 0x52).unwrap_or(u8::MAX);
        let flags = safe::read_u16(mi + 0x4c).unwrap_or(u16::MAX);
        out.push(Method {
            name,
            entry,
            info: mi,
            return_kind,
            parameters_count,
            flags,
        });
    }
    out
}

/// `Il2CppImage*` of an assembly. Images are packed with stride 0x48 and the
/// `il2cpp_get_corlib` result is *not* the first one (Assembly-CSharp sits four
/// entries earlier), so the search walks both directions. Exact short names win
/// over prefixes (`Assembly-CSharp` must not resolve to `Assembly-CSharp-firstpass`).
pub fn image_of(corlib_image: usize, assembly: &str) -> Option<usize> {
    let want = assembly.to_ascii_lowercase();
    let scan = |exact: bool| -> Option<usize> {
        for step in 0i64..768 {
            for sign in [1i64, -1] {
                let idx = step * sign;
                let Some(img) = corlib_image.checked_add_signed((idx * 0x48) as isize) else {
                    continue;
                };
                let name = match safe::read_usize(img + 0x08).and_then(name_at) {
                    Some(n) if !n.is_empty() => n,
                    _ => continue,
                };
                let low = name.to_ascii_lowercase();
                let hit = if exact {
                    low == want
                } else {
                    low.starts_with(&want)
                };
                if hit {
                    return Some(img);
                }
            }
        }
        None
    };
    scan(true).or_else(|| scan(false))
}

#[cfg(test)]
mod tests {
    use super::{FIELD_STRIDE, field_offset, method_return_kind_with};

    #[test]
    fn return_type_uses_slot_0x20_not_class_slot_0x28() {
        const METHOD_INFO: usize = 0x1000;
        const RETURN_TYPE: usize = 0x6fff_f8c6_2f60;
        const CLASS: usize = 0x3034_4d10;
        let kind = method_return_kind_with(
            METHOD_INFO,
            |address| match address {
                x if x == METHOD_INFO + 0x20 => Some(RETURN_TYPE),
                x if x == METHOD_INFO + 0x28 => Some(CLASS),
                _ => None,
            },
            |address| (address == RETURN_TYPE + 0x08).then_some(0x8001_0000),
        );
        assert_eq!(kind, 1, "LateUpdate must be accepted as void");
    }

    #[test]
    fn field_offset_comes_before_name_and_token_comes_after_it() {
        let mut record = [0u8; FIELD_STRIDE];
        record[0x08..0x0c].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        record[0x18..0x1c].copy_from_slice(&0x0400_002au32.to_le_bytes());
        assert_eq!(field_offset(&record), Some(0x1234_5678));
        assert_eq!(field_offset(&record[..0x0b]), None);
    }
}
