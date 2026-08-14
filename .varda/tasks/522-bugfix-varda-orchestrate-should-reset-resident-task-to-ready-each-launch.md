---
id: 522
project: /Users/nilleb/dev/nillebco/varda
assignee: codex
---

# Bugfix: varda orchestrate should reset resident task to ready each launch

# Bugfix: `varda orchestrate` should reset the resident task to `ready` on each launch

## Symptom
On a second `varda orchestrate` run, it fails with `task .../resident-orchestrator.md is not ready; current status is Failed`. The command reuses the scaffolded resident task (`resolve_or_scaffold_resident_task`), but if a prior launch left that task in a terminal state (`Failed`/`pending`/`done`), `run_task_command` refuses to run it. Manual workaround: `varda task set-status ready <id>`.

## Fix
In `orchestrate_command` (`src/main.rs`), after `resolve_or_scaffold_resident_task`, ensure the resident task is runnable: if its status is not `ready`/`backlog`, reset it to `ready` before delegating to `run_task_command`. (The resident task is a persistent, re-runnable driver — each `orchestrate` invocation is a fresh session, so a prior Failed/terminal status must not block the next launch.)
- Preserve prior recaps/session history (don't wipe the task body); only flip the status.
- Only touch the ORCHESTRATE resident task, not general `run` semantics.

## Test
- A resident task left in `Failed` (and `pending`/`done`) status is reset to `ready` and launches on the next `orchestrate` call.
- A fresh scaffold (first run) still works unchanged.

## Footprint
`src/main.rs` (orchestrate_command) + a test. Small.
