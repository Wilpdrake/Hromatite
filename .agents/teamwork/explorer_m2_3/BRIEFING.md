# BRIEFING — 2026-11-18T23:00:00Z

## Mission
Map exact `LateUpdate` separation from `raw/hook.rs` and `hromatite-eac` boundary cleanup (`hromatite-win`, `hromatite-hook`, remaining `hromatite-core` deps) for Stage 2.

## 🔒 My Identity
- Archetype: Explorer (`explorer_m2_3`)
- Roles: Investigation, Synthesis
- Working directory: `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m2_3`
- Original parent: `85b1c7b5-2e58-4ead-803f-1d6f6d001b70`
- Milestone: M2 (Stage 2 — `hromatite-win` & `hromatite-hook`)

## 🔒 Key Constraints
- Read-only investigation — do NOT implement or modify source files
- Write metadata (`DISPATCH.md`, `BRIEFING.md`, `progress.md`, `handoff.md`) only in working directory

## Current Parent
- Conversation ID: `85b1c7b5-2e58-4ead-803f-1d6f6d001b70`
- Updated: 2026-11-18T23:00:00Z

## Investigation State
- **Explored paths**: `ORIGINAL_REQUEST.md`, `PROJECT.md`, `docs/REFACTOR_AUDIT.md`, `Cargo.toml`, `crates/hromatite-core/Cargo.toml`, `crates/hromatite-core/src/lib.rs`, `crates/hromatite-core/src/lateupdate.rs`, `crates/hromatite-core/src/raw/{mod.rs, hook.rs, playerloop.rs, console.rs, gcguard.rs, photon.rs, kiero.rs, midhook.rs, hookset.rs, vmt.rs, iat.rs, ffi.rs, sys.rs, memory.rs, procs.rs, context.rs, hot.rs}`, `crates/hromatite-dll/{Cargo.toml, src/lib.rs}`, `crates/hromatite-eac/{Cargo.toml, src/lib.rs, src/hook.rs, src/eos.rs, src/evp.rs, src/eacimage.rs, src/keyhunt.rs, src/forge.rs, src/m3probe.rs, src/digest.rs, src/clock.rs, tests/hook_engine.rs, tests/eos_install.rs, tests/keyhunt_scan.rs}`
- **Key findings**:
  1. `raw/hook.rs` splits cleanly at line 944: lines 1–944 + tests 2172–2290 depend ONLY on `ffi`, `log`, `safe` (`hromatite-win`) and move to `hromatite-hook`; lines 946–2159 + tests 2294–2567 move to `hromatite-core/src/raw/lateupdate_hook.rs` (keeping `LateUpdate`, `tick`/`work`, `HOOKS`/`register`, `gcguard` coordination, `gcthreads` trace detours, `command`, plus legacy GC guard & dead `hookat` branches out of `hromatite-hook` until Stage 5).
  2. `Hook` needs `pub fn is_detached(&self) -> bool` (for `unhook` command at `hook.rs:2095`) and `pub fn prepare_span` (for legacy GC guard at `hook.rs:1296` until Stage 5).
  3. `hromatite-eac` switches `hook.rs` + `tests/hook_engine.rs`, `iat`, inline `mod win`, `VirtualQuery`/`ReadProcessMemory`/`GetModuleHandleA`, `safe`/`veh`/`procs`/`log`/`GAME_*` to `hromatite-win` + `hromatite-hook`, removing `crate::eos::hook_log` from `eac/src/hook.rs:1337`. Only `forge::pump_photon` (`photon`) and `m3probe::probe` (`m3`) keep `hromatite-core` temporarily until Stages 3–4.
- **Unexplored areas**: None within assigned scope.

## Key Decisions Made
- Recommend `crates/hromatite-core/src/raw/lateupdate_hook.rs` (with `pub use raw::lateupdate_hook;` and updating `hromatite-dll`, `playerloop.rs`, `console.rs`, or temporarily aliasing `pub use raw::lateupdate_hook as hook;` in `hromatite-core/src/lib.rs`).
- Keep `tools/test_gc_detour_abi.py`'s `gc_context_detour` + legacy GC guard + `hookat` dead code out of `hromatite-hook` (inside `lateupdate_hook.rs`) until Stage 5, while preserving the 3 active `gcguard` tests (`2545–2567`).

## Artifact Index
- `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m2_3/DISPATCH.md` — dispatch log
- `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m2_3/BRIEFING.md` — persistent state
- `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m2_3/progress.md` — heartbeat
- `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m2_3/handoff.md` — final Stage 2 boundary report
