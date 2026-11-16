//! Unity 6000.0.x IL2CPP layout measured on the supported GameAssembly build.
//! All foreign reads/calls go through `Host`; the class-table cache is owned by
//! one feature generation and dropped on reload or shutdown.

use crate::host::Host;
use crate::memory::parse_addr;
use hromatite_abi::{STATUS_ERROR, STATUS_NOT_READY, STATUS_OK};

pub const CLASS_TABLE_RVA: u32 = 0x0e66_c870;
pub const IMAGE_GET_CLASS_RVA: u32 = 0x00c2_a330;
pub const CORLIB_IMAGE_GETTER_RVA: u32 = 0x00c2_7f00;
pub const CLASS_MAX: usize = 120_000;

const CLASS_PARENT_OFF: usize = 0x10;
const CLASS_NS_OFF: usize = 0x18;
const CLASS_METHODS_OFF: usize = 0x50;
const CLASS_FIELDS_OFF: usize = 0x80;
const CLASS_NAME_OFF: usize = 0x98;
const CLASS_FIELDS_COUNT_OFF: usize = 0x124;
const FIELD_STRIDE: usize = 0x28;
const METHOD_NAME_OFF: usize = 0x18;
const METHOD_RETURN_TYPE_OFF: usize = 0x20;

#[derive(Debug, Default)]
pub struct Cache {
    class_table: usize,
}

pub struct Metadata<'a> {
    pub host: &'a Host,
    cache: &'a mut Cache,
}

pub struct Field {
    pub name: String,
    pub ty: usize,
    pub offset_or_index: u32,
    pub parent: usize,
}

pub struct Method {
    pub name: String,
    pub entry: usize,
    pub return_kind: u32,
    pub parameters_count: u8,
    pub flags: u16,
}

impl<'a> Metadata<'a> {
    pub fn new(host: &'a Host, cache: &'a mut Cache) -> Self {
        Self { host, cache }
    }

    fn name_at(&self, pointer: usize) -> Option<String> {
        if pointer < 0x10000 {
            return Some(String::new());
        }
        self.host.ascii_at(pointer, 256)
    }

    pub fn class_table(&mut self) -> Option<usize> {
        if self.cache.class_table != 0 {
            return Some(self.cache.class_table);
        }
        let (base, size) = self.host.game_assembly_range();
        let slot = base.checked_add(CLASS_TABLE_RVA as usize)?;
        if slot.checked_add(8)? > base.checked_add(size)? {
            return None;
        }
        let table = self.host.read_usize(slot)?;
        if table < 0x10000 || self.host.read_u8(table).is_none() {
            return None;
        }
        self.cache.class_table = table;
        Some(table)
    }

    pub fn class_name(&self, class: usize) -> Option<String> {
        self.name_at(self.host.read_usize(class.checked_add(CLASS_NAME_OFF)?)?)
    }

    pub fn class_namespace(&self, class: usize) -> Option<String> {
        self.name_at(self.host.read_usize(class.checked_add(CLASS_NS_OFF)?)?)
    }

    pub fn class_full_name(&self, class: usize) -> String {
        let name = self.class_name(class).unwrap_or_default();
        let namespace = self.class_namespace(class).unwrap_or_default();
        if namespace.is_empty() {
            name
        } else {
            format!("{namespace}.{name}")
        }
    }

    pub fn classes(&mut self, max: usize) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let Some(table) = self.class_table() else {
            return out;
        };
        for index in 0..max {
            let Some(slot) = index
                .checked_mul(8)
                .and_then(|offset| table.checked_add(offset))
            else {
                break;
            };
            let class = match self.host.read_usize(slot) {
                Some(class) => class,
                None => break,
            };
            if class < 0x10000 || self.host.read_u8(class).is_none() {
                continue;
            }
            out.push((index, class));
        }
        out
    }

    pub fn find(&mut self, namespace: &str, name: &str, max: usize) -> Vec<(usize, usize)> {
        let mut hits = Vec::new();
        for (index, class) in self.classes(max) {
            if self
                .class_name(class)
                .is_none_or(|class_name| class_name != name && !name.is_empty())
            {
                continue;
            }
            let class_namespace = self.class_namespace(class).unwrap_or_default();
            if !namespace.is_empty() && class_namespace != namespace {
                continue;
            }
            hits.push((index, class));
            if hits.len() >= 64 {
                break;
            }
        }
        hits
    }

    pub fn parent(&self, class: usize) -> Option<usize> {
        let parent = self.host.read_usize(class.checked_add(CLASS_PARENT_OFF)?)?;
        (parent >= 0x10000 && self.host.read_u8(parent).is_some()).then_some(parent)
    }

    pub fn chain(&self, class: usize) -> Vec<usize> {
        let mut out = vec![class];
        let mut current = class;
        for _ in 0..32 {
            match self.parent(current) {
                Some(parent) if !out.contains(&parent) => {
                    out.push(parent);
                    current = parent;
                }
                _ => break,
            }
        }
        out
    }

    pub fn fields(&self, class: usize) -> Vec<Field> {
        let mut out = Vec::new();
        let pointer = match self.host.read_usize(class + CLASS_FIELDS_OFF) {
            Some(pointer) if pointer >= 0x10000 && self.host.read_u8(pointer).is_some() => pointer,
            _ => return out,
        };
        let count = self
            .host
            .read_u16(class + CLASS_FIELDS_COUNT_OFF)
            .unwrap_or(0) as usize;
        for index in 0..count.min(512) {
            let record = pointer + index * FIELD_STRIDE;
            let name = self
                .host
                .read_usize(record + 0x10)
                .and_then(|pointer| self.name_at(pointer))
                .unwrap_or_default();
            out.push(Field {
                name,
                ty: self.host.read_usize(record).unwrap_or(0),
                offset_or_index: self.host.read_u32(record + 0x08).unwrap_or(0),
                parent: self.host.read_usize(record + 0x20).unwrap_or(0),
            });
        }
        out
    }

    fn method_return_kind(&self, info: usize) -> u32 {
        self.host
            .read_usize(info + METHOD_RETURN_TYPE_OFF)
            .and_then(|ty| self.host.read_u32(ty + 0x08))
            .map(|bits| (bits >> 16) & 0xff)
            .unwrap_or(u32::MAX)
    }

    pub fn methods(&self, class: usize) -> Vec<Method> {
        let mut out = Vec::new();
        let array = match self.host.read_usize(class + CLASS_METHODS_OFF) {
            Some(array) if array >= 0x10000 && self.host.read_u8(array).is_some() => array,
            _ => return out,
        };
        for index in 0..4096 {
            let info = match self.host.read_usize(array + index * 8) {
                Some(info) => info,
                None => break,
            };
            if info < 0x10000 || self.host.read_u8(info).is_none() {
                break;
            }
            let name = self
                .host
                .read_usize(info + METHOD_NAME_OFF)
                .and_then(|pointer| self.name_at(pointer))
                .unwrap_or_default();
            if name.is_empty() {
                break;
            }
            out.push(Method {
                name,
                entry: self.host.read_usize(info).unwrap_or(0),
                return_kind: self.method_return_kind(info),
                parameters_count: self.host.read_u8(info + 0x52).unwrap_or(u8::MAX),
                flags: self.host.read_u16(info + 0x4c).unwrap_or(u16::MAX),
            });
        }
        out
    }

    pub fn method_entry(methods: &[Method], name: &str) -> usize {
        methods
            .iter()
            .find(|method| method.name == name)
            .map_or(0, |method| method.entry)
    }

    pub fn method_in_chain(&self, class: usize, name: &str) -> Option<(usize, usize)> {
        for current in self.chain(class) {
            if let Some(method) = self.methods(current).into_iter().find(|m| m.name == name) {
                return Some((current, method.entry));
            }
        }
        None
    }

    pub fn find_domain(&self) -> Option<usize> {
        let (base, size) = self.host.game_assembly_range();
        let getter = base.checked_add(CORLIB_IMAGE_GETTER_RVA as usize)?;
        if getter.checked_add(8)? > base.checked_add(size)? {
            return None;
        }
        let image = self.host.call(getter, &[]).ok()?;
        (image >= 0x10000 && self.host.read_u8(image).is_some()).then_some(image)
    }

    pub fn image_of(&self, corlib_image: usize, assembly: &str) -> Option<usize> {
        let wanted = assembly.to_ascii_lowercase();
        let scan = |exact: bool| -> Option<usize> {
            for step in 0i64..768 {
                for sign in [1i64, -1] {
                    let index = step * sign;
                    let image = corlib_image.checked_add_signed((index * 0x48) as isize)?;
                    let name = match self
                        .host
                        .read_usize(image + 0x08)
                        .and_then(|pointer| self.name_at(pointer))
                    {
                        Some(name) if !name.is_empty() => name,
                        _ => continue,
                    };
                    let name = name.to_ascii_lowercase();
                    if (exact && name == wanted) || (!exact && name.starts_with(&wanted)) {
                        return Some(image);
                    }
                }
            }
            None
        };
        scan(true).or_else(|| scan(false))
    }

    fn image_get_class(&self) -> Option<usize> {
        let (base, size) = self.host.game_assembly_range();
        let function = base.checked_add(IMAGE_GET_CLASS_RVA as usize)?;
        (function.checked_add(8)? <= base.checked_add(size)?).then_some(function)
    }

    /// Find an already materialised class, or populate its assembly through the
    /// measured `il2cpp_image_get_class(image, index)` export.
    pub fn class_or_materialise(
        &mut self,
        namespace: &str,
        name: &str,
        assembly: &str,
    ) -> Option<usize> {
        if let Some((_, class)) = self.find(namespace, name, CLASS_MAX).first() {
            return Some(*class);
        }
        let function = self.image_get_class()?;
        let first = self.find_domain()?;
        let image = self.image_of(first, assembly)?;
        for index in 0..40_000usize {
            let class = self.host.call(function, &[image, index]).ok()?;
            if class < 0x10000 || self.host.read_u8(class).is_none() {
                continue;
            }
            if self.class_name(class).as_deref() == Some(name)
                && self.class_namespace(class).as_deref() == Some(namespace)
            {
                return Some(class);
            }
        }
        None
    }
}

pub fn dispatch(
    metadata: &mut Metadata<'_>,
    command: &str,
    a1: &str,
    a2: &str,
    rest: &str,
) -> (Vec<String>, i32) {
    match command {
        "m3f" => {
            let hits = metadata.find(a1, a2, CLASS_MAX);
            let mut out = vec![format!(
                "m3f {a1:?} {a2:?}: {} class(es), table {:#x}",
                hits.len(),
                metadata.class_table().unwrap_or(0)
            )];
            for (index, class) in hits.iter().take(32) {
                out.push(format!(
                    "  #{index} {class:#x}  {}",
                    metadata.class_full_name(*class)
                ));
            }
            (out, STATUS_OK)
        }
        "m3s" => {
            let wanted = a1.to_ascii_lowercase();
            let mut out = vec![format!("m3s {wanted:?}")];
            let mut count = 0usize;
            for (index, class) in metadata.classes(CLASS_MAX) {
                let name = metadata.class_name(class).unwrap_or_default();
                if wanted.is_empty() || name.to_ascii_lowercase().contains(&wanted) {
                    count += 1;
                    if count <= 40 {
                        out.push(format!(
                            "  #{index} {class:#x}  {}",
                            metadata.class_full_name(class)
                        ));
                    }
                }
            }
            out.insert(1, format!("  {count} match(es)"));
            (out, STATUS_OK)
        }
        "m3c" => match parse_addr(a1).filter(|class| *class >= 0x10000) {
            Some(class) => {
                let mut out = vec![format!(
                    "m3c {class:#x}  {}  (image {:#x})",
                    metadata.class_full_name(class),
                    metadata.host.read_usize(class).unwrap_or(0)
                )];
                let fields = metadata.fields(class);
                out.push(format!("  fields: {}", fields.len()));
                for field in fields.iter().take(40) {
                    out.push(format!(
                        "    +{:#06x} {:<28} type {:#x} parent {:#x}",
                        field.offset_or_index, field.name, field.ty, field.parent
                    ));
                }
                let methods = metadata.methods(class);
                out.push(format!("  methods: {}", methods.len()));
                for method in methods.iter().take(60) {
                    out.push(format!(
                        "    {:<36} entry {:#x} return_kind={} params={} flags={:#x}",
                        method.name,
                        method.entry,
                        method.return_kind,
                        method.parameters_count,
                        method.flags
                    ));
                }
                (out, STATUS_OK)
            }
            None => (
                vec!["usage: m3c <classaddr>".into()],
                hromatite_abi::STATUS_BAD_ARGUMENT,
            ),
        },
        "m3i" => match metadata.find_domain() {
            Some(first) => {
                let mut out = vec![format!("m3i: corlib image {first:#x}")];
                let wanted = if a1.is_empty() {
                    "UnityEngine.CoreModule"
                } else {
                    a1
                };
                match metadata.image_of(first, wanted) {
                    Some(image) => out.push(format!(
                        "  {wanted}: image {image:#x}, il2cpp_image_get_class {:#x}",
                        IMAGE_GET_CLASS_RVA
                    )),
                    None => out.push(format!("  {wanted}: not found")),
                }
                (out, STATUS_OK)
            }
            None => (
                vec!["m3i: get_corlib getter not found".into()],
                STATUS_NOT_READY,
            ),
        },
        "m3l" => {
            let assembly = if a1.is_empty() { "Assembly-CSharp" } else { a1 };
            let filter = a2.to_ascii_lowercase();
            let max = parse_addr(rest.trim()).unwrap_or(30_000).min(60_000);
            let Some(function) = metadata.image_get_class() else {
                return (
                    vec!["m3l: image_get_class outside GameAssembly.dll".into()],
                    STATUS_NOT_READY,
                );
            };
            let Some(first) = metadata.find_domain() else {
                return (
                    vec!["m3l: get_corlib getter not found".into()],
                    STATUS_NOT_READY,
                );
            };
            let Some(image) = metadata.image_of(first, assembly) else {
                return (
                    vec![format!("m3l: no image for '{assembly}'")],
                    STATUS_ERROR,
                );
            };
            let mut out = vec![format!(
                "m3l {assembly}: image {image:#x}, fn {function:#x}, filter {filter:?}, max {max}"
            )];
            let mut seen = 0usize;
            let mut shown = 0usize;
            let mut faults = 0usize;
            for index in 0..max {
                let class = match metadata.host.call(function, &[image, index]) {
                    Ok(class) => class,
                    Err(_) => {
                        faults += 1;
                        if faults > 32 {
                            out.push(format!("  stopped at {index}: too many faults"));
                            break;
                        }
                        continue;
                    }
                };
                if class < 0x10000 || metadata.host.read_u8(class).is_none() {
                    continue;
                }
                seen += 1;
                let name = metadata.class_name(class).unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                if filter.is_empty() || name.to_ascii_lowercase().contains(&filter) {
                    shown += 1;
                    if shown <= 60 {
                        out.push(format!(
                            "  #{index} {class:#x}  {}.{name}",
                            metadata.class_namespace(class).unwrap_or_default()
                        ));
                    }
                }
            }
            out.push(format!(
                "  {seen} class(es) seen, {shown} match(es), {faults} fault(s)"
            ));
            (out, STATUS_OK)
        }
        _ => unreachable!("only M3 commands are routed here"),
    }
}
