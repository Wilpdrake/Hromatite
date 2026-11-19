# BRIEFING — 2026-11-18T22:13:29Z

## Mission
Execute full code audit and 5-stage architectural refactoring of Hromatite (`hromatite-win`, `hromatite-hook`, `hromatite-render`, `hromatite-il2cpp`, `hromatite-eac`, injector/runtime split, unified `hot` router, dead code & `tools/` cleanup, git commits + push, `AGENTS.md` update).

## 🔒 My Identity
- Archetype: Project Orchestrator (self)
- Roles: orchestrator, user_liaison, human_reporter, successor
- Working directory: /home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/orchestrator
- Original parent: sentinel / parent
- Original parent conversation ID: c869fd6e-5d75-476b-9dda-794412fa1652
- Level: Project Orchestrator (top)
- Scope: Full repository refactoring (Stages 1–5)

## 🔒 My Workflow
- **Pattern**: Project Pattern (Survey -> Assess -> Decompose into sequential stages M1..M5 -> Delegate or Iterate -> Verify & Push)
- **Scope document**: /home/wilpdrake/RustroverProjects/Hromatite/PROJECT.md
1. **Decompose**: Survey with 3 parallel Explorers first, then map Stages 1–5 as sequential milestones (M1..M5) in PROJECT.md with interface contracts, file ownership, and verification gates.
2. **Dispatch & Execute**: Delegate or execute M1..M5 sequentially (since Cargo workspace refactoring stages depend on previous stage commits), enforcing Explorer -> Worker -> Reviewer -> Challenger -> Auditor -> Gate + git commit per stage.
3. **On failure**: Retry -> Replace -> Skip (never skip Auditor) -> Redistribute -> Redesign.
4. **Succession**: Self-succeed at 16 spawns (when all non-sub-orchestrator subagents complete).
- **Work items**:
  0. Survey (3 parallel Explorers) [done]
  1. Stage 1 — Ревизия и базовая линия [done: 5dfb77e]
  2. Stage 2 — Выделение фундамента (`hromatite-win` и `hromatite-hook`) [in-progress]
  3. Stage 3 — Разделение точек входа и вынос рендера (`hromatite-render`) [pending]
  4. Stage 4 — Декомпозиция крупных модулей и доменов (`hromatite-il2cpp`, `hromatite-eac`, host-инструменты) [pending]
  5. Stage 5 — Унификация `hot`-роутера, чистка мёртвого кода, `tools/`, `AGENTS.md`, `git push` [pending]
- **Current phase**: 2B (M2 Execution — Exploration)
- **Current focus**: Milestone M2 (Stage 2 — `hromatite-win` & `hromatite-hook`)

## 🔒 Key Constraints
- Never write, modify, or create source code files directly (only `.md` state/metadata files in `.agents/teamwork/`).
- Never run build/test commands directly — require subagents to do so.
- Never reuse a subagent after it has delivered its handoff — always spawn fresh.
- Mandatory workspace lints: `unsafe_op_in_unsafe_fn`, `undocumented_unsafe_blocks`, `missing_safety_doc`.
- Forensic Auditor (`teamwork_auditor`) has a binary veto and is non-skippable.
- Every dispatch MUST include the path to `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/ORIGINAL_REQUEST.md`.
- Each completed stage (1–5) must be committed separately to the current git branch, followed by `git push` (`git -c credential.helper='!gh auth git-credential' push`).

## Current Parent
- Conversation ID: c869fd6e-5d75-476b-9dda-794412fa1652
- Updated: 2026-11-18T22:13:29Z

## Key Decisions Made
- Survey phase completed (all 3 Explorers). Created `PROJECT.md` at project root.
- Tested `invoke_subagent(TypeName="self")`: not supported in this runtime; Project Orchestrator directly executes 2B loops for M1..M5.
- Stage 1 (M1) passed Iteration 4 Gate (`APPROVE` x4, `CLEAN`) and committed as `5dfb77e` (`docs/REFACTOR_AUDIT.md`).

## Team Roster
| Agent | Type | Work Item | Status | Conv ID |
|---|---|---|---|---|
| explorer_m2_1 | teamwork_explorer | M2 hromatite-win plan | in-progress | ef029fa5-817c-42ea-aa41-87bcf03445fd |
| explorer_m2_2 | teamwork_explorer | M2 hromatite-hook plan | in-progress | d63594ca-2452-47e4-a97c-0f6927f611ba |
| explorer_m2_3 | teamwork_explorer | M2 LateUpdate/EAC boundary | in-progress | cf83ed54-dc02-43ea-8fb9-e66fd63a6bb4 |

## Succession Status
- Succession required: no (self-succession via `self` unsupported in runtime; hard cap 128)
- Spawn count: 31 / 128
- Pending subagents: ef029fa5-817c-42ea-aa41-87bcf03445fd, d63594ca-2452-47e4-a97c-0f6927f611ba, cf83ed54-dc02-43ea-8fb9-e66fd63a6bb4
- Predecessor: none
- Successor: not yet spawned

## Active Timers
- Heartbeat cron: 85b1c7b5-2e58-4ead-803f-1d6f6d001b70/task-365
- Safety timer: 85b1c7b5-2e58-4ead-803f-1d6f6d001b70/task-366
- On succession: kill all timers before spawning successor

## Artifact Index
- `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/ORIGINAL_REQUEST.md` — verbatim user request
- `/home/wilpdrake/RustroverProjects/Hromatite/PROJECT.md` — project scope, feature inventory, milestones, contracts
- `/home/wilpdrake/RustroverProjects/Hromatite/docs/REFACTOR_AUDIT.md` — Stage 1 baseline & refactoring audit (`5dfb77e`)
- `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/orchestrator/GATE_STATUS.md` — gate verdicts
- `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/orchestrator/DISPATCH.md` — dispatch log
- `/home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/orchestrator/progress.md` — liveness & step checklist
