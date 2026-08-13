---
id: 574
project: /Users/nilleb/dev/varda-orchestrate-workspace
assignee: claude-worker
---

# spawned-subtask-64dec07f

Bugfix #534: shell agents (vmsbsh/vdocksh) must NOT invoke an interpreter agent for the recap.

Symptom: Running an interactive SHELL session (agent `shell`, `interactive_command = "sh"`, e.g. `vmsbsh`/`vdocksh`) triggers Varda's post-interactive interpreter/recap pass, which INVOKES AN AGENT (the `interpreter_agent`, defaulting to the same agent or a fallback real agent) to "interpret" the session. A bare shell has no Varda recap to produce — invoking an LLM agent for it is wasted, needs auth, and produces pointless output. The interpreter pass lives in `interpret_interactive_session` in src/runner.rs (defined ~line 888, called from two call sites around lines 546 and 698).

Fix: add a `skip_recap: bool` field on `AgentConfig` in src/config.rs (default `false`, so existing behavior is unchanged for every agent that doesn't set it). When `true` for the agent running the interactive session, `runner.rs` must SKIP calling `interpret_interactive_session` entirely at both call sites — the interactive session runs and tears down with NO agent invocation for interpretation, and the task closes with a minimal, non-LLM-produced note like "interactive shell session" instead of a recap. Then set `skip_recap = true` on the `[agents.shell]` config used by vmsbsh/vdocksh (search the repo/config templates for where the `shell` agent with `interactive_command = "sh"` is defined — likely in a default-config constant in src/config.rs or a shipped config template — and add the flag there).

Tests to add: (1) an interactive run with a `skip_recap = true` agent does NOT invoke the interpreter agent (assert no extra agent spawn / interpret_interactive_session not called) and the task still closes cleanly; (2) a normal agent (skip_recap unset or false) still runs the interpreter pass unchanged (regression).

Footprint: `src/config.rs` (AgentConfig field + shell agent default config) and `src/runner.rs` (gate around the two interpret_interactive_session call sites), plus tests. Small, scoped change — do not touch `src/main.rs`.

Run `cargo test` and `cargo build` when done and report Files touched per AGENTS.md — do not git add/commit.
