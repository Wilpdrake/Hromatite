# Project: Hromatite Architectural Refactoring (Stages 1–5)

## Architecture

Target layered crate graph (strictly acyclic, separating injector from resident runtime and pure protocol/ABI from OS/domain layers):

```
Host Linux Tools (x86_64-unknown-linux-gnu):
  hromatite-console  ──▶ hromatite-commands, hromatite-abi (hot wire/config), unicode-width
  hromatite-overlay  ──▶ X11, Xext (modularized with lib.rs + unit tests, workspace lints)

Pure Contracts (target-independent, #![forbid(unsafe_code)]):
  hromatite-abi      ──▶ C ABI (HostApiV1, FeatureApiV1), per-build offsets, shared hot wire/config/status
  hromatite-commands ──▶ Command catalog (SPECS), Scope helpers (is_resident_forwarded / is_router)

Windows Target Crates (x86_64-pc-windows-gnu):
  hromatite-win      ──▶ Win32 FFI, safe memory/VirtualQuery/memscan, VEH guard, PE parser, procs/PEB, SHA-256
  hromatite-hook     ──▶ hromatite-win (unified x64 inline hook + shared table, midhook, vmt, iat, hookset; 0 Unity/EOS deps)
  hromatite-render   ──▶ hromatite-win, hromatite-hook (Nuklear C shim + Nk API, kiero, d3d11, shaders, ui)
  hromatite-il2cpp   ──▶ hromatite-win, hromatite-hook, hromatite-abi (symbols, buildinfo, discover, il2cpp, m3, gcthreads, gcguard, lateupdate, playerloop, photon/{mod,decode,hook,emulate})
  hromatite-eac      ──▶ (default feature "runtime": hromatite-win, hromatite-hook; pure core: codec, payload, session, epoch, digest, capture, tap, eventlog, tailer, report/{mod,parser,generator,stream}, plus runtime eos/{mod,hooks,pipeline,diag}, evp, eacimage/{mod,elf,locator,report}, forge, keyhunt)
  hromatite-runtime  ──▶ hromatite-win, hromatite-hook, hromatite-il2cpp, hromatite-render, hromatite-eac, hromatite-abi, hromatite-commands (DllMain startup orchestration, unified hot router, localmap, hostapi, console reader/router, LateUpdate tick wiring, Photon<->EAC bridge)
  hromatite-dll      ──▶ hromatite-runtime (thin cdylib DllMain entry; NO manualmap/remote)
  hromatite-injector ──▶ hromatite-win (Hromatite.exe CLI + manualmap + remote; NO render/il2cpp/eac/runtime)
  hromatite-features ──▶ hromatite-abi, hromatite-commands, hromatite-eac (default-features = false -> protocol-only, NO hromatite-win/hook/core/render/il2cpp!)
  test-target        ──▶ standalone stand-in executable
```

### Dependency Rules (Hard Invariants)
1. `hromatite-injector` (`Hromatite.exe`) MUST depend ONLY on `hromatite-win` (and standard library) — NEVER on `hromatite-render`, `hromatite-il2cpp`, `hromatite-eac`, or `hromatite-runtime`.
2. `hromatite-dll` (`hromatite.dll`) MUST NOT depend on `manualmap` or `remote` (`hromatite-injector`).
3. `hromatite-hook` MUST NOT depend on `LateUpdate`, `EOSSDK`, `il2cpp`, or `render`.
4. `hromatite-eac` MUST NOT depend on `hromatite-il2cpp` (`photon` pump inverted via callback/trait in `hromatite-runtime`).
5. `hromatite-features` MUST NOT transitively link `hromatite-win`, `hromatite-hook`, `hromatite-render`, `hromatite-il2cpp`, or `hromatite-runtime`.
6. `hromatite-core` and its `src/raw/` monolith are completely dissolved by the end of Stage 4/5.

## Code Layout

```
Cargo.toml                          # Virtual workspace (members under crates/*), workspace lints
.cargo/config.toml                  # default target x86_64-pc-windows-gnu
crates/
  hromatite-abi/                    # Pure ABI, Build tables, shared hot protocol/config/status
  hromatite-commands/               # Command specs & scope predicates
  hromatite-win/                    # FFI, safe memory/VirtualQuery, VEH, PE parser, procs/PEB, remote RPM/WPM helpers, sha256, file log
  hromatite-hook/                   # Unified x64 inline hook engine + midhook + vmt + iat + hookset
  hromatite-render/                 # Nuklear (build.rs, shim, third_party, Nk), kiero, d3d11, shaders, ui
  hromatite-il2cpp/                 # symbols, buildinfo, discover, il2cpp, m3, gcthreads, gcguard, lateupdate, playerloop, photon/{mod,decode,hook,emulate}
  hromatite-eac/                    # Pure EAC protocol/session/report/{parser,generator,stream} + optional runtime feature: eos/{hooks,pipeline,diag}, evp, eacimage/{elf,locator,report}, forge, keyhunt
  hromatite-runtime/                # Resident orchestration: startup, hot router, localmap, hostapi, console, lateupdate tick loop, hot-smoke bin
  hromatite-injector/               # Bin "Hromatite" (Hromatite.exe): CLI, manualmap, remote
  hromatite-dll/                    # Cdylib "hromatite" (hromatite.dll): thin DllMain -> hromatite-runtime
  hromatite-features/               # Cdylib "hromatite_features": clean features over hromatite-abi + hromatite-commands + protocol-only hromatite-eac
  hromatite-console/                # Standalone Linux workspace: model/{mod,buffers,observe,input}, hot (using shared contract), etc.
  hromatite-overlay/                # Standalone Linux workspace: lib.rs + x11, draw, config, telemetry, demo, panel, esp, main.rs + workspace lints
  test-target/                      # Stand-in target binary
docs/
  REFACTOR_AUDIT.md                 # Stage 1 baseline & audit map (created in M1)
tools/                              # Cleaned & maintained scripts (obsolete scripts deleted/archived in M5)
AGENTS.md                           # Updated §3 and §17
```

## Feature Inventory

Every requirement from `ORIGINAL_REQUEST.md` and every finding from Survey Explorers 1–3 is assigned a milestone below:

| # | Feature / Work Item | Description | Milestone | Source |
|---|---|---|---|---|
| 1 | Baseline test/clippy/build capture | Verify & document `cargo test`, `cargo clippy`, `hromatite-console`, `hromatite-overlay`, `helpdump` baseline at `5064586` | M1 | R1, Survey 1 |
| 2 | Dead code, duplicate & `tools/` map | Document complete dead-code map, subsystem duplicate map, and `tools/` disposition in `docs/REFACTOR_AUDIT.md` | M1 | R1, Survey 2–3 |
| 3 | Extract `hromatite-win` | Consolidate FFI, `memory`/`memscan`, `VirtualQuery` walks, VEH (`context`), PE parser (`pe.rs` + dupes), `procs`/PEB, `sys`, `sha256`, `log`; eliminate FFI/mem dupes in `core/raw`, `eac`, `features` | M2 | R2, Survey 2 |
| 4 | Extract `hromatite-hook` | Unify `core/raw/hook.rs` (engine) + `eac/hook.rs` (engine + shared hooktable) + `midhook` + `vmt` + `iat` + `hookset` over `hromatite-win`; decouple `LateUpdate` & `EOSSDK` | M2 | R2, Survey 2 |
| 5 | Extract `hromatite-render` | Merge `hromatite-nuklear`, `kiero`, `d3d11`, `d3d11_shaders`, `ui` over `hromatite-win` + `hromatite-hook`, isolating COM/DXGI/D3D11/Nuklear | M3 | R3, Survey 2 |
| 6 | Split `hromatite-injector` & `hromatite-runtime` + `hromatite-dll` | Move `src/main.rs` + `manualmap` + `remote` into `hromatite-injector` (`Hromatite.exe`, depends only on `hromatite-win`); move resident startup, `localmap`, `hostapi`, `console`, `hot` runtime, `LateUpdate` tick into `hromatite-runtime` + thin `hromatite-dll`; move `hot-smoke` bin out of `Hromatite.exe` | M3 | R3, Survey 1–2 |
| 7 | Clean `hromatite-features` | Remove `memory.rs` and duplicate hex/addr/string/resident-list utils; decouple `hromatite-features` from `hromatite-core`/`hromatite-win`/`hromatite-hook`/`hromatite-render`; fix `localmap::feature_api_bytes_valid` (`frame` + `feature_header_valid`); tighten `#![allow(unsafe_code)]` | M3 | R3, Survey 1–3 |
| 8 | Extract `hromatite-il2cpp` | Move `symbols`, `buildinfo`, `discover`, `il2cpp`, `m3`, `gcthreads`, `gcguard`, `lateupdate`, `playerloop`, `photon` (split into `decode`, `hook`, `emulate`), and relevant `m3probe` checks into `hromatite-il2cpp`; replace hardcoded RVA in `il2cpp.rs`; update `tools/symmap.py` & `tools/refresh.sh` path if `symbols.rs` moves | M4 | R4, Survey 2–3 |
| 9 | Refactor & decompose `hromatite-eac` | Restrict `hromatite-eac` to protocol/session/EOSSDK hooks over `hromatite-win` + `hromatite-hook`; invert `forge::pump_photon` via callback/trait; split `eos.rs` -> `hooks`, `pipeline`, `diag`; decompose `report.rs`, `eacimage.rs`, `forge.rs`; deduplicate `install_evp` & `crc.rs` | M4 | R4, Survey 2 |
| 10 | Decompose `hromatite-overlay` & `hromatite-console` | Split `hromatite-overlay/src/main.rs` into `lib.rs` + `x11`, `draw`, `config`, `telemetry`, `demo`, `panel`, `esp`, `main`; enable workspace safety lints (`undocumented_unsafe_blocks`, `unsafe_op_in_unsafe_fn`, `missing_safety_doc`) & fix all clippy warnings + add unit tests; split `hromatite-console/src/model.rs` into `model/{mod,buffers,observe,input}` | M4 | R4, R6, Survey 1, 3 |
| 11 | Dissolve `hromatite-core` / `raw/` | Ensure `crates/hromatite-core` and `src/raw/` are fully dissolved across target crates with zero leftover monolith | M4 | AC, Survey 2 |
| 12 | Unify `hot` router & `hromatite-commands` routing | Unify `core/src/hot.rs`, `core/src/raw/hot.rs`, `console/src/hot.rs` via shared pure wire/config/status types in `hromatite-abi::hot` + runtime router in `hromatite-runtime`; fix `Stable`/`Requested`/`Canary` wire drift and `ABI=1` vs `2` mismatch; replace hardcoded resident lists with `hromatite-commands` predicate (fixing missing `"ui"`) | M5 | R5, Survey 3 |
| 13 | Dead code & `tools/` cleanup | Delete confirmed dead code (`m3dump.rs`, legacy GC guard, unreachable `hookat`/`unhook`/`hooks`/`tick`, unused `memscan`/`memory` spans, `gamebuild::is_supported`, `hlog!`, unused `ffi` externs, `discover::Image::data`, `seal`/`L2B`/`crypto.rs` if confirmed unused); clean `tools/` (remove/archive 11 obsolete scripts, fix `tools/test-hot-smoke.sh` `abi=2`, remove `hromatite-mode.launch-bak.1733464`) | M5 | R5, Survey 2–3 |
| 14 | Update `AGENTS.md`, verify all checks, commit & `git push` | Update `AGENTS.md` §3 and §17, run full verification suite (workspace Windows build/test-compile/clippy, Linux unit tests/clippy, `hromatite-console`, `hromatite-overlay`, `cargo tree` boundary checks), ensure clean git tree and `git push` | M5 | R5, R6, Survey 1, 3 |

## Milestones

| # | Name | Scope | Dependencies | Status |
|---|---|---|---|---|
| M1 | Stage 1 — Audit & Baseline | Produce `docs/REFACTOR_AUDIT.md` with baseline verification results, dead-code map, duplicate subsystem map, and `tools/` disposition; verify baseline commands; commit Stage 1 | none | DONE (`5dfb77e`) |
| M2 | Stage 2 — Foundation (`hromatite-win` & `hromatite-hook`) | Create `hromatite-win` and `hromatite-hook`; migrate FFI, memory/memscan/VirtualQuery, VEH, PE, procs/PEB, sha256, log, and unified inline/midhook/vmt/iat/hookset engine; strip duplicate FFI/VirtualQuery/hooking from `hromatite-core` and `hromatite-eac`; verify & commit Stage 2 | M1 | IN_PROGRESS |
| M3 | Stage 3 — Entry Split & `hromatite-render` | Create `hromatite-render`, `hromatite-injector` (`Hromatite.exe`, taking `manualmap`/`remote` and temporarily `crypto.rs`), and `hromatite-runtime`; make `hromatite-dll` thin over `hromatite-runtime`; move `hot-smoke` out of `Hromatite.exe`; clean `hromatite-features` (`memory.rs` removed while keeping `&Host` `qwords`/`bytes` + test, `hromatite-eac` `default-features = false` via `eos::state` incl. Counters/EPOCH, pure `forge` frame/ring vs `"runtime"` `pump_photon`, `"runtime"`-gated `m3probe`/`keyhunt`/`eacimage` live & inline runtime tests, `"eac.module"` -> `Scope::Resident` checked before `eac.*`) & fix `feature_api_bytes_valid`; verify & commit Stage 3 | M2 | PLANNED |
| M4 | Stage 4 — Domain & Host Decomposition | Move `crc32c_eac`/`seal_checksum` to `hromatite-abi::crc`; create `hromatite-il2cpp` (split `photon` into `decode`/`hook`/`emulate`); dissolve `hromatite-core` (deleting `m3dump.rs` & `core`-only dead code); decouple & decompose `hromatite-eac` (`eos` -> `state`/`hooks`/`pipeline`/`diag`, drop `m3probe`, `report`, `eacimage`, `forge` photon callback); decompose `hromatite-overlay/src/main.rs` (with workspace lints + tests) & `hromatite-console/src/model.rs`; verify & commit Stage 4 | M3 | PLANNED |
| M5 | Stage 5 — Unified `hot`, Dead Code, `tools/`, Docs & Push | Unify the 3 `hot.rs` implementations & command routing (`ui` fix, `ABI_VERSION=2` sync across `hot.rs` & `hromatite-reload.sh`/`test-hromatite-hot.sh`); remove remaining dead code (legacy GC guard, `hookat` branches, `--seal`/`crypto.rs`, duplicate `install_evp`/`EAC_KEYHUNT_EVP`); archive/remove obsolete `tools/` & fix `test-hot-smoke.sh` & update/archive `test_gc_detour_abi.py`; update `AGENTS.md` §3 & §17; run full verification; commit Stage 5 & `git push` | M4 | PLANNED |

## Interface Contracts

### `hromatite-win`
- Pure Win32/x64 OS boundary: `ffi`, `sys`, `memory` (safe RPM/WPM + `VirtualQuery` validated readers/writers/region walkers), `memscan`, `veh` (`context` per-thread guard `call0..call10` / `call_guarded`), `pe` (`PeImage` + live PE export/section/import helpers), `procs` (Toolhelp32 + `unlink_from_peb` + regions), `remote` (cross-process `Remote` handle/alloc/thread — or feature-gated `remote` for injector), `sha256`, `log`.
- No dependencies on Unity, IL2CPP, EOS, Nuklear, D3D11, or hooks.

### `hromatite-hook`
- Depends ONLY on `hromatite-win`.
- Exports unified x64 inline hook engine (supporting 5-byte relay jump, 12-byte `mov rax; jmp rax`, shared hook table for cross-DLL coordination, instruction decoder/relocator, near alloc), `midhook`, `vmt`, `iat`, `hookset`.
- Zero references to `LateUpdate`, `EOSSDK`, `console`, or `hot`.

### `hromatite-render`
- Depends on `hromatite-win`, `hromatite-hook`.
- Contains `nuklear` (C shim + safe `Nk`/`Ui`/`Input`/`Keys`/`Frame`), `kiero`, `d3d11`, `d3d11_shaders`, `ui`.
- Exposes clean registration/callback API (`Present` callback / UI toggle / frame draw callback) to `hromatite-runtime`; isolates all COM/DXGI/D3D11/User32/Nuklear symbols inside `hromatite-render`.

### `hromatite-il2cpp`
- Depends on `hromatite-win`, `hromatite-hook`, `hromatite-abi`.
- Contains `symbols`, `buildinfo`, `discover`, `il2cpp`, `m3`, `gcthreads`, `gcguard`, `lateupdate`, `playerloop`, `photon/{mod,decode,hook,emulate}`.
- Uses `hromatite_abi::crc::{crc32c_eac, seal_checksum}` (no edge to `hromatite-eac`).
- `playerloop` and `lateupdate` accept runtime tick callbacks/closures rather than hardcoding `hot` or `console`.

### `hromatite-eac`
- Depends on `hromatite-abi` (pure `crc32c_eac` / `hot` wire). Default `"runtime"` feature adds `hromatite-win`, `hromatite-hook` (disabled in `hromatite-features` via `default-features = false` so `hromatite-features` pulls zero Win32/hook code).
- Always-compiled pure core: `codec`, `payload`, `session`, `epoch`, `digest` (re-exporting `hromatite_abi::crc::crc32c_eac`), `capture`, `tap`, `eventlog`, `tailer`, `report/{mod,parser,generator,stream}`, `forge` pure frame/header/ring (`MessageHeader`, `seal_frame`, `push_frame`, `INTERVAL_MS`, `note`, ring helpers) **plus** callable `Delivery` & `pump_photon()` with `#[cfg(not(feature = "runtime"))]` unarmed fallback returning `Delivery { unarmed: true, ..Delivery::default() }` (or Stage 3 registered callback defaulting to unarmed, keeping `report.rs:644` & `forge.rs:1210` green), `eos::state` (`HookEvent`/`Counters`/`Report`/`Hooks` + 26 global atomics + `EPOCH`/`epoch_line()`, with `Hooks::install` guarded `#[cfg(all(windows, feature = "runtime"))]` / `#[cfg(not(all(windows, feature = "runtime")))] -> Err`, `iat::status()` & `EVP_CALLS`/`EVP_TARGETS` gated behind `"runtime"`), pure `eacimage/{elf,locator}` (slice/maps parsers only).
- Runtime feature (`"runtime"`): `eac/src/lib.rs` gates `pub mod {evp, m3probe, hook}` and `pub use hook::Hook` (`hromatite_hook::Hook`); `eos/{hooks,pipeline,diag}` (including `use hromatite_hook`, `install_iat`, `module_report`, `eos::tests` `2958-3532`), `evp`, `eacimage/{live,report}` (`NativeMemRead`, `locate_native`, `locate_unix`/`UnixMemRead`/`find_process_pid`/`maps_of`, `command`, plus `#[cfg(feature = "runtime")]` inline tests `1568, 1633`), `forge::pump_photon` runtime body (in Stage 4 inverted via trait/callback for Photon send instead of `hromatite-il2cpp`), `keyhunt` OS/runtime items (`Hunter`, `read_maps`, `read_block`, `read_window`, `enumerate_regions`, `find_in_region` over `hromatite-win`, preserving `#[cfg(not(windows))]` `process_vm_readv` for Linux host tests, plus `#[cfg(feature = "runtime")]` inline test `1160` & `required-features = ["runtime"]` on `tests/keyhunt_scan.rs` & `tests/eos_install.rs`), and (in Stage 3 before Stage 4 removal) `m3probe` + `eos.rs:1065`.

### `hromatite-abi` & `hromatite-commands`
- Both `#![forbid(unsafe_code)]`, target-independent (`windows-gnu` and `linux-gnu`).
- `hromatite-abi` holds `HostApiV1`, `FeatureApiV1`, `feature_header_valid`, `host_header_valid`, `build`, `crc` (`crc32c_eac`, `seal_checksum`), and (by M5) shared `hot` wire/config/status contracts (`HotPhase`, `HotStatus`, `HotConfig`, `FeatureManifest`, `parse_hot_status`, `format_hot_status`, `parse_feature_manifest`, `parse_versioned_name`, `parse_reload_request`).
- `hromatite-commands` holds `SPECS` (`eac.module` reclassified to `Scope::Resident` in Stage 3) and `is_resident_forwarded(name: &str)` / `is_router_command(name: &str)` helpers.

### `hromatite-injector` (`Hromatite.exe`) vs `hromatite-runtime` + `hromatite-dll` (`hromatite.dll`)
- `hromatite-injector` (`crates/hromatite-injector`, `[[bin]] name = "Hromatite"`): depends ONLY on `hromatite-win` (+ std). Contains CLI (`bridge`, `inject`, `procs`, `mods`), `manualmap`, `remote`.
- `hromatite-runtime`: depends on `hromatite-win`, `hromatite-hook`, `hromatite-render`, `hromatite-il2cpp`, `hromatite-eac`, `hromatite-abi`, `hromatite-commands`. Contains resident startup, unified `hot` router, `localmap`, `hostapi`, `console` reader/dispatch, `LateUpdate`/PlayerLoop tick wiring, Photon↔EAC forge wiring, and `hot_smoke` binary.
- `hromatite-dll`: depends ONLY on `hromatite-runtime` (and `hromatite-win` if needed for `DllMain` signature).

## Survey Baseline & Audit Summary
- Survey 1: `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/teamwork_explorer_survey_1/handoff.md`
- Survey 2: `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/teamwork_explorer_survey_2/handoff.md`
- Survey 3: `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/teamwork_explorer_survey_3/handoff.md`
