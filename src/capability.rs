//! M12 — per-task capability allowlist for headless runs.
//!
//! A headless Varda run has no interactive approver: the agent backend's own
//! permission layer denies any command it was not pre-authorized to execute (in
//! `-p`/print mode there is no human to say "yes"). That is exactly what blocked
//! the M4 sandbox-self-test agent, which needed host `msb`/`docker` to
//! live-verify and correctly degraded to `needs_user`.
//!
//! This module turns a task's declared [`allow_commands`] into the agent
//! backend's permission config for a single run — deterministically, with no
//! human and WITHOUT a blanket `--dangerously-skip-permissions`. For the Claude
//! Code backend that means a run-scoped settings file carrying
//! `permissions.allow: ["Bash(msb:*)", …]`, injected via `--settings`.
//!
//! [`allow_commands`]: crate::task::TaskFrontmatter::allow_commands
//!
//! ## Trust model
//!
//! The allowlist is scoped to exactly the declared commands — never a global
//! bypass. Each bare command name maps to a single `Bash(<cmd>:*)` prefix rule;
//! a command NOT on the list still blocks (and is surfaced back to the
//! orchestrator via the recap's `Blocked commands` section, see
//! [`crate::agent::parse_blocked_commands`], so a scripted re-run can widen the
//! list). Cross-ref: the sandbox-self-test host-allowlist carve-out documented
//! in the README.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Translate one allowlist entry into a Claude Code permission rule.
///
/// A bare command name (`"msb"`) becomes a `Bash(<cmd>:*)` prefix rule that
/// authorizes any invocation of that command. An entry that already looks like a
/// full agent tool pattern — it contains a `(` — is passed through verbatim so
/// callers can express richer rules (e.g. `"Bash(cargo test:*)"` or
/// `"Read(//etc/hosts)"`).
pub fn claude_permission_entry(token: &str) -> String {
    let token = token.trim();
    if token.contains('(') {
        token.to_owned()
    } else {
        format!("Bash({token}:*)")
    }
}

/// Translate the whole allowlist into Claude Code `permissions.allow` entries,
/// dropping empties and de-duplicating while preserving first-seen order.
pub fn claude_permission_allow(commands: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for command in commands {
        if command.trim().is_empty() {
            continue;
        }
        let entry = claude_permission_entry(command);
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    out
}

/// Build the run-scoped Claude Code settings document that pre-authorizes
/// `commands`. Only the `permissions.allow` list is set; everything else is left
/// to the agent's own user/project settings — this file layers additively on top
/// and never relaxes anything beyond the declared commands.
pub fn claude_settings_json(commands: &[String]) -> String {
    let allow = claude_permission_allow(commands);
    let value = serde_json::json!({
        "permissions": {
            "allow": allow,
        }
    });
    serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned())
}

/// Derive the run-scoped settings path that sits beside the Varda session log
/// (`<runs>/<session_id>.log` → `<runs>/<session_id>.settings.json`), so the
/// per-run artifact is co-located with the rest of the run's records and is
/// trivially discoverable for debugging.
pub fn run_settings_path(session_log_path: &Path) -> PathBuf {
    session_log_path.with_extension("settings.json")
}

/// Write the run-scoped Claude Code settings file for `commands` next to
/// `session_log_path` and return its path. Returns `Ok(None)` when the allowlist
/// is empty (nothing to authorize, so no file and no `--settings` injection).
pub fn write_claude_run_settings(
    session_log_path: &Path,
    commands: &[String],
) -> Result<Option<PathBuf>> {
    if claude_permission_allow(commands).is_empty() {
        return Ok(None);
    }
    let path = run_settings_path(session_log_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create run settings directory {}",
                parent.display()
            )
        })?;
    }
    std::fs::write(&path, claude_settings_json(commands))
        .with_context(|| format!("failed to write run settings {}", path.display()))?;
    Ok(Some(path))
}

/// True when `command` runs the Claude Code CLI, whose `-p` headless permission
/// model is the one this allowlist targets (codex `workspace-write` and copilot
/// `--allow-all-tools` already grant broad execution non-interactively).
pub fn is_claude_backend(command: &str) -> bool {
    command == "claude" || command.ends_with("/claude")
}

/// Single-quote `value` so it survives as one literal argument when spliced into
/// a string destined for `sh -c`, regardless of spaces or shell metacharacters it
/// contains.
pub fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_command_becomes_bash_prefix_rule() {
        assert_eq!(claude_permission_entry("msb"), "Bash(msb:*)");
        assert_eq!(claude_permission_entry("  docker "), "Bash(docker:*)");
    }

    #[test]
    fn full_tool_pattern_passes_through() {
        assert_eq!(
            claude_permission_entry("Bash(cargo test:*)"),
            "Bash(cargo test:*)"
        );
        assert_eq!(
            claude_permission_entry("Read(//etc/hosts)"),
            "Read(//etc/hosts)"
        );
    }

    #[test]
    fn allow_list_dedups_and_drops_empties() {
        let allow = claude_permission_allow(&[
            "msb".to_owned(),
            "".to_owned(),
            "  ".to_owned(),
            "msb".to_owned(),
            "docker".to_owned(),
        ]);
        assert_eq!(allow, vec!["Bash(msb:*)", "Bash(docker:*)"]);
    }

    #[test]
    fn settings_json_carries_only_allow_permissions() {
        let json = claude_settings_json(&["msb".to_owned(), "docker".to_owned()]);
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value["permissions"]["allow"],
            serde_json::json!(["Bash(msb:*)", "Bash(docker:*)"])
        );
        // No global bypass key is ever emitted.
        assert!(!json.contains("dangerously"));
        assert!(value["permissions"].get("deny").is_none());
    }

    #[test]
    fn run_settings_path_sits_beside_session_log() {
        let log = Path::new("/home/u/.varda/operations/runs/abc-123.log");
        assert_eq!(
            run_settings_path(log),
            Path::new("/home/u/.varda/operations/runs/abc-123.settings.json")
        );
    }

    #[test]
    fn empty_allowlist_writes_no_file() {
        let dir = std::env::temp_dir().join(format!("varda-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("session.log");
        assert!(write_claude_run_settings(&log, &[]).unwrap().is_none());
        assert!(!run_settings_path(&log).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_empty_allowlist_writes_readable_settings() {
        let dir = std::env::temp_dir().join(format!("varda-cap-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("session.log");
        let path = write_claude_run_settings(&log, &["msb".to_owned()])
            .unwrap()
            .expect("a settings file should be written");
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("Bash(msb:*)"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn claude_backend_detection() {
        assert!(is_claude_backend("claude"));
        assert!(is_claude_backend("/usr/local/bin/claude"));
        assert!(!is_claude_backend("codex"));
        assert!(!is_claude_backend("sh"));
    }

    #[cfg(unix)]
    #[test]
    fn shell_single_quoted_path_with_spaces_stays_one_argument_when_run_through_sh() {
        // Mirrors the operator-input round-trip test in runner.rs: prove the
        // quoting is not just visually plausible but actually survives a real
        // `sh -c` invocation as a single word, using `printf '<%s>'` to make word
        // splitting visible if the quoting were ever to regress.
        let path_with_space = "/Users/John Doe/.varda/runs/abc.settings.json";
        let quoted = shell_single_quote(path_with_space);

        let output = std::process::Command::new("sh")
            .arg("-c")
            .arg(format!("printf '<%s>' --settings {quoted}"))
            .output()
            .expect("sh should run printf with the quoted settings path");
        assert!(output.status.success(), "shell command failed: {output:?}");
        assert_eq!(
            String::from_utf8(output.stdout).expect("shell output is utf8"),
            format!("<--settings><{path_with_space}>"),
            "the space-containing path must arrive as ONE argument, not be word-split"
        );
    }
}
