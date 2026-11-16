//! Boehm GC thread-lifetime recovery with ownership-preserving cleanup.
//!
//! GC_remove_thread normally does CloseHandle -> unlink -> free. Two precise
//! detours make it unlink -> CloseHandle -> free instead. The entry collector
//! also removes already-terminated records through that same original function,
//! so it never leaks raw records or handles. The original no-argument suspend
//! pass runs under Boehm's own thread-list lock, which is recursive by design.

use crate::raw::hook::{Prep, patch, prepare, register};
use crate::{ffi, log, procs};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

const GC_REMOVE_THREAD_RVA: usize = 0xc417b0;
const GC_SKIP_CLOSE_RVA: usize = 0xc417f0;
const GC_AFTER_UNLINK_RVA: usize = 0xc41818;
const GC_UNLINKED_NEXT_RVA: usize = 0xc417f6;
const GC_SUSPEND_ENTRY_RVA: usize = 0xc41a20;
const GC_RESUME_ABORT_RVA: usize = 0xc41afb;
const GC_SUSPEND_ABORT_RVA: usize = 0xc41b65;
const GC_PROCESS_RECORD_RVA: usize = 0xc41a90;
const GC_ADVANCE_BUCKET_RVA: usize = 0xc41c6a;
const GC_THREADS_RVA: usize = 0x0e6b_b180;
const THREAD_LIST_CS_RVA: usize = 0x0e6b_b100;
const WIN64_CONTEXT_RBX: usize = 0x90;
const WIN64_CONTEXT_RIP: usize = 0xf8;

const SKIP_CLOSE_SIGNATURE: [u8; 6] = [0xff, 0x15, 0x92, 0x78, 0xac, 0x0a];
const AFTER_UNLINK_SIGNATURE: [u8; 7] = [0x48, 0x8d, 0x05, 0x41, 0xaf, 0xa2, 0x0d];
const SUSPEND_ENTRY_SIGNATURE: [u8; 15] = [
    0x48, 0x89, 0x5c, 0x24, 0x08, 0x48, 0x89, 0x6c, 0x24, 0x10, 0x48, 0x89, 0x74, 0x24, 0x18,
];

pub(crate) fn valid_gc_suspend_entry(bytes: &[u8]) -> bool {
    bytes == SUSPEND_ENTRY_SIGNATURE
}

static ENTRY_ORIG: AtomicUsize = AtomicUsize::new(0);
static SKIP_ORIG: AtomicUsize = AtomicUsize::new(0);
static AFTER_ORIG: AtomicUsize = AtomicUsize::new(0);
static ENTRY_PREP: AtomicUsize = AtomicUsize::new(0);
static ENTRY_ATTACHED: AtomicBool = AtomicBool::new(false);
static SKIP_ATTACHED: AtomicBool = AtomicBool::new(false);
static AFTER_ATTACHED: AtomicBool = AtomicBool::new(false);
static SKIP_PREP: AtomicUsize = AtomicUsize::new(0);
static AFTER_PREP: AtomicUsize = AtomicUsize::new(0);
static MODULE_BASE: AtomicUsize = AtomicUsize::new(0);
static THREADS_BASE: AtomicUsize = AtomicUsize::new(0);
static THREAD_LIST_CS: AtomicUsize = AtomicUsize::new(0);
static READY: AtomicBool = AtomicBool::new(false);
static DETACHING: AtomicBool = AtomicBool::new(false);
static ENTRY_ACTIVE: AtomicBool = AtomicBool::new(false);
static ENTRY_OWNER_TID: AtomicU32 = AtomicU32::new(0);
static INFLIGHT: AtomicUsize = AtomicUsize::new(0);
static REMOVED: AtomicU64 = AtomicU64::new(0);
static RECOVERED: AtomicU64 = AtomicU64::new(0);
static LAST_REMOVED: AtomicU32 = AtomicU32::new(0);
static LAST_RECOVERED: AtomicU32 = AtomicU32::new(0);

struct Flight;
impl Flight {
    fn new() -> Self {
        INFLIGHT.fetch_add(1, Ordering::AcqRel);
        Self
    }
}
impl Drop for Flight {
    fn drop(&mut self) {
        INFLIGHT.fetch_sub(1, Ordering::AcqRel);
    }
}

fn close_decisions_pending() -> bool {
    let book = match CLOSE_DECISIONS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    book.pending()
}

fn quiescent() -> bool {
    for _ in 0..100_000 {
        // A finished trampoline does not imply a finished removal. The first
        // callsite may already store its choice while the second has not fired.
        if INFLIGHT.load(Ordering::Acquire) == 0 && !close_decisions_pending() {
            return true;
        }
        std::thread::yield_now();
    }
    false
}

/// Block future guarded work and wait until every in-flight detour has left.
/// Origins remain published during the wait so old calls preserve their ABI.
pub fn before_unhook() -> bool {
    let was_detaching = DETACHING.swap(true, Ordering::AcqRel);
    let was_ready = READY.swap(false, Ordering::AcqRel);
    if quiescent() {
        true
    } else {
        DETACHING.store(was_detaching, Ordering::Release);
        READY.store(was_ready, Ordering::Release);
        false
    }
}

/// Apply each GC target's actual detach result, not one all-or-nothing flag.
/// DETACHING stays set while any GC target remains attached so a later partial
/// reinstall cannot silently promise a complete guard or drop pending decisions.
pub fn after_unhook(hooks: &[(usize, bool)]) {
    let ga = MODULE_BASE.load(Ordering::Acquire);
    let attached = |target: usize| ga != 0 && hooks.iter().any(|(candidate, detached)| *candidate == target && !*detached);
    ENTRY_ATTACHED.store(attached(ga + GC_SUSPEND_ENTRY_RVA), Ordering::Release);
    SKIP_ATTACHED.store(attached(ga + GC_SKIP_CLOSE_RVA), Ordering::Release);
    AFTER_ATTACHED.store(attached(ga + GC_AFTER_UNLINK_RVA), Ordering::Release);
    // Latch detachment until install() completes every GC target again. A late
    // pre-emitted detour therefore cannot admit a new decision after detach.
    DETACHING.store(true, Ordering::Release);
    READY.store(false, Ordering::Release);
    ENTRY_ACTIVE.store(false, Ordering::Release);
    ENTRY_OWNER_TID.store(0, Ordering::Release);
}

pub(crate) fn can_recover_boehm_abort(active: bool, owner: u32, current: u32) -> bool {
    // An admitted collection must remain recoverable while unhook waits for
    // other flights. READY gates only newly admitted instrumentation.
    active && owner != 0 && owner == current
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GcRecordAction {
    Keep,
    Unlink,
}

pub(crate) fn gc_record_action(wait: u32) -> GcRecordAction {
    if wait == 0 {
        GcRecordAction::Unlink
    } else {
        GcRecordAction::Keep
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CloseChoice {
    Original,
    Skip,
}

#[derive(Clone, Copy)]
struct CloseDecision {
    record: usize,
    choice: CloseChoice,
}

impl CloseDecision {
    const EMPTY: CloseDecision = CloseDecision {
        record: 0,
        choice: CloseChoice::Original,
    };
}

struct DecisionBook {
    entries: [CloseDecision; 64],
    next: usize,
}

impl DecisionBook {
    const fn new() -> Self {
        Self {
            entries: [CloseDecision::EMPTY; 64],
            next: 0,
        }
    }

    fn begin(&mut self, record: usize, relocated: bool) -> CloseChoice {
        let choice = if relocated {
            CloseChoice::Skip
        } else {
            CloseChoice::Original
        };
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.record == record) {
            entry.choice = choice;
            return choice;
        }
        let index = match self.entries.iter().position(|entry| entry.record == 0) {
            Some(index) => index,
            None => {
                let index = self.next % self.entries.len();
                self.next = self.next.wrapping_add(1);
                index
            }
        };
        self.entries[index] = CloseDecision { record, choice };
        choice
    }

    fn pending(&self) -> bool {
        self.entries.iter().any(|entry| entry.record != 0)
    }

    fn finish(&mut self, record: usize) -> CloseChoice {
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.record == record) {
            entry.record = 0;
            return entry.choice;
        }
        CloseChoice::Original
    }
}

static CLOSE_DECISIONS: std::sync::Mutex<DecisionBook> =
    std::sync::Mutex::new(DecisionBook::new());

fn begin_close(record: usize) -> CloseChoice {
    if DETACHING.load(Ordering::Acquire) || !READY.load(Ordering::Acquire) {
        // Only the relocated Skip case has cleanup to remember. Original follows
        // Boehm's own close path, needs no bookkeeping and cannot become stale.
        return CloseChoice::Original;
    }
    let relocated = READY.load(Ordering::Acquire);
    let mut book = match CLOSE_DECISIONS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    book.begin(record, relocated)
}

fn finish_close(record: usize) -> CloseChoice {
    let mut book = match CLOSE_DECISIONS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    book.finish(record)
}

unsafe fn terminate_record_by_tid(id: u32) {
    let ga = MODULE_BASE.load(Ordering::Acquire);
    if ga == 0 {
        return;
    }
    // SAFETY: id was read from a record currently linked under Boehm's list
    // lock. GC_remove_thread(tid) owns close+unlink+free for this exact build.
    let f: unsafe extern "system" fn(u32) =
        unsafe { core::mem::transmute(ga + GC_REMOVE_THREAD_RVA) };
    // SAFETY: id belongs to this exact linked record under the list lock.
    unsafe { f(id) };
}

unsafe fn original_wait(handle: usize) -> u32 {
    #[cfg(target_os = "windows")]
    // SAFETY: the handle is from the current linked record and Boehm is locked.
    return unsafe { ffi::WaitForSingleObject(handle as ffi::HANDLE, 0) };
    #[cfg(not(target_os = "windows"))]
    {
        let _ = handle;
        0x102
    }
}

unsafe fn sanitize_locked() {
    let base = THREADS_BASE.load(Ordering::Acquire);
    if base == 0 {
        return;
    }
    for bucket in 0..256usize {
        // SAFETY: base is the build-measured GC_threads table under its own lock.
        let link = base.wrapping_add(bucket * core::mem::size_of::<usize>());
        // SAFETY: link is a complete bucket pointer. Cross-bucket removal by tid
        // is deliberate here: this iteration validates only linked memory.
        let mut current = unsafe { core::ptr::read_unaligned(link as *const usize) };
        for _ in 0..8 {
            if current == 0 {
                break;
            }
            // SAFETY: the list lock prevents concurrent mutation while scalar
            // fields and the next link are read from this live record.
            let (next, id, handle, stack, blocked) = unsafe {
                (
                    core::ptr::read_unaligned(current as *const usize),
                    core::ptr::read_unaligned(current.wrapping_add(8) as *const u32),
                    core::ptr::read_unaligned(current.wrapping_add(0x10) as *const usize),
                    core::ptr::read_unaligned(current.wrapping_add(0x18) as *const usize),
                    core::ptr::read_unaligned(current.wrapping_add(0x28) as *const usize),
                )
            };
            if id != 0 && stack != 0 && blocked == 0 && handle != 0 && handle < usize::MAX - 1 {
                // SAFETY: this exact object is queried only until it returns a
                // signalled state. Invalid/unknown objects stay linked.
                let wait = unsafe { original_wait(handle) };
                if gc_record_action(wait) == GcRecordAction::Unlink {
                    // SAFETY: GC_remove_thread performs all cleanup itself. Do not
                    // continue walking this chain after it has freed a record.
                    unsafe { terminate_record_by_tid(id) };
                    LAST_REMOVED.store(id, Ordering::Relaxed);
                    REMOVED.fetch_add(1, Ordering::Release);
                    break;
                }
            }
            current = next;
        }
    }
}

/// Entry detour for the no-argument GC_suspend_all. Sanitization and the
/// original pass share the same outmost critical-section ownership.
///
/// # Safety
/// ENTRY_ORIG must be a trampoline for this exact build's GC_suspend_all, and
/// THREAD_LIST_CS its measured Boehm thread-list critical section.
unsafe extern "system" fn suspend_entry_detour() {
    let _flight = Flight::new();
    let original = ENTRY_ORIG.load(Ordering::Acquire);
    if original == 0 {
        return;
    }
    // SAFETY: original is a validated no-argument trampoline, with no altered
    // argument state on the fast path.
    let f: unsafe extern "system" fn() = unsafe { core::mem::transmute(original) };
    if !READY.load(Ordering::Acquire) {
        // SAFETY: simply execute the uninstrumented original when not active.
        unsafe { f() };
        return;
    }
    let cs = THREAD_LIST_CS.load(Ordering::Acquire);
    if cs == 0 {
        // SAFETY: optional instrumentation never suppresses Boehm itself.
        unsafe { f() };
        return;
    }
    // SAFETY: cs is a real CRITICAL_SECTION from GameAssembly; Windows permits
    // recursive ownership on its collector thread. Lifecycle waits for Flight.
    unsafe { ffi::EnterCriticalSection(cs as *mut core::ffi::c_void) };
    let was_active = ENTRY_ACTIVE.swap(true, Ordering::AcqRel);
    if !was_active {
        ENTRY_OWNER_TID.store(crate::gcthreads::own_tid(), Ordering::Release);
    }
    // SAFETY: the recursive outmost lock serializes all record lifetime work.
    unsafe { sanitize_locked() };
    // SAFETY: identical no-argument original trampoline, now after sanitization.
    unsafe { f() };
    ENTRY_ACTIVE.store(was_active, Ordering::Release);
    if !was_active {
        ENTRY_OWNER_TID.store(0, Ordering::Release);
    }
    // SAFETY: same lock and same owning thread as EnterCriticalSection.
    unsafe { ffi::LeaveCriticalSection(cs as *mut core::ffi::c_void) };
}

/// Route skip-CloseHandle. With READY set, skip directly to the original next
/// instruction after the call, then let AFTER_UNLINK close the handle instead.
///
/// # Safety
/// `handle` is RCX from the exact c417f0 call site, forwarded unchanged to the
/// original fallback trampoline when protection is not fully installed.
unsafe fn skip_close_route(_handle: ffi::HANDLE, record: usize) -> usize {
    let _flight = Flight::new();
    let ga = MODULE_BASE.load(Ordering::Acquire);
    if begin_close(record) == CloseChoice::Skip {
        return ga + GC_UNLINKED_NEXT_RVA;
    }
    let original = SKIP_ORIG.load(Ordering::Acquire);
    if original != 0 {
        return original;
    }
    #[cfg(target_os = "windows")]
    // SAFETY: partial-install fallback must preserve the original close and
    // still continue at c417f6 rather than return to GC_remove_thread's caller.
    unsafe { ffi::CloseHandle(_handle) };
    ga + GC_UNLINKED_NEXT_RVA
}

/// Naked continuation is mandatory: replacing a CALL with a JMP supplies no
/// post-call return address, so a normal Rust `return` would skip unlink/free.
///
/// # Safety
/// Only for the validated six-byte CloseHandle call-site patch.
#[unsafe(naked)]
unsafe extern "system" fn defer_close_detour() -> ! {
    core::arch::naked_asm!(
        "sub rsp, 0x30",
        "mov [rsp + 0x20], rcx",
        "mov [rsp + 0x28], rdx",
        "mov rdx, rbx",
        "call {route}",
        "mov rcx, [rsp + 0x20]",
        "mov rdx, [rsp + 0x28]",
        "add rsp, 0x30",
        "mov r11, rax",
        "jmp r11",
        route = sym skip_close_route,
    );
}

/// Close the just-unlinked record's handle here: after unlink, before GC_free.
/// RCX is not trusted at this site; the current record is still in RBX.
///
/// # Safety
/// The route runs only at c41818 in GC_remove_thread with the measured RBX
/// record layout, while Boehm's thread-list lock excludes other removals.
unsafe fn close_after_unlink_route(record: usize) -> usize {
    let _flight = Flight::new();
    // The decision was captured at THIS removal's first detour. Later activation
    // changes cannot turn it into a duplicate close or a skipped cleanup.
    if finish_close(record) == CloseChoice::Skip {
        // SAFETY: RBX was passed over this exact callsite's own ABI and points to
        // the removed record. +0x10 is its handle; this close is after unlink.
        let handle = unsafe { core::ptr::read_unaligned((record + 0x10) as *const usize) };
        // SAFETY: invoke the same Win32 close for the original duplicated handle.
        unsafe { ffi::CloseHandle(handle as ffi::HANDLE) };
    }
    AFTER_ORIG.load(Ordering::Acquire)
}

/// Naked wrapper because this is another call-site replacement.
///
/// # Safety
/// Only for the validated seven-byte prologue at c41818.
#[unsafe(naked)]
unsafe extern "system" fn close_after_unlink_detour() -> ! {
    core::arch::naked_asm!(
        "sub rsp, 0x30",
         "mov [rsp + 0x20], rcx",
         "mov rcx, rbx",
         "call {route}",
         "mov rcx, [rsp + 0x20]",
         "add rsp, 0x30",
         "mov r11, rax",
         "jmp r11",
         route = sym close_after_unlink_route,
    );
}

struct LinkedRecord {
    record: usize,
    id: u32,
    handle: usize,
    stack: usize,
    blocked: usize,
}

unsafe fn linked_by_pointer(base: usize, candidate: usize) -> Option<LinkedRecord> {
    if candidate == 0 {
        return None;
    }
    for bucket in 0..256usize {
        // SAFETY: base is the build-measured GC_threads table under list lock.
        let link = base.wrapping_add(bucket * core::mem::size_of::<usize>());
        // SAFETY: link is a complete bucket pointer in that table.
        let mut current = unsafe { core::ptr::read_unaligned(link as *const usize) };
        for _ in 0..8 {
            if current == 0 {
                break;
            }
            // SAFETY: Boehm is locked, so current remains allocated while we
            // compare it against RBX and, only on equality, read its fields.
            let next = unsafe { core::ptr::read_unaligned(current as *const usize) };
            if current == candidate {
                // SAFETY: pointer equality now proves a live linked record.
                let (id, handle, stack, blocked) = unsafe {
                    (
                        core::ptr::read_unaligned(current.wrapping_add(8) as *const u32),
                        core::ptr::read_unaligned(current.wrapping_add(0x10) as *const usize),
                        core::ptr::read_unaligned(current.wrapping_add(0x18) as *const usize),
                        core::ptr::read_unaligned(current.wrapping_add(0x28) as *const usize),
                    )
                };
                return Some(LinkedRecord {
                    record: current,
                    id,
                    handle,
                    stack,
                    blocked,
                });
            }
            current = next;
        }
    }
    None
}

fn next_after_abort(ga: usize, next: usize) -> (usize, usize) {
    if next != 0 {
        // Skip the removed record's "mov rbx,[rbx]" entirely: the saved next
        // pointer is inserted into RBX before the ordinary per-record checks.
        (next, ga + GC_PROCESS_RECORD_RVA)
    } else {
        // No list node remains: advance the bucket index, with no stale RBX read.
        (0, ga + GC_ADVANCE_BUCKET_RVA)
    }
}

fn abort_address(ga: usize, code: u32, address: usize) -> bool {
    ga != 0 && code == 0x8000_0003 && (address == ga + GC_RESUME_ABORT_RVA || address == ga + GC_SUSPEND_ABORT_RVA)
}

/// Last-resort recovery of an exact Boehm fatal, while the owner thread still
/// holds the record-list lock. Cleanup is delegated to GC_remove_thread, then
/// execution resumes using the next pointer saved before that cleanup.
///
/// # Safety
/// `info` must be a live OS EXCEPTION_POINTERS from the vectored callback. All
/// addresses, GC records and CONTEXT fields are gated on this exact game build.
pub unsafe fn recover_boehm_abort(info: *mut ffi::EXCEPTION_POINTERS) -> bool {
    let _flight = Flight::new();
    if !can_recover_boehm_abort(
        ENTRY_ACTIVE.load(Ordering::Acquire),
        ENTRY_OWNER_TID.load(Ordering::Acquire),
        crate::gcthreads::own_tid(),
    ) || info.is_null()
    {
        return false;
    }
    // SAFETY: info is OS-owned for this callback and was checked non-null.
    let exception = unsafe { (*info).ExceptionRecord };
    if exception.is_null() {
        return false;
    }
    // SAFETY: exception is the checked OS-owned record for this invocation.
    let (code, address) = unsafe { ((*exception).ExceptionCode, (*exception).ExceptionAddress) };
    let ga = MODULE_BASE.load(Ordering::Acquire);
    if !abort_address(ga, code, address) {
        return false;
    }
    // SAFETY: the OS supplies a complete Win64 CONTEXT with this callback.
    let context = unsafe { (*info).ContextRecord as *mut u8 };
    if context.is_null() {
        return false;
    }
    // SAFETY: RBX offset is the mingw-measured 0x90 field in that CONTEXT.
    let candidate = unsafe { core::ptr::read_unaligned(context.add(WIN64_CONTEXT_RBX).cast::<usize>()) };
    let base = THREADS_BASE.load(Ordering::Acquire);
    // SAFETY: the active owner holds Boehm's lock, so pointer validation cannot
    // race a record removal/free. A stale RBX is not dereferenced on a miss.
    let Some(record) = (unsafe { linked_by_pointer(base, candidate) }) else {
        return false;
    };
    if record.id == 0 || record.stack == 0 || record.blocked != 0 || record.handle == 0 {
        return false;
    }
    // SAFETY: handle belongs to the linked record and this is a zero-timeout wait.
    let wait = unsafe { original_wait(record.handle) };
    if gc_record_action(wait) != GcRecordAction::Unlink {
        return false;
    }
    // SAFETY: next is captured while record is still allocated and the active
    // owner holds Boehm's list lock, not on some later unsynchronized read.
    let next = unsafe { core::ptr::read_unaligned(record.record as *const usize) };
    // SAFETY: GC_remove_thread(tid) now uses the corrected unlink/close/free path.
    unsafe { terminate_record_by_tid(record.id) };
    let (saved_rbx, resume) = next_after_abort(ga, next);
    // SAFETY: CONTEXT fields at +0x90/+0xf8 are writable Win64 RBX/RIP slots.
    unsafe {
        core::ptr::write_unaligned(context.add(WIN64_CONTEXT_RBX).cast::<usize>(), saved_rbx);
        core::ptr::write_unaligned(context.add(WIN64_CONTEXT_RIP).cast::<usize>(), resume);
    }
    LAST_RECOVERED.store(record.id, Ordering::Relaxed);
    RECOVERED.fetch_add(1, Ordering::Release);
    true
}

pub fn report_lines() -> Vec<String> {
    let mut lines = Vec::new();
    let removed = REMOVED.swap(0, Ordering::AcqRel);
    if removed != 0 {
        lines.push(format!(
            "entry guard: {removed} terminated record(s) cleaned by GC_remove_thread; last id={:#x}",
            LAST_REMOVED.load(Ordering::Acquire)
        ));
    }
    let recovered = RECOVERED.swap(0, Ordering::AcqRel);
    if recovered != 0 {
        lines.push(format!(
            "VEH guard: recovered {recovered} Boehm abort(s) with saved next pointers; last id={:#x}",
            LAST_RECOVERED.load(Ordering::Acquire)
        ));
    }
    lines
}

fn reset_install() {
    READY.store(false, Ordering::Release);
    if !quiescent() {
        // Never clear origins beneath an in-flight detour. The caller keeps
        // original targets/trampolines alive, so a later reset can retry safely.
        log::log_tag("gct", "reset delayed until in-flight detours leave");
        return;
    }
    ENTRY_ACTIVE.store(false, Ordering::Release);
    ENTRY_OWNER_TID.store(0, Ordering::Release);
    // Origins and prepared trampolines deliberately stay valid for process life;
    // only activation and partial-install policy are reset here.
}

fn exact_bytes(target: usize, length: usize) -> Option<Vec<u8>> {
    crate::safe::read_bytes(target, length)
}

fn stored_prep(slot: &AtomicUsize, target: usize) -> Option<&'static Prep> {
    if slot.load(Ordering::Acquire) == 0 {
        let prep = Box::new(prepare(target)?);
        // Intentional process-lifetime retention: a prepared page, relay and
        // original trampoline must always outlive every possible detour call.
        slot.store(Box::into_raw(prep) as usize, Ordering::Release);
    }
    let address = slot.load(Ordering::Acquire);
    // SAFETY: a nonzero slot is only ever set to Box::into_raw(Prep) here. The
    // allocation is never freed or changed, so reads may use it for process life.
    Some(unsafe { &*(address as *const Prep) })
}

fn install_attached(prep: &Prep, detour: usize, attached: &AtomicBool) -> bool {
    if attached.load(Ordering::Acquire) {
        return true;
    }
    let Some(hook) = patch(prep, detour) else {
        return false;
    };
    register(hook);
    attached.store(true, Ordering::Release);
    true
}

/// Idempotent installation: validate destinations for this process, then patch
/// close handling, close-after-unlink and finally the collector entry. Prepared
/// pages and origins are never cleared or freed, including on a later failure;
/// a retry reuses them rather than allocating replacement trampoline pages.
pub fn install() -> bool {
    if READY.load(Ordering::Acquire) {
        return true;
    }
    reset_install();
    if crate::symbols::GAMEASSEMBLY_SHA256
        != "648f61b13e4f6daf6b144d03ff0fcdd0b294ca4b812ade90b7f7f9fcbd25903b"
    {
        log::log_tag("gct", "entry guard disabled: unsupported GameAssembly build");
        return false;
    }
    let Some((ga, _)) = procs::own_module(crate::GAME_ASSEMBLY) else {
        log::log_tag("gct", "entry guard disabled: GameAssembly is not loaded");
        return false;
    };
    let entry = ga + GC_SUSPEND_ENTRY_RVA;
    let skip = ga + GC_SKIP_CLOSE_RVA;
    let after = ga + GC_AFTER_UNLINK_RVA;
    if (ENTRY_PREP.load(Ordering::Acquire) == 0
        && exact_bytes(entry, SUSPEND_ENTRY_SIGNATURE.len())
            .map(|head| valid_gc_suspend_entry(&head))
            != Some(true))
        || (SKIP_PREP.load(Ordering::Acquire) == 0
            && exact_bytes(skip, SKIP_CLOSE_SIGNATURE.len()).as_deref()
                != Some(SKIP_CLOSE_SIGNATURE.as_slice()))
        || (AFTER_PREP.load(Ordering::Acquire) == 0
            && exact_bytes(after, AFTER_UNLINK_SIGNATURE.len()).as_deref()
                != Some(AFTER_UNLINK_SIGNATURE.as_slice()))
    {
        log::log_tag("gct", "entry guard: one or more live signatures do not match");
        return false;
    }
    let Some(skip_prep) = stored_prep(&SKIP_PREP, skip) else {
        return false;
    };
    let Some(after_prep) = stored_prep(&AFTER_PREP, after) else {
        return false;
    };
    let Some(entry_prep) = stored_prep(&ENTRY_PREP, entry) else {
        return false;
    };
    if skip_prep.prologue != SKIP_CLOSE_SIGNATURE.len()
        || after_prep.prologue != AFTER_UNLINK_SIGNATURE.len()
    {
        return false;
    }
    MODULE_BASE.store(ga, Ordering::Release);
    THREADS_BASE.store(ga + GC_THREADS_RVA, Ordering::Release);
    THREAD_LIST_CS.store(ga + THREAD_LIST_CS_RVA, Ordering::Release);
    SKIP_ORIG.store(skip_prep.trampoline, Ordering::Release);
    AFTER_ORIG.store(after_prep.trampoline, Ordering::Release);
    ENTRY_ORIG.store(entry_prep.trampoline, Ordering::Release);

    if !install_attached(skip_prep, defer_close_detour as *const () as usize, &SKIP_ATTACHED)
        || !install_attached(after_prep, close_after_unlink_detour as *const () as usize, &AFTER_ATTACHED)
        || !install_attached(entry_prep, suspend_entry_detour as *const () as usize, &ENTRY_ATTACHED)
    {
        reset_install();
        return false;
    }
    DETACHING.store(false, Ordering::Release);
    READY.store(true, Ordering::Release);
    log::log_tag(
        "gct",
        &format!("entry GC guard installed at {entry:#x}; CloseHandle moved after unlink"),
    );
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    static LIFECYCLE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[repr(C)]
    struct Record {
        next: usize,
        id: u32,
        pad: u32,
        handle: usize,
        stack: usize,
        reserved: usize,
        blocked: usize,
    }

    fn record(id: u32, handle: usize) -> Record {
        Record {
            next: 0,
            id,
            pad: 0,
            handle,
            stack: 0x7000_0000,
            reserved: 0,
            blocked: 0,
        }
    }

    #[test]
    fn linked_lookup_never_reads_an_unknown_candidate_or_leaves_a_cycle() {
        let mut table = [0usize; 256];
        let mut record = record(0x111, 0x1010);
        let ptr = &mut record as *mut Record as usize;
        record.next = ptr;
        table[0] = ptr;
        // SAFETY: a bounded synthetic chain is the only fixture; the unknown
        // candidate must not be dereferenced on a lookup miss.
        assert!(unsafe { linked_by_pointer(table.as_mut_ptr() as usize, 0xdead_beef) }.is_none());
        // SAFETY: this candidate is linked and therefore readable under fixture lock.
        let linked = unsafe { linked_by_pointer(table.as_mut_ptr() as usize, ptr) };
        assert_eq!(linked.map(|linked| linked.id), Some(0x111));
        assert_eq!(record.next, ptr);
    }

    #[test]
    fn close_choice_is_stable_across_activation_transitions() {
        let mut book = DecisionBook::new();
        assert!(!book.pending());
        assert_eq!(book.begin(0x1000, false), CloseChoice::Original);
        assert!(book.pending());
        // An uninhibited future READY flip must not change this invocation.
        assert_eq!(book.finish(0x1000), CloseChoice::Original);
        assert_eq!(book.finish(0x1000), CloseChoice::Original);

        assert_eq!(book.begin(0x2000, true), CloseChoice::Skip);
        assert_eq!(book.begin(0x3000, false), CloseChoice::Original);
        assert_eq!(book.finish(0x2000), CloseChoice::Skip);
        assert_eq!(book.finish(0x3000), CloseChoice::Original);
        assert!(!book.pending());
        // Unknown/already-completed records use original behavior, never skip.
        assert_eq!(book.finish(0x4000), CloseChoice::Original);
    }

    #[test]
    fn abort_resume_uses_saved_next_instead_of_freed_current_record() {
        let ga = 0x6fff_eaa1_0000;
        assert_eq!(next_after_abort(ga, 0x1234_5000), (0x1234_5000, ga + GC_PROCESS_RECORD_RVA));
        assert_eq!(next_after_abort(ga, 0), (0, ga + GC_ADVANCE_BUCKET_RVA));
        assert!(!can_recover_boehm_abort(false, 0x123, 0x123));
        assert!(!can_recover_boehm_abort(true, 0x123, 0x456));
        assert!(can_recover_boehm_abort(true, 0x123, 0x123));
    }

    #[test]
    fn detach_clears_activation_but_preserves_trampolines() {
        let _guard = LIFECYCLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        ENTRY_ORIG.store(0x1000, Ordering::Relaxed);
        SKIP_ORIG.store(0x2000, Ordering::Relaxed);
        AFTER_ORIG.store(0x3000, Ordering::Relaxed);
        after_unhook(&[]);
        assert!(!READY.load(Ordering::Relaxed));
        assert!(!ENTRY_ACTIVE.load(Ordering::Relaxed));
        assert_eq!(ENTRY_OWNER_TID.load(Ordering::Relaxed), 0);
        reset_install();
        assert_eq!(ENTRY_ORIG.load(Ordering::Relaxed), 0x1000);
        assert_eq!(SKIP_ORIG.load(Ordering::Relaxed), 0x2000);
        assert_eq!(AFTER_ORIG.load(Ordering::Relaxed), 0x3000);
    }

    #[test]
    fn partial_detach_recomputes_only_real_attachment_and_gates_decisions() {
        let _guard = LIFECYCLE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let ga = 0x6fff_eaa1_0000;
        MODULE_BASE.store(ga, Ordering::Relaxed);
        SKIP_ATTACHED.store(true, Ordering::Relaxed);
        AFTER_ATTACHED.store(true, Ordering::Relaxed);
        ENTRY_ATTACHED.store(true, Ordering::Relaxed);
        after_unhook(&[
            (ga + GC_SKIP_CLOSE_RVA, false),
            (ga + GC_AFTER_UNLINK_RVA, true),
            (ga + GC_SUSPEND_ENTRY_RVA, true),
        ]);
        assert!(SKIP_ATTACHED.load(Ordering::Relaxed));
        assert!(!AFTER_ATTACHED.load(Ordering::Relaxed));
        assert!(!ENTRY_ATTACHED.load(Ordering::Relaxed));
        assert!(DETACHING.load(Ordering::Relaxed));
        // New decisions are not admitted after that detach boundary.
        assert_eq!(begin_close(0x1234), CloseChoice::Original);
        after_unhook(&[]);
        assert!(!SKIP_ATTACHED.load(Ordering::Relaxed));
        assert!(DETACHING.load(Ordering::Relaxed));
        assert!(!close_decisions_pending());
    }
}
