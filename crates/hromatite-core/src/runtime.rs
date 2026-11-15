//! Read-only bridge: immutable IL2CPP metadata ↔ live codegen/class objects.
//! No `il2cpp_*` calls (safe from the QPC detour), no new threads.

#![forbid(unsafe_code)]

use crate::metadata::{FieldDef, ImageDef, Meta, MethodDef, TypeDef};
use crate::{meta, procs, safe};
use std::sync::Arc;

/// Stable API catalog facade. It never guesses an identity: `api_symbols.rs`
/// contains only surviving names and independently verified runtime anchors.
pub struct Api;

impl Api {
    pub fn rva(name: &str) -> Option<u32> { crate::api_symbols::rva(name) }

    pub fn address(name: &str) -> Result<usize, String> {
        let rva = Self::rva(name).ok_or_else(|| format!("API identity not verified: {name}"))?;
        let (base, _) = procs::own_module(crate::GAME_ASSEMBLY)
            .ok_or_else(|| "GameAssembly.dll is not loaded".to_string())?;
        Ok(base + rva as usize)
    }
}

#[derive(Clone, Debug)]
pub struct Runtime {
    pub meta: Arc<Meta>,
}

#[derive(Clone, Debug)]
pub struct CodegenModule {
    pub addr: usize,
    pub table: usize,
    pub count: usize,
    pub metadata_registration: usize,
}

#[derive(Clone, Debug)]
pub struct RuntimeClass {
    pub addr: usize,
    pub image_name: String,
}

#[derive(Clone, Copy, Debug)]
struct Il2CppTypeInfo {
    data: usize,
    type_enum: u8,
}

impl Runtime {
    pub fn locate() -> Result<Self, String> { Ok(Self { meta: Meta::locate()? }) }

    /// Find `Il2CppCodeGenModule` in GameAssembly's data by three independent
    /// invariants. Its export/name-address is a `*.dll` C string, method count is
    /// exactly metadata's range and methodPointers leads back into GameAssembly.
    /// Layout is measured live previously: name@0, count@8, pointers@0x10,
    /// metadataRegistration@0x78, codeRegistration@0x80 (Unity 6000.0.x).
    pub fn codegen(&self, image: &ImageDef, method_count: usize) -> Result<CodegenModule, String> {
        let want_name = self.meta.image_name(image)?;
        let (ga_base, ga_size) = procs::own_module(crate::GAME_ASSEMBLY)
            .ok_or_else(|| "GameAssembly.dll is not loaded".to_string())?;
        let ga_end = ga_base.checked_add(ga_size).ok_or_else(|| "GameAssembly range overflow".to_string())?;
        let mut modules = Vec::new();
        for region in procs::regions(200_000) {
            if !region.readable() || region.base >= ga_end || region.base + region.size <= ga_base ||
                region.size < 16 || (region.base + region.size) - region.base > ga_size {
                continue;
            }
            let end = (region.base + region.size).min(ga_end);
            let mut at = (region.base.max(ga_base) + 7) & !7usize;
            let chunk_start = at;
            while at < end {
                let chunk_end = end.min(at + 64 * 1024);
                // One guarded snapshot per 64 KiB: VirtualQuery per qword made
                // this lookup take seconds and monopolised a game frame.
                let bytes = crate::veh::call_with(|| safe::read_bytes(chunk_start, chunk_end - chunk_start))
                    .map_err(|e| format!("codegen read fault at {chunk_start:#x}: {e}"))?;
                let mut hit = None;
                if let Some(bytes) = bytes {
                    for off in (0..bytes.len().saturating_sub(15)).step_by(8) {
                        let count = u32::from_le_bytes(bytes[off + 8..off + 12].try_into()
                            .map_err(|_| "codegen count conversion".to_string())?) as usize;
                        if count == method_count {
                            let name_ptr = usize::from_le_bytes(bytes[off..off + 8].try_into()
                                .map_err(|_| "codegen pointer conversion".to_string())?);
                            if safe::ascii_at(name_ptr, want_name.len() + 1).as_deref() == Some(&want_name) {
                                hit = Some(chunk_start + off);
                                break;
                            }
                        }
                    }
                }
                if let Some(module) = hit {
                    let table = safe::read_usize(module + 0x10)
                        .ok_or_else(|| format!("codegen methodPointers unreadable at {module:#x}"))?;
                    if !(ga_base..ga_end).contains(&table) {
                        return Err(format!("codegen {module:#x} points outside GameAssembly"));
                    }
                    if method_count > 0 && table + method_count * 8 > ga_end {
                        return Err("codegen method table exceeds module bounds".into());
                    }
                    let metadata_registration = safe::read_usize(module + 0x78)
                        .ok_or_else(|| "metadataRegistration unreadable".to_string())?;
                    modules.push(CodegenModule { addr: module, table, count: method_count, metadata_registration });
                }
                at = at.max(chunk_end);
            }
        }
        match modules.len() {
            1 => Ok(modules.remove(0)),
            0 => Err(format!("codegen module not found: {want_name} ({method_count} methods)")),
            n => Err(format!("ambiguous codegen module {want_name}: {n} candidates")),
        }
    }

    pub fn pointer_in(&self, module: &CodegenModule, relative: usize) -> Result<usize, String> {
        if relative >= module.count {
            return Err(format!("method index {relative} exceeds codegen count {}", module.count));
        }
        let slot = module.table.checked_add(relative.checked_mul(8).ok_or_else(|| "method table overflow".to_string())?)
            .ok_or_else(|| "method slot overflow".to_string())?;
        let addr = safe::read_usize(slot).ok_or_else(|| format!("method pointer unreadable at {slot:#x}"))?;
        let (base, size) = procs::own_module(crate::GAME_ASSEMBLY).ok_or_else(|| "no GameAssembly".to_string())?;
        if addr < base || addr >= base + size {
            return Err(format!("method {} points outside GameAssembly: {addr:#x}", relative));
        }
        Ok(addr)
    }

    pub fn method_pointer(&self, image: &ImageDef, range: &std::ops::Range<usize>,
                          method: &MethodDef) -> Result<usize, String> {
        if !range.contains(&method.index) {
            return Err(format!("method {} is outside its image range {}..{}", method.index, range.start, range.end));
        }
        let module = self.codegen(image, range.len())?;
        self.pointer_in(&module, method.index - range.start)
    }

    pub fn method_rva(&self, image: &ImageDef, range: &std::ops::Range<usize>,
                      method: &MethodDef) -> Result<u32, String> {
        let addr = self.method_pointer(image, range, method)?;
        let base = procs::own_module(crate::GAME_ASSEMBLY).map(|v| v.0)
            .ok_or_else(|| "no GameAssembly base".to_string())?;
        u32::try_from(addr - base).map_err(|_| "method RVA does not fit u32".into())
    }

    /// `Il2CppMetadataRegistration`: count/pointer pairs with x64 alignment:
    /// genericClasses@0/8, genericInsts@0x10/0x18, genericMethodTable@0x20/0x28,
    /// types@0x30/0x38 (`il2cpp-metadata.h`). `Il2CppType`: data@0, enum@0x0c.
    fn type_info(&self, registration: usize, index: i32) -> Result<Il2CppTypeInfo, String> {
        if index < 0 { return Err(format!("negative type index {index}")); }
        if registration < 0x10000 { return Err("metadataRegistration is null or unbound".into()); }
        let count = safe::read_u32(registration + 0x30).ok_or_else(|| "typesCount unreadable".to_string())? as usize;
        if index as usize >= count {
            return Err(format!("type index {index} exceeds typesCount {count}"));
        }
        let table = safe::read_usize(registration + 0x38)
            .ok_or_else(|| "Il2CppMetadataRegistration.types unreadable".to_string())?;
        let slot = table.checked_add(index as usize * 8).ok_or_else(|| "type table overflow".to_string())?;
        let info = safe::read_usize(slot).ok_or_else(|| format!("type pointer unreadable at {slot:#x}"))?;
        if info == 0 { return Err(format!("type {index} is null")); }
        Ok(Il2CppTypeInfo {
            data: safe::read_usize(info).ok_or_else(|| format!("Il2CppType.data unreadable at {info:#x}"))?,
            type_enum: safe::read_u8(info + 0x0c).ok_or_else(|| "Il2CppType.type unreadable".to_string())?,
        })
    }

    pub fn rva_in(&self, module: &CodegenModule, relative: usize) -> Result<u32, String> {
        let addr = self.pointer_in(module, relative)?;
        let base = procs::own_module(crate::GAME_ASSEMBLY).map(|v| v.0)
            .ok_or_else(|| "no GameAssembly base".to_string())?;
        u32::try_from(addr - base).map_err(|_| "method RVA does not fit u32".into())
    }

    pub fn class_for(&self, _image: &ImageDef, registration: usize, t: &TypeDef) -> Result<Option<RuntimeClass>, String> {
        let info = self.type_info(registration, t.byval_type_index)?;
        if info.data == 0 { return Ok(None); }
        let class = info.data;
        let image_ptr = match safe::read_usize(class) {
            Some(v) => v,
            None => return Err(format!("class image back-pointer unreadable at {class:#x}")),
        };
        let image_name_ptr = match safe::read_usize(image_ptr) {
            Some(v) => v,
            None => return Ok(None),
        };
        let image_name = match safe::ascii_at(image_name_ptr, 160) {
            Some(v) if v.ends_with(".dll") => v,
            _ => return Ok(None),
        };
        let name = safe::read_usize(class + meta::CLASS_NAME_OFF)
            .and_then(|p| safe::ascii_at(p, 256));
        let namespace = safe::read_usize(class + meta::CLASS_NS_OFF)
            .and_then(|p| safe::ascii_at(p, 256)).unwrap_or_default();
        if name.as_deref() != Some(self.meta.string(t.name_index)?.as_str()) ||
            namespace != self.meta.string(t.namespace_index)? {
            return Err(format!(
                "Il2CppType.data for {} is not its Il2CppClass (got il2cpp type {:#x})",
                self.meta.type_name(t)?, info.type_enum
            ));
        }
        Ok(Some(RuntimeClass { addr: class, image_name }))
    }

    pub fn type_label(&self, registration: usize, index: i32) -> Result<String, String> {
        let info = self.type_info(registration, index)?;
        if let Some(name) = primitive_type_name(info.type_enum) {
            return Ok(name.into());
        }
        let name = safe::read_usize(info.data + meta::CLASS_NAME_OFF)
            .and_then(|p| safe::ascii_at(p, 256));
        let ns = safe::read_usize(info.data + meta::CLASS_NS_OFF)
            .and_then(|p| safe::ascii_at(p, 256)).unwrap_or_default();
        if let Some(name) = name {
            return Ok(if ns.is_empty() { name } else { format!("{ns}.{name}") });
        }
        Ok(format!("type#{index} (il2cpp type {:#x})", info.type_enum))
    }
}

fn primitive_type_name(value: u8) -> Option<&'static str> {
    Some(match value {
        0x01 => "void", 0x02 => "bool", 0x03 => "char", 0x04 => "sbyte", 0x05 => "byte",
        0x06 => "short", 0x07 => "ushort", 0x08 => "int", 0x09 => "uint", 0x0a => "long",
        0x0b => "ulong", 0x0c => "float", 0x0d => "double", 0x0e => "string",
        0x0f => "System.IntPtr(pointer)", 0x10 => "byref", 0x11 => "value-type",
        0x12 => "class", 0x13 => "generic-var", 0x14 => "array", 0x15 => "generic-instance",
        0x16 => "typedref", 0x18 => "System.IntPtr", 0x19 => "System.UIntPtr",
        0x1b => "function-pointer", 0x1c => "object", 0x1d => "szarray", 0x1e => "generic-method-var",
        _ => return None,
    })
}

pub struct TypeDump {
    pub ty: TypeDef,
    pub image: ImageDef,
    pub class: Option<RuntimeClass>,
    pub registration: usize,
    pub methods: Vec<(MethodDef, Option<u32>)>,
    pub fields: Vec<(FieldDef, String, Option<i32>)>,
}

pub fn dump_type(rt: &Runtime, namespace: &str, name: &str) -> Result<TypeDump, String> {
    let ty = rt.meta.r#type(namespace, name)?.ok_or_else(|| format!("type not found: {namespace}.{name}"))?;
    let types = rt.meta.types()?;
    let images = rt.meta.images()?;
    let image = rt.meta.type_image(&types, &images, &ty)?;
    let range = rt.meta.image_method_range(&types, &image)?;
    let module = rt.codegen(&image, range.len())?;
    let class = rt.class_for(&image, module.metadata_registration, &ty)?;
    let methods = rt.meta.methods_of(&ty)?.into_iter()
        .map(|m| {
            let rva = rt.rva_in(&module, m.index - range.start).ok();
            (m, rva)
        }).collect();
    let runtime_fields = class.as_ref().map(|c| meta::fields_of(c.addr)).unwrap_or_default();
    let fields = rt.meta.fields_of(&ty)?.into_iter().map(|f| {
        let field_name = rt.meta.field_name(&f)?;
        let label = rt.type_label(module.metadata_registration, f.type_index)
            .unwrap_or_else(|_| format!("type#{}", f.type_index));
        let offset = runtime_fields.iter().find(|(name, _)| *name == field_name).map(|(_, off)| *off);
        Ok((f, label, offset))
    }).collect::<Result<Vec<_>, String>>()?;
    Ok(TypeDump {
        ty, image, class, registration: module.metadata_registration,
        methods, fields,
    })
}

pub fn split_full_name(value: &str) -> Result<(&str, &str), String> {
    let (ns, name) = value.rsplit_once('.').ok_or_else(|| "expected Namespace.Type".to_string())?;
    if ns.is_empty() || name.is_empty() { return Err("expected Namespace.Type".into()); }
    Ok((ns, name))
}

/// Host console commands. This function only reads memory or a metadata file;
/// `dumpmeta` writes a file only when explicitly requested by that command.
pub fn console(cmd: &str, a1: &str, a2: &str, a3: &str) -> Option<Vec<String>> {
    let out = (|| -> Result<Vec<String>, String> {
        let rt = Runtime::locate()?;
        match cmd {
            "meta" => {
                let counts = rt.meta.counts()?;
                Ok(vec![
                    format!("metadata v{}: {} bytes", rt.meta.header().version, rt.meta.header().block_size),
                    format!("types {}, methods {}, fields {}, properties {}, images {}, assemblies {}",
                            counts.types, counts.methods, counts.fields, counts.properties, counts.images, counts.assemblies),
                    format!("API: {}/{} verified identities, 232 stub RVAs",
                            crate::api_symbols::API_IDENTIFIED_COUNT, crate::api_symbols::API_STUB_COUNT),
                ])
            }
            "typelist" => {
                let filter = a1.to_ascii_lowercase();
                let max = a2.parse::<usize>().unwrap_or(64).min(1024);
                let mut out = Vec::new();
                for ty in rt.meta.types()? {
                    if !filter.is_empty() && !rt.meta.type_name(&ty)?.to_ascii_lowercase().contains(&filter) {
                        continue;
                    }
                    out.push(format!("  #{}  {} (fields {}, methods {})",
                                     ty.index, rt.meta.type_name(&ty)?, ty.field_count, ty.method_count));
                    if out.len() >= max { break; }
                }
                if out.is_empty() { out.push("typelist: no matches".into()); }
                Ok(out)
            }
            "typedump" | "meths" | "fields2" => {
                let (ns, name) = split_full_name(a1)?;
                let dump = dump_type(&rt, ns, name)?;
                let mut out = vec![
                    format!("type #{} {} in {} (def {})", dump.ty.index, rt.meta.type_name(&dump.ty)?,
                            rt.meta.image_name(&dump.image)?, dump.ty.token),
                    match dump.class {
                        Some(c) => format!("class {:#x}, Il2CppImage {}, metadataRegistration {:#x}",
                                           c.addr, c.image_name, dump.registration),
                        None => format!("class not materialized; metadata fields remain available (registration {:#x})", dump.registration),
                    },
                ];
                if cmd != "meths" {
                    out.push(format!("fields: {}", dump.fields.len()));
                    for (field, label, offset) in dump.fields {
                        out.push(format!("  {} : {}  offset={} (def {})",
                                         rt.meta.field_name(&field)?, label,
                                         offset.map(|v| format!("{v:#x}")).unwrap_or_else(|| "?".into()), field.token));
                    }
                }
                if cmd != "fields2" {
                    out.push(format!("methods: {}", dump.methods.len()));
                    for (method, rva) in dump.methods {
                        out.push(format!("  {}  rva={}  token={:#x} flags={:#x} params={}",
                                         rt.meta.method_name(&method)?,
                                         rva.map(|v| format!("{v:#x}")).unwrap_or_else(|| "?".into()),
                                         method.token, method.flags, method.parameter_count));
                    }
                }
                Ok(out)
            }
            "dumpmeta" => {
                if a1.is_empty() { return Err("usage: dumpmeta <Z:\\path\\global-metadata.dat>".into()); }
                let bytes = rt.meta.dump();
                std::fs::write(a1, &bytes).map_err(|e| format!("dumpmeta {a1}: {e}"))?;
                Ok(vec![format!("dumpmeta: {} bytes -> {}", bytes.len(), a1)])
            }
            _ => Err(format!("runtime console: unknown command {cmd}")),
        }
    })();
    let result = match out {
        Ok(v) => v,
        Err(e) => vec![format!("{cmd}: {e} ({a1} {a2} {a3})")],
    };
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_cover_unity_primitives_only() {
        assert_eq!(primitive_type_name(0x0c), Some("float"));
        assert_eq!(primitive_type_name(0x0e), Some("string"));
        assert_eq!(primitive_type_name(0xff), None);
    }

    #[test]
    fn full_names_require_namespace_and_type() {
        assert_eq!(split_full_name("UnityEngine.Camera").unwrap(), ("UnityEngine", "Camera"));
        assert_eq!(split_full_name("A.B.C").unwrap(), ("A.B", "C"));
        assert!(split_full_name("Camera").is_err());
    }
}
