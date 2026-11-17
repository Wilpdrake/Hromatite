//! Inline-hook engine, ported from the C shim `EAC-RE/shim/shim.c`.
//!
//! Patch shape: `48 B8 <handler:u64> FF E0` — `mov rax, imm64 ; jmp rax`, 12
//! bytes, position independent, so the handler may live anywhere in the address
//! space (wine maps ntdll/EOSSDK near `0x6fff_xxxx_xxxx`, the payload far away).
//!
//! The trampoline is the first `total >= 12` bytes of the original prologue,
//! copied into a page allocated within **±512 MB** of the target (so rip-relative
//! `disp32` and `rel32` encodings stay reachable) and relocated instruction by
//! instruction:
//!
//! * rip-relative `disp32` — retargeted to the copy;
//! * `jcc rel8` — inverse `jcc +12 ; mov rax,target ; jmp rax` (14 bytes), the
//!   exact form the working shim emits (`reloc=jcc-abs+7` in
//!   `evidence/runA_final_2301/eacshim.log`);
//! * `jcc rel32` — a compact `jcc rel32` when it still reaches, else the same
//!   absolute form;
//! * `jmp rel8/rel32` and `call rel32` — rel32 if it reaches, else absolute;
//! * anything else — copied verbatim.
//!
//! The trampoline ends with an absolute jump to `entry + total`, never to
//! `entry` (a self-jump would resume at the patched prologue and recurse).
//!
//! A named shared section (`Local\hromatite_rust_hooktable_v1`) records
//! `{target -> trampoline, handler}` so a second injection into the same process
//! reuses the existing trampoline instead of re-hooking already patched bytes.

/// Bytes overwritten at the target entry.
pub const PATCH_LEN: usize = 12;
/// Allocation search radius: ±512 MB (prologue relocation needs ±2 GB at most,
/// the shim uses 512 MB and that is what is proven in the game).
#[cfg(windows)]
const NEAR_SPAN: usize = 0x2000_0000;
/// Page size of the trampoline allocation.
#[cfg(windows)]
const TRAMP_PAGE: usize = 0x1000;

/// One decoded instruction: its length and where a relative/rip-relative field
/// sits inside it, so the trampoline copy can be relocated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Insn {
    /// Instruction length in bytes (1..=15).
    pub len: usize,
    /// The instruction carries a rip-relative `disp32`.
    pub rip_rel: bool,
    /// Offset of that `disp32` inside the instruction.
    pub disp_off: usize,
    /// The instruction ends in a relative branch.
    pub rel: bool,
    /// Offset of the relative field inside the instruction.
    pub rel_off: usize,
    /// Relative field width in bytes: 1 or 4.
    pub rel_size: u8,
}

fn is_rex(byte: u8) -> bool {
    byte & 0xF0 == 0x40
}

/// One-byte opcodes that take a ModRM byte (the subset the shim trusts).
fn one_byte_has_modrm(op: u8) -> bool {
    matches!(
        op,
        0x00..=0x03
            | 0x08..=0x0B
            | 0x10..=0x13
            | 0x18..=0x1B
            | 0x20..=0x23
            | 0x28..=0x2B
            | 0x30..=0x33
            | 0x38..=0x3B
            | 0x62
            | 0x63
            | 0x69
            | 0x6B
            | 0x80
            | 0x81
            | 0x83
            | 0x84..=0x8F
            | 0xC0
            | 0xC1
            | 0xC6
            | 0xC7
            | 0xD0..=0xD3
            | 0xD8..=0xDF
            | 0xF6
            | 0xF7
            | 0xFE
            | 0xFF
    )
}

/// Decode one instruction. `None` = an opcode outside the trusted subset (the
/// caller must then treat the prologue as unhookable).
///
/// This is a length decoder, not a disassembler: it only needs to know how many
/// bytes to copy and which field to relocate, exactly like `decode_insn` in the
/// C shim.
pub fn decode(code: &[u8]) -> Option<Insn> {
    let mut i = 0usize;
    let mut rex = 0u8;
    let mut opsz16 = false;

    loop {
        let byte = *code.get(i)?;
        match byte {
            0x66 => {
                opsz16 = true;
                i += 1;
            }
            0x67 | 0xF0 | 0xF2 | 0xF3 | 0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65 => i += 1,
            _ => break,
        }
        if i > 8 {
            return None;
        }
    }
    let head = *code.get(i)?;
    if is_rex(head) {
        rex = head;
        i += 1;
    }
    let op = *code.get(i)?;
    i += 1;

    let mut out = Insn::default();
    let mut has_modrm = false;
    let mut imm = 0usize;

    if op == 0x0F {
        let op2 = *code.get(i)?;
        i += 1;
        match op2 {
            0x38 | 0x3A => {
                i += 1;
                has_modrm = true;
                if op2 == 0x3A {
                    imm = 1;
                }
            }
            0x80..=0x8F => {
                out.rel = true;
                out.rel_off = i;
                out.rel_size = 4;
                i += 4;
                out.len = i;
                return Some(out);
            }
            0x05 | 0x0B | 0x31 | 0x77 | 0xA2 | 0x34 | 0x35 | 0x0D => {}
            0x71 | 0x72 | 0x73 | 0xBA | 0xC2 | 0xC4 | 0xC5 | 0xC6 => {
                has_modrm = true;
                imm = 1;
            }
            _ => has_modrm = true,
        }
    } else {
        match op {
            0x90 | 0x98 | 0x99 | 0xC3 | 0xC9 | 0xCC | 0xF4 | 0xF5 | 0xF8 | 0xF9 | 0xFC | 0xFD
            | 0xCE | 0x9C | 0x9D | 0x9E | 0x9F => {}
            0x50..=0x5F => {}
            0x68 => imm = if opsz16 { 2 } else { 4 },
            0x6A => imm = 1,
            0x70..=0x7F | 0xEB => {
                out.rel = true;
                out.rel_off = i;
                out.rel_size = 1;
                i += 1;
                out.len = i;
                return Some(out);
            }
            0xE8 | 0xE9 => {
                out.rel = true;
                out.rel_off = i;
                out.rel_size = 4;
                i += 4;
                out.len = i;
                return Some(out);
            }
            0xC2 => imm = 2,
            0xB0..=0xB7 => imm = 1,
            0xB8..=0xBF => {
                imm = if rex & 8 != 0 {
                    8
                } else if opsz16 {
                    2
                } else {
                    4
                };
            }
            // alu `al, imm8` / `eax, imm32` forms carry no ModRM
            0xA8 => imm = 1,
            0xA9 => {
                imm = if opsz16 { 2 } else { 4 };
            }
            0x04 | 0x0C | 0x14 | 0x1C | 0x24 | 0x2C | 0x34 | 0x3C => imm = 1,
            0x05 | 0x0D | 0x15 | 0x1D | 0x25 | 0x2D | 0x35 | 0x3D => {
                imm = if opsz16 { 2 } else { 4 };
            }
            _ => {
                if !one_byte_has_modrm(op) {
                    return None;
                }
                has_modrm = true;
                if matches!(op, 0x80 | 0x83 | 0x6B | 0xC0 | 0xC1 | 0xC6 | 0xF6) {
                    imm = 1;
                } else if matches!(op, 0x81 | 0x69 | 0xC7 | 0xF7) {
                    imm = if opsz16 { 2 } else { 4 };
                }
            }
        }
    }

    if has_modrm {
        let modrm = *code.get(i)?;
        i += 1;
        // group 3: only `/0 test` and `/1 test` take an immediate — `/2.. /7`
        // (not/neg/mul/imul/div/idiv) are one byte shorter than the table says.
        if matches!(op, 0xF6 | 0xF7) {
            let reg = (modrm >> 3) & 7;
            imm = if reg <= 1 {
                if op == 0xF6 {
                    1
                } else if opsz16 {
                    2
                } else {
                    4
                }
            } else {
                0
            };
        }
        let mode = modrm >> 6;
        let rm = modrm & 7;
        let mut disp = 0usize;
        if mode != 3 {
            if rm == 4 {
                let sib = *code.get(i)?;
                i += 1;
                if mode == 0 && (sib & 7) == 5 {
                    out.rip_rel = true;
                    disp = 4;
                }
            } else if mode == 0 && rm == 5 {
                out.rip_rel = true;
                disp = 4;
            }
            if mode == 1 {
                disp = 1;
            } else if mode == 2 {
                disp = 4;
            }
            if disp != 0 {
                if out.rip_rel {
                    out.disp_off = i;
                }
                i += disp;
            }
        }
        if imm != 0 {
            i += imm;
        }
    } else if imm != 0 {
        i += imm;
    }

    out.len = i;
    (1..=15).contains(&i).then_some(out)
}

/// Length of a prologue covering at least [`PATCH_LEN`] bytes. `None` when an
/// unknown opcode is hit or the span exceeds 24 bytes (the shim's bound).
pub fn prologue_len(code: &[u8]) -> Option<usize> {
    let mut n = 0usize;
    let mut guard = 0usize;
    while n < PATCH_LEN {
        if guard > 8 {
            return None;
        }
        guard += 1;
        let insn = decode(code.get(n..)?)?;
        if insn.len == 0 {
            return None;
        }
        n += insn.len;
    }
    (n <= 24).then_some(n)
}

/// Length of `jmp qword ptr [rip+0] ; dq addr` — the register-free absolute
/// jump. `mov rax,imm64 ; jmp rax` is two bytes shorter but destroys `rax`,
/// which a relocated prologue may still be using (`mov eax,0x28 ; call
/// __chkstk ; sub rsp,rax` hands the stack size over in `rax`).
pub const ABS_JUMP: usize = 14;

/// Write `jmp qword ptr [rip+0] ; dq addr` at `at`.
fn push_abs_jump(tramp: &mut [u8], at: usize, addr: usize) -> Option<()> {
    let slot = tramp.get_mut(at..at + ABS_JUMP)?;
    slot[..6].copy_from_slice(&[0xFF, 0x25, 0, 0, 0, 0]);
    slot[6..].copy_from_slice(&(addr as u64).to_le_bytes());
    Some(())
}

/// Encode `mov rax, imm64 ; jmp rax`.
pub fn abs_jmp(addr: usize) -> [u8; PATCH_LEN] {
    let mut out = [0u8; PATCH_LEN];
    out[0] = 0x48;
    out[1] = 0xB8;
    out[2..10].copy_from_slice(&(addr as u64).to_le_bytes());
    out[10] = 0xFF;
    out[11] = 0xE0;
    out
}

/// If `code` already starts with the 12-byte patch, return its handler address.
pub fn looks_patched(code: &[u8]) -> Option<usize> {
    if code.len() >= PATCH_LEN
        && code[0] == 0x48
        && code[1] == 0xB8
        && code[10] == 0xFF
        && code[11] == 0xE0
    {
        let mut imm = [0u8; 8];
        imm.copy_from_slice(&code[2..10]);
        Some(u64::from_le_bytes(imm) as usize)
    } else {
        None
    }
}

/// The relocated trampoline plus the relocation label used in the log
/// (`jcc-abs+7`, `jcc-rel32+4`, `call`, `rip-rel`, ...).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Built {
    /// Bytes written into the trampoline page.
    pub len: usize,
    /// Human-readable relocation note for the hook log.
    pub note: String,
}

fn read_i32(code: &[u8], at: usize) -> Option<i32> {
    let bytes = code.get(at..at + 4)?;
    Some(i32::from_le_bytes(bytes.try_into().ok()?))
}

/// Copy and relocate the prologue into `tramp`.
///
/// `base` is the original entry address, `total` the number of bytes to copy
/// (from [`prologue_len`]); `tramp`'s own address is used to re-encode every
/// relative field. Returns the emitted length and the relocation note.
///
/// The final absolute jump goes to `base + total` — resuming at `base` would
/// re-enter the patch. `None` means the prologue cannot be relocated safely.
pub fn build_trampoline(tramp: &mut [u8], base: usize, code: &[u8], total: usize) -> Option<Built> {
    let tramp_addr = tramp.as_ptr() as usize;
    let mut old = 0usize;
    let mut nw = 0usize;
    // every rewritten field, so the install log shows the whole prologue, not
    // just the first relocation
    let mut notes: Vec<String> = Vec::new();

    let mut label = |text: &str| notes.push(text.to_string());

    while old < total {
        let insn = decode(code.get(old..)?)?;
        if insn.len == 0 {
            return None;
        }

        // rip-relative: keep the instruction, retarget the disp32.
        if insn.rip_rel {
            let disp = read_i32(code, old + insn.disp_off)? as i64;
            let target = (base + old + insn.len) as i64 + disp;
            let next = (tramp_addr + nw + insn.len) as i64;
            let new_disp = i32::try_from(target - next).ok()?;
            tramp
                .get_mut(nw..nw + insn.len)?
                .copy_from_slice(code.get(old..old + insn.len)?);
            tramp
                .get_mut(nw + insn.disp_off..nw + insn.disp_off + 4)?
                .copy_from_slice(&new_disp.to_le_bytes());
            label(&format!("rip-rel+{old}->{target:#x}"));
            old += insn.len;
            nw += insn.len;
            continue;
        }

        if insn.rel {
            let op = code[old];
            let is_jcc = (0x70..=0x7F).contains(&op)
                || (op == 0x0F && code.get(old + 1).is_some_and(|b| b & 0xF0 == 0x80));
            let is_call = op == 0xE8;
            let is_jmp = op == 0xE9 || op == 0xEB;

            let target = if insn.rel_size == 1 {
                (base + old + insn.len) as i64 + i64::from(code[old + insn.rel_off] as i8)
            } else {
                (base + old + insn.len) as i64 + i64::from(read_i32(code, old + insn.rel_off)?)
            };

            if is_jcc {
                let cond = if op == 0x0F {
                    code[old + 1] & 0x0F
                } else {
                    op & 0x0F
                };
                // The C shim only uses the compact rel32 form for an original
                // rel32 jcc; a rel8 jcc is exactly the case the absolute form
                // exists for (`reloc=jcc-abs+7` in the live evidence).
                let compact = if insn.rel_size == 4 {
                    let delta = target - (tramp_addr + nw + 6) as i64;
                    i32::try_from(delta).ok()
                } else {
                    None
                };
                if let Some(delta) = compact {
                    tramp
                        .get_mut(nw..nw + 2)?
                        .copy_from_slice(&[0x0F, 0x80 | cond]);
                    tramp
                        .get_mut(nw + 2..nw + 6)?
                        .copy_from_slice(&delta.to_le_bytes());
                    label(&format!("jcc-rel32+{old}->{target:#x}"));
                    nw += 6;
                } else {
                    // inverse jcc over a register-free `jmp [rip+0] <abs>`:
                    // `mov rax,imm64 ; jmp rax` would destroy rax, which the
                    // remaining prologue may still need (the EVP/`__chkstk`
                    // shape does exactly that).
                    tramp
                        .get_mut(nw..nw + 2)?
                        .copy_from_slice(&[0x70 | (cond ^ 1), ABS_JUMP as u8]);
                    push_abs_jump(tramp, nw + 2, target as usize)?;
                    label(&format!("jcc-abs+{old}->{target:#x}"));
                    nw += 2 + ABS_JUMP;
                }
            } else if is_jmp {
                let delta = target - (tramp_addr + nw + 5) as i64;
                if let Ok(delta) = i32::try_from(delta) {
                    *tramp.get_mut(nw)? = 0xE9;
                    tramp
                        .get_mut(nw + 1..nw + 5)?
                        .copy_from_slice(&delta.to_le_bytes());
                    label(&format!("jmp-rel32+{old}->{target:#x}"));
                    nw += 5;
                } else {
                    push_abs_jump(tramp, nw, target as usize)?;
                    label(&format!("jmp-abs+{old}->{target:#x}"));
                    nw += ABS_JUMP;
                }
            } else if is_call {
                let delta = target - (tramp_addr + nw + 5) as i64;
                if let Ok(delta) = i32::try_from(delta) {
                    *tramp.get_mut(nw)? = 0xE8;
                    tramp
                        .get_mut(nw + 1..nw + 5)?
                        .copy_from_slice(&delta.to_le_bytes());
                    nw += 5;
                } else {
                    // call qword ptr [rip+2] ; jmp +8 ; dq target
                    tramp
                        .get_mut(nw..nw + 6)?
                        .copy_from_slice(&[0xFF, 0x15, 0x02, 0x00, 0x00, 0x00]);
                    tramp
                        .get_mut(nw + 6..nw + 8)?
                        .copy_from_slice(&[0xEB, 0x08]);
                    tramp
                        .get_mut(nw + 8..nw + 8 + 8)?
                        .copy_from_slice(&(target as u64).to_le_bytes());
                    nw += 16;
                }
                label(&format!("call+{old}->{target:#x}"));
            } else {
                return None;
            }
            old += insn.len;
            continue;
        }

        tramp
            .get_mut(nw..nw + insn.len)?
            .copy_from_slice(code.get(old..old + insn.len)?);
        old += insn.len;
        nw += insn.len;
    }

    // jump back to `base + total` without touching any register: the copied
    // prologue may have just computed a value in rax that the rest of the
    // function needs (e.g. the `__chkstk` result used by `sub rsp,rax`).
    push_abs_jump(tramp, nw, base + total)?;
    nw += ABS_JUMP;
    let note = if notes.is_empty() {
        "verbatim".to_string()
    } else {
        notes.join(",")
    };
    Some(Built { len: nw, note })
}

/// A freshly installed hook. The trampoline page is owned by the caller (kept
/// alive for the process lifetime) and is what the detour must tail-call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hook {
    /// Original entry (now holding the 12-byte patch).
    pub target: usize,
    /// Relocated prologue; call this, never `target`.
    pub tramp: usize,
    /// Bytes of prologue relocated.
    pub copied: usize,
    /// Every relocation applied to the copy, as `kind+offset->addr` entries.
    /// Empty when the entry was reused (`copied == 0`).
    pub note: String,
    /// 1 = fresh patch, 2 = reused a trampoline from the shared table,
    /// 3 = the entry was patched by a foreign image (forwarded).
    pub mode: u8,
}

/// One row of the named shared table.
#[cfg(windows)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TableEntry {
    /// Target entry address.
    pub target: u64,
    /// Trampoline address.
    pub tramp: u64,
    /// Handler the patch currently jumps to.
    pub handler: u64,
}

#[cfg(windows)]
mod win {
    //! Minimal Win32 surface. No external crates: these are the declarations the
    //! shim used, and everything here is `extern "system"` (win64 ABI).

    use core::ffi::{c_char, c_void};

    pub type Handle = isize;

    /// `MEMORY_BASIC_INFORMATION` for x64 (48 bytes on win64).
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    pub struct MemBasic {
        pub base: *mut c_void,
        pub allocation_base: *mut c_void,
        pub allocation_protect: u32,
        pub region_size: usize,
        pub state: u32,
        pub protect: u32,
        pub kind: u32,
    }

    unsafe extern "system" {
        pub fn VirtualAlloc(addr: *mut c_void, size: usize, kind: u32, protect: u32)
        -> *mut c_void;
        pub fn VirtualQuery(addr: *const c_void, out: *mut MemBasic, len: usize) -> usize;
        pub fn VirtualProtect(addr: *mut c_void, size: usize, new: u32, old: *mut u32) -> i32;
        pub fn FlushInstructionCache(process: Handle, addr: *const c_void, size: usize) -> i32;
        pub fn GetCurrentProcess() -> Handle;
        pub fn CreateFileMappingA(
            file: Handle,
            attrs: *mut c_void,
            protect: u32,
            high: u32,
            low: u32,
            name: *const c_char,
        ) -> Handle;
        pub fn MapViewOfFile(
            mapping: Handle,
            access: u32,
            high: u32,
            low: u32,
            size: usize,
        ) -> *mut c_void;
    }

    pub const MEM_COMMIT: u32 = 0x1000;
    pub const MEM_RESERVE: u32 = 0x2000;
    pub const MEM_FREE: u32 = 0x1_0000;
    pub const PAGE_EXECUTE_READWRITE: u32 = 0x40;
    pub const PAGE_READWRITE: u32 = 0x04;
    pub const FILE_MAP_ALL_ACCESS: u32 = 0xF001F;
    pub const INVALID_HANDLE: Handle = 0;
}

/// Readable check via `VirtualQuery` (the shim's `safe_readable`).
///
/// # Safety
///
/// `addr` is only probed, never dereferenced by this function.
#[cfg(windows)]
pub unsafe fn readable(addr: usize, len: usize) -> bool {
    if addr == 0 {
        return false;
    }
    // SAFETY: VirtualQuery only inspects the address; the output struct is
    // fully initialised for a successful query.
    let mut info = win::MemBasic::default();
    // SAFETY: `info` is a valid, writable `MEMORY_BASIC_INFORMATION`.
    let got = unsafe {
        win::VirtualQuery(
            addr as *const core::ffi::c_void,
            &mut info,
            core::mem::size_of::<win::MemBasic>(),
        )
    };
    if got == 0 {
        return false;
    }
    let committed = info.state & win::MEM_COMMIT != 0;
    committed && addr + len <= info.base as usize + info.region_size
}

/// Allocate RWX memory within ±512 MB of `target`; falls back to anywhere.
///
/// # Safety
///
/// Pure OS allocation; the returned page is writable and executable.
#[cfg(windows)]
pub unsafe fn alloc_near(target: usize, size: usize) -> Option<usize> {
    let aligned = target & !0xFFFF;
    for pass in 0..2 {
        let direction: i64 = if pass == 0 { -1 } else { 1 };
        let mut distance = 0x1_0000usize;
        while distance < NEAR_SPAN {
            let delta = (direction * distance as i64) as isize;
            let candidate = aligned.wrapping_add_signed(delta);
            distance += 0x1_0000;
            if candidate == 0 {
                continue;
            }
            let mut info = win::MemBasic::default();
            // SAFETY: probe only; `info` is writable and correctly sized.
            let got = unsafe {
                win::VirtualQuery(
                    candidate as *const core::ffi::c_void,
                    &mut info,
                    core::mem::size_of::<win::MemBasic>(),
                )
            };
            if got == 0 || info.state != win::MEM_FREE {
                continue;
            }
            // SAFETY: the query above reported this range free.
            let page = unsafe {
                win::VirtualAlloc(
                    candidate as *mut core::ffi::c_void,
                    size,
                    win::MEM_COMMIT | win::MEM_RESERVE,
                    win::PAGE_EXECUTE_READWRITE,
                )
            };
            if !page.is_null() {
                return Some(page as usize);
            }
        }
    }
    // SAFETY: OS allocation with no placement constraint.
    let page = unsafe {
        win::VirtualAlloc(
            core::ptr::null_mut(),
            size,
            win::MEM_COMMIT | win::MEM_RESERVE,
            win::PAGE_EXECUTE_READWRITE,
        )
    };
    (!page.is_null()).then_some(page as usize)
}

/// Replace the 16-byte aligned block at `base16` atomically with `bytes`.
///
/// # Safety
///
/// `base16` must be 16-byte aligned and writable; `bytes` covers the whole
/// block. The compare-exchange is retried on a concurrent writer.
#[cfg(windows)]
unsafe fn cas16(base16: usize, bytes: &[u8; 16]) -> bool {
    let mut lo_src = [0u8; 8];
    let mut hi_src = [0u8; 8];
    lo_src.copy_from_slice(&bytes[..8]);
    hi_src.copy_from_slice(&bytes[8..]);
    let new_lo = u64::from_le_bytes(lo_src);
    let new_hi = u64::from_le_bytes(hi_src);
    for _ in 0..16 {
        // SAFETY: aligned 16-byte read of the writable patch window.
        let (old_lo, old_hi) = unsafe {
            (
                core::ptr::read_volatile(base16 as *const u64),
                core::ptr::read_volatile((base16 + 8) as *const u64),
            )
        };
        let mut rax = old_lo;
        let mut rdx = old_hi;
        // `rbx` is reserved by LLVM, so it is saved/restored inside the asm
        // block instead of being declared as an operand.
        // SAFETY: `base16` is 16-byte aligned and writable; rax:rdx holds the
        // expected value, rcx:rbx the replacement; rsp is restored by `pop`.
        unsafe {
            core::arch::asm!(
                "push rbx",
                "mov rbx, {lo}",
                "lock cmpxchg16b [{ptr}]",
                "pop rbx",
                ptr = in(reg) base16,
                lo = in(reg) new_lo,
                in("rcx") new_hi,
                inout("rax") rax,
                inout("rdx") rdx,
                options(),
            );
        }
        let swapped = i32::from(rax == old_lo && rdx == old_hi);
        if swapped != 0 {
            return true;
        }
    }
    false
}

/// Write `patch` at `target` (12 bytes) and flush the instruction cache.
///
/// # Safety
///
/// `target` must point at an executable, readable function entry and `patch`
/// must be exactly [`PATCH_LEN`] bytes.
#[cfg(windows)]
pub unsafe fn patch_entry(target: usize, patch: &[u8; PATCH_LEN]) -> Result<(), String> {
    let mut old_protect = 0u32;
    // SAFETY: target is a function entry; the page is made writable for the
    // duration of the patch and restored afterwards.
    let ok = unsafe {
        win::VirtualProtect(
            target as *mut core::ffi::c_void,
            PATCH_LEN + 4,
            win::PAGE_EXECUTE_READWRITE,
            &mut old_protect,
        )
    };
    if ok == 0 {
        return Err(format!("VirtualProtect failed at {target:#x}"));
    }

    let mut done = false;
    if target % 16 <= 4 {
        let base16 = target & !0xF;
        let mut window = [0u8; 16];
        // SAFETY: the block around the entry is inside the same page that was
        // just made writable (entry alignment is checked above).
        unsafe {
            core::ptr::copy_nonoverlapping(base16 as *const u8, window.as_mut_ptr(), 16);
        }
        let offset = target - base16;
        window[offset..offset + PATCH_LEN].copy_from_slice(patch);
        // SAFETY: `base16` is 16-byte aligned and writable.
        done = unsafe { cas16(base16, &window) };
        if !done {
            // A concurrent writer moved the window: fall through to the plain
            // copy below, which is what the shim always did.
        }
    }
    if !done {
        // SAFETY: destination is patched after VirtualProtect; source is a
        // local 12-byte array.
        unsafe {
            core::ptr::copy_nonoverlapping(patch.as_ptr(), target as *mut u8, PATCH_LEN);
        }
    }

    // SAFETY: restore the original protection; the value came from
    // VirtualProtect above.
    unsafe {
        win::VirtualProtect(
            target as *mut core::ffi::c_void,
            PATCH_LEN + 4,
            old_protect,
            &mut old_protect,
        );
    }
    // SAFETY: flush the patched range in the current process.
    unsafe {
        win::FlushInstructionCache(
            win::GetCurrentProcess(),
            target as *const core::ffi::c_void,
            PATCH_LEN,
        );
    }
    Ok(())
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
    use std::sync::OnceLock;
    static TABLE: OnceLock<usize> = OnceLock::new();
    let address = *TABLE.get_or_init(|| {
        // SAFETY: named mapping creation is process-wide; a null name buffer is
        // not used, the name is a NUL-terminated ASCII literal.
        let mapping = unsafe {
            win::CreateFileMappingA(
                win::INVALID_HANDLE,
                core::ptr::null_mut(),
                win::PAGE_READWRITE,
                0,
                core::mem::size_of::<RawTable>() as u32,
                c"Local\\hromatite_rust_hooktable_v1".as_ptr(),
            )
        };
        if mapping == 0 {
            return 0;
        }
        // SAFETY: the mapping handle is valid for the whole process.
        let view = unsafe { win::MapViewOfFile(mapping, win::FILE_MAP_ALL_ACCESS, 0, 0, 0) };
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

#[cfg(windows)]
fn table_lookup(target: usize) -> Option<TableEntry> {
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

#[cfg(windows)]
fn table_store(target: usize, tramp: usize, handler: usize, name: &str) {
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

/// Install one hook. Returns the trampoline the detour must tail-call.
///
/// # Safety
///
/// `target` must be the entry of an executable function whose first
/// [`PATCH_LEN`]-covering prologue is decodable, and `handler` must be a
/// `extern "system"` function taking the same arguments as `target` and
/// returning the same type. Both addresses must stay valid for the process
/// lifetime.
#[cfg(windows)]
pub unsafe fn install(target: usize, handler: usize, name: &str) -> Result<Hook, String> {
    if target == 0 {
        return Err("no export".to_string());
    }
    // SAFETY: the export entry is readable; 32 bytes are copied only after
    // probing the page.
    if !unsafe { readable(target, 32) } {
        return Err("target unreadable".to_string());
    }
    let mut original = [0u8; 32];
    // SAFETY: readable() above proved 32 readable bytes at the entry.
    unsafe {
        core::ptr::copy_nonoverlapping(target as *const u8, original.as_mut_ptr(), 32);
    }

    if let Some(imm) = looks_patched(&original) {
        if let Some(entry) = table_lookup(target) {
            let tramp = entry.tramp as usize;
            let mode = if entry.handler == handler as u64 {
                2
            } else {
                3
            };
            if mode == 3 {
                // Take ownership of the existing trampoline: patch the jump to
                // our handler so this (live) image keeps counting the calls.
                // SAFETY: the entry is patched already, so a 12-byte atomic
                // rewrite of the same window is safe.
                unsafe { patch_entry(target, &abs_jmp(handler))? };
                table_store(target, tramp, handler, name);
            }
            return Ok(Hook {
                target,
                tramp,
                copied: 0,
                note: String::new(),
                mode,
            });
        }
        // Someone else patched it and we know nothing about the trampoline:
        // forward to the handler it points at.
        return Ok(Hook {
            target,
            tramp: imm,
            copied: 0,
            note: String::new(),
            mode: 3,
        });
    }

    let total = prologue_len(&original).ok_or_else(|| "prologue undecodable".to_string())?;
    if total < PATCH_LEN {
        return Err(format!("prologue {total} < {PATCH_LEN}"));
    }

    // SAFETY: OS allocation near the target; returned page is RWX.
    let page = unsafe { alloc_near(target, TRAMP_PAGE) }
        .ok_or_else(|| "VirtualAlloc failed".to_string())?;
    // SAFETY: the page is a fresh, writable TRAMP_PAGE-byte allocation.
    let tramp = unsafe { core::slice::from_raw_parts_mut(page as *mut u8, TRAMP_PAGE) };
    let built = build_trampoline(tramp, target, &original, total)
        .ok_or_else(|| "trampoline relocation failed".to_string())?;

    // Publish the trampoline before patching the target, so a thread that
    // enters the hook immediately always finds a complete trampoline.
    // SAFETY: flush the freshly written trampoline code before it is reachable.
    unsafe {
        win::FlushInstructionCache(
            win::GetCurrentProcess(),
            page as *const core::ffi::c_void,
            built.len,
        );
    }

    // SAFETY: patch the validated entry with an absolute jump to the handler.
    unsafe { patch_entry(target, &abs_jmp(handler))? };

    let mut check = [0u8; 2];
    // SAFETY: read back the two opcode bytes just written.
    unsafe {
        core::ptr::copy_nonoverlapping(target as *const u8, check.as_mut_ptr(), 2);
    }
    if check != [0x48, 0xB8] {
        return Err("readback mismatch after patch".to_string());
    }

    table_store(target, page, handler, name);
    Ok(Hook {
        target,
        tramp: page,
        copied: total,
        note: built.note,
        mode: 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact prologue observed live in `eacshim.log`:
    /// `48 83 ec 28 | 48 85 c9 | 74 13 | 48 85 d2` (4+3+2+3 = 12 bytes).
    const LIVE_PROLOGUE: [u8; 16] = [
        0x48, 0x83, 0xEC, 0x28, 0x48, 0x85, 0xC9, 0x74, 0x13, 0x48, 0x85, 0xD2, 0x48, 0x8B, 0x01,
        0xC3,
    ];

    #[test]
    fn decode_matches_the_measured_prologue() {
        let first = decode(&LIVE_PROLOGUE).expect("sub rsp");
        assert_eq!(first.len, 4);
        let second = decode(&LIVE_PROLOGUE[4..]).expect("test rcx");
        assert_eq!(second.len, 3);
        let third = decode(&LIVE_PROLOGUE[7..]).expect("jz rel8");
        assert_eq!(third.len, 2);
        assert!(third.rel);
        assert_eq!(third.rel_size, 1);
        let fourth = decode(&LIVE_PROLOGUE[9..]).expect("test rdx");
        assert_eq!(fourth.len, 3);
    }

    #[test]
    fn prologue_len_is_twelve_for_the_live_export() {
        assert_eq!(prologue_len(&LIVE_PROLOGUE), Some(12));
        // `ret` alone cannot reach 12 bytes: decode keeps walking into what
        // follows, which is exactly what the shim does (padding/int3).
        let padding = [
            0x0F, 0x1F, 0x00, 0x0F, 0x1F, 0x00, 0x0F, 0x1F, 0x00, 0x0F, 0x1F, 0x00,
        ];
        assert_eq!(prologue_len(&padding), Some(12));
        assert_eq!(prologue_len(&[0x06]), None, "unknown opcode");
    }

    #[test]
    fn rel8_jcc_is_rewritten_as_inverse_jcc_plus_absolute_jump() {
        // Trampoline placed at +0x100000 from the target: a rel8 target cannot
        // reach, so the absolute form (inverse jcc +14 ; jmp [rip+0] <abs>)
        // must be emitted, exactly like `jcc-abs+7` in the live log — but with
        // a *register-free* jump, so the prologue keeps rax intact.
        let base = 0x6fff_e24e_6990usize;
        let tramp_addr = 0x6fff_e1e7_0000usize;
        let mut page = vec![0u8; 4096];
        // Simulate the trampoline living at `tramp_addr` by only checking the
        // emitted bytes, which do not depend on the page address for rel8.
        let built = build_trampoline(&mut page, base, &LIVE_PROLOGUE, 12).expect("relocate");
        assert!(built.note.starts_with("jcc-abs+7->"), "{}", built.note);
        // 4 + 3 + (2 + 14) + 3 + 14
        assert_eq!(built.len, 40);

        assert_eq!(&page[0..4], &[0x48, 0x83, 0xEC, 0x28]);
        assert_eq!(&page[4..7], &[0x48, 0x85, 0xC9]);
        // `74 13` -> inverse `75 0E` over `jmp qword [rip+0]`.
        assert_eq!(&page[7..9], &[0x75, ABS_JUMP as u8]);
        assert_eq!(&page[9..11], &[0xFF, 0x25]);
        let mut target_bytes = [0u8; 8];
        target_bytes.copy_from_slice(&page[15..23]);
        // jz target = base + 9 + 0x13 = base + 0x1C.
        assert_eq!(u64::from_le_bytes(target_bytes), (base + 0x1C) as u64);
        // Verbatim `test rdx,rdx` right after the 14-byte absolute jump.
        assert_eq!(&page[23..26], &[0x48, 0x85, 0xD2]);
        // Final register-free jump back to entry + 12.
        assert_eq!(&page[26..28], &[0xFF, 0x25]);
        let mut back = [0u8; 8];
        back.copy_from_slice(&page[32..40]);
        assert_eq!(u64::from_le_bytes(back), (base + 12) as u64);

        let _ = tramp_addr;
    }

    #[test]
    fn rip_relative_operand_is_retargeted_to_the_copy() {
        // `mov rax, [rip+0x100]` followed by `ret`; the copy must point at the
        // far target, not at the original +0x100. The trampoline must be near
        // the target or a disp32 cannot reach, so the synthetic target lives in
        // the same page as the copy (on Windows `alloc_near` guarantees this).
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x800;
        let code = [
            0x48, 0x8B, 0x05, 0x00, 0x01, 0x00, 0x00, 0xC3, 0xCC, 0xCC, 0xCC, 0xCC,
        ];
        let built = build_trampoline(&mut page, base, &code, 12).expect("relocate");
        assert!(built.note.starts_with("rip-rel+0"), "{}", built.note);
        let disp = i32::from_le_bytes(page[3..7].try_into().unwrap());
        // The relocated copy sits at `page_addr`; the operand must resolve to
        // the *original* absolute target `base + 7 + 0x100`.
        let resolved = (page_addr + 7) as i64 + i64::from(disp);
        assert_eq!(
            resolved,
            (base + 0x107) as i64,
            "disp must still resolve the original target"
        );
    }

    #[test]
    fn call_rel32_in_prologue_targets_the_original_callee() {
        // The BoringSSL `__chkstk` entry shape measured live on
        // `EVP_EncryptInit_ex`: `mov eax,0x28 ; call rel32 ; sub rsp,0x28`.
        // The relocated copy must call the *original* callee.
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x800;
        let callee = base + 5 + 5 + 0x40;

        let mut code = [0x90u8; 14];
        code[0..5].copy_from_slice(&[0xB8, 0x28, 0x00, 0x00, 0x00]);
        code[5] = 0xE8;
        code[6..10].copy_from_slice(&(callee as i64 - (base + 10) as i64).to_le_bytes()[..4]);
        code[10..14].copy_from_slice(&[0x48, 0x83, 0xEC, 0x28]);

        let built = build_trampoline(&mut page, base, &code, 14).expect("relocate");
        assert!(built.note.starts_with("call+5->"), "{}", built.note);
        assert_eq!(&page[5], &0xE8);
        let disp = i32::from_le_bytes(page[6..10].try_into().unwrap());
        assert_eq!(
            (page_addr + 5 + 5) as i64 + i64::from(disp),
            callee as i64,
            "the relocated call must still reach the original callee"
        );
    }

    #[test]
    fn lea_rip_relative_in_prologue_is_retargeted() {
        // `lea rcx,[rip+0x400] ; mov eax,[rip+0x100] ; ret`
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x800;
        let mut code = [0xCCu8; 17];
        code[0..3].copy_from_slice(&[0x48, 0x8D, 0x0D]);
        code[3..7].copy_from_slice(&0x400i32.to_le_bytes());
        code[7..9].copy_from_slice(&[0x8B, 0x05]);
        code[9..13].copy_from_slice(&0x100i32.to_le_bytes());
        code[13] = 0xC3;

        let built = build_trampoline(&mut page, base, &code, 14).expect("relocate");
        assert!(built.note.starts_with("rip-rel+0"), "{}", built.note);
        assert!(built.note.contains("rip-rel+7"), "{}", built.note);
        let lea = i32::from_le_bytes(page[3..7].try_into().unwrap());
        assert_eq!(
            (page_addr + 7) as i64 + i64::from(lea),
            (base + 7 + 0x400) as i64
        );
        let mov = i32::from_le_bytes(page[9..13].try_into().unwrap());
        assert_eq!(
            (page_addr + 13) as i64 + i64::from(mov),
            (base + 13 + 0x100) as i64
        );
    }

    #[test]
    fn every_relocation_is_reported_not_just_the_first() {
        // `mov eax,0x28 ; call rel32 ; lea rdx,[rip+x] ; ret`
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x800;
        let callee = base + 0x100;
        let mut code = [0xCCu8; 19];
        code[0..5].copy_from_slice(&[0xB8, 0x28, 0x00, 0x00, 0x00]);
        code[5] = 0xE8;
        code[6..10].copy_from_slice(&((callee as i64 - (base + 10) as i64) as i32).to_le_bytes());
        code[10..13].copy_from_slice(&[0x48, 0x8D, 0x15]);
        code[13..17].copy_from_slice(&0x200i32.to_le_bytes());
        code[17] = 0xC3;

        let built = build_trampoline(&mut page, base, &code, 18).expect("relocate");
        assert!(built.note.contains("call+5"), "{}", built.note);
        assert!(built.note.contains("rip-rel+10"), "{}", built.note);
    }

    #[test]
    fn prologue_len_never_splits_an_instruction() {
        // 12 bytes fall inside `sub rsp,0x28` (bytes 10..14) — the span must
        // round up to the whole instruction, never to 12.
        let chkstk = [
            0xB8, 0x28, 0x00, 0x00, 0x00, // mov eax,0x28
            0xE8, 0x10, 0x00, 0x00, 0x00, // call rel32
            0x48, 0x83, 0xEC, 0x28, // sub rsp,0x28
        ];
        assert_eq!(prologue_len(&chkstk), Some(14));
        // A truncated instruction at the end of the snapshot is not accepted.
        assert_eq!(prologue_len(&chkstk[..12]), None);
    }

    #[test]
    fn group3_registers_without_an_immediate_decode_short() {
        assert_eq!(decode(&[0x48, 0xF7, 0xD8]).map(|i| i.len), Some(3)); // neg rax
        assert_eq!(
            decode(&[0x48, 0xF7, 0xC1, 1, 0, 0, 0]).map(|i| i.len),
            Some(7),
            "test rcx, 1 keeps its imm32"
        );
        assert_eq!(decode(&[0xF6, 0xD3]).map(|i| i.len), Some(2)); // not bl
    }

    #[test]
    fn abs_jmp_round_trips_through_looks_patched() {
        let bytes = abs_jmp(0x1234_5678_9ABC_DEF0);
        assert_eq!(looks_patched(&bytes), Some(0x1234_5678_9ABC_DEF0));
        assert_eq!(looks_patched(&[0x90; 12]), None);
    }
}
