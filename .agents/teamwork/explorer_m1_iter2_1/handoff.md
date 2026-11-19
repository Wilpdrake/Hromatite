# Handoff Report — Explorer (`explorer_m1_iter2_1`, Milestone M1 Iteration 2)

**Type:** Hard  
**Summary:** Verified exact, dependency-invariant-compliant migration strategies for all 3 contradictions (`crc.rs`↔`photon`, `features/eac.rs`↔`eac` `default-features=false`, `eos.rs:1065`→`m3probe`) plus Stage 4/5 `crypto.rs`/`test_gc_detour_abi.py` sequencing so `docs/REFACTOR_AUDIT.md` §7.2–7.3 and `PROJECT.md` are 100% contradiction-free.

---

## 1. Observation

### 1.1 Item 1 — `crc32c_eac` & `seal_checksum` (`crc.rs`, `digest.rs`, `photon.rs`, `frames.rs`)
- `crates/hromatite-core/src/crc.rs:1-80` (`#![forbid(unsafe_code)]`): defines `POLY: u32 = 0x82F6_3B78`, `pub fn crc32c_eac(data: &[u8]) -> u32`, and `pub fn seal_checksum(frame: &mut [u8], at: usize) -> bool` using only `core` primitives. Lines `11-14` note it was duplicated from `hromatite_eac::digest::crc32c_eac` solely so `raw::photon` would not depend on `hromatite-eac`.
- `crates/hromatite-eac/src/digest.rs:195-208`: defines identical `pub fn crc32c_eac(data: &[u8]) -> u32` (used in `codec.rs`, `report.rs`, `forge.rs`, `eos.rs` tests), without `seal_checksum`.
- `crates/hromatite-core/src/raw/photon.rs:1054-1065`: `retarget_frame` calls `crate::crc::seal_checksum(&mut out, EAC_CHECKSUM_OFF)` at line `1063`; unit test at `:2643` calls `crate::crc::crc32c_eac(&zeroed)`.
- `crates/hromatite-core/tests/frames.rs:21, 25-26, 95, 100-108, 131` (`#![forbid(unsafe_code)]`): directly imports and tests `hromatite_core::crc::{crc32c_eac, seal_checksum}` alongside `hromatite_core::photon::{EAC_CHECKSUM_OFF, EAC_FRAME_LEN, parse_payload_records, retarget_frame}`.
- `crates/hromatite-abi/Cargo.toml:1-14` & `crates/hromatite-abi/src/lib.rs:6`: `#![forbid(unsafe_code)]`, zero crate dependencies, already depended upon by `hromatite-il2cpp`, `hromatite-features`, `hromatite-runtime`, and `hromatite-console`.

### 1.2 Item 2 — `hromatite-features` vs `hromatite-eac` (`default-features = false`)
- `grep_search` for `hromatite_eac` across `crates/hromatite-features/` identifies all uses:
  1. **Pure protocol/session/log/tap/tailer/codec** (`eac.rs:25-28, 30-33, 719, 838`; `lib.rs:722-736`): `codec`, `digest::sha256`, `encode_beat`, `eventlog`, `payload`, `session`, `tailer`, `tap`, `clock::now_ms`. None use `unsafe` or Win32/core.
  2. **`Hooks` & `HookEvent`** (`eac.rs:34, 90, 123, 156, 164-232, 239, 244, 259, 270, 278, 504, 507, 611, 784`; `lib.rs:672-686, 707`):
     - In `crates/hromatite-eac/src/eos.rs`, `HookEvent` (`395-431`), `Counters` (`435-472`), `Report` (`476-487`), `EXPORTS` (`97-119`), `EVENTS` (`322`), `Sink`/`LogFile` (`298-383`), atomic counters (`149-231`), and `Hooks` (`490-742`) are pure `std` EXCEPT: `Hooks::install(&mut self)` (`744-1090`, called from `poll` @ `540` & `install_now` @ `570`), `hromatite_core::iat::status()` (`682`), and `EVP_CALLS`/`EVP_TARGETS` (`726-740`).
     - In `crates/hromatite-features/src/lib.rs:657-710` (`hook_observations_drive_epochs_and_the_jsonl_log`), the test uses `Hooks::observe(HookEvent::...)`, `(api().tick)()`, and `send("eac.hooks")`, asserting `"eac.hooks: installed=false"` (`707`).
  3. **`keyhunt::Hunter`** (`eac.rs:29, 92, 125, 512-513, 614-618`):
     - `crates/hromatite-eac/src/keyhunt.rs`: `SEED_KEY`, `candidate_keys`, `Region`, `parse_maps`, `Matcher`, `Hit`, `FrameProbe`, `enabled`, `record_frame`, `probe`, `probe_from_capture`, `hexdump` are pure `std`. However, `Hunter` (`600-1060`) drives live memory scans calling `read_block` (`190-256`: `ReadProcessMemory`/`GetCurrentProcess` on Windows, `process_vm_readv`/`getpid` on non-Windows) and `enumerate_regions` (`298-369`: `VirtualQuery` on Windows).
     - `crates/hromatite-abi/src/lib.rs:50-62`: `HostApiV1` exposes `log`, `read`, `write`, `guarded_call` — it has **no** `VirtualQuery`/region-enumeration callback. Zero tests in `crates/hromatite-features/` exercise `Hunter`.
  4. **`eos::module_report`** (`eac.rs:547`: `"eac.module" => (hromatite_eac::eos::module_report(arg), STATUS_OK)`):
     - `crates/hromatite-eac/src/eos.rs:1387-1393` calls `crate::eacimage::command(arg)`.
     - `crates/hromatite-eac/src/eacimage.rs`: `live::probe_command` (`798-1025`) uses `hromatite_core::ffi::VirtualQuery` and `hromatite_core::safe`; `NativeMemRead` (`1237-1291`) uses `hromatite_core::veh::call_with` (`1277`) to read `/proc/<pid>/maps`-backed memfd ELF regions because Wine's `VirtualQuery` reports native memfd mappings as unmapped (`1158-1163`). Thus `HostApiV1.read` (which goes through `safe::read_upto` / `VirtualQuery`) cannot read the memfd ELF image, and `hromatite-features` cannot call `veh::call_with` or `VirtualQuery` (`PROJECT.md` Rule 5). Zero tests in `crates/hromatite-features/` call `eac.module`.
- `grep_search` for `hromatite_core` and `extern "` in `crates/hromatite-eac/` confirms all remaining Win32/hook/core references live in `eos.rs`, `evp.rs`, `eacimage.rs` (`mod live`, `NativeMemRead`), `keyhunt.rs` (`read_block`, `enumerate_regions`), `hook.rs` (moving to `hromatite-hook`), `m3probe.rs`, and `forge.rs:824, 834` (`pump_photon` -> `hromatite_core::photon`).

### 1.3 Item 3 — `m3probe.rs` vs `discover.rs` (`eos.rs:1065`)
- `crates/hromatite-eac/src/m3probe.rs:17-30, 74-154` is called at a single site (`crates/hromatite-eac/src/eos.rs:1064-1068` under `env_flag("EAC_M3_PROBE")`). It hardcodes `LEGACY_TABLE_RVA = 0x0e66_f0d0` and `Il2CppClass` offsets (`0x10`/`0x18`), scanning `±8 MiB` around the legacy RVA if `score < 24/32`.
- `crates/hromatite-core/src/raw/discover.rs:197-208, 477-508` (`class_table_global`, `il2cpp_anchors`) + `crates/hromatite-core/src/buildinfo.rs:331-335, 460` already derive `s_TypeInfoDefinitionTable` (`class_table`), `il2cpp_image_get_class`, and `il2cpp_get_corlib` deterministically from `.text` code shapes without hardcoded RVAs or brute-force scanning.

### 1.4 Item 4 — `tools/test_gc_detour_abi.py` and `manualmap.rs` (`--seal` / `crypto.rs`)
- `tools/test_gc_detour_abi.py` has 4 tests:
  1. `test_live_jump_restores_original_rcx_and_rdx` (`35-63`): disassembles `gc_context_detour` (`crates/hromatite-core/src/raw/hook.rs:1249`, called only by dead `install_legacy_gc_context_guard` at `:1269`, scheduled for Stage 5 deletion).
  2. `test_close_callsites_tail_jump_to_their_continuations` (`66-85`): disassembles `defer_close_detour` & `close_after_unlink_detour` (`crates/hromatite-core/src/raw/gcguard.rs:406, 447`, active code moving to `hromatite-il2cpp`).
  3. `test_gc_guard_takes_no_lock_across_a_collection` (`88-102`): reads hardcoded `ROOT / "crates/hromatite-core/src/raw/gcguard.rs"` (`97`, deleted in Stage 4) and asserts `suspend_entry_detour` does not call `EnterCriticalSection`/`LeaveCriticalSection`/`sanitize_locked` (currently **fails** at `gcguard.rs:357, 363, 371`).
  4. `test_suspend_call_site_stub_keeps_the_router_abi` (`105-132`): searches release DLL for `gc_suspend_detour`, which does not exist anywhere in `crates/` (currently **fails**).
- `crates/hromatite-core/src/manualmap.rs:34-73` (`seal_image`) calls `crate::crypto::{PAGE, xor_page}` (`40, 61`). `grep_search` confirms `manualmap.rs` is the **only** caller of `crates/hromatite-core/src/crypto.rs` in the entire workspace (`src/main.rs:157, 179-181` parses `--seal` into `manualmap::Options::seal`).

---

## 2. Logic Chain

1. **Contradiction 1 (`crc.rs` ↔ `photon` ↔ `eac`):**
   - By §1.1, `photon::emulate::retarget_frame` and `tests/frames.rs` move to `hromatite-il2cpp`, which depends on `{hromatite-win, hromatite-hook, hromatite-abi}` and MUST NOT depend on `hromatite-eac`.
   - `tests/frames.rs` directly tests `crc32c_eac` and `seal_checksum` (not just `retarget_frame`), so callback-inverting `retarget_frame` alone would still leave `frames.rs` unable to assert `crc32c_eac`/`seal_checksum` without duplication.
   - Moving `crc32c_eac` and `seal_checksum` (from `core/src/crc.rs`, `#![forbid(unsafe_code)]`, 0 external/std deps) into `hromatite-abi::crc`, and having `hromatite-eac` depend on `hromatite-abi` (with `hromatite-eac::digest` re-exporting `crc32c_eac` and optionally `seal_checksum`), gives `hromatite-il2cpp`, `frames.rs`, and `hromatite-eac` a single source of truth with zero unsafe code and zero forbidden edges (note: `hromatite-abi` is already a dependency of `hromatite-features`, `hromatite-il2cpp`, `hromatite-runtime`, and `hromatite-console`, so `hromatite-eac -> hromatite-abi` introduces no cycles and adds no Win32/hook deps to `hromatite-features`).

2. **Contradiction 2 (`hromatite-features` + `hromatite-eac = { default-features = false }`):**
   - By §1.2, three concrete call sites in `hromatite-features` currently pull Win32/hook/core code from `hromatite-eac`:
     - **(2a) `Hooks` / `HookEvent`:** `features/src/eac.rs` and `features/src/lib.rs:672-686` only need the pure observation/event-queue/counter state (`HookEvent`, `Counters`, `Report`, `Hooks` without the Win32 `install()`/`iat::status()`/`evp` bodies). Extracting `eos/state.rs` (always compiled in `hromatite-eac`) and gating `eos/{hooks, pipeline, diag}.rs` + `evp.rs` behind `#[cfg(feature = "runtime")]` — where `Hooks::install()` returns `Err("eos hooks require runtime feature")` when `not(feature = "runtime")` — makes `hromatite-features` and `hook_observations_drive_epochs_and_the_jsonl_log` compile and pass unchanged.
     - **(2b) `keyhunt::Hunter`:** Because `HostApiV1` has no region-enumeration callback and `features` cannot link `hromatite-win`, `Hunter`'s live scan cannot run in `hromatite-features` without `hromatite-win`. Gating `Hunter` + `read_block` + `enumerate_regions` behind `feature = "runtime"` (replacing duplicate `ReadProcessMemory`/`VirtualQuery` FFI with `hromatite-win`), removing `hunter: Hunter` from `features/src/eac.rs`, and ticking `Hunter` in `hromatite-runtime` when `EAC_KEYHUNT=1` removes all FFI from `keyhunt` in `hromatite-features` (and actually improves `keyhunt`, since `hromatite-runtime` owns `PROBE` directly; `keyhunt_scan.rs` runs under default `"runtime"` feature and passes unchanged). Pure key-table/parser helpers (`SEED_KEY`, `candidate_keys`, `Region`, `parse_maps`, `Matcher`, `hexdump`) can remain pure or be gated with `keyhunt` (with `[[test]] name = "keyhunt_scan" required-features = ["runtime"]` if gated).
     - **(2c) `eos::module_report` (`"eac.module"`):** `eacimage::command` needs `veh::call_with` (for Wine memfd ELF reads that `VirtualQuery` misses) and `VirtualQuery` (`mod live`), which `HostApiV1` does not provide and `hromatite-features` cannot link. Changing `"eac.module"` in `hromatite-commands/src/lib.rs:392` from `Scope::Feature` to `Scope::Resident` and dispatching `"eac.module"` in `hromatite-runtime` (via `hromatite_eac::eacimage::command(arg)` / `eos::module_report`, compiled with `"runtime"`), while removing `"eac.module"` from `features/src/eac.rs:547` (keeping pure ELF/`maps` parser `eacimage/{elf, locator}` always compiled and gating `eacimage/report.rs` / `live` / `NativeMemRead` under `"runtime"`), eliminates the last Win32/VEH dependency from `hromatite-features` with zero `HostApiV1` ABI changes and zero broken tests.

3. **Contradiction 3 (`eos.rs:1065` `m3probe::probe()`):**
   - By §1.3, `m3probe.rs` was a pre-`discover.rs` hardcoded-RVA heuristic (`0x0e66_f0d0`, `±8 MiB` scan) whose sole caller is `eos.rs:1065`. Because `discover::il2cpp_anchors` + `buildinfo` already derive `s_TypeInfoDefinitionTable` deterministically from code shapes, `m3probe::probe()` in `eos.rs:1065` can be cleanly dropped in Stage 4 and `eac/m3probe.rs` deleted (or, if `EAC_M3_PROBE=1` were ever wanted for debug, moved to `hromatite-il2cpp` and invoked from `hromatite-runtime` — never from `hromatite-eac`).

4. **Sequencing (`crypto.rs`, `m3dump.rs`, `tools/test_gc_detour_abi.py`):**
   - By §1.4, `manualmap.rs` moves to `hromatite-injector` in Stage 3 and still calls `crypto::{PAGE, xor_page}` until `--seal` removal in Stage 5; therefore `crypto.rs` must move temporarily into `hromatite-injector` in Stage 3 (so `hromatite-core` can be cleanly dissolved in Stage 4) and then be deleted alongside `--seal` / `seal_image` in Stage 5.
   - `m3dump.rs` and `core`-only dead code (`gamebuild::is_supported`, `hlog!`, `discover::Image::data`, unused `ffi`/`memscan`/`memory` spans) cannot remain in a deleted `hromatite-core` after Stage 4: explicitly drop unused items during Stage 2–4 extraction/dissolution (or mark as dropped at Stage 4 dissolution) while keeping Stage 5 for legacy GC guard (`gc_context_detour` etc.), `hookat`/`unhook`/`hooks`/`tick`, `--seal`/`L2B`/`crypto.rs`, `hot` unification, and `tools/` cleanup.
   - `tools/test_gc_detour_abi.py` must be reclassified in `docs/REFACTOR_AUDIT.md:206` (§6.1) from `KEEP` to `REFACTOR (Stage 4–5) / ARCHIVE` noting it already fails 2/4 on `5064586`, hardcodes `crates/hromatite-core/src/raw/gcguard.rs` (moved in Stage 4), and depends on `gc_context_detour` (deleted in Stage 5).

---

## 3. Caveats

- All findings are derived from static source/manifest inspection (`view_file`, `grep_search`, `list_dir`) against HEAD `5064586`; no source edits or `cargo` commands were executed by this explorer (build/test baseline was verified by `reviewer_m1_2` and `challenger_m1_1`).
- For `hromatite-eac` Cargo feature gating (`default = ["runtime"]`), `[[test]]` entries `eos_install` and `keyhunt_scan` in `crates/hromatite-eac/Cargo.toml` will need `required-features = ["runtime"]` if `cargo test -p hromatite-eac --no-default-features` is run, though workspace `cargo test` runs with default features enabled.

---

## 4. Conclusion

Exact migration edits required in `docs/REFACTOR_AUDIT.md` §6.1, §7.2–7.3 and `PROJECT.md` to eliminate all contradictions:

| # | Contradiction | Exact Resolution for `docs/REFACTOR_AUDIT.md` & `PROJECT.md` |
|---|---|---|
| 1 | `core/src/crc.rs` ↔ `photon` vs `eac` | Move `crc32c_eac` + `seal_checksum` (`#![forbid(unsafe_code)]`) into **`hromatite-abi::crc`**; `hromatite-il2cpp::photon` + `tests/frames.rs` use `hromatite_abi::crc`; add `hromatite-abi` dep to **`hromatite-eac`** and re-export `crc32c_eac` in `eac::digest` (deleting `core/src/crc.rs` in Stage 4). Update `PROJECT.md` `hromatite-abi` & `hromatite-eac` rows. |
| 2 | `hromatite-features` + `hromatite-eac` (`default-features = false`) | In **Stage 3**: (a) split pure `Hooks`/`HookEvent`/`Counters`/`Report`/`EXPORTS`/`EVENTS`/`Sink` into always-compiled `eac::eos::state` (and gate `eos/{hooks,pipeline,diag}`, `evp`, `iat::status` behind `"runtime"`; `Hooks::install()` returns `Err` when `not(feature = "runtime")`); (b) gate `keyhunt::Hunter` + OS memory scan behind `"runtime"` (over `hromatite-win`), remove `hunter` from `features/src/eac.rs`, and drive `Hunter` in `hromatite-runtime`; (c) change `"eac.module"` in `hromatite-commands` from `Scope::Feature` to `Scope::Resident` and dispatch it in `hromatite-runtime` (gating `eacimage` `live`/`NativeMemRead`/`command` under `"runtime"` and keeping ELF/`maps` parser pure). |
| 3 | `eos.rs:1065` `m3probe::probe()` | Drop `m3probe::probe()` from `hromatite-eac::eos` in **Stage 4** (and delete `eac/m3probe.rs`, already superseded by `discover::il2cpp_anchors` + `buildinfo`, or move to `hromatite-il2cpp` invoked only from `hromatite-runtime`). |
| 4 | Stage 4/5 sequencing (`crypto.rs`, `m3dump.rs`, `test_gc_detour_abi.py`) | Move `crypto.rs` temporarily to `hromatite-injector` in Stage 3 (with `manualmap.rs`), then delete `--seal`/`seal_image`/`crypto.rs` in Stage 5; drop `core`-only dead code (`m3dump.rs`, `gamebuild::is_supported`, `hlog!`, `discover::Image::data`, unused `ffi`/`memscan`/`memory` spans) during Stage 2–4 extraction/dissolution; mark `tools/test_gc_detour_abi.py` (`§6.1` row 206) as `REFACTOR (Stage 4–5) / ARCHIVE` (already 2/4 fail on `5064586`, hardcodes `core/src/raw/gcguard.rs`, checks `gc_context_detour`). |

---

## 5. Verification Method

From `/home/wilpdrake/RustroverProjects/Hromatite`, verify every cited line and dependency:
```bash
rg -n "crc32c_eac|seal_checksum" crates/
rg -n "hromatite_eac" crates/hromatite-features/
rg -n "m3probe|module_report|NativeMemRead|call_with|VirtualQuery" crates/hromatite-eac/
rg -n "crypto" crates/ src/
```
Inspect:
- `crates/hromatite-core/src/crc.rs`, `crates/hromatite-core/src/raw/photon.rs:1054-1065,2643`, `crates/hromatite-core/tests/frames.rs:25-26`, `crates/hromatite-abi/Cargo.toml`
- `crates/hromatite-features/src/eac.rs:29,34,90-92,123-125,512-513,547,614-618`, `crates/hromatite-features/src/lib.rs:657-710`
- `crates/hromatite-eac/src/eos.rs:395-742,1063-1068,1387-1393`, `crates/hromatite-eac/src/keyhunt.rs:190-369,600-1060`, `crates/hromatite-eac/src/eacimage.rs:798-1025,1158-1163,1237-1291`
- `crates/hromatite-eac/src/m3probe.rs` vs `crates/hromatite-core/src/raw/discover.rs:197-208,477-508`
- `tools/test_gc_detour_abi.py`, `crates/hromatite-core/src/manualmap.rs:34-73`, `crates/hromatite-core/src/crypto.rs`.

**Invalidation conditions:** Any post-`5064586` edits to the files above, `PROJECT.md`, or `docs/REFACTOR_AUDIT.md`.
