# Dispatch Log

## 2026-11-18T22:56:40Z
From: parent (85b1c7b5-2e58-4ead-803f-1d6f6d001b70)

You are Explorer 3 (`explorer_m2_3`) for **Milestone M2 (Stage 2 — `hromatite-win` & `hromatite-hook`)**.

MANDATORY FIRST STEP:
Read `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/ORIGINAL_REQUEST.md`, `/home/wilpdrake/RustroverProjects/Hromatite/PROJECT.md`, and `/home/wilpdrake/RustroverProjects/Hromatite/docs/REFACTOR_AUDIT.md` before starting.

Working directory (metadata only — write `progress.md` and `handoff.md` here):
`/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m2_3`

Your read-only mission (DO NOT modify any files) — map the exact `LateUpdate` separation and `hromatite-eac` boundary cleanup in Stage 2:
1. Inspect `crates/hromatite-core/src/raw/hook.rs` lines 945–2568 (`LateUpdate` tick/detour/install/status, `gcguard` coordination, `gcthreads` worker trace detours, `console::dispatch`, `hot::{tick,frame,command,dispatch}`, `playerloop::refresh_attachment`, `install_legacy_gc_context_guard`, `hookat`/`unhook`/`hooks`/`tick`) and `crates/hromatite-core/src/raw/playerloop.rs` (`hook::tick()` call) and `crates/hromatite-dll/src/lib.rs` (all `hook::*` calls).
   - Map exactly how `LateUpdate` + `tick`/`work` separates out of the hook engine into a dedicated module (`raw/lateupdate_hook.rs` or `raw/tick.rs` inside `hromatite-core` for Stage 2, ready to move to `hromatite-il2cpp`/`hromatite-runtime` in Stages 3–4) so `hromatite-hook` has ZERO `LateUpdate` or runtime coupling.
   - Check whether the legacy GC guard / unreachable `hookat` dead code should stay out of `hromatite-hook` (never moved into `hromatite-hook`!).
2. Inspect all `hromatite_core` & Win32 FFI usages inside `crates/hromatite-eac/src/` (`Cargo.toml`, `lib.rs`, `eos.rs`, `evp.rs`, `eacimage.rs`, `keyhunt.rs`, `forge.rs`, `m3probe.rs`):
   - Identify every call in `hromatite-eac` that switches from `hromatite_core` / inline `mod win` / inline `VirtualQuery`/`ReadProcessMemory` to `hromatite_win` and `hromatite_hook` in Stage 2.
   - Confirm which remaining `hromatite_core` calls (`forge.rs::pump_photon` -> `photon`, `m3probe.rs` -> `m3/procs/safe`) stay temporarily until Stage 3/4 so Stage 2 compiles cleanly with zero broken tests.

Write `progress.md` and `handoff.md` in your working directory and send a concise summary via `send_message`.
