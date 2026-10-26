//! Git integration.

use std::collections::HashSet;
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
/// careless agent recap can't break the run.
///
/// Returns the full SHA of the commit it made, or `None` when there was nothing
/// to commit.
pub fn commit_agent_files(
    project_repo: &Path,
    files: &[PathBuf],
    message: &str,
) -> Result<Option<String>> {
    if files.is_empty() {
        return Ok(None);
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
        return Ok(None);
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
        return Ok(None);
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

    head_commit(&repo).map(Some)
}

/// The `origin` remote URL of the repository containing `repo`, if it has one.
pub fn origin_url(repo: &Path) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let url = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!url.is_empty()).then_some(url)
}

/// Repo-relative paths with ANY local modification at `worktree` — tracked or
/// untracked (`git status --porcelain=v1 -z`), entries with no trailing
/// newline filtered out. Unlike [`worktree_dirty_paths`] this deliberately
/// includes untracked files: a newly created file is exactly the kind of
/// change an agent must report, so a snapshot that dropped it could never
/// catch an under-reported new file (#816).
///
/// Uses the NUL-delimited `-z` form rather than the human-readable porcelain
/// text: the latter can't be split-and-trimmed reliably, since rename/copy
/// records render as `R  old -> new` (a single "path" slice would then be the
/// whole ` -> ` composite, matching neither the old nor the new file) and
/// paths containing spaces, non-ASCII bytes, or other special characters get
/// quoted and C-escaped unless `core.quotepath=false`. With `-z`, each record
/// is `XY<space><path>\0`, except a rename/copy record (`X` or `Y` is `R` or
/// `C`) which is `XY<space><path>\0<origPath>\0` — two NUL-terminated fields,
/// neither quoted nor escaped. For a rename, both the new and the old path
/// are emitted, so a renamed-and-still-uncommitted file is recognized under
/// either name.
pub fn dirty_paths(worktree: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["status", "--porcelain=v1", "-z"])
        .output()
        .context("failed to check worktree status")?;
    if !output.status.success() {
        bail!(
            "git status failed in {}; stderr: {}",
            worktree.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut fields = stdout.split('\0').filter(|f| !f.is_empty());
    let mut paths = Vec::new();
    while let Some(record) = fields.next() {
        if record.len() < 3 {
            continue;
        }
        let status_code = &record[..2];
        let path = &record[3..];
        paths.push(path.to_owned());
        if (status_code.contains('R') || status_code.contains('C'))
            && let Some(orig_path) = fields.next()
        {
            paths.push(orig_path.to_owned());
        }
    }
    Ok(paths)
}

/// Repo-relative paths that became dirty at `worktree` between a prior
/// snapshot (`before`, from [`dirty_paths`]) and now, that are NOT present in
/// `reported` — the discrepancy signal behind #816 ("varda commits the
/// agent's `Files touched` list verbatim with no completeness check").
///
/// `before` paths are excluded even if they are still dirty afterward: an
/// operator's unrelated in-flight edit predates the run and must never be
/// swept in or flagged (AGENTS.md is explicit that unrelated pre-existing
/// changes must be left alone). Only paths the run itself newly dirtied, and
/// that the agent did not list, come back as a discrepancy.
pub fn unreported_changes(
    worktree: &Path,
    before: &[String],
    reported: &[PathBuf],
) -> Result<Vec<String>> {
    let absolute_repo = worktree
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", worktree.display()))?;
    let after = dirty_paths(worktree)?;
    let before_set: HashSet<&str> = before.iter().map(String::as_str).collect();
    let reported_set: HashSet<String> = reported
        .iter()
        .filter_map(|f| repo_relative_path_lenient(&absolute_repo, f).ok())
        .collect();

    Ok(after
        .into_iter()
        .filter(|p| !before_set.contains(p.as_str()))
        .filter(|p| !reported_set.contains(p))
        .collect())
}

pub fn commit_task_files(task_paths: &[&Path], message: &str) -> Result<()> {
    commit_task_files_within(task_paths, message, None)
}

/// [`commit_task_files`] with an enforced wall-clock bound across all of its git
/// invocations: a stuck hook or signing step is killed (with its process group)
/// at the deadline and the commit fails instead of blocking the caller.
pub fn commit_task_files_within(
    task_paths: &[&Path],
    message: &str,
    timeout: Option<std::time::Duration>,
) -> Result<()> {
    commit_task_files_until(
        task_paths,
        message,
        timeout.map(|t| std::time::Instant::now() + t),
    )
}

/// Marker error: a bounded git invocation hit its deadline and was stopped.
#[derive(Debug)]
struct GitTimedOut;

impl std::fmt::Display for GitTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("git timed out and was stopped")
    }
}

impl std::error::Error for GitTimedOut {}

/// [`commit_task_files_within`] against an absolute `deadline` that the caller
/// may share with other bounded git calls (e.g. repo discovery).
pub fn commit_task_files_until(
    task_paths: &[&Path],
    message: &str,
    deadline: Option<std::time::Instant>,
) -> Result<()> {
    if task_paths.is_empty() {
        return Ok(());
    }
    let repo = repo_root_for_path_until(task_paths[0], deadline)?;
    let lock = git_path_until(&repo, "index.lock", deadline).ok();

    let result = commit_in_repo(&repo, task_paths, message, deadline);
    if let Err(e) = &result {
        // Varda never deletes a git lock: it cannot prove the lock is still ours.
        // Git removes its own lockfiles on SIGTERM; if one survives, say so.
        if e.chain().any(|c| c.is::<GitTimedOut>()) {
            if let Some(lock) = lock.filter(|l| l.exists()) {
                return Err(anyhow::anyhow!(
                    "{e:#}; {} may be stale and must be removed by hand once no git process is running",
                    lock.display()
                ));
            }
        }
    }
    result
}

fn commit_in_repo(
    repo: &Path,
    task_paths: &[&Path],
    message: &str,
    deadline: Option<std::time::Instant>,
) -> Result<()> {
    let rel_paths: Vec<String> = task_paths
        .iter()
        .map(|p| repo_relative_path(repo, p))
        .collect::<Result<_>>()?;

    let mut add = Command::new("git");
    add.arg("-C").arg(repo).arg("add").args(&rel_paths);
    let output = run_bounded(add, deadline).context("failed to stage task files")?;

    if !output.status.success() {
        bail!(
            "git add failed; stderr: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let mut staged = Command::new("git");
    staged.arg("-C").arg(repo).args(["diff", "--cached", "--quiet"]);
    let output = run_bounded(staged, deadline).context("failed to check staged changes")?;
    // exit 0 = no diff, exit 1 = has diff
    if output.status.success() {
        return Ok(());
    }

    let mut commit = Command::new("git");
    commit.arg("-C").arg(repo).args(["commit", "-m", message]);
    let output = run_bounded(commit, deadline).context("failed to commit task files")?;

    if !output.status.success() {
        bail!(
            "git commit failed; stderr: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

/// Resolve `git rev-parse --git-path <name>` inside `repo` (bounded).
fn git_path_until(
    repo: &Path,
    name: &str,
    deadline: Option<std::time::Instant>,
) -> Result<PathBuf> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(repo).args(["rev-parse", "--git-path", name]);
    let output = run_bounded(cmd, deadline)?;
    if !output.status.success() {
        bail!("git rev-parse --git-path {name} failed");
    }
    let rel = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    Ok(if rel.is_absolute() { rel } else { repo.join(rel) })
}

/// A Job Object holding a spawned git and every process it starts, so a timeout
/// can kill hooks and filters too (Windows has no process groups to signal).
/// No kill-on-close: a background `git gc --auto` left by a command that
/// finished in time must survive the handle closing.
#[cfg(windows)]
struct ProcessTreeJob(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl ProcessTreeJob {
    /// Spawn `cmd` suspended, put it in a fresh job, then resume it, so not even
    /// its first descendant can start outside the job. Failing to establish
    /// the job degrades (with a warning) to killing git alone on timeout;
    /// failing to resume kills the suspended child and errors.
    fn spawn(cmd: &mut Command) -> Result<(std::process::Child, Option<Self>)> {
        use std::os::windows::process::CommandExt as _;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
        let mut child = cmd.spawn()?;
        let job = match Self::assign(&child) {
            Ok(job) => Some(job),
            Err(e) => {
                eprintln!(
                    "warning: cannot put git in a job object ({e}); a timeout will not stop its hooks"
                );
                None
            }
        };
        if let Err(e) = resume_process(child.id()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow::Error::from(e).context("failed to resume suspended git"));
        }
        Ok((child, job))
    }

    fn assign(child: &std::process::Child) -> std::io::Result<Self> {
        use std::os::windows::io::AsRawHandle as _;
        use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
        // SAFETY: plain Win32 calls on a handle we own and the live child's handle.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let job = Self(job);
            if AssignProcessToJobObject(job.0, child.as_raw_handle()) == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(job)
        }
    }

    fn terminate(&self) {
        // SAFETY: `self.0` is a job handle we own until drop.
        let ok = unsafe { windows_sys::Win32::System::JobObjects::TerminateJobObject(self.0, 1) };
        if ok == 0 {
            eprintln!(
                "warning: failed to terminate git's job object ({}); its hooks may outlive the timeout",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// Resume every thread of a process created suspended (only its main thread,
/// in practice). Errors if none was resumed.
#[cfg(windows)]
fn resume_process(pid: u32) -> std::io::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};
    // SAFETY: plain Win32 calls; every handle opened here is closed here.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let mut entry: THREADENTRY32 = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut resumed = 0;
        let mut failure = None;
        let mut more = Thread32First(snapshot, &mut entry) != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                let thread = OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID);
                if thread.is_null() || ResumeThread(thread) == u32::MAX {
                    failure = Some(std::io::Error::last_os_error());
                } else {
                    resumed += 1;
                }
                if !thread.is_null() {
                    CloseHandle(thread);
                }
            }
            more = Thread32Next(snapshot, &mut entry) != 0;
        }
        CloseHandle(snapshot);
        match (resumed, failure) {
            (0, Some(e)) => Err(e),
            (0, None) => Err(std::io::Error::other(format!("no thread of process {pid} found to resume"))),
            _ => Ok(()),
        }
    }
}

#[cfg(windows)]
impl Drop for ProcessTreeJob {
    fn drop(&mut self) {
        // SAFETY: closing the handle we own, once.
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

/// Run `cmd` to completion; with a `deadline`, poll it and, when the deadline
/// passes, SIGTERM its process group (so git can drop `index.lock` and hooks
/// die), wait a short grace period, then SIGKILL (Windows: terminate its Job
/// Object). Pipe readers are abandoned, not joined, on timeout: a grandchild
/// may still hold the pipe.
fn run_bounded(
    mut cmd: Command,
    deadline: Option<std::time::Instant>,
) -> Result<std::process::Output> {
    use std::process::Stdio;
    let Some(deadline) = deadline else {
        return Ok(cmd.output()?);
    };
    if std::time::Instant::now() >= deadline {
        return Err(GitTimedOut.into());
    }
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(not(windows))]
    let mut child = cmd.spawn()?;
    #[cfg(windows)]
    let (mut child, job) = ProcessTreeJob::spawn(&mut cmd)?;
    fn drain(pipe: Option<impl std::io::Read + Send + 'static>) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
            }
            let _ = tx.send(buf);
        });
        rx
    }
    let stdout = drain(child.stdout.take());
    let stderr = drain(child.stderr.take());
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            #[cfg(unix)]
            // SAFETY: plain kill(2) on the child's own process group.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGTERM);
            }
            // Nothing was signalled on Windows, so there is nothing to wait for.
            #[cfg(unix)]
            {
                let grace_end = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while std::time::Instant::now() < grace_end {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
            // Always escalate on the GROUP: a descendant may ignore SIGTERM even
            // though the leader exited.
            #[cfg(unix)]
            // SAFETY: plain kill(2) on the child's own process group (ESRCH ignored).
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            #[cfg(windows)]
            if let Some(job) = &job {
                job.terminate();
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(GitTimedOut.into());
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let grace = std::time::Duration::from_secs(2);
    Ok(std::process::Output {
        status,
        stdout: stdout.recv_timeout(grace).unwrap_or_default(),
        stderr: stderr.recv_timeout(grace).unwrap_or_default(),
    })
}

/// Stage and commit the removal of task files that have ALREADY been deleted from
/// disk. Unlike [`commit_task_files`], this never canonicalizes the (now-missing)
/// paths — it derives each repo-relative path from its still-present parent
/// directory, then `git add`s it so the deletion is recorded in the index.
pub fn commit_task_deletions(deleted_paths: &[&Path], message: &str) -> Result<()> {
    if deleted_paths.is_empty() {
        return Ok(());
    }
    let repo = repo_root_for_path(deleted_paths[0])?;
    let absolute_repo = repo
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", repo.display()))?;

    let rel_paths: Vec<String> = deleted_paths
        .iter()
        .map(|p| deleted_repo_relative_path(&absolute_repo, p))
        .collect::<Result<_>>()?;

    // Only paths git actually tracks can be `git add`ed once they are gone from
    // the working tree. An untracked-and-now-deleted file — e.g. a recap that was
    // never committed into `~/.varda` — makes `git add` abort the whole batch
    // with "pathspec did not match any files", stranding even the tracked
    // deletions alongside it. Stage just the tracked subset; if nothing here was
    // ever tracked there is simply nothing to record.
    let tracked = tracked_paths(&repo, &rel_paths)?;
    if tracked.is_empty() {
        return Ok(());
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .arg("add")
        .args(&tracked)
        .output()
        .context("failed to stage deleted task files")?;

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
        .context("failed to commit deleted task files")?;

    if !output.status.success() {
        bail!(
            "git commit failed; stderr: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

/// Repo-relative path for a file that may no longer exist: canonicalize the
/// (still-present) parent directory and re-attach the file name, so a deleted
/// file still maps into the repo without `canonicalize` failing on the missing
/// leaf.
fn deleted_repo_relative_path(absolute_repo: &Path, path: &Path) -> Result<String> {
    let parent = path
        .parent()
        .with_context(|| format!("path {} has no parent directory", path.display()))?;
    let file_name = path
        .file_name()
        .with_context(|| format!("path {} has no file name", path.display()))?;
    let absolute_parent = parent
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", parent.display()))?;
    let absolute_path = absolute_parent.join(file_name);
    let relative = absolute_path.strip_prefix(absolute_repo).with_context(|| {
        format!(
            "{} is not inside git repository {}",
            absolute_path.display(),
            absolute_repo.display()
        )
    })?;
    Ok(relative.to_string_lossy().into_owned())
}

/// Filter `rel_paths` down to the subset git currently tracks, returning git's
/// own view of each path. `git ls-files` reads the INDEX, not the working tree,
/// so a file already removed from disk still resolves as long as it was
/// committed — exactly what a deletion commit needs. Returning git's canonical
/// paths (rather than intersecting our strings) keeps quoting/normalization in
/// git's hands. Untracked paths simply do not appear in the output.
fn tracked_paths(repo: &Path, rel_paths: &[String]) -> Result<Vec<String>> {
    if rel_paths.is_empty() {
        return Ok(Vec::new());
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["ls-files", "-z", "--"])
        .args(rel_paths)
        .output()
        .context("failed to list tracked files")?;

    if !output.status.success() {
        bail!(
            "git ls-files failed; stderr: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let tracked = output
        .stdout
        .split(|&byte| byte == 0)
        .filter(|segment| !segment.is_empty())
        .map(|segment| String::from_utf8_lossy(segment).into_owned())
        .collect();
    Ok(tracked)
}

/// An isolated per-worker checkout: a self-contained git clone with its own
/// `wip/<slug>` branch, checked out at [`WorkerCheckout::path`]. This is the
/// isolation primitive behind the orchestrate resident's fan-out (task #578):
/// each spawned worker edits files in its OWN worktree/branch rather than a
/// shared rw mount, so two workers touching the same file become two real
/// branches that surface a merge conflict at integration time instead of a
/// silent last-writer-wins clobber.
///
/// The clone has its own `.git` directory, so ordinary read-only git commands
/// work inside the worker sandbox. The trusted host side commits the worker's
/// `files_touched` onto its branch with
/// [`commit_worker_changes`] and later integrates it with
/// [`merge_worker_branch`].
// NOTE (task #578): these worker-isolation primitives are the tested foundation
// for the orchestrate resident's merge-back loop. They are exercised by the unit
// tests below but not yet wired into `VardaSubtaskLauncher`/the resident control
// loop in `main.rs` — that wiring is the follow-up slice, and must land together
// with the resident-side commit+merge step so the live orchestrate path never
// regresses (isolated worktrees with no merge-back would strand worker changes).
#[allow(dead_code)]
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkerCheckout {
    /// Host path of the clone (mount this rw into the worker's sandbox).
    pub path: PathBuf,
    /// The `wip/<slug>` branch the clone has checked out.
    pub branch: String,
    /// The mother repo's branch that was checked out when this worktree was
    /// created off its `HEAD` (task #833). This is the merge-back TARGET,
    /// captured once at launch time rather than re-read from the mother's
    /// `HEAD` at merge time — so a branch switch in the mother mid-run cannot
    /// silently redirect where this worker's changes land. `None` when the
    /// mother was in detached-`HEAD` state at creation time.
    pub target_branch: Option<String>,
    /// The mother's exact `HEAD` commit at launch time, recorded only when
    /// `target_branch` is `None` (the mother was detached). There is no
    /// branch name to compare against in that case, so the commit is the only
    /// signal that can catch the mother moving out from under a detached
    /// launch before merge-back (follow-up to task #833).
    pub target_head: Option<String>,
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

/// Create an isolated clone + `wip/<slug>` branch off `mother_repo`'s current
/// HEAD, materialized at `worktree_path`. `--no-hardlinks` keeps its object
/// database independent from the mother. `worktree_path` must not already exist.
///
/// Returns the [`WorkerCheckout`] the resident mounts into the worker's box and
/// later harvests. Its real `.git` directory is available inside the sandbox.
#[allow(dead_code)]
pub fn create_worker_worktree(
    mother_repo: &Path,
    slug: &str,
    worktree_path: &Path,
) -> Result<WorkerCheckout> {
    let repo = repo_root_for_path(mother_repo)?;
    // Successful workers are independent clones, but older/crashed versions may
    // have left linked-worktree administration behind in the mother. Prune it
    // before allocating a new checkout so stale `.git/worktrees` entries do not
    // accumulate or block reuse of a path removed out-of-band.
    run_git_in(&repo, ["worktree", "prune"])
        .context("failed to prune stale worker worktree metadata")?;
    // Capture the mother's branch NOW, before the clone below runs, and pin the
    // clone to it explicitly with `--branch` rather than letting `git clone`
    // implicitly follow whatever the mother's `HEAD` resolves to at that later
    // moment. This is the merge-back target recorded on the checkout (task
    // #833): both what the worker branches from and where its branch is later
    // allowed to merge are fixed here, closing the gap a `git checkout` in the
    // mother between these two steps could otherwise open.
    let target_branch = current_branch(&repo)?;
    // When the mother is detached there is no branch name to pin the merge-back
    // target to, so the exact commit is recorded instead — the only thing that
    // can later prove the mother has (or hasn't) moved.
    //
    // `current_branch` and `head_commit` above are two separate `git`
    // subprocess calls, so there is a theoretical residual race if the
    // mother's `HEAD` moves between these two specific calls (not the
    // multi-second, human-paced race between capture and clone that task #833
    // is actually about — this window is two sequential local subprocesses
    // apart by milliseconds). Accepted as impractically narrow to close.
    let target_head = if target_branch.is_none() {
        Some(head_commit(&repo)?)
    } else {
        None
    };
    let branch = format!("wip/{slug}");
    let mut clone_cmd = Command::new("git");
    clone_cmd.args(["clone", "--no-hardlinks"]);
    if let Some(base) = &target_branch {
        clone_cmd.args(["--branch", base.as_str()]);
    }
    let output = clone_cmd
        .arg(&repo)
        .arg(worktree_path)
        .output()
        .context("failed to start worker clone")?;
    if !output.status.success() {
        bail!(
            "git clone failed for {} at {}; stderr: {}",
            repo.display(),
            worktree_path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    copy_git_identity(&repo, worktree_path)?;
    // The `--branch` flag above pins the named-branch case, but a detached
    // mother has no branch name to pin to: the plain `git clone` that just ran
    // simply followed whatever the mother's `HEAD` pointed at when the clone
    // itself executed, which can differ from `target_head` captured moments
    // earlier if the mother moved in between (the exact race task #833 item 4
    // asks to close). Explicitly checking out the captured commit here — before
    // branching off it below — makes the worker's base provably `target_head`
    // rather than whatever the clone happened to land on.
    if let Some(head) = &target_head {
        run_git_in(worktree_path, ["checkout", head.as_str()]).with_context(|| {
            format!(
                "failed to pin detached worker clone at {} to captured HEAD {head}",
                worktree_path.display()
            )
        })?;
    }
    run_git_in(worktree_path, ["checkout", "-b", branch.as_str()]).with_context(|| {
        format!(
            "failed to create worker branch {branch} in clone at {}",
            worktree_path.display()
        )
    })?;
    Ok(WorkerCheckout {
        path: worktree_path.to_path_buf(),
        branch,
        target_branch,
        target_head,
    })
}

/// The branch checked out at `repo`, or `None` when `HEAD` is detached.
/// `git rev-parse --abbrev-ref HEAD` prints the literal string `HEAD` in the
/// detached case, which is not a real branch name, so that is mapped to
/// `None` rather than trusted as one.
pub fn current_branch(repo: &Path) -> Result<Option<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .context("failed to determine current branch")?;
    if !output.status.success() {
        bail!(
            "git rev-parse --abbrev-ref HEAD failed in {}; stderr: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let branch = String::from_utf8(output.stdout)
        .context("branch name was not valid UTF-8")?
        .trim()
        .to_owned();
    if branch.is_empty() || branch == "HEAD" {
        Ok(None)
    } else {
        Ok(Some(branch))
    }
}

/// The commit `repo`'s `HEAD` currently resolves to, as a full SHA.
fn head_commit(repo: &Path) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .context("failed to determine HEAD commit")?;
    if !output.status.success() {
        bail!(
            "git rev-parse HEAD failed in {}; stderr: {}",
            repo.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8(output.stdout)
        .context("HEAD commit was not valid UTF-8")?
        .trim()
        .to_owned())
}

fn copy_git_identity(source_repo: &Path, clone: &Path) -> Result<()> {
    for (key, fallback) in [
        ("user.name", "Varda Worker"),
        ("user.email", "varda-worker@localhost"),
    ] {
        let output = Command::new("git")
            .arg("-C")
            .arg(source_repo)
            .args(["config", "--get", key])
            .output()
            .with_context(|| format!("failed to read {key} from mother repository"))?;
        let value = if output.status.success() {
            String::from_utf8(output.stdout)
                .with_context(|| format!("mother repository {key} was not valid UTF-8"))?
                .trim()
                .to_owned()
        } else {
            fallback.to_owned()
        };
        run_git_in(clone, ["config", key, value.as_str()])
            .with_context(|| format!("failed to configure {key} in worker clone"))?;
    }
    Ok(())
}

/// The outcome of attempting to commit a worker's `files_touched` onto its
/// branch. Distinguishing [`CommitOutcome::AlreadyCommitted`] from
/// [`CommitOutcome::NothingToCommit`] is what makes integration resumable
/// (task #708): a worker with `AlreadyCommitted` — for example because a
/// prior call already committed it before a later step failed — still has a
/// commit that needs to be merged, whereas `NothingToCommit` (an empty or
/// no-op worker) genuinely has nothing for the merge step to do.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitOutcome {
    /// `files` was empty, or every path fell outside the worktree — there was
    /// never anything for this worker to commit.
    NothingToCommit,
    /// A new commit was created by this call.
    CommittedNow,
    /// The reported files matched the worktree's current `HEAD` already (most
    /// often because an earlier call already committed them) — no new commit
    /// was needed, but a commit exists on the branch.
    AlreadyCommitted,
}

#[allow(dead_code)]
impl CommitOutcome {
    /// Whether the worker branch carries a commit for its `files_touched`,
    /// whether created just now or already present. Merge should be attempted
    /// whenever this is `true` — "already committed" must not be read as
    /// "already integrated".
    pub fn has_commit(self) -> bool {
        !matches!(self, CommitOutcome::NothingToCommit)
    }

    fn as_str(self) -> &'static str {
        match self {
            CommitOutcome::NothingToCommit => "nothing_to_commit",
            CommitOutcome::CommittedNow => "committed_now",
            CommitOutcome::AlreadyCommitted => "already_committed",
        }
    }
}

impl std::fmt::Display for CommitOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Commit the worker's edited files onto its branch, host-side. `files` are the
/// paths the worker reported under `Files touched`; paths outside the worktree
/// are skipped with a warning (a careless recap can't break integration).
/// Idempotent: calling this again after a successful commit reports
/// [`CommitOutcome::AlreadyCommitted`] rather than silently doing nothing, so
/// the caller can still resume the merge step (#708).
#[allow(dead_code)]
pub fn commit_worker_changes(
    checkout: &WorkerCheckout,
    files: &[PathBuf],
    message: &str,
) -> Result<CommitOutcome> {
    if files.is_empty() {
        return Ok(CommitOutcome::NothingToCommit);
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
        return Ok(CommitOutcome::NothingToCommit);
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
        return Ok(CommitOutcome::AlreadyCommitted);
    }

    run_git_in(worktree, ["commit", "-m", message]).context("failed to commit worker changes")?;
    Ok(CommitOutcome::CommittedNow)
}

/// Attempt to integrate `worker_branch` into the branch currently checked out at
/// `integration_worktree` (typically a dedicated integration worktree on the
/// resident's integration branch). Never leaves a half-merged tree behind: on
/// conflict it records the conflicted paths and runs `git merge --abort`, so the
/// caller can route those files to a resolver worker (WORKFLOW.md step 5).
///
/// A merge can also fail without producing any unmerged paths — e.g. a pre-merge
/// hook rejection or a refusal before any tree merge. That is not a content
/// conflict a resolver can work through, so it is surfaced as an `Err` rather
/// than a `MergeOutcome` with an empty conflict list (which the caller would
/// otherwise be unable to distinguish from a clean merge's empty list). If the
/// `git merge --abort` cleanup itself fails, that too is an `Err` including
/// stderr, so a half-merged tree is never silently left behind.
#[allow(dead_code)]
pub fn merge_worker_branch(
    integration_worktree: &Path,
    worker: &WorkerCheckout,
) -> Result<MergeOutcome> {
    let worker_branch = &worker.branch;

    // Refuse LOUDLY rather than silently merging onto whatever branch happens
    // to be checked out now (task #833): the worker's merge-back target was
    // fixed at launch time, so if the integration worktree has moved off it
    // since — an operator `git checkout` elsewhere in the same working copy —
    // merging here would land the worker's changes on the wrong branch with no
    // signal that anything went wrong.
    match &worker.target_branch {
        Some(expected) => {
            let actual = current_branch(integration_worktree)?;
            if actual.as_deref() != Some(expected.as_str()) {
                let actual_desc = actual.as_deref().unwrap_or("a detached HEAD");
                bail!(
                    "refusing to merge {worker_branch} into {}: it was launched against branch \
                     '{expected}', but the integration worktree is now on {actual_desc}. \
                     Check out '{expected}' there and retry, or resolve this manually.",
                    integration_worktree.display()
                );
            }
        }
        None => {
            // Launched against a detached HEAD: there is no branch name to
            // compare, so "unchanged" means still detached at the exact same
            // commit. Anything else — moved to a named branch, or moved to a
            // different commit while still detached — is the identical
            // silent-redirection bug reopened, so a bare `None == None` on
            // `target_branch` alone must never be treated as a match.
            let expected_head = worker
                .target_head
                .as_deref()
                .expect("detached-launch checkout must record its HEAD commit");
            let actual_branch = current_branch(integration_worktree)?;
            let actual_head = head_commit(integration_worktree)?;
            let unchanged = actual_branch.is_none() && actual_head == expected_head;
            if !unchanged {
                let actual_desc = match &actual_branch {
                    Some(branch) => format!("branch '{branch}' (commit {actual_head})"),
                    None => format!("a detached HEAD at commit {actual_head}"),
                };
                bail!(
                    "refusing to merge {worker_branch} into {}: it was launched against a \
                     detached HEAD at commit {expected_head}, but the integration worktree is \
                     now on {actual_desc}. Check out commit {expected_head} there and retry, or \
                     resolve this manually.",
                    integration_worktree.display()
                );
            }
        }
    }

    let fetch = Command::new("git")
        .arg("-C")
        .arg(integration_worktree)
        .arg("fetch")
        .arg(&worker.path)
        .arg(worker_branch)
        .output()
        .context("failed to fetch worker branch")?;
    if !fetch.status.success() {
        bail!(
            "git fetch of {worker_branch} from {} failed; stderr: {}",
            worker.path.display(),
            String::from_utf8_lossy(&fetch.stderr).trim()
        );
    }
    let output = Command::new("git")
        .arg("-C")
        .arg(integration_worktree)
        .args(["merge", "--no-ff", "--no-edit", "FETCH_HEAD"])
        .output()
        .context("failed to start git merge")?;
    if output.status.success() {
        return Ok(MergeOutcome {
            clean: true,
            conflicted_files: Vec::new(),
        });
    }
    let merge_stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();

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

    // Only abort when a merge is actually in progress (MERGE_HEAD set). Both a
    // content conflict and a pre-merge hook rejection leave MERGE_HEAD, whereas
    // a merge that refuses before touching the index (e.g. unrelated histories,
    // dirty tree) does not — aborting that would spuriously fail with
    // "no merge to abort".
    let merge_in_progress = Command::new("git")
        .arg("-C")
        .arg(integration_worktree)
        .args(["rev-parse", "-q", "--verify", "MERGE_HEAD"])
        .output()
        .context("failed to check for an in-progress merge")?
        .status
        .success();
    if merge_in_progress {
        // Abort so the integration worktree is left clean for the resolver flow.
        // A failed abort must never be swallowed: it can leave a half-merged
        // tree, so surface stderr to the caller.
        let abort = Command::new("git")
            .arg("-C")
            .arg(integration_worktree)
            .args(["merge", "--abort"])
            .output()
            .context("failed to run git merge --abort")?;
        if !abort.status.success() {
            bail!(
                "git merge --abort failed after a failed merge of {worker_branch}; \
                 the integration worktree may be left half-merged; stderr: {}",
                String::from_utf8_lossy(&abort.stderr).trim()
            );
        }
    }

    // A merge that fails without unmerged paths is not a content conflict the
    // resolver can work through (e.g. a pre-merge hook rejection). Report it as
    // an error so the caller never mistakes an empty conflict list for a clean
    // or resolvable merge.
    if conflicted_files.is_empty() {
        bail!(
            "git merge of {worker_branch} failed without unmerged paths \
             (not a content conflict); stderr: {merge_stderr}"
        );
    }

    Ok(MergeOutcome {
        clean: false,
        conflicted_files,
    })
}

/// Remove a worker clone once its branch has been integrated (or abandoned).
/// The branch is contained in the clone and is removed with it. `delete_branch`
/// is retained for caller compatibility; worker branches are never created in
/// the mother's ref namespace.
#[allow(dead_code)]
pub fn remove_worker_worktree(
    mother_repo: &Path,
    checkout: &WorkerCheckout,
    _delete_branch: bool,
) -> Result<()> {
    repo_root_for_path(mother_repo)?;
    std::fs::remove_dir_all(&checkout.path)
        .with_context(|| format!("failed to remove worker clone {}", checkout.path.display()))
}

/// Whether a worker clone holds nothing that would be lost by deleting it
/// (#1239): its tree is clean (no tracked edits AND no untracked files — a
/// worker's new files are untracked until the host commits them) and its
/// `HEAD` commit is already contained in the mother's merge-back target (the
/// recorded `target_branch`, else `target_head`, else the mother's `HEAD`).
/// A no-op worker whose `HEAD` is still the base it was cloned from counts as
/// integrated: nothing it produced is outstanding. A commit the mother has
/// never fetched (the branch was never merged) is not an ancestor, so the
/// clone is reported as NOT integrated. Root-run teardown deletes only clones
/// for which this returns `Ok(true)`.
pub fn worker_checkout_is_integrated(mother_repo: &Path, checkout: &WorkerCheckout) -> Result<bool> {
    let status = Command::new("git")
        .arg("-C")
        .arg(&checkout.path)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .output()
        .context("failed to check worker clone status")?;
    if !status.status.success() {
        bail!(
            "git status failed in worker clone {}; stderr: {}",
            checkout.path.display(),
            String::from_utf8_lossy(&status.stderr).trim()
        );
    }
    if !String::from_utf8_lossy(&status.stdout).trim().is_empty() {
        return Ok(false);
    }

    let worker_head = head_commit(&checkout.path)?;
    let repo = repo_root_for_path(mother_repo)?;
    let target = checkout
        .target_branch
        .clone()
        .or_else(|| checkout.target_head.clone())
        .unwrap_or_else(|| "HEAD".to_owned());
    let ancestor = Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["merge-base", "--is-ancestor", worker_head.as_str(), target.as_str()])
        .output()
        .context("failed to check whether the worker branch is integrated")?;
    // Exit 0 = ancestor; 1 = not an ancestor; anything else (e.g. 128 for an
    // object the mother never fetched) also means the work is not in the mother.
    Ok(ancestor.status.success())
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

/// A worker's harvested output, ready for host-side integration: the isolated
/// checkout it worked in ([`create_worker_worktree`]) plus the `files_touched`
/// paths it reported in its recap. The recap text is untrusted (WORKFLOW.md G4),
/// so only these two structured fields cross into the merge-back loop.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct WorkerHarvest {
    /// The isolated worktree/branch the worker edited.
    pub checkout: WorkerCheckout,
    /// Paths the worker reported under `Files touched`. Paths outside its
    /// worktree are skipped by [`commit_worker_changes`].
    pub files_touched: Vec<PathBuf>,
}

/// Per-worker result of the merge-back loop, for the resident to act on.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct WorkerIntegration {
    /// The worker's `wip/<slug>` branch.
    pub branch: String,
    /// Whether/how the worker's `files_touched` ended up committed onto its
    /// branch this call. `NothingToCommit` = empty/no-op worker; the other two
    /// variants both carry a commit, so merge is attempted for either (#708:
    /// "already committed" must not be read as "already integrated").
    pub commit_outcome: CommitOutcome,
    /// Merge result onto the integration branch. `None` when `commit_outcome`
    /// is `NothingToCommit` (merge not attempted). A non-`clean` outcome
    /// carries the conflicted paths the resident routes to a resolver
    /// (WORKFLOW.md step 5).
    pub merge: Option<MergeOutcome>,
    /// Dependency-manifest files this worker touched — the G5 flag the resident
    /// must surface before any human push.
    pub dependency_manifests: Vec<PathBuf>,
}

/// Repo-relative paths with local modifications to TRACKED content at
/// `worktree` — staged changes, unstaged edits, deletions, or an in-progress
/// merge/rebase (`git status --porcelain --untracked-files=no`, entries with
/// no trailing newline filtered out). Untracked files are deliberately
/// excluded: an integration worktree can permanently carry untracked local
/// tooling (e.g. `.claude/`, `.mcp.json`) that AGENTS.md says does not belong
/// in the repo, and flagging those as "dirty" would make every future
/// integration refuse forever. Used as the pre-flight check before
/// integration touches anything (#708 item 4): a dirty integration worktree
/// is exactly what let a concurrent agent's uncommitted `src/config.rs` edit
/// turn a mid-flow `git merge` failure into a stranded half-finished
/// integration.
fn worktree_dirty_paths(worktree: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .context("failed to check integration worktree status")?;
    if !output.status.success() {
        bail!(
            "git status failed in integration worktree; stderr: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        // porcelain lines are "XY <path>"; keep the path only.
        .map(|l| l.get(3..).unwrap_or(l).to_owned())
        .collect())
}

/// Run the WORKFLOW.md step-5 merge-back loop host-side over a wave of harvested
/// workers: for each, commit its `files_touched` onto its own `wip/<slug>`
/// branch ([`commit_worker_changes`]) and merge that branch onto the branch
/// checked out at `integration_worktree` ([`merge_worker_branch`]).
///
/// Resumable (#708): each worker's commit and merge steps are evaluated
/// independently. A worker whose branch already carries its commit — because
/// an earlier call committed it before a later step failed — still gets its
/// merge attempted; "already committed" is never read as "already integrated".
///
/// Refuses up front, before committing anything, if `integration_worktree`
/// itself has local modifications ([`worktree_dirty_paths`]): letting `git
/// merge` discover that mid-flow (after a worker's commit has already landed)
/// is exactly the half-finished state #708 exists to prevent. The refusal
/// names the offending paths so the caller can act on it, rather than a
/// generic git failure the caller cannot see the cause of.
///
/// This is otherwise deterministic glue, not policy: it never resolves
/// conflicts, never pushes, and never aborts the wave on a *content* conflict —
/// a conflicting merge is recorded (aborted so the integration tree stays
/// clean) and reported as the resolver's work queue, so later workers in the
/// wave still integrate. A merge that fails *without* unmerged paths (e.g. a
/// pre-merge hook rejection) is a hard error the primitive surfaces, and it
/// propagates here. Each worker's dependency-manifest touches are flagged (G5)
/// for the resident to relay to the human before any push (G3).
#[allow(dead_code)]
pub fn integrate_worker_branches(
    integration_worktree: &Path,
    workers: &[WorkerHarvest],
    commit_message_prefix: &str,
) -> Result<Vec<WorkerIntegration>> {
    let dirty = worktree_dirty_paths(integration_worktree)?;
    if !dirty.is_empty() {
        bail!(
            "integration worktree {} has local modifications and cannot be integrated into: {}",
            integration_worktree.display(),
            dirty.join(", ")
        );
    }

    let mut integrations = Vec::with_capacity(workers.len());
    for worker in workers {
        let branch = worker.checkout.branch.clone();
        let commit_outcome = commit_worker_changes(
            &worker.checkout,
            &worker.files_touched,
            &format!("{commit_message_prefix} {branch}"),
        )?;
        let merge = if commit_outcome.has_commit() {
            Some(merge_worker_branch(integration_worktree, &worker.checkout)?)
        } else {
            None
        };
        integrations.push(WorkerIntegration {
            branch,
            commit_outcome,
            merge,
            dependency_manifests: dependency_manifest_changes(&worker.files_touched),
        });
    }
    Ok(integrations)
}

pub(crate) fn repo_root_for_path(path: &Path) -> Result<PathBuf> {
    repo_root_for_path_until(path, None)
}

/// [`repo_root_for_path`] bounded by an absolute `deadline` (see [`run_bounded`]).
pub(crate) fn repo_root_for_path_until(
    path: &Path,
    deadline: Option<std::time::Instant>,
) -> Result<PathBuf> {
    let git_dir = if path.is_dir() {
        path
    } else {
        path.parent()
            .with_context(|| format!("path {} has no parent directory", path.display()))?
    };

    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(git_dir).args(["rev-parse", "--show-toplevel"]);
    let output = run_bounded(cmd, deadline)
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

    #[cfg(unix)]
    #[test]
    fn commit_task_files_within_kills_a_hanging_hook() {
        use std::os::unix::fs::PermissionsExt;
        let root = init_test_repo("hook-timeout");
        let hook = root.join(".git/hooks/pre-commit");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, "#!/bin/sh\nsleep 30\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        let file = root.join("a.md");
        std::fs::write(&file, "x").unwrap();
        let started = std::time::Instant::now();
        let res = commit_task_files_within(
            &[file.as_path()],
            "msg",
            Some(std::time::Duration::from_secs(1)),
        );
        assert!(res.is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(unix)]
    #[test]
    fn commit_task_files_within_survives_a_hanging_clean_filter() {
        let root = init_test_repo("filter-timeout");
        let run = |args: &[&str]| {
            let st = Command::new("git").arg("-C").arg(&root).args(args).status().unwrap();
            assert!(st.success());
        };
        run(&["config", "--local", "filter.slow.clean", "sleep 30; cat"]);
        std::fs::write(root.join(".gitattributes"), "*.md filter=slow\n").unwrap();
        let file = root.join("a.md");
        std::fs::write(&file, "x").unwrap();
        let started = std::time::Instant::now();
        let res = commit_task_files_within(
            &[file.as_path()],
            "msg",
            Some(std::time::Duration::from_secs(1)),
        );
        let err = res.expect_err("the stuck filter must time out");
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10), "{:?}", started.elapsed());
        // Git drops its own index.lock on SIGTERM; Varda never deletes it.
        assert!(!root.join(".git/index.lock").exists(), "git should clean its lock on SIGTERM");
        // Staging works again once the filter is gone.
        std::fs::remove_file(root.join(".gitattributes")).unwrap();
        commit_task_files_within(&[file.as_path()], "msg", Some(std::time::Duration::from_secs(20)))
            .expect("a later commit must succeed");
        std::fs::remove_dir_all(root).ok();
    }

    /// Windows: a process started by the child outlives it unless the whole Job
    /// Object is terminated.
    #[cfg(windows)]
    #[test]
    fn run_bounded_kills_the_process_tree_on_windows() {
        let dir = std::env::temp_dir().join(format!("varda-job-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        // The path travels in the environment, never interpolated into the
        // script (`-Command` joins trailing args into the script text).
        let mut cmd = Command::new("powershell");
        cmd.args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$p = Start-Process -PassThru -WindowStyle Hidden -FilePath ping \
             -ArgumentList '-n','120','127.0.0.1'; \
             Set-Content -LiteralPath $env:VARDA_TEST_PIDFILE -Value $p.Id; \
             Start-Sleep -Seconds 120",
        ])
        .env("VARDA_TEST_PIDFILE", &pidfile);
        let started = std::time::Instant::now();
        // PowerShell is slow to start; leave it time to record the pid.
        let deadline = started + std::time::Duration::from_secs(15);
        let err = run_bounded(cmd, Some(deadline)).expect_err("must time out");
        assert!(err.is::<GitTimedOut>());
        assert!(started.elapsed() < std::time::Duration::from_secs(30));
        let pid: u32 = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .expect("descendant never wrote its pid before the deadline (machine contention?)");
        // CSV rows: "Image Name","PID",... — match the PID column exactly.
        let alive = || {
            let out = Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
                .output()
                .expect("tasklist runs");
            assert!(
                out.status.success(),
                "tasklist failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .any(|row| row.split(',').nth(1) == Some(&format!("\"{pid}\"")))
        };
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while alive() {
            assert!(std::time::Instant::now() < end, "descendant {pid} survived");
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn run_bounded_kills_a_descendant_that_ignores_sigterm() {
        let dir = std::env::temp_dir().join(format!("varda-grp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        // The path travels as `$1`, never interpolated into the script.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sh -c 'trap \"\" TERM; echo $$ > \"$1\"; sleep 60' inner \"$1\" & sleep 60")
            .arg("outer")
            .arg(&pidfile);
        let started = std::time::Instant::now();
        // Generous enough for the descendant to start and write its pid even
        // on a contended machine; the assertion below names that case.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let err = run_bounded(cmd, Some(deadline)).expect_err("must time out");
        assert!(err.is::<GitTimedOut>());
        assert!(started.elapsed() < std::time::Duration::from_secs(15));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .expect("descendant never wrote its pid before the deadline (machine contention?)");
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match descendant_state(pid) {
                ProbeState::Gone | ProbeState::Zombie => break,
                ProbeState::Running => {}
                ProbeState::Unknown => panic!("cannot determine whether pid {pid} is alive"),
            }
            assert!(std::time::Instant::now() < end, "descendant {pid} survived");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    enum ProbeState {
        Gone,
        Zombie,
        Running,
        Unknown,
    }

    /// A killed process can linger as a zombie (signal 0 still succeeds) when
    /// its reaper is a container PID 1 that never waits, so a zombie counts as
    /// killed. Linux reads `/proc`; elsewhere `ps`, whose failure is Unknown,
    /// never "dead".
    #[cfg(unix)]
    fn descendant_state(pid: i32) -> ProbeState {
        // SAFETY: signal 0 only probes for existence.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return ProbeState::Gone;
        }
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            // Format: `pid (comm) S ...`; comm may contain spaces/parens.
            return match stat.rsplit_once(')').and_then(|(_, rest)| rest.trim_start().chars().next()) {
                Some('Z') | Some('X') => ProbeState::Zombie,
                Some(_) => ProbeState::Running,
                None => ProbeState::Unknown,
            };
        }
        match Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output() {
            Ok(o) if o.status.success() => {
                let stat = String::from_utf8_lossy(&o.stdout);
                match stat.trim().chars().next() {
                    Some('Z') => ProbeState::Zombie,
                    Some(_) => ProbeState::Running,
                    None => ProbeState::Gone,
                }
            }
            // `ps -p` also fails when the pid vanished after the probe above,
            // so only a still-present pid makes the failure inconclusive.
            // SAFETY: signal 0 only probes for existence.
            _ if unsafe { libc::kill(pid, 0) } != 0 => ProbeState::Gone,
            _ => ProbeState::Unknown,
        }
    }

    #[cfg(unix)]
    #[test]
    fn repo_root_for_path_until_honors_an_expired_deadline() {
        let root = init_test_repo("discovery-deadline");
        let err = repo_root_for_path_until(&root, Some(std::time::Instant::now()))
            .expect_err("an already-expired deadline must not hang or succeed");
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        assert_eq!(
            repo_root_for_path_until(&root, None).unwrap().canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
        std::fs::remove_dir_all(root).ok();
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
    fn commit_agent_files_returns_head_and_origin_url_reads_remote() {
        let repo = init_test_repo("commit-agent-files-hash");
        let file = repo.join("changed.txt");
        std::fs::write(&file, "hello").expect("file should be written");
        let sha = commit_agent_files(&repo, &[file.clone()], "agent commit")
            .expect("commit should succeed")
            .expect("a commit was made");
        assert_eq!(sha, head_commit(&repo).unwrap());
        // Nothing left to commit: no hash.
        assert_eq!(
            commit_agent_files(&repo, &[file], "again").unwrap(),
            None
        );

        assert_eq!(origin_url(&repo), None);
        let status = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["remote", "add", "origin", "https://dev.azure.com/o/p/_git/r"])
            .status()
            .expect("git remote should run");
        assert!(status.success());
        assert_eq!(
            origin_url(&repo).as_deref(),
            Some("https://dev.azure.com/o/p/_git/r")
        );
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

    #[test]
    fn commit_task_deletions_ignores_untracked_paths() {
        let repo = init_test_repo("delete-untracked");
        // A tracked task file plus an untracked recap — the exact `varda task
        // delete` shape where recaps live uncommitted in `~/.varda`.
        seed_commit(&repo, "task.md", "tracked\n");
        let untracked = repo.join("recap.md");
        std::fs::write(&untracked, "never committed").expect("recap should be written");

        // Both are removed from disk before the deletion commit, as the command
        // does.
        std::fs::remove_file(repo.join("task.md")).expect("task should be removed");
        std::fs::remove_file(&untracked).expect("recap should be removed");

        commit_task_deletions(&[&repo.join("task.md"), &untracked], "Delete task")
            .expect("deletion should not fail on the untracked recap");

        // The tracked deletion is recorded; the untracked path is silently skipped.
        let log = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["log", "--oneline"])
            .output()
            .expect("git log should run");
        let stdout = String::from_utf8_lossy(&log.stdout);
        assert!(stdout.contains("Delete task"), "log was {stdout}");

        let tracked = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["ls-files"])
            .output()
            .expect("git ls-files should run");
        let files = String::from_utf8_lossy(&tracked.stdout);
        assert!(!files.contains("task.md"), "task.md should be deleted: {files}");

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn commit_task_deletions_is_noop_when_nothing_tracked() {
        let repo = init_test_repo("delete-all-untracked");
        seed_commit(&repo, "keep.md", "anchor\n");
        let untracked = repo.join("recap.md");
        std::fs::write(&untracked, "never committed").expect("recap should be written");
        std::fs::remove_file(&untracked).expect("recap should be removed");

        commit_task_deletions(&[&untracked], "Delete task").expect("noop should succeed");

        // Only the seed commit exists — no empty "Delete task" commit was made.
        let log = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["log", "--oneline"])
            .output()
            .expect("git log should run");
        let stdout = String::from_utf8_lossy(&log.stdout);
        assert!(!stdout.contains("Delete task"), "should not commit: {stdout}");
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
    fn worker_clones_have_real_gitdirs_isolate_branches_and_surface_conflicts() {
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
        assert!(
            a.path.join(".git").is_dir(),
            "worker clone needs a real .git directory"
        );
        assert!(
            b.path.join(".git").is_dir(),
            "worker clone needs a real .git directory"
        );

        // Both workers edit the SAME file in their own worktrees.
        std::fs::write(a.path.join("shared.txt"), "from-a\n").expect("write a");
        std::fs::write(b.path.join("shared.txt"), "from-b\n").expect("write b");
        assert!(
            commit_worker_changes(&a, &[a.path.join("shared.txt")], "worker a")
                .expect("commit a")
                .has_commit()
        );
        assert!(
            commit_worker_changes(&b, &[b.path.join("shared.txt")], "worker b")
                .expect("commit b")
                .has_commit()
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
        // Merge-back targets the mother's branch, not the integration clone's
        // own `wip/integration` branch (task #833) — check it out first.
        let base_branch = current_branch(&repo)
            .expect("current branch")
            .expect("mother repo should be on a branch");
        run_git_in(&integration.path, ["checkout", base_branch.as_str()])
            .expect("checkout base branch in integration worktree");

        // First worker merges cleanly; the second conflicts on the same file.
        let first = merge_worker_branch(&integration.path, &a).expect("merge a");
        assert!(first.clean, "first merge should be clean: {first:?}");
        let second = merge_worker_branch(&integration.path, &b).expect("merge b");
        assert!(!second.clean, "second merge should conflict");
        assert_eq!(second.conflicted_files, vec!["shared.txt".to_owned()]);

        for wt in [&a, &b, &integration] {
            let _ = remove_worker_worktree(&repo, wt, false);
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    fn unique_worktree_path(repo: &Path, label: &str) -> PathBuf {
        repo.parent().unwrap().join(format!(
            "wt-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ))
    }

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "-q", "--verify", &format!("refs/heads/{branch}")])
            .output()
            .expect("git rev-parse should run")
            .status
            .success()
    }

    #[test]
    fn remove_worker_worktree_removes_clone_without_creating_mother_refs() {
        let repo = init_test_repo("worktree-branch-delete");
        seed_commit(&repo, "file.txt", "base\n");

        // Worker branches belong only to their clones, never to the mother.
        let kept_path = unique_worktree_path(&repo, "keep");
        let kept = create_worker_worktree(&repo, "keep", &kept_path).expect("worktree keep");
        assert!(!branch_exists(&repo, &kept.branch));
        remove_worker_worktree(&repo, &kept, false).expect("remove keep");
        assert!(!kept.path.exists(), "clone dir should be gone");
        assert!(
            !branch_exists(&repo, &kept.branch),
            "worker branch must not leak into mother refs"
        );

        // delete_branch = true: worktree and branch both gone.
        let gone_path = unique_worktree_path(&repo, "gone");
        let gone = create_worker_worktree(&repo, "gone", &gone_path).expect("worktree gone");
        assert!(!branch_exists(&repo, &gone.branch));
        remove_worker_worktree(&repo, &gone, true).expect("remove gone");
        assert!(!gone.path.exists(), "clone dir should be gone");
        assert!(
            !branch_exists(&repo, &gone.branch),
            "worker branch must remain absent from mother refs"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn merge_worker_branch_errors_when_merge_fails_without_conflicts() {
        let repo = init_test_repo("merge-no-conflict-failure");
        seed_commit(&repo, "file.txt", "base\n");

        // A worker branch with a clean, non-conflicting change.
        let worker_path = unique_worktree_path(&repo, "worker");
        let worker = create_worker_worktree(&repo, "worker", &worker_path).expect("worktree");
        std::fs::write(worker.path.join("other.txt"), "worker\n").expect("write");
        assert!(commit_worker_changes(&worker, &[worker.path.join("other.txt")], "worker")
            .expect("commit worker")
            .has_commit());

        // Integration clone whose git dir carries a pre-merge-commit hook that
        // rejects the merge. The trees merge cleanly (no unmerged paths) but the
        // merge still fails — this must be surfaced as an error, not a conflict.
        let int_path = unique_worktree_path(&repo, "int");
        let integration = create_worker_worktree(&repo, "integration", &int_path).expect("int wt");
        // Merge-back targets the mother's branch, not the integration clone's
        // own `wip/integration` branch (task #833) — check it out first.
        let base_branch = current_branch(&repo)
            .expect("current branch")
            .expect("mother repo should be on a branch");
        run_git_in(&integration.path, ["checkout", base_branch.as_str()])
            .expect("checkout base branch in integration worktree");
        // Install pre-merge-commit in this clone's independent git directory.
        let common_dir = String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(&integration.path)
                .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
                .output()
                .expect("rev-parse git common dir")
                .stdout,
        )
        .expect("utf8 git dir");
        let hooks_dir = PathBuf::from(common_dir.trim()).join("hooks");
        std::fs::create_dir_all(&hooks_dir).expect("create hooks dir");
        let hook = hooks_dir.join("pre-merge-commit");
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").expect("write hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755))
                .expect("chmod hook");
        }

        let err = merge_worker_branch(&integration.path, &worker)
            .expect_err("merge should error when it fails without unmerged paths");
        assert!(
            err.to_string().contains("without unmerged paths"),
            "unexpected error: {err}"
        );

        // The abort cleanup ran: no merge should be left in progress.
        let merge_in_progress = Command::new("git")
            .arg("-C")
            .arg(&integration.path)
            .args(["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .output()
            .expect("rev-parse MERGE_HEAD")
            .status
            .success();
        assert!(!merge_in_progress, "merge should have been aborted");

        for wt in [&worker, &integration] {
            let _ = remove_worker_worktree(&repo, wt, false);
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    fn rev_parse(repo: &std::path::Path, rev: &str) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", rev])
            .output()
            .expect("git rev-parse should run");
        assert!(output.status.success(), "git rev-parse {rev} failed");
        String::from_utf8(output.stdout)
            .expect("rev-parse output should be UTF-8")
            .trim()
            .to_owned()
    }

    #[test]
    fn create_worker_worktree_pins_detached_launch_to_captured_head() {
        let repo = init_test_repo("detached-head-pin");
        seed_commit(&repo, "first.txt", "first\n");
        // Sanity check for the explicit post-clone checkout in
        // `create_worker_worktree`'s detached-launch path — NOT a reproduction
        // of the race it defends against. A plain `git clone` of a source
        // already sitting on a detached HEAD lands on that same commit with no
        // intervening mutation, so this test would still pass with the
        // explicit checkout removed; it cannot deterministically exercise the
        // narrow window between capturing `target_head` and the clone
        // subprocess actually running, which would need a synchronization
        // point (e.g. a git hook) to inject a concurrent HEAD move. Accepted
        // as the same class of residual as the `current_branch`/`head_commit`
        // TOCTOU documented above `target_head`'s capture in
        // `create_worker_worktree`.
        let detached_commit = rev_parse(&repo, "HEAD");
        seed_commit(&repo, "second.txt", "second\n");
        let branch_tip = rev_parse(&repo, "HEAD");
        assert_ne!(
            detached_commit, branch_tip,
            "test setup should advance the named branch past the detached commit"
        );

        let status = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["checkout", "--detach", &detached_commit])
            .status()
            .expect("git checkout --detach should run");
        assert!(status.success(), "detaching mother HEAD failed");

        let worker_path = unique_worktree_path(&repo, "detached-worker");
        let worker =
            create_worker_worktree(&repo, "detached-worker", &worker_path).expect("worktree");

        assert!(
            worker.target_branch.is_none(),
            "a detached mother must not record a target_branch"
        );
        assert_eq!(
            worker.target_head.as_deref(),
            Some(detached_commit.as_str()),
            "target_head must record the mother's detached commit"
        );

        let worker_head = rev_parse(&worker.path, "HEAD");
        assert_eq!(
            worker_head, detached_commit,
            "the wip/<slug> branch must be created from the captured target_head, \
             not the clone's default-branch tip"
        );

        let _ = remove_worker_worktree(&repo, &worker, false);
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn merge_worker_branch_refuses_when_target_branch_has_moved_since_launch() {
        let repo = init_test_repo("merge-target-branch-moved");
        seed_commit(&repo, "file.txt", "base\n");
        let base_branch = current_branch(&repo)
            .expect("current branch")
            .expect("mother repo should be on a branch");

        // Worker launched against the mother's named branch.
        let worker_path = unique_worktree_path(&repo, "worker");
        let worker = create_worker_worktree(&repo, "worker", &worker_path).expect("worktree");
        assert_eq!(worker.target_branch.as_deref(), Some(base_branch.as_str()));
        std::fs::write(worker.path.join("worker-change.txt"), "from-worker\n").expect("write");
        assert!(
            commit_worker_changes(&worker, &[worker.path.join("worker-change.txt")], "worker")
                .expect("commit worker")
                .has_commit()
        );

        // Separate integration worktree, checked out to the same target branch
        // first — matching the pattern used elsewhere in this file — then
        // moved onto a different branch, simulating an operator `git checkout`
        // in the integration worktree between launch and merge-back.
        let int_path = unique_worktree_path(&repo, "int-target");
        let integration =
            create_worker_worktree(&repo, "int-target", &int_path).expect("integration worktree");
        run_git_in(&integration.path, ["checkout", base_branch.as_str()])
            .expect("checkout base branch in integration worktree");
        run_git_in(&integration.path, ["checkout", "-b", "other-branch"])
            .expect("checkout other-branch in integration worktree");

        let head_before = head_commit(&integration.path).expect("head before merge attempt");

        let err = merge_worker_branch(&integration.path, &worker)
            .expect_err("merge must refuse when the integration worktree has moved branches");
        let message = err.to_string();
        assert!(
            message.contains(&base_branch) && message.contains("other-branch"),
            "error should name both branches: {message}"
        );

        // Nothing changed: still on the new branch, same commit, no merge in
        // progress, and the worker's change never landed.
        assert_eq!(
            current_branch(&integration.path).expect("current branch"),
            Some("other-branch".to_owned())
        );
        assert_eq!(
            head_commit(&integration.path).expect("head after merge attempt"),
            head_before
        );
        let merge_in_progress = Command::new("git")
            .arg("-C")
            .arg(&integration.path)
            .args(["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .output()
            .expect("rev-parse MERGE_HEAD")
            .status
            .success();
        assert!(!merge_in_progress, "refused merge must not start a merge");
        assert!(
            !integration.path.join("worker-change.txt").exists(),
            "worker's change must not have merged"
        );

        for wt in [&worker, &integration] {
            let _ = remove_worker_worktree(&repo, wt, false);
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn merge_worker_branch_refuses_when_launched_detached_and_integration_now_on_a_branch() {
        let repo = init_test_repo("merge-detached-integration-moved");
        seed_commit(&repo, "file.txt", "base\n");
        let detached_head = head_commit(&repo).expect("head commit");
        run_git_in(&repo, ["checkout", detached_head.as_str()]).expect("detach mother HEAD");

        // Worker launched against a detached mother HEAD.
        let worker_path = unique_worktree_path(&repo, "worker");
        let worker = create_worker_worktree(&repo, "worker", &worker_path).expect("worktree");
        assert_eq!(worker.target_branch, None);
        assert_eq!(worker.target_head.as_deref(), Some(detached_head.as_str()));
        std::fs::write(worker.path.join("worker-change.txt"), "from-worker\n").expect("write");
        assert!(
            commit_worker_changes(&worker, &[worker.path.join("worker-change.txt")], "worker")
                .expect("commit worker")
                .has_commit()
        );

        // Separate integration worktree, detached at that same commit first —
        // matching the pattern used elsewhere in this file — then moved onto a
        // named branch, simulating an operator `git checkout` in the
        // integration worktree between launch and merge-back.
        let int_path = unique_worktree_path(&repo, "int-target");
        let integration =
            create_worker_worktree(&repo, "int-target", &int_path).expect("integration worktree");
        run_git_in(&integration.path, ["checkout", detached_head.as_str()])
            .expect("detach integration worktree at the captured HEAD");
        run_git_in(&integration.path, ["checkout", "-b", "new-branch"])
            .expect("checkout new-branch in integration worktree");

        let err = merge_worker_branch(&integration.path, &worker)
            .expect_err("merge must refuse when the integration worktree moved to a branch");
        let message = err.to_string();
        assert!(
            message.contains(&detached_head) && message.contains("new-branch"),
            "error should name both the detached commit and the new branch: {message}"
        );

        // Nothing changed: still on the new branch, same commit, no merge in
        // progress, and the worker's change never landed.
        assert_eq!(
            current_branch(&integration.path).expect("current branch"),
            Some("new-branch".to_owned())
        );
        assert_eq!(
            head_commit(&integration.path).expect("head after merge attempt"),
            detached_head
        );
        let merge_in_progress = Command::new("git")
            .arg("-C")
            .arg(&integration.path)
            .args(["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .output()
            .expect("rev-parse MERGE_HEAD")
            .status
            .success();
        assert!(!merge_in_progress, "refused merge must not start a merge");
        assert!(
            !integration.path.join("worker-change.txt").exists(),
            "worker's change must not have merged"
        );

        for wt in [&worker, &integration] {
            let _ = remove_worker_worktree(&repo, wt, false);
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn integrate_worker_branches_merges_clean_flags_manifests_and_queues_conflicts() {
        let repo = init_test_repo("integrate-worker-branches");
        seed_commit(&repo, "shared.txt", "base\n");

        // Worker A: disjoint new file + a dependency manifest (G5).
        let a_path = unique_worktree_path(&repo, "int-a");
        let a = create_worker_worktree(&repo, "int-a", &a_path).expect("worktree a");
        std::fs::write(a.path.join("a.txt"), "from-a\n").expect("write a");
        std::fs::write(a.path.join("Cargo.toml"), "[package]\n").expect("write manifest a");

        // Worker B: edits the shared file — will conflict once A's version merges.
        let b_path = unique_worktree_path(&repo, "int-b");
        let b = create_worker_worktree(&repo, "int-b", &b_path).expect("worktree b");
        std::fs::write(b.path.join("shared.txt"), "from-b\n").expect("write b");

        // Worker C: reports no files — a no-op worker that must not commit or merge.
        let c_path = unique_worktree_path(&repo, "int-c");
        let c = create_worker_worktree(&repo, "int-c", &c_path).expect("worktree c");

        // Integration worktree. Seed a conflicting edit on shared.txt so B collides.
        let int_path = unique_worktree_path(&repo, "int-target");
        let integration =
            create_worker_worktree(&repo, "int-target", &int_path).expect("integration worktree");
        // Merge-back targets the mother's branch, not the integration clone's
        // own `wip/int-target` branch (task #833) — check it out first.
        let base_branch = current_branch(&repo)
            .expect("current branch")
            .expect("mother repo should be on a branch");
        run_git_in(&integration.path, ["checkout", base_branch.as_str()])
            .expect("checkout base branch in integration worktree");
        std::fs::write(integration.path.join("shared.txt"), "from-integration\n")
            .expect("write integration");
        assert!(commit_worker_changes(
            &integration,
            &[integration.path.join("shared.txt")],
            "integration base",
        )
        .expect("commit integration base")
        .has_commit());

        let workers = vec![
            WorkerHarvest {
                checkout: a.clone(),
                files_touched: vec![a.path.join("a.txt"), a.path.join("Cargo.toml")],
            },
            WorkerHarvest {
                checkout: b.clone(),
                files_touched: vec![b.path.join("shared.txt")],
            },
            WorkerHarvest {
                checkout: c.clone(),
                files_touched: vec![],
            },
        ];

        let results = integrate_worker_branches(&integration.path, &workers, "merge worker")
            .expect("integration loop should not error on a content conflict");
        assert_eq!(results.len(), 3);

        // A: clean merge + G5 manifest flag.
        assert_eq!(results[0].branch, "wip/int-a");
        assert!(results[0].commit_outcome.has_commit());
        assert!(results[0].merge.as_ref().expect("a merged").clean);
        assert_eq!(
            results[0].dependency_manifests,
            vec![a.path.join("Cargo.toml")]
        );

        // B: conflict surfaced as the resolver's work queue, wave not aborted.
        assert_eq!(results[1].branch, "wip/int-b");
        assert!(results[1].commit_outcome.has_commit());
        let b_merge = results[1].merge.as_ref().expect("b merge attempted");
        assert!(!b_merge.clean);
        assert_eq!(b_merge.conflicted_files, vec!["shared.txt".to_owned()]);
        assert!(results[1].dependency_manifests.is_empty());

        // C: no files → no commit, no merge.
        assert_eq!(results[2].branch, "wip/int-c");
        assert_eq!(results[2].commit_outcome, CommitOutcome::NothingToCommit);
        assert!(results[2].merge.is_none());

        // The aborted conflict left the integration tree clean (A's merge intact).
        let merge_in_progress = Command::new("git")
            .arg("-C")
            .arg(&integration.path)
            .args(["rev-parse", "-q", "--verify", "MERGE_HEAD"])
            .output()
            .expect("rev-parse MERGE_HEAD")
            .status
            .success();
        assert!(!merge_in_progress, "conflict should have been aborted");

        // Prove the index/worktree is truly clean, not just that MERGE_HEAD is gone.
        let porcelain = Command::new("git")
            .arg("-C")
            .arg(&integration.path)
            .args(["status", "--porcelain"])
            .output()
            .expect("git status --porcelain");
        assert!(
            porcelain.stdout.is_empty(),
            "aborted conflict should leave a clean working tree, got: {}",
            String::from_utf8_lossy(&porcelain.stdout)
        );

        for wt in [&a, &b, &c, &integration] {
            let _ = remove_worker_worktree(&repo, wt, false);
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn integrate_worker_branches_refuses_dirty_integration_worktree_before_committing() {
        let repo = init_test_repo("integrate-dirty-tracked");
        std::fs::create_dir_all(repo.join("src")).expect("create src dir");
        seed_commit(&repo, "src/config.rs", "base\n");

        let worker_path = unique_worktree_path(&repo, "worker");
        let worker = create_worker_worktree(&repo, "worker", &worker_path).expect("worktree");
        std::fs::write(worker.path.join("a.txt"), "from-worker\n").expect("write a");

        let int_path = unique_worktree_path(&repo, "int-target");
        let integration =
            create_worker_worktree(&repo, "int-target", &int_path).expect("integration worktree");
        // A concurrent agent's uncommitted edit to a TRACKED file — the exact
        // shape of the original #708 incident.
        std::fs::write(integration.path.join("src/config.rs"), "concurrent edit\n")
            .expect("write concurrent edit");

        let workers = vec![WorkerHarvest {
            checkout: worker.clone(),
            files_touched: vec![worker.path.join("a.txt")],
        }];

        let err = integrate_worker_branches(&integration.path, &workers, "merge worker")
            .expect_err("dirty integration worktree must be refused before committing");
        assert!(
            err.to_string().contains("src/config.rs"),
            "unexpected error: {err}"
        );

        // The pre-flight check ran before any worker commit was made: the
        // worker's `a.txt` change is still unstaged in its worktree.
        let worker_status = Command::new("git")
            .arg("-C")
            .arg(&worker.path)
            .args(["status", "--porcelain"])
            .output()
            .expect("git status should run");
        assert!(
            String::from_utf8_lossy(&worker_status.stdout).contains("a.txt"),
            "worker should not have been committed when the pre-flight check refuses"
        );

        for wt in [&worker, &integration] {
            let _ = remove_worker_worktree(&repo, wt, false);
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn integrate_worker_branches_ignores_untracked_files_when_checking_dirty_worktree() {
        let repo = init_test_repo("integrate-dirty-untracked");
        seed_commit(&repo, "shared.txt", "base\n");

        let worker_path = unique_worktree_path(&repo, "worker");
        let worker = create_worker_worktree(&repo, "worker", &worker_path).expect("worktree");
        std::fs::write(worker.path.join("a.txt"), "from-worker\n").expect("write a");

        let int_path = unique_worktree_path(&repo, "int-target");
        let integration =
            create_worker_worktree(&repo, "int-target", &int_path).expect("integration worktree");
        // Merge-back targets the mother's branch, not the integration clone's
        // own `wip/int-target` branch (task #833) — check it out first.
        let base_branch = current_branch(&repo)
            .expect("current branch")
            .expect("mother repo should be on a branch");
        run_git_in(&integration.path, ["checkout", base_branch.as_str()])
            .expect("checkout base branch in integration worktree");
        // Untracked local tooling files (e.g. `.claude/`, `.mcp.json`) that
        // AGENTS.md says do not belong in the repo — must never block
        // integration, unlike a modification to a tracked file.
        std::fs::write(integration.path.join(".mcp.json"), "{}\n")
            .expect("write untracked file");

        let workers = vec![WorkerHarvest {
            checkout: worker.clone(),
            files_touched: vec![worker.path.join("a.txt")],
        }];

        let results = integrate_worker_branches(&integration.path, &workers, "merge worker")
            .expect("an untracked file in the integration worktree must not be refused");
        assert_eq!(results.len(), 1);
        assert!(results[0].commit_outcome.has_commit());
        assert!(results[0].merge.as_ref().expect("merged").clean);

        for wt in [&worker, &integration] {
            let _ = remove_worker_worktree(&repo, wt, false);
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
    fn unreported_changes_flags_an_unlisted_modified_file() {
        let repo = init_test_repo("unreported-modified");
        seed_commit(&repo, "a.txt", "base\n");
        seed_commit(&repo, "b.txt", "base\n");

        let before = dirty_paths(&repo).expect("snapshot before run");
        assert!(before.is_empty(), "freshly seeded repo should be clean");

        // Agent modifies both a.txt and b.txt but only reports b.txt.
        std::fs::write(repo.join("a.txt"), "changed\n").expect("write a");
        std::fs::write(repo.join("b.txt"), "changed\n").expect("write b");

        let unreported =
            unreported_changes(&repo, &before, &[repo.join("b.txt")]).expect("diff should run");
        assert_eq!(unreported, vec!["a.txt".to_owned()]);

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn unreported_changes_flags_an_unlisted_new_file() {
        let repo = init_test_repo("unreported-new-file");
        seed_commit(&repo, "seed.txt", "base\n");

        let before = dirty_paths(&repo).expect("snapshot before run");

        // Agent creates b.txt (untracked) but only reports a.txt.
        std::fs::write(repo.join("a.txt"), "from-agent\n").expect("write a");
        std::fs::write(repo.join("b.txt"), "from-agent-too\n").expect("write b");

        let unreported =
            unreported_changes(&repo, &before, &[repo.join("a.txt")]).expect("diff should run");
        assert_eq!(unreported, vec!["b.txt".to_owned()]);

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn unreported_changes_ignores_pre_existing_operator_dirt() {
        let repo = init_test_repo("unreported-pre-existing");
        seed_commit(&repo, "seed.txt", "base\n");

        // Operator's unrelated in-flight edit, present BEFORE the run starts.
        std::fs::write(repo.join("operator.txt"), "unrelated work\n").expect("write operator");
        let before = dirty_paths(&repo).expect("snapshot before run");
        assert_eq!(before, vec!["operator.txt".to_owned()]);

        // Agent reports nothing; the operator's file is still dirty afterward,
        // untouched by the agent. It must not be flagged.
        let unreported = unreported_changes(&repo, &before, &[]).expect("diff should run");
        assert!(
            unreported.is_empty(),
            "pre-existing operator dirt must not be flagged: {unreported:?}"
        );

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn unreported_changes_is_empty_when_report_is_complete() {
        let repo = init_test_repo("unreported-complete");
        seed_commit(&repo, "seed.txt", "base\n");

        let before = dirty_paths(&repo).expect("snapshot before run");
        std::fs::write(repo.join("a.txt"), "from-agent\n").expect("write a");

        let unreported =
            unreported_changes(&repo, &before, &[repo.join("a.txt")]).expect("diff should run");
        assert!(unreported.is_empty(), "fully reported change should not be flagged");

        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn dirty_paths_reports_matchable_paths_for_a_rename() {
        let repo = init_test_repo("dirty-paths-rename");
        seed_commit(&repo, "old-name.txt", "base\n");

        let status = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["mv", "old-name.txt", "new-name.txt"])
            .status()
            .expect("git mv should run");
        assert!(status.success(), "git mv failed");

        let paths = dirty_paths(&repo).expect("dirty_paths should run");

        // The composite " -> " human-readable form must never appear, and
        // both the new and old repo-relative names must be reported so a
        // rename still matches a reported file under either name.
        assert!(
            paths.iter().all(|p| !p.contains(" -> ")),
            "dirty_paths must not report the composite rename string: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p == "new-name.txt"),
            "dirty_paths should report the new name: {paths:?}"
        );
        assert!(
            paths.iter().any(|p| p == "old-name.txt"),
            "dirty_paths should also report the old name: {paths:?}"
        );

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
