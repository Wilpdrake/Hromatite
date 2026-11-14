//! Manual mapping: the DLL never appears in the target's PEB module list, so
//! an in-process module sweep finds nothing to look at.
//!
//! Known limitation: TLS callbacks are only logged, not invoked (a Rust cdylib
//! normally has none; check `tls` in the log before trusting that).

use crate::log;
use crate::pe::PeImage;
use crate::procs;
use crate::remote::Remote;

fn rd_u16(d: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([d[o], d[o + 1]])
}

fn rd_u32(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

fn rd_u64(d: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(d[o..o + 8].try_into().unwrap())
}

fn wr_u64(d: &mut [u8], o: usize, v: u64) {
    d[o..o + 8].copy_from_slice(&v.to_le_bytes());
}

fn apply_relocs(image: &mut [u8], img: &PeImage, delta: i64) -> usize {
    let (rva, size) = img.reloc;
    if rva == 0 || size == 0 {
        return 0;
    }
    let mut off = rva as usize;
    let end = (rva as usize + size as usize).min(image.len());
    let mut patched = 0;
    while off + 8 <= end {
        let page = rd_u32(image, off) as usize;
        let blk = rd_u32(image, off + 4) as usize;
        if blk < 8 || off + blk > end {
            break;
        }
        let count = (blk - 8) / 2;
        for i in 0..count {
            let e = rd_u16(image, off + 8 + i * 2);
            let ty = e >> 12;
            let ofs = (e & 0x0FFF) as usize;
            let p = page + ofs;
            match ty {
                0 => {}
                10 => {
                    if p + 8 <= image.len() {
                        let v = rd_u64(image, p);
                        wr_u64(image, p, (v as i64).wrapping_add(delta) as u64);
                        patched += 1;
                    }
                }
                other => log::log(&format!("manualmap: unhandled reloc type {other} at page {page:#x}")),
            }
        }
        off += blk;
    }
    patched
}

fn cstr_at(d: &[u8], o: usize) -> String {
    let end = d[o..].iter().position(|&c| c == 0).unwrap_or(0);
    String::from_utf8_lossy(&d[o..o + end]).to_string()
}

/// LoadLibraryW inside the target, for an import the game has not loaded yet.
fn remote_load(r: &Remote, dll: &str) -> Option<usize> {
    let k32 = match procs::module(r.pid, "kernel32.dll").map(|(b, _)| b) {
        Some(b) => b,
        None => {
            log::log("remote_load: kernel32.dll not in module list");
            return None;
        }
    };
    let llw = match r.export(k32, "LoadLibraryW") {
        Some(a) => a,
        None => {
            log::log("remote_load: kernel32!LoadLibraryW not resolved");
            return None;
        }
    };
    let w = crate::ffi::wide(dll);
    let bytes: &[u8] = unsafe { core::slice::from_raw_parts(w.as_ptr() as *const u8, w.len() * 2) };
    let arg = r.alloc(bytes.len())?;
    if !r.write(arg, bytes) {
        log::log("remote_load: cannot write dll path into target");
        return None;
    }
    let handle = match r.call(llw, arg, 30_000) {
        Some(code) if code != 0 => code as usize,
        Some(_) => {
            log::log(&format!("remote_load: LoadLibraryW({dll}) returned NULL"));
            return None;
        }
        None => {
            log::log("remote_load: CreateRemoteThread(LoadLibraryW) failed");
            return None;
        }
    };
    // wine's apiset stubs are not always listed under the name we asked for,
    // but HMODULE *is* the base address — fall back to it instead of failing.
    match procs::module(r.pid, dll).map(|(b, _)| b) {
        Some(b) => Some(b),
        None => {
            log::log(&format!(
                "remote_load: {dll} not in module list, using HMODULE {handle:#x} as base"
            ));
            Some(handle)
        }
    }
}

fn is_pe(r: &Remote, base: usize) -> bool {
    matches!(r.read_vec(base, 2), Some(b) if &b[..] == b"MZ")
}

/// Last-resort lookup: ask every module in the target for this export name.
/// Only used for wine's apiset stubs, which take module-like handles that are
/// not real image bases.
fn find_export_anywhere(r: &Remote, name: &str) -> Option<(usize, usize)> {
    let mods = procs::modules(r.pid);
    for (mname, base, _) in &mods {
        if !is_pe(r, *base) {
            continue;
        }
        if let Some(a) = r.export(*base, name) {
            log::log(&format!("find_export_anywhere: {name} in {mname} at {a:#x}"));
            return Some((*base, a));
        }
    }
    log::log(&format!(
        "find_export_anywhere: {name} not found among {} modules",
        mods.len()
    ));
    None
}

fn resolve_imports(image: &mut [u8], img: &PeImage, r: &Remote) -> Result<(), String> {
    let (rva, _size) = img.imports;
    if rva == 0 {
        return Ok(());
    }
    let mut d = rva as usize;
    while d + 20 <= image.len() {
        let oft = rd_u32(image, d) as usize;
        let name_rva = rd_u32(image, d + 12) as usize;
        let ft = rd_u32(image, d + 16) as usize;
        if name_rva == 0 {
            break;
        }
        let dll = cstr_at(image, name_rva);
        let mut modbase = match procs::module(r.pid, &dll) {
            Some((b, _)) => b,
            None => remote_load(r, &dll).ok_or_else(|| format!("import: cannot load {dll} in target"))?,
        };
        if !is_pe(r, modbase) {
            // wine apiset stub: LoadLibraryW hands back a fake HMODULE, so the
            // thunks have to be found by name in whichever module really owns
            // them (usually kernelbase). The first thunk holds the RVA of the
            // IMAGE_IMPORT_BY_NAME (2-byte hint, then the name).
            let first_name_rva = rd_u64(image, if oft != 0 { oft } else { ft }) as usize;
            let name = cstr_at(image, first_name_rva + 2);
            match find_export_anywhere(r, &name) {
                Some((real_base, addr)) => {
                    log::log(&format!(
                        "import: {dll} -> {name} lives in module {real_base:#x} (addr {addr:#x})"
                    ));
                    modbase = real_base;
                }
                None => log::log(&format!("import: {dll} is not a PE at {modbase:#x}, thunks may fail")),
            }
        }
        let mut thunk = if oft != 0 { oft } else { ft };
        let mut iat = ft;
        loop {
            if thunk + 8 > image.len() || iat + 8 > image.len() {
                break;
            }
            let entry = rd_u64(image, thunk);
            if entry == 0 {
                break;
            }
            let is_ord = img.is_64 && (entry >> 63) == 1;
            let addr = if is_ord {
                r.export_ordinal(modbase, (entry & 0xFFFF) as u16)
            } else {
                let name = cstr_at(image, entry as usize + 2);
                r.export(modbase, &name)
            }
            .ok_or_else(|| format!("import: unresolved thunk in {dll} (entry {entry:#x})"))?;
            wr_u64(image, iat, addr as u64);
            thunk += 8;
            iat += 8;
        }
        d += 20;
    }
    Ok(())
}

/// x64 stub that calls every TLS callback with (module_base, DLL_PROCESS_ATTACH, 0).
///
/// A `CreateRemoteThread` can only hand over one argument, and a Rust cdylib
/// *does* carry two TLS callbacks (checked with objdump on our own binary), so
/// they get a hand-encoded stub instead of being skipped.
fn tls_stub(base: u64, callbacks: &[u64]) -> Vec<u8> {
    let mut code = Vec::new();
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]); // sub rsp, 0x28
    for cb in callbacks {
        code.extend_from_slice(&[0x48, 0xB9]); // mov rcx, imm64
        code.extend_from_slice(&base.to_le_bytes());
        code.extend_from_slice(&[0xBA, 0x01, 0x00, 0x00, 0x00]); // mov edx, 1
        code.extend_from_slice(&[0x45, 0x31, 0xC0]); // xor r8d, r8d
        code.extend_from_slice(&[0x48, 0xB8]); // mov rax, imm64
        code.extend_from_slice(&cb.to_le_bytes());
        code.extend_from_slice(&[0xFF, 0xD0]); // call rax
    }
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28]); // add rsp, 0x28
    code.push(0xC3); // ret
    code
}

fn run_tls_callbacks(r: &Remote, base: usize, tls_rva: u32) -> Result<usize, String> {
    let dir = base + tls_rva as usize;
    let cb_va = r.u64(dir + 24).ok_or("tls: cannot read AddressOfCallBacks")?;
    if cb_va == 0 {
        return Ok(0);
    }
    let mut cbs = Vec::new();
    for i in 0..32u64 {
        match r.u64(cb_va as usize + (i as usize) * 8) {
            Some(v) if v != 0 => cbs.push(v),
            _ => break,
        }
    }
    if cbs.is_empty() {
        return Ok(0);
    }
    let code = tls_stub(base as u64, &cbs);
    let mem = r.alloc(code.len()).ok_or("tls: stub alloc failed")?;
    if !r.write(mem, &code) {
        return Err("tls: stub write failed".into());
    }
    r.call(mem, 0, 10_000).ok_or("tls: stub thread failed")?;
    log::log(&format!("manualmap: ran {} TLS callback(s)", cbs.len()));
    Ok(cbs.len())
}

/// Map `dll_path` into `pid` by hand. Returns the remote base address.
pub fn inject(pid: u32, dll_path: &str) -> Result<usize, String> {
    let img = PeImage::from_file(dll_path)?;
    let r = Remote::open(pid)?;
    let base = r.alloc(img.size_of_image as usize).ok_or("VirtualAllocEx failed")?;
    let delta = base as i64 - img.image_base as i64;

    let mut image = vec![0u8; img.size_of_image as usize];
    let hdr = (img.size_of_headers as usize).min(img.data.len()).min(image.len());
    image[..hdr].copy_from_slice(&img.data[..hdr]);
    for s in &img.sections {
        let n = s.rsize.min(s.vsize) as usize;
        let (src, dst) = (s.raw as usize, s.va as usize);
        if src + n <= img.data.len() && dst + n <= image.len() {
            image[dst..dst + n].copy_from_slice(&img.data[src..src + n]);
        }
    }

    let relocs = if delta != 0 { apply_relocs(&mut image, &img, delta) } else { 0 };
    resolve_imports(&mut image, &img, &r)?;

    if !r.write(base, &image) {
        return Err("WriteProcessMemory(image) failed".into());
    }

    if img.tls.0 != 0 {
        if let Err(e) = run_tls_callbacks(&r, base, img.tls.0) {
            log::log(&format!("manualmap: TLS callbacks failed: {e}"));
        }
    }

    // A bare CreateRemoteThread would invoke the entry point with only one
    // argument, leaving `reason` as garbage (wine does not special-case DLL
    // entry points), so DllMain must be called through our own stub.
    let entry = base + img.entry as usize;
    let stub = tls_stub(base as u64, &[entry as u64]);
    let mem = r.alloc(stub.len()).ok_or("entry stub alloc failed")?;
    if !r.write(mem, &stub) {
        return Err("entry stub write failed".into());
    }
    r.call(mem, 0, 30_000).ok_or("CreateRemoteThread(entry stub) failed")?;
    log::log(&format!(
        "manualmap: {dll_path} mapped at {base:#x} (delta {delta:#x}, relocs {relocs}), entry {entry:#x} returned"
    ));
    Ok(base)
}
