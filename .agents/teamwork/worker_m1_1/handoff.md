# Handoff Report — Worker M1 (`worker_m1_1`, Stage 1 — Audit & Baseline)

**Type:** Hard  
**Deliverable:** `/home/wilpdrake/RustroverProjects/Hromatite/docs/REFACTOR_AUDIT.md`  
**Git HEAD:** `5064586 fix(ui): pass D3D11 single-object pointers by value, pack VB/IB once per frame, and isolate VEH guard lanes per thread` (branch `master`, no tracked files modified; untracked: `.agents/`, `PROJECT.md`, `docs/REFACTOR_AUDIT.md`, `hromatite-mode.launch-bak.1733464`).

---

## 1. Observation

### 1.1 Independently Executed Baseline Commands (2026-11-18T22:24Z)

1. `cargo build --workspace` → **exit `0`** (`Finished dev profile ... in 0.15s`).
2. `cargo build --workspace --release` → **exit `0`** (`Finished release profile ... in 0.05s`).
3. `cargo test --workspace --no-run` → **exit `0`** (compiled all 16 `x86_64-pc-windows-gnu` test executables).
4. `cargo test --workspace` → **exit `101`** (`could not execute process target/x86_64-pc-windows-gnu/debug/deps/Hromatite-dd8f6c1f910d1c6e.exe: Exec format error (os error 8)` because no Wine runner is configured).
5. `cargo test --workspace --target x86_64-unknown-linux-gnu` → **exit `0`**:
   - **416 passed, 0 failed** + **1 doctest ok** (`hromatite_nuklear`).
   - Breakdown: `Hromatite` 0, `hromatite-abi` 8, `hromatite-commands` 4, `hromatite-core` 239 (225 unit + 7 `tests/frames.rs` + 7 `tests/report_stream.rs`), `hromatite-dll` 6, `hromatite-eac` 122 (118 unit + 0 `eos_install` + 1 `epochs` + 0 `hook_engine` + 3 `keyhunt_scan`), `hromatite-features` 24, `hromatite-nuklear` 13 (3 unit + 10 `tests/frame.rs`) + 1 doctest, `test-target` 0.
   - Emits 3 warnings in `hromatite-core` (+ 3 duplicates for lib test): `crates/hromatite-core/src/m3.rs:46` (`unused_imports`), `crates/hromatite-core/src/raw/d3d11_shaders.rs:5, 121` (`dead_code`).
6. `cargo clippy --workspace --all-targets` → **exit `0`** (`0` warnings, `0` errors).
7. `cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu` → **exit `0`**:
   - **8 warnings** in `hromatite-core` (+ 8 duplicate lines for lib test), **0 errors**:
     - `crates/hromatite-core/src/m3.rs:46` (`unused_imports`: `crate::procs`)
     - `crates/hromatite-core/src/raw/m3dump.rs:36` (`clippy::duplicated_attributes` vs `raw/mod.rs:31`)
     - `crates/hromatite-core/src/raw/d3d11_shaders.rs:5, 121` (`dead_code`: `VERTEX_SHADER`, `PIXEL_SHADER`)
     - `crates/hromatite-core/src/raw/hostapi.rs:122` (`clippy::needless_return`)
     - `crates/hromatite-core/src/raw/kiero.rs:266, 348, 404` (`clippy::needless_return`)
8. `cargo test --manifest-path crates/hromatite-console/Cargo.toml --target x86_64-unknown-linux-gnu` → **exit `0`**:
   - **67 passed, 0 failed** (58 unit + 6 `tests/hot_reload.rs` + 3 `tests/snapshot.rs`, 0 doctests).
9. `cargo clippy --manifest-path crates/hromatite-console/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu` → **exit `0`** (`0` warnings, `0` errors).
10. `nix-shell -p xorg.libX11 xorg.libXext --run "cargo test --manifest-path crates/hromatite-overlay/Cargo.toml --target x86_64-unknown-linux-gnu ..."` →
    - `cargo test`: **exit `0`**, **0 tests** (`running 0 tests`).
    - `cargo clippy --all-targets`: **exit `0`**, **9 warnings** in `crates/hromatite-overlay/src/main.rs` (`44` `upper_case_acronyms`; `803, 817, 863, 871, 903` `too_many_arguments`; `1176` `unnecessary_cast`; `1552, 1627` `collapsible_if`).
    - `cargo clippy --all-targets ... -- -D unsafe_op_in_unsafe_fn -D clippy::undocumented_unsafe_blocks -D clippy::missing_safety_doc`: **exit `101`**, **15 errors** (`clippy::undocumented_unsafe_blocks` at `src/main.rs:1420, 1422, 1423, 1431, 1432, 1433, 1435, 1448, 1583, 1584, 1586, 1636, 1639, 1640, 1642`) + **9 warnings**.
11. `cargo check --manifest-path var/research/helpdump/Cargo.toml --target x86_64-unknown-linux-gnu` → **exit `0`**.

### 1.2 Direct Spot-Checks in Codebase
- `cargo tree -p Hromatite`, `cargo tree -p hromatite-features`, `cargo tree -p hromatite-dll`: confirmed `Hromatite -> hromatite-core -> hromatite-nuklear` (`cc`), `hromatite-features -> hromatite-eac -> hromatite-core -> hromatite-nuklear`, and `hromatite-dll -> {hromatite-core, hromatite-eac}`.
- `crates/hromatite-core/src/raw/localmap.rs:306-327`: `feature_api_bytes_valid` validates `init`, `dispatch`, `tick`, `configure`, `shutdown`, omitting `FeatureApiV1.frame`.
- `crates/hromatite-core/src/raw/hot.rs:178`: `if values[4] != "1"` vs `crates/hromatite-abi/src/lib.rs:12` `ABI_VERSION: u32 = 2`; `tools/test-hot-smoke.sh:30` checks `feature: generation=$n abi=1`.
- `crates/hromatite-core/src/raw/hot.rs:1225-1247` & `crates/hromatite-features/src/feature.rs:374-380`: both hardcode 13 resident-only command names omitting `"ui"` (`crates/hromatite-commands/src/lib.rs:170-177`).
- `grep_search`: verified `m3dump` (only `raw/mod.rs:32`, `raw/m3dump.rs`, `AGENTS.md`), `install_legacy_gc_context_guard` (only `raw/hook.rs:1269`), `raw/hook.rs:1950-2018, 2081-2157` vs `raw/console.rs:1130`, `hlog!` (0 callers), `gamebuild::is_supported` (0 callers), `unsafe extern "system"` and `VirtualQuery` copies across `hromatite-core` and `hromatite-eac`.
- `ls -1 tools/*.py tools/*.sh | wc -l`: confirmed **33** scripts (`19` `.py` + `14` `.sh`) plus `8` files in `tools/data/` and untracked `hromatite-mode.launch-bak.1733464`.

---

## 2. Logic Chain

1. Observation 1.1 confirms every build, test, and clippy metric from Surveys 1–3 directly on `5064586`, establishing a reproducible green/warning baseline prior to refactoring.
2. Observation 1.2 + Survey 1–3 evidence proves the concrete boundary violations (`Hromatite.exe` pulling `nuklear`, `hromatite-features` pulling `hromatite-core`/`nuklear` via `hromatite-eac`, `hromatite-dll` pulling `manualmap`/`remote`, `forge` calling `photon`), subsystem duplicates (6 FFI blocks, 2 inline-hook engines, 5+ PE parsers, 3 `VirtualQuery` walkers, duplicate SHA-256/CRC-32C/EVP/`m3`/`telemetry`/`memory`, 3 `hot.rs` implementations with drift), dead code, monoliths, and obsolete `tools/` scripts.
3. Synthesizing these facts into `/home/wilpdrake/RustroverProjects/Hromatite/docs/REFACTOR_AUDIT.md` gives Stages 2–5 an exact, line-referenced blueprint aligned with `PROJECT.md`.

---

## 3. Caveats

- Stage 1 is audit-only: no Rust files, `Cargo.toml`, `tools/`, or `AGENTS.md` were edited, and no `git commit` was executed.
- Windows target runtime execution (`cargo test --workspace`) cannot run directly in this shell without a Wine runner (`os error 8`); Windows test compilation was verified via `cargo test --workspace --no-run` and unit tests ran on `x86_64-unknown-linux-gnu`.

---

## 4. Conclusion

Milestone M1 deliverable `/home/wilpdrake/RustroverProjects/Hromatite/docs/REFACTOR_AUDIT.md` is complete and independently verified against `5064586`. The repository is ready for the Stage 1 Gate and commit, followed by Stage 2 (`hromatite-win` and `hromatite-hook`).

---

## 5. Verification Method

Inspect `/home/wilpdrake/RustroverProjects/Hromatite/docs/REFACTOR_AUDIT.md` and run from `/home/wilpdrake/RustroverProjects/Hromatite`:
```bash
git status --short
cargo build --workspace && cargo build --workspace --release
cargo test --workspace --no-run
cargo test --workspace --target x86_64-unknown-linux-gnu
cargo clippy --workspace --all-targets
cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu
cargo test --manifest-path crates/hromatite-console/Cargo.toml --target x86_64-unknown-linux-gnu
cargo clippy --manifest-path crates/hromatite-console/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu
nix-shell -p xorg.libX11 xorg.libXext --run "cargo test --manifest-path crates/hromatite-overlay/Cargo.toml --target x86_64-unknown-linux-gnu && cargo clippy --manifest-path crates/hromatite-overlay/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu"
cargo check --manifest-path var/research/helpdump/Cargo.toml --target x86_64-unknown-linux-gnu
```
**Invalidation conditions:** Any edits to `Cargo.toml`, source files, or `tools/` invalidate the recorded baseline counts and line numbers.
