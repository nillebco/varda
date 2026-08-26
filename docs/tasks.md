# Tasks

Creating, running, inspecting and resuming tasks — the day-to-day surface.

# Create Your First Task

From inside the project you want to track, run:

```sh
varda task add "Summarize this project"
```

Or create a task for a project path from anywhere:

```sh
varda task add "Summarize this project" --project /some/project/path
```

Varda prompts for the assignee:

```text
Assignee [codex]:
```

Press Enter to accept the default allowed agent for that project route, or type another allowed agent name.

Varda then creates a markdown task file and opens it in `$EDITOR`. If `EDITOR` is not set, Varda falls back to `vi`.

The generated task starts like this:

```markdown
---
id: 1
status: ready
project: /some/project/path
assignee: codex
requires_user: false
---

# Summarize this project
```

Varda assigns the next available numeric task ID and prints it with the task path.

Add the task details under the heading, then save and quit your editor.

For a one-line task that should run immediately, pass `--exec`:

```sh
varda task add "Summarize this project" --exec
```

Varda still prompts for the assignee, creates the task, skips the editor, and processes the new task through the selected agent.

To run a task in the background and return immediately, add `--background`:

```sh
varda task add "Summarize this project" --exec --background
```

To keep the agent's output visible in the current shell and forward your terminal's stdin to the agent, add `--interactive`:

```sh
varda task add "Summarize this project" --exec --interactive
```

In interactive mode the agent's stderr appears directly in the terminal (so you can see tool calls and progress in real time), stdout is streamed to the terminal and also captured for the session log, and your keyboard input is forwarded to the agent's stdin after the initial prompt is delivered. Interactive runs bypass the configured `timeout_seconds`: Varda will wait for the agent for as long as the session lasts, and the time-limit hint is omitted from the agent prompt.

When an interactive agent starts or resumes, Varda sets the terminal title to `varda + <agent>`.

The interactive prompt also drops the structured recap requirements (file lists, `requires_user` markers, etc.), since those don't fit a free-form conversation. By default, interactive agents still leave project changes unstaged for Varda to commit after the session, but if the task or live conversation explicitly asks the agent to commit, the agent may create a commit scoped to its own work. Once the interactive session ends, Varda prints a status line and runs a non-interactive **interpretation pass**: the same agent backend re-reads the session log and produces the standard Varda recap. The interpretation pass is bounded by `timeout_seconds`.

When the interpreter pass starts, Varda prints a notice on stderr so you don't mistake the wait for a hang. During the post-session storage phase, Ctrl-C is temporarily disabled so Varda can persist the resume command, run the interpreter pass, write the recap, and update the task metadata without leaving the task half-recorded. On Unix, the interpreter subprocess also runs outside Varda's terminal process group so terminal Ctrl-C does not interrupt recap generation.

If the interpreter pass itself fails (times out, or the interpreter agent errors), Varda has no reliable outcome for the run — it does not know whether the underlying session succeeded, needs you, or failed. That "don't know" never resolves to `review`, since `review` is indistinguishable from a clean run awaiting your look; instead the task settles to `needs_user` (firing the usual desktop notification) with a recap carrying the interpretation error text, the driving agent's own session-end output, and a link to the session log, so `varda task show` explains what happened instead of showing nothing.

If the agent the route resolves to has a `resume_command_template` set in `config.toml`, Varda also captures the agent's own session id from its on-disk session storage (claude -> `~/.claude/projects/...`, codex -> `~/.codex/sessions/...`, copilot -> `~/.copilot/session-state/...`) and stores a ready-to-run resume command on the task in the `agent_resume_commands:` frontmatter list. A later `varda task resume` can offer to use that command instead of starting over. For unattended continuation templates, an optional `"{prompt}"` slot receives one-shot trusted operator steering from `<project>/.varda/INBOX.md`; Varda clears the file after materializing the next hop and omits the argument when the inbox is empty or absent. `opencode` is the exception: it keeps sessions in a SQLite database (`~/.local/share/opencode/opencode.db`) rather than per-session files, so Varda can't scan the filesystem for its session id — resume is unwired for opencode in this first pass (use `opencode run --continue` or `opencode session` to resume manually).

`make install` also installs four convenience wrappers next to the `varda` binary:

```sh
vclaude   "Task name" "Optional task description"
vcodex    "Task name" "Optional task description"
vcopilot  "Task name" "Optional task description"
vopencode "Task name" "Optional task description"
```

Each one is a thin shell alias for `varda task add --agent <agent> --exec --interactive`, so the call above is equivalent to:

```sh
varda task add --agent claude --exec --interactive "Task name" "Optional task description"
```

Use them when you want to start an interactive session with a specific agent without typing the full flag list.

### Pinning a sandbox (`--sandbox`)

By default the sandbox for a task is resolved from the nearest `.varda`, the
matched route, and `defaults.sandbox` (see [Sandbox providers](sandboxing.md#sandbox-providers)).
`varda task add --sandbox <NAME>` pins the task to a named central
`[sandboxes.<NAME>]` instead, overriding that resolution with the **highest**
precedence (task-pin → `.varda` → route → `defaults.sandbox` → `local`). Use it
to force a specific sandbox for a one-off task from any directory, regardless of
route — e.g. an interactive microsandbox shell over the current directory:

```sh
varda task add --sandbox msbshell --exec --interactive "shell"
```

The name is validated at creation time and again at run time: it must match a
configured `[sandboxes.<NAME>]` (or be the literal `local`, the identity
provider), otherwise the command errors. The pin is persisted as a `sandbox:`
field in the task frontmatter. It composes with `--exec`, `--interactive`, and
`--agent`. It does **not** relax the resident/orchestrate launch checks
(`enforce_resident_launch` runs for `varda orchestrate`, not plain `task add`).

`taskname` and `description` are two separate positional arguments, so quote any multi-word values:

```sh
vclaude "fix flaky integration test" "use the new fixture from PR #42"
```

Without quotes, the shell splits the input into individual arguments and `varda task add` rejects the extras.

Tasks are stored in the Varda operations folder, grouped into one folder per project:

```text
$VARDA_HOME/operations/tasks/<project-folder>/
```

For the example above, the file is:

```text
$VARDA_HOME/operations/tasks/some-project-path/summarize-this-project.md
```

## Repo-local task store (`.varda/`)

A repository can carry its own task DEFINITIONS and workflow rules alongside its
code. Create a `.varda/` directory at the repo root and Varda splits a task in
two:

- **Definition** (committed, travels with the code) —
  `.varda/tasks/<id>-<slug>.md` holds the frontmatter spec (`id`, `project`,
  `assignee`, `allow_commands`, cooperative bounds, `requires_user`) plus the
  brief. It NEVER carries runtime state.
- **State** (control plane, not committed to the code repo) — status
  transitions, recaps, session logs, and notifications stay under `~/.varda`,
  linked to the definition by `{repo, task_id}`.

Behavior:

- `varda task add` in a repo that has a `.varda/` directory writes the definition
  to `.varda/tasks/` and registers state in `~/.varda`.
- `varda task run <id>` reads the definition from the repo and writes state to
  `~/.varda`. A fresh clone/worktree that carries only the definition
  materializes its home state on first run — state is never committed back into
  the code repo. Materialization is portable and runnable:
  - It binds the runtime `project` to the CURRENT checkout (the repo you run
    from), not the absolute path the definition was committed with, so a clone
    or worktree routes against a real repo.
  - It lands the materialized task in `ready`, so the first `run` of a
    repo-defined task is accepted instead of stalling in `backlog`.
  - The lookup walks up to the repo root, so `run <id>` works from any
    subdirectory of the repo, not just its top level.
  - Materialization seeds the home file once, but the task BODY is never
    frozen there: every later read (`get_task`, `run`, `task list`, the CLI
    display) re-resolves the body live from `.varda/tasks/<id>-<slug>.md`. So
    editing the committed definition after creation — e.g. an operator
    recording a policy decision before spawning a worker — is visible on the
    very next read, with no re-sync step. If the definition can't be resolved
    cleanly (the store is unreadable, the definition file fails to parse, or
    more than one definition file claims the same id), the read fails with an
    explicit error instead of silently falling back to the stale home body.
- `varda task list` unions the repo's definitions with the home store, so a
  clone sees every task the code ships even before it has run anything.
- `.varda/WORKFLOW.md` documents the multi-agent contribution rules
  (worktree-per-task, no agent git commits, file ownership, local gate,
  cross-review, resolver + post-merge check).

**Back-compat:** repos WITHOUT a `.varda/` directory keep the existing
home-only behavior (`$VARDA_HOME/operations/tasks/<project-folder>/`)
unchanged. The `.varda/` *directory* is distinct from the legacy `.varda`
sandbox *file*; a repo has one or the other, and Varda never confuses them.

List tasks for the current project with:

```sh
varda task list
```

By default, `task list` shows active tasks only and hides `backlog` and `done`.
Include every task for the project with:

```sh
varda task list --all
```

Or list tasks for another project path:

```sh
varda task list --project /some/project/path
```

Then run:

```sh
varda task run "$HOME/.varda/operations/tasks/summarize-this-project.md"
```

Foreground non-interactive runs stream the agent's stdout to the terminal as it
runs, so you can watch progress while you wait. The full stdout is still captured
and parsed for the recap; stderr remains silent unless the run fails. Streaming
is automatically disabled when stdout is not a TTY (e.g. piped to a script that
captures the recap) and when running multiple tasks in parallel. Pass `--quiet`
to opt out for a single run:

```sh
varda task run 1 --quiet
```

Pass `--background` to detach immediately:

```sh
varda task run 1 --background
```

Pass `--interactive` to surface the agent in the current shell:

```sh
varda task run 1 --interactive
```

If you set `VARDA_HOME`, use that path instead.

You can also run by numeric task ID:

```sh
varda task run 1
```

Show a task and its associated recap with:

```sh
varda task show "$HOME/.varda/operations/tasks/summarize-this-project.md"
```

You can also show by numeric task ID:

```sh
varda task show 1
```

Any command that accepts a `<TASK>` argument accepts either an existing task file path or a numeric task ID from `varda task list`.

When `varda task run` starts, it reads the task's `project` field and uses that path to select the route and allowed agents.

The old top-level `varda run <task>` command is still available as a compatibility alias when the positional path is a task document. New single-task usage should prefer `varda task run <task>` or `varda run --task <task>`.

The old top-level `varda show task <task>` command is still available as a compatibility alias, but new usage should prefer `varda task show <task>`.

# Tasks Dashboard

Show a kanban board for the current project's tasks with:

```sh
varda task dashboard
```

The dashboard groups tasks by status and then prompts for a task ID or path. Selecting a task displays the full task markdown and every associated recap.

Serve a browser-based Trello-like dashboard with project and status filters:

```sh
varda task dashboard --web
```

The web dashboard is available at `http://127.0.0.1:8787/` by default. It loads tasks across all projects, pre-selects the current folder in the project filter when that project has tasks, refreshes task data every 30 seconds, and lets you select a task to inspect its markdown and associated recaps. Cards within each column are sorted by completion date (most recent recap or task update) descending. Drag a task card to the done column to mark it reviewed or archived. Use `--port` to choose a different local port.

Run the web dashboard detached from the terminal so it survives shell exit:

```sh
varda task dashboard --web --daemon
```

The command prints the daemon's PID and exits immediately. Stop it later with `kill <pid>`. `--daemon` requires `--web` and is supported on Unix systems.

Show all tasks across all projects:

```sh
varda task dashboard --all
```

Open a specific task from the dashboard without the prompt:

```sh
varda task dashboard --task 1
```

# Plan Ready Work

Create a reviewable execution plan with:

```sh
varda plan
```

When the current folder already has tasks in Varda's task store, the command plans ready tasks for that project. Otherwise, it plans ready tasks across all projects. Plans are written under the global operations folder:

```text
$VARDA_HOME/operations/plans/
```

The generated markdown includes YAML frontmatter with the scope, project, task counts, planner agent, and review-gate metadata. It assigns each ready task to the routed agent, explains the project/global selection, groups tasks into sequential and parallel candidate stages, and leaves execution behind an explicit user review gate.

Execute ready work with:

```sh
varda run
```

With no arguments, `varda run` finds every `ready` task in the operations task store and starts them in parallel through their routed agents.

Execute a reviewed plan with:

```sh
varda run "$HOME/.varda/operations/plans/global-workspace-ready-task-plan-1775000000.md"
```

Before running a plan, Varda asks the configured planner agent to convert the markdown plan to a lightweight JSON document using schema `varda.execution_plan.v1`, writes that JSON beside the markdown plan, then runs the listed tasks in parallel. The JSON contains a `tasks` array with task paths and optional metadata such as `id`, `title`, `agent`, `project`, `stage`, and `parallel_group`.

Run one task through the top-level command with:

```sh
varda run --task 1
```

# Task Statuses

Tasks move through a small state machine:

```text
ready -> running -> review
ready -> running -> needs_user
ready -> running -> failed
review -> done
```

Status meanings:

- `ready`: Varda may process the task.
- `running`: Varda has started processing the task.
- `review`: the agent completed a run, produced a recap, and the task is waiting for human review / follow-up. (Formerly `pending`; see migration below.)
- `needs_user`: the agent needs human input before work can continue.
- `failed`: the agent failed, timed out, or returned unusable output.
- `done`: the task has been reviewed or archived after completion.

## Migrating legacy `pending` task files

`review` was previously called `pending`. Reading a legacy `status: pending` task file still works (it loads as `review`), and `varda task update --set-status pending …` is accepted as an alias for `review`. To rewrite existing control-plane state files in place, run:

```bash
varda task migrate-status
```

It rewrites every `status: pending` file under the configured operations task directory to `status: review`, preserves all other frontmatter and the task body, is idempotent, and reports how many files were changed.

# Inspect And Repair A Run

Five `varda task` subcommands exist for looking at a run after the fact; they are the first thing to reach for when a task misbehaves.

| Command | What it does |
| --- | --- |
| `varda task inspect <TASK>` | Runtime diagnostics for a task: resolved agent config, the matched route, session logs, and any live processes. Start here when a run is behaving unexpectedly. |
| `varda task doctor <TASK>` | Probes the **latest** run by cross-checking independent authorities — the sandbox provider's own view of the box and the session log — and reports whether it booted, produced output, and reached a terminal state. Use it when a task looks stuck: it distinguishes "still working", "finished but the recap is late", and "the box died". |
| `varda task show <TASK>` | The task body (with any repo-local `.varda/` definition overlaid) plus its recap. |
| `varda task edit <TASK>` | Opens the markdown task in `$EDITOR`. |
| `varda task resolve <TASK>` | Prints the resolved file path for a task id or path — useful in scripts and when an id is ambiguous. |
| `varda task delete <TASK>` | Deletes the task's runtime **state** file and its recaps from the home store. The repo-local `.varda/tasks/` definition, if any, is not touched. |

> A task id resolves through both the home state store and the repo-local `.varda/tasks/` definitions. When the two disagree, `varda task show` renders the overlaid body — the same one the runner uses — while the raw file on disk may differ.

# Resume A Task

When an agent needs user input, the task is left in this state:

```yaml
status: needs_user
requires_user: true
```

Resume it with:

```sh
varda task resume "$HOME/.varda/operations/tasks/summarize-this-project.md"
```

Or resume it by numeric task ID:

```sh
varda task resume 1
```

`varda task resume` does this:

1. Sets `status: ready`.
2. Sets `requires_user: false`.
3. If the task was in `needs_user`, offers to open `$EDITOR` so you can add the missing user input.
4. If the task was not in `needs_user`, skips the editor prompt.
5. If `agent_resume_commands` contains a non-empty command, offers to resume the latest captured agent session.
6. If accepted, runs the captured command interactively and then runs the interpreter pass to produce the standard Varda recap.
7. If declined, if no captured command exists, or if `--fresh` is passed, starts a new agent session with `varda task run`.

Use `--fresh` to skip any captured command and start over:

```sh
varda task resume 1 --fresh
```

If you set `VARDA_HOME`, use that path instead of `$HOME/.varda`.

# Resume A Past Session

To move a past task back to `ready` without running it immediately, choose one of its recorded sessions:

```sh
varda task resume-session 1
```

Varda scans `operations/runs/*.log` for sessions tied to that task, prompts for the session to resume, stores the selected `agent_session_id` and `agent_session_log` in task frontmatter, sets `status: ready`, and clears `requires_user`.

Frontmatter precedence:

- `agent_resume_commands` is the only field `varda task resume` can execute directly. When multiple commands exist, Varda offers the latest non-empty entry.
- `agent_session_ids` and `agent_session_logs` remain the Varda run history and inspection trail. They are not enough by themselves to resume an agent-owned interactive session.
- `varda task resume-session` uses historical Varda run logs to attach a past `agent_session_id` and `agent_session_log` to a task, but it does not create an agent resume command.

[← back to the README](../README.md)
