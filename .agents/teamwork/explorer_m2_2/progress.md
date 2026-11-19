# Progress — `explorer_m2_2`

Last visited: 2026-11-18T23:02:00Z

- [x] Read `ORIGINAL_REQUEST.md`, `PROJECT.md`, `docs/REFACTOR_AUDIT.md`, `x64-trampoline-relocation/SKILL.md`
- [x] Initialize `DISPATCH.md` and `BRIEFING.md`
- [x] Inspect `crates/hromatite-core/src/raw/hook.rs` (engine lines 1–945 + LateUpdate/GC guard lines 946–2569)
- [x] Inspect `crates/hromatite-eac/src/hook.rs` (1,792 lines) & `crates/hromatite-eac/tests/hook_engine.rs`
- [x] Inspect `crates/hromatite-core/src/raw/{midhook.rs, vmt.rs, iat.rs, hookset.rs}`
- [x] Inspect all callers (`photon.rs`, `gcguard.rs`, `playerloop.rs`, `kiero.rs`, `eos.rs`, `evp.rs`, `dll/lib.rs`, `eos_install.rs`)
- [x] Check `explorer_m2_1` state (`hromatite-win` baseline)
- [x] Synthesize unified `hromatite-hook` crate design and write `handoff.md`
