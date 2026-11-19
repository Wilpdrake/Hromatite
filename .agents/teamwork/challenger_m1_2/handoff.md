# Handoff Report — Challenger 2 (`challenger_m1_2`) for Milestone M1

**Verdict:** **APPROVE**

## 1. Observation

1. **`tools/` inventory and line counts (`ls` / `find` / `wc -l`):**
   - `ls -1 tools/*.py | wc -l` → `19` (`4,705` total lines).
   - `ls -1 tools/*.sh | wc -l` → `14` (`2,160` total lines).
   - `find tools -maxdepth 1 -type f | wc -l` → `33` scripts total.
   - `find tools/data -maxdepth 1 -type f | wc -l` → `8` files (`build-245768f25214.json`, `build-648f61b13e4f.json`, `il2cpp-api-anchors.json`, `il2cpp-api-names.txt`, `il2cpp-export-names-245768f2.json`, `il2cpp-export-names.json`, `Il2CppKnownPatterns.inl`, `message.txt`).
   - Every per-file `wc -l` output matches `docs/REFACTOR_AUDIT.md` §6.1–6.2 (`buildgen.py` 1499, `test_buildgen.py` 358, `export_transfer.py` 338, `symmap.py` 161, `api_map.py` 428, `test_api_map.py` 97, `test_gc_detour_abi.py` 136, `test-console-logs.py` 199, `test-console-wheel.py` 147, `host_scan.py` 152, `gamediff.py` 84, `gct_table.py` 95, `resolve_api.py` 129, `meta_parse.py` 306, `dump.py` 74, `il2cpp_methods.py` 189, `test_dumps.py` 49, `test_il2cpp_methods.py` 142, `test_meta_parse.py` 122; `hromatite-launch.sh` 476, `hromatite-tui.sh` 20, `hromatite-autoconsole.sh` 118, `hromatite-overlay.sh` 51, `hromatite-reload.sh` 181, `hromatite-watch.sh` 102, `test-hromatite-hot.sh` 340, `test-hot-smoke.sh` 49, `eac_probe.sh` 312, `refresh.sh` 38, `test-loadlib.sh` 48, `hromatite-console.sh` 188, `test-seal.sh` 91, `il2cppdump.sh` 146).
   - `cmp -s hromatite-mode hromatite-mode.launch-bak.1733464` → exit `0` (`IDENTICAL`), matching §6.3.

2. **Active-script invocation check for DELETE/ARCHIVE scripts:**
   - Checked `tools/hromatite-launch.sh`, `tools/hromatite-reload.sh`, `tools/refresh.sh`, `tools/eac_probe.sh`, `tools/hromatite-tui.sh`, `tools/hromatite-overlay.sh`, `tools/hromatite-watch.sh`, `tools/hromatite-autoconsole.sh`, `tools/test-hromatite-hot.sh`, `tools/test-hot-smoke.sh`, and all KEEP Python scripts (`buildgen.py`, `test_buildgen.py`, `export_transfer.py`, `symmap.py`, `api_map.py`, `test_api_map.py`, `test_gc_detour_abi.py`, `test-console-logs.py`, `test-console-wheel.py`) via `grep -rnE`.
   - Result: `NO_ACTIVE_INVOCATIONS` — none of `gamediff.py`, `gct_table.py`, `resolve_api.py`, `meta_parse.py`, `dump.py`, `il2cpp_methods.py`, `test_dumps.py`, `test_il2cpp_methods.py`, `test_meta_parse.py`, `test-loadlib.sh`, `hromatite-console.sh`, `test-seal.sh`, `il2cppdump.sh`, or `host_scan.py` is called by any active script. (Only `tools/gamediff.py:2` mentions `tools/dump.py`, and `tools/il2cppdump.sh:143` / `tools/il2cpp_methods.py:4` cross-reference each other — all marked DELETE/ARCHIVE.)

3. **`tools/test-hot-smoke.sh:30` `abi=1` defect:**
   - `tools/test-hot-smoke.sh:30`: `grep -Fq "feature: generation=$n abi=1" "$LOG"`
   - `crates/hromatite-abi/src/lib.rs:12`: `pub const ABI_VERSION: u32 = 2;`
   - `crates/hromatite-features/src/feature.rs:226`: `"feature: generation={} abi={ABI_VERSION}"` (emits `abi=2`).
   - Additionally observed: `tools/hromatite-reload.sh:125,174,180` and `tools/test-hromatite-hot.sh:73,88` emit/expect `ABI=1` in manifest text because `crates/hromatite-core/src/raw/hot.rs:178` currently requires `values[4] == "1"`.

4. **`hromatite-overlay` under `nix-shell -p xorg.libX11 xorg.libXext`:**
   - `crates/hromatite-overlay/Cargo.toml`: contains `[workspace]`, no `[lints]` section.
   - `wc -l crates/hromatite-overlay/src/main.rs` → `2023`.
   - `cargo test --manifest-path crates/hromatite-overlay/Cargo.toml --target x86_64-unknown-linux-gnu` → exit `0`, `0 passed; 0 failed`.
   - `cargo clippy --manifest-path crates/hromatite-overlay/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu` → exit `0`, `9 warnings` (+ `9 duplicates` on bin test), `0 errors`.
   - `cargo clippy --manifest-path crates/hromatite-overlay/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu -- -D unsafe_op_in_unsafe_fn -D clippy::undocumented_unsafe_blocks -D clippy::missing_safety_doc` → exit `101`, `15 errors` (`clippy::undocumented_unsafe_blocks` at `src/main.rs` lines `1420, 1422, 1423, 1431, 1432, 1433, 1435, 1448, 1583, 1584, 1586, 1636, 1639, 1640, 1642`) + `9 warnings`.

5. **`hromatite-console`:**
   - `cargo test --manifest-path crates/hromatite-console/Cargo.toml --target x86_64-unknown-linux-gnu` → exit `0`, `67 passed; 0 failed` (`58` unit + `6` `tests/hot_reload.rs` + `3` `tests/snapshot.rs`).
   - `cargo clippy --manifest-path crates/hromatite-console/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu` → exit `0`, `0 warnings, 0 errors`.
   - `wc -l crates/hromatite-console/src/model.rs crates/hromatite-console/src/hot.rs` → `1332` and `98`.
   - `crates/hromatite-console/src/hot.rs:52-62` does not match `"stable"`, `"requested"`, or `"canary"` emitted by `crates/hromatite-core/src/raw/hot.rs:751-763`, mapping them to `HotState::Other`.

6. **`hromatite-eac`:**
   - `cargo test -p hromatite-eac --target x86_64-unknown-linux-gnu` → exit `0`, `122 passed; 0 failed` (`118` unit + `0` `eos_install` + `1` `epochs` + `0` `hook_engine` + `3` `keyhunt_scan`).
   - Monolith `wc -l` counts: `eos.rs` `3532`, `hook.rs` `1792`, `eacimage.rs` `1658`, `report.rs` `1644`, `forge.rs` `1228`, `keyhunt.rs` `1193`, `evp.rs` `728`, `digest.rs` `297`.
   - Verified `crates/hromatite-eac/src/forge.rs:824,834` calls `hromatite_core::photon::{template, emulate_send}`; `keyhunt.rs:301` has `#[allow(clashing_extern_declarations)]`; `eos.rs:1518-1549` and `hook.rs:865-917` declare local `mod win`; `eacimage.rs:853-902,1004-1024` calls `hromatite_core::ffi::VirtualQuery`; `digest.rs` duplicates `sha256` and `crc32c_eac`.
   - Verified `eos::install_evp` (`eos.rs:1093-1303`) duplicates `evp.rs` (5 targets vs 6 targets), and noted it is still invoked at `eos.rs:1049` when `EAC_KEYHUNT_EVP=1` (while `EAC_EVP=1` invokes `crate::evp::install` at `eos.rs:1077`).

## 2. Logic Chain

1. Every count, line number, test total, clippy warning/error count, and exact error line number in `docs/REFACTOR_AUDIT.md` for `tools/`, `hromatite-overlay`, `hromatite-console`, and `hromatite-eac` was reproduced directly by command execution and file inspection (Observations 1, 4, 5, 6).
2. All 14 scripts marked `DELETE` or `ARCHIVE` in `docs/REFACTOR_AUDIT.md` §6.1–6.2 have zero callers in any active script (`hromatite-launch.sh`, `hromatite-reload.sh`, `refresh.sh`, `eac_probe.sh`, `hromatite-tui.sh`, `hromatite-overlay.sh`, `hromatite-watch.sh`, `hromatite-autoconsole.sh`, `test-hromatite-hot.sh`, `test-hot-smoke.sh`, or KEEP Python scripts), confirming the cleanup plan will not break active workflows (Observation 2).
3. `tools/test-hot-smoke.sh:30` is empirically confirmed broken against `ABI_VERSION = 2` (`hromatite-features/src/feature.rs:226` emits `abi=2`, while the script greps `abi=1`) (Observation 3).
4. The two minor nuances discovered (`hromatite-reload.sh` / `test-hromatite-hot.sh` `ABI=1` manifest lines tied to `raw/hot.rs:178`, and `eos.rs:1049` still wiring `EAC_KEYHUNT_EVP` to `install_evp`) are execution details for Stages 4–5 rather than factual errors in the baseline audit, so `docs/REFACTOR_AUDIT.md` accurately captures the baseline at `5064586`.

## 3. Caveats

- This challenge scoped specifically to `tools/` inventory, `tools/test-hot-smoke.sh:30`, `hromatite-overlay`, `hromatite-console`, and `hromatite-eac`; workspace-wide Windows builds and `hromatite-core` / `hromatite-features` line-by-line dead-code items were left to Challenger 1.
- Stage 4/5 implementers should note two non-blocking details when acting on the audit:
  1. When updating `raw/hot.rs:178` from `ABI=1` to `ABI=2` in Stage 5, `tools/hromatite-reload.sh:125,174,180` and `tools/test-hromatite-hot.sh:73,88` must also be updated from `ABI=1` to `ABI=2`.
  2. `eos::install_evp` (`eos.rs:1093-1303`) is not zero-caller dead code today — it is still called at `eos.rs:1049` under `EAC_KEYHUNT_EVP=1`, so Stage 4/5 should redirect `EAC_KEYHUNT_EVP` to `evp::install` (or explicitly retire that flag) when deleting `install_evp`.

## 4. Conclusion

**APPROVE.** `docs/REFACTOR_AUDIT.md` is empirically accurate across `tools/` counts and line counts, script dependency isolation, the `tools/test-hot-smoke.sh:30` `abi=1` defect, `hromatite-overlay` clippy/workspace-lint failures (15 errors, 9 warnings), `hromatite-console` baseline (67 passed, 0 clippy issues, `hot.rs` state drift), and `hromatite-eac` baseline (122 passed, monolith line counts, duplicate FFI/EVP/digest/photon coupling).

## 5. Verification Method

Run from `/home/wilpdrake/RustroverProjects/Hromatite`:

```sh
ls -1 tools/*.py | wc -l && ls -1 tools/*.sh | wc -l && find tools/data -maxdepth 1 -type f | wc -l
wc -l tools/*.py tools/*.sh
grep -rnE '(gamediff|gct_table|resolve_api|meta_parse|dump\.py|il2cpp_methods|test_dumps|test_il2cpp_methods|test_meta_parse|test-loadlib|hromatite-console\.sh|test-seal|il2cppdump|host_scan)' tools/*.py tools/*.sh
sed -n '30p' tools/test-hot-smoke.sh
nix-shell -p xorg.libX11 xorg.libXext --run "cargo test --manifest-path crates/hromatite-overlay/Cargo.toml --target x86_64-unknown-linux-gnu && cargo clippy --manifest-path crates/hromatite-overlay/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu && cargo clippy --manifest-path crates/hromatite-overlay/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu -- -D unsafe_op_in_unsafe_fn -D clippy::undocumented_unsafe_blocks -D clippy::missing_safety_doc"
cargo test --manifest-path crates/hromatite-console/Cargo.toml --target x86_64-unknown-linux-gnu
cargo clippy --manifest-path crates/hromatite-console/Cargo.toml --all-targets --target x86_64-unknown-linux-gnu
cargo test -p hromatite-eac --target x86_64-unknown-linux-gnu
```

Invalidation condition: any commit modifying `tools/`, `crates/hromatite-overlay/`, `crates/hromatite-console/`, or `crates/hromatite-eac/` after `5064586` invalidates these baseline numbers.
