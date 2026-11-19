//! Managed LateUpdate command tick, GC guard entry points, and hook registry.
//!
//! The generic decoder/trampoline/patch engine lives in [`hromatite_hook`].
//! Runtime work runs on Unity's own managed thread; no payload-owned polling
//! thread is created. Managed method bodies always run after best-effort
//! guarded instrumentation.

use crate::ffi;
use crate::log;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

pub use hromatite_hook::decode::{Insn, decode_one, decode_span, prologue_len};
pub use hromatite_hook::inline::{
    Hook, Prep, RELAY_OFF, hook_inline, patch, prepare, prepare_span,
};
pub use hromatite_hook::mem::{
    PAGE_SIZE, alloc_near, flush, set_range_protect, write_mode, write_window,
};
pub use hromatite_hook::tramp::{adjusted_disp32, branch_into_prologue, rel32_ok};

// ---------------------------------------------------------------------------
// Tick source
// ---------------------------------------------------------------------------

/// Everything the resident runtime contributes to one managed tick.
///
/// `hromatite-core` owns the detours; the command channel, the feature router
/// and the console live in `hromatite-runtime`, which depends on this crate.
/// Publishing function pointers here - instead of the detour calling the
/// runtime - is what keeps the crate graph acyclic.
pub struct TickTarget {
    /// First tick only: create the command reader and log the console path.
    pub prepare: fn(),
    /// Every frame, unthrottled: the feature image's per-frame entry.
    pub frame: fn(),
    /// Throttle for the full work pass (`HROMATITE_TICK_MS`).
    pub interval_ms: fn() -> u64,
    /// Throttled work: GC reports, hot reload, command dispatch.
    pub work: fn(),
    /// True only while a reload transition needs the original method first.
    pub original_first: fn() -> bool,
}

static TICKS: AtomicU64 = AtomicU64::new(0);
static LAST_MS: AtomicU64 = AtomicU64::new(0);
static START_MS: AtomicU64 = AtomicU64::new(0);
static BUSY: AtomicBool = AtomicBool::new(false);
static FRAME_BUSY: AtomicBool = AtomicBool::new(false);
static TICK_TARGET: AtomicUsize = AtomicUsize::new(0);
static HOOKS: Mutex<Vec<Hook>> = Mutex::new(Vec::new());

fn now_ms() -> u64 {
    // SAFETY: parameterless Win32 query with no pointer preconditions.
    unsafe { ffi::GetTickCount64() }
}

/// Publish the resident's tick implementation. Called once at startup, before
/// any detour can fire; a later call replaces it.
pub fn set_tick_target(target: &'static TickTarget) {
    TICK_TARGET.store(target as *const TickTarget as usize, Ordering::Release);
}

fn tick_target() -> Option<&'static TickTarget> {
    let raw = TICK_TARGET.load(Ordering::Acquire) as *const TickTarget;
    // SAFETY: only `set_tick_target` writes this slot, and it accepts nothing
    // but a `&'static TickTarget`; the pointee therefore outlives the process.
    unsafe { raw.as_ref() }
}

/// Throttle runtime work on Unity's managed thread. Atomic claiming and the
/// BUSY flag keep re-entrant calls from overlapping a pass already in progress.
/// Until [`set_tick_target`] has run the tick only counts frames.
pub fn tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
    let Some(target) = tick_target() else {
        return;
    };
    // The per-frame entry is not throttled: `Graphics.DrawMesh` applies to
    // exactly one frame, so a 4 Hz path would flash the in-game overlay instead
    // of drawing it. It is bounded by contract (no file or command I/O).
    if !FRAME_BUSY.swap(true, Ordering::AcqRel) {
        let _ = log::guard("hook::frame", target.frame);
        FRAME_BUSY.store(false, Ordering::Release);
    }
    let now = now_ms();
    let iv = (target.interval_ms)().max(1);
    let last = LAST_MS.load(Ordering::Relaxed);
    if now.wrapping_sub(last) < iv {
        return;
    }
    if LAST_MS
        .compare_exchange(last, now, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    if BUSY.swap(true, Ordering::AcqRel) {
        return;
    }
    // Never unwind through the game's managed frame.
    let _ = log::guard("hook::tick", target.work);
    BUSY.store(false, Ordering::Release);
}

/// True only for reload transitions, where native work must not hold the game's
/// next callback behind mapping, init, or retirement.
pub fn original_first_required() -> bool {
    tick_target().map(|t| (t.original_first)()).unwrap_or(false)
}

// ---- detours --------------------------------------------------------------

/// `GC_suspend_all`'s indirect `kernel32!GetThreadContext` call, followed by
/// `test eax,eax` and `jne`. The trampoline carries all three instructions;
/// live and synthetic results therefore share Boehm's original success branch
/// instead of falling into its retry loop.
/// The site itself is per build (`GcAnchors::context_call`); only the shape is
/// stable: `call qword [rip+disp32]; test eax,eax`.
const GC_CONTEXT_CALL_PROLOGUE: usize = 8;

fn valid_gc_context_call_site(bytes: &[u8]) -> bool {
    bytes.len() >= GC_CONTEXT_CALL_PROLOGUE
        && bytes[0] == 0xff
        && bytes[1] == 0x15
        && bytes[6] == 0x85
        && bytes[7] == 0xc0
}

static GC_CONTEXT_ORIG: AtomicUsize = AtomicUsize::new(0);
static GC_THREADS_BASE: AtomicUsize = AtomicUsize::new(0);
static INLINE_REPAIRS: AtomicU64 = AtomicU64::new(0);
static INLINE_LAST_TID: AtomicU32 = AtomicU32::new(0);

/// Record only lock-free counters here. Allocation, formatting and file I/O
/// belong to `work`, after the collector has released the world.
fn note_inline_repair(id: u32) {
    INLINE_LAST_TID.store(id, Ordering::Relaxed);
    INLINE_REPAIRS.fetch_add(1, Ordering::Release);
}

/// Drain the lock-free inline-repair counters published by the
/// `GetThreadContext` guard; the caller formats and logs them.
pub fn take_inline_repairs() -> Option<(u64, u32)> {
    let count = INLINE_REPAIRS.swap(0, Ordering::AcqRel);
    (count != 0).then(|| (count, INLINE_LAST_TID.load(Ordering::Acquire)))
}

const WIN64_CONTEXT_FLAGS: usize = 0x30;
const WIN64_CONTEXT_SIZE: usize = ffi::CONTEXT_QWORDS * core::mem::size_of::<u64>();
const _: () = assert!(WIN64_CONTEXT_SIZE == 0x4d0);

/// Overwrite one caller-owned Win64 CONTEXT without an OS round trip or an
/// intermediate heap/stack buffer. ContextFlags is an input, so preserve it.
///
/// # Safety
/// `context..context+0x4d0` must be valid for unaligned reads and writes and
/// must not overlap a live Rust reference. This is Boehm's output buffer at the
/// exact GetThreadContext call-site, not an arbitrary address.
unsafe fn write_synthetic_dead_context_raw(context: *mut u8) {
    // SAFETY: the caller guarantees a complete 0x4d0-byte CONTEXT. The field
    // offsets and unaligned accesses are valid for the Win64 x64 layout.
    let flags =
        unsafe { core::ptr::read_unaligned(context.add(WIN64_CONTEXT_FLAGS).cast::<u32>()) };
    for index in 0..(WIN64_CONTEXT_SIZE / core::mem::size_of::<u64>()) {
        // SAFETY: index covers exactly 0x4d0 / 8 complete qwords. Explicit
        // unaligned stores prevent LLVM from calling memset in the stop-world.
        let out = unsafe { context.add(index * core::mem::size_of::<u64>()) };
        // SAFETY: out points to one valid qword in the caller's CONTEXT; the
        // instruction requires no stack, preserves flags and accepts misalignment.
        unsafe {
            core::arch::asm!(
                "mov qword ptr [{out}], 0",
                out = in(reg) out,
                options(nostack, preserves_flags)
            );
        }
    }
    // SAFETY: the same complete CONTEXT is live, and this four-byte field is
    // inside it; unaligned access supports the ABI-provided buffer verbatim.
    unsafe {
        core::ptr::write_unaligned(context.add(WIN64_CONTEXT_FLAGS).cast::<u32>(), flags);
    }
}

/// Find and unlink the record owning `handle` while Boehm holds its stop-world
/// lock. Every access is a direct unaligned scalar access into static table /
/// record memory already validated by the collector's own traversal.
///
/// # Safety
/// `base` must address all 256 writable GC_threads bucket pointers. Reachable
/// records must have the measured next/id/handle layout and remain allocated
/// and writable for the duration of the stop-world traversal.
unsafe fn unlink_by_handle_raw(base: usize, handle: usize) -> Option<u32> {
    if base == 0 {
        return None;
    }
    for bucket in 0..256usize {
        // SAFETY: the caller supplies all 256 writable bucket pointers.
        let link = base.wrapping_add(bucket * core::mem::size_of::<usize>());
        let mut previous = 0usize;
        // SAFETY: The caller supplies all 256 writable bucket pointers; link is
        // one complete pointer-sized cell in that table.
        let mut current = unsafe { core::ptr::read_unaligned(link as *const usize) };
        for _ in 0..8 {
            if current == 0 {
                break;
            }
            // SAFETY: caller guarantees the measured GC_Thread_Rep layout and
            // live record memory; unaligned loads avoid imposing Rust alignment
            // on Boehm's allocator.
            let id = unsafe { core::ptr::read_unaligned(current.wrapping_add(8) as *const u32) };
            // SAFETY: current is a traversed live GC record under the caller's
            // stop-world contract, and 0x10 is its complete handle field.
            let record_handle =
                unsafe { core::ptr::read_unaligned(current.wrapping_add(0x10) as *const usize) };
            if id != 0 && record_handle == handle {
                // SAFETY: same live record contract as the field loads above.
                let next = unsafe { core::ptr::read_unaligned(current as *const usize) };
                let predecessor = if previous == 0 { link } else { previous };
                // SAFETY: predecessor is either the validated bucket link or a
                // previously traversed live record's writable next field.
                unsafe { core::ptr::write_unaligned(predecessor as *mut usize, next) };
                return Some(id);
            }
            previous = current;
            // SAFETY: same live record contract as the field loads above.
            current = unsafe { core::ptr::read_unaligned(current as *const usize) };
        }
    }
    None
}

/// Complete the already-classified dead-thread call. Returning success does
/// not depend on finding the record: another Boehm detach may have removed it
/// after its handle was signalled, and falling back to GetThreadContext would
/// put this exact call back into the unbounded retry loop.
///
/// # Safety
/// Combines the contracts of `write_synthetic_dead_context_raw` and
/// `unlink_by_handle_raw`; invoked only from Boehm's GetThreadContext call-site.
unsafe fn complete_dead_gc_call_raw(
    base: usize,
    handle: usize,
    context: *mut u8,
) -> (i32, Option<u32>) {
    // SAFETY: forwarded caller contract; construct a valid output before the
    // best-effort unlink so resume can never observe a corrupt context.
    unsafe { write_synthetic_dead_context_raw(context) };
    // SAFETY: Boehm owns the stop-world lock for both operations.
    let removed = unsafe { unlink_by_handle_raw(base, handle) };
    (1, removed)
}

/// Select the copied instruction stream for this call-site. A signalled handle
/// enters at the copied `test eax,eax`; every other result takes the copied OS
/// call from its first byte. The naked caller sets EAX=1 before either jump:
/// the live OS call overwrites it, while the dead path preserves it for `test`.
///
/// # Safety
/// Called only by `gc_context_detour` with the exact Boehm call-site arguments.
unsafe extern "system" fn gc_context_route(thread: *mut u8, context: *mut u8) -> usize {
    let original = GC_CONTEXT_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return 0;
    }
    let base = GC_THREADS_BASE.load(Ordering::Acquire);
    if base != 0 {
        // SAFETY: RCX is Boehm's own duplicated thread handle. A zero-timeout
        // wait is nonblocking and borrows the handle without closing it.
        let wait = unsafe { ffi::WaitForSingleObject(thread as ffi::HANDLE, 0) };
        if wait == 0 {
            // SAFETY: WAIT_OBJECT_0 proves full termination. The exact call-site
            // contract and stop-world lock validate raw output/table/records.
            let (_, removed) = unsafe { complete_dead_gc_call_raw(base, thread as usize, context) };
            if let Some(id) = removed {
                note_inline_repair(id);
            }
            // Skip only the six-byte indirect OS call in the trampoline. The
            // naked wrapper supplies EAX=1 for the copied `test` and `jne`.
            return original.wrapping_add(6);
        }
    }
    original
}

/// Call-site entry must not `ret`: it was reached by a jump, so its stack return
/// address belongs to GC_suspend_all's caller. Calling an ordinary Rust detour
/// and returning 1 would therefore leave GC_suspend_all early and change its ABI.
/// Instead this tiny frame tail-jumps into the copied call or its following test.
///
/// # Safety
/// May only be patched over the validated 14-byte GetThreadContext call-site.
/// RCX/RDX are forwarded under Win64 ABI and RSP is restored before the jump.
#[unsafe(naked)]
pub unsafe extern "system" fn gc_context_detour() -> ! {
    core::arch::naked_asm!(
        // Entry RSP is 16-byte aligned. Reserve the Win64 shadow area plus two
        // slots for the original arguments: the router is allowed to clobber
        // both RCX and RDX under the same ABI as GetThreadContext itself.
        "sub rsp, 0x30",
        "mov [rsp + 0x20], rcx",
        "mov [rsp + 0x28], rdx",
        "call {route}",
        "mov rcx, [rsp + 0x20]",
        "mov rdx, [rsp + 0x28]",
        "add rsp, 0x30",
        "mov r11, rax",
        "mov eax, 1",
        "jmp r11",
        route = sym gc_context_route,
    );
}

#[allow(dead_code)]
fn install_legacy_gc_context_guard() -> bool {
    let Some((ga, size)) = crate::procs::own_module(crate::GAME_ASSEMBLY) else {
        log::log_tag("gct", "inline guard disabled: GameAssembly is not loaded");
        return false;
    };
    let Some(anchors) = crate::buildinfo::gc_anchors(ga, size) else {
        log::log_tag(
            "gct",
            "inline guard disabled: this GameAssembly build has no GC anchors",
        );
        return false;
    };
    let target = ga.wrapping_add(anchors.context_call as usize);
    let Some(head) = crate::safe::read_bytes(target, 14) else {
        log::log_tag(
            "gct",
            "inline guard disabled: Boehm call-site is unreadable",
        );
        return false;
    };
    if !valid_gc_context_call_site(&head) || head[8..] != [0x0f, 0x85, 0x8b, 0x00, 0x00, 0x00] {
        log::log_tag(
            "gct",
            "inline guard disabled: loaded Boehm call-site signature mismatch",
        );
        return false;
    }
    let Some(prep) = prepare_span(target, 14) else {
        log::log_tag(
            "gct",
            "Boehm GetThreadContext call-site trampoline rejected",
        );
        return false;
    };
    if prep.prologue != 14 {
        log::log_tag(
            "gct",
            "inline guard disabled: trampoline did not copy call; test; jne exactly",
        );
        return false;
    }
    let table_base = ga.wrapping_add(anchors.threads as usize);
    // Resolve the table once. The hot path only performs a zero-timeout wait,
    // direct scalar memory accesses and lock-free counter updates.
    GC_THREADS_BASE.store(table_base, Ordering::Release);
    // Publish before patching: a collector entering during the patch window
    // must always have a valid original trampoline to call.
    GC_CONTEXT_ORIG.store(prep.trampoline, Ordering::Release);
    match patch(&prep, gc_context_detour as *const () as usize) {
        Some(h) => {
            register(h);
            log::log_tag(
                "gct",
                &format!(
                    "Boehm GetThreadContext call-site guard installed at {target:#x}, table {table_base:#x}"
                ),
            );
            true
        }
        None => {
            GC_CONTEXT_ORIG.store(0, Ordering::Release);
            GC_THREADS_BASE.store(0, Ordering::Release);
            log::log_tag(
                "gct",
                "Boehm GetThreadContext call-site guard installation failed",
            );
            false
        }
    }
}

/// Install the synchronized entry-Guard used by every active payload mode.
/// The previous mid-function installer remains above only for its regression
/// fixtures and disassembly references; it is not used at runtime.
pub fn install_gc_context_guard() -> bool {
    crate::raw::gcguard::install()
}

// ---- worker-body provenance (why did a GC record leak?) -------------------

static GC_DISPATCH_ORIG: AtomicUsize = AtomicUsize::new(0);
static GC_UNREGISTER_ORIG: AtomicUsize = AtomicUsize::new(0);

/// Record the real worker body before the shared OS trampoline can free its
/// argument block. `RCX=inner`, with `{thread,fn,arg}` at offsets 0/8/16.
///
/// # Safety
/// The generated detour preserves the dispatcher ABI. `inner` is forwarded from
/// il2cpp and is read through the checked OS-backed reader; the trampoline must
/// name the same function before it is published.
pub unsafe extern "system" fn gc_worker_detour(inner: *mut u8) -> usize {
    let tid = crate::gcthreads::own_tid();
    if let Some([entry, arg]) = crate::safe::read_words::<2>((inner as usize).wrapping_add(8)) {
        crate::gcthreads::note_worker_body(tid, entry, arg);
    }
    let original = GC_DISPATCH_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return 0;
    }
    // SAFETY: original is the trampoline for the one-argument worker dispatcher;
    // forwarding preserves RAX even though il2cpp ignores it on this path.
    let f: unsafe extern "system" fn(*mut u8) -> usize = unsafe { core::mem::transmute(original) };
    // SAFETY: inner is the original, unmodified dispatcher argument.
    unsafe { f(inner) }
}

/// Mark an actual unregister attempt for the same tid. The original returns
/// zero in EAX on this build; usize is ABI-compatible and preserves it.
///
/// # Safety
/// No arguments are invented and the original trampoline is called only after
/// it has been published by the installer.
pub unsafe extern "system" fn gc_unregister_detour() -> usize {
    let tid = crate::gcthreads::own_tid();
    crate::gcthreads::note_unregister(tid);
    let original = GC_UNREGISTER_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return 0;
    }
    // SAFETY: original is the trampoline for the no-argument, EAX-returning
    // unregister function whose entry the descriptor provides.
    let f: unsafe extern "system" fn() -> usize = unsafe { core::mem::transmute(original) };
    // SAFETY: the no-argument trampoline supplies the original execution context.
    unsafe { f() }
}

static GC_REMOVE_ORIG: AtomicUsize = AtomicUsize::new(0);
static EXIT_THREAD_ORIG: AtomicUsize = AtomicUsize::new(0);
static END_THREAD_EX_ORIG: AtomicUsize = AtomicUsize::new(0);

/// The actual deletion of a Boehm record. `FUN_180c417b0(DWORD tid)`: it walks
/// the bucket without a NULL check, `CloseHandle`s the duplicated handle and
/// `GC_free`s the record. Hooking the wrapper `GC_unregister_my_thread` only
/// proves somebody asked; this proves the record went away.
///
/// # Safety
/// The detour preserves the one-DWORD argument in `ECX` and forwards the
/// original return value; nothing is invented.
pub unsafe extern "system" fn gc_remove_detour(tid: u32) -> usize {
    crate::gcthreads::note_gc_remove(tid);
    let original = GC_REMOVE_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return 0;
    }
    // SAFETY: original is the trampoline for the one-argument removal function
    // the descriptor named as `gc.remove_thread`; the argument is forwarded
    // unchanged.
    let f: unsafe extern "system" fn(u32) -> usize = unsafe { core::mem::transmute(original) };
    // SAFETY: as above — tid is the original caller's value.
    unsafe { f(tid) }
}

/// A thread that dies through `ExitThread`/`_endthreadex` never returns from its
/// body, so the body's `GC_unregister_my_thread` is skipped and Boehm keeps a
/// record for a thread that no longer exists. Recording this names the leak.
///
/// # Safety
/// Only the exit code / return address is read and both are forwarded unchanged.
pub unsafe extern "system" fn exit_thread_detour(code: u32) {
    crate::gcthreads::note_thread_exit(crate::gcthreads::own_tid(), "ExitThread", code);
    let original = EXIT_THREAD_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    // SAFETY: original is the kernel32!ExitThread trampoline (one DWORD, no
    // return); the code is forwarded unchanged and this call never returns.
    let f: unsafe extern "system" fn(u32) = unsafe { core::mem::transmute(original) };
    // SAFETY: as above.
    unsafe { f(code) }
}

/// `_endthreadex` — the msvcrt counterpart of `ExitThread`, same leak.
///
/// # Safety
/// Only the caller's code is forwarded unchanged.
pub unsafe extern "system" fn endthreadex_detour(code: u32) {
    crate::gcthreads::note_thread_exit(crate::gcthreads::own_tid(), "_endthreadex", code);
    let original = END_THREAD_EX_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    // SAFETY: original is the _endthreadex trampoline (one DWORD, no return).
    let f: unsafe extern "system" fn(u32) = unsafe { core::mem::transmute(original) };
    // SAFETY: as above.
    unsafe { f(code) }
}

/// Install a detour on a named export of a loaded module.
fn install_named(
    module: &str,
    func: &str,
    original: &AtomicUsize,
    detour: usize,
    label: &str,
) -> bool {
    if original.load(Ordering::Acquire) != 0 {
        return true;
    }
    let Some(target) = try_resolve(module, func) else {
        log::log_tag("gct", &format!("{label}: {module}!{func} not found"));
        return false;
    };
    let Some(prep) = prepare(target) else {
        log::log_tag(
            "gct",
            &format!("{label}: prologue rejected at {module}!{func} ({target:#x})"),
        );
        return false;
    };
    // Publish before patching: a thread that enters in the gap calls the original.
    original.store(prep.trampoline, Ordering::Release);
    match patch(&prep, detour) {
        Some(h) => {
            register(h);
            log::log_tag(
                "gct",
                &format!("{label}: hooked {module}!{func} at {target:#x}"),
            );
            true
        }
        None => {
            original.store(0, Ordering::Release);
            log::log_tag("gct", &format!("{label}: patch failed at {target:#x}"));
            false
        }
    }
}

fn install_gc_trace_at(
    ga: usize,
    rva: u32,
    original: &AtomicUsize,
    detour: usize,
    label: &str,
) -> bool {
    if original.load(Ordering::Acquire) != 0 {
        return true;
    }
    let target = ga.wrapping_add(rva as usize);
    let Some(prep) = prepare(target) else {
        log::log_tag(
            "gct",
            &format!("worker trace: prologue rejected at {label} ({target:#x})"),
        );
        return false;
    };
    original.store(prep.trampoline, Ordering::Release);
    match patch(&prep, detour) {
        Some(h) => {
            register(h);
            log::log_tag(
                "gct",
                &format!("worker trace: {label} hooked at {target:#x}"),
            );
            true
        }
        None => {
            original.store(0, Ordering::Release);
            log::log_tag(
                "gct",
                &format!("worker trace: patch failed at {label} ({target:#x})"),
            );
            false
        }
    }
}

/// Install worker provenance hooks only when explicitly requested. These
/// diagnostic detours execute from the same plaintext image as LateUpdate.
///
/// The three targets are not exports: the dispatcher and the GC removal are
/// derived per build (descriptor, else `raw::discover`). When any of them is
/// unknown this refuses; patching a guessed address is worse than losing a
/// diagnostic.
pub fn install_gc_worker_trace() -> bool {
    let Some((ga, size)) = crate::procs::own_module(crate::GAME_ASSEMBLY) else {
        log::log_tag("gct", "worker trace disabled: GameAssembly is not loaded");
        return false;
    };
    let Some(runtime) = crate::buildinfo::runtime_anchors(ga, size) else {
        log::log_tag(
            "gct",
            "worker trace disabled: no runtime anchors for this GameAssembly build",
        );
        return false;
    };
    let Some(gc) = crate::buildinfo::gc_anchors(ga, size) else {
        log::log_tag("gct", "worker trace disabled: no GC anchors for this build");
        return false;
    };
    if runtime.worker_dispatch == 0 || runtime.gc_unregister == 0 {
        log::log_tag(
            "gct",
            "worker trace disabled: dispatcher/unregister were not derived for this build",
        );
        return false;
    }
    let dispatch = install_gc_trace_at(
        ga,
        runtime.worker_dispatch,
        &GC_DISPATCH_ORIG,
        gc_worker_detour as *const () as usize,
        "worker dispatcher",
    );
    let unregister = install_gc_trace_at(
        ga,
        runtime.gc_unregister,
        &GC_UNREGISTER_ORIG,
        gc_unregister_detour as *const () as usize,
        "GC unregister",
    );
    // The wrapper only proves that somebody asked; the removal itself proves the
    // record went away. Without it "no unregister line" cannot distinguish
    // "the body skipped the call" from "our hook sat on the wrong wrapper".
    let remove = install_gc_trace_at(
        ga,
        gc.remove_thread,
        &GC_REMOVE_ORIG,
        gc_remove_detour as *const () as usize,
        "GC remove thread",
    );
    // Last piece of the puzzle: a thread that leaves through ExitThread never
    // returns from its body, so its cleanup call cannot run. These are plain
    // kernel32/msvcrt exports, not GameAssembly code, so they get their own
    // installer; a missing one is not fatal.
    let exit_thread = install_named(
        "kernel32.dll",
        "ExitThread",
        &EXIT_THREAD_ORIG,
        exit_thread_detour as *const () as usize,
        "worker exit",
    );
    let end_thread_ex = install_named(
        "ucrtbase.dll",
        "_endthreadex",
        &END_THREAD_EX_ORIG,
        endthreadex_detour as *const () as usize,
        "worker exit",
    ) || install_named(
        "msvcrt.dll",
        "_endthreadex",
        &END_THREAD_EX_ORIG,
        endthreadex_detour as *const () as usize,
        "worker exit",
    );
    log::log_tag(
        "gct",
        &format!(
            "worker trace installed: dispatcher={dispatch}, unregister={unregister}, remove={remove}, ExitThread={exit_thread}, _endthreadex={end_thread_ex}"
        ),
    );
    // `remove` and `unregister` are the diagnostics that matter; exit hooks are a
    // bonus (wine may route exits through ntdll instead).
    dispatch && unregister && remove
}

// ---- concrete IL2CPP LateUpdate -------------------------------------------

static LATE_ORIG: AtomicUsize = AtomicUsize::new(0);
static LATE_ENTRY: AtomicUsize = AtomicUsize::new(0);
static LATE_ATTACHED: AtomicBool = AtomicBool::new(false);
static LATE_CLASS: AtomicUsize = AtomicUsize::new(0);
static LATE_INFO: AtomicUsize = AtomicUsize::new(0);
static LATE_HITS: AtomicU64 = AtomicU64::new(0);
static LATE_TID: AtomicU64 = AtomicU64::new(0);

fn reset_late_observation() {
    LATE_HITS.store(0, Ordering::Relaxed);
    LATE_TID.store(0, Ordering::Relaxed);
}

fn managed_entry_in_module(entry: usize, module: Option<(usize, usize)>) -> bool {
    let Some((base, size)) = module else {
        return false;
    };
    entry >= base && entry < base.saturating_add(size)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LateInstallAction {
    Patch,
    AlreadyAttached,
}

fn late_install_action(
    last_entry: usize,
    attached: bool,
    candidate: usize,
) -> Result<LateInstallAction, String> {
    if attached && last_entry == candidate {
        return Ok(LateInstallAction::AlreadyAttached);
    }
    if last_entry == 0 || (!attached && last_entry == candidate) {
        return Ok(LateInstallAction::Patch);
    }
    Err("another LateUpdate target is configured; restart before changing it".into())
}

fn tick_then_original<T, E>(
    original_first: bool,
    tick_fn: impl FnOnce() -> Result<(), E>,
    original: impl FnOnce() -> T,
) -> T {
    // Instrumentation is explicitly best-effort; the managed body is mandatory.
    // Reload transactions run it before mapping/init/shutdown; ordinary tick work
    // retains the measured pre-original ordering.
    if original_first {
        let result = original();
        let _ = tick_fn();
        return result;
    }
    let _ = tick_fn();
    original()
}

/// IL2CPP instance-method ABI: `this` in rcx and hidden `MethodInfo*` in rdx.
/// Our throttled work runs before this method's original body. This gives
/// priority over the selected script only: Unity still owns ordering between
/// different MonoBehaviours, so a LateUpdate hook cannot mean "before every
/// script in the game".
///
/// # Safety
/// Both arguments are forwarded unchanged to the trampoline for the exact
/// managed method selected by `install_late_update`.
pub unsafe extern "system" fn late_update_detour(
    this: *mut core::ffi::c_void,
    method: *mut core::ffi::c_void,
) {
    let original = LATE_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    // SAFETY: install_late_update publishes the trampoline for the same
    // two-argument IL2CPP method before patching the target.
    let f: unsafe extern "system" fn(*mut core::ffi::c_void, *mut core::ffi::c_void) =
        unsafe { core::mem::transmute(original) };
    tick_then_original(
        original_first_required(),
        || {
            // A payload fault must not skip the game's method. The guard is
            // inside the pre-original half, so the original call remains the
            // next operation even when instrumentation fails.
            log::guard("late_update_detour::pre", || {
                let hit = LATE_HITS.fetch_add(1, Ordering::Relaxed) + 1;
                let tid = crate::gcthreads::own_tid() as u64;
                LATE_TID.store(tid, Ordering::Relaxed);
                if hit == 1 {
                    log::log_tag(
                        "late",
                        &format!("first pre-original LateUpdate hit: tid={tid:#x}, this={this:?}, MethodInfo={method:?}"),
                    );
                }
                tick();
            })
            .map(|_| ())
            .ok_or(())
        },
        // SAFETY: the original arguments are opaque to us and are not modified.
        || unsafe { f(this, method) },
    );
}

/// Make a lazily-created class available to the startup installer. This calls
/// IL2CPP and is therefore only for the injection stub, never a managed hook.
pub fn materialise_late_class(selector: crate::lateupdate::Selector) -> bool {
    use crate::lateupdate::Selector;
    if crate::lateupdate::resolve(selector).is_some() {
        return true;
    }
    if let Selector::Auto = selector {
        static LOGGED: AtomicBool = AtomicBool::new(false);
        if !LOGGED.swap(true, Ordering::Relaxed) {
            log::log_tag(
                "late",
                &format!(
                    "auto target is exactly {}; no arbitrary fallback",
                    crate::lateupdate::AUTO_CLASS
                ),
            );
        }
    }
    let Some(corlib) = crate::il2cpp::find_domain() else {
        return false;
    };
    // A pinned type index describes one metadata build. When the game update
    // moved it, ask il2cpp by name — one call per image, and the image that owns
    // the class is the only one that answers.
    if matches!(selector, Selector::Auto)
        && crate::symbols::GAMEASSEMBLY_SHA256 != crate::lateupdate::AUTO_BUILD_SHA256
    {
        return materialise_by_name(corlib);
    }
    let Some(image) = crate::m3::image_of(corlib, "Assembly-CSharp") else {
        return false;
    };
    let Some(f) = crate::m3::image_get_class_addr() else {
        return false;
    };
    let index = match selector {
        Selector::Auto => crate::lateupdate::AUTO_TYPE_INDEX,
        Selector::Index(index) => index,
        Selector::Address(_) => return false,
    };
    // SAFETY: f is the build-measured il2cpp_image_get_class(image, index) and
    // is called only from the injection stub. The class may initialize lazily,
    // but no managed constructor is invoked here and VEH contains runtime faults.
    let class = unsafe { crate::veh::call2(f, image, index) }.unwrap_or(0);
    class >= 0x10000
        && crate::safe::readable_at(class)
        // In auto mode this re-checks the fully-qualified name. A changed type
        // index must never turn into a hook on an unrelated method.
        && crate::lateupdate::resolve(selector).is_some()
}

/// `il2cpp_class_from_name(image, namespace, name)` for the auto target.
///
/// The class belongs to an assembly the caller does not know (a namespace is not
/// an image name), so every image is asked until one answers. The two C strings
/// live in static storage: the runtime keeps the pointers it was given.
fn materialise_by_name(corlib_image: usize) -> bool {
    use std::sync::OnceLock;
    static NAMES: OnceLock<(Vec<u8>, Vec<u8>)> = OnceLock::new();
    let Some(function) = crate::m3::class_from_name_addr() else {
        return false;
    };
    let (namespace, name) = match crate::lateupdate::AUTO_CLASS.rsplit_once('.') {
        Some(parts) => parts,
        None => ("", crate::lateupdate::AUTO_CLASS),
    };
    let (namespace, name) = NAMES.get_or_init(|| {
        let mut ns = namespace.as_bytes().to_vec();
        ns.push(0);
        let mut class = name.as_bytes().to_vec();
        class.push(0);
        (ns, class)
    });
    for image in crate::m3::images(corlib_image, 512) {
        // SAFETY: f is the build-measured il2cpp_class_from_name; both pointers
        // are NUL-terminated and outlive the process.
        let class = unsafe {
            crate::veh::call3(
                function,
                image,
                namespace.as_ptr() as usize,
                name.as_ptr() as usize,
            )
        }
        .unwrap_or(0);
        if class >= 0x10000
            && crate::safe::readable_at(class)
            && crate::lateupdate::resolve(crate::lateupdate::Selector::Auto).is_some()
        {
            return true;
        }
    }
    false
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// First managed tick of this installation: arm the runtime's one-time setup
/// (command reader, console path). Safe to call again - the runtime is idempotent.
pub(crate) fn prepare_tick() {
    let _ = START_MS.compare_exchange(0, now_ms(), Ordering::AcqRel, Ordering::Relaxed);
    if let Some(target) = tick_target() {
        (target.prepare)();
    }
}

/// Resolve and hook one concrete class's own `LateUpdate` method.
pub fn install_late_update(
    selector: crate::lateupdate::Selector,
) -> Result<crate::lateupdate::Candidate, String> {
    if env_flag("HROMATITE_SEAL") || env_flag("HROMATITE_L2B") {
        return Err(
            "LateUpdate is an in-image detour; full sealing is not supported for it yet".into(),
        );
    }
    let candidate = crate::lateupdate::resolve(selector).ok_or_else(|| match selector {
        crate::lateupdate::Selector::Auto => format!(
            "auto target {} is absent/ambiguous or has no valid instance void LateUpdate()",
            crate::lateupdate::AUTO_CLASS
        ),
        _ => "class has no materialised own method named `LateUpdate`".to_string(),
    })?;
    let ga = crate::procs::own_module(crate::GAME_ASSEMBLY);
    if !managed_entry_in_module(candidate.entry, ga) {
        return Err(format!(
            "managed entry {:#x} is outside GameAssembly.dll",
            candidate.entry
        ));
    }
    if late_install_action(
        LATE_ENTRY.load(Ordering::Acquire),
        LATE_ATTACHED.load(Ordering::Acquire),
        candidate.entry,
    )? == LateInstallAction::AlreadyAttached
    {
        return Ok(candidate);
    }
    let prep = prepare(candidate.entry)
        .ok_or_else(|| format!("cannot prepare a trampoline at {:#x}", candidate.entry))?;
    prepare_tick();
    reset_late_observation();
    LATE_CLASS.store(candidate.class, Ordering::Relaxed);
    LATE_INFO.store(candidate.info, Ordering::Relaxed);
    LATE_ORIG.store(prep.trampoline, Ordering::Release);
    let installed = patch(&prep, late_update_detour as *const () as usize);
    let Some(hook) = installed else {
        LATE_ORIG.store(0, Ordering::Release);
        LATE_ATTACHED.store(false, Ordering::Release);
        LATE_CLASS.store(0, Ordering::Relaxed);
        LATE_INFO.store(0, Ordering::Relaxed);
        return Err("patch failed (prologue, atomic window or page protection)".into());
    };
    LATE_ENTRY.store(candidate.entry, Ordering::Release);
    LATE_ATTACHED.store(true, Ordering::Release);
    register(hook);
    let class_name = crate::m3::class_full_name(candidate.class);
    let rva = ga
        .map(|(base, _)| candidate.entry.saturating_sub(base))
        .unwrap_or(0);
    log::log_tag(
        "late",
        &format!(
            "hooked {class_name} ({:#x}) index={:?}, MethodInfo={:#x}, entry={:#x} (GA+{rva:#x})",
            candidate.class, candidate.index, candidate.info, candidate.entry
        ),
    );
    Ok(candidate)
}

fn late_attachment_present(entry: usize, hooks: &[(usize, bool)]) -> bool {
    entry != 0
        && hooks
            .iter()
            .any(|(target, detached)| *target == entry && !*detached)
}

pub fn late_update_status() -> Vec<String> {
    let entry = LATE_ENTRY.load(Ordering::Acquire);
    if entry == 0 {
        return vec!["LateUpdate: not hooked (use `latec`, then `latehook #index`)".into()];
    }
    if !LATE_ATTACHED.load(Ordering::Acquire) {
        return vec![format!(
            "LateUpdate: detached entry={entry:#x}; repeat `latehook` for the same target to reinstall"
        )];
    }
    let hits = LATE_HITS.load(Ordering::Relaxed);
    let uptime = now_ms().saturating_sub(START_MS.load(Ordering::Relaxed));
    let rate = if uptime == 0 {
        0.0
    } else {
        hits as f64 * 1000.0 / uptime as f64
    };
    let class = LATE_CLASS.load(Ordering::Relaxed);
    vec![format!(
        "LateUpdate: pre-original (selected method only; Unity script order not guaranteed), class {class:#x} {}, MethodInfo={:#x}, entry={entry:#x}, hits={hits} ({rate:.1}/s), last tid={:#x}",
        crate::m3::class_full_name(class),
        LATE_INFO.load(Ordering::Relaxed),
        LATE_TID.load(Ordering::Relaxed)
    )]
}

// `hookat` slot: one diagnostic hook at a time (a generic detour cannot tell
// which target it belongs to, and per-hook thunks are not worth the asm yet).
static HOOKAT_ORIG: AtomicUsize = AtomicUsize::new(0);
static HOOKAT_HITS: AtomicU64 = AtomicU64::new(0);
static HOOKAT_LOG: AtomicU64 = AtomicU64::new(0);

/// Four-integer detour used by `hookat`: safe for x64 callers whose arguments
/// are integers (rcx/rdx/r8/r9 and rax are preserved); float arguments
/// (xmm0..3) are clobbered, so this is a *diagnostic* hook, not a wrapper.
unsafe extern "system" fn count_detour(a: u64, b: u64, c: u64, d: u64) -> u64 {
    let orig = HOOKAT_ORIG.load(Ordering::Acquire);
    let ret = if orig != 0 {
        let f: unsafe extern "system" fn(u64, u64, u64, u64) -> u64 =
            // SAFETY: orig is the four-integer trampoline generated by install_at.
            unsafe { core::mem::transmute(orig) };
        // SAFETY: all four integer arguments are forwarded unchanged.
        unsafe { f(a, b, c, d) }
    } else {
        0
    };
    let n = HOOKAT_HITS.fetch_add(1, Ordering::Relaxed) + 1;
    if n <= HOOKAT_LOG.load(Ordering::Relaxed) {
        log::log_tag("hook", &format!("hookat hit {n}, ret {ret:#x}"));
    }
    tick();
    ret
}

// ---- install --------------------------------------------------------------

fn try_resolve(module: &str, func: &str) -> Option<usize> {
    let (_, _) = crate::procs::own_module(module)?;
    crate::raw::sys::export_address(module, func)
}

pub(crate) fn register(h: Hook) {
    let mut g = match HOOKS.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    g.push(h);
}

/// Install a counting hook at an arbitrary address (`hookat`).
fn install_at(addr: usize, log_first: u64) -> Option<usize> {
    if HOOKAT_ORIG.load(Ordering::Acquire) != 0 {
        return None;
    }
    let p = prepare(addr)?;
    HOOKAT_LOG.store(log_first.max(1), Ordering::Relaxed);
    HOOKAT_HITS.store(0, Ordering::Relaxed);
    HOOKAT_ORIG.store(p.trampoline, Ordering::Release);
    match patch(&p, count_detour as *const () as usize) {
        Some(h) => {
            register(h);
            Some(addr)
        }
        None => {
            HOOKAT_ORIG.store(0, Ordering::Release);
            None
        }
    }
}

/// True once a hook is installed and the game is ticking us.
pub fn installed() -> bool {
    match HOOKS.lock() {
        Ok(g) => !g.is_empty(),
        Err(p) => !p.into_inner().is_empty(),
    }
}

// ---------------------------------------------------------------------------
// Console commands (`hook` is not in console.rs' match — it lands here)
// ---------------------------------------------------------------------------

fn parse_addr(s: &str) -> Option<usize> {
    let t = s.trim().trim_start_matches("0x").trim_start_matches("0X");
    usize::from_str_radix(t, 16).ok()
}

/// Handle `hook …` / `tick` style commands. `None` = "not mine", so
/// `console.rs` reports an unknown command.
pub fn command(line: &str) -> Option<Vec<String>> {
    let mut it = line.splitn(3, char::is_whitespace);
    let cmd = it.next().unwrap_or("").to_ascii_lowercase();
    let a1 = it.next().unwrap_or("");
    let a2 = it.next().unwrap_or("");
    match cmd.as_str() {
        "latec" => {
            let found = crate::lateupdate::candidates(120_000);
            let ga = crate::procs::own_module(crate::GAME_ASSEMBLY);
            let mut out = vec![format!(
                "LateUpdate candidates: {} (materialised classes only)",
                found.len()
            )];
            for candidate in found.iter().take(128) {
                let index = candidate
                    .index
                    .map(|i| format!("#{i}"))
                    .unwrap_or_else(|| "-".into());
                let rva = ga
                    .map(|(base, _)| candidate.entry.saturating_sub(base))
                    .unwrap_or(0);
                out.push(format!(
                    "  {index:>10} class={:#x} {}, entry={:#x} (GA+{rva:#x})",
                    candidate.class,
                    crate::m3::class_full_name(candidate.class),
                    candidate.entry
                ));
            }
            if found.len() > 128 {
                out.push(format!("  … {} more", found.len() - 128));
            }
            Some(out)
        }
        "latehook" => {
            let Some(selector) = crate::lateupdate::parse_selector(a1) else {
                return Some(vec![
                    "usage: latehook auto | #<type-index> | <class-address>".into(),
                ]);
            };
            let result = log::guard("latehook", || install_late_update(selector))
                .unwrap_or_else(|| Err("installer panicked (see PANIC line)".into()));
            Some(vec![match result {
                Ok(candidate) => format!(
                    "LateUpdate hook installed at {:#x}; use `late` for hits/tid",
                    candidate.entry
                ),
                Err(error) => format!("latehook: {error}"),
            }])
        }
        "late" => Some(late_update_status()),
        "unhook" => {
            if !crate::raw::gcguard::before_unhook() {
                return Some(vec!["GC guard is busy; unhook refused, retry later".into()]);
            }
            let mut n = 0;
            let mut g = match HOOKS.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            for h in g.iter_mut() {
                if h.unhook() {
                    n += 1;
                }
            }
            let installed: Vec<(usize, bool)> =
                g.iter().map(|h| (h.target, h.is_detached())).collect();
            crate::raw::gcguard::after_unhook(&installed);
            LATE_ATTACHED.store(
                late_attachment_present(LATE_ENTRY.load(Ordering::Acquire), &installed),
                Ordering::Release,
            );
            crate::playerloop::refresh_attachment(&installed);
            Some(vec![format!(
                "{n} hook(s) detached ({} still listed)",
                g.len()
            )])
        }
        "hooks" => {
            let g = match HOOKS.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            if g.is_empty() {
                return Some(vec![
                    "no hooks installed (try `latehook auto`, or `hookat <addr> [n]`)".into(),
                ]);
            }
            let mut out = vec![format!("{} hook(s):", g.len())];
            for (i, h) in g.iter().enumerate() {
                out.push(format!("  [{i}] {}", h.describe()));
            }
            Some(out)
        }
        "hookat" => {
            let addr = match parse_addr(a1) {
                Some(a) => a,
                None => return Some(vec!["usage: hookat <hexaddr> [log-first-n]".into()]),
            };
            let n = a2.trim().parse::<u64>().unwrap_or(8);
            match log::guard("hook::install_at", || install_at(addr, n)).flatten() {
                Some(a) => Some(vec![format!(
                    "hookat: counting hook at {a:#x} (first {n} hits logged, integer args only)"
                )]),
                None => Some(vec![format!(
                    "hookat: {addr:#x} failed (not readable / unknown opcode / one slot busy — see [hook] lines)"
                )]),
            }
        }
        "tick" => {
            let n = TICKS.load(Ordering::Relaxed);
            let start = START_MS.load(Ordering::Relaxed);
            let up = now_ms().saturating_sub(start);
            let rate = if up > 0 {
                n as f64 * 1000.0 / up as f64
            } else {
                0.0
            };
            Some(vec![format!(
                "tick {n}, uptime {up}ms ({rate:.1}/s), interval {}ms, hooks={}, busy={}",
                tick_target().map(|t| (t.interval_ms)()).unwrap_or(0),
                match HOOKS.lock() {
                    Ok(g) => g.len(),
                    Err(p) => p.into_inner().len(),
                },
                BUSY.load(Ordering::Relaxed),
            )])
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raw::gcguard::{
        GcRecordAction, can_recover_boehm_abort, gc_record_action, valid_gc_suspend_entry,
    };

    #[test]
    fn detached_history_does_not_mask_a_still_attached_late_update() {
        let entry = 0x1000;
        assert!(late_attachment_present(
            entry,
            &[(entry, true), (entry, false)]
        ));
        assert!(!late_attachment_present(entry, &[(entry, true)]));
        assert!(!late_attachment_present(entry, &[(0x2000, false)]));
    }

    #[test]
    fn late_update_lifecycle_distinguishes_attached_from_retained_trampoline() {
        use LateInstallAction::{AlreadyAttached, Patch};
        assert_eq!(late_install_action(0, false, 0x1000), Ok(Patch));
        assert_eq!(
            late_install_action(0x1000, true, 0x1000),
            Ok(AlreadyAttached)
        );
        assert_eq!(late_install_action(0x1000, false, 0x1000), Ok(Patch));
        assert!(late_install_action(0x1000, true, 0x2000).is_err());
        assert!(late_install_action(0x1000, false, 0x2000).is_err());
    }

    #[test]
    fn late_update_rearm_clears_hits_and_thread_identity() {
        LATE_HITS.store(7, Ordering::Relaxed);
        LATE_TID.store(0x1234, Ordering::Relaxed);
        reset_late_observation();
        assert_eq!(LATE_HITS.load(Ordering::Relaxed), 0);
        assert_eq!(LATE_TID.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn late_update_tick_runs_before_the_original_method() {
        let events = std::cell::RefCell::new(Vec::new());
        let returned = tick_then_original(
            false,
            || {
                events.borrow_mut().push("tick");
                Ok::<_, ()>(())
            },
            || {
                events.borrow_mut().push("original");
                42
            },
        );
        assert_eq!(returned, 42);
        assert_eq!(events.into_inner(), ["tick", "original"]);
    }

    #[test]
    fn reload_phase_runs_the_original_before_instrumentation() {
        let events = std::cell::RefCell::new(Vec::new());
        let returned = tick_then_original(
            true,
            || {
                events.borrow_mut().push("instrumentation");
                Err::<(), _>("feature delay")
            },
            || {
                events.borrow_mut().push("original");
                64usize
            },
        );
        assert_eq!(returned, 64);
        assert_eq!(events.into_inner(), ["original", "instrumentation"]);
    }

    #[test]
    fn late_update_original_survives_a_guarded_tick_failure() {
        let events = std::cell::RefCell::new(Vec::new());
        let returned = tick_then_original(
            false,
            || {
                events.borrow_mut().push("guard failed");
                Err::<(), _>("instrumentation fault")
            },
            || {
                events.borrow_mut().push("original");
                42
            },
        );
        assert_eq!(returned, 42);
        assert_eq!(events.into_inner(), ["guard failed", "original"]);
    }

    #[test]
    fn late_update_entry_is_limited_to_game_assembly() {
        let module = (0x6fff_0000, 0x0f44_0000);
        assert!(managed_entry_in_module(0x6fff_1000, Some(module)));
        assert!(managed_entry_in_module(
            0x6fff_0000 + 0x0f44_0000 - 1,
            Some(module)
        ));
        assert!(!managed_entry_in_module(
            0x6fff_0000 + 0x0f44_0000,
            Some(module)
        ));
        assert!(!managed_entry_in_module(0x1000, Some(module)));
        assert!(!managed_entry_in_module(0x6fff_1000, None));
    }

    #[test]
    fn gc_context_call_site_requires_the_measured_boehm_signature() {
        const CALL_AND_TEST: [u8; 8] = [0xff, 0x15, 0x60, 0x78, 0xac, 0x0a, 0x85, 0xc0];
        const CONTINUATION: [u8; 6] = [0x0f, 0x85, 0x8b, 0x00, 0x00, 0x00];
        let mut loaded = [0u8; 14];
        loaded[..8].copy_from_slice(&CALL_AND_TEST);
        loaded[8..].copy_from_slice(&CONTINUATION);

        assert!(valid_gc_context_call_site(&loaded));
        assert_eq!(prologue_len(&loaded, 14), Some(14));
        for index in [0usize, 1, 6, 7] {
            let mut changed = loaded;
            changed[index] ^= 1;
            assert!(
                !valid_gc_context_call_site(&changed),
                "byte {index} belongs to the version gate"
            );
        }
        assert!(!valid_gc_context_call_site(&loaded[..7]));
    }

    #[test]
    fn synthetic_dead_context_matches_win64_and_preserves_requested_flags() {
        const CONTEXT_CONTROL: u32 = 0x0010_0001;
        const CONTEXT_INTEGER: u32 = 0x0010_0002;
        const FLAGS: u32 = CONTEXT_CONTROL | CONTEXT_INTEGER;
        let mut storage = [0xa5u8; WIN64_CONTEXT_SIZE + 2];
        storage[1 + WIN64_CONTEXT_FLAGS..1 + WIN64_CONTEXT_FLAGS + 4]
            .copy_from_slice(&FLAGS.to_le_bytes());
        // SAFETY: storage keeps one extra byte on each side of the WINDOW.
        let context_ptr = unsafe { storage.as_mut_ptr().add(1) };
        // SAFETY: storage owns exactly one complete writable Win64 CONTEXT,
        // deliberately offset by one byte to exercise unaligned raw accesses.
        unsafe { write_synthetic_dead_context_raw(context_ptr) };

        assert_eq!(storage[0], 0xa5, "left canary");
        assert_eq!(storage[WIN64_CONTEXT_SIZE + 1], 0xa5, "right canary");
        // SAFETY: the writer touched only this exact live 0x4d0-byte extent.
        let context = unsafe { core::slice::from_raw_parts(context_ptr, WIN64_CONTEXT_SIZE) };
        assert_eq!(context.len(), 0x4d0, "Win64 CONTEXT is 1232 bytes");
        assert_eq!(
            u32::from_le_bytes(context[0x30..0x34].try_into().unwrap()),
            FLAGS,
            "ContextFlags is an input selecting the registers to retrieve"
        );
        assert!(context[..0x30].iter().all(|b| *b == 0));
        assert!(context[0x34..].iter().all(|b| *b == 0));
    }

    #[test]
    fn inline_repairs_are_deferred_and_coalesced_for_the_tick_logger() {
        INLINE_REPAIRS.store(0, Ordering::Relaxed);
        INLINE_LAST_TID.store(0, Ordering::Relaxed);

        note_inline_repair(0x1234);
        note_inline_repair(0x5678);
        assert_eq!(take_inline_repairs(), Some((2, 0x5678)));
        assert_eq!(take_inline_repairs(), None);
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct TestGcRecord {
        next: usize,
        id: u32,
        pad: u32,
        handle: usize,
    }

    impl TestGcRecord {
        fn new(id: u32, handle: usize) -> Self {
            Self {
                next: 0,
                id,
                pad: 0,
                handle,
            }
        }
    }

    #[test]
    fn raw_gc_unlink_removes_a_nested_record_without_disturbing_its_neighbours() {
        let mut table = [0usize; 256];
        let mut records = [
            TestGcRecord::new(0x111, 0x1010),
            TestGcRecord::new(0x222, 0x2020),
            TestGcRecord::new(0x333, 0x3030),
        ];
        let third = &mut records[2] as *mut TestGcRecord as usize;
        let second = &mut records[1] as *mut TestGcRecord as usize;
        records[0].next = second;
        records[1].next = third;
        table[37] = &mut records[0] as *mut TestGcRecord as usize;

        // SAFETY: table and all linked records are exclusively owned fixtures
        // with the measured ABI layout; every pointer remains live and writable.
        let removed = unsafe { unlink_by_handle_raw(table.as_mut_ptr() as usize, 0x2020) };

        assert_eq!(removed, Some(0x222));
        assert_eq!(records[0].next, third);
        assert_eq!(
            records[1].next, third,
            "the removed record's links stay inert"
        );
        assert_eq!(table[37], &mut records[0] as *mut TestGcRecord as usize);
    }

    #[test]
    fn raw_gc_unlink_is_bounded_when_a_bucket_contains_a_cycle() {
        let mut table = [0usize; 256];
        let mut record = TestGcRecord::new(0x111, 0x1010);
        let ptr = &mut record as *mut TestGcRecord as usize;
        record.next = ptr;
        table[0] = ptr;

        // SAFETY: the cycle is intentional and lives entirely in this fixture;
        // the helper must stop at its fixed per-bucket bound instead of hanging.
        let removed = unsafe { unlink_by_handle_raw(table.as_mut_ptr() as usize, 0x9990) };
        assert_eq!(removed, None);
        assert_eq!(record.next, ptr);
        assert_eq!(table[0], ptr);
    }

    #[test]
    fn dead_gc_call_succeeds_even_when_the_record_is_already_absent() {
        let mut table = [0usize; 256];
        let mut context = [0xa5u8; WIN64_CONTEXT_SIZE];
        context[WIN64_CONTEXT_FLAGS..WIN64_CONTEXT_FLAGS + 4]
            .copy_from_slice(&0x0010_0003u32.to_le_bytes());

        // SAFETY: this is exactly the hot-path completion after a signalled
        // handle was observed. The empty fixture models a concurrent detach:
        // success is mandatory; absence must not re-enter the original retry.
        let (result, removed) = unsafe {
            complete_dead_gc_call_raw(table.as_mut_ptr() as usize, 0x2020, context.as_mut_ptr())
        };

        assert_eq!(result, 1);
        assert_eq!(removed, None);
        assert_eq!(
            u32::from_le_bytes(context[0x30..0x34].try_into().unwrap()),
            0x0010_0003
        );
        assert!(context[0x34..].iter().all(|b| *b == 0));
    }

    #[test]
    fn gc_record_action_unlinks_only_on_the_original_signalled_handle() {
        assert_eq!(gc_record_action(0), GcRecordAction::Unlink);
        assert_eq!(gc_record_action(0x102), GcRecordAction::Keep);
        assert_eq!(gc_record_action(u32::MAX), GcRecordAction::Keep);
        assert_eq!(gc_record_action(0x80), GcRecordAction::Keep);
    }

    #[test]
    fn entry_signature_matches_the_measured_boehm_prologue() {
        assert!(valid_gc_suspend_entry(&[
            0x48, 0x89, 0x5c, 0x24, 0x08, 0x48, 0x89, 0x6c, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24,
            0x18,
        ]));
    }

    #[test]
    fn recovery_access_and_activation_are_explicit() {
        assert!(!can_recover_boehm_abort(false, 0x123, 0x123));
        assert!(!can_recover_boehm_abort(true, 0x123, 0x456));
        assert!(!can_recover_boehm_abort(true, 0, 0x123));
        assert!(can_recover_boehm_abort(true, 0x123, 0x123));
    }
}
