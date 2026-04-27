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
- `$VARDA_HOME/operations/runs/`: where agent session logs and notifications are written.
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
8. Review the task kanban board with `varda task dashboard`.
9. Create a reviewable ready-task plan with `varda plan`.
10. Run a task with `varda task run path/to/task.md`, run a reviewed plan with `varda run path/to/plan.md`, or run all ready tasks with `varda run`.
11. Show a task and its recap with `varda task show path/to/task.md`.
12. Varda finds the matching project route in the global config.
13. Varda verifies the assignee is allowed for that project.
14. Varda marks the task as `running`.
15. Varda starts the configured agent.
16. The agent has at most 10 minutes to work.
17. The agent must follow project instructions from `CLAUDE.md`, `AGENTS.md`, and `copilot-instructions.md` when those files exist.
18. The agent must produce a recap before it finishes, including a `Files touched` section that lists every created, modified, or deleted file as an absolute path.
19. Varda writes the recap under the global operations folder.
20. Varda updates the original task to `pending`, `needs_user`, or `failed`.
21. Varda commits the task update and recap with git.

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

The old top-level `varda run <task>` command is still available as a compatibility alias when the positional path is a task document. New single-task usage should prefer `varda task run <task>` or `varda run --task <task>`.

The old top-level `varda show task <task>` command is still available as a compatibility alias, but new usage should prefer `varda task show <task>`.

## Tasks Dashboard

Show a kanban board for the current project's tasks with:

```sh
varda task dashboard
```

The dashboard groups tasks by status and then prompts for a task ID or path. Selecting a task displays the full task markdown and every associated recap.

Serve a browser-based Trello-like dashboard with project and status filters:

```sh
varda task dashboard --web
```

The web dashboard is available at `http://127.0.0.1:8787/` by default. It loads tasks across all projects, pre-selects the current folder in the project filter when that project has tasks, refreshes task data every 30 seconds, and lets you select a task to inspect its markdown and associated recaps. Use `--port` to choose a different local port.

Show all tasks across all projects:

```sh
varda task dashboard --all
```

Open a specific task from the dashboard without the prompt:

```sh
varda task dashboard --task 1
```

## Plan Ready Work

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

## Task Statuses

Tasks move through a small state machine:

```text
ready -> running -> pending
ready -> running -> needs_user
ready -> running -> failed
pending -> done
```

Status meanings:

- `ready`: Varda may process the task.
- `running`: Varda has started processing the task.
- `pending`: the agent produced a recap and the task is ready for a later follow-up.
- `needs_user`: the agent needs human input before work can continue.
- `failed`: the agent failed, timed out, or returned unusable output.
- `done`: the task has been reviewed or archived after completion.

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

## Resume A Past Session

To move a past task back to `ready` without running it immediately, choose one of its recorded sessions:

```sh
varda task resume-session 1
```

Varda scans `operations/runs/*.log` for sessions tied to that task, prompts for the session to resume, stores the selected `agent_session_id` and `agent_session_log` in task frontmatter, sets `status: ready`, and clears `requires_user`.

## What The Agent Is Told

Varda tells the agent that it has a fixed time limit. The default is 600 seconds, or 10 minutes.

The agent is instructed to finish with a recap that includes:

- what was completed
- what remains
- blockers
- whether user interaction is required
- a suggested next agent, if useful
- a `Files touched` section with one absolute file path per created, modified, or deleted file, or `(none)` when no files changed

The agent must end every recap with exactly one machine-readable marker:

```text
requires_user: true
```

or

```text
requires_user: false
```

Varda uses that marker to set the task status to `needs_user` and write a notification file.
As a fallback, Varda also treats recap text such as `User interaction required: yes` or a `User Interaction Required` heading followed by `Yes: ...` as requiring user input.

Each task run also records the latest agent session in task frontmatter:

```yaml
agent_session_id: 2f6f0f2c-7ad9-4d78-b5b6-66a9eddfce54
agent_session_log: /home/user/.varda/operations/runs/2f6f0f2c-7ad9-4d78-b5b6-66a9eddfce54.log
```

Varda writes these fields before launching the agent, so an interrupted runner still leaves a resumable run pointer on the task. For Claude Code runs, Varda also records the discovered Claude transcript as `external_session_id` and `external_session_log` inside the session log when it can match the generated Claude JSONL file. While the agent runs, stdout and stderr are streamed into the session log instead of being buffered until process exit. If the agent process fails or times out, the synthetic failure recap includes the session ID and a link to that log file.

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

[agents.claude]
kind = "acp"
command = "claude"
args = ["-p", "--permission-mode", "acceptEdits", "--add-dir", "{project}"]

[agents.copilot]
kind = "acp"
command = "gh"
args = ["copilot", "suggest", "-t", "shell", "-"]

[git]
auto_commit = true
```

For now, `kind = "acp"` means Varda uses its ACP-facing agent abstraction. The concrete POC adapter drives the local Codex CLI with `codex exec` through stdin/stdout because this machine's Codex CLI does not expose a direct `--acp` flag.

When the generated Codex args contain `--cd "."`, Varda replaces that `.` at runtime with the task's `project` path. That is what makes the tracked project writable to Codex under `--sandbox workspace-write`, even though the task file itself lives in the global Varda control-plane folder.

The generated Claude Code args use `-p` for non-interactive output through stdin/stdout. Varda expands `{project}` in `--add-dir "{project}"` so Claude can access the tracked project while the task file remains in the global Varda control-plane folder.

The generated GitHub Copilot args use `gh copilot suggest -t shell -` to drive the GitHub CLI copilot extension through stdin/stdout. The `-` at the end signals stdin input mode. You must have the `gh` CLI installed with the copilot extension (`gh extension install github/gh-copilot`).

## Git Behavior

When `auto_commit = true`, Varda commits after each processed task.

For a normal task, the commit includes:

- the updated task markdown file
- the generated recap file
- the generated agent session log

For a task that needs user input, the commit also includes:

- a notification JSON file under the global `operations/runs/` folder

## Install The Claude Code Skill

Varda ships a Claude Code skill that lets any agent session manage tasks with natural language. Install it from the varda project directory:

```sh
varda skill install
```

This copies `skills/varda/SKILL.md` to `~/.claude/skills/varda/SKILL.md`. Pass `--link` to create a symlink instead so the skill stays in sync with the repository:

```sh
varda skill install --link
```

You can also point to a specific source file:

```sh
varda skill install /path/to/SKILL.md
```

Once installed, start any Claude Code session and use `/varda` to trigger the skill. The skill exposes all Varda commands (`task add`, `task list`, `task run`, `task show`, `task resume`, `plan`, `project add`).

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

- The Codex integration is a subprocess POC, not a full ACP protocol client yet.
- Notification is file-backed JSON plus terminal output.
- Task handoff to another agent is represented by `pending` plus recap metadata, but automatic reassignment is not implemented yet.
