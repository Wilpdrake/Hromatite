//! Manual mapping with the flags EAC's scans actually look at:
//!
//!   * pages start as `RW` and end up `RX` — no anonymous RWX anywhere;
//!   * PE headers are erased, so a "find the MZ in private memory" sweep fails;
//!   * TLS callbacks and the entry point run from *one* stub thread, so the game
//!     gains a single non-module thread instead of two;
//!   * optional module stomp: the image lives in the slack space of a module the
//!     game already mapped (its `/proc/<pid>/maps` entry then belongs to that
//!     module, not to an anonymous region).

#![forbid(unsafe_code)]

use crate::log;
use crate::pe::PeImage;
use crate::procs;
use crate::remote::Remote;

#[derive(Default)]
pub struct Options {
    /// Zero the PE headers before the image is written.
    pub erase_headers: bool,
    /// Map inside this module's tail slack instead of a fresh allocation.
    pub stomp: Option<String>,
    /// L2: after the payload's entry point returns, XOR every page of the
    /// mapping and drop it to `RW`. At rest the image is neither executable
    /// nor PE-parseable — a "find the module" sweep sees ciphertext.
    pub seal: bool,
}

/// XOR one page of the remote image in place with `crypto::xor_page` — the very
/// same function the payload's shim page re-runs in machine code (plan 4). Any
/// divergence here means the tick decrypts garbage, so the page index and the
/// page length are the contract, not an implementation detail.
fn seal_image(
    r: &Remote,
    base: usize,
    size: usize,
    keep: Option<(usize, usize)>,
) -> (usize, usize) {
    const PAGE: usize = crate::crypto::PAGE;
    let mut done = 0usize;
    let mut failed = 0usize;
    let mut off = 0usize;
    while off < size {
        let n = (size - off).min(PAGE);
        let addr = base + off;
        if let Some((k_lo, k_hi)) = keep
            && addr >= k_lo && addr < k_hi
        {
            off += n;
            continue;
        }
        if !r.protect(addr, n, crate::ffi::PAGE_READWRITE) {
            failed += 1;
            off += n;
            continue;
        }
        match r.read_vec(addr, n) {
            Some(mut buf) => {
                crate::crypto::xor_page((off / PAGE) as u64, &mut buf);
                if r.write(addr, &buf) {
                    done += 1;
                } else {
                    failed += 1;
                }
            }
            None => failed += 1,
        }
        off += n;
    }
    (done, failed)
}

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
            // `page` comes from the file: `page + ofs` may overflow on a
            // corrupt block and wrap to a *valid-looking* small RVA
            let p = match page.checked_add(ofs) {
                Some(p) => p,
                None => continue,
            };
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

/// NUL-terminated string inside the *local image buffer*. An RVA from the import
/// directory is attacker-controlled as far as we know, so out-of-range means
/// empty string, not a panic (`d[o..]` used to slice past the end).
fn cstr_at(d: &[u8], o: usize) -> String {
    if o >= d.len() {
        return String::new();
    }
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
    let bytes = crate::raw::sys::wide_bytes(dll);
    let arg = r.alloc(bytes.len(), crate::ffi::PAGE_READWRITE)?;
    if !r.write(arg, &bytes) {
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
            // the thunk array offset is another file-controlled RVA
            let src = if oft != 0 { oft } else { ft };
            if src + 8 > image.len() {
                d += 20;
                continue;
            }
            let first_name_rva = rd_u64(image, src) as usize;
            let name = cstr_at(image, first_name_rva.saturating_add(2));
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

/// x64 stub calling each target with (module_base, DLL_PROCESS_ATTACH, 0).
///
/// `CreateRemoteThread` hands over a single argument and wine does not
/// special-case DLL entry points, so TLS callbacks *and* DllMain both need this
/// shim. Combining them keeps the game at one stray thread instead of two.
fn dll_stub(base: u64, targets: &[u64], reserved: u64) -> Vec<u8> {
    let mut code = Vec::new();
    code.extend_from_slice(&[0x48, 0x83, 0xEC, 0x28]); // sub rsp, 0x28
    for cb in targets {
        code.extend_from_slice(&[0x48, 0xB9]); // mov rcx, imm64
        code.extend_from_slice(&base.to_le_bytes());
        code.extend_from_slice(&[0xBA, 0x01, 0x00, 0x00, 0x00]); // mov edx, 1
        // r8 = reserved argument: the payload's only channel for values the
        // injector knows and it cannot discover itself (the image range needed by
        // L2b — VirtualQuery reports the section split, not SizeOfImage, so the
        // shim used to decrypt the wrong range and ran the cipher on every call).
        code.extend_from_slice(&[0x49, 0xB8]); // mov r8, imm64
        code.extend_from_slice(&reserved.to_le_bytes());
        code.extend_from_slice(&[0x48, 0xB8]); // mov rax, imm64
        code.extend_from_slice(&cb.to_le_bytes());
        code.extend_from_slice(&[0xFF, 0xD0]); // call rax
    }
    code.extend_from_slice(&[0x48, 0x83, 0xC4, 0x28]); // add rsp, 0x28
    code.push(0xC3); // ret
    code
}

fn tls_targets(r: &Remote, base: usize, tls_rva: u32) -> Vec<u64> {
    let dir = base + tls_rva as usize;
    let cb_va = match r.u64(dir + 24) {
        Some(v) if v != 0 => v,
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    for i in 0..32usize {
        match r.u64(cb_va as usize + i * 8) {
            Some(v) if v != 0 => out.push(v),
            _ => break,
        }
    }
    out
}

/// Map the image inside a module the game already has, so the region is not
/// "private, executable, unbacked" in a memory-map sweep.
fn stomp_alloc(r: &Remote, module: &str, size: usize) -> Option<usize> {
    let (mbase, _msize) = procs::module(r.pid, module)?;
    let e = r.u32(mbase + 0x3C)? as usize;
    let opt = mbase + e + 24;
    let soimage = r.u32(opt + 56)? as usize;
    let nsec = r.u16(mbase + e + 6)? as usize;
    let opt_size = r.u16(mbase + e + 20)? as usize;
    let st = opt + opt_size;
    let mut end = 0usize;
    for i in 0..nsec {
        let b = st + i * 40;
        let va = r.u32(b + 12)? as usize;
        let vs = r.u32(b + 8)? as usize;
        end = end.max(va + vs);
    }
    let start = (end + 0xFFF) & !0xFFF;
    let avail = soimage.saturating_sub(start);
    log::log(&format!(
        "stomp: {module} base {mbase:#x} SizeOfImage {soimage:#x} last-used {end:#x} slack {avail:#x}, need {size:#x}"
    ));
    if avail < size {
        return None;
    }
    r.alloc_at(mbase + start, size, crate::ffi::PAGE_READWRITE)
}

/// Load the DLL through the target's loader: file-backed sections (an img
/// mapping in /proc/<pid>/maps belongs to a real file), at the cost of a PEB
/// entry — which the DLL removes itself (see `procs::unlink_from_peb`).
pub fn inject_loadlib(pid: u32, dll_win_path: &str) -> Result<usize, String> {
    let r = Remote::open(pid)?;
    let k32 = procs::module(pid, "kernel32.dll")
        .map(|(b, _)| b)
        .ok_or("kernel32.dll not found in target")?;
    let llw = r.export(k32, "LoadLibraryW").ok_or("kernel32!LoadLibraryW unresolved")?;

    let bytes = crate::raw::sys::wide_bytes(dll_win_path);
    let arg = r.alloc(bytes.len(), crate::ffi::PAGE_READWRITE).ok_or("alloc path failed")?;
    if !r.write(arg, &bytes) {
        return Err("write path failed".into());
    }
    let code = r
        .call(llw, arg, 60_000)
        .ok_or("CreateRemoteThread(LoadLibraryW) failed")?;
    if code == 0 {
        return Err(format!("LoadLibraryW returned NULL (err {:#x})", code));
    }
    log::log(&format!(
        "loadlib: {dll_win_path} mapped at {code:#x} by the target's loader"
    ));
    Ok(code as usize)
}

/// Map `dll_path` into `pid` by hand. Returns the remote base address.
pub fn inject(pid: u32, dll_path: &str, opts: &Options) -> Result<usize, String> {
    let img = PeImage::from_file(dll_path)?;
    let r = Remote::open(pid)?;
    let size = img.size_of_image as usize;

    let base = match opts.stomp.as_deref() {
        Some(module) => match stomp_alloc(&r, module, size) {
            Some(b) => {
                log::log(&format!("stomp: mapped into {module} at {b:#x}"));
                b
            }
            None => {
                log::log("stomp: not enough slack, falling back to a fresh allocation");
                r.alloc(size, crate::ffi::PAGE_READWRITE)
                    .ok_or("VirtualAllocEx failed")?
            }
        },
        None => r
            .alloc(size, crate::ffi::PAGE_READWRITE)
            .ok_or("VirtualAllocEx failed")?,
    };
    let delta = base as i64 - img.image_base as i64;

    let mut image = vec![0u8; size];
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

    if opts.erase_headers {
        for b in image[..hdr].iter_mut() {
            *b = 0;
        }
    }

    if !r.write(base, &image) {
        return Err("WriteProcessMemory(image) failed".into());
    }

    // RW -> RX for code, RW stays for data: no page is both writable and
    // executable, which is what the scanners key on.
    let mut rx = 0usize;
    for s in &img.sections {
        let n = s.vsize.min(s.rsize.max(s.vsize)) as usize;
        if n == 0 {
            continue;
        }
        let prot = if s.is_executable() {
            rx += 1;
            crate::ffi::PAGE_EXECUTE_READ
        } else {
            crate::ffi::PAGE_READWRITE
        };
        if !r.protect(base + s.va as usize, n, prot) {
            log::log(&format!("manualmap: protect failed for section @{:#x}", s.va));
        }
    }

    let mut targets = tls_targets(&r, base, img.tls.0);
    let tls_count = targets.len();
    let entry = base + img.entry as usize;
    targets.push(entry as u64);

    // (base, size, text_lo, text_hi) for the payload: it cannot read its own PE
    // header any more, and L2b-style sealing needs to know which pages must stay
    // plaintext (its own code) so the tick can run without a shim.
    // Keep everything the running tick touches: its code *and* the writable
    // sections (statics, Mutexes, the reader). Sealing `.data` while code still
    // runs is fatal — measured, the game dies right after `seal: 73 pages`.
    // What gets sealed then: headers, `.rdata`, `.reloc`.
    let text = img
        .sections
        .iter()
        .find(|s| s.is_executable())
        .map(|s| {
            let lo = base + s.va as usize;
            let hi = img
                .sections
                .iter()
                .filter(|x| !x.is_executable() && x.characteristics & 0x8000_0000 != 0)
                .map(|x| base + x.va as usize + x.vsize as usize)
                .max()
                .unwrap_or(lo + s.vsize as usize);
            (lo, hi.max(lo + s.vsize as usize))
        })
        .unwrap_or((0, 0));
    // Only the partial mode keeps pages plaintext; the shim mode seals everything
    // (it decrypts on entry instead).
    let partial = std::env::var("HROMATITE_L2B_PARTIAL")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false);
    let keep_range = if partial && text.0 != 0 && text.1 > text.0 {
        Some((text.0 & !0xFFF, (text.1 + 0xFFF) & !0xFFF))
    } else {
        None
    };
    // Executable sections only: the shim makes exactly this range `RX` inside the
    // window. Using the `keep` range instead turned `.data` read-only, and the
    // Rust detour faulted on its own statics (measured: `stage=7`, lock stuck).
    let code = img
        .sections
        .iter()
        .filter(|s| s.is_executable())
        .fold((usize::MAX, 0usize), |(lo, hi), s| {
            let l = base + s.va as usize;
            let h = l + s.vsize as usize;
            (lo.min(l), hi.max(h))
        });
    let (code_lo, code_hi) = if code.0 == usize::MAX { (0, 0) } else { code };
    let params = r
        .alloc(48, crate::ffi::PAGE_READWRITE)
        .ok_or("param alloc failed")?;
    let mut pbuf = [0u8; 48];
    pbuf[..8].copy_from_slice(&(base as u64).to_le_bytes());
    pbuf[8..16].copy_from_slice(&(size as u64).to_le_bytes());
    pbuf[16..24].copy_from_slice(&(text.0 as u64).to_le_bytes());
    pbuf[24..32].copy_from_slice(&(text.1 as u64).to_le_bytes());
    pbuf[32..40].copy_from_slice(&(code_lo as u64).to_le_bytes());
    pbuf[40..48].copy_from_slice(&(code_hi as u64).to_le_bytes());
    if !r.write(params, &pbuf) {
        return Err("param write failed".into());
    }
    let stub = dll_stub(base as u64, &targets, params as u64);
    let stub_mem = r
        .alloc(stub.len(), crate::ffi::PAGE_READWRITE)
        .ok_or("stub alloc failed")?;
    if !r.write(stub_mem, &stub) {
        return Err("stub write failed".into());
    }
    r.protect(stub_mem, stub.len(), crate::ffi::PAGE_EXECUTE_READ);
    // Sealing rewrites every page of the image, so the payload must be *done*:
    // wait far longer than the default when a seal is going to follow.
    // 30 min: interactive mode (HROMATITE_LIVE) keeps the entry point alive while
    // the host explores through the command file, and the seal has to happen
    // after the last answer, not on a timeout.
    let wait_ms = if opts.seal { 1_800_000 } else { 60_000 };
    let code = r
        .call(stub_mem, 0, wait_ms)
        .ok_or("CreateRemoteThread(stub) failed")?;
    if opts.seal && code == 0x103 {
        log::log("seal: stub thread still active (0x103), skipping seal to avoid rewriting live code");
    }

    log::log(&format!(
        "manualmap: {dll_path} mapped at {base:#x} (delta {delta:#x}, relocs {relocs}, rx sections {rx}, \
         tls {tls_count}, headers_erased {}, stomp {})",
        opts.erase_headers,
        opts.stomp.as_deref().unwrap_or("-")
    ));

    // L2: the payload has returned into the stub thread, so no instruction
    // fetch can land in the image any more — encrypt it and leave it RW.
    if opts.seal && code != 0x103 {
        let before = r.read_vec(base, 16).unwrap_or_default();
        let keep = keep_range.filter(|(lo, hi)| *lo > base && *hi <= base + size);
        let (pages, failed) = seal_image(&r, base, size, keep);
        let after = r.read_vec(base, 16).unwrap_or_default();
        log::log(&format!(
            "seal: {pages} pages XORed, {failed} failed, size {size:#x}, crypto=block8; \
             before {:02x?}; after {:02x?}",
            &before[..before.len().min(16)],
            &after[..after.len().min(16)]
        ));
    }
    Ok(base)
}
