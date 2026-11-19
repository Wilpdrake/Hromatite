# Handoff Report — Explorer 3 (`explorer_m2_3`)

**Core finding:** `crates/hromatite-core/src/raw/hook.rs` splits cleanly at line 944 into a pure x64 inline-hook engine (`1–944` + tests `2172–2290`, depending only on `hromatite-win`) and a dedicated `crates/hromatite-core/src/raw/lateupdate_hook.rs` runtime module (`946–2159` + tests `2294–2567`, keeping `LateUpdate`, `tick`/`work`, `HOOKS`/`register`, `gcguard`/`gcthreads`, and Stage-5 dead-code remnants completely out of `hromatite-hook`). Inside `crates/hromatite-eac`, all hooking (`hook.rs`, `tests/hook_engine.rs`, `iat`), inline Win32 FFI (`mod win`, `VirtualQuery`, `ReadProcessMemory`, `GetModuleHandleA`), and `hromatite_core::{ffi, safe, veh, procs, log, GAME_*}` usages switch to `hromatite-win` and `hromatite-hook` in Stage 2, leaving only `forge.rs::pump_photon` (`photon`) and `m3probe.rs::probe` (`m3`) on `hromatite-core` temporarily until Stages 3–4.

---

## 1. Observation

### 1.1 `crates/hromatite-core/src/raw/hook.rs` (2,569 lines) & Callers
- **Pure x64 relay-jump engine (`raw/hook.rs:1–944`, tests `2172–2290`):**
  - `Insn`, `modrm_len`, `decode_one`, `decode_span`, `prologue_len` (`26–299`)
  - `rel32_ok`, `adjusted_disp32`, `write_mode`, `branch_into_prologue`, `nearest_free_near`, `enum_near`, `alloc_near` (`305–450`)
  - `Hook` struct (`457–471`, `pub`: `target`, `detour`, `trampoline`, `prologue`; private: `alloc_base`, `alloc_size`, `orig32`, `base16`, `wmode`, `detached`), `write_window`, `set_range_protect`, `flush`, `RELAY_OFF`, `Prep`, `prepare`, `prepare_span`, `patch`, `hook_inline`, `Hook::unhook`, `Hook::describe` (`457–944`)
  - Engine tests (`2172–2290`): 8 unit tests covering decoding, relocation, alignment, `write_window`, prefixes, and internal branches.
  - External dependencies of lines `1–944`: **ONLY** `crate::ffi` (`MEMORY_BASIC_INFORMATION`, `VirtualQuery`, `VirtualAlloc`, `VirtualProtect`, `FlushInstructionCache`, `GetCurrentProcess`, `MEM_RESERVE`, `MEM_COMMIT`, `PAGE_READWRITE`, `PAGE_EXECUTE_READ`, `PAGE_EXECUTE_READWRITE`), `crate::log::log_tag`, and `crate::safe` (`readable_range`, `read_bytes`, `read_upto`).
- **Tick & runtime orchestration (`raw/hook.rs:946–1065`):**
  - Statics: `TICKS`, `LAST_MS`, `START_MS`, `INTERVAL_MS`, `BUSY`, `FRAME_BUSY`, `READER: Mutex<Option<console::Reader>>`, `HOOKS: Mutex<Vec<Hook>>` (`950–957`).
  - `now_ms()` (`959–962`, uses `ffi::GetTickCount64`), `pub fn tick()` (`966–993`, calls `crate::raw::hot::frame`, `crate::raw::hot::tick_interval_ms()`, `work`), `fn work()` (`995–1065`, calls `take_inline_repairs()`, `crate::raw::gcguard::report_lines()`, `crate::gcthreads::drain_worker_events()`, `crate::raw::hot::tick()`, `READER`, `crate::raw::hot::{command, resident_command, dispatch}`, `console::dispatch`).
- **Legacy mid-function Boehm GC guard (`raw/hook.rs:1069–1338`, tests `2397–2543`):**
  - `GC_CONTEXT_CALL_PROLOGUE`, `valid_gc_context_call_site`, `GC_CONTEXT_ORIG`, `GC_THREADS_BASE`, `INLINE_REPAIRS`, `INLINE_LAST_TID`, `note_inline_repair`, `take_inline_repairs`, `WIN64_CONTEXT_*`, `write_synthetic_dead_context_raw`, `unlink_by_handle_raw`, `complete_dead_gc_call_raw`, `gc_context_route`, `#[unsafe(naked)] pub unsafe extern "system" fn gc_context_detour()`, `#[allow(dead_code)] fn install_legacy_gc_context_guard()`.
  - `install_legacy_gc_context_guard()` (`1269–1338`) has 0 callers; calls `prepare_span(target, 14)` (`1296`), `patch` (`1317`), `register` (`1319`). Note: `tools/test_gc_detour_abi.py:37` greps `nm` output of release DLL for `gc_context_detour` until Stage 5.
- **Active GC guard entry & worker trace (`raw/hook.rs:1340–1623`):**
  - `pub fn install_gc_context_guard() -> bool` (`1343–1345`) -> `crate::raw::gcguard::install()`.
  - `GC_DISPATCH_ORIG`, `GC_UNREGISTER_ORIG`, `GC_REMOVE_ORIG`, `EXIT_THREAD_ORIG`, `END_THREAD_EX_ORIG`, 5 `*_detour` fns (`1359–1454`), `install_named` (`1457–1495`, calls `try_resolve` at `1978`), `install_gc_trace_at` (`1497–1534`), `pub fn install_gc_worker_trace()` (`1543–1623`).
- **Concrete IL2CPP `LateUpdate` lifecycle (`raw/hook.rs:1625–1946, 1978–1989`):**
  - `LATE_ORIG`, `LATE_ENTRY`, `LATE_ATTACHED`, `LATE_CLASS`, `LATE_INFO`, `LATE_HITS`, `LATE_TID`, `reset_late_observation`, `managed_entry_in_module`, `LateInstallAction`, `late_install_action`, `tick_then_original`, `pub unsafe extern "system" fn late_update_detour` (`1693–1729`, calls `crate::raw::hot::original_first_required()` and `tick()`), `pub fn materialise_late_class` (`1733–1781`), `materialise_by_name` (`1788–1825`), `env_flag`, `pub(crate) fn prepare_tick()` (`1833–1846`), `pub fn install_late_update` (`1849–1913`), `late_attachment_present` (`1915–1920`), `pub fn late_update_status` (`1922–1946`), `try_resolve` (`1978–1981`), `pub(crate) fn register(h: Hook)` (`1983–1989`).
  - Tests (`2294–2395`): 7 `LateUpdate` lifecycle/ordering/module-bounds unit tests.
  - Active `gcguard` tests (`2545–2567`): 3 tests (`gc_record_action_unlinks_only_on_the_original_signalled_handle`, `entry_signature_matches_the_measured_boehm_prologue`, `recovery_access_and_activation_are_explicit`) importing `crate::raw::gcguard::*` (`2164–2166`).
- **Dead/unreachable `hookat` & unused `command` arms (`raw/hook.rs:1948–1974, 1991–2028, 2081–2156`):**
  - `HOOKAT_ORIG`, `HOOKAT_HITS`, `HOOKAT_LOG`, `count_detour`, `install_at`, `pub fn installed()` (`2013–2018`, 0 callers), `parse_addr` (`2024–2027`).
  - `pub fn command(line: &str)` (`2031–2159`): `raw/console.rs:1130–1132` only dispatches `"latec" | "latehook" | "late"`; arms `"unhook"` (`2081–2106`, reads private field `h.detached` at `2095` and calls `gcguard::{before_unhook, after_unhook}` + `playerloop::refresh_attachment(&installed)`), `"hooks"` (`2107–2122`), `"hookat"` (`2123–2137`), `"tick"` (`2138–2156`) are unreachable.
- **All callers of `raw/hook.rs` across the workspace:**
  - `crates/hromatite-core/src/lib.rs:30`: `pub use raw::hook;`
  - `crates/hromatite-dll/src/lib.rs:9, 113, 125, 263, 274`: calls ONLY `hook::materialise_late_class`, `hook::install_late_update`, `hook::install_gc_context_guard`, `hook::install_gc_worker_trace`.
  - `crates/hromatite-core/src/raw/playerloop.rs:300, 421, 426, 439, 447`: calls `crate::hook::{tick, prepare_tick, register}` (runtime) AND `crate::hook::{prepare, patch}` (engine).
  - `crates/hromatite-core/src/raw/console.rs:1131`: calls `crate::hook::command(line)`.
  - `crates/hromatite-core/src/raw/gcguard.rs:9`: `use crate::raw::hook::{Prep, patch, prepare, register};`.
  - `crates/hromatite-core/src/raw/photon.rs:1973, 1980, 1990, 2011, 2015, 2024`: calls `crate::raw::hook::{prepare, patch, register}`.
  - `crates/hromatite-core/src/raw/kiero.rs:163, 170, 238, 417`: uses `crate::raw::hook::{Hook, hook_inline}`.
  - `crates/hromatite-core/src/raw/midhook.rs:24–27`: imports `super::hook::{adjusted_disp32, alloc_near, branch_into_prologue, decode_span, flush, rel32_ok, set_range_protect, write_mode, write_window, PAGE_SIZE, RELAY_OFF}` (moves into `hromatite-hook` in Stage 2).

### 1.2 `crates/hromatite-eac/` Dependencies & Win32/Hook Usage
- `crates/hromatite-eac/Cargo.toml:14`: currently depends only on `hromatite-core = { path = "../hromatite-core" }`.
- `crates/hromatite-eac/src/lib.rs:56–57, 70`: declares `pub mod hook;` and `pub use hook::Hook;`.
- `crates/hromatite-eac/src/hook.rs` (`1–1793`) & `tests/hook_engine.rs` (`1–261`):
  - Contains the 12-byte `mov rax, imm64; jmp rax` inline engine + `Local\hromatite_rust_hooktable_v1` shared table, inline `mod win` (`865–917`: `MemBasic`, `VirtualAlloc`, `VirtualQuery`, `VirtualProtect`, `FlushInstructionCache`, `GetCurrentProcess`, `CreateFileMappingA`, `MapViewOfFile`), `readable` (`925–945`), and **one back-edge to `eos`** at line `1337`: `crate::eos::hook_log(format!("[hook] {name}: {}", hook.describe()));`.
- `crates/hromatite-eac/src/eos.rs`:
  - `75`: `use crate::hook;` -> `hook::install` at `1005`, `1283`.
  - `1181, 1187, 1199, 1267`: `unsafe { hook::readable(addr, len) }` inside `log_evp` and `install_evp`.
  - `682, 1398`: `hromatite_core::iat::status()` (`682`), `use hromatite_core::iat;` (`1398`, `iat::module_imports` at `1421`, `iat::patch_module` at `1424`).
  - `748, 1517–1615`: inline `mod win` (`GetModuleHandleA`, `DosHeader`, `ExportDirectory`) and `resolve_export(module, name)` (called at `999`).
  - `1354`: `hromatite_core::log::log_tag("iat", line)`.
  - `1404–1406, 1408`: `hromatite_core::{GAME_EXE, GAME_ASSEMBLY, UNITY_PLAYER}` and `hromatite_core::procs::modules(std::process::id())`.
  - `2801, 2895, 2898, 2938, 2940`: `hromatite_core::safe::write_bytes` and `hromatite_core::safe::read_u32`.
  - `1065`: `crate::m3probe::probe()` (internal to `hromatite-eac`).
- `crates/hromatite-eac/src/evp.rs`:
  - `264, 580`: `use crate::hook;` and `hook::install(address, handlers[index], target.name)` (note line `585` already calls `hook_log` itself!).
  - `276–329`: `readable` uses `hook::win::MemBasic` & `hook::win::VirtualQuery` (lines `282, 285, 288`), `slice` uses `core::slice::from_raw_parts` (`312`), `read_u32` uses `core::ptr::read_unaligned` (`328`).
- `crates/hromatite-eac/src/eacimage.rs`:
  - `800–801, 862, 886, 906, 968, 1009`: `hromatite_core::ffi::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery}` and `hromatite_core::safe::{read_bytes, scan_yield}`.
  - `1277`: `hromatite_core::veh::call_with`.
- `crates/hromatite-eac/src/keyhunt.rs`:
  - `190–218` (`#[cfg(windows)] fn read_block`): inline `unsafe extern "system" { fn ReadProcessMemory(...); fn GetCurrentProcess() -> isize; }` relying on partial-copy behavior (`read.min(dst.len())`, even when `ReadProcessMemory` returns `0` across holes). Note `#[cfg(not(windows))] fn read_block` (`222–256`, `process_vm_readv`/`getpid`) is used by Linux host test `tests/keyhunt_scan.rs`.
  - `275–363` (`#[cfg(windows)] fn enumerate_regions`): duplicate `struct MemoryBasic`, `#[allow(clashing_extern_declarations)]`, `unsafe extern "system" { fn VirtualQuery(...); }` walking `0x1_0000..0x0000_7fff_ffff_0000` for `MEM_COMMIT` readable regions.
- `crates/hromatite-eac/src/forge.rs`:
  - `824, 834`: `pump_photon()` calls `hromatite_core::photon::template()` and `hromatite_core::photon::emulate_send(&frame)`. (Lines `459, 822` and `report.rs:56, 598` are doc-comment links only.)
- `crates/hromatite-eac/src/m3probe.rs`:
  - `17`: `use hromatite_core::{m3, procs, safe, GAME_ASSEMBLY};`
  - `35, 51, 54, 57, 66, 141, 142, 145`: `safe::{read_bytes, readable_at, read_usize}`.
  - `76`: `procs::own_module(GAME_ASSEMBLY)`.
  - `92, 95, 96`: `m3::{class_at, class_namespace, class_name}`.

---

## 2. Logic Chain

### 2.1 Exact `LateUpdate` Separation in Stage 2
1. Because `raw/hook.rs:1–944` (and tests `2172–2290`) only reference `ffi`, `log`, and `safe` (all moving to `hromatite-win`), moving lines `1–944` + `midhook.rs` + `vmt.rs` + `iat.rs` + `hookset.rs` + `eac/src/hook.rs` into `hromatite-hook` gives `hromatite-hook` zero dependencies on `LateUpdate`, `gcguard`, `gcthreads`, `console`, `hot`, `playerloop`, `EOSSDK`, or `hromatite-core`.
2. Moving lines `946–2159` + tests `2294–2567` out of `raw/hook.rs` into `crates/hromatite-core/src/raw/lateupdate_hook.rs` (kept in `hromatite-core` for Stage 2, ready for `hromatite-il2cpp`/`hromatite-runtime` in Stages 3–4) preserves every runtime symbol (`tick`, `prepare_tick`, `register`, `command`, `materialise_late_class`, `install_late_update`, `late_update_status`, `install_gc_context_guard`, `install_gc_worker_trace`) without leaking any runtime coupling into `hromatite-hook`.
3. Two small `hromatite-hook` API details prevent Stage 2 compile errors in `lateupdate_hook.rs`:
   - `Hook::is_detached(&self) -> bool` (needed by `command("unhook")` at `hook.rs:2095`, since `Hook.detached` is private).
   - `pub fn prepare_span(target: usize, min: usize) -> Option<Prep>` (needed by `install_legacy_gc_context_guard` at `hook.rs:1296` until its Stage 5 removal).
4. Updating `raw/playerloop.rs` (`prepare`/`patch` -> `hromatite_hook`, `tick`/`prepare_tick`/`register` -> `lateupdate_hook`), `raw/gcguard.rs` (`Prep`/`prepare`/`patch` -> `hromatite_hook`, `register` -> `lateupdate_hook`), `raw/photon.rs` (`prepare`/`patch` -> `hromatite_hook`, `register` -> `lateupdate_hook`), `raw/kiero.rs` (`Hook`/`hook_inline` -> `hromatite_hook`), `raw/console.rs` (`command` -> `lateupdate_hook`), and `hromatite-dll/src/lib.rs` (`hook` -> `lateupdate_hook` or temporary `pub use raw::lateupdate_hook as hook;` in `hromatite-core/src/lib.rs`) cleanly separates engine vs. runtime across all callers.
5. Legacy GC guard (`1069–1338`, `2397–2543`, `take_inline_repairs` at `998`) and unreachable `hookat`/`unhook`/`hooks`/`tick`/`installed` (`1948–1974`, `1991–2018`, `2081–2156`) **must stay out of `hromatite-hook`** (remain inside `hromatite-core/src/raw/lateupdate_hook.rs` until Stage 5 deletion, preserving `tools/test_gc_detour_abi.py` and the 3 active `gcguard` tests at `2545–2567`).

### 2.2 Exact `hromatite-eac` Stage 2 Migration Matrix
Because `hromatite-win` and `hromatite-hook` must have zero edges to `hromatite-eac` or `hromatite-core`, while `forge::pump_photon` and `m3probe::probe` cannot invert/drop `hromatite-core` until Stages 3–4:

| File / Symbol in `hromatite-eac` | Stage 2 Action | Target Crate / API |
|---|---|---|
| `Cargo.toml` | Add `hromatite-win`, `hromatite-hook`; keep `hromatite-core` temporarily | `hromatite-win`, `hromatite-hook`, (`hromatite-core` temp) |
| `src/hook.rs` & `tests/hook_engine.rs` | Move to `hromatite-hook`; remove `mod win`, `readable`, and `crate::eos::hook_log` (`1337`); `eos.rs` logs `installed.describe()` directly (like `evp.rs:585`) | `hromatite-hook` |
| `src/lib.rs:56–57, 70` | Remove `pub mod hook;`; `pub use hromatite_hook::Hook;` (and `pub use hromatite_hook as hook;` if external callers use `hromatite_eac::hook`) | `hromatite-hook` |
| `eos.rs:75, 1005, 1283` | `hook::install` -> `hromatite_hook` install + log `installed.describe()` via `hook_log` | `hromatite-hook` |
| `eos.rs:682, 1398, 1421, 1424` | `hromatite_core::iat::{status, module_imports, patch_module}` | `hromatite_hook::iat` |
| `eos.rs:748, 999, 1517–1615` | Remove `mod win` (`GetModuleHandleA`, `DosHeader`, `ExportDirectory`) and `resolve_export` | `hromatite_win` (`ffi::GetModuleHandleA` / `sys::module_handle`, `pe::live_export`) |
| `eos.rs:1181, 1187, 1199, 1267` | `unsafe { hook::readable(addr, len) }` -> safe `readable_range(addr, len)` | `hromatite_win::memory` |
| `eos.rs:1354` | `hromatite_core::log::log_tag` | `hromatite_win::log::log_tag` |
| `eos.rs:1404–1406, 1408` | `GAME_EXE`, `GAME_ASSEMBLY`, `UNITY_PLAYER`, `procs::modules` | `hromatite_win` |
| `eos.rs:2801, 2895, 2898, 2938, 2940` | `hromatite_core::safe::{read_u32, write_bytes}` | `hromatite_win::memory` |
| `evp.rs:264, 580` | `crate::hook::install` | `hromatite_hook` |
| `evp.rs:276–329` | Remove `hook::win::{MemBasic, VirtualQuery}` + raw `readable`/`slice`/`read_u32` | `hromatite_win::memory::{readable_range, read_bytes, read_u32}` |
| `eacimage.rs:800–801, 862, 886, 906, 968, 1009` | `hromatite_core::{ffi, safe}` (`VirtualQuery`, `MEM_COMMIT`, `PAGE_*`, `read_bytes`, `scan_yield`) | `hromatite_win::{ffi, memory}` |
| `eacimage.rs:1277` | `hromatite_core::veh::call_with` | `hromatite_win::veh::call_with` |
| `keyhunt.rs:190–218` (`#[cfg(windows)]`) | Remove inline `ReadProcessMemory`/`GetCurrentProcess`; preserve partial-copy semantics (`read.min(dst.len())`) via `hromatite_win::memory::read_partial` (keep `#[cfg(not(windows))]` `process_vm_readv`) | `hromatite_win::memory` |
| `keyhunt.rs:275–363` (`#[cfg(windows)]`) | Remove `MemoryBasic`, `#[allow(clashing_extern_declarations)]`, `VirtualQuery`; use `hromatite_win::procs::regions(usize::MAX)` filtered by `.readable()` | `hromatite_win::procs` |
| `m3probe.rs:17, 35, 51, 54, 57, 66, 76, 141–145` | Switch `procs`, `safe`, `GAME_ASSEMBLY` to `hromatite_win` | `hromatite_win` |
| `m3probe.rs:17, 92, 95, 96` (`m3`) | **Keep temporarily in Stage 2** (`hromatite_core::m3`) until Stage 4 removal | `hromatite_core::m3` |
| `forge.rs:824, 834` (`pump_photon`) | **Keep temporarily in Stage 2** (`hromatite_core::photon::{template, emulate_send}`) until Stage 3 gating & Stage 4 inversion | `hromatite_core::photon` |

---

## 3. Caveats

1. **`raw/context.rs:286` cross-boundary note (outside this task's file list, flagged for Stage 2 implementers):** `raw/context.rs::handler` currently calls `crate::raw::gcguard::recover_boehm_abort(info)`. When `context.rs` moves to `hromatite-win`, `hromatite-win` cannot depend on `hromatite-core::gcguard` — it will need a pre-handler callback/hook slot registered by `gcguard` at startup.
2. **Semantic differences to preserve when migrating `hromatite-eac` readers:**
   - `keyhunt.rs::read_block` (`191–218`) intentionally relies on partial-copy `ReadProcessMemory` (`read.min(dst.len())` even when `ReadProcessMemory` returns `0`), unlike `raw/memory.rs::read_copy` which fails closed on partial copies.
   - `evp.rs::readable` (`298`) currently matches `state == 0x1000 || 0x2000` (`MEM_RESERVE`), whereas `raw/memory.rs::readable_range` properly requires `MEM_COMMIT` (`0x1000`) and validates region boundaries. Switching `evp.rs` to `hromatite_win::memory::{readable_range, read_bytes, read_u32}` is strictly safer.
   - `eacimage.rs::live::walk` (`853–902`) starts at `0usize`, counts all regions (`seen += 1`) up to `MAX_REGIONS`, and yields every 64 regions, so it should use `hromatite_win::ffi::VirtualQuery` (or a dedicated `walk` helper in `hromatite-win`) rather than `procs::regions` (which only collects `MEM_COMMIT` starting at `0x10000`).
3. **Doc-comment links:** `crates/hromatite-eac/src/report.rs:56, 598` and `forge.rs:459, 822` mention `hromatite_core::photon::emulate_send` in doc comments (harmless in Stage 2, update in Stage 4 when `photon` moves to `hromatite-il2cpp`).

---

## 4. Conclusion

1. **Split `raw/hook.rs` cleanly at line 944:**
   - Move `raw/hook.rs:1–944` + tests `2172–2290` (plus `midhook.rs`, `vmt.rs`, `iat.rs`, `hookset.rs`, and `eac/src/hook.rs` + `eac/tests/hook_engine.rs`) into `hromatite-hook`. Expose `Hook::is_detached(&self) -> bool` and `prepare_span` on `hromatite-hook` for `lateupdate_hook.rs`.
   - Move `raw/hook.rs:946–2159` + tests `2294–2567` into `crates/hromatite-core/src/raw/lateupdate_hook.rs` (re-export as `pub use raw::lateupdate_hook;` and temporarily `pub use raw::lateupdate_hook as hook;` or update `hromatite-dll`, `playerloop.rs`, `console.rs`). Keep legacy GC guard (`1069–1338`, `2397–2543`) and unreachable `hookat`/`unhook`/`hooks`/`tick`/`installed` out of `hromatite-hook` inside `lateupdate_hook.rs` until Stage 5, while preserving the 3 active `gcguard` tests (`2545–2567`).
2. **Clean `hromatite-eac` onto `hromatite-win` + `hromatite-hook` in Stage 2:**
   - Replace all inline Win32 `extern "system"` / `mod win` / `MemoryBasic` / `VirtualQuery` / `ReadProcessMemory` / `GetModuleHandleA` / `resolve_export` in `hook.rs`, `eos.rs`, `evp.rs`, `eacimage.rs`, and `keyhunt.rs` with `hromatite-win` and `hromatite-hook` (preserving `#[cfg(not(windows))]` `process_vm_readv` in `keyhunt.rs` and partial-copy `read_partial` semantics).
   - Remove `crate::eos::hook_log` from `eac/src/hook.rs:1337` when moving it into `hromatite-hook`; let `eos.rs` log `installed.describe()` directly.
   - Keep `hromatite-core` temporarily in `crates/hromatite-eac/Cargo.toml` ONLY for `forge.rs::pump_photon` (`hromatite_core::photon`) and `m3probe.rs::probe` (`hromatite_core::m3`), leaving all existing Stage 2 builds and tests green.

---

## 5. Verification Method

After implementing Stage 2 edits, verify independently with:

1. **Dependency & boundary checks:**
   - `cargo tree -p hromatite-hook` — must show ONLY `hromatite-win` (zero `hromatite-core`, `hromatite-eac`, `hromatite-abi`, `hromatite-commands`, `hromatite-nuklear`).
   - `rg -n "LateUpdate|late_update|gcguard|gcthreads|EOSSDK|eos::|hot::|console::|hook_log|gc_context_detour|hookat" crates/hromatite-hook` — must return 0 matches.
   - `rg -n "extern \"system\"|VirtualQuery|ReadProcessMemory|GetModuleHandle|MemoryBasic|DosHeader|ExportDirectory" crates/hromatite-eac/src` — must return 0 matches.
   - `rg -n "hromatite_core" crates/hromatite-eac/src` — must match ONLY `forge.rs` (`photon`) and `m3probe.rs` (`m3`) (plus doc comments in `report.rs`/`forge.rs` until Stage 4).
2. **Build, clippy & test commands (project baseline):**
   - `cargo build --workspace` (exit 0, 0 errors)
   - `cargo test --workspace --no-run` (all Windows test binaries compile)
   - `cargo clippy --workspace --all-targets` (0 warnings/errors, all workspace unsafe lints respected)
   - `cargo test --workspace --target x86_64-unknown-linux-gnu` (all host unit & integration tests pass, including `hromatite-hook` engine tests, `hromatite-core` `lateupdate_hook`/active `gcguard` tests, and `hromatite-eac` `keyhunt_scan`/`epochs` tests)
3. **Invalidation conditions:**
   - Any reference from `hromatite-hook` to `hromatite-core`, `hromatite-eac`, `LateUpdate`, or `EOSSDK`.
   - Any remaining duplicate `VirtualQuery` / `ReadProcessMemory` / `GetModuleHandleA` FFI block in `crates/hromatite-eac/src/`.
   - Failure of `tests/keyhunt_scan.rs` on Linux host or `cargo test --workspace --no-run` on `x86_64-pc-windows-gnu`.
