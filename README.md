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

This creates the global Varda control-plane folder.

Varda uses `$VARDA_HOME` when it is set. Otherwise it uses:

```text
$HOME/.varda
```

The folder looks like this:

```text
$HOME/.varda/
  .git/
  config.toml
  operations/
    tasks/
    recaps/
    runs/
```

The important files are:

- `$VARDA_HOME/config.toml` or `$HOME/.varda/config.toml`: tells Varda which agents are allowed for which project paths.
- `$VARDA_HOME/operations/tasks/`: where Varda stores markdown task files.
- `$VARDA_HOME/operations/recaps/`: where agent recaps are written.
- `$VARDA_HOME/operations/runs/`: where notification records are written.

`varda init` also runs `git init` in the control-plane folder. This matters because `varda task run` commits task updates, recaps, and notifications into that repository.

## The Basic Flow

1. Add project routes with `varda project add`.
2. Create a markdown task with `varda task add`.
3. Varda records the project path in the task frontmatter.
4. Varda asks for an assignee, defaulting to the first allowed agent for that project.
5. Varda creates the task file and opens it in `$EDITOR`.
6. Write the task details and save the file.
7. List project tasks with `varda task list`.
8. Run `varda task run path/to/task.md`.
9. Show a task and its recap with `varda task show path/to/task.md`.
10. Varda finds the matching project route in the global config.
11. Varda verifies the assignee is allowed for that project.
12. Varda marks the task as `running`.
13. Varda starts the configured agent.
14. The agent has at most 10 minutes to work.
15. The agent must produce a recap before it finishes.
16. Varda writes the recap under the global operations folder.
17. Varda updates the original task to `pending`, `needs_user`, or `failed`.
18. Varda commits the task update and recap with git.

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

The agents listed in `--agents` must already exist in the global config.

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

Tasks are stored in the Varda operations folder:

```text
$VARDA_HOME/operations/tasks/
```

For the example above, the file is:

```text
$VARDA_HOME/operations/tasks/summarize-this-project.md
```

List tasks for the current project with:

```sh
varda task list
```

Or list tasks for another project path:

```sh
varda task list --project /some/project/path
```

Then run:

```sh
varda task run "$HOME/.varda/operations/tasks/summarize-this-project.md"
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

The old top-level `varda run <task>` command is still available as a compatibility alias, but new usage should prefer `varda task run <task>`.

The old top-level `varda show task <task>` command is still available as a compatibility alias, but new usage should prefer `varda task show <task>`.

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

## Resume A Task

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
5. Commits that resume edit to the Varda home git repo.
6. Runs the task immediately with `varda task run`.

If you set `VARDA_HOME`, use that path instead of `$HOME/.varda`.

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

The default global config looks like this:

```toml
[defaults]
timeout_seconds = 600
operations_dir = "operations"

[[routes]]
glob = "**"
agents = ["codex"]

[agents.codex]
kind = "acp"
command = "codex"
args = ["exec", "--cd", ".", "--sandbox", "workspace-write", "-"]

[git]
auto_commit = true
```

For now, `kind = "acp"` means Varda uses its ACP-facing agent abstraction. The concrete POC adapter drives the local Codex CLI with `codex exec` through stdin/stdout because this machine's Codex CLI does not expose a direct `--acp` flag.

When the generated Codex args contain `--cd "."`, Varda replaces that `.` at runtime with the task's `project` path. That is what makes the tracked project writable to Codex under `--sandbox workspace-write`, even though the task file itself lives in the global Varda control-plane folder.

## Git Behavior

When `auto_commit = true`, Varda commits after each processed task.

For a normal task, the commit includes:

- the updated task markdown file
- the generated recap file

For a task that needs user input, the commit also includes:

- a notification JSON file under the global `operations/runs/` folder

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
