# Agent Instructions

Do NOT run `git add`, `git commit`, or any other git history-modifying command. Varda owns committing for any task it drives.

When you edit, create, delete, format, or otherwise change tracked project files:

1. Run the relevant verification for the change when practical.
2. Leave the changes in the working tree, unstaged.
3. List every changed file (one absolute path per line) under the `Files touched` heading of your recap. Varda stages and commits exactly those paths after the run.

If unrelated user changes are already present, do not revert them and do not list them under `Files touched` unless they are required for the current change.

Think if a README update could make sense.

## Working principles

Non-negotiable expectations for any agent — human or AI — working this repo.

**Verify; report faithfully.** Run the actual build and tests for your change (`cargo check --all-targets`, `cargo test`) and report the real result, including failures. NEVER claim a passing build or test you did not observe. Be precise about state in your recap — distinguish *written* from *compiles* from *tested* from *reviewed*. If a command was blocked (no network, permission, missing dependency), say so under a `Blocked commands` heading. An honest "I could not run cargo" beats a fabricated green check.

**Diagnose from evidence, not speculation.** Read the code, the logs, and the actual runtime state before you name a cause or propose a fix. Do not present hypotheticals as findings, and do not pad a report by listing everything that *might* be wrong. When you assert a root cause, it must be one you traced.

**Correct yourself.** If new evidence disproves a theory you stated, say so plainly and drop it — do not ship a fix built on a dead hypothesis. Distinguish a real defect from transient noise (e.g. machine contention) before filing it as a bug.

**Scope your claims.** State what you verified and what you did not (e.g. "unit-tested, not run end-to-end"), and flag residual risk. Do not overclaim "done."

**File durable fixes as tasks.** When you find a systemic or recurring problem outside your current change, file a varda task capturing the root cause, concrete fix actions, and how to verify — not just the symptom.

**Keep commits focused.** One logical change per commit; do not blend unrelated edits — a feature, an infra tweak, and pre-existing noise are three commits, not one. Customer-specific work (e.g. ADB) and local tooling (`.claude/`, editor configs, host-local `~/.varda` config) do not belong in the varda repo.

**Cross-review with a different agent.** A change is reviewed by an agent other than the one that authored it.
