//! Process and module discovery through Toolhelp32. Works inside a wine
//! prefix the same way it works on Windows: the "processes" are wine's.

use crate::ffi::*;

pub fn find_pid(name: &str) -> Option<u32> {
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut e = PROCESSENTRY32W::default();
        e.dwSize = core::mem::size_of::<PROCESSENTRY32W>() as u32;
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
    unsafe {
        let mut out = Vec::new();
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut e = PROCESSENTRY32W::default();
        e.dwSize = core::mem::size_of::<PROCESSENTRY32W>() as u32;
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
    unsafe {
        let mut out = Vec::new();
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut e = MODULEENTRY32W::default();
        e.dwSize = core::mem::size_of::<MODULEENTRY32W>() as u32;
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

fn read_ptr(addr: usize) -> usize {
    unsafe { *(addr as *const usize) }
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
    unsafe {
        let peb: usize;
        core::arch::asm!("mov {}, gs:[0x60]", out(reg) peb, options(nostack, nomem, preserves_flags));
        if peb == 0 {
            return false;
        }
        let ldr = read_ptr(peb + 0x18);
        if ldr == 0 {
            return false;
        }
        let head = ldr + 0x20; // InMemoryOrderModuleList
        let mut cur = read_ptr(head);
        let mut walked = 0;
        // NOTE: `cur` points at the InMemoryOrderLinks field, which sits 0x10
        // bytes into LDR_DATA_TABLE_ENTRY, so every field offset is shifted by
        // -0x10: DllBase = +0x20, SizeOfImage = +0x28, BaseDllName = +0x48.
        while cur != head && cur != 0 && walked < 512 {
            let base = read_ptr(cur + 0x20);
            if base == module_base {
                let flink = read_ptr(cur);
                let blink = read_ptr(cur + 8);
                // splice out
                *((blink + 0) as *mut usize) = flink;
                *((flink + 8) as *mut usize) = blink;
                // also drop from the load/in-memory lists is not needed for
                // enumeration by name; zeroing our own links keeps it tidy.
                *((cur) as *mut usize) = cur;
                *((cur + 8) as *mut usize) = cur;
                return true;
            }
            cur = read_ptr(cur);
            walked += 1;
        }
        crate::log::log(&format!(
            "peb: walked {walked} entries, self base {module_base:#x} not found (ldr {ldr:#x})"
        ));
        false
    }
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
        let p = self.protect & 0xFF;
        (p & 0x10) != 0 || p == PAGE_EXECUTE_READWRITE || p == PAGE_EXECUTE_WRITECOPY
    }
}

/// Walk our own address space with VirtualQuery. Used by the il2cpp
/// recon: the table UnityPlayer fills in is *not* inside its image, so the
/// scan has to reach the heap as well.
pub fn regions(max_regions: usize) -> Vec<Region> {
    let mut out = Vec::new();
    unsafe {
        let mut addr = 0x10000usize;
        let top = 0x0000_7FFF_FFFF_0000usize;
        while addr < top && out.len() < max_regions {
            let mut mbi = MEMORY_BASIC_INFORMATION::default();
            let n = VirtualQuery(
                addr as *const core::ffi::c_void,
                &mut mbi,
                core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            );
            if n == 0 || mbi.RegionSize == 0 {
                break;
            }
            if mbi.State == MEM_COMMIT {
                out.push(Region {
                    base: mbi.BaseAddress,
                    size: mbi.RegionSize,
                    protect: mbi.Protect,
                    ty: mbi.Type,
                });
            }
            addr = mbi.BaseAddress.wrapping_add(mbi.RegionSize);
        }
    }
    out
}

/// Module base + size inside *our own* process (used by the injected DLL).
pub fn own_module(name: &str) -> Option<(usize, usize)> {
    unsafe {
        let w = wide(name);
        let h = GetModuleHandleW(w.as_ptr());
        if h.is_null() {
            return None;
        }
        let base = h as usize;
        // SizeOfImage is at OptionalHeader + 56 for both PE32 and PE32+.
        let e_lfanew = *(base as *const u32).add(0x3C as usize / 4) as usize;
        let opt = base + e_lfanew + 4 + 20;
        let size = *((opt + 56) as *const u32) as usize;
        Some((base, size))
    }
}
