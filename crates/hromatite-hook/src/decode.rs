//! x64 instruction-length and relative-operand decoder (prologue subset).
//!
//! Pure Rust and target-independent so every opcode class and relocation
//! offset can be tested on the Linux host. Unknown opcodes fail closed (`None`)
//! instead of guessing an instruction boundary.

#![forbid(unsafe_code)]

/// One decoded instruction: its length and where a relative/rip-relative field
/// sits inside it, so the trampoline copy can be relocated.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Insn {
    /// Instruction length in bytes (1..=15).
    pub len: usize,
    /// Offset of the primary opcode byte (after legacy prefixes and REX).
    pub op_off: usize,
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
    /// The branch is conditional (`jcc`, `loop`, `jrcxz`).
    pub cond: bool,
    /// Compatibility field for `(offset, bits)` relative branch queries.
    pub rel_bits: Option<(usize, u8)>,
    /// Compatibility field for `RIP`-relative `disp32` offset queries.
    pub rip: Option<usize>,
}

impl Insn {
    /// `(offset_in_insn, bits)` of a relative branch displacement (`8` or `32`).
    pub fn rel_field(&self) -> Option<(usize, u8)> {
        self.rel_bits
    }

    /// Byte offset of a RIP-relative `disp32` inside the instruction.
    pub fn rip_field(&self) -> Option<usize> {
        self.rip
    }
}

/// One-byte opcodes that take a ModRM byte.
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

/// Two-byte (`0F xx`) opcodes that take a ModRM byte (and optional `imm8`).
fn two_byte_modrm(op2: u8) -> Option<usize> {
    match op2 {
        0x10..=0x17
        | 0x1F
        | 0x28..=0x2F
        | 0x40..=0x4F
        | 0x50..=0x5F
        | 0x60..=0x6F
        | 0x74..=0x76
        | 0x7C..=0x7F
        | 0x90..=0x9F
        | 0xA3
        | 0xAB
        | 0xAF
        | 0xB0..=0xB7
        | 0xBB..=0xBF
        | 0xC0
        | 0xC1
        | 0xC3
        | 0xC7
        | 0xD0..=0xEF
        | 0xF1..=0xFE => Some(0),
        0x70..=0x73 | 0xA4 | 0xAC | 0xBA | 0xC2 | 0xC4..=0xC6 => Some(1),
        _ => None,
    }
}

/// Decode one instruction. `None` = an opcode outside the trusted subset (the
/// caller must then treat the prologue as unhookable).
pub fn decode(code: &[u8]) -> Option<Insn> {
    let mut i = 0usize;
    let mut rex = 0u8;
    let mut saw_rex = false;
    let mut opsz16 = false;
    let mut guard = 0usize;

    loop {
        guard += 1;
        if guard > 9 {
            return None;
        }
        let byte = *code.get(i)?;
        match byte {
            0x66 => {
                if saw_rex {
                    return None;
                }
                opsz16 = true;
                i += 1;
            }
            0x67 | 0xF0 | 0xF2 | 0xF3 | 0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65 => {
                if saw_rex {
                    return None;
                }
                i += 1;
            }
            0x40..=0x4F => {
                rex = byte;
                saw_rex = true;
                i += 1;
            }
            _ => break,
        }
    }

    let rex_w = (rex & 0x08) != 0;
    let imm16 = opsz16 && !rex_w;
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
            // endbr64 / endbr32 (CET landing pad)
            0x1E => {
                let m3 = *code.get(i)?;
                if m3 == 0xFA || m3 == 0xFB {
                    i += 1;
                } else {
                    return None;
                }
            }
            0x38 | 0x3A => {
                let _op3 = *code.get(i)?;
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
                out.rel_bits = Some((i, 32));
                i += 4;
                if i > code.len() {
                    return None;
                }
                out.len = i;
                return (1..=15).contains(&i).then_some(out);
            }
            0x05 | 0x0B | 0x31 | 0x77 | 0xA2 | 0x34 | 0x35 => {}
            0x0D => {
                has_modrm = true;
            }
            _ => {
                imm = two_byte_modrm(op2)?;
                has_modrm = true;
            }
        }
    } else {
        match op {
            0x90 | 0x98 | 0x99 | 0xC3 | 0xC9 | 0xCC | 0xF4 | 0xF5 | 0xF8 | 0xF9 | 0xFC | 0xFD
            | 0xCE | 0x9C | 0x9D | 0x9E | 0x9F => {}
            0x50..=0x5F => {}
            0x68 => imm = if imm16 { 2 } else { 4 },
            0x6A => imm = 1,
            0x70..=0x7F => {
                out.rel = true;
                out.cond = true;
                out.rel_off = i;
                out.rel_size = 1;
                out.rel_bits = Some((i, 8));
                i += 1;
                if i > code.len() {
                    return None;
                }
                out.len = i;
                return (1..=15).contains(&i).then_some(out);
            }
            0xEB => {
                out.rel = true;
                out.rel_off = i;
                out.rel_size = 1;
                out.rel_bits = Some((i, 8));
                i += 1;
                if i > code.len() {
                    return None;
                }
                out.len = i;
                return (1..=15).contains(&i).then_some(out);
            }
            0xE0..=0xE3 => {
                out.rel = true;
                out.cond = true;
                out.rel_off = i;
                out.rel_size = 1;
                out.rel_bits = Some((i, 8));
                i += 1;
                if i > code.len() {
                    return None;
                }
                out.len = i;
                return (1..=15).contains(&i).then_some(out);
            }
            0xE8 | 0xE9 => {
                out.rel = true;
                out.rel_off = i;
                out.rel_size = 4;
                out.rel_bits = Some((i, 32));
                i += 4;
                if i > code.len() {
                    return None;
                }
                out.len = i;
                return (1..=15).contains(&i).then_some(out);
            }
            0xC2 => imm = 2,
            0xB0..=0xB7 => imm = 1,
            0xB8..=0xBF => {
                imm = if rex_w {
                    8
                } else if opsz16 {
                    2
                } else {
                    4
                };
            }
            0xA8 => imm = 1,
            0xA9 => imm = if imm16 { 2 } else { 4 },
            0x04 | 0x0C | 0x14 | 0x1C | 0x24 | 0x2C | 0x34 | 0x3C => imm = 1,
            0x05 | 0x0D | 0x15 | 0x1D | 0x25 | 0x2D | 0x35 | 0x3D => {
                imm = if imm16 { 2 } else { 4 };
            }
            _ => {
                if !one_byte_has_modrm(op) {
                    return None;
                }
                has_modrm = true;
                if matches!(op, 0x80 | 0x83 | 0x6B | 0xC0 | 0xC1 | 0xC6 | 0xF6) {
                    imm = 1;
                } else if matches!(op, 0x81 | 0x69 | 0xC7 | 0xF7) {
                    imm = if imm16 { 2 } else { 4 };
                }
            }
        }
    }

    if has_modrm {
        let modrm = *code.get(i)?;
        i += 1;
        // group 3: only `/0 test` and `/1 test` take an immediate.
        if matches!(op, 0xF6 | 0xF7) {
            let reg = (modrm >> 3) & 7;
            imm = if reg <= 1 {
                if op == 0xF6 {
                    1
                } else if imm16 {
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
                    out.rip = Some(i);
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

    if i > code.len() {
        return None;
    }
    out.len = i;
    (1..=15).contains(&i).then_some(out)
}

/// Alias for [`decode`].
pub fn decode_one(b: &[u8]) -> Option<Insn> {
    decode(b)
}

/// Decode instructions until at least `min` bytes are covered; return the
/// covered length and the decoded instructions.
pub fn decode_span(bytes: &[u8], min: usize) -> Option<(usize, Vec<Insn>)> {
    let mut off = 0usize;
    let mut out = Vec::with_capacity(8);
    while off < min {
        if out.len() > 32 || off >= bytes.len() {
            return None;
        }
        let ins = decode(&bytes[off..])?;
        if ins.len == 0 {
            return None;
        }
        off += ins.len;
        out.push(ins);
    }
    Some((off, out))
}

/// Decode until at least `min` bytes are covered with explicit instruction-count
/// and byte-length bounds, naming the first offending opcode on failure.
pub fn prologue_span_bounded(
    code: &[u8],
    min: usize,
    max_insns: usize,
    max_bytes: usize,
) -> Result<(usize, usize), String> {
    let mut n = 0usize;
    let mut insns = 0usize;
    while n < min {
        if insns > max_insns {
            return Err(format!("more than {max_insns} instructions within {min} bytes"));
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
    if n > max_bytes {
        return Err(format!("prologue {n} bytes exceeds the {max_bytes}-byte bound"));
    }
    Ok((n, insns))
}

/// Length of a prologue that is long enough to hold a `min`-byte patch.
/// `None` = an opcode we do not decode was hit, so the caller must not patch.
pub fn prologue_len(bytes: &[u8], min: usize) -> Option<usize> {
    decode_span(bytes, min).map(|(n, _)| n)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn len(b: &[u8]) -> Option<usize> {
        decode_one(b).map(|i| i.len)
    }

    #[test]
    fn decodes_common_prologues() {
        assert_eq!(len(&[0x48, 0x83, 0xEC, 0x28]), Some(4)); // sub rsp, 0x28
        assert_eq!(len(&[0x48, 0x81, 0xEC, 0x28, 0x01, 0, 0]), Some(7)); // sub rsp, 0x128
        assert_eq!(len(&[0xF3, 0x0F, 0x1E, 0xFA]), Some(4)); // endbr64
        assert_eq!(len(&[0x48, 0x89, 0x5C, 0x24, 0x08]), Some(5)); // mov [rsp+8], rbx
        assert_eq!(len(&[0x40, 0x55]), Some(2)); // push rbp
        assert_eq!(len(&[0x48, 0x8B, 0xC4]), Some(3)); // mov rax, rsp
        assert_eq!(len(&[0x48, 0x8B, 0x05, 0, 0, 0, 0]), Some(7)); // mov rax, [rip+0]
        assert_eq!(len(&[0x48, 0x8D, 0x0D, 0, 0, 0, 0]), Some(7)); // lea rcx, [rip+0]
        assert_eq!(len(&[0xE9, 0, 0, 0, 0]), Some(5)); // jmp rel32
        assert_eq!(len(&[0xE8, 0, 0, 0, 0]), Some(5)); // call rel32
        assert_eq!(len(&[0xFF, 0x25, 0, 0, 0, 0]), Some(6)); // jmp [rip+0]
        assert_eq!(len(&[0x0F, 0x1F, 0x44, 0x00, 0x00]), Some(5)); // nop dword [rax+rax]
        assert_eq!(len(&[0x48, 0xB8, 0, 0, 0, 0, 0, 0, 0, 0]), Some(10)); // mov rax, imm64
        assert_eq!(len(&[0xC3]), Some(1));
        assert_eq!(len(&[0x6A, 0x00]), Some(2)); // push 0
        assert_eq!(len(&[0xC5, 0xF8, 0x77]), None); // VEX: unknown on purpose
    }

    #[test]
    fn prologue_covers_five_bytes() {
        let b = [0x40, 0x55, 0x48, 0x83, 0xEC, 0x28];
        assert_eq!(prologue_len(&b, 5), Some(6));
        let ins = decode_one(&[0x48, 0x8B, 0x05, 1, 0, 0, 0]).unwrap();
        assert_eq!(ins.rip, Some(3));
        assert!(ins.rip_rel);
        assert_eq!(ins.disp_off, 3);
    }

    #[test]
    fn immediate_modrm_forms_have_exact_lengths_and_rip_offsets() {
        let shuffle = [0x0F, 0xC6, 0x05, 4, 0, 0, 0, 0x7f];
        let shuffle_ins = decode_one(&shuffle).unwrap();
        assert_eq!(shuffle_ins.len, shuffle.len());
        assert_eq!(shuffle_ins.rip, Some(3));

        let store = [0xC7, 0x05, 4, 0, 0, 0, 0x78, 0x56, 0x34, 0x12];
        let store_ins = decode_one(&store).unwrap();
        assert_eq!(store_ins.len, store.len());
        assert_eq!(store_ins.rip, Some(2));
    }

    #[test]
    fn operand_size_prefix_selects_imm16() {
        let mov_ax = [0x66, 0xB8, 0x34, 0x12];
        assert_eq!(decode_one(&mov_ax).unwrap().len, mov_ax.len());
        assert!(decode_one(&[0x48, 0x66, 0xB8, 0x34, 0x12]).is_none());
        let push_rex = [0x66, 0x48, 0x68, 0x78, 0x56, 0x34, 0x12];
        assert_eq!(decode_one(&push_rex).unwrap().len, push_rex.len());
        let test_rex = [0x66, 0x48, 0xA9, 0x78, 0x56, 0x34, 0x12];
        assert_eq!(decode_one(&test_rex).unwrap().len, test_rex.len());

        let push = [0x66, 0x68, 0x34, 0x12];
        assert_eq!(decode_one(&push).unwrap().len, push.len());

        let sub = [0x66, 0x81, 0xEC, 0x34, 0x12];
        assert_eq!(decode_one(&sub).unwrap().len, sub.len());

        let store = [0x66, 0xC7, 0x05, 4, 0, 0, 0, 0x78, 0x56];
        let ins = decode_one(&store).unwrap();
        assert_eq!(ins.len, store.len());
        assert_eq!(ins.rip, Some(3));
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
}
