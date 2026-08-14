---
id: 534
project: /Users/nilleb/dev/nillebco/varda
assignee: claude
---

# Bugfix: skip interpreter/recap pass for shell agents (vmsbsh/vdocksh)

# Bugfix: shell agents (vmsbsh/vdocksh) must NOT invoke an interpreter agent for the recap

## Symptom
Running an interactive SHELL session (agent `shell`, `interactive_command = "sh"`, e.g. `vmsbsh`/`vdocksh`) triggers Varda's post-interactive interpreter/recap pass, which INVOKES AN AGENT (the `interpreter_agent`, defaulting to the same agent, or a fallback real agent) to "interpret" the session. A bare shell has no Varda recap to produce — invoking an LLM agent for it is wrong (wasted call, needs auth, pointless output).

## Fix
Add a way to SKIP the interpreter/recap pass for an agent. Suggested: `skip_recap: bool` (or `interactive_only: bool`) on `AgentConfig` (`src/config.rs`), default false. When true, `runner.rs`'s post-interactive finalization (`interpret_interactive_session` / the interpreter pass) is SKIPPED — the interactive session runs and tears down with NO agent invocation for interpretation, and the task closes without an LLM-produced recap (a minimal "interactive shell session" note is fine).
- Set `skip_recap = true` on the `[agents.shell]` config used by vmsbsh/vdocksh.
- The default (false) preserves current behavior for real agents.

## Tests
- An interactive run with a `skip_recap` agent does NOT invoke the interpreter agent (no extra agent spawn); the task still closes cleanly.
- A normal agent (skip_recap unset/false) still runs the interpreter pass (regression).

## Footprint
`src/config.rs` (AgentConfig field), `src/runner.rs` (gate the interpreter pass), tests. Small.
