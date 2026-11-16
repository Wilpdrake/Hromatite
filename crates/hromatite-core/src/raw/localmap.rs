//! In-process loading for the controlled, versioned feature-image ABI. No remote
//! process or thread creation is used. This is not a general-purpose Windows
//! loader: all mapping, initialization, API calls and retirement must use the
//! same quiescent thread; features must not create threads.
//!
//! Only PE TLS callbacks are invoked here. The OS loader's static-TLS index and
//! per-thread template allocation are not installed, and future threads receive
//! no `DLL_THREAD_*` notifications. Unwind tables are likewise not registered;
//! foreign initialization must not depend on SEH unwinding. Entry, TLS and ABI
//! entry calls each have an inner VEH boundary so ordinary caught failures can
//! still run explicit cleanup instead of relying on `Drop` across longjmp.
//! VEH contention or exhausted nesting can make that boundary unavailable (the
//! shared guard then runs the call unguarded); an outer longjmp can still skip
//! all cleanup. Foreign side effects and locks are never transactionally undone.
//! Therefore load/retire must not run under loader lock or under another VEH
//! owner, and feature initialization must be minimal and return errors normally.

// The public M1 surface is consumed by the separate hot-reload integration.
#![allow(dead_code)]

use crate::pe::PeImage;

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    let end = offset.checked_add(2).ok_or("PE offset overflow")?;
    let value = bytes.get(offset..end).ok_or("truncated PE integer")?;
    Ok(u16::from_le_bytes(
        value.try_into().map_err(|_| "invalid PE u16")?,
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let end = offset.checked_add(4).ok_or("PE offset overflow")?;
    let value = bytes.get(offset..end).ok_or("truncated PE integer")?;
    Ok(u32::from_le_bytes(
        value.try_into().map_err(|_| "invalid PE u32")?,
    ))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    let end = offset.checked_add(8).ok_or("PE offset overflow")?;
    let value = bytes.get(offset..end).ok_or("truncated PE integer")?;
    Ok(u64::from_le_bytes(
        value.try_into().map_err(|_| "invalid PE u64")?,
    ))
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) -> Result<(), String> {
    let end = offset.checked_add(8).ok_or("PE offset overflow")?;
    let target = bytes
        .get_mut(offset..end)
        .ok_or("relocation target exceeds the image")?;
    target.copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn apply_relocations(bytes: &mut [u8], pe: &PeImage, base: usize) -> Result<usize, String> {
    let (rva, size) = pe.reloc;
    if rva == 0 && size == 0 {
        return Ok(0);
    }
    if rva == 0 || size == 0 {
        return Err("invalid relocation directory".into());
    }
    let mut offset = rva as usize;
    let end = offset
        .checked_add(size as usize)
        .filter(|end| *end <= bytes.len())
        .ok_or("relocation directory exceeds the image")?;
    let delta = (base as u64).wrapping_sub(pe.image_base);
    let mut patched = 0;
    while offset < end {
        let page = read_u32(bytes, offset)? as usize;
        let block_size = read_u32(bytes, offset + 4)? as usize;
        if block_size < 8 || !block_size.is_multiple_of(2) {
            return Err(format!("invalid relocation block size {block_size:#x}"));
        }
        let block_end = offset
            .checked_add(block_size)
            .filter(|block_end| *block_end <= end)
            .ok_or("relocation block exceeds its directory")?;
        for entry in 0..(block_size - 8) / 2 {
            let value = read_u16(bytes, offset + 8 + entry * 2)?;
            let kind = value >> 12;
            if kind == 0 {
                continue;
            }
            if kind != 10 {
                return Err(format!("unsupported AMD64 relocation type {kind}"));
            }
            let target = page
                .checked_add((value & 0x0fff) as usize)
                .ok_or("relocation target overflow")?;
            target
                .checked_add(8)
                .filter(|end| *end <= bytes.len())
                .ok_or("relocation target exceeds the image")?;
            if delta != 0 {
                let relocated = read_u64(bytes, target)?.wrapping_add(delta);
                write_u64(bytes, target, relocated)?;
                patched += 1;
            }
        }
        offset = block_end;
    }
    Ok(patched)
}

fn c_string(bytes: &[u8], offset: usize, max: usize) -> Result<String, String> {
    let end_limit = offset
        .checked_add(max.min(bytes.len().saturating_sub(offset)))
        .ok_or("string offset overflow")?;
    let tail = bytes
        .get(offset..end_limit)
        .ok_or("string exceeds the image")?;
    let end = tail
        .iter()
        .position(|byte| *byte == 0)
        .ok_or("unterminated PE string")?;
    String::from_utf8(tail[..end].to_vec()).map_err(|_| "PE string is not UTF-8".into())
}

#[derive(Debug, PartialEq, Eq)]
enum ExportValue {
    Address(u32),
    Forwarder(String),
}

fn parse_named_export(
    bytes: &[u8],
    exports: (u32, u32),
    wanted: &[u8],
) -> Result<ExportValue, String> {
    let (rva, size) = exports;
    if rva == 0 || size < 40 {
        return Err("feature DLL has no usable export directory".into());
    }
    let dir = rva as usize;
    let dir_end = dir
        .checked_add(size as usize)
        .filter(|end| *end <= bytes.len())
        .ok_or("export directory exceeds the image")?;
    let function_count = read_u32(bytes, dir + 20)? as usize;
    let name_count = read_u32(bytes, dir + 24)? as usize;
    if name_count > 65_536 {
        return Err("unreasonable export name count".into());
    }
    let functions = read_u32(bytes, dir + 28)? as usize;
    let names = read_u32(bytes, dir + 32)? as usize;
    let ordinals = read_u32(bytes, dir + 36)? as usize;
    for index in 0..name_count {
        let name_rva = read_u32(bytes, names + index * 4)? as usize;
        if c_string(bytes, name_rva, 256)?.as_bytes() != wanted {
            continue;
        }
        let ordinal = read_u16(bytes, ordinals + index * 2)? as usize;
        if ordinal >= function_count {
            return Err("export ordinal exceeds the address table".into());
        }
        let function_rva = read_u32(bytes, functions + ordinal * 4)?;
        if function_rva == 0 {
            return Err("feature export has no address".into());
        }
        if function_rva as usize >= rva as usize && (function_rva as usize) < dir_end {
            return Ok(ExportValue::Forwarder(c_string(
                bytes,
                function_rva as usize,
                256,
            )?));
        }
        function_rva
            .checked_add(1)
            .filter(|end| *end as usize <= bytes.len())
            .ok_or("feature export address exceeds the image")?;
        return Ok(ExportValue::Address(function_rva));
    }
    Err(format!(
        "missing {} export",
        String::from_utf8_lossy(wanted)
    ))
}

fn find_named_export(bytes: &[u8], exports: (u32, u32), wanted: &[u8]) -> Result<u32, String> {
    match parse_named_export(bytes, exports, wanted)? {
        ExportValue::Address(rva) => Ok(rva),
        ExportValue::Forwarder(_) => Err("feature export must not be a forwarder".into()),
    }
}

#[derive(Debug, PartialEq, Eq)]
enum ImportSymbol {
    Name(String),
    Ordinal(u16),
}

fn resolve_imports_with<F>(
    bytes: &mut [u8],
    imports: (u32, u32),
    mut resolve: F,
) -> Result<(), String>
where
    F: FnMut(&str, ImportSymbol) -> Result<u64, String>,
{
    let (rva, size) = imports;
    if rva == 0 && size == 0 {
        return Ok(());
    }
    if rva == 0 || size == 0 {
        return Err("invalid import directory".into());
    }
    let mut descriptor = rva as usize;
    let directory_end = descriptor
        .checked_add(size as usize)
        .filter(|end| *end <= bytes.len())
        .ok_or("import directory exceeds the image")?;
    let mut descriptors = 0usize;
    loop {
        if descriptor
            .checked_add(20)
            .is_none_or(|end| end > directory_end)
        {
            return Err("unterminated import descriptor table".into());
        }
        let original_thunks = read_u32(bytes, descriptor)? as usize;
        let name_rva = read_u32(bytes, descriptor + 12)? as usize;
        let first_thunk = read_u32(bytes, descriptor + 16)? as usize;
        if original_thunks == 0 && name_rva == 0 && first_thunk == 0 {
            return Ok(());
        }
        if name_rva == 0 || first_thunk == 0 {
            return Err("incomplete import descriptor".into());
        }
        let dll = c_string(bytes, name_rva, 260)?;
        let lookup = if original_thunks == 0 {
            first_thunk
        } else {
            original_thunks
        };
        let mut terminated = false;
        for index in 0..65_536usize {
            let entry_offset = lookup
                .checked_add(index * 8)
                .ok_or("import lookup overflow")?;
            let iat_offset = first_thunk
                .checked_add(index * 8)
                .ok_or("import IAT overflow")?;
            let entry = read_u64(bytes, entry_offset)?;
            if entry == 0 {
                terminated = true;
                break;
            }
            let symbol = if entry & 0x8000_0000_0000_0000 != 0 {
                ImportSymbol::Ordinal((entry & 0xffff) as u16)
            } else {
                let name = (entry as usize)
                    .checked_add(2)
                    .ok_or("import name overflow")?;
                ImportSymbol::Name(c_string(bytes, name, 256)?)
            };
            let address =
                resolve(&dll, symbol).map_err(|error| format!("import {dll}: {error}"))?;
            if address == 0 {
                return Err(format!("import {dll}: resolver returned a null address"));
            }
            write_u64(bytes, iat_offset, address)?;
        }
        if !terminated {
            return Err(format!("unterminated import thunk array for {dll}"));
        }
        if descriptors == 4095 {
            return Err("too many import descriptors".into());
        }
        descriptors += 1;
        descriptor += 20;
    }
}

fn api_set_fallback_allowed(module: &str, symbol: &ImportSymbol) -> bool {
    let module = module.to_ascii_lowercase();
    matches!(symbol, ImportSymbol::Name(_))
        && (module.starts_with("api-ms-") || module.starts_with("ext-ms-"))
}

fn feature_api_range_valid(base: usize, size: usize, api: usize) -> bool {
    api != 0
        && api >= base
        && api.is_multiple_of(core::mem::align_of::<hromatite_abi::FeatureApiV1>())
        && api
            .checked_add(core::mem::size_of::<hromatite_abi::FeatureApiV1>())
            .zip(base.checked_add(size))
            .is_some_and(|(end, image_end)| end <= image_end)
}

fn feature_api_bytes_valid(bytes: &[u8]) -> bool {
    use hromatite_abi::FeatureApiV1;
    let table_size = core::mem::size_of::<FeatureApiV1>();
    if bytes.len() != table_size
        || !read_u32(bytes, 0).is_ok_and(|size| size as usize >= table_size)
        || !read_u32(bytes, 4).is_ok_and(|version| version == hromatite_abi::ABI_VERSION)
    {
        return false;
    }
    // Do not form &FeatureApiV1 until every non-nullable Rust function pointer
    // has been checked as plain bytes. A null callback would make that typed
    // reference invalid before the ABI version check could reject it.
    [
        core::mem::offset_of!(FeatureApiV1, init),
        core::mem::offset_of!(FeatureApiV1, dispatch),
        core::mem::offset_of!(FeatureApiV1, tick),
        core::mem::offset_of!(FeatureApiV1, configure),
        core::mem::offset_of!(FeatureApiV1, shutdown),
    ]
    .into_iter()
    .all(|offset| read_u64(bytes, offset).is_ok_and(|address| address != 0))
}

fn finish_import_resolution<T>(
    result: Result<T, String>,
    loaded: &mut Vec<usize>,
    owned: &mut Vec<usize>,
) -> Result<T, String> {
    // Drain the ledger before propagating an error: every LoadLibraryW reference
    // remains owned by the outer cleanup even when a later import is unresolved.
    owned.append(loaded);
    result
}

fn detach_notifications<T, E>(
    callbacks: &[u32],
    completed: usize,
    call_entry: bool,
    mut tls: T,
    mut entry: E,
) -> Result<(), String>
where
    T: FnMut(u32) -> Result<(), String>,
    E: FnMut() -> Result<(), String>,
{
    let mut errors = Vec::new();
    // Native loaders send TLS detach before the DLL entry's PROCESS_DETACH.
    // Only callbacks whose PROCESS_ATTACH returned may receive detach.
    for callback in callbacks[..completed.min(callbacks.len())].iter().rev() {
        if let Err(error) = tls(*callback) {
            errors.push(format!("TLS detach {callback:#x}: {error}"));
        }
    }
    if call_entry && let Err(error) = entry() {
        errors.push(format!("DllMain detach: {error}"));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn push_cleanup_error(errors: &mut Vec<String>, stage: &str, result: Result<(), String>) {
    if let Err(error) = result {
        errors.push(format!("{stage}: {error}"));
    }
}

fn protection_plan(pe: &PeImage) -> Result<Vec<(u32, u32, u32)>, String> {
    const PAGE: usize = 4096;
    let image_end = (pe.size_of_image as usize)
        .checked_add(PAGE - 1)
        .map(|end| end & !(PAGE - 1))
        .ok_or("image protection range overflow")?;
    let mut pages = Vec::new();
    for section in &pe.sections {
        let size = section.rsize.max(section.vsize);
        if size == 0 {
            continue;
        }
        let section_end = (section.va as usize)
            .checked_add(size as usize)
            .filter(|end| *end <= pe.size_of_image as usize)
            .ok_or("section protection range exceeds the image")?;
        let start = section.va as usize & !(PAGE - 1);
        let end = section_end
            .checked_add(PAGE - 1)
            .map(|end| end & !(PAGE - 1))
            .filter(|end| *end <= image_end)
            .ok_or("aligned section protection range exceeds the image")?;
        if start == 0 && pe.size_of_headers != 0 {
            return Err("section and erased headers share a protection page".into());
        }
        let protection = if section.is_executable() {
            crate::ffi::PAGE_EXECUTE_READ
        } else if section.characteristics & 0x8000_0000 != 0 {
            crate::ffi::PAGE_READWRITE
        } else {
            crate::ffi::PAGE_READONLY
        };
        pages.push((start, end, protection));
    }
    pages.sort_unstable_by_key(|page| page.0);
    let mut merged: Vec<(usize, usize, u32)> = Vec::new();
    for (start, end, protection) in pages {
        if let Some(last) = merged.last_mut()
            && start < last.1
        {
            if last.2 != protection {
                return Err(format!(
                    "sections with conflicting protection share page {start:#x}"
                ));
            }
            last.1 = last.1.max(end);
            continue;
        }
        if let Some(last) = merged.last_mut()
            && start == last.1
            && last.2 == protection
        {
            last.1 = end;
            continue;
        }
        merged.push((start, end, protection));
    }
    merged
        .into_iter()
        .map(|(start, end, protection)| {
            let size = u32::try_from(end - start).map_err(|_| "protection range is too large")?;
            Ok((start as u32, size, protection))
        })
        .collect()
}

struct PreparedImage {
    bytes: Vec<u8>,
    exports: (u32, u32),
    tls: (u32, u32),
}

impl PreparedImage {
    fn from_pe(pe: &PeImage, _base: usize) -> Result<Self, String> {
        if pe.machine != 0x8664 || !pe.is_64 || !pe.is_dll {
            return Err("local feature image must be an AMD64 PE32+ DLL".into());
        }
        let size = pe.size_of_image as usize;
        let header_len = (pe.size_of_headers as usize).min(pe.data.len());
        if header_len > size {
            return Err("PE headers exceed the mapped image".into());
        }
        let mut bytes = vec![0u8; size];
        bytes[..header_len].copy_from_slice(&pe.data[..header_len]);
        for section in &pe.sections {
            let source = (section.raw as usize)
                .checked_add(section.rsize as usize)
                .filter(|end| *end <= pe.data.len())
                .ok_or("section raw data exceeds the file")?;
            let image_end = (section.va as usize)
                .checked_add((section.rsize.max(section.vsize)) as usize)
                .filter(|end| *end <= size)
                .ok_or("section data exceeds SizeOfImage")?;
            let _ = image_end;
            let dst = section.va as usize;
            let src = section.raw as usize;
            bytes[dst..dst + section.rsize as usize].copy_from_slice(&pe.data[src..source]);
        }
        if _base as u64 != pe.image_base && pe.reloc == (0, 0) {
            return Err("image needs relocation but has no relocation directory".into());
        }
        apply_relocations(&mut bytes, pe, _base)?;
        Ok(Self {
            bytes,
            exports: pe.exports,
            tls: pe.tls,
        })
    }

    fn feature_entry_rva(&self) -> Result<u32, String> {
        find_named_export(self.bytes.as_slice(), self.exports, b"hromatite_feature_v1")
    }

    fn tls_callback_rvas(&self, base: usize) -> Result<Vec<u32>, String> {
        let (rva, size) = self.tls;
        if rva == 0 && size == 0 {
            return Ok(Vec::new());
        }
        if rva == 0 || size < 40 {
            return Err("invalid AMD64 TLS directory".into());
        }
        let callback_array = read_u64(&self.bytes, rva as usize + 24)?;
        if callback_array == 0 {
            return Ok(Vec::new());
        }
        let array_rva = callback_array
            .checked_sub(base as u64)
            .filter(|rva| (*rva as usize) < self.bytes.len())
            .ok_or("TLS callback array is outside the image")? as usize;
        let mut callbacks = Vec::new();
        for index in 0..64usize {
            let callback = read_u64(&self.bytes, array_rva + index * 8)?;
            if callback == 0 {
                return Ok(callbacks);
            }
            let callback_rva = callback
                .checked_sub(base as u64)
                .filter(|rva| (*rva as usize) < self.bytes.len())
                .ok_or("TLS callback is outside the image")?;
            callbacks.push(callback_rva as u32);
        }
        Err("TLS callback array exceeds the 64-entry limit".into())
    }
}

/// Owning handle for one manually mapped feature generation. Dropping it leaks
/// deliberately; [`FeatureImage::retire`] is the only detach/free operation.
pub struct FeatureImage {
    base: usize,
    size: usize,
    api: *const hromatite_abi::FeatureApiV1,
    #[cfg(target_os = "windows")]
    entry_rva: u32,
    #[cfg(target_os = "windows")]
    tls_callbacks: Vec<u32>,
    #[cfg(target_os = "windows")]
    dependencies: Vec<usize>,
}

impl FeatureImage {
    /// Map an AMD64 feature DLL in this process and invoke TLS/`DllMain` on this
    /// thread. After success, the parent initializes the table returned by
    /// [`FeatureImage::api`]. Never call this under the Windows loader lock.
    pub fn load(path: &str) -> Result<Self, String> {
        #[cfg(target_os = "windows")]
        {
            imp::load(path)
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = path;
            Err("local feature mapping requires Windows".into())
        }
    }

    pub fn base(&self) -> usize {
        self.base
    }

    pub fn size(&self) -> usize {
        self.size
    }

    /// Borrow the validated table until this consuming owner is retired.
    /// The raw pointer does not extend the image lifetime.
    pub fn api(&self) -> *const hromatite_abi::FeatureApiV1 {
        self.api
    }

    /// Consume this generation, send completed TLS callbacks their detach in
    /// reverse order, then `DllMain(PROCESS_DETACH)`, and finally `VirtualFree`.
    /// Owned dependency references are released after the final `VirtualFree` attempt.
    ///
    /// # Safety
    /// The caller must prove that no thread is executing in this image and no
    /// in-flight or future caller can use `api()` or any image-derived pointer.
    /// The feature's shutdown/quiescence protocol must already have completed.
    pub unsafe fn retire(self) -> Result<(), String> {
        #[cfg(target_os = "windows")]
        {
            // SAFETY: the consuming API and its caller contract are unchanged.
            unsafe { imp::retire(self) }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = self;
            Err("local feature mapping requires Windows".into())
        }
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use super::{
        ExportValue, ImportSymbol, PreparedImage, api_set_fallback_allowed, detach_notifications,
        feature_api_bytes_valid, feature_api_range_valid, finish_import_resolution,
        protection_plan, push_cleanup_error, resolve_imports_with,
    };
    use crate::pe::PeImage;
    use crate::raw::{ffi, memory};
    use hromatite_abi::{FeatureApiV1, FeatureEntryFn};

    unsafe extern "system" {
        #[link_name = "FreeLibrary"]
        fn free_library(module: ffi::HMODULE) -> ffi::BOOL;
    }

    #[derive(Clone)]
    struct LiveModule {
        base: usize,
        size: usize,
        exports: (u32, u32),
    }

    impl LiveModule {
        fn from_base(base: usize) -> Option<Self> {
            if base == 0 || memory::read_bytes(base, 2)?.as_slice() != b"MZ" {
                return None;
            }
            let pe = memory::read_u32(base.checked_add(0x3c)?)? as usize;
            if memory::read_bytes(base.checked_add(pe)?, 4)?.as_slice() != b"PE\0\0" {
                return None;
            }
            let optional = base.checked_add(pe)?.checked_add(24)?;
            let magic = memory::read_u16(optional)?;
            let directory = optional.checked_add(if magic == 0x20b { 112 } else { 96 })?;
            let size = memory::read_u32(optional.checked_add(56)?)? as usize;
            if size == 0 || size > 1024 * 1024 * 1024 {
                return None;
            }
            Some(Self {
                base,
                size,
                exports: (
                    memory::read_u32(directory)?,
                    memory::read_u32(directory.checked_add(4)?)?,
                ),
            })
        }

        fn read(&self, rva: usize, len: usize) -> Option<Vec<u8>> {
            rva.checked_add(len).filter(|end| *end <= self.size)?;
            memory::read_bytes(self.base.checked_add(rva)?, len)
        }

        fn u16(&self, rva: usize) -> Option<u16> {
            Some(u16::from_le_bytes(self.read(rva, 2)?.try_into().ok()?))
        }

        fn u32(&self, rva: usize) -> Option<u32> {
            Some(u32::from_le_bytes(self.read(rva, 4)?.try_into().ok()?))
        }

        fn string(&self, rva: usize, max: usize) -> Option<String> {
            let bytes = self.read(rva, max.min(self.size.checked_sub(rva)?))?;
            let end = bytes.iter().position(|byte| *byte == 0)?;
            String::from_utf8(bytes[..end].to_vec()).ok()
        }

        fn export(&self, symbol: &ImportSymbol) -> Result<Option<ExportValue>, String> {
            let (export_rva, export_size) = self.exports;
            if export_rva == 0 || export_size < 40 {
                return Ok(None);
            }
            let dir = export_rva as usize;
            let dir_end = dir
                .checked_add(export_size as usize)
                .filter(|end| *end <= self.size)
                .ok_or("dependency export directory exceeds its image")?;
            let ordinal_base = self.u32(dir + 16).ok_or("truncated export directory")?;
            let function_count = self.u32(dir + 20).ok_or("truncated export directory")? as usize;
            let functions = self.u32(dir + 28).ok_or("truncated export directory")? as usize;
            let index = match symbol {
                ImportSymbol::Ordinal(ordinal) => (*ordinal as usize)
                    .checked_sub(ordinal_base as usize)
                    .filter(|index| *index < function_count)
                    .ok_or("dependency ordinal is outside its export table")?,
                ImportSymbol::Name(wanted) => {
                    let name_count =
                        self.u32(dir + 24).ok_or("truncated export directory")? as usize;
                    if name_count > 65_536 {
                        return Err("unreasonable dependency export name count".into());
                    }
                    let names = self.u32(dir + 32).ok_or("truncated export directory")? as usize;
                    let ordinals = self.u32(dir + 36).ok_or("truncated export directory")? as usize;
                    let mut found = None;
                    for index in 0..name_count {
                        let entry = names
                            .checked_add(index.checked_mul(4).ok_or("export name table overflow")?)
                            .ok_or("export name table overflow")?;
                        let name_rva = self
                            .u32(entry)
                            .ok_or("truncated dependency exported name")?;
                        if self.string(name_rva as usize, 256).as_deref() == Some(wanted.as_str()) {
                            let ordinal = self
                                .u16(ordinals + index * 2)
                                .ok_or("truncated dependency export ordinal")?
                                as usize;
                            if ordinal >= function_count {
                                return Err(
                                    "dependency export ordinal exceeds the address table".into()
                                );
                            }
                            found = Some(ordinal);
                            break;
                        }
                    }
                    let Some(index) = found else { return Ok(None) };
                    index
                }
            };
            let function_rva = self
                .u32(functions + index * 4)
                .ok_or("truncated dependency export address")?;
            if function_rva == 0 {
                return Ok(None);
            }
            if function_rva as usize >= dir && (function_rva as usize) < dir_end {
                let forwarder = self
                    .string(function_rva as usize, 256)
                    .ok_or("unterminated dependency forwarder")?;
                return Ok(Some(ExportValue::Forwarder(forwarder)));
            }
            if function_rva as usize >= self.size {
                return Err("dependency export address exceeds its image".into());
            }
            Ok(Some(ExportValue::Address(function_rva)))
        }
    }

    struct DependencyResolver {
        loaded: Vec<usize>,
    }

    impl DependencyResolver {
        fn open_module(
            &mut self,
            name: &str,
        ) -> Result<(ffi::HMODULE, Option<LiveModule>), String> {
            let wide = ffi::wide(name);
            // SAFETY: `wide` is NUL terminated and lives through both calls.
            let mut handle = unsafe { ffi::GetModuleHandleW(wide.as_ptr()) };
            if handle.is_null() {
                // SAFETY: same buffer contract; the new loader reference is retained
                // in `loaded` until feature retirement or failed-load cleanup.
                handle = unsafe { ffi::LoadLibraryW(wide.as_ptr()) };
                if handle.is_null() {
                    // SAFETY: reads this thread's error slot immediately after failure.
                    let error = unsafe { ffi::GetLastError() };
                    return Err(format!("LoadLibraryW({name}) failed, GetLastError={error}"));
                }
                self.loaded.push(handle as usize);
            }
            let live = LiveModule::from_base(handle as usize);
            Ok((handle, live))
        }

        fn get_proc_address(&self, module: ffi::HMODULE, symbol: &ImportSymbol) -> Option<usize> {
            let storage;
            let name = match symbol {
                ImportSymbol::Name(name) => {
                    storage = ffi::cstr(name);
                    storage.as_ptr()
                }
                ImportSymbol::Ordinal(ordinal) => *ordinal as usize as *const u8,
            };
            // SAFETY: named strings are NUL terminated and ordinal pointers use
            // MAKEINTRESOURCE. The module handle is borrowed from the local loader.
            let address = unsafe { ffi::GetProcAddress(module, name) };
            (!address.is_null()).then_some(address as usize)
        }

        fn forwarder(forwarder: &str) -> Result<(String, ImportSymbol), String> {
            let (module, function) = forwarder
                .split_once('.')
                .ok_or_else(|| format!("invalid forwarder {forwarder}"))?;
            let module = if module.to_ascii_lowercase().ends_with(".dll") {
                module.to_string()
            } else {
                format!("{module}.dll")
            };
            let symbol = if let Some(ordinal) = function.strip_prefix('#') {
                ImportSymbol::Ordinal(
                    ordinal
                        .parse::<u16>()
                        .map_err(|_| format!("invalid forwarder ordinal {function}"))?,
                )
            } else {
                ImportSymbol::Name(function.to_string())
            };
            Ok((module, symbol))
        }

        fn resolve(
            &mut self,
            module: &str,
            symbol: ImportSymbol,
            depth: u8,
        ) -> Result<usize, String> {
            if depth >= 8 {
                return Err("forwarder recursion limit exceeded".into());
            }
            let (handle, live) = self.open_module(module)?;
            if let Some(module) = &live
                && let Some(export) = module.export(&symbol)?
            {
                match export {
                    ExportValue::Address(rva) => {
                        let address = module
                            .base
                            .checked_add(rva as usize)
                            .ok_or("dependency export address overflow")?;
                        return Ok(address);
                    }
                    ExportValue::Forwarder(forwarder) => {
                        let (module, symbol) = Self::forwarder(&forwarder)?;
                        return self.resolve(&module, symbol, depth + 1);
                    }
                }
            }
            if let Some(address) = self.get_proc_address(handle, &symbol) {
                return Ok(address);
            }
            if api_set_fallback_allowed(module, &symbol)
                && let ImportSymbol::Name(name) = &symbol
                && live.is_none()
                && let Some(address) = self.find_export_anywhere(name)?
            {
                return Ok(address);
            }
            Err(format!("cannot resolve {module}!{symbol:?}"))
        }

        fn find_export_anywhere(&mut self, name: &str) -> Result<Option<usize>, String> {
            // SAFETY: parameterless process-id query.
            let pid = unsafe { ffi::GetCurrentProcessId() };
            for (_, base, _) in crate::procs::modules(pid).into_iter().take(512) {
                let Some(module) = LiveModule::from_base(base) else {
                    continue;
                };
                match module.export(&ImportSymbol::Name(name.to_string())) {
                    Ok(Some(ExportValue::Address(rva))) => {
                        return module
                            .base
                            .checked_add(rva as usize)
                            .map(Some)
                            .ok_or_else(|| "dependency export address overflow".into());
                    }
                    Ok(Some(ExportValue::Forwarder(forwarder))) => {
                        let (module, symbol) = Self::forwarder(&forwarder)?;
                        return self.resolve(&module, symbol, 1).map(Some);
                    }
                    Ok(None) => {}
                    Err(_) => {}
                }
            }
            Ok(None)
        }
    }

    unsafe fn dll_main(base: usize, entry_rva: u32, reason: u32) -> i32 {
        let address = base + entry_rva as usize;
        // SAFETY: the caller validated the entry RVA and the image remains mapped.
        let entry: unsafe extern "system" fn(usize, u32, usize) -> i32 =
            unsafe { core::mem::transmute(address) };
        // SAFETY: follows the DllMain ABI on the current thread with reserved=0.
        unsafe { entry(base, reason, 0) }
    }

    unsafe fn tls_callback(base: usize, callback_rva: u32, reason: u32) {
        let address = base + callback_rva as usize;
        // SAFETY: the caller recorded and validated this callback while building.
        let callback: unsafe extern "system" fn(usize, u32, usize) =
            unsafe { core::mem::transmute(address) };
        // SAFETY: follows the TLS callback ABI on the current thread.
        unsafe { callback(base, reason, 0) }
    }

    fn guarded_tls(base: usize, callback_rva: u32, reason: u32) -> Result<(), String> {
        crate::veh::call_guarded(|| {
            // SAFETY: the callback was validated in an executable image section;
            // the inner VEH boundary returns here before mapper cleanup must run.
            unsafe { tls_callback(base, callback_rva, reason) };
        })
        .map_err(|fault| format!("{fault}"))
    }

    fn guarded_dll_main(base: usize, entry_rva: u32, reason: u32) -> Result<i32, String> {
        crate::veh::call_guarded(|| {
            // SAFETY: the entry was validated in an executable image section;
            // returning through this inner guard keeps failed-load cleanup reachable.
            unsafe { dll_main(base, entry_rva, reason) }
        })
        .map_err(|fault| format!("{fault}"))
    }

    fn notifications(
        base: usize,
        entry_rva: u32,
        callbacks: &[u32],
        completed: usize,
        entry_returned: bool,
    ) -> Result<(), String> {
        detach_notifications(
            callbacks,
            completed,
            entry_returned,
            |callback| guarded_tls(base, callback, 0),
            || guarded_dll_main(base, entry_rva, 0).map(|_| ()),
        )
    }

    fn release_libraries(libraries: &[usize]) -> Result<(), String> {
        let mut errors = Vec::new();
        for module in libraries.iter().rev() {
            // SAFETY: each handle represents one LoadLibraryW reference owned here.
            if unsafe { free_library(*module as ffi::HMODULE) } == 0 {
                // SAFETY: read immediately after the failing call on this thread.
                let error = unsafe { ffi::GetLastError() };
                errors.push(format!(
                    "FreeLibrary({module:#x}) failed, GetLastError={error}"
                ));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }

    fn code_rva_valid(pe: &PeImage, rva: u32) -> bool {
        pe.sections.iter().any(|section| {
            section.is_executable()
                && rva >= section.va
                && (rva as usize).checked_add(1).is_some_and(|end| {
                    end <= section.va as usize + section.rsize.max(section.vsize) as usize
                })
        })
    }

    pub fn load(path: &str) -> Result<super::FeatureImage, String> {
        let pe = PeImage::from_file(path)?;
        if pe.machine != 0x8664 || !pe.is_64 || !pe.is_dll {
            return Err("local feature image must be an AMD64 PE32+ DLL".into());
        }
        if pe.entry != 0 && !code_rva_valid(&pe, pe.entry) {
            return Err("feature DllMain is not in an executable section".into());
        }
        // SAFETY: requests a new non-executable RW allocation in this process.
        let base = unsafe {
            ffi::VirtualAlloc(
                0,
                pe.size_of_image as usize,
                ffi::MEM_COMMIT | ffi::MEM_RESERVE,
                ffi::PAGE_READWRITE,
            )
        } as usize;
        if base == 0 {
            // SAFETY: reads this thread's error slot immediately after failure.
            let error = unsafe { ffi::GetLastError() };
            return Err(format!(
                "VirtualAlloc(feature image) failed, GetLastError={error}"
            ));
        }

        let mut dependencies = Vec::new();
        let mut completed_callbacks = 0usize;
        let mut entry_returned = false;
        let mut recorded_callbacks = Vec::new();
        let result = (|| -> Result<super::FeatureImage, String> {
            let mut prepared = PreparedImage::from_pe(&pe, base)?;
            let feature_entry = prepared.feature_entry_rva()?;
            if !code_rva_valid(&pe, feature_entry) {
                return Err("feature export is not in an executable section".into());
            }
            let callbacks = prepared.tls_callback_rvas(base)?;
            if let Some(callback) = callbacks.iter().find(|rva| !code_rva_valid(&pe, **rva)) {
                return Err(format!(
                    "TLS callback {callback:#x} is not in an executable section"
                ));
            }
            recorded_callbacks = callbacks.clone();
            {
                let mut resolver = DependencyResolver { loaded: Vec::new() };
                let resolved =
                    resolve_imports_with(&mut prepared.bytes, pe.imports, |module, symbol| {
                        resolver
                            .resolve(module, symbol, 0)
                            .map(|address| address as u64)
                    });
                finish_import_resolution(resolved, &mut resolver.loaded, &mut dependencies)?;
            }

            let headers = pe.size_of_headers as usize;
            // SAFETY: source and destination are separate live allocations of `size`;
            // the destination is RW and exclusively owned by this load operation.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    prepared.bytes.as_ptr(),
                    base as *mut u8,
                    prepared.bytes.len(),
                );
                if headers != 0 {
                    core::ptr::write_bytes(base as *mut u8, 0, headers);
                }
            }
            for (rva, size, protection) in protection_plan(&pe)? {
                let mut old = 0;
                // SAFETY: every range came from the validated PE section plan.
                if unsafe {
                    ffi::VirtualProtect(
                        (base + rva as usize) as *mut u8,
                        size as usize,
                        protection,
                        &mut old,
                    )
                } == 0
                {
                    return Err(format!("VirtualProtect(section {rva:#x}) failed"));
                }
            }
            if headers != 0 {
                let mut old = 0;
                // SAFETY: the header range belongs to this allocation and is no longer used.
                if unsafe {
                    ffi::VirtualProtect(base as *mut u8, headers, ffi::PAGE_NOACCESS, &mut old)
                } == 0
                {
                    return Err("VirtualProtect(erased headers) failed".into());
                }
            }
            // SAFETY: flushes exactly the freshly written allocation before executing it.
            if unsafe {
                ffi::FlushInstructionCache(
                    ffi::GetCurrentProcess(),
                    base as *const u8,
                    prepared.bytes.len(),
                )
            } == 0
            {
                return Err("FlushInstructionCache(feature image) failed".into());
            }

            for callback in &callbacks {
                guarded_tls(base, *callback, ffi::DLL_PROCESS_ATTACH)
                    .map_err(|fault| format!("TLS attach {callback:#x}: {fault}"))?;
                // A fault longjmps past this increment, so cleanup will not send a
                // second notification to the faulting or any not-yet-called callback.
                completed_callbacks += 1;
            }
            if pe.entry != 0 {
                let attached = guarded_dll_main(base, pe.entry, ffi::DLL_PROCESS_ATTACH)
                    .map_err(|fault| format!("DllMain attach: {fault}"))?;
                // FALSE is a completed attach and receives detach, just like the
                // native loader; an exception skips this assignment and gets none.
                entry_returned = true;
                if attached == 0 {
                    return Err("feature DllMain rejected PROCESS_ATTACH".into());
                }
            }
            // SAFETY: the export RVA was resolved from PE tables before erasure and
            // validated as executable. hromatite_feature_v1 has no arguments.
            let feature: FeatureEntryFn =
                unsafe { core::mem::transmute(base + feature_entry as usize) };
            let api = crate::veh::call_guarded(|| feature())
                .map_err(|fault| format!("hromatite_feature_v1: {fault}"))?;
            if api.is_null() {
                return Err("hromatite_feature_v1 returned null".into());
            }
            let api_address = api as usize;
            if !feature_api_range_valid(base, pe.size_of_image as usize, api_address) {
                return Err("feature API pointer is outside the aligned image bounds".into());
            }
            let api_bytes = memory::read_bytes(api_address, core::mem::size_of::<FeatureApiV1>())
                .ok_or("feature API table is not completely readable")?;
            if !feature_api_bytes_valid(&api_bytes) {
                return Err(
                    "feature API has the wrong size/version or a null lifecycle callback".into(),
                );
            }
            Ok(super::FeatureImage {
                base,
                size: pe.size_of_image as usize,
                api,
                entry_rva: pe.entry,
                tls_callbacks: callbacks,
                dependencies: dependencies.clone(),
            })
        })();

        match result {
            Ok(image) => Ok(image),
            Err(error) => {
                let mut errors = vec![error];
                push_cleanup_error(
                    &mut errors,
                    "failed-load detach",
                    notifications(
                        base,
                        pe.entry,
                        &recorded_callbacks,
                        completed_callbacks,
                        entry_returned,
                    ),
                );
                // SAFETY: no FeatureImage escaped and every callback that returned
                // has been offered its detach notification; failures are contained.
                let freed = unsafe { ffi::VirtualFree(base as *mut u8, 0, ffi::MEM_RELEASE) } != 0;
                if !freed {
                    errors.push("VirtualFree failed during failed-load cleanup".into());
                }
                push_cleanup_error(
                    &mut errors,
                    "failed-load dependency release",
                    release_libraries(&dependencies),
                );
                Err(errors.join("; "))
            }
        }
    }

    /// # Safety
    /// The caller must have drained every call into the image.
    pub unsafe fn retire(image: super::FeatureImage) -> Result<(), String> {
        // The consuming contract makes this a one-way retirement: even failed
        // detach notifications must not cause a second detach or leave the image
        // published for future calls.
        let mut errors = Vec::new();
        push_cleanup_error(
            &mut errors,
            "retirement detach",
            notifications(
                image.base,
                image.entry_rva,
                &image.tls_callbacks,
                image.tls_callbacks.len(),
                image.entry_rva != 0,
            ),
        );
        // SAFETY: this is the unique owned allocation and no image code can run
        // again under the consuming caller's drain contract.
        if unsafe { ffi::VirtualFree(image.base as *mut u8, 0, ffi::MEM_RELEASE) } == 0 {
            errors.push("VirtualFree(feature image) failed".into());
        }
        push_cleanup_error(
            &mut errors,
            "retirement dependency release",
            release_libraries(&image.dependencies),
        );
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("; "))
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(not(target_os = "windows"))]
    #[test]
    fn host_load_fails_closed_without_win32() {
        let error = super::FeatureImage::load("unused.dll").err().unwrap();
        assert!(error.contains("Windows"), "{error}");
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn owning_public_api_has_the_consuming_lifecycle_signature() {
        let load: fn(&str) -> Result<super::FeatureImage, String> = super::FeatureImage::load;
        let borrow_api: fn(&super::FeatureImage) -> *const hromatite_abi::FeatureApiV1 =
            super::FeatureImage::api;
        let retire: unsafe fn(super::FeatureImage) -> Result<(), String> =
            super::FeatureImage::retire;
        let _ = (load, borrow_api, retire);
    }

    #[test]
    fn resolves_ordinal_import_thunks() {
        let mut data = fixture();
        data[0x110..0x114].copy_from_slice(&0x1300u32.to_le_bytes());
        data[0x114..0x118].copy_from_slice(&40u32.to_le_bytes());
        data[0x700..0x704].copy_from_slice(&0x1340u32.to_le_bytes());
        data[0x70c..0x710].copy_from_slice(&0x1380u32.to_le_bytes());
        data[0x710..0x714].copy_from_slice(&0x1360u32.to_le_bytes());
        data[0x740..0x748].copy_from_slice(&0x8000_0000_0000_002au64.to_le_bytes());
        data[0x780..0x78d].copy_from_slice(b"KERNEL32.dll\0");
        let pe = PeImage::from_bytes(data).unwrap();
        let mut image = super::PreparedImage::from_pe(&pe, 0x140000000).unwrap();
        super::resolve_imports_with(&mut image.bytes, pe.imports, |_dll, symbol| {
            assert_eq!(symbol, super::ImportSymbol::Ordinal(42));
            Ok(0x7fff_1234_5000)
        })
        .unwrap();
        assert_eq!(
            u64::from_le_bytes(image.bytes[0x1360..0x1368].try_into().unwrap()),
            0x7fff_1234_5000
        );
    }
    use crate::pe::PeImage;

    fn fixture() -> Vec<u8> {
        let mut data = vec![0u8; 0x800];
        data[..2].copy_from_slice(b"MZ");
        data[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        data[0x80..0x84].copy_from_slice(b"PE\0\0");
        data[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
        data[0x86..0x88].copy_from_slice(&1u16.to_le_bytes());
        data[0x96..0x98].copy_from_slice(&0x2022u16.to_le_bytes());
        data[0x94..0x96].copy_from_slice(&0xf0u16.to_le_bytes());
        data[0x98..0x9a].copy_from_slice(&0x20bu16.to_le_bytes());
        data[0xa8..0xac].copy_from_slice(&0x1000u32.to_le_bytes());
        data[0xb0..0xb8].copy_from_slice(&0x140000000u64.to_le_bytes());
        data[0xd0..0xd4].copy_from_slice(&0x2000u32.to_le_bytes());
        data[0xd4..0xd8].copy_from_slice(&0x400u32.to_le_bytes());
        data[0x188..0x18c].copy_from_slice(b".tex");
        data[0x18c..0x190].copy_from_slice(b"t\0\0\0");
        data[0x190..0x194].copy_from_slice(&0x400u32.to_le_bytes());
        data[0x194..0x198].copy_from_slice(&0x1000u32.to_le_bytes());
        data[0x198..0x19c].copy_from_slice(&0x400u32.to_le_bytes());
        data[0x19c..0x1a0].copy_from_slice(&0x400u32.to_le_bytes());
        data[0x1ac..0x1b0].copy_from_slice(&0x6000_0020u32.to_le_bytes());
        data
    }

    #[test]
    fn builder_lays_out_headers_and_complete_section_raw_data() {
        let mut data = fixture();
        data[0x3f0] = 0x5a;
        data[0x400] = 0x48;
        data[0x7ff] = 0xc3;
        let pe = PeImage::from_bytes(data).unwrap();
        let image = super::PreparedImage::from_pe(&pe, 0x140000000).unwrap();
        assert_eq!(image.bytes.len(), 0x2000);
        assert_eq!(image.bytes[0x3f0], 0x5a);
        assert_eq!(image.bytes[0x1000], 0x48);
        assert_eq!(image.bytes[0x13ff], 0xc3);
    }

    #[test]
    fn builder_applies_dir64_relocations_at_the_allocated_base() {
        let mut data = fixture();
        data[0x130..0x134].copy_from_slice(&0x1100u32.to_le_bytes());
        data[0x134..0x138].copy_from_slice(&12u32.to_le_bytes());
        data[0x500..0x504].copy_from_slice(&0x1000u32.to_le_bytes());
        data[0x504..0x508].copy_from_slice(&12u32.to_le_bytes());
        data[0x508..0x50a].copy_from_slice(&0xa050u16.to_le_bytes());
        data[0x450..0x458].copy_from_slice(&0x140002222u64.to_le_bytes());
        let pe = PeImage::from_bytes(data).unwrap();
        let image = super::PreparedImage::from_pe(&pe, 0x150000000).unwrap();
        assert_eq!(
            u64::from_le_bytes(image.bytes[0x1050..0x1058].try_into().unwrap()),
            0x150002222
        );
    }

    #[test]
    fn builder_rejects_rebased_images_without_relocations() {
        let pe = PeImage::from_bytes(fixture()).unwrap();
        let error = super::PreparedImage::from_pe(&pe, 0x150000000)
            .err()
            .unwrap();
        assert!(error.contains("relocation"), "{error}");
    }

    #[test]
    fn builder_checks_relocation_targets_even_without_a_base_delta() {
        let mut data = fixture();
        data[0x130..0x134].copy_from_slice(&0x1100u32.to_le_bytes());
        data[0x134..0x138].copy_from_slice(&12u32.to_le_bytes());
        data[0x500..0x504].copy_from_slice(&0x1ffcu32.to_le_bytes());
        data[0x504..0x508].copy_from_slice(&12u32.to_le_bytes());
        data[0x508..0x50a].copy_from_slice(&0xa008u16.to_le_bytes());
        let pe = PeImage::from_bytes(data).unwrap();
        let error = super::PreparedImage::from_pe(&pe, 0x140000000)
            .err()
            .unwrap();
        assert!(error.contains("relocation"), "{error}");
    }

    #[test]
    fn export_parser_distinguishes_forwarders_from_code_addresses() {
        let mut data = fixture();
        data[0x108..0x10c].copy_from_slice(&0x1200u32.to_le_bytes());
        data[0x10c..0x110].copy_from_slice(&0x100u32.to_le_bytes());
        data[0x614..0x618].copy_from_slice(&1u32.to_le_bytes());
        data[0x618..0x61c].copy_from_slice(&1u32.to_le_bytes());
        data[0x61c..0x620].copy_from_slice(&0x1240u32.to_le_bytes());
        data[0x620..0x624].copy_from_slice(&0x1250u32.to_le_bytes());
        data[0x624..0x628].copy_from_slice(&0x1260u32.to_le_bytes());
        data[0x640..0x644].copy_from_slice(&0x12a0u32.to_le_bytes());
        data[0x650..0x654].copy_from_slice(&0x1280u32.to_le_bytes());
        data[0x680..0x695].copy_from_slice(b"hromatite_feature_v1\0");
        data[0x6a0..0x6b8].copy_from_slice(b"KERNELBASE.VirtualAlloc\0");
        let pe = PeImage::from_bytes(data).unwrap();
        let image = super::PreparedImage::from_pe(&pe, 0x140000000).unwrap();
        assert_eq!(
            super::parse_named_export(&image.bytes, image.exports, b"hromatite_feature_v1")
                .unwrap(),
            super::ExportValue::Forwarder("KERNELBASE.VirtualAlloc".into())
        );
    }

    #[test]
    fn resolves_named_import_thunks_through_the_supplied_local_resolver() {
        let mut data = fixture();
        data[0x110..0x114].copy_from_slice(&0x1300u32.to_le_bytes());
        data[0x114..0x118].copy_from_slice(&40u32.to_le_bytes());
        data[0x700..0x704].copy_from_slice(&0x1340u32.to_le_bytes());
        data[0x70c..0x710].copy_from_slice(&0x1380u32.to_le_bytes());
        data[0x710..0x714].copy_from_slice(&0x1360u32.to_le_bytes());
        data[0x740..0x748].copy_from_slice(&0x13a0u64.to_le_bytes());
        data[0x780..0x78d].copy_from_slice(b"KERNEL32.dll\0");
        data[0x7a2..0x7af].copy_from_slice(b"VirtualAlloc\0");
        let pe = PeImage::from_bytes(data).unwrap();
        let mut image = super::PreparedImage::from_pe(&pe, 0x140000000).unwrap();
        let mut calls = 0;
        super::resolve_imports_with(&mut image.bytes, pe.imports, |dll, symbol| {
            calls += 1;
            assert_eq!(dll, "KERNEL32.dll");
            assert_eq!(symbol, super::ImportSymbol::Name("VirtualAlloc".into()));
            Ok(0x7fff_1234_5000)
        })
        .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(
            u64::from_le_bytes(image.bytes[0x1360..0x1368].try_into().unwrap()),
            0x7fff_1234_5000
        );
    }

    #[test]
    fn finds_the_feature_export_in_the_mapped_image() {
        let mut data = fixture();
        data[0x108..0x10c].copy_from_slice(&0x1200u32.to_le_bytes());
        data[0x10c..0x110].copy_from_slice(&0x100u32.to_le_bytes());
        data[0x610..0x614].copy_from_slice(&1u32.to_le_bytes());
        data[0x614..0x618].copy_from_slice(&1u32.to_le_bytes());
        data[0x618..0x61c].copy_from_slice(&1u32.to_le_bytes());
        data[0x61c..0x620].copy_from_slice(&0x1240u32.to_le_bytes());
        data[0x620..0x624].copy_from_slice(&0x1250u32.to_le_bytes());
        data[0x624..0x628].copy_from_slice(&0x1260u32.to_le_bytes());
        data[0x640..0x644].copy_from_slice(&0x1010u32.to_le_bytes());
        data[0x650..0x654].copy_from_slice(&0x1280u32.to_le_bytes());
        data[0x660..0x662].copy_from_slice(&0u16.to_le_bytes());
        data[0x680..0x695].copy_from_slice(b"hromatite_feature_v1\0");
        let pe = PeImage::from_bytes(data).unwrap();
        let image = super::PreparedImage::from_pe(&pe, 0x140000000).unwrap();
        assert_eq!(image.feature_entry_rva().unwrap(), 0x1010);
    }

    #[test]
    fn records_tls_callback_rvas_before_header_erasure() {
        let mut data = fixture();
        data[0x150..0x154].copy_from_slice(&0x1100u32.to_le_bytes());
        data[0x154..0x158].copy_from_slice(&40u32.to_le_bytes());
        data[0x518..0x520].copy_from_slice(&0x140001380u64.to_le_bytes());
        data[0x780..0x788].copy_from_slice(&0x140001010u64.to_le_bytes());
        data[0x788..0x790].copy_from_slice(&0x140001020u64.to_le_bytes());
        let pe = PeImage::from_bytes(data).unwrap();
        let image = super::PreparedImage::from_pe(&pe, 0x140000000).unwrap();
        assert_eq!(
            image.tls_callback_rvas(0x140000000).unwrap(),
            vec![0x1010, 0x1020]
        );
    }

    #[test]
    fn protection_plan_never_leaves_writable_executable_sections() {
        let mut data = fixture();
        data.resize(0xc00, 0);
        data[0x86..0x88].copy_from_slice(&2u16.to_le_bytes());
        data[0xd0..0xd4].copy_from_slice(&0x3000u32.to_le_bytes());
        data[0x1b0..0x1b5].copy_from_slice(b".data");
        data[0x1b8..0x1bc].copy_from_slice(&0x200u32.to_le_bytes());
        data[0x1bc..0x1c0].copy_from_slice(&0x2000u32.to_le_bytes());
        data[0x1c0..0x1c4].copy_from_slice(&0x200u32.to_le_bytes());
        data[0x1c4..0x1c8].copy_from_slice(&0x800u32.to_le_bytes());
        data[0x1d4..0x1d8].copy_from_slice(&0xc000_0040u32.to_le_bytes());
        let pe = PeImage::from_bytes(data).unwrap();
        let plan = super::protection_plan(&pe).unwrap();
        assert_eq!(
            plan,
            vec![
                (0x1000, 0x1000, crate::ffi::PAGE_EXECUTE_READ),
                (0x2000, 0x1000, crate::ffi::PAGE_READWRITE)
            ]
        );
        assert!(
            plan.iter()
                .all(|entry| entry.2 != crate::ffi::PAGE_EXECUTE_READWRITE)
        );
    }

    #[test]
    fn import_failure_still_transfers_owned_library_references() {
        let mut loaded = vec![0x1000, 0x2000];
        let mut owned = vec![0x9000];
        let result = super::finish_import_resolution(
            Err::<(), _>("unresolved second import".into()),
            &mut loaded,
            &mut owned,
        );
        assert!(result.is_err());
        assert!(loaded.is_empty());
        assert_eq!(owned, vec![0x9000, 0x1000, 0x2000]);
    }

    #[test]
    fn partial_attach_detaches_only_callbacks_that_returned_in_loader_order() {
        let events = std::cell::RefCell::new(Vec::new());
        let result = super::detach_notifications(
            &[0x10, 0x20, 0x30],
            1,
            true,
            |callback| {
                events.borrow_mut().push(("TLS", callback));
                Err("synthetic VEH exit".into())
            },
            || {
                events.borrow_mut().push(("entry", 0));
                Ok(())
            },
        );
        assert_eq!(events.into_inner(), vec![("TLS", 0x10), ("entry", 0)]);
        let error = result.err().unwrap();
        assert!(
            error.contains("TLS detach") && error.contains("VEH"),
            "{error}"
        );
    }

    #[test]
    fn faulted_entry_does_not_get_a_second_notification() {
        let mut events = Vec::new();
        super::detach_notifications(
            &[0x20, 0x10],
            1,
            false,
            |callback| {
                events.push(callback);
                Ok(())
            },
            || panic!("an excepting attach must not be detached"),
        )
        .unwrap();
        assert_eq!(events, vec![0x20]);
    }

    #[test]
    fn unterminated_import_thunks_are_an_error_not_a_successful_cap() {
        const LOOKUP: usize = 0x1400;
        const IAT: usize = 0x82000;
        const ENTRIES: usize = 65_536;
        let mut data = vec![0u8; 0x104000];
        data[0x1000..0x1004].copy_from_slice(&(LOOKUP as u32).to_le_bytes());
        data[0x100c..0x1010].copy_from_slice(&0x1040u32.to_le_bytes());
        data[0x1010..0x1014].copy_from_slice(&(IAT as u32).to_le_bytes());
        data[0x1040..0x104d].copy_from_slice(b"KERNEL32.dll\0");
        for index in 0..ENTRIES {
            data[LOOKUP + index * 8..LOOKUP + (index + 1) * 8]
                .copy_from_slice(&0x8000_0000_0000_0001u64.to_le_bytes());
        }
        let mut calls = 0usize;
        let result = super::resolve_imports_with(&mut data, (0x1000, 40), |_dll, _symbol| {
            calls += 1;
            Ok(0x7fff_1234_5000)
        });
        let error = result.expect_err("a capped array without NUL must be rejected");
        assert!(error.contains("unterminated import thunk"), "{error}");
        assert_eq!(calls, ENTRIES);
    }

    #[test]
    fn global_export_fallback_is_only_for_api_set_names() {
        let named = super::ImportSymbol::Name("VirtualAlloc".into());
        assert!(!super::api_set_fallback_allowed("KERNEL32.dll", &named));
        assert!(!super::api_set_fallback_allowed(
            "api-ms-win-core-synch-l1-2-0.dll",
            &super::ImportSymbol::Ordinal(1)
        ));
        assert!(super::api_set_fallback_allowed(
            "api-ms-win-core-synch-l1-2-0.dll",
            &named
        ));
        assert!(super::api_set_fallback_allowed(
            "ext-ms-win-test-l1-1-0.dll",
            &named
        ));
    }

    #[test]
    fn feature_api_range_rejects_unaligned_and_wrapping_pointers() {
        let size = core::mem::size_of::<hromatite_abi::FeatureApiV1>();
        assert!(super::feature_api_range_valid(0x1000, 0x1000, 0x1008));
        assert!(!super::feature_api_range_valid(0x1000, 0x1000, 0x1001));
        assert!(super::feature_api_range_valid(0x1000, size, 0x1000));
        assert!(!super::feature_api_range_valid(0x1000, size - 1, 0x1000));
        assert!(!super::feature_api_range_valid(
            0x1000,
            0x1000,
            usize::MAX - 7
        ));
    }

    #[test]
    fn feature_api_snapshot_rejects_null_lifecycle_entries() {
        use hromatite_abi::FeatureApiV1;
        let mut bytes = vec![0u8; core::mem::size_of::<FeatureApiV1>()];
        let table_size = bytes.len() as u32;
        bytes[0..4].copy_from_slice(&table_size.to_le_bytes());
        bytes[4..8].copy_from_slice(&hromatite_abi::ABI_VERSION.to_le_bytes());
        for offset in [
            core::mem::offset_of!(FeatureApiV1, init),
            core::mem::offset_of!(FeatureApiV1, dispatch),
            core::mem::offset_of!(FeatureApiV1, tick),
            core::mem::offset_of!(FeatureApiV1, configure),
        ] {
            bytes[offset..offset + 8].copy_from_slice(&0x1010u64.to_le_bytes());
        }
        assert!(
            !super::feature_api_bytes_valid(&bytes),
            "null shutdown was accepted by header-only validation"
        );
        let shutdown = core::mem::offset_of!(FeatureApiV1, shutdown);
        bytes[shutdown..shutdown + 8].copy_from_slice(&0x1010u64.to_le_bytes());
        assert!(super::feature_api_bytes_valid(&bytes));
        bytes[4..8].copy_from_slice(&0u32.to_le_bytes());
        assert!(!super::feature_api_bytes_valid(&bytes));
    }

    #[test]
    fn protection_plan_rejects_writable_data_sharing_a_code_page() {
        let mut data = fixture();
        data[0x86..0x88].copy_from_slice(&2u16.to_le_bytes());
        data[0xd0..0xd4].copy_from_slice(&0x3000u32.to_le_bytes());
        data[0x1b0..0x1b5].copy_from_slice(b".data");
        data[0x1b8..0x1bc].copy_from_slice(&0x100u32.to_le_bytes());
        data[0x1bc..0x1c0].copy_from_slice(&0x1400u32.to_le_bytes());
        data[0x1c0..0x1c4].copy_from_slice(&0x200u32.to_le_bytes());
        data[0x1c4..0x1c8].copy_from_slice(&0x400u32.to_le_bytes());
        data[0x1d4..0x1d8].copy_from_slice(&0xc000_0040u32.to_le_bytes());
        let pe = PeImage::from_bytes(data).unwrap();
        let error = super::protection_plan(&pe).expect_err("VirtualProtect is page granular");
        assert!(
            error.contains("page") && error.contains("protection"),
            "{error}"
        );
    }

    #[test]
    fn builder_rejects_executable_images() {
        let mut data = fixture();
        data[0x96..0x98].copy_from_slice(&0x0022u16.to_le_bytes());
        assert!(!PeImage::from_bytes(data.clone()).unwrap().is_dll);
        let pe = PeImage::from_bytes(data).unwrap();
        let error = super::PreparedImage::from_pe(&pe, 0x140000000)
            .err()
            .unwrap();
        assert!(error.contains("DLL"), "{error}");
    }

    #[test]
    fn builder_rejects_images_that_are_not_amd64() {
        let mut data = fixture();
        data[0x84..0x86].copy_from_slice(&0x14cu16.to_le_bytes());
        let pe = PeImage::from_bytes(data).unwrap();
        let error = super::PreparedImage::from_pe(&pe, 0x140000000)
            .err()
            .unwrap();
        assert!(error.contains("AMD64"), "{error}");
    }
}
