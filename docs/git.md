# Git behavior

What Varda commits, the verification gate, and the dirty-tree check.

# Git Behavior

When `auto_commit = true`, Varda commits after each processed task.

For a normal task, the commit includes:

- the updated task markdown file
- the generated recap file
- the generated agent session log

For a task that needs user input, the commit also includes:

- a notification JSON file under the global `operations/runs/` folder

Agents must NOT run `git add` or `git commit` themselves during normal non-interactive runs. Instead, every agent recap must include a `Files touched` heading listing one absolute path per line (or `(none)`). Varda parses that section and, before committing the operations metadata, stages and commits exactly those paths in the project repo (which may differ from the operations repo). This avoids interactive git prompts inside the agent process and keeps the commit boundary under Varda's control. Paths outside the project's git repo are skipped with a warning.

Interactive runs use the same Varda-owned commit flow by default. The exception is an explicit user request: when the task text or live interactive conversation asks the agent to commit, the interactive agent may stage and commit only its own changes. Varda still runs the interpretation pass afterward; if the reported project files are already committed, Varda's project-file commit step has nothing further to commit.

## Host-side verification gate

The agent that produced a change is not a trustworthy witness to whether it builds: it may run inside a sandbox with a warmed dependency cache that lets it build there while the host disagrees, or its own network/filesystem restrictions can make it self-report failures that don't reproduce on the host. So before committing a task's `files_touched`, Varda can run a HOST-side verification gate.

Configure it with `verify` on a `[[routes]]` entry — a list of shell commands run in order, in the project directory:

```toml
[[routes]]
glob = "**/my-rust-project/**"
agents = ["claude"]
verify = ["cargo check --all-targets", "cargo test"]
```

- If `verify` is empty (the default), the gate is a no-op — the recap says `verification: skipped` rather than implying verification happened.
- If every command exits zero, the recap says `verification: passed` (with the commands run), and `files_touched` is committed as usual.
- If a command exits non-zero, the recap says `verification: failed` with the failing command and its combined stdout/stderr, `files_touched` is **not** committed (the edits stay uncommitted in the worker's own worktree/`wip/` branch — nothing is lost), and the task settles to `failed` instead of `review`/`done` — UNLESS the task had already settled to `needs_user`, in which case that status is preserved (a genuine question for a human outranks a build failure).

The verification result is always appended to the recap under a `## Verification` heading as structured, greppable text, so a parent task or a human can tell "verified green" from "not verified" from "verified red" without re-running anything.

When running on macOS, Varda also sends a best-effort native notification signal for tasks that need user input. Signal delivery failures are reported to stderr but do not prevent the notification JSON from being written.

## Pre-run dirty-tree check

Before launching an agent, Varda runs `git status --porcelain` against the project repository declared in the task's `project` frontmatter. If anything is reported (modified, staged, or untracked), Varda:

- skips the agent invocation entirely,
- writes a recap explaining the conflict and listing the offending entries,
- sets the task to `needs_user` and fires the macOS notification,
- leaves the user's working tree untouched.

This protects in-progress local work from being entangled with agent edits and keeps the post-run commit unambiguous. Once the listed entries are committed, stashed, or discarded, set the task back to `ready` and re-run it. The check is silently skipped when the project path is missing or not inside a git repository.

[← back to the README](../README.md)
