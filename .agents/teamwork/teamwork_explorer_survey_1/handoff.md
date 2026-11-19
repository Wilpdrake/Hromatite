# Handoff Report — Survey Explorer 1 (Workspace, Build/Test/Clippy Baseline & Dependency Graph)

**Core finding:** The workspace builds cleanly for `x86_64-pc-windows-gnu` (debug and release, `clippy` 0 warnings), and all **416** workspace tests + **1** doctest pass on `x86_64-unknown-linux-gnu` (`clippy` 8 warnings), while `cargo test --workspace` without `--target` fails at runtime (`os error 8`) because `.cargo/config.toml` defaults to Windows without a Wine runner. Architecturally, the dependency graph violates Stage 2–3 boundaries: `hromatite-features` links `hromatite-core` + `hromatite-nuklear` transitively via `hromatite-eac`, `Hromatite.exe` shares `hromatite-core` with `hromatite.dll`, and `hromatite-overlay` omits workspace lints (15 `undocumented_unsafe_blocks` errors if enforced).

---

## 1. Observation

### 1.1 Git Baseline State
- Command: `git status && git branch --show-current && git log -n 10 --oneline` (exit code `0`)
- Branch: `master`
- HEAD: `5064586 fix(ui): pass D3D11 single-object pointers by value, pack VB/IB once per frame, and isolate VEH guard lanes per thread`
- Working tree: clean of tracked modifications; untracked entries only:
  - `.agents/`
  - `hromatite-mode.launch-bak.1733464`
- Recent 10 commits:
  ```
  5064586 (HEAD -> master) fix(ui): pass D3D11 single-object pointers by value, pack VB/IB once per frame, and isolate VEH guard lanes per thread
  8144de5 docs(agents): log the project digest file in §17
  282a20b docs(project): single-file digest of project skills, memory and repo state
  eca7a6c docs(agents): next live run is nuklear-only (UI=1, DRAW=0) — acceptance criteria
  4771601 docs(agents): live crash triage — UpdateSubresource box bug, single-thread VEH guard
  411b30f fix(ui): UpdateSubresource had pDstBox dropped; UI off by default
  b561a68 docs(agents): live run — Present hook installed; backend fails on GetImmediateContext
  4558ee0 fix(ui): probe the GetImmediateContext slot window instead of trusting one index
  1235e9e chore(ui): log the device pointer and surrounding vtable slots on backend failure
  66383dc docs(agents): live run showed PAGE_EXECUTE_READ was missed — fixed and recorded
  ```

### 1.2 Cargo Configuration & Manifest Inventory
- `.cargo/config.toml` (lines 1–10):
  - `[build] target = "x86_64-pc-windows-gnu"`
  - `[target.x86_64-pc-windows-gnu]`: `linker = "x86_64-w64-mingw32-gcc"`, `ar = "x86_64-w64-mingw32-ar"`, `rustflags = ["-C", "link-arg=-L/run/current-system/sw/lib"]`.
- Root `Cargo.toml` (lines 1–39):
  - `resolver = "3"`, `edition = "2024"`, `[profile.release] opt-level = 2, panic = "unwind"`.
  - `[workspace.lints.rust] unsafe_op_in_unsafe_fn = "deny"`
  - `[workspace.lints.clippy] undocumented_unsafe_blocks = "deny"`, `missing_safety_doc = "deny"`
  - Workspace members (8 crates + root `Hromatite` = 9 packages):
    `crates/hromatite-abi`, `crates/hromatite-commands`, `crates/hromatite-core`, `crates/hromatite-dll`, `crates/hromatite-eac`, `crates/hromatite-features`, `crates/hromatite-nuklear`, `crates/test-target`.
- Standalone crates outside `workspace.members` (3 `Cargo.toml` files found via `find_by_name`):
  1. `crates/hromatite-console/Cargo.toml` — `[workspace]`, host Linux binary+lib, duplicates `[lints.rust]` and `[lints.clippy]` from root `Cargo.toml`, `[profile.release] opt-level = 2, strip = true`.
  2. `crates/hromatite-overlay/Cargo.toml` — `[workspace]`, host Linux binary (`X11`/`Xext` FFI), **no `[lints]` section**, `[profile.release] opt-level = 2`.
  3. `var/research/helpdump/Cargo.toml` — `[workspace]`, research scratch binary depending on `crates/hromatite-commands`, no `[lints]` section.

### 1.3 Baseline Build, Test & Clippy Matrix

| Scope | Exact Command | Exit Code | Result Summary |
|---|---|---|---|
| Workspace tests (default Windows target) | `cargo test --workspace` | `101` | Compiles cleanly, then fails executing `target/x86_64-pc-windows-gnu/debug/deps/Hromatite-dd8f6c1f910d1c6e.exe`: `Exec format error (os error 8)` (no Wine runner configured). |
| Workspace tests (compile-only Windows) | `cargo test --workspace --no-run` | `0` | All 16 test binaries for `x86_64-pc-windows-gnu` compiled with 0 warnings/errors. |
| Workspace tests (Linux host target) | `cargo test --workspace --target x86_64-unknown-linux-gnu` | `0` | **416 passed, 0 failed** + **1 doctest ok** (`hromatite-nuklear`). Emits **3 warnings** in `hromatite-core` (plus 3 duplicate lines for lib test). |
| Workspace clippy (default Windows target) | `cargo clippy --workspace --all-targets` | `0` | **0 warnings, 0 errors.** |
| Workspace clippy (Linux host target) | `cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu` | `0` | **8 warnings** in `hromatite-core` (plus 8 duplicate lines for lib test), 0 errors. |
| Workspace build (debug, Windows) | `cargo build --workspace` | `0` | Built `Hromatite.exe` (17M), `hromatite.dll` (22M), `hromatite_features.dll` (24M), `test-target.exe` (4.9M). |
| Workspace build (release, Windows) | `cargo build --workspace --release` | `0` | Succeeds with 0 warnings/errors. |
| `hromatite-console` tests (Linux target) | `cargo test --manifest-path crates/hromatite-console/Cargo.toml --target x86_64-unknown-linux-gnu` | `0` | **67 passed, 0 failed** (58 unit + 6 `tests/hot_reload.rs` + 3 `tests/snapshot.rs`). |
| `hromatite-console` clippy (Linux target) | `cargo clippy --manifest-path crates/hromatite-console/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu` | `0` | **0 warnings, 0 errors.** |
| `hromatite-console` clippy (without `--target`) | `cargo clippy --manifest-path crates/hromatite-console/Cargo.toml --all-targets` | `101` | Inherits `.cargo/config.toml` (`x86_64-pc-windows-gnu`) → **5 errors** (`E0433` `std::os::unix`, `E0599` `.dev()`/`.ino()` in `src/fileio.rs`). |
| `hromatite-overlay` tests (bare env) | `cargo test --manifest-path crates/hromatite-overlay/Cargo.toml --target x86_64-unknown-linux-gnu` | `101` | Linker failure: `unable to find library -lX11`, `-lXext` (missing in bare env; `tools/hromatite-overlay.sh` wraps with `nix-shell`). |
| `hromatite-overlay` tests (`nix-shell`) | `nix-shell -p xorg.libX11 xorg.libXext --run "cargo test --manifest-path crates/hromatite-overlay/Cargo.toml --target x86_64-unknown-linux-gnu"` | `0` | **0 tests** (compiles, links, runs 0 tests). |
| `hromatite-overlay` clippy (Linux target) | `cargo clippy --manifest-path crates/hromatite-overlay/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu` | `0` | **9 warnings** (plus 9 duplicates), 0 errors (no `[lints]` in `Cargo.toml`). |
| `hromatite-overlay` clippy (without `--target`) | `cargo clippy --manifest-path crates/hromatite-overlay/Cargo.toml --all-targets` | `0` | Same **9 warnings** (plus 9 duplicates); checks as windows target only because `main.rs` has no `#[cfg(unix)]` or `std::os::unix` imports, though it would fail linking on Windows. |
| `hromatite-overlay` with workspace lints | `cargo clippy --manifest-path crates/hromatite-overlay/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu -- -D unsafe_op_in_unsafe_fn -D clippy::undocumented_unsafe_blocks -D clippy::missing_safety_doc` | `101` (clippy failed) | **15 errors** (`undocumented_unsafe_blocks`), **9 warnings**. |
| `var/research/helpdump` check | `cargo check --manifest-path var/research/helpdump/Cargo.toml --target x86_64-unknown-linux-gnu` | `0` | Checks cleanly. |

Per-crate test breakdown (`cargo test --workspace --target x86_64-unknown-linux-gnu`):
- `Hromatite`: 0
- `hromatite-abi`: 8
- `hromatite-commands`: 4
- `hromatite-core`: 239 (225 unit + 7 `tests/frames.rs` + 7 `tests/report_stream.rs`)
- `hromatite-dll`: 6
- `hromatite-eac`: 122 (118 unit + 0 `tests/eos_install.rs` + 1 `tests/epochs.rs` + 0 `tests/hook_engine.rs` + 3 `tests/keyhunt_scan.rs`)
- `hromatite-features`: 24
- `hromatite-nuklear`: 13 (3 unit + 10 `tests/frame.rs`) + 1 doctest
- `test-target`: 0

Warnings on `x86_64-unknown-linux-gnu` in `hromatite-core` (8 clippy warnings):
1. `crates/hromatite-core/src/m3.rs:46` — `unused_imports`: `use crate::procs;`
2. `crates/hromatite-core/src/raw/m3dump.rs:36` — `clippy::duplicated_attributes`: `#![cfg_attr(not(windows), allow(dead_code))]` duplicates `#[allow(dead_code)]` at `crates/hromatite-core/src/raw/mod.rs:31`
3. `crates/hromatite-core/src/raw/d3d11_shaders.rs:5` — `dead_code`: `VERTEX_SHADER`
4. `crates/hromatite-core/src/raw/d3d11_shaders.rs:121` — `dead_code`: `PIXEL_SHADER`
5. `crates/hromatite-core/src/raw/hostapi.rs:122` — `clippy::needless_return`
6–8. `crates/hromatite-core/src/raw/kiero.rs:266, 348, 404` — `clippy::needless_return` (3 occurrences)

Warnings in `hromatite-overlay` (`crates/hromatite-overlay/src/main.rs`):
- `clippy::upper_case_acronyms`: line 44 (`type XID`)
- `clippy::too_many_arguments`: lines 803 (`fill`), 817 (`round_rect`), 863 (`line`), 871 (`circle`), 903 (`arc`) (5 warnings; line 1027 already has `#[allow(clippy::too_many_arguments)]`)
- `clippy::unnecessary_cast`: line 1176 (`height as u32`)
- `clippy::collapsible_if`: lines 1552, 1627 (2 warnings)
- Under workspace lints: 15 `clippy::undocumented_unsafe_blocks` errors in `src/main.rs` (including lines 1584, 1586, 1636, 1639, 1640, 1642, etc.).

### 1.4 Crate Dependency Graph & Crate Types

#### Root Workspace (`Cargo.toml`, default target `x86_64-pc-windows-gnu`)

| Crate | Path | Crate Type(s) | Direct Internal Deps | Direct External Deps | Build Script / C |
|---|---|---|---|---|---|
| `Hromatite` | `src/main.rs` | `bin` (`Hromatite.exe`) | `hromatite-core` | none | — |
| `hromatite-dll` | `crates/hromatite-dll` | `cdylib` (`hromatite.dll`) | `hromatite-core`, `hromatite-eac` | none | — |
| `hromatite-features` | `crates/hromatite-features` | `cdylib` (`hromatite_features.dll`) | `hromatite-abi`, `hromatite-commands`, `hromatite-eac` | none | — |
| `hromatite-eac` | `crates/hromatite-eac` | `rlib` | `hromatite-core` | none | — |
| `hromatite-core` | `crates/hromatite-core` | `rlib` | `hromatite-abi`, `hromatite-commands`, `hromatite-nuklear` | none | — |
| `hromatite-nuklear` | `crates/hromatite-nuklear` | `rlib` | none | none (runtime) | `build.rs` uses `cc = "1"` (`1.11.0` + `find-msvc-tools 0.1.15`, `shlex 2.0.1`) to compile `nuklear_shim.c` |
| `hromatite-abi` | `crates/hromatite-abi` | `rlib` | none | none | — |
| `hromatite-commands` | `crates/hromatite-commands` | `rlib` | none | none | — |
| `test-target` | `crates/test-target` | `bin` (`test-target.exe`) | none | none | — |

#### Standalone Host Crates (`[workspace]`, target `x86_64-unknown-linux-gnu`)

| Crate | Path | Crate Type(s) | Direct Internal Deps | Direct External Deps | Native Links |
|---|---|---|---|---|---|
| `hromatite-console` | `crates/hromatite-console` | `lib` + `bin` (`hromatite-console`) | `hromatite-commands` | `unicode-width = "0.2.3"` | libc `tcgetattr`/`tcsetattr`/`ioctl` |
| `hromatite-overlay` | `crates/hromatite-overlay` | `bin` (`hromatite-overlay`) | none | none | `-lX11`, `-lXext` |
| `helpdump` | `var/research/helpdump` | `bin` (`helpdump`) | `hromatite-commands` | none | — |

Dependency edges observed in `Cargo.lock` (lines 5–85):
- `Hromatite -> hromatite-core -> {hromatite-abi, hromatite-commands, hromatite-nuklear}`
- `hromatite-dll -> {hromatite-core, hromatite-eac}`
- `hromatite-eac -> hromatite-core`
- `hromatite-features -> {hromatite-abi, hromatite-commands, hromatite-eac}` (therefore **transitively** `-> hromatite-core -> hromatite-nuklear`!)

### 1.5 Unsafe Posture & Existing `#[allow(...)]` Suppressions
Workspace lints in `Cargo.toml` (`unsafe_op_in_unsafe_fn = "deny"`, `undocumented_unsafe_blocks = "deny"`, `missing_safety_doc = "deny"`) are inherited by all 9 workspace crates via `[lints] workspace = true` and duplicated in `hromatite-console/Cargo.toml`, but **absent** in `hromatite-overlay/Cargo.toml` and `var/research/helpdump/Cargo.toml`.

`unsafe_code` policy per crate:
- `#![forbid(unsafe_code)]`: `Hromatite` (`src/main.rs:9`), `test-target` (`crates/test-target/src/main.rs:4`), `hromatite-abi` (`crates/hromatite-abi/src/lib.rs:6`), `hromatite-commands` (`crates/hromatite-commands/src/lib.rs:12`), `hromatite-console` (`crates/hromatite-console/src/lib.rs:1`, though `crates/hromatite-console/src/main.rs` has `unsafe extern "C"` + `unsafe` termios/ioctl blocks).
- `#![deny(unsafe_code)]` with selective `#[allow(unsafe_code)]`:
  - `hromatite-core/src/lib.rs:6,19` (`mod raw;`)
  - `hromatite-dll/src/lib.rs:7,327` (`DllMain`)
  - `hromatite-nuklear/src/lib.rs:33,35` (`mod sys;`)
  - `hromatite-eac/src/lib.rs:39,45-58` (6 modules: `eacimage`, `eos`, `evp`, `m3probe`, `hook`, `keyhunt`; plus `#![allow(unsafe_code)]` in `tests/hook_engine.rs:16`, `tests/eos_install.rs:20`).
- `#![allow(unsafe_code)]` crate-wide: `hromatite-features/src/lib.rs:5`.
- No `unsafe_code` attribute: `hromatite-overlay/src/main.rs`.

Notable `dead_code` / other `#[allow(...)]` suppressions:
- `crates/hromatite-core/src/raw/mod.rs:31` (`#[allow(dead_code)] pub mod m3dump;`) + `m3dump.rs:36` (`#![cfg_attr(not(windows), allow(dead_code))]`)
- `crates/hromatite-core/src/raw/{ffi.rs,hostapi.rs,memscan.rs,localmap.rs}` (`#![allow(dead_code)]`, `ffi.rs` also `non_snake_case, non_camel_case_types`)
- `crates/hromatite-core/src/raw/{d3d11,ui,kiero,console,vmt,photon,iat}.rs`, `crates/hromatite-eac/src/{hook,evp,eos}.rs`: `#![cfg_attr(not(windows), allow(dead_code))]`
- `crates/hromatite-core/src/raw/hot.rs:21`: `#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]`
- `crates/hromatite-core/src/raw/hook.rs:1268`, `discover.rs:51`, `hromatite-nuklear/src/lib.rs:36,234` (`#[allow(dead_code)]`)
- `crates/hromatite-eac/src/keyhunt.rs:301`: `#[allow(clashing_extern_declarations)]`
- `crates/hromatite-overlay/src/main.rs:27`: `#![allow(non_snake_case)]`

---

## 2. Logic Chain

1. **Transitive leakage into `hromatite-features` (Stage 3 & 4 gap):**
   - `hromatite-features/src/lib.rs:3-4` states: *"No core runtime, VEH handler, panic hook or leaked cache lives in this image."*
   - Yet `hromatite-features/Cargo.toml:14` depends on `hromatite-eac`, and `hromatite-eac/Cargo.toml:14` depends on `hromatite-core` (used by `eacimage.rs`, `m3probe.rs`, `forge.rs`, `eos.rs`, plus doc/comment refs in `report.rs`).
   - By `Cargo.lock:36-74`, `hromatite-features` transitively compiles and links `hromatite-core` and `hromatite-nuklear` (including `nuklear_shim.c`), bloating `hromatite_features.dll` to 24 MB and breaking isolation.
   - Furthermore, `hromatite-features/src/eac.rs:547` calls `hromatite_eac::eos::module_report` directly and `eac.rs:29` imports `Hunter` (`keyhunt`), pulling OS/FFI-heavy `hromatite-eac` modules into the feature DLL.

2. **Missing entry-point split between injector and resident (Stage 3 gap):**
   - `src/main.rs:11-13` (`Hromatite.exe`) depends on `hromatite-core`, which bundles Win32/manualmap (`manualmap`, `remote`, `pe`, `procs`) together with IL2CPP (`m3`, `lateupdate`, `playerloop`, `photon`, `gcthreads`), D3D11/Nuklear (`d3d11`, `kiero`, `ui`, `hromatite-nuklear`), hooking (`hook`, `midhook`, `vmt`, `iat`, `hookset`), and resident runtime (`console`, `hostapi`, `localmap`, `raw::hot`).
   - Additionally, `src/main.rs:200` invokes `hot_runtime::smoke(&paths, rounds)` (`crates/hromatite-core/src/raw/hot.rs:1212`), coupling the CLI `hot-smoke` command directly to `raw::hot` (resident hot-reload router).
   - Conversely, `hromatite-dll/Cargo.toml:11` depends on `hromatite-core`, so `hromatite.dll` compiles `manualmap` and `remote` (cross-process injection code).

3. **Duplicate FFI/memory/hook layers across crates (Stage 2 gap):**
   - `hromatite-core/src/raw/{ffi.rs, memory.rs, context.rs, memscan.rs, procs.rs}` and `hromatite-core/src/pe.rs` live inside `hromatite-core` alongside domain logic.
   - Meanwhile, `hromatite-eac` declares its own Win32 FFI (`VirtualQuery` in `hook.rs:889` AND `keyhunt.rs:304`, plus calls to `hromatite_core::ffi::VirtualQuery` in `eacimage.rs:862,1009`) and implements a second x64 inline hook engine (`crates/hromatite-eac/src/hook.rs`, 1,792 lines) alongside `crates/hromatite-core/src/raw/hook.rs` (2,568 lines). `keyhunt.rs:301` even carries `#[allow(clashing_extern_declarations)]` because of duplicate `extern` bindings.
   - `crates/hromatite-features/src/memory.rs` (82 lines) sits in `hromatite-features` as diagnostic hex/address/qword formatting over `host::Host`.

4. **Monolithic files and duplicated `hot` implementations (Stages 4 & 5 gaps):**
   - Three `hot.rs` modules exist: `crates/hromatite-core/src/hot.rs` (944 lines, pure contracts), `crates/hromatite-core/src/raw/hot.rs` (1,396 lines, resident router + `smoke`), and `crates/hromatite-console/src/hot.rs` (98 lines, host log parser).
   - Large monolithic modules requiring decomposition per Stage 4: `eos.rs` (3,532 lines), `photon.rs` (2,717 lines), `hromatite-overlay/src/main.rs` (2,023 lines), `eacimage.rs` (1,658 lines), `report.rs` (1,644 lines), `hromatite-console/src/model.rs` (1,332 lines), `forge.rs` (1,228 lines).
   - Confirmed dead/unwired code: `crates/hromatite-core/src/raw/m3dump.rs` (`#[allow(dead_code)]` at `raw/mod.rs:31`), plus `VERTEX_SHADER` / `PIXEL_SHADER` in `d3d11_shaders.rs` and unused `use crate::procs;` in `m3.rs:46`.

5. **Host crate workspace & lint gaps (Stages 1, 4, R6):**
   - `hromatite-console` and `hromatite-overlay` have standalone `[workspace]` tables because `.cargo/config.toml` sets `[build] target = "x86_64-pc-windows-gnu"` at repo root. Consequently, running `cargo clippy` or `cargo test` on them without `--target x86_64-unknown-linux-gnu` fails (`hromatite-console`) or checks the wrong target (`hromatite-overlay`).
   - `hromatite-overlay/Cargo.toml` does not enforce the workspace lints (`unsafe_op_in_unsafe_fn`, `undocumented_unsafe_blocks`, `missing_safety_doc`); enforcing them reveals 15 `undocumented_unsafe_blocks` errors and 9 existing clippy warnings.
   - `hromatite-overlay` also requires `libX11` + `libXext` (available via `nix-shell -p libx11 libxext` or `xorg.libX11 xorg.libXext`), not present in the bare shell environment.
   - `hromatite-features/src/lib.rs:5` has crate-wide `#![allow(unsafe_code)]`, bypassing the tighter `#![deny(unsafe_code)]` + localized `#[allow(unsafe_code)]` pattern used in `hromatite-core` and `hromatite-dll`.

---

## 3. Caveats

- `cargo test --workspace` executes `.exe` test binaries directly unless a Wine runner is configured; Windows-target tests were verified to *compile* (`cargo test --workspace --no-run` exit `0`), while unit-test execution was verified on `x86_64-unknown-linux-gnu` (`416 passed`, `1 doctest ok`). Any `#[cfg(windows)]`-only runtime code paths in unit tests are not executed on the Linux host target.
- Internal symbol-by-symbol dependencies within `crates/hromatite-core/src/raw/*` (27 files) and the inventory of `tools/` (33 files) are only surveyed at module/file-count level here; detailed internal dependency and dead-code mapping belong to the companion survey explorers.
- `var/research/helpdump` is outside `crates/` (an ad-hoc research crate under `var/research/`) and was checked for Cargo layout completeness only.

---

## 4. Conclusion

Baseline build, clippy (Windows), and host-target test suites are green today (`416` workspace tests + `67` console tests + `0` overlay tests), providing a clean starting point at commit `5064586`. To satisfy Stages 1–5 in `ORIGINAL_REQUEST.md`, the Cargo/workspace refactoring must:
1. Extract `hromatite-win` (FFI/memory/VEH/PE/procs/remote) and `hromatite-hook` (single inline/midhook/vmt/iat/hookset engine) so `hromatite-eac` drops its dependency on `hromatite-core` and stops duplicating `VirtualQuery`/inline hooking.
2. Break the transitive `hromatite-features -> hromatite-eac -> hromatite-core -> hromatite-nuklear` chain by splitting pure EAC protocol/codec/session logic from `EOSSDK` hooks/runtime (or splitting `hromatite-eac` appropriately so `hromatite-features` links zero `core`/`win`/`hook`/`render` code).
3. Split `Hromatite` (`hromatite-injector`, `Hromatite.exe`) from `hromatite-runtime` + `hromatite-dll` (`hromatite.dll`), moving `hot-smoke` (`localmap` test) out of `raw::hot` / `hromatite-runtime` or into a dedicated injector/smoke module if kept.
4. Isolate `hromatite-il2cpp` and `hromatite-render` (`hromatite-nuklear`, `kiero`, `d3d11`, `d3d11_shaders`, `ui`).
5. Clean the 8 Linux-target clippy warnings in `hromatite-core`, remove `m3dump.rs` and unused shaders, bring `hromatite-overlay` under the 3 workspace safety lints (fixing 15 `undocumented_unsafe_blocks` + 9 clippy warnings), and tighten `#![allow(unsafe_code)]` in `hromatite-features`.

---

## 5. Verification Method

Run from `/home/wilpdrake/RustroverProjects/Hromatite` (read-only checks):
```bash
git status && git branch --show-current && git log -n 10 --oneline
cargo build --workspace
cargo test --workspace --no-run
cargo test --workspace --target x86_64-unknown-linux-gnu
cargo clippy --workspace --all-targets
cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu
cargo test --manifest-path crates/hromatite-console/Cargo.toml --target x86_64-unknown-linux-gnu
cargo clippy --manifest-path crates/hromatite-console/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu
nix-shell -p xorg.libX11 xorg.libXext --run "cargo test --manifest-path crates/hromatite-overlay/Cargo.toml --target x86_64-unknown-linux-gnu"
cargo clippy --manifest-path crates/hromatite-overlay/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu -- -D unsafe_op_in_unsafe_fn -D clippy::undocumented_unsafe_blocks -D clippy::missing_safety_doc
```
**Invalidation conditions:** Any edit to `Cargo.toml`, `.cargo/config.toml`, `Cargo.lock`, or source files invalidates the counts and exit codes above.
