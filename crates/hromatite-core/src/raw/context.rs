//! Guarded calls: VEH-based "setjmp/longjmp" for calling functions we have not
//! proven safe, and for containing Rust panics inside the payload.
//!
//! `catch_unwind` cannot catch an access violation; only a vectored exception
//! handler can turn a fault inside a game function into an `Err`. Worse, a panic
//! in the payload cannot be unwound at all: the image is manually mapped with
//! its PE headers erased, so the OS unwinder cannot find `.pdata` and dies
//! mid-unwind (measured live: the `PANIC` line appears, then the game is gone and
//! the injector reports "275 pages failed"). Both cases therefore go through the
//! same mechanism: capture a context, let the exception happen, and *longjmp*
//! back. No unwind tables involved.
//!
//! # How the jump works
//!
//! `RtlCaptureContext` writes the current register state into our buffer and
//! returns normally (first return). We then call the target. If it faults (or
//! raises a Rust panic while `CATCH_PANIC` is set), the handler sees the armed
//! slot, records the fault, and calls `RtlRestoreContext` with the captured
//! context — which re-enters `call_with` right after `RtlCaptureContext`, this
//! time with `resumed == true`. That second pass returns `Err(Fault)` to *our*
//! frame as if nothing happened.
//!
//! # Nesting
//!
//! Guards nest: a command runs under `log::guard`, then calls a game function
//! under `call2`, and that function may panic. Contexts are kept on a small
//! stack (`MAX_DEPTH`), the innermost one catches, and the parent slot is
//! restored afterwards. `CATCH_PANIC` is sticky downwards (an inner guard never
//! un-catches a panic the outer guard asked to catch).
//!
//! # Rules for callers
//!
//! Restoring a context **abandons every intermediate frame**: their destructors
//! never run, their stack memory is reused. So:
//!   * the target must be an `extern "system"` function (or plain data reads);
//!   * do not keep a `MutexGuard`, a file handle or any other `Drop` resource
//!     alive across the guarded call — on the fault path it leaks (a leaked lock
//!     is a hang, not just memory).
//!
//! The payload is single-threaded by design; if two threads guard at once the
//! second one runs unguarded (logged once per process) rather than corrupting
//! the first one's context.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::ffi;
use crate::log;

/// How deep guards may nest before the innermost ones run unguarded.
const MAX_DEPTH: usize = 8;

static INSTALLED: AtomicBool = AtomicBool::new(false);
/// Thread that currently owns the guard stack (0 = free). Re-entrant: the same
/// thread may nest `call_with` (a command guard that calls into a guarded game
/// function), another thread has to run unguarded.
static LOCK_TID: AtomicU32 = AtomicU32::new(0);
/// Number of armed guards on the stack (single payload thread → global is fine).
static DEPTH: AtomicUsize = AtomicUsize::new(0);
/// Context pointer of the **innermost** armed guard; the handler jumps here.
static SAVED: AtomicUsize = AtomicUsize::new(0);
static GUARD_TID: AtomicU32 = AtomicU32::new(0);
/// While set, a Rust panic (SEH code `0x20474343`) is caught and longjmp'ed
/// instead of being unwound. Sticky for nested guards.
static CATCH_PANIC: AtomicBool = AtomicBool::new(false);
static LAST_CODE: AtomicU32 = AtomicU32::new(0);
static LAST_ADDR: AtomicU64 = AtomicU64::new(0);
static LOGGED_CONTENTION: AtomicBool = AtomicBool::new(false);

/// SEH code `x86_64-pc-windows-gnu` uses for Rust panics ("CGC " in ASCII).
const RUST_PANIC_CODE: u32 = 0x2047_4343;

/// x64 `CONTEXT` is 1232 bytes and must be 16-byte aligned: `RtlCaptureContext`
/// stores XMM registers with aligned moves.
#[repr(align(16))]
pub struct CtxBuf([u64; ffi::CONTEXT_QWORDS]);

impl CtxBuf {
    const ZERO: CtxBuf = CtxBuf([0u64; ffi::CONTEXT_QWORDS]);
}

/// One nesting level: what to restore when this level finishes, and whether the
/// handler already jumped back into it.
struct Slot {
    saved: usize,
    resumed: bool,
    prev_saved: usize,
    prev_tid: u32,
    prev_catch: bool,
}

impl Slot {
    const EMPTY: Slot = Slot {
        saved: 0,
        resumed: false,
        prev_saved: 0,
        prev_tid: 0,
        prev_catch: false,
    };
}

static mut CTX: [CtxBuf; MAX_DEPTH] = [CtxBuf::ZERO; MAX_DEPTH];
static mut SLOTS: [Slot; MAX_DEPTH] = [Slot::EMPTY; MAX_DEPTH];

/// What went wrong inside a guarded call.
#[derive(Clone, Copy, Debug)]
pub struct Fault {
    pub code: u32,
    pub addr: usize,
}

impl Fault {
    /// True when the "fault" was actually a Rust panic, not a hardware exception.
    pub fn is_panic(&self) -> bool {
        self.code == RUST_PANIC_CODE
    }
}

impl core::fmt::Display for Fault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.is_panic() {
            write!(f, "panic (0x{:08x}) at {:#x}", self.code, self.addr)
        } else {
            write!(f, "exception {:#010x} at {:#x}", self.code, self.addr)
        }
    }
}

/// Install the handler once. Safe to call from any thread, any number of times.
/// `first = 1` so we see the exception before the game's own handlers.
pub fn install() -> bool {
    if INSTALLED.load(Ordering::Acquire) {
        return true;
    }
    // SAFETY: `handler` has the system ABI and static code lifetime; the OS
    // invokes it with its documented EXCEPTION_POINTERS argument.
    let h = unsafe {
        ffi::AddVectoredExceptionHandler(1, handler as *const () as *const core::ffi::c_void)
    };
    let ok = !h.is_null();
    INSTALLED.store(ok, Ordering::Release);
    if ok {
        log::log_tag("veh", "installed (first=1, guard-armed only)");
    } else {
        log::log_tag(
            "veh",
            "AddVectoredExceptionHandler failed — guarded calls are unguarded",
        );
    }
    ok
}

pub fn installed() -> bool {
    INSTALLED.load(Ordering::Acquire)
}

/// Extract the two scalar fields shared by logging and recovery. The OS owns
/// `info` only for the duration of the vectored callback.
fn exception_parts(info: *mut ffi::EXCEPTION_POINTERS) -> (u32, usize) {
    if info.is_null() {
        return (0, 0);
    }
    // SAFETY: `info` is supplied by the OS exception dispatcher and remains live
    // for this callback; ExceptionRecord is either null or a live C record.
    let rec = unsafe { (*info).ExceptionRecord };
    if rec.is_null() {
        return (0, 0);
    }
    // SAFETY: `rec` is the OS-owned record checked non-null immediately above;
    // both fields are plain integers valid for every bit pattern.
    (unsafe { (*rec).ExceptionCode }, unsafe {
        (*rec).ExceptionAddress
    })
}

fn exception_info0(info: *mut ffi::EXCEPTION_POINTERS) -> Option<usize> {
    if info.is_null() {
        return None;
    }
    // SAFETY: info/rec are OS-owned callback records, valid for this callback.
    let rec = unsafe { (*info).ExceptionRecord };
    if rec.is_null() {
        return None;
    }
    // SAFETY: `rec` is the non-null OS record from above; its parameter count is
    // validated before indexing into the fixed inline array.
    let n = unsafe { (*rec).NumberParameters } as usize;
    (n > 0).then(|| {
        // SAFETY: NumberParameters > 0 proves element zero is present.
        unsafe { (*rec).ExceptionInformation[0] }
    })
}

/// `HROMATITE_LOG_AV=1`: report every exception that passes through while no
/// guard is armed. A payload thread that dies silently (no panic, no log line)
/// leaves no other trace — this names the faulting address instead.
fn log_passing(info: *mut ffi::EXCEPTION_POINTERS) {
    use std::sync::atomic::AtomicBool;
    static ON: AtomicBool = AtomicBool::new(false);
    static INIT: AtomicBool = AtomicBool::new(false);
    static SEEN: AtomicUsize = AtomicUsize::new(0);
    if !INIT.swap(true, Ordering::Relaxed) {
        ON.store(
            std::env::var("HROMATITE_LOG_AV")
                .map(|v| v != "0" && !v.is_empty())
                .unwrap_or(false),
            Ordering::Relaxed,
        );
    }
    // 16 was too few: a thread that dies of an uncaught exception is the thing
    // we are looking for, and the game raises plenty of benign SEH in between.
    static CAP: AtomicUsize = AtomicUsize::new(0);
    let cap = match CAP.load(Ordering::Relaxed) {
        0 => {
            let v = std::env::var("HROMATITE_LOG_AV_MAX")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(500usize);
            CAP.store(v, Ordering::Relaxed);
            v
        }
        v => v,
    };
    if !ON.load(Ordering::Relaxed) || SEEN.load(Ordering::Relaxed) >= cap {
        return;
    }
    let (code, addr) = exception_parts(info);
    // The thread id is the whole point: a thread that dies without unregistering
    // from Boehm shows up here first if its exit was an uncaught exception.
    if SEEN.fetch_add(1, Ordering::Relaxed) < cap {
        let tid = crate::raw::sys::current_thread_id();
        let extra = exception_info0(info).map_or_else(String::new, |a| format!(" info0={a:#x}"));
        log::log_tag(
            "veh",
            &format!("passing exception {code:#010x} at {addr:#x} tid={tid:#x}{extra} (unguarded)"),
        );
    }
}

unsafe extern "system" fn handler(info: *mut ffi::EXCEPTION_POINTERS) -> i32 {
    // Before ordinary payload guards: the GC recovery gate itself remains closed
    // unless a fully installed entry detour owns Boehm's record-list lock.
    // SAFETY: info is the OS-owned callback record described by recover_boehm_abort.
    if unsafe { crate::raw::gcguard::recover_boehm_abort(info) } {
        return ffi::EXCEPTION_CONTINUE_EXECUTION;
    }
    // Cheap gates first: on the game's own exceptions this must cost nothing.
    let depth = DEPTH.load(Ordering::Acquire);
    if depth == 0 {
        log_passing(info);
        return ffi::EXCEPTION_CONTINUE_SEARCH;
    }
    if GUARD_TID.load(Ordering::Relaxed) != crate::raw::sys::current_thread_id() {
        return ffi::EXCEPTION_CONTINUE_SEARCH;
    }

    let (code, addr) = exception_parts(info);
    // A Rust panic only counts as "ours" when a guard asked for it; otherwise it
    // must reach whatever `catch_unwind` the caller set up.
    if code == RUST_PANIC_CODE && !CATCH_PANIC.load(Ordering::Relaxed) {
        return ffi::EXCEPTION_CONTINUE_SEARCH;
    }

    LAST_CODE.store(code, Ordering::Relaxed);
    LAST_ADDR.store(addr as u64, Ordering::Relaxed);

    let saved = SAVED.load(Ordering::Relaxed);
    log::log_tag(
        "veh",
        &format!("caught {code:#010x} at {addr:#x} — restoring captured context ({saved:#x})"),
    );
    if saved == 0 {
        // No context to go back to (should not happen): let the game see it.
        return ffi::EXCEPTION_CONTINUE_SEARCH;
    }
    // SAFETY: depth is the active 1..=MAX_DEPTH value published by call_with,
    // so depth-1 indexes an initialized slot owned by the current thread.
    unsafe {
        SLOTS[depth - 1].resumed = true;
    }
    // Does not return when the restored context differs from the current one.
    // SAFETY: saved points to the 16-byte-aligned, fully sized CONTEXT captured
    // by this same thread. The handler never publishes a null/non-context pointer.
    unsafe { ffi::RtlRestoreContext(saved as *const u64, core::ptr::null()) };
    // Only reachable if the "restore" was a no-op.
    ffi::EXCEPTION_CONTINUE_SEARCH
}

/// Undo one nesting level: pop the slot and re-arm the parent (if any).
fn pop(depth: usize) {
    // SAFETY: callers pass the active depth in 1..=MAX_DEPTH and hold LOCK_TID,
    // so no other thread can mutate this slot or its previous-link fields.
    unsafe {
        let slot = &mut SLOTS[depth - 1];
        let prev_saved = slot.prev_saved;
        let prev_tid = slot.prev_tid;
        let prev_catch = slot.prev_catch;
        slot.resumed = false;
        slot.saved = 0;

        DEPTH.store(depth - 1, Ordering::Release);
        SAVED.store(prev_saved, Ordering::Relaxed);
        GUARD_TID.store(prev_tid, Ordering::Relaxed);
        CATCH_PANIC.store(prev_catch, Ordering::Relaxed);
    }
}

/// Run `f` under the vectored handler; a hardware exception inside it becomes
/// `Err(Fault)` in this frame instead of a dead process.
fn call_with_mode<R>(f: impl FnOnce() -> R, catch_panic: bool) -> Result<R, Fault> {
    install();

    let tid = crate::raw::sys::current_thread_id();
    // Re-entrant per thread: the slots are global, so another *thread* has to
    // run unguarded rather than clobber the owner's context.
    let mut acquired = false;
    match LOCK_TID.compare_exchange(0, tid, Ordering::Acquire, Ordering::Relaxed) {
        Ok(_) => acquired = true,
        Err(owner) if owner == tid => {}
        Err(_) => {
            if !LOGGED_CONTENTION.swap(true, Ordering::Relaxed) {
                log::log_tag("veh", "another thread holds the guard — running unguarded");
            }
            return Ok(f());
        }
    }
    let depth = DEPTH.load(Ordering::Acquire);
    if depth >= MAX_DEPTH {
        if acquired {
            LOCK_TID.store(0, Ordering::Release);
        }
        log::log_tag("veh", "guard stack full — running unguarded");
        return Ok(f());
    }

    let previous_catch = if catch_panic {
        CATCH_PANIC.swap(true, Ordering::AcqRel)
    } else {
        CATCH_PANIC.load(Ordering::Relaxed)
    };

    // SAFETY: depth < MAX_DEPTH and LOCK_TID is held; the context storage is
    // statically allocated, aligned and large enough for RtlCaptureContext.
    let ctx_ptr = unsafe { CTX[depth].0.as_mut_ptr() as usize };
    // SAFETY: depth < MAX_DEPTH and this thread owns the global guard stack, so
    // the mutable slot cannot alias another active access.
    unsafe {
        let slot = &mut SLOTS[depth];
        slot.saved = ctx_ptr;
        slot.resumed = false;
        slot.prev_saved = SAVED.load(Ordering::Relaxed);
        slot.prev_tid = GUARD_TID.load(Ordering::Relaxed);
        slot.prev_catch = previous_catch;
    }
    SAVED.store(ctx_ptr, Ordering::Relaxed);
    GUARD_TID.store(tid, Ordering::Relaxed);
    DEPTH.store(depth + 1, Ordering::Release);

    // SAFETY: ctx_ptr targets CTX[depth], an aligned CONTEXT_QWORDS buffer.
    unsafe { ffi::RtlCaptureContext(ctx_ptr as *mut u64) };

    // Second return: the handler longjmp'ed here.
    // SAFETY: depth is still the active slot; only the current thread and its
    // vectored handler access `resumed` at this point.
    if unsafe { SLOTS[depth].resumed } {
        pop(depth + 1);
        if acquired {
            LOCK_TID.store(0, Ordering::Release);
        }
        return Err(Fault {
            code: LAST_CODE.load(Ordering::Relaxed),
            addr: LAST_ADDR.load(Ordering::Relaxed) as usize,
        });
    }

    let value = f();
    pop(depth + 1);
    if acquired {
        LOCK_TID.store(0, Ordering::Release);
    }
    Ok(value)
}

/// Run `f` under the vectored handler without changing panic-catching state.
pub fn call_with<R>(f: impl FnOnce() -> R) -> Result<R, Fault> {
    call_with_mode(f, false)
}

/// Like [`call_with`], but a Rust **panic** raised inside `f` is caught by the
/// same handler and turned into `Err` (code `0x20474343`).
///
/// This is what `log::guard` uses: the only reliable containment for a payload
/// whose unwind tables are invisible to the OS.
///
/// Cost: the panic payload and every local of `f` leak (their `Drop` never runs).
/// The panic hook still logs file/line/message before the raise, so nothing is
/// lost but memory.
pub fn call_guarded<R>(f: impl FnOnce() -> R) -> Result<R, Fault> {
    // `call_with_mode` snapshots and restores the parent's flag in its slot.
    // A normally returning nested guard therefore cannot disarm its parent;
    // a contended thread never mutates the owning thread's flag at all.
    call_with_mode(f, true)
}

/// Call a raw function pointer with 0..10 integer/pointer arguments.
/// The signatures are `extern "system"` because that is how Win32/x64 code is
/// called; on x64 the calling convention is the same for `"C"`, but being
/// explicit keeps the intent clear.
///
/// Beyond four arguments the compiler emits the home space and stack slots
/// itself — no hand-written thunk is involved, and VEH still owns the frame.
macro_rules! guarded_calls {
    ($($name:ident($($arg:ident),+)),* $(,)?) => {
        $(
            /// # Safety
            /// `f` must be an executable Win64 address with exactly this
            /// signature. All pointer arguments and callee side effects must be
            /// valid; a mismatched callee can corrupt memory even when it
            /// returns without raising an exception.
            #[allow(clippy::too_many_arguments)]
            pub unsafe fn $name(f: usize, $($arg: usize),+) -> Result<usize, Fault> {
                if f == 0 {
                    return Ok(0);
                }
                call_with(|| {
                    // SAFETY: f is supplied for this exact Win64 signature; VEH
                    // captures this thread before the raw call.
                    let fp: extern "system" fn($($arg: usize),+) -> usize = unsafe { core::mem::transmute(f) };
                    fp($($arg),+)
                })
            }
        )*
    };
}

/// # Safety
/// `f` must be an executable Win64 address taking no arguments, returning an
/// integer or a pointer; a mismatched callee can corrupt memory.
pub unsafe fn call0(f: usize) -> Result<usize, Fault> {
    if f == 0 {
        return Ok(0);
    }
    call_with(|| {
        // SAFETY: the caller supplies an executable Win64 address; VEH converts
        // a bad address or signature into Err instead of escaping.
        let fp: extern "system" fn() -> usize = unsafe { core::mem::transmute(f) };
        fp()
    })
}

guarded_calls!(
    call1(a),
    call2(a, b),
    call3(a, b, c),
    call4(a, b, c, d),
    call5(a, b, c, d, e),
    call6(a, b, c, d, e, f),
    call7(a, b, c, d, e, f, g),
    call8(a, b, c, d, e, f, g, h),
    call9(a, b, c, d, e, f, g, h, i),
    call10(a, b, c, d, e, f, g, h, i, j),
);
