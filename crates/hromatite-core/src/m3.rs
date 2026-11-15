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
//!            +0x08              → uint32 token
//!            +0x10              → const char* name
//!            +0x18              → uint32 metadata offset/index
//!            +0x20              → Il2CppClass* parent        (stride 0x28)
//! MethodInfo +0x00              → methodPointer
//!            +0x18              → const char* name
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
const FIELD_OFFSET_OFF: usize = 0x18;
const FIELD_PARENT_OFF: usize = 0x20;

const METHOD_NAME_OFF: usize = 0x18;

/// Resolved class table (cached: the pointer is stable for the run).
pub fn class_table() -> Option<usize> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CACHE: AtomicUsize = AtomicUsize::new(0);
    let cached = CACHE.load(Ordering::Relaxed);
    if cached != 0 {
        return Some(cached);
    }
    let (base, size) = procs::own_module(crate::GAME_ASSEMBLY)?;
    let slot = base.checked_add(CLASS_TABLE_RVA as usize)?;
    if slot + 8 > base + size && !safe::readable_at(slot) {
        return None;
    }
    let t = safe::read_usize(slot)?;
    if t < 0x10000 || !safe::readable_at(t) {
        return None;
    }
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
    if ns.is_empty() { n } else { format!("{ns}.{n}") }
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

pub struct Field {
    pub name: String,
    pub ty: usize,
    pub offset_or_index: u32,
    pub parent: usize,
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
        let off = safe::read_u32(e + FIELD_OFFSET_OFF).unwrap_or(0);
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
        out.push(Method { name, entry, info: mi });
    }
    out
}

/// `Il2CppImage*` of an assembly: images are `getter + i*0x48`, names at `+0x08`.
pub fn image_of(corlib_or_first_image: usize, assembly: &str) -> Option<usize> {
    let want = assembly.to_ascii_lowercase();
    for i in 0..512 {
        let img = corlib_or_first_image + i * 0x48;
        let name = match safe::read_usize(img + 0x08).and_then(name_at) {
            Some(n) if !n.is_empty() => n,
            _ => break,
        };
        if name.to_ascii_lowercase() == want
            || name.to_ascii_lowercase().starts_with(&want)
        {
            return Some(img);
        }
    }
    None
}
