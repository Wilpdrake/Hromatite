//! Minimal hand-written Win32 surface. No external crates: the DLL must build
//! with nothing but mingw, and every symbol here is one we actually call.
#![allow(non_snake_case, non_camel_case_types, dead_code)]

pub type HANDLE = *mut core::ffi::c_void;
pub type HMODULE = *mut core::ffi::c_void;
pub type BOOL = i32;
pub type DWORD = u32;

pub const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
pub const TH32CS_SNAPMODULE: u32 = 0x0000_0008;
pub const TH32CS_SNAPMODULE32: u32 = 0x0000_0010;
pub const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;

pub const PROCESS_CREATE_THREAD: u32 = 0x0002;
pub const PROCESS_VM_OPERATION: u32 = 0x0008;
pub const PROCESS_VM_READ: u32 = 0x0010;
pub const PROCESS_VM_WRITE: u32 = 0x0020;
pub const PROCESS_QUERY_INFORMATION: u32 = 0x0400;

pub const MEM_COMMIT: u32 = 0x1000;
pub const MEM_RESERVE: u32 = 0x2000;
pub const MEM_RELEASE: u32 = 0x8000;

pub const PAGE_NOACCESS: u32 = 0x01;
pub const PAGE_READONLY: u32 = 0x02;
pub const PAGE_READWRITE: u32 = 0x04;
pub const PAGE_WRITECOPY: u32 = 0x08;
pub const PAGE_EXECUTE: u32 = 0x10;
pub const PAGE_EXECUTE_READ: u32 = 0x20;
pub const PAGE_EXECUTE_READWRITE: u32 = 0x40;
pub const PAGE_EXECUTE_WRITECOPY: u32 = 0x80;
pub const PAGE_GUARD: u32 = 0x100;
pub const PAGE_NOCACHE: u32 = 0x200;

pub const MEM_IMAGE: u32 = 0x0100_0000;
pub const MEM_MAPPED: u32 = 0x0040_0000;

/// x64 layout (48 bytes): every field is 8-byte aligned, the two DWORD pairs
/// are padded. Must match both Windows and wine's implementation.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct MEMORY_BASIC_INFORMATION {
    pub BaseAddress: usize,
    pub AllocationBase: usize,
    pub AllocationProtect: u32,
    pub __alignment1: u32,
    pub RegionSize: usize,
    pub State: u32,
    pub Protect: u32,
    pub Type: u32,
    pub __alignment2: u32,
}

pub const INFINITE: u32 = 0xFFFF_FFFF;
pub const DLL_PROCESS_ATTACH: u32 = 1;

/// `PROCESS_MEMORY_COUNTERS` (psapi). Used as a watchdog around calls into
/// unknown exports: one of the il2cpp API entries is an allocator, and feeding
/// it a domain pointer as a size argument OOM-killed the game (7.4 GB anon).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PROCESS_MEMORY_COUNTERS {
    pub cb: u32,
    pub PageFaultCount: u32,
    pub PeakWorkingSetSize: usize,
    pub WorkingSetSize: usize,
    pub QuotaPeakPagedPoolUsage: usize,
    pub QuotaPagedPoolUsage: usize,
    pub QuotaPeakNonPagedPoolUsage: usize,
    pub QuotaNonPagedPoolUsage: usize,
    pub PagefileUsage: usize,
    pub PeakPagefileUsage: usize,
}

// ---- exceptions / vectored handler (plan 2) -------------------------------

pub const EXCEPTION_CONTINUE_EXECUTION: i32 = -1;
pub const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
pub const EXCEPTION_EXECUTE_HANDLER: i32 = 1;

pub const EXCEPTION_ACCESS_VIOLATION: u32 = 0xC000_0005;
pub const EXCEPTION_ILLEGAL_INSTRUCTION: u32 = 0xC000_001D;
pub const EXCEPTION_PRIV_INSTRUCTION: u32 = 0xC000_0096;
pub const EXCEPTION_INT_DIVIDE_BY_ZERO: u32 = 0xC000_0094;
pub const EXCEPTION_STACK_OVERFLOW: u32 = 0xC000_00FD;

/// x64 layout: 4 DWORDs, a pointer, a counter, and 15 ulong-sized params.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EXCEPTION_RECORD {
    pub ExceptionCode: u32,
    pub ExceptionFlags: u32,
    pub ExceptionRecord: *mut EXCEPTION_RECORD,
    pub ExceptionAddress: usize,
    pub NumberParameters: u32,
    pub __pad: u32,
    pub ExceptionInformation: [usize; 15],
}

impl Default for EXCEPTION_RECORD {
    fn default() -> Self {
        // SAFETY: plain old data
        unsafe { core::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct EXCEPTION_POINTERS {
    pub ExceptionRecord: *mut EXCEPTION_RECORD,
    pub ContextRecord: *mut core::ffi::c_void,
}

impl Default for EXCEPTION_POINTERS {
    fn default() -> Self {
        // SAFETY: plain old data
        unsafe { core::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PROCESSENTRY32W {
    pub dwSize: u32,
    pub cntUsage: u32,
    pub th32ProcessID: u32,
    pub th32DefaultHeapID: usize,
    pub th32ModuleID: u32,
    pub cntThreads: u32,
    pub th32ParentProcessID: u32,
    pub pcPriClassBase: i32,
    pub dwFlags: u32,
    pub szExeFile: [u16; 260],
}

impl Default for PROCESSENTRY32W {
    fn default() -> Self {
        // SAFETY: plain old data, all-zero is a valid initial state.
        unsafe { core::mem::zeroed() }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MODULEENTRY32W {
    pub dwSize: u32,
    pub th32ModuleID: u32,
    pub th32ProcessID: u32,
    pub GlblcntUsage: u32,
    pub ProccntUsage: u32,
    pub modBaseAddr: *mut u8,
    pub modBaseSize: u32,
    pub hModule: HMODULE,
    pub szModule: [u16; 256],
    pub szExePath: [u16; 260],
}

impl Default for MODULEENTRY32W {
    fn default() -> Self {
        unsafe { core::mem::zeroed() }
    }
}

unsafe extern "system" {
    pub fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> HANDLE;
    pub fn Process32FirstW(h: HANDLE, e: *mut PROCESSENTRY32W) -> BOOL;
    pub fn Process32NextW(h: HANDLE, e: *mut PROCESSENTRY32W) -> BOOL;
    pub fn Module32FirstW(h: HANDLE, e: *mut MODULEENTRY32W) -> BOOL;
    pub fn Module32NextW(h: HANDLE, e: *mut MODULEENTRY32W) -> BOOL;

    pub fn OpenProcess(access: u32, inherit: BOOL, pid: u32) -> HANDLE;
    pub fn CloseHandle(h: HANDLE) -> BOOL;

    pub fn ReadProcessMemory(h: HANDLE, base: *const u8, buf: *mut u8, size: usize, read: *mut usize) -> BOOL;
    pub fn WriteProcessMemory(h: HANDLE, base: *mut u8, buf: *const u8, size: usize, written: *mut usize) -> BOOL;

    pub fn VirtualAlloc(addr: usize, size: usize, ty: u32, protect: u32) -> *mut u8;
    pub fn VirtualAllocEx(h: HANDLE, addr: usize, size: usize, ty: u32, protect: u32) -> *mut u8;
    pub fn VirtualProtectEx(h: HANDLE, addr: *mut u8, size: usize, newp: u32, old: *mut u32) -> BOOL;
    pub fn VirtualQuery(addr: *const core::ffi::c_void, mbi: *mut MEMORY_BASIC_INFORMATION, len: usize) -> usize;

    pub fn CreateRemoteThread(
        h: HANDLE,
        sa: usize,
        stack: usize,
        start: usize,
        param: usize,
        flags: u32,
        tid: *mut u32,
    ) -> HANDLE;
    pub fn CreateThread(
        sa: *mut core::ffi::c_void,
        stack: usize,
        start: usize,
        param: *mut core::ffi::c_void,
        flags: u32,
        tid: *mut u32,
    ) -> HANDLE;
    pub fn WaitForSingleObject(h: HANDLE, ms: u32) -> u32;
    pub fn GetExitCodeThread(h: HANDLE, code: *mut u32) -> BOOL;

    pub fn GetModuleHandleW(name: *const u16) -> HMODULE;
    pub fn GetProcAddress(m: HMODULE, name: *const u8) -> *mut core::ffi::c_void;
    pub fn LoadLibraryW(path: *const u16) -> HMODULE;
    pub fn GetModuleFileNameW(m: HMODULE, buf: *mut u16, size: u32) -> u32;
    pub fn GetLastError() -> u32;

    // present in ntdll and forwarders both under Windows and wine
    pub fn AddVectoredExceptionHandler(first: u32, handler: *const core::ffi::c_void) -> *mut core::ffi::c_void;
    pub fn RemoveVectoredExceptionHandler(handle: *mut core::ffi::c_void) -> u32;
    pub fn SetUnhandledExceptionFilter(filter: *const core::ffi::c_void) -> *mut core::ffi::c_void;

    pub fn VirtualProtect(addr: *mut u8, size: usize, newp: u32, old: *mut u32) -> BOOL;
    pub fn FlushInstructionCache(h: HANDLE, base: *const u8, size: usize) -> BOOL;
    pub fn GetCurrentProcess() -> HANDLE;
    pub fn GetCurrentThreadId() -> u32;
    pub fn GetTickCount64() -> u64;
    // kernel32 in wine, psapi on Windows (both present under Proton)
    pub fn K32GetProcessMemoryInfo(
        h: HANDLE,
        counters: *mut PROCESS_MEMORY_COUNTERS,
        cb: u32,
    ) -> BOOL;

    /// setjmp/longjmp, x64 flavour: capture the caller's context, and later
    /// restore it to resume there. This is how a vectored handler abandons a
    /// faulting game call and returns an error to *our* frame.
    /// The buffer is 1232 bytes on x64 (`CONTEXT`, must be 16-byte aligned);
    /// its contents are opaque to us.
    pub fn RtlCaptureContext(ctx: *mut u64);
    /// Non-returning when the restored context differs from the current one.
    pub fn RtlRestoreContext(ctx: *const u64, exception_record: *const core::ffi::c_void);
}

/// Size of an x64 `CONTEXT` in qwords (1232 bytes), 16-byte aligned by host code.
pub const CONTEXT_QWORDS: usize = 154;

/// Rust `&str` -> NUL-terminated wide buffer (stack or heap, caller keeps it).
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(core::iter::once(0)).collect()
}

pub fn cstr(s: &str) -> Vec<u8> {
    s.bytes().chain(core::iter::once(0)).collect()
}

pub fn wide_to_string(buf: &[u16]) -> String {
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end])
}
