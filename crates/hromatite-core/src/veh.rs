//! Guarded calls: VEH-based "setjmp/longjmp" for calling functions we have not
//! proven safe.
//!
//! `catch_unwind` cannot catch an access violation; only a vectored exception
//! handler can turn a fault inside a game function into an `Err`. The handler
//! acts only while our own guard is armed **on the same thread** (flag + thread
//! id), so the game's own exceptions are never swallowed.
//!
//! # How the jump works
//!
//! `RtlCaptureContext` writes the current register state into our buffer and
//! returns normally (first return). We then call the target. If it faults, the
//! handler sees `IN_GUARD`, records the fault, and calls `RtlRestoreContext`
//! with the captured context — which re-enters `call_with` right after
//! `RtlCaptureContext`, but this time with `RESUMED == true`. That second pass
//! returns `Err(Fault)` to *our* frame as if nothing happened.
//!
//! # Rules for callers
//!
//! Restoring a context **abandons every intermediate frame** of the faulting
//! call: their destructors never run, their stack memory is simply reused. So:
//!   * the target must be an `extern "system"` function (or plain data reads);
//!   * the closure passed to `call_with` must not hold `&mut` borrows that the
//!     rest of the program observes afterwards — on the fault path they are
//!     leaked, not released;
//!   * do not let Rust values with `Drop` live across the guarded call inside
//!     the closure (a leaked `Vec` is a heap leak, a leaked lock is a hang).
//!
//! Nesting is not supported (one saved context per thread): a nested guard on
//! the same thread is logged as a configuration error and runs unguarded inside
//! the outer guard, so a fault still unwinds to the *outer* frame.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use crate::ffi;
use crate::log;

static INSTALLED: AtomicBool = AtomicBool::new(false);
/// Armed only while our frame is the fault target.
static IN_GUARD: AtomicBool = AtomicBool::new(false);
static GUARD_TID: AtomicU32 = AtomicU32::new(0);
/// Pointer to the 16-byte-aligned `CONTEXT` buffer of the armed `call_with`.
static SAVED: AtomicUsize = AtomicUsize::new(0);
/// Set by the handler just before `RtlRestoreContext`; `call_with` checks it on
/// the second return from `RtlCaptureContext`.
static RESUMED: AtomicBool = AtomicBool::new(false);
static LAST_CODE: AtomicU32 = AtomicU32::new(0);
static LAST_ADDR: AtomicU64 = AtomicU64::new(0);

/// SEH code `x86_64-pc-windows-gnu` uses for Rust panics ("CGC " in ASCII).
/// Those unwind normally, so the handler steps aside.
const RUST_PANIC_CODE: u32 = 0x2047_4343;

/// What went wrong inside a guarded call (never a Rust panic — those are
/// `log::guard`'s job; this is a hardware exception).
#[derive(Clone, Copy, Debug)]
pub struct Fault {
    pub code: u32,
    pub addr: usize,
}

impl core::fmt::Display for Fault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "exception {:#010x} at {:#x}", self.code, self.addr)
    }
}

/// Install the handler once. Safe to call from any thread, any number of times.
/// `first = 1` so we see the exception before the game's own handlers.
pub fn install() -> bool {
    if INSTALLED.load(Ordering::Acquire) {
        return true;
    }
    let h = unsafe {
        ffi::AddVectoredExceptionHandler(1, handler as *const () as *const core::ffi::c_void)
    };
    let ok = !h.is_null();
    INSTALLED.store(ok, Ordering::Release);
    if ok {
        log::log_tag("veh", "installed (first=1, guard-armed only)");
    } else {
        log::log_tag("veh", "AddVectoredExceptionHandler failed — guarded calls are unguarded");
    }
    ok
}

pub fn installed() -> bool {
    INSTALLED.load(Ordering::Acquire)
}

unsafe extern "system" fn handler(info: *mut ffi::EXCEPTION_POINTERS) -> i32 {
    // Cheap gates first: on the game's own exceptions this must cost nothing.
    if !IN_GUARD.load(Ordering::Acquire) {
        return ffi::EXCEPTION_CONTINUE_SEARCH;
    }
    if GUARD_TID.load(Ordering::Relaxed) != unsafe { ffi::GetCurrentThreadId() } {
        return ffi::EXCEPTION_CONTINUE_SEARCH;
    }

    let mut code = 0u32;
    let mut addr = 0usize;
    if !info.is_null() {
        let rec = unsafe { (*info).ExceptionRecord };
        if !rec.is_null() {
            code = unsafe { (*rec).ExceptionCode };
            addr = unsafe { (*rec).ExceptionAddress };
        }
    }
    // `x86_64-pc-windows-gnu` unwinds Rust panics with its own SEH code
    // ("CGC " = 0x20474343). Those must reach `catch_unwind` normally: restoring
    // our context instead would skip every destructor on the way out.
    if code == RUST_PANIC_CODE {
        return ffi::EXCEPTION_CONTINUE_SEARCH;
    }

    LAST_CODE.store(code, Ordering::Relaxed);
    LAST_ADDR.store(addr as u64, Ordering::Relaxed);

    let saved = SAVED.load(Ordering::Relaxed);
    log::log_tag(
        "veh",
        &format!(
            "caught {code:#010x} at {addr:#x} — restoring captured context ({saved:#x})"
        ),
    );
    if saved == 0 {
        // No context to go back to (should not happen): let the game see it.
        return ffi::EXCEPTION_CONTINUE_SEARCH;
    }
    RESUMED.store(true, Ordering::Release);
    // Does not return when the restored context differs from the current one.
    unsafe { ffi::RtlRestoreContext(saved as *const u64, core::ptr::null()) };
    // Only reachable if the "restore" was a no-op.
    ffi::EXCEPTION_CONTINUE_SEARCH
}

/// Clears the armed flag on every exit path, including early returns of the
/// closure body and panics (a panic that left `IN_GUARD` set would hijack the
/// next unrelated game exception).
struct Disarm;

impl Drop for Disarm {
    fn drop(&mut self) {
        IN_GUARD.store(false, Ordering::Release);
        SAVED.store(0, Ordering::Relaxed);
        RESUMED.store(false, Ordering::Relaxed);
        GUARD_TID.store(0, Ordering::Relaxed);
    }
}

/// x64 `CONTEXT` is 1232 bytes and must be 16-byte aligned: `RtlCaptureContext`
/// stores XMM registers with aligned moves.
#[repr(align(16))]
struct CtxBuf([u64; ffi::CONTEXT_QWORDS]);

/// Run `f` under the vectored handler; a hardware exception inside it becomes
/// `Err(Fault)` in this frame instead of a dead process.
pub fn call_with<R>(f: impl FnOnce() -> R) -> Result<R, Fault> {
    install();

    let tid = unsafe { ffi::GetCurrentThreadId() };
    if IN_GUARD.load(Ordering::Acquire) {
        if GUARD_TID.load(Ordering::Relaxed) == tid {
            // One saved context per thread: nesting would clobber it.
            log::log_tag("veh", "nested guard on the same thread — running inside the outer guard");
            return Ok(f());
        }
        // Different thread: our statics are global, so this thread has to wait
        // its turn or it would overwrite the armed context. Single-threaded
        // payload (inline DllMain) makes this a non-issue; be explicit anyway.
        log::log_tag("veh", "guard armed on another thread — running unguarded");
        return Ok(f());
    }

    let mut ctx = CtxBuf([0u64; ffi::CONTEXT_QWORDS]);
    GUARD_TID.store(tid, Ordering::Relaxed);
    SAVED.store(ctx.0.as_ptr() as usize, Ordering::Relaxed);
    RESUMED.store(false, Ordering::Relaxed);
    IN_GUARD.store(true, Ordering::Release);
    let _disarm = Disarm;

    unsafe { ffi::RtlCaptureContext(ctx.0.as_mut_ptr()) };

    // Second return: the handler longjmp'ed here.
    if RESUMED.load(Ordering::Acquire) {
        return Err(Fault {
            code: LAST_CODE.load(Ordering::Relaxed),
            addr: LAST_ADDR.load(Ordering::Relaxed) as usize,
        });
    }

    Ok(f())
}

/// Call a raw function pointer with 0..3 integer/pointer arguments.
/// The signatures are `extern "system"` because that is how Win32/x64 code is
/// called; on x64 the calling convention is the same for `"C"`, but being
/// explicit keeps the intent clear.
pub fn call0(f: usize) -> Result<usize, Fault> {
    if f == 0 {
        return Ok(0);
    }
    call_with(|| unsafe {
        let fp: extern "system" fn() -> usize = core::mem::transmute(f);
        fp()
    })
}

pub fn call1(f: usize, a: usize) -> Result<usize, Fault> {
    if f == 0 {
        return Ok(0);
    }
    call_with(|| unsafe {
        let fp: extern "system" fn(usize) -> usize = core::mem::transmute(f);
        fp(a)
    })
}

pub fn call2(f: usize, a: usize, b: usize) -> Result<usize, Fault> {
    if f == 0 {
        return Ok(0);
    }
    call_with(|| unsafe {
        let fp: extern "system" fn(usize, usize) -> usize = core::mem::transmute(f);
        fp(a, b)
    })
}

pub fn call3(f: usize, a: usize, b: usize, c: usize) -> Result<usize, Fault> {
    if f == 0 {
        return Ok(0);
    }
    call_with(|| unsafe {
        let fp: extern "system" fn(usize, usize, usize) -> usize = core::mem::transmute(f);
        fp(a, b, c)
    })
}
