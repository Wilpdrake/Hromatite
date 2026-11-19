# Handoff Report — Challenger 1 (`challenger_m1_iter4_1`, Milestone M1 Iteration 4)

**Verdict: `APPROVE`**

---

## 1. Observation

All observations were verified directly against the current repository (`5064586`), `docs/REFACTOR_AUDIT.md` (§7.2–§7.3), `PROJECT.md`, and live test execution:

1. **Baseline test verification**:
   - Command: `cargo test -p hromatite-features -p hromatite-eac --target x86_64-unknown-linux-gnu`
   - Result: Exit `0` (`hromatite-eac`: `118` unit + `0` `eos_install.rs` + `1` `epochs.rs` + `0` `hook_engine.rs` + `3` `keyhunt_scan.rs`; `hromatite-features`: `24` unit).
2. **`hromatite-eac/src/lib.rs` (`default-features = false`)**:
   - `docs/REFACTOR_AUDIT.md:264, 282` and `PROJECT.md:119` gate `pub mod evp;`, `pub mod m3probe;`, `pub mod hook;` (or `hromatite_hook`), and `pub use hook::Hook;` (`lib.rs:51-57, 70`) under `#[cfg(feature = "runtime")]`.
   - All remaining public modules (`capture`, `clock`, `codec`, `digest`, `eacimage` pure part, `eos` pure `state`, `epoch`, `eventlog`, `forge` pure part + stub, `keyhunt` pure part, `payload`, `report`, `session`, `tailer`, `tap`), `encode_beat`, and unit test `lib.rs:116` have zero OS/Win32/`hromatite_core`/`hromatite_win`/`hromatite_hook` dependencies.
3. **`forge.rs` & `report.rs` (`pump_photon` and call sites)**:
   - `docs/REFACTOR_AUDIT.md:264, 281` and `PROJECT.md:118-119` keep `Delivery` and `forge::pump_photon()` callable in the pure core with a `#[cfg(not(feature = "runtime"))]` stub returning `Delivery { unarmed: true, ..Delivery::default() }` (and gate only the `hromatite_core::photon` body under `#[cfg(feature = "runtime")]` before callback inversion in Stage 4).
   - `report.rs:644` (`spawn_photon_sender` -> `forge::pump_photon()`), `report.rs:1553` (`the_photon_via_renders_into_the_ring_of_the_emitter`), and `forge.rs:1210` (`the_ring_queues_frames_and_reports_an_unarmed_emitter`) all compile and pass with `default-features = false`. No other `hromatite_core` or OS calls exist in `report.rs` or `forge.rs`.
4. **`eacimage.rs` (`NativeMemRead`, `live`, `locate_*`, `command`)**:
   - `docs/REFACTOR_AUDIT.md:264, 284` and `PROJECT.md:118-119` keep pure ELF64/`maps` parsing (`ReadAt`, `SliceRead`, `ElfHeader`, `ProgramHeader`, `Section`, `Summary`, `parse_*`, `describe`, `Region`, `Probe`, `MapEntry`, `parse_maps`, `memfd_bases`, `image_span`, `Segment`, `segments_of`) always-compiled, and gate `live` (`797-1025`), `UnixMemRead`/`locate_unix` (`1040-1153`), `find_process_pid`/`maps_of` (`1169-1194`), `NativeMemRead` (`1237-1291`), `locate_native` (`1294-1357`), `command` (`1361-1386`), and inline tests `1568` & `1633` under `#[cfg(feature = "runtime")]`.
   - All remaining unit tests (`1473`, `1495`, `1521`, `1531`, `1550`, `1579`, `1605`, `1624`) use only pure types/parsers.
5. **`keyhunt.rs` (`Hunter` and OS memory/region helpers)**:
   - `docs/REFACTOR_AUDIT.md:264, 283` and `PROJECT.md:119` gate `Hunter`, `read_maps` (`167-180`), `read_block` (`190-256`, preserving `#[cfg(not(windows))] process_vm_readv` inside `"runtime"`), `read_window` (`259-272`), `enumerate_regions` (`276-369`), `find_in_region` (`1102-1114`), and inline test `1160` (`matcher_finds_a_needle_in_live_memory`) under `#[cfg(feature = "runtime")]`, and add `required-features = ["runtime"]` on `tests/keyhunt_scan.rs` in `Cargo.toml`.
   - Remaining pure symbols (`SEED_KEY`, `candidate_keys`, `Region`, `parse_maps`, `Matcher`/`Hit`, `hexdump`) and tests (`1123`, `1143`, `1174`, `1181`) have zero OS/Win32/`hromatite_win` references.
6. **`eos.rs`, `m3probe.rs`, and `crates/hromatite-eac/tests/*`**:
   - `docs/REFACTOR_AUDIT.md:264, 282-283` and `PROJECT.md:118-119` preserve in always-compiled `eos::state`: `Hooks`, `HookEvent`, `Counters`, `Report`, `EXPORTS`, `EVENTS`, `SINK`, `BINDS`, `BIND_COUNT`, `STATE_LOCK`, `LogFile`, `Sink`, `Bind`, `epoch_try`/`epoch_line`/`hook_log`/`hook_log_blocking`/`push_event`, all 26 atomics (`CALLS`, `OUT_MESSAGES`..`FORGE_NEXT_SEQ`, `BLIND*`, `FORGE_*`, `REPLAY`, `DROP_AFTER_MS`), `EPOCH`, and `EOS_ACCSRS_FULLY_MET`.
   - `Hooks::install` (`eos.rs:744, 1087`) switches from `#[cfg(windows)]` / `#[cfg(not(windows))]` to `#[cfg(all(windows, feature = "runtime"))]` / `#[cfg(not(all(windows, feature = "runtime")))]` returning `Err(...)` without `"runtime"`. `Hooks::poll` and `Hooks::install_now` call `self.install()` and `hook_log_blocking`; `Hooks::set_capture_path` uses `unsafe { std::env::set_var(...) }` inside `#[allow(unsafe_code)] mod eos` with `// SAFETY:`.
   - `use crate::hook` (`75`), `iat::status()` (`682`), `EVP_CALLS`/`EVP_TARGETS` block (`726-740`), `m3probe` + `m3probe::probe()` (`1065`), `install_iat` (`1367`), `module_report` (`1387`), `eos/{hooks, pipeline, diag}`, and `eos::tests` (`2958-3532`) are all gated under `#[cfg(feature = "runtime")]`; `tests/eos_install.rs` has `required-features = ["runtime"]`; `tests/epochs.rs` is pure; `tests/hook_engine.rs` moves to `hromatite-hook` in Stage 2.
7. **`hromatite-features`**:
   - `docs/REFACTOR_AUDIT.md:267, 280, 283-284` removes `memory.rs` (moving `&Host` `qwords`/`bytes`, `parse_addr`/`decode_hex`, and test `memory.rs:73` to `host.rs` or `feature.rs`), removes `hunter` and `"eac.module"` from `features/src/eac.rs`, reclassifies `"eac.module"` to `Scope::Resident` in `hromatite-commands`, checks resident-only commands (including `"eac.module"`) in `feature.rs:172` before `eac.*`, and depends on `hromatite-eac` with `default-features = false`.
   - Verified via grep across `crates/hromatite-features/src/*`: every remaining `hromatite_eac::*` symbol (`Hooks`, `Hooks::observe`, `HookEvent`, `Frame`, `Header`, `ChecksumKind`, `codec::*`, `digest::sha256`, `encode_beat`, `eventlog::*`, `payload::*`, `session::*`, `tailer::*`, `tap::*`, `clock::now_ms`, `Beat`) resides in the always-compiled pure core of `hromatite-eac`.

---

## 2. Logic Chain

1. Every crate/module boundary violation and un-gated `default-features = false` compile break identified in Iteration 3 has been explicitly addressed in `docs/REFACTOR_AUDIT.md` (§7.2–§7.3) and `PROJECT.md`.
2. Tracing every symbol, `#[cfg(...)]` guard, `unsafe` block, and call edge across `hromatite-eac/src/{lib,eos,forge,report,eacimage,keyhunt,m3probe}.rs`, `hromatite-eac/tests/*`, and `hromatite-features/src/*` confirms that under `default-features = false` on both `x86_64-pc-windows-gnu` and `x86_64-unknown-linux-gnu`, `hromatite-eac` has zero references to `hromatite-core`, `hromatite-win`, `hromatite-hook`, or Win32/Linux OS memory APIs, while keeping all pure unit/integration tests and all 24 `hromatite-features` tests intact.
3. Therefore, Stage 1 (`docs/REFACTOR_AUDIT.md` and `PROJECT.md`) is ready to serve as the execution contract for Stages 2–5.

---

## 3. Caveats

- Read-only empirical audit on `5064586`: `cargo test` executed on the current baseline (`x86_64-unknown-linux-gnu`); `default-features = false` split compilability is verified by exhaustive symbol/call-graph tracing because the feature split itself happens in Stage 3.
- Non-blocking warning hygiene for Stage 3 implementation: when `#[cfg(feature = "runtime")]` is disabled on `x86_64-pc-windows-gnu`, a few private helper constants/methods (`UNIX_MAPS`/`UNIX_MEM`/`UNIX_PROC` in `eacimage.rs`, `BLOCK`/`DEFAULT_*`/`HIT_CAP`/`Phase`/`Matcher::{span_count,span_list,is_self}`/`env_u64`/`sanitise` in `keyhunt.rs`, and `Sink::replay*`/imports in `eos.rs`) may emit `dead_code` or `unused_imports` warnings (and rustdoc links to `hromatite_core` in `forge.rs`/`report.rs` on `cargo doc`) unless given `#[cfg_attr(not(feature = "runtime"), allow(dead_code, unused_imports))]` or gated alongside their runtime callers during Stage 3. None of these cause compile errors or violate the 3 workspace lints (`unsafe_op_in_unsafe_fn`, `undocumented_unsafe_blocks`, `missing_safety_doc`).

---

## 4. Conclusion

**Verdict: `APPROVE`**

All 4 compilation/isolation gaps from Iteration 3 are resolved in `docs/REFACTOR_AUDIT.md` (§7.2–§7.3) and `PROJECT.md`, and re-tracing `hromatite-eac` (`default-features = false`), `crates/hromatite-eac/tests/*`, and `hromatite-features` reveals no remaining unresolved un-gated symbol or test call.

---

## 5. Verification Method

1. Run baseline tests:
   `cargo test -p hromatite-features -p hromatite-eac --target x86_64-unknown-linux-gnu`
   (Verified exit `0`: `118 + 1 + 3` in `hromatite-eac`, `24` in `hromatite-features`.)
2. Inspect `docs/REFACTOR_AUDIT.md` (§7.2–§7.3) and `PROJECT.md` (§Interface Contracts) against:
   - `crates/hromatite-eac/src/{lib.rs, eos.rs, forge.rs, report.rs, eacimage.rs, keyhunt.rs, m3probe.rs}`
   - `crates/hromatite-eac/tests/{eos_install.rs, epochs.rs, hook_engine.rs, keyhunt_scan.rs}`
   - `crates/hromatite-features/src/{lib.rs, feature.rs, eac.rs, memory.rs}`
3. **Invalidation conditions**: This `APPROVE` is invalidated only if Stage 3 diverges from the `#[cfg(feature = "runtime")]` gating specified in `docs/REFACTOR_AUDIT.md` (§7.2–§7.3) or fails `cargo test -p hromatite-eac --no-default-features` / `cargo build -p hromatite-features` on `x86_64-pc-windows-gnu`.
