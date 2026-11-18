//! Import-address-table (IAT) hooks of the *calling* modules.

#![cfg_attr(not(windows), allow(dead_code))]

use core::sync::atomic::{AtomicUsize, Ordering};

/// `IMAGE_ORDINAL_FLAG64`: the thunk names an export by ordinal, not by RVA.
const IMAGE_ORDINAL_FLAG64: u64 = 1 << 63;
/// `IMAGE_IMPORT_DESCRIPTOR` is five `u32`s.
const DESC_LEN: usize = 20;
/// Bound on the descriptor chain: a corrupt terminator must not walk forever.
const MAX_DESCRIPTORS: usize = 512;
/// Bound on one thunk array (a PE never imports thousands from one DLL).
const MAX_THUNKS: usize = 4096;
/// Bound on the exporter's name table.
const MAX_EXPORTS: usize = 4096;
/// Longest import/export name we accept.
const MAX_NAME: usize = 128;

// --------------------------------------------------------------- reader layer

/// Byte access to a foreign (or synthetic) image.
pub trait Reader {
    /// Read `len` bytes at `addr`, or `None` when the range is unreadable.
    fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>>;
}

fn u16_at<R: Reader + ?Sized>(reader: &R, addr: usize) -> Option<u16> {
    let bytes = reader.read(addr, 2)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn u32_at<R: Reader + ?Sized>(reader: &R, addr: usize) -> Option<u32> {
    let bytes = reader.read(addr, 4)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn u64_at<R: Reader + ?Sized>(reader: &R, addr: usize) -> Option<u64> {
    let bytes = reader.read(addr, 8)?;
    let mut word = [0u8; 8];
    word.copy_from_slice(&bytes);
    Some(u64::from_le_bytes(word))
}

fn usize_at<R: Reader + ?Sized>(reader: &R, addr: usize) -> Option<usize> {
    u64_at(reader, addr).map(|value| value as usize)
}

fn cstr_at<R: Reader + ?Sized>(reader: &R, addr: usize, max: usize) -> Option<String> {
    let max = max.min(MAX_NAME);
    let mut text = Vec::with_capacity(max.min(48));
    while text.len() < max {
        let at = addr.checked_add(text.len())?;
        let chunk = reader.read(at, 8)?;
        for byte in chunk {
            if byte == 0 {
                return (!text.is_empty()).then(|| String::from_utf8_lossy(&text).into_owned());
            }
            if !(32..127).contains(&byte) {
                return None;
            }
            text.push(byte);
        }
    }
    None
}

/// Import/export names are compared case-insensitively and without the `.dll`
/// suffix or a directory prefix.
pub fn same_dll(imported: &str, wanted: &str) -> bool {
    fn base_name(name: &str) -> &str {
        name.rsplit(['\\', '/']).next().unwrap_or(name).trim()
    }
    fn strip_suffix(name: &str) -> &str {
        let bytes = name.as_bytes();
        if bytes.len() > 4 && bytes[bytes.len() - 4..].eq_ignore_ascii_case(b".dll") {
            &name[..name.len() - 4]
        } else {
            name
        }
    }
    let (imported, wanted) = (base_name(imported), base_name(wanted));
    imported.eq_ignore_ascii_case(wanted)
        || strip_suffix(imported).eq_ignore_ascii_case(strip_suffix(wanted))
}

// ------------------------------------------------------------ descriptors

struct Descriptor {
    name: u32,
    original_first_thunk: u32,
    first_thunk: u32,
}

fn image_nt<R: Reader + ?Sized>(reader: &R, base: usize) -> Option<usize> {
    let lfanew = u32_at(reader, base.checked_add(0x3C)?)? as usize;
    let nt = base.checked_add(lfanew)?;
    if u32_at(reader, nt)? != 0x0000_4550 {
        return None;
    }
    let magic = u16_at(reader, nt.checked_add(24)?)?;
    (magic == 0x20B || magic == 0x10B).then_some(nt)
}

fn data_directory<R: Reader + ?Sized>(reader: &R, nt: usize, index: usize) -> Option<u32> {
    let magic = u16_at(reader, nt.checked_add(24)?)?;
    let directories = match magic {
        0x20B => 112,
        0x10B => 96,
        _ => return None,
    };
    u32_at(reader, nt.checked_add(24 + directories + index * 8)?)
}

fn descriptors<R: Reader + ?Sized>(reader: &R, base: usize) -> Vec<Descriptor> {
    let mut out = Vec::new();
    let Some(nt) = image_nt(reader, base) else {
        return out;
    };
    let Some(import_rva) = data_directory(reader, nt, 1) else {
        return out;
    };
    if import_rva == 0 {
        return out;
    }
    let Some(start) = base.checked_add(import_rva as usize) else {
        return out;
    };
    for index in 0..MAX_DESCRIPTORS {
        let Some(at) = start.checked_add(index * DESC_LEN) else {
            break;
        };
        let (Some(original_first_thunk), Some(name), Some(first_thunk)) = (
            u32_at(reader, at),
            u32_at(reader, at + 12),
            u32_at(reader, at + 16),
        ) else {
            break;
        };
        if name == 0 && first_thunk == 0 {
            break;
        }
        out.push(Descriptor {
            name,
            original_first_thunk,
            first_thunk,
        });
    }
    out
}

// -------------------------------------------------------------- exporter names

/// Export names indexed by ordinal index, for matching ordinal imports.
pub struct OrdinalNames {
    base: u32,
    names: Vec<Option<String>>,
}

impl OrdinalNames {
    pub fn name(&self, ordinal: u16) -> Option<&str> {
        let index = (u32::from(ordinal)).checked_sub(self.base)? as usize;
        self.names.get(index)?.as_deref()
    }
}

/// Parse the export directory of the module at `base` into an ordinal→name map.
pub fn exporter_ordinals<R: Reader + ?Sized>(reader: &R, base: usize) -> Option<OrdinalNames> {
    let nt = image_nt(reader, base)?;
    let export_rva = data_directory(reader, nt, 0)? as usize;
    if export_rva == 0 {
        return None;
    }
    let directory = base.checked_add(export_rva)?;
    let ordinal_base = u32_at(reader, directory.checked_add(16)?)?;
    let count = u32_at(reader, directory.checked_add(24)?)? as usize;
    let names_rva = u32_at(reader, directory.checked_add(32)?)?;
    let ordinals_rva = u32_at(reader, directory.checked_add(36)?)?;
    if count == 0 || count > MAX_EXPORTS || names_rva == 0 || ordinals_rva == 0 {
        return None;
    }
    let mut names = vec![None; count];
    for index in 0..count {
        let Some(name_rva) = u32_at(
            reader,
            base.checked_add(names_rva as usize)?
                .checked_add(index * 4)?,
        ) else {
            break;
        };
        let Some(ordinal_index) = u16_at(
            reader,
            base.checked_add(ordinals_rva as usize)?
                .checked_add(index * 2)?,
        ) else {
            break;
        };
        let Some(name) = base
            .checked_add(name_rva as usize)
            .and_then(|at| cstr_at(reader, at, MAX_NAME))
        else {
            continue;
        };
        if let Some(cell) = names.get_mut(ordinal_index as usize) {
            *cell = Some(name);
        }
    }
    Some(OrdinalNames {
        base: ordinal_base,
        names,
    })
}

// ------------------------------------------------------------------ slot walk

/// One IAT entry that imports the wanted symbol from the wanted DLL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub slot: usize,
    pub name: Option<String>,
    pub ordinal: Option<u16>,
}

impl Slot {
    pub fn label(&self) -> String {
        match (&self.name, self.ordinal) {
            (Some(name), _) => name.clone(),
            (None, Some(ordinal)) => format!("#{ordinal}"),
            (None, None) => "?".to_string(),
        }
    }
}

/// What to look for inside one image.
pub struct Want<'a> {
    pub dll: &'a str,
    pub func: &'a str,
    pub address: Option<usize>,
}

/// Every IAT slot of the image at `base` that imports `want.func` from `want.dll`.
pub fn slots<R: Reader + ?Sized>(
    reader: &R,
    base: usize,
    size: usize,
    want: &Want<'_>,
    ordinals: Option<&OrdinalNames>,
) -> Vec<Slot> {
    let mut out = Vec::new();
    let as_ordinal = want
        .func
        .strip_prefix('#')
        .and_then(|digits| digits.parse::<u16>().ok());
    for descriptor in descriptors(reader, base) {
        let Some(dll_name) = base
            .checked_add(descriptor.name as usize)
            .and_then(|at| cstr_at(reader, at, MAX_NAME))
        else {
            continue;
        };
        if !same_dll(&dll_name, want.dll) {
            continue;
        }
        let named = descriptor.original_first_thunk != 0;
        let lookup_rva = if named {
            descriptor.original_first_thunk
        } else {
            descriptor.first_thunk
        };
        let Some(lookup_base) = base.checked_add(lookup_rva as usize) else {
            continue;
        };
        for index in 0..MAX_THUNKS {
            let Some(entry) = u64_at(reader, lookup_base + index * 8) else {
                break;
            };
            if entry == 0 {
                break;
            }
            let Some(slot) = base.checked_add(descriptor.first_thunk as usize + index * 8) else {
                break;
            };
            if size != 0 && slot.saturating_sub(base) >= size {
                break;
            }
            let by_ordinal = named && entry & IMAGE_ORDINAL_FLAG64 != 0;
            if by_ordinal {
                let ordinal = (entry & 0xFFFF) as u16;
                let resolved = ordinals.and_then(|names| names.name(ordinal));
                if resolved == Some(want.func) || as_ordinal == Some(ordinal) {
                    out.push(Slot {
                        slot,
                        name: resolved.map(str::to_string),
                        ordinal: Some(ordinal),
                    });
                }
                continue;
            }
            if named {
                let name = base
                    .checked_add(entry as u32 as usize)
                    .and_then(|at| cstr_at(reader, at, MAX_NAME));
                if name.as_deref() == Some(want.func) {
                    out.push(Slot {
                        slot,
                        name,
                        ordinal: None,
                    });
                }
                continue;
            }
            if want
                .address
                .is_some_and(|address| entry as usize == address)
            {
                out.push(Slot {
                    slot,
                    name: None,
                    ordinal: None,
                });
            }
        }
    }
    out
}

/// First matching slot of the image, for diagnostics.
pub fn first_slot<R: Reader + ?Sized>(
    reader: &R,
    base: usize,
    size: usize,
    want: &Want<'_>,
    ordinals: Option<&OrdinalNames>,
) -> Option<Slot> {
    slots(reader, base, size, want, ordinals).into_iter().next()
}

/// Does the image at `base` import anything at all from `dll`?
pub fn imports_dll<R: Reader + ?Sized>(reader: &R, base: usize, dll: &str) -> bool {
    descriptors(reader, base).iter().any(|descriptor| {
        base.checked_add(descriptor.name as usize)
            .and_then(|at| cstr_at(reader, at, MAX_NAME))
            .is_some_and(|name| same_dll(&name, dll))
    })
}

// ---------------------------------------------------------------- live layer

/// Result of one intended write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outcome {
    pub func: String,
    pub slot: usize,
    pub old: usize,
    pub new: usize,
    pub ok: bool,
    pub detail: String,
}

impl Outcome {
    pub fn line(&self, module: &str, dll: &str) -> String {
        format!(
            "{module} {dll}!{} slot={:#x} old={:#x} new={:#x} ok={} {}",
            self.func, self.slot, self.old, self.new, self.ok, self.detail
        )
    }
}

static PATCHES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
static PATCHED: AtomicUsize = AtomicUsize::new(0);

fn note(line: String) {
    if let Ok(mut lines) = PATCHES.lock() {
        lines.push(line);
    }
}

/// Diagnostics for `eac.hooks`: one line per slot plus the running total.
pub fn status() -> String {
    let lines = PATCHES
        .lock()
        .map(|lines| lines.clone())
        .unwrap_or_default();
    if lines.is_empty() {
        return format!(
            "iat: no slots patched ({})",
            PATCHED.load(Ordering::Relaxed)
        );
    }
    format!(
        "iat: patched={}\n{}",
        PATCHED.load(Ordering::Relaxed),
        lines.join("\n")
    )
}

#[cfg(windows)]
fn locate(module: &str) -> Option<(usize, usize)> {
    if let Some(found) = hromatite_win::procs::own_module(module) {
        return Some(found);
    }
    let pid = hromatite_win::sys::current_process_id();
    hromatite_win::procs::module(pid, module)
}

#[cfg(windows)]
struct Live;

#[cfg(windows)]
impl Reader for Live {
    fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>> {
        hromatite_win::memory::read_bytes(addr, len)
    }
}

#[cfg(windows)]
fn write_slot(slot: usize, value: usize) -> Result<String, String> {
    use hromatite_win::{ffi, memory as safe};
    let size = core::mem::size_of::<usize>();
    let mut previous = 0u32;
    // SAFETY: `slot` is a live 8-byte IAT entry inside a mapped image and
    // `previous` is a valid out-pointer for the old protection.
    let opened = unsafe {
        ffi::VirtualProtectEx(
            ffi::GetCurrentProcess(),
            slot as *mut u8,
            size,
            ffi::PAGE_READWRITE,
            &mut previous,
        )
    };
    if opened == 0 {
        // SAFETY: parameterless query of the calling thread's last error.
        let error = unsafe { ffi::GetLastError() };
        return Err(format!("VirtualProtectEx(rw) failed, err={error}"));
    }
    let wrote =
        safe::write_bytes(slot, &value.to_le_bytes()) && safe::read_usize(slot) == Some(value);
    let restore = if previous & 0xFF == 0 {
        ffi::PAGE_READONLY
    } else {
        previous
    };
    let mut scratch = 0u32;
    // SAFETY: same range we just retargeted; `scratch` receives the previous
    // (i.e. our temporary) protection and is otherwise unused.
    let restored = unsafe {
        ffi::VirtualProtectEx(
            ffi::GetCurrentProcess(),
            slot as *mut u8,
            size,
            restore,
            &mut scratch,
        )
    } != 0;
    if !wrote {
        return Err("WriteProcessMemory failed".to_string());
    }
    if !restored {
        return Ok(format!("patched, protection {restore:#x} not restored"));
    }
    Ok(String::new())
}

/// Patch every IAT slot of `module` that imports `wanted` from `dll`.
pub fn patch_module(
    module: &str,
    dll: &str,
    wanted: &[(&str, usize)],
) -> Result<Vec<Outcome>, String> {
    #[cfg(not(windows))]
    {
        let _ = (module, dll, wanted);
        Ok(Vec::new())
    }
    #[cfg(windows)]
    {
        let Some((base, size)) = locate(module) else {
            return Err(format!("{module} is not loaded"));
        };
        let reader = Live;
        let Some(exporter) = locate(dll) else {
            return Err(format!("{dll} is not loaded"));
        };
        let ordinals = exporter_ordinals(&reader, exporter.0);
        let mut out = Vec::with_capacity(wanted.len());
        for &(func, replacement) in wanted {
            let want = Want {
                dll,
                func,
                address: hromatite_win::sys::export_address(dll, func),
            };
            let found = slots(&reader, base, size, &want, ordinals.as_ref());
            if found.is_empty() {
                out.push(Outcome {
                    func: func.to_string(),
                    slot: 0,
                    old: 0,
                    new: replacement,
                    ok: false,
                    detail: format!("no {dll}!{func} import slot"),
                });
                continue;
            }
            for entry in found {
                let Some(old) = usize_at(&reader, entry.slot) else {
                    out.push(Outcome {
                        func: func.to_string(),
                        slot: entry.slot,
                        old: 0,
                        new: replacement,
                        ok: false,
                        detail: "slot unreadable".to_string(),
                    });
                    continue;
                };
                if old == replacement {
                    out.push(Outcome {
                        func: func.to_string(),
                        slot: entry.slot,
                        old,
                        new: replacement,
                        ok: true,
                        detail: "already patched".to_string(),
                    });
                    continue;
                }
                let (ok, detail) = match write_slot(entry.slot, replacement) {
                    Ok(detail) => {
                        PATCHED.fetch_add(1, Ordering::Relaxed);
                        (true, detail)
                    }
                    Err(reason) => (false, reason),
                };
                let outcome = Outcome {
                    func: func.to_string(),
                    slot: entry.slot,
                    old,
                    new: replacement,
                    ok,
                    detail,
                };
                note(outcome.line(module, dll));
                out.push(outcome);
            }
        }
        Ok(out)
    }
}

/// Rewrite the first IAT slot of `module` that imports `dll!func`.
pub fn replace(module: &str, dll: &str, func: &str, replacement: usize) -> Result<usize, String> {
    let outcomes = patch_module(module, dll, &[(func, replacement)])?;
    match outcomes.into_iter().next() {
        Some(outcome) if outcome.ok => Ok(outcome.old),
        Some(outcome) => Err(outcome.detail),
        None => Err("no slot".to_string()),
    }
}

/// Diagnostic: the first slot of `module` that imports `dll!func`.
pub fn find_slot(module: &str, dll: &str, func: &str) -> Option<usize> {
    #[cfg(not(windows))]
    {
        let _ = (module, dll, func);
        None
    }
    #[cfg(windows)]
    {
        let (base, size) = locate(module)?;
        let reader = Live;
        let ordinals = locate(dll).and_then(|(base, _)| exporter_ordinals(&reader, base));
        let want = Want {
            dll,
            func,
            address: hromatite_win::sys::export_address(dll, func),
        };
        first_slot(&reader, base, size, &want, ordinals.as_ref()).map(|slot| slot.slot)
    }
}

/// Does `module` import anything from `dll`? Cheap gate before a full patch.
pub fn module_imports(module: &str, dll: &str) -> bool {
    #[cfg(not(windows))]
    {
        let _ = (module, dll);
        false
    }
    #[cfg(windows)]
    {
        let Some((base, _)) = locate(module) else {
            return false;
        };
        imports_dll(&Live, base, dll)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Image {
        base: usize,
        bytes: Vec<u8>,
    }

    impl Image {
        const BASE: usize = 0x1_4000_0000;

        fn new(len: usize) -> Self {
            Self {
                base: Self::BASE,
                bytes: vec![0u8; len],
            }
        }

        fn at(&self, rva: usize) -> usize {
            self.base + rva
        }

        fn u16(&mut self, rva: usize, value: u16) {
            self.bytes[rva..rva + 2].copy_from_slice(&value.to_le_bytes());
        }

        fn u32(&mut self, rva: usize, value: u32) {
            self.bytes[rva..rva + 4].copy_from_slice(&value.to_le_bytes());
        }

        fn u64(&mut self, rva: usize, value: u64) {
            self.bytes[rva..rva + 8].copy_from_slice(&value.to_le_bytes());
        }

        fn str(&mut self, rva: usize, value: &str) {
            self.bytes[rva..rva + value.len()].copy_from_slice(value.as_bytes());
        }

        fn headers(&mut self) {
            self.u16(0, 0x5A4D); // "MZ"
            self.u32(0x3C, 0x40); // e_lfanew
            self.u32(0x40, 0x0000_4550); // "PE\0\0"
            self.u16(0x40 + 24, 0x20B); // PE32+
            self.u32(0x40 + 24 + 56, self.bytes.len() as u32); // SizeOfImage
        }

        fn data_dir(&mut self, index: usize, rva: u32, size: u32) {
            let at = 0x40 + 24 + 112 + index * 8;
            self.u32(at, rva);
            self.u32(at + 4, size);
        }
    }

    impl Reader for Image {
        fn read(&self, addr: usize, len: usize) -> Option<Vec<u8>> {
            let offset = addr.checked_sub(self.base)?;
            self.bytes
                .get(offset..offset.checked_add(len)?)
                .map(<[u8]>::to_vec)
        }
    }

    fn caller_image() -> Image {
        let mut image = Image::new(0x1000);
        image.headers();
        image.data_dir(1, 0x200, 20 * 4);

        image.u32(0x200, 0x300);
        image.u32(0x20C, 0x400);
        image.u32(0x210, 0x500);
        image.str(0x400, "EOSSDK-Win64-Shipping.dll");

        image.u64(0x300, 0x600);
        image.u64(0x308, 0x640);
        image.u64(0x310, IMAGE_ORDINAL_FLAG64 | 26);
        image.u64(0x318, 0);
        image.str(0x600, "EOS_AntiCheatClient_BeginSession");
        image.str(0x640, "EOS_AntiCheatClient_AddNotifyMessageToServer");

        image.u64(0x500, 0x7FF0_0000_1000);
        image.u64(0x508, 0x7FF0_0000_2000);
        image.u64(0x510, 0x7FF0_0000_3000);

        image.u32(0x214, 0x280);
        image.u32(0x220, 0x420);
        image.u32(0x224, 0x520);
        image.str(0x420, "KERNEL32.dll");
        image.u64(0x280, 0x680);
        image.u64(0x288, 0);
        image.str(0x680, "EOS_AntiCheatClient_BeginSession");
        image.u64(0x520, 0x7FF0_0000_9000);
        image
    }

    fn exporter_image() -> Image {
        let mut image = Image::new(0x1000);
        image.headers();
        image.data_dir(0, 0x200, 40);
        image.u32(0x200 + 16, 25); // Base
        image.u32(0x200 + 20, 2); // NumberOfFunctions
        image.u32(0x200 + 24, 2); // NumberOfNames
        image.u32(0x200 + 28, 0x300); // AddressOfFunctions
        image.u32(0x200 + 32, 0x320); // AddressOfNames
        image.u32(0x200 + 36, 0x340); // AddressOfNameOrdinals
        image.u32(0x320, 0x400);
        image.u32(0x324, 0x440);
        image.u16(0x340, 0);
        image.u16(0x342, 1);
        image.str(0x400, "EOS_AntiCheatClient_BeginSession");
        image.str(0x440, "EOS_AntiCheatClient_ProtectMessage");
        image
    }

    fn want<'a>(dll: &'a str, func: &'a str) -> Want<'a> {
        Want {
            dll,
            func,
            address: None,
        }
    }

    #[test]
    fn a_named_import_is_found_at_its_iat_slot() {
        let image = caller_image();
        let found = slots(
            &image,
            image.base,
            image.bytes.len(),
            &want(
                "EOSSDK-Win64-Shipping.dll",
                "EOS_AntiCheatClient_BeginSession",
            ),
            None,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].slot, image.at(0x500));
        assert_eq!(
            found[0].name.as_deref(),
            Some("EOS_AntiCheatClient_BeginSession")
        );
        assert_eq!(found[0].ordinal, None);
        assert_eq!(found[0].label(), "EOS_AntiCheatClient_BeginSession");

        let second = slots(
            &image,
            image.base,
            image.bytes.len(),
            &want(
                "EOSSDK-Win64-Shipping.dll",
                "EOS_AntiCheatClient_AddNotifyMessageToServer",
            ),
            None,
        );
        assert_eq!(second.len(), 1, "the second name owns the second slot");
        assert_eq!(second[0].slot, image.at(0x508));
    }

    #[test]
    fn the_dll_name_matches_any_spelling() {
        let image = caller_image();
        for spelling in [
            "eossdk-win64-shipping.dll",
            "EOSSDK-Win64-Shipping",
            "C:\\Games\\VRChat\\EOSSDK-Win64-Shipping.DLL",
        ] {
            let found = slots(
                &image,
                image.base,
                image.bytes.len(),
                &want(spelling, "EOS_AntiCheatClient_BeginSession"),
                None,
            );
            assert_eq!(found.len(), 1, "spelling {spelling} must match");
        }
        assert!(same_dll(
            "EOSSDK-Win64-Shipping.dll",
            "eossdk-win64-shipping"
        ));
        assert!(!same_dll(
            "EOSSDK-Win64-Shipping.dll",
            "EOSSDK-Win64-Shipping2.dll"
        ));
    }

    #[test]
    fn an_ordinal_import_is_matched_through_the_exporter_names() {
        let image = caller_image();
        let exporter = exporter_image();
        let names = exporter_ordinals(&exporter, exporter.base).expect("export directory");
        assert_eq!(names.name(25), Some("EOS_AntiCheatClient_BeginSession"));
        assert_eq!(names.name(26), Some("EOS_AntiCheatClient_ProtectMessage"));
        assert_eq!(names.name(24), None, "below the ordinal base");

        let found = slots(
            &image,
            image.base,
            image.bytes.len(),
            &want(
                "EOSSDK-Win64-Shipping.dll",
                "EOS_AntiCheatClient_ProtectMessage",
            ),
            Some(&names),
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].slot, image.at(0x510));
        assert_eq!(found[0].ordinal, Some(26));
        assert_eq!(
            found[0].name.as_deref(),
            Some("EOS_AntiCheatClient_ProtectMessage")
        );

        let by_number = slots(
            &image,
            image.base,
            image.bytes.len(),
            &want("EOSSDK-Win64-Shipping.dll", "#26"),
            None,
        );
        assert_eq!(by_number.len(), 1);
        assert_eq!(by_number[0].slot, image.at(0x510));
        assert_eq!(by_number[0].label(), "#26");
    }

    #[test]
    fn an_already_bound_image_is_matched_by_value() {
        let mut image = Image::new(0x1000);
        image.headers();
        image.data_dir(1, 0x200, 40);
        image.u32(0x20C, 0x400);
        image.u32(0x210, 0x500);
        image.str(0x400, "EOSSDK-Win64-Shipping.dll");
        image.u64(0x500, 0x7FF0_0000_ABCD);
        image.u64(0x508, 0x7FF0_0000_1234);

        let mut bound = want(
            "EOSSDK-Win64-Shipping.dll",
            "EOS_AntiCheatClient_BeginSession",
        );
        bound.address = Some(0x7FF0_0000_1234);
        let found = slots(&image, image.base, image.bytes.len(), &bound, None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].slot, image.at(0x508));

        let mut wrong = want(
            "EOSSDK-Win64-Shipping.dll",
            "EOS_AntiCheatClient_BeginSession",
        );
        wrong.address = Some(0xDEAD_BEEF);
        assert!(slots(&image, image.base, image.bytes.len(), &wrong, None).is_empty());
    }

    #[test]
    fn only_the_descriptor_of_the_requested_dll_contributes() {
        let image = caller_image();
        assert!(imports_dll(&image, image.base, "eossdk-win64-shipping"));
        assert!(imports_dll(&image, image.base, "KERNEL32.dll"));
        assert!(!imports_dll(&image, image.base, "UnityPlayer.dll"));

        let kernel = slots(
            &image,
            image.base,
            image.bytes.len(),
            &want("kernel32.dll", "EOS_AntiCheatClient_BeginSession"),
            None,
        );
        assert_eq!(kernel.len(), 1);
        assert_eq!(
            kernel[0].slot,
            image.at(0x520),
            "the kernel32 slot, not EOSSDK's"
        );
    }

    #[test]
    fn an_absent_symbol_and_a_truncated_image_yield_no_slots() {
        let image = caller_image();
        assert!(
            slots(
                &image,
                image.base,
                image.bytes.len(),
                &want(
                    "EOSSDK-Win64-Shipping.dll",
                    "EOS_AntiCheatClient_EndSession"
                ),
                None
            )
            .is_empty()
        );

        let stub = Image::new(0x40);
        assert!(
            slots(
                &stub,
                stub.base,
                stub.bytes.len(),
                &want("x.dll", "y"),
                None
            )
            .is_empty()
        );
        assert!(exporter_ordinals(&stub, stub.base).is_none());
        assert_eq!(
            first_slot(&stub, stub.base, 0, &want("x.dll", "y"), None),
            None
        );
    }

    #[test]
    fn the_image_size_bounds_the_walk() {
        let image = caller_image();
        let found = slots(
            &image,
            image.base,
            0x400,
            &want(
                "EOSSDK-Win64-Shipping.dll",
                "EOS_AntiCheatClient_BeginSession",
            ),
            None,
        );
        assert!(found.is_empty());
    }

    #[test]
    fn the_outcome_line_names_module_dll_function_and_addresses() {
        let outcome = Outcome {
            func: "EOS_AntiCheatClient_BeginSession".to_string(),
            slot: 0x1234,
            old: 0xABCD,
            new: 0xEF01,
            ok: true,
            detail: String::new(),
        };
        let line = outcome.line("GameAssembly.dll", "EOSSDK-Win64-Shipping.dll");
        assert!(line.contains("GameAssembly.dll"));
        assert!(line.contains("EOSSDK-Win64-Shipping.dll!EOS_AntiCheatClient_BeginSession"));
        assert!(line.contains("old=0xabcd"), "{line}");
        assert!(line.contains("new=0xef01"), "{line}");
        assert!(status().starts_with("iat:"));
    }
}
