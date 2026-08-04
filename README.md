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
      <project-folder>/
    recaps/
    runs/
```

The important files are:

- `$VARDA_HOME/config.toml` or `$HOME/.varda/config.toml`: tells Varda which agents are allowed for which project paths.
- `$VARDA_HOME/operations/tasks/`: where Varda stores markdown task files, grouped into one folder per project.
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

To use a custom Copilot launcher for one folder, define a separate agent and route that folder to it:

```toml
[[routes]]
glob = "/some/project/path/**"
agents = ["project-copilot"]

[agents.project-copilot]
kind = "acp"
command = "/path/to/copilot-wrapper"
args = ["-"]
working_dir = "{project}"

[agents.project-copilot.env]
COPILOT_CUSTOM_VALUE = "enabled"
```

`command`, `args`, `working_dir`, and environment variable values support `{project}` and `{task}` placeholders.

Agents can also declare an optional prompt budget:

```toml
[agents.small-context-agent]
kind = "acp"
command = "small-agent"
args = ["-"]
max_prompt_tokens = 32000
```

Before Varda allocates a task, it estimates the full agent prompt, including project instructions and task plan content. If the default agent is over budget, Varda uses the first allowed agent with enough budget. If an explicitly assigned agent is over budget, Varda stops and reports which allowed agents can fit the task.

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

If the agent the route resolves to has a `resume_command_template` set in `config.toml`, Varda also captures the agent's own session id from its on-disk session storage (claude -> `~/.claude/projects/...`, codex -> `~/.codex/sessions/...`, copilot -> `~/.copilot/session-state/...`) and stores a ready-to-run resume command on the task in the `agent_resume_commands:` frontmatter list. A later `varda task resume` can offer to use that command instead of starting over.

`make install` also installs three convenience wrappers next to the `varda` binary:

```sh
vclaude  "Task name" "Optional task description"
vcodex   "Task name" "Optional task description"
vcopilot "Task name" "Optional task description"
```

Each one is a thin shell alias for `varda task add --agent <agent> --exec --interactive`, so the call above is equivalent to:

```sh
varda task add --agent claude --exec --interactive "Task name" "Optional task description"
```

Use them when you want to start an interactive session with a specific agent without typing the full flag list.

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
5. If `agent_resume_commands` contains a non-empty command, offers to resume the latest captured agent session.
6. If accepted, runs the captured command interactively and then runs the interpreter pass to produce the standard Varda recap.
7. If declined, if no captured command exists, or if `--fresh` is passed, starts a new agent session with `varda task run`.

Use `--fresh` to skip any captured command and start over:

```sh
varda task resume 1 --fresh
```

If you set `VARDA_HOME`, use that path instead of `$HOME/.varda`.

## Resume A Past Session

To move a past task back to `ready` without running it immediately, choose one of its recorded sessions:

```sh
varda task resume-session 1
```

Varda scans `operations/runs/*.log` for sessions tied to that task, prompts for the session to resume, stores the selected `agent_session_id` and `agent_session_log` in task frontmatter, sets `status: ready`, and clears `requires_user`.

Frontmatter precedence:

- `agent_resume_commands` is the only field `varda task resume` can execute directly. When multiple commands exist, Varda offers the latest non-empty entry.
- `agent_session_ids` and `agent_session_logs` remain the Varda run history and inspection trail. They are not enough by themselves to resume an agent-owned interactive session.
- `varda task resume-session` uses historical Varda run logs to attach a past `agent_session_id` and `agent_session_log` to a task, but it does not create an agent resume command.

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

Varda writes these fields before launching the agent, so an interrupted runner still leaves a resumable run pointer on the task. For Claude Code runs, Varda also records the discovered Claude transcript as `external_session_id` and `external_session_log` inside the session log when it can match the generated Claude JSONL file. While the agent runs, stdout and stderr are streamed into the session log instead of being buffered until process exit. If the agent process fails or times out, the synthetic failure recap includes the session ID and a link to that log file. Timeout recaps also ask for the unfinished work to be delegated to a Varda long-running runner task and record `long_running_task_requested=true` in the session log.

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
args = ["exec", "--cd", ".", "--add-dir", "{varda_project}", "--add-dir", "{varda_home}", "--sandbox", "workspace-write", "-"]
interactive_command = "sh"
interactive_args = ["-c", "codex \"$(cat $VARDA_PROMPT_FILE)\" -C {project} --add-dir {varda_project} --add-dir {varda_home} -s workspace-write"]
resume_command_template = "codex resume -C {project} --add-dir {varda_project} --add-dir {varda_home} -s workspace-write {external_session_id}"

[agents.claude]
kind = "acp"
command = "claude"
args = ["-p", "--permission-mode", "acceptEdits", "--add-dir", "{project}", "--add-dir", "{varda_project}", "--add-dir", "{varda_home}"]
interactive_command = "sh"
interactive_args = ["-c", "claude \"$(cat $VARDA_PROMPT_FILE)\" --add-dir {project} --add-dir {varda_project} --add-dir {varda_home} --permission-mode acceptEdits"]
resume_command_template = "claude --resume {external_session_id} --add-dir {project} --add-dir {varda_project} --add-dir {varda_home} --permission-mode acceptEdits"

[agents.copilot]
kind = "acp"
command = "sh"
args = ["-c", "copilot -p \"$(cat)\" --allow-all-tools --add-dir {project} --add-dir {varda_project} --add-dir {varda_home} -s"]
resume_command_template = "copilot --resume={external_session_id} --add-dir {project} --add-dir {varda_project} --add-dir {varda_home} --allow-all-tools"

[roles.tester]
backend = "codex"
instructions = """
You are the tester agent. Your role is to verify an implementation...
"""

[git]
auto_commit = true
```

Run `varda config edit` to open the global config in `$EDITOR`. If `EDITOR` is not set, Varda falls back to `vi`.

For now, `kind = "acp"` means Varda uses its ACP-facing agent abstraction. The concrete POC adapter drives the local Codex CLI with `codex exec` through stdin/stdout because this machine's Codex CLI does not expose a direct `--acp` flag.

When the generated Codex args contain `--cd "."`, Varda replaces that `.` at runtime with the task's `project` path. That is what makes the tracked project writable to Codex under `--sandbox workspace-write`, even though the task file itself lives in the global Varda control-plane folder. Varda also expands `{varda_project}` to the Varda source project directory and `{varda_home}` to the Varda control-plane directory, then passes both as additional writable directories to the default Codex, Claude, and Copilot launch commands, so interpreter and resume sessions can create follow-up Varda tasks after the current task finishes.

### Sandbox providers

Varda can run an agent inside an isolated sandbox instead of directly on your machine:

```toml
[defaults]
sandbox = "local"          # default provider for routes that don't override it

[[routes]]
glob = "/work/project/**"
agents = ["codex"]
sandbox = "docker"         # per-route override

[sandboxes.docker]
image = "varda:latest"        # required for the docker provider
egress = ["api.example.com"]  # optional egress allow-list (default-deny)
mounts = ["/opt/cache"]       # optional extra read-only host mounts
```

The effective provider for a route resolves as `route.sandbox` → `defaults.sandbox` → `"local"`. All keys are optional, so existing `config.toml` files parse and re-serialize unchanged. These keys are orthogonal to Codex's own `--sandbox workspace-write` CLI flag in the agent `args`.

**`local`** (the default) runs the agent exactly as before — no isolation.

**`docker`** wraps the agent invocation in `docker run --rm --init -i` and executes it inside the named `image`. The container's environment is built solely from the agent's configured `env` (passed as `-e K=V`); the host environment is not inherited.

- **Project-only mounts.** Only the task's **project directory** is bind-mounted, at the same absolute path, so host secrets outside the project (e.g. `~/.aws`) are not reachable from inside the container. Any paths listed under `mounts` are added as **read-only** bind mounts; nothing else on the host is visible.
- **Default-deny egress with an allow-list.** With no `egress` hosts the container gets `--network none` — it is fully offline. Declaring `egress` hosts attaches the container to the bridge network, disables ambient DNS (`--dns 0.0.0.0`), and pins **only** the allow-listed hostnames to their host-resolved IPs via `--add-host`. A non-allow-listed hostname cannot resolve and is therefore unreachable, while allow-listed hosts stay reachable. (This is a name-resolution allow-list; IP-level firewalling of raw egress is a later milestone.)
- **Resume-capture without exposing `$HOME`.** The container's `HOME` is set to a dedicated per-session host directory (`~/.varda/sessions/{session_id}`) that is bind-mounted read-write — the host's real `$HOME` is never mounted, so credentials stay out of the container. Because the agent writes its own session store (claude/copilot/codex) under that HOME, Varda reads it back from the host after the run and produces a working `resume_command`.

Current limitations:

- **Non-interactive runs only.** Starting an interactive or resume session under a non-`local` sandbox returns a clear error (interactive-under-sandbox is a later milestone).

### Roles

Roles are prompt personas that layer on top of an agent backend. They let you assign a different behavioral mode (e.g. verification, planning, review) without changing the underlying executable.

The default config ships with a `tester` role that runs on the `codex` backend:

```toml
[roles.tester]
backend = "codex"
instructions = """..."""
```

Add a role to a project route to allow it as an assignee:

```sh
varda project add "/some/project/path/**" --agents codex,tester
```

Assign a task to `tester` after implementation when you want the agent to define and execute a test plan, decide whether the task is complete, and record failed checks plus suggested follow-up when verification does not pass.

To define a custom role, add a `[roles.<name>]` entry to your config with a `backend` that names an existing agent and an optional `instructions` string. No code changes are needed — Varda injects the instructions as a `## Role` section in the agent prompt at run time.

The generated Claude Code args use `-p` for non-interactive output through stdin/stdout. Varda expands `{project}` in `--add-dir "{project}"` so Claude can access the tracked project while the task file remains in the global Varda control-plane folder. The default Claude and Copilot configs also receive `{varda_project}` and `{varda_home}` as extra `--add-dir` entries.

The generated GitHub Copilot args use `gh copilot suggest -t shell -` to drive the GitHub CLI copilot extension through stdin/stdout. The `-` at the end signals stdin input mode. You must have the `gh` CLI installed with the copilot extension (`gh extension install github/gh-copilot`).

Agent configs may also set `working_dir = "{project}"` and an `[agents.<name>.env]` table. This is useful for per-project wrapper scripts, including Copilot launchers that need custom environment variables.

## Git Behavior

When `auto_commit = true`, Varda commits after each processed task.

For a normal task, the commit includes:

- the updated task markdown file
- the generated recap file
- the generated agent session log

For a task that needs user input, the commit also includes:

- a notification JSON file under the global `operations/runs/` folder

Agents must NOT run `git add` or `git commit` themselves during normal non-interactive runs. Instead, every agent recap must include a `Files touched` heading listing one absolute path per line (or `(none)`). Varda parses that section and, before committing the operations metadata, stages and commits exactly those paths in the project repo (which may differ from the operations repo). This avoids interactive git prompts inside the agent process and keeps the commit boundary under Varda's control. Paths outside the project's git repo are skipped with a warning.

Interactive runs use the same Varda-owned commit flow by default. The exception is an explicit user request: when the task text or live interactive conversation asks the agent to commit, the interactive agent may stage and commit only its own changes. Varda still runs the interpretation pass afterward; if the reported project files are already committed, Varda's project-file commit step has nothing further to commit.

When running on macOS, Varda also sends a best-effort native notification signal for tasks that need user input. Signal delivery failures are reported to stderr but do not prevent the notification JSON from being written.

### Pre-run dirty-tree check

Before launching an agent, Varda runs `git status --porcelain` against the project repository declared in the task's `project` frontmatter. If anything is reported (modified, staged, or untracked), Varda:

- skips the agent invocation entirely,
- writes a recap explaining the conflict and listing the offending entries,
- sets the task to `needs_user` and fires the macOS notification,
- leaves the user's working tree untouched.

This protects in-progress local work from being entangled with agent edits and keeps the post-run commit unambiguous. Once the listed entries are committed, stashed, or discarded, set the task back to `ready` and re-run it. The check is silently skipped when the project path is missing or not inside a git repository.

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
- Notification is file-backed JSON plus terminal output, with a best-effort macOS native signal for tasks that need user input.
- Task handoff to another agent is represented by `pending` plus recap metadata, but automatic reassignment is not implemented yet.
