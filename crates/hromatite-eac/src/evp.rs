//! BoringSSL EVP probes inside `EOSSDK-Win64-Shipping.dll`.
//!
//! The anti-cheat frame body is *not* encrypted (see
//! `~/EAC-RE/findings/frame_structure.md`), but the SDK's own protector does
//! keep an AES-256-GCM context and installs a **per-session key** after the
//! server's key-delivery message (`self + 0x1C` is the "session key present"
//! flag, `findings/eossdk.md` §6). The hard-coded pre-auth key is assembled
//! from `movl` immediates on the constructor's stack, so it can only be seen
//! by hooking the crypto path, not by scanning memory.
//!
//! Every target is addressed by the RE offset (BoringSSL is statically linked,
//! so nothing names these in the export table) and checked against the exact
//! prologue bytes taken from the DLL on disk. A drift disables the hook
//! instead of patching the middle of a function.
//!
//! The five `EVP_*` wrappers are the ones `EOS_AntiCheat_ProtectMessage`
//! calls (`findings/eossdk.md` §5.3, decompiled in
//! `work/eossdk/decomp7/180405c40_protect.c`):
//!
//! ```text
//! 0xcdb040  ctx_new()                    -> EVP_CIPHER_CTX_new
//! 0xcdbcb0  (ctx, cipher, impl, key, iv)  -> *Init_ex (protector re-inits with NULLs)
//! 0xcdbce0  (ctx, out, outl*, in, inl)    -> *Update
//! 0xcdbb60  (ctx, out, outl*)             -> *Final_ex
//! 0xcdaf90  (ctx, 0x10, 16, tag*)         -> CIPHER_CTX_ctrl(GET_TAG)
//! ```
//!
//! The `Update`/`Final` detours therefore report the *real* plaintext and
//! ciphertext lengths. AES-GCM is a stream mode, so a 512-byte plaintext stays
//! 512 bytes of ciphertext and `Final` adds nothing; the NetProtect envelope on
//! top is then `plaintext + 22` (4-byte sequence + 1-byte type + 1-byte flags +
//! 16-byte tag), which is what `GetProtectMessageOutputLength` reports. Those
//! two independent numbers are what make the `+22` claim checkable live.
//!
//! Enable with `EAC_EVP=1`. Each detour logs its own relocation record (see
//! `crate::hook::Hook::describe`), so a live "5/5 installed but the game died"
//! names the prologue, the copied instructions and every rewritten field.
// Every target of this module is a Win32 code patch; on the host the
// bookkeeping statics are simply unused.
#![cfg_attr(not(windows), allow(dead_code))]

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// NetProtect envelope overhead measured in RE: 4-byte sequence + 1-byte type +
/// 1-byte flags + 16-byte AEAD tag.
pub const ENVELOPE_OVERHEAD: u64 = 22;
/// AES-GCM tag length the protector requests (`ctrl(ctx, 0x10, 0x10, tag)`).
pub const GCM_TAG_BYTES: u64 = 16;
/// `EVP_CTRL_GCM_GET_TAG` (`EVP_CIPHER_CTX_ctrl` argument 2).
pub const CTRL_GET_TAG: u64 = 0x10;

/// What an `EVP_EncryptUpdate` of `plaintext` bytes must report as `outl`.
///
/// GCM is a stream mode: no padding, no length change. The tag is produced
/// separately (`Final` + `ctrl(GET_TAG)`), which is exactly why the envelope
/// grows by `GCM_TAG_BYTES` and not by a block.
pub fn ciphertext_len(plaintext: u64) -> u64 {
    plaintext
}

/// What `GetProtectMessageOutputLength`/`ProtectMessage` must report for
/// `plaintext` bytes of payload.
pub fn envelope_len(plaintext: u64) -> u64 {
    plaintext + ENVELOPE_OVERHEAD
}

/// One entry point worth hooking for key material.
pub struct Target {
    /// Name used in the log.
    pub name: &'static str,
    /// Offset from the image base (`0x180000000` in the RE listing).
    pub rva: usize,
    /// Exact prologue from the DLL on disk: a drift disables the hook.
    pub signature: &'static [u8],
    pub kind: Kind,
    /// Argument shape, for the log and for readers of this table.
    pub args: &'static str,
}

/// What the detour can say about the call.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// `EVP_CIPHER_CTX_new` — count only, there is no key yet.
    CtxNew,
    /// `EVP_*Init_ex`-shaped: `r9` is the key pointer, the fifth argument
    /// (`[rsp+0x28]`) the IV pointer.
    Init,
    /// `EVP_*Update(ctx, out, outl, in, inl)` — the plaintext length arrives in
    /// the fifth argument and the produced ciphertext length is written to
    /// `*outl`, so both real lengths are observable.
    Update,
    /// `EVP_*Final_ex(ctx, out, outl)` — GCM writes no extra bytes here.
    Final,
    /// `EVP_CIPHER_CTX_ctrl(ctx, type, len, ptr)`; `type == 0x10` hands out the
    /// 16-byte AEAD tag.
    Ctrl,
    /// Protector constructor: key lives on its own stack, so only arguments
    /// are logged (the useful capture is the `Init` call it makes).
    Ctor,
}

/// Verified on the current build (`EOSSDK-Win64-Shipping.dll`,
/// `VRChat_Data/Plugins/x86_64/`). `0x405af0` comes from
/// `findings/eossdk.md` §6.2, the five `0xcdbxxx`/`0xcdaxxx` addresses from
/// §6.1 and their call sites from `work/eossdk/decomp7/180405c40_protect.c`.
pub const TARGETS: [Target; 6] = [
    Target {
        name: "protector_ctor@405af0",
        rva: 0x40_5af0,
        signature: &[
            0x48, 0x89, 0x5c, 0x24, 0x10, 0x48, 0x89, 0x7c, 0x24, 0x18, 0x55, 0x48, 0x8b, 0xec,
        ],
        kind: Kind::Ctor,
        args: "(self)",
    },
    Target {
        name: "evp@cdb040(ctx_new)",
        rva: 0xcd_b040,
        signature: &[
            0xb8, 0x28, 0x00, 0x00, 0x00, 0xe8, 0xe6, 0xff, 0x18, 0x00, 0x48, 0x2b, 0xe0,
        ],
        kind: Kind::CtxNew,
        args: "()",
    },
    Target {
        name: "evp@cdbcb0(init)",
        rva: 0xcd_bcb0,
        signature: &[
            0xb8, 0x38, 0x00, 0x00, 0x00, 0xe8, 0x76, 0xf3, 0x18, 0x00, 0x48, 0x2b, 0xe0,
        ],
        kind: Kind::Init,
        args: "(ctx, cipher, impl, key, iv)",
    },
    Target {
        name: "evp@cdbce0(update)",
        rva: 0xcd_bce0,
        signature: &[
            0xb8, 0x38, 0x00, 0x00, 0x00, 0xe8, 0x46, 0xf3, 0x18, 0x00, 0x48, 0x2b, 0xe0, 0x83,
            0x79, 0x10, 0x00,
        ],
        kind: Kind::Update,
        args: "(ctx, out, outl, in, inl)",
    },
    Target {
        name: "evp@cdbb60(final)",
        rva: 0xcd_bb60,
        signature: &[
            0x48, 0x89, 0x5c, 0x24, 0x18, 0x48, 0x89, 0x6c, 0x24, 0x20, 0x41, 0x56, 0xb8,
        ],
        kind: Kind::Final,
        args: "(ctx, out, outl)",
    },
    Target {
        name: "evp@cdaf90(ctrl)",
        rva: 0xcd_af90,
        signature: &[
            0xb8, 0x38, 0x00, 0x00, 0x00, 0xe8, 0x96, 0x00, 0x19, 0x00, 0x48, 0x2b, 0xe0,
        ],
        kind: Kind::Ctrl,
        args: "(ctx, type, len, ptr)",
    },
];

const SLOTS: usize = TARGETS.len();

static TRAMPS: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
static CALLS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
/// Last plaintext length the `Update` wrapper reported.
static LAST_IN: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
/// Last ciphertext length read back from `*outl` after the call.
static LAST_OUT: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
/// Last tag length seen on `ctrl(ctx, GET_TAG, len, …)`.
static LAST_TAG_BYTES: AtomicU64 = AtomicU64::new(0);
/// Envelope length implied by the last `Update` (`plaintext + 22`).
static ENVELOPE: AtomicU64 = AtomicU64::new(0);
/// How many `Update` calls produced `outl == inl` with a 16-byte tag — the
/// two facts the `+22` envelope is made of.
static ENVELOPE_CONSISTENT: AtomicU64 = AtomicU64::new(0);
/// How many `Update` calls returned a ciphertext length different from `inl`.
static UPDATE_LENGTH_MISMATCH: AtomicU64 = AtomicU64::new(0);
/// Set once the probes are installed, so `install()` stays idempotent.
static INSTALLED: AtomicUsize = AtomicUsize::new(0);

/// How many calls per slot are logged verbatim before the 1-in-100 sampling.
const VERBOSE_CALLS: u64 = 64;

/// Index of a target by name (used by `eac.hooks` status and by tests).
pub fn slot_of(name: &str) -> Option<usize> {
    TARGETS.iter().position(|target| target.name == name)
}

pub fn calls() -> [u64; SLOTS] {
    let mut out = [0u64; SLOTS];
    for (slot, value) in out.iter_mut().enumerate() {
        *value = CALLS[slot].load(Ordering::Relaxed);
    }
    out
}

/// `(last plaintext, last ciphertext-or-outl)` per slot, as seen by the detours.
pub fn lengths() -> [(u64, u64); SLOTS] {
    let mut out = [(0u64, 0u64); SLOTS];
    for (slot, value) in out.iter_mut().enumerate() {
        *value = (
            LAST_IN[slot].load(Ordering::Relaxed),
            LAST_OUT[slot].load(Ordering::Relaxed),
        );
    }
    out
}

/// What the EVP layer measured, in one block: per-target hit counts and the
/// length bookkeeping that corroborates the `+22` envelope.
pub fn status() -> Vec<String> {
    let mut out = vec![format!(
        "evp: installed={}/{}, envelope(plaintext)={} last, tag={}B, consistent={}, update-mismatch={}",
        INSTALLED.load(Ordering::Acquire),
        SLOTS,
        ENVELOPE.load(Ordering::Relaxed),
        LAST_TAG_BYTES.load(Ordering::Relaxed),
        ENVELOPE_CONSISTENT.load(Ordering::Relaxed),
        UPDATE_LENGTH_MISMATCH.load(Ordering::Relaxed),
    )];
    let lengths = lengths();
    for (slot, target) in TARGETS.iter().enumerate() {
        let (plain, out_len) = lengths[slot];
        out.push(format!(
            "  {} [{}]: calls={} tramp={:#x} in={plain} out={out_len}",
            target.name,
            target.args,
            CALLS[slot].load(Ordering::Relaxed),
            TRAMPS[slot].load(Ordering::Acquire),
        ));
    }
    out
}

/// One install attempt, reported back to `eac.hooks` and the log.
pub struct Report {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
    pub tramp: usize,
}

impl Report {
    pub fn line(&self) -> String {
        format!(
            " {}: ok={} tramp={:#x} {}",
            self.name, self.ok, self.tramp, self.detail
        )
    }
}

#[cfg(windows)]
mod imp {
    use super::{
        CALLS, CTRL_GET_TAG, ENVELOPE, ENVELOPE_CONSISTENT, GCM_TAG_BYTES, INSTALLED, Kind,
        LAST_IN, LAST_OUT, LAST_TAG_BYTES, Report, SLOTS, TARGETS, TRAMPS, Target,
        UPDATE_LENGTH_MISMATCH, VERBOSE_CALLS, ciphertext_len, envelope_len,
    };
    use crate::capture::hex_prefix;
    use crate::eos::hook_log;
    use crate::hook;
    use core::sync::atomic::Ordering;

    /// Six integer argument registers, as the interrupted call had them.
    type Fn6 = unsafe extern "system" fn(u64, u64, u64, u64, u64, u64) -> u64;

    /// True when `[addr, addr+len)` is mapped with read access.
    ///
    /// # Safety
    ///
    /// Pure query: `VirtualQuery` is called on the current process and the
    /// result is only used to decide whether reading is safe.
    unsafe fn readable(addr: usize, len: usize) -> bool {
        if addr < 0x1_0000 || addr.checked_add(len).is_none() {
            return false;
        }
        // SAFETY: `MemBasic` is a plain POD structure of raw integers and
        // pointers; an all-zero value is a valid initialiser for it.
        let mut info: hook::win::MemBasic = unsafe { core::mem::zeroed() };
        // SAFETY: `info` is a valid writable structure of the declared size.
        let got = unsafe {
            hook::win::VirtualQuery(
                addr as *const _,
                &mut info,
                core::mem::size_of::<hook::win::MemBasic>(),
            )
        };
        if got == 0 {
            return false;
        }
        // PROTECT bits: 0x04 = PAGE_READWRITE, 0x02 = PAGE_READONLY,
        // 0x20 = EXECUTE_READ, 0x40 = EXECUTE_READWRITE, 0x80 = WRITECOPY.
        let protect = info.protect & 0xff;
        matches!(protect, 0x02 | 0x04 | 0x08 | 0x20 | 0x40 | 0x80)
            && (info.state == 0x1000 || info.state == 0x2000)
    }

    /// Read `len` bytes at `addr` when they are mapped.
    ///
    /// # Safety
    ///
    /// The range must be readable in this process; guarded by [`readable`].
    unsafe fn slice(addr: u64, len: usize) -> Option<&'static [u8]> {
        // SAFETY: pure query; `readable` only asks the loader about the range.
        if addr == 0 || !unsafe { readable(addr as usize, len) } {
            return None;
        }
        // SAFETY: `readable` just proved the range is mapped for `len` bytes.
        Some(unsafe { core::slice::from_raw_parts(addr as *const u8, len) })
    }

    /// Read the 32-bit length an EVP call wrote through its `int *outl`.
    ///
    /// # Safety
    ///
    /// `addr` is an output pointer supplied by the intercepted call; it is only
    /// dereferenced after `readable` proved the four bytes are mapped.
    unsafe fn read_u32(addr: u64) -> Option<u32> {
        // SAFETY: pure query; `readable` only asks the loader about the range.
        if addr < 0x1_0000 || !unsafe { readable(addr as usize, 4) } {
            return None;
        }
        // SAFETY: the range was just proved readable; the call wrote an `int`
        // there (Win32 `int *outl`), which needs no stronger alignment.
        Some(unsafe { core::ptr::read_unaligned(addr as *const u32) })
    }

    fn hex_or_unknown(value: Option<u64>) -> String {
        match value {
            Some(v) if v != u64::MAX => v.to_string(),
            _ => "?".to_string(),
        }
    }

    fn log_call(slot: usize, calls: u64, args: &[u64; 6]) {
        let target = &TARGETS[slot];
        let [a1, a2, a3, a4, a5, a6] = *args;
        let mut line = format!(
            "[evp] {} #{} args={a1:#x},{a2:#x},{a3:#x},{a4:#x},{a5:#x},{a6:#x}",
            target.name, calls
        );
        match target.kind {
            Kind::CtxNew => {}
            Kind::Ctor => {
                // The constructor builds the pre-auth key on its own stack; log
                // the first qwords of the frame so a later analysis can find it.
                // SAFETY: guarded by `slice`.
                if let Some(bytes) = unsafe { slice(a1, 32) } {
                    line.push_str(&format!(" self32={}", hex_prefix(bytes, 32)));
                }
            }
            Kind::Init => {
                line.push_str(&format!(" cipher={a2:#x} key={a4:#x} iv={a5:#x}"));
                // SAFETY: guarded by `slice`.
                if let Some(key) = unsafe { slice(a4, 32) } {
                    line.push_str(&format!(" key32={}", hex_prefix(key, 32)));
                }
                // SAFETY: guarded by `slice`.
                if let Some(iv) = unsafe { slice(a5, 16) } {
                    line.push_str(&format!(" iv16={}", hex_prefix(iv, 16)));
                }
                // A cipher context is 0x100-ish bytes; its first qword points
                // at the EVP_CIPHER, which is enough to identify the algorithm.
                // SAFETY: guarded by `slice`.
                if let Some(ctx) = unsafe { slice(a1, 16) } {
                    line.push_str(&format!(" ctx16={}", hex_prefix(ctx, 16)));
                }
            }
            Kind::Update => {
                // `inl` is the fifth argument (an integer, not a pointer) and
                // `outl` is the `int *` in the third.
                line.push_str(&format!(" in={a5} outl={a3:#x}"));
                // SAFETY: guarded by `slice`.
                if let Some(plain) = unsafe { slice(a4, 32) } {
                    line.push_str(&format!(" in32={}", hex_prefix(plain, 32)));
                }
            }
            Kind::Final => {}
            Kind::Ctrl => {
                line.push_str(&format!(" ctrl={a2:#x} len={a3}"));
                if a2 == CTRL_GET_TAG {
                    let want = (a3 as usize).min(GCM_TAG_BYTES as usize);
                    // SAFETY: guarded by `slice`.
                    if let Some(tag) = unsafe { slice(a4, want) } {
                        line.push_str(&format!(" tag{want}={}", hex_prefix(tag, want)));
                    }
                }
            }
        }
        hook_log(line);
    }

    /// Length bookkeeping *after* the original body ran: the real plaintext and
    /// ciphertext sizes, and the envelope they imply.
    fn note_lengths(slot: usize, calls: u64, args: &[u64; 6], ret: u64) {
        let target = &TARGETS[slot];
        let [_, _, a3, a4, a5, _] = *args;
        match target.kind {
            Kind::Update => {
                let plaintext = a5;
                // SAFETY: `a3` is the `int *outl` the call just filled in.
                let produced = unsafe { read_u32(a3) }.map(u64::from);
                LAST_IN[slot].store(plaintext, Ordering::Relaxed);
                LAST_OUT[slot].store(produced.unwrap_or(u64::MAX), Ordering::Relaxed);
                if produced.is_some_and(|len| len != ciphertext_len(plaintext)) {
                    UPDATE_LENGTH_MISMATCH.fetch_add(1, Ordering::Relaxed);
                }
                let envelope = envelope_len(plaintext);
                ENVELOPE.store(envelope, Ordering::Relaxed);
                let tag = LAST_TAG_BYTES.load(Ordering::Relaxed);
                if produced == Some(ciphertext_len(plaintext)) && tag == GCM_TAG_BYTES {
                    ENVELOPE_CONSISTENT.fetch_add(1, Ordering::Relaxed);
                }
                hook_log(format!(
                    "[evp] {} #{} in={plaintext} out={} ret={ret:#x} -> envelope {envelope} (in+{}: {tag}B tag + 6B header), ctrl tag={}",
                    target.name,
                    calls,
                    hex_or_unknown(produced),
                    envelope - plaintext,
                    if tag == 0 {
                        "?".to_string()
                    } else {
                        tag.to_string()
                    },
                ));
            }
            Kind::Final => {
                // SAFETY: `a3` is the `int *outl` the call just filled in.
                let produced = unsafe { read_u32(a3) }.map(u64::from);
                if let Some(value) = produced {
                    LAST_OUT[slot].store(value, Ordering::Relaxed);
                }
                hook_log(format!(
                    "[evp] {} #{} final_out={} ret={ret:#x} (GCM: no extra plaintext bytes)",
                    target.name,
                    calls,
                    hex_or_unknown(produced),
                ));
            }
            Kind::Ctrl => {
                // `ctrl(ctx, GET_TAG, 16, tag)` proves the tag width the
                // envelope's `+16` term relies on.
                if a4 != 0 && a3 != 0 {
                    LAST_TAG_BYTES.store(a3, Ordering::Relaxed);
                }
            }
            Kind::CtxNew | Kind::Init | Kind::Ctor => {}
        }
    }

    macro_rules! detour {
        ($name:ident, $slot:expr) => {
            /// Generic six-slot detour: log, tail-call the trampoline, then
            /// record the lengths the call produced.
            ///
            /// # Safety
            ///
            /// Called by the patched entry with the original argument
            /// registers; `a1..a6` are those registers, passed through
            /// unchanged.
            unsafe extern "system" fn $name(
                a1: u64,
                a2: u64,
                a3: u64,
                a4: u64,
                a5: u64,
                a6: u64,
            ) -> u64 {
                let calls = CALLS[$slot].fetch_add(1, Ordering::Relaxed) + 1;
                let args = [a1, a2, a3, a4, a5, a6];
                let verbose = calls <= VERBOSE_CALLS || calls % 100 == 0;
                if verbose {
                    let _ = std::panic::catch_unwind(|| log_call($slot, calls, &args));
                }
                let tramp = TRAMPS[$slot].load(Ordering::Acquire);
                if tramp == 0 {
                    return 0;
                }
                // SAFETY: the trampoline is the relocated prologue of this very
                // function, ending in an absolute jump back into it.
                let function: Fn6 = unsafe { core::mem::transmute(tramp) };
                // SAFETY: same ABI and argument registers as the interrupted call.
                let ret = unsafe { function(a1, a2, a3, a4, a5, a6) };
                if verbose {
                    let _ = std::panic::catch_unwind(|| note_lengths($slot, calls, &args, ret));
                }
                ret
            }
        };
    }

    detour!(slot0, 0);
    detour!(slot1, 1);
    detour!(slot2, 2);
    detour!(slot3, 3);
    detour!(slot4, 4);
    detour!(slot5, 5);

    /// Function-pointer casts are not const-evaluable, so the table is built at
    /// install time (once per process).
    fn handlers() -> [usize; SLOTS] {
        [
            slot0 as *const () as usize,
            slot1 as *const () as usize,
            slot2 as *const () as usize,
            slot3 as *const () as usize,
            slot4 as *const () as usize,
            slot5 as *const () as usize,
        ]
    }

    /// Install every probe whose prologue still matches. Idempotent, and one
    /// failing target never stops the others.
    ///
    /// # Safety
    ///
    /// `module` must be the base of `EOSSDK-Win64-Shipping.dll` in this
    /// process and must stay mapped for the process lifetime.
    pub unsafe fn install(module: isize) -> Vec<Report> {
        let mut reports = Vec::with_capacity(SLOTS);
        if INSTALLED.load(Ordering::Acquire) != 0 {
            for (index, target) in TARGETS.iter().enumerate() {
                let tramp = TRAMPS[index].load(Ordering::Acquire);
                reports.push(Report {
                    name: target.name,
                    ok: tramp != 0,
                    detail: if tramp != 0 {
                        "already installed".to_string()
                    } else {
                        "skipped by an earlier install".to_string()
                    },
                    tramp,
                });
            }
            return reports;
        }
        let handlers = handlers();
        for (index, target) in TARGETS.iter().enumerate() {
            let address = module as usize + target.rva;
            // SAFETY: the range is only compared after `readable` proves it is
            // mapped, and a readable range cannot fault.
            let seen = if unsafe { readable(address, target.signature.len()) } {
                // SAFETY: `readable` just proved the range is mapped.
                unsafe { core::slice::from_raw_parts(address as *const u8, target.signature.len()) }
            } else {
                &[]
            };
            if seen != target.signature {
                let detail = if seen.is_empty() {
                    format!("unmapped at {address:#x}")
                } else {
                    format!(
                        "prologue mismatch at {address:#x}: want {} got {}",
                        hex_prefix(target.signature, target.signature.len()),
                        hex_prefix(seen, seen.len())
                    )
                };
                hook_log(format!(
                    "[evp] {} refused: {detail} ({} bytes, {})",
                    target.name,
                    target.signature.len(),
                    target.args
                ));
                reports.push(Report {
                    name: target.name,
                    ok: false,
                    detail,
                    tramp: 0,
                });
                continue;
            }
            // SAFETY: `address` is a function entry whose prologue matches the
            // recorded bytes, and the handler has the same six-integer ABI.
            // A panic inside the installer must not take the SDK thread down,
            // so every target is installed behind `catch_unwind`.
            let outcome = std::panic::catch_unwind(|| unsafe {
                hook::install(address, handlers[index], target.name)
            });
            match outcome {
                Ok(Ok(installed)) => {
                    TRAMPS[index].store(installed.tramp, Ordering::Release);
                    hook_log(format!(
                        "[evp] {} installed: {}",
                        target.name,
                        installed.describe()
                    ));
                    reports.push(Report {
                        name: target.name,
                        ok: true,
                        detail: format!(
                            "prologue={}B/{} insn reloc[{}]",
                            installed.copied, installed.insns, installed.note
                        ),
                        tramp: installed.tramp,
                    });
                }
                Ok(Err(reason)) => {
                    hook_log(format!("[evp] {} failed: {reason}", target.name));
                    reports.push(Report {
                        name: target.name,
                        ok: false,
                        detail: reason,
                        tramp: 0,
                    });
                }
                Err(_) => {
                    hook_log(format!("[evp] {} failed: installer panicked", target.name));
                    reports.push(Report {
                        name: target.name,
                        ok: false,
                        detail: "installer panicked".to_string(),
                        tramp: 0,
                    });
                }
            }
        }
        let ok = reports.iter().filter(|report| report.ok).count();
        // At least one probe is enough to make this pass idempotent: the hook
        // engine reuses its trampolines, and a retry of a *failed* target would
        // re-patch an entry that already carries our jump.
        if ok != 0 {
            INSTALLED.store(ok, Ordering::Release);
        }
        reports
    }

    /// Log which target matched which signature, for the host-side debug of a
    /// freshly re-downloaded SDK.
    pub fn dump_targets(module: isize) -> Vec<String> {
        TARGETS
            .iter()
            .map(|target: &Target| {
                let address = module as usize + target.rva;
                // SAFETY: guarded by `readable` inside `slice`.
                let bytes = unsafe { slice(address as u64, target.signature.len()) };
                match bytes {
                    Some(bytes) => format!(
                        " {}: {}",
                        target.name,
                        hex_prefix(bytes, target.signature.len())
                    ),
                    None => format!(" {}: unmapped", target.name),
                }
            })
            .collect()
    }
}

#[cfg(windows)]
pub use imp::{dump_targets, install};

#[cfg(not(windows))]
pub fn install(_module: isize) -> Vec<Report> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_match_their_targets() {
        assert_eq!(TARGETS.len(), SLOTS);
        for target in TARGETS.iter() {
            assert!(!target.signature.is_empty(), "{}", target.name);
            assert!(target.rva > 0x1000, "{}", target.name);
            // The patched entry is 12 bytes, so the recorded prologue must at
            // least cover them — otherwise the signature check proves nothing.
            assert!(
                target.signature.len() >= 12,
                "{}: {}-byte signature",
                target.name,
                target.signature.len()
            );
        }
        assert_eq!(slot_of("protector_ctor@405af0"), Some(0));
        assert_eq!(slot_of("evp@cdbcb0(init)"), Some(2));
        assert_eq!(slot_of("evp@cdbce0(update)"), Some(3));
        assert_eq!(slot_of("nope"), None);
    }

    #[test]
    fn all_five_evp_wrappers_are_present_with_their_argument_shapes() {
        // The five addresses `EOS_AntiCheat_ProtectMessage` calls (protect.c):
        // ctx_new, init, update, final and the GET_TAG ctrl.
        let kinds: Vec<(usize, Kind, &str)> = TARGETS
            .iter()
            .map(|target| (target.rva, target.kind, target.args))
            .collect();
        assert_eq!(
            kinds,
            vec![
                (0x40_5af0, Kind::Ctor, "(self)"),
                (0xcd_b040, Kind::CtxNew, "()"),
                (0xcd_bcb0, Kind::Init, "(ctx, cipher, impl, key, iv)"),
                (0xcd_bce0, Kind::Update, "(ctx, out, outl, in, inl)"),
                (0xcd_bb60, Kind::Final, "(ctx, out, outl)"),
                (0xcd_af90, Kind::Ctrl, "(ctx, type, len, ptr)"),
            ]
        );
    }

    #[test]
    fn the_envelope_overhead_is_the_tag_plus_the_six_byte_header() {
        // GCM keeps the ciphertext as long as the plaintext...
        for plaintext in [1u64, 512, 0x1_0000] {
            assert_eq!(ciphertext_len(plaintext), plaintext);
            // ...the tag is 16 bytes and the NetProtect header 6, which is
            // exactly the `+0x16` that `GetProtectMessageOutputLength` returns.
            assert_eq!(
                envelope_len(plaintext),
                ciphertext_len(plaintext) + GCM_TAG_BYTES + 6
            );
            assert_eq!(envelope_len(plaintext) - plaintext, 22);
        }
        assert_eq!(CTRL_GET_TAG, 0x10, "EVP_CTRL_GCM_GET_TAG");
    }

    #[test]
    fn counters_start_at_zero() {
        assert_eq!(calls(), [0; SLOTS]);
        assert_eq!(lengths(), [(0, 0); SLOTS]);
        assert_eq!(ENVELOPE.load(Ordering::Relaxed), 0);
    }
}
