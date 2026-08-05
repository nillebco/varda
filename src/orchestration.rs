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

// The pure policy engine ([`SpawnLedger`]/[`OrchestrationPolicy`]) is now consumed
// by the live [`SpawnBroker`] below, which mediates the `spawn_subtask` MCP tool
// and hands accepted spawns to a host [`SubtaskLauncher`]. The one remaining piece
// is the MCP *transport* that carries the broker's JSON-RPC into a running sandbox
// (a stdio/socket channel reachable only from the box); until that lands, some
// broker entry points are exercised only by tests, so keep the crate quiet.
#![allow(dead_code)]

use std::collections::BTreeMap;

use globset::Glob;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
        }
    }
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
    pub fn new() -> Self {
        Self::default()
    }

    /// Total subtasks spawned so far this run.
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

    /// Record an accepted spawn against the caller (parent) and the global budget.
    /// Call this only after [`authorize`](Self::authorize) succeeded AND the sibling
    /// sandbox was actually launched, so the ledger reflects reality.
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
/// Optional read-back tools (result plumbing). Present in the manifest so a master
/// can discover them; the live wiring for results flows through varda memory.
pub const AWAIT_SUBTASK_TOOL: &str = "await_subtask";
pub const SUBTASK_RESULT_TOOL: &str = "subtask_result";

/// The host-side seam that actually launches an AUTHORIZED subtask in its own
/// sibling sandbox. The broker calls this ONLY after
/// [`SpawnLedger::authorize_and_record`] has succeeded, passing the re-validated
/// request and the assigned child depth. Implementations reuse the normal run
/// path (materialize a task + run it in a fresh box); they MUST NOT nest inside
/// the caller's sandbox (invariant 3). Returns the new subtask id.
pub trait SubtaskLauncher {
    fn launch(&mut self, req: &SpawnRequest, grant: &SpawnGrant) -> anyhow::Result<SubtaskId>;
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

/// The live broker: policy + ledger + lineage registry + a host launcher. One
/// broker instance backs one root run; its lineage map is the real spawn tree the
/// `max_depth` cap is enforced against.
pub struct SpawnBroker<L: SubtaskLauncher> {
    policy: OrchestrationPolicy,
    ledger: SpawnLedger,
    /// Task id → depth in the tree. The root master is registered at depth 0 by
    /// [`SpawnBroker::new`]; each accepted child is registered at its granted depth
    /// so its own future spawns resolve their depth correctly.
    depths: BTreeMap<SubtaskId, u32>,
    launcher: L,
}

impl<L: SubtaskLauncher> SpawnBroker<L> {
    /// Create a broker for a root run. `root_id` is the master task id; it is
    /// registered at depth 0 so its spawns land at depth 1.
    pub fn new(policy: OrchestrationPolicy, root_id: impl Into<SubtaskId>, launcher: L) -> Self {
        let mut depths = BTreeMap::new();
        depths.insert(root_id.into(), 0);
        Self {
            policy,
            ledger: SpawnLedger::new(),
            depths,
            launcher,
        }
    }

    /// Total subtasks spawned so far this run (for observability/tests).
    pub fn global_spawned(&self) -> u32 {
        self.ledger.global_spawned()
    }

    /// Depth of a known task, or `None` if it was never registered.
    pub fn depth_of(&self, id: &str) -> Option<u32> {
        self.depths.get(id).copied()
    }

    /// Handle one `spawn_subtask` request from `parent_id` (the id of the sandbox
    /// that owns the broker channel — the host knows this; it is NOT attacker
    /// supplied). Gates on policy, launches a sibling on success, and records
    /// lineage for the new child. Every denial is a hard error (invariant 5).
    pub fn spawn_subtask(
        &mut self,
        parent_id: &str,
        req: SpawnRequest,
    ) -> Result<SubtaskId, BrokerError> {
        let parent_depth = self
            .depths
            .get(parent_id)
            .copied()
            .ok_or_else(|| BrokerError::UnknownParent(parent_id.to_owned()))?;

        let ctx = SpawnContext {
            parent_id: parent_id.to_owned(),
            parent_depth,
        };

        let grant = self
            .ledger
            .authorize_and_record(&self.policy, &ctx, &req)
            .map_err(BrokerError::Denied)?;

        match self.launcher.launch(&req, &grant) {
            Ok(child_id) => {
                self.depths.insert(child_id.clone(), grant.child_depth);
                Ok(child_id)
            }
            Err(e) => {
                // The host failed to launch: undo the ledger record so the failed
                // attempt does not consume fan-out / global budget.
                self.ledger.unrecord(parent_id);
                Err(BrokerError::Launch(e.to_string()))
            }
        }
    }

    /// The MCP `tools/list` manifest: exactly the narrow spawn tool plus the
    /// optional read-backs. Nothing else is advertised across the boundary.
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
                {"name": AWAIT_SUBTASK_TOOL, "description": "Block until a spawned sub-task finishes (result flows via varda memory).", "inputSchema": {"type": "object", "required": ["subtask_id"], "properties": {"subtask_id": {"type": "string"}}}},
                {"name": SUBTASK_RESULT_TOOL, "description": "Fetch the recap/result of a finished sub-task.", "inputSchema": {"type": "object", "required": ["subtask_id"], "properties": {"subtask_id": {"type": "string"}}}}
            ]
        })
    }

    /// Dispatch one MCP JSON-RPC request arriving on the broker channel owned by
    /// `parent_id`. Handles `initialize`, `tools/list`, and `tools/call`; a
    /// `spawn_subtask` call is gated through [`SpawnBroker::spawn_subtask`] and a
    /// denial is returned as an MCP tool error (`isError: true`) carrying the
    /// [`SpawnDenied`] reason — never a silent cap. Returns the JSON-RPC response.
    pub fn handle_rpc(&mut self, parent_id: &str, request: &Value) -> Value {
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

    fn handle_tool_call(&mut self, id: Value, parent_id: &str, params: Option<&Value>) -> Value {
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
                    Ok(child_id) => rpc_result(id, tool_text(&format!("subtask_id: {child_id}"), false)),
                    Err(e) => rpc_result(id, tool_text(&e.to_string(), true)),
                }
            }
            AWAIT_SUBTASK_TOOL | SUBTASK_RESULT_TOOL => rpc_result(
                id,
                tool_text(
                    "not available on this channel: sub-task results flow back through varda memory",
                    true,
                ),
            ),
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
        let mut ledger = SpawnLedger::new();
        let deny_ctx = ctx("deep", 5); // depth-exceeded
        assert!(ledger.authorize(&policy, &deny_ctx, &req()).is_err());
        assert_eq!(ledger.global_spawned(), 0);
        assert_eq!(ledger.children_of("deep"), 0);
    }

    // --- Live broker -------------------------------------------------------

    /// Deterministic launcher for tests: hands back sequential child ids and
    /// records every spawn it was asked to launch, so tests can assert the host
    /// only ever launches AUTHORIZED spawns (never one the policy denied).
    struct MockLauncher {
        next: u32,
        launched: Vec<(SpawnRequest, u32)>,
        fail: bool,
    }
    impl MockLauncher {
        fn new() -> Self {
            Self { next: 0, launched: Vec::new(), fail: false }
        }
        fn failing() -> Self {
            Self { next: 0, launched: Vec::new(), fail: true }
        }
    }
    impl SubtaskLauncher for MockLauncher {
        fn launch(&mut self, req: &SpawnRequest, grant: &SpawnGrant) -> anyhow::Result<SubtaskId> {
            if self.fail {
                anyhow::bail!("simulated host launch failure");
            }
            self.launched.push((req.clone(), grant.child_depth));
            self.next += 1;
            Ok(format!("sub-{}", self.next))
        }
    }

    #[test]
    fn broker_launches_authorized_spawn_and_returns_child_id() {
        let mut broker = SpawnBroker::new(base_policy(), "root", MockLauncher::new());
        let id = broker.spawn_subtask("root", req()).expect("should launch");
        assert_eq!(id, "sub-1");
        assert_eq!(broker.depth_of("sub-1"), Some(1));
        assert_eq!(broker.global_spawned(), 1);
    }

    #[test]
    fn broker_denial_never_reaches_the_launcher() {
        // Disabled policy: the launcher must not be called at all.
        let mut broker = SpawnBroker::new(OrchestrationPolicy::default(), "root", MockLauncher::new());
        match broker.spawn_subtask("root", req()) {
            Err(BrokerError::Denied(SpawnDenied::Disabled)) => {}
            other => panic!("expected Disabled denial, got {other:?}"),
        }
        assert_eq!(broker.global_spawned(), 0);
    }

    #[test]
    fn broker_tracks_lineage_depth_across_the_real_tree() {
        // max_depth = 2: root(0) → child(1) → grandchild(2) ok; great-grandchild(3) denied.
        let mut broker = SpawnBroker::new(base_policy(), "root", MockLauncher::new());
        let child = broker.spawn_subtask("root", req()).unwrap();
        assert_eq!(broker.depth_of(&child), Some(1));
        let grand = broker.spawn_subtask(&child, req()).unwrap();
        assert_eq!(broker.depth_of(&grand), Some(2));
        // Now a spawn from the depth-2 grandchild would land at depth 3 > max_depth.
        match broker.spawn_subtask(&grand, req()) {
            Err(BrokerError::Denied(SpawnDenied::DepthExceeded { attempted: 3, max: 2 })) => {}
            other => panic!("expected DepthExceeded, got {other:?}"),
        }
    }

    #[test]
    fn broker_rejects_unknown_parent_lineage_spoof() {
        // A caller the broker never spawned cannot spawn (can't fake a shallow depth).
        let mut broker = SpawnBroker::new(base_policy(), "root", MockLauncher::new());
        match broker.spawn_subtask("ghost", req()) {
            Err(BrokerError::UnknownParent(id)) => assert_eq!(id, "ghost"),
            other => panic!("expected UnknownParent, got {other:?}"),
        }
    }

    #[test]
    fn broker_rolls_back_budget_when_host_launch_fails() {
        let mut broker = SpawnBroker::new(base_policy(), "root", MockLauncher::failing());
        match broker.spawn_subtask("root", req()) {
            Err(BrokerError::Launch(_)) => {}
            other => panic!("expected Launch error, got {other:?}"),
        }
        // A failed launch must not consume budget/fan-out.
        assert_eq!(broker.global_spawned(), 0);
        assert_eq!(broker.depth_of("sub-1"), None);
    }

    #[test]
    fn rpc_tools_list_exposes_only_the_narrow_spawn_surface() {
        let mut broker = SpawnBroker::new(base_policy(), "root", MockLauncher::new());
        let resp = broker.handle_rpc("root", &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}));
        let names: Vec<String> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(names, vec![SPAWN_SUBTASK_TOOL, AWAIT_SUBTASK_TOOL, SUBTASK_RESULT_TOOL]);
    }

    #[test]
    fn rpc_spawn_call_returns_subtask_id_on_success() {
        let mut broker = SpawnBroker::new(base_policy(), "root", MockLauncher::new());
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
        let mut broker = SpawnBroker::new(policy, "root", MockLauncher::new());
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
        let mut broker = SpawnBroker::new(base_policy(), "root", MockLauncher::new());
        let resp = broker.handle_rpc(
            "root",
            &json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                    "params": {"name": SPAWN_SUBTASK_TOOL, "arguments": {"brief": "x", "sandbox": "local"}}}),
        );
        assert_eq!(resp["result"]["isError"], json!(true));
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("must run in an isolating sibling box"), "got {text}");
    }
}
