# Varda

Varda is a small Rust CLI for running markdown tasks through configured agents.

The current proof of concept tracks work across multiple projects from one operations folder. It routes each task by the task's project path, runs an allowed agent with a 10 minute limit, stores the agent recap, updates the task status, and commits the changed files with git.

## Start Here

You begin from a git repository that contains this project.

Build and install the executable:

```sh
make install
```

By default this installs `varda` to `~/.local/bin/varda`. Make sure that directory is in your `PATH`.

After that, the docs assume you can run `varda` directly.

```sh
varda init
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

- `.varda/config.toml`: tells Varda which agents are allowed for which project paths.
- `.varda/operations/tasks/`: where you put markdown task files.
- `.varda/operations/recaps/`: where agent recaps are written.
- `.varda/operations/runs/`: where notification records are written.

## The Basic Flow

1. Add project routes with `varda project add`.
2. Create a markdown task with `varda task add`.
3. Varda records the project path in the task frontmatter.
4. Varda asks for an assignee, defaulting to the first allowed agent for that project.
5. Varda creates the task file and opens it in `$EDITOR`.
6. Write the task details and save the file.
7. Run `varda run path/to/task.md`.
8. Varda finds the matching project route in `.varda/config.toml`.
9. Varda verifies the assignee is allowed for that project.
10. Varda marks the task as `running`.
11. Varda starts the configured agent.
12. The agent has at most 10 minutes to work.
13. The agent must produce a recap before it finishes.
14. Varda writes the recap under `.varda/operations/recaps/`.
15. Varda updates the original task to `pending`, `needs_user`, or `failed`.
16. Varda commits the task update and recap with git.

## Add Project Routes

Routes match project paths, not task file paths.

The default config allows Codex for all project paths:

```toml
[[routes]]
glob = "**"
agents = ["codex"]
```

Add another project route with:

```sh
varda project add "/some/project/path/**" --agents codex,claude
```

The agents listed in `--agents` must already exist in `.varda/config.toml`.

Routes are checked in order. Put more specific project routes before broad catch-all routes when you edit the config manually.

## Create Your First Task

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
status: ready
project: /some/project/path
assignee: codex
requires_user: false
---

# Summarize this project
```

Add the task details under the heading, then save and quit your editor.

Tasks are stored in the Varda operations folder:

```text
.varda/operations/tasks/
```

For the example above, the file is:

```text
.varda/operations/tasks/summarize-this-project.md
```

Then run:

```sh
varda run .varda/operations/tasks/summarize-this-project.md
```

When `varda run` starts, it reads the task's `project` field and uses that path to select the route and allowed agents.

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
glob = "**"
agents = ["codex"]

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
make test
```

Check formatting:

```sh
make fmt
```

Build the CLI:

```sh
make build
```

Build the release executable:

```sh
make release
```

Install it to `~/.local/bin`:

```sh
make install
```

## Current Limitations

- The dashboard is currently a folder structure, not a UI.
- The Codex integration is a subprocess POC, not a full ACP protocol client yet.
- Notification is file-backed JSON plus terminal output.
- Task handoff to another agent is represented by `pending` plus recap metadata, but automatic reassignment is not implemented yet.
