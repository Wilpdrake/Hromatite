## 2026-11-18T22:36:11Z
Sender: 85b1c7b5-2e58-4ead-803f-1d6f6d001b70 (parent)
Role: Worker M1 Iteration 2 (`worker_m1_2`) for Milestone M1 (Stage 1 — Ревизия и базовая линия).
Exclusive write ownership: `/home/wilpdrake/RustroverProjects/Hromatite/docs/REFACTOR_AUDIT.md` (no Rust source/Cargo.toml/tools/AGENTS.md changes).
Task: Update `docs/REFACTOR_AUDIT.md` to resolve all 5 review/challenge findings cleanly:
1. `crc.rs` <-> `photon` / `frames.rs` / `eac` in §7.2 & §7.3.
2. `hromatite-features` + `hromatite-eac = { default-features = false }` 3 Stage 3 decouplings in §7.2 & §7.3.
3. `eac/m3probe.rs` & `eos.rs:1065` in §7.2 & §7.3.
4. Stage 4 vs Stage 5 sequencing & omitted files in §7.2 & §7.3 (`memscan::call3`, `core/src/lib.rs` constants, `crypto.rs` temp move, `m3dump.rs`/dead code dropped in Stage 2–4, `game_fixture.rs`/`test_support.rs`/`test-target`/`eac/tests/*`).
5. `test_gc_detour_abi.py` reclassification in §6.1, `hromatite-reload.sh` / `test-hromatite-hot.sh` ABI=1 in §6.2 & §7.3, `eos::install_evp` / `EAC_KEYHUNT_EVP=1` in §4.1 & §7.3.
Verify `git status --short` and `git diff` clean of any source/config changes, update `progress.md`, write `handoff.md`, report via `send_message`.
