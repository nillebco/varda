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
pub fn commit_agent_files(project_repo: &Path, files: &[PathBuf], message: &str) -> Result<()> {
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

/// An isolated per-worker checkout: a dedicated git worktree with its own
/// `wip/<slug>` branch, checked out at [`WorkerCheckout::path`]. This is the
/// isolation primitive behind the orchestrate resident's fan-out (task #578):
/// each spawned worker edits files in its OWN worktree/branch rather than a
/// shared rw mount, so two workers touching the same file become two real
/// branches that surface a merge conflict at integration time instead of a
/// silent last-writer-wins clobber.
///
/// The worker itself never runs git in-box (agents are forbidden from
/// committing per `AGENTS.md`); it only edits files. The trusted host side
/// commits the worker's `files_touched` onto its branch with
/// [`commit_worker_changes`] and later integrates it with
/// [`merge_worker_branch`].
// NOTE (task #578): these worker-isolation primitives are the tested foundation
// for the orchestrate resident's merge-back loop. They are exercised by the unit
// tests below but not yet wired into `VardaSubtaskLauncher`/the resident control
// loop in `main.rs` — that wiring is the follow-up slice, and must land together
// with the resident-side commit+merge step so the live orchestrate path never
// regresses (isolated worktrees with no merge-back would strand worker changes).
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct WorkerCheckout {
    /// Host path of the worktree (mount this rw into the worker's sandbox).
    pub path: PathBuf,
    /// The `wip/<slug>` branch the worktree has checked out.
    pub branch: String,
}

/// The result of integrating a worker branch onto an integration branch.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    /// True when the merge applied cleanly (no conflicting hunks).
    pub clean: bool,
    /// Repo-relative paths that conflicted (empty on a clean merge). A
    /// non-empty list is the resolver's work queue.
    pub conflicted_files: Vec<String>,
}

/// Create an isolated worktree + `wip/<slug>` branch off `mother_repo`'s current
/// HEAD, materialized at `worktree_path`. The branch is unique per worker so
/// concurrent workers never share a ref. `worktree_path` must not already exist.
///
/// Returns the [`WorkerCheckout`] the resident mounts into the worker's box and
/// later harvests. The common gitdir stays with the mother repo, so all git
/// operations happen host-side where the whole repo is visible.
#[allow(dead_code)]
pub fn create_worker_worktree(
    mother_repo: &Path,
    slug: &str,
    worktree_path: &Path,
) -> Result<WorkerCheckout> {
    let repo = repo_root_for_path(mother_repo)?;
    let branch = format!("wip/{slug}");
    let path_str = worktree_path
        .to_str()
        .with_context(|| format!("worktree path {} is not valid UTF-8", worktree_path.display()))?;
    run_git_in(
        &repo,
        ["worktree", "add", "-b", branch.as_str(), path_str, "HEAD"],
    )
    .with_context(|| {
        format!(
            "failed to create worker worktree for branch {branch} at {}",
            worktree_path.display()
        )
    })?;
    Ok(WorkerCheckout {
        path: worktree_path.to_path_buf(),
        branch,
    })
}

/// Commit the worker's edited files onto its branch, host-side. `files` are the
/// paths the worker reported under `Files touched`; paths outside the worktree
/// are skipped with a warning (a careless recap can't break integration).
/// Returns `true` when a commit was created, `false` when there was nothing to
/// commit (an empty or no-op worker).
#[allow(dead_code)]
pub fn commit_worker_changes(
    checkout: &WorkerCheckout,
    files: &[PathBuf],
    message: &str,
) -> Result<bool> {
    if files.is_empty() {
        return Ok(false);
    }
    let worktree = &checkout.path;
    let canonical = worktree
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", worktree.display()))?;

    let mut rel_paths: Vec<String> = Vec::with_capacity(files.len());
    for file in files {
        match repo_relative_path_lenient(&canonical, file) {
            Ok(rel) => rel_paths.push(rel),
            Err(error) => {
                eprintln!(
                    "skipping file {} reported by worker: {error:#}",
                    file.display()
                );
            }
        }
    }
    if rel_paths.is_empty() {
        return Ok(false);
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .arg("add")
        .arg("--")
        .args(&rel_paths)
        .output()
        .context("failed to stage worker files")?;
    if !output.status.success() {
        bail!(
            "git add failed in worker worktree; stderr: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    if !has_staged_changes(worktree)? {
        return Ok(false);
    }

    run_git_in(worktree, ["commit", "-m", message])
        .context("failed to commit worker changes")?;
    Ok(true)
}

/// Attempt to integrate `worker_branch` into the branch currently checked out at
/// `integration_worktree` (typically a dedicated integration worktree on the
/// resident's integration branch). Never leaves a half-merged tree behind: on
/// conflict it records the conflicted paths and runs `git merge --abort`, so the
/// caller can route those files to a resolver worker (WORKFLOW.md step 5).
#[allow(dead_code)]
pub fn merge_worker_branch(
    integration_worktree: &Path,
    worker_branch: &str,
) -> Result<MergeOutcome> {
    let output = Command::new("git")
        .arg("-C")
        .arg(integration_worktree)
        .args(["merge", "--no-ff", "--no-edit", worker_branch])
        .output()
        .context("failed to start git merge")?;
    if output.status.success() {
        return Ok(MergeOutcome {
            clean: true,
            conflicted_files: Vec::new(),
        });
    }

    let unmerged = Command::new("git")
        .arg("-C")
        .arg(integration_worktree)
        .args(["diff", "--name-only", "--diff-filter=U"])
        .output()
        .context("failed to list conflicted files")?;
    let conflicted_files: Vec<String> = String::from_utf8_lossy(&unmerged.stdout)
        .lines()
        .map(str::to_owned)
        .filter(|l| !l.is_empty())
        .collect();

    // Abort so the integration worktree is left clean for the resolver flow.
    let _ = Command::new("git")
        .arg("-C")
        .arg(integration_worktree)
        .args(["merge", "--abort"])
        .output();

    Ok(MergeOutcome {
        clean: false,
        conflicted_files,
    })
}

/// Remove a worker worktree once its branch has been integrated (or abandoned).
/// Uses `--force` because the worktree may hold uncommitted scratch. The branch
/// ref is left intact for post-hoc review; prune it separately if desired.
#[allow(dead_code)]
pub fn remove_worker_worktree(mother_repo: &Path, checkout: &WorkerCheckout) -> Result<()> {
    let repo = repo_root_for_path(mother_repo)?;
    let path_str = checkout
        .path
        .to_str()
        .with_context(|| format!("worktree path {} is not valid UTF-8", checkout.path.display()))?;
    run_git_in(&repo, ["worktree", "remove", "--force", path_str])
        .with_context(|| format!("failed to remove worker worktree {}", checkout.path.display()))
}

/// Dependency-manifest files among `files`, for the G5 gate: manifest and
/// lockfile changes must be flagged for the human before any push. Matches by
/// file name so it is path-prefix agnostic (worktree vs mother checkout).
#[allow(dead_code)]
pub fn dependency_manifest_changes(files: &[PathBuf]) -> Vec<PathBuf> {
    const MANIFESTS: &[&str] = &[
        "Cargo.toml",
        "Cargo.lock",
        "package.json",
        "package-lock.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "go.mod",
        "go.sum",
        "requirements.txt",
        "pyproject.toml",
        "poetry.lock",
        "Gemfile",
        "Gemfile.lock",
    ];
    files
        .iter()
        .filter(|f| {
            f.file_name()
                .and_then(|n| n.to_str())
                .map(|n| MANIFESTS.contains(&n))
                .unwrap_or(false)
        })
        .cloned()
        .collect()
}

pub(crate) fn repo_root_for_path(path: &Path) -> Result<PathBuf> {
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
        let outside =
            std::env::temp_dir().join(format!("varda-git-outside-{}.txt", std::process::id()));
        std::fs::write(&outside, "ignored").expect("outside file should be written");

        commit_agent_files(&repo, &[inside.clone(), outside.clone()], "agent commit")
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
        assert!(
            stdout.trim().is_empty(),
            "expected no commits, got {stdout}"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    fn seed_commit(repo: &Path, name: &str, contents: &str) {
        std::fs::write(repo.join(name), contents).expect("seed file should be written");
        for args in [
            vec!["add", name],
            vec!["commit", "-m", "seed"],
        ] {
            let status = Command::new("git")
                .arg("-C")
                .arg(repo)
                .args(&args)
                .status()
                .expect("git should run");
            assert!(status.success(), "git {args:?} failed");
        }
    }

    #[test]
    fn worker_worktrees_isolate_branches_and_surface_conflicts() {
        let repo = init_test_repo("worker-isolation");
        seed_commit(&repo, "shared.txt", "base\n");

        // Two workers each get an isolated worktree + branch off the same HEAD.
        let a_path = repo.parent().unwrap().join(format!(
            "wt-a-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let b_path = repo.parent().unwrap().join(format!(
            "wt-b-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(1)
        ));
        let a = create_worker_worktree(&repo, "task-a", &a_path).expect("worktree a");
        let b = create_worker_worktree(&repo, "task-b", &b_path).expect("worktree b");
        assert_eq!(a.branch, "wip/task-a");
        assert_eq!(b.branch, "wip/task-b");

        // Both workers edit the SAME file in their own worktrees.
        std::fs::write(a.path.join("shared.txt"), "from-a\n").expect("write a");
        std::fs::write(b.path.join("shared.txt"), "from-b\n").expect("write b");
        assert!(
            commit_worker_changes(&a, &[a.path.join("shared.txt")], "worker a").expect("commit a")
        );
        assert!(
            commit_worker_changes(&b, &[b.path.join("shared.txt")], "worker b").expect("commit b")
        );

        // Integration worktree on a fresh integration branch.
        let int_path = repo.parent().unwrap().join(format!(
            "wt-int-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(2)
        ));
        let integration =
            create_worker_worktree(&repo, "integration", &int_path).expect("integration worktree");

        // First worker merges cleanly; the second conflicts on the same file.
        let first = merge_worker_branch(&integration.path, &a.branch).expect("merge a");
        assert!(first.clean, "first merge should be clean: {first:?}");
        let second = merge_worker_branch(&integration.path, &b.branch).expect("merge b");
        assert!(!second.clean, "second merge should conflict");
        assert_eq!(second.conflicted_files, vec!["shared.txt".to_owned()]);

        for wt in [&a, &b, &integration] {
            let _ = remove_worker_worktree(&repo, wt);
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn dependency_manifest_changes_flags_manifests_and_lockfiles() {
        let flagged = dependency_manifest_changes(&[
            PathBuf::from("/repo/src/main.rs"),
            PathBuf::from("/repo/Cargo.toml"),
            PathBuf::from("/repo/frontend/package.json"),
            PathBuf::from("/repo/Cargo.lock"),
            PathBuf::from("/repo/README.md"),
        ]);
        assert_eq!(
            flagged,
            vec![
                PathBuf::from("/repo/Cargo.toml"),
                PathBuf::from("/repo/frontend/package.json"),
                PathBuf::from("/repo/Cargo.lock"),
            ]
        );
        assert!(dependency_manifest_changes(&[PathBuf::from("/repo/src/lib.rs")]).is_empty());
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
