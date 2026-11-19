# Handoff Report — Challenger 1 (`challenger_m1_iter2_1`, Milestone M1 Iteration 2)

**Verdict: `REQUEST_CHANGES`**

---

## 1. Observation

All claims below were verified directly from source files and local command execution at commit `5064586`:

1. **Baseline test run**:
   - Command: `cargo test --workspace --target x86_64-unknown-linux-gnu`
   - Result: Exit `0`; `hromatite-features` ran `24` tests (`24 passed, 0 failed`), `hromatite-commands` ran `4` tests, `hromatite-abi` ran `8` tests, `hromatite-core` ran `225` unit + `7` `frames.rs` + `7` `report_stream.rs`, `hromatite-eac` ran `118` unit + `1` `epochs.rs` + `3` `keyhunt_scan.rs` + `0` `eos_install.rs` + `0` `hook_engine.rs`.
2. **`hromatite-eac` pure core vs `runtime` feature coupling**:
   - `PROJECT.md:118-119` and `docs/REFACTOR_AUDIT.md:264` place `report/{mod,parser,generator,stream}.rs` in the **always-compiled pure core** (`default-features = false`), and place `forge.rs` under the `"runtime"` feature (`PROJECT.md:119`: `Runtime feature ("runtime"): ... forge (takes a trait/callback for Photon send instead of depending on hromatite-il2cpp)`).
   - In `crates/hromatite-eac/src/report.rs:75` (and lines `149, 161, 338, 356, 397, 561, 574, 581, 598, 613, 631, 639, 643, 644, 648, 651, 813, 954, 1247-1472, 1553-1590`), `report.rs` **unconditionally** imports and calls `crate::forge::*` (including `forge::pump_photon()` at `report.rs:644` and test calls `crate::forge::{clear_ring, ring_pushed, take_log, drain_frames}` at `report.rs:1562-1589`).
   - In `crates/hromatite-eac/src/forge.rs:824, 834`, `pump_photon()` directly calls `hromatite_core::photon::template()` and `hromatite_core::photon::emulate_send(&frame)`.
   - In `crates/hromatite-eac/src/m3probe.rs:17`, `m3probe` unconditionally imports `hromatite_core::{m3, procs, safe, GAME_ASSEMBLY}`, and `crates/hromatite-eac/src/eos.rs:1065` calls `crate::m3probe::probe()`.
   - In `docs/REFACTOR_AUDIT.md:281-285` (Stage 3) and `PROJECT.md:90` (M3), `hromatite-features` is switched to `hromatite-eac = { default-features = false }` in **Stage 3**, whereas `forge` photon callback inversion and `m3probe` removal are scheduled for **Stage 4** (`PROJECT.md:91`, `docs/REFACTOR_AUDIT.md:290`). Stage 3 does not gate `m3probe.rs` or split `forge.rs`.
3. **`eos::state` vs `eos::hooks` split (`crates/hromatite-features/src/lib.rs:657-710`)**:
   - `lib.rs:672-688` calls `hromatite_eac::Hooks::observe(...)`, `(api().tick)()` (`self.hooks.poll(...)` + `self.hooks.drain_events()` in `crates/hromatite-features/src/eac.rs:504-507`), and `send("eac.hooks")` (`self.hooks.status_lines()` in `eac.rs:239`), asserting `console.contains("eac.hooks: installed=false")` at `lib.rs:707`.
   - `docs/REFACTOR_AUDIT.md:264, 282` puts `Hooks, HookEvent, Counters, Report, EXPORTS, EVENTS, Sink` into `eos/state.rs` and gates `eos/{hooks, pipeline, diag}`, `evp`, and `iat::status` under `"runtime"`.
   - In `crates/hromatite-eac/src/eos.rs:587-608` (`Hooks::counters`) and `659-742` (`Hooks::status_lines`), `Hooks` reads 26 global atomics (`OUT_MESSAGES`, `IN_MESSAGES`, `FORWARDED`, `DROPPED`, `REPLAYED`, `PROTECT_*`, `OVERHEAD_*`, `CALLS`, `BLIND*`, `FORGE_*`, `REPLAY`, `DROP_AFTER_MS`), calls `epoch_line()` (`eos.rs:683, 1950`, using `EPOCH: Mutex<epoch::Gate>`), calls `hromatite_core::iat::status()` (`eos.rs:682`), and reads `EVP_CALLS` / `EVP_TARGETS` (`eos.rs:726-740`).
   - `crates/hromatite-features/src/eac.rs:611` reads `self.hooks.status_lines().into_iter().nth(1)`.
4. **`eacimage.rs` (`798-1291`) and `keyhunt.rs` unit/integration tests under `default-features = false`**:
   - `crates/hromatite-eac/src/eacimage.rs:1277` (`NativeMemRead::read_at`) unconditionally (not `#[cfg(windows)]`) calls `hromatite_core::veh::call_with`.
   - `eacimage.rs:1568` (`the_module_command_always_answers_and_never_touches_memory_on_the_host`) calls `command(...)`, and `eacimage.rs:1633` (`segment_table_translates_file_offsets_the_way_the_loader_mapped_them`) constructs `NativeMemRead::new(...)` — both are inline unit tests inside `eacimage.rs`, while `docs/REFACTOR_AUDIT.md:264, 284` gates `NativeMemRead` and `command` under `"runtime"`.
   - `crates/hromatite-eac/src/keyhunt.rs:1160` (`matcher_finds_a_needle_in_live_memory`) is an inline unit test calling `find_in_region` -> `read_window` -> `read_block` (OS memory read, gated under `"runtime"` per `docs/REFACTOR_AUDIT.md:264, 283`).
   - `crates/hromatite-eac/tests/keyhunt_scan.rs` runs on Linux (`--target x86_64-unknown-linux-gnu`) via `keyhunt.rs:223-256` (`#[cfg(not(windows))] fn read_block` using `process_vm_readv`).
5. **`hromatite-commands/src/lib.rs:386-393` (`"eac.module"`), `features/src/feature.rs`, and `features/src/memory.rs`**:
   - `crates/hromatite-commands/src/lib.rs:548-574` passes if `"eac.module"` changes from `Scope::Feature` to `Scope::Resident` (the invariants check `reload`, `eac.status`, and `resident.len() + feature.len() - shared == help_lines(Scope::Both).len()`).
   - `crates/hromatite-features/src/feature.rs:172` intercepts `if command == "eac" || command.starts_with("eac.")` **before** the resident-only refusal match at `feature.rs:374-380`.
   - `crates/hromatite-features/src/memory.rs:73` owns 1 of the 24 unit tests (`addresses_and_hex_writes_validate_their_format`), and `memory::qwords` / `memory::bytes` take `&crate::host::Host` (internal to `hromatite-features`).
6. **`hromatite-abi::crc` cycle check (`photon.rs`, `frames.rs`, `digest.rs`)**:
   - `crates/hromatite-abi/Cargo.toml:10` has zero dependencies and `#![forbid(unsafe_code)]`.
   - `crates/hromatite-core/src/crc.rs` (`POLY = 0x82F6_3B78`, `crc32c_eac`, `seal_checksum`) is pure Rust with 2 unit tests (`crc::tests`).
   - `crates/hromatite-core/src/raw/photon.rs` uses `crate::crc::seal_checksum` (`1063`) and `crate::crc::crc32c_eac` (`2643`).
   - `crates/hromatite-core/tests/frames.rs:25-26` uses `hromatite_core::crc::{crc32c_eac, seal_checksum}` and `hromatite_core::photon::{...}`. `crates/hromatite-core/tests/report_stream.rs:31` uses only `hromatite_core::photon::parse_payload_records`.
   - `crates/hromatite-eac/src/digest.rs:195-208` implements the exact same `crc32c_eac` (`0x82f6_3b78`, `init=0`, `xorout=0`, vector `b"123456789" == 0x58e3_fa20`).
7. **§4.1 Dead code & §6.1–6.2 `tools/` spot-check**:
   - Verified `gamebuild.rs:21` (`is_supported`), `memscan.rs:19` (`#![allow(dead_code)]`), `hook.rs:1950-2158` (`count_detour`, `install_at`, `installed`, `unhook`/`hooks`/`hookat`/`tick` branches), `tools/hromatite-reload.sh:125,174,180` (`ABI=1`), `tools/test-hromatite-hot.sh:73,88` (`ABI=1`), `tools/test-hot-smoke.sh:30` (`abi=1`), and `tools/test_gc_detour_abi.py:35-133` (`gc_context_detour` + hardcoded `crates/hromatite-core/src/raw/gcguard.rs`).

---

## 2. Logic Chain

1. **Why `hromatite-features` CANNOT compile with `hromatite-eac = { default-features = false }` in Stage 3 as currently written (Critical)**:
   - Observation 2 proves a Stage-3 Catch-22: `report.rs` is required in always-compiled pure core (`default-features = false`), yet `report.rs` unconditionally calls `crate::forge::*`, including `forge::pump_photon()`. Meanwhile `forge.rs` is specified under `"runtime"` AND unconditionally calls `hromatite_core::photon::*` (until Stage 4), and `m3probe.rs` unconditionally imports `hromatite_core::*` (until Stage 4).
   - Consequently, in Stage 3: gating `forge.rs` under `"runtime"` breaks compilation of pure `report.rs` under `default-features = false`, while leaving `forge.rs` (and `m3probe.rs`) ungated forces `hromatite-eac` (`default-features = false`) to retain its `hromatite-core` dependency — pulling `hromatite-nuklear` back into `hromatite-features` and violating Hard Invariant #5.
2. **Why `eos::state` as described will fail to compile `Hooks::counters` / `Hooks::status_lines` without additional items (High)**:
   - Observation 3 shows `lib.rs:688` (`hook_observations_drive_epochs_and_the_jsonl_log`) calls `send("eac.hooks")` -> `Hooks::status_lines()`, which requires `Counters` atomics (`OUT_MESSAGES`..`FORGE_NEXT_SEQ`, `BLIND*`), `EPOCH` (`epoch_line()`), and gating `iat::status()` + `EVP_CALLS`/`EVP_TARGETS` (`eos.rs:726-740`) behind `#[cfg(feature = "runtime")]`. None of those atomics, `EPOCH`, or `EVP_CALLS`/`EVP_TARGETS` gating are listed in §7.2/§7.3's `eos/state.rs` specification.
3. **Why `hromatite-eac --no-default-features` tests will fail to compile unless inline tests are also gated (Medium)**:
   - Observation 4 shows `eacimage.rs:1568, 1633` and `keyhunt.rs:1160` live inside `#[cfg(test)] mod tests` of library modules and directly call `command`, `NativeMemRead::new`, and `find_in_region` (all slated for `feature = "runtime"`). Because Cargo's `required-features` applies only to integration tests/bins, `cargo test -p hromatite-eac --no-default-features` (or workspace tests checking pure mode) will fail unless those inline tests are `#[cfg(feature = "runtime")]` or their pure sub-functions (`segments_of`, `NativeMemRead::address_of` if separated) are tested purely. Additionally, `keyhunt_scan.rs` requires preserving a `#[cfg(not(windows))]` `process_vm_readv` path when migrating `keyhunt` OS reads to `hromatite-win`.
4. **Why `"eac.module"` and `memory.rs` cleanup need minor adjustments in `hromatite-features` (Medium)**:
   - Observation 5 shows `feature.rs:172` intercepts `"eac.module"` before the resident-only refusal arm (`feature.rs:374`), so `Feature::execute("eac.module")` will misreport `"unknown eac command"` (or `"disabled by configuration"`) unless resident-command checking precedes `command.starts_with("eac.")`. Also deleting `memory.rs` requires preserving test #24 (`addresses_and_hex_writes_validate_their_format`) and relocating `&Host`-dependent `qwords`/`bytes` into `host.rs` or `feature.rs`.
5. **Why `hromatite-abi::crc` DOES satisfy `photon.rs`, `frames.rs`, and `eac::digest` with zero cycles (Verified)**:
   - Observation 6 shows `hromatite-abi` is a zero-dependency `#![forbid(unsafe_code)]` root crate. Moving `crc32c_eac` and `seal_checksum` there lets `hromatite-il2cpp::photon`, `hromatite-il2cpp` `tests/frames.rs`, and `hromatite-eac::digest` share the single implementation with strictly acyclic edges (`hromatite-il2cpp -> hromatite-abi` and `hromatite-eac -> hromatite-abi`).

---

## 3. Caveats

- Read-only challenge: no Cargo manifests or Rust source files were modified to test-compile `--no-default-features` directly; all compilation failures under the proposed feature split are deduced by tracing exact imports, function calls, and `#[cfg(...)]` attributes in the live files cited above.
- Windows target test execution (`cargo test --workspace`) is not runnable directly on Linux (`Exec format error (os error 8)`); only `--target x86_64-unknown-linux-gnu` was executed.

---

## 4. Conclusion

**Verdict: `REQUEST_CHANGES`**

The `hromatite-abi::crc` design (`photon.rs`, `frames.rs`, `eac::digest`) is sound and cycle-free. However, the Stage 3 blueprint in `docs/REFACTOR_AUDIT.md` (§7.2–§7.3) and `PROJECT.md` has concrete compilation/boundary gaps that prevent `hromatite-features` and `hromatite-eac --no-default-features` from compiling and passing all 24 tests in Stage 3 as currently specified.

### Required Blueprint Changes (before M1 sign-off)
1. **Resolve Stage 3 `report.rs` / `forge.rs` / `m3probe.rs` -> `hromatite_core` Catch-22**:
   - In Stage 3 (not deferred to Stage 4), either split `forge` so its pure frame/header/ring types (`MessageHeader`, `seal_frame`, `push_frame`, `INTERVAL_MS`, `note`, etc.) remain in pure core while `pump_photon` is gated behind `"runtime"` (or inverted via callback in Stage 3), **and** gate `m3probe` (and `eos.rs:1065`) behind `"runtime"` in Stage 3 so `hromatite-eac` with `default-features = false` has **zero** `hromatite-core` dependency in Stage 3.
2. **Complete `eos::state` specification for `Hooks::status_lines` & `Hooks::counters`**:
   - Explicitly include the `Counters`/`BLIND*`/`FORGE_*`/`REPLAY`/`DROP_AFTER_MS` atomics and `EPOCH` (`epoch_line()`) in `eos/state.rs`, and explicitly gate `hromatite_core::iat::status()` (`eos.rs:682`) **and** `EVP_CALLS`/`EVP_TARGETS` (`eos.rs:726-740`) under `#[cfg(feature = "runtime")]`, so `hromatite-features`'s `hook_observations_drive_epochs_and_the_jsonl_log` (`lib.rs:657-710`) compiles and passes `send("eac.hooks")` under `default-features = false`.
3. **Gate inline runtime tests in `eacimage.rs` & `keyhunt.rs` and preserve Linux `keyhunt_scan` path**:
   - Explicitly mark `eacimage.rs:1568`, `eacimage.rs:1633` (`NativeMemRead::new`), and `keyhunt.rs:1160` (`find_in_region`) with `#[cfg(feature = "runtime")]` (or test only pure `segments_of`/layout separately), add `required-features = ["runtime"]` to `tests/keyhunt_scan.rs`, and preserve the `#[cfg(not(windows))]` `process_vm_readv` reader for host `keyhunt_scan` tests when moving OS scans to `hromatite-win`.
4. **Fix `feature.rs` `"eac.module"` ordering & `memory.rs` test preservation**:
   - Note in Stage 3 that `feature.rs::execute` must check `is_resident_only` (including `"eac.module"`) **before** `command.starts_with("eac.")`, and preserve `memory::tests::addresses_and_hex_writes_validate_their_format` (moving `&Host` helpers `qwords`/`bytes` into `host.rs`/`feature.rs`) so `hromatite-features` retains all 24 tests.

---

## 5. Verification Method

To independently verify these findings:
1. Run baseline host tests:
   `cargo test --workspace --target x86_64-unknown-linux-gnu`
   Verify exit `0` and `24 passed` in `hromatite-features`.
2. Inspect the citations directly:
   - `crates/hromatite-eac/src/report.rs:75, 561-655, 1553-1590` vs `crates/hromatite-eac/src/forge.rs:823-835` vs `crates/hromatite-eac/src/m3probe.rs:17` vs `docs/REFACTOR_AUDIT.md:264, 281-291`.
   - `crates/hromatite-features/src/lib.rs:657-710` & `crates/hromatite-features/src/eac.rs:239, 504-507, 611` vs `crates/hromatite-eac/src/eos.rs:587-608, 659-742`.
   - `crates/hromatite-eac/src/eacimage.rs:1277, 1568, 1633` and `crates/hromatite-eac/src/keyhunt.rs:223-256, 1160`.
   - `crates/hromatite-features/src/feature.rs:172, 374` & `crates/hromatite-features/src/memory.rs:73`.
   - `crates/hromatite-abi/Cargo.toml`, `crates/hromatite-core/src/crc.rs`, `crates/hromatite-core/src/raw/photon.rs:1063, 2643`, `crates/hromatite-core/tests/frames.rs:25-26`, `crates/hromatite-eac/src/digest.rs:195-208`.
3. **Invalidation conditions**: This `REQUEST_CHANGES` verdict is resolved once `docs/REFACTOR_AUDIT.md` (§7.2–§7.3) and `PROJECT.md` are updated to address the 4 items in §4 above.
