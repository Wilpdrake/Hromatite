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
    crate::safe::read_usize(addr)
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
    use crate::safe;
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
            let ok = safe::write_usize(blink, flink)
                && safe::write_usize(flink.wrapping_add(8), blink)
                // drop from the load/in-memory lists is not needed for
                // enumeration by name; zeroing our own links keeps it tidy.
                && safe::write_usize(cur, cur)
                && safe::write_usize(cur + 8, cur);
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
        let p = self.protect & 0xFF;
        (p & 0x10) != 0 || p == PAGE_EXECUTE_READWRITE || p == PAGE_EXECUTE_WRITECOPY
    }
}

/// Walk our own address space with VirtualQuery. Used by the il2cpp
/// recon: the table UnityPlayer fills in is *not* inside its image, so the
/// scan has to reach the heap as well.
pub fn regions(max_regions: usize) -> Vec<Region> {
    let mut out = Vec::new();
    // SAFETY: every iteration provides a correctly sized MBI output buffer;
    // address progression is bounded by `top` and stops on a failed/empty query.
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

/// Module base + size inside *our own* process (used by the injected DLL).
pub fn own_module(name: &str) -> Option<(usize, usize)> {
    let w = wide(name);
    // SAFETY: `w` is NUL-terminated UTF-16 and remains alive throughout the call.
    let h = unsafe { GetModuleHandleW(w.as_ptr()) };
    if h.is_null() {
        return None;
    }
    let base = h as usize;
    // SizeOfImage is at OptionalHeader + 56 for both PE32 and PE32+.
    // Read the header through `safe`: wine's apiset "handles" are not images.
    let e_lfanew = crate::safe::read_u32(base + 0x3C)? as usize;
    let opt = base.checked_add(e_lfanew)?.checked_add(4 + 20)?;
    let size = crate::safe::read_u32(opt + 56)? as usize;
    Some((base, size))
}
