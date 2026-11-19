## 2026-11-18T22:56:39Z

From: `85b1c7b5-2e58-4ead-803f-1d6f6d001b70`

You are Explorer 2 (`explorer_m2_2`) for **Milestone M2 (Stage 2 — `hromatite-win` & `hromatite-hook`)**.

MANDATORY FIRST STEP:
Read `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/ORIGINAL_REQUEST.md`, `/home/wilpdrake/RustroverProjects/Hromatite/PROJECT.md`, `/home/wilpdrake/RustroverProjects/Hromatite/docs/REFACTOR_AUDIT.md`, and `/home/wilpdrake/.gemini/config/skills/x64-trampoline-relocation/SKILL.md` before starting.

Working directory (metadata only — write `progress.md` and `handoff.md` here):
`/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m2_2`

Your read-only mission (DO NOT modify any files) — design the exact `hromatite-hook` crate unification:
1. Inspect both x64 inline hook engines and all companion hooking modules + callers:
   - `crates/hromatite-core/src/raw/hook.rs` (lines 1–945: 5-byte `jmp rel32` + relay page, `alloc_near` ±2 GiB, `cmpxchg16b`, `decode_one`/`decode_span`, `Hook`, `prepare`/`patch`/`register`)
   - `crates/hromatite-eac/src/hook.rs` (1,792 lines: 12-byte `mov rax; jmp rax`, `alloc_near` ±512 MiB, `cas16`/`patch_entry`, inverted-`jcc` short-branch & `rel32` expansion, named shared hooktable `Local\hromatite_rust_hooktable_v1`)
   - `crates/hromatite-eac/tests/hook_engine.rs`
   - `crates/hromatite-core/src/raw/{midhook.rs, vmt.rs, iat.rs, hookset.rs}`
   - All callers of inline/midhook/vmt/iat across `hromatite-core` (`photon.rs`, `gcguard.rs`, `playerloop.rs`, `kiero.rs`, `raw/hook.rs` LateUpdate) and `hromatite-eac` (`eos.rs`, `evp.rs`).
2. Design the unified `crates/hromatite-hook` API поверх `hromatite-win`:
   - Zero dependencies on `LateUpdate`, `EOSSDK`, `console`, `hot`, `il2cpp`, or `hromatite-core`.
   - How the unified inline hook engine supports BOTH the 5-byte relay-jump API (`prepare`/`patch`/`register`/relay + `midhook` + `hookset`) AND the 12-byte `mov rax; jmp rax` / shared hook table / `jcc` relocation used by `hromatite-eac` (or unifies trampoline relocation cleanly per `x64-trampoline-relocation` skill so both EAC and IL2CPP hooks share the unified decoder/relocator/patcher/hooktable!).
   - Verify exact public API signatures needed by `hromatite-core` (re-exporting `midhook`, `vmt`, `iat`, `hookset`, inline hook primitives) and `hromatite-eac` (`eos.rs`, `evp.rs`, `hook_engine.rs` moving to `hromatite-hook/tests/`).

Write `progress.md` and `handoff.md` in your working directory and send a concise summary via `send_message`.
