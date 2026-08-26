# Configuration

Routes, agents, and the full `config.toml` reference.

## The control plane

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
- `$VARDA_HOME/operations/runs/`: where agent session logs and notification records are written.

`varda init` also runs `git init` in the control-plane folder. This matters because `varda task run` commits task updates, recaps, and notifications into that repository.

# Add Project Routes

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

## Supported agents

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

# Configuration

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

Run `varda config show` to print the central `config.toml` file exactly as it is on disk, or `varda config show --resolved` to print the fully merged config — every `include` fragment merged in, with precedence applied — as it is actually used at runtime.

`kind = "acp"` is currently the only legal value, and it selects nothing: `AgentKind` is a
single-variant enum that is constructed everywhere and matched on nowhere. Varda does not
implement the Agent Client Protocol for any agent. Every agent — Codex, Claude, Copilot,
opencode — is launched as a subprocess, with the prompt piped on stdin (or staged as a guest
file under a sandbox) and the recap scraped from stdout. The only per-agent branching is
session-id discovery, which reads each CLI's own on-disk session store to build a resume
command; that is file scraping, not protocol negotiation.

When the generated Codex args contain `--cd "."`, Varda replaces that `.` at runtime with the task's `project` path. That is what makes the tracked project writable to Codex under `--sandbox workspace-write`, even though the task file itself lives in the global Varda control-plane folder. Varda also expands `{varda_project}` to the Varda source project directory and `{varda_home}` to the Varda control-plane directory, then passes both as additional writable directories to the default Codex, Claude, and Copilot launch commands, so interpreter and resume sessions can create follow-up Varda tasks after the current task finishes.

## Configuration reference

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
| Shareable config bundles | `include = ["team-bundle.toml"]` merges another TOML fragment's `[[routes]]`/`[sandboxes.*]`/`[agents.*]` into the central config; a central name always wins over an included one, and among includes a later one wins. The table form `{ path = "...", sha256 = "..." }` pins the fragment: at load time `varda` hashes the exact bytes it read and compares against the pin. A malformed pin (not 64 lowercase hex characters) is rejected when the central config is parsed, before any fragment is read. An entry without `sha256` is unaffected. The read-only `task inspect` and `task doctor` commands warn loudly and continue with the unverified content on a mismatch, so they can still report the true (if possibly stale) route/agent/sandbox. Commands that launch or dispatch work (`task run`, `orchestrate`, `plan`, `resume`, …) go through launch-time bundle approval on a mismatch instead of refusing outright: `varda` diffs the bundle's *capabilities* (not raw bytes) against the last-approved copy it keeps under `~/.varda/approved-bundles/` — sandbox escapes and host-command execution first, worded as plain consequences — and if nothing security-relevant changed (a comment, a key reorder, a pure capability removal) it silently re-pins and proceeds without prompting. If the capability surface *did* change, only an interactive human on an attached terminal is ever asked to approve; a headless run (`task run` under cron, no TTY) and any process already running inside a varda-managed sandbox (a spawned worker or the resident) always refuse outright instead — the latter can never be offered the prompt, since a sandboxed process approving its own capability escalation would defeat the control. Declining the prompt falls back to the previously-approved bundle when one exists (rather than leaving the run with nothing to run), or refuses outright on a first use with nothing to fall back to. A fragment declaring its own `include` is rejected (nested includes are not supported), and any key inside a fragment that this `varda` version does not recognize (typo, or version skew with whoever authored the bundle) fails config load loudly instead of being silently dropped; the central `config.toml` itself stays as permissive as before. |
| Host requirement validation | `requires_commands = ["fnox"]` and `requires_secrets = ["tfc-token"]` (also settable inside an included fragment) fail `varda`'s config load loudly, listing every unmet requirement, when a command is missing from `$PATH` or a secret does not resolve via `fnox get NAME`. |

Credential entries must name exactly one source (`from_env`, `from_secret`, `from_fnox`, or `command`) and one target (`env` or `file`). Store secret names, not resolved secret values, in config.

[← back to the README](../README.md)
