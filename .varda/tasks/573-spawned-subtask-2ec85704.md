---
id: 573
project: /Users/nilleb/dev/varda-orchestrate-workspace
assignee: claude-worker
---

# spawned-subtask-2ec85704

Bugfix #522: `varda orchestrate` should reset the resident task to `ready` on each launch.

Symptom: On a second `varda orchestrate` run, it fails with `task .../resident-orchestrator.md is not ready; current status is Failed` (see the exact error format at src/main.rs:2871 and src/runner.rs:440,633 — `"task {} is not ready; current status is {:?}"`). The command reuses the scaffolded resident task (`resolve_or_scaffold_resident_task`, src/main.rs:3123), but if a prior launch left that task in a terminal state (Failed/pending/done), `run_task_command` (src/main.rs:3211) refuses to run it. Manual workaround today: `varda task set-status ready <id>`.

Fix: in `orchestrate_command` (src/main.rs:3154), right after the call to `resolve_or_scaffold_resident_task` at src/main.rs:3200 and before delegating to `run_task_command` at line 3208, ensure the resident task is runnable: if its status is not `Ready`/`Backlog` (see `TaskStatus` enum in src/task.rs:143), reset it to `Ready` before calling `run_task_command`. Preserve prior recap/session history — only flip the status field, don't wipe the task body. Only touch this ORCHESTRATE resident-task path, not general `run` command semantics for other tasks.

Tests to add: (1) a resident task left in `Failed` (and separately `pending`/`done`) status gets reset to `Ready` and launches successfully on the next `orchestrate` call; (2) a fresh first-time scaffold still works unchanged (regression).

Footprint: `src/main.rs` only (orchestrate_command + a helper if needed), plus tests. Small, scoped change — do not touch `src/runner.rs`, `src/config.rs`, or `src/task.rs` beyond reading their existing public API (TaskStatus, task load/save helpers already used elsewhere in main.rs).

Run `cargo test` and `cargo build` when done and report Files touched per AGENTS.md — do not git add/commit.
