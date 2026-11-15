//! Who does the Boehm GC think is alive?
//!
//! `Fatal error in GC — SuspendThread loop failed` (crash dumps: `0x80000003`
//! int3 at `GameAssembly.dll+0xc41b65`) is Boehm's `GC_suspend()` giving up
//! after 1 000 000 `SuspendThread → GetThreadContext → ResumeThread` retries.
//! The retry only runs when the *context* cannot be fetched, and under wine that
//! happens for a thread that has already terminated: Boehm still holds its
//! `GC_thread` record (and the handle inside it, which keeps the thread object
//! alive), so `SuspendThread` "succeeds" forever while `GetThreadContext` always
//! fails.
//!
//! This module reads Boehm's own thread table and compares it with the threads
//! the process really has. A record whose id is no longer live is exactly the
//! thread that will kill the game at the next collection — so logging this
//! periodically turns "the game died somewhere after the inject" into
//! "thread 0x%x died and was never unregistered".
//!
//! Layout comes from bdwgc `win32_threads.c` (`GC_Thread_Rep`, x64, win32):
//!   +0x00 next, +0x08 id (DWORD), +0x10 handle, +0x18 stack_base.
//! The table is `STATIC GC_thread GC_threads[THREAD_TABLE_SZ]` (256 buckets,
//! array of pointers, indexed by `((id >> 8) ^ id) % 256`). It is in the game's
//! `.data`, so its address is `GameAssembly base + rva`; the RVAs below were
//! recovered statically from the two functions that use it
//! (`GC_new_thread`: store into the bucket; `GC_lookup_thread_inner`/detach:
//! `mov (%r14,%rsi,8),%rbx; cmp %ecx,0x8(%rbx); mov (%rbx),%rbx;
//! mov 0x10(%rbx),%rcx; call CloseHandle`). Several candidates are kept because
//! a wrong base is silent, and detection is cheap: a real table's entries point
//! at structs whose id matches a live thread id.

#![forbid(unsafe_code)]

use crate::safe;
use crate::{procs, GAME_ASSEMBLY};

/// Buckets in `GC_threads[]` (`THREAD_TABLE_SZ`, power of two).
pub const BUCKETS: usize = 256;

const OFF_NEXT: usize = 0x00;
const OFF_ID: usize = 0x08;
const OFF_HANDLE: usize = 0x10;
const OFF_STACK_BASE: usize = 0x18;

/// RVAs of `GC_threads[]` candidates, from static analysis of this build
/// (GameAssembly sha256 `648f61b1…`). Re-derive them after a game update:
/// search `.text` for `c1 e8 08` (shr eax,8) followed by `33 c1` and a
/// `mov (%r14,%rsi,8),%rbx` / `cmp %ecx,0x8(%rbx)` chain.
pub const CANDIDATE_RVAS: &[u32] = &[0x0e6b_b180, 0x0e66_b180, 0x0e66_c760];

#[derive(Clone, Copy, Debug)]
pub struct Entry {
    pub bucket: u32,
    pub ptr: usize,
    pub id: u32,
    pub handle: u64,
    pub stack_base: u64,
    pub live: bool,
}

#[derive(Clone, Debug)]
pub struct Table {
    pub base: usize,
    pub rva: u32,
    pub entries: Vec<Entry>,
    pub live: usize,
    pub dead: usize,
}

impl Table {
    /// One line, safe to write into the log every few seconds.
    pub fn summary(&self) -> String {
        let mut dead: Vec<String> = self
            .entries
            .iter()
            .filter(|e| !e.live)
            .map(|e| format!("{:#x}", e.id))
            .collect();
        dead.sort();
        dead.dedup();
        format!(
            "gc_threads @ {:#x} (rva {:#x}): {} record(s), {} live, {} DEAD{}; our tid {:#x}",
            self.base,
            self.rva,
            self.entries.len(),
            self.live,
            self.dead,
            if dead.is_empty() {
                String::new()
            } else {
                format!(" [{}]", dead.join(" "))
            },
            own_tid()
        )
    }

    /// Full dump for the `gct` console command.
    pub fn lines(&self) -> Vec<String> {
        let mut out = vec![self.summary()];
        for e in &self.entries {
            out.push(format!(
                "  [{}] ptr={:#x} id={:#x} handle={:#x} stack={:#x} {}",
                e.bucket,
                e.ptr,
                e.id,
                e.handle,
                e.stack_base,
                if e.live { "live" } else { "DEAD" }
            ));
        }
        out
    }
}

pub fn own_tid() -> u32 {
    crate::raw::sys::current_thread_id()
}

/// Every thread id that currently belongs to this process.
pub fn live_tids() -> Vec<u32> {
    crate::raw::sys::live_thread_ids()
}

fn read_record(ptr: usize) -> Option<(u32, u64, u64)> {
    let id = safe::read_u32(ptr + OFF_ID)?;
    let handle = safe::read_u64(ptr + OFF_HANDLE)?;
    let stack_base = safe::read_u64(ptr + OFF_STACK_BASE).unwrap_or(0);
    Some((id, handle, stack_base))
}

/// Score one candidate bucket array: how many buckets hold a pointer to
/// something that looks like a `GC_Thread_Rep` with a live thread id.
fn score(base: usize, tids: &[u32]) -> (usize, usize, usize) {
    let mut nonnull = 0usize;
    let mut plausible = 0usize;
    let mut live = 0usize;
    for i in 0..BUCKETS {
        let p = match safe::read_usize(base + i * 8) {
            Some(0) | None => continue,
            Some(p) => p,
        };
        nonnull += 1;
        if let Some((id, handle, _)) = read_record(p) {
            // wine handles are small multiples of 4; a thread id is a small int
            let looks = id != 0 && id < 0x10_0000 && handle % 4 == 0 && handle < 0x1000_0000;
            if looks {
                plausible += 1;
                if tids.contains(&id) {
                    live += 1;
                }
            }
        }
    }
    (nonnull, plausible, live)
}

fn find_table(tids: &[u32], forced: Option<u32>) -> Option<(usize, u32)> {
    let ga = procs::own_module(GAME_ASSEMBLY)?.0;
    let cands: Vec<u32> = match forced {
        Some(rva) => vec![rva],
        None => CANDIDATE_RVAS.to_vec(),
    };
    let mut best: Option<(usize, u32, usize)> = None;
    for rva in cands {
        let base = ga.wrapping_add(rva as usize);
        let (_nonnull, plausible, live) = score(base, tids);
        if plausible == 0 {
            continue;
        }
        // A real table has a few records and most of them alive. The first
        // candidate is the one the static tool derives (`tools/gct_table.py`),
        // so accept it on plausibility alone — the interesting case is exactly
        // the one where a record is dead.
        let primary = rva == CANDIDATE_RVAS[0];
        if (live >= 3 && live * 2 >= plausible) || (primary && plausible >= 3) {
            match best {
                Some((_, _, bl)) if bl >= live => {}
                _ => best = Some((base, rva, live)),
            }
        }
    }
    best.map(|(b, r, _)| (b, r))
}

/// Walk `GC_threads[]` and classify every record as live/dead.
pub fn snapshot(forced_rva: Option<u32>) -> Option<Table> {
    let tids = live_tids();
    let (base, rva) = find_table(&tids, forced_rva)?;
    let mut entries = Vec::new();
    for bucket in 0..BUCKETS {
        let mut p = safe::read_usize(base + bucket * 8).unwrap_or(0);
        let mut depth = 0;
        while p != 0 && depth < 8 {
            match read_record(p) {
                Some((id, handle, stack_base)) => entries.push(Entry {
                    bucket: bucket as u32,
                    ptr: p,
                    id,
                    handle,
                    stack_base,
                    live: tids.contains(&id),
                }),
                None => break,
            }
            p = safe::read_usize(p + OFF_NEXT).unwrap_or(0);
            depth += 1;
        }
    }
    let live = entries.iter().filter(|e| e.live).count();
    let dead = entries.len() - live;
    Some(Table {
        base,
        rva,
        entries,
        live,
        dead,
    })
}

/// Start address of a thread (`ThreadQuerySetWin32StartAddress`), i.e. the
/// function it was created with — that is what names an engine thread.
pub fn thread_start_address(tid: u32) -> Option<usize> {
    crate::raw::sys::thread_start_address(tid)
}

fn module_of(addr: usize) -> String {
    let pid = crate::raw::sys::current_process_id();
    for (name, base, size) in procs::modules(pid) {
        if addr >= base && addr < base + size {
            return format!("{}+{:#x}", name, addr - base);
        }
    }
    String::new()
}

/// Every thread of this process: id, start address, and whether Boehm knows it.
/// A thread that exists here but has no GC record (or vice versa) is the whole
/// story of "SuspendThread loop failed".
pub fn thread_rows(table: Option<&Table>) -> Vec<String> {
    let tids = live_tids();
    let mut out = vec![format!("threads: {} live, our tid {:#x}", tids.len(), own_tid())];
    let gc_ids: Vec<u32> = table.map(|t| t.entries.iter().map(|e| e.id).collect()).unwrap_or_default();
    for tid in tids {
        let start = thread_start_address(tid);
        let (addr, who) = match start {
            Some(a) => (format!("{a:#x}"), module_of(a)),
            None => ("?".to_string(), String::new()),
        };
        out.push(format!(
            "  {tid:#06x} start={addr:>14} {:<40} gc_record={}",
            who,
            if gc_ids.contains(&tid) { "yes" } else { "NO" }
        ));
    }
    // records whose thread is gone
    if let Some(t) = table {
        for e in t.entries.iter().filter(|e| !e.live) {
            out.push(format!(
                "  DEAD record id={:#x} bucket={} ptr={:#x} handle={:#x} stack={:#x}",
                e.id, e.bucket, e.ptr, e.handle, e.stack_base
            ));
        }
    }
    out
}

/// `suspended` lives in the last byte of `finalizer_skipped/nested/suspended`
/// (u16, u8, u8) — measured on this build, where the record ends with
/// `context_sp` at +0x40 (RETRY_GET_THREAD_CONTEXT is compiled in).
pub const OFF_SUSPENDED: usize = 0x3b;

/// Remove records whose thread is gone from `GC_threads[bucket]`.
///
/// This is the only repair that helps: `GC_suspend_all` walks the table and
/// calls `GC_suspend` on every record with `suspended == 0`, and for a dead
/// thread that loop can never succeed — 1 000 000 retries and then an `int3`.
/// Merely setting `suspended = 1` is worse: `GC_resume_all` then aborts with
/// "ResumeThread failed". Unlinking makes the collector forget the thread
/// entirely, which is what `GC_unregister_my_thread` should have done.
pub fn repair(table: &Table) -> Vec<String> {
    let mut out = Vec::new();
    for e in table.entries.iter().filter(|e| !e.live) {
        // A record may only be removed once its thread object is signalled:
        // while the thread is still running its `GC_unregister_my_thread`, the
        // lookup walks the bucket by id and has no NULL check.
        if alive_by_handle(e.handle) != Some(false) {
            out.push(format!(
                "record id={:#x}: thread object not signalled yet — kept",
                e.id
            ));
            continue;
        }
        // find the predecessor in this bucket (the chain is short: 1-2 nodes)
        let mut prev = 0usize;
        let mut cur = safe::read_usize(table.base + e.bucket as usize * 8).unwrap_or(0);
        let mut guard = 0;
        while cur != 0 && guard < 16 {
            if cur == e.ptr {
                let next = safe::read_usize(cur + OFF_NEXT).unwrap_or(0);
                let ok = if prev == 0 {
                    safe::write_usize(table.base + e.bucket as usize * 8, next)
                } else {
                    safe::write_usize(prev + OFF_NEXT, next)
                };
                out.push(format!(
                    "unlinked dead record id={:#x} from bucket {} (next {:#x}) -> {}",
                    e.id, e.bucket, next, ok
                ));
                break;
            }
            prev = cur;
            cur = safe::read_usize(cur + OFF_NEXT).unwrap_or(0);
            guard += 1;
        }
        if guard >= 16 {
            out.push(format!("record id={:#x}: chain too long, not unlinked", e.id));
        }
    }
    out
}

/// A running thread can still answer `STILL_ACTIVE` (`0x103`) while it is in
/// its exit path, so liveness is decided by the wait below, not by that code.
/// Is the thread behind this handle still running? `None` = the query itself
/// failed (a handle we do not own, or a recycled one).
///
/// Two questions are asked, and only a thread that is **fully terminated** may
/// be reported dead:
///   * `GetExitCodeThread` — cheap, but under wine it can still answer
///     `STILL_ACTIVE` for a thread that is in its exit path;
///   * `WaitForSingleObject(handle, 0)` — the thread object is signalled only
///     once the thread has completely finished, including its
///     `GC_unregister_my_thread` call.
///
/// That distinction matters: unlinking a record while its thread is still inside
/// `GC_lookup_thread_inner` makes the walk run off the end of the bucket
/// (`cmp %ecx,0x8(%rbx)` with `rbx == NULL`, measured: AV at
/// `GameAssembly+0xc417dc` in thread `0x378` right after we reaped it).
fn alive_by_handle(handle: u64) -> Option<bool> {
    if handle == 0 || handle == u64::MAX || handle == u64::MAX - 1 {
        // 0 = none, -1 = pseudo "current thread" sentinels
        return None;
    }
    let rc = crate::raw::sys::wait_handle(handle, 0)?;
    match rc {
        WAIT_OBJECT_0 => Some(false), // signalled: the thread has fully exited
        WAIT_TIMEOUT => Some(true),   // still running (even if merely suspended)
        _ => None,
    }
}

const WAIT_OBJECT_0: u32 = 0;
const WAIT_TIMEOUT: u32 = 0x102;

/// Unlink one record, given its bucket and pointer. Returns true on success.
fn unlink_at(base: usize, bucket: usize, ptr: usize) -> bool {
    let mut prev = 0usize;
    let mut cur = safe::read_usize(base + bucket * 8).unwrap_or(0);
    let mut guard = 0;
    while cur != 0 && guard < 16 {
        if cur == ptr {
            let next = safe::read_usize(cur + OFF_NEXT).unwrap_or(0);
            return if prev == 0 {
                safe::write_usize(base + bucket * 8, next)
            } else {
                safe::write_usize(prev + OFF_NEXT, next)
            };
        }
        prev = cur;
        cur = safe::read_usize(cur + OFF_NEXT).unwrap_or(0);
        guard += 1;
    }
    false
}

/// Atomic counterpart of `reap()` for the GC's own `GetThreadContext` call.
/// Return the id only if Boehm's handle matches a record and the thread object
/// is signalled. `WAIT_TIMEOUT` means alive, so a suspended thread is *never*
/// unlinked. This closes the race that any periodic reap necessarily misses:
/// the same dying thread may already sit inside `GC_suspend` before the next
/// tick. The caller must still return a valid (synthetic) CONTEXT because the
/// loop is already executing; the removal also keeps `GC_resume_all` safe.
pub fn unlink_if_dead(handle: u64) -> Option<u32> {
    if alive_by_handle(handle) != Some(false) {
        return None;
    }
    let ga = procs::own_module(GAME_ASSEMBLY)?.0;
    let base = ga.wrapping_add(CANDIDATE_RVAS[0] as usize);
    for bucket in 0..BUCKETS {
        let mut p = safe::read_usize(base + bucket * 8).unwrap_or(0);
        let mut depth = 0;
        while p != 0 && depth < 8 {
            let id = safe::read_u32(p + OFF_ID).unwrap_or(0);
            if id != 0 && safe::read_u64(p + OFF_HANDLE) == Some(handle) {
                return unlink_at(base, bucket, p).then_some(id);
            }
            p = safe::read_usize(p + OFF_NEXT).unwrap_or(0);
            depth += 1;
        }
    }
    None
}

/// **The fix that races the collector.** Read the table and unlink any record
/// whose own handle reports an exited thread (`GetExitCodeThread != 0x103`) —
/// the game's threads die on a schedule of seconds and `GC_suspend_all` then
/// spins 1e6 times on the corpse (`SuspendThread loop failed: thread id=…,
/// errcode= 0x5` = ACCESS_DENIED for a terminated thread) before `int3`.
///
/// Uses Boehm's own handle, so no `Toolhelp32` snapshot (which can block for
/// minutes under wine) and no ambiguity about a dead-but-openable id. Called
/// from the console loop every ~400 ms; the window between the death and the
/// collection is ~1–6 s, so this has to be much faster than the 8 s watcher.
pub fn reap() -> Vec<String> {
    let mut out = Vec::new();
    let (base, _rva) = match find_table(&[], None) {
        Some(v) => v,
        None => {
            // `find_table` needs live ids for scoring; fall back to the known rva
            let ga = match procs::own_module(GAME_ASSEMBLY) {
                Some((b, _)) => b,
                None => return out,
            };
            (ga.wrapping_add(CANDIDATE_RVAS[0] as usize), CANDIDATE_RVAS[0])
        }
    };
    for bucket in 0..BUCKETS {
        let mut p = safe::read_usize(base + bucket * 8).unwrap_or(0);
        let mut depth = 0;
        while p != 0 && depth < 8 {
            let next = safe::read_usize(p + OFF_NEXT).unwrap_or(0);
            let id = safe::read_u32(p + OFF_ID).unwrap_or(0);
            let handle = safe::read_u64(p + OFF_HANDLE).unwrap_or(0);
            if id != 0 && alive_by_handle(handle) == Some(false) {
                // Second opinion before touching the table: unlinking a thread
                // that is merely suspended (or whose handle got recycled) would
                // drop its stack from the mark phase and free live objects.
                if live_tids().contains(&id) {
                    out.push(format!(
                        "record id={:#x} looks dead to its handle but the thread is alive — kept",
                        id
                    ));
                    continue;
                }
                // What the thread "said" as it died: a plain `ret` gives 0, a
                // `TerminateThread(code)` gives its code, and an uncaught
                // exception usually leaves the OS code. This is the only way to
                // tell "exited normally but skipped GC_unregister_my_thread"
                // from "was killed".
                let exit_code = crate::raw::sys::thread_exit_code(handle);
                let start = {
                    let mut g = match STARTS.lock() {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                    g.get_or_insert_with(std::collections::HashMap::new).get(&id).copied()
                };
                let body = worker_body_label(id);
                let start = start
                    .map(|a| format!("{a:#x} {}", module_of(a)))
                    .unwrap_or_default();
                let detail = if body.is_empty() {
                    start
                } else {
                    format!("{start} body={body}")
                };
                let ok = unlink_at(base, bucket, p);
                out.push(format!(
                    "reaped dead record id={:#x} handle={:#x} bucket={} exitcode={} start={} -> {ok}",
                    id,
                    handle,
                    bucket,
                    exit_code.map_or_else(|| "?".into(), |code| format!("{code:#x}")),
                    detail,
                ));
            }
            p = next;
            depth += 1;
        }
    }
    out
}

/// Should `watch()` auto-repair? On by default: without it the next collection
/// aborts the process, so the alternative is certain death.
fn auto_fix() -> bool {
    !matches!(
        std::env::var("HROMATITE_GCT_FIX").as_deref(),
        Ok("0") | Ok("no") | Ok("off")
    )
}

static LAST_TIDS: std::sync::Mutex<Option<Vec<u32>>> = std::sync::Mutex::new(None);
/// tid → start address, remembered from the moment the thread appeared. The
/// address is unreadable once the thread is gone, and "thread 0x3a0 exited" is
/// useless without knowing which engine entry point it was.
static STARTS: std::sync::Mutex<Option<std::collections::HashMap<u32, usize>>> =
    std::sync::Mutex::new(None);

/// Concrete worker body from the shared il2cpp dispatcher. The OS start address
/// is always the same trampoline; `(entry, arg)` is what identifies the job.
static WORKER_BODIES: std::sync::Mutex<
    Option<std::collections::HashMap<u32, (usize, usize)>>,
> = std::sync::Mutex::new(None);

pub fn note_worker_body(tid: u32, entry: usize, arg: usize) {
    let mut g = match WORKER_BODIES.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    g.get_or_insert_with(std::collections::HashMap::new)
        .insert(tid, (entry, arg));
}

pub fn worker_body(tid: u32) -> Option<(usize, usize)> {
    let mut g = match WORKER_BODIES.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    g.as_mut()?.get(&tid).copied()
}

pub fn worker_body_label(tid: u32) -> String {
    worker_body(tid)
        .map(|(entry, arg)| format!("{entry:#x} {} arg={arg:#x}", module_of(entry)))
        .unwrap_or_default()
}

/// Forget clean exits but preserve dead tids whose stale GC record still needs
/// a diagnosis. Live threads are retained even before registration completes.
pub fn prune_worker_bodies(live_tids: &[u32], gc_registered: &[u32]) {
    let mut g = match WORKER_BODIES.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    if let Some(bodies) = g.as_mut() {
        bodies.retain(|tid, _| live_tids.contains(tid) || gc_registered.contains(tid));
    }
}

static FIRST: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);
static IDLE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static LAST_SIG: std::sync::Mutex<(usize, usize, usize)> = std::sync::Mutex::new((0, 0, 0));

/// Periodic watcher: logs thread births/deaths and, the moment a thread dies
/// while Boehm still holds its record, the full record (this is the thread that
/// will abort the collector on the next `GC_suspend`).
pub fn watch() -> Vec<String> {
    let mut out = Vec::new();
    let tids = live_tids();
    let prev = {
        let mut g = match LAST_TIDS.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        g.replace(tids.clone())
    };
    let mut table = snapshot(None);
    let registered: Vec<u32> = table
        .as_ref()
        .map(|t| t.entries.iter().map(|e| e.id).collect())
        .unwrap_or_default();
    prune_worker_bodies(&tids, &registered);
    // First scan: name every thread that already exists. A thread we never saw
    // start is otherwise anonymous when it dies thirty seconds later.
    if FIRST.swap(false, std::sync::atomic::Ordering::Relaxed) {
        out.push("first scan — naming every thread".to_string());
        out.extend(thread_rows(table.as_ref()));
        // Remember them all: a thread that dies later must still have a name.
        let mut g = match STARTS.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let m = g.get_or_insert_with(std::collections::HashMap::new);
        for tid in &tids {
            if let Some(a) = thread_start_address(*tid) {
                m.insert(*tid, a);
            }
        }
    }
    if let Some(t) = table.as_ref() {
        out.push(t.summary());
        // Heal: a dead record makes the next collection spin 1e6 times and then
        // `int3` the process. Unlinking it is the only repair that helps.
        if t.dead > 0 {
            if auto_fix() {
                let fixes = repair(t);
                let fixed = !fixes.is_empty();
                out.extend(fixes);
                if fixed {
                    table = snapshot(None);
                    if let Some(t2) = table.as_ref() {
                        out.push(format!("after repair: {}", t2.summary()));
                    }
                }
            } else {
                out.push("HROMATITE_GCT_FIX=0: dead record left in place".to_string());
            }
        }
    } else {
        out.push("table not found".to_string());
    }
    // Freeze detector: the game spawning/reaping no threads and the GC table not
    // moving means the process is wedged (a dead worker holding a lock), not
    // busy. This is the line that separates "hung" from "slow".
    let sig = (
        tids.len(),
        table.as_ref().map(|t| t.entries.len()).unwrap_or(0),
        table.as_ref().map(|t| t.dead).unwrap_or(0),
    );
    {
        let mut last = match LAST_SIG.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if *last == sig {
            let n = IDLE.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
            if n.is_multiple_of(5) {
                out.push(format!(
                    "counts idle for ~{} s (quiet menu looks the same as a freeze — cross-check the Unity log mtime)",
                    n * 8
                ));
            }
        } else {
            *last = sig;
            IDLE.store(0, std::sync::atomic::Ordering::Relaxed);
        }
    }
    if let Some(prev) = prev {
        let now: std::collections::HashSet<u32> = tids.iter().copied().collect();
        let before: std::collections::HashSet<u32> = prev.iter().copied().collect();
        for gone in before.difference(&now) {
            let start = {
                let mut g = match STARTS.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                g.get_or_insert_with(std::collections::HashMap::new).remove(gone)
            };
            match start {
                Some(a) => out.push(format!(
                    "thread {gone:#x} exited (start was {a:#x} {})",
                    module_of(a)
                )),
                None => out.push(format!("thread {gone:#x} exited (was alive at the previous poll)")),
            }
            if let Some(t) = table.as_ref() {
                for e in t.entries.iter().filter(|e| e.id == *gone) {
                    out.push(format!(
                        "  ^ Boehm STILL holds it: bucket={} ptr={:#x} handle={:#x} stack={:#x} live={}",
                        e.bucket, e.ptr, e.handle, e.stack_base, e.live
                    ));
                }
            }
        }
        // A thread's start address is the only thing that names it while it is
        // alive, so print it here: the "exited" line may come thirty seconds
        // later, and by then the address is gone.
        for new in now.difference(&before) {
            let start = thread_start_address(*new);
            if let Some(a) = start {
                let mut g = match STARTS.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                g.get_or_insert_with(std::collections::HashMap::new).insert(*new, a);
            }
            let who = start.map(module_of).unwrap_or_default();
            match start {
                Some(a) => out.push(format!(
                    "thread {new:#x} started at {a:#x} {who} (gc_record={})",
                    table
                        .as_ref()
                        .map(|t| t.entries.iter().any(|e| e.id == *new))
                        .unwrap_or(false)
                )),
                None => out.push(format!("thread {new:#x} started (start address unavailable)")),
            }
        }
    }
    // a dead record that was already dead before is worth one line every time
    out
}

/// Undo a leaked suspension.
///
/// A collection suspends every thread, then resumes it. If the pass dies half-way
/// (abort, a fault in a resume, our own longjmp), the world stays suspended and
/// the game looks frozen while our own thread keeps running. Boehm's records
/// carry the handle, so `ResumeThread` on each of them is a cheap way to find out
/// whether that is what happened.
pub fn resume_all(table: &Table) -> Vec<String> {
    let mut out = Vec::new();
    for e in &table.entries {
        if e.handle == 0 {
            continue;
        }
        match crate::raw::sys::resume_thread(e.handle) {
            Some(v) => out.push(format!("ResumeThread(id {:#x}, h {:#x}) -> {v:#x}", e.id, e.handle)),
            None => out.push(format!("ResumeThread(id {:#x}) could not be resolved/called", e.id)),
        }
    }
    out
}

/// Diagnostic for the log/console: how each candidate scored, even when none
/// qualifies (so a failed detection is not silent).
pub fn probe(forced_rva: Option<u32>) -> Vec<String> {
    let tids = live_tids();
    let ga = match procs::own_module(GAME_ASSEMBLY) {
        Some((b, _)) => b,
        None => return vec!["gct: GameAssembly.dll not loaded".into()],
    };
    let cands: Vec<u32> = match forced_rva {
        Some(rva) => vec![rva],
        None => CANDIDATE_RVAS.to_vec(),
    };
    let mut out = vec![format!(
        "gct: {} live thread(s) in this process, our tid {:#x}",
        tids.len(),
        own_tid()
    )];
    for rva in cands {
        let (nonnull, plausible, live) = score(ga.wrapping_add(rva as usize), &tids);
        out.push(format!(
            "  rva {rva:#x} -> base {:#x}: buckets non-null {nonnull}, plausible {plausible}, id matches live {live}",
            ga.wrapping_add(rva as usize)
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{note_worker_body, prune_worker_bodies, worker_body};

    #[test]
    fn worker_registry_resolves_a_reaped_tid_to_its_real_body() {
        let tid = 0xf00d_0042;
        note_worker_body(tid, 0xbd1ca0, 0x1234);
        assert_eq!(worker_body(tid), Some((0xbd1ca0, 0x1234)));
        prune_worker_bodies(&[], &[tid]);
        assert_eq!(worker_body(tid), Some((0xbd1ca0, 0x1234)));
        prune_worker_bodies(&[], &[]);
        assert_eq!(worker_body(tid), None);
    }
}
