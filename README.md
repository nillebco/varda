# Varda

**Write a task in markdown. Varda routes it to the right agent, runs it in a sandbox, and commits the result.**

Varda is a small Rust CLI that turns a folder of markdown files into an agent work queue. Each task carries its own project path; Varda matches that path to a route, checks the agent is allowed there, runs it under whatever isolation you configured, captures a recap, and commits everything to a git-tracked control plane.

One operations folder tracks work across every project you own — and Varda can run itself, spawning sandboxed workers that merge their branches back.

```sh
varda task add "Fix the flaky auth test" --exec
```

## Quickstart

```sh
make install          # installs varda to ~/.local/bin
varda init            # creates the control plane at ~/.varda
```

Route your project to an agent, then write a task:

```sh
varda project add "/path/to/project/**" --agents claude,codex
cd /path/to/project
varda task add "Summarize this project" --edit   # --edit opens $EDITOR
varda task run <task>                       # or: varda run  (runs everything ready)
varda task show <task>                      # the recap
```

`varda init` also runs `git init` on `~/.varda`, because every task update, recap and notification is committed there.

## The loop

1. **Route** — the task's project path picks a route; the route decides which agents are allowed.
2. **Run** — Varda marks the task `running` and launches the agent, in a sandbox if the route says so.
3. **Instruct** — the agent is handed your `CLAUDE.md` / `AGENTS.md` / `copilot-instructions.md`, the task body, and a hard requirement to produce a recap listing every file it touched.
4. **Bound** — it runs under a cooperative time and token budget rather than an unbounded loop.
5. **Resolve** — the task lands on `review`, `needs_user`, or `failed`, and the recap is written.
6. **Commit** — Varda commits the task update and recap to the control plane.

`needs_user` raises a desktop notification, so a task that stalls on a question doesn't sit silently.

## Sandboxing

A route can pin a task to a sandbox, and the agent never sees your host:

```toml
[[routes]]
glob = "/path/to/project/**"
agents = ["claude"]
sandbox = "worker"
```

Providers: **docker** and **microsandbox** (microVM). Each box gets a scoped credential minted on the host — never your `~/.aws`, never your `~/.azure` — an egress allowlist instead of open internet, and only the mounts you declare — with identity files forced read-only and credential paths denylisted.

Credentials are named, not embedded: config references a host env var or a secret-store key, and the value is resolved at launch. A repo-committed `.varda` is treated as untrusted and clamped to a hardening floor.

→ [Sandboxing](docs/sandboxing.md)

## Orchestration

Varda can drive itself. `varda orchestrate` starts a resident agent that reads the task board, spawns sandboxed workers into isolated git worktrees, and merges their branches back — surfacing conflicts rather than clobbering them.

```sh
varda orchestrate --interactive --workspace .
```

Workers reach the host only through a narrow MCP broker: spawn, collect, integrate, and task-board tools. Nothing else crosses the boundary, and an agent can never grant itself more capability than its route allows.

→ [Orchestration](docs/orchestration.md)

## Agents

`varda init` ships four ready to use. Add any of them to a route's `--agents`.

| Agent | Backend | Permissions | Resume |
|---|---|---|---|
| `codex` | OpenAI Codex CLI | `--sandbox workspace-write` | yes |
| `claude` | Claude Code CLI | `--permission-mode acceptEdits` | yes |
| `copilot` | GitHub Copilot CLI | `--allow-all-tools` | yes |
| `opencode` | [opencode](https://opencode.ai) | `--auto` | no |

Agents can declare `max_prompt_tokens`; Varda estimates the full prompt and picks an allowed agent that fits, or tells you which ones would.

→ [Configuration](docs/configuration.md) · [The agent contract](docs/agent-contract.md)

## Daily driving

```sh
varda task list                  # tasks for this project
varda task dashboard --web --all # kanban board, every project
varda plan                       # a reviewable plan of ready work
varda task inspect <task>        # agent, route, live processes
varda task doctor <task>         # probe the last run
varda task resume <task>         # continue a needs_user task
```

→ [Tasks](docs/tasks.md) · [Git behavior](docs/git.md)

## Claude Code skill

```sh
varda skill install        # or --link to stay in sync with the repo
```

Then `/varda` in any Claude Code session to manage tasks in natural language.

## Development

```sh
make test      # tests
make fmt       # formatting
make build     # debug build
make release   # release build
make install   # install to ~/.local/bin
```

## Documentation

| | |
|---|---|
| [Tasks](docs/tasks.md) | creating, running, inspecting, resuming |
| [Configuration](docs/configuration.md) | routes, agents, `config.toml` reference |
| [Sandboxing](docs/sandboxing.md) | providers, credentials, isolation invariants |
| [Orchestration](docs/orchestration.md) | sub-task spawning, the resident, roles |
| [The agent contract](docs/agent-contract.md) | what the agent is told, execution bounds |
| [Git behavior](docs/git.md) | what gets committed, verification gate |

## Current limitations

- Agents are driven as subprocesses — spawn the CLI, pipe the prompt on stdin, scrape the recap
  from stdout — not over the Agent Client Protocol. `kind = "acp"` is a vestigial config field
  with a single legal value that selects nothing.
- Notification is file-backed JSON plus terminal output, with a best-effort macOS native signal for tasks that need user input.
- Task handoff to another agent is represented by `review` plus recap metadata; automatic reassignment is not implemented yet.
