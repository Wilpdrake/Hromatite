# M1 Empirical Challenge Handoff — `challenger_m1_1`

**Verdict:** `APPROVE` (with one non-blocking §6.1 disposition note on `tools/test_gc_detour_abi.py`)

---

## 1. Observation

### 1.1 Baseline Commit & Workspace State
- `git rev-parse --short HEAD` → `5064586`.
- `git status --short` → `?? .agents/`, `?? PROJECT.md`, `?? docs/REFACTOR_AUDIT.md`, `?? hromatite-mode.launch-bak.1733464`.

### 1.2 Transitive Dependency Leaks (`cargo tree`)
- `cargo tree -p Hromatite`:
  `Hromatite v0.1.0` → `hromatite-core v0.1.0` → `{hromatite-abi, hromatite-commands, hromatite-nuklear (-> [build-dependencies] cc v1.11.0)}`.
- `cargo tree -p hromatite-features`:
  `hromatite-features v0.1.0` → `{hromatite-abi, hromatite-commands, hromatite-eac}` → `hromatite-core` → `{hromatite-abi, hromatite-commands, hromatite-nuklear (-> cc)}`.
  `target/x86_64-pc-windows-gnu/debug/hromatite_features.dll` size is `24M`.
- `cargo tree -p hromatite-dll`:
  `hromatite-dll v0.1.0` → `{hromatite-core, hromatite-eac (-> hromatite-core)}`.
- `cargo tree -p hromatite-eac`:
  `hromatite-eac v0.1.0` → `hromatite-core` → `{hromatite-abi, hromatite-commands, hromatite-nuklear}`.
- `crates/hromatite-features/src/lib.rs:5` has `#![allow(unsafe_code)]`, contradicting its doc comment at lines 3–4.

### 1.3 Dead Code Claims (`grep_search` + `view_file`)
- `m3dump.rs` (`546` lines): declared as `#[allow(dead_code)] pub mod m3dump;` at `crates/hromatite-core/src/raw/mod.rs:31-32`. `grep_search` for `m3dump` across the repo finds zero callers in Rust source.
- `install_legacy_gc_context_guard` (`crates/hromatite-core/src/raw/hook.rs:1269`): marked `#[allow(dead_code)]`; `grep_search` finds 0 callers. `take_inline_repairs` (`raw/hook.rs:1097`) is called at `raw/hook.rs:998` and tested at `2455-2456`, but `note_inline_repair` (`1092`) is only called by `gc_context_route` (`1215`) → `gc_context_detour` (`1249`) → `install_legacy_gc_context_guard` (`1317`).
- `hookat` / `install_at` / `unhook` / `hooks` / `tick` branches in `hook::command`: `grep_search` for `hook::command` finds a single caller in `crates/hromatite-core/src/raw/console.rs:1131`, guarded by `match cmd.as_str() { "latec" | "latehook" | "late" => ... }` (`console.rs:1130`). Thus `"unhook"`, `"hooks"`, `"hookat"`, `"tick"` in `raw/hook.rs:2081-2156` are unreachable; `install_at` (`raw/hook.rs:1992, 2129`), `count_detour` (`1957`), `HOOKAT_*` (`1950-1952`), and `hook::installed()` (`2013`, 0 callers) have no live callers. (`before_unhook` in `gcguard.rs:115` and `refresh_attachment` in `playerloop.rs:524` are likewise only called from that unreachable `"unhook"` branch.)
- `gamebuild::is_supported` (`crates/hromatite-core/src/gamebuild.rs:21-23`): `grep_search` finds 0 callers.
- `hlog!` (`crates/hromatite-core/src/log.rs:114-117`): `grep_search` finds 0 callers.
- `memscan.rs` (`crates/hromatite-core/src/raw/memscan.rs:19`, `#![allow(dead_code)]`): `grep_search` for `memscan` finds external calls only in `crates/hromatite-core/src/raw/photon.rs:1389-1391` (`stash_cstr` and `call3`). `ScanSpan`, `readable_spans`, `spans_within`, `RegionMap`, `scan_ascii`, `scan_qwords`, `scan_words`, and `call2` have 0 non-test callers.
- Additional spot-checks:
  - `crates/hromatite-core/src/raw/memory.rs`: `find_qword_in_span` (`676`), `matches_in_span` (`280`), `read_usize_in_span` (`265`) have 0 non-test callers.
  - `crates/hromatite-core/src/raw/ffi.rs`: `CreateThread` (`237`), `RemoveVectoredExceptionHandler` (`261`), `SetUnhandledExceptionFilter` (`262`) have 0 callers.
  - `crates/hromatite-core/src/raw/discover.rs:51-52`: `#[allow(dead_code)] pub data: Section`.
  - `crates/hromatite-eac/src/eos.rs:1255`: `install_evp` called only at `eos.rs:1049`.

### 1.4 `hot.rs` & ABI Drift Claims (`view_file` + `grep_search`)
- `ABI=1` vs `ABI=2`:
  - `crates/hromatite-abi/src/lib.rs:12`: `pub const ABI_VERSION: u32 = 2;`
  - `crates/hromatite-features/src/feature.rs:226`: formats `"feature: generation={} abi={ABI_VERSION}"` (`abi=2`).
  - `crates/hromatite-core/src/raw/hot.rs:178-180`: `if values[4] != "1" { return Err(format!("unsupported manifest ABI {}", values[4])); }`
  - `tools/test-hot-smoke.sh:30`: `grep -Fq "feature: generation=$n abi=1" "$LOG"`
- `Stable`/`Requested`/`Canary` wire drift:
  - `crates/hromatite-core/src/raw/hot.rs:751-763`: emits `"stable"`, `"requested"`, `"canary"`.
  - `crates/hromatite-console/src/hot.rs:52-62`: matches only `idle|empty|ready`, `loading|reloading`, `draining`, `switching`, `active`, `rolling_back|rollback`, `retiring`, `error|failed`; all others fall into `HotState::Other`.
- Missing `"ui"` in resident lists:
  - `crates/hromatite-commands/src/lib.rs:170-177`: declares `name: "ui"` with `scope: Scope::Resident`.
  - `crates/hromatite-core/src/raw/hot.rs:1225-1247` (`resident_command`) and `crates/hromatite-features/src/feature.rs:374-375` both list the same 13 names (`"gct"`, `"threads"`, `"gctfix"`, `"gctresume"`, `"loopc"`, `"loop"`, `"latec"`, `"latehook"`, `"late"`, `"panic"`, `"av"`, `"seal"`, `"quit"`) and omit `"ui"`.
- `FeatureApiV1.frame` omission in `localmap.rs`:
  - `crates/hromatite-abi/src/lib.rs:67-94`: `FeatureApiV1` has 6 function pointers (`init`, `dispatch`, `tick`, `frame`, `configure`, `shutdown`).
  - `crates/hromatite-core/src/raw/localmap.rs:306-327` (`feature_api_bytes_valid`): checks `init`, `dispatch`, `tick`, `configure`, `shutdown` only — omitting `frame`. `grep_search` confirms `hromatite_abi::feature_header_valid` is only called in `hromatite-abi` unit tests (`crates/hromatite-abi/src/lib.rs:143`).

### 1.5 Baseline Commands, Monolith Counts, and `tools/` Spot-Check
- `cargo build --workspace`, `cargo build --workspace --release`, `cargo test --workspace --no-run` (16 `.exe` test binaries): all exited `0`.
- `cargo test --workspace --target x86_64-unknown-linux-gnu`: exited `0`; `416` passed (`Hromatite`: 0, `hromatite-abi`: 8, `hromatite-commands`: 4, `hromatite-core`: 225 + 7 + 7 = 239, `hromatite-dll`: 6, `hromatite-eac`: 118 + 0 + 1 + 0 + 3 = 122, `hromatite-features`: 24, `hromatite-nuklear`: 3 + 10 = 13 + 1 doctest, `test-target`: 0).
- `cargo clippy --workspace --all-targets`: exited `0`, 0 warnings.
- `cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu`: exited `0`, 8 warnings in `hromatite-core` (+ 8 duplicates for lib test).
- `cargo test --manifest-path crates/hromatite-console/Cargo.toml --target x86_64-unknown-linux-gnu`: `67` passed (58 unit + 6 `hot_reload` + 3 `snapshot`); `clippy` exited `0`, 0 warnings.
- `nix-shell -p xorg.libX11 xorg.libXext --run "cargo test ..."`: `0` tests; `clippy` produced `9` warnings (+ duplicates); with `-D unsafe_op_in_unsafe_fn -D clippy::undocumented_unsafe_blocks -D clippy::missing_safety_doc`, exited `101` with `15` `clippy::undocumented_unsafe_blocks` errors + `9` warnings.
- `cargo check --manifest-path var/research/helpdump/Cargo.toml --target x86_64-unknown-linux-gnu`: exited `0`.
- `wc -l` on all 8 monoliths + `m3dump.rs` matched `docs/REFACTOR_AUDIT.md` exactly (`3532`, `2717`, `2568`, `2023`, `1658`, `1644`, `1332`, `1228`, `546`).
- `tools/` count: `19` `.py`, `14` `.sh`, `8` files in `tools/data/` — matches `docs/REFACTOR_AUDIT.md`.
- **Empirical discrepancy in §6.1 (`tools/test_gc_detour_abi.py`):**
  Running `python3 -m unittest tools/test_gc_detour_abi.py` against release `hromatite.dll` at `5064586` exits `1` (`Ran 4 tests ... FAILED (failures=2)`):
  - `test_gc_guard_takes_no_lock_across_a_collection` FAILS because `suspend_entry_detour` in `crates/hromatite-core/src/raw/gcguard.rs` calls `EnterCriticalSection` and `sanitize_locked()`.
  - `test_suspend_call_site_stub_keeps_the_router_abi` FAILS (`AssertionError: gc_suspend_detour symbol missing from release DLL`).
  - `test_live_jump_restores_original_rcx_and_rdx` passes solely because `gc_context_detour` (`raw/hook.rs:1249`) is still compiled into the release DLL, even though `REFACTOR_AUDIT.md` §4.1 (line 160) & §7.3 (line 288) schedule it for deletion in Stage 5.

---

## 2. Logic Chain
1. By §1.2, §1.3, §1.4, and §1.5, every dependency leak (`cargo tree`), dead-code target (`m3dump.rs`, legacy GC guard, unreachable `hook::command` branches/helpers, `gamebuild::is_supported`, `hlog!`, unused `memscan`/`memory`/`ffi` items), `hot.rs` / `localmap.rs` drift bug, monolith line count, and baseline test/clippy result reported in `docs/REFACTOR_AUDIT.md` was reproduced verbatim on `5064586`.
2. By §1.5, the single factual discrepancy found is in `docs/REFACTOR_AUDIT.md:206` (§6.1), which labels `tools/test_gc_detour_abi.py` as `KEEP | Активный ABI-тест release DLL`. Because that script already fails 2/4 tests on baseline `5064586` and its first test asserts the presence of `gc_context_detour`, Stage 5 dead-code removal of `gc_context_detour` will also break the remaining passing test unless `test_gc_detour_abi.py` is updated or archived/deleted in Stage 5.
3. Since this discrepancy affects only one row of the Stage 5 `tools/` disposition table and does not undermine the Stage 1 baseline, dependency boundary map, dead-code map, or Stages 2–4 plan, the audit achieves Milestone M1's objective (`APPROVE`) with this §6.1 note recorded for Stage 5 (or optional pre-commit edit to row 206).

---

## 3. Caveats
- Did not execute Windows `.exe` test binaries under Wine (`cargo test --workspace` without `--target` is documented to fail with `Exec format error` in §1.2).
- Did not execute live Proton smoke scripts (`tools/test-hot-smoke.sh`, `tools/test-hromatite-hot.sh`, `tools/test-console-logs.py`, `tools/test-console-wheel.py`) or `tools/test_buildgen.py` / `tools/test_api_map.py`.
- Did not verify every individual line range inside the large monolith decomposition table (§5) beyond confirming file totals and the targeted cross-references (`eos::install_evp`, `forge::pump_photon`, `hook::command`).

---

## 4. Conclusion
**Verdict: `APPROVE`.**
`docs/REFACTOR_AUDIT.md` is empirically accurate across all verified dependency-tree leaks, dead-code items, `hot.rs` / `localmap.rs` contract drifts, monolith line counts, and baseline build/test/clippy matrices at commit `5064586`.
Actionable follow-up (non-blocking for M1, required before/during Stage 5): update `docs/REFACTOR_AUDIT.md:206` (or address in Stage 5) to reflect that `tools/test_gc_detour_abi.py` currently fails 2/4 tests on `5064586` and depends on `gc_context_detour`, so it must be refactored or archived/removed when `gc_context_detour` is deleted.

---

## 5. Verification Method
Run from `/home/wilpdrake/RustroverProjects/Hromatite` at commit `5064586`:
```bash
cargo tree -p Hromatite
cargo tree -p hromatite-features
cargo build --workspace --release
cargo test --workspace --no-run
cargo test --workspace --target x86_64-unknown-linux-gnu
cargo clippy --workspace --all-targets
cargo clippy --workspace --all-targets --target x86_64-unknown-linux-gnu
cargo test --manifest-path crates/hromatite-console/Cargo.toml --target x86_64-unknown-linux-gnu
nix-shell -p xorg.libX11 xorg.libXext --run "cargo clippy --manifest-path crates/hromatite-overlay/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu -- -D unsafe_op_in_unsafe_fn -D clippy::undocumented_unsafe_blocks -D clippy::missing_safety_doc"
python3 -m unittest tools/test_gc_detour_abi.py
```
Inspect `/home/wilpdrake/RustroverProjects/Hromatite/docs/REFACTOR_AUDIT.md` and `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/challenger_m1_1/handoff.md`.
Invalidation condition: any post-`5064586` edits to workspace manifests or source files before Stage 1 is committed.
