//! Unity 6000.0.x IL2CPP layout measured on the supported GameAssembly build.
//! All foreign reads/calls go through `Host`; the class-table cache is owned by
//! one feature generation and dropped on reload or shutdown.

use crate::host::{Host, parse_addr};
use hromatite_abi::build::{Il2cppAnchors, Layout};
use hromatite_abi::{STATUS_ERROR, STATUS_NOT_READY, STATUS_OK};

pub const CLASS_MAX: usize = 120_000;

/// `Il2CppTypeEnum` values this crate discriminates overloads by.
const TYPE_SZARRAY: u32 = 0x1d;

#[derive(Debug, Default)]
pub struct Cache {
    class_table: usize,
}

pub struct Metadata<'a> {
    pub host: &'a Host,
    cache: &'a mut Cache,
    /// Addresses and struct offsets of the build we are attached to, taken from
    /// `hromatite_abi::build` by the SHA-256 the resident reports.
    api: Il2cppAnchors,
    layout: Layout,
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
    /// Address of the `MethodInfo` record itself, for callers that must look at
    /// the signature (parameter types) instead of guessing from the name.
    pub info: usize,
    pub return_kind: u32,
    pub parameters_count: u8,
    pub flags: u16,
}

impl<'a> Metadata<'a> {
    /// Resolve the per-build numbers; `None` when this build has no descriptor,
    /// which every caller must treat as "metadata unavailable" rather than
    /// guessing offsets.
    pub fn new(host: &'a Host, cache: &'a mut Cache) -> Option<Self> {
        let build = hromatite_abi::build::select(&host.game_sha_hex())?;
        Some(Self { host, cache, api: build.il2cpp, layout: build.layout })
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
        let slot = base.checked_add(self.api.class_table as usize)?;
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
        self.name_at(self.host.read_usize(class.checked_add(self.layout.class_name as usize)?)?)
    }

    pub fn class_namespace(&self, class: usize) -> Option<String> {
        self.name_at(self.host.read_usize(class.checked_add(self.layout.class_namespace as usize)?)?)
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
        let parent = self.host.read_usize(class.checked_add(self.layout.class_parent as usize)?)?;
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
        let pointer = match self.host.read_usize(class + self.layout.class_fields as usize) {
            Some(pointer) if pointer >= 0x10000 && self.host.read_u8(pointer).is_some() => pointer,
            _ => return out,
        };
        let count = self
            .host
            .read_u16(class + self.layout.class_fields_count as usize)
            .unwrap_or(0) as usize;
        for index in 0..count.min(512) {
            let record = pointer + index * self.layout.field_stride as usize;
            let name = self
                .host
                .read_usize(record + self.layout.field_name as usize)
                .and_then(|pointer| self.name_at(pointer))
                .unwrap_or_default();
            let parent = if self.layout.field_parent == 0 {
                0
            } else {
                self.host
                    .read_usize(record + self.layout.field_parent as usize)
                    .unwrap_or(0)
            };
            out.push(Field {
                name,
                ty: self
                    .host
                    .read_usize(record + self.layout.field_type as usize)
                    .unwrap_or(0),
                offset_or_index: self
                    .host
                    .read_u32(record + self.layout.field_offset as usize)
                    .unwrap_or(0),
                parent,
            });
        }
        out
    }

    fn method_return_kind(&self, info: usize) -> u32 {
        self.host
            .read_usize(info + self.layout.method_return_type as usize)
            .and_then(|ty| self.host.read_u32(ty + 0x08))
            .map(|bits| (bits >> 16) & 0xff)
            .unwrap_or(u32::MAX)
    }

    pub fn methods(&self, class: usize) -> Vec<Method> {
        let mut out = Vec::new();
        let array = match self.host.read_usize(class + self.layout.class_methods as usize) {
            Some(array) if array >= 0x10000 && self.host.read_u8(array).is_some() => array,
            _ => return out,
        };
        // The `MethodInfo**` array is not NULL-terminated, so the walk is bounded
        // structurally: a zero/unmapped slot or an empty name ends it.
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
                .read_usize(info + self.layout.method_name as usize)
                .and_then(|pointer| self.name_at(pointer))
                .unwrap_or_default();
            if name.is_empty() {
                break;
            }
            out.push(Method {
                name,
                entry: self.host.read_usize(info).unwrap_or(0),
                info,
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

    /// `Il2CppTypeEnum` of the first parameter, read from the signature.
    ///
    /// Method overloads that only differ in the first parameter type (the two
    /// `Material.SetInt` shapes, `string` versus `int`) are indistinguishable by
    /// name and arity, so the type kind is the only honest discriminator.
    pub fn first_parameter_kind(&self, info: usize) -> Option<u32> {
        self.parameter_kind(info, 0)
    }

    /// `Il2CppTypeEnum` of parameter `index` (zero based), from the signature.
    pub fn parameter_kind(&self, info: usize, index: u8) -> Option<u32> {
        let ty = self.parameter_type(info, index)?;
        self.type_kind(ty)
    }

    /// Address of the `Il2CppType` of parameter `index` (zero based).
    pub fn parameter_type(&self, info: usize, index: u8) -> Option<usize> {
        let parameters = self
            .host
            .read_usize(info.checked_add(self.layout.method_parameters as usize)?)?;
        if parameters < 0x10000 {
            return None;
        }
        self.host
            .read_usize(parameters.checked_add(index as usize * 8)?)
            .filter(|ty| *ty >= 0x10000)
    }

    /// `Il2CppTypeEnum` of a type record: the enum sits in bits 16..24 of the
    /// second dword (`attrs:16, type:8, num_mods:6, byref:1, pinned:1`).
    pub fn type_kind(&self, ty: usize) -> Option<u32> {
        self.host
            .read_u32(ty.checked_add(0x08)?)
            .map(|bits| (bits >> 16) & 0xff)
    }

    /// `Il2CppClass*` for a `const Il2CppType*`, via
    /// `il2cpp_class_from_il2cpp_type`. The type's `data` union does not hold a
    /// class pointer for CLASS/VALUETYPE (it holds a metadata index), so this
    /// call is the only reliable way to turn a signature parameter into a class.
    pub fn class_of_type(&self, ty: usize) -> Option<usize> {
        if self.api.class_from_type == 0 || ty < 0x10000 {
            return None;
        }
        let (base, size) = self.host.game_assembly_range();
        let function = base.checked_add(self.api.class_from_type as usize)?;
        if function.checked_add(8)? > base.checked_add(size)? {
            return None;
        }
        let class = self.host.call(function, &[ty]).ok()?;
        (class >= 0x10000).then_some(class)
    }

    /// `Il2CppClass*` of parameter `index`.
    pub fn parameter_class(&self, info: usize, index: u8) -> Option<usize> {
        self.class_of_type(self.parameter_type(info, index)?)
    }

    /// True when parameter `index` is a managed one-dimensional array (`T[]`).
    pub fn parameter_is_array(&self, info: usize, index: u8) -> bool {
        self.parameter_kind(info, index) == Some(TYPE_SZARRAY)
    }

    /// Element class of parameter `index` when it is `T[]`: an `SZARRAY` type's
    /// `data` points at an `Il2CppArrayType` whose first field is the element's
    /// `Il2CppType`.
    pub fn parameter_array_element_class(&self, info: usize, index: u8) -> Option<usize> {
        let ty = self.parameter_type(info, index)?;
        if self.type_kind(ty)? != TYPE_SZARRAY {
            return None;
        }
        let array_type = self.host.read_usize(ty).filter(|value| *value >= 0x10000)?;
        let element = self
            .host
            .read_usize(array_type)
            .filter(|value| *value >= 0x10000)?;
        self.class_of_type(element)
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
        let getter = base.checked_add(self.api.get_corlib as usize)?;
        if getter.checked_add(8)? > base.checked_add(size)? {
            return None;
        }
        let image = self.host.call(getter, &[]).ok()?;
        (image >= 0x10000 && self.host.read_u8(image).is_some()).then_some(image)
    }

    pub fn image_of(&self, corlib_image: usize, assembly: &str) -> Option<usize> {
        let wanted = assembly.to_ascii_lowercase();
        // The image records are contiguous but their stride is build specific
        // (`0x48` on 648f61b1…, `0x50` on 245768f2…), so walking them with a
        // literal would silently find nothing on the next game update.
        let stride = i64::from(self.layout.image_stride);
        if stride <= 0 {
            return None;
        }
        let name_offset = self.layout.image_name as usize;
        let scan = |exact: bool| -> Option<usize> {
            for step in 0i64..768 {
                for sign in [1i64, -1] {
                    let index = step * sign;
                    let image = corlib_image.checked_add_signed((index * stride) as isize)?;
                    let name = match self
                        .host
                        .read_usize(image + name_offset)
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

    pub fn image_get_class_rva(&self) -> u32 {
        self.api.image_get_class
    }

    /// The build's il2cpp anchors, so a feature resolves each entry point once
    /// instead of re-selecting the descriptor on every call.
    pub fn api_anchors(&self) -> Il2cppAnchors {
        self.api
    }

    /// Corlib image plus the record of another assembly, by short name.
    pub fn image_of_assembly(&self, assembly: &str) -> Option<usize> {
        let corlib = self.find_domain()?;
        self.image_of(corlib, assembly)
    }

    /// `il2cpp_class_from_name(image, namespace, name)`: one call, and the only
    /// way to reach a class the definition table has not materialised without
    /// walking (and thereby materialising) the whole image.
    pub fn class_by_name(&self, image: usize, namespace: &str, name: &str) -> Option<usize> {
        let (base, size) = self.host.game_assembly_range();
        if self.api.class_from_name == 0 {
            return None;
        }
        let function = base.checked_add(self.api.class_from_name as usize)?;
        if function.checked_add(8)? > base.checked_add(size)? {
            return None;
        }
        let namespace = std::ffi::CString::new(namespace).ok()?;
        let name = std::ffi::CString::new(name).ok()?;
        let class = self
            .host
            .call(
                function,
                &[image, namespace.as_ptr() as usize, name.as_ptr() as usize],
            )
            .ok()?;
        (class >= 0x10000).then_some(class)
    }

    fn image_get_class(&self) -> Option<usize> {
        let (base, size) = self.host.game_assembly_range();
        let function = base.checked_add(self.api.image_get_class as usize)?;
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
                        metadata.image_get_class_rva()
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
