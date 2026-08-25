//! Host-side verification gate (#674): before a worker's `files_touched` is
//! committed, run the project's configured `verify` commands on the HOST. The
//! sandbox that produced the change is not a trustworthy witness to whether it
//! builds or tests clean — a warmed cache can let it build there while it
//! still fails on the host, and its own network/filesystem restrictions can
//! make it self-report failures that do not reproduce on the host either. The
//! host is the authority; the box is not.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};

/// Result of running a route's configured `verify` commands against a
/// project's working tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationOutcome {
    /// No `verify` command configured for the matched route; the gate did not run.
    Skipped,
    /// Every configured command exited zero, in order.
    Passed { commands: Vec<String> },
    /// A command exited non-zero. Combined stdout+stderr, for the recap and
    /// for whoever picks the task up next.
    Failed { command: String, output: String },
}

impl VerificationOutcome {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Skipped => "skipped",
            Self::Passed { .. } => "passed",
            Self::Failed { .. } => "failed",
        }
    }
}

/// Run `commands` in order inside `project_path`, stopping at the first
/// failure. Each entry is a full shell line executed via `sh -c`, so config
/// can use pipelines/flags freely (e.g. `cargo check --all-targets`).
pub fn run_verification(project_path: &Path, commands: &[String]) -> Result<VerificationOutcome> {
    if commands.is_empty() {
        return Ok(VerificationOutcome::Skipped);
    }
    for command in commands {
        let output = Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(project_path)
            .output()
            .with_context(|| {
                format!(
                    "failed to run verification command `{command}` in {}",
                    project_path.display()
                )
            })?;
        if !output.status.success() {
            let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
            combined.push_str(&String::from_utf8_lossy(&output.stderr));
            return Ok(VerificationOutcome::Failed {
                command: command.clone(),
                output: combined,
            });
        }
    }
    Ok(VerificationOutcome::Passed {
        commands: commands.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tempdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "varda-verify-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn no_commands_is_skipped() {
        let dir = tempdir();
        let outcome = run_verification(&dir, &[]).unwrap();
        assert_eq!(outcome, VerificationOutcome::Skipped);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn all_commands_succeeding_is_passed() {
        let dir = tempdir();
        let outcome = run_verification(&dir, &["true".to_owned(), "true".to_owned()]).unwrap();
        assert_eq!(
            outcome,
            VerificationOutcome::Passed {
                commands: vec!["true".to_owned(), "true".to_owned()]
            }
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_failing_command_is_failed_and_stops_the_sequence() {
        let dir = tempdir();
        let marker = dir.join("should-not-exist");
        let outcome = run_verification(
            &dir,
            &["exit 1".to_owned(), format!("touch {}", marker.display())],
        )
        .unwrap();
        match outcome {
            VerificationOutcome::Failed { command, .. } => assert_eq!(command, "exit 1"),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert!(!marker.exists(), "sequence should stop at first failure");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn failure_output_captures_stderr() {
        let dir = tempdir();
        let outcome = run_verification(&dir, &["echo boom 1>&2; exit 1".to_owned()]).unwrap();
        match outcome {
            VerificationOutcome::Failed { output, .. } => {
                assert!(output.contains("boom"), "output was: {output}")
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        fs::remove_dir_all(&dir).ok();
    }

    /// Regression for the ef64e83 incident: a `Dockerfile.agents` change added
    /// `cargo fetch --locked` after copying only `Cargo.toml`/`Cargo.lock`, so
    /// `cargo` failed with "no targets specified in the manifest". The gate
    /// must refuse (report Failed), not silently pass, when the configured
    /// verification command fails this way.
    #[test]
    fn refuses_a_manifest_only_cargo_fetch_like_failure() {
        let dir = tempdir();
        fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"regress\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        // No `src/`, so `cargo fetch` (if available) or our stand-in shell
        // check fails exactly as `ef64e83` did: no targets in the manifest.
        let outcome = run_verification(
            &dir,
            &[format!(
                "test -f {}/src/main.rs -o -f {}/src/lib.rs",
                dir.display(),
                dir.display()
            )],
        )
        .unwrap();
        assert!(
            matches!(outcome, VerificationOutcome::Failed { .. }),
            "expected the gate to refuse a manifest with no targets, got {outcome:?}"
        );
        fs::remove_dir_all(&dir).ok();
    }
}
