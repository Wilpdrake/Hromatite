//! Narrow, safe wrappers for the Win32 calls needed outside the raw modules.
//! Each wrapper owns the complete pointer/buffer setup and checks OS results.

use crate::raw::ffi;

pub fn current_process_id() -> u32 {
    // SAFETY: parameterless Win32 query with no pointer preconditions.
    unsafe { ffi::GetCurrentProcessId() }
}

pub fn current_thread_id() -> u32 {
    // SAFETY: parameterless Win32 query with no pointer preconditions.
    unsafe { ffi::GetCurrentThreadId() }
}

/// Thread ids owned by this process. Allocation is limited by the OS snapshot.
pub fn live_thread_ids() -> Vec<u32> {
    let mut out = Vec::new();
    // SAFETY: the snapshot is closed on every path; each THREADENTRY32 remains
    // alive and correctly sized for every First/Next call.
    unsafe {
        let snap = ffi::CreateToolhelp32Snapshot(ffi::TH32CS_SNAPTHREAD, 0);
        if snap.is_null() || snap == ffi::INVALID_HANDLE_VALUE {
            return out;
        }
        let me = ffi::GetCurrentProcessId();
        let mut e = ffi::THREADENTRY32 {
            dwSize: core::mem::size_of::<ffi::THREADENTRY32>() as u32,
            ..Default::default()
        };
        if ffi::Thread32First(snap, &mut e) != 0 {
            loop {
                if e.th32OwnerProcessID == me && e.th32ThreadID != 0 {
                    out.push(e.th32ThreadID);
                }
                e.dwSize = core::mem::size_of::<ffi::THREADENTRY32>() as u32;
                if ffi::Thread32Next(snap, &mut e) == 0 {
                    break;
                }
            }
        }
        ffi::CloseHandle(snap);
    }
    out
}

/// Raw `WaitForSingleObject` result, or `None` for an invalid/sentinel handle.
pub fn wait_handle(handle: u64, timeout_ms: u32) -> Option<u32> {
    if handle == 0 || handle >= u64::MAX - 1 {
        return None;
    }
    // SAFETY: the integer came from a live GC record and is used only as an
    // opaque OS handle for a nonblocking query; it is never closed by us.
    Some(unsafe { ffi::WaitForSingleObject(handle as ffi::HANDLE, timeout_ms) })
}

/// Exit code of a thread referenced by an opaque OS handle.
pub fn thread_exit_code(handle: u64) -> Option<u32> {
    if handle == 0 || handle >= u64::MAX - 1 {
        return None;
    }
    let mut code = 0u32;
    // SAFETY: `code` is a valid output pointer and the syscall is synchronous;
    // the handle is borrowed from the GC record and is not closed here.
    (unsafe { ffi::GetExitCodeThread(handle as ffi::HANDLE, &mut code) } != 0).then_some(code)
}

/// Win32 start address recorded for a thread.
pub fn thread_start_address(tid: u32) -> Option<usize> {
    // SAFETY: the opened thread handle is always closed; both output pointers
    // reference live locals of the exact size passed to NtQueryInformationThread.
    unsafe {
        let h = ffi::OpenThread(ffi::THREAD_QUERY_INFORMATION, 0, tid);
        if h.is_null() || h == ffi::INVALID_HANDLE_VALUE {
            return None;
        }
        let mut start = 0usize;
        let mut ret = 0u32;
        let status = ffi::NtQueryInformationThread(
            h,
            9, // ThreadQuerySetWin32StartAddress
            &mut start as *mut usize as *mut core::ffi::c_void,
            core::mem::size_of::<usize>() as u32,
            &mut ret,
        );
        ffi::CloseHandle(h);
        (status == 0 && start != 0).then_some(start)
    }
}

/// Resolve a NUL-terminated export name in a loaded module.
pub fn export_address(module: &str, name: &str) -> Option<usize> {
    // SAFETY: both encoding helpers append NUL and remain alive through the
    // calls; GetModuleHandleW/GetProcAddress borrow their inputs synchronously.
    unsafe {
        let module = ffi::GetModuleHandleW(ffi::wide(module).as_ptr());
        if module.is_null() {
            return None;
        }
        let p = ffi::GetProcAddress(module, ffi::cstr(name).as_ptr());
        (!p.is_null()).then_some(p as usize)
    }
}

/// Invoke the actual Win32 ResumeThread export with an opaque borrowed handle.
pub fn resume_thread(handle: u64) -> Option<i32> {
    let addr = export_address("kernel32.dll", "ResumeThread")?;
    // SAFETY: export_address resolves exactly kernel32!ResumeThread, whose ABI
    // is one HANDLE argument. The GC-owned handle is borrowed, never closed here.
    let f: unsafe extern "system" fn(ffi::HANDLE) -> i32 = unsafe { core::mem::transmute(addr) };
    // SAFETY: the handle originates from a GC thread record and is only borrowed.
    Some(unsafe { f(handle as ffi::HANDLE) })
}

/// UTF-16LE path/argument bytes owned safely by the caller.
pub fn wide_bytes(s: &str) -> Vec<u8> {
    ffi::wide(s)
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect()
}
