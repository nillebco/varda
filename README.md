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
20. Varda updates the original task to `review`, `needs_user`, or `failed`.
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

### Supported agents

`varda init` ships default `[agents.*]` blocks for four agents; add any of them to a route's `--agents` list to use them:

| Agent | Backend | Permission model | Resume |
|---|---|---|---|
| `codex`    | OpenAI Codex CLI    | `--sandbox workspace-write` | yes (`codex resume`) |
| `claude`   | Claude Code CLI     | `--permission-mode acceptEdits` | yes (`--resume`) |
| `copilot`  | GitHub Copilot CLI  | `--allow-all-tools` | yes (`--resume=`) |
| `opencode` | [opencode](https://opencode.ai) | `--auto` (auto-approve) | no (see below) |

`opencode` reads `AGENTS.md` natively as project instructions. Its default headless launcher pipes the prompt on stdin and runs `opencode run --auto --dir {project} "$(cat)"`; interactive sessions use `opencode run -i`. Two current limitations: opencode supports a single working directory (`--dir`, no `--add-dir` equivalent), and it stores sessions in a SQLite database rather than per-session files, so Varda cannot discover its session id for automatic resume — resume manually with `opencode run --continue` or `opencode session`.

Agents can also declare an optional prompt budget:

```toml
[agents.small-context-agent]
kind = "acp"
command = "small-agent"
args = ["-"]
max_prompt_tokens = 32000
```

Before Varda allocates a task, it estimates the full agent prompt, including project instructions and task plan content. If the default agent is over budget, Varda uses the first allowed agent with enough budget. If an explicitly assigned agent is over budget, Varda stops and reports which allowed agents can fit the task.

Agents that stream stdout/stderr while working can set `streams_output = true`. Leave it unset, or set it to `false`, for buffered agents such as print-mode CLIs that only write output when the process exits; Varda will then rely on process exit plus `max_seconds` instead of killing solely because the session log is quiet.

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

#### Pinning a sandbox (`--sandbox`)

By default the sandbox for a task is resolved from the nearest `.varda`, the
matched route, and `defaults.sandbox` (see [Sandbox providers](#sandbox-providers)).
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

### Repo-local task store (`.varda/`)

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

### Migrating legacy `pending` task files

`review` was previously called `pending`. Reading a legacy `status: pending` task file still works (it loads as `review`), and `varda task update --set-status pending …` is accepted as an alias for `review`. To rewrite existing control-plane state files in place, run:

```bash
varda task migrate-status
```

It rewrites every `status: pending` file under the configured operations task directory to `status: review`, preserves all other frontmatter and the task body, is idempotent, and reports how many files were changed.

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

## Execution bounds (cooperative, not a hard kill)

Older Varda wrapped each non-interactive run in a single `timeout_seconds` (600 s)
wall-clock limit and hard-killed the child on expiry — losing uncommitted partial
work and, under a sandbox, leaking containers/volumes. Varda is moving to a
**cooperative bounds** model that never kills a productive session mid-work:

- **Idle watchdog** (`idle_timeout_seconds`, default 180) — cancels a session only
  after that many seconds of *total silence* (no stdout/stderr activity). Productive
  long runs never trip it; a wedged or hung child does.
- **Auto-resume loop** (`max_continuations`, default 0) — off by default. When
  explicitly enabled for an agent/workflow that signals "done" by omitting a
  resume command, Varda captures the agent's resume command and dispatches a
  **fresh continuation session** (new session id + log each hop), seeded with that
  command. Each hop's recap is preserved in order and stitched into the final
  recap. Reaching `max_continuations` with work still remaining stops gracefully as
  `needs_user` (never an infinite loop, never a silently dropped tail). Do not
  enable this for agents that emit a resume command on every successful completion
  unless they have a separate completion signal.
- **Operation budget** (`max_seconds`) — soft ceiling tracked across the whole
  task. `max_seconds` accepts an integer or `"none"`. A completed agent result is
  authoritative even when sandbox cleanup is still running: Varda settles the task
  first and preserves its `requires_user` marker and `Files touched`.
  If the ceiling arrives before any complete recap, the run **stops and marks the
  task `needs_user`** with a synthesized budget recap — a graceful checkpoint,
  never a kill.
- **Reserved tool-call budget** (`max_tool_calls`) — parsed from config/frontmatter
  for forward compatibility, but not enforced yet because the current agent stream
  does not expose a reliable per-run tool-call count. Non-zero values print a
  warning and are otherwise ignored.

`timeout_seconds` remains as a **deprecated alias**: when `max_seconds` is unset it
supplies the soft ceiling, so existing configs behave unchanged.

The idle watchdog observes the session log's growth as its activity signal: the acp
streaming path appends every stdout/stderr chunk to the log, so a growing log is a
direct proxy for a productive run and a stalled log is a wedged child. Reaching the
soft `max_seconds` ceiling without a complete recap stops the run gracefully as
`needs_user`; a silent stall past `idle_timeout_seconds` cancels the wedged session
and suggests a long-running runner follow-up.

**Per-task overrides.** Any of the four bounds can be overridden per task via its
frontmatter — the keys `idle_timeout`, `max_seconds`, `max_continuations`, and
`max_tool_calls` sit at the top level of the task's YAML and win over the matching
`defaults.*` config value (an unset key falls back to the default). For example, a
task that legitimately needs a longer quiet window:

```yaml
---
status: ready
project: /work/project
assignee: claude
idle_timeout: 600          # override defaults.idle_timeout_seconds for this task
max_continuations: 2       # cap auto-resume hops for this task
---
```

**Sandbox teardown on cancel.** A watchdog/budget cancel drops the in-flight agent
future to stop the child (`kill_on_drop`). Because Rust has no async `Drop`, the acp
run owns the sandbox session in a guard (`SessionTeardownGuard`) that runs
`session.teardown()` on **every** exit path: inline on a natural end, and detached
onto the runtime on a cancel — so an idle/budget kill of a sandboxed run no longer
leaks its `varda-sbx-*` container/volume.

## Configuration

The default global config looks like this:

```toml
[defaults]
timeout_seconds = 600      # DEPRECATED alias for max_seconds (see below)
operations_dir = "operations"
idle_timeout_seconds = 180 # cancel a session only after this many seconds of total silence
max_seconds = "none"       # soft total ceiling across all continuations; "none" = no ceiling
max_continuations = 0      # auto-resume hops; 0 = OFF
max_tool_calls = 0         # reserved; non-zero warns but is not enforced yet

[[routes]]
glob = "**"
agents = ["codex"]

[agents.codex]
kind = "acp"
command = "codex"
args = ["exec", "--cd", ".", "--add-dir", "{varda_project}", "--add-dir", "{varda_home}", "--sandbox", "workspace-write", "-"]
streams_output = true
interactive_command = "sh"
interactive_args = ["-c", "codex \"$(cat $VARDA_PROMPT_FILE)\" -C {project} --add-dir {varda_project} --add-dir {varda_home} -s workspace-write"]
resume_command_template = "codex resume -C {project} --add-dir {varda_project} --add-dir {varda_home} -s workspace-write {external_session_id}"

[agents.claude]
kind = "acp"
command = "claude"
args = ["-p", "--permission-mode", "acceptEdits", "--add-dir", "{project}", "--add-dir", "{varda_project}", "--add-dir", "{varda_home}"]
interactive_command = "sh"
interactive_args = ["-c", "claude \"$(cat $VARDA_PROMPT_FILE)\" --add-dir {project} --add-dir {varda_project} --add-dir {varda_home} --permission-mode acceptEdits"]
resume_command_template = "claude --resume {external_session_id} --add-dir {project} --add-dir {varda_project} --add-dir {varda_home} --permission-mode acceptEdits \"{prompt}\""

[agents.copilot]
kind = "acp"
command = "sh"
args = ["-c", "copilot -p \"$(cat)\" --allow-all-tools --add-dir {project} --add-dir {varda_project} --add-dir {varda_home} -s"]
interactive_command = "sh"
interactive_args = ["-c", "copilot \"$(cat $VARDA_PROMPT_FILE)\" --allow-all-tools --add-dir {project} --add-dir {varda_project} --add-dir {varda_home}"]
resume_command_template = "copilot --resume={external_session_id} --add-dir {project} --add-dir {varda_project} --add-dir {varda_home} --allow-all-tools"

[agents.shell]
kind = "acp"
command = "sh"
args = ["-c", "cat"]
interactive_command = "sh"
interactive_args = ["-i"]
skip_recap = true  # bare interactive shell (vmsbsh/vdocksh): no Varda recap to produce, so skip the interpreter pass

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

### Configuration reference

These are the main config knobs that shape execution. The shipped default config keeps optional examples commented so copy-pasting a stanza is explicit and does not change behavior during `varda init`.

| Knob | One-line example |
|---|---|
| Interactive interpreter agent | `interpreter_agent = "codex"` on `[agents.claude]` makes Codex produce the post-interactive Varda recap. |
| Skip interpreter/recap pass | `skip_recap = true` on `[agents.shell]` (used by `vmsbsh`/`vdocksh`) skips the post-interactive interpreter pass entirely — no agent is invoked to interpret a bare shell session. |
| Agent output streaming | `streams_output = true` is set for Codex; leave it unset or `false` for buffered print-mode agents. |
| Agent static env | `[agents.claude.env] STATIC_TOOL_VALUE = "enabled"` injects a non-secret agent-specific value. |
| Per-route env | `env = { GCLOUD_PROJECT = "example-project" }` on `[[routes]]` injects trusted project constants. |
| Sandbox env | `[sandboxes.dev.env] TOOLCHAIN_HOME = "/opt/toolchain"` injects image-intrinsic, non-secret constants. |
| Devcontainer image source | `image_from = "devcontainer"` on `[sandboxes.dev]` uses only the project's `.devcontainer` image/build definition. |
| Dockerfile build image | `build = "./Dockerfile.varda"` on `[sandboxes.custom]` builds the sandbox image at prepare time. |
| Credential from env | `[[agents.claude.credentials]] from_env = "CLAUDE_SANDBOX_TOKEN"; env = "ANTHROPIC_API_KEY"` copies a host env value into a scoped in-box env var. |
| Credential from secret | `from_secret = "tfc-token"; env = "TF_TOKEN_app_terraform_io"` resolves a named host secret; the config stores the name only. |
| Credential from command | `command = "az account get-access-token --query accessToken -o tsv"; env = "AZURE_TOKEN"` mints a short-lived value on the host. |
| Credential file target | `from_secret = "gcp-service-account-json"; file = "/home/agent/.config/gcloud/application_default_credentials.json"` stages one read-only file. |
| GCP deploy recipe | `command = "gcloud auth print-access-token --impersonate-service-account=deployer@example-project.iam.gserviceaccount.com"; env = "CLOUDSDK_AUTH_ACCESS_TOKEN"` lets the box run `gcloud run deploy SERVICE --source . --project "$GCLOUD_PROJECT"` without mounting `~/.config/gcloud`. |
| Terraform Cloud recipe | `from_secret = "tfc-token"; env = "TF_TOKEN_app_terraform_io"` exposes only the Terraform Cloud token value. |
| Azure DevOps recipe | `from_secret = "azdo-pat"; env = "AZURE_DEVOPS_EXT_PAT"` exposes only the PAT value. |
| Azure CLI recipe | `from_secret = "azure-client-id"; env = "AZURE_CLIENT_ID"` plus client secret and tenant secret env entries supports service-principal auth without `~/.azure`. |
| Orchestration defaults | `[orchestration] enabled = true; max_depth = 2; max_fanout = 4; global_child_budget = 16; deny_sandboxes = ["local"]` gates sandboxed subtask spawning. |
| Route orchestration override | `[routes.orchestration] enabled = false` disables spawning for that matched route. |
| Repo-local tasks and config | `.varda/tasks/<id>-<slug>.md` stores committed task definitions; `.varda/config.toml` can carry repo workflow rules while runtime state remains in `$VARDA_HOME`. |

Credential entries must name exactly one source (`from_env`, `from_secret`, `from_fnox`, or `command`) and one target (`env` or `file`). Store secret names, not resolved secret values, in config.

### Sandbox providers

Varda can run an agent inside an isolated sandbox instead of directly on your machine:

```toml
[defaults]
sandbox = "local"          # default provider for routes that don't override it

[[routes]]
glob = "/work/project/**"
agents = ["codex"]
sandbox = "docker"         # per-route override
env = { GCLOUD_PROJECT = "healthy-silo-31898" }  # per-project static, non-secret env

[sandboxes.docker]
image = "varda:latest"        # image/rootfs, or use `build` below instead
primitive = "docker"          # isolation kind; defaults to "docker" when omitted
egress = ["api.example.com"]  # optional egress allow-list (default-deny)
# egress_mode = "strict"      # docker: strict/proxy = forward-proxy sidecar; "dns-pin" = legacy
# egress_proxy_image = "vimagick/tinyproxy"  # override the forward-proxy image (proxy mode)
mounts = ["/opt/cache"]       # optional extra read-only host mounts
env = { TOOLCHAIN_HOME = "/opt/toolchain" }  # image-intrinsic static env

[sandboxes.rustvm]
build = "./testdata/Dockerfile.rust"  # build the image from a Dockerfile at prepare()

[sandboxes.devc]
image_from = "devcontainer"   # take the image from the project's .devcontainer/
```

The effective provider for a route resolves as `route.sandbox` → `defaults.sandbox` → `"local"`. All keys are optional, so existing `config.toml` files parse and re-serialize unchanged. These keys are orthogonal to Codex's own `--sandbox workspace-write` CLI flag in the agent `args`.

Two knobs are deliberately separate. **`image`/`build`** decides *what tools are installed* (the OCI image or rootfs); **`primitive`** decides *what kind of boundary* runs it. The same image can run under `docker` (shared kernel) or an own-kernel microVM, so nested projects can pick both independently.

- **`image`** names a pre-existing image tag/reference.
- **`build`** points at a Dockerfile; the docker provider builds it at run time and uses the resulting content-addressed tag (an unchanged Dockerfile reuses the cached image). When both are set, `build` wins.
- **`image_from = "devcontainer"`** sources the image from the project's own [`.devcontainer/devcontainer.json`](https://containers.dev/) (falling back to a top-level `.devcontainer.json`) so Varda doesn't duplicate the environment definition. At `prepare()` it reads the devcontainer's `image` (used verbatim) or its `build.dockerfile` + `build.context` (built via docker, content-addressed like `build`), resolved relative to the `.devcontainer/` dir. The JSONC form (comments, trailing commas) is tolerated. `image_from` takes precedence over `image`/`build` when several are set.
  - **Isolation invariant (image source only).** A `devcontainer.json` may declare host `mounts` (`~/.aws`/SSH), docker-socket forwarding, access-widening `runArgs`, or lifecycle hooks (`postCreateCommand`, …). Varda takes **only the image/build** and **nothing else** — those fields are not even deserialized, so they can never leak into the run. Varda keeps sole control of mounts, egress, and credentials (the same project-only-mount, default-deny-egress, no-`$HOME` hardening as any other docker sandbox). A devcontainer that tries to bind-mount host `$HOME` does **not** make the host `$HOME`/`~/.aws` visible in the container.
- **`primitive`** is one of `"docker"` (default), `"local"`, `"microsandbox"`, or `"clawk"`. `microsandbox` is an own-kernel microVM runtime backed by the `msb` CLI (see below); `clawk` uses the `clawk` CLI and fails fast with `sandbox primitive 'clawk' requires the clawk CLI on PATH` when it is not installed.

**`local`** (the default provider name) runs the agent exactly as before — no isolation. Setting `primitive = "local"` on a named sandbox does the same, even if an `image` is present.

**`docker`** wraps the agent invocation in `docker run --rm --init -i` and executes it inside the resolved `image` (or the image built from `build`). The container's environment is built solely from the agent's configured `env` (passed as `-e K=V`); the host environment is not inherited.

- **Project-only mounts, plus opt-in context mounts.** Only the task's **project directory** is bind-mounted by default, at the same absolute path, so host secrets outside the project (e.g. `~/.aws`) are not reachable. Extra mounts may be declared at two trusted origins that **merge** (union, de-duplicated by target): `[sandboxes.X].mounts` (image-intrinsic, same for every project using that image) and `Route.mounts` (project context, e.g. a route for `**/dev/AsianDevBank/**` also mounting `~/dev/brain/AsianDevBank:ro`). Extra mounts are **read-only by default**.
- **Static env maps.** Non-secret static values may be declared at `[agents.X].env`, `[sandboxes.X].env`, `[[routes]].env`, and inline `.varda` `[sandbox].env`. They merge as `agent.env` → `sandbox.env` → `route.env` → `.varda` env, so the more-specific origin wins. Values support the same `{project}` and `~` expansion as agent env and mount paths. Use this for project constants such as `GCLOUD_PROJECT`; secrets and tokens belong in `auth_token_env`/credential injection, not static env.
- **Mount grammar (`source:target:mode`, docker-style).** `SOURCE` (target = same absolute path, `:ro`) · `SOURCE:ro|:w` · `SOURCE:TARGET` (absolute TARGET, `:ro`) · `SOURCE:TARGET:ro|:w`; a TOML table form `{ source, target, mode }` is also accepted. `~` and `{project}` expand; relative sources resolve against the project root.
- **Host mount visibility (VM-backed docker).** With a VM-backed daemon (Colima/Lima/Docker Desktop) only paths the VM actually shares are visible; a bind-mount whose **source is outside the VM's shared tree binds as an empty stub** (docker creates the mount point inside the VM). Keep mount sources — including the project and any context dirs — under a VM-mounted root (e.g. Colima's configured mount). See "Resume-capture" for how this affects the session store.
- **Egress modes are explicit.** With no `egress` hosts the sandbox gets `--network none` and is fully offline; this is strict/offline for every provider. For a non-empty Docker allow-list, `egress_mode` picks the enforcement:
  - **`strict` (default) / `proxy` — allow-listing forward-proxy sidecar.** Varda stands up a per-session **internal** docker network (no route to the internet) plus a small forward-proxy container that is dual-homed onto that internal network *and* the bridge. The sandbox joins only the internal network and receives `HTTP_PROXY`/`HTTPS_PROXY` pointing at the proxy (`http://egress-proxy:8888`); the proxy default-denies and forwards HTTP(S) CONNECT to the allow-listed hosts **only**. This is **real enforcement** — a denied host is genuinely unroutable, not just DNS-broken — and it works with apps that do their own DNS resolution (claude-code, codex), which the DNS-pin mode breaks. It needs no `NET_ADMIN`. Trade-off: it covers **proxy-aware HTTP(S)** traffic (the agent + git-over-https + npm/pip/cargo registries); raw non-proxy TCP is not forwarded. The proxy image is `vimagick/tinyproxy` by default and is overridable per sandbox via `egress_proxy_image` (any image running a tinyproxy-compatible proxy that reads `/etc/varda-proxy/tinyproxy.conf`).
  - **`dns-pin` — legacy name-pin (compat).** Varda attaches the container to the bridge network, disables ambient DNS (`--dns 0.0.0.0`), and pins only the allow-listed hostnames via `--add-host`. It blocks non-allow-listed **hostnames**, but an agent that already knows an IP can still make direct-IP connections, and apps doing their own DNS (bypassing `/etc/hosts`) break. Opt into it explicitly only for the legacy worker behavior; it is **not** clawk/microsandbox-equivalent firewalling.
- **Resident egress is stricter than worker egress.** `varda orchestrate` validates the long-lived resident against an agent-specific exact host inventory and a strict egress provider: Claude may use `api.anthropic.com` and `platform.claude.com` (the latter is a hard startup-connectivity requirement from Claude Code v2.x on); Codex/OpenAI may use `api.openai.com`, `chatgpt.com`, and `auth.openai.com`; Copilot resident mode currently fails closed until exact non-push Copilot auth/API endpoints are known. Do not add blanket `github.com` to resident egress. Ordinary worker sandboxes may still opt into broader route/user-approved egress where the workflow explicitly permits it.
- **Resume-capture without exposing `$HOME` (per-session volume + `docker cp`).** The container's `HOME` is a dedicated **per-session docker named volume** (not a host bind mount) — the host's real `$HOME` is never mounted, so credentials stay out. The agent writes its session store (claude/copilot/codex) under that HOME; after the run Varda `docker cp`s the store out of the container to a host directory (`~/.varda/sessions/{session_id}`) and reads it back to produce a working `resume_command`. Because the volume lives in daemon storage and `docker cp` streams through the daemon to the host, this works on **any** backend — including a VM-backed daemon whose share excludes `~/.varda` (e.g. a Colima profile mounting only `~/dev`), where a host bind of the session dir would silently bind an empty in-VM stub. The container drops `--rm` so it outlives its process long enough for the copy, then teardown removes both container and volume.
- **Fail-loud mounts.** A declared bind-mount whose host source does not exist is rejected with a clear error rather than silently mounting an empty stub (on a VM-backed daemon docker would otherwise create an empty in-VM mount point that *looks* successful).

**`microsandbox`** shells to the `msb` CLI (install with the microsandbox project; expects `msb` on `PATH`) and runs the agent inside an **own-kernel microVM** — a stronger inward boundary than docker's shared kernel, plus Windows coverage. It mirrors the docker provider: the same `image`/`build` inputs (a `build` Dockerfile is built via docker into a tag `msb` runs), the same project-only + opt-in merged mounts (`msb --mount HOST:GUEST`, read-only by default), the same resume-capture model (the guest `HOME` lives in VM storage and is `msb cp`-ed out to `~/.varda/sessions/{session_id}` after the run, so the host `$HOME`/credentials are never exposed), and default-deny egress (fully offline with no `egress`; `egress` hosts become per-host `msb` net allow-rules — enforced in-guest, so hostnames/CIDRs are passed directly rather than pre-resolved to IPs as docker requires). Because msb 0.6.x has no env-file option, Varda stages `env`-target credential values in a private read-only file with `--copy-file` and imports them inside the guest; secret values never appear in the ps-visible `msb run` argv. Ordinary non-secret environment settings continue to use `--env`. The keys never enter the VM, and an OCI image can bake in the agent CLI (e.g. the copilot CLI for the Windows path). *The `msb` argv spellings are centralized in `MicrosandboxSession::wrap`/`extract_session_store`; confirm them against your installed `msb --help` — see the M4 task notes on live verification.*

**`clawk`** shells to the `clawk` CLI (expects `clawk` on `PATH`) and runs the agent in a disposable Linux microVM. Varda mounts the task project read-write at the same absolute path, applies extra directory mounts from the normal merged mount grammar as read-only unless `:rw`/`:w` is declared, and refuses unsupported file-level bind mounts loudly. Curated identity files and staged credential values still use the existing Varda identity channels, so host credential directories are never mounted. Network policy starts default-deny: an empty `egress` keeps the VM offline; configured hosts are applied before launch through `clawk network allow <sandbox> <host>`. The guest `HOME` is copied back with `clawk cp` after the run into `~/.varda/sessions/{session_id}`, so resume capture is post-run only (`store_is_live = false`) rather than live-polled. Normal unit tests cover clawk command construction without requiring the runtime; live clawk smoke coverage is `#[ignore]` and should be run only on machines with clawk installed. *The clawk argv spellings are centralized in `ClawkSession::wrap`/`extract_session_store`; confirm them against your installed `clawk --help` before relying on a new clawk release.*

#### Interactive sandbox (real agents): TTY, prompt staging, injected auth, and the docker lifecycle

`--interactive` runs now put the **real coding agents** (`claude`, `codex`, `copilot`) — not just a bare shell — inside `docker` and `microsandbox`, not just `local`. Each default agent ships an `interactive_command`/`interactive_args` that launches the agent through a login shell reading the staged prompt (`sh -c '<agent> "$(cat $VARDA_PROMPT_FILE)" …'`); when the resolved sandbox is not `local`, Varda attaches **your terminal** to that agent *inside the box*. The project is mounted, but `~/.aws`/`~/.ssh`/`~/.claude`/etc. stay invisible (the same isolation as a batch run), and teardown removes the container/microVM and its volume on every exit path.

- **The agent authenticates via injected identity, not a creds-dir mount.** The interactive path reuses the same `SandboxIdentity` channels as a batch run (M11/M11-ext): a scoped, host-minted token injected as an in-box env var (or staged as a read-only `0o400` file), the read-only git identity (`GIT_AUTHOR_*`/`GIT_COMMITTER_*`), and — when `forward_ssh_agent` is set and a live `$SSH_AUTH_SOCK` exists — the host **SSH agent socket forwarded** as a bind (`… :/ssh-agent`) with the in-guest `SSH_AUTH_SOCK` pointing at it, so `git push` signs on the host and **no private key ever enters the box**. `~/.aws`/`~/.ssh` and every credential dir remain unmounted; identity injection is mode-agnostic (the batch and interactive argv carry the exact same channels).
- **A real terminal is required.** A sandboxed interactive launch needs a TTY on stdin (`docker -it` / `msb -t` fail without one). Varda checks `stdin` up front and fails clearly if you pipe input or run headless — use a batch run (no `--interactive`) or `sandbox = "local"` in that case.
- **The prompt is staged *into* the guest.** The task prompt is copied to `/home/agent/.varda-prompt.txt` inside the box and exposed as `$VARDA_PROMPT_FILE`, so the in-guest agent can read the task even though the host temp file is not visible in the guest. (Under `local`, `$VARDA_PROMPT_FILE` points at a host temp, exactly as before.)
- **Docker interactive is a different lifecycle, not just `-it`.** Because the container `HOME` is a per-session named volume and a host bind of `~/.varda` hits the VM-visibility trap, Varda cannot `docker run` an interactive session directly. Instead it:
  1. `docker create … -it …` — create the container (do **not** run it),
  2. `docker cp` the staged prompt into it,
  3. `docker start -ai <container>` — attach *your* TTY,
  4. `docker cp <container>:/home/agent/. <host session store>` — extract the session store after you exit,
  5. `docker rm -f` the container and `docker volume rm -f` its volume.

  `microsandbox` and `clawk` need no such dance: they stage the prompt with native pre-boot copy flags and run the VM command with a TTY. Session capture and the `resume_command` are produced after the run by copying the guest HOME back to the host.
- **Ctrl-C** under `-it` propagates to the guest process; the `SessionTeardownGuard` still fires on the way out, so no `varda-sbx-*` container or volume leaks.
- **The interpretation pass stays local.** After the interactive session ends, Varda's post-session interpretation pass only reads the host session log to produce the recap and the captured `resume_command` (no untrusted exec), so it runs **un-sandboxed** on the host. An optional `interpreter_agent` on the agent config selects which agent runs that pass; when unset it defaults to the same agent that drove the session (a real agent re-reads its own transcript; a bare `sh` shell that can't emit a Varda recap should point `interpreter_agent` at a real agent).

> Resuming an interactive session under a sandbox is not yet supported (the fresh-shell launch is); resume runs remain `local`-only.

#### Per-folder `.varda` (untrusted origin) and the hardening floor

A folder can commit its own sandbox choice in a **`.varda`** file. When resolving the sandbox for a task, Varda walks **up** from the task's project/target path to the routing root and uses the **nearest `.varda`**. Precedence:

```
task-pin (task add --sandbox)  →  nearest .varda  →  central route (glob)  →  defaults.sandbox  →  "local"
```

A task-pinned sandbox (`varda task add --sandbox <NAME>`, persisted as the
`sandbox:` task-frontmatter field) is a trusted operator origin, so it wins over
even the nearest `.varda` and is not subject to the `.varda` hardening floor.

`.varda` is TOML in one of two forms:

```toml
# Reference form — select a central [sandboxes.X]:
sandbox = "rust"
```

```toml
# Inline form — a self-contained sandbox:
[sandbox]
image = "rust:latest"
primitive = "docker"
mounts = ["ctx:/ctx"]        # relative to the .varda dir
egress = ["crates.io"]
env = { GCLOUD_PROJECT = "healthy-silo-31898" }
```

Inline `mounts` join the docker/microsandbox mount merge as a **third origin** (`MountOrigin::Varda`), unioned with the trusted `Sandbox`- and `Route`-origin mounts.

**Hardening floor (security).** Central `config.toml` (routes and `[sandboxes]`) stays **trusted**; a `.varda` is committed alongside possibly-untrusted code, so it is **clamped** — the floor applies **only** to the `.varda` origin (trust-by-origin: the same mount from a `Route` is allowed):

- **No escaping the box.** A `.varda` cannot select `primitive = "local"` unless `defaults.allow_local_varda = true` (default false).
- **In-tree, read-only mounts.** A `.varda` mount SOURCE must resolve **inside the project root** (out-of-tree / host paths are rejected), and is forced `:ro` unless `defaults.allow_varda_writable_mounts = true`.
- **Safe target.** A mount TARGET may not be `/`, a system dir (`/etc`, `/usr`, …), nor collide with / shadow the project mount.
- **Egress ceiling.** If `defaults.egress_ceiling` is set, a `.varda` may not widen egress beyond it.
- **Env key floor.** A `.varda` env map cannot set reserved process/control keys (`PATH`, `HOME`, `LD_PRELOAD`, `LD_LIBRARY_PATH`, `DYLD_*`, `VARDA_*`, `SSH_AUTH_SOCK`) and cannot override trusted agent/route env or a credential injection target.
- On any violation Varda **refuses to run** with an error naming the offending `.varda` and key.

**Credential-directory denylist (ALL origins, trusted config included).** Any mount whose SOURCE resolves (symlinks followed) into a known secret/identity store is refused regardless of origin: `~/.claude`, `~/.codex`, `~/.copilot`, `~/.aws`, `~/.azure`, `~/.terraform.d`, `~/.ssh`, `~/.config/gcloud`, `~/.config/fnox`, `~/.gnupg`, `~/.kube`, `~/.docker`, `~/.netrc`, `~/.git-credentials`. These carry live LLM tokens **and** cross-project history; mounting one defeats the sandbox and leaks other clients' data.

#### Passing identity & auth into the box (three channels)

A sandboxed agent must be authenticated to run the LLM **and** "know who the user is" — but it must get there **without** mounting `~/.claude`/`~/.codex`/`~/.copilot`/`~/.aws`/`~/.ssh` (those carry live tokens + cross-project history; see the credential denylist above). Modeled on how [clawk](https://github.com/) does it, Varda forwards identity through **three separable, opt-in channels**. Guiding principle: **share the minimum**.

**1. Auth-token injection (channel 1).** Instead of mounting a credential dir, inject a **dedicated, scoped, rotatable sandbox token** as an env var so the agent boots already signed in. The token is resolved from a **host env var / secret store at run time — never a raw secret in the repo or the task**. Prefer a purpose-provisioned token (own spend limit, lower privilege) over the user's primary credential.

```toml
[agents.claude]
# Name of a HOST env var holding the scoped sandbox token (resolved from the
# environment / a secret store like fnox — NOT a literal secret here).
auth_token_env = "CLAUDE_SANDBOX_TOKEN"
# In-box env var the agent reads its token from (defaults to auth_token_env).
auth_token_target = "ANTHROPIC_API_KEY"
```

At `prepare`, Varda reads `$CLAUDE_SANDBOX_TOKEN` on the host and re-exports its value into the box as `-e ANTHROPIC_API_KEY=…`. If the host var is unset/empty the run still boots (with a warning) — it just isn't authenticated. The stronger form — the agent never holds the key, LLM calls proxied through a broker — is the **M8 capability-broker** pattern (out of scope here).

**Multiple credentials per agent (`[[agents.X.credentials]]`).** The single `auth_token_env` pair is **one-entry sugar** over a general credential list. Each entry names exactly one **source** (where the scoped value is minted, on the host) and exactly one **target** (how it reaches the box). None of them ever mounts a credential dir — only the resolved, minimal value crosses the boundary.

| | key | meaning |
|---|---|---|
| **source** (one) | `from_env = "NAME"` | value of a HOST env var |
| | `from_secret = "name"` | a named secret from the host store (`fnox get name`) |
| | `from_fnox = "name"` | explicit alias of `from_secret` — same `fnox get name` resolution; prefer it when standardizing on fnox as the store |
| | `command = "…"` | run on the HOST at `prepare`; stdout (newline-trimmed) is the value — for host-minted, least-privilege, short-lived tokens |
| **target** (one) | `env = "IN_BOX_VAR"` | scoped `-e IN_BOX_VAR=…` (default) |
| | `file = "/guest/abs/path"` | the minimal value is staged as a **read-only** (`0o400`) file in the guest — in **both** batch and interactive runs (docker `cp` / msb `--copy-file`), and both the host temp and the guest copy are removed on teardown |

A `from_env`/`from_secret` source that is unset is skipped (the box still boots unauthenticated); a `command` that errors or prints nothing **fails the run loudly** — a broken mint must never silently degrade to an unauthenticated session. `refresh_seconds` is accepted for forward-compat but the value is currently minted **once** at `prepare` (periodic re-mint is a follow-up). Sources belong in the **trusted central `config.toml`** only; a `.varda` may reference a secret *name* (`from_secret`) but never a raw value or a `command`.

Recipes (each injects a scoped value — **no** `~/.config/gcloud`, `~/.azure`, `~/.aws`, `~/.terraform.d` mount):

```toml
# GCP deploy with a host-minted, impersonated, short-lived access token — no local Docker.
# Use with `gcloud run deploy --source` inside the box.
[[agents.claude.credentials]]
command = "gcloud auth print-access-token --impersonate-service-account=deployer@PROJECT.iam"
env = "CLOUDSDK_AUTH_ACCESS_TOKEN"
[[agents.claude.credentials]]
command = "gcloud auth print-access-token --impersonate-service-account=deployer@PROJECT.iam"
env = "GOOGLE_OAUTH_ACCESS_TOKEN"

# Terraform Cloud API token from the host secret store.
[[agents.claude.credentials]]
from_secret = "tfc-token"
env = "TF_TOKEN_app_terraform_io"

# Azure DevOps PAT from the host secret store.
[[agents.claude.credentials]]
from_secret = "azdo-pat"
env = "AZURE_DEVOPS_EXT_PAT"

# Azure CLI — service principal via env (NOT a ~/.azure mount)…
[[agents.claude.credentials]]
from_secret = "azure-client-id"
env = "AZURE_CLIENT_ID"
[[agents.claude.credentials]]
from_secret = "azure-client-secret"
env = "AZURE_CLIENT_SECRET"
[[agents.claude.credentials]]
from_secret = "azure-tenant-id"
env = "AZURE_TENANT_ID"
# …or a host-minted access token instead of the SP secret:
[[agents.claude.credentials]]
command = "az account get-access-token --query accessToken -o tsv"
env = "AZURE_TOKEN"
```

**fnox-bound static env vars.** A plain static env var — in `[agents.X].env`, `[sandboxes.X].env`, or a `[[routes]].env` — can be **bound to a fnox secret** instead of carrying a literal value, using the whole-value sentinel `"${fnox:NAME}"`. fnox lives on the **exterior**, side-by-side with Varda: at `prepare` Varda resolves the secret on the host (`fnox get NAME`) and injects **only the resolved value** into the box. The agent/sandbox never contacts fnox, never sees the sentinel, and `~/.config/fnox` is never mounted — same isolation model as credential injection, extended to the static env maps.

```toml
# The value is resolved from fnox on the host; the box sees only NPM_TOKEN=<value>.
[sandboxes.build.env]
NPM_TOKEN = "${fnox:npm-publish-token}"

[[routes]]
glob = "infra/**"
env = { HCLOUD_TOKEN = "${fnox:hcloud-token}" }
```

Only a **whole-value** `"${fnox:NAME}"` is a binding (never a substring of a larger literal), so the resolved secret is never embedded in — nor logged as part of — a bigger string. A missing/failed/empty fnox resolution **fails the run loudly** (redacted: only the key and secret *name* are surfaced, never the value). A fnox binding declared by an **untrusted `.varda`** is **refused** — repo-committed config must not be able to bind an arbitrary host secret and exfiltrate it through the agent's env; use the trusted central `config.toml` for fnox-bound env.

**2. Git identity via SSH-agent forwarding (channel 2).** Forward the host **SSH agent socket** so `git push` signs/authenticates on the host and **private keys never enter the box** (`ls ~/.ssh` in-guest stays empty). The read-only git identity is forwarded as `GIT_AUTHOR_*`/`GIT_COMMITTER_*` env so commits are attributed correctly without mounting `~/.gitconfig`.

```toml
[defaults]
forward_ssh_agent = true            # bind $SSH_AUTH_SOCK → /ssh-agent, set SSH_AUTH_SOCK
git_user_name  = "Ada Lovelace"
git_user_email = "ada@example.com"
```

(docker binds the socket file directly; on the own-kernel microVM the socket bind is best-effort at directory granularity.)

**3. Curated identity/context (channel 3).** `defaults.identity_context` mounts specific, **read-only FILES** so the agent learns "who the user is" without secrets:

```toml
[defaults]
identity_context = ["~/.claude/CLAUDE.md:/root/CLAUDE.md:ro", "~/profile.md:/root/profile.md:ro"]
```

Files only (never a whole dotdir, never `projects/` transcripts); the credential-**filename** denylist still applies, so a `.credentials.json` can never sneak in even when the file lives inside an otherwise-denylisted dir. All three channels are off by default — nothing is forwarded unless you opt in.

#### Isolation invariants (never violate these)

These rules are what make the sandbox meaningful; they also gate the future nested-orchestration broker:

1. **Never mount the docker socket** (`/var/run/docker.sock`) into an agent container — it is equivalent to host root.
2. **Never mount `~/.varda`** or install the `varda` binary into an agent container — it hands the agent the control plane.
3. **No `--privileged` and no docker-in-docker** for agent containers.
4. **Extra mounts default `:ro`; the host `$HOME` is never mounted** — the session store uses a dedicated per-session volume/dir, not a host `$HOME` bind.

Current limitations:

- **Resume is `local`-only.** Fresh interactive sessions run under `docker`/`microsandbox` (the real `claude`/`codex`/`copilot` agents attach to your TTY inside the box — see [Interactive sandbox](#interactive-sandbox-real-agents-tty-prompt-staging-injected-auth-and-the-docker-lifecycle)). **Resuming** an interactive session under a non-`local` sandbox still returns a clear error and remains `local`-only.
### Per-folder `.varda` overrides

A project subtree can pin its own sandbox by placing a `.varda` file in (or above) the task's project directory. At launch Varda walks up from the project path to the git root and uses the **nearest** `.varda`, with precedence:

```
task-pin (task add --sandbox)  →  nearest .varda  →  central route sandbox  →  defaults.sandbox  →  "local"
```

A `.varda` is TOML in one of two forms:

```toml
# reference a central [sandboxes.X]
sandbox = "rustvm"
```
```toml
# or inline a self-contained sandbox
[sandbox]
image     = "rust:1.95"
primitive = "docker"
mounts    = ["../docs:/docs:ro"]
env       = { GCLOUD_PROJECT = "healthy-silo-31898" }
```

Because a `.varda` is repo-committed (attacker-influenceable on untrusted code), the inline form is clamped by a **hardening floor**: it cannot select `primitive = "local"` (unless `defaults.allow_local_varda`), cannot mount sources outside the project or any credential directory (see the invariants below), cannot target `/` or a system dir or shadow the project mount, cannot widen egress beyond `defaults.egress_ceiling`, and cannot set reserved env keys or override trusted agent/route env or credential injection targets. A floor violation refuses the run with a clear error. Central `config.toml` mounts and env are trusted and skip the `.varda` floor — except the credential denylist, which applies to every mount origin.

### Isolation invariants (never violate)

These rules are what make the sandbox meaningful; breaking any turns it into theatre:

1. **Never mount the docker socket** (`/var/run/docker.sock`) into an agent container — it is equivalent to host root.
2. **Never mount `~/.varda`** (the control-plane root: tasks, config, routes, all sessions) or install the `varda` binary into an agent container — it hands the agent the control plane. A single per-session scratch dir is not the root; the docker/microVM providers use a per-session volume + `cp`, not a host bind of `~/.varda`.
3. **No `--privileged` / no docker-in-docker** for agent containers. Sub-sandboxes are siblings spawned by the host, never nested.
4. **Never mount credential/identity directories** (`~/.claude`, `~/.codex`, `~/.copilot`, `~/.aws`, `~/.ssh`, `~/.config/gcloud`, `~/.config/fnox`, …) to "authenticate the agent" — they hold live tokens and cross-project history. Pass identity via an injected scoped token + SSH-agent forwarding + a curated read-only profile file instead. Enforced by `CREDENTIAL_DENYLIST` / `check_credential_denylist` across **all** mount origins.
5. Extra mounts default to read-only; the host `$HOME` is never mounted.

### Nested orchestration (gated sub-task spawning)

A "master" agent task running inside a sandbox may need to **decompose work** and have sub-agents complete subtasks. It must not gain host access to do so — that would defeat the sandbox. The model is *control-plane orchestrates, agent only requests*:

- The master gets exactly **one** spawn capability across the boundary: an MCP tool `spawn_subtask(brief, route?, sandbox?, agent?) -> {subtask_id}`. To harvest results it then blocks on the **collect channel** — `await_subtask(subtask_id) -> {subtask_id, status}`, `await_subtasks(subtask_ids[]) -> [{subtask_id, status}]` (the wave primitive), and `subtask_result(subtask_id) -> {status, files_touched, blocked_commands, recap}`. Nothing else crosses.
- The same broker also exposes a **task control-plane** surface (task #640) so a sandboxed agent can see and update its project's task board without ever mounting `~/.varda`: `list_tasks(status?) -> [{id, slug, status, title, assignee}]` and `get_task(id) -> {id, slug, status, title, assignee, body}`, both scoped to the caller's own project (a cross-project id is refused, indistinguishable from unknown), and `set_task_status(id, status)` — the write side that closes the status-drift loop (an agent that finishes now marks itself `done`/`needs_user`/`failed` instead of a human running `varda task set-status` by hand later). `set_task_status` is **self-only** (the caller's own task id, resolved from the same host-known lineage the spawn gate uses — never attacker-supplied) and only accepts a `running -> {done, needs_user, failed}` transition; `review -> done` is refused outright since review is a human-only gate.
- Host-side Varda (outside every sandbox) receives the request and, before launching anything:
  - validates it against **policy** — which routes/agents/sandboxes the master may spawn (allow/deny lists);
  - enforces **depth**, **fan-out**, and a **global child budget** so a master cannot recursively fork-bomb;
  - optionally requires **human approval** for a given spawn depth (e.g. the first level);
  - runs each subtask in its **own sibling sandbox** — spawned by the host, never nested/DinD inside the master;
  - returns results back through the broker.
- Even a fully compromised master can only *ask* the broker; it never holds the capability to spawn host processes. The sandbox covers the inward radius; the broker covers the outward radius.

The host-side policy engine and the live broker both live in `src/orchestration.rs`:

- **Policy engine** — `OrchestrationPolicy` + `SpawnLedger::authorize`/`authorize_and_record`. Every cap is a **hard error** (`SpawnDenied`), never a silent truncation. Safe defaults: spawning is **disabled**, and the `local` (no-isolation) sandbox is denied so a spawned subtask cannot escape the box.
- **Broker** — `SpawnBroker` uses shared spawn state containing the ledger plus a **lineage registry** (task id → tree depth), a host `SubtaskLauncher` seam, and a host `SubtaskResults` seam (the collect side). It speaks MCP JSON-RPC (`handle_rpc`): `tools/list` advertises exactly `spawn_subtask`, `await_subtask`, `await_subtasks`, and `subtask_result`, and each `spawn_subtask` call is gated through `authorize_and_record` **before** the host is asked to launch. A denial comes back as an MCP tool error (`isError: true`) carrying the `SpawnDenied` reason. The caller **never supplies its own depth** — the broker looks it up from the lineage registry, so a compromised master cannot claim a shallow depth to dodge the recursion cap, and an unknown caller cannot spawn at all. If the host launch fails after authorization, the ledger is rolled back (`SpawnLedger::unrecord`) so a failed attempt consumes no budget.
- **Collect channel** — `await_subtask*` **block** by polling the `SubtaskResults` seam on a ~1s interval until the child reaches a **terminal** status (`done`/`failed`/`needs_user`/`review`), bounded by an absolute ceiling (30 min) so a wedged-but-still-resolvable child returns a timeout error rather than hanging forever. A subtask id that cannot be RESOLVED at all — unknown id, an ambiguous duplicate, or a failed state load — is a distinct outcome (`SubtaskStatus::Unresolved`) from "still running" and is surfaced as a tool error immediately, without waiting out the ceiling (#653). `subtask_result` resolves the child's recap and parses its `Files touched` / `Blocked commands` sections (the same `parse_files_touched`/`parse_blocked_commands` the runner uses) — it returns no resume command, since resuming is a resident host action, not a worker's. The concrete host `SubtaskResults` (`VardaSubtaskResults`) resolves a subtask id → home STATE via `task::lookup_task_state` and reads the recap file; the resident (un-sandboxed) host reuses the same impl directly. `task::find_task_by_id` (which `lookup_task_state` calls) resolves an id through a persistent id→path index (`operations/tasks/.task_index.json`) instead of rescanning every task file on every poll; a cache miss or stale entry falls back to a full-tree scan that also rebuilds the index, so a missing/corrupt index self-heals rather than staying slow (#653).
- **Transport** — when the effective orchestration policy is enabled for a task, the run path starts a per-session MCP transport (`src/mcp_transport.rs`) that speaks newline-delimited JSON-RPC and dispatches into the live broker; no host process or docker capability is handed to the agent. Spawn authorization holds the shared broker state only while checking and recording policy, then releases it before the synchronous child run starts, so other MCP connections are not blocked on the global broker state for the whole child run. **The transport is selected by the sandbox primitive** (`config::primitive_needs_tcp_broker`):
  - `local`/`docker` (shared kernel) — a per-session **Unix socket** under the mounted project tree (`{project}/.varda-mcp/{session}.sock`), passed to the sandbox as `VARDA_MCP_SOCKET`. The guest reaches it through the bind mount.
  - `microsandbox`/`clawk` (own-kernel microVM) — the project bind mount shares the socket *file* over virtio-fs but **not** its AF_UNIX endpoint, so an in-guest `connect()` is refused. Instead the broker binds a **host TCP** listener on an ephemeral port and advertises it to the guest as `VARDA_MCP_ADDR` (host:port) plus `VARDA_MCP_PORT` (the port alone). The broker BINDS to **host loopback** (`127.0.0.1`) by default — but the guest's own `127.0.0.1` is *not* the host, so the guest-visible connect host is exported separately as **`VARDA_MCP_HOST=host.microsandbox.internal`** (a name msb resolves to the host machine). The guest bridge dials `host.microsandbox.internal:$VARDA_MCP_PORT`. The listener binds a **host-only** interface — loopback by default, overridable via `VARDA_BROKER_BIND_IP` — never `0.0.0.0`; it is ephemeral and torn down with the session, and the broker is capability-gated regardless of reachability, so a reachable port grants no capability the socket did not.
    - **Host access must be allowed.** msb DENIES host access by default, so `MicrosandboxSession::wrap` adds `--net-rule allow@host` (the reserved `host` group = the local trusted orchestrator running the broker) *only* when a broker is wired for the run (the guest env carries `VARDA_MCP_HOST`/`VARDA_MCP_ADDR`), alongside the per-egress-host `allow@<host>` rules under `--net-default-egress deny`. It is never added unconditionally and never broadened past the `host` group.
    - **Guest MCP bridge (`.mcp.json`).** The bridge lives in the orchestrate workspace's `.mcp.json` (outside this crate). When a TCP broker is in play it must connect over TCP to the host-internal name, falling back to the Unix socket when only `VARDA_MCP_SOCKET` is set:
      ```jsonc
      // TCP (microsandbox/clawk): dial the host over host.microsandbox.internal
      { "command": "sh", "args": ["-c", "exec socat - TCP:${VARDA_MCP_HOST:-host.microsandbox.internal}:$VARDA_MCP_PORT"] }
      // Unix socket (local/docker): fall back to the bind-mounted socket
      { "command": "sh", "args": ["-c", "exec socat - UNIX-CONNECT:$VARDA_MCP_SOCKET"] }
      ```
      Whenever `orchestrate` scaffolds or refreshes the resident `.mcp.json`, it writes the bridge in this exact form so a VM-backed guest reaches the broker under `--net-default-egress deny` without a manual `VARDA_BROKER_BIND_IP`.
- **Launcher** — the concrete host `SubtaskLauncher` materializes a normal Varda task with `create_task`, marks it ready, and runs it through the existing dispatcher. That means a spawned subtask runs as a host-started sibling sandbox selected by the normal route/`.varda` provider path, never as nested docker-in-docker inside the master. The current launcher is synchronous and uses `tokio::task::block_in_place`, so it requires the CLI's multi-thread Tokio runtime; descendant runs inherit the parent's shared spawn state so depth, fan-out, and global child budget compose across generations.

**Config surface.** `OrchestrationPolicy` is exposed through `config.toml` as a top-level `[orchestration]` table (defaults) and an optional per-`[[routes]]` `orchestration` override; `Config::resolve_orchestration_for(path)` returns the route override when the matched route sets one, else the global defaults, so untrusted code can be pinned to a stricter (or deliberately looser) spawn policy than the default:

```toml
[orchestration]          # global defaults (omitted table ⇒ locked-down default)
enabled       = true
max_depth     = 2
max_fanout    = 4
global_child_budget = 16
deny_sandboxes = ["local"]

[[routes]]
glob = "**/untrusted/**"
agents = ["claude"]
[routes.orchestration]   # stricter policy just for this route
enabled = false
```

#### Orchestration isolation invariants (MANDATORY — never violate)

These must hold for the broker **and** the base sandbox; violating any makes the sandbox theatre:

1. **Never mount the docker socket** (`/var/run/docker.sock`, `docker.sock`/`podman.sock` by any path) into any agent container — it is equivalent to host root. Enforced by `DOCKER_SOCKET_BASENAMES` / `DOCKER_SOCKET_PATHS` in `check_control_plane_denylist`, across all mount origins.
2. **Never mount `~/.varda`** (or install the `varda` binary) into an agent container — it hands the agent the control plane. Enforced by `CONTROL_PLANE_DENYLIST` in `check_control_plane_denylist`, across all mount origins.
3. **No `--privileged` / no docker-in-docker** for agent containers. Sub-sandboxes are **siblings spawned by the host**, never nested inside the master.
4. Spawning is reachable **only** through the gated `spawn_subtask` MCP tool mediated by host-side Varda — never via host process access, the docker socket, or a mounted control plane.
5. Every spawn is bounded by **depth + fan-out + global child budget**; exceeding a bound is a hard error, not a silent cap.

Invariants 1 and 2 are enforced at the mount layer (folded into `check_credential_denylist`, so every mount call site is covered); invariant 5 is enforced by the `src/orchestration.rs` policy engine and re-checked on every `SpawnBroker` tool call.

> **Status:** the policy engine, live broker, Unix-socket MCP transport, concrete sibling-task launcher, collect channel (`await_subtask`/`await_subtasks`/`subtask_result` + the host `SubtaskResults` seam), and `[orchestration]` config surface are implemented and covered by unit tests. **Remaining:** the launcher is still synchronous/blocking (a non-blocking launcher so a master can spawn a wave then `await_subtasks` is a follow-up), and a docker-backed negative-isolation integration test (`--ignored`) that exercises a real sandboxed master end-to-end and asserts no docker socket / no `~/.varda` in the guest.

### Self-hosting orchestrator (`varda orchestrate`)

`varda orchestrate` launches the **RESIDENT** — a long-lived orchestrator agent that drives Varda's own dev loop by spawning capped workers through the broker above. Unlike the earlier un-sandboxed resident sketch, the resident now runs **inside an isolating sandbox** with a dedicated workspace mounted read-write; it merges worker branches **in-box** against that mount. The blast radius is therefore bounded to *local, un-pushed work in the workspace* plus the *capped worker budget* — nothing reaches a remote from inside the box.

The resident authenticates with a long-lived Claude token via the `claude-resident` agent's
`from_env = "CLAUDE_CODE_OAUTH_TOKEN"` credential. Rather than exporting the raw token, keep
it as a reference in [fnox](https://fnox.jdx.dev) (a `fnox.toml` maps `CLAUDE_CODE_OAUTH_TOKEN`
to a `pass://` Proton Pass reference) and launch through `fnox exec` — fnox resolves it on the
host and varda copies it *scoped* into the box (fnox stays on the exterior; `~/.config/fnox`
is never mounted). A ready `fnox.toml` ships in the orchestrate workspace.

**codex on the ChatGPT subscription (#521).** A spawned `codex` worker lands in the `worker`
sandbox (msb, `varda-agents:latest`, uid-1001 `agent`), which boots with no `~/.codex` and would
hit `401 Unauthorized: Missing bearer` on `api.openai.com`. The `[agents.codex]` credential mints
`CODEX_AUTH_B64` from the host's ChatGPT OAuth `~/.codex/auth.json` (id_token + long-lived
refresh_token, `OPENAI_API_KEY: null`) and a small in-guest prelude decodes it into
`$CODEX_HOME/auth.json` — **agent-owned and writable**, so codex refreshes the ~1h id_token in-run
(over `auth.openai.com`) and writes its sessions/rollouts. The prelude is gated on the guest HOME
(`/home/agent`), so an un-sandboxed local `vcodex` never runs it and never clobbers the operator's
real `~/.codex`. A `file`-target credential would NOT work here: it stages `0o400` owned by the
host uid, which the uid-1001 msb agent cannot read (docker agents like `adb-copilot` run as root, so
file-target works there). No `OPENAI_API_KEY` / no per-token API spend. **Security:** the injected
refresh_token is long-lived and powerful but bounded by the worker box — egress is OpenAI/Anthropic
only with **no push credential**, so a compromised worker can burn ChatGPT quota but cannot
exfiltrate the token to an attacker host. Time-boxing is weak (revoke = kill the ChatGPT
session/device); for true short-TTL prefer the host-proxy model (box holds no credential).

```bash
# Headless: run the resident autonomously until it terminates or signals needs_user.
# `fnox exec` injects CLAUDE_CODE_OAUTH_TOKEN from the pass:// reference; run from the
# workspace so ./fnox.toml is discovered.
cd /path/to/orchestration/workspace && fnox exec -- varda orchestrate

# Interactive: attach your terminal (M13b), operator in the conversation, broker available.
fnox exec -- varda orchestrate --interactive

# Point it at a specific dedicated workspace (default: <varda_home>/orchestrate/workspace).
fnox exec -- varda orchestrate --workspace /path/to/orchestration/workspace
```

The command resolves (or scaffolds) a `resident-orchestrator` task under the workspace whose body points at the workspace's **`.varda/WORKFLOW.md`** — that file holds the loop *intelligence* (a separate concern); `orchestrate` only handles the command, routing, and enforcement. It then delegates to the standard run path, which for an orchestration-enabled route wraps the session in the interactive spawn broker so `spawn_subtask` is served for the whole session.

**Load-bearing gates — asserted in code before launch (`config::enforce_resident_launch`), a violation FAILS LOUDLY:**

| Gate | Requirement | Rejected when… |
|---|---|---|
| **G1** workspace | a **dedicated** directory mounted **rw** | the workspace is `$HOME` or a home-ancestor, or it is not mounted read-write |
| **G2** isolation | an **isolating** sandbox (`primitive != "local"`) | the route resolves to `local`/un-sandboxed |
| **G2** network | **strict, firewall-enforced egress to the resident agent's exact LLM endpoints only** — Claude may use `api.anthropic.com` and `platform.claude.com` (the latter is a hard startup-connectivity requirement from Claude Code v2.x on); Codex/OpenAI may use `api.openai.com`, `chatgpt.com`, and `auth.openai.com`; an empty `egress` ⇒ `--network none` also passes for supported agents. A non-empty resident allow-list requires an enforced-egress provider: `microsandbox`/`clawk` (in-guest IP firewalling) or docker under `strict`/`proxy` (the allow-listing forward-proxy sidecar). Docker DNS-pin mode is refused for residents. Copilot resident mode currently fails closed until exact non-push Copilot auth/API endpoints are known. `github.com` and every other general host stay denied. Match is case-insensitive EXACT host (no wildcard/suffix, so `api.openai.com.evil.com` is denied). | the sandbox declares any egress host outside the selected resident agent's allowlist, uses `egress_mode = "dns-pin"`, or selects Copilot as resident before exact non-push endpoints are configured |
| **G2** no push cred | the resident identity carries **no `git push` credential**, across *every* channel one can reach the box through | `forward_ssh_agent = true`; a credential targets a push channel (env `GITHUB_TOKEN`/`SSH_AUTH_SOCK`/… or a file `.ssh/` key, `*credential*` store, `.config/gh/hosts.yml`, `.netrc`, askpass script); a push-enabling key in the resident's **effective env** (agent + sandbox + route `env` maps — `GITHUB_TOKEN`, `GIT_ASKPASS`, `SSH_AUTH_SOCK`, `GIT_SSH_COMMAND`, `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_*`, `GIT_CONFIG_GLOBAL`/`SYSTEM`, `GIT_TERMINAL_PROMPT`, …); or the workspace's `.git/config` (incl. submodules) carries a token-embedded remote URL or a `credential.helper` |
| **G7** broker caps | orchestration **enabled** and `local` in `deny_sandboxes` | spawning is disabled, or workers could land un-sandboxed |

**Human-gated push.** Because the resident's egress is restricted to LLM endpoints (no `github.com`, no general hosts) and it holds no push credential, it *cannot* push its merged result to a remote. Pushing back out is a deliberate, separate step performed **on the host by a human** after reviewing the workspace — the sandbox produces local commits/branches, never a remote mutation. This is the same clawk-style split the interactive sandbox uses for identity, taken to its strict end: the box has *no* path to a remote at all.

Configure the sandboxed resident route in `config.toml` (see the commented `[sandboxes.orchestration]` / `[[routes]]` / `[routes.orchestration]` example the default config ships):

```toml
[sandboxes.orchestration]
image = "your-dev-image:latest"
primitive = "microsandbox"    # strict egress enforcement — NEVER "local"; docker is allowed under strict/proxy (forward-proxy sidecar) or egress = []
egress = ["api.anthropic.com", "platform.claude.com"]  # exact Claude resident endpoints only; no wildcard/github.com. Codex uses api.openai.com/chatgpt.com/auth.openai.com.

[[routes]]
glob = "/path/to/orchestration/workspace/**"
agents = ["claude"]           # resident identity carries NO git push credential. Copilot resident is unsupported until exact non-push endpoints are known.
sandbox = "orchestration"
mounts = ["/path/to/orchestration/workspace:/workspace:rw"]  # dedicated rw workspace

[routes.orchestration]
enabled = true
max_depth = 1                 # resident depth0 -> workers depth1
max_fanout = 16               # a full worker/reviewer/resolver wave
global_child_budget = 64
deny_sandboxes = ["local"]    # spawned workers must never land un-sandboxed
```

> **Project/workspace mount is auto-provided rw + host-visible.** Every sandbox mounts the project at its own absolute host path **once, read-write** (so a resident's in-box merges land on the host and are committable). For `orchestrate` the project *is* the workspace, so the G1 gate's explicit rw `mounts` entry can name the same host path as its guest target (`…/workspace:…/workspace:rw`) or a distinct one (`…/workspace:/workspace:rw`) — a mount that resolves to the **same guest path** as the auto project mount is de-duplicated rather than emitted twice (microsandbox/`msb` rejects two volumes on one guest path). NB: `msb run` 0.6.8 has **no** `--project` flag; the project is a plain `--mount-dir HOST:GUEST:rw` bind and the workdir is set via `--workdir`.

> A docker-backed live end-to-end test (`#[ignore = "requires docker"]` — `orchestrate_live_resident`) captures the full scenario: a resident in a box spawns one worker that edits a file on a branch, merges it in-box, the change is visible on the host through the mount, and `~/.aws`/host `$HOME` were never visible and no push occurred. The microsandbox rw/host-visibility guarantee has its own `#[ignore = "requires the msb (microsandbox) runtime"]` check (`microsandbox_workspace_mount_is_rw_and_host_visible_live`).

### Per-task capability allowlist (headless permission grants)

A headless Varda run has **no interactive approver**. The agent backend's own permission layer therefore denies any command it was not pre-authorized to execute — in Claude Code `-p`/print mode there is no human to answer the "allow this command?" prompt, so the command simply fails and the agent (correctly) degrades to `needs_user`. This is the friction that blocked the sandbox-self-test agent, which needed host `msb`/`docker` to live-verify.

Declare the commands a task is allowed to run in its frontmatter:

```yaml
---
id: 123
status: ready
project: /Users/you/dev/project
assignee: claude
allow_commands:
  - msb            # → Bash(msb:*)
  - docker         # → Bash(docker:*)
  - "Bash(cargo test:*)"   # full tool patterns pass through verbatim
---
```

At run time Varda translates `allow_commands` into the backend's permission config — for Claude Code, a **run-scoped settings file** (`<runs>/<session_id>.settings.json`) carrying `permissions.allow`, injected via `--settings`. Each bare name becomes a single `Bash(<cmd>:*)` prefix rule; an entry that already looks like a tool pattern (it contains `(`) is passed through unchanged. This is:

- **Deterministic** — no human in the loop.
- **Scoped** — only the declared commands are authorized; a command *not* on the list still blocks. This is **not** `--dangerously-skip-permissions`; no global bypass is ever introduced.
- **Per-task** — the grant lives on the task, not on the agent config or a shared route, so one task's allowance does not widen another's.

> Backend coverage: this targets the Claude Code backend, whose headless permission model is the one that blocks un-approved commands. The `codex` (`--sandbox workspace-write`), `copilot` (`--allow-all-tools`), and `opencode` (`--auto`) backends already grant broad non-interactive execution, so `allow_commands` is a no-op there.

#### Actionable denial (scripted re-run)

When the permission layer blocks a command, the agent lists it under a `Blocked commands` heading in its recap (one command per line). Varda parses that into the structured `blocked_commands` field of the run outcome and prints it:

```
blocked_commands: msb, docker build
hint: add these to the task's `allow_commands` frontmatter and re-run to authorize them headlessly
```

An orchestrator can read that list, append the names to `allow_commands`, and re-run **automatically** — rather than guessing which capability was missing.

#### Sandbox-self-test carve-out (host allowlist, not sandboxed)

There is one class of task that **must** run with an explicit *host* allowlist rather than inside a sandbox: tasks that **develop or test the sandbox providers themselves**. Building/running a microsandbox (`msb`) or docker image is exactly the operation the isolation invariants forbid *inside* an agent container — **no `--privileged`, no docker-in-docker; sub-sandboxes are siblings spawned by the host, never nested** (see [Isolation invariants](#isolation-invariants-never-violate) invariant 3 and [Orchestration isolation invariants](#orchestration-isolation-invariants-mandatory--never-violate) invariant 3). You cannot nest a docker/microVM build inside a box that is itself denied the docker socket and DinD.

So a sandbox-provider task runs **on the host** (`sandbox = "local"`, or no sandbox) with a narrow `allow_commands = ["msb", "docker", "cargo"]`. The capability allowlist keeps that host execution **deterministic and scoped to the named build/test commands** instead of requiring an interactive approver or a blanket bypass. This is the deliberate exception to "everything runs sandboxed", and it exists precisely *because* those commands operate the isolation layer that everything else relies on.

The deeper fix (tracked separately) is to run Varda's own agents *inside* the sandbox and then safely relax in-box permissions — a strong L1 isolation primitive substitutes for L2 approval prompts. This per-task allowlist is the near-term, deterministic step that also remains necessary for the self-test carve-out above.

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
- Task handoff to another agent is represented by `review` plus recap metadata, but automatic reassignment is not implemented yet.
