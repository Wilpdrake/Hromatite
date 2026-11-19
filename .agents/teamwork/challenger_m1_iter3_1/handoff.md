# Handoff Report — Challenger 1 (`challenger_m1_iter3_1`, Milestone M1 Iteration 3)

**Verdict: `REQUEST_CHANGES`**

---

## 1. Observation

All claims below were verified directly from source files at commit `5064586` (and updated `docs/REFACTOR_AUDIT.md` / `PROJECT.md`) plus baseline test execution:

1. **Baseline execution check**:
   - Command: `cargo test -p hromatite-features -p hromatite-eac --target x86_64-unknown-linux-gnu`
   - Result: Exit `0`; `hromatite-eac` (`118` unit + `0` `eos_install.rs` + `1` `epochs.rs` + `0` `hook_engine.rs` + `3` `keyhunt_scan.rs`), `hromatite-features` (`24` unit).
2. **`report.rs` & `forge.rs` (`pump_photon` call sites)**:
   - `docs/REFACTOR_AUDIT.md:264, 282` and `PROJECT.md:118-119` place `report` (`report.rs` / `report/{mod,parser,generator,stream}.rs`) in the always-compiled pure core (`default-features = false`), and place `forge::pump_photon` under `#[cfg(feature = "runtime")]` in Stage 3 (before callback inversion in Stage 4).
   - In `crates/hromatite-eac/src/report.rs:600-662`, `fn spawn_photon_sender` is called unconditionally by `Stream::new` (`report.rs:714`). At **`report.rs:644`**, `spawn_photon_sender` unconditionally calls `let delivery = forge::pump_photon();`.
   - In `crates/hromatite-eac/src/forge.rs:1192-1227`, the inline test `the_ring_queues_frames_and_reports_an_unarmed_emitter` unconditionally calls `let delivery = pump_photon();` at **`forge.rs:1210`**.
   - Neither `report.rs:644` nor `forge.rs:1210` is gated in `docs/REFACTOR_AUDIT.md` (§7.2–§7.3) or `PROJECT.md`.
3. **`eacimage.rs` (`locate_native` -> `NativeMemRead`)**:
   - `docs/REFACTOR_AUDIT.md:264, 284` and `PROJECT.md:118-119` gate `live`, `NativeMemRead`, `command`, and inline tests `eacimage.rs:1568, 1633` under `#[cfg(feature = "runtime")]`, leaving `eacimage/` file splitting to Stage 4 (`docs/REFACTOR_AUDIT.md:290`).
   - In `crates/hromatite-eac/src/eacimage.rs:1294-1357`, `pub fn locate_native(probe: &Probe)` unconditionally constructs `NativeMemRead::new(...)` at **`eacimage.rs:1312`** (and `UnixMemRead`/`locate_unix` at `1040-1153`, `find_process_pid`/`maps_of` at `1169-1194` serve `command`/`locate_native`). `locate_native` is not mentioned in Stage 3's gating list in `docs/REFACTOR_AUDIT.md:264, 284`.
4. **`keyhunt.rs` (`read_block`, `enumerate_regions`, `read_window`, `find_in_region`)**:
   - In `crates/hromatite-eac/src/keyhunt.rs:190-218` (`#[cfg(windows)] fn read_block`) and `276-363` (`#[cfg(windows)] fn enumerate_regions`), Stage 2 replaces raw Win32 `extern "system"` (`ReadProcessMemory`, `GetCurrentProcess`, `VirtualQuery`) with `hromatite-win` (`docs/REFACTOR_AUDIT.md:260, 277`).
   - In Stage 3, `hromatite-win` is an optional `"runtime"` dependency of `hromatite-eac` (`docs/REFACTOR_AUDIT.md:264`). However, `docs/REFACTOR_AUDIT.md:264, 283` and `PROJECT.md:119` only gate `keyhunt::Hunter` and inline test `keyhunt.rs:1160` (`find_in_region`) under `"runtime"`, without gating `read_block` (`190-256`), `read_window` (`259-272`), `enumerate_regions` (`276-369`), `read_maps` (`167-180`), and `pub fn find_in_region` (`1102-1114`).
   - Pure test `keyhunt.rs:1143` (`matcher_finds_every_candidate_including_unaligned_copies`) uses only `Matcher`/`candidate_keys`; `Matcher::span_count`/`span_list`/`is_self` (`424-442`) are used only by `Hunter`.
5. **`hromatite-eac/src/lib.rs:70` (`pub use hook::Hook`) and `eos.rs` gating details**:
   - `crates/hromatite-eac/src/lib.rs:51-57, 70` declares `pub mod evp;`, `pub mod m3probe;`, `pub mod hook;` and **`pub use hook::Hook;`** (which becomes `hromatite_hook::Hook` after Stage 2).
   - `crates/hromatite-eac/src/eos.rs:75` has `use crate::hook;` (later `hromatite_hook`). `eos.rs:744` (`Hooks::install`) is guarded by `#[cfg(windows)]` (with `#[cfg(not(windows))]` at `1087`), `eos.rs:1367` defines `pub fn install_iat()`, `eos.rs:1387` defines `pub fn module_report()`, and `eos.rs:2958-3532` (`#[cfg(test)] mod tests`) calls `reset_state()`, `default_dir()`, `drop_active()`, `note_overhead()`, `BINDS`, `BIND_COUNT`, `FORGE_CAPTURE`, `REPORT_STREAM`, and `emit_forge_frame()`.
   - Note: `eos::state` helpers `hook_log` (`368`), `epoch_note_live` (`1956`), `epoch_reset` (`1964`), `epoch_arm` (`1969`), `epoch_allows` (`1988`) are called only by `eos/{hooks,pipeline}` (under `runtime`).
6. **`m3probe.rs` and remaining pure modules + `hromatite-features`**:
   - `m3probe.rs` is called solely from `eos.rs:1065` (inside `Hooks::install`), so gating `pub mod m3probe` + `eos.rs:1065` behind `#[cfg(feature = "runtime")]` isolates it cleanly.
   - Pure modules `capture.rs`, `clock.rs`, `codec.rs`, `digest.rs`, `epoch.rs`, `eventlog.rs`, `payload.rs`, `session.rs`, `tailer.rs`, `tap.rs` have zero references to `hromatite_core`, `extern "system"`, `hook`, `evp`, `m3probe`, `eos`, `eacimage`, or `keyhunt`.
   - `crates/hromatite-features/src/*` (`lib.rs`, `feature.rs`, `eac.rs`, `host.rs`, `m3.rs`, `render.rs`, `telemetry.rs`, `game_fixture.rs`, `test_support.rs`) references only `hromatite-abi`, `hromatite-commands`, and `hromatite-eac` (once `Hunter` and `eos::module_report` are removed from `eac.rs` and `feature.rs:172` checks resident-only commands before `eac.*`, per updated §7.2–§7.3).

---

## 2. Logic Chain

1. **Critical — `report.rs:644` & `forge.rs:1210` fail to compile when `forge::pump_photon` is gated under `"runtime"`**:
   - By Observation 2, `report.rs` is compiled unconditionally under `default-features = false` (required by `hromatite-features`), and `spawn_photon_sender` at `report.rs:644` directly calls `forge::pump_photon()`, while `forge.rs:1210` calls `pump_photon()` in unit tests.
   - Gating `forge::pump_photon` under `#[cfg(feature = "runtime")]` without gating `report.rs:644` and `forge.rs:1210` (or keeping `pump_photon` as a `#[cfg(not(feature = "runtime"))]` stub returning `Delivery { unarmed: true, ..Default::default() }`) causes an immediate unresolved-function compile error in `hromatite-features` (`cargo build`) and `hromatite-eac --no-default-features` (`cargo test`).
2. **High — `eacimage.rs:1312` (`locate_native`) fails to compile under `default-features = false`**:
   - By Observation 3, `NativeMemRead` is gated under `#[cfg(feature = "runtime")]` in Stage 3 while `eacimage.rs` is still a single file (split occurs in Stage 4). Because `locate_native` (`eacimage.rs:1312`) constructs `NativeMemRead::new(...)` and is not listed among gated items in `docs/REFACTOR_AUDIT.md:264, 284`, `hromatite-eac --no-default-features` fails to compile at `eacimage.rs:1312`.
3. **High — `keyhunt.rs:190-369, 1102-1114` still pull `hromatite-win` on Windows (`x86_64-pc-windows-gnu`)**:
   - By Observation 4, Stage 2 migrates `#[cfg(windows)] read_block` (`keyhunt.rs:190-218`) and `#[cfg(windows)] enumerate_regions` (`276-363`) to `hromatite-win`. Because `hromatite-features` builds for `x86_64-pc-windows-gnu`, `#[cfg(windows)]` is active! Leaving `read_block`, `read_window`, `enumerate_regions`, and `find_in_region` un-gated when `hromatite-win` is behind `"runtime"` breaks compilation of `hromatite-eac` (`default-features = false`) on Windows.
4. **High/Medium — `lib.rs:70` (`pub use hook::Hook`) and `eos.rs:744, 1367, 1387, 2958-3532`**:
   - By Observation 5, `hromatite-hook` is behind `"runtime"`, so `lib.rs:70` (`pub use hromatite_hook::Hook;`) must also be `#[cfg(feature = "runtime")]`. Likewise `eos.rs:744` must change from `#[cfg(windows)]` to `#[cfg(all(windows, feature = "runtime"))]` (with fallback `#[cfg(not(all(windows, feature = "runtime")))]`), and `eos.rs:75`, `install_iat` (`1367`), `module_report` (`1387`), and `eos::tests` (`2958-3532`) must be gated under `#[cfg(feature = "runtime")]` (moving into `eos/{hooks,pipeline,diag}`).

---

## 3. Caveats

- Read-only empirical audit: `cargo test` was executed on the current `5064586` baseline (`x86_64-unknown-linux-gnu`); Stage 3 `default-features = false` compilation breaks were deduced by tracing every symbol, `#[cfg(...)]` guard, and call edge across the actual source files.

---

## 4. Conclusion

**Verdict: `REQUEST_CHANGES`**

Iteration 2's 4 blueprint issues were properly integrated into `docs/REFACTOR_AUDIT.md` (§7.2–§7.3) and `PROJECT.md`, and `m3probe.rs` + `hromatite-features` (`eac.rs`, `feature.rs`, `memory.rs`) are now clean. However, tracing every remaining un-gated symbol in `hromatite-eac` against `default-features = false` exposes **4 concrete compilation / isolation breaks** that will still block Stage 3 unless resolved in `docs/REFACTOR_AUDIT.md` (§7.2–§7.3) and `PROJECT.md`:

1. **`report.rs:644` & `forge.rs:1210` (`pump_photon`)**: Either keep `forge::pump_photon()` callable with a `#[cfg(not(feature = "runtime"))]` body returning `Delivery { unarmed: true, ..Default::default() }` (preserving `report.rs:644`, `report.rs:1553`, and `forge.rs:1210` without `hromatite-core` in Stage 3), **or** gate `report.rs:644` (`spawn_photon_sender` `pump_photon()` call) and `forge.rs:1209-1213` under `#[cfg(feature = "runtime")]`.
2. **`eacimage.rs:1294-1357` (`locate_native`)**: Explicitly gate `locate_native` (plus `locate_unix`/`UnixMemRead`/`find_process_pid`/`maps_of`) under `#[cfg(feature = "runtime")]` alongside `NativeMemRead`, `live`, `command`, and tests `1568, 1633`.
3. **`keyhunt.rs:167-369, 1102-1114` (`read_block`, `read_window`, `enumerate_regions`, `read_maps`, `find_in_region`)**: Explicitly gate all OS memory-read/region-walk functions (`read_maps`, `read_block`, `read_window`, `enumerate_regions`, `find_in_region`, plus `Hunter` and test `1160`) under `#[cfg(feature = "runtime")]` (preserving `#[cfg(not(windows))]` `process_vm_readv` inside `"runtime"` for `tests/keyhunt_scan.rs`), so `default-features = false` on Windows does not reference `hromatite-win`.
4. **`lib.rs:70` & `eos.rs:75, 744, 1367, 1387, 2958-3532`**: Explicitly gate `pub use hromatite_hook::Hook;` (`lib.rs:70`), `pub mod evp; pub mod m3probe;` (`lib.rs:52, 55`), `use hromatite_hook` (`eos.rs:75`), `install_iat` (`1367`), `module_report` (`1387`), and `eos::tests` (`2958-3532`) under `#[cfg(feature = "runtime")]`, and change `Hooks::install` (`eos.rs:744, 1087`) from `#[cfg(windows)]` / `#[cfg(not(windows))]` to `#[cfg(all(windows, feature = "runtime"))]` / `#[cfg(not(all(windows, feature = "runtime")))]` (with `#[cfg_attr(not(feature = "runtime"), allow(dead_code))]` on `eos::state` if `hook_log`/`epoch_*` helpers stay in `state.rs`).

---

## 5. Verification Method

1. Run baseline host tests:
   `cargo test -p hromatite-features -p hromatite-eac --target x86_64-unknown-linux-gnu`
   Verify exit `0` (`118` unit + `1` `epochs` + `3` `keyhunt_scan` in `hromatite-eac`, `24` in `hromatite-features`).
2. Inspect exact remaining un-gated sites:
   - `crates/hromatite-eac/src/report.rs:644, 714, 1553-1590` and `crates/hromatite-eac/src/forge.rs:823-862, 1209-1213`
   - `crates/hromatite-eac/src/eacimage.rs:1294-1357` (`1312`: `NativeMemRead::new`)
   - `crates/hromatite-eac/src/keyhunt.rs:190-369, 1102-1114`
   - `crates/hromatite-eac/src/lib.rs:51-57, 70` and `crates/hromatite-eac/src/eos.rs:75, 744, 1087, 1367, 1387, 2958-3532`
3. **Invalidation conditions**: This `REQUEST_CHANGES` verdict is resolved once `docs/REFACTOR_AUDIT.md` (§7.2–§7.3) and `PROJECT.md` are updated to cover the 4 items in §4 above.
