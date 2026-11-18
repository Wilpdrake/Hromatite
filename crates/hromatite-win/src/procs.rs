//! Process and module discovery through Toolhelp32. Works inside a wine
//! prefix the same way it works on Windows: the "processes" are wine's.

use crate::ffi::*;

pub fn find_pid(name: &str) -> Option<u32> {
    // SAFETY: Toolhelp owns the entry buffer only during each call; the snapshot
    // is closed on every path and no pointer escapes this function.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut e = PROCESSENTRY32W {
            dwSize: core::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snap, &mut e);
        let mut found = None;
        while ok != 0 {
            if wide_to_string(&e.szExeFile).eq_ignore_ascii_case(name) {
                found = Some(e.th32ProcessID);
                break;
            }
            ok = Process32NextW(snap, &mut e);
        }
        CloseHandle(snap);
        found
    }
}

pub fn all_pids() -> Vec<(u32, String)> {
    // SAFETY: each entry is initialised to the required size and the snapshot is
    // closed before returning; output strings own their UTF-16 contents.
    unsafe {
        let mut out = Vec::new();
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut e = PROCESSENTRY32W {
            dwSize: core::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snap, &mut e);
        while ok != 0 {
            out.push((e.th32ProcessID, wide_to_string(&e.szExeFile)));
            ok = Process32NextW(snap, &mut e);
        }
        CloseHandle(snap);
        out
    }
}

pub fn modules(pid: u32) -> Vec<(String, usize, usize)> {
    // SAFETY: see find_pid: entries remain alive during enumeration and the
    // Toolhelp snapshot handle is closed on all paths.
    unsafe {
        let mut out = Vec::new();
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut e = MODULEENTRY32W {
            dwSize: core::mem::size_of::<MODULEENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Module32FirstW(snap, &mut e);
        while ok != 0 {
            out.push((
                wide_to_string(&e.szModule),
                e.modBaseAddr as usize,
                e.modBaseSize as usize,
            ));
            ok = Module32NextW(snap, &mut e);
        }
        CloseHandle(snap);
        out
    }
}

pub fn module(pid: u32, name: &str) -> Option<(usize, usize)> {
    modules(pid)
        .into_iter()
        .find(|(n, _, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, b, s)| (b, s))
}

/// One word of the loader list. `None` = the address is not readable, which is
/// the normal end-of-list / corrupt-list signal (never deref without it: a bad
/// `Flink` is an AV inside `DllMain`, and the game dies with it).
fn read_ptr(addr: usize) -> Option<usize> {
    crate::memory::read_usize(addr)
}

/// Remove our own LDR_DATA_TABLE_ENTRY from the PEB module lists.
///
/// With a file-backed mapping (`LoadLibrary`) the pages look legitimate, but
/// the module is listed in the PEB — this takes it out of the enumeration while
/// the mapping itself stays section-backed.
///
/// Offsets (x64, both Windows and wine's LDR emulation):
///   PEB+0x18 → Ldr, Ldr+0x20 → InMemoryOrderModuleList,
///   entry+0x00 Flink/Blink, +0x10 DllBase, +0x20 SizeOfImage,
///   +0x30 FullDllName, +0x40 BaseDllName.
pub fn unlink_from_peb(module_base: usize) -> bool {
    use crate::memory;
    let peb: usize;
    // SAFETY: Win64 guarantees GS:[0x60] is the current thread's PEB. The asm
    // changes no flags, stack or other registers and does not dereference Rust data.
    unsafe {
        core::arch::asm!("mov {}, gs:[0x60]", out(reg) peb, options(nostack, nomem, preserves_flags));
    }
    if peb == 0 {
        return false;
    }
    // Every step of the walk is a guarded read: the loader list is ours to
    // corrupt if we splice it wrong, and a stale Flink is an AV in DllMain.
    let ldr = match read_ptr(peb + 0x18) {
        Some(v) if v != 0 => v,
        _ => return false,
    };
    let head = match ldr.checked_add(0x20) {
        Some(h) => h, // InMemoryOrderModuleList
        None => return false,
    };
    let mut cur = match read_ptr(head) {
        Some(v) => v,
        None => return false,
    };
    let mut walked = 0;
    // NOTE: `cur` points at the InMemoryOrderLinks field, which sits 0x10
    // bytes into LDR_DATA_TABLE_ENTRY, so every field offset is shifted by
    // -0x10: DllBase = +0x20, SizeOfImage = +0x28, BaseDllName = +0x48.
    while cur != head && cur != 0 && walked < 512 {
        let base = match read_ptr(cur + 0x20) {
            Some(v) => v,
            None => break,
        };
        if base == module_base {
            let flink = match read_ptr(cur) {
                Some(v) => v,
                None => break,
            };
            let blink = match read_ptr(cur + 8) {
                Some(v) => v,
                None => break,
            };
            // splice out — only if all four writes land in readable memory
            let ok = memory::write_usize(blink, flink)
                && memory::write_usize(flink.wrapping_add(8), blink)
                // drop from the load/in-memory lists is not needed for
                // enumeration by name; zeroing our own links keeps it tidy.
                && memory::write_usize(cur, cur)
                && memory::write_usize(cur + 8, cur);
            if !ok {
                crate::log::log(&format!(
                    "peb: entry for {module_base:#x} found at {cur:#x} but a link is unreadable"
                ));
            }
            return ok;
        }
        cur = match read_ptr(cur) {
            Some(v) => v,
            None => break,
        };
        walked += 1;
    }
    crate::log::log(&format!(
        "peb: walked {walked} entries, self base {module_base:#x} not found (ldr {ldr:#x})"
    ));
    false
}

/// One committed, readable region of our own address space.
#[derive(Clone, Copy)]
pub struct Region {
    pub base: usize,
    pub size: usize,
    pub protect: u32,
    pub ty: u32,
}

impl Region {
    pub fn readable(&self) -> bool {
        let p = self.protect & 0xFF;
        p != PAGE_NOACCESS && p != 0 && (self.protect & PAGE_GUARD) == 0
    }
    pub fn executable(&self) -> bool {
        // All four executable page protections, not just `PAGE_EXECUTE`: a
        // module's `.text` is normally `PAGE_EXECUTE_READ` (0x20), which the old
        // `p & 0x10` test missed — that made `d3d11::find_swapchain` see no
        // executable region in `dxgi.dll` at all.
        matches!(
            self.protect & 0xFF,
            PAGE_EXECUTE | PAGE_EXECUTE_READ | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY
        )
    }
}

/// Walk our own address space with VirtualQuery. Used by the il2cpp
/// recon: the table UnityPlayer fills in is *not* inside its image, so the
/// scan has to reach the heap as well.
pub fn regions(max_regions: usize) -> Vec<Region> {
    let mut out = Vec::new();
    let mut addr = 0x10000usize;
    let top = 0x0000_7FFF_FFFF_0000usize;
    while addr < top && out.len() < max_regions {
        let Some(region) = crate::memory::query_region(addr) else {
            break;
        };
        if region.is_committed() {
            out.push(Region {
                base: region.base,
                size: region.size,
                protect: region.protect,
                ty: region.ty,
            });
        }
        let next = region.base.wrapping_add(region.size);
        if next <= addr {
            break;
        }
        addr = next;
    }
    out
}

/// Claim a process-wide named object so a second copy of the payload stays
/// dormant.
///
/// Manual mapping does not register the module in the PEB, so a retrying
/// launcher cannot see an already injected copy and maps another one (measured:
/// three copies in one process, each installing its own VEH and hooks). A named
/// mutex lives in the wine server object namespace, so every copy sees it no
/// matter how it was loaded.
///
/// Returns `true` when this call created the object (this is the first copy),
/// `false` when another copy already holds it. Failure to create the object is
/// treated as "first copy": this guard must never block the payload.
pub fn claim_single_instance(name: &str) -> bool {
    const ERROR_ALREADY_EXISTS: u32 = 183;
    let wide_name = wide(name);
    // SAFETY: `wide_name` is NUL-terminated UTF-16 and lives for the call; the
    // returned handle is null or an owned reference to a kernel object.
    let handle = unsafe { CreateMutexW(core::ptr::null(), 0, wide_name.as_ptr()) };
    if handle.is_null() {
        return true;
    }
    // SAFETY: GetLastError has no preconditions and is read immediately after
    // the call that may have set it.
    let already = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already {
        // We only opened the existing object, so drop our reference to it.
        // SAFETY: the handle came from CreateMutexW and is not used afterwards.
        unsafe {
            CloseHandle(handle);
        }
        return false;
    }
    // Deliberately not closed: the object must outlive this call so the next
    // copy sees it. A HANDLE is a raw pointer, so nothing is leaked by Rust.
    let _ = handle;
    true
}

/// Full path of a module loaded in *our own* process (used to hash a build).
pub fn own_module_path(name: &str) -> Option<std::path::PathBuf> {
    let w = wide(name);
    // SAFETY: `w` is NUL-terminated UTF-16 and remains alive throughout the call;
    // the returned handle is process-owned and never closed here.
    let module = unsafe { GetModuleHandleW(w.as_ptr()) };
    if module.is_null() {
        return None;
    }
    let mut buffer = [0u16; 4096];
    // SAFETY: `buffer` is a correctly sized writable UTF-16 destination.
    let length = unsafe { GetModuleFileNameW(module, buffer.as_mut_ptr(), buffer.len() as u32) }
        as usize;
    if length == 0 || length >= buffer.len() {
        return None;
    }
    Some(std::path::PathBuf::from(wide_to_string(&buffer[..length])))
}

/// Module base + size inside *our own* process (used by the injected DLL).
pub fn own_module(name: &str) -> Option<(usize, usize)> {
    let base = crate::sys::module_handle(name)?;
    let size = crate::pe::live_image_size(base)?;
    Some((base, size))
}

#[cfg(test)]
mod region_tests {
    use super::Region;

    fn region(protect: u32) -> Region {
        Region {
            base: 0x1000,
            size: 0x1000,
            protect,
            ty: 0x1000,
        }
    }

    #[test]
    fn every_executable_page_protection_is_recognised() {
        // `.text` is normally PAGE_EXECUTE_READ; missing it made the swapchain
        // scan report "no executable region" for dxgi.dll.
        for protect in [0x10, 0x20, 0x40, 0x80] {
            assert!(
                region(protect).executable(),
                "{protect:#x} must be executable"
            );
        }
        for protect in [0x01, 0x02, 0x04, 0x08, 0x00] {
            assert!(
                !region(protect).executable(),
                "{protect:#x} must not be executable"
            );
        }
    }

    #[test]
    fn guard_pages_are_not_readable() {
        assert!(!region(0x104).readable(), "PAGE_GUARD|PAGE_READWRITE");
        assert!(region(0x04).readable());
        assert!(!region(0x01).readable());
    }
}
