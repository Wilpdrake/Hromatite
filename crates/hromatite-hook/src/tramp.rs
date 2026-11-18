//! 2-pass x64 trampoline planner and relocator.
//!
//! Relocates a copied instruction window from `base` into a trampoline buffer:
//! * rip-relative `disp32` — retargeted to the same absolute address;
//! * `jcc rel8` — inverted condition hopping over a register-free
//!   `jmp qword [rip+0] <target>` (16 bytes);
//! * `loop`/`loope`/`loopne`/`jrcxz` (`E0..E3`) — `loop +2 ; jmp short over ;
//!   jmp [rip+0] <target>`;
//! * `jcc rel32`/`jmp rel32`/`call rel32` — original encoding while the new
//!   displacement fits, else `jmp [rip+0] <target>` or
//!   `call [rip+2] ; jmp +8 ; dq target` (register-free);
//! * intra-prologue branches follow the relocated instruction offset in the
//!   trampoline copy (and reject mid-instruction targets).

#![forbid(unsafe_code)]

use crate::decode::{Insn, decode};

/// Length of `jmp qword ptr [rip+0] ; dq addr` — the register-free absolute jump.
pub const ABS_JUMP: usize = 14;

/// Length of `call qword ptr [rip+2] ; jmp +8 ; dq addr` — the register-free
/// absolute call.
pub const ABS_CALL: usize = 16;

/// True when `to - from` fits in a signed 32-bit displacement.
pub fn rel32_ok(from: usize, to: usize) -> bool {
    let d = to as i64 - from as i64;
    d >= i32::MIN as i64 && d <= i32::MAX as i64
}

/// Adjust a 32-bit signed displacement after moving an instruction by `+delta`
/// (`delta = trampoline - old_address`).
pub fn adjusted_disp32(original: i32, delta: i64) -> Option<i32> {
    (original as i64 - delta).try_into().ok()
}

/// True when a relative branch at `target + off` (length `len`, displacement `disp`)
/// lands inside `[target, target + prologue)`.
pub fn branch_into_prologue(
    target: usize,
    prologue: usize,
    off: usize,
    len: usize,
    disp: i64,
) -> bool {
    let absolute = target as i64 + (off + len) as i64 + disp;
    absolute >= target as i64 && absolute < target as i64 + prologue as i64
}

/// Write `jmp qword ptr [rip+0] ; dq addr` into a 14-byte slot.
pub fn push_abs_jump_slice(slot: &mut [u8], addr: usize) -> Result<(), String> {
    let slot = slot
        .get_mut(..ABS_JUMP)
        .ok_or_else(|| "absolute jump does not fit in the trampoline page".to_string())?;
    slot[..6].copy_from_slice(&[0xFF, 0x25, 0, 0, 0, 0]);
    slot[6..].copy_from_slice(&(addr as u64).to_le_bytes());
    Ok(())
}

/// Write `call qword ptr [rip+2] ; jmp +8 ; dq addr` into a 16-byte slot.
pub fn push_abs_call_slice(slot: &mut [u8], addr: usize) -> Result<(), String> {
    let slot = slot
        .get_mut(..ABS_CALL)
        .ok_or_else(|| "absolute call does not fit in the trampoline page".to_string())?;
    slot[..6].copy_from_slice(&[0xFF, 0x15, 0x02, 0, 0, 0]);
    slot[6..8].copy_from_slice(&[0xEB, (ABS_CALL - 8) as u8]);
    slot[8..].copy_from_slice(&(addr as u64).to_le_bytes());
    Ok(())
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
    Verbatim,
    Rip32,
    Rel,
    CondAbs,
    LoopAbs,
    CondAbsLong,
    JmpRel32,
    JmpAbs,
    CallRel32,
    CallAbs,
}

impl Form {
    fn size(self, insn: &Insn) -> usize {
        match self {
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

#[derive(Clone, Copy, Debug)]
struct Step {
    old: usize,
    insn: Insn,
    new: usize,
    form: Form,
    target: i64,
}

fn rel_disp(insn: &Insn, code: &[u8], old: usize) -> Option<i64> {
    if insn.rel_size == 1 {
        Some(i64::from(*code.get(old + insn.rel_off)? as i8))
    } else {
        Some(i64::from(read_i32(code, old + insn.rel_off)?))
    }
}

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
    let inside = target >= base as i64 && target < (base + copied) as i64;
    if insn.cond {
        if insn.rel_size == 1 {
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

/// Copy and relocate the prologue into `tramp`, using `tramp.as_ptr() as usize`
/// as the trampoline's runtime address.
pub fn build_trampoline(
    tramp: &mut [u8],
    base: usize,
    code: &[u8],
    total: usize,
) -> Result<Built, String> {
    let tramp_addr = tramp.as_ptr() as usize;
    build_trampoline_at(tramp, tramp_addr, base, code, total)
}

/// Copy and relocate the prologue into `tramp`, encoding relative fields for a
/// trampoline mapped at `tramp_addr`.
pub fn build_trampoline_at(
    tramp: &mut [u8],
    tramp_addr: usize,
    base: usize,
    code: &[u8],
    total: usize,
) -> Result<Built, String> {
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
                        let keep = insn.rel_off;
                        out[..keep].copy_from_slice(&chunk[..keep]);
                        out[keep - 1] ^= 1;
                        out[keep] = ABS_JUMP as u8;
                        push_abs_jump_slice(&mut out[keep + 1..], target as usize)?;
                    }
                    Form::LoopAbs => {
                        let keep = insn.rel_off;
                        out[..keep].copy_from_slice(&chunk[..keep]);
                        out[keep] = 2;
                        out[keep + 1] = 0xEB;
                        out[keep + 2] = ABS_JUMP as u8;
                        push_abs_jump_slice(&mut out[keep + 3..], target as usize)?;
                    }
                    Form::CondAbsLong => {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relocation_preserves_the_absolute_target() {
        assert_eq!(adjusted_disp32(5, 0x1000), Some(-4091));
        assert_eq!(adjusted_disp32(i32::MIN, 0x1000), None);
        assert_eq!(adjusted_disp32(i32::MAX, -0x1000), None);
    }

    #[test]
    fn relative_branches_into_the_overwrite_window_are_detected() {
        assert!(branch_into_prologue(0x2000, 5, 0, 2, 1)); // 0x2000 -> 0x2003
        assert!(branch_into_prologue(0x2000, 5, 0, 2, -1)); // backward to 0x2001
        assert!(!branch_into_prologue(0x2000, 5, 0, 2, 0x100)); // external target
        assert!(!branch_into_prologue(0x2000, 5, 0, 2, 3)); // original continuation
    }
}
