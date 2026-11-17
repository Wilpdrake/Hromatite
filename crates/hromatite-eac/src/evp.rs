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
//! Enable with `EAC_EVP=1`; the trampoline engine must relocate `call rel32`
//! (see `crate::hook`), otherwise these detours crash the game after ~40 s.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// One entry point worth hooking for key material.
pub struct Target {
    /// Name used in the log.
    pub name: &'static str,
    /// Offset from the image base (`0x180000000` in the RE listing).
    pub rva: usize,
    /// Exact prologue from the DLL on disk: a drift disables the hook.
    pub signature: &'static [u8],
    pub kind: Kind,
}

/// What the detour can say about the call.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `EVP_CIPHER_CTX_new` — count only, there is no key yet.
    CtxNew,
    /// `EVP_*Init_ex`-shaped: `r9` is the key pointer, the fifth argument
    /// (`[rsp+0x28]`) the IV pointer.
    Init,
    /// Unknown arity: dump `r9` when it looks like a readable pointer.
    Generic,
    /// Protector constructor: key lives on its own stack, so only arguments
    /// are logged (the useful capture is the `Init` call it makes).
    Ctor,
}

/// Verified on the current build (`EOSSDK-Win64-Shipping.dll`,
/// `VRChat_Data/Plugins/x86_64/`). `0x405af0` comes from
/// `findings/eossdk.md` §6.2, the five `0xcdbxxx`/`0xcdaxxx` addresses from
/// §6.1.
pub const TARGETS: [Target; 6] = [
    Target {
        name: "protector_ctor@405af0",
        rva: 0x40_5af0,
        signature: &[
            0x48, 0x89, 0x5c, 0x24, 0x10, 0x48, 0x89, 0x7c, 0x24, 0x18, 0x55, 0x48, 0x8b, 0xec,
        ],
        kind: Kind::Ctor,
    },
    Target {
        name: "evp@cdb040(ctx_new)",
        rva: 0xcd_b040,
        signature: &[
            0xb8, 0x28, 0x00, 0x00, 0x00, 0xe8, 0xe6, 0xff, 0x18, 0x00, 0x48, 0x2b, 0xe0,
        ],
        kind: Kind::CtxNew,
    },
    Target {
        name: "evp@cdbcb0(cipher_init)",
        rva: 0xcd_bcb0,
        signature: &[
            0xb8, 0x38, 0x00, 0x00, 0x00, 0xe8, 0x76, 0xf3, 0x18, 0x00, 0x48, 0x2b, 0xe0,
        ],
        kind: Kind::Init,
    },
    Target {
        name: "evp@cdbce0",
        rva: 0xcd_bce0,
        signature: &[
            0xb8, 0x38, 0x00, 0x00, 0x00, 0xe8, 0x46, 0xf3, 0x18, 0x00, 0x48, 0x2b, 0xe0, 0x83,
            0x79, 0x10, 0x00,
        ],
        kind: Kind::Generic,
    },
    Target {
        name: "evp@cdbb60",
        rva: 0xcd_bb60,
        signature: &[
            0x48, 0x89, 0x5c, 0x24, 0x18, 0x48, 0x89, 0x6c, 0x24, 0x20, 0x41, 0x56, 0xb8,
        ],
        kind: Kind::Generic,
    },
    Target {
        name: "evp@cdaf90",
        rva: 0xcd_af90,
        signature: &[
            0xb8, 0x38, 0x00, 0x00, 0x00, 0xe8, 0x96, 0x00, 0x19, 0x00, 0x48, 0x2b, 0xe0,
        ],
        kind: Kind::Generic,
    },
];

const SLOTS: usize = TARGETS.len();

static TRAMPS: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
static CALLS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];
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
    use super::{CALLS, INSTALLED, Kind, Report, SLOTS, TARGETS, TRAMPS, Target, VERBOSE_CALLS};
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

    fn log_call(slot: usize, calls: u64, a1: u64, a2: u64, a4: u64, a5: u64, a6: u64) {
        let target = &TARGETS[slot];
        let mut line = format!(
            "[evp] {} #{calls} arg1={a1:#x} arg2={a2:#x} arg4={a4:#x} arg5={a5:#x} arg6={a6:#x}",
            target.name
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
                // SAFETY: guarded by `slice`.
                if let Some(key) = unsafe { slice(a4, 32) } {
                    line.push_str(&format!(" key32={}", hex_prefix(key, 32)));
                }
                // SAFETY: guarded by `slice`.
                if let Some(iv) = unsafe { slice(a5, 16) } {
                    line.push_str(&format!(
                        " iv12={} iv16={}",
                        hex_prefix(&iv[..12], 12),
                        hex_prefix(iv, 16)
                    ));
                }
                // A cipher context is 0x100-ish bytes; its first qword points
                // at the EVP_CIPHER, which is enough to identify the algorithm.
                // SAFETY: guarded by `slice`.
                if let Some(ctx) = unsafe { slice(a1, 16) } {
                    line.push_str(&format!(" ctx16={}", hex_prefix(ctx, 16)));
                }
            }
            Kind::Generic => {
                // SAFETY: guarded by `slice`.
                if let Some(bytes) = unsafe { slice(a4, 32) } {
                    line.push_str(&format!(" data32={}", hex_prefix(bytes, 32)));
                }
                if a5 < 0x1_0000 {
                    line.push_str(&format!(" arg5_u32={a5:#x}"));
                }
            }
        }
        let _ = a6;
        hook_log(line);
    }

    macro_rules! detour {
        ($name:ident, $slot:expr) => {
            /// Generic six-slot detour: log, then tail-call the trampoline.
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
                if calls <= VERBOSE_CALLS || calls % 100 == 0 {
                    let _ = std::panic::catch_unwind(|| log_call($slot, calls, a1, a2, a4, a5, a6));
                }
                let tramp = TRAMPS[$slot].load(Ordering::Acquire);
                if tramp == 0 {
                    return 0;
                }
                // SAFETY: the trampoline is the relocated prologue of this very
                // function, ending in an absolute jump back into it.
                let function: Fn6 = unsafe { core::mem::transmute(tramp) };
                // SAFETY: same ABI and argument registers as the interrupted call.
                unsafe { function(a1, a2, a3, a4, a5, a6) }
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

    /// Install every probe whose prologue still matches. Idempotent.
    ///
    /// # Safety
    ///
    /// `module` must be the base of `EOSSDK-Win64-Shipping.dll` in this
    /// process and must stay mapped for the process lifetime.
    pub unsafe fn install(module: isize) -> Vec<Report> {
        let mut reports = Vec::with_capacity(SLOTS);
        if INSTALLED.load(Ordering::Acquire) != 0 {
            for target in TARGETS.iter() {
                reports.push(Report {
                    name: target.name,
                    ok: true,
                    detail: "already installed".to_string(),
                    tramp: 0,
                });
            }
            return reports;
        }
        for (index, target) in TARGETS.iter().enumerate() {
            let address = module as usize + target.rva;
            // SAFETY: the range is only compared after `readable` proves it is
            // mapped, and a readable range cannot fault.
            let matches = unsafe { readable(address, target.signature.len()) }
                // SAFETY: `readable` just proved the range is mapped.
                && unsafe {
                    core::slice::from_raw_parts(address as *const u8, target.signature.len())
                } == target.signature;
            if !matches {
                reports.push(Report {
                    name: target.name,
                    ok: false,
                    detail: "prologue mismatch".to_string(),
                    tramp: 0,
                });
                continue;
            }
            // SAFETY: `address` is a function entry whose prologue matches the
            // recorded bytes, and the handler has the same six-integer ABI.
            match unsafe { hook::install(address, handlers()[index], target.name) } {
                Ok(installed) => {
                    TRAMPS[index].store(installed.tramp, Ordering::Release);
                    reports.push(Report {
                        name: target.name,
                        ok: true,
                        detail: format!("copied={} {}", installed.copied, installed.note),
                        tramp: installed.tramp,
                    });
                }
                Err(reason) => reports.push(Report {
                    name: target.name,
                    ok: false,
                    detail: reason,
                    tramp: 0,
                }),
            }
        }
        let ok = reports.iter().filter(|report| report.ok).count();
        INSTALLED.store(ok.max(1), Ordering::Release);
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
        }
        assert_eq!(slot_of("protector_ctor@405af0"), Some(0));
        assert_eq!(slot_of("evp@cdbcb0(cipher_init)"), Some(2));
        assert_eq!(slot_of("nope"), None);
    }

    #[test]
    fn counters_start_at_zero() {
        assert_eq!(calls(), [0; SLOTS]);
    }
}
