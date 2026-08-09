---
id: 552
project: /Users/nilleb/dev/varda-orchestrate-workspace
assignee: claude
---

# Orchestrate: resident route must permit spawned worker agents + loud spawn failures

# Orchestrate: resident route must permit spawned worker agents; workers need the worker sandbox; spawn failures must be loud

## Bug (show-stopper, hit live)
`varda orchestrate` scaffolds a resident route that lists ONLY the resident agent
(`agents = ["claude-resident"]`), but the spawn broker launches workers as `claude`
on the SAME project path. Route resolution enforces the route's `agents` allowlist,
so every spawned worker failed with "agent 'claude' is not allowed for route
'**/dev/varda-orchestrate-workspace'" and stayed stuck at `ready`. The resident
then called `await_subtasks` and DEADLOCKED on workers that could never start
(only broke via the await timeout / manual kill). The failure was SILENT — the
launcher only `eprintln!`'d to host stderr; the master got no error.

## Fixes already applied
1. CONFIG (`~/.varda/config.toml`): resident route now
   `agents = ["claude-resident", "claude"]` — the route must permit every agent the
   resident spawns, not just the resident. Comment added explaining why.
2. WORKFLOW (`.varda/WORKFLOW.md` step 2): resident MUST call `spawn_subtask` with
   `agent="claude"` and `sandbox="worker"`. Without the pin, workers inherit the
   resident's LLM-only `orchestrate` sandbox and any cargo/dependency fetch fails;
   the `worker` sandbox has crates.io/github egress.
3. CODE (`src/main.rs` `VardaSubtaskLauncher::launch`): (a) PREFLIGHT
   `routing::match_route` before creating/spawning the subtask — a non-runnable
   agent now fails `spawn_subtask` LOUDLY (error back to the master) instead of
   stranding a `ready` task; (b) if the background run errors, force the subtask to
   `Failed` so an awaiting master observes a terminal status instead of hanging.

## Remaining / deeper work
- DESIGN: an orchestration-authorized spawn is validated by `OrchestrationPolicy`
  (allow_agents/allow_sandboxes) yet ALSO re-gated by the route's static `agents`
  allowlist — double-gating that caused this. Decide the intended contract:
  either broker-spawned runs are authorized by policy and should not be blocked by
  the route agents list, or the route is the single source of truth and
  `varda orchestrate` must VALIDATE at launch (in `enforce_resident_launch`) that
  the route permits the worker agent(s) + has a resolvable worker sandbox, failing
  loudly with actionable guidance.
- Consider a configured default worker sandbox for spawns so the resident doesn't
  have to remember `sandbox="worker"` (belt-and-suspenders with the WORKFLOW hint).
- Add a regression test for `launch()` preflight (agent not on route -> spawn errors).

## Exit criteria
- A fresh `varda orchestrate` wave spawns workers that actually RUN (not stuck
  ready), in the worker sandbox, and merge back in-box.
- A misconfigured spawn (agent not permitted) fails `spawn_subtask` immediately with
  a clear message; no silent `ready` + await deadlock.
