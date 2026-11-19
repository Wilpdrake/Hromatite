# BRIEFING — 2026-11-18T22:35:00Z

## Mission
Verify the exact migration strategy for the 3 contradictions flagged by `reviewer_m1_2` so `docs/REFACTOR_AUDIT.md` §7.2–7.3 and `PROJECT.md` are 100% contradiction-free.

## 🔒 My Identity
- Archetype: Explorer
- Roles: Read-only investigator, synthesizer
- Working directory: /home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m1_iter2_1
- Original parent: 85b1c7b5-2e58-4ead-803f-1d6f6d001b70
- Milestone: M1 Iteration 2

## 🔒 Key Constraints
- Read-only investigation — do NOT modify project source code
- Write reports/metadata only to `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m1_iter2_1/`
- Send concise summary via `send_message` to `85b1c7b5-2e58-4ead-803f-1d6f6d001b70`

## Current Parent
- Conversation ID: 85b1c7b5-2e58-4ead-803f-1d6f6d001b70
- Updated: 2026-11-18T22:35:00Z

## Investigation State
- **Explored paths**:
  - `.agents/teamwork/ORIGINAL_REQUEST.md`, `PROJECT.md`, `docs/REFACTOR_AUDIT.md`, `reviewer_m1_2/handoff.md`, `challenger_m1_1/handoff.md`
  - `crates/hromatite-core/src/{crc.rs, manualmap.rs, crypto.rs, buildinfo.rs}`, `crates/hromatite-core/src/raw/{photon.rs, discover.rs, gcguard.rs, hook.rs, hostapi.rs}`, `crates/hromatite-core/tests/{frames.rs, report_stream.rs}`
  - `crates/hromatite-eac/{Cargo.toml, src/{lib.rs, digest.rs, clock.rs, evp.rs, forge.rs, keyhunt.rs, eos.rs, eacimage.rs, m3probe.rs}, tests/{eos_install.rs, keyhunt_scan.rs, hook_engine.rs, epochs.rs}}`
  - `crates/hromatite-features/{Cargo.toml, src/{lib.rs, feature.rs, host.rs, eac.rs}}`
  - `crates/hromatite-abi/{Cargo.toml, src/lib.rs}`, `crates/hromatite-commands/src/lib.rs`, `crates/hromatite-dll/src/lib.rs`, `tools/test_gc_detour_abi.py`, `src/main.rs`
- **Key findings**:
  1. Moving `crc32c_eac` + `seal_checksum` to `hromatite-abi::crc` (`#![forbid(unsafe_code)]`, 0 deps) and having `hromatite-eac` depend on `hromatite-abi` (re-exporting in `digest.rs`) satisfies `hromatite-il2cpp::photon`, `frames.rs`, and `hromatite-eac` with zero duplication and zero forbidden edges.
  2. For `hromatite-features` to compile with `hromatite-eac = { default-features = false }`: (a) split pure `HookEvent`/`Counters`/`Report`/`Hooks` state out of `eos.rs` (always compiled; `install`/`iat::status`/`evp` gated behind `feature = "runtime"`), (b) gate `keyhunt::Hunter` (OS `VirtualQuery`/`ReadProcessMemory`/`process_vm_readv` scanning) behind `"runtime"` and tick it in `hromatite-runtime` (removing `hunter` from `features/src/eac.rs`, since `HostApiV1` has no region enumerator and `features` cannot link `hromatite-win`), (c) move `"eac.module"` from `Scope::Feature` to `Scope::Resident` in `hromatite-commands` and dispatch it in `hromatite-runtime` (via `eacimage::command`), since `NativeMemRead` needs `veh::call_with` (Wine `VirtualQuery` misses memfd ELF) and `live` needs `VirtualQuery`.
  3. `eos.rs:1065` `m3probe::probe()` can be cleanly dropped from `hromatite-eac::eos` because `raw/discover.rs` (`discover::il2cpp_anchors` -> `class_table_global`) + `buildinfo.rs` completely supersede `m3probe`'s hardcoded `LEGACY_TABLE_RVA` heuristic.
  4. `tools/test_gc_detour_abi.py` fails 2/4 tests on `5064586`, reads deleted path `crates/hromatite-core/src/raw/gcguard.rs` in test 3, and asserts deleted `gc_context_detour` in test 1; `crypto.rs` is called solely by `manualmap.rs:40, 61` (`seal_image`) so it must move temporarily to `hromatite-injector` in Stage 3 if `--seal` removal stays in Stage 5.
- **Unexplored areas**: None within scope.

## Key Decisions Made
- Completed read-only verification of all 4 items and prepared exact migration recommendations for `docs/REFACTOR_AUDIT.md` and `PROJECT.md`.

## Artifact Index
- `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m1_iter2_1/DISPATCH.md` — dispatch log
- `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m1_iter2_1/BRIEFING.md` — working memory
- `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m1_iter2_1/progress.md` — task checklist
- `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m1_iter2_1/handoff.md` — 5-component handoff report
