# The agent contract

What Varda tells an agent, what it requires back, and the bounds it runs under.

# What The Agent Is Told

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

# Execution bounds (cooperative, not a hard kill)

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

[← back to the README](../README.md)
