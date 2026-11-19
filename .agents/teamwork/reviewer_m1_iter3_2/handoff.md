# Handoff Report — Reviewer 2 (`reviewer_m1_iter3_2`, Milestone M1 Iteration 3)

**Type:** Hard  
**Verdict:** `REQUEST_CHANGES`

---

## 1. Observation

1. **Baseline verification commands (reproduced at commit `5064586`)**:
   - `cargo clippy --workspace --all-targets`: exit `0`, `0 warnings, 0 errors`.
   - `cargo test --workspace --target x86_64-unknown-linux-gnu`: exit `0`, `416 passed, 0 failed` (`Hromatite: 0`, `hromatite-abi: 8`, `hromatite-commands: 4`, `hromatite-core: 225 + 7 + 7`, `hromatite-dll: 6`, `hromatite-eac: 118 + 0 + 1 + 0 + 3`, `hromatite-features: 24`, `hromatite-nuklear: 3 + 10`, `test-target: 0`) + `1 doctest ok`, with the 3 expected `hromatite-core` host-cfg warnings (`m3.rs:46`, `d3d11_shaders.rs:5,121`).
   - `git status --short && git diff --stat`: only untracked `?? .agents/`, `?? PROJECT.md`, `?? docs/REFACTOR_AUDIT.md`; empty `git diff`.

2. **Cross-check of `docs/REFACTOR_AUDIT.md` §7.2–§7.3 (`lines 264, 268, 281–284`) against `hromatite-eac` & `hromatite-features` sources**:
   - **Blocker found in `report.rs` + `forge.rs` (`pump_photon`)**:
     - `docs/REFACTOR_AUDIT.md:264` and `281` place `report/{mod, parser, generator, stream}.rs` and pure `forge` frame/header/ring helpers in always-compiled pure core (`default-features = false`), and state: `forge::pump_photon (закрыт #[cfg(feature = "runtime")] в Stage 3, в Stage 4 инвертируется через callback/trait)` (`line 264`; similarly `line 281`: `закрыв forge::pump_photon (или инвертировав его через callback) ... под #[cfg(feature = "runtime")] уже в Stage 3`).
     - In `crates/hromatite-eac/src/report.rs:600-662`, `fn spawn_photon_sender` (called unconditionally by `Stream::new` at `report.rs:714`, which is part of pure core and called by `report.rs:1566` `the_photon_via_renders_into_the_ring_of_the_emitter`) **unconditionally calls `forge::pump_photon()` at `report.rs:644`**.
     - In `crates/hromatite-eac/src/forge.rs:1192-1227`, inline unit test `the_ring_queues_frames_and_reports_an_unarmed_emitter` (untagged `#[test]`, not `#[cfg(feature = "runtime")]`) **unconditionally calls `pump_photon()` at `forge.rs:1210`**.
     - By contrast, `Hooks::install()` in `docs/REFACTOR_AUDIT.md:264, 282` explicitly specifies a non-runtime fallback (`Hooks::install() возвращает Err при not(feature = "runtime")`), whereas no `#[cfg(not(feature = "runtime"))]` stub or caller gating is specified for `pump_photon()`.
   - **Resolved items verified clean**:
     - `eos/state.rs` (`docs/REFACTOR_AUDIT.md:264, 282` vs `crates/hromatite-eac/src/eos.rs:503-742, 1950`): all 26 atomics (`OUT_MESSAGES`..`FORGE_NEXT_SEQ`, `CALLS`, `PROTECT_*`, `OVERHEAD_*`, `BLIND*`, `FORGE_*`, `REPLAY`, `DROP_AFTER_MS`), `EPOCH`/`epoch_line()`, `EXPORTS`/`EVENTS`/`SINK`, non-runtime `Hooks::install() -> Err`, and `#[cfg(feature = "runtime")]` gating for `iat::status()` (`eos.rs:682`), `EVP_CALLS`/`EVP_TARGETS` (`eos.rs:726-740`), and `m3probe` (`eos.rs:1065`) are accounted for.
     - `eacimage.rs` & `keyhunt.rs` (`docs/REFACTOR_AUDIT.md:264, 283, 284` vs `eacimage.rs:1277, 1568, 1633` and `keyhunt.rs:223-256, 1160`, `tests/{keyhunt_scan.rs, eos_install.rs}`): inline runtime tests `eacimage.rs:1568, 1633` and `keyhunt.rs:1160` are explicitly gated under `#[cfg(feature = "runtime")]`, `required-features = ["runtime"]` is specified for `tests/keyhunt_scan.rs` and `tests/eos_install.rs`, and `#[cfg(not(windows))]` `process_vm_readv` (`keyhunt.rs:223-256`) is preserved.
     - `hromatite-features` (`docs/REFACTOR_AUDIT.md:268, 281` vs `feature.rs:6, 172, 374`, `m3.rs:6`, `memory.rs:1-82`, `eac.rs:29, 92, 125, 512-514, 547, 614-619`, `lib.rs:5`): resident-only check (including `"eac.module"`) precedes `command == "eac" || command.starts_with("eac.")` (`feature.rs:172`); `memory.rs` deletion preserves `&Host` helpers `qwords`/`bytes`, `parse_addr`/`decode_hex`, and test `memory.rs:73` (`addresses_and_hex_writes_validate_their_format`); crate-level `#![allow(unsafe_code)]` (`lib.rs:5`) narrows to `#![deny(unsafe_code)]` + local allows.
   - **Minor items observed**:
     - `crates/hromatite-eac/src/lib.rs:51-57, 70` exposes `pub mod evp;`, `pub mod m3probe;`, `pub mod hook;`, and `pub use hook::Hook;` unconditionally; `docs/REFACTOR_AUDIT.md` should note gating `evp`, `m3probe`, and any `Hook` re-export behind `#[cfg(feature = "runtime")]`.
     - `crates/hromatite-features/src/eac.rs:614-619` (`status_lines`) also reads `self.hunter.enabled()` / `self.hunter.dir()`; when `hunter: Hunter` is removed from `features/src/eac.rs` in Stage 3, lines `614-619` (alongside `29, 92, 125, 512-514`) must be removed/relocated to `hromatite-runtime`.

---

## 2. Logic Chain

1. **Why Stage 3 still has a compilation blocker in `hromatite-eac --no-default-features` (Critical)**:
   - `report.rs` is always compiled in pure mode (`default-features = false`), and `Stream::new` (`report.rs:714`) unconditionally calls `spawn_photon_sender` (`report.rs:600`), which unconditionally calls `forge::pump_photon()` (`report.rs:644`).
   - Simultaneously, `forge::tests::the_ring_queues_frames_and_reports_an_unarmed_emitter` (`forge.rs:1210`) unconditionally calls `pump_photon()`.
   - Gating `forge::pump_photon` under `#[cfg(feature = "runtime")]` in Stage 3 (`docs/REFACTOR_AUDIT.md:264, 281`) without specifying a `#[cfg(not(feature = "runtime"))]` stub (or callback registration in Stage 3) causes `report.rs:644` and `forge.rs:1210` to fail to compile under `default-features = false` (Obs. 2).
2. **Why the remaining 3 challenger items and baseline checks pass**:
   - Every symbol touched by `Hooks::counters` and `Hooks::status_lines` in pure-mode `hromatite-features` tests, every runtime-only inline/integration test in `eacimage.rs` and `keyhunt.rs`, and both `"eac.module"` ordering and `memory.rs` test preservation in `hromatite-features` are accurately covered in `docs/REFACTOR_AUDIT.md` §7.2–§7.3 (Obs. 2).
   - `cargo clippy --workspace --all-targets` and `cargo test --workspace --target x86_64-unknown-linux-gnu` pass cleanly on an untouched tracked git tree (Obs. 1).

---

## 3. Caveats

- Read-only review: `--no-default-features` compilation was not executed directly because the `runtime` Cargo feature and split crates do not exist yet at Stage 1 (`5064586`); findings are established by tracing every call site of `pump_photon`, `m3probe`, `hromatite_core`, `Hook`, and `Hunter`.
- Windows test binaries cannot execute directly on the Linux host without a Wine runner (`cargo test --workspace --no-run` was covered by `REFACTOR_AUDIT.md`, and `cargo clippy --workspace --all-targets` verified Windows targets cleanly).

---

## 4. Conclusion

**Verdict: `REQUEST_CHANGES`**

Baseline `clippy` and Linux host tests (`416 passed + 1 doctest`) are green and tracked files remain clean. Three of the four Stage 3 blockers from Iteration 2 (`eos::state` completeness, `eacimage`/`keyhunt` test gating + `process_vm_readv`, and `feature.rs`/`memory.rs`) are resolved in `docs/REFACTOR_AUDIT.md` §7.2–§7.3. However, one Stage 3 compilation blocker remains before M1 sign-off:

1. **[Major/Critical] `report.rs:644` & `forge.rs:1210` vs `#[cfg(feature = "runtime")]` on `forge::pump_photon`**:
   - **Where:** `docs/REFACTOR_AUDIT.md:264, 281` (and `PROJECT.md:119`); `crates/hromatite-eac/src/report.rs:644`, `crates/hromatite-eac/src/forge.rs:823, 1210`.
   - **Fix direction:** Specify either a `#[cfg(not(feature = "runtime"))] pub fn pump_photon() -> Delivery { Delivery { unarmed: true, ..Delivery::default() } }` stub in `forge.rs` (parallel to `Hooks::install()`) **or** invert `pump_photon` via a runtime-registered callback in Stage 3 (returning `unarmed: true` when unregistered) so both `report.rs:644` and `forge.rs:1210` compile and pass cleanly under `default-features = false`.
2. **[Minor] `hromatite-eac/src/lib.rs` exports & `features/src/eac.rs:614-619`**:
   - Note explicitly in §7.2/§7.3 that `lib.rs` gates `evp`, `m3probe`, and any `Hook` re-export under `#[cfg(feature = "runtime")]`, and that `features/src/eac.rs:614-619` (`self.hunter` status line) is removed alongside `self.hunter` tick.

---

## 5. Verification Method

From `/home/wilpdrake/RustroverProjects/Hromatite`:
1. Confirm baseline commands pass:
   ```bash
   cargo clippy --workspace --all-targets
   cargo test --workspace --target x86_64-unknown-linux-gnu
   ```
2. Inspect the remaining blocker directly:
   - `crates/hromatite-eac/src/report.rs:600-662, 712-718, 1553-1590`
   - `crates/hromatite-eac/src/forge.rs:823-862, 1192-1227`
   - `docs/REFACTOR_AUDIT.md:264, 281`
3. **Invalidation conditions:** This `REQUEST_CHANGES` verdict is resolved once `docs/REFACTOR_AUDIT.md` (§7.2–§7.3) and `PROJECT.md` specify the `#[cfg(not(feature = "runtime"))]` stub or Stage 3 callback inversion for `forge::pump_photon()` (covering `report.rs:644` and `forge.rs:1210`).
