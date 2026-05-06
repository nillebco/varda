//! Git integration.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub fn commit_task_update(
    task_path: &Path,
    recap_path: &Path,
    session_log_path: &Path,
    notification_path: Option<&Path>,
) -> Result<()> {
    let repo = repo_root_for_path(task_path)?;
    ensure_same_repo(&repo, recap_path)?;
    ensure_same_repo(&repo, session_log_path)?;

    let task_arg = repo_relative_path(&repo, task_path)?;
    let recap_arg = repo_relative_path(&repo, recap_path)?;
    let session_log_arg = repo_relative_path(&repo, session_log_path)?;

    run_git_in(
        &repo,
        [
            "add",
            task_arg.as_str(),
            recap_arg.as_str(),
            session_log_arg.as_str(),
        ],
    )
    .context("failed to stage task update")?;

    if let Some(notification_path) = notification_path {
        ensure_same_repo(&repo, notification_path)?;
        let notification_arg = repo_relative_path(&repo, notification_path)?;
        run_git_in(&repo, ["add", notification_arg.as_str()])
            .context("failed to stage notification")?;
    }

    let message = format!("Update task {}", task_path.display());
    run_git_in(&repo, ["commit", "-m", message.as_str()])
        .context("failed to commit task update")?;

    Ok(())
}

pub fn commit_task_plan(task_path: &Path, plan_path: &Path) -> Result<()> {
    let repo = repo_root_for_path(task_path)?;
    ensure_same_repo(&repo, plan_path)?;

    let task_arg = repo_relative_path(&repo, task_path)?;
    let plan_arg = repo_relative_path(&repo, plan_path)?;

    run_git_in(&repo, ["add", task_arg.as_str(), plan_arg.as_str()])
        .context("failed to stage task plan")?;

    let message = format!("Plan task {}", task_path.display());
    run_git_in(&repo, ["commit", "-m", message.as_str()]).context("failed to commit task plan")?;

    Ok(())
}

pub fn commit_task_file(task_path: &Path, message: &str) -> Result<()> {
    let repo = repo_root_for_path(task_path)?;
    let task_arg = repo_relative_path(&repo, task_path)?;

    run_git_in(&repo, ["add", task_arg.as_str()]).context("failed to stage task file")?;

    if !has_staged_changes(&repo)? {
        return Ok(());
    }

    run_git_in(&repo, ["commit", "-m", message]).context("failed to commit task file")?;

    Ok(())
}

/// Stage and commit a set of agent-touched files inside `project_repo`.
///
/// `project_repo` should be the project root (or any path inside it); the repo
/// root is discovered automatically. Paths that fall outside the discovered
/// repo are skipped with a warning rather than aborting the commit, so a
/// careless agent recap can't break the run. Returns Ok(()) when there is
/// nothing to commit.
pub fn commit_agent_files(
    project_repo: &Path,
    files: &[PathBuf],
    message: &str,
) -> Result<()> {
    if files.is_empty() {
        return Ok(());
    }
    let repo = repo_root_for_path(project_repo)?;
    let absolute_repo = repo
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", repo.display()))?;

    let mut rel_paths: Vec<String> = Vec::with_capacity(files.len());
    for file in files {
        match repo_relative_path_lenient(&absolute_repo, file) {
            Ok(rel) => rel_paths.push(rel),
            Err(error) => {
                eprintln!(
                    "skipping file {} reported by agent: {error:#}",
                    file.display()
                );
            }
        }
    }

    if rel_paths.is_empty() {
        return Ok(());
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .arg("add")
        .arg("--")
        .args(&rel_paths)
        .output()
        .context("failed to stage agent files")?;

    if !output.status.success() {
        bail!(
            "git add failed; stderr: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    if !has_staged_changes(&repo)? {
        return Ok(());
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["commit", "-m", message])
        .output()
        .context("failed to commit agent files")?;

    if !output.status.success() {
        bail!(
            "git commit failed; stderr: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

pub fn commit_task_files(task_paths: &[&Path], message: &str) -> Result<()> {
    if task_paths.is_empty() {
        return Ok(());
    }
    let repo = repo_root_for_path(task_paths[0])?;

    let rel_paths: Vec<String> = task_paths
        .iter()
        .map(|p| repo_relative_path(&repo, p))
        .collect::<Result<_>>()?;

    let output = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .arg("add")
        .args(&rel_paths)
        .output()
        .context("failed to stage task files")?;

    if !output.status.success() {
        bail!(
            "git add failed; stderr: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    if !has_staged_changes(&repo)? {
        return Ok(());
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["commit", "-m", message])
        .output()
        .context("failed to commit task files")?;

    if !output.status.success() {
        bail!(
            "git commit failed; stderr: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

fn repo_root_for_path(path: &Path) -> Result<PathBuf> {
    let git_dir = if path.is_dir() {
        path
    } else {
        path.parent()
            .with_context(|| format!("path {} has no parent directory", path.display()))?
    };

    let output = Command::new("git")
        .arg("-C")
        .arg(git_dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .with_context(|| format!("failed to inspect git repository for {}", path.display()))?;

    if !output.status.success() {
        bail!(
            "{} is not inside a git repository; stderr: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let root = String::from_utf8(output.stdout).context("git repo path was not valid UTF-8")?;
    Ok(PathBuf::from(root.trim()))
}

fn ensure_same_repo(repo: &Path, path: &Path) -> Result<()> {
    let path_repo = repo_root_for_path(path)?;

    if path_repo != repo {
        bail!(
            "task update spans multiple git repositories: {} and {}",
            repo.display(),
            path_repo.display()
        );
    }

    Ok(())
}

fn repo_relative_path(repo: &Path, path: &Path) -> Result<String> {
    let absolute_path = path
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", path.display()))?;
    let absolute_repo = repo
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", repo.display()))?;
    let relative = absolute_path
        .strip_prefix(&absolute_repo)
        .with_context(|| {
            format!(
                "{} is not inside git repository {}",
                absolute_path.display(),
                absolute_repo.display()
            )
        })?;

    Ok(relative.display().to_string())
}

/// Like `repo_relative_path` but tolerates paths that no longer exist (e.g.
/// files that the agent deleted): when canonicalization fails, fall back to
/// the path as supplied. The repo argument must already be canonicalized.
fn repo_relative_path_lenient(canonical_repo: &Path, path: &Path) -> Result<String> {
    let absolute_path = match path.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()
                    .context("failed to read current directory")?
                    .join(path)
            }
        }
    };
    let relative = absolute_path
        .strip_prefix(canonical_repo)
        .with_context(|| {
            format!(
                "{} is not inside git repository {}",
                absolute_path.display(),
                canonical_repo.display()
            )
        })?;
    Ok(relative.display().to_string())
}

fn has_staged_changes(repo: &Path) -> Result<bool> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["diff", "--cached", "--quiet"])
        .output()
        .context("failed to check staged changes")?;
    // exit 0 = no diff, exit 1 = has diff
    Ok(!output.status.success())
}

fn run_git_in<const N: usize>(repo: &Path, args: [&str; N]) -> Result<()> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn init_test_repo(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "varda-git-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&root).expect("test root should be created");
        let status = Command::new("git")
            .arg("-C")
            .arg(&root)
            .arg("init")
            .arg("--quiet")
            .status()
            .expect("git init should run");
        assert!(status.success(), "git init failed");
        for (key, value) in [
            ("user.email", "varda-test@example.com"),
            ("user.name", "Varda Test"),
            ("commit.gpgsign", "false"),
        ] {
            let status = Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["config", "--local", key, value])
                .status()
                .expect("git config should run");
            assert!(status.success(), "git config {key} failed");
        }
        root
    }

    #[test]
    fn commit_agent_files_stages_and_commits_listed_paths() {
        let repo = init_test_repo("commit-agent-files");
        let inside = repo.join("changed.txt");
        std::fs::write(&inside, "hello").expect("file should be written");
        let outside = std::env::temp_dir().join(format!(
            "varda-git-outside-{}.txt",
            std::process::id()
        ));
        std::fs::write(&outside, "ignored").expect("outside file should be written");

        commit_agent_files(
            &repo,
            &[inside.clone(), outside.clone()],
            "agent commit",
        )
        .expect("commit should succeed");

        let log = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["log", "--oneline"])
            .output()
            .expect("git log should run");
        assert!(log.status.success());
        let stdout = String::from_utf8_lossy(&log.stdout);
        assert!(stdout.contains("agent commit"), "log was {stdout}");

        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn commit_agent_files_is_noop_when_list_is_empty() {
        let repo = init_test_repo("commit-agent-files-empty");
        commit_agent_files(&repo, &[], "should not commit").expect("noop should succeed");
        let log = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["log", "--oneline"])
            .output()
            .expect("git log should run");
        let stdout = String::from_utf8_lossy(&log.stdout);
        assert!(stdout.trim().is_empty(), "expected no commits, got {stdout}");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn repo_relative_path_handles_absolute_paths() {
        let root = std::env::temp_dir().join(format!("varda-git-{}", std::process::id()));
        let nested = root.join("operations/tasks/example.md");
        std::fs::create_dir_all(nested.parent().expect("nested path should have a parent"))
            .expect("test directories should be created");
        std::fs::write(&nested, "test").expect("test file should be written");

        let relative = repo_relative_path(&root, &nested).expect("path should become relative");

        assert_eq!(relative, "operations/tasks/example.md");
    }
}
