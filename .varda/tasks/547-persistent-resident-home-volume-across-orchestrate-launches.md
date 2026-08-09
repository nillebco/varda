---
id: 547
project: /Users/nilleb/dev/nillebco/varda
assignee: claude
---

# Persistent resident HOME across `varda orchestrate` launches (survive session state between runs)

# Persistent resident HOME volume for the self-hosting orchestrator

## Goal
Give the `varda orchestrate` RESIDENT a HOME that PERSISTS across separate `varda orchestrate` invocations, so its Claude Code session state (config, onboarding, MCP registration, conversation/resume state, caches) carries over instead of starting from an empty box every launch.

## Background / why
Each `varda orchestrate` currently spins a FRESH ephemeral microsandbox box; the guest HOME (`/home/agent`) lives in VM storage and is discarded when the box exits (docker path uses a *per-session* named volume — also not reused across runs). Consequences observed during the dogfood:
- Onboarding/theme ran every launch until we baked `hasCompletedOnboarding` into the image (`Dockerfile.agent`) — a workaround, not real persistence.
- No cross-run Claude session/resume state: the resident can't resume its own prior conversation; every launch is cold.
- Any per-resident learned state (caches, tool config) is lost each run.

This is distinct from the existing **per-session** HOME volume (docker) / VM-storage HOME (msb), which is intentionally ephemeral for one-off worker/task runs. The resident is conceptually long-lived across launches and wants opt-in persistence.

## Key constraints (do NOT regress the isolation model)
- MUST NOT mount the host's real `~/.claude` / `~/.config/*` / any credential dir — the CREDENTIAL_DENYLIST / control-plane denylist stay in force. Auth still comes ONLY from the injected `CLAUDE_CODE_OAUTH_TOKEN` at prepare time; the persistent HOME must never become a channel for host credentials or a push cred.
- The persistent store is a DEDICATED varda-owned volume/dir (e.g. under `<varda_home>/orchestrate/home/` or a named volume `varda-resident-home`), scoped to the resident identity — NOT a bind of any user dir.
- Reconcile with the microsandbox `--copy-file` re-owns-parent-to-root gotcha (see the `/opt/varda/prompt.txt` staging fix in acp.rs): whatever backs HOME must land agent-owned (uid 1001) and writable, and must not be shadowed by any copy-file target.
- Keep the G1/G2/G7 resident gates intact (isolating sandbox, LLM-only egress, no push cred, broker caps). A persistent HOME is state, not a network/push widening.

## Design sketch
- Add an opt-in persistent-HOME backing for the resident route/sandbox:
  - microsandbox: a host dir bind-mounted at `/home/agent` (agent-owned; pre-create + chown to uid 1001 on the host, or seed once) — verify msb bind ownership maps correctly for uid 1001, else use an msb volume if/when supported.
  - docker: a STABLE named volume (e.g. `varda-resident-home`) instead of the per-session `varda-sbx-<session>` volume, reused across runs.
- Gate it behind explicit config (e.g. `[sandboxes.orchestrate] persistent_home = true` or a resident-specific flag) so ordinary worker sandboxes stay ephemeral by default.
- Seed the onboarding config into the persistent HOME on first init (then drop/keep the image bake as a fallback).
- Ensure extract_session_store / resume-capture still works with a persistent HOME (don't double-copy or clobber).

## Exit criteria
- Two consecutive `varda orchestrate` launches share the resident's `~/.claude` state: no re-onboarding, and Claude session/resume state from the prior launch is present.
- The persistent HOME is a dedicated varda-owned store — no host credential dir is mounted; CLAUDE_CODE_OAUTH_TOKEN injection unchanged; all resident gates still pass.
- HOME is agent-owned (uid 1001) and writable in-guest (no EACCES); the `--copy-file` prompt/cred staging still lands outside HOME.
- Worker/one-off sandboxes remain ephemeral (persistence is opt-in for the resident only).

## Prior art / links
- Ephemeral HOME today: `src/sandbox/mod.rs` (docker per-session named volume ~L1188/L1519; msb VM-storage HOME + `extract_session_store`).
- Onboarding bake + copy-file/HOME ownership fix: `Dockerfile.agent`, `src/acp.rs` GUEST_PROMPT_FILE → `/opt/varda/prompt.txt`.
- Resident gates: `config::enforce_resident_launch`; egress allowlists in `src/config.rs`.
