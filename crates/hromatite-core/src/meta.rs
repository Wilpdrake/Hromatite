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
const METHOD_PTR_OFF: usize = 0x00;
const METHOD_NAME_OFF: usize = 0x10;

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

/// A class pointer is trusted only when its first word is the image we asked
/// for and its name is a real string: heap garbage passes neither test.
pub fn is_class_of(p: usize, image: usize) -> bool {
    if p < 0x10000 || !safe::readable_at(p) {
        return false;
    }
    match safe::read_usize(p + CLASS_IMAGE_OFF) {
        Some(img) if img == image => {}
        _ => return false,
    }
    let name_ptr = match safe::read_usize(p + CLASS_NAME_OFF) {
        Some(v) => v,
        None => return false,
    };
    matches!(safe::ascii_at(name_ptr, 64), Some(s) if !s.is_empty())
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
    let mut ofs = 0x20usize;
    while ofs <= 0x200 {
        let slot = match safe::read_usize(module + ofs) {
            Some(v) => v,
            None => break,
        };
        if slot >= 0x10000 && safe::readable_at(slot) {
            let mut votes = 0usize;
            for i in 0..8 {
                let p = match safe::read_usize(slot + i * 8) {
                    Some(p) => p,
                    None => break,
                };
                if is_class_of(p, image) {
                    votes += 1;
                } else {
                    break;
                }
            }
            if votes >= 4 && best.map(|b| votes > b.2).unwrap_or(true) {
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
        if !is_class_of(p, image) {
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
