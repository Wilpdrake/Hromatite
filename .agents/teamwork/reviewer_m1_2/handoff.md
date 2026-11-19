# Handoff Report — Reviewer 2 (`reviewer_m1_2`, Milestone M1)

**Type:** Hard  
**Verdict:** `REQUEST_CHANGES`  
**Overall Risk Assessment:** MEDIUM (baseline and Stage 1 maps are accurate; Stage 2 is unblocked, but `docs/REFACTOR_AUDIT.md` §7.2–7.3 has 3 concrete dependency/gating contradictions for Stages 3–4 and 1 Stage 4/5 sequencing contradiction)

---

## 1. Observation

### 1.1 Independent Verification of `hromatite-console` & `hromatite-overlay`
1. `cargo test --manifest-path crates/hromatite-console/Cargo.toml --target x86_64-unknown-linux-gnu` → **exit `0`**: **67 passed, 0 failed** (`58` unit in `src/lib.rs`, `0` in `src/main.rs`, `6` in `tests/hot_reload.rs`, `3` in `tests/snapshot.rs`).
2. `cargo clippy --manifest-path crates/hromatite-console/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu` → **exit `0`**, **0 warnings, 0 errors**.
3. `nix-shell -p xorg.libX11 xorg.libXext --run "cargo test --manifest-path crates/hromatite-overlay/Cargo.toml --target x86_64-unknown-linux-gnu && cargo clippy --manifest-path crates/hromatite-overlay/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu"` → **exit `0`**: **0 tests** (`running 0 tests`), **9 clippy warnings** in `crates/hromatite-overlay/src/main.rs` (`44` `upper_case_acronyms`, `803/817/863/871/903` `too_many_arguments`, `1176` `unnecessary_cast`, `1552/1627` `collapsible_if`).
4. `nix-shell -p xorg.libX11 xorg.libXext --run "cargo clippy --manifest-path crates/hromatite-overlay/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu -- -D unsafe_op_in_unsafe_fn -D clippy::undocumented_unsafe_blocks -D clippy::missing_safety_doc"` → **exit `101`**: **15 errors** (`clippy::undocumented_unsafe_blocks` at `src/main.rs:1420, 1422, 1423, 1431, 1432, 1433, 1435, 1448, 1583, 1584, 1586, 1636, 1639, 1640, 1642`) + **9 warnings**.

### 1.2 Direct Repo Cross-Check (`5064586`)
- `git status --short && git log -1 --oneline`: HEAD is `5064586`; untracked files are `.agents/`, `PROJECT.md`, `docs/REFACTOR_AUDIT.md`, `hromatite-mode.launch-bak.1733464`.
- Directory counts match `docs/REFACTOR_AUDIT.md`:
  - `crates/hromatite-core/src/`: 14 `.rs` files + `raw/` (27 `.rs` files) + `tests/{frames.rs, report_stream.rs}`.
  - `crates/hromatite-eac/src/`: 19 `.rs` files + `tests/{eos_install.rs, epochs.rs, hook_engine.rs, keyhunt_scan.rs}` + `tests/fixtures/`.
  - `crates/hromatite-features/src/`: 10 `.rs` files (`eac.rs`, `feature.rs`, `game_fixture.rs`, `host.rs`, `lib.rs`, `m3.rs`, `memory.rs`, `render.rs`, `telemetry.rs`, `test_support.rs`).
  - `tools/`: 33 scripts (`19` `.py` + `14` `.sh`) + `tools/data/` (8 files).
- Spot-checked code citations in `docs/REFACTOR_AUDIT.md` and `worker_m1_1/handoff.md`:
  - `crates/hromatite-core/src/raw/localmap.rs:318-326`: omits `FeatureApiV1.frame` and does not call `hromatite_abi::feature_header_valid`.
  - `crates/hromatite-core/src/raw/hot.rs:178, 751-763, 1225-1247` vs `crates/hromatite-console/src/hot.rs:52-62`, `crates/hromatite-commands/src/lib.rs:170-177`, and `tools/test-hot-smoke.sh:30`: confirms `abi=1` vs `ABI_VERSION=2`, `stable`/`requested`/`canary` → `HotState::Other`, and missing `"ui"`.
  - Contradictions found in direct source inspection:
    - `crates/hromatite-core/src/raw/photon.rs:1063` calls `crate::crc::seal_checksum(&mut out, EAC_CHECKSUM_OFF)` and `:2643` calls `crate::crc::crc32c_eac(&zeroed)`; `crates/hromatite-core/tests/frames.rs:25` imports `hromatite_core::crc::{crc32c_eac, seal_checksum}`.
    - `crates/hromatite-features/src/eac.rs:29, 34, 90, 92, 123, 125, 547` uses `hromatite_eac::keyhunt::Hunter`, `hromatite_eac::{HookEvent, Hooks}` (currently in `eos.rs`), and `hromatite_eac::eos::module_report(arg)`; `crates/hromatite-features/src/lib.rs:672-686` calls `hromatite_eac::Hooks::observe(HookEvent::...)`.
    - `crates/hromatite-eac/src/eos.rs:1065` calls `crate::m3probe::probe()`, and `crates/hromatite-eac/src/m3probe.rs:17` uses `hromatite_core::{m3, procs, safe, GAME_ASSEMBLY}`.
    - `crates/hromatite-core/src/manualmap.rs:40, 61` calls `crate::crypto::{PAGE, xor_page}`.

---

## 2. Logic Chain

### 2.1 Review Summary
**Verdict:** `REQUEST_CHANGES`

`docs/REFACTOR_AUDIT.md` (§1–§6) and `worker_m1_1/handoff.md` accurately capture the `5064586` baseline, subsystem duplicates, dead code, monoliths, and `tools/` inventory. All 12 target components (`hromatite-win`, `hromatite-hook`, `hromatite-render`, `hromatite-il2cpp`, `hromatite-eac`, `hromatite-injector`, `hromatite-runtime`, `hromatite-dll`, `hromatite-features`, `hromatite-console`, `hromatite-overlay`, and unified `hot`) are accounted for in §7 and align with R2–R5 and `PROJECT.md`. However, cross-checking §7.2–7.3 against actual call sites reveals 3 Major architectural contradictions and 2 Minor sequencing/completeness omissions in `docs/REFACTOR_AUDIT.md` that would violate `PROJECT.md` Hard Invariants or fail compilation in Stages 3–4 if followed literally.

### 2.2 Findings & Adversarial Challenges

#### [Major] Finding 1 — `core/src/crc.rs` omitted from §7.2 and breaks `hromatite-il2cpp::photon` decoupling
- **What:** `docs/REFACTOR_AUDIT.md` §3.5 identifies `core/src/crc.rs` as a duplicate of `eac/src/digest.rs`, and §7.3 (Stage 4) states `"убрать install_evp и дубль crc.rs"` under `hromatite-eac`, while omitting `core/src/crc.rs` from §7.2.
- **Where:** `docs/REFACTOR_AUDIT.md:134, 263–264, 284` vs `crates/hromatite-core/src/raw/photon.rs:1063, 2643` and `crates/hromatite-core/tests/frames.rs:25`.
- **Why (Attack Scenario):** `photon::emulate::retarget_frame` moves to `hromatite-il2cpp`, which depends ONLY on `hromatite-win`, `hromatite-hook`, and `hromatite-abi` (`PROJECT.md` Architecture). Deleting `crc.rs` in Stage 4 leaves `hromatite-il2cpp::photon` unable to compute `seal_checksum` / `crc32c_eac` without adding a dependency on `hromatite-eac`.
- **Suggestion:** Explicitly assign `crc32c_eac`/`seal_checksum` in §7.2 either to a pure shared crate (`hromatite-abi` or a small pure helper), OR invert `retarget_frame` via a checksum callback/trait provided by `hromatite-runtime`/`frames` test.

#### [Major] Finding 2 — `hromatite-features/src/eac.rs` & tests cannot compile against `hromatite-eac` with `default-features = false` in Stage 3
- **What:** §7.2 gates `eos/*` and `keyhunt.rs` behind `hromatite-eac`'s `"runtime"` feature while switching `hromatite-features` to `hromatite-eac` (`default-features = false`) in Stage 3, without specifying the migration of `features/src/eac.rs` and `features/src/lib.rs` tests off `eos` and `keyhunt`.
- **Where:** `docs/REFACTOR_AUDIT.md:92, 253, 264, 268, 280` vs `crates/hromatite-features/src/eac.rs:29, 34, 90, 92, 123, 125, 547` and `crates/hromatite-features/src/lib.rs:672-686`.
- **Why (Attack Scenario):** Turning off `"runtime"` in Stage 3 removes `hromatite_eac::eos::module_report`, `hromatite_eac::keyhunt::Hunter`, and `hromatite_eac::{Hooks, HookEvent}` (currently re-exported from `eos`), causing `hromatite-features` (`cargo build` & `cargo test`) to fail immediately.
- **Suggestion:** Specify in §7.2 / §7.3 (Stage 3): (a) extract pure observation state `Hooks`/`HookEvent` out of `eos.rs` into a pure module (always compiled in `hromatite-eac`), (b) split/replace `keyhunt::Hunter` pure model vs Win32 scan (or remove unused `Hunter` / move `eac.module` to resident/pure query via `HostApiV1`).

#### [Major] Finding 3 — Moving `eac/m3probe.rs` to `hromatite-il2cpp` leaves a forbidden edge from `hromatite-eac::eos`
- **What:** §7.2 moves `eac/m3probe.rs` into `hromatite-il2cpp` ("или удаление при замене `discover.rs`"), without noting its active call site in `eos.rs:1065`.
- **Where:** `docs/REFACTOR_AUDIT.md:263` vs `crates/hromatite-eac/src/eos.rs:1065` and `crates/hromatite-eac/src/m3probe.rs:17`.
- **Why (Attack Scenario):** `PROJECT.md` Rule 4 forbids `hromatite-eac -> hromatite-il2cpp`. Moving `m3probe.rs` out of `hromatite-eac` without removing or inverting `eos.rs:1065` breaks compilation of `hromatite-eac`.
- **Suggestion:** State explicitly in §7.2 / §7.3 (Stage 4) that `eos.rs:1065` must drop `m3probe::probe()` (or accept an optional probe callback from `hromatite-runtime`).

#### [Minor] Finding 4 — Stage 4 vs Stage 5 sequencing contradiction for `hromatite-core` dissolution (`m3dump.rs`, `crypto.rs`)
- **What:** §7.3 (Stage 4) requires completely dissolving `crates/hromatite-core` and `src/raw/`, while §7.3 (Stage 5) defers deleting `raw/m3dump.rs`, `gamebuild::is_supported`, `hlog!`, `discover::Image::data`, unused `ffi` externs, `memscan`/`memory` spans, and `crypto.rs` (which is still called by `manualmap.rs` moved to `hromatite-injector` in Stage 3!).
- **Where:** `docs/REFACTOR_AUDIT.md:265, 283, 288` vs `crates/hromatite-core/src/manualmap.rs:40, 61`.
- **Suggestion:** Clarify in §7.2 / §7.3 that `m3dump.rs` and `core`-only dead items are either deleted during Stage 4's dissolution of `hromatite-core` (not left behind in a deleted crate), and note temporary placement of `crypto.rs` in `hromatite-injector` in Stage 3 if `--seal` removal waits until Stage 5.

#### [Minor] Finding 5 — Minor file-inventory omissions in §7.2
- **What:** `crates/hromatite-features/src/{game_fixture.rs, test_support.rs}` (`#[cfg(test)]` modules), `crates/test-target`, and `crates/hromatite-eac/tests/{eos_install.rs, epochs.rs, keyhunt_scan.rs}` + `tests/fixtures/` are present in the repo (`§1.3`) but omitted from the §7.2 migration/retention table.
- **Suggestion:** Mention that `game_fixture.rs` + `test_support.rs` remain in `hromatite-features`, `eos_install`/`epochs`/`keyhunt_scan` + `fixtures/` remain in `hromatite-eac`, and `test-target` remains unchanged.

### 2.3 Verified Claims
- `hromatite-console` passes 67 tests (`58` unit + `6` `hot_reload` + `3` `snapshot`) and `clippy` with 0 warnings on `x86_64-unknown-linux-gnu` → verified via `cargo test` & `cargo clippy` → **PASS**.
- `hromatite-overlay` compiles with 0 tests and 9 clippy warnings in `nix-shell -p xorg.libX11 xorg.libXext`, and fails with 15 `clippy::undocumented_unsafe_blocks` errors when workspace lints are forced → verified via `nix-shell` commands → **PASS**.
- `crates/hromatite-core/src/raw/localmap.rs:306-327` omits `FeatureApiV1.frame` → verified via `view_file` → **PASS**.
- `raw/hot.rs` (`abi=1`, `stable`/`requested`/`canary`, hardcoded 13-command list missing `"ui"`) vs `hromatite-abi`/`hromatite-console`/`hromatite-commands`/`tools/test-hot-smoke.sh:30` → verified via `view_file` → **PASS**.
- `crates/hromatite-core/Cargo.toml`, `crates/hromatite-eac/Cargo.toml`, `crates/hromatite-features/Cargo.toml`, `crates/hromatite-dll/Cargo.toml`, `src/main.rs` dependency violations → verified via `view_file` → **PASS**.
- Directory counts (`27` files in `raw/`, `14` in `core/src/`, `19` in `eac/src/`, `33` scripts in `tools/`, `8` in `tools/data/`) → verified via `list_dir` → **PASS**.

---

## 3. Caveats

- Full Windows workspace compilation (`cargo build --workspace`, `cargo test --workspace --no-run`, `cargo clippy --workspace --all-targets`) and Linux workspace test execution (`416 passed`) were not re-run by Reviewer 2 (covered by `worker_m1_1` and Reviewer 1); Reviewer 2 independently ran `hromatite-console` and `hromatite-overlay` plus manifest/source inspection.
- `tools/` scripts (beyond `tools/test-hot-smoke.sh`) were not individually executed.
- Stage 2 (`hromatite-win`, `hromatite-hook`) in `docs/REFACTOR_AUDIT.md` has no contradictions and is ready to proceed once the doc clarifications above are applied (or accepted as Stage 3–4 execution notes by the orchestrator).

---

## 4. Conclusion

**Verdict: `REQUEST_CHANGES`**

The M1 baseline measurements (`hromatite-console` 67/67 pass, `hromatite-overlay` 0 tests / 9 warnings / 15 `undocumented_unsafe_blocks` under workspace lints) and §1–§6 audit maps in `docs/REFACTOR_AUDIT.md` are verified and accurate, and all 12 target components from R2–R5 and `PROJECT.md` are accounted for. Before committing Stage 1, update `docs/REFACTOR_AUDIT.md` §7.2–7.3 to resolve the 3 dependency/feature-gating contradictions (`crc.rs`↔`photon`, `features/src/eac.rs`↔`eac` `default-features=false`, `eos.rs:1065`→`m3probe`), clarify Stage 4 vs Stage 5 sequencing for `m3dump.rs`/`crypto.rs`, and note the omitted test/helper files (`game_fixture.rs`, `test_support.rs`, `test-target`, remaining `eac/tests/`).

---

## 5. Verification Method

From `/home/wilpdrake/RustroverProjects/Hromatite`:
```bash
cargo test --manifest-path crates/hromatite-console/Cargo.toml --target x86_64-unknown-linux-gnu
cargo clippy --manifest-path crates/hromatite-console/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu
nix-shell -p xorg.libX11 xorg.libXext --run "cargo test --manifest-path crates/hromatite-overlay/Cargo.toml --target x86_64-unknown-linux-gnu && cargo clippy --manifest-path crates/hromatite-overlay/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu"
nix-shell -p xorg.libX11 xorg.libXext --run "cargo clippy --manifest-path crates/hromatite-overlay/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu -- -D unsafe_op_in_unsafe_fn -D clippy::undocumented_unsafe_blocks -D clippy::missing_safety_doc"
```
Inspect `docs/REFACTOR_AUDIT.md` §7.2–7.3 against `crates/hromatite-core/src/raw/photon.rs:1063`, `crates/hromatite-features/src/eac.rs:29,34,547`, `crates/hromatite-eac/src/eos.rs:1065`, and `crates/hromatite-core/src/manualmap.rs:40,61`.  
**Invalidation conditions:** Any edits to `docs/REFACTOR_AUDIT.md`, `PROJECT.md`, `Cargo.toml`, or Rust sources invalidate these line references and counts.
