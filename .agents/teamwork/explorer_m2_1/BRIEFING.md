# BRIEFING — 2026-11-18T23:02:00Z

## Mission
Read-only investigation and exact module-by-module extraction design for `crates/hromatite-win` in Milestone M2 (Stage 2).

## 🔒 My Identity
- Archetype: Explorer
- Roles: Investigator, Synthesizer
- Working directory: /home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m2_1
- Original parent: 85b1c7b5-2e58-4ead-803f-1d6f6d001b70
- Milestone: M2 (Stage 2 — `hromatite-win` & `hromatite-hook`)

## 🔒 Key Constraints
- Read-only investigation — do NOT modify any source files
- Write only metadata (`DISPATCH.md`, `BRIEFING.md`, `progress.md`, `handoff.md`) in working directory
- Respect workspace lints: `unsafe_op_in_unsafe_fn = "deny"`, `undocumented_unsafe_blocks = "deny"`, `missing_safety_doc = "deny"`
- Keep Stage 2 clean without breaking Stage 3 boundaries

## Current Parent
- Conversation ID: 85b1c7b5-2e58-4ead-803f-1d6f6d001b70
- Updated: 2026-11-18T23:02:00Z

## Investigation State
- **Explored paths**:
  - `ORIGINAL_REQUEST.md`, `PROJECT.md`, `docs/REFACTOR_AUDIT.md`, root `Cargo.toml`, `crates/{hromatite-core,hromatite-eac,hromatite-features,hromatite-dll,hromatite-abi,hromatite-commands}/Cargo.toml`
  - `crates/hromatite-core/src/{lib.rs, pe.rs, sha256.rs, log.rs, manualmap.rs, gcthreads.rs}`
  - `crates/hromatite-core/src/raw/{mod.rs, ffi.rs, sys.rs, memory.rs, memscan.rs, context.rs, procs.rs, remote.rs, localmap.rs, ui.rs, discover.rs, gcguard.rs, iat.rs, vmt.rs, midhook.rs, hookset.rs, hook.rs, kiero.rs, d3d11.rs, photon.rs, playerloop.rs, hostapi.rs, hot.rs, console.rs, il2cpp.rs}`
  - `crates/hromatite-eac/src/{lib.rs, hook.rs, eos.rs, eacimage.rs, keyhunt.rs, evp.rs, digest.rs, m3probe.rs}`, `crates/hromatite-eac/tests/{eos_install.rs, keyhunt_scan.rs}`
  - `crates/hromatite-features/src/memory.rs`, `crates/hromatite-dll/src/lib.rs`, `src/main.rs`
- **Key findings**:
  - `hromatite-win` can have ZERO dependencies (pure `std`/`core` + `[lints] workspace = true`).
  - `context.rs` (`veh`) currently calls `crate::raw::gcguard::recover_boehm_abort(info)` directly; decouple via `veh::register_pre_handler(PreHandler)` called in `gcguard::install()`.
  - `log::guard` calls `veh::call_guarded`, which stays intra-crate in `hromatite-win`; dead `hlog!` macro dropped.
  - `memscan.rs` only exports `stash_cstr` and `call3` (only caller: `raw/photon.rs:1389-1391`); unused scanners (`ScanSpan`, `readable_spans`, `spans_within`, `RegionMap`, `scan_ascii`, `scan_qwords`, `scan_words`, `call2`) and unused `memory.rs` span helpers (`ScanChunks`, `from_snapshot`, `read_*_in_span`, `matches_in_span`, `find_qword_in_span`) dropped.
  - `memory.rs` retains the 7 `#[cfg(not(target_os = "windows"))] #[unsafe(no_mangle)]` link stubs (`VirtualQuery`, `GetCurrentProcess`, `ReadProcessMemory`, `WriteProcessMemory`, `VirtualAlloc`, `VirtualProtect`, `FlushInstructionCache`) for Linux host tests, plus adds `read_partial` (for `keyhunt`) and `query_region` (for `eacimage`).
  - `hromatite-features/src/memory.rs` is pure `&Host` (zero Win32 FFI); stays untouched in Stage 2 (removed in Stage 3).
  - `remote.rs` should be feature-gated `#[cfg(feature = "remote")]` in `hromatite-win` (enabled by `hromatite-core` in Stage 2, and strictly by `hromatite-injector` in Stage 3).
- **Unexplored areas**: None within Stage 2 `hromatite-win` scope.

## Key Decisions Made
- Completed full evidence-backed extraction design for `crates/hromatite-win`.

## Artifact Index
- `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m2_1/DISPATCH.md` — dispatch message log
- `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m2_1/BRIEFING.md` — persistent situational awareness
- `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m2_1/progress.md` — liveness heartbeat
- `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/explorer_m2_1/handoff.md` — 5-component extraction plan
