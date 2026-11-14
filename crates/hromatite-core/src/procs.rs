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
