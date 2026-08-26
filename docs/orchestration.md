# Orchestration

Sub-task spawning, the self-hosting resident, capability grants, and roles.

## Nested orchestration (gated sub-task spawning)

A "master" agent task running inside a sandbox may need to **decompose work** and have sub-agents complete subtasks. It must not gain host access to do so — that would defeat the sandbox. The model is *control-plane orchestrates, agent only requests*:

- The master gets exactly **one** spawn capability across the boundary: an MCP tool `spawn_subtask(brief, route?, sandbox?, agent?) -> {subtask_id}`. To harvest results it then blocks on the **collect channel** — `await_subtask(subtask_id) -> {subtask_id, status}`, `await_subtasks(subtask_ids[]) -> [{subtask_id, status}]` (the wave primitive), and `subtask_result(subtask_id) -> {status, files_touched, blocked_commands, recap}`. Nothing else crosses.
- The same broker also exposes a **task control-plane** surface (task #640) so a sandboxed agent can see and update its project's task board without ever mounting `~/.varda`: `list_tasks(status?) -> [{id, slug, status, title, assignee}]` and `get_task(id) -> {id, slug, status, title, assignee, body}`, both scoped to the caller's own project (a cross-project id is refused, indistinguishable from unknown), and `set_task_status(id, status)` — the write side that closes the status-drift loop (an agent that finishes now marks itself `done`/`needs_user`/`failed` instead of a human running `varda task set-status` by hand later). `set_task_status` is **role-scoped** (task #687): the caller may always settle its own task id; the ROOT/orchestrator of the broker's spawn tree (the resident, registered at depth 0 — host-known, never attacker-supplied) may additionally settle ANY task id within its own project, since `task_control_plane_project` already scopes the whole control-plane surface to one project. An ordinary spawned worker (depth >= 1) stays self-only, even for a sibling task it spawned itself. It only accepts a `running -> {done, needs_user, failed}` transition; `review -> done` is refused outright since review is a human-only gate, even for the root. Finally, `create_task(title, body?, assignee?, sandbox?, status?) -> {id, slug}` (task #717) mints a NEW task on the caller's own board **without launching it** — the "file this for later" primitive an agent uses to record a finding, defect, or follow-up it noticed mid-run instead of spawning a worker for it right now, burying it in a recap, or dictating it to a human. There is no `project` argument (always the caller's own), `status` may only be `backlog` (default) or `ready`, and — unlike `spawn_subtask` — it consumes no fan-out and no child budget, since nothing is launched.
- Host-side Varda (outside every sandbox) receives the request and, before launching anything:
  - validates it against **policy** — which routes/agents/sandboxes the master may spawn (allow/deny lists);
  - enforces **depth**, **fan-out**, and a **global child budget** so a master cannot recursively fork-bomb;
  - optionally requires **human approval** for a given spawn depth (e.g. the first level);
  - runs each subtask in its **own sibling sandbox** — spawned by the host, never nested/DinD inside the master;
  - returns results back through the broker.
- Even a fully compromised master can only *ask* the broker; it never holds the capability to spawn host processes. The sandbox covers the inward radius; the broker covers the outward radius.

The host-side policy engine and the live broker both live in `src/orchestration.rs`:

- **Policy engine** — `OrchestrationPolicy` + `SpawnLedger::authorize`/`authorize_and_record`. Every cap is a **hard error** (`SpawnDenied`), never a silent truncation. Safe defaults: spawning is **disabled**, and the `local` (no-isolation) sandbox is denied so a spawned subtask cannot escape the box. `max_fanout` bounds **concurrent** children per parent, not a lifetime total: `SpawnLedger::release` frees a parent's slot the first time `await_subtask`/`await_subtasks` observes that child reach a terminal status, so a resident that runs many sequential waves over a long session is never permanently capped after its first `max_fanout` spawns (#716). `global_child_budget` is the separate, genuinely cumulative lifetime/cost ceiling and is never decremented.
- **Broker** — `SpawnBroker` uses shared spawn state containing the ledger plus a **lineage registry** (task id → tree depth), a host `SubtaskLauncher` seam, and a host `SubtaskResults` seam (the collect side). It speaks MCP JSON-RPC (`handle_rpc`): `tools/list` advertises exactly eleven tools — the spawn side (`spawn_subtask`, `run_subtask`), the collect side (`await_subtask`, `await_subtasks`, `subtask_result`, `subtask_diff`), the integrate side (`integrate_subtasks`), and the task-board surface (`list_tasks`, `get_task`, `create_task`, `set_task_status`) — and each `spawn_subtask` call is gated through `authorize_and_record` **before** the host is asked to launch. A denial comes back as an MCP tool error (`isError: true`) carrying the `SpawnDenied` reason. The caller **never supplies its own depth** — the broker looks it up from the lineage registry, so a compromised master cannot claim a shallow depth to dodge the recursion cap, and an unknown caller cannot spawn at all. If the host launch fails after authorization, the ledger is rolled back (`SpawnLedger::unrecord`) so a failed attempt consumes no budget.
- **Collect channel** — `await_subtask*` **block** by polling the `SubtaskResults` seam on a ~1s interval until the child reaches a **terminal** status (`done`/`failed`/`needs_user`/`review`), bounded by an absolute ceiling (30 min) so a wedged-but-still-resolvable child returns a timeout error rather than hanging forever. A subtask id that cannot be RESOLVED at all — unknown id, an ambiguous duplicate, or a failed state load — is a distinct outcome (`SubtaskStatus::Unresolved`) from "still running" and is surfaced as a tool error immediately, without waiting out the ceiling (#653). `subtask_result` resolves the child's recap and parses its `Files touched` / `Blocked commands` sections (the same `parse_files_touched`/`parse_blocked_commands` the runner uses) — it returns no resume command, since resuming is a resident host action, not a worker's. The concrete host `SubtaskResults` (`VardaSubtaskResults`) resolves a subtask id → home STATE via `task::lookup_task_state` and reads the recap file; the resident (un-sandboxed) host reuses the same impl directly. `task::find_task_by_id` (which `lookup_task_state` calls) resolves an id through a persistent id→path index (`operations/tasks/.task_index.json`) instead of rescanning every task file on every poll; a cache miss or stale entry falls back to a full-tree scan that also rebuilds the index, so a missing/corrupt index self-heals rather than staying slow (#653).
- **Transport** — when the effective orchestration policy is enabled for a task, the run path starts a per-session MCP transport (`src/mcp_transport.rs`) that speaks newline-delimited JSON-RPC and dispatches into the live broker; no host process or docker capability is handed to the agent. Spawn authorization holds the shared broker state only while checking and recording policy, then releases it before the synchronous child run starts, so other MCP connections are not blocked on the global broker state for the whole child run. **The transport is selected by the sandbox primitive** (`config::primitive_needs_tcp_broker`):
  - `local`/`docker` (shared kernel) — a per-session **Unix socket** under the mounted project tree (`{project}/.varda-mcp/{session}.sock`), passed to the sandbox as `VARDA_MCP_SOCKET`. The guest reaches it through the bind mount.
  - `microsandbox` (own-kernel microVM) — the project bind mount shares the socket *file* over virtio-fs but **not** its AF_UNIX endpoint, so an in-guest `connect()` is refused. Instead the broker binds a **host TCP** listener on an ephemeral port and advertises it to the guest as `VARDA_MCP_ADDR` (host:port) plus `VARDA_MCP_PORT` (the port alone). The broker BINDS to **host loopback** (`127.0.0.1`) by default — but the guest's own `127.0.0.1` is *not* the host, so the guest-visible connect host is exported separately as **`VARDA_MCP_HOST=host.microsandbox.internal`** (a name msb resolves to the host machine). The guest bridge dials `host.microsandbox.internal:$VARDA_MCP_PORT`. The listener binds a **host-only** interface — loopback by default, overridable via `VARDA_BROKER_BIND_IP` — never `0.0.0.0`; it is ephemeral and torn down with the session, and the broker is capability-gated regardless of reachability, so a reachable port grants no capability the socket did not.
    - **Host access must be allowed.** msb DENIES host access by default, so `MicrosandboxSession::wrap` adds `--net-rule allow@host` (the reserved `host` group = the local trusted orchestrator running the broker) *only* when a broker is wired for the run (the guest env carries `VARDA_MCP_HOST`/`VARDA_MCP_ADDR`), alongside the per-egress-host `allow@<host>` rules under `--net-default-egress deny`. It is never added unconditionally and never broadened past the `host` group.
    - **Guest MCP bridge (`.mcp.json`).** The bridge lives in the orchestrate workspace's `.mcp.json` (outside this crate). When a TCP broker is in play it must connect over TCP to the host-internal name, falling back to the Unix socket when only `VARDA_MCP_SOCKET` is set:
      ```jsonc
      // TCP (microsandbox): dial the host over host.microsandbox.internal
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

### Orchestration isolation invariants (MANDATORY — never violate)

These must hold for the broker **and** the base sandbox; violating any makes the sandbox theatre:

1. **Never mount the docker socket** (`/var/run/docker.sock`, `docker.sock`/`podman.sock` by any path) into any agent container — it is equivalent to host root. Enforced by `DOCKER_SOCKET_BASENAMES` / `DOCKER_SOCKET_PATHS` in `check_control_plane_denylist`, across all mount origins.
2. **Never mount `~/.varda`** (or install the `varda` binary) into an agent container — it hands the agent the control plane. Enforced by `CONTROL_PLANE_DENYLIST` in `check_control_plane_denylist`, across all mount origins.
3. **No `--privileged` / no docker-in-docker** for agent containers. Sub-sandboxes are **siblings spawned by the host**, never nested inside the master.
4. Spawning is reachable **only** through the gated `spawn_subtask` MCP tool mediated by host-side Varda — never via host process access, the docker socket, or a mounted control plane.
5. Every spawn is bounded by **depth + fan-out + global child budget**; exceeding a bound is a hard error, not a silent cap.

Invariants 1 and 2 are enforced at the mount layer (folded into `check_credential_denylist`, so every mount call site is covered); invariant 5 is enforced by the `src/orchestration.rs` policy engine and re-checked on every `SpawnBroker` tool call.

> **Status:** the policy engine, live broker, Unix-socket MCP transport, concrete sibling-task launcher, collect channel (`await_subtask`/`await_subtasks`/`subtask_result` + the host `SubtaskResults` seam), and `[orchestration]` config surface are implemented and covered by unit tests. **Remaining:** the launcher is still synchronous/blocking (a non-blocking launcher so a master can spawn a wave then `await_subtasks` is a follow-up), and a docker-backed negative-isolation integration test (`--ignored`) that exercises a real sandboxed master end-to-end and asserts no docker socket / no `~/.varda` in the guest.

## Self-hosting orchestrator (`varda orchestrate`)

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
| **G2** network | **strict, firewall-enforced egress to the resident agent's exact LLM endpoints only** — Claude may use `api.anthropic.com` and `platform.claude.com` (the latter is a hard startup-connectivity requirement from Claude Code v2.x on); Codex/OpenAI may use `api.openai.com`, `chatgpt.com`, and `auth.openai.com`; an empty `egress` ⇒ `--network none` also passes for supported agents. A non-empty resident allow-list requires an enforced-egress provider: `microsandbox` (in-guest IP firewalling) or docker under `strict`/`proxy` (the allow-listing forward-proxy sidecar). Docker DNS-pin mode is refused for residents. Copilot resident mode currently fails closed until exact non-push Copilot auth/API endpoints are known. `github.com` and every other general host stay denied. Match is case-insensitive EXACT host (no wildcard/suffix, so `api.openai.com.evil.com` is denied). | the sandbox declares any egress host outside the selected resident agent's allowlist, uses `egress_mode = "dns-pin"`, or selects Copilot as resident before exact non-push endpoints are configured |
| **G2** no push cred | the resident identity carries **no `git push` credential**, across *every* channel one can reach the box through | `forward_ssh_agent = true`; a credential targets a push channel (env `GITHUB_TOKEN`/`SSH_AUTH_SOCK`/… or a file `.ssh/` key, `*credential*` store, `.config/gh/hosts.yml`, `.netrc`, askpass script); a push-enabling key in the resident's **effective env** (agent + sandbox + route `env` maps — `GITHUB_TOKEN`, `GIT_ASKPASS`, `SSH_AUTH_SOCK`, `GIT_SSH_COMMAND`, `GIT_CONFIG_COUNT`/`GIT_CONFIG_KEY_*`, `GIT_CONFIG_GLOBAL`/`SYSTEM`, `GIT_TERMINAL_PROMPT`, …); or the workspace's `.git/config` (incl. submodules) carries a token-embedded remote URL or a `credential.helper` |
| **G7** broker caps | orchestration **enabled** and `local` in `deny_sandboxes` | spawning is disabled, or workers could land un-sandboxed |

**Human-gated push.** Because the resident's egress is restricted to LLM endpoints (no `github.com`, no general hosts) and it holds no push credential, it *cannot* push its merged result to a remote. Pushing back out is a deliberate, separate step performed **on the host by a human** after reviewing the workspace — the sandbox produces local commits/branches, never a remote mutation. This is the same split the interactive sandbox uses for identity, taken to its strict end: the box has *no* path to a remote at all.

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
max_fanout = 16               # CONCURRENT children (a full worker/reviewer/resolver wave); frees up as each child settles, so a resident can run many sequential waves — it is not a lifetime cap
global_child_budget = 64      # the true LIFETIME/cost ceiling across the whole session
deny_sandboxes = ["local"]    # spawned workers must never land un-sandboxed
```

> **Project/workspace mount is auto-provided rw + host-visible.** Every sandbox mounts the project at its own absolute host path **once, read-write** (so a resident's in-box merges land on the host and are committable). For `orchestrate` the project *is* the workspace, so the G1 gate's explicit rw `mounts` entry can name the same host path as its guest target (`…/workspace:…/workspace:rw`) or a distinct one (`…/workspace:/workspace:rw`) — a mount that resolves to the **same guest path** as the auto project mount is de-duplicated rather than emitted twice (microsandbox/`msb` rejects two volumes on one guest path). NB: `msb run` 0.6.8 has **no** `--project` flag; the project is a plain `--mount-dir HOST:GUEST:rw` bind and the workdir is set via `--workdir`.

> A docker-backed live end-to-end test (`#[ignore = "requires docker"]` — `orchestrate_live_resident`) captures the full scenario: a resident in a box spawns one worker that edits a file on a branch, merges it in-box, the change is visible on the host through the mount, and `~/.aws`/host `$HOME` were never visible and no push occurred. The microsandbox rw/host-visibility guarantee has its own `#[ignore = "requires the msb (microsandbox) runtime"]` check (`microsandbox_workspace_mount_is_rw_and_host_visible_live`).

## Per-task capability allowlist (headless permission grants)

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

### Actionable denial (scripted re-run)

When the permission layer blocks a command, the agent lists it under a `Blocked commands` heading in its recap (one command per line). Varda parses that into the structured `blocked_commands` field of the run outcome and prints it:

```
blocked_commands: msb, docker build
hint: add these to the task's `allow_commands` frontmatter and re-run to authorize them headlessly
```

An orchestrator can read that list, append the names to `allow_commands`, and re-run **automatically** — rather than guessing which capability was missing.

### Sandbox-self-test carve-out (host allowlist, not sandboxed)

There is one class of task that **must** run with an explicit *host* allowlist rather than inside a sandbox: tasks that **develop or test the sandbox providers themselves**. Building/running a microsandbox (`msb`) or docker image is exactly the operation the isolation invariants forbid *inside* an agent container — **no `--privileged`, no docker-in-docker; sub-sandboxes are siblings spawned by the host, never nested** (see [Isolation invariants](sandboxing.md#isolation-invariants-never-violate) invariant 3 and [Orchestration isolation invariants](#orchestration-isolation-invariants-mandatory--never-violate) invariant 3). You cannot nest a docker/microVM build inside a box that is itself denied the docker socket and DinD.

So a sandbox-provider task runs **on the host** (`sandbox = "local"`, or no sandbox) with a narrow `allow_commands = ["msb", "docker", "cargo"]`. The capability allowlist keeps that host execution **deterministic and scoped to the named build/test commands** instead of requiring an interactive approver or a blanket bypass. This is the deliberate exception to "everything runs sandboxed", and it exists precisely *because* those commands operate the isolation layer that everything else relies on.

The deeper fix (tracked separately) is to run Varda's own agents *inside* the sandbox and then safely relax in-box permissions — a strong L1 isolation primitive substitutes for L2 approval prompts. This per-task allowlist is the near-term, deterministic step that also remains necessary for the self-test carve-out above.

## Roles

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

[← back to the README](../README.md)
