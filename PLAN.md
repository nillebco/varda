# Varda Implementation Plan

Varda is a Rust-based operations runner for driving agents through ACP. It routes markdown task files to configured agents, enforces a bounded execution window, captures recaps, updates task state, and versions those updates with git.

## Goals

- Read a configuration file that maps task paths or glob patterns to agents.
- Read markdown task files with YAML frontmatter describing task state.
- Select and invoke the correct agent for a task.
- Run agent execution with a hard 10 minute time limit.
- Instruct agents to produce an end-user recap before the time limit expires.
- Reference that recap from the task markdown file.
- Mark the task as pending for follow-up processing, unless user interaction is required.
- Notify the user when interaction is required.
- Update task markdown files through git-versioned changes.
- Provide an operations dashboard folder via `varda init`.
- Start with a POC that drives Codex from this same project.

## Initial POC Scope

Implement the smallest useful CLI:

```text
varda init
varda run path/to/task.md
```

Initial project layout:

```text
varda/
  Cargo.toml
  src/
    main.rs
    config.rs
    task.rs
    routing.rs
    agent.rs
    acp.rs
    git.rs
    notify.rs
  .varda/
    config.toml
    operations/
      tasks/
      recaps/
      runs/
```

Example configuration:

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
args = ["--acp"]

[git]
auto_commit = true
```

Example task:

```markdown
---
id: 1
status: ready
assignee: codex
recap: null
requires_user: false
---

# Task

Implement the requested change.
```

## Rust Dependencies

Prefer established crates and keep the implementation small:

- `clap` for the CLI.
- `serde` for typed config and task metadata.
- `toml` for configuration parsing.
- `gray_matter` for markdown frontmatter parsing.
- `globset` for route matching.
- `tokio` for async process handling and timeout enforcement.
- `anyhow` and `thiserror` for errors.
- `camino` for UTF-8 paths.
- `uuid` and `time` for run identifiers and timestamps.

For ACP, first check whether a usable Rust ACP client crate exists. If not, hide the transport behind a small trait and implement only the subprocess/stdin-stdout path needed for the Codex POC.

```rust
trait AgentClient {
    async fn run_task(&self, request: AgentRunRequest) -> Result<AgentRunResult>;
}
```

## Task State Machine

Start with a small explicit state model:

```text
ready -> running -> pending
ready -> running -> needs_user
ready -> running -> failed
```

State meanings:

- `ready`: the task can be picked up.
- `running`: Varda has assigned it to an agent.
- `pending`: the agent produced a recap and the task should continue with another agent or a later run.
- `needs_user`: the agent cannot proceed without user interaction.
- `failed`: the runner or agent failed, timed out, or returned invalid output.

The POC should update markdown frontmatter after each transition.

## Agent Execution Contract

Every agent invocation should include clear execution instructions:

```text
You are processing a task managed by Varda.

You have at most 10 minutes.

Before the time limit expires, produce a concise recap for the end user.
The recap must include:
- what you completed
- what remains
- any blockers
- whether user interaction is required
- suggested next agent, if applicable

If you need user input, stop and mark the result as requires_user.
```

Runtime behavior:

- Start the timer before invoking ACP.
- Include the deadline in the agent instructions.
- Enforce the timeout with `tokio::time::timeout`.
- Terminate the agent process if it exceeds the limit.
- If no recap is produced, write a synthetic failure recap.
- Store recaps under `.varda/operations/recaps/<run-id>.md`.
- Add the recap path to the original task frontmatter.
- Set task status to `pending`, `needs_user`, or `failed`.

## Git Integration

After updating the task markdown and writing the recap:

```text
git add task.md recap.md
git commit -m "Update task <task-name>"
```

For the POC, make this configurable:

```toml
[git]
auto_commit = true
```

The implementation should call git as a subprocess and fail clearly if the working directory is not inside a git repository.

## Notification Mechanism

Start minimal:

- Print notifications to stdout or stderr.
- Write notification records to `.varda/operations/runs/<run-id>.notification.json`.

Later notification backends can include:

- macOS notifications.
- Webhooks.
- Email.
- Dashboard event streams.

For `needs_user`, the POC should write a notification record and avoid assigning the task to another agent automatically.

## `varda init`

`varda init` creates:

```text
.varda/
  config.toml
  operations/
    tasks/
    recaps/
    runs/
    README.md
```

It should refuse to overwrite an existing config unless `--force` is passed.

## Codex ACP POC

The first real milestone should support:

```text
varda init
varda run .varda/operations/tasks/example.md
```

Expected behavior:

- Route selection chooses the `codex` agent.
- Varda starts Codex through ACP.
- Varda sends the markdown task body plus frontmatter context.
- Varda waits up to 10 minutes.
- Varda saves the recap.
- Varda updates the task to `pending`, `needs_user`, or `failed`.
- Varda commits the task and recap changes.

If Codex ACP support is not immediately available as a stable Rust client, implement the ACP layer as a subprocess JSON-RPC/stdin-stdout adapter behind `AgentClient`.

## Milestones

1. Scaffold the Rust CLI.
2. Implement `varda init`.
3. Implement config loading and glob route selection.
4. Implement markdown frontmatter read/write.
5. Implement a fake agent client for tests.
6. Add the Codex ACP client behind the same trait.
7. Implement `varda run`.
8. Add git commit support.
9. Add file/stdout notification support.
10. Add focused tests around routing, task state transitions, and timeout handling.

## Commit Policy

Commit after every completed implementation step. Keep commits small and focused so task evolution is visible in git history.
