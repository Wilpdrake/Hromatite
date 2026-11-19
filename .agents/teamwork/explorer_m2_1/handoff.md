# Handoff Report — Explorer 1 (`explorer_m2_1`): `hromatite-win` Extraction Plan (M2 / Stage 2)

**Core Summary:** `crates/hromatite-win` can be extracted as a **zero-dependency** crate (`edition = "2024"`, `[lints] workspace = true`) consolidating all 6 Win32 FFI copies, safe memory/`VirtualQuery` walkers, VEH guard (decoupled from `gcguard` via a `PreHandler` registration hook), file+live PE helpers, `procs`/PEB, `sha256`, `log`, `memscan::{stash_cstr, call3}`, `GAME_*` constants, and `#[cfg(feature = "remote")] remote`. `hromatite-core` cleanly re-exports `hromatite_win::*` in Stage 2 while `hromatite-eac` switches directly to `hromatite-win` for all OS/FFI/RPM/VirtualQuery/PE helpers.

---

## 1. Observation

1. **Workspace configuration (`Cargo.toml:1-23`)**:
   - Workspace uses `edition = "2024"`, `resolver = "3"`, and `[workspace.lints]` with `rust.unsafe_op_in_unsafe_fn = "deny"`, `clippy.undocumented_unsafe_blocks = "deny"`, `clippy.missing_safety_doc = "deny"`.
   - `crates/hromatite-core/Cargo.toml` depends on `hromatite-abi`, `hromatite-commands`, `hromatite-nuklear`.
   - `crates/hromatite-eac/Cargo.toml` depends on `hromatite-core` (for `m3` in `m3probe.rs:17`, `photon` in `forge.rs:824,834`, `iat` in `eos.rs:682,1398`, and `ffi`/`safe`/`veh`/`procs`/`log`/`GAME_*` in `eos.rs`, `eacimage.rs`, `m3probe.rs`).

2. **Files slated for `hromatite-win` in `crates/hromatite-core/src/`**:
   - `lib.rs:6-48`: `#![deny(unsafe_code)]`, exports `log`, `pe`, `sha256`, `raw::{ffi, context as veh, memory as safe, procs, remote, sys, memscan}`, and `GAME_EXE = "VRChat.exe"`, `GAME_ASSEMBLY = "GameAssembly.dll"`, `UNITY_PLAYER = "UnityPlayer.dll"`.
   - `raw/ffi.rs:1-300`: Win32 FFI + structs (`MEMORY_BASIC_INFORMATION`, `EXCEPTION_RECORD`, `EXCEPTION_POINTERS`, `PROCESSENTRY32W`, `MODULEENTRY32W`, `THREADENTRY32`, `wide`, `cstr`, `wide_to_string`, `CONTEXT_QWORDS`). Grep confirms 3 dead externs with 0 callers: `CreateThread` (`ffi.rs:237`), `RemoveVectoredExceptionHandler` (`ffi.rs:261`), `SetUnhandledExceptionFilter` (`ffi.rs:262`).
   - `raw/sys.rs:1-122`: safe wrappers (`current_process_id`, `current_thread_id`, `live_thread_ids`, `wait_handle`, `thread_exit_code`, `thread_start_address`, `export_address`, `resume_thread`, `wide_bytes`). Calls `crate::raw::ffi`.
   - `raw/context.rs:1-535`: VEH per-thread guard (`LANES`/`CTX`/`SLOTS`, `Fault`, `install`, `installed`, `call_with`, `call_guarded`, `call0..call10`).
     - **Critical coupling found at `raw/context.rs:286`**: inside `unsafe extern "system" fn handler`, line 286 calls `unsafe { crate::raw::gcguard::recover_boehm_abort(info) }`.
     - Lines 273, 290, 366 call `crate::raw::sys::current_thread_id()`.
   - `log.rs:1-118`: `#![forbid(unsafe_code)]`; `log::guard` (`log.rs:100`) calls `crate::veh::call_guarded(f)`. Macro `hlog!` (`log.rs:114-117`) has 0 callers across the repository.
   - `pe.rs:1-254`: `#![forbid(unsafe_code)]`; checked `PeImage` + `Section` (`from_file`, `from_bytes`, `is_executable`) + 4 unit tests.
   - `sha256.rs:1-218`: `#![forbid(unsafe_code)]`; `digest`, `hex_digest`, `digest_reader`, `hex_digest_file` + 4 unit tests.
   - `raw/procs.rs:1-342`: Toolhelp32 (`find_pid`, `all_pids`, `modules`, `module`), `unlink_from_peb` (`gs:[0x60]` inline asm), `Region`, `regions`, `claim_single_instance`, `own_module_path`, `own_module` + 2 unit tests (`region_tests`).
   - `raw/memory.rs:1-766`:
     - Lines 23-123: `#[cfg(not(target_os = "windows"))] #[unsafe(no_mangle)]` link stubs for `VirtualQuery`, `GetCurrentProcess`, `ReadProcessMemory`, `WriteProcessMemory`, `VirtualAlloc`, `VirtualProtect`, `FlushInstructionCache`.
     - Used externally: `scan_yield`, `read_words` (`hook.rs:1361`), `diagnostic_raw_read` (`console.rs:1124`), `span_at`, `readable_at`, `readable_range`, `read_copy`, `read_upto`, `read_bytes`, `read_u64`/`read_u32`/`read_u16`/`read_u8`/`read_usize`, `write_usize`, `write_bytes`, `ascii_at`, `ascii_lossy`, `utf16_lossy`.
     - Unused externally (only internal/memscan/unit tests): `Span::from_snapshot`, `Span::refresh`, `Span::bytes`, `read_u64_in_span`, `read_usize_in_span`, `read_u8_in_span`, `matches_in_span`, `ScanChunks`, `find_qword_in_span`.
   - `raw/memscan.rs:1-338`: grep confirms ONLY `stash_cstr` and `call3` are called externally (`raw/photon.rs:1389-1391`). `ScanSpan`, `readable_spans`, `spans_within`, `RegionMap`, `ascii_offsets`, `qwords`, `scan_ascii`, `scan_qwords`, `scan_words`, `call2` have 0 external callers.
   - `raw/remote.rs:1-219`: `Remote` (`open`, `read`, `read_vec`, `read_string`, `u16`, `u32`, `u64`, `write`, `alloc`, `alloc_at`, `protect`, `call`, `export`, `export_ordinal`). Used only by `manualmap.rs` and `src/main.rs` (`Hromatite.exe`).

3. **Inline Win32 FFI / VirtualQuery / RPM / PE copies across other modules**:
   - `crates/hromatite-core/src/raw/localmap.rs:624-627`: inline `unsafe extern "system" { #[link_name = "FreeLibrary"] fn free_library(module: ffi::HMODULE) -> ffi::BOOL; }`. (`LiveModule` at `630-750` already uses `memory::read_*` safely.)
   - `crates/hromatite-core/src/raw/ui.rs:110-116`: inline `unsafe extern "system"` user32 declarations (`GetForegroundWindow`, `GetClientRect`, `GetCursorPos`, `ScreenToClient`, `GetAsyncKeyState`).
   - `crates/hromatite-core/src/raw/discover.rs:79-128, 270-305`: `#![forbid(unsafe_code)]`; IL2CPP-specific section extractor (`.text`, `.rdata`, `.data`, `il2cpp`, `.pdata`) and export table walker over `safe::read_*` (stays in `hromatite-core` until `hromatite-il2cpp` in Stage 4).
   - `crates/hromatite-eac/src/hook.rs:865-917` (`mod win`): duplicates `MemBasic`, `VirtualAlloc`, `VirtualQuery`, `VirtualProtect`, `FlushInstructionCache`, `GetCurrentProcess`, and adds `CreateFileMappingA` / `MapViewOfFile`, `MEM_FREE`, `FILE_MAP_ALL_ACCESS` (migrated to `hromatite-hook` over `hromatite_win::ffi`).
   - `crates/hromatite-eac/src/eos.rs:1517-1615`: inline `mod win` (`GetModuleHandleA`, `DosHeader`, `ExportDirectory`) + unsafe `resolve_export(module, name)` (`eos.rs:1557-1615`, raw pointer derefs). Also calls `hromatite_core::{log, GAME_EXE, GAME_ASSEMBLY, UNITY_PLAYER, procs, safe}` (`eos.rs:1354, 1404-1408, 2801, 2895, 2898, 2938, 2940`).
   - `crates/hromatite-eac/src/eacimage.rs:800-801, 853-902, 1004-1024, 1277`: mixes `hromatite_core::ffi::VirtualQuery` with `hromatite_core::safe`, hand-rolls `walk()` and `region_at()`, and calls `hromatite_core::veh::call_with`.
   - `crates/hromatite-eac/src/keyhunt.rs:190-218, 275-363`: inline `ReadProcessMemory`/`GetCurrentProcess` (`read_block` relying on partial copies on holes: `read.min(dst.len())` at line 217) + inline `VirtualQuery` (`#[allow(clashing_extern_declarations)]` at line 301; also note `MEM_IMAGE = 0x1_0000` / `MEM_MAPPED = 0x4_0000` typo at `308-309` vs Win32 `0x0100_0000` / `0x0040_0000`). Lines 222-256 contain Linux-only `process_vm_readv`/`getpid` for host tests (`tests/keyhunt_scan.rs`).
   - `crates/hromatite-eac/src/evp.rs:276-329`: calls `hook::win::VirtualQuery` / `MemBasic` in `readable`, `slice`, `read_u32`. Every caller of `slice` (`evp.rs:351-387`) only passes `&[u8]` to `hex_prefix`.
   - `crates/hromatite-eac/src/m3probe.rs:17`: `use hromatite_core::{m3, procs, safe, GAME_ASSEMBLY};`.
   - `crates/hromatite-core/src/raw/{hook, hot, playerloop, photon}.rs`: 4 independent `now_ms()` wrappers calling `ffi::GetTickCount64()`.
   - `crates/hromatite-features/src/memory.rs:1-82`: pure Rust (`parse_addr`, `qwords`, `bytes`, `decode_hex` over `&Host` from `hromatite-abi`); **zero Win32 FFI**. Belongs to Stage 3 `hromatite-features` cleanup, not `hromatite-win`.

---

## 2. Logic Chain

1. **Zero-dependency `hromatite-win`**:
   - None of `ffi`, `sys`, `memory`, `memscan`, `veh`, `pe`, `procs`, `remote`, `sha256`, or `log` require `hromatite-abi`, `hromatite-commands`, or any external crate (Observation 2).
   - The single upward dependency from `context.rs:286` into `raw::gcguard::recover_boehm_abort` is easily inverted via an atomic function pointer `veh::register_pre_handler(PreHandler)` installed by `gcguard::install()`, preserving exact runtime semantics (zero locks/allocs in VEH, active whenever `ENTRY_ACTIVE` can be set) while keeping `hromatite-win` dependency-free.
2. **Intra-crate wiring & dead-code filtering**:
   - `log::guard` calls `veh::call_guarded`, which becomes an intra-crate call `crate::veh::call_guarded` inside `hromatite-win`, while `hlog!` (0 callers, Observation 2) is dropped.
   - Dropping dead externs (`CreateThread`, `RemoveVectoredExceptionHandler`, `SetUnhandledExceptionFilter`), unused `memscan` scanners (`ScanSpan`, `RegionMap`, `scan_*`, `call2`), and unused `memory` span-snapshot machinery (`ScanChunks`, `from_snapshot`, `read_*_in_span`, `matches_in_span`, `find_qword_in_span`) eliminates dead code without touching any active caller (Observation 2, `REFACTOR_AUDIT.md` §7.3).
   - Keeping the 7 `#[cfg(not(target_os = "windows"))] #[unsafe(no_mangle)]` stubs in `hromatite-win/src/memory.rs` ensures `hromatite-hook`, `hromatite-core`, and `hromatite-eac` continue to link on `x86_64-unknown-linux-gnu` host tests (`cargo test --workspace --target x86_64-unknown-linux-gnu`) whenever non-Windows code paths reference `ffi::VirtualAlloc`/`VirtualProtect`/`VirtualQuery`/`FlushInstructionCache`/`GetCurrentProcess`/`ReadProcessMemory`/`WriteProcessMemory`.
3. **Eliminating all 6 inline Win32 FFI / VirtualQuery / PE copies**:
   - Adding `FreeLibrary`, `CreateFileMappingA`, `MapViewOfFile`, `FILE_MAP_ALL_ACCESS`, `MEM_FREE`, and the 5 `user32` functions (`GetForegroundWindow`, `GetClientRect`, `GetCursorPos`, `ScreenToClient`, `GetAsyncKeyState`) to `hromatite-win::ffi` removes inline FFI blocks from `localmap.rs`, `ui.rs`, and `hromatite-hook` (Observation 3).
   - Adding `memory::read_partial` (partial-copy `ReadProcessMemory` returning `usize`), `memory::query_region` (single `VirtualQuery` returning `MemoryRegion`), `sys::module_handle` (`GetModuleHandleW` safe wrapper returning `Option<usize>`), `sys::tick_count_ms`, and `pe::resolve_live_export(base, name)` (safe bounds-checked export walk over `memory::read_*`) eliminates all inline FFI, `VirtualQuery`, `#[allow(clashing_extern_declarations)]`, and raw-pointer PE parsing in `eac/{eos.rs, eacimage.rs, keyhunt.rs, evp.rs}` (Observation 3).
4. **Stage 2 boundary preservation (`hromatite-core` vs `hromatite-eac`)**:
   - Feature-gating `#[cfg(feature = "remote")] pub mod remote;` in `hromatite-win` lets `hromatite-core` enable `features = ["remote"]` in Stage 2 (keeping `Hromatite.exe` and `manualmap.rs` working via `hromatite_core::remote` re-export) and lets Stage 3 enable `"remote"` solely on `hromatite-injector` so `hromatite-runtime`/`hromatite-dll`/`hromatite-hook`/`hromatite-eac` never pull `Remote`.
   - Re-exporting `ffi`, `safe`, `veh`, `procs`, `remote`, `pe`, `sha256`, `log`, `GAME_*` in `hromatite-core/src/lib.rs` (and `sys`, `memscan`, `memory`, `ffi`, `procs`, `remote`, `veh as context` in `hromatite-core/src/raw/mod.rs`) keeps `Hromatite.exe`, `hromatite-dll`, and all remaining `hromatite-core` modules compiling untouched in Stage 2, while `hromatite-eac` switches directly to `hromatite_win` (and `hromatite_hook`) and drops its remaining `hromatite-core` OS imports (`m3` and `photon` stay on `hromatite-core` until Stage 3/4).
5. **Safety & lint compliance**:
   - Crate root `#![deny(unsafe_code)]`; pure modules `pe.rs`, `sha256.rs`, `log.rs` retain `#![forbid(unsafe_code)]`; FFI/OS modules (`ffi`, `sys`, `memory`, `memscan`, `veh`, `procs`, `remote`) carry `#[allow(unsafe_code)]` while strictly satisfying workspace lints (`unsafe_op_in_unsafe_fn`, `undocumented_unsafe_blocks`, `missing_safety_doc`).

---

## 3. Caveats

1. **Test count migration**: Moving `pe.rs` (4 tests), `sha256.rs` (4 tests), `context.rs` (1 test), `procs.rs` (2 tests), and `memscan::stash_cstr` (1 test) shifts 12 existing tests from `hromatite-core` to `hromatite-win`. Dropping the 3 unused `memscan` tests (`region_map_*`, `ascii_offsets_*`, `qword_offsets_*`) and replacing the 4 unused `memory` span-snapshot tests (`span_reads_are_bounds_checked`, `candidate_limit_*`, `refresh_failure_*`, `qword_scan_*`) with unit tests for the retained `Span`/`MemoryRegion` helpers changes total unit test counts slightly unless equivalent coverage tests are added in `hromatite-win::memory` and `hromatite-win::pe` (`resolve_live_export`).
2. **Stage 2 coordination with `hromatite-hook` (Explorer 2)**: `hromatite-core/src/raw/hook.rs` retains `LateUpdate`/`tick`/`gc_worker_detour` until Stage 4 while delegating inline hooking to `hromatite-hook`. `hromatite-eac/src/eos.rs` switches `hromatite_core::iat` → `hromatite_hook::iat` and `crate::hook` → `hromatite_hook` in coordination with Explorer 2.
3. **`hromatite-features/src/memory.rs`**: Confirmed to be pure `&Host` (not Win32); deliberately left in `hromatite-features` until Stage 3 per `PROJECT.md` / `REFACTOR_AUDIT.md`.
4. **`CreateFileMappingA` `INVALID_HANDLE_VALUE` vs `0`**: `crates/hromatite-eac/src/hook.rs:916,1156` passed `0` (`INVALID_HANDLE = 0`) to `CreateFileMappingA`, whereas `ffi::INVALID_HANDLE_VALUE` is `-1isize as HANDLE`. `hromatite-win::ffi` exposes `INVALID_HANDLE_VALUE` and standard `HANDLE` (`*mut c_void`); Explorer 2 should verify whether `hromatite-hook` passes `INVALID_HANDLE_VALUE` or `core::ptr::null_mut()` under Wine/Proton.

---

## 4. Conclusion — Exact Module-by-Module Extraction Plan for `crates/hromatite-win`

### A. Workspace & `crates/hromatite-win/Cargo.toml`
- Add `"crates/hromatite-win"` to `[workspace] members` in `/home/wilpdrake/RustroverProjects/Hromatite/Cargo.toml`.
- Create `/home/wilpdrake/RustroverProjects/Hromatite/crates/hromatite-win/Cargo.toml`:
  ```toml
  [package]
  name = "hromatite-win"
  version = "0.1.0"
  edition = "2024"
  description = "Win32 FFI, safe memory/VirtualQuery, VEH guard, PE parser, procs/PEB, SHA-256 and file logging"

  [lib]
  name = "hromatite_win"
  path = "src/lib.rs"

  [features]
  remote = []

  [dependencies]

  [lints]
  workspace = true
  ```

### B. `crates/hromatite-win/src/` Module Layout & Exact Contents

| File | Unsafe Policy | Contents & Notes |
|---|---|---|
| `src/lib.rs` | `#![deny(unsafe_code)]` | Exports: `#[allow(unsafe_code)] pub mod ffi;`, `#[allow(unsafe_code)] pub mod memory;`, `#[allow(unsafe_code)] pub mod memscan;`, `pub mod log;`, `pub mod pe;`, `#[allow(unsafe_code)] pub mod procs;`, `#[cfg(feature = "remote")] #[allow(unsafe_code)] pub mod remote;`, `pub mod sha256;`, `#[allow(unsafe_code)] pub mod sys;`, `#[allow(unsafe_code)] pub mod veh;`. Constants: `pub const GAME_EXE: &str = "VRChat.exe";`, `pub const GAME_ASSEMBLY: &str = "GameAssembly.dll";`, `pub const UNITY_PLAYER: &str = "UnityPlayer.dll";`. |
| `src/ffi.rs` | `#[allow(unsafe_code)]` | From `core/src/raw/ffi.rs` **minus** dead `CreateThread`, `RemoveVectoredExceptionHandler`, `SetUnhandledExceptionFilter`. **Add**: `MEM_FREE = 0x1_0000`, `FILE_MAP_ALL_ACCESS = 0xF_001F`; `FreeLibrary(m: HMODULE) -> BOOL`; `GetModuleHandleA(name: *const core::ffi::c_char) -> HMODULE`; `CreateFileMappingA(file: HANDLE, attrs: *const core::ffi::c_void, protect: u32, high: u32, low: u32, name: *const core::ffi::c_char) -> HANDLE`; `MapViewOfFile(mapping: HANDLE, access: u32, high: u32, low: u32, size: usize) -> *mut core::ffi::c_void`; `user32` (`GetForegroundWindow() -> isize`, `GetClientRect(isize, *mut i32) -> i32`, `GetCursorPos(*mut i32) -> i32`, `ScreenToClient(isize, *mut i32) -> i32`, `GetAsyncKeyState(i32) -> i16`). |
| `src/sys.rs` | `#[allow(unsafe_code)]` | From `core/src/raw/sys.rs` (`current_process_id`, `current_thread_id`, `live_thread_ids`, `wait_handle`, `thread_exit_code`, `thread_start_address`, `export_address`, `resume_thread`, `wide_bytes`). **Add**: `pub fn module_handle(name: &str) -> Option<usize>` (`GetModuleHandleW(wide(name))`) and `pub fn tick_count_ms() -> u64` (`GetTickCount64` on Windows, `Instant` fallback on non-Windows). |
| `src/memory.rs` | `#[allow(unsafe_code)]` | Keep the 7 `#[cfg(not(target_os = "windows"))] #[unsafe(no_mangle)]` host link stubs (`VirtualQuery`, `GetCurrentProcess`, `ReadProcessMemory`, `WriteProcessMemory`, `VirtualAlloc`, `VirtualProtect`, `FlushInstructionCache`). Keep `scan_yield`, `read_words`, `diagnostic_raw_read` (`/// # Safety`), `span_at`, `readable_at`, `readable_range`, `writable_range`, `read_copy`, `read_upto`, `read_bytes`, `read_u64`/`u32`/`u16`/`u8`/`usize`, `write_usize`, `write_bytes`, `ascii_at`, `ascii_lossy`, `utf16_lossy`. **Add**: `MemoryRegion { base, size, state, protect, ty }` + `pub fn query_region(addr: usize) -> Option<MemoryRegion>` (shared by `span_at`, `procs::regions`, and `eacimage::live`) and `pub fn read_partial(addr: usize, buf: &mut [u8]) -> usize` (for `keyhunt::read_block`). **Drop**: unused `Span::from_snapshot`, `Span::refresh`, `Span::bytes`, `ScanChunks`, `read_*_in_span`, `matches_in_span`, `find_qword_in_span`. Add unit tests for `Span::contains`/`writable`, `MemoryRegion::is_readable`, `read_bytes`/`read_upto` fail-closed on host. |
| `src/memscan.rs` | `#[allow(unsafe_code)]` | **Keep only**: `stash_cstr`, `call3` (calling `crate::veh::call3`), and `tests::stash_cstr_returns_distinct_stable_nul_terminated_copies`. Remove `#![allow(dead_code)]`. **Drop**: `ScanSpan`, `readable_spans`, `spans_within`, `RegionMap`, `ascii_offsets`, `qwords`, `scan_ascii`, `scan_qwords`, `scan_words`, `call2` and their 3 dead tests. |
| `src/veh.rs` | `#[allow(unsafe_code)]` | From `core/src/raw/context.rs`. Replace direct `crate::raw::gcguard::recover_boehm_abort(info)` at line 286 with `PreHandler` hook: `pub type PreHandler = unsafe fn(*mut ffi::EXCEPTION_POINTERS) -> bool;`, `pub unsafe fn register_pre_handler(f: PreHandler)` (`/// # Safety` documented), invoked via `PRE_HANDLER: AtomicUsize` in `handler`. Replace `crate::raw::sys::current_thread_id()` with `crate::sys::current_thread_id()`. Keep `Fault`, `CtxBuf`, `install`, `installed`, `call_with`, `call_guarded`, `call0..call10`, and `per_thread_lanes_*` unit test. |
| `src/pe.rs` | `#![forbid(unsafe_code)]` | From `core/src/pe.rs` (`Section`, `PeImage` + 4 tests). **Add** safe live-image helpers over `crate::memory`: `pub fn live_image_size(base: usize) -> Option<usize>` and `pub fn resolve_live_export(base: usize, name: &str) -> Option<usize>` (bounds-checked PE32+ export directory walk replacing `eos.rs:1557`'s unsafe raw-pointer `resolve_export`). |
| `src/procs.rs` | `#[allow(unsafe_code)]` | From `core/src/raw/procs.rs` (`find_pid`, `all_pids`, `modules`, `module`, `unlink_from_peb`, `Region`, `regions`, `claim_single_instance`, `own_module_path`, `own_module` + 2 tests). Uses `crate::memory` (for `read_usize`/`write_usize`/`query_region`), `crate::ffi`, `crate::log`. |
| `src/remote.rs` | `#[allow(unsafe_code)]` + `#[cfg(feature = "remote")]` | From `core/src/raw/remote.rs` (`Remote` + `Drop`). Uses `crate::ffi`, `crate::procs`, `crate::log`. |
| `src/sha256.rs` | `#![forbid(unsafe_code)]` | Verbatim from `core/src/sha256.rs` + 4 tests. |
| `src/log.rs` | `#![forbid(unsafe_code)]` | From `core/src/log.rs` (`path`, `log_tag`, `log`, `log_echo`, `install_panic_hook`, `guard` calling `crate::veh::call_guarded`). **Drop**: unused `hlog!` macro. |

### C. Stage 2 Migration of Existing Crates

1. **`crates/hromatite-core`**:
   - `Cargo.toml`: add `hromatite-win = { path = "../hromatite-win", features = ["remote"] }` (and `hromatite-hook = { path = "../hromatite-hook" }`).
   - Delete migrated files: `src/{log.rs, pe.rs, sha256.rs}` and `src/raw/{ffi.rs, sys.rs, memory.rs, memscan.rs, context.rs, procs.rs, remote.rs}`.
   - `src/lib.rs`: replace `pub mod {log, pe, sha256}`, `pub use raw::{context as veh, ffi, memory as safe, procs, remote}`, and `GAME_*` constants with:
     ```rust
     pub use hromatite_win::{
         GAME_ASSEMBLY, GAME_EXE, UNITY_PLAYER, ffi, log, pe, procs, remote, memory as safe,
         sha256, veh,
     };
     ```
   - `src/raw/mod.rs`: replace deleted `pub mod {context, ffi, memory, memscan, procs, remote, sys}` with:
     ```rust
     pub use hromatite_win::{ffi, memory, memscan, procs, remote, sys, veh as context};
     ```
     (So `crate::raw::sys::*`, `crate::raw::memscan::*`, `crate::raw::memory::*`, `crate::raw::ffi::*` inside `manualmap.rs`, `gcthreads.rs`, `localmap.rs`, `photon.rs`, `kiero.rs`, `hook.rs` continue compiling cleanly.)
   - `src/raw/gcguard.rs::install()`: register `unsafe { crate::veh::register_pre_handler(recover_boehm_abort) };` before `install_attached(...)`.
   - `src/raw/localmap.rs:624-627`: delete inline `FreeLibrary` extern; call `ffi::FreeLibrary(...)`.
   - `src/raw/ui.rs:110-116`: delete inline `user32` extern block; import `GetAsyncKeyState`, `GetClientRect`, `GetCursorPos`, `GetForegroundWindow`, `ScreenToClient` from `crate::ffi`.

2. **`crates/hromatite-eac`**:
   - `Cargo.toml`: add `hromatite-win = { path = "../hromatite-win" }` and `hromatite-hook = { path = "../hromatite-hook" }` (keep `hromatite-core` until Stage 3/4 for `m3` and `photon`).
   - `src/eacimage.rs`: switch `hromatite_core::{ffi, safe, veh}` → `hromatite_win::{ffi, memory as safe, veh}`; replace `live::walk` and `live::region_at` `VirtualQuery` calls with `hromatite_win::memory::query_region`.
   - `src/keyhunt.rs`: delete `#[cfg(windows)] read_block` inline `ReadProcessMemory`/`GetCurrentProcess` → call `hromatite_win::memory::read_partial(addr, dst)`; delete `#[cfg(windows)] MemoryBasic` + `enumerate_regions` inline `VirtualQuery` + `#[allow(clashing_extern_declarations)]` → iterate `hromatite_win::procs::regions(65_536)` with `r.readable()` and `hromatite_win::ffi::{MEM_IMAGE, MEM_MAPPED}`.
   - `src/eos.rs`: delete `mod win` + unsafe `resolve_export` (`1517-1615`) → use `hromatite_win::sys::module_handle` and `hromatite_win::pe::resolve_live_export`; switch `hromatite_core::{log, GAME_*, procs, safe}` (`1354, 1404-1408, 2801, 2895, 2898, 2938, 2940`) → `hromatite_win::{log, GAME_*, procs, memory as safe}` (and `hromatite_core::iat` → `hromatite_hook::iat`).
   - `src/evp.rs`: replace `readable`/`slice`/`read_u32` (`hook::win::VirtualQuery`/`MemBasic`) → `hromatite_win::memory::{read_bytes, read_u32}` (eliminating `unsafe` in `log_call` and `note_lengths`).
   - `src/m3probe.rs:17`: switch `procs, safe, GAME_ASSEMBLY` to `hromatite_win::{procs, memory as safe, GAME_ASSEMBLY}` (keeping `hromatite_core::m3` until Stage 4).

3. **`Hromatite` (`src/main.rs`) & `crates/hromatite-dll/src/lib.rs`**:
   - Zero edits required in Stage 2 (they continue compiling through `hromatite-core`'s re-exports until Stage 3 separates `hromatite-injector` and `hromatite-runtime`).

---

## 5. Verification Method

After implementing `hromatite-win` (together with `hromatite-hook` in Stage 2):
1. **Windows debug & release builds**:
   - `cargo build --workspace` (exit 0, 0 warnings)
   - `cargo build --workspace --release` (exit 0, 0 warnings)
   - `cargo build -p hromatite-win --no-default-features` and `cargo build -p hromatite-win --features remote` (verify `"remote"` feature isolation)
2. **Windows test compilation & Clippy (safety lints)**:
   - `cargo test --workspace --no-run` (exit 0)
   - `cargo clippy --workspace --all-targets` (0 warnings, 0 errors — enforces `unsafe_op_in_unsafe_fn`, `clippy::undocumented_unsafe_blocks`, `clippy::missing_safety_doc`)
3. **Linux host unit tests & Clippy**:
   - `cargo test --workspace --target x86_64-unknown-linux-gnu` (all `hromatite-win`, `hromatite-hook`, `hromatite-core`, `hromatite-eac`, `hromatite-features`, `hromatite-dll`, `hromatite-abi`, `hromatite-commands`, `hromatite-nuklear` tests pass, 0 failures)
   - `cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu`
4. **Dependency graph & boundary checks**:
   - `cargo tree -p hromatite-win --edges normal` (only `std`/`core`, 0 internal crate deps)
   - `cargo tree -p hromatite-hook --edges normal` (only `hromatite-win`)
   - `grep_search` across `crates/` for `unsafe extern "system"` confirms only `hromatite-win/src/ffi.rs` (and test-only `eac/tests/eos_install.rs`) contains Win32 `extern "system"` blocks.
5. **Invalidation conditions**: Any failure to resolve `#[no_mangle]` stubs on Linux host test binaries, any `missing_safety_doc`/`undocumented_unsafe_blocks` clippy error, or `veh` pre-handler failing to invoke `gcguard::recover_boehm_abort` invalidates the extraction until fixed.
