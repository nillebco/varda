---
name: varda
description: This skill should be used when the user asks to "add a varda task", "create a task", "list varda tasks", "run a task", "show a task", "resume a task", "plan tasks", "add a project to varda", or manage their AI-agent task pipeline via Varda. Varda routes markdown tasks to AI agents (Claude, Codex, Copilot) and tracks their lifecycle.
version: 0.1.0
---

# Varda Task Management

Varda is a CLI tool that routes markdown tasks to AI agents and tracks their lifecycle. Tasks live in `~/.varda/operations/tasks/` as markdown files with YAML frontmatter.

## Quick Reference

| Goal | Command |
|------|---------|
| Create a task | `varda task add "task name" --project /path/to/project --agent claude` |
| Create and run immediately | `varda task add "task name" --project /path --agent claude --exec` |
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

- User wants to **delegate work to an agent**: `varda task add --exec`
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
