//! Bounds-checked parser for decrypted `global-metadata.dat` versions 29–31.
//!
//! Layout source: Il2CppDumper `Il2CppDumper/Il2Cpp/MetadataClass.cs`, fields
//! gated for Unity metadata v29+. `Meta` owns one immutable snapshot, so enum
//! operations never race the game heap and `dump()` is byte-for-byte identical
//! to what was parsed. Offsets are comments in each definition below.

use crate::procs;
use std::sync::{Arc, Mutex};

pub const MAGIC: u32 = 0xFAB1_1BAF;
pub const HEADER_SIZE: usize = 64 * 4;
pub const TYPE_DEF_SIZE: usize = 88;
pub const IMAGE_DEF_SIZE: usize = 32;
pub const ASSEMBLY_DEF_SIZE: usize = 80;
pub const FIELD_DEF_SIZE: usize = 12;
pub const PROPERTY_DEF_SIZE: usize = 20;
pub const MAX_METADATA_SIZE: usize = 128 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub version: i32,
    pub string_literal: (u32, i32),
    pub string_literal_data: (u32, i32),
    pub strings: (u32, i32),
    pub events: (u32, i32),
    pub properties: (u32, i32),
    pub methods: (u32, i32),
    pub fields: (u32, i32),
    pub type_definitions: (u32, i32),
    pub images: (u32, i32),
    pub assemblies: (u32, i32),
    /// Smallest range containing every header section present in this version.
    pub block_size: usize,
}

impl Header {
    pub fn method_def_size(&self) -> usize {
        if self.version >= 31 { 36 } else { 32 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StringLiteral {
    pub length: u32,
    pub data_index: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageDef {
    pub index: usize,
    pub name_index: u32,
    pub assembly_index: i32,
    pub type_start: i32,
    pub type_count: u32,
    pub entry_point_index: i32,
    pub token: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssemblyDef {
    pub index: usize,
    pub image_index: i32,
    pub token: u32,
    pub referenced_start: i32,
    pub referenced_count: i32,
    pub name_index: u32,
    pub major: i32,
    pub minor: i32,
    pub build: i32,
    pub revision: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypeDef {
    pub index: usize,
    pub name_index: u32,
    pub namespace_index: u32,
    pub byval_type_index: i32,
    pub declaring_type_index: i32,
    pub parent_index: i32,
    pub element_type_index: i32,
    pub generic_container_index: i32,
    pub flags: u32,
    pub field_start: i32,
    pub method_start: i32,
    pub property_start: i32,
    pub field_count: u16,
    pub method_count: u16,
    pub property_count: u16,
    pub token: u32,
    pub bitfield: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodDef {
    pub index: usize,
    pub name_index: u32,
    pub declaring_type: i32,
    pub return_type: i32,
    pub return_parameter_token: Option<u32>,
    pub parameter_start: i32,
    pub generic_container_index: i32,
    pub token: u32,
    pub flags: u16,
    pub iflags: u16,
    pub slot: u16,
    pub parameter_count: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldDef {
    pub index: usize,
    pub name_index: u32,
    pub type_index: i32,
    pub token: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PropertyDef {
    pub index: usize,
    pub name_index: u32,
    pub get: i32,
    pub set: i32,
    pub attrs: u32,
    pub token: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Counts {
    pub types: usize,
    pub methods: usize,
    pub fields: usize,
    pub properties: usize,
    pub images: usize,
    pub assemblies: usize,
}

#[derive(Debug)]
pub struct Meta {
    data: Arc<Vec<u8>>,
    header: Header,
}

impl Meta {
    pub fn from_bytes(data: Vec<u8>) -> Result<Self, String> {
        let header = parse_header(&data)?;
        if data.len() < header.block_size {
            return Err(format!(
                "short metadata: {} bytes, sections need {}",
                data.len(), header.block_size
            ));
        }
        Ok(Self { data: Arc::new(data), header })
    }

    pub fn header(&self) -> &Header { &self.header }
    pub fn bytes(&self) -> &[u8] { self.data.as_slice() }
    pub fn dump(&self) -> Vec<u8> { self.data.as_ref()[..self.header.block_size].to_vec() }

    /// Locate and cache the first structurally valid v29–31 block. Failures are
    /// not cached: an early call during `il2cpp_init` may legitimately be too soon.
    pub fn locate() -> Result<Arc<Self>, String> {
        static CACHE: Mutex<Option<Arc<Meta>>> = Mutex::new(None);
        if let Some(m) = match CACHE.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        }.clone() {
            return Ok(m);
        }
        let found = locate_process()?;
        let arc = Arc::new(found);
        let mut cache = match CACHE.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if let Some(m) = cache.clone() { return Ok(m); }
        *cache = Some(arc.clone());
        Ok(arc)
    }

    fn i32_at(&self, off: usize) -> Result<i32, String> {
        let b = self.data.get(off..off + 4).ok_or_else(|| format!("i32 outside metadata at {off:#x}"))?;
        Ok(i32::from_le_bytes(b.try_into().map_err(|_| "4-byte conversion")?))
    }

    fn u16_at(&self, off: usize) -> Result<u16, String> {
        let b = self.data.get(off..off + 2).ok_or_else(|| format!("u16 outside metadata at {off:#x}"))?;
        Ok(u16::from_le_bytes(b.try_into().map_err(|_| "2-byte conversion")?))
    }

    fn table(&self, section: (u32, i32), stride: usize, what: &str) -> Result<(usize, usize), String> {
        let (off, size) = section;
        if size == 0 {
            return Ok((0, 0));
        }
        if size < 0 || stride == 0 || size as usize % stride != 0 {
            return Err(format!("{what}: invalid table offset={off:#x}, size={size:#x}, stride={stride}"));
        }
        let start = off as usize;
        let end = start.checked_add(size as usize).ok_or_else(|| format!("{what}: overflow"))?;
        if start < HEADER_SIZE || end > self.header.block_size || end > self.data.len() {
            return Err(format!("{what}: table {start:#x}..{end:#x} out of bounds"));
        }
        Ok((start, size as usize / stride))
    }

    fn rows<T>(&self, section: (u32, i32), stride: usize, what: &str,
               mut parse: impl FnMut(usize, usize) -> Result<T, String>) -> Result<Vec<T>, String> {
        let (start, count) = self.table(section, stride, what)?;
        let mut out = Vec::with_capacity(count.min(1_000_000));
        for i in 0..count {
            let off = start.checked_add(i.checked_mul(stride).ok_or_else(|| format!("{what}: row overflow"))?)
                .ok_or_else(|| format!("{what}: offset overflow"))?;
            out.push(parse(i, off)?);
        }
        Ok(out)
    }

    pub fn counts(&self) -> Result<Counts, String> {
        Ok(Counts {
            types: self.table(self.header.type_definitions, TYPE_DEF_SIZE, "types")?.1,
            methods: self.table(self.header.methods, self.header.method_def_size(), "methods")?.1,
            fields: self.table(self.header.fields, FIELD_DEF_SIZE, "fields")?.1,
            properties: self.table(self.header.properties, PROPERTY_DEF_SIZE, "properties")?.1,
            images: self.table(self.header.images, IMAGE_DEF_SIZE, "images")?.1,
            assemblies: self.table(self.header.assemblies, ASSEMBLY_DEF_SIZE, "assemblies")?.1,
        })
    }

    /// A metadata string is an offset inside the string blob, not a file offset.
    pub fn string(&self, index: u32) -> Result<String, String> {
        let start = self.header.strings.0 as usize + index as usize;
        if start < self.header.strings.0 as usize ||
            start >= self.header.strings.0 as usize + self.header.strings.1 as usize {
            return Err(format!("string index {index:#x} outside blob"));
        }
        let rest = self.data.get(start..self.header.strings.0 as usize + self.header.strings.1 as usize)
            .ok_or_else(|| "string blob bounds".to_string())?;
        let nul = rest.iter().position(|b| *b == 0).ok_or_else(|| format!("unterminated string {index:#x}"))?;
        std::str::from_utf8(&rest[..nul]).map(str::to_owned)
            .map_err(|e| format!("string {index:#x} is not UTF-8: {e}"))
    }

    pub fn string_literals(&self) -> Result<Vec<StringLiteral>, String> {
        self.rows(self.header.string_literal, 8, "string literals", |_index, o| Ok(StringLiteral {
            length: self.i32_at(o)? as u32,
            data_index: self.i32_at(o + 4)?,
        }))
    }

    pub fn literal(&self, literal: &StringLiteral) -> Result<String, String> {
        if literal.data_index < 0 {
            return Err("negative literal data index".into());
        }
        let base = self.header.string_literal_data.0 as usize;
        let end_base = base.checked_add(self.header.string_literal_data.1 as usize)
            .ok_or_else(|| "literal blob overflow".to_string())?;
        let start = base.checked_add(literal.data_index as usize)
            .ok_or_else(|| "literal offset overflow".to_string())?;
        let end = start.checked_add(literal.length as usize)
            .ok_or_else(|| "literal length overflow".to_string())?;
        if start < base || end > end_base || end > self.data.len() {
            return Err("literal range outside blob".into());
        }
        std::str::from_utf8(&self.data[start..end]).map(str::to_owned)
            .map_err(|e| format!("literal is not UTF-8: {e}"))
    }

    pub fn type_name(&self, t: &TypeDef) -> Result<String, String> {
        Ok(format!(
            "{}.{}",
            self.string(t.namespace_index)?,
            self.string(t.name_index)?
        ).trim_start_matches('.').to_string())
    }

    pub fn image_name(&self, i: &ImageDef) -> Result<String, String> { self.string(i.name_index) }
    pub fn assembly_name(&self, a: &AssemblyDef) -> Result<String, String> { self.string(a.name_index) }
    pub fn method_name(&self, m: &MethodDef) -> Result<String, String> { self.string(m.name_index) }
    pub fn field_name(&self, f: &FieldDef) -> Result<String, String> { self.string(f.name_index) }

    pub fn types(&self) -> Result<Vec<TypeDef>, String> {
        self.rows(self.header.type_definitions, TYPE_DEF_SIZE, "types", |index, o| Ok(TypeDef {
            index,
            name_index: self.i32_at(o)? as u32,
            namespace_index: self.i32_at(o + 4)? as u32,
            byval_type_index: self.i32_at(o + 8)?,
            declaring_type_index: self.i32_at(o + 12)?,
            parent_index: self.i32_at(o + 16)?,
            element_type_index: self.i32_at(o + 20)?,
            generic_container_index: self.i32_at(o + 24)?,
            flags: self.i32_at(o + 28)? as u32,
            field_start: self.i32_at(o + 32)?,
            method_start: self.i32_at(o + 36)?,
            property_start: self.i32_at(o + 44)?,
            field_count: self.u16_at(o + 68)?,
            method_count: self.u16_at(o + 64)?,
            property_count: self.u16_at(o + 66)?,
            token: self.i32_at(o + 84)? as u32,
            bitfield: self.i32_at(o + 80)? as u32,
        }))
    }

    pub fn methods(&self) -> Result<Vec<MethodDef>, String> {
        let v31 = self.header.version >= 31;
        self.rows(self.header.methods, self.header.method_def_size(), "methods", |index, o| Ok(MethodDef {
            index,
            name_index: self.i32_at(o)? as u32,
            declaring_type: self.i32_at(o + 4)?,
            return_type: self.i32_at(o + 8)?,
            return_parameter_token: if v31 { Some(self.i32_at(o + 12)? as u32) } else { None },
            parameter_start: self.i32_at(o + if v31 { 16 } else { 12 })?,
            generic_container_index: self.i32_at(o + if v31 { 20 } else { 16 })?,
            token: self.i32_at(o + if v31 { 24 } else { 20 })? as u32,
            flags: self.u16_at(o + if v31 { 28 } else { 24 })?,
            iflags: self.u16_at(o + if v31 { 30 } else { 26 })?,
            slot: self.u16_at(o + if v31 { 32 } else { 28 })?,
            parameter_count: self.u16_at(o + if v31 { 34 } else { 30 })?,
        }))
    }

    pub fn fields(&self) -> Result<Vec<FieldDef>, String> {
        self.rows(self.header.fields, FIELD_DEF_SIZE, "fields", |index, o| Ok(FieldDef {
            index,
            name_index: self.i32_at(o)? as u32,
            type_index: self.i32_at(o + 4)?,
            token: self.i32_at(o + 8)? as u32,
        }))
    }

    pub fn properties(&self) -> Result<Vec<PropertyDef>, String> {
        self.rows(self.header.properties, PROPERTY_DEF_SIZE, "properties", |index, o| Ok(PropertyDef {
            index,
            name_index: self.i32_at(o)? as u32,
            get: self.i32_at(o + 4)?,
            set: self.i32_at(o + 8)?,
            attrs: self.i32_at(o + 12)? as u32,
            token: self.i32_at(o + 16)? as u32,
        }))
    }

    pub fn images(&self) -> Result<Vec<ImageDef>, String> {
        self.rows(self.header.images, IMAGE_DEF_SIZE, "images", |index, o| Ok(ImageDef {
            index,
            name_index: self.i32_at(o)? as u32,
            assembly_index: self.i32_at(o + 4)?,
            type_start: self.i32_at(o + 8)?,
            type_count: self.i32_at(o + 12)? as u32,
            entry_point_index: self.i32_at(o + 24)?,
            token: self.i32_at(o + 28)? as u32,
        }))
    }

    pub fn assemblies(&self) -> Result<Vec<AssemblyDef>, String> {
        self.rows(self.header.assemblies, ASSEMBLY_DEF_SIZE, "assemblies", |index, o| Ok(AssemblyDef {
            index,
            image_index: self.i32_at(o)?,
            token: self.i32_at(o + 4)? as u32,
            referenced_start: self.i32_at(o + 8)?,
            referenced_count: self.i32_at(o + 12)?,
            name_index: self.i32_at(o + 16)? as u32,
            major: self.i32_at(o + 40)?,
            minor: self.i32_at(o + 44)?,
            build: self.i32_at(o + 48)?,
            revision: self.i32_at(o + 52)?,
        }))
    }

    pub fn r#type(&self, namespace: &str, name: &str) -> Result<Option<TypeDef>, String> {
        for t in self.types()? {
            if self.string(t.namespace_index)? == namespace && self.string(t.name_index)? == name {
                return Ok(Some(t));
            }
        }
        Ok(None)
    }

    pub fn type_image(&self, types: &[TypeDef], images: &[ImageDef], t: &TypeDef) -> Result<ImageDef, String> {
        let _ = types; // signature documents that callers use the same parsed snapshot
        images.iter().find(|i| i.type_start >= 0 &&
            t.index >= i.type_start as usize &&
            (t.index as u64) < i.type_start as u64 + i.type_count as u64)
            .cloned().ok_or_else(|| format!("no image owns type definition {}", t.index))
    }

    /// Member arrays are contiguous ranges selected by the type definition.
    pub fn methods_of(&self, t: &TypeDef) -> Result<Vec<MethodDef>, String> {
        let start = checked_member_range(t.method_start, t.method_count, self.counts()?.methods, "methods")?;
        Ok(self.methods()?.get(start).ok_or_else(|| "method slice bounds".to_string())?.to_vec())
    }

    pub fn fields_of(&self, t: &TypeDef) -> Result<Vec<FieldDef>, String> {
        let start = checked_member_range(t.field_start, t.field_count, self.counts()?.fields, "fields")?;
        Ok(self.fields()?.get(start).ok_or_else(|| "field slice bounds".to_string())?.to_vec())
    }

    pub fn properties_of(&self, t: &TypeDef) -> Result<Vec<PropertyDef>, String> {
        let start = checked_member_range(t.property_start, t.property_count, self.counts()?.properties, "properties")?;
        Ok(self.properties()?.get(start).ok_or_else(|| "property slice bounds".to_string())?.to_vec())
    }

    /// Method-definition interval owned by one image. Metadata emits these as a
    /// contiguous range; `codegen.methodPointers` uses the same relative index.
    pub fn image_method_range(&self, types: &[TypeDef], image: &ImageDef) -> Result<std::ops::Range<usize>, String> {
        let mut lo = usize::MAX;
        let mut hi = 0usize;
        let mut sum = 0usize;
        for t in types {
            if t.index < image.type_start as usize ||
                (t.index as u64) >= image.type_start as u64 + image.type_count as u64 {
                continue;
            }
            if t.method_count == 0 { continue; }
            if t.method_start < 0 { return Err(format!("negative methodStart for type {}", t.index)); }
            lo = lo.min(t.method_start as usize);
            hi = hi.max(t.method_start as usize + t.method_count as usize);
            sum += t.method_count as usize;
        }
        if lo == usize::MAX { return Ok(0..0); }
        if hi - lo != sum {
            return Err(format!("image {} methods are not contiguous: {lo}..{hi}, sum {sum}", image.index));
        }
        if hi > self.counts()?.methods { return Err(format!("image {} method range out of bounds", image.index)); }
        Ok(lo..hi)
    }
}

fn checked_member_range(start: i32, count: u16, total: usize, what: &str) -> Result<std::ops::Range<usize>, String> {
    if count == 0 { return Ok(0..0); }
    if start < 0 { return Err(format!("{what}: negative member start {start}")); }
    let start = start as usize;
    let end = start.checked_add(count as usize).ok_or_else(|| format!("{what}: range overflow"))?;
    if end > total { return Err(format!("{what}: range {start}..{end} exceeds {total}")); }
    Ok(start..end)
}

fn i32(data: &[u8], off: usize) -> Result<i32, String> {
    let b = data.get(off..off + 4).ok_or_else(|| format!("header truncated at {off:#x}"))?;
    Ok(i32::from_le_bytes(b.try_into().map_err(|_| "4-byte conversion")?))
}

fn pair(data: &[u8], index: usize) -> Result<(u32, i32), String> {
    Ok((i32(data, index * 4)? as u32, i32(data, (index + 1) * 4)?))
}

fn parse_header(data: &[u8]) -> Result<Header, String> {
    if data.len() < HEADER_SIZE { return Err("metadata header is shorter than 256 bytes".into()); }
    if i32(data, 0)? as u32 != MAGIC { return Err("metadata magic AF 1B B1 FA not found".into()); }
    let version = i32(data, 4)?;
    if !(29..=31).contains(&version) { return Err(format!("unsupported metadata version {version} (need 29–31)")); }
    let indices = [
        2, 4, 6, 8, 10, 12, 14, 16, 18, 20, 22, 24, 26, 28, 30, 32, 34, 36, 38, 40,
        42, 44, 46, 48, 50, 52, 54, 56, 58, 60, 62,
    ];
    let mut block_size = HEADER_SIZE;
    for index in indices {
        let (off, size) = pair(data, index)?;
        if size < 0 || (off == 0 && size != 0) {
            return Err(format!("invalid header section {index}: {off:#x}/{size:#x}"));
        }
        if size == 0 { continue; }
        if off as usize % 4 != 0 { return Err(format!("misaligned metadata section {index}: {off:#x}")); }
        let end = (off as usize).checked_add(size as usize).ok_or_else(|| "metadata section overflow".to_string())?;
        if end > MAX_METADATA_SIZE { return Err(format!("metadata section {index} ends at {end:#x}, over safety cap")); }
        block_size = block_size.max(end);
    }
    let h = Header {
        version,
        string_literal: pair(data, 2)?,
        string_literal_data: pair(data, 4)?,
        strings: pair(data, 6)?,
        events: pair(data, 8)?,
        properties: pair(data, 10)?,
        methods: pair(data, 12)?,
        fields: pair(data, 24)?,
        type_definitions: pair(data, 40)?,
        images: pair(data, 42)?,
        assemblies: pair(data, 44)?,
        block_size,
    };
    if h.strings.1 <= 0 || h.type_definitions.1 <= 0 || h.images.1 <= 0 || h.assemblies.1 <= 0 {
        return Err("metadata lacks string/type/image/assembly tables".into());
    }
    if h.type_definitions.1 as usize % TYPE_DEF_SIZE != 0 ||
        h.images.1 as usize % IMAGE_DEF_SIZE != 0 ||
        h.assemblies.1 as usize % ASSEMBLY_DEF_SIZE != 0 ||
        h.methods.1 as usize % h.method_def_size() != 0 ||
        h.fields.1 as usize % FIELD_DEF_SIZE != 0 ||
        h.properties.1 as usize % PROPERTY_DEF_SIZE != 0 {
        return Err("metadata table size is not a whole number of records".into());
    }
    Ok(h)
}

fn candidate(at: usize) -> Result<Meta, String> {
    let prefix = crate::safe::read_bytes(at, HEADER_SIZE).ok_or_else(|| "unreadable header".to_string())?;
    let h = parse_header(&prefix)?;
    let data = crate::safe::read_bytes(at, h.block_size)
        .ok_or_else(|| format!("cannot snapshot {:#x} bytes of metadata", h.block_size))?;
    Meta::from_bytes(data)
}

/// Regions are probed largest-first: the real blob is tens of megabytes and this
/// avoids spending a tick reviewing thousands of tiny heap regions first.
fn locate_process() -> Result<Meta, String> {
    let budget = std::env::var("HROMATITE_SCAN_MB").ok().and_then(|v| v.parse().ok())
        .unwrap_or(512usize) * 1024 * 1024;
    let mut regions: Vec<_> = procs::regions(200_000).into_iter()
        .filter(|r| r.readable() && !r.executable() && r.size >= 0x1000).collect();
    regions.sort_by_key(|r| std::cmp::Reverse(r.size));
    let mut spent = 0usize;
    let mut rejected = 0usize;
    for r in regions {
        spent = spent.saturating_add(r.size);
        if spent > budget { break; }
        crate::safe::scan_yield();
        const CHUNK: usize = 64 * 1024;
        let mut off = 0usize;
        while off < r.size {
            let n = CHUNK.min(r.size - off);
            let base = r.base + off;
            let hits = crate::veh::call_with(|| {
                let bytes = unsafe { core::slice::from_raw_parts(base as *const u8, n) };
                let mut hits = Vec::new();
                for i in (0..bytes.len().saturating_sub(3)).step_by(4) {
                    if bytes[i..i + 4] == MAGIC.to_le_bytes() { hits.push(base + i); }
                }
                hits
            }).map_err(|e| format!("metadata scan fault at {base:#x}: {e}"))?;
            for at in hits {
                match candidate(at) {
                    Ok(m) => {
                        crate::log::log_tag("meta", &format!(
                            "decrypted metadata @ {at:#x}: v{}, {} bytes",
                            m.header.version, m.header.block_size
                        ));
                        return Ok(m);
                    }
                    Err(_) => rejected += 1,
                }
            }
            off += n;
        }
    }
    Err(format!("decrypted global-metadata.dat not found ({} rejected candidate(s))", rejected))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_i32(data: &mut [u8], off: usize, value: i32) {
        data[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }
    fn put_u16(data: &mut [u8], off: usize, value: u16) {
        data[off..off + 2].copy_from_slice(&value.to_le_bytes());
    }
    fn section(data: &mut [u8], index: usize, off: usize, size: usize) {
        put_i32(data, index * 4, off as i32);
        put_i32(data, index * 4 + 4, size as i32);
    }

    fn fixture(version: i32) -> Vec<u8> {
        let mut d = vec![0u8; 0xC00];
        put_i32(&mut d, 0, MAGIC as i32);
        put_i32(&mut d, 4, version);
        let strings = b"UnityEngine\0Camera\0get_main\0fieldOfView\0orthographic\0TestAssembly\0";
        d[0x400..0x400 + strings.len()].copy_from_slice(strings);
        section(&mut d, 6, 0x400, strings.len());
        section(&mut d, 12, 0x500, if version >= 31 { 36 } else { 32 });
        section(&mut d, 24, 0x600, 24);
        section(&mut d, 10, 0x700, 20);
        section(&mut d, 40, 0x800, 88);
        section(&mut d, 42, 0x900, 32);
        section(&mut d, 44, 0xA00, 80);
        // Camera: UnityEngine @0, Camera @12, get_main @19, field @28, property @40.
        put_i32(&mut d, 0x800, 12);
        put_i32(&mut d, 0x804, 0);
        put_i32(&mut d, 0x808, 123);
        put_i32(&mut d, 0x80C, -1);
        put_i32(&mut d, 0x810, -1);
        put_i32(&mut d, 0x814, -1);
        put_i32(&mut d, 0x818, -1);
        put_i32(&mut d, 0x81C, 0x2000);
        put_i32(&mut d, 0x820, 0); // fields
        put_i32(&mut d, 0x824, 0); // methods
        put_i32(&mut d, 0x82C, 0); // properties
        put_u16(&mut d, 0x840, 1); // method_count
        put_u16(&mut d, 0x842, 1); // property_count
        put_u16(&mut d, 0x844, 2); // field_count
        put_i32(&mut d, 0x850, 0x02000001);
        put_i32(&mut d, 0x854, 0);
        put_i32(&mut d, 0x500, 19);
        put_i32(&mut d, 0x504, 0);
        put_i32(&mut d, 0x508, 12);
        put_i32(&mut d, 0x500 + if version >= 31 { 16 } else { 12 }, -1);
        put_i32(&mut d, 0x500 + if version >= 31 { 24 } else { 20 }, 0x06000001);
        put_u16(&mut d, 0x500 + if version >= 31 { 28 } else { 24 }, 0x0086);
        put_i32(&mut d, 0x600, 28);
        put_i32(&mut d, 0x604, 50);
        put_i32(&mut d, 0x608, 0x04000001);
        put_i32(&mut d, 0x60C, 40);
        put_i32(&mut d, 0x610, 51);
        put_i32(&mut d, 0x614, 0x04000002);
        put_i32(&mut d, 0x700, 40);
        put_i32(&mut d, 0x704, 0);
        put_i32(&mut d, 0x708, -1);
        put_i32(&mut d, 0x70C, 1);
        put_i32(&mut d, 0x710, 0x17000001);
        put_i32(&mut d, 0x900, 53);
        put_i32(&mut d, 0x904, 0);
        put_i32(&mut d, 0x908, 0);
        put_i32(&mut d, 0x90C, 1);
        put_i32(&mut d, 0x918, -1);
        put_i32(&mut d, 0x91C, 2);
        put_i32(&mut d, 0xA00, 0);
        put_i32(&mut d, 0xA04, 3);
        put_i32(&mut d, 0xA10, 53);
        put_i32(&mut d, 0xA28, 6000);
        put_i32(&mut d, 0xA2C, 0);
        d
    }

    #[test]
    fn parses_v31_catalog() {
        let m = Meta::from_bytes(fixture(31)).unwrap();
        assert_eq!(m.counts().unwrap(), Counts { types: 1, methods: 1, fields: 2, properties: 1, images: 1, assemblies: 1 });
        let t = m.r#type("UnityEngine", "Camera").unwrap().unwrap();
        assert_eq!(m.type_name(&t).unwrap(), "UnityEngine.Camera");
        let methods = m.methods_of(&t).unwrap();
        assert_eq!(m.method_name(&methods[0]).unwrap(), "get_main");
        assert_eq!(methods[0].flags, 0x0086);
        assert_eq!(methods[0].return_parameter_token, Some(0));
        let fields = m.fields_of(&t).unwrap();
        assert_eq!(m.field_name(&fields[0]).unwrap(), "fieldOfView");
        assert_eq!(fields[1].type_index, 51);
        let image = m.type_image(&m.types().unwrap(), &m.images().unwrap(), &t).unwrap();
        assert_eq!(m.image_method_range(&m.types().unwrap(), &image).unwrap(), 0..1);
        assert_eq!(m.image_name(&image).unwrap(), "TestAssembly");
        assert_eq!(m.assemblies().unwrap()[0].major, 6000);
    }

    #[test]
    fn parses_string_literals() {
        let mut d = fixture(31);
        section(&mut d, 2, 0xB00, 8);
        section(&mut d, 4, 0xB40, 5);
        put_i32(&mut d, 0xB00, 5);
        put_i32(&mut d, 0xB04, 0);
        d[0xB40..0xB45].copy_from_slice(b"hello");
        let m = Meta::from_bytes(d).unwrap();
        assert_eq!(m.literal(&m.string_literals().unwrap()[0]).unwrap(), "hello");
    }

    #[test]
    fn parses_v29_and_v30_method_width() {
        for version in [29, 30] {
            let m = Meta::from_bytes(fixture(version)).unwrap();
            let t = m.types().unwrap().remove(0);
            assert_eq!(m.method_name(&m.methods_of(&t).unwrap()[0]).unwrap(), "get_main");
        }
    }

    #[test]
    fn rejects_bad_headers_without_panics() {
        assert!(Meta::from_bytes(Vec::new()).is_err());
        let mut d = fixture(31);
        put_i32(&mut d, 4, 32);
        assert!(Meta::from_bytes(d).unwrap_err().contains("version"));
        let mut d = fixture(31);
        section(&mut d, 40, 0x800, 89);
        assert!(Meta::from_bytes(d).unwrap_err().contains("whole number"));
        let mut d = fixture(31);
        section(&mut d, 6, usize::MAX - 8, 100);
        assert!(Meta::from_bytes(d).is_err());
    }

    #[test]
    fn member_ranges_are_checked() {
        let mut d = fixture(31);
        put_i32(&mut d, 0x824, 1_000);
        let m = Meta::from_bytes(d).unwrap();
        assert!(m.methods_of(&m.types().unwrap()[0]).is_err());
    }
}
