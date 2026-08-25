---
name: varda
description: This skill should be used when the user asks to "add a varda task", "create a task", "list varda tasks", "run a task", "show a task", "resume a task", "plan tasks", "add a project to varda", or manage their AI-agent task pipeline via Varda. Varda routes markdown tasks to AI agents (Claude, Codex, Copilot) and tracks their lifecycle.
version: 0.3.0
---

# Varda Task Management

Varda is a CLI tool that routes markdown tasks to AI agents and tracks their lifecycle. Tasks live in `~/.varda/operations/tasks/` as markdown files with YAML frontmatter.

## Quick Reference

| Goal | Command |
|------|---------|
| Create a task | `varda task add "task name" --project /path/to/project --agent claude` |
| Create with a detailed body, ready to run | `varda task add "task name" --project /path --agent claude --ready < body.md` |
| Run a one-line task immediately (body ignored) | `varda task add "do X" --project /path --agent claude --exec` |
| Run a task in the background (no wait) | `varda task run <id> --background` |
| Run a task in the foreground (wait until done) | `varda task run <id>` |
| Set a task's status directly | `varda task set-status <id> ready` |
| Runtime diagnostics / live processes | `varda task inspect <id>` |
| List tasks for a project | `varda task list --project /path/to/project` |
| List all tasks (from project dir) | `varda task list` |
| Run a task | `varda task run <task_path_or_id>` |
| Show task + recap | `varda task show <task_path_or_id>` |
| Plan a task (agent-driven) | `varda task plan <task_path_or_id>` |
| Resume a needs_user task | `varda task resume <task_path_or_id>` |
| Generate execution plan | `varda plan` |
| Add a project route | `varda project add "/path/**" --agents claude,codex` |
| Initialize Varda | `varda init` |

## Task Statuses

| Status | Meaning |
|--------|---------|
| `ready` | Waiting to be run |
| `running` | Agent is currently executing |
| `pending` | Completed but not yet reviewed |
| `needs_user` | Agent requires user input before continuing |
| `failed` | Execution failed |

## Task File Format

Tasks are markdown files with YAML frontmatter at `~/.varda/operations/tasks/`:

```markdown
---
id: 42
status: ready
project: /Users/nilleb/dev/myproject
assignee: claude
---

# Task Title

Describe what the agent should do here. Be specific and include context.
```

## Common Workflows

### Create a task for a project

Use `varda task add` with `--project` pointing to the target repository and `--agent` to select the agent. With `--exec`, the task runs immediately without opening the editor.

```bash
varda task add "fix login bug" --project /Users/nilleb/dev/myapp --agent claude --exec
```

Without `--exec`, Varda opens `$EDITOR` so the user can fill in the task details before it becomes ready.

### Create a detailed task non-interactively, then run it (preferred for agents)

`--exec` treats the **task name itself as the entire one-line task** — any body you provide is ignored, and it would otherwise block on `$EDITOR`. So for a real task with a multi-paragraph brief, **do not use `--exec`**. Instead give the body and mark it ready in one shot, then run it separately:

- The **task name** is the first positional arg.
- The **body** is the second positional arg, OR is read from **stdin** when stdin is not a terminal (pipe or `<` redirect — best for long, multi-line briefs to avoid shell-quoting pain).
- `--ready` sets status to `ready` immediately (skips the backlog), so the task can be run without an editor round-trip.

```bash
# Write the brief to a temp file, then pipe it as the task body:
varda task add "Short task title" \
  --project /Users/nilleb/dev/brain --agent claude --ready < /tmp/task-body.md
# → prints: created task #NNN /Users/.../tasks/.../short-task-title.md

# Then start it in the background so it doesn't block the session:
varda task run NNN --background          # prints the pid
varda task list --project /Users/nilleb/dev/brain   # confirm status flips to "running"
```

`varda task run` flags: `--background` (spawn and return immediately), `--interactive` (forward stdin/stdout), `--quiet` (suppress live streaming).

**Project + agent choice for research/writing tasks:** the catch-all route `glob = "**"` in `~/.varda/config.toml` maps to agents `codex` and `claude`. A non-code investigation whose deliverable is a vault doc can use `--project /Users/nilleb/dev/brain --agent claude`; the agent runs with `--add-dir {project}` so it can write into `ADB/Documentation/` etc. Always pass an explicit `--agent` to skip the interactive assignee prompt.

### Run modes & waiting for a task

There is **no `wait` command and no queue/lock** to block on an *already-detached* background run. Whether `varda task run` waits is decided entirely by the run mode you pick at launch:

| Mode | Flag | Waiting behavior |
|------|------|------------------|
| **Foreground** (default) | *(none)* | **Blocks until the agent finishes**, streams stdout live, then prints the recap. This *is* the built-in "wait in line." |
| Background | `--background` | Detaches immediately, returns a PID — does **not** wait. |
| Interactive | `--interactive` | Waits for the whole session; **bypasses `timeout_seconds`**. |

So to "wait in line" for a task, run it in the **foreground** (omit `--background`) and the call blocks until done. Notes:

- Parallel runs are **allowed, not serialized** — running multiple tasks at once just disables live stdout streaming (also disabled when stdout isn't a TTY). There is no lock to queue behind.
- For an *already* backgrounded run there's no Varda-native wait: wait at the OS level (`wait <pid>` in the same shell) or poll `varda task list` / `varda task show <id>`. `varda task inspect <id>` shows live-process diagnostics but does **not** block.
- Prefer a **foreground** run when you want to act on the result immediately; reserve `--background` for fire-and-forget where you'll poll later.

### Re-spin a finished task with new indications

To iterate on a task that already ran (status `pending`/`failed`) — e.g. to feed back corrections or verified facts — append your notes to the task `.md` body, flip it back to runnable, and run again. The new run appends another recap + session id; prior recaps are preserved.

```bash
varda task edit <id>                 # append an "ITERATION 2" section with new indications
varda task set-status <id> ready     # pending → ready (or just run; run re-executes regardless)
varda task run <id>                  # foreground to wait, or --background to detach
```

`varda task show <id>` then prints **all** recaps in order, so you can compare iterations.

### Check task status and recap

After a task finishes, `varda task show <id>` prints the task frontmatter, body, and the agent's recap (what was done, what remains, blockers).

```bash
varda task show 42
```

### Handle needs_user tasks

When an agent sets status to `needs_user`, it requires user input before proceeding. Use `resume` to update the task with new information and re-run it:

```bash
varda task resume 42
```

Varda prompts whether to open the editor to add context, then re-runs the task.

### Generate an execution plan

Run `varda plan` from a project directory to produce a prioritized plan for all `ready` tasks. The plan is written to `~/.varda/operations/plans/` and must be reviewed before tasks are executed.

```bash
cd /Users/nilleb/dev/myproject
varda plan
```

### Add a new project to Varda

```bash
varda project add "/Users/nilleb/dev/myproject/**" --agents claude
```

The glob pattern matches the project path during task routing.

## File Locations

| Path | Contents |
|------|----------|
| `~/.varda/config.toml` | Routes, agent definitions, git settings |
| `~/.varda/operations/tasks/` | Task markdown files |
| `~/.varda/operations/recaps/` | Agent recap outputs |
| `~/.varda/operations/runs/` | Run metadata and notification records |
| `~/.varda/operations/plans/` | Execution plans |

## Referring to Tasks

`task_path_or_id` accepts either:
- An absolute or relative path to the `.md` file
- A numeric task ID (e.g., `42`) — Varda resolves it from the task store

## When to Use Which Command

- User wants to **delegate a detailed task to an agent**: `varda task add ... --ready < body.md` then `varda task run <id> --background`
- User wants to **fire off a quick one-liner**: `varda task add "do X" --exec` (the name *is* the task; body ignored)
- User wants to **review what was done**: `varda task show`
- User wants to **see all pending work**: `varda task list`
- Agent stopped and **needs information**: `varda task resume`
- User wants a **prioritized overview**: `varda plan`

## Installation

To make this skill available from any Claude Code session, run from the varda project directory:

```bash
varda skill install
```

Pass `--link` to symlink instead of copy so the skill stays in sync with the repository:

```bash
varda skill install --link
```
