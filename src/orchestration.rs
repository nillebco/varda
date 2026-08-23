//! Orchestration M8 — host-side broker policy for gated sub-task spawning.
//!
//! A "master" agent task running inside a sandbox (docker/microVM) may need to
//! decompose work and have sub-agents complete subtasks. It must NOT gain host
//! access to do so — that would defeat the sandbox. The master can only *request*
//! a subtask through one narrow MCP tool (`spawn_subtask`); the trusted host
//! control plane (varda, outside every sandbox) validates the request against
//! policy and, if permitted, runs each subtask in its OWN sibling sandbox.
//!
//! This module is the **host-side** policy engine. It is deliberately pure
//! (no I/O, no process spawning): it decides whether a spawn request is allowed
//! and records accepted spawns against depth / fan-out / global-budget caps. The
//! wiring that exposes `spawn_subtask` into a sandbox and launches the sibling box
//! lives in the run path; this engine is what that wiring must consult before it
//! spawns anything.
//!
//! ## Isolation invariants (MANDATORY — see README "Nested orchestration")
//! These hold for the broker AND the base sandbox; violating any makes the
//! sandbox theater:
//! 1. Never mount the docker socket (`/var/run/docker.sock`) into any agent box.
//! 2. Never mount `~/.varda` (or install the `varda` binary) into an agent box.
//! 3. No `--privileged` / no docker-in-docker for agent boxes. Sub-sandboxes are
//!    **siblings spawned by the host**, never nested inside the master.
//! 4. Spawning is reachable ONLY through the gated `spawn_subtask` MCP tool
//!    mediated by this host-side policy — never via host process access, the
//!    docker socket, or a mounted control plane.
//! 5. Every spawn is bounded by depth + fan-out + global child budget; exceeding a
//!    bound is a hard error ([`SpawnDenied`]), never a silent cap.
//!
//! Invariants 1 and 2 are enforced at the mount layer (see
//! [`crate::sandbox::check_control_plane_denylist`]). Invariant 5 is enforced here.

// The pure policy engine ([`SpawnLedger`]/[`OrchestrationPolicy`]) is consumed by
// the live [`SpawnBroker`] below, which mediates the `spawn_subtask` MCP tool and
// hands accepted spawns to a host [`SubtaskLauncher`]. The run path exposes that
// broker over the sandbox-visible Unix-socket MCP transport in `mcp_transport`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use globset::Glob;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::agent::{parse_blocked_commands, parse_files_touched};
use crate::git::{self, WorkerCheckout, WorkerHarvest};
use crate::task::TaskStatus;

/// Merge-back tool: harvest a wave of finished workers' isolated worktrees and
/// integrate each `wip/<slug>` branch onto the resident's integration worktree,
/// surfacing per-worker conflicts (resolver queue) and G5 manifest flags. This is
/// the host-side step-5 wiring (WORKFLOW.md); it never trusts recap TEXT — only
/// the structured `files_touched` and the launcher-recorded `WorkerCheckout`
/// cross into it (G4).
pub const INTEGRATE_SUBTASKS_TOOL: &str = "integrate_subtasks";

/// Launcher-side registry mapping an accepted subtask id to the isolated
/// [`WorkerCheckout`] the launcher created for it (worktree host path + `wip/`
/// branch). The launcher is the ONLY component that knows a worker's worktree
/// path, so it records it here; the broker's `integrate_subtasks` tool reads it
/// back to run the host-side merge. Shared (`Arc<Mutex<_>>`) so the detached
/// launcher and the broker observe the same map. A subtask absent from the map
/// ran WITHOUT isolation (non-git mother → shared-mount degrade), so it has no
/// branch to integrate and `integrate_subtasks` skips it.
#[derive(Debug, Clone, Default)]
pub struct WorkerRegistry(Arc<Mutex<BTreeMap<SubtaskId, WorkerCheckout>>>);

impl WorkerRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the isolated checkout the launcher created for `id`.
    pub fn record(&self, id: SubtaskId, checkout: WorkerCheckout) {
        self.0
            .lock()
            .expect("worker registry mutex poisoned")
            .insert(id, checkout);
    }

    /// The checkout recorded for `id`, if the worker got an isolated worktree.
    pub fn get(&self, id: &str) -> Option<WorkerCheckout> {
        self.0
            .lock()
            .expect("worker registry mutex poisoned")
            .get(id)
            .cloned()
    }
}

/// Identifier of a task in the spawn tree. The root master task has some id; each
/// accepted subtask gets its own. Used to attribute fan-out and depth.
pub type SubtaskId = String;

/// Host-side policy governing which spawns a sandboxed master may request and how
/// many. All caps are **hard**: exceeding one is a [`SpawnDenied`] error, never a
/// silent truncation (invariant 5).
///
/// Deny always beats allow. An empty allow-list means "allow any (subject to the
/// deny-list)"; a non-empty allow-list means "allow ONLY these".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrchestrationPolicy {
    /// Master switch. When false, every spawn request is denied — a sandboxed
    /// master cannot spawn at all. This is the safe default.
    #[serde(default)]
    pub enabled: bool,
    /// Maximum tree depth. The root master is depth 0; a subtask it spawns is
    /// depth 1, and so on. A request whose resulting child depth exceeds this is
    /// rejected. Prevents recursive fork-bombing. Example: 2.
    #[serde(default = "default_max_depth")]
    pub max_depth: u32,
    /// Maximum direct children a single parent may spawn. Bounds fan-out per node.
    #[serde(default = "default_max_fanout")]
    pub max_fanout: u32,
    /// Global cap on the total number of subtasks spawned across the WHOLE tree,
    /// for one root run. The ultimate fork-bomb backstop.
    #[serde(default = "default_global_child_budget")]
    pub global_child_budget: u32,
    /// Agents the master may spawn. Empty ⇒ any (minus `deny_agents`).
    #[serde(default)]
    pub allow_agents: Vec<String>,
    /// Agents the master may never spawn. Wins over `allow_agents`.
    #[serde(default)]
    pub deny_agents: Vec<String>,
    /// Route globs the master may target. Empty ⇒ any (minus `deny_routes`).
    #[serde(default)]
    pub allow_routes: Vec<String>,
    /// Route globs the master may never target. Wins over `allow_routes`.
    #[serde(default)]
    pub deny_routes: Vec<String>,
    /// Sandbox names a subtask may run in. Empty ⇒ any (minus `deny_sandboxes`).
    /// NOTE: `"local"` (no isolation) should be denied here so a spawned subtask
    /// cannot escape the box; the run path additionally clamps this.
    #[serde(default)]
    pub allow_sandboxes: Vec<String>,
    /// Sandbox names a subtask may never run in. Wins over `allow_sandboxes`.
    /// `"local"` belongs here by default.
    #[serde(default = "default_deny_sandboxes")]
    pub deny_sandboxes: Vec<String>,
    /// If set, spawns landing at this child depth require explicit human approval
    /// before the host will launch them (e.g. `Some(1)` gates the first level).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_approval_at_depth: Option<u32>,
    /// Sandbox a broker-spawned subtask runs in when it does not request one
    /// explicitly. WITHOUT this a spawn falls through to the *route's* sandbox —
    /// on the orchestrate route that is the LLM-only resident box, so a worker
    /// can't fetch crates / build / reach non-Anthropic model APIs (the failure
    /// that stranded subtasks #642/#649/#636). Point it at the worker sandbox so
    /// spawns land where real work can happen, without the resident having to
    /// pass `sandbox=` on every call. An explicit request still wins over this.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_worker_sandbox: Option<String>,
}

fn default_max_depth() -> u32 {
    2
}
fn default_max_fanout() -> u32 {
    4
}
fn default_global_child_budget() -> u32 {
    16
}
fn default_deny_sandboxes() -> Vec<String> {
    vec!["local".to_owned()]
}

impl OrchestrationPolicy {
    /// True when this policy equals the safe default (spawning disabled, `local`
    /// denied). Used by config serde to omit the `[orchestration]` table when it
    /// carries nothing but defaults, so a plain `config.toml` round-trips clean.
    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

impl Default for OrchestrationPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            max_depth: default_max_depth(),
            max_fanout: default_max_fanout(),
            global_child_budget: default_global_child_budget(),
            allow_agents: Vec::new(),
            deny_agents: Vec::new(),
            allow_routes: Vec::new(),
            deny_routes: Vec::new(),
            allow_sandboxes: Vec::new(),
            deny_sandboxes: default_deny_sandboxes(),
            require_approval_at_depth: None,
            default_worker_sandbox: None,
        }
    }
}

/// The sandbox a broker spawn should pin onto its subtask: an explicit `sandbox=`
/// request wins; otherwise the route's configured `default_worker_sandbox`;
/// otherwise `None` (fall through to normal route resolution). Pure so the
/// precedence is unit-testable without a live launcher. See #636.
pub fn spawn_sandbox_override(
    requested: Option<&str>,
    policy: &OrchestrationPolicy,
) -> Option<String> {
    requested
        .map(str::to_owned)
        .or_else(|| policy.default_worker_sandbox.clone())
}

/// Validate the EFFECTIVE agent/sandbox a launcher is about to use — i.e. after
/// resolving any fallback (`self.fallback_agent`, [`spawn_sandbox_override`]) —
/// against the same `allow_agents`/`deny_agents`/`allow_sandboxes`/
/// `deny_sandboxes` policy that [`SpawnLedger::authorize`] enforces on an
/// EXPLICIT `req.agent`/`req.sandbox`. `authorize` only sees a value the caller
/// supplied; a caller-omitted field resolves to a fallback further down the
/// launch path that `authorize` never observes, so without this check a
/// configured `default_worker_sandbox` (or fallback agent) could silently
/// bypass `deny_sandboxes`/`deny_agents`. Call this AFTER fallback resolution
/// and BEFORE the host actually launches the subtask.
pub fn check_effective_placement(
    policy: &OrchestrationPolicy,
    agent: &str,
    sandbox: Option<&str>,
) -> Result<(), SpawnDenied> {
    if !list_allows(&policy.allow_agents, &policy.deny_agents, agent) {
        return Err(SpawnDenied::AgentNotAllowed {
            agent: agent.to_owned(),
        });
    }
    if let Some(sandbox) = sandbox
        && !list_allows(&policy.allow_sandboxes, &policy.deny_sandboxes, sandbox)
    {
        return Err(SpawnDenied::SandboxNotAllowed {
            sandbox: sandbox.to_owned(),
        });
    }
    Ok(())
}

/// A spawn request as it arrives from the sandboxed master through the broker.
/// `route`/`agent`/`sandbox` are the *requested* placement; the host resolves and
/// re-validates them — the master never picks the actual host command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRequest {
    /// Free-text brief describing the subtask (becomes the child task body).
    pub brief: String,
    /// Requested route glob, if the master named one.
    pub route: Option<String>,
    /// Requested agent, if the master named one.
    pub agent: Option<String>,
    /// Requested sandbox name, if the master named one.
    pub sandbox: Option<String>,
    /// Set true only after a human approved this specific spawn (satisfies
    /// `require_approval_at_depth`).
    pub approved: bool,
}

/// Context of the requester: which task is asking and how deep it already sits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnContext {
    /// Id of the requesting (parent) task.
    pub parent_id: SubtaskId,
    /// Depth of the requesting task in the tree (root master = 0).
    pub parent_depth: u32,
}

/// Why a spawn was refused. Every variant is a HARD error surfaced to the master
/// (invariant 5): the broker never silently caps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnDenied {
    /// Orchestration is disabled by policy; no spawns allowed.
    Disabled,
    /// Resulting child depth would exceed `max_depth`.
    DepthExceeded { attempted: u32, max: u32 },
    /// The parent already has `max_fanout` children.
    FanoutExceeded { parent: SubtaskId, max: u32 },
    /// The global child budget for this run is exhausted.
    BudgetExceeded { spent: u32, budget: u32 },
    /// The requested agent is not permitted.
    AgentNotAllowed { agent: String },
    /// The requested route is not permitted.
    RouteNotAllowed { route: String },
    /// The requested sandbox is not permitted (e.g. `local`, which escapes).
    SandboxNotAllowed { sandbox: String },
    /// This spawn depth requires human approval that was not granted.
    ApprovalRequired { depth: u32 },
}

impl std::fmt::Display for SpawnDenied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnDenied::Disabled => {
                write!(f, "sub-task spawning is disabled by orchestration policy")
            }
            SpawnDenied::DepthExceeded { attempted, max } => write!(
                f,
                "spawn denied: child depth {attempted} exceeds max_depth {max} (recursion cap)"
            ),
            SpawnDenied::FanoutExceeded { parent, max } => write!(
                f,
                "spawn denied: parent '{parent}' already has max_fanout {max} children"
            ),
            SpawnDenied::BudgetExceeded { spent, budget } => write!(
                f,
                "spawn denied: global child budget exhausted ({spent}/{budget} spent)"
            ),
            SpawnDenied::AgentNotAllowed { agent } => {
                write!(
                    f,
                    "spawn denied: agent '{agent}' is not permitted by policy"
                )
            }
            SpawnDenied::RouteNotAllowed { route } => {
                write!(
                    f,
                    "spawn denied: route '{route}' is not permitted by policy"
                )
            }
            SpawnDenied::SandboxNotAllowed { sandbox } => write!(
                f,
                "spawn denied: sandbox '{sandbox}' is not permitted by policy (a subtask must run in an isolating sibling box, never `local`)"
            ),
            SpawnDenied::ApprovalRequired { depth } => write!(
                f,
                "spawn denied: spawns at depth {depth} require human approval"
            ),
        }
    }
}

impl std::error::Error for SpawnDenied {}

/// An accepted spawn: what the host should launch, plus the assigned child depth.
/// Returned by [`SpawnLedger::authorize`]; the caller records it via
/// [`SpawnLedger::record`] once the sibling sandbox is actually launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnGrant {
    /// Depth the child will occupy (parent_depth + 1).
    pub child_depth: u32,
}

/// Mutable book-keeping across one root run: how many children each parent has and
/// how many total spawns have happened, so depth / fan-out / budget can be checked.
#[derive(Debug, Clone, Default)]
pub struct SpawnLedger {
    global_spawned: u32,
    children: BTreeMap<SubtaskId, u32>,
}

impl SpawnLedger {
    #[cfg(test)]
    pub fn new() -> Self {
        Self::default()
    }

    /// Total subtasks spawned so far this run.
    #[cfg(test)]
    pub fn global_spawned(&self) -> u32 {
        self.global_spawned
    }

    /// How many direct children `parent` has spawned so far.
    pub fn children_of(&self, parent: &str) -> u32 {
        self.children.get(parent).copied().unwrap_or(0)
    }

    /// Validate a spawn against the policy and the current ledger WITHOUT mutating
    /// state. Returns the [`SpawnGrant`] to launch, or a hard [`SpawnDenied`].
    ///
    /// Check order (fail closed, cheapest structural caps first):
    /// disabled → depth → fan-out → global budget → agent → route → sandbox →
    /// approval. Deny-lists always beat allow-lists.
    pub fn authorize(
        &self,
        policy: &OrchestrationPolicy,
        ctx: &SpawnContext,
        req: &SpawnRequest,
    ) -> Result<SpawnGrant, SpawnDenied> {
        if !policy.enabled {
            return Err(SpawnDenied::Disabled);
        }

        let child_depth = ctx.parent_depth + 1;
        if child_depth > policy.max_depth {
            return Err(SpawnDenied::DepthExceeded {
                attempted: child_depth,
                max: policy.max_depth,
            });
        }

        if self.children_of(&ctx.parent_id) + 1 > policy.max_fanout {
            return Err(SpawnDenied::FanoutExceeded {
                parent: ctx.parent_id.clone(),
                max: policy.max_fanout,
            });
        }

        if self.global_spawned + 1 > policy.global_child_budget {
            return Err(SpawnDenied::BudgetExceeded {
                spent: self.global_spawned,
                budget: policy.global_child_budget,
            });
        }

        if let Some(agent) = &req.agent
            && !list_allows(&policy.allow_agents, &policy.deny_agents, agent)
        {
            return Err(SpawnDenied::AgentNotAllowed {
                agent: agent.clone(),
            });
        }

        if let Some(route) = &req.route
            && !glob_list_allows(&policy.allow_routes, &policy.deny_routes, route)
        {
            return Err(SpawnDenied::RouteNotAllowed {
                route: route.clone(),
            });
        }

        if let Some(sandbox) = &req.sandbox
            && !list_allows(&policy.allow_sandboxes, &policy.deny_sandboxes, sandbox)
        {
            return Err(SpawnDenied::SandboxNotAllowed {
                sandbox: sandbox.clone(),
            });
        }

        if let Some(gate_depth) = policy.require_approval_at_depth
            && child_depth == gate_depth
            && !req.approved
        {
            return Err(SpawnDenied::ApprovalRequired { depth: child_depth });
        }

        Ok(SpawnGrant { child_depth })
    }

    /// Record an accepted spawn reservation against the caller (parent) and the
    /// global budget. Call this only after [`authorize`](Self::authorize)
    /// succeeded; if the sibling sandbox then fails to launch, roll it back with
    /// [`unrecord`](Self::unrecord) so the ledger reflects reality.
    pub fn record(&mut self, parent: &str) {
        *self.children.entry(parent.to_owned()).or_insert(0) += 1;
        self.global_spawned += 1;
    }

    /// Roll back one recorded spawn for `parent` (both its fan-out tally and the
    /// global budget). Used by the live broker when an authorized spawn is
    /// recorded but the host then fails to launch the sibling box, so a failed
    /// attempt never permanently consumes budget. Saturates at zero.
    pub fn unrecord(&mut self, parent: &str) {
        if let Some(count) = self.children.get_mut(parent)
            && *count > 0
        {
            *count -= 1;
            self.global_spawned = self.global_spawned.saturating_sub(1);
        }
    }

    /// Convenience: authorize and, on success, record in one step. Returns the
    /// grant so the caller knows the child depth.
    pub fn authorize_and_record(
        &mut self,
        policy: &OrchestrationPolicy,
        ctx: &SpawnContext,
        req: &SpawnRequest,
    ) -> Result<SpawnGrant, SpawnDenied> {
        let grant = self.authorize(policy, ctx, req)?;
        self.record(&ctx.parent_id);
        Ok(grant)
    }
}

/// Exact allow/deny decision for a plain-string value (agent, sandbox). Deny wins;
/// an empty allow-list means "allow all not denied".
fn list_allows(allow: &[String], deny: &[String], value: &str) -> bool {
    if deny.iter().any(|d| d == value) {
        return false;
    }
    allow.is_empty() || allow.iter().any(|a| a == value)
}

/// Glob allow/deny decision (routes). Deny wins; empty allow-list means "allow all
/// not denied". A malformed glob never matches (fails closed on the allow side).
fn glob_list_allows(allow: &[String], deny: &[String], value: &str) -> bool {
    if deny.iter().any(|g| glob_matches(g, value)) {
        return false;
    }
    allow.is_empty() || allow.iter().any(|g| glob_matches(g, value))
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    match Glob::new(pattern) {
        Ok(g) => g.compile_matcher().is_match(value),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------------------
// Live broker — the `spawn_subtask` MCP tool + host-mediated sibling launch.
//
// The broker is the ONLY thing a sandboxed master can reach across the boundary
// (invariant 4). It owns the [`SpawnLedger`] and a depth registry (lineage), and
// on every tool call it consults [`SpawnLedger::authorize_and_record`] BEFORE it
// asks the host to launch anything. The actual launch is delegated to a host
// [`SubtaskLauncher`], which reuses the normal run path to start each subtask in
// its OWN sibling sandbox — never nested, never with the docker socket or
// `~/.varda` in reach (invariants 1–3). This module never spawns a process
// itself; it decides, records lineage, and calls the launcher.
// ---------------------------------------------------------------------------

/// Name of the single spawn tool exposed into the sandbox. Nothing else crosses.
pub const SPAWN_SUBTASK_TOOL: &str = "spawn_subtask";
/// Run an EXISTING task (by id) instead of minting a fresh one from a brief. Same
/// host-mediation and policy gate as [`SPAWN_SUBTASK_TOOL`]; avoids the duplicate
/// task doc a master would otherwise create by re-describing a task it already
/// planned. See [`SpawnBroker::run_subtask`].
pub const RUN_SUBTASK_TOOL: &str = "run_subtask";
/// Collect-side read-back tools (result plumbing). A master spawns with
/// [`SPAWN_SUBTASK_TOOL`], then blocks on one of these to harvest the child's
/// terminal status and recap once the host STATE store records them.
pub const AWAIT_SUBTASK_TOOL: &str = "await_subtask";
pub const SUBTASK_RESULT_TOOL: &str = "subtask_result";
/// Host-computed unified diff of a finished subtask's worktree — the reviewer's
/// window into the reviewed worker's actual changes (which it cannot reach itself).
pub const SUBTASK_DIFF_TOOL: &str = "subtask_diff";
/// Wave primitive: block until EVERY listed subtask reaches a terminal status.
pub const AWAIT_SUBTASKS_TOOL: &str = "await_subtasks";
/// Task control-plane read/write tools (task #640): let a sandboxed agent see
/// and update its OWN project's tasks without any `~/.varda` mount. See
/// [`TaskControlPlane`] for the gating.
pub const LIST_TASKS_TOOL: &str = "list_tasks";
pub const GET_TASK_TOOL: &str = "get_task";
pub const SET_TASK_STATUS_TOOL: &str = "set_task_status";

/// Tool error returned by `await_subtask*`/`subtask_result` when no collect
/// channel is wired ([`NoSubtaskResults`]): the pre-collect stub semantics,
/// short-circuited so an unwired broker reports at once rather than polling to
/// the [`SpawnBroker::max_wait`] ceiling.
const RESULTS_UNAVAILABLE: &str =
    "not available on this channel: sub-task results flow back through varda memory";

/// Default poll cadence for `await_subtask*`: how often the broker re-reads a
/// child's status while blocking. Task-spec range is 1–2s; 1s keeps a wave
/// responsive without hammering the STATE store.
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// Absolute ceiling on a single `await_subtask*` call. A wedged child that never
/// reaches a terminal status must not hang the master forever, so the collect
/// channel gives up after this and returns a timeout error (never a silent
/// success). 30 minutes comfortably outlasts a normal headless subtask run.
const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(30 * 60);

/// A terminal status is one a subtask run has actually settled on: the runner
/// writes exactly `Done`/`Failed`/`NeedsUser`/`Review` when a headless run ends
/// (`runner::run_task`). `Ready`/`Running`/`Backlog` mean "still in flight", so
/// the collect channel keeps polling.
fn is_terminal(status: TaskStatus) -> bool {
    matches!(
        status,
        TaskStatus::Done | TaskStatus::Failed | TaskStatus::NeedsUser | TaskStatus::Review
    )
}

/// The host-side seam the collect channel reads through: resolve a subtask id to
/// its current terminal status and (once finished) its recap TEXT. The broker is
/// pure and has no handle to the `~/.varda` STATE store, so the run path injects a
/// concrete implementation (see `main::VardaSubtaskResults`) that resolves ids via
/// the `task::` helpers. Kept read-only and object-safe so one impl can serve both
/// the sandboxed broker and the resident host.
pub trait SubtaskResults: Send + Sync {
    /// Current status of `id`, or `None` if no task carries that id.
    fn status(&self, id: &str) -> Option<TaskStatus>;
    /// Most recent recap TEXT for `id`, or `None` if the task is unknown or has
    /// not yet produced a recap.
    fn recap(&self, id: &str) -> Option<String>;
    /// Unified diff of the finished subtask's WORKING-TREE changes — the worker's
    /// actual output — computed HOST-side from its worktree. This is what a
    /// cross-reviewer inspects: it cannot `git diff` the reviewed worker itself
    /// (that worker's uncommitted changes live in its own out-of-tree worktree,
    /// which is never mounted into another box — M8/#578), so the host hands it the
    /// diff text. `None` when unavailable; default: none.
    fn diff(&self, _id: &str) -> Option<String> {
        None
    }
    /// Whether a real collect channel is wired. A wired provider leaves the
    /// default `true`; the no-op [`NoSubtaskResults`] overrides to `false` so
    /// `await_subtask*`/`subtask_result` short-circuit INSTANTLY with the "not
    /// available on this channel" error instead of polling every id to the
    /// [`SpawnBroker::max_wait`] ceiling (30 min) before timing out. This
    /// preserves the pre-collect stub semantics for an unwired broker.
    fn is_available(&self) -> bool {
        true
    }
}

/// Default results seam: no collect channel wired. `status`/`recap` always return
/// `None` and `is_available` is `false`, so `await_subtask*`/`subtask_result`
/// short-circuit INSTANTLY with the "not available on this channel" error instead
/// of polling to the ceiling — preserving the pre-collect behaviour until the run
/// path injects a real [`SubtaskResults`] via [`SpawnBroker::with_results`].
struct NoSubtaskResults;

impl SubtaskResults for NoSubtaskResults {
    fn status(&self, _id: &str) -> Option<TaskStatus> {
        None
    }
    fn recap(&self, _id: &str) -> Option<String> {
        None
    }
    fn is_available(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Task control-plane read/write tools — `list_tasks` / `get_task` /
// `set_task_status`. Same host-mediation shape as the spawn/collect tools
// above: a sandboxed agent has no mount of `~/.varda` (M8), so it can only
// SEE or UPDATE its own project's tasks by asking the host through this
// narrow, scoped seam. See `SpawnBroker::list_tasks`/`get_task`/
// `set_task_status` for the gating; `TaskControlPlane` is the host-side I/O
// the run path injects (`main::VardaTaskControlPlane`, backed by `task::`).
// ---------------------------------------------------------------------------

/// Redacted task-board entry returned by `list_tasks`. Only the fields a
/// caller needs to see its project's board — never a host filesystem path,
/// recap, run log, or credential (M8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskListEntry {
    pub id: u64,
    pub slug: String,
    pub status: String,
    pub title: String,
    pub assignee: Option<String>,
}

/// Redacted task detail returned by `get_task`: the definition body plus its
/// status/assignee. Deliberately excludes recaps, run-log paths, and session
/// ids/logs — those are host filesystem paths that would leak the
/// control-plane layout through the broker (see
/// `TaskFrontmatter::sanitized_for_prompt`, the same redaction principle
/// applied here).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskDetail {
    pub id: u64,
    pub slug: String,
    pub status: String,
    pub title: String,
    pub assignee: Option<String>,
    pub body: String,
}

/// Host-side seam the task control-plane tools read/write through. The
/// broker never touches `~/.varda` itself — it calls this trait, and the run
/// path injects a concrete impl (`main::VardaTaskControlPlane`) that wraps
/// the `task::` helpers. Every method is scoped by the CALLER'S OWN
/// `project_path`, which the broker resolves itself from how it was wired
/// (never attacker-supplied) — see [`SpawnBroker::with_task_control_plane`].
pub trait TaskControlPlane: Send + Sync {
    /// The tasks belonging to `project_path`, optionally filtered by status.
    fn list_tasks(&self, project_path: &Path, status: Option<TaskStatus>) -> Vec<TaskListEntry>;
    /// One task by id, scoped to `project_path`. `None` if `id` does not
    /// belong to that project (including a real id from ANOTHER project —
    /// this is how cross-project reads are refused).
    fn get_task(&self, project_path: &Path, id: u64) -> Option<TaskDetail>;
    /// Write a new status for `id`, scoped to `project_path`. Callers MUST
    /// have already validated the transition (see
    /// [`validate_self_status_transition`]) — this is the low-level write,
    /// not a gate. `Err` carries a human-readable reason (e.g. unknown id).
    fn set_task_status(&self, project_path: &Path, id: u64, status: TaskStatus) -> Result<(), String>;
}

/// Default control-plane seam: no host wiring. Used until the run path
/// injects a real impl via [`SpawnBroker::with_task_control_plane`], mirroring
/// [`NoSubtaskResults`].
struct NoTaskControlPlane;

impl TaskControlPlane for NoTaskControlPlane {
    fn list_tasks(&self, _project_path: &Path, _status: Option<TaskStatus>) -> Vec<TaskListEntry> {
        Vec::new()
    }
    fn get_task(&self, _project_path: &Path, _id: u64) -> Option<TaskDetail> {
        None
    }
    fn set_task_status(&self, _project_path: &Path, _id: u64, _status: TaskStatus) -> Result<(), String> {
        Err(RESULTS_UNAVAILABLE.to_owned())
    }
}

/// Validate a `set_task_status` request BEFORE it reaches the control-plane
/// write. Two rules, both hard (never a silent no-op):
///
/// 1. Guests may only land on a TERMINAL self-report status: `done`,
///    `needs_user`, or `failed`. `backlog`/`ready`/`running`/`review` are
///    host-managed transitions a guest may never assert.
/// 2. The task must currently be `running`. In particular `review -> done`
///    is a human review gate: forbidden here rather than allowed-with-a-flag,
///    so a self-marked completion can never be confused with a reviewed one
///    (task #640's chosen option — simpler than threading an
///    "agent-asserted" bit through the frontmatter/CLI for the same
///    guarantee: a guest simply cannot move a task out of `review`).
fn validate_self_status_transition(current: TaskStatus, requested: TaskStatus) -> Result<(), String> {
    if !matches!(
        requested,
        TaskStatus::Done | TaskStatus::NeedsUser | TaskStatus::Failed
    ) {
        return Err(format!(
            "set_task_status denied: guests may only set 'done', 'needs_user', or 'failed' (not '{}')",
            requested.as_str()
        ));
    }
    match current {
        TaskStatus::Running => Ok(()),
        TaskStatus::Review => Err(
            "set_task_status denied: task is in 'review' — review -> done is a human-only gate \
             and cannot be self-asserted by a guest agent"
                .to_owned(),
        ),
        other => Err(format!(
            "set_task_status denied: task is '{}', not 'running' — set_task_status only closes \
             out an in-flight run",
            other.as_str()
        )),
    }
}

/// The host-side seam that actually launches an AUTHORIZED subtask in its own
/// sibling sandbox. The broker calls this ONLY after
/// [`SpawnLedger::authorize_and_record`] has succeeded, passing the re-validated
/// request and the assigned child depth. Implementations reuse the normal run
/// path (materialize a task + run it in a fresh box); they MUST NOT nest inside
/// the caller's sandbox (invariant 3). Returns the new subtask id.
pub trait SubtaskLauncher {
    fn launch(&mut self, req: &SpawnRequest, grant: &SpawnGrant) -> anyhow::Result<SubtaskId>;

    /// Run an EXISTING task (identified by `task_id`) in its own sibling sandbox —
    /// the `run_subtask` path. Unlike [`launch`](Self::launch), no task doc is
    /// created; the host resolves the id to its existing task, RE-VALIDATES that
    /// task's own placement (route/agent/sandbox) through normal route resolution
    /// exactly as `launch` does (the id is caller-supplied and never trusted), and
    /// normalizes its status so any prior state is runnable without a separate
    /// "prepare the task" tool. Implementations MUST refuse to re-run a task that
    /// is already executing in-process (a live [`JoinHandle`]) so a second run can
    /// never orphan the first. `grant` carries the authorized child depth so
    /// lineage is recorded identically to a fresh spawn. Returns the task id.
    fn run_existing(
        &mut self,
        task_id: &str,
        grant: &SpawnGrant,
    ) -> anyhow::Result<SubtaskId>;
}

/// Why a `spawn_subtask` call failed, distinguishing a policy denial (a hard
/// [`SpawnDenied`], surfaced verbatim to the master) from an unknown-lineage
/// spoof attempt and a host-side launch failure.
#[derive(Debug)]
pub enum BrokerError {
    /// The policy engine rejected the spawn. Surfaced to the caller unchanged.
    Denied(SpawnDenied),
    /// The calling task id is not in the lineage registry — it was never spawned
    /// through this broker, so its depth is unknown and it may not spawn. Closes
    /// the "claim a shallow depth to dodge the recursion cap" spoof: the caller
    /// never supplies its own depth, the broker looks it up.
    UnknownParent(SubtaskId),
    /// The policy allowed the spawn but the host failed to launch the sibling box.
    /// The ledger is rolled back so the failed attempt does not consume budget.
    Launch(String),
}

impl std::fmt::Display for BrokerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrokerError::Denied(d) => write!(f, "{d}"),
            BrokerError::UnknownParent(id) => write!(
                f,
                "spawn denied: caller '{id}' was not spawned through this broker (unknown lineage)"
            ),
            BrokerError::Launch(msg) => write!(f, "subtask launch failed: {msg}"),
        }
    }
}

impl std::error::Error for BrokerError {}

#[derive(Debug, Default)]
struct SpawnTreeState {
    ledger: SpawnLedger,
    /// Task id → depth in the tree. The root master is registered at depth 0 (or
    /// at its inherited depth for a spawned subtask run); each accepted child is
    /// registered at its granted depth so future brokers resolve depth correctly.
    depths: BTreeMap<SubtaskId, u32>,
    /// Detached child runs still owned by this root orchestration run. The run
    /// path drains these on normal completion before tearing down the broker
    /// socket, or aborts them when the parent run itself errors/cancels.
    handles: BTreeMap<SubtaskId, JoinHandle<()>>,
}

/// Shared spawn tree state for one root orchestration run. Clone and pass this
/// handle to every descendant broker so `max_depth`, `max_fanout`, and
/// `global_child_budget` compose across generations.
#[derive(Debug, Clone, Default)]
pub struct SharedSpawnState(Arc<Mutex<SpawnTreeState>>);

impl SharedSpawnState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_handle(&self, id: SubtaskId, handle: JoinHandle<()>) {
        self.0
            .lock()
            .expect("spawn state mutex poisoned")
            .handles
            .insert(id, handle);
    }

    pub fn drain_handles(&self) -> Vec<(SubtaskId, JoinHandle<()>)> {
        std::mem::take(&mut self.0.lock().expect("spawn state mutex poisoned").handles)
            .into_iter()
            .collect()
    }

    /// Whether a LIVE (not-yet-finished) detached run exists for `id` in this root
    /// run. The `run_subtask` path consults this to refuse re-running a task that
    /// is currently executing in-process, which would clobber its [`JoinHandle`]
    /// and orphan the running child. A finished-but-undrained handle is NOT live:
    /// a completed or crashed prior run may be safely re-run.
    pub fn has_live_handle(&self, id: &str) -> bool {
        self.0
            .lock()
            .expect("spawn state mutex poisoned")
            .handles
            .get(id)
            .is_some_and(|handle| !handle.is_finished())
    }

    #[cfg(test)]
    pub fn handle_count(&self) -> usize {
        self.0
            .lock()
            .expect("spawn state mutex poisoned")
            .handles
            .len()
    }
}

/// The live broker: policy + shared ledger/lineage registry + a host launcher.
/// All brokers for one root run share [`SharedSpawnState`], so the lineage map is
/// the real spawn tree the `max_depth` cap is enforced against.
pub struct SpawnBroker<L: SubtaskLauncher> {
    policy: OrchestrationPolicy,
    state: SharedSpawnState,
    launcher: Mutex<L>,
    /// Collect-side seam (id → status/recap). Defaults to [`NoSubtaskResults`];
    /// the run path swaps in a real impl via [`SpawnBroker::with_results`].
    results: Box<dyn SubtaskResults>,
    /// How often `await_subtask*` re-reads a child's status while blocking.
    poll_interval: Duration,
    /// Absolute ceiling on one `await_subtask*` call before it times out.
    max_wait: Duration,
    /// Launcher-side registry of isolated worker checkouts (subtask id →
    /// worktree/branch). Populated as the launcher creates worktrees; read by
    /// `integrate_subtasks` to run the host-side merge-back. Empty until wired.
    worker_registry: WorkerRegistry,
    /// The resident's own integration worktree — its mounted workspace root.
    /// `integrate_subtasks` commits + merges each worker branch onto the branch
    /// checked out here. `None` disables merge-back (the tool reports unavailable),
    /// keeping the resident purely advisory until the run path wires it.
    integration_worktree: Option<PathBuf>,
    /// Task control-plane seam (task #640). Defaults to [`NoTaskControlPlane`];
    /// the run path swaps in a real impl via
    /// [`SpawnBroker::with_task_control_plane`].
    task_control_plane: Box<dyn TaskControlPlane>,
    /// The CALLER'S OWN project, scoping every `list_tasks`/`get_task`/
    /// `set_task_status` call. `None` until wired — the tools then behave as
    /// unavailable rather than reading/writing an arbitrary project.
    task_control_plane_project: Option<PathBuf>,
}

impl<L: SubtaskLauncher> SpawnBroker<L> {
    /// Create a broker for a root run. `root_id` is the master task id; it is
    /// registered at depth 0 so its spawns land at depth 1.
    #[cfg(test)]
    pub fn new(policy: OrchestrationPolicy, root_id: impl Into<SubtaskId>, launcher: L) -> Self {
        Self::with_shared_state(policy, root_id, 0, SharedSpawnState::new(), launcher)
    }

    /// Create a broker for a descendant run, sharing the root run's ledger and
    /// lineage state. `root_depth` must be the depth granted by the parent broker.
    pub fn with_shared_state(
        policy: OrchestrationPolicy,
        root_id: impl Into<SubtaskId>,
        root_depth: u32,
        state: SharedSpawnState,
        launcher: L,
    ) -> Self {
        state
            .0
            .lock()
            .expect("spawn state mutex poisoned")
            .depths
            .insert(root_id.into(), root_depth);
        Self {
            policy,
            state,
            launcher: Mutex::new(launcher),
            results: Box::new(NoSubtaskResults),
            poll_interval: DEFAULT_POLL_INTERVAL,
            max_wait: DEFAULT_MAX_WAIT,
            worker_registry: WorkerRegistry::new(),
            integration_worktree: None,
            task_control_plane: Box::new(NoTaskControlPlane),
            task_control_plane_project: None,
        }
    }

    /// Wire the merge-back capability: the launcher-side [`WorkerRegistry`] the
    /// tool harvests worktrees from, and the resident's `integration_worktree`
    /// (its mounted workspace) each worker branch is merged onto. Until this is
    /// called, `integrate_subtasks` reports "not available on this channel".
    /// Builder style, mirroring [`with_results`](Self::with_results).
    pub fn with_integration(
        mut self,
        worker_registry: WorkerRegistry,
        integration_worktree: PathBuf,
    ) -> Self {
        self.worker_registry = worker_registry;
        self.integration_worktree = Some(integration_worktree);
        self
    }

    /// Wire the task control-plane capability (task #640): the host-side
    /// [`TaskControlPlane`] impl plus the CALLER'S OWN `project_path`, which
    /// scopes every `list_tasks`/`get_task`/`set_task_status` call. Until this
    /// is called those tools report "not available on this channel". Builder
    /// style, mirroring [`with_results`](Self::with_results).
    pub fn with_task_control_plane(
        mut self,
        control_plane: impl TaskControlPlane + 'static,
        project_path: PathBuf,
    ) -> Self {
        self.task_control_plane = Box::new(control_plane);
        self.task_control_plane_project = Some(project_path);
        self
    }

    /// Inject the collect-side [`SubtaskResults`] seam. Consumes and returns the
    /// broker (builder style) so wiring reads
    /// `SpawnBroker::with_shared_state(..).with_results(host_impl)`. Until this is
    /// called the broker uses [`NoSubtaskResults`] and the read-back tools report
    /// "not available on this channel".
    pub fn with_results(mut self, results: impl SubtaskResults + 'static) -> Self {
        self.results = Box::new(results);
        self
    }

    /// Override the blocking poll cadence and absolute timeout for
    /// `await_subtask*`. Builder style; used by tests to keep the never-terminal
    /// bound check fast, and available to the run path to cap by a caller's
    /// `max_seconds` when desired.
    #[cfg(test)]
    pub fn with_poll_timing(mut self, poll_interval: Duration, max_wait: Duration) -> Self {
        self.poll_interval = poll_interval;
        self.max_wait = max_wait;
        self
    }

    /// Total subtasks spawned so far this run (for observability/tests).
    #[cfg(test)]
    pub fn global_spawned(&self) -> u32 {
        self.state
            .0
            .lock()
            .expect("spawn state mutex poisoned")
            .ledger
            .global_spawned()
    }

    /// Depth of a known task, or `None` if it was never registered.
    #[cfg(test)]
    pub fn depth_of(&self, id: &str) -> Option<u32> {
        self.state
            .0
            .lock()
            .expect("spawn state mutex poisoned")
            .depths
            .get(id)
            .copied()
    }

    /// Handle one `spawn_subtask` request from `parent_id` (the id of the sandbox
    /// that owns the broker channel — the host knows this; it is NOT attacker
    /// supplied). Gates on policy, launches a sibling on success, and records
    /// lineage for the new child. Every denial is a hard error (invariant 5).
    pub fn spawn_subtask(
        &self,
        parent_id: &str,
        req: SpawnRequest,
    ) -> Result<SubtaskId, BrokerError> {
        self.gated_launch(parent_id, &req, |grant| {
            self.launcher
                .lock()
                .expect("subtask launcher mutex poisoned")
                .launch(&req, grant)
        })
    }

    /// Handle one `run_subtask` request from `parent_id`: run an EXISTING task
    /// (chosen by `task_id`) in its own sibling sandbox, instead of minting a fresh
    /// task from a brief. This is the anti-duplication path — a master that already
    /// planned a task runs it directly rather than re-describing it into a second
    /// doc. It goes through the IDENTICAL structural gate as [`spawn_subtask`]
    /// (lineage + depth/fan-out/budget) via [`gated_launch`], so the two paths can
    /// never drift on policy. The launcher re-validates the existing task's own
    /// route/agent/sandbox (the definitive placement enforcement) and normalizes
    /// its status, so any prior state is runnable with no separate "prepare" tool.
    pub fn run_subtask(
        &self,
        parent_id: &str,
        task_id: &str,
    ) -> Result<SubtaskId, BrokerError> {
        // The structural gate needs no requested placement: the launcher re-resolves
        // and re-validates the existing task's own route/agent/sandbox (exactly as
        // the spawn path relies on route resolution for definitive enforcement).
        // Depth, fan-out and budget are all the ledger reads from the request here.
        let req = SpawnRequest {
            brief: String::new(),
            route: None,
            agent: None,
            sandbox: None,
            approved: false,
        };
        self.gated_launch(parent_id, &req, |grant| {
            self.launcher
                .lock()
                .expect("subtask launcher mutex poisoned")
                .run_existing(task_id, grant)
        })
    }

    /// Shared gate for BOTH spawn paths (`spawn_subtask` / `run_subtask`): resolve
    /// the caller's depth (lineage guard — the caller never supplies its own
    /// depth), run the SAME [`SpawnLedger::authorize_and_record`] structural gate,
    /// then invoke `launch` to actually start the child. On a launch failure the
    /// ledger reservation is rolled back so a failed attempt never permanently
    /// consumes fan-out / global budget. Keeping ONE gate is why the fresh-brief
    /// and existing-task paths can never diverge on policy.
    fn gated_launch(
        &self,
        parent_id: &str,
        req: &SpawnRequest,
        launch: impl FnOnce(&SpawnGrant) -> anyhow::Result<SubtaskId>,
    ) -> Result<SubtaskId, BrokerError> {
        let grant = {
            let mut state = self.state.0.lock().expect("spawn state mutex poisoned");
            let parent_depth = state
                .depths
                .get(parent_id)
                .copied()
                .ok_or_else(|| BrokerError::UnknownParent(parent_id.to_owned()))?;
            let ctx = SpawnContext {
                parent_id: parent_id.to_owned(),
                parent_depth,
            };
            state
                .ledger
                .authorize_and_record(&self.policy, &ctx, req)
                .map_err(BrokerError::Denied)?
        };

        match launch(&grant) {
            Ok(child_id) => {
                self.state
                    .0
                    .lock()
                    .expect("spawn state mutex poisoned")
                    .depths
                    .insert(child_id.clone(), grant.child_depth);
                Ok(child_id)
            }
            Err(e) => {
                // The host failed to launch: undo the ledger record so the failed
                // attempt does not consume fan-out / global budget.
                self.state
                    .0
                    .lock()
                    .expect("spawn state mutex poisoned")
                    .ledger
                    .unrecord(parent_id);
                Err(BrokerError::Launch(e.to_string()))
            }
        }
    }

    /// Block until `id` reaches a terminal status, polling the injected
    /// [`SubtaskResults`] every [`Self::poll_interval`]. Returns the terminal
    /// status, or `None` if the [`Self::max_wait`] ceiling elapses first (a wedged
    /// child) or no collect channel is wired. Never blocks past the cap. An
    /// unwired provider short-circuits at once (no polling) — callers translate
    /// that `None` into the [`RESULTS_UNAVAILABLE`] error via `is_available`.
    fn await_subtask(&self, id: &str) -> Option<TaskStatus> {
        if !self.results.is_available() {
            return None;
        }
        let start = Instant::now();
        loop {
            if let Some(status) = self.results.status(id)
                && is_terminal(status)
            {
                return Some(status);
            }
            if start.elapsed() >= self.max_wait {
                return None;
            }
            std::thread::sleep(self.poll_interval);
        }
    }

    /// Block until EVERY id reaches a terminal status; return `(id, status)` for
    /// each once all are settled. `None` if the ceiling elapses before every id is
    /// terminal, or no collect channel is wired. This is the wave primitive.
    fn await_subtasks(&self, ids: &[String]) -> Option<Vec<(String, TaskStatus)>> {
        if !self.results.is_available() {
            return None;
        }
        let start = Instant::now();
        loop {
            let settled: Vec<(String, TaskStatus)> = ids
                .iter()
                .filter_map(|id| {
                    self.results
                        .status(id)
                        .filter(|s| is_terminal(*s))
                        .map(|s| (id.clone(), s))
                })
                .collect();
            if settled.len() == ids.len() {
                return Some(settled);
            }
            if start.elapsed() >= self.max_wait {
                return None;
            }
            std::thread::sleep(self.poll_interval);
        }
    }

    /// Harvest a finished subtask's result: its status plus the files-touched /
    /// blocked-commands parsed from its recap (mirroring `runner::run_task`) and
    /// the raw recap text. `None` if the child is unknown or has produced no recap
    /// yet. This never blocks — the master is expected to `await_subtask` first.
    fn subtask_result(&self, id: &str) -> Option<Value> {
        let recap = self.results.recap(id)?;
        let files_touched: Vec<String> = parse_files_touched(&recap)
            .into_iter()
            .map(|p| p.display().to_string())
            .collect();
        let blocked_commands = parse_blocked_commands(&recap);
        let status = self.results.status(id).map(TaskStatus::as_str);
        Some(json!({
            "subtask_id": id,
            "status": status,
            "files_touched": files_touched,
            "blocked_commands": blocked_commands,
            "recap": recap,
        }))
    }

    /// Host-side merge-back (WORKFLOW.md step 5): for each finished subtask id,
    /// harvest its isolated [`WorkerCheckout`] from the launcher registry, parse
    /// its `files_touched` HOST-side from the recap (structured fields only —
    /// never the recap text, G4), and run [`git::integrate_worker_branches`]
    /// against the resident's `integration_worktree`. Returns per-worker
    /// `{subtask_id, branch, committed, clean, conflicted_files,
    /// dependency_manifests}` so the resident routes conflicts to a resolver and
    /// surfaces the G5 flag. Never resolves, never pushes (G2/G3).
    ///
    /// A subtask absent from the registry ran WITHOUT isolation (non-git mother →
    /// shared-mount degrade); it is reported as `skipped` so the resident knows no
    /// branch was integrated for it. This never blocks — the master `await`s
    /// first.
    ///
    /// CLEANUP OWNERSHIP (task #598 open question, resolved here): this tool does
    /// NOT remove worktrees or delete `wip/<slug>` branches. Deleting a branch at
    /// integration time would destroy the very reviewable unit the isolation
    /// exists to produce — the resident routes conflicts to a resolver and the
    /// human reviews per-branch diffs AFTER merge-back. So the checkout and its
    /// branch outlive integration; teardown belongs to the run-path lifecycle
    /// (after cross-review / at root-run completion), reusing
    /// [`git::remove_worker_worktree`] with `delete_branch = true` only once the
    /// branch is no longer needed for review. The registry therefore keeps each
    /// entry for the whole root run rather than draining it here.
    fn integrate_subtasks(&self, ids: &[String]) -> anyhow::Result<Value> {
        let integration_worktree = self
            .integration_worktree
            .as_ref()
            .expect("integrate_subtasks called without an integration worktree wired");

        // Harvest structured inputs ONLY (G4): the launcher-recorded checkout and
        // the recap-parsed files_touched. The recap TEXT never crosses into the
        // merge. A subtask with no recorded checkout is skipped (shared-mount).
        let mut harvested: Vec<(String, WorkerHarvest)> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        for id in ids {
            let Some(checkout) = self.worker_registry.get(id) else {
                skipped.push(id.clone());
                continue;
            };
            let files_touched: Vec<PathBuf> = self
                .results
                .recap(id)
                .map(|recap| parse_files_touched(&recap))
                .unwrap_or_default();
            harvested.push((
                id.clone(),
                WorkerHarvest {
                    checkout,
                    files_touched,
                },
            ));
        }

        let workers: Vec<WorkerHarvest> = harvested.iter().map(|(_, w)| w.clone()).collect();
        let integrations =
            git::integrate_worker_branches(integration_worktree, &workers, "merge worker")?;

        let mut results = Vec::with_capacity(integrations.len() + skipped.len());
        for ((id, _), integration) in harvested.iter().zip(integrations.iter()) {
            let (clean, conflicted_files) = match &integration.merge {
                Some(outcome) => (
                    Some(outcome.clean),
                    outcome.conflicted_files.clone(),
                ),
                None => (None, Vec::new()),
            };
            let manifests: Vec<String> = integration
                .dependency_manifests
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            results.push(json!({
                "subtask_id": id,
                "branch": integration.branch,
                "committed": integration.committed,
                "clean": clean,
                "conflicted_files": conflicted_files,
                "dependency_manifests": manifests,
            }));
        }
        for id in skipped {
            results.push(json!({
                "subtask_id": id,
                "skipped": true,
                "reason": "no isolated worktree recorded (ran on the shared mount)",
            }));
        }
        Ok(json!({ "integrations": results }))
    }

    /// `list_tasks`: the caller's OWN project's tasks, optionally filtered by
    /// status. Empty (never an error) when the control plane is unwired or
    /// the project is empty — `list_tasks` is advisory, unlike the write
    /// path, so there is nothing to refuse here.
    fn list_tasks(&self, status: Option<TaskStatus>) -> Vec<TaskListEntry> {
        let Some(project) = &self.task_control_plane_project else {
            return Vec::new();
        };
        self.task_control_plane.list_tasks(project, status)
    }

    /// `get_task`: one task by id, scoped to the caller's OWN project. `None`
    /// both when the control plane is unwired AND when `id` belongs to a
    /// different project — the caller cannot distinguish "doesn't exist" from
    /// "not yours", which is the point (no project-existence oracle).
    fn get_task(&self, id: u64) -> Option<TaskDetail> {
        let project = self.task_control_plane_project.as_ref()?;
        self.task_control_plane.get_task(project, id)
    }

    /// `set_task_status`: the write side that closes the status-drift loop.
    /// Two structural gates run BEFORE the control-plane write, both hard
    /// errors (never a silent no-op):
    ///
    /// 1. Self-only — `parent_id` (the id of the task that owns this broker
    ///    channel; host-known, never attacker-supplied — see
    ///    [`SpawnBroker::gated_launch`] for the same lineage-guard pattern)
    ///    must equal `id`. A guest may close out only its OWN task, never a
    ///    sibling's or an arbitrary id — even one in the same project.
    /// 2. [`validate_self_status_transition`] — target must be a terminal
    ///    self-report status and the task must currently be `running`.
    fn set_task_status(&self, parent_id: &str, id: u64, status: TaskStatus) -> Result<(), String> {
        let project = self
            .task_control_plane_project
            .as_ref()
            .ok_or_else(|| RESULTS_UNAVAILABLE.to_owned())?;
        if parent_id.parse::<u64>().ok() != Some(id) {
            return Err(format!(
                "set_task_status denied: caller '{parent_id}' may only set the status of its own \
                 task, not '{id}'"
            ));
        }
        let current = self
            .task_control_plane
            .get_task(project, id)
            .ok_or_else(|| format!("set_task_status denied: task '{id}' not found in this project"))?;
        let current_status: TaskStatus = current.status.parse().map_err(|_| {
            format!(
                "set_task_status denied: task '{id}' has an unrecognized status '{}'",
                current.status
            )
        })?;
        validate_self_status_transition(current_status, status)?;
        self.task_control_plane.set_task_status(project, id, status)
    }

    /// The MCP `tools/list` manifest: exactly the narrow spawn tool plus the
    /// collect-side read-backs. Nothing else is advertised across the boundary.
    pub fn tool_manifest() -> Value {
        json!({
            "tools": [
                {
                    "name": SPAWN_SUBTASK_TOOL,
                    "description": "Request the host to run a sub-task in its own sibling sandbox. Host-mediated and policy-gated; returns the assigned subtask id or a denial reason.",
                    "inputSchema": {
                        "type": "object",
                        "required": ["brief"],
                        "properties": {
                            "brief": {"type": "string", "description": "What the sub-task should accomplish (becomes its task body)."},
                            "route": {"type": "string", "description": "Optional requested route glob (host re-validates)."},
                            "agent": {"type": "string", "description": "Optional requested agent (host re-validates)."},
                            "sandbox": {"type": "string", "description": "Optional requested sandbox name; must isolate (never `local`)."}
                        }
                    }
                },
                {
                    "name": RUN_SUBTASK_TOOL,
                    "description": "Request the host to run an EXISTING task (by id) in its own sibling sandbox, instead of creating a new one from a brief. Use this to run a task you already planned rather than re-describing it (which would duplicate it). Host-mediated and policy-gated identically to spawn_subtask; the task's own route/agent/sandbox are re-validated and its status is reset so any prior state is runnable. Returns the task id or a denial reason.",
                    "inputSchema": {
                        "type": "object",
                        "required": ["task_id"],
                        "properties": {
                            "task_id": {"type": "string", "description": "Id of an existing task to run, regardless of its current status."}
                        }
                    }
                },
                {"name": AWAIT_SUBTASK_TOOL, "description": "Block until a spawned sub-task reaches a terminal status; returns {subtask_id, status}.", "inputSchema": {"type": "object", "required": ["subtask_id"], "properties": {"subtask_id": {"type": "string"}}}},
                {"name": AWAIT_SUBTASKS_TOOL, "description": "Block until ALL listed sub-tasks reach a terminal status; returns [{subtask_id, status}]. The wave primitive.", "inputSchema": {"type": "object", "required": ["subtask_ids"], "properties": {"subtask_ids": {"type": "array", "items": {"type": "string"}}}}},
                {"name": SUBTASK_RESULT_TOOL, "description": "Fetch a finished sub-task's result: {status, files_touched, blocked_commands, recap}.", "inputSchema": {"type": "object", "required": ["subtask_id"], "properties": {"subtask_id": {"type": "string"}}}},
                {"name": SUBTASK_DIFF_TOOL, "description": "Fetch the unified DIFF of a finished sub-task's working-tree changes (its actual code output), computed on the host. Use this to CROSS-REVIEW a worker: a reviewer cannot `git diff` the reviewed worker itself (its changes are uncommitted in an out-of-tree worktree that is never mounted into the reviewer's box), so get the diff here and hand it to the reviewer in the review brief. Empty string means no changes.", "inputSchema": {"type": "object", "required": ["subtask_id"], "properties": {"subtask_id": {"type": "string"}}}},
                {"name": INTEGRATE_SUBTASKS_TOOL, "description": "Merge a wave of finished sub-tasks' isolated worktree branches onto the integration workspace. Commits each worker's files_touched onto its wip/ branch host-side and 3-way merges it, returning per-worker {branch, committed, clean, conflicted_files, dependency_manifests}. A non-clean merge lists the conflicted files for a resolver; dependency_manifests flags Cargo.toml/package.json/lockfile changes (G5). Never pushes (G2/G3). Call after await_subtasks.", "inputSchema": {"type": "object", "required": ["subtask_ids"], "properties": {"subtask_ids": {"type": "array", "items": {"type": "string"}}}}},
                {"name": LIST_TASKS_TOOL, "description": "List YOUR OWN project's tasks: [{id, slug, status, title, assignee}]. Host-mediated; never crosses into another project. Use this (or `.varda/tasks/*.md` in the workspace) to discover work — there is no GitHub egress and no `varda` CLI inside the box.", "inputSchema": {"type": "object", "properties": {"status": {"type": "string", "description": "Optional status filter: backlog, ready, running, review, needs_user, failed, or done."}}}},
                {"name": GET_TASK_TOOL, "description": "Read one of your project's tasks by id: {id, slug, status, title, assignee, body}. Returns not-found for any id outside your own project (cross-project ids are never distinguishable from unknown ones).", "inputSchema": {"type": "object", "required": ["id"], "properties": {"id": {"type": "integer", "description": "Numeric task id."}}}},
                {"name": SET_TASK_STATUS_TOOL, "description": "Close out YOUR OWN task once you are finished: set its status to done, needs_user, or failed. Only allowed for the caller's own task id, and only from 'running' — a task in 'review' is a human-only gate and cannot be self-marked done.", "inputSchema": {"type": "object", "required": ["id", "status"], "properties": {"id": {"type": "integer", "description": "Must be the caller's own task id."}, "status": {"type": "string", "enum": ["done", "needs_user", "failed"]}}}}
            ]
        })
    }

    /// Dispatch one MCP JSON-RPC request arriving on the broker channel owned by
    /// `parent_id`. Handles `initialize`, `tools/list`, and `tools/call`; a
    /// `spawn_subtask` call is gated through [`SpawnBroker::spawn_subtask`] and a
    /// denial is returned as an MCP tool error (`isError: true`) carrying the
    /// [`SpawnDenied`] reason — never a silent cap. Returns the JSON-RPC response.
    pub fn handle_rpc(&self, parent_id: &str, request: &Value) -> Value {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        match method {
            "initialize" => rpc_result(
                id,
                json!({
                    "protocolVersion": "2024-11-05",
                    "serverInfo": {"name": "varda-spawn-broker", "version": env!("CARGO_PKG_VERSION")},
                    "capabilities": {"tools": {}}
                }),
            ),
            "tools/list" => rpc_result(id, Self::tool_manifest()),
            "tools/call" => self.handle_tool_call(id, parent_id, request.get("params")),
            other => rpc_error(id, -32601, &format!("method not found: {other}")),
        }
    }

    fn handle_tool_call(&self, id: Value, parent_id: &str, params: Option<&Value>) -> Value {
        let Some(params) = params else {
            return rpc_error(id, -32602, "missing params");
        };
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let args = params.get("arguments").cloned().unwrap_or(json!({}));
        match name {
            SPAWN_SUBTASK_TOOL => {
                let Some(brief) = args.get("brief").and_then(Value::as_str) else {
                    return rpc_error(id, -32602, "spawn_subtask requires a `brief`");
                };
                let req = SpawnRequest {
                    brief: brief.to_owned(),
                    route: str_arg(&args, "route"),
                    agent: str_arg(&args, "agent"),
                    sandbox: str_arg(&args, "sandbox"),
                    // Approval is a host-side decision, never asserted by the box.
                    approved: false,
                };
                match self.spawn_subtask(parent_id, req) {
                    Ok(child_id) => {
                        rpc_result(id, tool_text(&format!("subtask_id: {child_id}"), false))
                    }
                    Err(e) => rpc_result(id, tool_text(&e.to_string(), true)),
                }
            }
            RUN_SUBTASK_TOOL => {
                let Some(task_id) = args.get("task_id").and_then(Value::as_str) else {
                    return rpc_error(id, -32602, "run_subtask requires a `task_id`");
                };
                match self.run_subtask(parent_id, task_id) {
                    Ok(child_id) => {
                        rpc_result(id, tool_text(&format!("subtask_id: {child_id}"), false))
                    }
                    Err(e) => rpc_result(id, tool_text(&e.to_string(), true)),
                }
            }
            AWAIT_SUBTASK_TOOL => {
                let Some(sid) = args.get("subtask_id").and_then(Value::as_str) else {
                    return rpc_error(id, -32602, "await_subtask requires a `subtask_id`");
                };
                if !self.results.is_available() {
                    return rpc_result(id, tool_text(RESULTS_UNAVAILABLE, true));
                }
                match self.await_subtask(sid) {
                    Some(status) => rpc_result(
                        id,
                        tool_text(
                            &json!({"subtask_id": sid, "status": status.as_str()}).to_string(),
                            false,
                        ),
                    ),
                    None => rpc_result(
                        id,
                        tool_text(
                            &format!(
                                "await_subtask timed out after {}s waiting for subtask '{sid}' to finish",
                                self.max_wait.as_secs()
                            ),
                            true,
                        ),
                    ),
                }
            }
            AWAIT_SUBTASKS_TOOL => {
                let ids: Vec<String> = args
                    .get("subtask_ids")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                if ids.is_empty() {
                    return rpc_error(
                        id,
                        -32602,
                        "await_subtasks requires a non-empty `subtask_ids` array",
                    );
                }
                if !self.results.is_available() {
                    return rpc_result(id, tool_text(RESULTS_UNAVAILABLE, true));
                }
                match self.await_subtasks(&ids) {
                    Some(settled) => {
                        let payload: Vec<Value> = settled
                            .into_iter()
                            .map(|(sid, status)| json!({"subtask_id": sid, "status": status.as_str()}))
                            .collect();
                        rpc_result(id, tool_text(&json!(payload).to_string(), false))
                    }
                    None => rpc_result(
                        id,
                        tool_text(
                            &format!(
                                "await_subtasks timed out after {}s; not all subtasks reached a terminal status",
                                self.max_wait.as_secs()
                            ),
                            true,
                        ),
                    ),
                }
            }
            SUBTASK_RESULT_TOOL => {
                let Some(sid) = args.get("subtask_id").and_then(Value::as_str) else {
                    return rpc_error(id, -32602, "subtask_result requires a `subtask_id`");
                };
                if !self.results.is_available() {
                    return rpc_result(id, tool_text(RESULTS_UNAVAILABLE, true));
                }
                match self.subtask_result(sid) {
                    Some(result) => rpc_result(id, tool_text(&result.to_string(), false)),
                    None => rpc_result(
                        id,
                        tool_text(
                            &format!(
                                "no result available for subtask '{sid}': unknown id or no recap yet"
                            ),
                            true,
                        ),
                    ),
                }
            }
            SUBTASK_DIFF_TOOL => {
                let Some(sid) = args.get("subtask_id").and_then(Value::as_str) else {
                    return rpc_error(id, -32602, "subtask_diff requires a `subtask_id`");
                };
                if !self.results.is_available() {
                    return rpc_result(id, tool_text(RESULTS_UNAVAILABLE, true));
                }
                match self.results.diff(sid) {
                    Some(diff) => rpc_result(id, tool_text(&diff, false)),
                    None => rpc_result(
                        id,
                        tool_text(
                            &format!(
                                "no diff available for subtask '{sid}': unknown id or its worktree \
                                 could not be read"
                            ),
                            true,
                        ),
                    ),
                }
            }
            INTEGRATE_SUBTASKS_TOOL => {
                let ids: Vec<String> = args
                    .get("subtask_ids")
                    .and_then(Value::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                if ids.is_empty() {
                    return rpc_error(
                        id,
                        -32602,
                        "integrate_subtasks requires a non-empty `subtask_ids` array",
                    );
                }
                if self.integration_worktree.is_none() {
                    return rpc_result(id, tool_text(RESULTS_UNAVAILABLE, true));
                }
                match self.integrate_subtasks(&ids) {
                    Ok(result) => rpc_result(id, tool_text(&result.to_string(), false)),
                    Err(error) => rpc_result(
                        id,
                        tool_text(
                            &format!("integrate_subtasks failed: {error:#}"),
                            true,
                        ),
                    ),
                }
            }
            LIST_TASKS_TOOL => {
                let status = match str_arg(&args, "status").map(|s| s.parse::<TaskStatus>()) {
                    None => None,
                    Some(Ok(status)) => Some(status),
                    Some(Err(error)) => return rpc_result(id, tool_text(&error.to_string(), true)),
                };
                let entries = self.list_tasks(status);
                rpc_result(id, tool_text(&json!(entries).to_string(), false))
            }
            GET_TASK_TOOL => {
                let Some(task_id) = args.get("id").and_then(Value::as_u64) else {
                    return rpc_error(id, -32602, "get_task requires an integer `id`");
                };
                match self.get_task(task_id) {
                    Some(detail) => rpc_result(id, tool_text(&json!(detail).to_string(), false)),
                    None => rpc_result(
                        id,
                        tool_text(&format!("no task '{task_id}' found in this project"), true),
                    ),
                }
            }
            SET_TASK_STATUS_TOOL => {
                let Some(task_id) = args.get("id").and_then(Value::as_u64) else {
                    return rpc_error(id, -32602, "set_task_status requires an integer `id`");
                };
                let Some(status_str) = args.get("status").and_then(Value::as_str) else {
                    return rpc_error(id, -32602, "set_task_status requires a `status`");
                };
                let status = match status_str.parse::<TaskStatus>() {
                    Ok(status) => status,
                    Err(error) => return rpc_result(id, tool_text(&error.to_string(), true)),
                };
                match self.set_task_status(parent_id, task_id, status) {
                    Ok(()) => rpc_result(
                        id,
                        tool_text(
                            &format!("task '{task_id}' set to {}", status.as_str()),
                            false,
                        ),
                    ),
                    Err(error) => rpc_result(id, tool_text(&error, true)),
                }
            }
            other => rpc_error(id, -32601, &format!("unknown tool: {other}")),
        }
    }
}

fn str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .filter(|s| !s.is_empty())
}

fn tool_text(text: &str, is_error: bool) -> Value {
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resident_policy() -> OrchestrationPolicy {
        OrchestrationPolicy {
            enabled: true,
            max_depth: 1,
            max_fanout: 16,
            global_child_budget: 64,
            deny_sandboxes: vec!["local".to_owned()],
            ..Default::default()
        }
    }

    fn base_policy() -> OrchestrationPolicy {
        OrchestrationPolicy {
            enabled: true,
            max_depth: 2,
            max_fanout: 2,
            global_child_budget: 3,
            ..Default::default()
        }
    }

    fn ctx(parent: &str, depth: u32) -> SpawnContext {
        SpawnContext {
            parent_id: parent.to_owned(),
            parent_depth: depth,
        }
    }

    fn req() -> SpawnRequest {
        SpawnRequest {
            brief: "do a thing".to_owned(),
            route: None,
            agent: None,
            sandbox: None,
            approved: false,
        }
    }

    #[test]
    fn disabled_policy_denies_everything() {
        let policy = OrchestrationPolicy::default(); // enabled = false
        let ledger = SpawnLedger::new();
        assert_eq!(
            ledger.authorize(&policy, &ctx("root", 0), &req()),
            Err(SpawnDenied::Disabled)
        );
    }

    #[test]
    fn default_policy_is_locked_down() {
        // The safe default must be: spawning off, local sandbox denied.
        let p = OrchestrationPolicy::default();
        assert!(!p.enabled);
        assert!(p.deny_sandboxes.contains(&"local".to_owned()));
    }

    #[test]
    fn allows_a_simple_spawn_when_enabled() {
        let policy = base_policy();
        let mut ledger = SpawnLedger::new();
        let grant = ledger
            .authorize_and_record(&policy, &ctx("root", 0), &req())
            .expect("first spawn should be allowed");
        assert_eq!(grant.child_depth, 1);
        assert_eq!(ledger.global_spawned(), 1);
        assert_eq!(ledger.children_of("root"), 1);
    }

    #[test]
    fn depth_cap_is_a_hard_error() {
        let policy = base_policy(); // max_depth = 2
        let ledger = SpawnLedger::new();
        // parent already at depth 2 ⇒ child would be depth 3 ⇒ rejected.
        assert_eq!(
            ledger.authorize(&policy, &ctx("deep", 2), &req()),
            Err(SpawnDenied::DepthExceeded {
                attempted: 3,
                max: 2
            })
        );
        // parent at depth 1 ⇒ child depth 2 == max ⇒ allowed.
        assert!(ledger.authorize(&policy, &ctx("mid", 1), &req()).is_ok());
    }

    #[test]
    fn fanout_cap_rejects_the_over_limit_child_not_silently() {
        let policy = base_policy(); // max_fanout = 2
        let mut ledger = SpawnLedger::new();
        ledger
            .authorize_and_record(&policy, &ctx("root", 0), &req())
            .unwrap();
        ledger
            .authorize_and_record(&policy, &ctx("root", 0), &req())
            .unwrap();
        // third child of the same parent ⇒ hard error, not a silent cap.
        assert_eq!(
            ledger.authorize(&policy, &ctx("root", 0), &req()),
            Err(SpawnDenied::FanoutExceeded {
                parent: "root".to_owned(),
                max: 2
            })
        );
        // A DIFFERENT parent still has its own fan-out budget.
        assert!(ledger.authorize(&policy, &ctx("other", 0), &req()).is_ok());
    }

    #[test]
    fn global_budget_cap_is_a_hard_error() {
        let policy = base_policy(); // budget = 3, fanout = 2
        let mut ledger = SpawnLedger::new();
        // Spread across parents so fan-out never trips first.
        ledger
            .authorize_and_record(&policy, &ctx("a", 0), &req())
            .unwrap();
        ledger
            .authorize_and_record(&policy, &ctx("b", 0), &req())
            .unwrap();
        ledger
            .authorize_and_record(&policy, &ctx("c", 0), &req())
            .unwrap();
        assert_eq!(
            ledger.authorize(&policy, &ctx("d", 0), &req()),
            Err(SpawnDenied::BudgetExceeded {
                spent: 3,
                budget: 3
            })
        );
    }

    #[test]
    fn agent_allow_and_deny_lists() {
        let mut policy = base_policy();
        policy.allow_agents = vec!["claude".to_owned(), "codex".to_owned()];
        policy.deny_agents = vec!["codex".to_owned()]; // deny beats allow
        let ledger = SpawnLedger::new();

        let mut r = req();
        r.agent = Some("claude".to_owned());
        assert!(ledger.authorize(&policy, &ctx("root", 0), &r).is_ok());

        r.agent = Some("copilot".to_owned()); // not in allow-list
        assert_eq!(
            ledger.authorize(&policy, &ctx("root", 0), &r),
            Err(SpawnDenied::AgentNotAllowed {
                agent: "copilot".to_owned()
            })
        );

        r.agent = Some("codex".to_owned()); // allowed but also denied ⇒ denied
        assert!(matches!(
            ledger.authorize(&policy, &ctx("root", 0), &r),
            Err(SpawnDenied::AgentNotAllowed { .. })
        ));
    }

    #[test]
    fn route_glob_allow_and_deny() {
        let mut policy = base_policy();
        policy.allow_routes = vec!["/work/**".to_owned()];
        policy.deny_routes = vec!["/work/secret/**".to_owned()];
        let ledger = SpawnLedger::new();

        let mut r = req();
        r.route = Some("/work/proj/a".to_owned());
        assert!(ledger.authorize(&policy, &ctx("root", 0), &r).is_ok());

        r.route = Some("/work/secret/x".to_owned()); // denied glob wins
        assert_eq!(
            ledger.authorize(&policy, &ctx("root", 0), &r),
            Err(SpawnDenied::RouteNotAllowed {
                route: "/work/secret/x".to_owned()
            })
        );

        r.route = Some("/elsewhere".to_owned()); // outside allow-list
        assert!(matches!(
            ledger.authorize(&policy, &ctx("root", 0), &r),
            Err(SpawnDenied::RouteNotAllowed { .. })
        ));
    }

    #[test]
    fn sandbox_local_is_denied_by_default_to_prevent_escape() {
        let policy = base_policy(); // deny_sandboxes defaults to ["local"]
        let ledger = SpawnLedger::new();
        let mut r = req();
        r.sandbox = Some("local".to_owned());
        assert_eq!(
            ledger.authorize(&policy, &ctx("root", 0), &r),
            Err(SpawnDenied::SandboxNotAllowed {
                sandbox: "local".to_owned()
            })
        );
        r.sandbox = Some("docker".to_owned());
        assert!(ledger.authorize(&policy, &ctx("root", 0), &r).is_ok());
    }

    #[test]
    fn check_effective_placement_catches_a_fallback_sandbox_denied_by_policy() {
        // `authorize` only re-checks `req.sandbox` when the CALLER supplied one
        // explicitly. `default_worker_sandbox`/`fallback_agent` resolve to a
        // value further down the launch path that `authorize` never sees — this
        // is the seam `check_effective_placement` re-checks post-fallback.
        let policy = base_policy(); // deny_sandboxes defaults to ["local"]
        assert_eq!(
            check_effective_placement(&policy, "claude", Some("local")),
            Err(SpawnDenied::SandboxNotAllowed {
                sandbox: "local".to_owned()
            })
        );
        assert!(check_effective_placement(&policy, "claude", Some("docker")).is_ok());
        assert!(check_effective_placement(&policy, "claude", None).is_ok());
    }

    #[test]
    fn check_effective_placement_catches_a_denied_fallback_agent() {
        let mut policy = base_policy();
        policy.deny_agents = vec!["local-agent".to_owned()];
        assert_eq!(
            check_effective_placement(&policy, "local-agent", Some("docker")),
            Err(SpawnDenied::AgentNotAllowed {
                agent: "local-agent".to_owned()
            })
        );
        assert!(check_effective_placement(&policy, "claude", Some("docker")).is_ok());
    }

    #[test]
    fn first_level_can_require_human_approval() {
        let mut policy = base_policy();
        policy.require_approval_at_depth = Some(1);
        let ledger = SpawnLedger::new();

        let mut r = req();
        r.approved = false;
        assert_eq!(
            ledger.authorize(&policy, &ctx("root", 0), &r),
            Err(SpawnDenied::ApprovalRequired { depth: 1 })
        );

        r.approved = true;
        assert!(ledger.authorize(&policy, &ctx("root", 0), &r).is_ok());

        // Deeper spawns (depth 2) are not gated by a depth-1 approval requirement.
        let mut deep = req();
        deep.approved = false;
        assert!(ledger.authorize(&policy, &ctx("mid", 1), &deep).is_ok());
    }

    #[test]
    fn record_only_after_authorize_keeps_ledger_truthful() {
        // A denied spawn must not consume budget.
        let policy = base_policy();
        let ledger = SpawnLedger::new();
        let deny_ctx = ctx("deep", 5); // depth-exceeded
        assert!(ledger.authorize(&policy, &deny_ctx, &req()).is_err());
        assert_eq!(ledger.global_spawned(), 0);
        assert_eq!(ledger.children_of("deep"), 0);
    }

    #[test]
    fn resident_policy_stops_workers_from_spawning_with_max_depth_one() {
        let policy = resident_policy();
        let ledger = SpawnLedger::new();

        assert!(
            ledger
                .authorize(&policy, &ctx("resident", 0), &req())
                .is_ok()
        );
        assert_eq!(
            ledger.authorize(&policy, &ctx("worker", 1), &req()),
            Err(SpawnDenied::DepthExceeded {
                attempted: 2,
                max: 1
            })
        );
    }

    #[test]
    fn resident_policy_denies_spawned_children_from_landing_local() {
        let policy = resident_policy();
        let ledger = SpawnLedger::new();
        let mut local = req();
        local.sandbox = Some("local".to_owned());

        assert_eq!(
            ledger.authorize(&policy, &ctx("resident", 0), &local),
            Err(SpawnDenied::SandboxNotAllowed {
                sandbox: "local".to_owned()
            })
        );

        let mut isolated = req();
        isolated.sandbox = Some("docker".to_owned());
        assert!(
            ledger
                .authorize(&policy, &ctx("resident", 0), &isolated)
                .is_ok()
        );
    }

    #[test]
    fn resident_max_fanout_sixteen_fits_full_wave_then_denies_seventeenth() {
        let policy = resident_policy();
        let mut ledger = SpawnLedger::new();

        for _ in 0..12 {
            ledger
                .authorize_and_record(&policy, &ctx("resident", 0), &req())
                .unwrap();
        }
        assert_eq!(ledger.children_of("resident"), 12);

        for _ in 0..4 {
            ledger
                .authorize_and_record(&policy, &ctx("resident", 0), &req())
                .unwrap();
        }
        assert_eq!(ledger.children_of("resident"), 16);
        assert_eq!(
            ledger.authorize(&policy, &ctx("resident", 0), &req()),
            Err(SpawnDenied::FanoutExceeded {
                parent: "resident".to_owned(),
                max: 16
            })
        );
    }

    #[test]
    fn resident_global_child_budget_is_enforced_across_parents() {
        let mut policy = resident_policy();
        policy.max_fanout = 64;
        policy.global_child_budget = 3;
        let mut ledger = SpawnLedger::new();

        ledger
            .authorize_and_record(&policy, &ctx("resident-a", 0), &req())
            .unwrap();
        ledger
            .authorize_and_record(&policy, &ctx("resident-b", 0), &req())
            .unwrap();
        ledger
            .authorize_and_record(&policy, &ctx("resident-c", 0), &req())
            .unwrap();

        assert_eq!(
            ledger.authorize(&policy, &ctx("resident-d", 0), &req()),
            Err(SpawnDenied::BudgetExceeded {
                spent: 3,
                budget: 3
            })
        );
    }

    #[test]
    fn documented_resident_route_orchestration_override_round_trips() {
        let toml_src = r#"
[defaults]
timeout_seconds = 600
operations_dir = "operations"

[[routes]]
glob = "/Users/nilleb/dev/nillebco/varda/**"
agents = ["trusted-resident"]
sandbox = "local"

[routes.orchestration]
enabled = true
max_depth = 1
max_fanout = 16
global_child_budget = 64
deny_sandboxes = ["local"]
"#;
        let config: crate::config::Config = toml::from_str(toml_src).unwrap();
        let route_policy = config.routes[0].orchestration.as_ref().unwrap();

        assert!(route_policy.enabled);
        assert_eq!(route_policy.max_depth, 1);
        assert_eq!(route_policy.max_fanout, 16);
        assert_eq!(route_policy.global_child_budget, 64);
        assert_eq!(route_policy.deny_sandboxes, vec!["local".to_owned()]);

        let serialized = toml::to_string(&config).unwrap();
        let reparsed: crate::config::Config = toml::from_str(&serialized).unwrap();
        assert_eq!(
            reparsed.routes[0].orchestration.as_ref(),
            Some(route_policy)
        );
    }

    // --- Live broker -------------------------------------------------------

    /// Deterministic launcher for tests: hands back sequential child ids and
    /// records every spawn it was asked to launch, so tests can assert the host
    /// only ever launches AUTHORIZED spawns (never one the policy denied).
    struct MockLauncher {
        prefix: String,
        next: u32,
        launched: Vec<(SpawnRequest, u32)>,
        /// Existing-task runs the broker asked for: (task_id, granted depth).
        ran: Vec<(String, u32)>,
        fail: bool,
    }
    impl MockLauncher {
        fn new() -> Self {
            Self::with_prefix("sub")
        }
        fn with_prefix(prefix: &str) -> Self {
            Self {
                prefix: prefix.to_owned(),
                next: 0,
                launched: Vec::new(),
                ran: Vec::new(),
                fail: false,
            }
        }
        fn failing() -> Self {
            Self {
                prefix: "sub".to_owned(),
                next: 0,
                launched: Vec::new(),
                ran: Vec::new(),
                fail: true,
            }
        }
    }
    impl SubtaskLauncher for MockLauncher {
        fn launch(&mut self, req: &SpawnRequest, grant: &SpawnGrant) -> anyhow::Result<SubtaskId> {
            if self.fail {
                anyhow::bail!("simulated host launch failure");
            }
            self.launched.push((req.clone(), grant.child_depth));
            self.next += 1;
            Ok(format!("{}-{}", self.prefix, self.next))
        }

        fn run_existing(
            &mut self,
            task_id: &str,
            grant: &SpawnGrant,
        ) -> anyhow::Result<SubtaskId> {
            if self.fail {
                anyhow::bail!("simulated host launch failure");
            }
            // The run path returns the EXISTING task's own id (no new doc minted).
            self.ran.push((task_id.to_owned(), grant.child_depth));
            Ok(task_id.to_owned())
        }
    }

    #[test]
    fn broker_launches_authorized_spawn_and_returns_child_id() {
        let broker = SpawnBroker::new(base_policy(), "root", MockLauncher::new());
        let id = broker.spawn_subtask("root", req()).expect("should launch");
        assert_eq!(id, "sub-1");
        assert_eq!(broker.depth_of("sub-1"), Some(1));
        assert_eq!(broker.global_spawned(), 1);
    }

    #[test]
    fn broker_denial_never_reaches_the_launcher() {
        // Disabled policy: the launcher must not be called at all.
        let broker = SpawnBroker::new(OrchestrationPolicy::default(), "root", MockLauncher::new());
        match broker.spawn_subtask("root", req()) {
            Err(BrokerError::Denied(SpawnDenied::Disabled)) => {}
            other => panic!("expected Disabled denial, got {other:?}"),
        }
        assert_eq!(broker.global_spawned(), 0);
    }

    #[test]
    fn broker_tracks_lineage_depth_across_the_real_tree() {
        // max_depth = 2: root(0) → child(1) → grandchild(2) ok; great-grandchild(3) denied.
        let broker = SpawnBroker::new(base_policy(), "root", MockLauncher::new());
        let child = broker.spawn_subtask("root", req()).unwrap();
        assert_eq!(broker.depth_of(&child), Some(1));
        let grand = broker.spawn_subtask(&child, req()).unwrap();
        assert_eq!(broker.depth_of(&grand), Some(2));
        // Now a spawn from the depth-2 grandchild would land at depth 3 > max_depth.
        match broker.spawn_subtask(&grand, req()) {
            Err(BrokerError::Denied(SpawnDenied::DepthExceeded {
                attempted: 3,
                max: 2,
            })) => {}
            other => panic!("expected DepthExceeded, got {other:?}"),
        }
    }

    #[test]
    fn shared_state_enforces_depth_across_generation_brokers() {
        let state = SharedSpawnState::new();
        let parent = SpawnBroker::with_shared_state(
            base_policy(),
            "root",
            0,
            state.clone(),
            MockLauncher::with_prefix("child"),
        );
        let child_id = parent.spawn_subtask("root", req()).unwrap();

        let child = SpawnBroker::with_shared_state(
            base_policy(),
            child_id.clone(),
            1,
            state.clone(),
            MockLauncher::with_prefix("grand"),
        );
        let grand_id = child.spawn_subtask(&child_id, req()).unwrap();

        let grand = SpawnBroker::with_shared_state(
            base_policy(),
            grand_id.clone(),
            2,
            state,
            MockLauncher::with_prefix("great"),
        );
        match grand.spawn_subtask(&grand_id, req()) {
            Err(BrokerError::Denied(SpawnDenied::DepthExceeded {
                attempted: 3,
                max: 2,
            })) => {}
            other => panic!("expected DepthExceeded across brokers, got {other:?}"),
        }
    }

    #[test]
    fn shared_state_enforces_global_budget_across_generation_brokers() {
        let mut policy = base_policy();
        policy.max_depth = 10;
        policy.max_fanout = 10;
        policy.global_child_budget = 2;
        let state = SharedSpawnState::new();
        let parent = SpawnBroker::with_shared_state(
            policy.clone(),
            "root",
            0,
            state.clone(),
            MockLauncher::with_prefix("child"),
        );
        let child_id = parent.spawn_subtask("root", req()).unwrap();

        let child = SpawnBroker::with_shared_state(
            policy.clone(),
            child_id.clone(),
            1,
            state.clone(),
            MockLauncher::with_prefix("grand"),
        );
        let grand_id = child.spawn_subtask(&child_id, req()).unwrap();

        let grand = SpawnBroker::with_shared_state(
            policy,
            grand_id.clone(),
            2,
            state,
            MockLauncher::with_prefix("great"),
        );
        match grand.spawn_subtask(&grand_id, req()) {
            Err(BrokerError::Denied(SpawnDenied::BudgetExceeded {
                spent: 2,
                budget: 2,
            })) => {}
            other => panic!("expected BudgetExceeded across brokers, got {other:?}"),
        }
    }

    #[test]
    fn broker_rejects_unknown_parent_lineage_spoof() {
        // A caller the broker never spawned cannot spawn (can't fake a shallow depth).
        let broker = SpawnBroker::new(base_policy(), "root", MockLauncher::new());
        match broker.spawn_subtask("ghost", req()) {
            Err(BrokerError::UnknownParent(id)) => assert_eq!(id, "ghost"),
            other => panic!("expected UnknownParent, got {other:?}"),
        }
    }

    #[test]
    fn broker_rolls_back_budget_when_host_launch_fails() {
        let broker = SpawnBroker::new(base_policy(), "root", MockLauncher::failing());
        match broker.spawn_subtask("root", req()) {
            Err(BrokerError::Launch(_)) => {}
            other => panic!("expected Launch error, got {other:?}"),
        }
        // A failed launch must not consume budget/fan-out.
        assert_eq!(broker.global_spawned(), 0);
        assert_eq!(broker.depth_of("sub-1"), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shared_state_tracks_and_drains_detached_handles() {
        let state = SharedSpawnState::new();
        state.insert_handle(
            "child".to_owned(),
            tokio::spawn(async move {
                tokio::task::yield_now().await;
            }),
        );

        assert_eq!(state.handle_count(), 1);
        let handles = state.drain_handles();
        assert_eq!(state.handle_count(), 0);
        assert_eq!(handles.len(), 1);

        handles.into_iter().next().unwrap().1.await.unwrap();
    }

    #[test]
    fn rpc_tools_list_exposes_only_the_narrow_spawn_surface() {
        let broker = SpawnBroker::new(base_policy(), "root", MockLauncher::new());
        let resp = broker.handle_rpc(
            "root",
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        );
        let names: Vec<String> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                SPAWN_SUBTASK_TOOL,
                RUN_SUBTASK_TOOL,
                AWAIT_SUBTASK_TOOL,
                AWAIT_SUBTASKS_TOOL,
                SUBTASK_RESULT_TOOL,
                SUBTASK_DIFF_TOOL,
                INTEGRATE_SUBTASKS_TOOL,
                LIST_TASKS_TOOL,
                GET_TASK_TOOL,
                SET_TASK_STATUS_TOOL
            ]
        );
    }

    #[test]
    fn spawn_sandbox_override_prefers_request_then_default() {
        let mut policy = OrchestrationPolicy::default();
        // No request and no configured default → None (fall through to the route).
        assert_eq!(spawn_sandbox_override(None, &policy), None);
        // The configured default applies when the spawn requests no sandbox — this
        // is what makes a worker land in the `worker` box without the resident
        // having to remember `sandbox=` on every call (#636).
        policy.default_worker_sandbox = Some("worker".to_owned());
        assert_eq!(
            spawn_sandbox_override(None, &policy).as_deref(),
            Some("worker")
        );
        // An explicit request wins over the default.
        assert_eq!(
            spawn_sandbox_override(Some("adc"), &policy).as_deref(),
            Some("adc")
        );
    }

    #[test]
    fn rpc_spawn_call_returns_subtask_id_on_success() {
        let broker = SpawnBroker::new(base_policy(), "root", MockLauncher::new());
        let resp = broker.handle_rpc(
            "root",
            &json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call",
                    "params": {"name": SPAWN_SUBTASK_TOOL, "arguments": {"brief": "do it"}}}),
        );
        assert_eq!(resp["result"]["isError"], json!(false));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("subtask_id: sub-1"), "got {text}");
    }

    #[test]
    fn rpc_denied_spawn_surfaces_reason_as_tool_error_not_silent_cap() {
        // budget = 3 across distinct parents, then the 4th trips BudgetExceeded and
        // the RPC layer must surface it as isError:true with the reason text.
        let mut policy = base_policy();
        policy.max_fanout = 10; // don't let fan-out trip first
        let broker = SpawnBroker::new(policy, "root", MockLauncher::new());
        for _ in 0..3 {
            let r = broker.handle_rpc(
                "root",
                &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                        "params": {"name": SPAWN_SUBTASK_TOOL, "arguments": {"brief": "x"}}}),
            );
            assert_eq!(r["result"]["isError"], json!(false));
        }
        let denied = broker.handle_rpc(
            "root",
            &json!({"jsonrpc": "2.0", "id": 9, "method": "tools/call",
                    "params": {"name": SPAWN_SUBTASK_TOOL, "arguments": {"brief": "x"}}}),
        );
        assert_eq!(denied["result"]["isError"], json!(true));
        let text = denied["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("global child budget exhausted"), "got {text}");
    }

    #[test]
    fn rpc_local_sandbox_request_is_refused() {
        // A master asking to place its subtask in the escape-hatch `local` box is
        // refused (deny_sandboxes defaults to ["local"]).
        let broker = SpawnBroker::new(base_policy(), "root", MockLauncher::new());
        let resp = broker.handle_rpc(
            "root",
            &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                    "params": {"name": SPAWN_SUBTASK_TOOL, "arguments": {"brief": "x", "sandbox": "local"}}}),
        );
        assert_eq!(resp["result"]["isError"], json!(true));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(
            text.contains("must run in an isolating sibling box"),
            "got {text}"
        );
    }

    // --- run_subtask (run an existing task, not a fresh brief) -------------

    #[test]
    fn run_subtask_returns_the_existing_task_id_and_records_lineage() {
        // The run path reuses the EXISTING id (no new doc), and the broker registers
        // that id at the granted depth exactly like a spawn.
        let broker = SpawnBroker::new(base_policy(), "root", MockLauncher::new());
        let id = broker
            .run_subtask("root", "task-42")
            .expect("should run the existing task");
        assert_eq!(id, "task-42");
        assert_eq!(broker.depth_of("task-42"), Some(1));
        assert_eq!(broker.global_spawned(), 1);
    }

    #[test]
    fn run_subtask_shares_the_same_budget_gate_as_spawn() {
        // budget = 3: two spawns + one run exhaust it, the 4th (a run) is denied —
        // proof the two paths draw from ONE ledger, not separate gates.
        let mut policy = base_policy();
        policy.max_fanout = 10; // don't let fan-out trip first
        let broker = SpawnBroker::new(policy, "root", MockLauncher::new());
        broker.spawn_subtask("root", req()).unwrap();
        broker.spawn_subtask("root", req()).unwrap();
        broker.run_subtask("root", "task-7").unwrap();
        match broker.run_subtask("root", "task-8") {
            Err(BrokerError::Denied(SpawnDenied::BudgetExceeded { spent: 3, budget: 3 })) => {}
            other => panic!("expected BudgetExceeded, got {other:?}"),
        }
        assert_eq!(broker.global_spawned(), 3);
    }

    #[test]
    fn run_subtask_from_unknown_parent_is_refused() {
        // Same lineage guard as spawn: a caller not registered through the broker
        // cannot run anything (its depth is unknown, so the recursion cap can't hold).
        let broker = SpawnBroker::new(base_policy(), "root", MockLauncher::new());
        match broker.run_subtask("ghost", "task-1") {
            Err(BrokerError::UnknownParent(id)) => assert_eq!(id, "ghost"),
            other => panic!("expected UnknownParent, got {other:?}"),
        }
        assert_eq!(broker.global_spawned(), 0);
    }

    #[test]
    fn run_subtask_launch_failure_rolls_back_the_budget() {
        // A host-side run failure must not permanently consume budget (unrecord),
        // exactly like a failed spawn.
        let broker = SpawnBroker::new(base_policy(), "root", MockLauncher::failing());
        match broker.run_subtask("root", "task-1") {
            Err(BrokerError::Launch(_)) => {}
            other => panic!("expected Launch error, got {other:?}"),
        }
        assert_eq!(broker.global_spawned(), 0);
    }

    #[test]
    fn rpc_run_call_returns_the_existing_task_id_on_success() {
        let broker = SpawnBroker::new(base_policy(), "root", MockLauncher::new());
        let resp = broker.handle_rpc(
            "root",
            &json!({"jsonrpc": "2.0", "id": 5, "method": "tools/call",
                    "params": {"name": RUN_SUBTASK_TOOL, "arguments": {"task_id": "task-99"}}}),
        );
        assert_eq!(resp["result"]["isError"], json!(false));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("subtask_id: task-99"), "got {text}");
    }

    #[test]
    fn rpc_run_call_without_task_id_is_a_param_error() {
        let broker = SpawnBroker::new(base_policy(), "root", MockLauncher::new());
        let resp = broker.handle_rpc(
            "root",
            &json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call",
                    "params": {"name": RUN_SUBTASK_TOOL, "arguments": {}}}),
        );
        assert_eq!(resp["error"]["code"], json!(-32602));
    }

    // --- Collect channel (461a) --------------------------------------------

    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::task::TaskStatus;

    /// A `(id, nth-call) -> status` rule the [`MockResults`] mock consults.
    type StatusFn = Box<dyn Fn(&str, u32) -> Option<TaskStatus> + Send + Sync>;

    /// Controllable [`SubtaskResults`] mock: `status_fn` receives (id, nth-call)
    /// so a test can flip a child to terminal after N polls; recaps are canned.
    struct MockResults {
        counts: Mutex<BTreeMap<String, u32>>,
        status_fn: StatusFn,
        recaps: BTreeMap<String, String>,
    }

    impl MockResults {
        fn new(
            status_fn: impl Fn(&str, u32) -> Option<TaskStatus> + Send + Sync + 'static,
        ) -> Self {
            Self {
                counts: Mutex::new(BTreeMap::new()),
                status_fn: Box::new(status_fn),
                recaps: BTreeMap::new(),
            }
        }
        fn with_recap(mut self, id: &str, recap: &str) -> Self {
            self.recaps.insert(id.to_owned(), recap.to_owned());
            self
        }
    }

    impl SubtaskResults for MockResults {
        fn status(&self, id: &str) -> Option<TaskStatus> {
            let n = {
                let mut c = self.counts.lock().unwrap();
                let e = c.entry(id.to_owned()).or_insert(0);
                *e += 1;
                *e
            };
            (self.status_fn)(id, n)
        }
        fn recap(&self, id: &str) -> Option<String> {
            self.recaps.get(id).cloned()
        }
    }

    fn fast_timing<L: SubtaskLauncher>(broker: SpawnBroker<L>) -> SpawnBroker<L> {
        broker.with_poll_timing(Duration::from_millis(1), Duration::from_secs(5))
    }

    #[test]
    fn await_subtask_blocks_until_terminal_not_before() {
        // Child reports Running for its first two polls, then Done on the third.
        let polls = Arc::new(AtomicU32::new(0));
        let p = polls.clone();
        let results = MockResults::new(move |_, _| {
            let n = p.fetch_add(1, Ordering::SeqCst) + 1;
            Some(if n >= 3 {
                TaskStatus::Done
            } else {
                TaskStatus::Running
            })
        });
        let broker = fast_timing(
            SpawnBroker::new(base_policy(), "root", MockLauncher::new()).with_results(results),
        );
        assert_eq!(broker.await_subtask("child"), Some(TaskStatus::Done));
        // It could only return on a terminal poll, so it must have polled ≥ 3×.
        assert!(
            polls.load(Ordering::SeqCst) >= 3,
            "await returned before the child was terminal"
        );
    }

    #[test]
    fn await_subtask_times_out_on_wedged_child() {
        // Never terminal ⇒ the bound must fire and return a timeout (not hang).
        let results = MockResults::new(|_, _| Some(TaskStatus::Running));
        let broker = SpawnBroker::new(base_policy(), "root", MockLauncher::new())
            .with_results(results)
            .with_poll_timing(Duration::from_millis(1), Duration::from_millis(30));
        assert_eq!(broker.await_subtask("child"), None);
    }

    #[test]
    fn rpc_await_subtask_timeout_is_a_tool_error() {
        let results = MockResults::new(|_, _| Some(TaskStatus::Running));
        let broker = SpawnBroker::new(base_policy(), "root", MockLauncher::new())
            .with_results(results)
            .with_poll_timing(Duration::from_millis(1), Duration::from_millis(20));
        let resp = broker.handle_rpc(
            "root",
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": AWAIT_SUBTASK_TOOL, "arguments": {"subtask_id": "child"}}}),
        );
        assert_eq!(resp["result"]["isError"], json!(true));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("timed out"), "got {text}");
    }

    #[test]
    fn rpc_await_subtask_returns_status_when_terminal() {
        let results = MockResults::new(|_, _| Some(TaskStatus::Done));
        let broker = fast_timing(
            SpawnBroker::new(base_policy(), "root", MockLauncher::new()).with_results(results),
        );
        let resp = broker.handle_rpc(
            "root",
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": AWAIT_SUBTASK_TOOL, "arguments": {"subtask_id": "child"}}}),
        );
        assert_eq!(resp["result"]["isError"], json!(false));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert_eq!(parsed, json!({"subtask_id": "child", "status": "done"}));
    }

    #[test]
    fn subtask_result_returns_recap_and_parsed_sections() {
        let recap = "# Done\n\n## Files touched\n/abs/a.rs\n/abs/b.rs\n\n## Blocked commands\nmsb\ndocker build\n\nrequires_user: false\n";
        let results = MockResults::new(|_, _| Some(TaskStatus::Done)).with_recap("child", recap);
        let broker =
            SpawnBroker::new(base_policy(), "root", MockLauncher::new()).with_results(results);
        let result = broker.subtask_result("child").expect("result present");
        assert_eq!(result["status"], json!("done"));
        assert_eq!(result["files_touched"], json!(["/abs/a.rs", "/abs/b.rs"]));
        assert_eq!(result["blocked_commands"], json!(["msb", "docker build"]));
        assert_eq!(result["recap"], json!(recap));
        // Unknown / no-recap child ⇒ no result.
        assert!(broker.subtask_result("ghost").is_none());
    }

    #[test]
    fn await_subtasks_returns_only_once_all_terminal() {
        // `a` is terminal immediately; `b` only on its 4th poll. The call must not
        // return until BOTH are settled.
        let bpolls = Arc::new(AtomicU32::new(0));
        let bc = bpolls.clone();
        let results = MockResults::new(move |id, _| match id {
            "a" => Some(TaskStatus::Done),
            "b" => {
                let n = bc.fetch_add(1, Ordering::SeqCst) + 1;
                Some(if n >= 4 {
                    TaskStatus::Failed
                } else {
                    TaskStatus::Running
                })
            }
            _ => None,
        });
        let broker = fast_timing(
            SpawnBroker::new(base_policy(), "root", MockLauncher::new()).with_results(results),
        );
        let settled = broker
            .await_subtasks(&["a".to_owned(), "b".to_owned()])
            .expect("both settle before the cap");
        assert!(
            bpolls.load(Ordering::SeqCst) >= 4,
            "await_subtasks returned before `b` was terminal"
        );
        let map: BTreeMap<_, _> = settled.into_iter().collect();
        assert_eq!(map["a"], TaskStatus::Done);
        assert_eq!(map["b"], TaskStatus::Failed);
    }

    #[test]
    fn unwired_broker_await_short_circuits_instantly() {
        // Default results seam (NoSubtaskResults): no collect channel wired.
        // A generous max_wait would hang 30 min IF the loop polled to the ceiling;
        // the short-circuit must return at once instead. Cap set to 1h so any
        // polling would blow the timing assertion by orders of magnitude.
        let broker = SpawnBroker::new(base_policy(), "root", MockLauncher::new())
            .with_poll_timing(Duration::from_secs(1), Duration::from_secs(3600));

        let t = Instant::now();
        assert_eq!(broker.await_subtask("child"), None);
        assert_eq!(
            broker.await_subtasks(&["a".to_owned(), "b".to_owned()]),
            None
        );
        assert!(
            t.elapsed() < Duration::from_secs(1),
            "unwired await polled instead of short-circuiting ({:?})",
            t.elapsed()
        );

        // Over the RPC surface it is the "not available on this channel" tool
        // error (the old stub semantics), NOT a "timed out" error.
        for args in [
            json!({"name": AWAIT_SUBTASK_TOOL, "arguments": {"subtask_id": "child"}}),
            json!({"name": AWAIT_SUBTASKS_TOOL, "arguments": {"subtask_ids": ["child"]}}),
            json!({"name": SUBTASK_RESULT_TOOL, "arguments": {"subtask_id": "child"}}),
        ] {
            let resp = broker.handle_rpc(
                "root",
                &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": args}),
            );
            assert_eq!(resp["result"]["isError"], json!(true));
            let text = resp["result"]["content"][0]["text"].as_str().unwrap();
            assert!(
                text.contains("not available on this channel"),
                "expected not-available error, got {text}"
            );
            assert!(
                !text.contains("timed out"),
                "unwired hit the poll ceiling: {text}"
            );
        }
    }

    #[test]
    fn manifest_advertises_the_collect_tools() {
        let manifest = SpawnBroker::<MockLauncher>::tool_manifest();
        let names: Vec<&str> = manifest["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert!(names.contains(&AWAIT_SUBTASK_TOOL));
        assert!(names.contains(&AWAIT_SUBTASKS_TOOL));
        assert!(names.contains(&SUBTASK_RESULT_TOOL));
        assert!(names.contains(&INTEGRATE_SUBTASKS_TOOL));
    }

    // --- integrate_subtasks (task #598 merge-back) -------------------------

    use std::path::Path;
    use std::process::Command as StdCommand;

    fn init_repo(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "varda-orch-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).unwrap();
        for args in [
            vec!["init", "--quiet"],
            vec!["config", "--local", "user.email", "t@example.com"],
            vec!["config", "--local", "user.name", "T"],
            vec!["config", "--local", "commit.gpgsign", "false"],
        ] {
            assert!(
                StdCommand::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(&args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::write(root.join("shared.txt"), "base\n").unwrap();
        for args in [vec!["add", "shared.txt"], vec!["commit", "-m", "seed"]] {
            assert!(
                StdCommand::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(&args)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        root
    }

    fn recap_with_files(files: &[&Path]) -> String {
        let mut recap = String::from("# Done\n\n## Files touched\n");
        for f in files {
            recap.push_str(&f.display().to_string());
            recap.push('\n');
        }
        recap.push_str("\nrequires_user: false\n");
        recap
    }

    /// Two workers edit the SAME file in isolated worktrees. `integrate_subtasks`
    /// must merge the first cleanly and surface the second as a REAL conflict
    /// (routed to a resolver via `conflicted_files`), never a silent clobber.
    #[test]
    fn integrate_subtasks_surfaces_a_real_conflict_never_clobbers() {
        let repo = init_repo("integrate-tool");

        let wt_a = repo.parent().unwrap().join(format!(
            "orch-wt-a-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let wt_b = repo.parent().unwrap().join(format!(
            "orch-wt-b-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(1)
        ));
        let a = git::create_worker_worktree(&repo, "worker-a", &wt_a).unwrap();
        let b = git::create_worker_worktree(&repo, "worker-b", &wt_b).unwrap();

        // Both edit the same file to conflicting contents in their own worktrees.
        std::fs::write(a.path.join("shared.txt"), "from-a\n").unwrap();
        std::fs::write(b.path.join("shared.txt"), "from-b\n").unwrap();

        // The resident's integration worktree (its own branch off HEAD).
        let int = repo.parent().unwrap().join(format!(
            "orch-wt-int-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(2)
        ));
        let integration = git::create_worker_worktree(&repo, "integration", &int).unwrap();

        // Registry: launcher recorded both isolated checkouts.
        let registry = WorkerRegistry::new();
        registry.record("a".to_owned(), a.clone());
        registry.record("b".to_owned(), b.clone());

        // Results seam feeds each worker's recap (structured files_touched only).
        let a_files = a.path.join("shared.txt");
        let b_files = b.path.join("shared.txt");
        let recap_a = recap_with_files(&[a_files.as_path()]);
        let recap_b = recap_with_files(&[b_files.as_path()]);
        let results = MockResults::new(|_, _| Some(TaskStatus::Done))
            .with_recap("a", &recap_a)
            .with_recap("b", &recap_b);

        let broker = SpawnBroker::new(base_policy(), "resident", MockLauncher::new())
            .with_results(results)
            .with_integration(registry, integration.path.clone());

        let value = broker
            .integrate_subtasks(&["a".to_owned(), "b".to_owned()])
            .expect("integration loop should not error on a content conflict");
        let integrations = value["integrations"].as_array().unwrap();
        assert_eq!(integrations.len(), 2);

        // First worker merged cleanly.
        assert_eq!(integrations[0]["subtask_id"], json!("a"));
        assert_eq!(integrations[0]["committed"], json!(true));
        assert_eq!(integrations[0]["clean"], json!(true));
        assert_eq!(integrations[0]["conflicted_files"], json!([] as [String; 0]));

        // Second worker surfaced a REAL conflict on the shared file — the resolver
        // queue — not a silent last-writer-wins overwrite.
        assert_eq!(integrations[1]["subtask_id"], json!("b"));
        assert_eq!(integrations[1]["committed"], json!(true));
        assert_eq!(integrations[1]["clean"], json!(false));
        assert_eq!(integrations[1]["conflicted_files"], json!(["shared.txt"]));

        // The aborted conflict left the integration tree clean (A's merge intact).
        let porcelain = StdCommand::new("git")
            .arg("-C")
            .arg(&integration.path)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(
            porcelain.stdout.is_empty(),
            "conflict must be aborted, tree left clean; got {}",
            String::from_utf8_lossy(&porcelain.stdout)
        );

        for wt in [&a, &b, &integration] {
            let _ = git::remove_worker_worktree(&repo, wt, false);
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// A subtask that was never recorded in the registry (non-git mother → shared
    /// mount) is reported `skipped`, not merged.
    #[test]
    fn integrate_subtasks_skips_unregistered_shared_mount_workers() {
        let repo = init_repo("integrate-skip");
        let int = repo.parent().unwrap().join(format!(
            "orch-skip-int-{}",
            std::process::id()
        ));
        let integration = git::create_worker_worktree(&repo, "integration", &int).unwrap();

        let broker = SpawnBroker::new(base_policy(), "resident", MockLauncher::new())
            .with_results(MockResults::new(|_, _| Some(TaskStatus::Done)))
            .with_integration(WorkerRegistry::new(), integration.path.clone());

        let value = broker
            .integrate_subtasks(&["ghost".to_owned()])
            .expect("skip path should not error");
        let integrations = value["integrations"].as_array().unwrap();
        assert_eq!(integrations.len(), 1);
        assert_eq!(integrations[0]["subtask_id"], json!("ghost"));
        assert_eq!(integrations[0]["skipped"], json!(true));

        let _ = git::remove_worker_worktree(&repo, &integration, false);
        let _ = std::fs::remove_dir_all(&repo);
    }

    // -----------------------------------------------------------------
    // Task control-plane tools (task #640).
    // -----------------------------------------------------------------

    /// In-memory [`TaskControlPlane`] keyed by `(project, id)`, so tests can
    /// assert cross-project scoping without touching the filesystem.
    struct StubControlPlane {
        tasks: BTreeMap<(String, u64), TaskDetail>,
    }

    impl StubControlPlane {
        fn new() -> Self {
            Self {
                tasks: BTreeMap::new(),
            }
        }

        fn with_task(mut self, project: &str, detail: TaskDetail) -> Self {
            self.tasks.insert((project.to_owned(), detail.id), detail);
            self
        }
    }

    impl TaskControlPlane for StubControlPlane {
        fn list_tasks(&self, project_path: &Path, status: Option<TaskStatus>) -> Vec<TaskListEntry> {
            let project = project_path.display().to_string();
            self.tasks
                .iter()
                .filter(|((p, _), _)| p == &project)
                .filter(|(_, detail)| {
                    status.is_none_or(|status| detail.status == status.as_str())
                })
                .map(|(_, detail)| TaskListEntry {
                    id: detail.id,
                    slug: detail.slug.clone(),
                    status: detail.status.clone(),
                    title: detail.title.clone(),
                    assignee: detail.assignee.clone(),
                })
                .collect()
        }

        fn get_task(&self, project_path: &Path, id: u64) -> Option<TaskDetail> {
            let project = project_path.display().to_string();
            self.tasks.get(&(project, id)).cloned()
        }

        fn set_task_status(
            &self,
            project_path: &Path,
            id: u64,
            _status: TaskStatus,
        ) -> Result<(), String> {
            let project = project_path.display().to_string();
            if self.tasks.contains_key(&(project, id)) {
                Ok(())
            } else {
                Err(format!("task '{id}' not found"))
            }
        }
    }

    fn task_detail(id: u64, status: TaskStatus) -> TaskDetail {
        TaskDetail {
            id,
            slug: format!("task-{id}"),
            status: status.as_str().to_owned(),
            title: format!("Task {id}"),
            assignee: Some("claude".to_owned()),
            body: "do the thing".to_owned(),
        }
    }

    /// (c) `list_tasks` returns only the caller's own project's tasks, never
    /// another project's, even though the control plane holds both.
    #[test]
    fn list_tasks_returns_only_the_callers_own_project() {
        let plane = StubControlPlane::new()
            .with_task("/proj/a", task_detail(1, TaskStatus::Running))
            .with_task("/proj/a", task_detail(2, TaskStatus::Done))
            .with_task("/proj/b", task_detail(99, TaskStatus::Running));

        let broker = SpawnBroker::new(base_policy(), "1", MockLauncher::new())
            .with_task_control_plane(plane, PathBuf::from("/proj/a"));

        let entries = broker.list_tasks(None);
        let ids: Vec<u64> = entries.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![1, 2]);
        assert!(
            entries.iter().all(|e| e.id != 99),
            "a sibling project's task id must never appear"
        );

        let running_only = broker.list_tasks(Some(TaskStatus::Running));
        assert_eq!(running_only.len(), 1);
        assert_eq!(running_only[0].id, 1);
    }

    /// (a) A cross-project id is refused: `get_task` for an id that exists
    /// but belongs to a DIFFERENT project than the caller's must report
    /// not-found, exactly like an unknown id — never leak that it exists
    /// elsewhere.
    #[test]
    fn get_task_refuses_a_cross_project_id() {
        let plane = StubControlPlane::new()
            .with_task("/proj/a", task_detail(1, TaskStatus::Running))
            .with_task("/proj/b", task_detail(99, TaskStatus::Running));

        let broker = SpawnBroker::new(base_policy(), "1", MockLauncher::new())
            .with_task_control_plane(plane, PathBuf::from("/proj/a"));

        assert!(broker.get_task(1).is_some(), "own-project id must resolve");
        assert!(
            broker.get_task(99).is_none(),
            "a real id from ANOTHER project must be refused, not leaked"
        );
        assert!(broker.get_task(12345).is_none(), "unknown id must also refuse");
    }

    /// (b) Forbidden status transitions are refused: guests may not move a
    /// task out of `review` (the human gate), and may not self-target a
    /// task that is not their own.
    #[test]
    fn set_task_status_refuses_forbidden_transitions() {
        let plane = StubControlPlane::new()
            .with_task("/proj/a", task_detail(1, TaskStatus::Running))
            .with_task("/proj/a", task_detail(2, TaskStatus::Review));

        let broker = SpawnBroker::new(base_policy(), "1", MockLauncher::new())
            .with_task_control_plane(plane, PathBuf::from("/proj/a"));

        // Self, running -> done: allowed.
        assert!(broker.set_task_status("1", 1, TaskStatus::Done).is_ok());

        // review -> done is a human-only gate, even for the task's own caller.
        let broker2 = SpawnBroker::new(base_policy(), "2", MockLauncher::new())
            .with_task_control_plane(
                StubControlPlane::new()
                    .with_task("/proj/a", task_detail(1, TaskStatus::Running))
                    .with_task("/proj/a", task_detail(2, TaskStatus::Review)),
                PathBuf::from("/proj/a"),
            );
        let err = broker2
            .set_task_status("2", 2, TaskStatus::Done)
            .expect_err("review -> done must be refused");
        assert!(err.contains("review"), "got {err}");

        // A caller may never set another task's (even its own project's) status.
        let err = broker
            .set_task_status("1", 2, TaskStatus::Done)
            .expect_err("setting a non-self task must be refused");
        assert!(err.contains("own"), "got {err}");

        // Guests may never target a non-terminal status.
        let broker3 = SpawnBroker::new(base_policy(), "3", MockLauncher::new())
            .with_task_control_plane(
                StubControlPlane::new().with_task("/proj/a", task_detail(3, TaskStatus::Running)),
                PathBuf::from("/proj/a"),
            );
        let err = broker3
            .set_task_status("3", 3, TaskStatus::Running)
            .expect_err("non-terminal target status must be refused");
        assert!(err.contains("guests may only set"), "got {err}");
    }

    #[test]
    fn rpc_set_task_status_denial_is_a_tool_error() {
        let plane = StubControlPlane::new().with_task("/proj/a", task_detail(5, TaskStatus::Review));
        let broker = SpawnBroker::new(base_policy(), "5", MockLauncher::new())
            .with_task_control_plane(plane, PathBuf::from("/proj/a"));

        let resp = broker.handle_rpc(
            "5",
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                    "params": {"name": SET_TASK_STATUS_TOOL, "arguments": {"id": 5, "status": "done"}}}),
        );
        assert_eq!(resp["result"]["isError"], json!(true));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("review"), "got {text}");
    }
}
