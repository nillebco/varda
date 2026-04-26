# Varda

Varda is a small Rust CLI for running markdown tasks through configured agents.

The current proof of concept routes task files to Codex, runs Codex with a 10 minute limit, stores the agent recap, updates the task status, and commits the changed files with git.

## Start Here

You begin from a git repository that contains this project.

```sh
cargo run -- init
```

This creates the Varda operations folder:

```text
.varda/
  config.toml
  operations/
    tasks/
    recaps/
    runs/
```

The important files are:

- `.varda/config.toml`: tells Varda which agent handles which task paths.
- `.varda/operations/tasks/`: where you put markdown task files.
- `.varda/operations/recaps/`: where agent recaps are written.
- `.varda/operations/runs/`: where notification records are written.

## The Basic Flow

1. Create a markdown task file under `.varda/operations/tasks/`.
2. Give the task YAML frontmatter with `status: ready`.
3. Run `varda run path/to/task.md`.
4. Varda finds the matching route in `.varda/config.toml`.
5. Varda marks the task as `running`.
6. Varda starts the configured agent.
7. The agent has at most 10 minutes to work.
8. The agent must produce a recap before it finishes.
9. Varda writes the recap under `.varda/operations/recaps/`.
10. Varda updates the original task to `pending`, `needs_user`, or `failed`.
11. Varda commits the task update and recap with git.

## Create Your First Task

Create this file:

```text
.varda/operations/tasks/codex/example.md
```

With this content:

```markdown
---
status: ready
assignee: codex
requires_user: false
---

# Task

Read the repository and write a short summary of what Varda currently does.
```

Then run:

```sh
cargo run -- run .varda/operations/tasks/codex/example.md
```

The default config routes files matching this pattern to Codex:

```text
.varda/operations/tasks/codex/**/*.md
```

## Task Statuses

Tasks move through a small state machine:

```text
ready -> running -> pending
ready -> running -> needs_user
ready -> running -> failed
```

Status meanings:

- `ready`: Varda may process the task.
- `running`: Varda has started processing the task.
- `pending`: the agent produced a recap and the task is ready for a later follow-up.
- `needs_user`: the agent needs human input before work can continue.
- `failed`: the agent failed, timed out, or returned unusable output.

## What The Agent Is Told

Varda tells the agent that it has a fixed time limit. The default is 600 seconds, or 10 minutes.

The agent is instructed to finish with a recap that includes:

- what was completed
- what remains
- blockers
- whether user interaction is required
- a suggested next agent, if useful

If the agent needs user input, it should include this exact marker in its recap:

```text
requires_user: true
```

Varda uses that marker to set the task status to `needs_user` and write a notification file.

## Configuration

The default `.varda/config.toml` looks like this:

```toml
[defaults]
timeout_seconds = 600
operations_dir = ".varda/operations"

[[routes]]
glob = ".varda/operations/tasks/codex/**/*.md"
agent = "codex"

[agents.codex]
kind = "acp"
command = "codex"
args = ["exec", "--cd", ".", "--sandbox", "workspace-write", "--ask-for-approval", "never", "-"]

[git]
auto_commit = true
```

For now, `kind = "acp"` means Varda uses its ACP-facing agent abstraction. The concrete POC adapter drives the local Codex CLI with `codex exec` through stdin/stdout because this machine's Codex CLI does not expose a direct `--acp` flag.

## Git Behavior

When `auto_commit = true`, Varda commits after each processed task.

For a normal task, the commit includes:

- the updated task markdown file
- the generated recap file

For a task that needs user input, the commit also includes:

- a notification JSON file under `.varda/operations/runs/`

## Development

Run tests:

```sh
cargo test
```

Check formatting:

```sh
cargo fmt --check
```

Build the CLI:

```sh
cargo build
```

## Current Limitations

- The dashboard is currently a folder structure, not a UI.
- The Codex integration is a subprocess POC, not a full ACP protocol client yet.
- Notification is file-backed JSON plus terminal output.
- Task handoff to another agent is represented by `pending` plus recap metadata, but automatic reassignment is not implemented yet.
