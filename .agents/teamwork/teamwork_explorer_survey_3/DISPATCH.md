# DISPATCH

## 2026-11-18T22:14:26Z
Sender: `85b1c7b5-2e58-4ead-803f-1d6f6d001b70`
Mission: Survey Explorer 3 (Host Tools, ABI/Commands, `tools/`, Root Artifacts & `AGENTS.md` Audit).
- Read `.agents/teamwork/ORIGINAL_REQUEST.md` in full.
- Inventory all files (with line counts) in `crates/hromatite-abi/`, `crates/hromatite-commands/`, `crates/hromatite-console/`, `crates/hromatite-overlay/`, `crates/test-target/`, and any other crates under `crates/`.
- Analyze decomposition needs for `hromatite-overlay/src/main.rs`, `hromatite-console/src/model.rs`, `hromatite-console/src/hot.rs` (relation to `core/src/hot.rs` and `core/src/raw/hot.rs`), `hromatite-abi`, `hromatite-commands`.
- Audit `tools/` directory completely (KEEP / ARCHIVE / DELETE).
- Check root artifacts (`hromatite-mode*`, `api_map.json`, `symbols-*.json`, `docs/`, `var/`) and inspect `AGENTS.md` (§3 and end of §17).
- Write `progress.md` and `handoff.md` in `.agents/teamwork/teamwork_explorer_survey_3/`, then `send_message` summary.
