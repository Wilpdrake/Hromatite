//! Process-wide named shared hook table (`Local\hromatite_rust_hooktable_v1`).
//!
//! Records `{target -> trampoline, handler}` so a second injection or reload
//! into the same process reuses the existing trampoline instead of re-hooking
//! already patched prologue bytes.

#![cfg_attr(not(windows), allow(dead_code))]

/// One row of the named shared table.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TableEntry {
    /// Target entry address.
    pub target: u64,
    /// Trampoline address.
    pub tramp: u64,
    /// Handler the patch currently jumps to.
    pub handler: u64,
}

#[cfg(windows)]
const TABLE_SIG: u32 = 0x5254_4B48; // 'H K T R'
#[cfg(windows)]
const TABLE_SLOTS: usize = 64;

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct RawEntry {
    target: u64,
    tramp: u64,
    handler: u64,
    used: u32,
    name: [u8; 52],
}

#[cfg(windows)]
#[repr(C)]
struct RawTable {
    sig: u32,
    count: u32,
    entries: [RawEntry; TABLE_SLOTS],
}

#[cfg(windows)]
fn table() -> Option<&'static mut RawTable> {
    use hromatite_win::ffi;
    use std::sync::OnceLock;
    static TABLE: OnceLock<usize> = OnceLock::new();
    let address = *TABLE.get_or_init(|| {
        // SAFETY: named mapping creation is process-wide; a null file handle
        // (or INVALID_HANDLE_VALUE) backs it by the system paging file, and the
        // name is a NUL-terminated ASCII literal.
        let mapping = unsafe {
            ffi::CreateFileMappingA(
                core::ptr::null_mut(),
                core::ptr::null(),
                ffi::PAGE_READWRITE,
                0,
                core::mem::size_of::<RawTable>() as u32,
                c"Local\\hromatite_rust_hooktable_v1".as_ptr(),
            )
        };
        if mapping.is_null() {
            return 0;
        }
        // SAFETY: the mapping handle is valid for the whole process.
        let view = unsafe { ffi::MapViewOfFile(mapping, ffi::FILE_MAP_ALL_ACCESS, 0, 0, 0) };
        view as usize
    });
    if address == 0 {
        return None;
    }
    // SAFETY: the view is a writable RawTable-sized region mapped above.
    let table = unsafe { &mut *(address as *mut RawTable) };
    if table.sig != TABLE_SIG {
        // SAFETY: fresh mapping is zeroed; claim it once.
        unsafe {
            core::ptr::write_bytes(table as *mut RawTable, 0, 1);
        }
        table.sig = TABLE_SIG;
    }
    Some(table)
}

/// Look up an existing hook entry for `target` in the process-wide shared table.
#[cfg(windows)]
pub fn lookup(target: usize) -> Option<TableEntry> {
    let table = table()?;
    table
        .entries
        .iter()
        .take(TABLE_SLOTS)
        .find(|entry| entry.used != 0 && entry.target == target as u64)
        .map(|entry| TableEntry {
            target: entry.target,
            tramp: entry.tramp,
            handler: entry.handler,
        })
}

#[cfg(not(windows))]
pub fn lookup(_target: usize) -> Option<TableEntry> {
    None
}

/// Record or update `(target, tramp, handler, name)` in the process-wide shared table.
#[cfg(windows)]
pub fn store(target: usize, tramp: usize, handler: usize, name: &str) {
    let Some(table) = table() else { return };
    let slot = table
        .entries
        .iter()
        .position(|entry| entry.used != 0 && entry.target == target as u64)
        .or_else(|| table.entries.iter().position(|entry| entry.used == 0));
    let Some(index) = slot else { return };
    let entry = &mut table.entries[index];
    entry.target = target as u64;
    entry.tramp = tramp as u64;
    entry.handler = handler as u64;
    entry.used = 1;
    let bytes = name.as_bytes();
    let len = bytes.len().min(entry.name.len() - 1);
    entry.name[..len].copy_from_slice(&bytes[..len]);
    entry.name[len] = 0;
    table.count = table.entries.iter().filter(|entry| entry.used != 0).count() as u32;
}

#[cfg(not(windows))]
pub fn store(_target: usize, _tramp: usize, _handler: usize, _name: &str) {}
