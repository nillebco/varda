# Varda contribution & parallel-work rules

This file travels with the code (committed under `.varda/`). It defines how
multiple AI agents (and humans) work this repository in parallel without
stepping on one another. Runtime STATE (status, recaps, session logs,
notifications) is NEVER stored here — it lives in the `~/.varda` control plane,
keyed by `{repo, task_id}`. Only DEFINITIONS (`.varda/tasks/*.md`) and rules
(this file, optional `.varda/config.toml`) are committed.

## Task definitions vs. runtime state

- `.varda/tasks/<id>-<slug>.md` — the durable task DEFINITION: frontmatter spec
  (`id`, `project`, `assignee`, `allow_commands`, cooperative bounds,
  `requires_user`) plus the brief. Committed with the code.
- `~/.varda/operations/` — runtime STATE: status transitions, recaps
  (`operations/recaps`), session logs (`operations/runs`), notifications. NOT
  committed to the code repo.
- `varda task add` in a repo that has a `.varda/` directory writes the DEFINITION
  here and registers state in `~/.varda`, linked by id + repo path.
- `varda task run <id>` reads the DEFINITION from the repo, writes STATE to
  `~/.varda`, and commits only CODE changes (via the `Files touched` flow).
- A clone/worktree that carries the DEFINITION but has no home state
  materializes a fresh `~/.varda` state file on first `run` — state is never
  committed back into the code repo.

## Worktree-per-task

- Each task runs in its own git worktree/branch based on the mother branch.
- Work ONLY inside your assigned worktree. Do not touch the mother checkout or
  another task's worktree.
- Branch names track the task (e.g. `feat/<slug>`).

## No agent git commits

- Agents MUST NOT run `git add`, `git commit`, `git push`, `git rebase`, or
  `git merge`. Varda owns committing.
- Agents leave changes unstaged in the working tree and list every changed file
  (one path per line) under a `## Files touched` heading in their recap. Varda
  stages and commits exactly those paths.

## File ownership & disjoint footprints

- Tasks meant to run in parallel are scoped to DISJOINT file footprints so they
  never conflict. Confirm your footprint before editing.
- If unrelated user changes are already present, do not revert them and do not
  list them under `Files touched` unless required for your change.

## Local PR / rebase / gate

- After an agent finishes, Varda runs the local gate (build + `cargo test`)
  before integrating the branch.
- Integration rebases/merges the task branch onto the mother branch; the agent
  never performs this step.

## Cross-review

- Every authored task is reviewed by a DIFFERENT agent than the author
  (e.g. author `claude` → reviewer `codex`). The reviewer inspects the diff for
  correctness and adherence to these rules.

## Resolver + post-merge check (resident role)

- When parallel branches touch overlapping files, a resolver agent merges them
  and re-runs the gate.
- A resident role performs the post-merge check: it verifies the integrated
  tree still builds and passes tests before the mother branch advances.

## Secrets

- Task DEFINITIONS reference secret NAMES only (per M11) — never resolved secret
  values. Never commit secrets or runtime state into the repo.
