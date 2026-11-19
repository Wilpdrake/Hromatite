# BRIEFING — 2026-11-18T22:13:25Z

## Mission
Oversee Hromatite Stages 1–5 refactoring via General path (`teamwork_orchestrator`), monitor progress via crons, and enforce independent victory verification before completion.

## 🔒 My Identity
- Archetype: sentinel
- Working directory: /home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/sentinel
- Orchestrator: 85b1c7b5-2e58-4ead-803f-1d6f6d001b70
- Victory Auditor: to be spawned on victory claim

## 🔒 Key Constraints
- No technical decisions — relay only
- Victory Audit is MANDATORY before reporting completion
- Refactoring scope: Stages 1–5 (hromatite-win, hromatite-hook, hromatite-il2cpp, hromatite-render, hromatite-eac, injector/runtime split, monolith decomposition, hot router unification, dead code/tools cleanup)
- Enforce workspace lints (`unsafe_op_in_unsafe_fn`, `undocumented_unsafe_blocks`, `missing_safety_doc`)
- Commit each completed stage and push (`git -c credential.helper='!gh auth git-credential' push`), update `AGENTS.md` (§3, §17)
- User communication in Russian

## User Context
- **Last user request**: Approved task prompt for Hromatite refactoring Stages 1–5
- **Pending clarifications**: none
- **Delivered results**: none

## Project Status
- **Phase**: in progress
- **Routing Decision**: General (`teamwork_orchestrator`) — multi-stage architectural refactoring across multiple crates; not Document Review, Math/Proof, or SWE Light.

## Victory Audit Status
- **Triggered**: no
- **Verdict**: pending
- **Retry count**: 0

## Artifact Index
- /home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/ORIGINAL_REQUEST.md — Verbatim approved task prompt
- /home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/sentinel/BRIEFING.md — Sentinel persistent briefing
- /home/wilpdrake/RustroverProjects/Hromatite/.agents/teamwork/orchestrator/ — Orchestrator working directory
