# Project Ready Task Execution Plan

- Scope: project
- Generated date: 2026-04-27
- Project: `/Users/nilleb/dev/nillebco/varda`
- Selection rule: the current folder is known as a project because ready tasks in the Varda task store reference this exact project path; therefore this plan considers project tasks rather than tasks across all projects.
- Planner agent: codex
- Execution should wait for explicit user confirmation.

## Ready Tasks

- `global-run.md`: Global run (agent: codex)
- `global-runner.md` / #3: Global runner (agent: codex)
- `planner-agent.md`: Planner agent (agent: codex)
- `resume-session-interactively.md`: Resume session interactively (agent: codex)
- `run-a-specific-task.md`: Run a specific task (agent: codex)
- `support-claude.md`: Claude support (agent: codex)
- `support-sessions.md`: Support sessions (agent: codex)
- `tasks-dashboard.md`: Tasks Dashboard (agent: codex)
- `version-the-task.md`: Version the task (agent: codex)

## Priority And Dependencies

- First: `version-the-task`, because stable task snapshots and IDs reduce ambiguity for every later run.
- Next: `global-run`, `global-runner`, and `planner-agent`, because they define how ready work is selected, planned, reviewed, and executed.
- Then: `run-a-specific-task`, because explicit task targeting should align with the newer run/planning semantics.
- Then: `support-sessions` and `resume-session-interactively`, because session persistence depends on the run/task lifecycle.
- Then: `tasks-dashboard`, because dashboard behavior depends on stable status, task identity, and recap/session metadata.
- Optional/independent: `support-claude` can run after route and agent configuration assumptions are clear; it can be parallel with dashboard work if it only changes agent configuration and invocation code.

## Execution Stages

1. Stage 1, sequential: complete task versioning and ID consistency.
2. Stage 2, sequential: implement planning and global run command semantics.
3. Stage 3, sequential: align explicit task running with the new execution model.
4. Stage 4, parallel candidates: session tracking/resume work and dashboard work, provided they touch disjoint modules.
5. Stage 5, optional parallel candidate: Claude support, if it is isolated to configuration and agent invocation.
6. Stage 6, sequential validation: run CLI checks against representative tasks, update docs, and request confirmation before executing the ready set.

## Review Gate

The next step is user review. The plan should be confirmed or edited before Varda executes the ready tasks.
