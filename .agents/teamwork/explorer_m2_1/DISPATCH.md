# Dispatch Log

## 2026-11-18T22:56:38Z
From: `85b1c7b5-2e58-4ead-803f-1d6f6d001b70` (parent)
Message:
You are Explorer 1 (`explorer_m2_1`) for **Milestone M2 (Stage 2 — `hromatite-win` & `hromatite-hook`)**.

MANDATORY FIRST STEP:
Read `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/ORIGINAL_REQUEST.md`, `/home/wilpdrake/RustroverProjects/Hromatite/PROJECT.md`, and `/home/wilpdrake/RustroverProjects/Hromatite/docs/REFACTOR_AUDIT.md` (§3, §4, §7) before starting.

Working directory (metadata only — write `progress.md` and `handoff.md` here):
`/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m2_1`

Your read-only mission (DO NOT modify any files) — design the exact `hromatite-win` crate extraction:
1. Inspect all files slated for `hromatite-win`:
   - `crates/hromatite-core/src/raw/{ffi.rs, sys.rs, memory.rs, memscan.rs, context.rs, procs.rs, remote.rs}`
   - `crates/hromatite-core/src/{pe.rs, sha256.rs, log.rs, lib.rs}`
   - Inline Win32 FFI/memory/PE copies in `crates/hromatite-core/src/raw/{localmap.rs, ui.rs, discover.rs}` and `crates/hromatite-eac/src/{hook.rs, eos.rs, eacimage.rs, keyhunt.rs}`.
   - `crates/hromatite-features/src/memory.rs` (confirm its relation to Win32 vs `Host`).
2. Produce an exact module-by-module plan for `crates/hromatite-win`:
   - `Cargo.toml` (inheritance of `[lints] workspace = true`, edition 2024, zero internal deps — or check if `hromatite-win` needs zero deps!).
   - Module layout (`ffi`, `sys`, `memory`, `memscan`, `veh`/`context`, `pe`, `procs`, `remote`, `sha256`, `log`, `GAME_*` constants).
   - Check `log.rs` (`log::guard` calls `veh::call_guarded`) and `memscan.rs` (`stash_cstr` + `call3` vs unused scanners) and `memory.rs` (`#[cfg(not(target_os = "windows"))] #[unsafe(no_mangle)]` link stubs for host unit tests!).
   - Check how `hromatite-core` can cleanly re-export `hromatite_win::*` in Stage 2 (keeping `Hromatite.exe`, `hromatite-dll`, and `hromatite-core` modules building without breaking Stage 3 boundaries) while `hromatite-eac` switches directly to `hromatite-win` for all FFI/VirtualQuery/RPM/PE helpers!
   - Check every `unsafe` rule (`unsafe_op_in_unsafe_fn`, `undocumented_unsafe_blocks`, `missing_safety_doc`) and `#[deny(unsafe_code)]` / `#[allow(unsafe_code)]` boundaries.

Write `progress.md` and `handoff.md` in your working directory and send a concise summary via `send_message`.
