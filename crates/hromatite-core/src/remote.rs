//! Handle to another process plus the four memory primitives we need.
//! Deliberately tiny: every call is a direct Win32 call, no abstractions that
//! would hide an injected thread or a stray allocation.

use crate::ffi::*;

pub struct Remote {
    pub pid: u32,
    pub h: HANDLE,
}

impl Remote {
    pub fn open(pid: u32) -> Result<Self, String> {
        let access = PROCESS_CREATE_THREAD
            | PROCESS_VM_OPERATION
            | PROCESS_VM_READ
            | PROCESS_VM_WRITE
            | PROCESS_QUERY_INFORMATION;
        let h = unsafe { OpenProcess(access, 0, pid) };
        if h.is_null() {
            Err(format!("OpenProcess(pid={pid}) failed, GetLastError={}", unsafe { GetLastError() }))
        } else {
            Ok(Self { pid, h })
        }
    }

    pub fn read(&self, addr: usize, buf: &mut [u8]) -> bool {
        unsafe {
            ReadProcessMemory(self.h, addr as *const u8, buf.as_mut_ptr(), buf.len(), core::ptr::null_mut()) != 0
        }
    }

    pub fn read_vec(&self, addr: usize, len: usize) -> Option<Vec<u8>> {
        let mut v = vec![0u8; len];
        self.read(addr, &mut v).then_some(v)
    }

    pub fn read_string(&self, addr: usize, max: usize) -> Option<String> {
        let mut buf = vec![0u8; max];
        if !self.read(addr, &mut buf) {
            return None;
        }
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        Some(String::from_utf8_lossy(&buf[..end]).to_string())
    }

    pub fn u16(&self, addr: usize) -> Option<u16> {
        let b = self.read_vec(addr, 2)?;
        Some(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&self, addr: usize) -> Option<u32> {
        let b = self.read_vec(addr, 4)?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&self, addr: usize) -> Option<u64> {
        let b = self.read_vec(addr, 8)?;
        Some(u64::from_le_bytes(b.try_into().ok()?))
    }

    pub fn write(&self, addr: usize, buf: &[u8]) -> bool {
        unsafe {
            WriteProcessMemory(self.h, addr as *mut u8, buf.as_ptr(), buf.len(), core::ptr::null_mut()) != 0
        }
    }

    pub fn alloc(&self, size: usize, protect: u32) -> Option<usize> {
        self.alloc_at(0, size, protect)
    }

    /// Allocate at a requested address (0 = anywhere). Used for module stomping,
    /// where the region must land inside an already-mapped image.
    pub fn alloc_at(&self, addr: usize, size: usize, protect: u32) -> Option<usize> {
        let p = unsafe {
            VirtualAllocEx(self.h, addr, size, MEM_COMMIT | MEM_RESERVE, protect)
        };
        (!p.is_null()).then_some(p as usize)
    }

    pub fn protect(&self, addr: usize, size: usize, prot: u32) -> bool {
        let mut old = 0u32;
        unsafe { VirtualProtectEx(self.h, addr as *mut u8, size, prot, &mut old) != 0 }
    }

    /// Run a function in the target; returns its exit code.
    /// Any filter struct we hand over is dropped immediately: a remote thread
    /// that outlives it is a crash waiting to happen.
    pub fn call(&self, start: usize, param: usize, wait_ms: u32) -> Option<u32> {
        let h = unsafe { CreateRemoteThread(self.h, 0, 0, start, param, 0, core::ptr::null_mut()) };
        if h.is_null() {
            crate::log::log(&format!(
                "call: CreateRemoteThread(start {start:#x}) failed, GetLastError={}",
                unsafe { GetLastError() }
            ));
            return None;
        }
        unsafe {
            WaitForSingleObject(h, wait_ms);
            let mut code = 0u32;
            GetExitCodeThread(h, &mut code);
            CloseHandle(h);
            Some(code)
        }
    }

    /// Resolve an export by name from the target's *own* copy of a module,
    /// so wine's per-process module bases never matter.
    ///
    /// Wine's `kernel32` is mostly forwarders into `kernelbase`, so a raw RVA
    /// can point at the export directory instead of code; those are followed.
    pub fn export(&self, module_base: usize, name: &str) -> Option<usize> {
        self.export_in(module_base, name, 0)
    }

    fn export_in(&self, module_base: usize, name: &str, depth: u8) -> Option<usize> {
        if depth > 4 {
            return None;
        }
        let e_lfanew = self.u32(module_base + 0x3C)? as usize;
        let opt = module_base + e_lfanew + 24;
        let magic = self.u16(opt)?;
        let dd = if magic == 0x20B { opt + 112 } else { opt + 96 };
        let exp_rva = self.u32(dd)? as usize;
        let exp_size = self.u32(dd + 4)? as usize;
        if exp_rva == 0 {
            return None;
        }
        let dir = module_base + exp_rva;
        let n_names = self.u32(dir + 24)? as usize;
        let funcs = self.u32(dir + 28)? as usize;
        let names = self.u32(dir + 32)? as usize;
        let ords = self.u32(dir + 36)? as usize;
        for i in 0..n_names.min(8192) {
            let nrva = self.u32(module_base + names + i * 4)? as usize;
            if let Some(s) = self.read_string(module_base + nrva, 96) {
                if s == name {
                    let ord = self.u16(module_base + ords + i * 2)? as usize;
                    let frva = self.u32(module_base + funcs + ord * 4)? as usize;
                    let in_dir = frva >= exp_rva && frva < exp_rva + exp_size;
                    if !in_dir {
                        return Some(module_base + frva);
                    }
                    let fwd = self.read_string(module_base + frva, 128)?;
                    let (mdll, func) = fwd.split_once('.')?;
                    let mb = crate::procs::module(self.pid, &format!("{mdll}.dll"))
                        .map(|(b, _)| b)
                        .or_else(|| crate::procs::module(self.pid, mdll).map(|(b, _)| b))?;
                    return self.export_in(mb, func, depth + 1);
                }
            }
        }
        None
    }

    /// Export by ordinal (imports that carry `IMAGE_ORDINAL_FLAG`).
    pub fn export_ordinal(&self, module_base: usize, ordinal: u16) -> Option<usize> {
        let e_lfanew = self.u32(module_base + 0x3C)? as usize;
        let opt = module_base + e_lfanew + 24;
        let magic = self.u16(opt)?;
        let dd = if magic == 0x20B { opt + 112 } else { opt + 96 };
        let exp_rva = self.u32(dd)? as usize;
        if exp_rva == 0 {
            return None;
        }
        let dir = module_base + exp_rva;
        let base_ord = self.u32(dir + 16)?;
        let n_funcs = self.u32(dir + 20)?;
        let idx = ordinal as usize;
        if idx < base_ord as usize || idx >= base_ord as usize + n_funcs as usize {
            return None;
        }
        let funcs = self.u32(dir + 28)? as usize;
        let frva = self.u32(module_base + funcs + (idx - base_ord as usize) * 4)? as usize;
        (frva != 0).then_some(module_base + frva)
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.h);
        }
    }
}
