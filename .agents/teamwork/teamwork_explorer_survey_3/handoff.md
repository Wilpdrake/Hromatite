# Handoff Report — Survey Explorer 3 (Host Tools, ABI/Commands, `tools/`, Root Artifacts & `AGENTS.md` Audit)

**Core Summary:** Inventoried all 10 crates under `crates/` (including standalone host crates `hromatite-console` and `hromatite-overlay`), mapped exact decomposition boundaries for `hromatite-overlay/src/main.rs` (2,023 lines) and `hromatite-console/src/model.rs` (1,332 lines), identified wire-format/validation drift across the three `hot.rs` files + `hromatite-abi`/`hromatite-commands`, and audited all 33 scripts in `tools/`, root artifacts, and `AGENTS.md` (§3 & §17).

---

## 1. Observation

### 1.1 Workspace & Standalone Crate Inventory (exact `wc -l` newline counts)

Note: Root `Cargo.toml` (`members` lines 2–11) builds 8 Windows-target crates (`x86_64-pc-windows-gnu`), while `crates/hromatite-console/Cargo.toml` (line 23) and `crates/hromatite-overlay/Cargo.toml` (line 12) declare independent `[workspace]` tables for the Linux host (`x86_64-unknown-linux-gnu`).

#### Requested Target Crates

| Crate / File | Lines (`wc -l`) | Role / Notes |
|---|---:|---|
| **`crates/hromatite-abi/`** | **674** | Pure C ABI + per-build descriptor table (`#![forbid(unsafe_code)]`) |
| `Cargo.toml` | 13 | No external deps |
| `src/lib.rs` | 155 | `ABI_VERSION = 2`, `HostApiV1`, `FeatureApiV1`, `host_header_valid`, `feature_header_valid`, 3 tests |
| `src/build.rs` | 506 | `GcAnchors`, `Il2cppAnchors`, `RuntimeAnchors`, `Layout`, `Build`, `KNOWN` (`BUILD_648F61B1`, `BUILD_245768F2`), 5 tests |
| **`crates/hromatite-commands/`** | **603** | Single source of truth for payload commands (`#![forbid(unsafe_code)]`) |
| `Cargo.toml` | 14 | No external deps |
| `src/lib.rs` | 589 | `Risk`, `Scope`, `Spec`, `SPECS` (41 commands), `help_lines`, `names`, `find`, 4 tests |
| **`crates/hromatite-console/`** | **4,660** | Standalone Linux host TUI (depends on `unicode-width`, `hromatite-commands`) |
| `Cargo.toml` | 34 | Independent `[workspace]`, `hromatite_console` lib + `hromatite-console` bin |
| `README.md` | 110 | Operator & headless verification docs |
| `src/lib.rs` | 39 | `#![forbid(unsafe_code)]`, module declarations + 2 parser tests |
| `src/main.rs` | 400 | CLI dispatch, `--snapshot`, PTY loop, manual Linux `termios`/`ioctl` FFI |
| `src/commands.rs` | 73 | Thin wrapper re-exporting `hromatite_commands::{Risk, SPECS, Scope, Spec}` + 3 tests |
| `src/config.rs` | 160 | `HELP`, CLI arg parsing (`--prefix`, `--log`, `--view`, `--demo`, `--snapshot`, `--size`) |
| `src/demo.rs` | 82 | Synthetic demo lines (`DEMO_LINES`, `DEMO_GAME_LINES`, `DEMO_EAC_LINES`) |
| `src/fileio.rs` | 271 | Incremental `LogTail` + `CommandWriter` (`create + append`) |
| `src/hot.rs` | 98 | `HotState`, `HotStatus`, `parse_hot_status` for `[hot] ...` log lines |
| `src/input.rs` | 463 | `Key`, `KeyDecoder`, `History`, `complete()` |
| `src/model.rs` | 1,332 | Monolithic UI state, log buffers, telemetry parser, key dispatch, palette, manual + tests |
| `src/parser.rs` | 92 | `LineKind`, `LogLine`, `parse_line` |
| `src/render.rs` | 655 | Fixed-size ANSI/plain frame renderer (`RenderContext`, `Style`, `Frame`, `render`) |
| `src/sources.rs` | 406 | Wildcard log source planner & `SourceTail` (`payload`, `game`, `eac`, `hooks`, `gc`, `stdout`, `launch`) |
| `src/telemetry.rs` | 103 | Rate-limited `AutoRefresh` scheduler (`pos` 1s, summary 5s, stale gate 10s) |
| `tests/hot_reload.rs` | 180 | 5 integration tests for `hot` parsing, palette, F7, confirmation, rendering |
| `tests/snapshot.rs` | 66 | Snapshot rendering tests |
| **`crates/hromatite-overlay/`** | **2,038** | Standalone Linux host X11/Xext overlay (`#![allow(non_snake_case)]`) |
| `Cargo.toml` | 15 | Independent `[workspace]`, zero Cargo dependencies (links `X11`, `Xext`) |
| `src/main.rs` | 2,023 | Monolithic Xlib/Xext FFI, config, telemetry, drawing primitives, panel UI, ESP layer, event loop, demo |
| **`crates/test-target/`** | **29** | Stand-in `VRChat.exe` for injector/mapper tests (`#![forbid(unsafe_code)]`) |
| `Cargo.toml` | 13 | No external deps |
| `src/main.rs` | 16 | Prints PID and sleeps 500ms in a loop |

#### Other Crates Under `crates/` (for complete workspace context)

- **Root package `Hromatite`** (`Cargo.toml` 39L, `src/main.rs` 247L): Injector CLI (`bridge`, `inject`, `hot-smoke`, `procs`, `mods`).
- **`crates/hromatite-core/`** (21,210 lines total across `Cargo.toml` 21L + 14 top-level `src/*.rs` + 27 `src/raw/*.rs` + `tests/frames.rs` + `tests/report_stream.rs`): Largest `raw/` files are `photon.rs` (2,717), `hook.rs` (2,568), `d3d11.rs` (1,756), `localmap.rs` (1,622), `hot.rs` (1,396), `console.rs` (1,287), `iat.rs` (1,043), `gcguard.rs` (1,025), `discover.rs` (883), `playerloop.rs` (786), `memory.rs` (765), `kiero.rs` (731), `vmt.rs` (717), `m3dump.rs` (546), `context.rs` (534), `midhook.rs` (512).
- **`crates/hromatite-dll/`** (442L): `Cargo.toml` (18L), `src/lib.rs` (424L).
- **`crates/hromatite-eac/`** (14,193L + `tests/`): `Cargo.toml` (17L) + 19 `src/*.rs` (largest: `eos.rs` 3,532, `hook.rs` 1,792, `eacimage.rs` 1,658, `report.rs` 1,644, `forge.rs` 1,228, `keyhunt.rs` 1,193, `evp.rs` 728).
- **`crates/hromatite-features/`** (5,381L): `Cargo.toml` (17L) + 10 `src/*.rs` (`render.rs` 1,255, `eac.rs` 858, `lib.rs` 810, `m3.rs` 624, `telemetry.rs` 594, `feature.rs` 391, `game_fixture.rs` 295, `test_support.rs` 267, `host.rs` 188, `memory.rs` 81).
- **`crates/hromatite-nuklear/`** (1,262L Rust + `build.rs`, `nuklear_shim.c`, `third_party/nuklear/`, `tests/frame.rs`): `Cargo.toml` (21L), `src/lib.rs` (602L), `src/sys.rs` (639L).

---

### 1.2 Decomposition Findings

#### A. `crates/hromatite-overlay/src/main.rs` (2,024 lines viewed; 0 unit tests)
Exact contiguous sections observed:
- **Lines 37–272**: Raw `Xlib` & `Xext` FFI (`Display`, `Window`, `XSetWindowAttributes`, `XRectangle`, constants `CW_*`/`SHAPE_*`/`KEY_F8`/`KEY_F9`, `#[link(name = "X11")]` & `#[link(name = "Xext")]` extern blocks).
- **Lines 274–314**: Color palette (`C_BG`..`C_BLACK`) and color helpers (`lerp_col`, `dist_color`).
- **Lines 316–402** & **2000–2023**: `Config` (`hromatite-overlay.conf` load/save) and `prefix_from_args`.
- **Lines 404–629**: `Target`, `Telemetry`, `parse_vec3`, `kv`, `kv_vec3`, `kv_rest`, `Telemetry::feed`, `Telemetry::animate`, `Telemetry::best_dist`.
- **Lines 633–645** & **801–960**: `Gfx` handle struct and 2D X11 drawing primitives (`fill`, `round_rect`, `line`, `circle`, `arc`, `text`, `text_right`, `fit`).
- **Lines 647–799**: `Ui`, widget IDs (`W_ESP`..`W_OPA`), layout constants, `Row`, `build_rows`, `place`, `panel_height`, `hit`, `set_slider`.
- **Lines 962–1023** & **1801–1957**: Window management & shaping (`rounded_mask`, `shape_window`, `apply_opacity`, `i32_at`, `make_window`, `load_font`, `find_window`, `game_origin`, `window_rect`).
- **Lines 1025–1256**: Panel & radar rendering (`draw_panel`, `draw_radar`, `radar_plot`, `fade_cone`).
- **Lines 1258–1362**: ESP layer rendering + `XShapeCombineRectangles` rect collection (`draw_esp`).
- **Lines 1364–1799**: `main()` orchestration (X11 setup, log tailing & `hromatite-cmd.txt` polling, frame pacing, resize, X11 event loop).
- **Lines 1959–1998**: `demo_feed` synthetic target generator.

**Proposed `hromatite-overlay` module split** (plus adding `src/lib.rs` so pure modules can be unit-tested without an X display):
1. `src/x11.rs` — Raw Xlib/Xext FFI, `Gfx`, window creation/lookup/geometry, font loading, `shape_window`, `apply_opacity`, `rounded_mask`.
2. `src/draw.rs` — Color palette (`theme` constants, `lerp_col`, `dist_color`) and X11 drawing primitives (`fill`, `round_rect`, `line`, `circle`, `arc`, `text`, `text_right`, `fit`).
3. `src/config.rs` — `Config`, `load`/`save`, `prefix_from_args`.
4. `src/telemetry.rs` — `Target`, `Telemetry`, `kv`/`kv_vec3`/`kv_rest`/`parse_vec3`, `feed`, `animate`, `best_dist`.
5. `src/demo.rs` — `demo_feed`.
6. `src/panel.rs` — `Ui`, `Row`, `build_rows`, `place`, `panel_height`, `hit`, `set_slider`, `draw_panel`, `draw_radar`, `radar_plot`, `fade_cone`.
7. `src/esp.rs` — `draw_esp`.
8. `src/main.rs` — App loop, log tailing / command writer, X11 event dispatch.

#### B. `crates/hromatite-console/src/model.rs` (1,332 lines)
Exact sections observed:
- **Lines 9–123**: `MAX_LINES`, `WHEEL_ROWS`, `Mode`, `LogBuffer`, `Action`, `ConsoleModel`, `ConsoleModel::new`.
- **Lines 125–310, 331–370**: Multi-source buffer lifecycle (`configure_sources`, `select_source`, `show_source`, `step_source`, `source_summary`, `ingest`, `ingest_from`, `ingest_replay`, `ingest_lines`, `record_sent`, `lines`, `visible_lines`, `matches_filters`, `window`).
- **Lines 312–329, 855–884, 928–990**: Telemetry observation & parsing (`observe`, `parse_feature_log`, `update_telemetry`, `keyed_value`, `player_count`, `tick_hz`, `hot_status`, `feature_generation`, `feature_abi`, `gc_healthy`).
- **Lines 372–570, 582–769, 894–903**: Key handling, scrolling, submission, confirmation gate, text editing (`handle`, `set_viewport`, `buffer_max_scroll_back`, `scroll_view`, `prepare_submission`, `local_view_command`, `begin_search`, `begin_palette`, `after_edit`, `previous_char`, `next_char`).
- **Lines 800–820, 905–926**: Command palette filtering (`palette_entries`, `selected_palette_command`) and `field_manual`.
- **Lines 771–892**: State getters/setters.
- **Lines 992–1332**: Unit tests (340 lines).

**Proposed `hromatite-console/src/model/` submodule split** (re-exporting `Action`, `ConsoleModel`, `Mode`, `field_manual` from `model/mod.rs` to preserve public API):
1. `model/mod.rs` — `ConsoleModel` struct, `Mode`, `Action`, `new`, state accessors, `tests`.
2. `model/buffers.rs` — `LogBuffer`, source selection/summary, `ingest*`, filtering, `window`, `set_viewport`/`scroll_view`.
3. `model/observe.rs` — `observe`, `parse_feature_log`, `update_telemetry`, `keyed_value` (or share log-line telemetry parsing with `hromatite-overlay`).
4. `model/input.rs` (or `dispatch.rs`) — `handle`, `prepare_submission`, `local_view_command`, `begin_search`, `begin_palette`, `after_edit`, `previous_char`/`next_char`, `palette_entries`, `field_manual`.

#### C. `hot.rs` Trio Comparison & Unification
1. `crates/hromatite-core/src/hot.rs` (944 lines): Pure `#![forbid(unsafe_code)]` domain model (`HotConfig`, `FeatureSetting`, `ConfigError`, `parse_config`, `Phase`, `GenerationRole`, `InFlight`, `HotError`, `Tick`, `Fault`, `HotReload` state machine + 407 lines of tests).
2. `crates/hromatite-core/src/raw/hot.rs` (1,396 lines):
   - Lines 9–223: Pure protocol/artifact helpers (`canonical_generation`, `ReloadSelection`, `parse_reload_request`, `VersionedName`, `parse_versioned_name`, `config_text`, `replace_config`, `FeatureManifest`, `windows_artifact_path`, `parse_feature_manifest`, `FeatureArtifact`, `bootstrap_feature_requested`) — currently trapped in `raw/hot.rs` despite having zero Win32 dependencies!
   - Lines 225–1182: `#[cfg(target_os = "windows")] mod imp` (`Runtime`, `ImageGeneration`, `FeatureImage` loading/canary/retire/rollback, `choose_artifact`, `read_config_file`/`write_config_file`, `smoke_impl`).
   - Lines 1184–1268: Public hooks (`tick`, `frame`, `original_first_required`, `smoke`, `resident_command`, `dispatch`, `command`).
3. `crates/hromatite-console/src/hot.rs` (98 lines): Host-side parser (`HotState`, `HotStatus`, `parse_hot_status`) for `[hot] state=... generation=... previous=... pending=... sha=... flight=... error=...`.

**Observed drift & duplication between the three `hot.rs` files:**
- **State enum drift**: `core/src/raw/hot.rs:751-763` emits `"idle"`, `"stable"`, `"requested"`, `"draining"`, `"loading"`, `"canary"`, `"active"`, `"rolling_back"`, `"failed"`. Meanwhile `console/src/hot.rs:52-62` maps `"idle"|"empty"|"ready" -> Idle`, `"loading"|"reloading" -> Loading`, `"active" -> Active`, `"error"|"failed" -> Error`, etc., causing `"stable"`, `"requested"`, `"canary"` to fall into `HotState::Other`!
- **Manifest ABI version mismatch**: `core/src/raw/hot.rs:178` checks `values[4] != "1"` (`ABI=1`), while `hromatite_abi::ABI_VERSION == 2` (`hromatite-abi/src/lib.rs:12`). Meanwhile `tools/test-hot-smoke.sh:30` still asserts `"feature: generation=$n abi=1"` even though `hromatite-features/src/feature.rs:226` emits `abi={ABI_VERSION}` (`abi=2`).
- **Hardcoded `resident_command` drift**: `core/src/raw/hot.rs:1225-1247` and `hromatite-features/src/feature.rs:374-380` both hardcode the same 13 resident-only command strings instead of querying `hromatite_commands`. Both lists omit `"ui"`, even though `hromatite-commands/src/lib.rs:170-177` declares `ui` as `Scope::Resident`.

#### D. `hromatite-abi` & `hromatite-commands` Audit
- **`hromatite-abi` dead code / gaps**:
  - `feature_header_valid` (`crates/hromatite-abi/src/lib.rs:108`) is only called in `hromatite-abi`'s own unit tests. Meanwhile `crates/hromatite-core/src/raw/localmap.rs:306-327` (`feature_api_bytes_valid`) hand-rolls validation of `FeatureApiV1` and **omits checking `frame`** (added in `ABI_VERSION = 2`) in lines 318–324!
  - `STATUS_BAD_ABI`, `host_header_valid`, `build::{Build, GcAnchors, Il2cppAnchors, RuntimeAnchors, Layout, select, newest, newest_sha_bytes}` are all actively used.
  - **Missing shared abstractions**:
    - For `hot` unification: `hromatite-abi` (which is pure `#![forbid(unsafe_code)]` and can be depended on by `hromatite-console` on Linux as well as `hromatite-runtime`/`hromatite-core`) lacks a shared `hot` protocol module (`HotPhase`/`HotStatus` wire format, `MANIFEST_FORMAT_VERSION`, `HotConfig`, `parse_config`, `parse_hot_status`, `parse_feature_manifest`, `parse_versioned_name`, `parse_reload_request`).
    - For `hromatite-il2cpp`: `HostApiV1` exposes `game_assembly_sha256`, `log`, `read`, `write`, `guarded_call`, forcing `hromatite-features/src/m3.rs:52` to call `hromatite_abi::build::select()` directly (failing if resident `raw::discover` resolved an unknown build!). Also `core/src/m3.rs` (492L) and `features/src/m3.rs` (624L) duplicate IL2CPP class/method/field walking over different memory abstractions (`crate::safe` vs `Host`). A shared trait or resolved `Build` accessor is needed for `hromatite-il2cpp`.
    - For `hromatite-render`: `HostApiV1` has `MAX_CALL_ARGS = 10` for `Graphics.DrawMesh`, but no shared render/overlay surface or frame-hook contract between resident D3D11/Nuklear (`hromatite-render`) and feature-side Unity ESP (`hromatite-features/src/render.rs`).
- **`hromatite-commands` dead code / gaps**:
  - No dead types (`Risk`, `Scope`, `Spec`, `SPECS`, `help_lines`, `names`, `find` are all actively used).
  - Missing helper `Scope::is_resident_forwarded()` / `is_resident_only(name: &str)` (distinguishing `reload`/`rollback`/`config` router commands from `gct`/`threads`/`ui`/`late*`/`panic`/`av`/`quit` resident commands) so `hot` router and `hromatite-features` stop hardcoding lists and missing `"ui"`.
  - `"seal"` (`crates/hromatite-commands/src/lib.rs:467-474`, `"stop the payload console (legacy name)"`) is still present in `SPECS`, `core/src/raw/console.rs:1133`, `core/src/raw/hot.rs:1244`, `features/src/feature.rs:375`, and `console/src/input.rs:335` (legacy alias for `quit`; verify in Stage 5 whether to keep or remove).

---

### 1.3 Complete `tools/` Audit (33 Scripts + `tools/data/`)

#### Python Scripts (19)

| File | Lines | Purpose | Active Refs | Disposition | Rationale |
|---|---:|---|---|---|---|
| `tools/buildgen.py` | 1,499 | Generates `hromatite-abi/src/build.rs` + `tools/data/build-*.json` + `il2cpp-export-names-*.json` | `AGENTS.md`, `test_buildgen.py`, `build.rs`, `buildinfo.rs` | **KEEP** | Core per-build offset/anchor generator |
| `tools/test_buildgen.py` | 358 | Synthetic + two-build oracle acceptance tests for `buildgen.py` | `AGENTS.md`, `build.rs` | **KEEP** | Acceptance suite for `buildgen.py` |
| `tools/export_transfer.py` | 338 | Transfers obfuscated IL2CPP export names across `GameAssembly.dll` updates | `AGENTS.md`, `buildgen.py`, `discover.rs` | **KEEP** | Used by `buildgen.py` / build-update workflow |
| `tools/symmap.py` | 161 | Generates `symbols.rs` + `symbols-<sha12>.json` from `GameAssembly.dll` & `UnityPlayer.dll` | `AGENTS.md`, `refresh.sh`, `symbols.rs` | **KEEP** | Core export map generator (update path when moving `symbols.rs` to `hromatite-il2cpp`) |
| `tools/api_map.py` | 428 | Parses `UnityPlayer.dll` lazy resolver stubs → `api_map.json` | `refresh.sh`, `AGENTS.md`, `test_api_map.py` | **KEEP** (or review) | Invoked by `refresh.sh --check`; note `DEFAULT_OUT` (`crates/hromatite-core/src/api_symbols.rs` line 43) does not exist |
| `tools/test_api_map.py` | 97 | Offline unit tests for `api_map.py` | `AGENTS.md` | **KEEP** (with `api_map.py`) | Tests `api_map.py` |
| `tools/test_gc_detour_abi.py` | 136 | Disassembles `gc_context_detour` in release `hromatite.dll` to verify Win64 ABI | `AGENTS.md` | **KEEP** | Active ABI regression test on release DLL |
| `tools/test-console-logs.py` | 199 | PTY integration test for `hromatite-console` multi-log switching | `AGENTS.md` | **KEEP** | Active TUI test |
| `tools/test-console-wheel.py` | 147 | PTY integration test for `hromatite-console` SGR mouse-wheel scrolling | `AGENTS.md` | **KEEP** | Active TUI test |
| `tools/host_scan.py` | 152 | Ad-hoc `/proc/<pid>/mem` scanner for `Il2CppClass` by name/namespace | `AGENTS.md` | **KEEP** (or **ARCHIVE**) | Useful host-only RE helper when `ptrace_scope=0` |
| `tools/gamediff.py` | 84 | Diffs `api_map.json` (schema 2) and `dump.py` JSON outputs | `AGENTS.md`, `docs/baselines/README.md` | **ARCHIVE** (or **KEEP** if trimmed to `api_map` diff) | Partially tied to deferred `dump.py` pipeline |
| `tools/gct_table.py` | 95 | Early standalone RVA finder for `GC_threads[]` (docstring cites deleted `gcthreads.rs` candidate list) | `AGENTS.md` | **ARCHIVE** / **DELETE** | Fully superseded by `buildgen.py` (`GcAnchors::threads`) |
| `tools/resolve_api.py` | 129 | Early offline pattern matcher referencing hardcoded `~/Downloads/Il2CppKnownPatterns.inl` | `AGENTS.md` | **DELETE** / **ARCHIVE** | Superseded by `api_map.py` + `buildgen.py` + `export_transfer.py` |
| `tools/meta_parse.py` | 306 | Python mirror of deleted `hromatite-core::metadata` for decrypted `global-metadata.dat` | `AGENTS.md`, `docs/archive/*` | **ARCHIVE** / **DELETE** | Part of abandoned `IL2CPP-DUMP` pipeline (`docs/archive/README.md`) |
| `tools/dump.py` | 74 | Exports decrypted metadata via `meta_parse.py` | `gamediff.py`, `docs/archive/*` | **ARCHIVE** / **DELETE** | Part of abandoned `IL2CPP-DUMP` pipeline |
| `tools/il2cpp_methods.py` | 189 | Converts `Il2CppDumper/script.json` → `methods-*.json` / `game_methods.rs` | `il2cppdump.sh`, `docs/archive/*` | **ARCHIVE** / **DELETE** | Part of abandoned `IL2CPP-DUMP` pipeline (`game_methods.rs` never existed) |
| `tools/test_dumps.py` | 49 | Tests `dump.py` + `gamediff.py` | `docs/HROMATITE-SKILLS-AND-MEMORY.md` only | **ARCHIVE** / **DELETE** | Tests dead `dump.py` |
| `tools/test_il2cpp_methods.py` | 142 | Tests `il2cpp_methods.py` | `AGENTS.md`, `docs/archive/IL2CPP-DUMP.md` | **ARCHIVE** / **DELETE** | Tests dead `il2cpp_methods.py` |
| `tools/test_meta_parse.py` | 122 | Tests `meta_parse.py` | `docs/HROMATITE-SKILLS-AND-MEMORY.md` only | **ARCHIVE** / **DELETE** | Tests dead `meta_parse.py` |

#### Shell Scripts (14)

| File | Lines | Purpose | Active Refs | Disposition | Rationale |
|---|---:|---|---|---|---|
| `tools/hromatite-launch.sh` | 476 | Steam Launch Options wrapper, reads `hromatite-mode`, attaches/injects | `hromatite-mode`, `AGENTS.md`, `eac_probe.sh` | **KEEP** | Primary game launch & injection entry point |
| `tools/hromatite-tui.sh` | 20 | Builds & runs host `hromatite-console` | `hromatite-autoconsole.sh`, `docs/HOT-RELOAD.md`, `AGENTS.md` | **KEEP** | Primary TUI launcher |
| `tools/hromatite-autoconsole.sh` | 118 | Watches `~/.hromatite-live` and opens terminal with `hromatite-tui.sh` | `hromatite-launch.sh`, `AGENTS.md` | **KEEP** | Active auto-console watcher |
| `tools/hromatite-overlay.sh` | 51 | Builds & launches `hromatite-overlay` via `nix-shell -p xorg.libX11 xorg.libXext` | `AGENTS.md`, `hromatite-overlay/src/main.rs` | **KEEP** | Primary overlay launcher |
| `tools/hromatite-reload.sh` | 181 | Builds & atomically publishes `features-N-<sha12>.dll` + `current.manifest` | `hromatite-watch.sh`, `test-hromatite-hot.sh`, `docs/HOT-RELOAD.md` | **KEEP** | Core hot-reload publisher |
| `tools/hromatite-watch.sh` | 102 | Opt-in polling watcher calling `hromatite-reload.sh` | `test-hromatite-hot.sh`, `docs/HOT-RELOAD.md`, `AGENTS.md` | **KEEP** | Active hot-reload watcher |
| `tools/test-hromatite-hot.sh` | 340 | Hermetic regression tests for `hromatite-reload.sh` & `hromatite-watch.sh` | `docs/HOT-RELOAD.md`, `AGENTS.md` | **KEEP** | Active publisher/watcher test suite |
| `tools/test-hot-smoke.sh` | 49 | Wine/Proton smoke test via `Hromatite.exe hot-smoke` | `AGENTS.md` | **KEEP** (fix `abi=2`!) | Active smoke test, **broken at line 30** (`abi=1` vs `ABI_VERSION=2`) |
| `tools/eac_probe.sh` | 312 | Post-run PASS/FAIL/UNKNOWN log analyzer for EAC/Photon | `hromatite-launch.sh` | **KEEP** | Active EAC telemetry verifier |
| `tools/refresh.sh` | 38 | Runs `symmap.py` + `api_map.py --check` on game update & appends `AGENTS.md` | `AGENTS.md` | **KEEP** | Active post-update refresh (update `symbols.rs` path in Stage 4) |
| `tools/test-loadlib.sh` | 48 | Isolated Proton rig for `--mode loadlib` + PEB-unlink | `AGENTS.md` | **ARCHIVE** (or **KEEP**) | Manual test rig if `--mode loadlib` remains supported |
| `tools/hromatite-console.sh` | 188 | Old shell log tailer + `--status` (`SEAL_SIG` mem scanner) | `AGENTS.md`, `docs/archive/IL2CPP-DUMP.md` | **DELETE** / **ARCHIVE** | Superseded by Rust `hromatite-console` (`hromatite-tui.sh`) and `--seal` removal |
| `tools/test-seal.sh` | 91 | Test rig for `inject --seal` (L2 whole-image seal) | `AGENTS.md` | **DELETE** | Obsolete: L2 `--seal` mode removed from runtime (`hromatite-mode` & `AGENTS.md` §0) |
| `tools/il2cppdump.sh` | 146 | Host half of `dumpmeta` → `Il2CppDumper` pipeline | `AGENTS.md`, `docs/archive/IL2CPP-DUMP.md` | **ARCHIVE** / **DELETE** | `dumpmeta` was removed from payload (`docs/archive/README.md`) |

#### `tools/data/` (8 files, 240 KB total)
- **Active (`buildgen.py` / `test_buildgen.py`)**: `build-648f61b13e4f.json` (2,614 B), `build-245768f25214.json` (3,146 B), `il2cpp-export-names.json` (100,555 B), `il2cpp-export-names-245768f2.json` (79,649 B).
- **Early/helper inputs (`api_map.py` / reference)**: `il2cpp-api-anchors.json` (939 B), `il2cpp-api-names.txt` (6,415 B), `Il2CppKnownPatterns.inl` (29,289 B), `message.txt` (6,049 B, `g_Il2CppNames` C++ vector).

---

### 1.4 Root Artifacts & `AGENTS.md` (§3 and End of §17)

- **Root artifacts**:
  - `hromatite-mode` (49 lines, 3,033 B, tracked): Active runtime env config read by `tools/hromatite-launch.sh` (`HROMATITE_UI=1`, `HROMATITE_DRAW=0`, `HROMATITE_PLAYER_LOOP=1`).
  - `hromatite-mode.launch-bak.1733464` (49 lines, 3,033 B, **untracked** in `git status`): Identical copy of `hromatite-mode`, leftover backup from `tools/hromatite-launch.sh:126` (`$MODE_FILE.launch-bak.$$`) after an interrupted run → safe to delete.
  - `symbols-245768f25214.json` (22,441 B, tracked): Current build symbol map emitted by `tools/symmap.py` (referenced by `docs/baselines/README.md:9-10`).
  - `api_map.json` (67,292 B, tracked): Output of `tools/api_map.py`.
  - `docs/` (21 files, 14,430 lines total): 16 root docs (`EAC-*.md`, `HOT-RELOAD.md`, `IL2CPP-EXPORT-NAMES.md`, `PROMPT-BUILD-GENERATOR.md`, `PROMPT-EAC-CUT.md`, `UI-NUKLEAR-PLAN.md`, `HROMATITE-SKILLS-AND-MEMORY.md` 9,979 lines), `docs/archive/` (`IL2CPP-DUMP.md`, `PROMPT-il2cpp-base.md`, `README.md`), `docs/baselines/` (`README.md`, `symbols-648f61b13e4f.json`).
  - `var/` (gitignored except `var/README.md` 80 lines): Runtime logs, crash dumps, mode backups, hot-reload modules, GameAssembly cache, `EAC-RE/`, and `research/`, symlinked from `$HOME` and Proton prefix.
- **`AGENTS.md` (4,955 lines / 469,465 B)**:
  - **§3 «Структура репозитория» (lines 131–186)**: Severely outdated — still depicts the initial M0/M1 layout (`Cargo.toml` with only `core, dll, test-target`, `crates/hromatite-core/{ffi,log,procs,remote,pe,manualmap,il2cpp,symbols}.rs`), without `hromatite-abi`, `hromatite-commands`, `hromatite-eac`, `hromatite-features`, `hromatite-nuklear`, `hromatite-console`, `hromatite-overlay`, or the refactored Stage 2–4 crates. Must be updated in Stage 5 to reflect the final crate graph, build commands (including host crates), and CLI flags.
  - **§17 «Журнал работ» (starts line 766, ends line 4955)**: Last entry is `### 2026-11-18 — Исправление краша UI (HROMATITE_UI=1): косвенность указателей D3D11, упаковка VB/IB, сохранение состояния и per-thread VEH` (lines 4924–4955, documenting commit `5064586`). Format convention per header rule & recent entries:
    - Append-only `### YYYY-MM-DD — <Short Title>` subsection at the bottom of §17.
    - Bullet items: what changed (file paths, symbols), commit SHA(s), `Чем подтверждено:` (exact test/clippy/build commands & counts), and `Осталось:` (remaining live/manual checks).

---

## 2. Logic Chain

1. **Host crates (`hromatite-console`, `hromatite-overlay`) are decoupled from the root `[workspace]`** (`crates/hromatite-console/Cargo.toml:23`, `crates/hromatite-overlay/Cargo.toml:12`) because root `.cargo/config.toml` targets `x86_64-pc-windows-gnu` whereas they build natively for `x86_64-unknown-linux-gnu`. Therefore Stage 1/3/4/5 CI/verification must run `cargo test` and `cargo clippy` separately with `--manifest-path` + `--target x86_64-unknown-linux-gnu` for both host crates.
2. **Decomposition of `hromatite-overlay/src/main.rs` and `hromatite-console/src/model.rs`** follows naturally from their existing comment-delimited sections: separating pure state/layout/telemetry parsing from terminal/X11 side effects keeps public behavior identical while enabling unit tests on `hromatite-overlay`'s `Config`, `Telemetry`, `build_rows`/`place`/`hit`, and `rounded_mask` (currently 0 tests).
3. **`hot` unification requires `hromatite-abi` (not `hromatite-core`) as the shared contract location**, because `hromatite-console` is a Linux host crate that already depends on `hromatite-commands` (`crates/hromatite-console/Cargo.toml:12`) and cannot pull in Windows-only FFI. Moving `HotConfig`, `HotReload` (or status wire types `HotPhase`/`HotStatus`), `parse_hot_status`/`format_hot_status`, `parse_feature_manifest`, `parse_versioned_name`, and `parse_reload_request` into `hromatite-abi` (or a pure `hromatite-hot` crate depended on by `hromatite-abi`/`hromatite-runtime`/`hromatite-console`) eliminates `HotState` vs `Phase` drift (`Stable`/`Requested`/`Canary` → `Other`) and manifest vs struct ABI ambiguity.
4. **Cleaning `hromatite-abi`, `hromatite-commands`, and `tools/`**:
   - Replacing `localmap::feature_api_bytes_valid`'s manual field list with a complete check (including `frame`, plus `feature_header_valid`) closes a real ABI v2 validation gap.
   - Moving the hardcoded 13-command resident list out of `core/src/raw/hot.rs` and `features/src/feature.rs` into a `hromatite-commands` helper fixes the missing `"ui"` routing bug.
   - Archiving/deleting the `IL2CPP-DUMP` Python/shell tools (`meta_parse.py`, `dump.py`, `il2cpp_methods.py`, `il2cppdump.sh`, `test_dumps.py`, `test_il2cpp_methods.py`, `test_meta_parse.py`) and obsolete `--seal`/`shell-console` tools (`test-seal.sh`, `hromatite-console.sh`, `gct_table.py`, `resolve_api.py`) removes ~1,900 lines of dead tooling while preserving active `buildgen`/`symmap`/`launch`/`reload`/`eac_probe`/`tui`/`overlay` workflows.

---

## 3. Caveats

- All line counts in §1.1 were measured via `wc -l` (counts `\n`, so a file without a trailing newline or viewed in `view_file` shows `wc_lines + 1` in 1-indexed line display).
- `crates/hromatite-core/src/raw/` internals (other than `hot.rs`, `hostapi.rs`, `localmap.rs`, `console.rs`, `m3dump.rs`), `crates/hromatite-eac/`, and `crates/hromatite-nuklear/` were inventoried by file/line count only, as their deep decomposition analysis belongs to Survey Explorers 1 & 2.
- Disposition of `tools/api_map.py` + `api_map.json` + `tools/gamediff.py` depends on whether `tools/refresh.sh` keeps running `api_map.py --check` or drops it in favor of `buildgen.py` alone; flagged explicitly in the table.
- No build/test commands (`cargo test`, `cargo clippy`, `python3 -m unittest`) were executed in this read-only survey.

---

## 4. Conclusion

1. **Inventory complete**: Target crates total **7,954 lines** across `abi` (674), `commands` (603), `console` (4,660), `overlay` (2,038), and `test-target` (29), alongside 40,946 lines in `core`/`dll`/`eac`/`features`/`nuklear`.
2. **Stage 4 decompositions ready**:
   - Split `crates/hromatite-overlay/src/main.rs` into `lib.rs` + `x11.rs`, `draw.rs`, `config.rs`, `telemetry.rs`, `demo.rs`, `panel.rs`, `esp.rs`, `main.rs`.
   - Split `crates/hromatite-console/src/model.rs` into `model/mod.rs`, `model/buffers.rs`, `model/observe.rs`, `model/input.rs`.
3. **Stage 5 `hot`, `abi`, and `commands` cleanup targets**:
   - Unify `core/src/hot.rs`, `core/src/raw/hot.rs` (pure lines 9–223), and `console/src/hot.rs` around shared pure types/parsers (`HotPhase`, `HotStatus`, `HotConfig`, `HotReload`, `parse_feature_manifest`, `parse_hot_status`) accessible to both Windows runtime and Linux console.
   - Fix `localmap::feature_api_bytes_valid` to validate `FeatureApiV1.frame` and reuse `hromatite_abi::feature_header_valid`.
   - Add a `hromatite_commands` resident-command predicate (`Scope::Resident` excluding router commands `reload`/`rollback`/`config`) so `ui` is routed properly and `hot.rs`/`feature.rs` drop hardcoded lists.
   - Remove untracked `hromatite-mode.launch-bak.1733464`, fix `tools/test-hot-smoke.sh:30` (`abi=2`), update `tools/symmap.py` & `tools/refresh.sh` if `symbols.rs` moves to `hromatite-il2cpp`, prune/archive the 11 obsolete/deferred scripts in `tools/`, and update `AGENTS.md` §3 and §17.

---

## 5. Verification Method

Independent verification commands (read-only):

```bash
# 1. Verify exact line counts in target crates and tools/
wc -l crates/hromatite-abi/Cargo.toml crates/hromatite-abi/src/*.rs \
      crates/hromatite-commands/Cargo.toml crates/hromatite-commands/src/*.rs \
      crates/hromatite-console/Cargo.toml crates/hromatite-console/src/*.rs crates/hromatite-console/tests/*.rs \
      crates/hromatite-overlay/Cargo.toml crates/hromatite-overlay/src/*.rs \
      crates/test-target/Cargo.toml crates/test-target/src/*.rs

# 2. Verify host crates build & test independently of root Windows workspace
cargo test --manifest-path crates/hromatite-console/Cargo.toml --target x86_64-unknown-linux-gnu
cargo clippy --manifest-path crates/hromatite-console/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu -- -D warnings
nix-shell -p xorg.libX11 xorg.libXext --run \
  "cargo clippy --manifest-path crates/hromatite-overlay/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu -- -D warnings"

# 3. Verify ABI/command/tool findings directly in source
sed -n '306,327p' crates/hromatite-core/src/raw/localmap.rs   # missing `frame` check
sed -n '1225,1247p' crates/hromatite-core/src/raw/hot.rs       # hardcoded list missing `ui`
sed -n '25,35p' tools/test-hot-smoke.sh                        # stale `abi=1` assertion
git status --short                                             # untracked hromatite-mode.launch-bak.1733464
```

**Invalidation conditions**: Any refactor commit modifying `crates/hromatite-abi/`, `crates/hromatite-commands/`, `crates/hromatite-console/`, `crates/hromatite-overlay/`, `tools/`, or `AGENTS.md` will change the line counts and file paths reported above.
