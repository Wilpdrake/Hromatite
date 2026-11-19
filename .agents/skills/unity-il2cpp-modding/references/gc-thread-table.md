# Boehm GC thread table: diagnosing `SuspendThread loop failed` and frozen games

Unity's IL2CPP ships bdwgc's `win32_threads.c`. Almost every "the game died / froze after we
injected" report on this stack resolves to **one** object: a `GC_thread` record whose OS thread is
gone. Two very different symptoms come out of it, and they need opposite reactions:

| Symptom | What happens |
|---|---|
| `Fatal error in GC — SuspendThread loop failed` (Unity writes it to stderr and `<game>.gc.log`) | `GC_suspend` fails either at `SuspendThread` or while fetching a context from a dead/invalid handle, retries `MAX_SUSPEND_THREAD_RETRIES` (1e6) times, then `ABORT` → `int3` in `GameAssembly.dll` |
| Game freezes, no dialog, no dump, CPU ~0.1 core | the collection is stuck (wine's `get_thread_context` waits for a signal ack that a terminated thread never gives), so the world stays suspended — everything registered in the GC table is parked while your own thread keeps running |

A live payload thread that still answers console commands while the game is frozen is the proof that
the freeze is a suspension/deadlock, not a scan in your code. Log its tid at startup, and log thread
birth/death continuously; that log is what turns "it hung again" into a named thread.

## 1. Parse the minidumps first — they answer what the log cannot

Run `scripts/minidump_threads.py <crash.dmp> [--stack 0x400]` — it prints the exception (tid, code,
module+RVA), the suspend histogram, a per-thread RIP table and, with `--stack`, the stack words that
resolve to modules. Unity keeps the dumps at
`<prefix>/drive_c/users/steamuser/AppData/Local/Temp/<Game>/<Game>/Crashes/Crash_*/crash.dmp`.
A minidump is header (`"MDMP"`, 4-byte signature, stream count, stream-directory RVA) + a directory of
`(type, size, rva)`. Useful streams: **3** thread list (`MINIDUMP_THREAD`, 48 bytes: `ThreadId`,
`SuspendCount`, … stack `MemoryDescriptor`, `Context{Size,Rva}`), **4** module list (108 bytes/entry;
`BaseOfImage@0`, `SizeOfImage@8`, `ModuleNameRva@20`), **5** memory list, **6** exception stream
(`ThreadId`, `ExceptionRecord{Code, Flags, Address, …}`, `ThreadContext{Size,Rva}`), **15** memory-info.

`CONTEXT` (x64) is 1232 bytes with `Rsp@0x98`, `Rip@0xF8`, `Rcx@0x80`, `Rdx@0x88`, `Rbx@0x90`, `R8@0xB8`, `R9@0xC0`.
Resolve every address against the module list → `name+RVA`.

What to read out of it:

1. **The abort site.** `0x80000003` (int3) inside `GameAssembly.dll` + a nearby `.rdata` string
   (`SuspendThread loop failed`, `ResumeThread failed`, `SuspendThread loop failed: thread id= 0x%x,
   handle= %p, errcode= 0x%X`) is the ABORT macro. On the aborting thread's stack look for the decimal
   `1_000_000` (`0xf4240`): it means the retry counter was exhausted.
2. **The suspend histogram.** If every thread has `SuspendCount ≥ 1` except the aborting one, the
   thread the collector failed on is **not in the process any more** — a stale record, not an
   unsuspendable live thread. Measured across eight runs: 110–200 threads, `{0:1, 1:104, 2:10}`-shaped
   histograms every time, and the int3 at the same `GameAssembly` abort site every time.
3. Cross-check timing against the payload log: the abort lands 55 s … 7 min after the injector returns,
   with different last commands (or none at all) — the operation is irrelevant, only the dead thread is.
4. **Two `int3` fatals look alike — check the module, not the suspend counts.** `0x80000003` in
   `GameAssembly.dll` next to the Boehm strings is the collector abort. `0x80000003` in **`UnityPlayer.dll`**
   with *every* thread at `SuspendCount = 1` is Unity's own `DebugBreak`, and that uniform suspension is
   the crash reporter freezing the process to write a consistent dump — not a stop-the-world collection.
   Only the first flavour is cured by table repair. For the second, look for the thread that died just
   before: the captured game stdout may carry `wine client error:<tid>: write: Bad file descriptor`,
   which names it (and matches a `reaped dead record` line in your own log within seconds).

## 2. Find `GC_threads[]` offline (it is zero-initialised, so search the code, not the data)

`STATIC GC_thread GC_threads[THREAD_TABLE_SZ]` — `THREAD_TABLE_SZ = 256`, an array of pointers in
`GameAssembly.dll`'s `.data`, hash index `(((id >> 8) ^ id) % 256)`. The lookup/detach path compiles to

```
mov    %ecx,%eax            ; id
shr    $0x8,%eax
xor    %ecx,%eax
movzbl %al,%esi
lea    <disp32>(%rip),%r14  ; 4c 8d 35 ..  <-- this target IS GC_threads
mov    (%r14,%rsi,8),%rbx   ; 49 8b 1c f6
cmp    %ecx,0x8(%rbx)       ; 39 4b 08    <-- GC_thread.id
mov    (%rbx),%rbx          ; 48 8b 1b    <-- .next
mov    0x10(%rbx),%rcx      ; 48 8b 4b 10 <-- .handle, then CloseHandle
```

Scan `.text` for `49 8b 1c f6` followed within 12 bytes by `39 4b 08`, then look up to 0x50 bytes above
for `4c 8d 35` and compute `rva = next_ip + disp32`. Re-derive this after every game update; a Unity 6
VRChat build had the table at RVA `0xe6bb180`, in the uninitialised tail of `.data` (file offset past
`.data`'s raw size — a good confirmation that you found an array that starts zeroed).

Record layout (x64, `GC_WIN32_THREADS`, `RETRY_GET_THREAD_CONTEXT` compiled in), verified against live
memory and against the disassembly above:

```
+0x00 next             +0x08 id (DWORD)        +0x10 handle
+0x18 stack_base       +0x20 last_stack_min    +0x28 thread_blocked_sp
+0x30 traced_stack_sect
+0x38 finalizer_skipped(u16), finalizer_nested(u8), suspended(u8)   <- suspended @ 0x3B
+0x40 context_sp       (present because RETRY_GET_THREAD_CONTEXT)
+0x50 context_regs[]   (a copy of the saved registers; a dead thread's RIP slot here often equals its
                        start address's return address, e.g. GameAssembly+0xbf8d10)
```

## 3. Watch it from inside the payload

- `Thread32First/Next` (`TH32CS_SNAPTHREAD`, filter `th32OwnerProcessID == GetCurrentProcessId()`)
  gives the live ids; the table gives the GC's ids. Print `N record(s), L live, D DEAD [ids]` every
  few seconds — a dead id appears ~13 s before an abort, which is the window for everything below.
- Name threads **when they start**, never after: `NtQueryInformationThread(h, 9 /*
  ThreadQuerySetWin32StartAddress */, &pv, sizeof pv, &ret)` with `OpenThread(THREAD_QUERY_INFORMATION)`.
  A start address plus the module map (`name+RVA`) is the only identity a thread has; pool threads are
  usually all identical (`GameAssembly+0xbf8d10` for the job workers on one build), so also log the
  moment they exit.
- Do a **full** thread dump once, on the first scan: a thread that started before the watcher did is
  otherwise anonymous at death.
- Detect the freeze instead of guessing: if the thread count and the record count stop changing for
  ~40 s, say so in the log. That line separates "hung" from "slow". Caveat: a quiet menu produces the
  same flat counts, so cross-check the Unity log's mtime before calling it a freeze.
- **Diffing the live thread set is too slow to heal anything — poll the record's own handle instead.**
  Measured: the dead record was noticed and unlinked, and the abort still followed 6 s later because the
  collection had already entered its retry loop; an 8 s watcher loses that race, and the 1e6-iteration
  loop itself only takes ~6 s. So reap every few hundred milliseconds (each console poll), one cheap
  handle query per record and no `Toolhelp32` snapshot, and keep the slower full `Thread32` watcher only
  for naming threads and printing the summary.
- **The handle query has to be `WaitForSingleObject(handle, 0)`, not `GetExitCodeThread`.**
  `GetExitCodeThread` still answers `STILL_ACTIVE` (`0x103`) for a thread that is already in its exit
  path, and `Thread32` stops listing a thread before that path finishes — both therefore say "alive"
  when the collector's problem is real, and say "dead" when it is not yet safe to touch the table.
  A thread object is signalled (`WAIT_OBJECT_0`) only after the thread has completely finished,
  `GC_unregister_my_thread` included; `WAIT_TIMEOUT` means alive, even while suspended.

## 4. Repair semantics (read this before writing bytes)

- **Unlinking the dead record from its bucket is the only heal.** `GC_suspend_all` walks the buckets and
  calls `GC_suspend` for each record with `suspended == 0`; remove the node and the collector never sees
  the dead thread again (this is what `GC_unregister_my_thread` should have done). Do it as
  `GC_threads[bucket] = rec->next` when the record is the head, else `prev->next = rec->next`.
- **`suspended = 1` alone is worse**: `GC_resume_all` does
  `if (ResumeThread(handle) == -1) ABORT("ResumeThread failed")`, so a flagged dead record turns the
  suspend loop into a resume abort.
- **Zeroing the handle does not help**: `SuspendThread(0)` fails, so the retry loop still runs.
- Repair is cheap and idempotent, so auto-apply it when a dead record is seen (with an env kill switch),
  and keep a manual command for the case where the automatic pass is off.
- **Splice only after the thread object is signalled — the lookup has no NULL check.** The bucket walk
  compiles to `mov (%r14,%rsi,8),%rbx; cmp %ecx,0x8(%rbx); je found; mov (%rbx),%rbx; …`, i.e. it derefs
  the bucket head without testing it. If you empty a bucket while its thread is still inside
  `GC_lookup_thread_inner` / `GC_unregister_my_thread`, that thread's own walk hits NULL and faults at
  `GameAssembly+0xc417dc` (`cmp %ecx,0x8(%rbx)` with `rbx = 0`) — measured: the reaped id later appears as
  the dump's *exception thread*, which reads like "our repair broke the game" rather than "we were 4 s
  early". Gate the splice on `WaitForSingleObject(rec->handle, 0) == WAIT_OBJECT_0`; `WAIT_TIMEOUT`, a
  failed call or the `-1`/`-2` pseudo handles all mean "keep it". Unlinking a genuinely live thread is
  also bad in a quieter way: its stack drops out of the mark phase and objects only it references get
  freed.
- Status after a live run: two dead records were reaped ~0.8 s after their threads exited and the
  collections that followed completed (`World-stopped marking took 5 msecs`, no
  `SuspendThread loop failed`) — the repair covers the abort class. A freeze already in progress is a
  different story (§5), and a second fatal flavour can appear in the same session (§1, item 4).

## 5. The host can read *and write* the frozen process

`/proc/<pid>/mem` of the wine game process is openable `r+b` from the host even with
`ptrace_scope=1`, so a frozen game can be inspected and patched without reinjecting:
read the table (`GC_threads[bucket]` → chain), verify the record (`id`, `handle`, `stack_base` match
what the payload logged), write one byte or unlink one node, then watch `/proc/<pid>/stat`
(`utime+stime`), `/proc/<pid>/task` count and any log line whose timestamp advances.
Measured on a frozen process: writing `suspended=1` and (separately) unlinking the dead record both
left it frozen — the collector was already stuck inside `GetThreadContext`, so the repair cannot
unblock the *current* pass. Repair prevents the next one; `ResumeThread` over every record's handle is
the probe for "the world was left suspended" (harmless when nothing is suspended).

## 6. Capture the game's own output, or you debug blind

- Under Steam a Proton game's stdout/stderr is swallowed. In the launch wrapper: `"$@" >> ~/…/game-<ts>.log 2>&1`
  **before** injecting, `export GC_PRINT_STATS=1` (and `GC_PRINT_VERBOSE_STATS=0`; both strings are
  compiled into `GameAssembly.dll`), so per-collection stats land in the capture / `VRChat.gc.log`.
- `<game_dir>/VRChat.gc.log` is written next to the executable and **overwritten at every fatal** — copy
  it before and after each run, or the only evidence of the last crash is gone.
- `GC_LOOP_ON_ABORT=1` converts the abort into a spin, so a gdb inside wine can read `t->id` out of the
  `GC_suspend` frame; useful when the log variant of the message has no thread id.
- **The parameterised line is the cheapest confirmation there is** — read `<game>.gc.log` before doing any
  reasoning about who died. Observed on a live crash:
  `SuspendThread loop failed: thread id= 0x37c, handle= 00000000000004CC, errcode= 0x5`, where
  `errcode 0x5` is `ERROR_ACCESS_DENIED` — the exact error wine returns for `SuspendThread` on a
  **terminated** thread. That single line names the victim and rules out "our live thread is
  unsuspendable"; cross-check the id against your own stub tid and against your `DEAD [ids]` log line
  before writing any code.

## 7. Analysing a 250 MB `GameAssembly.dll`

`x86_64-w64-mingw32-objdump -d --start-address=0x… --stop-address=0x…` silently prints nothing for
addresses deep inside a huge PE (sections are `0x180…`-based and binutils gives up partway). Workaround
that always works: slice the bytes out of the file yourself (`file_off = rva - section_virtual_address +
raw_pointer`) into a small `.bin`, then disassemble as flat x86-64:

```bash
objdump -D -b binary -m i386:x86-64 --adjust-vma=0x<rva> slice.bin
```

When walking PE headers in Python, locate the section table at `pe_offset + 4 + 20 + SizeOfOptionalHeader`; using a fixed optional-header size shifts every section entry and makes a correct slice disassemble as unrelated instructions.

RIP-relative displacements in that listing are relative to the slice unless `--adjust-vma` is set, so
compute `target = next_ip + disp32` with the real RVA — a mis-computed table target looks like "the
pattern is wrong" rather than "the address is wrong".

No disassembler means no xref view, so build one in Python over `.text`: every direct call is
`e8 <rel32>` (`target = addr + 5 + rel32`), and every reference to a function or a table is a
RIP-relative `lea`/`mov` — `[48|4c] [8d|8b] [05|0d|15|1d|25|2d|35|3d] <disp32>`
(`target = addr + 7 + disp32`). Two patterns answer "who calls this" and "where is this pointer
stored" in seconds, and that is how both the GC table and the thread trampoline below were located.

## 8. Who the dying threads actually are (thread anatomy)

The stale records belong to engine pool workers, and their `start` address is a **shared trampoline**,
not a per-thread body. On the reviewed build it sits at `GameAssembly+0xbf8d10` and is unmistakable:

```
sub  $0x58,%rsp
movups (%rcx),%xmm0 ; movups %xmm0,0x20(%rsp)   ; copy {thread*, fn, out_tid*} from the parameter
movsd  0x10(%rcx),%xmm0 ; movsd %xmm0,0x40(%rsp)
call   <runtime helper>
call   *<iat slot>
mov    %eax,(%rcx)      ; store the returned id into *out_tid
call   *%rax            ; fn(thread*)   <-- the thread body
xor %eax,%eax ; ret
```

Its creator (find it with the call/xref scan above: exactly one `lea` targets the trampoline) allocates
a 0x18-byte `{thread, fn, arg}` record and calls
`CreateThread(NULL, thread_stack_size, <trampoline>, record, 0x10000, &tid)`.

Consequences worth internalising:

- **`GC_unregister_my_thread` runs inside `fn`, not in the trampoline.** The trampoline only returns, so
  a body that exits early (or a runtime path that skips detach) leaves the record behind while the OS
  thread is gone — exactly the leak §2–§4 heal.
- The two GC-side functions have different hygiene, and mixing them up costs a crash: the **lookup**
  (`GC_lookup_thread_inner`, entry `GameAssembly+0xc417b0` on that build — the `cmp %ecx,0x8(%rbx)` from
  §4 sits ~0x2c into it) derefs the bucket head **without** a NULL test, while
  **`GC_unregister_my_thread`** (`0xc41920` on that same build; re-derive it) tests the bucket and follows the chain safely. Only the
  lookup forces the "splice after signalled" rule.
- `GC_unregister_my_thread` also carries an **assert path ending in `int3`**, gated on a global flag
  (`cmpl $0, <global>` → call assert → `int3`). That is a second, independent source of `int3` in
  `GameAssembly.dll`: a fatal there without the Boehm suspend/resume strings next to it means a thread
  unregistered while the collector had that flag clear.
- **Do not assume an uncaught exception.** With a first-chance VEH logging every passing exception (with
  its tid, capped a few hundred), eight minutes of live play produced **zero** exception events while
  four of these workers started and exited normally — the leak is a lifetime/ordering bug, not a crash
  inside the thread. Instrument instead of theorising: keep a `tid → start address` map filled at birth,
  and at reap time log the remembered start address **and** the `GetExitCodeThread` value — `exitcode=0x0`
  reads "exited normally, detach skipped", anything else (or a value passed to `TerminateThread`) points
  at whoever killed it. Both facts must be captured while the thread is alive; afterwards it is gone.

## 9. Inline race guard: cover the active payload mode, then prove it ran

A periodic reaper is only hygiene: it cannot interrupt a `GC_suspend` retry loop
that has already started. A `reaped dead record ...` line in the same second as
the fatal message therefore proves the stale record, **not** a successful repair.
Success requires an inline repair at the proven failing call-site followed by a later
completed `GC #N` in `<game>.gc.log`.

Install the inline guard according to the payload lifetime rather than hiding it
behind one mode's setup routine:

| Payload mode | Guard policy |
|---|---|
| Unsealed stub-thread console (`LIVE>0`, no frame tick) | Arm `GetThreadContext` directly before recon/console work; a tick installer is never called in this mode |
| Unsealed QPC/frame hook | Arm alongside tick installation, exactly once |
| Unsealed managed `LateUpdate` | Arm directly from the injection stub before class materialisation or managed-hook installation; there is no QPC installer to inherit |
| Full encrypt-at-rest/seal | Do not leave an in-image detour behind; the callback must live in the external shim or remain disabled |

- Unit-test the mode-selection policy so the unsealed stub-console case cannot
  silently lose its guard and sealing cannot re-enable unsafe in-image code. A
  host test cannot prove that a live hook was installed, so also log a distinct
  boot marker in the selected mode.
- Treat `inline GetThreadContext guard installed` as necessary evidence, not the
  final criterion. The final evidence is `inline guard: stale record ... unlinked`
  followed by another completed collection without `SuspendThread loop failed`.
- If the latest fatal produced no new minidump, diagnose it from the fresh
  parameterised GC line, the payload's stub tid, the victim's remembered start
  address and its exit code. Do not attach an older dump's exception address or
  histogram to the new run merely because the visible error strings match.

### When an armed guard still misses

1. Preserve the current `<game>.gc.log` before restarting, then read the record
   named by the new fatal line directly from `/proc/<pid>/mem`: walk the bucket
   chain and capture `id`, `handle`, `stack_base`, `next` and `suspended`.
   Check that the process and module base belong to the current run.
2. Read the exact patched call-site, relay and detour bytes as well. A still-linked
   stale record plus a valid jump is evidence that the guard did not repair it;
   it is not evidence that installation failed, nor that a newly changed tick
   source caused the collection.
3. Instrument the classifier before changing its policy: publish the exact
   `WaitForSingleObject` result and the immediately-read `GetLastError` through
   lock-free counters, drained on an ordinary tick. Separate `WAIT_TIMEOUT`,
   `WAIT_FAILED` and invalid/sentinel handles. Never turn any failed wait into
   permission to unlink a live record. A closed-handle explanation remains a
   hypothesis until the wait result and error are actually observed.
4. Keep stop-the-world detours free of file I/O. Validate any behaviour change
   with a repair counter followed by a later completed collection, not merely
   with a valid patch and an `installed` boot line.

### Guard the earlier `SuspendThread` call too

- **Disassemble `GC_unregister_my_thread` and every takeover/remove helper before choosing guard sites.** Some IL2CPP/Boehm combinations compile record removal as `CloseHandle(record->handle)` followed by publishing the predecessor link; with a different stop-world lock, the collector can observe the record after its handle was closed. The remaining linked node therefore proves neither an open handle nor a completed unregister.
- **An exact `GetThreadContext` guard cannot repair a handle that already failed `SuspendThread`.** Guard the indirect `SuspendThread` call at Boehm's precise call-site as well, verify its full `call; cmp eax,-1; je …` signature before patching, and relocate the entire sequence. On a recoverable record, unlink it and return to the instruction that advances `current = current->next`; do not continue into `ResumeThread`, the `+1` counter or `GetThreadContext` with the dead handle.
- **Keep periodic-reaper semantics narrower than stop-world recovery.** The ordinary reaper still requires `WAIT_OBJECT_0`. In the exact collector call-site only, `WAIT_OBJECT_0` means terminated and `WAIT_FAILED` with `GetLastError() == ERROR_INVALID_HANDLE` means the handle is closed; both permit unlink. `WAIT_TIMEOUT`, `WAIT_ABANDONED`, `WAIT_FAILED` with another error, and pseudo handles stay on the original path — guessing from an unknown failure can remove a live thread's roots.
- **Prefer pointer matching while the table is locked.** Find the exact record pointer in its bucket and splice its predecessor to `next`; require that the record was actually linked and that the next pointer remains bounded and readable. An id-only lookup can confuse recycled identities, and a wait result alone does not authorize dereferencing an arbitrary exception register.
- **Save the live Win64 argument registers in every naked detour.** Preserve RCX for `SuspendThread`, as well as RCX/RDX for `GetThreadContext`, in slots outside the 32-byte call shadow space; restore RSP before tail-jumping to the relocated original or a validated absolute continuation. Test these instructions against the release DLL, not only Rust source or a synthetic pure function.

### If adding a last-resort VEH, keep recovery theorem-shaped

A VEH is not a licence to continue after arbitrary corruption. Treat each recovery as a narrowly proved state transition, and do not call it verified until a live abort is caught and a subsequent collection completes:

- Match `0x80000003` at exactly the disassembled Boehm `int3` addresses, in the exact loaded module. On the reviewed build those are the `ResumeThread`-failure and suspend-retry-abort sites; diagnostic `int3` in an unregister function or `UnityPlayer` is not equivalent.
- Prove which register holds the current record at those instructions (on the reviewed Win64 code, RBX at `CONTEXT+0x90`) and which continuation advances the loop. Never infer this from source structure or a nearby stack word.
- Require the record to be still linked and its wait classification to be terminated/closed. Unlink that exact pointer, emit only lock-free counters, then redirect `Rip` to the ordinary next-record instruction. If any proof fails, return `EXCEPTION_CONTINUE_SEARCH` and preserve normal VEH/stack-guard behaviour.
- Never handle a timeout or unknown error this way, and never suppress access violation or unrelated break exceptions: continued execution with corrupted heap/loop state is not a repair.
- Gate the implementation with build hash and loaded bytes, compile with the actual Win64 headers, assert the record/CONTEXT offsets with the toolchain, and test both positive recovery gates and every negative case. Static disassembly and host tests establish only implementation preconditions; the acceptance event remains a recovered fatal followed by a completed `GC #N` without suspend/resume failure.

### Synthetic `CONTEXT` and the hot-path boundary

- **Write the context before removing the record.** If the caller's output buffer
  is null or not fully writable, leave the table untouched and let the original
  call/retry path continue; never report success after a partial write.
- **Use the exact target ABI.** On Win64 `sizeof(CONTEXT) == 0x4d0` (1232),
  `ContextFlags` is at `0x30`, `Rsp` at `0x98` and `Rip` at `0xf8`. Preserve the
  requested `ContextFlags` instead of replacing the whole structure with an
  oversized zero write. Confirm the declarations with a mingw `_Static_assert`
  for `sizeof` and `offsetof`; add a host test for the synthetic byte writer so
  an over-wide write fails before it corrupts the caller.
- **Resolve everything expensive before patching.** Cache `GameAssembly` and the
  table base during installation. In the detour itself do only the bounded handle
  check/table walk/context write, with no module-name conversion, Toolhelp,
  loader lookup, mutex, allocation, formatting or file I/O: stop-the-world can
  leave a suspended thread owning one of those locks.
- **Coalesce reporting through atomics and log on the ordinary tick.** The event
  is not proven until a later collection completes without `SuspendThread loop
  failed` or `ResumeThread failed`; an `inline guard` line from inside the GC lock
  must not itself perform file I/O.

## 10. Preserve ownership through removal, patch and detach

- **Disassemble the whole removal function.** In this build `FUN_180c417b0` does
  `CloseHandle(record.handle)` before bucket unlink and `GC_free`. A collector can
  observe the invalid handle at `SuspendThread` (RVA `0xc41ab6`), so a guard only
  on `GetThreadContext` (`0xc41ad2`) never executes. Correct order is
  `call+test je` at `0xc41ab2`, loop/abort at `0xc41b65`, next record at
  `0xc41c5e`.
- **Move cleanup, not the thread identity.** Do not accept `ERROR_INVALID_HANDLE`
  as termination and do not call `OpenThread(tid)` as proof of incarnation. Handle
  or tid reuse makes that inference unsound. Prefer the build's own
  `GC_remove_thread(tid)` for close/unlink/free after the *existing original
  handle is signalled*. Invalid/unknown records remain untouched.
- **Two precise source positions are enough.** The original close belongs to
  `0xc417f0`; `0xc41818` is after bucket unlink but before sentinel/free. A skip
  at the first callsite and close at the second keeps cleanup atomic. Every
  callsite patch needs a naked tail continuation, not a returning Rust function:
  replacing a CALL with a JMP supplies no return address, so an ordinary return
  skips the rest of the original function.
- **The close choice belongs to the removal, not a global activation flag.**
  `ready` flips between those calls must not cause duplicate or skipped close.
  Record per-record `Skip` choices only; `Original` is stateless and follows
  Boehm's own close. Quiescence includes pending removal decisions, not just
  finished trampoline invocations. Latch detachment until complete reinstall,
  and recompute attachment per target from each actual `Hook.detached` result.
- **Recovery must own the collector lock and the current thread.** Require
  `ENTRY_ACTIVE` plus `ENTRY_OWNER_TID == current_thread_id`, not `READY`: unhook
  can wait while an admitted collection still needs rescue. Before touching RBX,
  find that pointer in the live table. Capture `next` before calling the original
  cleanup, then resume at `0xc41a90` for a nonempty chain (or `0xc41c6a` for an
  empty chain), never at `mov rbx,[rbx]` on the freed record.
- **Retained trampolines are lifecycle state, not options.** Keep origins/prepared
  pages valid for process life; patch success is registered immediately with a
  target attached flag so a retry cannot snapshot its own E9 as the original.
  Rewrite the relay only after making its page RWX again and publish it back to RX.
- **Approve only the whole path.** Host-proof must include exact close/control flow,
  saved-next recovery, partial prepare/patch/unhook retry, close-decision lifecycle,
  and release disassembly for naked continuations. A clean host suite without
  those fault-injection cases is not sufficient evidence. The live criterion is
  `entry guard: ... cleaned by GC_remove_thread` (or `VEH guard: recovered ...`)
  followed by a later completed GC and no Boehm fatal.
