//! Unity 6 (6000.0.x) il2cpp layout read straight out of the game's memory.
//!
//! **Every offset and RVA here is per build** and comes from
//! `hromatite_abi::build` (selected by the SHA-256 of the loaded
//! `GameAssembly.dll`). A literal in this file is a bug: the previous build
//! moved the image stride (`0x48` → `0x50`), the class `methods` slot
//! (`0x50` → `0x58`), `parent` (`0x10` → `0xa0`), the `MethodInfo.name` slot
//! (`0x18` → `0x20`) and the `FieldInfo` stride (`0x28` → `0x20`). When the build
//! cannot be identified every accessor returns `None` and the caller must
//! refuse, never guess.
//!
//! ```text
//! 648f61b1…                       245768f2…
//! GameAssembly .data +0xe66c870  → Il2CppClass**  (+0xe66f0d0)
//! Il2CppClass +0x00              → const Il2CppImage*
//!            +0x18              → const char* namespaze
//!            +0x50 (+0x58)      → const MethodInfo**  (array of pointers)
//!            +0x80 (+0x88)      → FieldInfo[]         (inline records)
//!            +0x98              → const char* name
//!            +0x10 (+0xa0)      → Il2CppClass* parent
//!            +0x124             → uint16 fieldsCount
//! MethodInfo +0x00              → methodPointer
//!            +0x18 (+0x20)      → const char* name
//!            +0x20 (+0x28)      → const Il2CppType* return_type
//!            +0x30              → parameters
//!            +0x4c / +0x52      → u16 flags / u8 parameters_count
//! FieldInfo  +0x00              → const char* name        (0x10 → 0x00)
//!            +0x08              → int32 offset
//!            +0x08 (+0x10)      → const Il2CppType* type
//! ```
//!
//! The image array is not the "domain": `il2cpp_get_corlib` returns the *corlib
//! `Il2CppImage`*, and the images of every assembly are laid out back to back
//! (stride `0x48` / `0x50`, short name at `+0x08` on both). `il2cpp_image_get_class`
//! (`0xc2a330` / `0xc2c0a0`), `il2cpp_class_from_name` (`0xc28050` / `0xc29dc0`)
//! and the class table pointer come from the same descriptor. Reading the class
//! table needs no API call at all, which is why this module is safe from the
//! game's own thread.

#![forbid(unsafe_code)]

use core::mem::size_of;

use hromatite_abi::build::{Il2cppAnchors, Layout};

use crate::procs;
use crate::safe;

/// Descriptor of the module this process is attached to.
#[cfg(target_os = "windows")]
pub fn loaded_build() -> Option<&'static hromatite_abi::build::Build> {
    let (base, size) = procs::own_module(crate::GAME_ASSEMBLY)?;
    crate::buildinfo::resolve(base, size)
}

/// Host builds have no module: every accessor below then reports "unavailable".
#[cfg(not(target_os = "windows"))]
pub fn loaded_build() -> Option<&'static hromatite_abi::build::Build> {
    None
}

/// Struct layout of the loaded build.
pub fn layout() -> Option<Layout> {
    loaded_build().map(|build| build.layout)
}

/// il2cpp anchors (RVAs) of the loaded build.
pub fn anchors() -> Option<Il2cppAnchors> {
    loaded_build().map(|build| build.il2cpp)
}

#[cfg(target_os = "windows")]
fn module() -> Option<(usize, usize)> {
    procs::own_module(crate::GAME_ASSEMBLY)
}

#[cfg(not(target_os = "windows"))]
fn module() -> Option<(usize, usize)> {
    None
}

/// Absolute address of an il2cpp export inside the loaded module.
fn function(rva: u32) -> Option<usize> {
    if rva == 0 {
        // Discovery leaves unknown anchors at zero; that is "no evidence", not
        // "the module base".
        return None;
    }
    let (base, size) = module()?;
    let f = base.checked_add(rva as usize)?;
    (f.checked_add(16)? <= base.checked_add(size)?).then_some(f)
}

/// `il2cpp_image_get_class(image, index)` of the loaded build.
pub fn image_get_class_addr() -> Option<usize> {
    function(anchors()?.image_get_class)
}

/// `il2cpp_class_from_name(image, namespace, name)` of the loaded build.
pub fn class_from_name_addr() -> Option<usize> {
    function(anchors()?.class_from_name)
}

fn name_at(ptr: usize) -> Option<String> {
    if ptr < 0x10000 {
        return Some(String::new());
    }
    safe::ascii_at(ptr, 256)
}

/// Resolved class table (cached: the pointer is stable for the run).
pub fn class_table() -> Option<usize> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static CACHE: AtomicUsize = AtomicUsize::new(0);
    let cached = CACHE.load(Ordering::Relaxed);
    if cached != 0 {
        return Some(cached);
    }
    let (base, size) = module()?;
    let slot = base.checked_add(anchors()?.class_table as usize)?;
    if slot + 8 > base + size && !safe::readable_at(slot) {
        return None;
    }
    let table = safe::read_usize(slot)?;
    if table < 0x10000 || !safe::readable_at(table) {
        return None;
    }
    CACHE.store(table, Ordering::Relaxed);
    Some(table)
}

/// `Il2CppClass*` for a `typeDefinitionIndex`.
pub fn class_at(index: usize) -> Option<usize> {
    let table = class_table()?;
    let p = safe::read_usize(table + index * 8)?;
    (p >= 0x10000 && safe::readable_at(p)).then_some(p)
}

/// Parent class (`Il2CppClass.parent`).
pub fn parent(class: usize) -> Option<usize> {
    let off = layout()?.class_parent as usize;
    let p = safe::read_usize(class + off)?;
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

/// Class name (`Il2CppClass.name`).
pub fn class_name(class: usize) -> Option<String> {
    let off = layout()?.class_name as usize;
    name_at(safe::read_usize(class + off)?)
}

/// Class namespace (`Il2CppClass.namespaze`).
pub fn class_namespace(class: usize) -> Option<String> {
    let off = layout()?.class_namespace as usize;
    name_at(safe::read_usize(class + off)?)
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

pub struct Field {
    pub name: String,
    pub ty: usize,
    pub offset_or_index: u32,
    pub parent: usize,
}

/// Read one `FieldInfo` record's `offset` (int32) at the build's slot.
fn field_offset(record: &[u8], offset_slot: usize) -> Option<u32> {
    let bytes = record.get(offset_slot..offset_slot + size_of::<u32>())?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

/// Fields of a class through the build's `FieldInfo` record.
pub fn fields(class: usize) -> Vec<Field> {
    let mut out = Vec::new();
    let Some(shape) = layout() else {
        return out;
    };
    let stride = shape.field_stride as usize;
    let ptr = match safe::read_usize(class + shape.class_fields as usize) {
        Some(p) if p >= 0x10000 && safe::readable_at(p) => p,
        _ => return out,
    };
    let count = safe::read_u16(class + shape.class_fields_count as usize).unwrap_or(0) as usize;
    for i in 0..count.min(512) {
        let e = ptr + i * stride;
        let ty = safe::read_usize(e + shape.field_type as usize).unwrap_or(0);
        let name = safe::read_usize(e + shape.field_name as usize)
            .and_then(name_at)
            .unwrap_or_default();
        let off = safe::read_bytes(e, stride)
            .as_deref()
            .and_then(|record| field_offset(record, shape.field_offset as usize))
            .unwrap_or(0);
        // `field_parent: 0` means the build no longer stores the owning class.
        let parent = if shape.field_parent == 0 {
            0
        } else {
            safe::read_usize(e + shape.field_parent as usize).unwrap_or(0)
        };
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

fn method_return_kind_with<R, U>(mi: usize, return_slot: usize, read_usize: R, read_u32: U) -> u32
where
    R: FnOnce(usize) -> Option<usize>,
    U: FnOnce(usize) -> Option<u32>,
{
    read_usize(mi + return_slot)
        .and_then(|ty| read_u32(ty + 0x08))
        .map(|bits| (bits >> 16) & 0xff)
        .unwrap_or(u32::MAX)
}

/// Methods of a class (`Il2CppClass.methods` is an array of `MethodInfo*`).
pub fn methods(class: usize) -> Vec<Method> {
    let mut out = Vec::new();
    let Some(shape) = layout() else {
        return out;
    };
    let arr = match safe::read_usize(class + shape.class_methods as usize) {
        Some(p) if p >= 0x10000 && safe::readable_at(p) => p,
        _ => return out,
    };
    let name_off = shape.method_name as usize;
    let return_off = shape.method_return_type as usize;
    let flags_off = shape.method_flags as usize;
    let count_off = shape.method_param_count as usize;
    for i in 0..4096 {
        let mi = match safe::read_usize(arr + i * 8) {
            Some(p) => p,
            None => break,
        };
        if mi < 0x10000 || !safe::readable_at(mi) {
            break;
        }
        let name = safe::read_usize(mi + name_off)
            .and_then(name_at)
            .unwrap_or_default();
        if name.is_empty() {
            break;
        }
        let entry = safe::read_usize(mi).unwrap_or(0);
        let return_kind = method_return_kind_with(mi, return_off, safe::read_usize, safe::read_u32);
        let parameters_count = safe::read_u8(mi + count_off).unwrap_or(u8::MAX);
        let flags = safe::read_u16(mi + flags_off).unwrap_or(u16::MAX);
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

/// `Il2CppImage` table layout of the module this process is attached to.
///
/// `None` when the build cannot be identified — the caller must then refuse,
/// never guess an offset.
pub fn image_layout() -> Option<Layout> {
    layout()
}

/// `Il2CppImage*` of an assembly.
///
/// The images are packed contiguously and `il2cpp_get_corlib` returns one of
/// them, not the first: Assembly-CSharp sits several entries earlier on both
/// known builds. **The stride and the name offset are build specific**, so they
/// come from the build descriptor. Exact short names win over prefixes
/// (`Assembly-CSharp` must not resolve to `Assembly-CSharp-firstpass`).
pub fn image_of(corlib_image: usize, assembly: &str) -> Option<usize> {
    let layout = image_layout()?;
    image_of_with(corlib_image, assembly, &layout)
}

/// Same search with an explicit layout (testable without a live module).
pub fn image_of_with(corlib_image: usize, assembly: &str, layout: &Layout) -> Option<usize> {
    let stride = i64::from(layout.image_stride);
    if stride <= 0 {
        return None;
    }
    let name_offset = layout.image_name as usize;
    let want = assembly.to_ascii_lowercase();
    let scan = |exact: bool| -> Option<usize> {
        for step in 0i64..768 {
            for sign in [1i64, -1] {
                let idx = step * sign;
                let Some(img) = corlib_image.checked_add_signed((idx * stride) as isize) else {
                    continue;
                };
                let name = match safe::read_usize(img + name_offset).and_then(name_at) {
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
    use super::{field_offset, method_return_kind_with};

    /// Both builds keep the return descriptor next to the class pointer; the
    /// slots differ by +8, so the accessor takes the slot as an argument.
    #[test]
    fn return_type_reads_the_descriptor_slot_not_the_class_slot() {
        const METHOD_INFO: usize = 0x1000;
        const RETURN_TYPE: usize = 0x6fff_f8c6_2f60;
        const CLASS: usize = 0x3034_4d10;
        for (return_slot, class_slot) in [(0x20usize, 0x28usize), (0x28, 0x30)] {
            let kind = method_return_kind_with(
                METHOD_INFO,
                return_slot,
                |address| match address {
                    x if x == METHOD_INFO + return_slot => Some(RETURN_TYPE),
                    x if x == METHOD_INFO + class_slot => Some(CLASS),
                    _ => None,
                },
                |address| (address == RETURN_TYPE + 0x08).then_some(0x8001_0000),
            );
            assert_eq!(kind, 1, "LateUpdate must be accepted as void");
        }
    }

    /// `FieldInfo.offset` sits at `+0x08` on both builds; the record is 0x28 on
    /// the old one and 0x20 on the new one, so a short slice must be rejected.
    #[test]
    fn field_offset_reads_the_descriptor_slot_and_rejects_short_records() {
        for stride in [0x28usize, 0x20] {
            let mut record = [0u8; 0x28];
            record[0x08..0x0c].copy_from_slice(&0x1234_5678u32.to_le_bytes());
            assert_eq!(field_offset(&record[..stride], 0x08), Some(0x1234_5678));
            assert_eq!(field_offset(&record[..0x0b], 0x08), None);
        }
    }
}
