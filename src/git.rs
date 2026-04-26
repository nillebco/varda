//! Git integration.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

pub fn commit_task_update(
    task_path: &Path,
    recap_path: &Path,
    notification_path: Option<&Path>,
) -> Result<()> {
    let task_arg = path_arg(task_path);
    let recap_arg = path_arg(recap_path);

    run_git(["add", task_arg.as_str(), recap_arg.as_str()])
        .context("failed to stage task update")?;

    if let Some(notification_path) = notification_path {
        let notification_arg = path_arg(notification_path);
        run_git(["add", notification_arg.as_str()]).context("failed to stage notification")?;
    }

    let message = format!("Update task {}", task_path.display());
    run_git(["commit", "-m", message.as_str()]).context("failed to commit task update")?;

    Ok(())
}

fn run_git<const N: usize>(args: [&str; N]) -> Result<()> {
    let output = Command::new("git")
        .args(args)
        .output()
        .context("failed to start git")?;

    if !output.status.success() {
        bail!(
            "git exited with status {}; stderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

fn path_arg(path: &Path) -> String {
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_arg_uses_display_path() {
        assert_eq!(path_arg(Path::new("task.md")), "task.md");
    }
}
