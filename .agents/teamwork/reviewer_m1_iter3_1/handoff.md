# Handoff Report — Reviewer 1 (`reviewer_m1_iter3_1`, Milestone M1 Iteration 3)

**Type:** Hard  
**Verdict:** `REQUEST_CHANGES`  
**Overall Risk Assessment:** MEDIUM (Items 2, 3, 4, `tools/` notes, and git cleanliness are resolved; one Stage 3 compilation contradiction remains in `report.rs:644` & `forge.rs:1210` calling `forge::pump_photon`)

---

## 1. Observation

1. **Git cleanliness (`git status --short && git diff --stat && git diff`)**:
   - Output:
     ```text
     ?? .agents/
     ?? PROJECT.md
     ?? docs/REFACTOR_AUDIT.md
     ```
   - `git diff --stat` and `git diff` returned empty with exit `0`; zero tracked files were modified.

2. **Item 1 (`challenger_m1_iter2_1` Catch-22: `report.rs` / `forge.rs` / `m3probe.rs` -> `hromatite_core`)**:
   - `docs/REFACTOR_AUDIT.md:264` (`§7.2`, `hromatite-eac`) places pure `forge` frame/header/ring types/helpers in always-compiled pure core, and places `forge::pump_photon` under `feature = "runtime"` in Stage 3: `forge::pump_photon (закрыт #[cfg(feature = "runtime")] в Stage 3, в Stage 4 инвертируется через callback/trait)`. `docs/REFACTOR_AUDIT.md:281` (`§7.3` Stage 3.3) says `закрыв forge::pump_photon (или инвертировав его через callback) и m3probe (+ вызов eos.rs:1065) под #[cfg(feature = "runtime")] уже в Stage 3`. `PROJECT.md:90, 118-119` similarly separates pure `forge` from `Runtime feature ("runtime"): ... forge::pump_photon`.
   - Direct inspection of `crates/hromatite-eac/src/report.rs:600-662, 714, 1553-1590` confirms:
     - `Stream::new` (`report.rs:714`, part of pure `report`) unconditionally calls `spawn_photon_sender(&report, config)`.
     - `spawn_photon_sender` (`report.rs:644`) **unconditionally** calls `let delivery = forge::pump_photon();`.
     - Inline test `report.rs:1553` (`the_photon_via_renders_into_the_ring_of_the_emitter`) exercises `Stream::new` with `Via::Photon`.
   - Direct inspection of `crates/hromatite-eac/src/forge.rs:1192-1227` confirms:
     - Inline test `forge.rs:1210` (`the_ring_queues_frames_and_reports_an_unarmed_emitter`) **unconditionally** calls `let delivery = pump_photon();` and asserts `delivery.unarmed` (`1206-1213`).
   - Direct inspection of `crates/hromatite-eac/src/lib.rs:51, 55, 57, 70` shows unconditional `pub mod evp;`, `pub mod m3probe;`, `pub mod hook;`, and `pub use hook::Hook;`.

3. **Item 2 (`challenger_m1_iter2_1`: `eos::state` for `Hooks::status_lines` & `Hooks::counters`)**:
   - Verified `crates/hromatite-eac/src/eos.rs:587-608, 659-742`: `Hooks::counters` and `Hooks::status_lines` depend on the 26 global atomics (`OUT_MESSAGES`..`FORGE_NEXT_SEQ`, `CALLS`, `PROTECT_*`, `OVERHEAD_*`, `BLIND*`, `FORGE_*`, `REPLAY`, `DROP_AFTER_MS`), `epoch_line()`/`EPOCH`, `hromatite_core::iat::status()` (`eos.rs:682`), and `EVP_CALLS`/`EVP_TARGETS` (`eos.rs:726-740`).
   - Verified `docs/REFACTOR_AUDIT.md:264, 282` and `PROJECT.md:90, 118` explicitly include all 26 atomics and `EPOCH`/`epoch_line()` in `eos/state.rs`, return `Err` from `Hooks::install()` when `not(feature = "runtime")`, and gate `iat::status()` (`eos.rs:682`) and `EVP_CALLS`/`EVP_TARGETS` (`eos.rs:726-740`) under `#[cfg(feature = "runtime")]`.

4. **Item 3 (`challenger_m1_iter2_1`: Inline tests in `eacimage.rs` & `keyhunt.rs` + Linux `keyhunt_scan`)**:
   - Verified `crates/hromatite-eac/src/eacimage.rs:1568, 1633` and `crates/hromatite-eac/src/keyhunt.rs:223-256, 1160`.
   - Verified `docs/REFACTOR_AUDIT.md:264, 283-284` and `PROJECT.md:90, 119` explicitly gate inline tests `eacimage.rs:1568, 1633` and `keyhunt.rs:1160` under `#[cfg(feature = "runtime")]`, require `required-features = ["runtime"]` on `tests/keyhunt_scan.rs` and `tests/eos_install.rs`, and preserve `#[cfg(not(windows))]` `process_vm_readv` (`keyhunt.rs:223-256`).

5. **Item 4 (`challenger_m1_iter2_1`: `"eac.module"` ordering & `memory.rs` test preservation in `hromatite-features`)**:
   - Verified `crates/hromatite-features/src/feature.rs:172, 374-380` and `crates/hromatite-features/src/memory.rs:1-82`.
   - Verified `docs/REFACTOR_AUDIT.md:268, 280, 284` and `PROJECT.md:90, 124` explicitly reclassify `"eac.module"` to `Scope::Resident`, place resident-only checking **before** `command == "eac" || command.starts_with("eac.")` (`feature.rs:172`), and relocate `&Host` helpers `qwords`/`bytes`, `parse_addr`/`decode_hex`, and test `memory.rs:73` (`addresses_and_hex_writes_validate_their_format`) into `host.rs` or `feature.rs`.

6. **`tools/hromatite-reload.sh` & `tools/test-hromatite-hot.sh` notes (`reviewer_m1_iter2_2` / `challenger_m1_iter2_2`)**:
   - Verified `tools/hromatite-reload.sh:67` (`[ "$OLD_ABI" = 1 ]`), `:99` (`crates/hromatite-core/src/symbols.rs`), `:125, 174, 180` (`ABI=1`), and `tools/test-hromatite-hot.sh:73, 88` (`ABI=1`), `:225` (`crates/hromatite-core/src/symbols.rs`).
   - Verified `docs/REFACTOR_AUDIT.md:228, 230, 288, 292` and `PROJECT.md:92` accurately track `:99`/`:225` (`symbols.rs`) in Stage 4 and `:67,125,174,180` / `:73,88` (`ABI=1 -> 2`) in Stage 5.

---

## 2. Logic Chain

### Review Summary
**Verdict:** `REQUEST_CHANGES`

1. Items 2, 3, 4 from `challenger_m1_iter2_1`, the `hromatite-reload.sh`/`test-hromatite-hot.sh` notes, `PROJECT.md` alignment for those items, and `git diff` cleanliness are all verified and pass (Obs. 1, 3, 4, 5, 6).
2. Adversarial stress-testing of Item 1 under `hromatite-eac = { default-features = false }` reveals a concrete compilation break in `docs/REFACTOR_AUDIT.md:264` (and `PROJECT.md:119`): gating `forge::pump_photon` behind `#[cfg(feature = "runtime")]` in Stage 3 breaks **both** pure `report.rs:644` (`spawn_photon_sender`, called by `Stream::new` at `714` and tested at `1553`) **and** `forge.rs:1210` (`the_ring_queues_frames_and_reports_an_unarmed_emitter`) because neither call site nor `Delivery`/`pump_photon` non-runtime fallback is specified (Obs. 2).
3. Similarly, `crates/hromatite-eac/src/lib.rs:51, 55, 57, 70` (`evp`, `m3probe`, `hook`, `pub use hook::Hook`) must be gated behind `#[cfg(feature = "runtime")]` so `lib.rs` compiles under `default-features = false` (Obs. 2).

### Findings

#### [Major] `forge::pump_photon` Stage 3 gating breaks `report.rs:644` and `forge.rs:1210` under `default-features = false`
- **What:** `docs/REFACTOR_AUDIT.md:264` (`§7.2`, `hromatite-eac`) instructs gating `forge::pump_photon` under `#[cfg(feature = "runtime")]` in Stage 3 (`закрыт #[cfg(feature = "runtime")] в Stage 3, в Stage 4 инвертируется через callback/trait`), without providing a `#[cfg(not(feature = "runtime"))]` fallback or gating its callers in pure core.
- **Where:** `docs/REFACTOR_AUDIT.md:264, 281` (and `PROJECT.md:118-119`), affecting `crates/hromatite-eac/src/report.rs:644` (`spawn_photon_sender` / `Stream::new:714` / test `1553`) and `crates/hromatite-eac/src/forge.rs:1210` (`the_ring_queues_frames_and_reports_an_unarmed_emitter`).
- **Why:** Under `--no-default-features`, `report.rs` and `forge.rs` inline tests are compiled. Removing `forge::pump_photon` (or `Delivery`) when `not(feature = "runtime")` causes `E0425: cannot find function pump_photon` at `report.rs:644` and `forge.rs:1210`.
- **Suggestion:** Specify in `docs/REFACTOR_AUDIT.md:264, 281` (and `PROJECT.md:118-119`) that `Delivery` and `forge::pump_photon()` stay in pure `forge` with a `#[cfg(not(feature = "runtime"))]` unarmed stub returning `Delivery { unarmed: true, ..Delivery::default() }` (or a pure registered callback slot defaulting to unarmed) while only the `hromatite_core::photon` body is `#[cfg(feature = "runtime")]` in Stage 3 — **or** explicitly gate `spawn_photon_sender`'s `pump_photon()` call (`report.rs:644`) and `forge.rs:1210` under `#[cfg(feature = "runtime")]`. Also note `eac/src/lib.rs:51, 55, 57, 70` (`evp`, `m3probe`, `hook`, `pub use hook::Hook`) under `#[cfg(feature = "runtime")]`.

### Verified Claims
- `worker_m1_3` claim that `git diff` is empty and only `.agents/`, `PROJECT.md`, `docs/REFACTOR_AUDIT.md` are untracked → verified via `git status --short && git diff --stat && git diff` → **PASS**.
- `worker_m1_3` claim that Items 2, 3, 4 and `tools/hromatite-reload.sh:67,99` + `tools/test-hromatite-hot.sh:225` are accurately resolved in `docs/REFACTOR_AUDIT.md` (§6.2, §7.2, §7.3) and aligned with `PROJECT.md` → verified via `view_file` → **PASS**.
- `worker_m1_3` claim that Item 1 (`forge::pump_photon` gating under `#[cfg(feature = "runtime")]` in Stage 3) resolves `report.rs` compilation under `default-features = false` → verified against `report.rs:644, 714, 1553` and `forge.rs:1210` → **FAIL** (unconditional callers break if `pump_photon` is gated out without a pure unarmed fallback/callback or caller gating).

### Coverage Gaps
- Direct compilation test of `--no-default-features` was not executed (read-only audit phase; verified by tracing exact callers and `use` paths). Risk level: low once the blueprint fix above is applied.

---

## 3. Caveats

- Read-only review: no repository source or documentation files were modified.
- `PROJECT.md` (lines `90`, `118–119`) has the same `forge::pump_photon` under `Runtime feature ("runtime")` phrasing; `worker_m1_3` noted `PROJECT.md` was outside its write scope, so the orchestrator should keep `PROJECT.md:118-119` synchronized when updating `docs/REFACTOR_AUDIT.md`.

---

## 4. Conclusion

**Verdict: `REQUEST_CHANGES`**

Tracked git state is completely clean (`git diff` empty), and Items 2, 3, 4 plus `hromatite-reload.sh`/`test-hromatite-hot.sh` notes are accurately resolved in `docs/REFACTOR_AUDIT.md` and `PROJECT.md`. One concrete Stage 3 compilation defect remains in Item 1: `docs/REFACTOR_AUDIT.md:264` (and `PROJECT.md:119`) gates `forge::pump_photon` behind `#[cfg(feature = "runtime")]` in Stage 3 without providing a pure `#[cfg(not(feature = "runtime"))]` unarmed fallback (or gating `report.rs:644` and `forge.rs:1210`), which breaks `report.rs` (`Stream::new` → `spawn_photon_sender`) and `forge::tests::the_ring_queues_frames_and_reports_an_unarmed_emitter` under `default-features = false`.

---

## 5. Verification Method

From `/home/wilpdrake/RustroverProjects/Hromatite`:
1. Inspect `docs/REFACTOR_AUDIT.md` lines `228-230, 264, 268, 281-292` and `PROJECT.md` lines `90, 92, 118-119, 124`.
2. Inspect `crates/hromatite-eac/src/report.rs:600-662, 714, 1553`, `crates/hromatite-eac/src/forge.rs:823, 1210`, and `crates/hromatite-eac/src/lib.rs:51-70`.
3. Verify tracked-file cleanliness:
   ```bash
   git status --short && git diff --stat && git diff
   ```
   Expected: only untracked `.agents/`, `PROJECT.md`, `docs/REFACTOR_AUDIT.md`, empty `git diff`.
4. **Invalidation conditions:** Resolved once `docs/REFACTOR_AUDIT.md` (§7.2–§7.3) and `PROJECT.md` (§Interface Contracts `hromatite-eac`) specify either a pure `#[cfg(not(feature = "runtime"))]` unarmed `pump_photon()` fallback / callback slot or gating `report.rs:644` + `forge.rs:1210` (plus `eac/src/lib.rs` `runtime` mod/re-export gates).
