# BRIEFING — 2026-11-18T23:02:00Z

## Mission
Design the exact unified `crates/hromatite-hook` crate API on top of `hromatite-win`, unifying both x64 inline hook engines (`core/raw/hook.rs` and `eac/hook.rs`) plus companion modules (`midhook`, `vmt`, `iat`, `hookset`) and documenting exact caller contracts.

## 🔒 My Identity
- Archetype: Teamwork Explorer (`explorer_m2_2`)
- Roles: Read-only Investigator / Synthesizer
- Working directory: `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m2_2`
- Original parent: `85b1c7b5-2e58-4ead-803f-1d6f6d001b70`
- Milestone: M2 (Stage 2 — `hromatite-win` & `hromatite-hook`)

## 🔒 Key Constraints
- Read-only investigation — do NOT modify any source files.
- Write metadata (`DISPATCH.md`, `BRIEFING.md`, `progress.md`, `handoff.md`) only inside `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m2_2`.
- `hromatite-hook` must depend ONLY on `hromatite-win` (`LateUpdate`, `EOSSDK`, `console`, `hot`, `il2cpp`, `hromatite-core` strictly excluded).
- Follow `x64-trampoline-relocation` skill rules and workspace lint rules (`unsafe_op_in_unsafe_fn`, `undocumented_unsafe_blocks`, `missing_safety_doc`).

## Current Parent
- Conversation ID: `85b1c7b5-2e58-4ead-803f-1d6f6d001b70`
- Updated: 2026-11-18T23:02:00Z

## Investigation State
- **Explored paths**:
  - `ORIGINAL_REQUEST.md`, `PROJECT.md`, `docs/REFACTOR_AUDIT.md`, `x64-trampoline-relocation/SKILL.md`
  - `crates/hromatite-core/src/raw/{hook.rs, midhook.rs, vmt.rs, iat.rs, hookset.rs, gcguard.rs, playerloop.rs, photon.rs, kiero.rs, memory.rs, ffi.rs, sys.rs, procs.rs, console.rs, mod.rs}`
  - `crates/hromatite-core/src/{lib.rs, log.rs, buildinfo.rs}`
  - `crates/hromatite-eac/src/{lib.rs, hook.rs, eos.rs, evp.rs}` and `crates/hromatite-eac/tests/{hook_engine.rs, eos_install.rs}`
  - `crates/hromatite-dll/src/lib.rs`, `Cargo.toml`, `crates/hromatite-core/Cargo.toml`, `crates/hromatite-eac/Cargo.toml`
- **Key findings**:
  - Engine 1 (`raw/hook.rs:1-945`) provides 5-byte `jmp rel32` + relay page (`Prep`/`prepare`/`patch`/`hook_inline`/`Hook::unhook`), required by `gcguard.rs` (6B/7B sites), `playerloop.rs`, `photon.rs`, `kiero.rs`, `hookset.rs`, `midhook.rs`.
  - Engine 2 (`eac/hook.rs`) provides 12-byte `mov rax; jmp rax` + named shared table `Local\hromatite_rust_hooktable_v1` + 2-pass `build_trampoline` (full `x64-trampoline-relocation` compliance: inverted `jcc`, `loop`/`jrcxz`, intra-prologue branch following, register-free `jmp [rip+0]`/`call [rip+2]`), required by `eos.rs`, `evp.rs`, `hook_engine.rs`.
  - Both decoders have complementary coverage (`raw/hook.rs` handles `endbr64/32` + `imm16 = opsize_66 && !rex_w`; `eac/hook.rs` handles `0F 38`/`0F 3A`/`imul`/`al/eax` + `op_off`/`cond`, though `eac/hook.rs` wrongly included `0x62` in `one_byte_has_modrm` and wildcard `_ => has_modrm = true` on `0F`).
  - `eac/hook.rs:1337` calls `crate::eos::hook_log` (must move to `hromatite_win::log::log_tag` inside `hromatite-hook`, with `eos.rs` logging `installed.describe()` to `eos::hook_log`).
  - `table.rs` requires `CreateFileMappingA`/`MapViewOfFile`/`FILE_MAP_ALL_ACCESS` in `hromatite-win::ffi`.
- **Unexplored areas**: None within assigned scope.

## Key Decisions Made
- Design `crates/hromatite-hook` with shared low-level core (`decode`, `tramp`, `mem`, `table`) and clean typed frontends (`inline` for 5-byte relay + unhook, `abs` for 12-byte + shared table, plus `midhook`, `hookset`, `vmt`, `iat`).
- Unify `tramp::build_trampoline` across `abs`, `inline`, and `midhook` so all three obey `x64-trampoline-relocation` rules and log per-detour relocations.

## Artifact Index
- `DISPATCH.md` — dispatch log
- `BRIEFING.md` — working memory
- `progress.md` — liveness heartbeat
- `handoff.md` — final 5-component report
