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
//! * rip-relative `disp32` — retargeted to the same absolute address;
//! * `jcc rel8` — the **inverted** condition hopping over a register-free
//!   `jmp qword [rip+0] <target>` (16 bytes): a taken branch runs the absolute
//!   jump, a not-taken one lands on the fall-through;
//! * `loop`/`loope`/`loopne`/`jrcxz` (`E0..E3`) — the same, spelled
//!   `loop +2 ; jmp short over ; jmp [rip+0] <target>` because those have no
//!   inverse opcode;
//! * `jcc rel32`/`jmp rel32`/`call rel32` — the original encoding while the new
//!   displacement still fits, else `jmp [rip+0] <target>` or
//!   `call [rip+2] ; jmp +8 ; dq target` (all register-free: the copied
//!   prologue may still be using `rax`);
//! * a branch whose target is one of the copied bytes follows the **copy**,
//!   since those bytes are now the patch;
//! * anything else — copied verbatim.
//!
//! The trampoline ends with an absolute jump to `entry + total`, never to
//! `entry` (a self-jump would resume at the patched prologue and recurse).
//!
//! A named shared section (`Local\hromatite_rust_hooktable_v1`) records
//! `{target -> trampoline, handler}` so a second injection into the same process
//! reuses the existing trampoline instead of re-hooking already patched bytes.
// Every target of this module is a Win32 code patch; on the host the
// bookkeeping statics are simply unused.
#![cfg_attr(not(windows), allow(dead_code))]

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
    /// Offset of the primary opcode byte (after legacy prefixes and REX), so a
    /// relative branch is classified from the opcode and not from `code[0]`.
    pub op_off: usize,
    /// The branch is conditional (`jcc`, `loop`, `jrcxz`). Only those can be
    /// rewritten as `cond-short over an absolute jump`.
    pub cond: bool,
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
    let op_off = i;
    let op = *code.get(i)?;
    i += 1;

    let mut out = Insn {
        op_off,
        ..Insn::default()
    };
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
                out.cond = true;
                out.rel_off = i;
                out.rel_size = 4;
                i += 4;
                out.len = i;
                return Some(out);
            }
            0x05 | 0x0B | 0x31 | 0x77 | 0xA2 | 0x34 | 0x35 | 0x0D => {}
            // `pshufd` and friends: ModRM *and* an imm8 the generic path would miss
            0x70 => {
                has_modrm = true;
                imm = 1;
            }
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
            0x70..=0x7F => {
                out.rel = true;
                out.cond = true;
                out.rel_off = i;
                out.rel_size = 1;
                i += 1;
                out.len = i;
                return Some(out);
            }
            0xEB => {
                out.rel = true;
                out.rel_off = i;
                out.rel_size = 1;
                i += 1;
                out.len = i;
                return Some(out);
            }
            // `loopne`/`loope`/`loop`/`jrcxz` — short conditional branches with
            // the same rewrite as a `jcc` (the condition itself is not inverted,
            // only the *target* moves over the absolute jump).
            0xE0..=0xE3 => {
                out.rel = true;
                out.cond = true;
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

/// Length of a prologue covering at least [`PATCH_LEN`] whole instructions, and
/// how many they are. `Err` names the first byte that cannot be decoded, so a
/// refused hook can say *why* instead of silently skipping.
pub fn prologue_span(code: &[u8]) -> Result<(usize, usize), String> {
    let mut n = 0usize;
    let mut insns = 0usize;
    while n < PATCH_LEN {
        if insns > 8 {
            return Err(format!("more than 8 instructions within {PATCH_LEN} bytes"));
        }
        let insn = decode(code.get(n..).unwrap_or(&[])).ok_or_else(|| {
            format!(
                "undecodable opcode {:#04x} at +{n}",
                code.get(n).copied().unwrap_or(0)
            )
        })?;
        if insn.len == 0 {
            return Err(format!("zero-length instruction at +{n}"));
        }
        n += insn.len;
        insns += 1;
    }
    if n > 24 {
        return Err(format!("prologue {n} bytes exceeds the 24-byte bound"));
    }
    Ok((n, insns))
}

/// Length of a prologue covering at least [`PATCH_LEN`] bytes. `None` when an
/// unknown opcode is hit or the span exceeds 24 bytes (the shim's bound).
pub fn prologue_len(code: &[u8]) -> Option<usize> {
    prologue_span(code).ok().map(|(len, _)| len)
}

/// Length of `jmp qword ptr [rip+0] ; dq addr` — the register-free absolute
/// jump. `mov rax,imm64 ; jmp rax` is two bytes shorter but destroys `rax`,
/// which a relocated prologue may still be using (`mov eax,0x28 ; call
/// __chkstk ; sub rsp,rax` hands the stack size over in `rax`).
pub const ABS_JUMP: usize = 14;

/// Length of `call qword ptr [rip+2] ; jmp +8 ; dq addr` — the register-free
/// absolute call. The extra `jmp` hops over the address so the fall-through
/// continues right after the call.
pub const ABS_CALL: usize = 16;

/// Write `jmp qword ptr [rip+0] ; dq addr` into a 14-byte slot.
fn push_abs_jump_slice(slot: &mut [u8], addr: usize) -> Result<(), String> {
    let slot = slot
        .get_mut(..ABS_JUMP)
        .ok_or_else(|| "absolute jump does not fit in the trampoline page".to_string())?;
    slot[..6].copy_from_slice(&[0xFF, 0x25, 0, 0, 0, 0]);
    slot[6..].copy_from_slice(&(addr as u64).to_le_bytes());
    Ok(())
}

/// Write `call qword ptr [rip+2] ; jmp +8 ; dq addr` into a 16-byte slot.
fn push_abs_call_slice(slot: &mut [u8], addr: usize) -> Result<(), String> {
    let slot = slot
        .get_mut(..ABS_CALL)
        .ok_or_else(|| "absolute call does not fit in the trampoline page".to_string())?;
    slot[..6].copy_from_slice(&[0xFF, 0x15, 0x02, 0, 0, 0]);
    slot[6..8].copy_from_slice(&[0xEB, (ABS_CALL - 8) as u8]);
    slot[8..].copy_from_slice(&(addr as u64).to_le_bytes());
    Ok(())
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

/// One field of the copied prologue that had to be re-encoded, kept as data so
/// the install log can name every rewrite and its distance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reloc {
    /// `call-rel32`, `call-abs`, `jmp-rel32`, `jmp-abs`, `jcc-rel32`, `jcc-abs`
    /// or `rip-rel`.
    pub kind: &'static str,
    /// Offset of the instruction inside the original prologue.
    pub at: usize,
    /// Absolute address the field now points at (the copy itself for a branch
    /// that stayed inside the prologue).
    pub target: i64,
    /// `target - (address of the rewritten instruction)`, signed.
    pub distance: i64,
}

impl Reloc {
    /// `call-rel32+5->0x6fffe2c98890(-0x18ffe6)` — kind, prologue offset,
    /// absolute target and the signed distance from the copy to it.
    pub fn label(&self) -> String {
        format!(
            "{}+{}->{:#x}({:+})",
            self.kind, self.at, self.target, self.distance
        )
    }
}

/// The relocated trampoline plus everything the install log reports about it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Built {
    /// Bytes written into the trampoline page.
    pub len: usize,
    /// Human-readable relocation note (`jcc-abs+7->0x…(+0x…)`, comma joined;
    /// `verbatim` when nothing moved).
    pub note: String,
    /// How many instructions the copied prologue holds.
    pub insns: usize,
    /// Every rewritten field, in prologue order.
    pub relocs: Vec<Reloc>,
}

fn read_i32(code: &[u8], at: usize) -> Option<i32> {
    let bytes = code.get(at..at + 4)?;
    Some(i32::from_le_bytes(bytes.try_into().ok()?))
}

fn fits_i32(value: i64) -> bool {
    (i32::MIN as i64..=i32::MAX as i64).contains(&value)
}

/// How one copied instruction is re-encoded at its new address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Form {
    /// Copied byte for byte.
    Verbatim,
    /// Kept, with its `disp32` retargeted.
    Rip32,
    /// Kept verbatim, with the relative field rewritten in place (any form that
    /// still reaches from the copy).
    Rel,
    /// `inverse-jcc +ABS_JUMP ; jmp [rip+0] <target>`: the *inverted* condition
    /// skips the absolute jump into the fall-through, so a taken branch still
    /// reaches the original target.
    CondAbs,
    /// `loop/jrcxz +2 ; jmp-short over ; jmp [rip+0] <target>` for `E0..E3`,
    /// which have no inverse opcode (17-18 bytes).
    LoopAbs,
    /// `0F 8x ; jmp [rip+0] <target>` with the inverted condition (a rel32 `jcc`
    /// out of reach).
    CondAbsLong,
    JmpRel32,
    JmpAbs,
    CallRel32,
    /// `call [rip+2] ; jmp +8 ; dq <target>`.
    CallAbs,
}

impl Form {
    fn size(self, insn: &Insn) -> usize {
        match self {
            // The absolute conditional forms keep whatever prefixes the original
            // carried, so their size depends on where the relative field sits.
            Form::Verbatim | Form::Rip32 | Form::Rel => insn.len,
            Form::CondAbs => insn.rel_off + 1 + ABS_JUMP,
            Form::LoopAbs => insn.rel_off + 1 + 2 + ABS_JUMP,
            Form::CondAbsLong => insn.rel_off + 4 + ABS_JUMP,
            Form::JmpRel32 | Form::CallRel32 => 5,
            Form::JmpAbs => ABS_JUMP,
            Form::CallAbs => ABS_CALL,
        }
    }

    fn kind(self) -> &'static str {
        match self {
            Form::Verbatim => "verbatim",
            Form::Rip32 => "rip-rel",
            Form::Rel => "rel32",
            Form::CondAbs | Form::LoopAbs | Form::CondAbsLong => "jcc-abs",
            Form::JmpRel32 => "jmp-rel32",
            Form::JmpAbs => "jmp-abs",
            Form::CallRel32 => "call-rel32",
            Form::CallAbs => "call-abs",
        }
    }
}

/// One instruction of the planned copy: where it starts, how it is encoded and
/// where its relative field points before the move.
#[derive(Clone, Copy, Debug)]
struct Step {
    /// Offset inside the original prologue.
    old: usize,
    insn: Insn,
    /// Offset inside the trampoline copy.
    new: usize,
    form: Form,
    /// Absolute branch target, or the absolute address a rip-relative operand
    /// resolves to.
    target: i64,
}

fn rel_disp(insn: &Insn, code: &[u8], old: usize) -> Option<i64> {
    if insn.rel_size == 1 {
        Some(i64::from(*code.get(old + insn.rel_off)? as i8))
    } else {
        Some(i64::from(read_i32(code, old + insn.rel_off)?))
    }
}

/// Choose the encoding of one instruction. Needs only the original prologue and
/// the layout so far, so the same decision is available before and after the
/// offsets of the copy are known.
fn plan(
    insn: &Insn,
    code: &[u8],
    old: usize,
    base: usize,
    copied: usize,
    tramp_addr: usize,
    new_off: usize,
) -> Result<(Form, i64), String> {
    if !insn.rel && !insn.rip_rel {
        return Ok((Form::Verbatim, 0));
    }
    // A rip-relative operand keeps its `disp32` position (`disp_off`), a branch
    // its relative field (`rel_off`); both are resolved to an absolute address
    // here and re-encoded at the new offset later.
    let disp = if insn.rip_rel {
        i64::from(
            read_i32(code, old + insn.disp_off)
                .ok_or_else(|| format!("rip disp32 at +{old} is truncated"))?,
        )
    } else {
        rel_disp(insn, code, old).ok_or_else(|| format!("relative field at +{old} is truncated"))?
    };
    let next = (base + old + insn.len) as i64;
    let target = next + disp;
    if insn.rip_rel {
        return Ok((Form::Rip32, target));
    }
    // A branch that lands inside the prologue follows the *copy*, because the
    // first PATCH_LEN bytes of the original are the `mov rax,imm64 ; jmp rax`
    // patch — jumping there would re-enter the detour (or a stale handler).
    let inside = target >= base as i64 && target < (base + copied) as i64;
    if insn.cond {
        if insn.rel_size == 1 {
            // A rel8 condition reaches 127 bytes; the copy lives in another
            // page, so the short form never survives relocation. `jcc` has an
            // inverse opcode, the `loop`/`jrcxz` family does not.
            let opcode = *code
                .get(old + insn.rel_off - 1)
                .ok_or_else(|| format!("branch opcode at +{old} is truncated"))?;
            let form = if (0x70..=0x7F).contains(&opcode) {
                Form::CondAbs
            } else {
                Form::LoopAbs
            };
            return Ok((form, target));
        }
        let fits = inside || fits_i32(target - (tramp_addr + new_off + 6) as i64);
        return Ok((if fits { Form::Rel } else { Form::CondAbsLong }, target));
    }
    let op = *code
        .get(old + insn.op_off)
        .ok_or_else(|| format!("opcode at +{old} is past the snapshot"))?;
    let is_call = op == 0xE8;
    let fits = inside || fits_i32(target - (tramp_addr + new_off + 5) as i64);
    Ok(match (is_call, fits) {
        (true, true) => (Form::CallRel32, target),
        (true, false) => (Form::CallAbs, target),
        (false, true) => (Form::JmpRel32, target),
        (false, false) => (Form::JmpAbs, target),
    })
}

/// Follow a branch of the copy back into the copy when its target is one of the
/// overwritten prologue bytes; anything else keeps its original address.
fn map_target(
    target: i64,
    base: usize,
    copied: usize,
    steps: &[Step],
    tramp_addr: usize,
) -> Result<i64, String> {
    if target >= base as i64 && target < (base + copied) as i64 {
        let old = (target - base as i64) as usize;
        return steps
            .iter()
            .find(|step| step.old == old)
            .map(|step| (tramp_addr + step.new) as i64)
            .ok_or_else(|| {
                format!("branch target +{old:#x} lands in the middle of an instruction")
            });
    }
    Ok(target)
}

fn page_full(at: usize) -> String {
    format!("trampoline page is too small at +{at}")
}

/// Copy and relocate the prologue into `tramp`.
///
/// `base` is the original entry address, `total` the number of bytes to copy
/// (from [`prologue_len`]); `tramp`'s own address is used to re-encode every
/// relative field. Returns the emitted length and the relocation note.
///
/// The final absolute jump goes to `base + total` — resuming at `base` would
/// re-enter the patch. `None` means the prologue cannot be relocated safely.
pub fn build_trampoline(
    tramp: &mut [u8],
    base: usize,
    code: &[u8],
    total: usize,
) -> Result<Built, String> {
    let tramp_addr = tramp.as_ptr() as usize;

    // ---- pass 1: decode the prologue and lay the copy out ------------------
    let mut steps: Vec<Step> = Vec::with_capacity(8);
    let mut old = 0usize;
    let mut nw = 0usize;
    while old < total {
        let insn = decode(code.get(old..).unwrap_or(&[])).ok_or_else(|| {
            format!(
                "undecodable opcode {:#04x} at +{old}",
                code.get(old).copied().unwrap_or(0)
            )
        })?;
        if insn.len == 0 {
            return Err(format!("zero-length instruction at +{old}"));
        }
        let (form, target) = plan(&insn, code, old, base, total, tramp_addr, nw)?;
        steps.push(Step {
            old,
            insn,
            new: nw,
            form,
            target,
        });
        nw += form.size(&insn);
        old += insn.len;
    }
    if old != total {
        return Err(format!("decoded {old} of {total} prologue bytes"));
    }

    // ---- pass 2: emit, now that every target offset is known ---------------
    let mut relocs: Vec<Reloc> = Vec::new();
    for step in &steps {
        let insn = &step.insn;
        let at = step.new;
        let size = step.form.size(insn);
        let chunk = code
            .get(step.old..step.old + insn.len)
            .ok_or_else(|| format!("instruction at +{} is truncated", step.old))?;
        let out = tramp.get_mut(at..at + size).ok_or_else(|| page_full(at))?;
        match step.form {
            Form::Verbatim => out.copy_from_slice(chunk),
            Form::Rip32 => {
                let next = (tramp_addr + at + insn.len) as i64;
                let disp = i32::try_from(step.target - next).map_err(|_| {
                    format!(
                        "rip-relative operand at +{} ({:#x}) is out of disp32 reach",
                        step.old, step.target
                    )
                })?;
                out.copy_from_slice(chunk);
                out[insn.disp_off..insn.disp_off + 4].copy_from_slice(&disp.to_le_bytes());
                relocs.push(Reloc {
                    kind: Form::Rip32.kind(),
                    at: step.old,
                    target: step.target,
                    distance: step.target - (tramp_addr + at) as i64,
                });
            }
            form => {
                let target = map_target(step.target, base, total, &steps, tramp_addr)?;
                match form {
                    Form::Rel => {
                        let ro = insn.rel_off;
                        let bits = insn.rel_size;
                        let disp = target - (tramp_addr + at + insn.len) as i64;
                        out.copy_from_slice(chunk);
                        if bits == 32 {
                            let v = i32::try_from(disp)
                                .map_err(|_| format!("rel32 at +{} lost its reach", step.old))?;
                            out[ro..ro + 4].copy_from_slice(&v.to_le_bytes());
                        } else {
                            let v = i8::try_from(disp)
                                .map_err(|_| format!("rel8 at +{} lost its reach", step.old))?;
                            out[ro] = v as u8;
                        }
                    }
                    Form::CondAbs => {
                        // `jcc <disp8>`: keep the prefixes, invert the condition
                        // so that *taking* the branch runs the absolute jump to
                        // the original target while *not* taking it lands on the
                        // fall-through right after the 14-byte jump.
                        let keep = insn.rel_off;
                        out[..keep].copy_from_slice(&chunk[..keep]);
                        out[keep - 1] ^= 1;
                        out[keep] = ABS_JUMP as u8;
                        push_abs_jump_slice(&mut out[keep + 1..], target as usize)?;
                    }
                    Form::LoopAbs => {
                        // `loop`/`loope`/`loopne`/`jrcxz` have no inverse
                        // opcode: keep it as it is, and let the *taken* branch
                        // skip a `jmp short` that would have skipped the
                        // absolute jump.
                        let keep = insn.rel_off;
                        out[..keep].copy_from_slice(&chunk[..keep]);
                        out[keep] = 2;
                        out[keep + 1] = 0xEB;
                        out[keep + 2] = ABS_JUMP as u8;
                        push_abs_jump_slice(&mut out[keep + 3..], target as usize)?;
                    }
                    Form::CondAbsLong => {
                        // `0F 8x <disp32>` out of reach: same shape, one byte
                        // wider prefix/opcode part, still inverted.
                        let keep = insn.rel_off;
                        out[..keep].copy_from_slice(&chunk[..keep]);
                        out[keep - 1] ^= 1;
                        out[keep..keep + 4].copy_from_slice(&(ABS_JUMP as i32).to_le_bytes());
                        push_abs_jump_slice(&mut out[keep + 4..], target as usize)?;
                    }
                    Form::JmpRel32 | Form::CallRel32 => {
                        out[0] = if form == Form::CallRel32 { 0xE8 } else { 0xE9 };
                        let disp = i32::try_from(target - (tramp_addr + at + 5) as i64)
                            .map_err(|_| format!("branch at +{} lost its reach", step.old))?;
                        out[1..5].copy_from_slice(&disp.to_le_bytes());
                    }
                    Form::JmpAbs => push_abs_jump_slice(out, target as usize)?,
                    Form::CallAbs => push_abs_call_slice(out, target as usize)?,
                    Form::Verbatim | Form::Rip32 => unreachable!("handled above"),
                }
                relocs.push(Reloc {
                    kind: form.kind(),
                    at: step.old,
                    target,
                    distance: target - (tramp_addr + at) as i64,
                });
            }
        }
    }

    // jump back to `base + total` without touching any register: the copied
    // prologue may have just computed a value in rax that the rest of the
    // function needs (e.g. the `__chkstk` result used by `sub rsp,rax`).
    push_abs_jump_slice(
        tramp.get_mut(nw..).ok_or_else(|| page_full(nw))?,
        base + total,
    )?;
    let len = nw + ABS_JUMP;
    let note = if relocs.is_empty() {
        "verbatim".to_string()
    } else {
        relocs
            .iter()
            .map(Reloc::label)
            .collect::<Vec<_>>()
            .join(",")
    };
    Ok(Built {
        len,
        note,
        insns: steps.len(),
        relocs,
    })
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
    /// How many instructions the copied prologue holds (0 when reused).
    pub insns: usize,
    /// How many of those were re-encoded (0 when reused).
    pub relocs: usize,
    /// 1 = fresh patch, 2 = reused a trampoline from the shared table,
    /// 3 = the entry was patched by a foreign image (forwarded).
    pub mode: u8,
}

impl Hook {
    /// One log line naming what the trampoline contains: prologue length and
    /// instruction count, how many fields moved and in which form, the
    /// trampoline address and its distance from the target.
    pub fn describe(&self) -> String {
        let delta = self.tramp as i64 - self.target as i64;
        if self.copied == 0 {
            return format!(
                "mode={} target={:#x} tramp={:#x} ({delta:+#x}, reused) {}",
                self.mode, self.target, self.tramp, self.note
            );
        }
        format!(
            "prologue={}B/{} insn, reloc={}/{} [{}], target={:#x} -> tramp={:#x} ({delta:+#x}, {:.1} MB), mode={}",
            self.copied,
            self.insns,
            self.relocs,
            self.insns,
            if self.note.is_empty() {
                "verbatim"
            } else {
                &self.note
            },
            self.target,
            self.tramp,
            delta as f64 / (1024.0 * 1024.0),
            self.mode
        )
    }
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
pub(crate) mod win {
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
                insns: 0,
                relocs: 0,
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
            insns: 0,
            relocs: 0,
            mode: 3,
        });
    }

    let (total, insns) =
        prologue_span(&original).map_err(|reason| format!("prologue at {target:#x}: {reason}"))?;
    if total < PATCH_LEN {
        return Err(format!("prologue {total} < {PATCH_LEN}"));
    }

    // SAFETY: OS allocation near the target; returned page is RWX.
    let page = unsafe { alloc_near(target, TRAMP_PAGE) }
        .ok_or_else(|| "VirtualAlloc failed".to_string())?;
    // SAFETY: the page is a fresh, writable TRAMP_PAGE-byte allocation.
    let tramp = unsafe { core::slice::from_raw_parts_mut(page as *mut u8, TRAMP_PAGE) };
    let built = build_trampoline(tramp, target, &original, total)
        .map_err(|reason| format!("trampoline relocation at {target:#x}: {reason}"))?;

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
    let hook = Hook {
        target,
        tramp: page,
        copied: total,
        note: built.note,
        insns,
        relocs: built.relocs.len(),
        mode: 1,
    };
    // One line per installed detour: prologue length, instruction count, how
    // many fields were re-encoded and in which form, the trampoline address and
    // its distance from the target. This is the record that makes a live "hook
    // installed but the game died 40 s later" diagnosable.
    crate::eos::hook_log(format!("[hook] {name}: {}", hook.describe()));
    Ok(hook)
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

    /// Absolute address a `jmp [rip+0] ; dq` at `at` jumps to.
    fn abs_jump_target(page: &[u8], at: usize) -> usize {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&page[at + 6..at + 14]);
        u64::from_le_bytes(raw) as usize
    }

    #[test]
    fn rel8_jcc_becomes_a_cond_short_over_an_absolute_jump() {
        // A rel8 condition reaches 127 bytes, the copy lives in another page:
        // `jz +0x1C` must become `jz +14 ; jmp [rip+0] <base+0x1C>`. The
        // condition stays as it was — only the short jump now hops *over* the
        // absolute jump, so the taken branch reaches the fall-through code.
        let mut page = vec![0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x800;
        let built = build_trampoline(&mut page, base, &LIVE_PROLOGUE, 12).expect("relocate");
        assert!(built.note.starts_with("jcc-abs+7->"), "{}", built.note);
        // 4 + 3 + (2 + 14) + 3 + 14
        assert_eq!(built.len, 40);
        assert_eq!(built.insns, 4);
        assert_eq!(built.relocs.len(), 1);

        assert_eq!(&page[0..4], &[0x48, 0x83, 0xEC, 0x28]);
        assert_eq!(&page[4..7], &[0x48, 0x85, 0xC9]);
        // `jz` -> `jnz` over the absolute jump: taking the original branch must
        // fall through to the jump, not skip it.
        assert_eq!(
            &page[7..9],
            &[0x75, ABS_JUMP as u8],
            "jnz over the abs jump"
        );
        assert_eq!(&page[9..11], &[0xFF, 0x25]);
        // jz target = base + 9 + 0x13 = base + 0x1C.
        assert_eq!(abs_jump_target(&page, 9), base + 0x1C);
        // Verbatim `test rdx,rdx` right after the 14-byte absolute jump.
        assert_eq!(&page[23..26], &[0x48, 0x85, 0xD2]);
        assert_eq!(&page[26..28], &[0xFF, 0x25]);
        assert_eq!(abs_jump_target(&page, 26), base + 12);
    }

    #[test]
    fn jrcxz_and_loop_rel8_branches_are_relocated_too() {
        // `sub rsp,0x28 ; jrcxz +6 ; test rcx,rcx ; nop ; nop` — E3 is not a
        // jcc, but the same rewrite applies: keep the opcode and the condition,
        // hop over the absolute jump, land the absolute jump on the target.
        let code = [
            0x48, 0x83, 0xEC, 0x28, // sub rsp,0x28
            0xE3, 0x06, // jrcxz +6 -> base + 12
            0x48, 0x85, 0xC9, // test rcx,rcx
            0x90, 0x90, 0x90, // padding to 12 bytes
        ];
        assert_eq!(prologue_len(&code), Some(12));
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x800;
        let built = build_trampoline(&mut page, base, &code, 12).expect("relocate");
        assert_eq!(built.relocs[0].kind, "jcc-abs");
        assert_eq!(built.relocs[0].at, 4);
        // `jrcxz +2 ; jmp short over ; jmp [rip+0] <target>`: the taken branch
        // skips the `EB` hop and runs the absolute jump.
        assert_eq!(&page[4..8], &[0xE3, 0x02, 0xEB, ABS_JUMP as u8]);
        assert_eq!(abs_jump_target(&page, 8), base + 12);
        // The copied `test rcx,rcx` follows the 18-byte form.
        assert_eq!(&page[22..25], &[0x48, 0x85, 0xC9]);
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
        assert!(built.note.starts_with("call-rel32+5->"), "{}", built.note);
        assert_eq!(&page[5], &0xE8);
        let disp = i32::from_le_bytes(page[6..10].try_into().unwrap());
        assert_eq!(
            (page_addr + 5 + 5) as i64 + i64::from(disp),
            callee as i64,
            "the relocated call must still reach the original callee"
        );
    }

    #[test]
    fn call_rel32_beyond_reach_goes_through_an_absolute_slot() {
        // A trampoline 8 GiB away from the callee: the rel32 form cannot be
        // re-encoded, so the copy must use `call [rip+2] ; jmp +8 ; dq callee`,
        // which is register-free as well.
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x2_0000_0000;
        let callee = base + 0x100;
        let mut code = [0x90u8; 14];
        code[0..5].copy_from_slice(&[0xB8, 0x28, 0, 0, 0]);
        code[5] = 0xE8;
        code[6..10].copy_from_slice(&((callee as i64 - (base + 10) as i64) as i32).to_le_bytes());
        code[10..14].copy_from_slice(&[0x48, 0x83, 0xEC, 0x28]);

        let built = build_trampoline(&mut page, base, &code, 14).expect("relocate");
        assert_eq!(built.relocs[0].kind, "call-abs");
        assert_eq!(&page[5..11], &[0xFF, 0x15, 0x02, 0, 0, 0]);
        assert_eq!(&page[11..13], &[0xEB, (ABS_CALL - 8) as u8]);
        let mut slot = [0u8; 8];
        slot.copy_from_slice(&page[13..21]);
        assert_eq!(u64::from_le_bytes(slot) as usize, callee);
        // The instruction after the call still runs, then the jump back.
        assert_eq!(&page[21..25], &[0x48, 0x83, 0xEC, 0x28]);
    }

    #[test]
    fn jcc_rel32_out_of_reach_becomes_cond_short_plus_absolute() {
        // `sub rsp,0x28 ; jne rel32 ; nop ; nop` copied 8 GiB away: the rel32
        // condition no longer reaches, so it keeps its opcode, hops over the
        // absolute jump and the absolute jump holds the original target.
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x2_0000_0000;
        let mut code = [0x90u8; 12];
        code[0..4].copy_from_slice(&[0x48, 0x83, 0xEC, 0x28]);
        code[4..6].copy_from_slice(&[0x0F, 0x85]);
        code[6..10]
            .copy_from_slice(&(((base + 12) as i64 - (base + 10) as i64) as i32).to_le_bytes());

        let built = build_trampoline(&mut page, base, &code, 12).expect("relocate");
        assert_eq!(built.relocs[0].kind, "jcc-abs");
        // `jne` -> `je` over the absolute jump, so a taken `jne` still reaches
        // the original target.
        assert_eq!(&page[4..6], &[0x0F, 0x84]);
        assert_eq!(
            i32::from_le_bytes(page[6..10].try_into().unwrap()),
            ABS_JUMP as i32
        );
        assert_eq!(abs_jump_target(&page, 10), base + 12);
    }

    #[test]
    fn a_branch_into_the_prologue_follows_the_copy() {
        // `test rcx,rcx ; call rel32(-5)` calls *itself*: after the move the
        // original bytes are the 12-byte `mov rax,imm64 ; jmp rax` patch, so
        // the target has to become the copy of that instruction.
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x800;
        let mut code = [0x90u8; 12];
        code[0..3].copy_from_slice(&[0x48, 0x85, 0xC9]);
        code[3..8].copy_from_slice(&[0xE8, 0xFB, 0xFF, 0xFF, 0xFF]); // call +3

        let built = build_trampoline(&mut page, base, &code, 12).expect("relocate");
        assert_eq!(built.relocs[0].kind, "call-rel32");
        assert_eq!(
            built.relocs[0].target as usize,
            page_addr + 3,
            "the self-call must land in the copy, not on the patched original"
        );
        assert_eq!(i32::from_le_bytes(page[4..8].try_into().unwrap()), -5);
    }

    #[test]
    fn a_branch_into_the_middle_of_an_instruction_is_refused() {
        // `test rcx,rcx ; jmp rel32 -> base+1` (inside the `test`): there is no
        // copy of that byte, so the hook must be refused rather than guess.
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        let base = page_addr + 0x800;
        let mut code = [0x90u8; 12];
        code[0..3].copy_from_slice(&[0x48, 0x85, 0xC9]);
        code[3..8].copy_from_slice(&[0xE9, 0xF9, 0xFF, 0xFF, 0xFF]); // jmp +1
        let err = build_trampoline(&mut page, base, &code, 12).expect_err("mid-instruction target");
        assert!(err.contains("middle of an instruction"), "{err}");
    }

    #[test]
    fn prologue_span_names_the_offending_byte() {
        assert_eq!(prologue_span(&LIVE_PROLOGUE), Ok((12, 4)));
        let err = prologue_span(&[0x48, 0x83, 0xEC, 0x28, 0x06]).expect_err("0x06 is not decoded");
        assert!(err.contains("undecodable opcode 0x06 at +4"), "{err}");
        // A truncated instruction at the end of the snapshot is not accepted.
        assert!(prologue_span(&[0x48, 0x81, 0xEC, 0x28]).is_err());
    }

    #[test]
    fn the_real_boring_ssl_evp_prologues_relocate_their_chkstk_call() {
        // Byte-exact prologues of the five BoringSSL entry points the EOSSDK
        // protector calls, read from `EOSSDK-Win64-Shipping.dll`
        // (`findings/eossdk.md` §6.1). All five start with the `__chkstk`
        // shape `mov eax,0xNN ; call rel32(__chkstk)`, and all five of those
        // calls land on RVA 0xE6B030 — the case that used to crash the game.
        const RVAS: [(usize, [u8; 16]); 5] = [
            (
                0xcd_b040,
                [
                    0xB8, 0x28, 0x00, 0x00, 0x00, 0xE8, 0xE6, 0xFF, 0x18, 0x00, 0x48, 0x2B, 0xE0,
                    0x44, 0x8D, 0x40,
                ],
            ),
            (
                0xcd_bcb0,
                [
                    0xB8, 0x38, 0x00, 0x00, 0x00, 0xE8, 0x76, 0xF3, 0x18, 0x00, 0x48, 0x2B, 0xE0,
                    0x48, 0x8B, 0x44,
                ],
            ),
            (
                0xcd_bce0,
                [
                    0xB8, 0x38, 0x00, 0x00, 0x00, 0xE8, 0x46, 0xF3, 0x18, 0x00, 0x48, 0x2B, 0xE0,
                    0x83, 0x79, 0x10,
                ],
            ),
            (
                0xcd_af90,
                [
                    0xB8, 0x38, 0x00, 0x00, 0x00, 0xE8, 0x96, 0x00, 0x19, 0x00, 0x48, 0x2B, 0xE0,
                    0x48, 0x8B, 0x01,
                ],
            ),
            // No call in its 12-byte prologue: two stores and `push r14`.
            (
                0xcd_bb60,
                [
                    0x48, 0x89, 0x5C, 0x24, 0x18, 0x48, 0x89, 0x6C, 0x24, 0x20, 0x41, 0x56, 0xB8,
                    0x30, 0x00, 0x00,
                ],
            ),
        ];
        const CHKSTK_RVA: i64 = 0xE6_B030;
        let mut page = [0u8; 4096];
        let page_addr = page.as_ptr() as usize;
        for (rva, bytes) in RVAS {
            let base = page_addr + 0x800;
            let added = base as i64 - rva as i64;
            assert_eq!(
                prologue_len(&bytes),
                Some(if rva == 0xcd_bb60 { 12 } else { 13 }),
                "{rva:#x}"
            );
            let built = build_trampoline(
                &mut page,
                base,
                &bytes,
                if rva == 0xcd_bb60 { 12 } else { 13 },
            )
            .expect("relocate");
            if rva == 0xcd_bb60 {
                assert!(built.relocs.is_empty(), "{rva:#x}: {}", built.note);
                assert_eq!(built.note, "verbatim");
            } else {
                assert_eq!(built.relocs.len(), 1, "{rva:#x}: {}", built.note);
                assert_eq!(built.relocs[0].kind, "call-rel32");
                assert_eq!(built.relocs[0].at, 5);
                assert_eq!(
                    built.relocs[0].target - added,
                    CHKSTK_RVA,
                    "{rva:#x} must call __chkstk"
                );
            }
            // ...and the copy always ends with the absolute jump back.
            assert_eq!(
                &page[built.len - ABS_JUMP..built.len - ABS_JUMP + 2],
                &[0xFF, 0x25]
            );
        }
    }

    #[test]
    fn describe_reports_the_relocations_of_a_installed_hook() {
        let hook = Hook {
            target: 0x6ffe_0000,
            tramp: 0x6ff0_0000,
            copied: 13,
            note: "call-rel32+5->0x6ffe3b020(+0x3b020)".to_string(),
            insns: 3,
            relocs: 1,
            mode: 1,
        };
        let line = hook.describe();
        assert!(line.contains("prologue=13B/3 insn"), "{line}");
        assert!(line.contains("reloc=1/3"), "{line}");
        assert!(line.contains("call-rel32+5->"), "{line}");
        assert!(line.contains("tramp=0x6ff00000"), "{line}");

        let reused = Hook {
            copied: 0,
            insns: 0,
            relocs: 0,
            mode: 2,
            note: String::new(),
            target: 0x1000,
            tramp: 0x2000,
        };
        assert!(
            reused.describe().contains("reused"),
            "{}",
            reused.describe()
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
        assert!(built.note.contains("call-rel32+5"), "{}", built.note);
        assert!(built.note.contains("rip-rel+10"), "{}", built.note);
        assert_eq!(built.insns, 4);
        assert_eq!(built.relocs.len(), 2);
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
