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

use crate::ffi::{self, CloseHandle, TH32CS_SNAPTHREAD, THREADENTRY32};
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
    unsafe { ffi::GetCurrentThreadId() }
}

/// Every thread id that currently belongs to this process.
pub fn live_tids() -> Vec<u32> {
    let mut out = Vec::new();
    unsafe {
        let snap = ffi::CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snap.is_null() || snap == ffi::INVALID_HANDLE_VALUE {
            return out;
        }
        let me = ffi::GetCurrentProcessId();
        let mut e = THREADENTRY32::default();
        e.dwSize = core::mem::size_of::<THREADENTRY32>() as u32;
        if ffi::Thread32First(snap, &mut e) != 0 {
            loop {
                if e.th32OwnerProcessID == me && e.th32ThreadID != 0 {
                    out.push(e.th32ThreadID);
                }
                e.dwSize = core::mem::size_of::<THREADENTRY32>() as u32;
                if ffi::Thread32Next(snap, &mut e) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snap);
    }
    out
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
    unsafe {
        let h = ffi::OpenThread(ffi::THREAD_QUERY_INFORMATION, 0, tid);
        if h.is_null() || h == ffi::INVALID_HANDLE_VALUE {
            return None;
        }
        let mut start: usize = 0;
        let mut ret: u32 = 0;
        let st = ffi::NtQueryInformationThread(
            h,
            THREAD_QUERY_START_ADDRESS,
            &mut start as *mut usize as *mut core::ffi::c_void,
            core::mem::size_of::<usize>() as u32,
            &mut ret,
        );
        ffi::CloseHandle(h);
        if st == 0 && start != 0 {
            Some(start)
        } else {
            None
        }
    }
}

const THREAD_QUERY_START_ADDRESS: u32 = 9;

fn module_of(addr: usize) -> String {
    let pid = unsafe { ffi::GetCurrentProcessId() };
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

static LAST_TIDS: std::sync::Mutex<Option<Vec<u32>>> = std::sync::Mutex::new(None);

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
    let table = snapshot(None);
    if let Some(t) = table.as_ref() {
        out.push(t.summary());
    } else {
        out.push("table not found".to_string());
    }
    if let Some(prev) = prev {
        let now: std::collections::HashSet<u32> = tids.iter().copied().collect();
        let before: std::collections::HashSet<u32> = prev.iter().copied().collect();
        for gone in before.difference(&now) {
            out.push(format!("thread {gone:#x} exited (was alive at the previous poll)"));
            if let Some(t) = table.as_ref() {
                for e in t.entries.iter().filter(|e| e.id == *gone) {
                    out.push(format!(
                        "  ^ Boehm STILL holds it: bucket={} ptr={:#x} handle={:#x} stack={:#x} live={}",
                        e.bucket, e.ptr, e.handle, e.stack_base, e.live
                    ));
                }
            }
        }
        for new in now.difference(&before) {
            out.push(format!("thread {new:#x} started"));
        }
    }
    // a dead record that was already dead before is worth one line every time
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
