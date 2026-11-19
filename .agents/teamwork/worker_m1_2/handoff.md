# Handoff Report — Worker M1 Iteration 2 (`worker_m1_2`)

**Type:** Hard

---

## 1. Observation

1. Direct source spot-checks against HEAD `5064586` confirmed:
   - `crates/hromatite-core/src/lib.rs:45-47`: defines `GAME_EXE`, `GAME_ASSEMBLY`, `UNITY_PLAYER`, referenced across `src/main.rs` (`hromatite-injector`), `crates/hromatite-eac/src/eos.rs:1404-1406`, `crates/hromatite-core/src/raw/photon.rs`, `buildinfo.rs`, `gcthreads.rs`, `m3.rs`, `il2cpp.rs`, `playerloop.rs`, `gcguard.rs`, `console.rs`, `hostapi.rs`, `hot.rs`.
   - `crates/hromatite-core/src/raw/memscan.rs:264-274` & `crates/hromatite-core/src/raw/photon.rs:1389-1391`: `photon.rs` calls both `memscan::stash_cstr` and `memscan::call3`.
   - `crates/hromatite-commands/src/lib.rs:386-393`: `"eac.module"` currently has `scope: Scope::Feature`.
   - `crates/hromatite-eac/src/eos.rs:1047-1052, 1064-1068`: `EAC_KEYHUNT_EVP=1` calls `install_evp(module, &mut reports)` at line `1049`; `EAC_M3_PROBE=1` calls `crate::m3probe::probe()` at line `1065`.
   - `crates/hromatite-core/tests/report_stream.rs:31`: imports `hromatite_core::photon::parse_payload_records` only.
   - `tools/hromatite-reload.sh:125, 174, 180` and `tools/test-hromatite-hot.sh:73, 88`: emit/assert `ABI=1` in manifest text alongside `raw/hot.rs:178`.
2. Updated `/home/wilpdrake/RustroverProjects/Hromatite/docs/REFACTOR_AUDIT.md` in place:
   - **§4.1 (line 169):** noted `eos::install_evp` is called at `eos.rs:1049` under `EAC_KEYHUNT_EVP=1`, requiring redirect to `evp::install` or flag retirement upon deletion.
   - **§6.1 (line 206):** reclassified `tools/test_gc_detour_abi.py` from `KEEP` to `REFACTOR (Stage 4–5) / ARCHIVE`, citing 2/4 baseline failures on `5064586`, hardcoded `crates/hromatite-core/src/raw/gcguard.rs`, and its assertion on Stage-5-deleted `gc_context_detour`.
   - **§6.2 (lines 228, 230):** noted `tools/hromatite-reload.sh:125,174,180` and `tools/test-hromatite-hot.sh:73,88` (`ABI=1` manifests) must be updated to `ABI=2` alongside `raw/hot.rs:178` in Stage 5.
   - **§7.1–§7.3 (lines 253–296):**
     - Mapped `core/src/crc.rs` (`crc32c_eac`, `seal_checksum`) -> `hromatite-abi::crc`, `hromatite-il2cpp::photon` + `tests/frames.rs` -> `hromatite_abi::crc`, `hromatite-eac` -> `hromatite-abi` dep + `digest.rs` re-export, and Stage 4 deletion of `core/src/crc.rs`.
     - Detailed the 3 Stage 3 decouplings for `hromatite-features` + `hromatite-eac = { default-features = false }`: (a) pure `eac::eos::state` (`Hooks`/`HookEvent`/`Counters`/`Report`/`EXPORTS`/`EVENTS`/`Sink`) always compiled, `eos/{hooks,pipeline,diag}`, `evp`, `iat::status` under `"runtime"` (`Hooks::install()` returns `Err` when `not(feature = "runtime")`); (b) `keyhunt::Hunter` + OS memory scans under `"runtime"` over `hromatite-win`, removing `hunter` from `features/src/eac.rs` and ticking `Hunter` in `hromatite-runtime`; (c) `"eac.module"` moved from `Scope::Feature` to `Scope::Resident` in `hromatite-commands` and dispatched in `hromatite-runtime`, keeping `eacimage/{elf,locator}` pure and gating `live`/`NativeMemRead`/`command` under `"runtime"`.
     - Explicitly dropped `m3probe::probe()` at `eos.rs:1065` and deleted `eac/m3probe.rs` in Stage 4 (superseded by `discover::il2cpp_anchors` + `buildinfo`).
     - Routed `memscan::call3` with `stash_cstr` and `GAME_EXE`/`GAME_ASSEMBLY`/`UNITY_PLAYER` -> `hromatite-win` (with `hromatite-il2cpp` reuse/re-export).
     - Clarified `crypto.rs` temporary move into `hromatite-injector` in Stage 3 (with `manualmap.rs`) prior to `--seal`/`crypto.rs` deletion in Stage 5, and Stage 2–4 drop of `m3dump.rs` and `core`-only dead code (`gamebuild::is_supported`, `hlog!`, `discover::Image::data`, unused `ffi`/`memscan`/`memory` spans).
     - Added explicit retention/placement entries for `features/src/{game_fixture.rs, test_support.rs}`, `crates/test-target`, and `eac/tests/{eos_install.rs, epochs.rs, hook_engine.rs -> hromatite-hook, keyhunt_scan.rs}` + `tests/fixtures/`.
3. Ran `git status --short && git diff --stat` and `git diff`:
   - `git status --short`: `?? .agents/`, `?? PROJECT.md`, `?? docs/REFACTOR_AUDIT.md`
   - `git diff`: empty (no tracked files modified).

---

## 2. Logic Chain

1. Placing `crc32c_eac`/`seal_checksum` in `hromatite-abi::crc` allows `hromatite-il2cpp::photon`, `tests/frames.rs`, and `hromatite-eac::digest` to share a single `#![forbid(unsafe_code)]` implementation without introducing `hromatite-il2cpp -> hromatite-eac` or violating `hromatite-features` isolation (Obs. 2).
2. Splitting pure `eos::state` and pure `eacimage/{elf,locator}` from `"runtime"`-gated `eos/{hooks,pipeline,diag}`, `evp`, `keyhunt::Hunter`, and `eacimage/{live,report}`, combined with moving `"eac.module"` to `Scope::Resident` and ticking `Hunter` in `hromatite-runtime`, resolves all 3 compilation/dependency blockers in `hromatite-features` under `hromatite-eac = { default-features = false }` without expanding `HostApiV1` (Obs. 1, 2).
3. Dropping `m3probe::probe()` at `eos.rs:1065` and deleting `eac/m3probe.rs` in Stage 4 removes the forbidden `hromatite-eac -> hromatite-il2cpp` edge while relying on deterministic `discover::il2cpp_anchors` + `buildinfo` (Obs. 1, 2).
4. Temporarily placing `crypto.rs` in `hromatite-injector` in Stage 3, dropping `m3dump.rs` and `core`-only dead code during Stage 2–4 extraction/dissolution, and aligning `hromatite-reload.sh` / `test-hromatite-hot.sh` / `test_gc_detour_abi.py` / `EAC_KEYHUNT_EVP` in Stages 4–5 eliminates all Stage 3–5 sequencing contradictions while leaving all source/config/tools/`AGENTS.md` files untouched in Stage 1 (Obs. 1–3).

---

## 3. Caveats

- Only `docs/REFACTOR_AUDIT.md` was modified; `PROJECT.md` was read-only per exclusive write ownership and already matches the resolved architecture (no contradictions observed between `PROJECT.md` and the updated `docs/REFACTOR_AUDIT.md`).
- No `cargo` builds/tests were re-run in Iteration 2 because zero Rust source files or Cargo manifests were modified (`git diff` empty).

---

## 4. Conclusion

All 5 review/challenge findings (`crc.rs`↔`photon`/`frames.rs`/`eac`, `hromatite-features` + `hromatite-eac` `default-features=false` 3 decouplings, `eos.rs:1065` + `m3probe.rs`, Stage 4/5 sequencing & omitted file placements, and `test_gc_detour_abi.py`/`hromatite-reload.sh`/`test-hromatite-hot.sh`/`eos::install_evp`) are cleanly resolved in `/home/wilpdrake/RustroverProjects/Hromatite/docs/REFACTOR_AUDIT.md`, with `git status --short` and `git diff` verified clean of any source/config changes.

---

## 5. Verification Method

From `/home/wilpdrake/RustroverProjects/Hromatite`:
```bash
git status --short
git diff
```
Inspect `/home/wilpdrake/RustroverProjects/Hromatite/docs/REFACTOR_AUDIT.md` lines `169`, `206`, `228–231`, `253–296` against `/home/wilpdrake/RustroverProjects/Hromatite/PROJECT.md` and the cited source lines (`crates/hromatite-core/src/lib.rs:45-47`, `crates/hromatite-commands/src/lib.rs:386-393`, `crates/hromatite-eac/src/eos.rs:1049,1065`, `tools/hromatite-reload.sh:125,174,180`, `tools/test-hromatite-hot.sh:73,88`).

**Invalidation conditions:** Any subsequent edit to `docs/REFACTOR_AUDIT.md`, `PROJECT.md`, or workspace sources/manifests.
