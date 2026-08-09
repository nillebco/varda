//! Varda configuration loading and initialization.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const VARDA_HOME_ENV: &str = "VARDA_HOME";
pub const CONFIG_FILENAME: &str = "config.toml";
pub const OPERATIONS_DIRNAME: &str = "operations";
pub const TASKS_DIRNAME: &str = "tasks";
pub const RECAPS_DIRNAME: &str = "recaps";
pub const RUNS_DIRNAME: &str = "runs";
pub const OPERATIONS_README: &str = "README.md";
/// Fallback sandbox provider when neither a route nor defaults specify one.
/// Wired into task execution in a later `SandboxProvider` milestone (M1).
#[allow(dead_code)]
pub const DEFAULT_SANDBOX_PROVIDER: &str = "local";

/// The ONLY hosts a Claude net-restricted sandboxed resident may reach.
///
/// The resident is a CLOUD LLM agent that cannot function without reaching its
/// provider API, so a fully net-denied box would be inert. Rather than open the
/// network, `enforce_resident_launch` permits egress ONLY to the selected agent's
/// fixed LLM-endpoint allowlist and denies everything else — crucially
/// `github.com` and any other host — so there is still NO `git push` and NO
/// arbitrary-host exfiltration.
///
/// Matching is a case-insensitive EXACT host comparison (see `enforce_resident_launch`):
/// no wildcard/suffix matching, so a look-alike like `api.openai.com.attacker.com`
/// stays denied.
///
pub const CLAUDE_RESIDENT_EGRESS_ALLOWLIST: &[&str] = &["api.anthropic.com"];

/// The ONLY hosts a Codex/OpenAI net-restricted sandboxed resident may reach.
pub const CODEX_RESIDENT_EGRESS_ALLOWLIST: &[&str] =
    &["api.openai.com", "chatgpt.com", "auth.openai.com"];

/// The known-safe Copilot resident endpoint set.
///
/// Intentionally empty today: Copilot needs endpoints that overlap GitHub, and a
/// blanket `github.com` allow rule would create a resident push/exfiltration route.
/// Until exact non-push Copilot auth/API hosts are proven, Copilot resident mode
/// fails closed. Ordinary worker sandboxes can still opt into broader egress via
/// route/user policy; this resident inventory is deliberately stricter.
pub const COPILOT_RESIDENT_EGRESS_ALLOWLIST: &[&str] = &[];

const DEFAULT_CONFIG: &str = r#"[defaults]
timeout_seconds = 600
operations_dir = "operations"
# M10 cooperative execution bounds (replace the old hard wall-clock kill):
idle_timeout_seconds = 180  # cancel only after this many seconds of total silence
max_seconds = "none"        # soft total ceiling across all continuations; "none" = no ceiling
max_continuations = 0       # auto-resume hops; 0 = OFF (default). See note: only enable for agents that signal "done" by omitting a resume command
max_tool_calls = 0          # reserved; non-zero warns but is not enforced yet

[[routes]]
glob = "**"
agents = ["codex"]
# sandbox = "devcontainer"
# env = { GCLOUD_PROJECT = "example-project" }  # trusted, non-secret per-route env

# [routes.orchestration]  # optional per-route override; replaces [orchestration]
# enabled = false
# max_depth = 1
# max_fanout = 1
# global_child_budget = 2

# Sandboxed resident route (self-hosting orchestrator — `varda orchestrate`).
# The resident runs INSIDE an isolating sandbox with a DEDICATED workspace mounted
# rw; it drives workers and merges their branches IN-BOX against that mount. Blast
# radius = the local, un-pushed workspace + the capped worker budget. This supersedes
# the old un-sandboxed `local` resident example — the four load-bearing gates are
# asserted in code before launch and a violation FAILS LOUDLY:
#   G1  dedicated rw workspace mount (never $HOME / a home-ancestor / ~/dev)
#   G2  isolating sandbox (never `local`), net-restricted to the selected resident
#       agent's exact LLM-endpoint allowlist ONLY (github.com etc. stay denied — no
#       push, no exfil), and NO push credential
#       (nothing that lets the box authenticate `git push` to a remote)
#   G7  broker caps bound the worker fan-out (see [routes.orchestration] below)
# Pushing back out is a separate, human-gated step performed on the HOST.
#
# [sandboxes.orchestration]
# image = "your-dev-image:latest"
# primitive = "microsandbox"    # strict egress enforcement — NEVER "local"; docker may only be used with egress = []
# egress = ["api.anthropic.com"]  # resident agent's exact LLM endpoints ONLY — no wildcard/github.com
#
# [[routes]]
# glob = "/path/to/orchestration/workspace/**"
# agents = ["claude"]           # Claude resident; Codex uses api.openai.com/chatgpt.com/auth.openai.com.
#                               # Copilot resident is unsupported until exact non-push endpoints are known.
# sandbox = "orchestration"
# mounts = ["/path/to/orchestration/workspace:/workspace:rw"]  # dedicated rw workspace
#
# [routes.orchestration]
# enabled = true
# max_depth = 1                 # resident depth0 -> workers depth1; worker spawns hit DepthExceeded
# max_fanout = 16               # one resident can launch a full worker/reviewer/resolver wave
# global_child_budget = 64      # multi-wave interactive sessions keep headroom
# deny_sandboxes = ["local"]    # spawned workers must never land un-sandboxed

[agents.codex]
kind = "acp"
command = "codex"
args = ["exec", "--cd", ".", "--add-dir", "{varda_project}", "--add-dir", "{varda_home}", "--sandbox", "workspace-write", "-"]
streams_output = true  # Codex streams live output; leave false/unset for buffered agents.
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
# interpreter_agent = "codex"  # optional: agent for post-interactive recap interpretation

[agents.copilot]
kind = "acp"
command = "sh"
args = ["-c", "copilot -p \"$(cat)\" --allow-all-tools --add-dir {project} --add-dir {varda_project} --add-dir {varda_home} -s"]
interactive_command = "sh"
interactive_args = ["-c", "copilot \"$(cat $VARDA_PROMPT_FILE)\" --allow-all-tools --add-dir {project} --add-dir {varda_project} --add-dir {varda_home}"]
resume_command_template = "copilot --resume={external_session_id} --add-dir {project} --add-dir {varda_project} --add-dir {varda_home} --allow-all-tools"

# Scoped credential examples: sources are host env vars, named secrets, or
# host-minted command output; targets are in-box env vars or read-only files.
# Secret NAMES only here — never paste a resolved secret value into config.toml.
#
# Static env source -> in-box env target.
# [[agents.claude.credentials]]
# from_env = "CLAUDE_SANDBOX_TOKEN"
# env = "ANTHROPIC_API_KEY"
#
# Secret-store source -> read-only file target.
# [[agents.claude.credentials]]
# from_secret = "gcp-service-account-json"
# file = "/home/agent/.config/gcloud/application_default_credentials.json"
#
# GCP deploy with an impersonated, host-minted token; run
# `gcloud run deploy SERVICE --source . --project "$GCLOUD_PROJECT"` in the box.
# [[agents.claude.credentials]]
# command = "gcloud auth print-access-token --impersonate-service-account=deployer@example-project.iam.gserviceaccount.com"
# env = "CLOUDSDK_AUTH_ACCESS_TOKEN"
# [[agents.claude.credentials]]
# command = "gcloud auth print-access-token --impersonate-service-account=deployer@example-project.iam.gserviceaccount.com"
# env = "GOOGLE_OAUTH_ACCESS_TOKEN"
#
# Terraform Cloud API token from the host secret store.
# [[agents.claude.credentials]]
# from_secret = "tfc-token"
# env = "TF_TOKEN_app_terraform_io"
#
# Azure DevOps PAT from the host secret store.
# [[agents.claude.credentials]]
# from_secret = "azdo-pat"
# env = "AZURE_DEVOPS_EXT_PAT"
#
# Azure CLI service principal via env, or a host-minted access token.
# [[agents.claude.credentials]]
# from_secret = "azure-client-id"
# env = "AZURE_CLIENT_ID"
# [[agents.claude.credentials]]
# from_secret = "azure-client-secret"
# env = "AZURE_CLIENT_SECRET"
# [[agents.claude.credentials]]
# from_secret = "azure-tenant-id"
# env = "AZURE_TENANT_ID"
# [[agents.claude.credentials]]
# command = "az account get-access-token --query accessToken -o tsv"
# env = "AZURE_TOKEN"
#
# Command-minted credential with refresh hint.
# [[agents.claude.credentials]]
# command = "security find-generic-password -w -s varda-sandbox-token"
# env = "CUSTOM_SANDBOX_TOKEN"
# refresh_seconds = 1800

[roles.tester]
backend = "codex"
instructions = """
You are the tester agent. Your role is to verify an implementation after the implementation agent has finished.

Tester workflow:
- Read the task, any attached plan, existing recaps, and the current project state before deciding what to test.
- Define a concise test plan in your recap before or while executing it.
- Execute the practical checks needed to verify the implementation, using the project's existing verification commands when available.
- Decide explicitly whether the original task is complete.
- If verification succeeds, state that the implementation is verified and what evidence supports that decision.
- If verification fails, update the task with the failed checks and required follow-up when the task file is writable. In all cases, include the failed checks, exact follow-up work, and the suggested next agent to re-run the task.
- Only request user interaction when verification is blocked by missing information, credentials, environment access, or a decision that an agent cannot make."""

[git]
auto_commit = true

# [sandboxes.devcontainer]
# image_from = "devcontainer"          # use .devcontainer image/build only
# primitive = "docker"
# env = { GCLOUD_PROJECT = "example-project" }
#
# [sandboxes.custom]
# build = "./Dockerfile.varda"         # build at prepare time and run that image
# primitive = "docker"
#
# [orchestration]                      # safe defaults when omitted: disabled
# enabled = true                       # allow sandboxed masters to request subtasks
# max_depth = 2
# max_fanout = 4                       # sibling cap per parent
# global_child_budget = 16
# deny_sandboxes = ["local"]           # spawned children must remain isolated
#
# Repo-local task definitions live under `.varda/tasks/`; repo workflow rules can
# live in `.varda/config.toml`. Runtime state stays in the central Varda home.
"#;

const OPERATIONS_README_CONTENT: &str = r#"# Varda Operations

This folder contains task files, agent recaps, and run records managed by Varda.

- `tasks/`: markdown tasks with YAML frontmatter, grouped by project folder.
- `recaps/`: end-user recaps produced by agents.
- `runs/`: run metadata and notification records.
"#;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub defaults: Defaults,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentConfig>,
    #[serde(default)]
    pub roles: BTreeMap<String, RoleConfig>,
    #[serde(default)]
    pub git: GitConfig,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sandboxes: BTreeMap<String, SandboxConfig>,
    /// Nested-orchestration (M8) defaults: whether a sandboxed master may request
    /// sub-task spawns and the caps that bound them. Safe default: spawning
    /// disabled, `local` sandbox denied. A `[[routes]]` entry may override this
    /// wholesale for the code it matches (see [`Config::resolve_orchestration_for`]).
    #[serde(
        default,
        skip_serializing_if = "crate::orchestration::OrchestrationPolicy::is_default"
    )]
    pub orchestration: crate::orchestration::OrchestrationPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoleConfig {
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Defaults {
    /// DEPRECATED single-session wall-clock ceiling. Retained as a back-compat
    /// alias: when `max_seconds` is unset, this feeds [`Defaults::effective_max_seconds`].
    /// The M10 cooperative-bounds model replaces the old hard-kill with an idle
    /// watchdog + auto-resume loop + soft budget, so this value no longer hard-kills
    /// a productive session mid-work.
    pub timeout_seconds: u64,
    pub operations_dir: String,
    /// M10 idle watchdog: cancel a session only after this many seconds of total
    /// silence (no stdout/stderr activity). Productive long runs never trip it; a
    /// wedged/hung child does. Default 180.
    #[serde(default = "default_idle_timeout_seconds")]
    pub idle_timeout_seconds: u64,
    /// M10 soft total ceiling across the WHOLE task (all auto-resume continuations).
    /// Accepts an integer number of seconds or the string `"none"` (no ceiling).
    /// Unset ⇒ fall back to the deprecated `timeout_seconds` alias. On exceed, the
    /// loop STOPS and the task is marked `needs_user` with the accumulated recap —
    /// never a mid-work kill.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_seconds: Option<MaxSeconds>,
    /// M10 max auto-resume hops (fresh continuation sessions) for one task.
    /// Default 0 = auto-resume OFF: `resume_command.is_some()` is not a reliable
    /// "more work" signal (a real agent's session is always resumable), so it is an
    /// explicit opt-in for workflows whose agent signals "done" by omitting a resume
    /// command.
    #[serde(default = "default_max_continuations")]
    pub max_continuations: u32,
    /// Reserved M10 tool-call budget across the whole task. `0` = unlimited.
    /// Non-zero values warn at run time but are not enforced yet because the
    /// current agent stream does not expose a reliable tool-call count.
    #[serde(default)]
    pub max_tool_calls: u64,
    /// Default sandbox provider applied to routes that do not set their own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// M6b hardening floor knobs — all clamp the UNTRUSTED `.varda` origin only;
    /// the central `config.toml` (routes/sandboxes) stays trusted.
    ///
    /// Allow a `.varda` to select `primitive = "local"` (escape the box). Default
    /// false: an attacker-influenceable `.varda` must never opt out of isolation.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_local_varda: bool,
    /// Allow a `.varda` mount to be writable. Default false: `.varda` mounts are
    /// forced `:ro`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_varda_writable_mounts: bool,
    /// Egress ceiling: if set, a `.varda` may not widen egress beyond this host
    /// allow-list. `None` ⇒ no ceiling clamp (still bounded by the trusted route).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_ceiling: Option<Vec<String>>,
    /// Curated, READ-ONLY identity/context mounts — the sanctioned way to tell the
    /// agent "who the user is" WITHOUT mounting credential dirs. Each entry is a
    /// specific FILE (never a dir) following the `source[:target][:mode]` grammar,
    /// e.g. `"~/.claude/CLAUDE.md:/root/CLAUDE.md:ro"`. The credential-file denylist
    /// still applies so a `.credentials.json` can never sneak in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identity_context: Vec<String>,
    /// M11 git-identity forwarding. When true, forward the host SSH agent SOCKET
    /// (`$SSH_AUTH_SOCK`) into the box so `git push` signs/authenticates on the
    /// host and private keys never enter it (clawk-style). Off by default.
    #[serde(default, skip_serializing_if = "is_false")]
    pub forward_ssh_agent: bool,
    /// M11 git-identity forwarding: read-only `user.name` / `user.email` handed to
    /// the box as `GIT_AUTHOR_*`/`GIT_COMMITTER_*` env so commits are attributed
    /// correctly without mounting `~/.gitconfig`. Unset ⇒ not forwarded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_user_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_user_email: Option<String>,
}

/// serde `skip_serializing_if` helper: omit `false` booleans so the default
/// config round-trips without emitting the new hardening keys.
fn is_false(value: &bool) -> bool {
    !*value
}

/// The two forms the M10 `max_seconds` soft ceiling may take in config/frontmatter:
/// an explicit integer number of seconds, or the keyword `"none"` (no ceiling).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MaxSeconds {
    /// `max_seconds = 1200`
    Seconds(u64),
    /// `max_seconds = "none"` — no soft total ceiling (loop until other bounds hit).
    Keyword(String),
}

fn default_idle_timeout_seconds() -> u64 {
    180
}

fn default_max_continuations() -> u32 {
    // Auto-resume is OFF by default. The loop's "more work" trigger is
    // `resume_command.is_some()`, but real headless agents (codex/copilot with a
    // resume template) produce a resume command on EVERY natural completion — a
    // session is always resumable — so a non-zero default would loop every task up
    // to the cap. It is an explicit opt-in for workflows whose agent signals "done"
    // by OMITTING a resume command.
    0
}

impl Defaults {
    /// Resolve the M10 soft total ceiling in seconds. `None` means "no ceiling"
    /// (the loop is bounded only by `max_continuations` / `max_tool_calls`).
    ///
    /// Precedence: an explicit `max_seconds` wins; when unset, fall back to the
    /// deprecated `timeout_seconds` alias (treating `0` as "no ceiling").
    ///
    /// Consumed by the M10 runner auto-resume loop (landing as the next increment)
    /// and by the config tests; `allow(dead_code)` until the runner side is wired.
    #[allow(dead_code)]
    pub fn effective_max_seconds(&self) -> Option<u64> {
        match &self.max_seconds {
            Some(over) => effective_max_seconds(over, self.timeout_seconds),
            // No explicit ceiling ⇒ fall back to the deprecated alias (0 ⇒ none).
            None => match self.timeout_seconds {
                0 => None,
                secs => Some(secs),
            },
        }
    }
}

/// Resolve a [`MaxSeconds`] soft ceiling into seconds, given the deprecated
/// `timeout_seconds` alias to fall back on. `None` means "no ceiling".
///
/// Shared by [`Defaults::effective_max_seconds`] and the per-task frontmatter
/// override path so both interpret the keyword/typo/`0` rules identically.
pub fn effective_max_seconds(max_seconds: &MaxSeconds, timeout_seconds: u64) -> Option<u64> {
    match max_seconds {
        MaxSeconds::Seconds(secs) => Some(*secs),
        MaxSeconds::Keyword(word) if word.eq_ignore_ascii_case("none") => None,
        // Any other keyword is treated as "no explicit ceiling" → fall through
        // to the deprecated alias so a typo never silently zeroes the budget.
        MaxSeconds::Keyword(_) => match timeout_seconds {
            0 => None,
            secs => Some(secs),
        },
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Route {
    pub glob: String,
    #[serde(default)]
    pub agents: Vec<String>,
    /// Sandbox provider for this route; overrides the default when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// Project-context mounts tied to the code↔context mapping (M6a). These
    /// compose with the image-intrinsic `[sandboxes.X].mounts` (effective set =
    /// their union). Each entry follows the `source[:target][:mode]` grammar
    /// parsed by [`crate::sandbox::parse_mount`]; both are trusted origins in
    /// M6a, so no hardening floor applies yet (that arrives with the untrusted
    /// `.varda` origin in M6b).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<String>,
    /// Trusted per-route static, non-secret environment variables. These are
    /// injected into sandboxed runs after expansion; more-specific origins win.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Per-route nested-orchestration policy. When set, it REPLACES the top-level
    /// `[orchestration]` defaults for tasks this route matches (so untrusted code
    /// can be pinned to a stricter — or, deliberately, a looser — spawn policy than
    /// the global default). Unset ⇒ inherit `Config::orchestration`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orchestration: Option<crate::orchestration::OrchestrationPolicy>,
}

/// How a sandbox's `egress` allow-list is ENFORCED — an explicit, honest name for
/// the difference between a name-resolution allow-list and a real network firewall.
///
/// The distinction matters because the two look identical in config (`egress =
/// [...]`) but give VERY different guarantees. The docker provider's allow-list is
/// DNS-only: it breaks ambient name resolution (`--dns 0.0.0.0`) and re-pins the
/// allow-listed hostnames (`--add-host`), so a *hostname* not on the list cannot
/// resolve — but an agent that already knows an IP can still open a raw connection
/// to ANY address. That is NOT a firewall, and must never be claimed as
/// clawk/microsandbox-equivalent.
///
/// `Strict` is the conservative DEFAULT: a non-empty allow-list is honored only when
/// the provider can actually firewall egress at the IP level (microsandbox/clawk).
/// The docker provider, which cannot enforce that portably, REFUSES a non-empty
/// strict allow-list at build time rather than silently downgrading to DNS-pin.
/// `DnsPin` is an explicit opt-in acknowledging the name-only, direct-IP-bypassable
/// guarantee. An EMPTY `egress` is fully offline (`--network none`) and is strict in
/// every mode.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum EgressMode {
    /// Real IP-level egress firewalling, or a hard refusal before launch. Conservative
    /// default: no silent downgrade to a weaker guarantee.
    #[default]
    Strict,
    /// Name-resolution allow-list only (docker `--dns 0.0.0.0` + `--add-host`). Blocks
    /// non-allow-listed HOSTNAMES but NOT direct-IP egress. Must be opted into explicitly.
    DnsPin,
}

/// Whether a sandbox `primitive` can enforce a non-empty egress allow-list at the IP
/// level (a real firewall), as opposed to only pinning hostnames. `microsandbox` and
/// `clawk` firewall egress via their net-rule backends; `docker` cannot do so
/// portably (its allow-list is DNS-pin only); `local` has no isolation at all.
pub fn primitive_enforces_strict_egress(primitive: &str) -> bool {
    matches!(primitive, "microsandbox" | "clawk")
}

/// Whether the spawn broker must be served over TCP (rather than a project-mounted
/// Unix socket) for this `primitive`.
///
/// Own-kernel microVM primitives (`microsandbox`, `clawk`) share the project tree
/// over virtio-fs, which exposes the socket *file* but not its AF_UNIX endpoint —
/// an in-guest `connect()` is refused. Those guests reach the host over TCP (their
/// default gateway) instead. `local` and shared-kernel `docker` see the real
/// socket through the bind mount, so they keep the Unix-socket transport.
///
/// NB: docker-on-a-VM (Colima / Docker Desktop) has the same virtio-fs limitation
/// as a microVM, but varda cannot portably tell a VM-backed docker host from a
/// native-Linux one, so `docker` stays on the unix socket here; a VM-backed docker
/// host that needs the broker should use `microsandbox`/`clawk`.
pub fn primitive_needs_tcp_broker(primitive: &str) -> bool {
    matches!(primitive, "microsandbox" | "clawk")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Path to a Dockerfile to build the sandbox image from. When set, the
    /// docker provider builds it at `prepare()` and uses the resulting tag.
    /// Mutually exclusive-ish with `image` (build wins when both are set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// External image source. Currently only `"devcontainer"`: the docker
    /// provider discovers the project's `.devcontainer/devcontainer.json` (or
    /// `.devcontainer.json`) at `prepare()` and derives the run image from its
    /// `image` field (used verbatim) or its `build.dockerfile` + `build.context`
    /// (`docker build`-ed). This is an IMAGE SOURCE only — varda takes the
    /// image/build and NOTHING else: a devcontainer's `mounts`, `runArgs`,
    /// docker-socket forwarding, and lifecycle hooks (`postCreateCommand`, …) are
    /// deliberately ignored so varda keeps sole control of mounts, egress, and
    /// creds (the M2/M3 isolation invariant). `image_from` wins over an explicit
    /// `image`/`build` when both are set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_from: Option<String>,
    /// Isolation primitive: `"docker"` | `"microsandbox"` | `"clawk"` | `"local"`.
    /// Orthogonal to the image/rootfs: the same OCI image can run under docker
    /// (shared kernel) or an own-kernel microVM.
    #[serde(default = "default_primitive")]
    pub primitive: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<String>,
    /// Trusted image-intrinsic static, non-secret environment variables.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub egress: Vec<String>,
    /// How `egress` is ENFORCED. Defaults to [`EgressMode::Strict`] (fail-closed): a
    /// non-empty allow-list under a provider that cannot IP-firewall (docker) is
    /// refused at build time rather than silently treated as a DNS-only allow-list.
    /// Set `egress_mode = "dns-pin"` to explicitly accept the name-only guarantee.
    #[serde(default)]
    pub egress_mode: EgressMode,
}

/// Default isolation primitive when a `[sandboxes.<name>]` entry omits one.
pub fn default_primitive() -> String {
    "docker".to_owned()
}

/// Filename of the folder-local, repo-committed (UNTRUSTED) sandbox config.
/// Resolved into the live run path by [`Config::resolve_sandbox_for`], which is
/// invoked from `build_client` when a task carries a project path.
pub const VARDA_FILE: &str = ".varda";

/// A parsed `.varda` file. It carries a single `sandbox` key that is EITHER a
/// reference to a central `[sandboxes.X]` (string) OR an inline, self-contained
/// `[sandbox]` block (table). UNTRUSTED — always clamped by the M6b hardening
/// floor via [`resolve_sandbox_for`] before use.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct VardaFile {
    pub sandbox: VardaSandbox,
}

/// The two forms a `.varda` `sandbox` value may take.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum VardaSandbox {
    /// `sandbox = "rust"` — select a central `[sandboxes.rust]`.
    Reference(String),
    /// `[sandbox]` block — a self-contained sandbox definition.
    Inline(SandboxConfig),
}

/// The fully-resolved sandbox for a task path, after walk-up + precedence +
/// (for the untrusted `.varda` origin) the hardening floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSandbox {
    /// Effective sandbox name: a central name, `"inline"` for an inline `.varda`,
    /// or `"local"`.
    pub name: String,
    /// Effective sandbox config (central table entry, inline `.varda`, or a
    /// synthetic `local`).
    pub config: SandboxConfig,
    /// Trusted project-context route mounts (origin `Route`).
    pub route_mounts: Vec<String>,
    /// UNTRUSTED, already-hardened `.varda` inline mounts (origin `Varda`), each a
    /// `source:target:mode` string ready to apply (source made absolute, forced
    /// `:ro` unless allowed).
    pub varda_mounts: Vec<String>,
    /// Merged static env from trusted sandbox/route origins plus the hardened
    /// `.varda` origin. Agent env is merged later by `AcpSubprocessClient`.
    pub env: BTreeMap<String, String>,
    /// Keys supplied by the untrusted `.varda` origin, retained so the run path
    /// can reject collisions with agent-specific credential injection targets.
    pub varda_env_keys: Vec<String>,
    /// Path of the `.varda` that supplied the config, when one was used.
    pub varda_file: Option<PathBuf>,
}

/// Walk UP from `start` (inclusive) to `routing_root` (inclusive) and return the
/// nearest existing `.varda` file. `None` when none is found in range.
pub fn find_nearest_varda(start: &Path, routing_root: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_file() {
        start.parent()?
    } else {
        start
    };
    loop {
        let candidate = dir.join(VARDA_FILE);
        if candidate.is_file() {
            return Some(candidate);
        }
        if dir == routing_root {
            return None;
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return None,
        }
    }
}

impl Config {
    /// Resolve the effective sandbox for a task at `project_path`, honoring the
    /// precedence `task-pinned → nearest .varda → central route (glob) →
    /// defaults.sandbox → "local"` and clamping the untrusted `.varda` origin with
    /// the M6b hardening floor. `routing_root` bounds the upward `.varda` walk.
    ///
    /// `pinned` carries the task-frontmatter `sandbox` override (`varda task add
    /// --sandbox <NAME>`). When set it wins over EVERY other origin: the named
    /// central `[sandboxes.<NAME>]` (or `"local"`) is used directly, the `.varda`
    /// walk is skipped, and the route's project-context mounts/env are still
    /// applied. The name must resolve to a configured sandbox (or `"local"`),
    /// otherwise a clear error is returned.
    pub fn resolve_sandbox_for(
        &self,
        project_path: &Path,
        routing_root: &Path,
        pinned: Option<&str>,
    ) -> Result<ResolvedSandbox> {
        // Trusted baseline from the central route: its glob-selected sandbox name
        // and project-context mounts. Used directly when no `.varda` applies.
        let route = crate::routing::find_route_public(self, project_path).ok();
        let route_mounts = route.map(|r| r.mounts.clone()).unwrap_or_default();
        let route_env = route.map(|r| r.env.clone()).unwrap_or_default();
        let central_name = route
            .and_then(|r| r.sandbox.clone())
            .or_else(|| self.defaults.sandbox.clone())
            .unwrap_or_else(|| DEFAULT_SANDBOX_PROVIDER.to_owned());

        // Task-pinned sandbox wins over `.varda`, route, and defaults. It is a
        // TRUSTED origin (set by the operator via `--sandbox`), so no `.varda`
        // hardening floor applies — but the name must exist in config.
        if let Some(name) = pinned {
            if name != DEFAULT_SANDBOX_PROVIDER && !self.sandboxes.contains_key(name) {
                bail!(
                    "task-pinned sandbox '{name}' is not configured; \
                     add a `[sandboxes.{name}]` entry or use `local`"
                );
            }
            let config = self.sandbox_config_by_name(name);
            let env = merge_static_env(&config.env, &route_env, &BTreeMap::new());
            return Ok(ResolvedSandbox {
                name: name.to_owned(),
                config,
                route_mounts,
                varda_mounts: Vec::new(),
                env,
                varda_env_keys: Vec::new(),
                varda_file: None,
            });
        }

        let Some(varda_path) = find_nearest_varda(project_path, routing_root) else {
            let config = self.sandbox_config_by_name(&central_name);
            let env = merge_static_env(&config.env, &route_env, &BTreeMap::new());
            return Ok(ResolvedSandbox {
                name: central_name,
                config,
                route_mounts,
                varda_mounts: Vec::new(),
                env,
                varda_env_keys: Vec::new(),
                varda_file: None,
            });
        };

        let text = fs::read_to_string(&varda_path)
            .with_context(|| format!("failed to read `.varda` at {}", varda_path.display()))?;
        let parsed: VardaFile = toml::from_str(&text)
            .with_context(|| format!("failed to parse `.varda` at {}", varda_path.display()))?;
        let varda_dir = varda_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        match parsed.sandbox {
            VardaSandbox::Reference(name) => {
                let config = self.sandbox_config_by_name(&name);
                self.enforce_varda_primitive_floor(&config.primitive, &varda_path)?;
                let env = merge_static_env(&config.env, &route_env, &BTreeMap::new());
                Ok(ResolvedSandbox {
                    name,
                    config,
                    route_mounts,
                    varda_mounts: Vec::new(),
                    env,
                    varda_env_keys: Vec::new(),
                    varda_file: Some(varda_path),
                })
            }
            VardaSandbox::Inline(config) => {
                self.enforce_varda_primitive_floor(&config.primitive, &varda_path)?;
                self.enforce_egress_ceiling(&config.egress, &varda_path)?;
                self.enforce_varda_env_floor(&config.env, &route_env, &varda_path)?;
                let varda_mounts = self.harden_inline_varda_mounts(
                    &config.mounts,
                    project_path,
                    &varda_dir,
                    &varda_path,
                )?;
                let env = merge_static_env(&BTreeMap::new(), &route_env, &config.env);
                let varda_env_keys = config.env.keys().cloned().collect();
                let config = SandboxConfig {
                    mounts: Vec::new(),
                    env: BTreeMap::new(),
                    ..config
                };
                Ok(ResolvedSandbox {
                    name: "inline".to_owned(),
                    config,
                    route_mounts,
                    varda_mounts,
                    env,
                    varda_env_keys,
                    varda_file: Some(varda_path),
                })
            }
        }
    }

    /// Resolve the effective nested-orchestration policy for a task at
    /// `project_path`: the glob-matched route's `orchestration` override when it
    /// sets one, otherwise the top-level `[orchestration]` defaults. Consulted by
    /// the run path when standing up the `spawn_subtask` broker for a sandboxed
    /// master, so every live spawn is gated by exactly the policy that governs the
    /// code being worked on.
    ///
    pub fn resolve_orchestration_for(
        &self,
        project_path: &Path,
    ) -> crate::orchestration::OrchestrationPolicy {
        crate::routing::find_route_public(self, project_path)
            .ok()
            .and_then(|route| route.orchestration.clone())
            .unwrap_or_else(|| self.orchestration.clone())
    }

    fn sandbox_config_by_name(&self, name: &str) -> SandboxConfig {
        if name == DEFAULT_SANDBOX_PROVIDER {
            return SandboxConfig {
                primitive: DEFAULT_SANDBOX_PROVIDER.to_owned(),
                ..SandboxConfig::default()
            };
        }
        self.sandboxes.get(name).cloned().unwrap_or_default()
    }

    /// Floor: an untrusted `.varda` may not select `primitive = "local"` (escape
    /// the box) unless `defaults.allow_local_varda`.
    fn enforce_varda_primitive_floor(&self, primitive: &str, varda_file: &Path) -> Result<()> {
        if primitive == "local" && !self.defaults.allow_local_varda {
            bail!(
                "`.varda` at {} selects `primitive = \"local\"` (escapes the sandbox); \
                 refused unless `defaults.allow_local_varda = true`",
                varda_file.display()
            );
        }
        Ok(())
    }

    /// Floor: an untrusted `.varda` may not widen egress beyond
    /// `defaults.egress_ceiling` (when set).
    fn enforce_egress_ceiling(&self, egress: &[String], varda_file: &Path) -> Result<()> {
        if let Some(ceiling) = &self.defaults.egress_ceiling {
            for host in egress {
                if !ceiling.iter().any(|allowed| allowed == host) {
                    bail!(
                        "`.varda` at {} requests egress host '{host}' beyond `defaults.egress_ceiling`",
                        varda_file.display()
                    );
                }
            }
        }
        Ok(())
    }

    /// Floor: an untrusted `.varda` env map may not set reserved process-control
    /// keys or override any trusted central route key. Agent-specific credential
    /// targets are checked later in the run path, where the selected agent is known.
    fn enforce_varda_env_floor(
        &self,
        env: &BTreeMap<String, String>,
        route_env: &BTreeMap<String, String>,
        varda_file: &Path,
    ) -> Result<()> {
        for key in env.keys() {
            if is_reserved_varda_env_key(key) {
                bail!(
                    "`.varda` at {} declares env key '{key}', which is reserved and may not be set by `.varda`",
                    varda_file.display()
                );
            }
            if route_env.contains_key(key) {
                bail!(
                    "`.varda` at {} declares env key '{key}', which would override trusted route env",
                    varda_file.display()
                );
            }
        }
        Ok(())
    }

    /// Floor: harden each inline `.varda` mount (in-tree SOURCE, credential
    /// denylist, forced `:ro` unless allowed, safe TARGET) and return them as
    /// ready-to-apply `source:target:mode` strings.
    fn harden_inline_varda_mounts(
        &self,
        mounts: &[String],
        project_root: &Path,
        varda_dir: &Path,
        varda_file: &Path,
    ) -> Result<Vec<String>> {
        let mut out = Vec::with_capacity(mounts.len());
        for raw in mounts {
            let spec = crate::sandbox::parse_mount(raw).with_context(|| {
                format!("invalid `.varda` mount '{raw}' in {}", varda_file.display())
            })?;
            // `.varda` mount paths are relative to the `.varda` dir, not the
            // project root; make SOURCE absolute against `varda_dir` first.
            let source = if spec.source.is_absolute() {
                spec.source.clone()
            } else {
                varda_dir.join(&spec.source)
            };
            crate::sandbox::check_credential_denylist(&source)?;
            let abs_spec = crate::sandbox::MountSpec {
                source,
                target: spec.target.clone(),
                writable: spec.writable,
            };
            let hardened = crate::sandbox::harden_varda_mount(
                &abs_spec,
                project_root,
                self.defaults.allow_varda_writable_mounts,
                varda_file,
            )?;
            let mode = if hardened.writable { "rw" } else { "ro" };
            out.push(format!(
                "{}:{}:{mode}",
                hardened.source.display(),
                hardened.target.display()
            ));
        }
        Ok(out)
    }
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            image: None,
            build: None,
            image_from: None,
            primitive: default_primitive(),
            mounts: Vec::new(),
            env: BTreeMap::new(),
            egress: Vec::new(),
            egress_mode: EgressMode::Strict,
        }
    }
}

fn merge_static_env(
    sandbox_env: &BTreeMap<String, String>,
    route_env: &BTreeMap<String, String>,
    varda_env: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.extend(sandbox_env.clone());
    env.extend(route_env.clone());
    env.extend(varda_env.clone());
    env
}

pub fn is_reserved_varda_env_key(key: &str) -> bool {
    matches!(
        key,
        "PATH" | "HOME" | "LD_PRELOAD" | "LD_LIBRARY_PATH" | "SSH_AUTH_SOCK"
    ) || key.starts_with("DYLD_")
        || key.starts_with("VARDA_")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentConfig {
    pub kind: AgentKind,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_prompt_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Whether the agent streams stdout/stderr while working. When `true`, Varda's
    /// idle watchdog may treat session-log growth as a liveness signal. When
    /// `false` or unset, the agent is buffered-safe: no log growth alone is not
    /// evidence that the child is wedged, so only natural process exit or the
    /// cumulative `max_seconds` ceiling stops it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streams_output: Option<bool>,
    /// M11 auth-token injection. Name of a HOST env var (resolved from the
    /// environment / a secret store like `fnox`, NEVER a raw secret in the repo)
    /// holding a DEDICATED, scoped, rotatable sandbox token — not the user's
    /// primary credential. At `prepare` its value is read and injected into the
    /// box as a scoped env var so the agent boots already authenticated, without
    /// mounting `~/.claude`/`~/.codex`/etc. Cross-ref: the stronger "agent never
    /// holds the key" form is the M8 capability-broker (out of scope here).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token_env: Option<String>,
    /// In-box env var name the agent reads its token from (e.g.
    /// `ANTHROPIC_API_KEY`). Defaults to `auth_token_env` when unset, so the same
    /// name can be re-exported into the box.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token_target: Option<String>,
    /// M11-ext — a LIST of scoped credential injections (`[[agents.X.credentials]]`).
    /// Each entry names exactly one SOURCE (host env var, host secret store, or a
    /// host command whose stdout is a short-lived token) and exactly one TARGET
    /// (a scoped in-box env var, or a read-only file staged in the guest). The
    /// legacy `auth_token_env`/`auth_token_target` pair is one-entry sugar over
    /// this list (see [`AgentConfig::effective_credentials`]). Sources live only in
    /// the TRUSTED central config, never `.varda`; a credential DIR is never mounted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub credentials: Vec<CredentialConfig>,
    /// Command to use when running interactively (inherits terminal stdio).
    /// When set, the agent is spawned with all streams inherited from the terminal
    /// and $VARDA_PROMPT_FILE points to a file containing the task prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactive_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactive_args: Option<Vec<String>>,
    /// Template used to build the resume command after an interactive session ends.
    /// `{external_session_id}` is replaced with the agent's own session id (discovered
    /// from the agent's session storage), and `{project}` with the task's project path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_command_template: Option<String>,
    /// Agent used for the post-session interpretation/finalization pass over an
    /// interactive run's session log (M13a §7). That pass only reads the host
    /// session log and emits a recap — no untrusted exec — so it always runs
    /// UN-sandboxed and local. A bare `sh` interactive command cannot produce a
    /// Varda recap on its own, so when this is unset it defaults to the SAME agent
    /// that drove the interactive session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interpreter_agent: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Acp,
}

/// One scoped credential injection for a sandboxed agent (M11-ext).
///
/// Each entry names exactly one **source** — where the scoped value is minted on
/// the HOST at `prepare` time — and exactly one **target** — how the (minimal,
/// scoped) value is exposed INSIDE the box. A credential DIR is never mounted; only
/// the resolved value crosses the boundary.
///
/// Sources belong in the TRUSTED central config only. `.varda` may reference secret
/// NAMES (`from_secret`) but must never carry a raw value or a `command`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct CredentialConfig {
    /// SOURCE: read the value from this HOST env var at prepare time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_env: Option<String>,
    /// SOURCE: read the value from this named secret in the host secret store
    /// (`fnox` / Proton Pass), resolved by `fnox get <name>` at prepare time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_secret: Option<String>,
    /// SOURCE: run this command on the HOST at prepare time and use its stdout
    /// (trailing newline trimmed) — for host-minted, least-privilege short-lived
    /// tokens. The minting identity stays on the host; the box only sees the result.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// TARGET (default): inject the value as this scoped in-box env var.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    /// TARGET: stage the value as a read-only file at this absolute GUEST path
    /// (via the session's `stage_file`; cleaned on teardown).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Re-mint (`command` sources) after this many seconds for long interactive
    /// sessions. Parsed and preserved for forward-compat; periodic refresh is a
    /// follow-up — today the value is minted ONCE at `prepare`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_seconds: Option<u64>,
}

/// The validated SOURCE of a [`CredentialConfig`] (exactly one is set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource<'a> {
    /// Host env var name.
    Env(&'a str),
    /// Host secret-store name (`fnox`).
    Secret(&'a str),
    /// Host command whose stdout is the value.
    Command(&'a str),
}

/// The validated TARGET of a [`CredentialConfig`] (exactly one is set).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialTarget<'a> {
    /// Scoped in-box env var name.
    Env(&'a str),
    /// Absolute guest path for a staged read-only file.
    File(&'a str),
}

impl CredentialConfig {
    /// Validate and return the single source, erroring when none or more than one is set.
    pub fn source(&self) -> Result<CredentialSource<'_>> {
        let set = self.from_env.is_some() as u8
            + self.from_secret.is_some() as u8
            + self.command.is_some() as u8;
        if set == 0 {
            bail!(
                "credential entry has no source: set exactly one of `from_env`, `from_secret`, or `command`"
            );
        }
        if set > 1 {
            bail!(
                "credential entry sets multiple sources: use exactly one of `from_env`, `from_secret`, or `command`"
            );
        }
        if let Some(name) = &self.from_env {
            Ok(CredentialSource::Env(name))
        } else if let Some(name) = &self.from_secret {
            Ok(CredentialSource::Secret(name))
        } else {
            Ok(CredentialSource::Command(
                self.command.as_deref().expect("command set"),
            ))
        }
    }

    /// Validate and return the single target, erroring when none or both are set.
    pub fn target(&self) -> Result<CredentialTarget<'_>> {
        match (self.env.as_deref(), self.file.as_deref()) {
            (Some(_), Some(_)) => {
                bail!("credential entry sets both `env` and `file`: choose exactly one target")
            }
            (Some(env), None) => Ok(CredentialTarget::Env(env)),
            (None, Some(file)) => Ok(CredentialTarget::File(file)),
            (None, None) => {
                bail!("credential entry has no target: set exactly one of `env` or `file`")
            }
        }
    }
}

impl AgentConfig {
    /// Effective credential list: the explicit `[[agents.X.credentials]]` entries,
    /// plus the legacy `auth_token_env`/`auth_token_target` single-token pair folded
    /// in as one-entry sugar (`from_env` → `env`, defaulting the target name to the
    /// source name). The legacy entry is appended last so explicit entries win on a
    /// duplicate target during resolution.
    pub fn effective_credentials(&self) -> Vec<CredentialConfig> {
        let mut creds = self.credentials.clone();
        if let Some(src) = &self.auth_token_env {
            creds.push(CredentialConfig {
                from_env: Some(src.clone()),
                env: Some(
                    self.auth_token_target
                        .clone()
                        .unwrap_or_else(|| src.clone()),
                ),
                ..Default::default()
            });
        }
        creds
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitConfig {
    #[serde(default = "default_auto_commit")]
    pub auto_commit: bool,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            auto_commit: default_auto_commit(),
        }
    }
}

impl Config {
    /// Resolve the effective sandbox provider for a route.
    ///
    /// Precedence: `route.sandbox` → `defaults.sandbox` → [`DEFAULT_SANDBOX_PROVIDER`].
    pub fn effective_sandbox<'a>(&'a self, route: &'a Route) -> &'a str {
        route
            .sandbox
            .as_deref()
            .or(self.defaults.sandbox.as_deref())
            .unwrap_or(DEFAULT_SANDBOX_PROVIDER)
    }
}

// ---------------------------------------------------------------------------
// Sandboxed-resident route enforcement (#461 capstone — `varda orchestrate`).
//
// The self-hosting orchestrator (the RESIDENT) runs inside a sandbox with a
// dedicated workspace mounted rw. These functions assert, IN CODE (not just
// docs), the load-bearing security gates before the resident is allowed to
// launch. A violation is a loud, refused launch — never a silent downgrade.
// ---------------------------------------------------------------------------

/// In-box env var NAMES that would hand the sandbox a `git push` credential — OR a
/// channel through which one can be injected (an askpass helper, an `SSH_AUTH_SOCK`
/// forward, a `GIT_CONFIG_*` override that installs a credential helper). A resident
/// receiving any of these — via a credential target OR via a plain `[…].env` map —
/// could authenticate a push to a remote, which the sandboxed-resident model forbids
/// (pushing is a separate, human-gated HOST step). Matched case-insensitively.
///
/// `GIT_CONFIG_KEY_*` / `GIT_CONFIG_VALUE_*` are matched by prefix in
/// [`env_key_enables_push`], not listed here, because their suffix is unbounded.
pub const PUSH_CREDENTIAL_ENV_TARGETS: &[&str] = &[
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "GITLAB_TOKEN",
    "GIT_TOKEN",
    "GIT_PASSWORD",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "GIT_SSH_COMMAND",
    "GIT_SSH",
    "SSH_AUTH_SOCK",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_TERMINAL_PROMPT",
];

/// Substrings of a staged-file TARGET path that would hand the sandbox a git push
/// credential: an SSH private key, a `*credential*` store file (`.git-credentials`,
/// `~/.config/git/credentials`, any `credential.helper = store --file <path>`
/// target), the gh CLI token store (`~/.config/gh/hosts.yml`), an askpass/credential
/// helper SCRIPT, or a netrc. Broad on purpose — a resident that stages ANY of these
/// as a file is carrying a push credential regardless of the exact filename.
pub const PUSH_CREDENTIAL_FILE_MARKERS: &[&str] = &[
    "credential", // .git-credentials, .config/git/credentials, *-credential-store, helper stores
    ".netrc",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "/.ssh/",
    ".config/gh", // gh CLI hosts.yml token store (any file under .config/gh)
    "askpass",    // askpass / credential-helper scripts
];

/// True when an in-box env var NAMED `key` would hand the sandbox a push credential
/// or a channel to inject one. Covers the fixed [`PUSH_CREDENTIAL_ENV_TARGETS`] list
/// plus the unbounded `GIT_CONFIG_KEY_*` / `GIT_CONFIG_VALUE_*` family (which can set
/// `credential.helper` from the environment). Case-insensitive.
pub fn env_key_enables_push(key: &str) -> bool {
    if PUSH_CREDENTIAL_ENV_TARGETS
        .iter()
        .any(|known| known.eq_ignore_ascii_case(key))
    {
        return true;
    }
    let upper = key.to_ascii_uppercase();
    upper.starts_with("GIT_CONFIG_KEY_") || upper.starts_with("GIT_CONFIG_VALUE_")
}

/// The first push-enabling key present in `env`, if any. `env` is the resident's
/// EFFECTIVE env (agent + sandbox + route + `.varda`, merged with the same
/// precedence as the real launch) — presence of the key is what matters, not its
/// origin or value, so a push-enabling var slipped in through ANY `[…].env` map is
/// caught here even though it is not a `[[credentials]]` entry.
pub fn env_enables_push(env: &BTreeMap<String, String>) -> Option<String> {
    env.keys().find(|key| env_key_enables_push(key)).cloned()
}

/// True when injecting `cred` into the box would give it a credential capable of
/// authenticating `git push` to a remote. Conservative: an unresolvable/ambiguous
/// target is treated as NOT a push cred here (the `target()` validation runs
/// elsewhere) so this never masks a config error — it only classifies valid ones.
pub fn credential_enables_push(cred: &CredentialConfig) -> bool {
    match cred.target() {
        Ok(CredentialTarget::Env(name)) => env_key_enables_push(name),
        Ok(CredentialTarget::File(path)) => {
            let lower = path.to_ascii_lowercase();
            PUSH_CREDENTIAL_FILE_MARKERS
                .iter()
                .any(|marker| lower.contains(marker))
        }
        Err(_) => false,
    }
}

/// Return the resident egress allowlist for one backend agent.
///
/// This is intentionally NOT a worker sandbox policy. Workers may use whatever
/// egress their trusted route/user configuration permits. The long-lived resident
/// is stricter because allowing broad GitHub domains there can create a push or
/// exfiltration path from the orchestrator workspace.
pub fn resident_egress_allowlist_for_agent(agent: &str) -> Result<&'static [&'static str]> {
    let a = agent.to_ascii_lowercase();
    match a.as_str() {
        "claude" => Ok(CLAUDE_RESIDENT_EGRESS_ALLOWLIST),
        "codex" | "openai" => Ok(CODEX_RESIDENT_EGRESS_ALLOWLIST),
        "copilot" if !COPILOT_RESIDENT_EGRESS_ALLOWLIST.is_empty() => {
            Ok(COPILOT_RESIDENT_EGRESS_ALLOWLIST)
        }
        "copilot" => bail!(
            "Copilot resident sandbox egress is unsupported until exact non-push Copilot auth/API endpoints are known; \
             do not add blanket `github.com` to resident egress. Use Claude/Codex for `varda orchestrate`, or run \
             Copilot only as an ordinary worker sandbox with explicit route/user-approved egress."
        ),
        // Custom, operator-configured resident agents (trusted config) inherit the
        // endpoint policy of the LLM family in their name, e.g. `claude-resident`.
        // Copilot stays fail-closed. (Follow-up: a config-declared endpoint set.)
        _ if a.contains("copilot") => bail!(
            "Copilot resident sandbox egress is unsupported until exact non-push Copilot auth/API endpoints are known."
        ),
        _ if a.contains("claude") => Ok(CLAUDE_RESIDENT_EGRESS_ALLOWLIST),
        _ if a.contains("codex") || a.contains("openai") => Ok(CODEX_RESIDENT_EGRESS_ALLOWLIST),
        other => bail!(
            "resident endpoint policy for agent '{other}' is not configured; add exact LLM endpoints before using it as a sandboxed resident"
        ),
    }
}

/// Inspect a workspace's `.git/config` (and any submodule configs under
/// `.git/modules`) for a pre-seeded push credential: a remote URL with an EMBEDDED
/// credential (`https://x-access-token:TOKEN@…`, `https://user:pass@…`) or a
/// configured `credential.helper`. A resident that *merges* worker branches is fine;
/// a workspace whose `.git/config` already carries a token-bearing remote or a
/// credential helper hands the box a push credential and is REFUSED.
///
/// Absent/unreadable configs are treated as clean (a bare workspace has no `.git`);
/// this only classifies configs it can read.
pub fn enforce_workspace_git_config(workspace: &Path) -> Result<()> {
    let git_dir = workspace.join(".git");
    inspect_git_config_file(&git_dir.join("config"), workspace)?;
    let modules = git_dir.join("modules");
    if modules.is_dir() {
        inspect_git_config_tree(&modules, workspace, 0)?;
    }
    Ok(())
}

/// Bounded recursive walk of a `.git/modules` tree, inspecting every `config` file.
fn inspect_git_config_tree(dir: &Path, workspace: &Path, depth: usize) -> Result<()> {
    if depth > 12 {
        return Ok(());
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            inspect_git_config_tree(&path, workspace, depth + 1)?;
        } else if path.file_name().and_then(|n| n.to_str()) == Some("config") {
            inspect_git_config_file(&path, workspace)?;
        }
    }
    Ok(())
}

/// Read one git config file and reject an embedded-credential remote or a configured
/// credential helper. Missing/unreadable file ⇒ clean.
fn inspect_git_config_file(path: &Path, workspace: &Path) -> Result<()> {
    let Ok(text) = fs::read_to_string(path) else {
        return Ok(());
    };
    if let Some(reason) = git_config_text_has_push_credential(&text) {
        bail!(
            "orchestration workspace {} has a pre-seeded push credential in its git config ({reason}); \
             the sandboxed resident MUST NOT carry a credential that can push to a remote. Remove the \
             embedded-credential remote / credential helper — pushing is a separate, human-gated host step.",
            workspace.display()
        );
    }
    Ok(())
}

/// Classify git-config TEXT (INI-like) for a push credential. Returns a redacted
/// reason (never the secret itself) when the config embeds a credential in a remote
/// URL or configures a `credential.helper`.
fn git_config_text_has_push_credential(text: &str) -> Option<String> {
    let mut section = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') {
            section = line
                .trim_start_matches('[')
                .trim_end_matches(']')
                .trim()
                .to_ascii_lowercase();
            continue;
        }
        let (key, value) = match line.split_once('=') {
            Some((k, v)) => (k.trim().to_ascii_lowercase(), v.trim()),
            None => (line.to_ascii_lowercase(), ""),
        };
        // `[credential]` / `[credential "https://…"]` with a non-empty helper.
        if section.starts_with("credential") && key == "helper" && !value.is_empty() {
            return Some("credential.helper is configured".to_owned());
        }
        // A remote (or any) URL that embeds userinfo (`user[:pass]@host`).
        if key == "url" && url_embeds_credential(value) {
            return Some("remote url embeds an inline credential".to_owned());
        }
    }
    None
}

/// True when `url` carries a non-empty userinfo component (`scheme://userinfo@host…`)
/// — e.g. `https://x-access-token:TOKEN@github.com/…` or `https://user:pass@…`. The
/// userinfo value itself is never returned, so callers cannot leak the secret.
fn url_embeds_credential(url: &str) -> bool {
    let Some((_, after_scheme)) = url.split_once("://") else {
        return false;
    };
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    match authority.split_once('@') {
        Some((userinfo, _host)) => !userinfo.is_empty(),
        None => false,
    }
}

/// Refuse a workspace mount of `$HOME` itself or any ancestor of `$HOME` (e.g.
/// `/`, `/Users`, `/home`). The orchestration workspace must be a DEDICATED
/// directory so the blast radius stays bounded to un-pushed work — mounting a
/// home-ancestor rw would expose credential stores and the whole dev tree.
pub fn enforce_dedicated_workspace(workspace: &Path) -> Result<()> {
    if !workspace.is_absolute() {
        bail!(
            "orchestration workspace {} must be an absolute path",
            workspace.display()
        );
    }
    let ws = canonical_or_self(workspace);
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        let home = canonical_or_self(&home);
        if ws == home {
            bail!(
                "orchestration workspace {} is $HOME; use a dedicated directory (never your home or dev tree)",
                workspace.display()
            );
        }
        if home.starts_with(&ws) {
            bail!(
                "orchestration workspace {} is an ancestor of $HOME ({}); use a dedicated directory",
                workspace.display(),
                home.display()
            );
        }
    }
    Ok(())
}

/// Best-effort canonicalization for prefix comparison; falls back to the path as
/// written when it does not yet exist on the host.
fn canonical_or_self(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// True when `mounts` (merged `source[:target][:mode]` strings) mount `workspace`
/// as a READ-WRITE bind — the resident must be able to write its merged result to
/// the host-visible mount.
pub fn workspace_mounted_rw(mounts: &[String], workspace: &Path) -> bool {
    let ws = canonical_or_self(workspace);
    mounts.iter().any(|raw| {
        let Ok(spec) = crate::sandbox::parse_mount(raw) else {
            return false;
        };
        spec.writable && canonical_or_self(&spec.source) == ws
    })
}

/// Assert every load-bearing gate of the sandboxed-resident model before launch.
///
/// - **G1** — `workspace` is a dedicated dir (not `$HOME`/home-ancestor) and is
///   mounted **rw** in `mounts`.
/// - **G2** — the sandbox is **isolating** (`primitive != "local"`, name != `local`),
///   **net-restricted** (every `egress` host must be in the selected agent's exact
///   LLM endpoint allowlist; `github.com`/general hosts stay denied so there is no
///   push or exfiltration; an empty `egress` ⇒ `--network none` still passes), and
///   the resident identity carries **no push credential**. "No push credential"
///   spans every channel a credential can reach the box through: a forwarded SSH
///   agent, a push-capable `credentials` target, a push-enabling key in the
///   resident's EFFECTIVE `env` (agent + sandbox + route + `.varda`, per
///   `effective_env`), or a pre-seeded push credential in the workspace's
///   `.git/config`.
/// - **G7** — orchestration is enabled (so the broker is wired) and `local` is in
///   `deny_sandboxes` (spawned workers can never land un-sandboxed).
///
/// Any violation is a hard, refused launch. Net-deny already contains an actual push
/// today; this CLAIMED gate is defense-in-depth — if net-deny is ever misconfigured,
/// the resident must still hold no push credential.
#[allow(clippy::too_many_arguments)]
pub fn enforce_resident_launch(
    resident_agent: &str,
    sandbox_name: &str,
    sandbox: &SandboxConfig,
    mounts: &[String],
    workspace: &Path,
    credentials: &[CredentialConfig],
    effective_env: &BTreeMap<String, String>,
    forward_ssh_agent: bool,
    orchestration: &crate::orchestration::OrchestrationPolicy,
) -> Result<()> {
    // G2 — isolating sandbox. A resident about to launch un-sandboxed is refused.
    if sandbox_name == DEFAULT_SANDBOX_PROVIDER || sandbox.primitive == "local" {
        bail!(
            "resident route resolved to an un-sandboxed provider (name='{sandbox_name}', primitive='{}'); \
             the orchestrator MUST run in an isolating sandbox (docker/microsandbox). Set an isolating \
             `sandbox` on the workspace route.",
            sandbox.primitive
        );
    }

    // G2 — clawk-parity egress semantics. A non-empty resident allow-list is only
    // acceptable when the provider can close the direct-IP bypass surface. Docker's
    // compatibility mode is DNS-pin only (`--dns 0.0.0.0` + `--add-host`), so it
    // blocks undeclared hostnames but still permits raw direct-IP egress on the
    // bridge network. For residents, do not silently downgrade that guarantee.
    if !sandbox.egress.is_empty() {
        if sandbox.egress_mode != EgressMode::Strict {
            bail!(
                "resident sandbox '{sandbox_name}' declares non-empty egress with `egress_mode = \"dns-pin\"`; \
                 residents require strict/clawk-parity egress semantics because DNS pinning still allows \
                 direct-IP bypass. Use `microsandbox`/`clawk`, or set `egress = []` for fully offline."
            );
        }
        if !primitive_enforces_strict_egress(&sandbox.primitive) {
            bail!(
                "resident sandbox '{sandbox_name}' uses primitive '{}' with non-empty egress in strict mode, \
                 but this provider cannot enforce an IP-level firewall. Docker's DNS-pin allow-list can be \
                 bypassed by direct-IP egress, so the resident launch is refused rather than downgraded. \
                 Use `microsandbox`/`clawk`, or set `egress = []` for fully offline.",
                sandbox.primitive
            );
        }
    }

    // G2 — LLM-only egress. The resident is a cloud LLM agent that MUST reach its
    // provider API, so a fully net-denied box would be inert. Permit egress ONLY to
    // that agent's fixed LLM-endpoint allowlist and deny everything else — crucially
    // `github.com` and any other host — so there is still NO push and NO arbitrary-host
    // exfiltration. An EMPTY egress still passes (fully offline is allowed). Matching
    // is a case-insensitive EXACT host comparison: no wildcard/suffix, so
    // `api.openai.com.attacker.com` is denied.
    let resident_allowlist = resident_egress_allowlist_for_agent(resident_agent)?;
    for host in &sandbox.egress {
        let allowed = resident_allowlist
            .iter()
            .any(|a| a.eq_ignore_ascii_case(host));
        if !allowed {
            bail!(
                "resident agent '{resident_agent}' sandbox '{sandbox_name}' allows egress to '{host}', which is not an approved endpoint for that agent; \
                 a net-restricted resident may reach ONLY its exact LLM API allowlist ({:?}) so there is no push \
                 and no exfiltration. Remove '{host}' from resident `egress` (github.com and general hosts stay denied).",
                resident_allowlist
            );
        }
    }

    // G2 — no push credential. The resident's identity must not reach a remote.
    if forward_ssh_agent {
        bail!(
            "resident identity forwards the SSH agent (`forward_ssh_agent = true`), which enables `git push`; \
             the sandboxed resident MUST NOT carry a push credential. Pushing is a separate, human-gated host step."
        );
    }
    for cred in credentials {
        if credential_enables_push(cred) {
            let target = match cred.target() {
                Ok(CredentialTarget::Env(name)) => format!("env `{name}`"),
                Ok(CredentialTarget::File(path)) => format!("file `{path}`"),
                Err(_) => "unknown target".to_owned(),
            };
            bail!(
                "resident identity injects a git push credential ({target}); the sandboxed resident MUST NOT \
                 carry a credential that can push to a remote. Remove it — pushing is a separate, human-gated host step."
            );
        }
    }
    // G2 — no push-enabling env. A push credential (or a channel to inject one) can
    // reach the box through ANY `[agents.X].env` / `[sandboxes.X].env` / `[[routes]].env`
    // map, not only via `[[credentials]]`. Scan the merged effective env.
    if let Some(key) = env_enables_push(effective_env) {
        bail!(
            "resident environment sets `{key}`, which hands the box a git push credential (or a channel to \
             inject one); the sandboxed resident MUST NOT carry a push credential. Remove it from the \
             agent/sandbox/route env — pushing is a separate, human-gated host step."
        );
    }

    // G1 — dedicated, rw workspace.
    enforce_dedicated_workspace(workspace)?;
    if !workspace_mounted_rw(mounts, workspace) {
        bail!(
            "orchestration workspace {} is not mounted read-write in the resident sandbox; add a \
             `{}:/workspace:rw` mount so the resident can merge worker branches against the host-visible dir.",
            workspace.display(),
            workspace.display()
        );
    }
    // G2 (defense-in-depth) — a workspace whose `.git/config` already carries a
    // token-bearing remote or a credential helper is a pre-seeded push credential.
    enforce_workspace_git_config(workspace)?;

    // G7 — broker wired + workers isolated.
    if !orchestration.enabled {
        bail!(
            "orchestration is disabled for the resident route; enable `[routes.orchestration] enabled = true` \
             so the spawn broker is wired and the resident can launch capped workers."
        );
    }
    if !orchestration.deny_sandboxes.iter().any(|s| s == "local") {
        bail!(
            "resident orchestration policy does not deny the `local` sandbox; add `local` to `deny_sandboxes` \
             so spawned workers can never land un-sandboxed."
        );
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitResult {
    pub config_path: String,
    pub operations_dir: String,
}

pub fn init_workspace(force: bool) -> Result<InitResult> {
    let home = varda_home()?;
    let config_path = home.join(CONFIG_FILENAME);
    let operations_dir = home.join(OPERATIONS_DIRNAME);
    let tasks_dir = operations_dir.join(TASKS_DIRNAME);
    let recaps_dir = operations_dir.join(RECAPS_DIRNAME);
    let runs_dir = operations_dir.join(RUNS_DIRNAME);
    let operations_readme = operations_dir.join(OPERATIONS_README);

    if config_path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            config_path.display()
        );
    }

    fs::create_dir_all(&home)
        .with_context(|| format!("failed to create Varda home {}", home.display()))?;
    ensure_git_repo(&home)?;
    fs::create_dir_all(&tasks_dir).context("failed to create tasks directory")?;
    fs::create_dir_all(&recaps_dir).context("failed to create recaps directory")?;
    fs::create_dir_all(&runs_dir).context("failed to create runs directory")?;

    fs::write(&config_path, DEFAULT_CONFIG).context("failed to write default config")?;
    ensure_keep_file(&tasks_dir.join(".gitkeep"))?;
    ensure_keep_file(&recaps_dir.join(".gitkeep"))?;
    ensure_keep_file(&runs_dir.join(".gitkeep"))?;

    if !operations_readme.exists() || force {
        fs::write(&operations_readme, OPERATIONS_README_CONTENT)
            .context("failed to write operations README")?;
    }

    Ok(InitResult {
        config_path: config_path.display().to_string(),
        operations_dir: operations_dir.display().to_string(),
    })
}

fn ensure_git_repo(path: &Path) -> Result<()> {
    if path.join(".git").exists() {
        return Ok(());
    }

    let output = Command::new("git")
        .arg("init")
        .arg(path)
        .output()
        .with_context(|| format!("failed to start git init for {}", path.display()))?;

    if !output.status.success() {
        bail!(
            "git init {} failed with status {}; stderr: {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

pub fn varda_home() -> Result<PathBuf> {
    if let Ok(home) = std::env::var(VARDA_HOME_ENV)
        && !home.trim().is_empty()
    {
        return Ok(PathBuf::from(home));
    }

    let home = std::env::var("HOME").context("HOME is not set and VARDA_HOME was not provided")?;
    Ok(PathBuf::from(home).join(".varda"))
}

pub fn config_file() -> Result<PathBuf> {
    Ok(varda_home()?.join(CONFIG_FILENAME))
}

pub fn load_config(path: impl AsRef<Path>) -> Result<Config> {
    let path = path.as_ref();
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    let mut config: Config = toml::from_str(&content)
        .with_context(|| format!("failed to parse config at {}", path.display()))?;
    resolve_config_paths(path, &mut config)?;
    remove_legacy_codex_exec_args(&mut config);
    add_varda_project_dir_to_default_agents(&mut config);

    Ok(config)
}

pub fn save_config(path: impl AsRef<Path>, config: &Config) -> Result<()> {
    let path = path.as_ref();
    let content = toml::to_string_pretty(config).context("failed to serialize config")?;
    fs::write(path, content)
        .with_context(|| format!("failed to write config at {}", path.display()))?;

    Ok(())
}

pub fn add_project_route(path: impl AsRef<Path>, glob: String, agents: Vec<String>) -> Result<()> {
    if agents.is_empty() {
        bail!("project route must allow at least one agent");
    }

    let mut config = load_config_raw(&path)?;

    for agent in &agents {
        if !config.agents.contains_key(agent) && !config.roles.contains_key(agent) {
            bail!("unknown agent or role '{agent}'");
        }
    }

    config.routes.insert(
        0,
        Route {
            glob,
            agents,
            sandbox: None,
            mounts: Vec::new(),
            env: BTreeMap::new(),
            orchestration: None,
        },
    );
    save_config(path, &config)
}

fn load_config_raw(path: impl AsRef<Path>) -> Result<Config> {
    let path = path.as_ref();
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    let config = toml::from_str(&content)
        .with_context(|| format!("failed to parse config at {}", path.display()))?;

    Ok(config)
}

fn resolve_config_paths(path: &Path, config: &mut Config) -> Result<()> {
    let operations_dir = Path::new(&config.defaults.operations_dir);
    if operations_dir.is_absolute() {
        return Ok(());
    }

    let config_dir = path
        .parent()
        .with_context(|| format!("config path {} has no parent", path.display()))?;
    config.defaults.operations_dir = config_dir.join(operations_dir).display().to_string();

    Ok(())
}

fn remove_legacy_codex_exec_args(config: &mut Config) {
    for agent in config.agents.values_mut() {
        if agent.command != "codex" || !agent.args.iter().any(|arg| arg == "exec") {
            continue;
        }

        let mut cleaned = Vec::with_capacity(agent.args.len());
        let mut index = 0;

        while index < agent.args.len() {
            if agent.args[index] == "--ask-for-approval" {
                index += 1;
                if agent.args.get(index).is_some_and(|value| value == "never") {
                    index += 1;
                }
                continue;
            }

            cleaned.push(agent.args[index].clone());
            index += 1;
        }

        agent.args = cleaned;
    }
}

fn add_varda_project_dir_to_default_agents(config: &mut Config) {
    for agent in config.agents.values_mut() {
        match agent.command.as_str() {
            "codex" => add_codex_varda_project_dir(agent),
            "claude" => add_varda_dirs_as_arg_pairs(&mut agent.args),
            "sh" if agent.args.first().is_some_and(|arg| arg == "-c")
                && agent
                    .args
                    .get(1)
                    .is_some_and(|arg| arg.contains("copilot ")) =>
            {
                add_varda_dirs_to_shell_arg(&mut agent.args);
            }
            _ => {}
        }

        if agent
            .interactive_command
            .as_deref()
            .is_some_and(|command| command == "sh")
        {
            let is_wrapped_agent = agent
                .interactive_args
                .as_deref()
                .and_then(|args| args.get(1))
                .is_some_and(|arg| {
                    arg.contains("codex ") || arg.contains("claude ") || arg.contains("copilot ")
                });
            if is_wrapped_agent && let Some(args) = agent.interactive_args.as_mut() {
                add_varda_dirs_to_shell_arg(args);
            }
        }

        if let Some(template) = agent.resume_command_template.as_mut() {
            if template.contains("codex resume") && !template.contains(" -C ") {
                template.push_str(" -C {project}");
            }
            if template.contains("codex resume") && !template.contains(" -s ") {
                template.push_str(" -s workspace-write");
            }
            if template.contains("codex resume")
                || template.contains("claude --resume")
                || template.contains("copilot --resume=")
            {
                add_shell_fragment_once(template, "--add-dir {varda_project}", "{varda_project}");
                add_shell_fragment_once(template, "--add-dir {varda_home}", "{varda_home}");
            }
        }
    }
}

fn add_codex_varda_project_dir(agent: &mut AgentConfig) {
    let mut additions = Vec::new();
    if !agent.args.iter().any(|arg| arg == "{varda_project}") {
        additions.extend(["--add-dir".to_owned(), "{varda_project}".to_owned()]);
    }
    if !agent.args.iter().any(|arg| arg == "{varda_home}") {
        additions.extend(["--add-dir".to_owned(), "{varda_home}".to_owned()]);
    }
    if additions.is_empty() {
        return;
    }

    let insert_at = agent
        .args
        .iter()
        .position(|arg| arg == "--sandbox")
        .unwrap_or(agent.args.len());
    agent.args.splice(insert_at..insert_at, additions);
}

fn add_varda_dirs_as_arg_pairs(args: &mut Vec<String>) {
    add_arg_pair_once(args, "--add-dir", "{varda_project}");
    add_arg_pair_once(args, "--add-dir", "{varda_home}");
}

fn add_arg_pair_once(args: &mut Vec<String>, flag: &str, value: &str) {
    if args.iter().any(|arg| arg == value) {
        return;
    }
    args.push(flag.to_owned());
    args.push(value.to_owned());
}

fn add_varda_dirs_to_shell_arg(args: &mut [String]) {
    if let Some(shell_command) = args.get_mut(1) {
        add_shell_fragment_once(
            shell_command,
            "--add-dir {varda_project}",
            "{varda_project}",
        );
        add_shell_fragment_once(shell_command, "--add-dir {varda_home}", "{varda_home}");
    }
}

fn add_shell_fragment_once(command: &mut String, addition: &str, marker: &str) {
    if !command.contains(marker) {
        command.push(' ');
        command.push_str(addition);
    }
}

fn ensure_keep_file(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::write(path, "").with_context(|| format!("failed to write {}", path.display()))?;
    }

    Ok(())
}

fn default_auto_commit() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent_with_credentials(credentials: Vec<CredentialConfig>) -> AgentConfig {
        AgentConfig {
            kind: AgentKind::Acp,
            command: "claude".to_owned(),
            args: vec![],
            max_prompt_tokens: None,
            working_dir: None,
            env: BTreeMap::new(),
            streams_output: None,
            auth_token_env: None,
            auth_token_target: None,
            credentials,
            interactive_command: None,
            interactive_args: None,
            resume_command_template: None,
            interpreter_agent: None,
        }
    }

    #[test]
    fn credential_source_and_target_validation() {
        // Exactly one source + one target resolves.
        let ok = CredentialConfig {
            from_env: Some("HOST_TOKEN".to_owned()),
            env: Some("IN_BOX".to_owned()),
            ..Default::default()
        };
        assert_eq!(ok.source().unwrap(), CredentialSource::Env("HOST_TOKEN"));
        assert_eq!(ok.target().unwrap(), CredentialTarget::Env("IN_BOX"));

        // No source / no target both error.
        let bare = CredentialConfig::default();
        assert!(bare.source().is_err(), "no source must error");
        let no_target = CredentialConfig {
            command: Some("mint".to_owned()),
            ..Default::default()
        };
        assert!(no_target.target().is_err(), "no target must error");

        // Multiple sources / both targets error.
        let two_sources = CredentialConfig {
            from_env: Some("A".to_owned()),
            command: Some("mint".to_owned()),
            env: Some("X".to_owned()),
            ..Default::default()
        };
        assert!(two_sources.source().is_err(), "multiple sources must error");
        let two_targets = CredentialConfig {
            from_secret: Some("s".to_owned()),
            env: Some("X".to_owned()),
            file: Some("/etc/tok".to_owned()),
            ..Default::default()
        };
        assert!(two_targets.target().is_err(), "both targets must error");
    }

    #[test]
    fn effective_credentials_folds_legacy_auth_token_sugar() {
        // Legacy single-token pair becomes a trailing `from_env` → `env` entry, with
        // the target defaulting to the source name when `auth_token_target` is unset.
        let mut agent = agent_with_credentials(vec![]);
        agent.auth_token_env = Some("HOST_ANTHROPIC".to_owned());
        let creds = agent.effective_credentials();
        assert_eq!(creds.len(), 1);
        assert_eq!(
            creds[0].source().unwrap(),
            CredentialSource::Env("HOST_ANTHROPIC")
        );
        assert_eq!(
            creds[0].target().unwrap(),
            CredentialTarget::Env("HOST_ANTHROPIC")
        );

        agent.auth_token_target = Some("ANTHROPIC_API_KEY".to_owned());
        let creds = agent.effective_credentials();
        assert_eq!(
            creds[0].target().unwrap(),
            CredentialTarget::Env("ANTHROPIC_API_KEY")
        );

        // Explicit entries come first, legacy sugar appended last.
        agent.credentials = vec![CredentialConfig {
            command: Some("mint".to_owned()),
            file: Some("/home/agent/.token".to_owned()),
            ..Default::default()
        }];
        let creds = agent.effective_credentials();
        assert_eq!(creds.len(), 2);
        assert_eq!(
            creds[0].target().unwrap(),
            CredentialTarget::File("/home/agent/.token")
        );
        assert_eq!(
            creds[1].source().unwrap(),
            CredentialSource::Env("HOST_ANTHROPIC")
        );
    }

    #[test]
    fn credentials_list_parses_from_toml() {
        let toml = r#"kind = "acp"
command = "claude"

[[credentials]]
command = "gcloud auth print-access-token"
env = "CLOUDSDK_AUTH_ACCESS_TOKEN"

[[credentials]]
from_secret = "tfc-token"
env = "TF_TOKEN_app_terraform_io"

[[credentials]]
from_env = "HOST_TOKEN"
file = "/home/agent/.config/token"
refresh_seconds = 1800
"#;
        let agent: AgentConfig = toml::from_str(toml).unwrap();
        assert_eq!(agent.credentials.len(), 3);
        assert_eq!(
            agent.credentials[0].source().unwrap(),
            CredentialSource::Command("gcloud auth print-access-token")
        );
        assert_eq!(
            agent.credentials[1].source().unwrap(),
            CredentialSource::Secret("tfc-token")
        );
        assert_eq!(
            agent.credentials[2].target().unwrap(),
            CredentialTarget::File("/home/agent/.config/token")
        );
        assert_eq!(agent.credentials[2].refresh_seconds, Some(1800));
    }

    #[test]
    fn legacy_config_without_m10_bounds_parses_with_defaults() {
        // A pre-M10 config sets only `timeout_seconds`; the new bounds must fall
        // back to their serde defaults and the deprecated alias must feed the
        // soft ceiling unchanged.
        let legacy = r#"[defaults]
timeout_seconds = 600
operations_dir = "operations"

[[routes]]
glob = "**"
agents = ["codex"]

[agents.codex]
kind = "acp"
command = "codex"
args = []
"#;
        let config: Config = toml::from_str(legacy).expect("legacy config should parse");
        assert_eq!(config.defaults.idle_timeout_seconds, 180);
        assert_eq!(config.defaults.max_continuations, 0);
        assert_eq!(config.defaults.max_tool_calls, 0);
        assert_eq!(config.defaults.max_seconds, None);
        // With no explicit `max_seconds`, the deprecated `timeout_seconds` alias
        // supplies the soft ceiling — existing configs behave unchanged.
        assert_eq!(config.defaults.effective_max_seconds(), Some(600));
    }

    #[test]
    fn max_seconds_accepts_integer_and_none_keyword() {
        let with_int = r#"[defaults]
timeout_seconds = 600
operations_dir = "operations"
max_seconds = 1200
"#;
        let config: Config = toml::from_str(with_int).expect("integer max_seconds should parse");
        assert_eq!(config.defaults.max_seconds, Some(MaxSeconds::Seconds(1200)));
        assert_eq!(config.defaults.effective_max_seconds(), Some(1200));

        // Explicit "none" overrides the alias with "no ceiling".
        let none = r#"[defaults]
timeout_seconds = 600
operations_dir = "operations"
max_seconds = "none"
"#;
        let config: Config = toml::from_str(none).expect("none max_seconds should parse");
        assert_eq!(config.defaults.effective_max_seconds(), None);

        // `timeout_seconds = 0` with no explicit ceiling ⇒ no ceiling.
        let zero = r#"[defaults]
timeout_seconds = 0
operations_dir = "operations"
"#;
        let config: Config = toml::from_str(zero).expect("zero timeout should parse");
        assert_eq!(config.defaults.effective_max_seconds(), None);
    }

    #[test]
    fn parses_default_config() {
        let config: Config = toml::from_str(DEFAULT_CONFIG).expect("default config should parse");

        assert_eq!(config.defaults.timeout_seconds, 600);
        assert_eq!(config.defaults.idle_timeout_seconds, 180);
        assert_eq!(config.defaults.max_continuations, 0);
        assert_eq!(config.defaults.max_tool_calls, 0);
        // `max_seconds = "none"` ⇒ no soft ceiling, ignoring the deprecated alias.
        assert_eq!(
            config.defaults.max_seconds,
            Some(MaxSeconds::Keyword("none".to_owned()))
        );
        assert_eq!(config.defaults.effective_max_seconds(), None);
        assert_eq!(config.routes[0].agents, vec!["codex"]);
        assert_eq!(config.agents["codex"].command, "codex");
        assert!(!config.agents.contains_key("tester"));
        assert_eq!(config.roles["tester"].backend, "codex");
        assert!(config.roles["tester"].instructions.is_some());
        assert_eq!(config.agents["claude"].command, "claude");
        assert_eq!(
            config.agents["claude"].args,
            vec![
                "-p",
                "--permission-mode",
                "acceptEdits",
                "--add-dir",
                "{project}",
                "--add-dir",
                "{varda_project}",
                "--add-dir",
                "{varda_home}"
            ]
        );
        assert_eq!(
            config.agents["claude"].interactive_command.as_deref(),
            Some("sh")
        );
        assert!(
            config.agents["claude"]
                .interactive_args
                .as_ref()
                .is_some_and(|a| a[0] == "-c")
        );
        assert_eq!(config.agents["copilot"].command, "sh");
        assert_eq!(
            config.agents["copilot"].args,
            vec![
                "-c",
                "copilot -p \"$(cat)\" --allow-all-tools --add-dir {project} --add-dir {varda_project} --add-dir {varda_home} -s"
            ]
        );
        // M13b: copilot runs interactively under the sandbox too, seeded from the
        // staged prompt file — mirroring claude/codex.
        assert_eq!(
            config.agents["copilot"].interactive_command.as_deref(),
            Some("sh")
        );
        let copilot_interactive = config.agents["copilot"]
            .interactive_args
            .as_ref()
            .expect("copilot must have interactive_args wired for the sandbox interactive path");
        assert_eq!(copilot_interactive[0], "-c");
        assert!(
            copilot_interactive[1].contains("copilot \"$(cat $VARDA_PROMPT_FILE)\""),
            "copilot interactive must read the prompt from the staged VARDA_PROMPT_FILE, got: {}",
            copilot_interactive[1]
        );
        // Normalization must fold the varda dirs into the copilot interactive arg.
        assert!(copilot_interactive[1].contains("--add-dir {varda_project}"));
        assert!(copilot_interactive[1].contains("--add-dir {varda_home}"));
        assert_eq!(config.agents["codex"].max_prompt_tokens, None);
        assert!(
            !config.agents["codex"]
                .args
                .iter()
                .any(|arg| arg == "--ask-for-approval")
        );
        assert!(config.git.auto_commit);
    }

    #[test]
    fn strips_legacy_codex_exec_approval_args_on_load() {
        let path =
            std::env::temp_dir().join(format!("varda-legacy-codex-{}.toml", std::process::id()));
        let config = DEFAULT_CONFIG.replace(
            r#"args = ["exec", "--cd", ".", "--add-dir", "{varda_project}", "--add-dir", "{varda_home}", "--sandbox", "workspace-write", "-"]"#,
            r#"args = ["exec", "--cd", ".", "--sandbox", "workspace-write", "--ask-for-approval", "never", "-"]"#,
        );
        fs::write(&path, config).expect("config should be written");

        let config = load_config(&path).expect("legacy config should load");
        fs::remove_file(path).expect("config should be removed");

        assert_eq!(
            config.agents["codex"].args,
            vec![
                "exec",
                "--cd",
                ".",
                "--add-dir",
                "{varda_project}",
                "--add-dir",
                "{varda_home}",
                "--sandbox",
                "workspace-write",
                "-"
            ]
        );
    }

    #[test]
    fn prepends_project_route_before_catch_all() {
        let path = std::env::temp_dir().join(format!("varda-config-{}.toml", std::process::id()));
        fs::write(&path, DEFAULT_CONFIG).expect("config should be written");

        add_project_route(
            &path,
            "/work/project/**".to_owned(),
            vec!["codex".to_owned()],
        )
        .expect("project route should be prepended");

        let config = load_config(&path).expect("config should reload");
        fs::remove_file(path).expect("config should be removed");

        assert_eq!(config.routes.len(), 2);
        assert_eq!(config.routes[0].glob, "/work/project/**");
        assert_eq!(config.routes[0].agents, vec!["codex"]);
        assert_eq!(config.routes[1].glob, "**");
    }

    #[test]
    fn resolves_relative_operations_dir_against_config_dir() {
        let root = std::env::temp_dir().join(format!("varda-home-{}", std::process::id()));
        fs::create_dir_all(&root).expect("config directory should be created");
        let path = root.join("config.toml");
        fs::write(&path, DEFAULT_CONFIG).expect("config should be written");

        let config = load_config(&path).expect("config should load");

        assert_eq!(
            config.defaults.operations_dir,
            root.join("operations").display().to_string()
        );
    }

    #[test]
    fn default_config_round_trips_without_sandbox_keys() {
        let config: Config = toml::from_str(DEFAULT_CONFIG).expect("default config should parse");

        assert!(config.sandboxes.is_empty());
        assert!(config.defaults.sandbox.is_none());
        assert!(config.routes.iter().all(|route| route.sandbox.is_none()));

        let serialized = toml::to_string_pretty(&config).expect("config should serialize");
        // The only "sandbox" occurrences must be Codex's own `--sandbox` CLI args,
        // never our config keys (`sandbox = ` under [defaults]/[[routes]] or a
        // `[sandboxes]` table).
        assert!(
            !serialized.contains("sandbox = ") && !serialized.contains("[sandboxes"),
            "no sandbox config keys should be emitted when none are set: {serialized}"
        );

        let reparsed: Config = toml::from_str(&serialized).expect("serialized config should parse");
        assert_eq!(config, reparsed);
    }

    #[test]
    fn documented_config_knobs_parse_and_round_trip() {
        let documented = format!(
            r#"{DEFAULT_CONFIG}

[[routes]]
glob = "/work/gcp/**"
agents = ["claude"]
sandbox = "devcontainer"
env = {{ GCLOUD_PROJECT = "example-project" }}

[routes.orchestration]
enabled = false
max_depth = 1
max_fanout = 1
global_child_budget = 2

[[agents.claude.credentials]]
from_env = "CLAUDE_SANDBOX_TOKEN"
env = "ANTHROPIC_API_KEY"

[[agents.claude.credentials]]
from_secret = "gcp-service-account-json"
file = "/home/agent/.config/gcloud/application_default_credentials.json"

[[agents.claude.credentials]]
command = "gcloud auth print-access-token --impersonate-service-account=deployer@example-project.iam.gserviceaccount.com"
env = "CLOUDSDK_AUTH_ACCESS_TOKEN"

[[agents.claude.credentials]]
command = "gcloud auth print-access-token --impersonate-service-account=deployer@example-project.iam.gserviceaccount.com"
env = "GOOGLE_OAUTH_ACCESS_TOKEN"

[[agents.claude.credentials]]
from_secret = "tfc-token"
env = "TF_TOKEN_app_terraform_io"

[[agents.claude.credentials]]
from_secret = "azdo-pat"
env = "AZURE_DEVOPS_EXT_PAT"

[[agents.claude.credentials]]
from_secret = "azure-client-id"
env = "AZURE_CLIENT_ID"

[[agents.claude.credentials]]
from_secret = "azure-client-secret"
env = "AZURE_CLIENT_SECRET"

[[agents.claude.credentials]]
from_secret = "azure-tenant-id"
env = "AZURE_TENANT_ID"

[[agents.claude.credentials]]
command = "az account get-access-token --query accessToken -o tsv"
env = "AZURE_TOKEN"

[[agents.claude.credentials]]
command = "security find-generic-password -w -s varda-sandbox-token"
env = "CUSTOM_SANDBOX_TOKEN"
refresh_seconds = 1800

[agents.claude.env]
STATIC_TOOL_VALUE = "enabled"

[agents.local-shell]
kind = "acp"
command = "sh"
args = ["-c", "cat"]
interactive_command = "sh"
interactive_args = ["-i"]
interpreter_agent = "codex"

[sandboxes.devcontainer]
image_from = "devcontainer"
primitive = "docker"
env = {{ GCLOUD_PROJECT = "example-project" }}

[sandboxes.custom]
build = "./Dockerfile.varda"
primitive = "docker"

[orchestration]
enabled = true
max_depth = 2
max_fanout = 4
global_child_budget = 16
deny_sandboxes = ["local"]
"#
        );
        let config: Config =
            toml::from_str(&documented).expect("documented config examples should parse");

        assert_eq!(
            config.agents["local-shell"].interpreter_agent.as_deref(),
            Some("codex")
        );
        assert_eq!(config.agents["claude"].credentials.len(), 11);
        assert!(
            config.agents["claude"]
                .credentials
                .iter()
                .any(
                    |cred| cred.from_env.as_deref() == Some("CLAUDE_SANDBOX_TOKEN")
                        && cred.env.as_deref() == Some("ANTHROPIC_API_KEY")
                )
        );
        assert!(
            config.agents["claude"]
                .credentials
                .iter()
                .any(
                    |cred| cred.from_secret.as_deref() == Some("gcp-service-account-json")
                        && cred.file.as_deref()
                            == Some(
                                "/home/agent/.config/gcloud/application_default_credentials.json"
                            )
                )
        );
        assert!(config.agents["claude"].credentials.iter().any(|cred| {
            cred.command
                .as_deref()
                .is_some_and(|command| command.contains("gcloud auth print-access-token"))
                && cred.env.as_deref() == Some("CLOUDSDK_AUTH_ACCESS_TOKEN")
        }));
        assert_eq!(
            config.routes[1]
                .env
                .get("GCLOUD_PROJECT")
                .map(String::as_str),
            Some("example-project")
        );
        assert_eq!(
            config.sandboxes["devcontainer"]
                .env
                .get("GCLOUD_PROJECT")
                .map(String::as_str),
            Some("example-project")
        );
        assert_eq!(
            config.sandboxes["devcontainer"].image_from.as_deref(),
            Some("devcontainer")
        );
        assert_eq!(
            config.sandboxes["custom"].build.as_deref(),
            Some("./Dockerfile.varda")
        );
        assert!(config.orchestration.enabled);
        assert_eq!(config.orchestration.max_fanout, 4);
        assert!(
            config.routes[1]
                .orchestration
                .as_ref()
                .is_some_and(|policy| !policy.enabled && policy.max_depth == 1)
        );
        assert_eq!(config.agents["codex"].streams_output, Some(true));

        let serialized = toml::to_string_pretty(&config).expect("config should serialize");
        let reparsed: Config = toml::from_str(&serialized).expect("serialized config should parse");
        assert_eq!(config, reparsed);
    }

    #[test]
    fn resolves_effective_sandbox_provider_precedence() {
        let mut config: Config =
            toml::from_str(DEFAULT_CONFIG).expect("default config should parse");

        let route = config.routes[0].clone();
        assert_eq!(config.effective_sandbox(&route), DEFAULT_SANDBOX_PROVIDER);

        config.defaults.sandbox = Some("docker".to_owned());
        assert_eq!(config.effective_sandbox(&route), "docker");

        let route_with_sandbox = Route {
            glob: "**".to_owned(),
            agents: vec!["codex".to_owned()],
            sandbox: Some("firejail".to_owned()),
            mounts: Vec::new(),
            env: BTreeMap::new(),
            orchestration: None,
        };
        assert_eq!(config.effective_sandbox(&route_with_sandbox), "firejail");
    }

    #[test]
    fn parses_sandboxes_table() {
        let toml = format!(
            "{DEFAULT_CONFIG}\n[sandboxes.docker]\nimage = \"varda:latest\"\nmounts = [\"/tmp\"]\negress = [\"api.example.com\"]\n"
        );
        let config: Config = toml::from_str(&toml).expect("config with sandboxes should parse");

        let docker = &config.sandboxes["docker"];
        assert_eq!(docker.image.as_deref(), Some("varda:latest"));
        assert_eq!(docker.mounts, vec!["/tmp"]);
        assert_eq!(docker.egress, vec!["api.example.com"]);
        assert_eq!(docker.egress_mode, EgressMode::Strict);
        // `primitive` defaults to "docker" when omitted, `build` to None.
        assert_eq!(docker.primitive, "docker");
        assert!(docker.build.is_none());
    }

    #[test]
    fn parses_explicit_egress_mode() {
        let toml = format!(
            "{DEFAULT_CONFIG}\n[sandboxes.docker]\nimage = \"varda:latest\"\negress = [\"api.example.com\"]\negress_mode = \"dns-pin\"\n"
        );
        let config: Config = toml::from_str(&toml).expect("config with egress mode should parse");
        assert_eq!(config.sandboxes["docker"].egress_mode, EgressMode::DnsPin);
    }

    #[test]
    fn sandbox_config_round_trips_primitive_and_build() {
        let toml = format!(
            "{DEFAULT_CONFIG}\n[sandboxes.rustvm]\nbuild = \"./testdata/Dockerfile.rust\"\nprimitive = \"microsandbox\"\n"
        );
        let config: Config = toml::from_str(&toml).expect("config with build should parse");

        let rustvm = &config.sandboxes["rustvm"];
        assert!(rustvm.image.is_none());
        assert_eq!(rustvm.build.as_deref(), Some("./testdata/Dockerfile.rust"));
        assert_eq!(rustvm.primitive, "microsandbox");

        // Round-trip: serialize then reparse to an identical config.
        let serialized = toml::to_string_pretty(&config).expect("config should serialize");
        let reparsed: Config = toml::from_str(&serialized).expect("serialized config should parse");
        assert_eq!(config, reparsed);
        // The explicit primitive survives serialization.
        assert_eq!(reparsed.sandboxes["rustvm"].primitive, "microsandbox");
    }

    #[test]
    fn initializes_git_repo_when_needed() {
        let root = std::env::temp_dir().join(format!("varda-git-init-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).expect("old test directory should be removed");
        }
        fs::create_dir_all(&root).expect("test directory should be created");

        ensure_git_repo(&root).expect("git repo should initialize");

        assert!(root.join(".git").exists());
        fs::remove_dir_all(root).expect("test directory should be removed");
    }
}

#[cfg(test)]
mod m6b_tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("varda-m6b-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn base_config() -> Config {
        let mut c: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        c.routes = vec![Route {
            glob: "**".to_owned(),
            agents: vec!["codex".to_owned()],
            sandbox: None,
            mounts: vec![],
            orchestration: None,
            env: BTreeMap::new(),
        }];
        c
    }

    #[test]
    fn walk_up_finds_nearest_varda() {
        let root = tmp("walkup");
        let nested = root.join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("a").join(VARDA_FILE), "sandbox = \"rust\"\n").unwrap();
        fs::write(nested.join(VARDA_FILE), "sandbox = \"go\"\n").unwrap();

        let found = find_nearest_varda(&nested, &root).unwrap();
        assert_eq!(found, nested.join(VARDA_FILE));

        // From the middle dir the higher `.varda` is the nearest.
        let mid = root.join("a/b");
        assert_eq!(
            find_nearest_varda(&mid, &root).unwrap(),
            root.join("a").join(VARDA_FILE)
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reference_and_central_precedence() {
        let root = tmp("ref");
        let proj = root.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let mut config = base_config();
        config.sandboxes.insert(
            "rust".to_owned(),
            SandboxConfig {
                image: Some("rust:latest".to_owned()),
                ..SandboxConfig::default()
            },
        );

        // No `.varda` ⇒ central route/defaults ⇒ "local".
        let r = config.resolve_sandbox_for(&proj, &root, None).unwrap();
        assert_eq!(r.name, "local");
        assert!(r.varda_file.is_none());

        // Reference `.varda` selects the central sandbox by name.
        fs::write(proj.join(VARDA_FILE), "sandbox = \"rust\"\n").unwrap();
        let r = config.resolve_sandbox_for(&proj, &root, None).unwrap();
        assert_eq!(r.name, "rust");
        assert_eq!(r.config.image.as_deref(), Some("rust:latest"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn task_pinned_sandbox_wins_over_route_and_varda() {
        let root = tmp("pinned");
        let proj = root.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let mut config = base_config();
        config.sandboxes.insert(
            "msbshell".to_owned(),
            SandboxConfig {
                image: Some("msb:latest".to_owned()),
                ..SandboxConfig::default()
            },
        );

        // Route/defaults would resolve "local"; the task-pin forces "msbshell".
        let r = config
            .resolve_sandbox_for(&proj, &root, Some("msbshell"))
            .unwrap();
        assert_eq!(r.name, "msbshell");
        assert_eq!(r.config.image.as_deref(), Some("msb:latest"));
        assert!(r.varda_file.is_none());

        // Even a `.varda` reference is overridden by the task-pin (highest precedence).
        fs::write(proj.join(VARDA_FILE), "sandbox = \"rust\"\n").unwrap();
        let r = config
            .resolve_sandbox_for(&proj, &root, Some("msbshell"))
            .unwrap();
        assert_eq!(r.name, "msbshell");
        assert!(r.varda_file.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn task_pinned_unknown_sandbox_errors() {
        let root = tmp("pinned-unknown");
        let proj = root.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let config = base_config();

        let err = config
            .resolve_sandbox_for(&proj, &root, Some("nope"))
            .unwrap_err();
        assert!(err.to_string().contains("nope"));
        assert!(err.to_string().contains("not configured"));

        // "local" is always a valid pin (identity provider), never an error.
        let r = config
            .resolve_sandbox_for(&proj, &root, Some("local"))
            .unwrap();
        assert_eq!(r.name, "local");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn no_pin_leaves_route_based_resolution_unchanged() {
        let root = tmp("nopin");
        let proj = root.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let mut config = base_config();
        config.sandboxes.insert(
            "rust".to_owned(),
            SandboxConfig {
                image: Some("rust:latest".to_owned()),
                ..SandboxConfig::default()
            },
        );
        // A `.varda` reference still governs when no task-pin is supplied.
        fs::write(proj.join(VARDA_FILE), "sandbox = \"rust\"\n").unwrap();
        let r = config.resolve_sandbox_for(&proj, &root, None).unwrap();
        assert_eq!(r.name, "rust");
        assert!(r.varda_file.is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn inline_varda_mount_is_hardened_and_ro() {
        let root = tmp("inline");
        let proj = root.join("proj");
        fs::create_dir_all(proj.join("ctx")).unwrap();
        let config = base_config();
        fs::write(
            proj.join(VARDA_FILE),
            "[sandbox]\nimage = \"x:1\"\nmounts = [\"ctx:/ctx\"]\n",
        )
        .unwrap();

        let r = config.resolve_sandbox_for(&proj, &root, None).unwrap();
        assert_eq!(r.name, "inline");
        assert_eq!(r.varda_mounts.len(), 1);
        // Forced :ro (writable not allowed by default) and source made absolute.
        assert!(r.varda_mounts[0].ends_with(":/ctx:ro"));
        assert!(r.varda_mounts[0].starts_with(proj.join("ctx").to_str().unwrap()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn static_env_merges_agent_sandbox_route_varda_precedence_inputs() {
        let root = tmp("envmerge");
        let proj = root.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let mut config = base_config();
        config.routes[0].sandbox = Some("rust".to_owned());
        config.routes[0]
            .env
            .insert("FROM_ROUTE".to_owned(), "route".to_owned());
        config.routes[0]
            .env
            .insert("SHARED".to_owned(), "route".to_owned());
        config.sandboxes.insert(
            "rust".to_owned(),
            SandboxConfig {
                env: BTreeMap::from([
                    ("FROM_SANDBOX".to_owned(), "sandbox".to_owned()),
                    ("SHARED".to_owned(), "sandbox".to_owned()),
                ]),
                ..SandboxConfig::default()
            },
        );

        let r = config.resolve_sandbox_for(&proj, &root, None).unwrap();
        assert_eq!(r.env["FROM_SANDBOX"], "sandbox");
        assert_eq!(r.env["FROM_ROUTE"], "route");
        assert_eq!(r.env["SHARED"], "route");

        // A normal .varda env key is allowed and wins over the sandbox origin.
        fs::write(
            proj.join(VARDA_FILE),
            "[sandbox]\nimage = \"x:1\"\nenv = { FROM_VARDA = \"varda\", FROM_SANDBOX = \"varda\" }\n",
        )
        .unwrap();
        let r = config.resolve_sandbox_for(&proj, &root, None).unwrap();
        assert_eq!(r.env["FROM_SANDBOX"], "varda");
        assert_eq!(r.env["FROM_ROUTE"], "route");
        assert_eq!(r.env["FROM_VARDA"], "varda");
        assert_eq!(r.varda_env_keys, vec!["FROM_SANDBOX", "FROM_VARDA"]);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn varda_env_rejects_reserved_key_and_trusted_override() {
        let root = tmp("envfloor");
        let proj = root.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let mut config = base_config();
        config.routes[0]
            .env
            .insert("TRUSTED".to_owned(), "route".to_owned());

        fs::write(
            proj.join(VARDA_FILE),
            "[sandbox]\nimage = \"x:1\"\nenv = { PATH = \"/tmp\" }\n",
        )
        .unwrap();
        let err = config.resolve_sandbox_for(&proj, &root, None).unwrap_err();
        assert!(err.to_string().contains("PATH"), "{err}");

        fs::write(
            proj.join(VARDA_FILE),
            "[sandbox]\nimage = \"x:1\"\nenv = { TRUSTED = \"override\" }\n",
        )
        .unwrap();
        let err = config.resolve_sandbox_for(&proj, &root, None).unwrap_err();
        assert!(err.to_string().contains("TRUSTED"), "{err}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn floor_rejects_local_primitive() {
        let root = tmp("local");
        let proj = root.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let config = base_config();
        fs::write(
            proj.join(VARDA_FILE),
            "[sandbox]\nimage = \"x:1\"\nprimitive = \"local\"\n",
        )
        .unwrap();
        let err = config.resolve_sandbox_for(&proj, &root, None).unwrap_err();
        assert!(err.to_string().contains("primitive"), "{err}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn floor_rejects_out_of_tree_source() {
        let root = tmp("outoftree");
        let proj = root.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let config = base_config();
        fs::write(
            proj.join(VARDA_FILE),
            "[sandbox]\nimage = \"x:1\"\nmounts = [\"/etc:/data\"]\n",
        )
        .unwrap();
        let err = config.resolve_sandbox_for(&proj, &root, None).unwrap_err();
        assert!(
            err.to_string().contains("outside the project root"),
            "{err}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn floor_rejects_system_target_and_egress_over_ceiling() {
        let root = tmp("systgt");
        let proj = root.join("proj");
        fs::create_dir_all(proj.join("ctx")).unwrap();
        let config = base_config();
        fs::write(
            proj.join(VARDA_FILE),
            "[sandbox]\nimage = \"x:1\"\nmounts = [\"ctx:/etc\"]\n",
        )
        .unwrap();
        let err = config.resolve_sandbox_for(&proj, &root, None).unwrap_err();
        assert!(err.to_string().contains("system dir"), "{err}");

        // Egress ceiling clamp.
        let mut config = base_config();
        config.defaults.egress_ceiling = Some(vec!["api.ok.com".to_owned()]);
        fs::write(
            proj.join(VARDA_FILE),
            "[sandbox]\nimage = \"x:1\"\negress = [\"evil.example.com\"]\n",
        )
        .unwrap();
        let err = config.resolve_sandbox_for(&proj, &root, None).unwrap_err();
        assert!(err.to_string().contains("egress_ceiling"), "{err}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn writable_varda_mount_allowed_when_opted_in() {
        let root = tmp("writable");
        let proj = root.join("proj");
        fs::create_dir_all(proj.join("ctx")).unwrap();
        let mut config = base_config();
        config.defaults.allow_varda_writable_mounts = true;
        fs::write(
            proj.join(VARDA_FILE),
            "[sandbox]\nimage = \"x:1\"\nmounts = [\"ctx:/ctx:rw\"]\n",
        )
        .unwrap();
        let r = config.resolve_sandbox_for(&proj, &root, None).unwrap();
        assert!(
            r.varda_mounts[0].ends_with(":/ctx:rw"),
            "{:?}",
            r.varda_mounts
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// M6b-wire: the LIVE run path (resolve → merge origins → build provider), not
    /// `resolve_sandbox_for` in isolation. A reference `.varda` in a project
    /// SUBFOLDER selects that central sandbox's provider at run time.
    #[test]
    fn run_path_reference_varda_selects_provider() {
        let root = tmp("runref");
        let sub = root.join("service");
        fs::create_dir_all(&sub).unwrap();
        let mut config = base_config();
        config.sandboxes.insert(
            "rust".to_owned(),
            SandboxConfig {
                image: Some("rust:latest".to_owned()),
                primitive: "docker".to_owned(),
                ..SandboxConfig::default()
            },
        );
        fs::write(sub.join(VARDA_FILE), "sandbox = \"rust\"\n").unwrap();

        let resolved = config.resolve_sandbox_for(&sub, &root, None).unwrap();
        let mounts = crate::sandbox::merge_mount_origins(
            &resolved.config.mounts,
            &resolved.route_mounts,
            &resolved.varda_mounts,
        );
        let provider = crate::sandbox::provider_from_config(
            &resolved.name,
            &resolved.config,
            mounts,
            &crate::sandbox::SandboxIdentity::default(),
        )
        .unwrap();
        assert_eq!(provider.name(), "rust");
        let _ = fs::remove_dir_all(&root);
    }

    /// M6b-wire: an inline `.varda` mount flows through the run path as a
    /// `MountOrigin::Varda` in the merged set handed to the provider (hardened,
    /// `:ro`), so it is applied rather than dropped.
    #[test]
    fn run_path_inline_varda_produces_varda_origin() {
        let root = tmp("runinline");
        let proj = root.join("proj");
        fs::create_dir_all(proj.join("ctx")).unwrap();
        let config = base_config();
        fs::write(
            proj.join(VARDA_FILE),
            "[sandbox]\nimage = \"x:1\"\nmounts = [\"ctx:/ctx\"]\n",
        )
        .unwrap();

        let resolved = config.resolve_sandbox_for(&proj, &root, None).unwrap();
        let mounts = crate::sandbox::merge_mount_origins(
            &resolved.config.mounts,
            &resolved.route_mounts,
            &resolved.varda_mounts,
        );
        let varda: Vec<_> = mounts
            .iter()
            .filter(|(origin, _)| *origin == crate::sandbox::MountOrigin::Varda)
            .collect();
        assert_eq!(
            varda.len(),
            1,
            "expected one Varda-origin mount: {mounts:?}"
        );
        assert!(varda[0].1.ends_with(":/ctx:ro"), "{:?}", varda[0].1);
        // The provider builds from the inline config.
        let provider = crate::sandbox::provider_from_config(
            &resolved.name,
            &resolved.config,
            mounts,
            &crate::sandbox::SandboxIdentity::default(),
        )
        .unwrap();
        assert_eq!(provider.name(), "inline");
        let _ = fs::remove_dir_all(&root);
    }

    /// M6b-wire: a floor-violating inline `.varda` (primitive = "local", i.e. an
    /// escape from the box) refuses the run at resolution with a clear error,
    /// before any provider is built.
    #[test]
    fn run_path_floor_violation_refuses_before_provider() {
        let root = tmp("runfloor");
        let proj = root.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let config = base_config();
        fs::write(
            proj.join(VARDA_FILE),
            "[sandbox]\nimage = \"x:1\"\nprimitive = \"local\"\n",
        )
        .unwrap();
        let err = config.resolve_sandbox_for(&proj, &root, None).unwrap_err();
        assert!(err.to_string().contains("primitive"), "{err}");
        let _ = fs::remove_dir_all(&root);
    }
}

#[cfg(test)]
mod orchestration_config_tests {
    use super::*;
    use crate::orchestration::OrchestrationPolicy;

    #[test]
    fn default_config_leaves_orchestration_locked_down() {
        // The shipped default config must not enable spawning.
        let c: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        assert!(!c.orchestration.enabled);
        assert!(c.orchestration.is_default());
    }

    #[test]
    fn orchestration_table_parses_from_toml() {
        let toml_src = r#"
[defaults]
timeout_seconds = 0
operations_dir = "ops"

[orchestration]
enabled = true
max_depth = 3
max_fanout = 5
allow_agents = ["claude"]

[[routes]]
glob = "**"
agents = ["claude"]
"#;
        let c: Config = toml::from_str(toml_src).unwrap();
        assert!(c.orchestration.enabled);
        assert_eq!(c.orchestration.max_depth, 3);
        assert_eq!(c.orchestration.max_fanout, 5);
        assert_eq!(c.orchestration.allow_agents, vec!["claude".to_owned()]);
        // deny_sandboxes still defaults to ["local"] even when only some keys are set.
        assert!(c.orchestration.deny_sandboxes.contains(&"local".to_owned()));
    }

    #[test]
    fn resolve_prefers_route_override_then_falls_back_to_defaults() {
        let strict = OrchestrationPolicy::default(); // disabled
        let permissive = OrchestrationPolicy {
            enabled: true,
            max_fanout: 9,
            ..OrchestrationPolicy::default()
        };
        let mut c: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        c.orchestration = permissive.clone();
        c.routes = vec![
            Route {
                glob: "**/locked/**".to_owned(),
                agents: vec!["claude".to_owned()],
                sandbox: None,
                mounts: vec![],
                env: BTreeMap::new(),
                orchestration: Some(strict.clone()),
            },
            Route {
                glob: "**".to_owned(),
                agents: vec!["claude".to_owned()],
                sandbox: None,
                mounts: vec![],
                env: BTreeMap::new(),
                orchestration: None,
            },
        ];

        // A path matching the override route gets the stricter policy.
        let locked = c.resolve_orchestration_for(Path::new("/work/locked/proj"));
        assert_eq!(locked, strict);
        // A path without an override inherits the top-level defaults.
        let other = c.resolve_orchestration_for(Path::new("/work/other/proj"));
        assert_eq!(other, permissive);
    }
}

#[cfg(test)]
mod resident_tests {
    use super::*;
    use crate::orchestration::OrchestrationPolicy;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("varda-resident-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// An isolating, net-denied sandbox — the required resident placement.
    fn isolating_sandbox() -> SandboxConfig {
        SandboxConfig {
            image: Some("dev:latest".to_owned()),
            primitive: "microsandbox".to_owned(),
            egress: vec![],
            ..SandboxConfig::default()
        }
    }

    /// A broker-enabled policy that denies `local` — the required resident policy.
    fn resident_policy() -> OrchestrationPolicy {
        OrchestrationPolicy {
            enabled: true,
            deny_sandboxes: vec!["local".to_owned()],
            ..OrchestrationPolicy::default()
        }
    }

    fn env_cred(target: &str) -> CredentialConfig {
        CredentialConfig {
            from_env: Some("HOST".to_owned()),
            env: Some(target.to_owned()),
            ..Default::default()
        }
    }

    /// An empty effective env — the default for gates that don't exercise the env
    /// scan.
    fn no_env() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    /// A single-entry effective env, for the push-via-env adversarial cases.
    fn env_map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    /// Initialize `<workspace>/.git/config` with the given contents, returning the
    /// workspace so callers can chain it into `enforce_resident_launch`.
    fn seed_git_config(workspace: &Path, contents: &str) {
        let git_dir = workspace.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        fs::write(git_dir.join("config"), contents).unwrap();
    }

    #[test]
    fn credential_enables_push_flags_push_channels_only() {
        assert!(credential_enables_push(&env_cred("GITHUB_TOKEN")));
        assert!(credential_enables_push(&env_cred("gh_token"))); // case-insensitive
        assert!(credential_enables_push(&env_cred("SSH_AUTH_SOCK")));
        assert!(credential_enables_push(&CredentialConfig {
            from_secret: Some("k".to_owned()),
            file: Some("/home/agent/.ssh/id_ed25519".to_owned()),
            ..Default::default()
        }));
        // A plain LLM API key is NOT a push credential.
        assert!(!credential_enables_push(&env_cred("ANTHROPIC_API_KEY")));
        assert!(!credential_enables_push(&CredentialConfig {
            from_secret: Some("gcp".to_owned()),
            file: Some("/home/agent/.config/gcloud/adc.json".to_owned()),
            ..Default::default()
        }));
    }

    #[test]
    fn dedicated_workspace_rejects_home_and_ancestors() {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        assert!(
            enforce_dedicated_workspace(&home).is_err(),
            "$HOME must be refused as a workspace"
        );
        if let Some(parent) = home.parent() {
            assert!(
                enforce_dedicated_workspace(parent).is_err(),
                "an ancestor of $HOME must be refused"
            );
        }
        assert!(
            enforce_dedicated_workspace(Path::new("relative/dir")).is_err(),
            "a relative workspace must be refused"
        );
        let dedicated = tmp("dedicated-ok");
        enforce_dedicated_workspace(&dedicated).expect("a dedicated dir must be accepted");
    }

    #[test]
    fn workspace_rw_mount_detection() {
        let ws = tmp("rw-detect");
        let rw = vec![format!("{}:/workspace:rw", ws.display())];
        let ro = vec![format!("{}:/workspace:ro", ws.display())];
        assert!(workspace_mounted_rw(&rw, &ws));
        assert!(!workspace_mounted_rw(&ro, &ws), "ro mount is not writable");
        assert!(!workspace_mounted_rw(&[], &ws), "no mount");
    }

    #[test]
    fn happy_path_passes_every_gate() {
        let ws = tmp("happy");
        let mounts = vec![format!("{}:/workspace:rw", ws.display())];
        enforce_resident_launch(
            "claude",
            "orchestration",
            &isolating_sandbox(),
            &mounts,
            &ws,
            &[env_cred("ANTHROPIC_API_KEY")],
            &no_env(),
            false,
            &resident_policy(),
        )
        .expect("a well-formed sandboxed-resident route must pass");
    }

    #[test]
    fn rejects_unsandboxed_resident() {
        let ws = tmp("local");
        let mounts = vec![format!("{}:/workspace:rw", ws.display())];
        // name == "local"
        let err = enforce_resident_launch(
            "claude",
            "local",
            &SandboxConfig {
                primitive: "local".to_owned(),
                ..SandboxConfig::default()
            },
            &mounts,
            &ws,
            &[],
            &no_env(),
            false,
            &resident_policy(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("isolating sandbox"), "{err}");
    }

    /// Run `enforce_resident_launch` for a resident whose ONLY departure from the happy
    /// path is the given agent/egress list. Returns Ok(()) / the refusal error.
    fn resident_agent_with_egress(agent: &str, tag: &str, egress: &[&str]) -> Result<()> {
        let ws = tmp(tag);
        let mounts = vec![format!("{}:/workspace:rw", ws.display())];
        let mut sandbox = isolating_sandbox();
        sandbox.egress = egress.iter().map(|h| (*h).to_owned()).collect();
        enforce_resident_launch(
            agent,
            "orchestration",
            &sandbox,
            &mounts,
            &ws,
            &[],
            &no_env(),
            false,
            &resident_policy(),
        )
    }

    fn resident_with_egress(tag: &str, egress: &[&str]) -> Result<()> {
        resident_agent_with_egress("claude", tag, egress)
    }

    #[test]
    fn resident_endpoint_inventory_is_agent_specific() {
        assert_eq!(
            resident_egress_allowlist_for_agent("claude").unwrap(),
            CLAUDE_RESIDENT_EGRESS_ALLOWLIST
        );
        assert_eq!(
            resident_egress_allowlist_for_agent("codex").unwrap(),
            CODEX_RESIDENT_EGRESS_ALLOWLIST
        );
        assert_eq!(
            resident_egress_allowlist_for_agent("openai").unwrap(),
            CODEX_RESIDENT_EGRESS_ALLOWLIST
        );
        assert!(
            resident_egress_allowlist_for_agent("copilot")
                .unwrap_err()
                .to_string()
                .contains("unsupported"),
            "Copilot must fail closed until exact non-push endpoints are known"
        );
    }

    #[test]
    fn allows_claude_resident_endpoint_egress() {
        resident_with_egress("claude-llm", CLAUDE_RESIDENT_EGRESS_ALLOWLIST)
            .expect("Claude egress limited to the Claude LLM allowlist must pass");
        // Matching is case-insensitive on the host.
        resident_with_egress("claude-case", &["API.Anthropic.Com"])
            .expect("host match must be case-insensitive");
    }

    #[test]
    fn allows_codex_resident_endpoint_egress() {
        resident_agent_with_egress("codex", "codex-llm", CODEX_RESIDENT_EGRESS_ALLOWLIST)
            .expect("Codex egress limited to the OpenAI allowlist must pass");
        resident_agent_with_egress("codex", "codex-one", &["api.openai.com"])
            .expect("a single Codex endpoint must pass");
    }

    #[test]
    fn rejects_cross_agent_resident_endpoint_egress() {
        let err = resident_agent_with_egress("claude", "claude-openai", &["api.openai.com"])
            .unwrap_err();
        assert!(
            err.to_string().contains("api.openai.com"),
            "Claude resident must not inherit Codex endpoints: {err}"
        );

        let err =
            resident_agent_with_egress("codex", "codex-claude", &["api.anthropic.com"])
                .unwrap_err();
        assert!(
            err.to_string().contains("api.anthropic.com"),
            "Codex resident must not inherit Claude endpoints: {err}"
        );
    }

    #[test]
    fn rejects_copilot_resident_until_exact_non_push_endpoints_are_known() {
        for egress in [&[][..], &["github.com"][..]] {
            let err = resident_agent_with_egress("copilot", "copilot", egress).unwrap_err();
            let msg = err.to_string();
            assert!(msg.contains("Copilot resident sandbox egress is unsupported"), "{msg}");
            assert!(msg.contains("github.com"), "{msg}");
        }
    }

    #[test]
    fn empty_egress_still_passes() {
        // Fully offline (`--network none`) remains allowed.
        resident_with_egress("offline", &[]).expect("an empty egress (offline) must still pass");
    }

    #[test]
    fn rejects_github_egress() {
        // github.com is a push/exfil host — never an LLM endpoint.
        let err = resident_with_egress("gh", &["github.com"]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("github.com"),
            "error must name the host: {msg}"
        );
        assert!(
            msg.contains("LLM"),
            "error must state LLM-only policy: {msg}"
        );
    }

    #[test]
    fn one_bad_host_taints_the_whole_egress_list() {
        // A single non-LLM host is refused even alongside a legitimate LLM endpoint.
        let err = resident_with_egress("mixed", &["api.anthropic.com", "github.com"]).unwrap_err();
        assert!(
            err.to_string().contains("github.com"),
            "the offending host must be named: {err}"
        );
    }

    #[test]
    fn rejects_docker_resident_non_empty_egress_in_strict_mode() {
        let ws = tmp("docker-strict");
        let mounts = vec![format!("{}:/workspace:rw", ws.display())];
        let sandbox = SandboxConfig {
            image: Some("dev:latest".to_owned()),
            primitive: "docker".to_owned(),
            egress: vec!["api.anthropic.com".to_owned()],
            ..SandboxConfig::default()
        };
        let err = enforce_resident_launch(
            "claude-resident",
            "orchestration",
            &sandbox,
            &mounts,
            &ws,
            &[],
            &no_env(),
            false,
            &resident_policy(),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("direct-IP"), "{msg}");
        assert!(msg.contains("refused rather than downgraded"), "{msg}");
        assert!(msg.contains("microsandbox"), "{msg}");
    }

    #[test]
    fn rejects_dns_pin_resident_non_empty_egress() {
        let ws = tmp("resident-dns-pin");
        let mounts = vec![format!("{}:/workspace:rw", ws.display())];
        let sandbox = SandboxConfig {
            image: Some("dev:latest".to_owned()),
            primitive: "docker".to_owned(),
            egress: vec!["api.anthropic.com".to_owned()],
            egress_mode: EgressMode::DnsPin,
            ..SandboxConfig::default()
        };
        let err = enforce_resident_launch(
            "claude-resident",
            "orchestration",
            &sandbox,
            &mounts,
            &ws,
            &[],
            &no_env(),
            false,
            &resident_policy(),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("dns-pin"), "{msg}");
        assert!(msg.contains("direct-IP bypass"), "{msg}");
        assert!(msg.contains("residents require strict"), "{msg}");
    }

    #[test]
    fn rejects_lookalike_hosts_no_suffix_bypass() {
        // Exact match only — a suffix/subdomain of an allowlisted host is NOT admitted.
        for host in [
            "api.openai.com.evil.com",
            "notapi.anthropic.com",
            "api.anthropic.com.attacker.com",
            "evil-api.anthropic.com",
        ] {
            let err = resident_with_egress("lookalike", &[host]).unwrap_err();
            assert!(
                err.to_string().contains(host),
                "look-alike '{host}' must be refused by name: {err}"
            );
        }
    }

    #[test]
    fn rejects_forwarded_ssh_agent_and_push_cred() {
        let ws = tmp("push");
        let mounts = vec![format!("{}:/workspace:rw", ws.display())];
        // Forwarded SSH agent = push channel.
        let err = enforce_resident_launch(
            "claude",
            "orchestration",
            &isolating_sandbox(),
            &mounts,
            &ws,
            &[],
            &no_env(),
            true,
            &resident_policy(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("SSH agent"), "{err}");
        // A push-token credential.
        let err = enforce_resident_launch(
            "claude",
            "orchestration",
            &isolating_sandbox(),
            &mounts,
            &ws,
            &[env_cred("GITHUB_TOKEN")],
            &no_env(),
            false,
            &resident_policy(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("push credential"), "{err}");
    }

    #[test]
    fn rejects_workspace_not_mounted_rw() {
        let ws = tmp("nomount");
        let err = enforce_resident_launch(
            "claude",
            "orchestration",
            &isolating_sandbox(),
            &[format!("{}:/workspace:ro", ws.display())],
            &ws,
            &[],
            &no_env(),
            false,
            &resident_policy(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("read-write"), "{err}");
    }

    #[test]
    fn rejects_disabled_or_local_allowing_policy() {
        let ws = tmp("policy");
        let mounts = vec![format!("{}:/workspace:rw", ws.display())];
        // Broker disabled.
        let mut disabled = resident_policy();
        disabled.enabled = false;
        let err = enforce_resident_launch(
            "claude",
            "orchestration",
            &isolating_sandbox(),
            &mounts,
            &ws,
            &[],
            &no_env(),
            false,
            &disabled,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("orchestration is disabled"),
            "{err}"
        );
        // Workers not pinned away from `local`.
        let mut allows_local = resident_policy();
        allows_local.deny_sandboxes = vec![];
        let err = enforce_resident_launch(
            "claude",
            "orchestration",
            &isolating_sandbox(),
            &mounts,
            &ws,
            &[],
            &no_env(),
            false,
            &allows_local,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("deny the `local` sandbox"),
            "{err}"
        );
    }

    // ---- Finding 1 (BLOCKING) — push-enabling env via any `[…].env` map ----

    #[test]
    fn env_key_enables_push_classifies_channels() {
        // Direct token channels.
        assert!(env_key_enables_push("GITHUB_TOKEN"));
        assert!(env_key_enables_push("gh_token")); // case-insensitive
        assert!(env_key_enables_push("GITLAB_TOKEN"));
        // Askpass / ssh channels.
        assert!(env_key_enables_push("GIT_ASKPASS"));
        assert!(env_key_enables_push("SSH_ASKPASS"));
        assert!(env_key_enables_push("GIT_SSH_COMMAND"));
        assert!(env_key_enables_push("GIT_SSH"));
        assert!(env_key_enables_push("SSH_AUTH_SOCK"));
        // GIT_CONFIG_* credential-helper injection.
        assert!(env_key_enables_push("GIT_CONFIG_COUNT"));
        assert!(env_key_enables_push("GIT_CONFIG_KEY_0"));
        assert!(env_key_enables_push("GIT_CONFIG_VALUE_0"));
        assert!(env_key_enables_push("GIT_CONFIG_GLOBAL"));
        assert!(env_key_enables_push("GIT_CONFIG_SYSTEM"));
        assert!(env_key_enables_push("GIT_TERMINAL_PROMPT"));
        // Plain, non-push env is fine.
        assert!(!env_key_enables_push("ANTHROPIC_API_KEY"));
        assert!(!env_key_enables_push("PATH"));
        assert!(!env_key_enables_push("RUST_LOG"));
    }

    #[test]
    fn rejects_push_enabling_env_map() {
        let ws = tmp("env-push");
        let mounts = vec![format!("{}:/workspace:rw", ws.display())];
        // A resident whose merged env carries GITHUB_TOKEN is refused — even though
        // it arrives via a plain `[…].env` map, not a `[[credentials]]` entry.
        for pairs in [
            &[("GITHUB_TOKEN", "ghp_x")][..],
            &[("GIT_ASKPASS", "/tmp/ask.sh")][..],
            &[("GIT_CONFIG_KEY_0", "credential.helper")][..],
            &[("GIT_CONFIG_COUNT", "1")][..],
            &[("SSH_AUTH_SOCK", "/tmp/agent.sock")][..],
        ] {
            let err = enforce_resident_launch(
                "claude",
                "orchestration",
                &isolating_sandbox(),
                &mounts,
                &ws,
                &[],
                &env_map(pairs),
                false,
                &resident_policy(),
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("push credential"),
                "env {pairs:?} must be refused: {err}"
            );
        }
        // A benign env map still passes.
        enforce_resident_launch(
            "claude",
            "orchestration",
            &isolating_sandbox(),
            &mounts,
            &ws,
            &[],
            &env_map(&[("ANTHROPIC_API_KEY", "sk"), ("RUST_LOG", "info")]),
            false,
            &resident_policy(),
        )
        .expect("a benign env map must pass");
    }

    // ---- Finding 2 (BLOCKING) — file-target credential shapes ----

    #[test]
    fn credential_enables_push_flags_git_credential_file_shapes() {
        let file_cred = |path: &str| CredentialConfig {
            from_secret: Some("k".to_owned()),
            file: Some(path.to_owned()),
            ..Default::default()
        };
        // gh CLI token store.
        assert!(credential_enables_push(&file_cred(
            "/home/agent/.config/gh/hosts.yml"
        )));
        // git credential store files, any name/location.
        assert!(credential_enables_push(&file_cred(
            "/home/agent/.git-credentials"
        )));
        assert!(credential_enables_push(&file_cred(
            "/home/agent/.config/git/credentials"
        )));
        assert!(credential_enables_push(&file_cred(
            "/opt/creds/my-credential-store"
        )));
        // askpass / credential-helper scripts.
        assert!(credential_enables_push(&file_cred(
            "/home/agent/bin/git-askpass.sh"
        )));
        // Still NOT flagged: an LLM/cloud key file.
        assert!(!credential_enables_push(&file_cred(
            "/home/agent/.config/gcloud/adc.json"
        )));
    }

    #[test]
    fn rejects_file_target_push_credential() {
        let ws = tmp("file-push");
        let mounts = vec![format!("{}:/workspace:rw", ws.display())];
        let file_cred = |path: &str| CredentialConfig {
            from_secret: Some("k".to_owned()),
            file: Some(path.to_owned()),
            ..Default::default()
        };
        for path in [
            "/home/agent/.config/gh/hosts.yml",
            "/home/agent/.config/git/credentials",
        ] {
            let err = enforce_resident_launch(
                "claude",
                "orchestration",
                &isolating_sandbox(),
                &mounts,
                &ws,
                &[file_cred(path)],
                &no_env(),
                false,
                &resident_policy(),
            )
            .unwrap_err();
            assert!(
                err.to_string().contains("push credential"),
                "file {path} must be refused: {err}"
            );
        }
    }

    // ---- Finding 3 (BLOCKING) — pre-seeded workspace `.git/config` ----

    #[test]
    fn url_embeds_credential_classifies_remotes() {
        assert!(url_embeds_credential(
            "https://x-access-token:ghp_secret@github.com/foo/bar.git"
        ));
        assert!(url_embeds_credential("https://user:pass@example.com/repo"));
        assert!(url_embeds_credential("https://token@github.com/foo/bar"));
        // Clean remotes — no embedded userinfo.
        assert!(!url_embeds_credential("https://github.com/foo/bar.git"));
        assert!(!url_embeds_credential("git@github.com:foo/bar.git")); // ssh, no scheme://
        assert!(!url_embeds_credential("../relative/path"));
    }

    #[test]
    fn rejects_workspace_git_config_with_embedded_token() {
        let ws = tmp("git-token");
        let mounts = vec![format!("{}:/workspace:rw", ws.display())];
        seed_git_config(
            &ws,
            "[remote \"origin\"]\n\turl = https://x-access-token:ghp_secret@github.com/foo/bar.git\n",
        );
        let err = enforce_resident_launch(
            "claude",
            "orchestration",
            &isolating_sandbox(),
            &mounts,
            &ws,
            &[],
            &no_env(),
            false,
            &resident_policy(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("pre-seeded push credential"),
            "{err}"
        );
        // The secret itself is never echoed in the refusal.
        assert!(
            !err.to_string().contains("ghp_secret"),
            "must not leak token: {err}"
        );
    }

    #[test]
    fn rejects_workspace_git_config_with_credential_helper() {
        let ws = tmp("git-helper");
        let mounts = vec![format!("{}:/workspace:rw", ws.display())];
        seed_git_config(
            &ws,
            "[remote \"origin\"]\n\turl = https://github.com/foo/bar.git\n[credential]\n\thelper = store\n",
        );
        let err = enforce_resident_launch(
            "claude",
            "orchestration",
            &isolating_sandbox(),
            &mounts,
            &ws,
            &[],
            &no_env(),
            false,
            &resident_policy(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("pre-seeded push credential"),
            "{err}"
        );
    }

    #[test]
    fn accepts_workspace_git_config_with_clean_remote() {
        let ws = tmp("git-clean");
        let mounts = vec![format!("{}:/workspace:rw", ws.display())];
        seed_git_config(
            &ws,
            "[remote \"origin\"]\n\turl = https://github.com/foo/bar.git\n\tfetch = +refs/heads/*:refs/remotes/origin/*\n[branch \"main\"]\n\tremote = origin\n",
        );
        enforce_resident_launch(
            "claude",
            "orchestration",
            &isolating_sandbox(),
            &mounts,
            &ws,
            &[env_cred("ANTHROPIC_API_KEY")],
            &no_env(),
            false,
            &resident_policy(),
        )
        .expect("a workspace with a clean, credential-free remote must pass");
    }
}
