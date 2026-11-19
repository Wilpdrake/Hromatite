# Handoff Report — Challenger 2 (`challenger_m1_iter3_2`, Milestone M1 Iteration 3)

**Type:** Hard  
**Verdict:** `APPROVE`

---

## 1. Observation

1. **Direct inspection of `crates/hromatite-features/src/feature.rs:170-185, 370-385`, `eac.rs:547`, `memory.rs`, `m3.rs:6`, and `raw/hot.rs:1225-1247`**:
   - `crates/hromatite-features/src/feature.rs:172-184` executes `if command == "eac" || command.starts_with("eac.") { ... self.eac.dispatch(&command, arg.trim()) ... }` before the resident-only refusal match at `feature.rs:374-380` (`"gct" | "threads" | "gctfix" | "gctresume" | "loopc" | "loop" | "latec" | "latehook" | "late" | "panic" | "av" | "seal" | "quit"`).
   - `crates/hromatite-features/src/eac.rs:547` handles `"eac.module" => (hromatite_eac::eos::module_report(arg), STATUS_OK)`.
   - `crates/hromatite-features/src/memory.rs` (82 lines) defines `parse_addr`, `qwords(&Host, usize, usize)`, `bytes(&Host, usize, usize)`, `decode_hex`, and `#[test] fn addresses_and_hex_writes_validate_their_format` (`73-80`). `grep_search` across `crates/hromatite-features` shows only two production consumers: `feature.rs:6` (`use crate::memory::{bytes, decode_hex, parse_addr, qwords};`) and `m3.rs:6` (`use crate::memory::parse_addr;`).
   - `crates/hromatite-core/src/raw/hot.rs:1225-1247` (`resident_command`) still hardcodes the 13 resident names (checked by `hook.rs:1040` before falling through to `console::dispatch`), and `crates/hromatite-core/src/raw/console.rs` currently has no `"eac.module"` arm.

2. **Direct inspection of `tools/hromatite-reload.sh:60-185` and `tools/test-hromatite-hot.sh:70-230` (plus hermetic execution)**:
   - `tools/hromatite-reload.sh:67` checks `[ "$OLD_ABI" = 1 ] || fail 'malformed current.manifest: unsupported ABI'`; `:99` extracts `GAMEASSEMBLY_SHA256` from `"$PROJECT/crates/hromatite-core/src/symbols.rs"`; `:125, 174, 180` emit `ABI=1`.
   - `tools/test-hromatite-hot.sh:73, 88` assert `ABI=1` in expected manifest fixtures; `:225` reads `"$PROJECT/crates/hromatite-core/src/symbols.rs"`.
   - Running `sh tools/test-hromatite-hot.sh` exited `0` with all 10 hermetic tests passing: `atomic`, `no-overwrite`, `concurrent`, `zero-padded`, `dry-run`, `wrong-hash`, `command-rollback`, `malformed`, `watch`, `watch-retry`.

3. **Direct inspection of `docs/REFACTOR_AUDIT.md` (§6.2, §7.2, §7.3) and `crates/hromatite-eac` claims in `worker_m1_3/handoff.md`**:
   - `crates/hromatite-eac/src/eos.rs:587-608, 659-742` reads the exact 26 global atomics listed in `docs/REFACTOR_AUDIT.md:264`, calls `epoch_line()` (`eos.rs:683, 1950`), calls `hromatite_core::iat::status()` (`eos.rs:682`), and reads `EVP_CALLS`/`EVP_TARGETS` (`eos.rs:726-740`). `eos.rs:1065` calls `crate::m3probe::probe()`.
   - `crates/hromatite-eac/src/forge.rs:725-805` contains the pure ring/counters/helpers used by `report.rs:75-78`, while `forge.rs:823-835` (`pump_photon`) is the sole runtime caller of `hromatite_core::photon::{template, emulate_send}`.
   - `crates/hromatite-eac/src/eacimage.rs:1568, 1633` and `keyhunt.rs:1160` match the inline runtime test locations, and `keyhunt.rs:222-256` uses `process_vm_readv`/`getpid` under `#[cfg(not(windows))]`.

4. **Executed Rust test & clippy suites (`--target x86_64-unknown-linux-gnu`) and git cleanliness**:
   - `cargo test --manifest-path crates/hromatite-console/Cargo.toml --target x86_64-unknown-linux-gnu`: exited `0` — **67 passed, 0 failed** (`58` unit + `6` `tests/hot_reload.rs` + `3` `tests/snapshot.rs`).
   - `cargo clippy --manifest-path crates/hromatite-console/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu -- -D warnings`: exited `0` — **0 warnings, 0 errors**.
   - `cargo test -p hromatite-features --target x86_64-unknown-linux-gnu`: exited `0` — **24 passed, 0 failed**.
   - `cargo test -p hromatite-eac --target x86_64-unknown-linux-gnu`: exited `0` — **122 passed, 0 failed** (`118` unit + `1` `tests/epochs.rs` + `3` `tests/keyhunt_scan.rs` + `0` `tests/eos_install.rs` + `0` `tests/hook_engine.rs`).
   - `git status --short && git diff --stat`: only untracked `?? .agents/`, `?? PROJECT.md`, `?? docs/REFACTOR_AUDIT.md`; `git diff` is empty.

---

## 2. Logic Chain

1. Every file path, line number, symbol count, and test/clippy claim in `docs/REFACTOR_AUDIT.md` (§6.2, §7.2, §7.3) and `worker_m1_3/handoff.md` reproduces verbatim against the current repository (Obs. 1–4).
2. Reordering the resident-only check ahead of `feature.rs:172` and moving `"eac.module"` to `Scope::Resident` + `hromatite-runtime` resolves the Stage 3 feature/EAC boundary conflict without losing any of the 24 `hromatite-features` tests when `memory.rs` helpers and `memory.rs:73` are relocated (Obs. 1, 4).
3. All Stage 4 (`symbols.rs` path in `hromatite-reload.sh:99`, `test-hromatite-hot.sh:225`) and Stage 5 (`ABI=1` / `OLD_ABI=1` in `hromatite-reload.sh:67,125,174,180` and `test-hromatite-hot.sh:73,88`) script coupling points are accurately captured in §6.2 and §7.3, and the baseline hermetic script suite and `hromatite-console` suite pass cleanly (Obs. 2, 4).

---

## 3. Caveats

- **Stage 3 implementation detail (non-blocking for M1):** When Stage 3 reclassifies `"eac.module"` to `Scope::Resident` (before Stage 5 unifies `hot.rs` via `hromatite-commands`), Stage 3 execution of `"eac.module"` while a feature DLL is loaded requires adding `"eac.module"` to `raw/hot.rs::resident_command` (`1225-1247`) alongside adding the `"eac.module"` dispatch arm in `raw/console.rs` (consistent with `docs/REFACTOR_AUDIT.md:266, 281-285`).
- Windows runtime execution under Wine/Proton was not executed in this read-only Linux-host verification pass.

---

## 4. Conclusion

**Verdict: `APPROVE`**

All empirical checks against `crates/hromatite-features/src/{feature.rs,memory.rs}`, `crates/hromatite-eac`, `tools/hromatite-reload.sh`, `tools/test-hromatite-hot.sh`, `hromatite-console`, and `docs/REFACTOR_AUDIT.md` (§6.2, §7.2, §7.3) pass and confirm `worker_m1_3/handoff.md`. Stage 1 (`M1`) audit & baseline documentation is ready for sign-off.

---

## 5. Verification Method

From `/home/wilpdrake/RustroverProjects/Hromatite`:
```bash
cargo test --manifest-path crates/hromatite-console/Cargo.toml --target x86_64-unknown-linux-gnu
cargo clippy --manifest-path crates/hromatite-console/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu -- -D warnings
cargo test -p hromatite-features --target x86_64-unknown-linux-gnu
cargo test -p hromatite-eac --target x86_64-unknown-linux-gnu
sh tools/test-hromatite-hot.sh
git status --short && git diff
```
- **Invalidation conditions:** Any change to `docs/REFACTOR_AUDIT.md`, `crates/hromatite-features`, `crates/hromatite-eac`, `crates/hromatite-console`, or `tools/{hromatite-reload.sh,test-hromatite-hot.sh}` before M1 commit.
