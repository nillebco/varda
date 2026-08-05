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

// The policy engine is fully unit-tested but not yet consulted by the run path:
// the live `spawn_subtask` MCP tool that will call `SpawnLedger::authorize` before
// launching a sibling sandbox is the remaining step of this milestone. Silence
// dead-code warnings until that wiring lands rather than leave the crate noisy.
#![allow(dead_code)]

use std::collections::BTreeMap;

use globset::Glob;
use serde::{Deserialize, Serialize};

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
}
