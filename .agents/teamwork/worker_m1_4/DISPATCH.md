## 2026-11-18T22:50:15Z

You are Worker M1 Iteration 4 (`worker_m1_4`) for **Milestone M1 (Stage 1 — Ревизия и базовая линия)**.

MANDATORY FIRST STEP:
Read:
1. `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/ORIGINAL_REQUEST.md`
2. `/home/wilpdrake/RustroverProjects/Hromatite/PROJECT.md`
3. `/home/wilpdrake/RustroverProjects/Hromatite/docs/REFACTOR_AUDIT.md`
4. `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/challenger_m1_iter3_1/handoff.md`
5. `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/reviewer_m1_iter3_1/handoff.md`
6. `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/reviewer_m1_iter3_2/handoff.md`

Working directory (metadata only — write `progress.md` and `handoff.md` here):
`/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/worker_m1_4`

Exclusive write ownership:
- `/home/wilpdrake/RustroverProjects/Hromatite/docs/REFACTOR_AUDIT.md` (DO NOT modify any Rust source files, Cargo.toml, tools/, or AGENTS.md in Stage 1).

Task:
Update `/home/wilpdrake/RustroverProjects/Hromatite/docs/REFACTOR_AUDIT.md` (§7.2, §7.3) to cleanly resolve the remaining Stage 3 `--no-default-features` compilation details from `challenger_m1_iter3_1`, `reviewer_m1_iter3_1`, `reviewer_m1_iter3_2`, and `challenger_m1_iter3_2`:
1. **`forge::pump_photon` (`report.rs:644`, `forge.rs:1210`)**:
   - Specify that `Delivery` and `forge::pump_photon()` remain callable in always-compiled pure `forge` with a `#[cfg(not(feature = "runtime"))]` unarmed stub returning `Delivery { unarmed: true, ..Delivery::default() }` (or a registered callback slot defaulting to unarmed) while only the `hromatite_core::photon` body is `#[cfg(feature = "runtime")]` in Stage 3 (inverted via callback/trait in Stage 4), keeping `report.rs:644`, `report.rs:1553`, and `forge.rs:1210` green without `hromatite-core`!
2. **`eacimage.rs` (`locate_native` & Unix helpers)**:
   - Explicitly gate `locate_native` (`1294-1357`, constructs `NativeMemRead::new` at `1312`) plus `locate_unix`/`UnixMemRead`/`find_process_pid`/`maps_of` (`1040-1194`) under `#[cfg(feature = "runtime")]` alongside `NativeMemRead`, `live`, `command`, and tests `1568, 1633`.
3. **`keyhunt.rs` OS memory-read/region-walk functions**:
   - Explicitly gate `read_maps` (`167-180`), `read_block` (`190-256`), `read_window` (`259-272`), `enumerate_regions` (`276-369`), and `find_in_region` (`1102-1114`) under `#[cfg(feature = "runtime")]` alongside `Hunter` and inline test `1160` (preserving `#[cfg(not(windows))]` `process_vm_readv` inside `"runtime"` for `tests/keyhunt_scan.rs`), so Windows `default-features = false` does not touch `hromatite-win`.
4. **`hromatite-eac/src/lib.rs` & `eos.rs` gating + `features/src/eac.rs:614-619` + `raw/hot.rs` temporary `"eac.module"` note**:
   - Explicitly gate `pub mod evp;`, `pub mod m3probe;`, `pub mod hook;` (or `hromatite_hook`), and `pub use hook::Hook;` (`lib.rs:51-57, 70`), `use crate::hook` (`eos.rs:75`), `install_iat` (`1367`), `module_report` (`1387`), and `eos::tests` (`2958-3532`) under `#[cfg(feature = "runtime")]`, and change `Hooks::install` (`eos.rs:744, 1087`) from `#[cfg(windows)]`/`#[cfg(not(windows))]` to `#[cfg(all(windows, feature = "runtime"))]`/`#[cfg(not(all(windows, feature = "runtime")))]` (with `#[cfg_attr(not(feature = "runtime"), allow(dead_code))]` on `eos::state` internal helpers `hook_log`/`epoch_*`).
   - Note removing `self.hunter` status lines in `crates/hromatite-features/src/eac.rs:614-619` alongside `self.hunter` tick, and temporarily adding `"eac.module"` to `raw/hot.rs::resident_command` (`1225-1247`) in Stage 3 alongside `raw/console.rs` dispatch before Stage 5 unifies routing via `hromatite-commands`.

Verify `git status --short` and `git diff` remain clean of any source/config changes, update `progress.md`, write `handoff.md` in your working directory, and report back via `send_message`.
