## 2026-11-18T22:14:24Z

Sender: 85b1c7b5-2e58-4ead-803f-1d6f6d001b70

You are Survey Explorer 2 (Core, EAC, Features, Nuklear, DLL & Duplicate Subsystems Audit).

MANDATORY FIRST STEP:
Read `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/ORIGINAL_REQUEST.md` in full before starting work.

Working directory (metadata only — write `progress.md` and `handoff.md` here):
`/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/teamwork_explorer_survey_2`

Your read-only mission (DO NOT modify any source files):
1. Inventory all files (with line counts) in:
   - `src/`
   - `crates/hromatite-core/` (especially `raw/` vs top-level)
   - `crates/hromatite-dll/`
   - `crates/hromatite-eac/`
   - `crates/hromatite-features/`
   - `crates/hromatite-nuklear/`
2. Map all duplicate subsystems and coupling violations mentioned in R2–R5:
   - Win32 FFI (`ffi`, `sys`, inline `extern "system"` declarations in `core/raw`, `hromatite-eac`, `hromatite-features/src/memory.rs`, etc.).
   - Memory read/write/scan (`VirtualQuery` checks, `memscan`, `memory.rs`, `remote`, etc.) and VEH-guard (`context`).
   - PE parser, processes/modules/PEB (`procs`, `pe`).
   - Two x64 inline-hook engines (`crates/hromatite-core/src/raw/hook.rs` and `crates/hromatite-eac/src/hook.rs`) plus `midhook`, `vmt`, `iat`, `hookset`, and how `LateUpdate` or `EOSSDK` are currently coupled to hooks.
   - Render/UI (`hromatite-nuklear`, `kiero`, `d3d11`, `d3d11_shaders`, `ui`).
   - Unity/IL2CPP modules (`symbols`, `buildinfo`, `m3`, `gcthreads`, `playerloop`, `lateupdate`, `photon`) — specifically inspect `photon.rs` structure and couplings.
   - `hromatite-eac` modules (`eos.rs`, `report.rs`, `eacimage.rs`, `forge.rs`, etc.) — identify non-EAC responsibilities that belong in `win`/`hook` and internal structure of large files.
   - `hot` implementations (`core/src/hot.rs`, `core/src/raw/hot.rs`, plus check how `console/src/hot.rs` interacts).
3. Audit dead code: find all `#[allow(dead_code)]`, unused modules/files (including `m3dump.rs` and old diagnostic branches), unused functions/structs/constants across these crates. Verify references with grep before flagging anything as dead code!

Output requirements:
- Update `progress.md` in your working directory as you work (include `Last visited: <UTC timestamp>`).
- Write a detailed `handoff.md` in `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/teamwork_explorer_survey_2/handoff.md` mapping every file to its target crate (`hromatite-win`, `hromatite-hook`, `hromatite-render`, `hromatite-il2cpp`, `hromatite-eac`, `hromatite-injector`, `hromatite-runtime`, `hromatite-dll`, `hromatite-features`, or DELETE), plus exact dead-code findings and coupling notes.
- Send a concise summary message back with the path to your `handoff.md`.
