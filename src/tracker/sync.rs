//! `varda tracker list|pull` (milestone T1, CLI half): the acknowledgement
//! gate and the idempotent import of tracker work items into home tasks.
//!
//! A task is matched to its work item by `external.{backend,id}`, so a second
//! pull never duplicates. Refreshes never touch a `running` task.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};

use super::{IssueTracker, RemoteIssue, status_from_remote};
use crate::config::{Config, TrackerConfig, TrackerKind};
use crate::task::{self, ExternalRef, TaskStatus};

/// What a pull did (or, with `--dry-run`, would do) for one work item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullAction {
    Created,
    Updated,
    MarkedDone,
    Unchanged,
    SkippedRunning,
    /// New remote item already in a `done` state: not worth importing.
    SkippedClosed,
    /// New remote item in a state that must not be imported (in-flight
    /// statuses, or a remote state the `status_map` does not know).
    SkippedState,
}

impl PullAction {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Created => "create",
            Self::Updated => "update",
            Self::MarkedDone => "mark done",
            Self::Unchanged => "unchanged",
            Self::SkippedRunning => "skip (local task running)",
            Self::SkippedClosed => "skip (already closed remotely)",
            Self::SkippedState => "skip (remote state not importable)",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PullItem {
    pub id: String,
    pub title: String,
    pub action: PullAction,
    /// Every file written for this item (home task, plus the repo definition).
    pub paths: Vec<PathBuf>,
    /// Extra operator-facing explanation printed after the item line.
    pub note: Option<String>,
}

/// Result of pulling one tracker: the items that succeeded (with the paths they
/// wrote) and the per-item errors, so a late failure never discards bookkeeping
/// for earlier writes.
#[derive(Debug, Default)]
pub struct PullOutcome {
    pub items: Vec<PullItem>,
    pub errors: Vec<String>,
}

/// One-line summary of a tracker's filter for `tracker list`.
pub fn filter_summary(tracker: &TrackerConfig) -> String {
    let f = &tracker.filter;
    let mut parts = Vec::new();
    if let Some(wiql) = &f.wiql {
        parts.push(format!("wiql={}", wiql.chars().take(40).collect::<String>()));
    }
    if !f.tags.is_empty() {
        parts.push(format!("tags={}", f.tags.join(",")));
    }
    if !f.states.is_empty() {
        parts.push(format!("states={}", f.states.join(",")));
    }
    if let Some(area) = &f.area_path {
        parts.push(format!("area={area}"));
    }
    if parts.is_empty() {
        "(none)".to_owned()
    } else {
        parts.join(" ")
    }
}

/// Refuse a tracker that was never acknowledged, or whose acknowledgement does
/// not cover every agent the `project_path` route allows.
pub fn check_acknowledged(config: &Config, tracker: &TrackerConfig) -> Result<()> {
    let setup_hint = match tracker.kind {
        TrackerKind::Github => format!(
            "run `varda tracker setup github --repo OWNER/NAME --project-path PATH --name {}`",
            tracker.name
        ),
        TrackerKind::Jira => format!(
            "run `varda tracker setup jira --site URL --project-key KEY --email EMAIL --repo PATH --name {}`",
            tracker.name
        ),
        TrackerKind::Azdo => format!(
            "run `varda tracker setup azdo --org ORG --project PROJECT --repo PATH --name {}`",
            tracker.name
        ),
    };
    let Some(ack) = tracker.acknowledged.as_ref() else {
        bail!(
            "tracker '{}' has not been acknowledged; {setup_hint}",
            tracker.name
        );
    };
    let route = crate::routing::find_route_public(config, Path::new(&tracker.project_path))
        .with_context(|| format!("tracker '{}' project_path has no route", tracker.name))?;
    let missing: Vec<&str> = route
        .agents
        .iter()
        .filter(|agent| !ack.agents.iter().any(|a| a == *agent))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        bail!(
            "tracker '{}' acknowledgement does not cover agent(s) {} allowed by route '{}'; {setup_hint}",
            tracker.name,
            missing.join(", "),
            route.glob
        );
    }
    Ok(())
}

fn task_body(issue: &RemoteIssue) -> String {
    format!(
        "# {}\n\n[{}]({})\n\n{}\n",
        issue.title,
        issue.id,
        issue.url,
        issue.body_markdown.trim_end()
    )
}

fn is_done(tracker: &TrackerConfig, issue: &RemoteIssue) -> bool {
    status_from_remote(tracker, &issue.remote_state, &issue.tags) == Some(TaskStatus::Done)
}

/// Import/refresh every work item `source` returns for `tracker`. Writes task
/// files unless `dry_run`; the caller commits the returned paths.
pub async fn pull_tracker(
    config: &Config,
    tracker: &TrackerConfig,
    source: &dyn IssueTracker,
    dry_run: bool,
) -> Result<PullOutcome> {
    check_acknowledged(config, tracker)?;
    let issues = source
        .query(&tracker.filter)
        .await
        .map_err(|e| anyhow::anyhow!("tracker '{}': {e}", tracker.name))?;
    let mut outcome = PullOutcome::default();
    for issue in issues {
        match apply_issue(config, tracker, &issue, dry_run) {
            Ok((action, paths)) => {
                let note = (action == PullAction::SkippedState).then(|| {
                    format!(
                        "remote state '{}' is not importable with this status_map; to import items in that state, set `ready` (or `backlog`) to '{}' in the tracker's status_map",
                        issue.remote_state, issue.remote_state
                    )
                });
                outcome.items.push(PullItem {
                    id: issue.id,
                    title: issue.title,
                    action,
                    paths,
                    note,
                })
            }
            Err(e) => outcome
                .errors
                .push(format!("tracker '{}' item {}: {e:#}", tracker.name, issue.id)),
        }
    }
    Ok(outcome)
}

fn apply_issue(
    config: &Config,
    tracker: &TrackerConfig,
    issue: &RemoteIssue,
    dry_run: bool,
) -> Result<(PullAction, Vec<PathBuf>)> {
    apply_issue_with(config, tracker, issue, dry_run, task::write_task_and_definition_body)
}

/// The home task name an imported item gets. The tracker name keeps two
/// trackers that return the same title and native id for one project from
/// colliding on the task filename.
fn import_task_name(tracker: &TrackerConfig, issue: &RemoteIssue) -> String {
    format!("{} {}-{}", issue.title, tracker.name, issue.id)
}

/// Local-only options for a task created by `task add --tracker`. They go into
/// the repo DEFINITION too (via `create_task_with_egress`), because
/// `sync_definition_fields_from_repo` re-copies those fields on every dispatch.
#[derive(Debug, Clone, Default)]
pub struct LocalTaskOptions {
    pub assignee: Option<String>,
    pub sandbox: Option<String>,
    pub egress: Vec<String>,
}

/// Create the home task for a new remote item, with `external` set.
///
/// `create_task` and the write that records `external` are two steps, so a
/// failure (or crash) between them leaves a task that carries no `external`.
/// The filename is derived from title + tracker + remote id, so the next
/// pull finds that orphan at the same path and ADOPTS it instead of failing on
/// "already exists" or importing a duplicate. A file that already carries a
/// *different* `external` is never adopted.
fn import_new_issue(
    config: &Config,
    tracker: &TrackerConfig,
    issue: &RemoteIssue,
    status: TaskStatus,
    local: &LocalTaskOptions,
    write: impl FnOnce(&task::TaskDocument) -> Result<Vec<PathBuf>>,
) -> Result<Vec<PathBuf>> {
    let project = Path::new(&tracker.project_path);
    let name = import_task_name(tracker, issue);
    let orphan = task::task_file_path(config, project, &name)?;
    let path = if orphan.exists() {
        // Read the home file WITHOUT the definition overlay: a crash after the
        // definition write leaves the remote body there, while the home file
        // (written last) is still the pristine `create_task` seed.
        let existing = task::load_task_raw(&orphan)?;
        // Only a pristine `create_task` leftover (no `external`, untouched body).
        if existing.frontmatter.external.is_some()
            || existing.body.trim() != format!("# {name}")
        {
            bail!("task {} already exists", orphan.display());
        }
        orphan
    } else {
        task::create_task_with_egress(
            config,
            &name,
            project,
            local.assignee.as_deref(),
            None,
            local.sandbox.as_deref(),
            &local.egress,
        )?
    };
    let mut doc = task::load_task(&path)?;
    doc.frontmatter.status = status;
    doc.frontmatter.external = Some(ExternalRef {
        backend: tracker.name.clone(),
        id: issue.id.clone(),
        url: issue.url.clone(),
        rev: Some(issue.rev),
        synced_status: None,
        remote_state: Some(issue.remote_state.clone()),
        sync_error: None,
    });
    doc.body = task_body(issue);
    write(&doc)
}

fn apply_issue_with(
    config: &Config,
    tracker: &TrackerConfig,
    issue: &RemoteIssue,
    dry_run: bool,
    write_new: impl FnOnce(&task::TaskDocument) -> Result<Vec<PathBuf>>,
) -> Result<(PullAction, Vec<PathBuf>)> {
    match task::find_task_by_external(config, &tracker.name, &issue.id)? {
        None => {
            if is_done(tracker, issue) {
                return Ok((PullAction::SkippedClosed, Vec::new()));
            }
            let status = match status_from_remote(tracker, &issue.remote_state, &issue.tags) {
                Some(s @ (TaskStatus::Backlog | TaskStatus::Ready)) => s,
                _ => return Ok((PullAction::SkippedState, Vec::new())),
            };
            if dry_run {
                return Ok((PullAction::Created, Vec::new()));
            }
            let paths = import_new_issue(config, tracker, issue, status, &LocalTaskOptions::default(), write_new)?;
            Ok((PullAction::Created, paths))
        }
        Some(path) => {
            let mut doc = task::load_task(&path)?;
            if doc.frontmatter.status == TaskStatus::Running {
                return Ok((PullAction::SkippedRunning, Vec::new()));
            }
            let seen_rev = doc.frontmatter.external.as_ref().and_then(|e| e.rev);
            if seen_rev == Some(issue.rev) {
                return Ok((PullAction::Unchanged, Vec::new()));
            }
            let close = is_done(tracker, issue) && doc.frontmatter.status != TaskStatus::Done;
            let mut paths = Vec::new();
            if !dry_run {
                if let Some(ext) = doc.frontmatter.external.as_mut() {
                    ext.rev = Some(issue.rev);
                    ext.remote_state = Some(issue.remote_state.clone());
                    ext.url = issue.url.clone();
                }
                doc.body = task_body(issue);
                if close {
                    doc.frontmatter.status = TaskStatus::Done;
                }
                paths = task::write_task_and_definition_body(&doc)?;
            }
            let action = if close {
                PullAction::MarkedDone
            } else {
                PullAction::Updated
            };
            Ok((action, paths))
        }
    }
}

/// `varda tracker list`: print the table and an auth check per tracker.
/// Returns an error after printing everything if any check failed.
pub async fn list_command(config: &Config) -> Result<()> {
    if config.trackers.is_empty() {
        println!("no trackers configured (add a [[trackers]] entry to the central config)");
        return Ok(());
    }
    let mut failures = 0usize;
    for t in &config.trackers {
        let auth = match super::build_tracker(t, config) {
            Err(e) => Err(format!("{e:#}")),
            Ok(backend) => backend.check_auth().await.map_err(|e| e.to_string()),
        };
        let auth_text = match &auth {
            Ok(()) => "ok".to_owned(),
            Err(e) => {
                failures += 1;
                format!("FAILED: {e}")
            }
        };
        println!(
            "{}\t{:?}\t{}\tfilter: {}\tacknowledged: {}\tauth: {}",
            t.name,
            t.kind,
            t.project_path,
            filter_summary(t),
            if t.acknowledged.is_some() { "yes" } else { "no" },
            auth_text
        );
    }
    if failures > 0 {
        bail!("{failures} tracker auth check(s) failed");
    }
    Ok(())
}

/// Outcome of [`commit_in_groups`].
#[derive(Debug, Default)]
struct CommitReport {
    committed: usize,
    errors: Vec<String>,
    uncommitted: Vec<PathBuf>,
}

/// Group `paths` by git repository and commit each group independently: a
/// path whose repository cannot be found, or a group whose commit fails, is
/// recorded (with its paths in `uncommitted`) and the remaining groups are
/// still attempted.
fn commit_in_groups(
    paths: &[PathBuf],
    root_for: impl Fn(&Path) -> Result<PathBuf>,
    mut commit: impl FnMut(&[&Path]) -> Result<()>,
) -> CommitReport {
    let mut report = CommitReport::default();
    let mut groups: Vec<(PathBuf, Vec<&Path>)> = Vec::new();
    for path in paths {
        match root_for(path) {
            Ok(root) => match groups.iter_mut().find(|(r, _)| *r == root) {
                Some((_, group)) => group.push(path),
                None => groups.push((root, vec![path])),
            },
            Err(e) => {
                report.errors.push(format!("{}: {e:#}", path.display()));
                report.uncommitted.push(path.clone());
            }
        }
    }
    for (root, refs) in &groups {
        match commit(refs) {
            Ok(()) => report.committed += refs.len(),
            Err(e) => {
                report.errors.push(format!("{}: {e:#}", root.display()));
                report.uncommitted.extend(refs.iter().map(|p| p.to_path_buf()));
            }
        }
    }
    report
}

/// `varda tracker pull`: every tracker (or just `only`), best-effort across
/// trackers, then one control-plane commit of what was written.
pub async fn pull_command(config: &Config, only: Option<&str>, dry_run: bool) -> Result<()> {
    let selected: Vec<&TrackerConfig> = config
        .trackers
        .iter()
        .filter(|t| only.is_none_or(|n| t.name == n))
        .collect();
    if selected.is_empty() {
        match only {
            Some(name) => bail!("no tracker named '{name}' is configured"),
            None => bail!("no trackers configured"),
        }
    }
    pull_selected(config, selected, dry_run, None).await
}

/// Upper bound for one background / `run --pull` pull of a single tracker.
pub const PULL_TIMEOUT: Duration = Duration::from_secs(120);
/// Smallest `orchestrate --pull-every` interval.
pub const MIN_PULL_EVERY_SECS: u64 = 60;

/// Builds a tracker backend (may run a credential command: blocking).
type BuildFn =
    dyn Fn(&TrackerConfig, Option<Duration>) -> Result<Box<dyn IssueTracker>> + Send + Sync;
/// Commits one repo group of task files (blocking).
/// The deadline is shared by repo discovery and the commit of the whole step.
type CommitFn = dyn Fn(&[&Path], Option<Instant>) -> Result<()> + Send + Sync;
/// Finds a path's git root (blocking), bounded by the same shared deadline.
type RootFn = fn(&Path, Option<Instant>) -> Result<PathBuf>;

/// Pull every tracker in `selected`, best-effort across trackers, then one
/// control-plane commit of what was written.
async fn pull_selected(
    config: &Config,
    selected: Vec<&TrackerConfig>,
    dry_run: bool,
    bound: Option<Duration>,
) -> Result<()> {
    let build: Arc<BuildFn> = Arc::new(super::build_tracker_within);
    let commit: Arc<CommitFn> = Arc::new(commit_pulled);
    pull_selected_with(config, selected, dry_run, bound, build, crate::git::repo_root_for_path_until, commit)
        .await
}

fn commit_pulled(refs: &[&Path], deadline: Option<Instant>) -> Result<()> {
    crate::git::commit_task_files_until(refs, "Pull tracker work items", deadline)
}

/// Upper bound for the control-plane `git commit` after a pull.
const COMMIT_TIMEOUT: Duration = Duration::from_secs(60);

async fn pull_selected_with(
    config: &Config,
    selected: Vec<&TrackerConfig>,
    dry_run: bool,
    bound: Option<Duration>,
    build: Arc<BuildFn>,
    root_for: RootFn,
    commit: Arc<CommitFn>,
) -> Result<()> {
    let mut failed = 0usize;
    let mut commit_failures = 0usize;
    let mut written: Vec<PathBuf> = Vec::new();
    for t in selected {
        // The bound applies per tracker, so a stalled tracker cannot discard
        // what the others already wrote: everything completed is committed below.
        // Credential resolution may run a command: build off the executor
        // thread, with the same bound.
        let for_build = t.clone();
        let build = build.clone();
        let one = async {
            let backend = tokio::task::spawn_blocking(move || build(&for_build, bound))
                .await
                .map_err(|e| anyhow::anyhow!("building tracker failed: {e}"))??;
            pull_tracker(config, t, backend.as_ref(), dry_run).await
        };
        let result = match bound {
            Some(b) => match tokio::time::timeout(b, one).await {
                Ok(r) => r,
                Err(_) => Err(anyhow::anyhow!("timed out after {}s", b.as_secs())),
            },
            None => one.await,
        };
        match result {
            Ok(outcome) => {
                for item in &outcome.items {
                    let prefix = if dry_run { "would " } else { "" };
                    println!("{}: {prefix}{} {} {}", t.name, item.action.label(), item.id, item.title);
                    if let Some(note) = &item.note {
                        println!("{}:   note: {note}", t.name);
                    }
                    written.extend(item.paths.iter().cloned());
                }
                if !outcome.errors.is_empty() {
                    failed += 1;
                    for e in &outcome.errors {
                        eprintln!("{e}");
                    }
                }
            }
            Err(e) => {
                failed += 1;
                eprintln!("{}: {e:#}", t.name);
            }
        }
    }
    if !dry_run && config.git.auto_commit && !written.is_empty() {
        // git runs blocking (and bounded by `commit`'s own deadline), off the
        // executor thread.
        let report = tokio::task::spawn_blocking(move || {
            let deadline = Some(Instant::now() + COMMIT_TIMEOUT);
            commit_in_groups(
                &written,
                |p| root_for(p, deadline),
                |refs| commit(refs, deadline),
            )
        })
        .await
        .map_err(|e| anyhow::anyhow!("commit step failed: {e}"))?;
        if report.committed > 0 {
            println!("committed {} task file(s)", report.committed);
        }
        if !report.errors.is_empty() {
            for e in &report.errors {
                eprintln!("commit failed: {e}");
            }
            eprintln!("left uncommitted (commit these by hand):");
            for p in &report.uncommitted {
                eprintln!("  {}", p.display());
            }
            commit_failures = report.errors.len();
        }
    }
    if commit_failures > 0 {
        bail!("{commit_failures} commit group(s) failed");
    }
    if failed > 0 {
        bail!("{failed} tracker(s) failed");
    }
    Ok(())
}

/// `varda run --pull`: pull every configured tracker before running ready
/// tasks. Best-effort and bounded: a failing or slow tracker is reported and the
/// run proceeds with what is already on the board. No trackers ⇒ a no-op.
pub async fn pull_before_run(config: &Config, bound: Duration) {
    if config.trackers.is_empty() {
        eprintln!("--pull: no trackers configured; nothing to pull");
        return;
    }
    let selected: Vec<&TrackerConfig> = config.trackers.iter().collect();
    // The bound is applied per tracker inside `pull_selected`.
    if let Err(e) = pull_selected(config, selected, false, Some(bound)).await {
        eprintln!("--pull: {e:#} (continuing with the existing board)");
    }
}

/// Canonicalize `p`; when it does not exist (yet), canonicalize its deepest
/// existing ancestor and re-append the remaining components, so symlinked
/// prefixes (macOS `/var` -> `/private/var`) resolve the same on both sides.
fn resolved_path(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    let mut cur = p;
    while let Some(parent) = cur.parent() {
        if let Some(name) = cur.file_name() {
            rest.push(name);
        }
        if let Ok(mut base) = parent.canonicalize() {
            base.extend(rest.iter().rev());
            return base;
        }
        cur = parent;
    }
    p.to_path_buf()
}

fn resolved(path: &str) -> PathBuf {
    resolved_path(Path::new(path))
}

/// Names of the trackers whose `project_path` the `workspace` covers: the same
/// directory, or one inside the other (the workspace defaults to the repo root,
/// but may also be a parent holding several projects).
pub fn trackers_covered_by(config: &Config, workspace: &Path) -> Vec<String> {
    let workspace = resolved_path(workspace);
    config
        .trackers
        .iter()
        .filter(|t| {
            let project = resolved(&t.project_path);
            project.starts_with(&workspace) || workspace.starts_with(&project)
        })
        .map(|t| t.name.clone())
        .collect()
}

/// Validate `--pull-every`.
pub fn check_pull_every(secs: u64) -> Result<Duration> {
    if secs < MIN_PULL_EVERY_SECS {
        bail!("--pull-every must be at least {MIN_PULL_EVERY_SECS} seconds (got {secs})");
    }
    Ok(Duration::from_secs(secs))
}

/// Host-side background loop for `varda orchestrate --pull-every`: pull the
/// named trackers now and then every `every`, each pull bounded by `bound`.
/// Runs until aborted; never returns an error (failures are logged).
pub async fn pull_loop(config: Config, names: Vec<String>, every: Duration, bound: Duration) {
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        pull_names_once(&config, &names, bound).await;
    }
}

async fn pull_names_once(config: &Config, names: &[String], bound: Duration) {
    let selected: Vec<&TrackerConfig> = config
        .trackers
        .iter()
        .filter(|t| names.contains(&t.name))
        .collect();
    if let Err(e) = pull_selected(config, selected, false, Some(bound)).await {
        eprintln!("orchestrate pull: {e:#}");
    }
}

/// `varda task add --tracker NAME`: create the work item first (carrying the
/// pickup tag), then the home task with `external` set. Refuses an
/// unacknowledged tracker BEFORE anything is created remotely.
pub async fn add_tracker_task(
    config: &Config,
    tracker_name: &str,
    title: &str,
    description: Option<&str>,
    local: &LocalTaskOptions,
    bound: Duration,
) -> Result<PathBuf> {
    let tracker = config
        .trackers
        .iter()
        .find(|t| t.name == tracker_name)
        .with_context(|| format!("no tracker named '{tracker_name}' is configured"))?;
    check_acknowledged(config, tracker)?;
    let for_build = tracker.clone();
    let backend = tokio::task::spawn_blocking(move || {
        super::build_tracker_within(&for_build, Some(bound))
    })
    .await
    .context("building tracker failed")??;
    add_tracker_task_via(config, tracker, backend.as_ref(), title, description, local, bound).await
}

async fn add_tracker_task_via(
    config: &Config,
    tracker: &TrackerConfig,
    backend: &dyn IssueTracker,
    title: &str,
    description: Option<&str>,
    local: &LocalTaskOptions,
    bound: Duration,
) -> Result<PathBuf> {
    let draft = super::IssueDraft {
        title: title.to_owned(),
        body_markdown: description.unwrap_or_default().to_owned(),
        // The pickup tag makes the item visible to later pulls too.
        tags: tracker.filter.tags.clone(),
        // Keep the item inside an area-filtered pull.
        area_path: tracker.filter.area_path.clone(),
        ..super::IssueDraft::default()
    };
    let issue = tokio::time::timeout(bound, backend.create(&draft))
        .await
        .map_err(|_| anyhow::anyhow!("creating the work item timed out after {}s", bound.as_secs()))?
        .map_err(|e| anyhow::anyhow!("tracker '{}': {e}", tracker.name))?;
    // From here the work item exists: say so if the local half fails, and a
    // later `tracker pull` imports it (it carries the pickup tag).
    let status = match status_from_remote(tracker, &issue.remote_state, &issue.tags) {
        Some(s @ (TaskStatus::Backlog | TaskStatus::Ready)) => s,
        _ => TaskStatus::Backlog,
    };
    import_new_issue(config, tracker, &issue, status, local, task::write_task_and_definition_body)
        .map(|paths| paths.into_iter().next().expect("home task path"))
        .map_err(|e| {
            anyhow::anyhow!(
                "created work item {} ({}) but the local task could not be written: {e:#}; `varda tracker pull` will import it",
                issue.id,
                issue.url
            )
        })
}

#[cfg(test)]
mod tests {
    use super::super::fake::FakeTracker;
    use super::super::{AZDO_AGILE_STATUS_MAP, status_map_from};
    use super::*;
    use crate::config::{CredentialConfig, Route, TrackerAck, TrackerFilter};
    use std::collections::BTreeMap;

    fn setup(name: &str) -> (PathBuf, Config, TrackerConfig) {
        let root = std::env::temp_dir().join(format!("varda-sync-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let tracker = TrackerConfig {
            name: "azdo-test".to_owned(),
            kind: TrackerKind::Azdo,
            project_path: project.display().to_string(),
            credential: CredentialConfig {
                from_env: Some("PAT".to_owned()),
                ..CredentialConfig::default()
            },
            filter: TrackerFilter::default(),
            status_map: status_map_from(AZDO_AGILE_STATUS_MAP),
            acknowledged: Some(TrackerAck {
                by: "me".to_owned(),
                at: "now".to_owned(),
                agents: vec!["claude".to_owned()],
            }),
            extra: BTreeMap::new(),
        };
        let config = Config {
            defaults: crate::config::Defaults {
                operations_dir: root.join("ops").display().to_string(),
                sandbox: None,
                ..Default::default()
            },
            routes: vec![Route {
                glob: format!("{}/**", root.display()),
                agents: vec!["claude".to_owned()],
                sandbox: None,
                mounts: vec![],
                env: BTreeMap::new(),
                orchestration: None,
                verify: vec![],
                untrusted: false,
            }],
            agents: BTreeMap::new(),
            roles: BTreeMap::new(),
            git: crate::config::GitConfig { auto_commit: false },
            sandboxes: BTreeMap::new(),
            orchestration: crate::orchestration::OrchestrationPolicy::default(),
            include: Vec::new(),
            requires_commands: Vec::new(),
            requires_secrets: Vec::new(),
            trackers: vec![tracker.clone()],
        };
        (root, config, tracker)
    }

    fn count_tasks(root: &Path) -> usize {
        fn walk(dir: &Path, n: &mut usize) {
            let Ok(rd) = std::fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    walk(&p, n);
                } else if p.extension().is_some_and(|x| x == "md") {
                    *n += 1;
                }
            }
        }
        let mut n = 0;
        walk(&root.join("ops/tasks"), &mut n);
        n
    }

    #[tokio::test]
    async fn first_pull_creates_second_is_noop() {
        let (root, config, t) = setup("idem");
        let fake = FakeTracker::new(t.clone());
        fake.insert("New", "alpha", &["varda:ready"]);
        fake.insert("New", "beta", &[]);
        let items = pull_tracker(&config, &t, &fake, false).await.unwrap().items;
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.action == PullAction::Created));
        assert_eq!(count_tasks(&root), 2);
        let alpha = task::load_task(&items[0].paths[0]).unwrap();
        assert_eq!(alpha.frontmatter.status, TaskStatus::Ready);
        assert_eq!(alpha.frontmatter.project.as_deref(), Some(t.project_path.as_str()));
        assert!(alpha.body.contains("https://fake.example/items/1"));
        let beta = task::load_task(&items[1].paths[0]).unwrap();
        assert_eq!(beta.frontmatter.status, TaskStatus::Backlog);

        let again = pull_tracker(&config, &t, &fake, false).await.unwrap().items;
        assert!(again.iter().all(|i| i.action == PullAction::Unchanged));
        assert_eq!(count_tasks(&root), 2);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn rev_bump_refreshes_but_running_task_is_left_alone() {
        let (root, config, t) = setup("refresh");
        let fake = FakeTracker::new(t.clone());
        let a = fake.insert("New", "alpha", &[]);
        let b = fake.insert("New", "beta", &[]);
        let items = pull_tracker(&config, &t, &fake, false).await.unwrap().items;
        let mut running = task::load_task(&items[1].paths[0]).unwrap();
        running.frontmatter.status = TaskStatus::Running;
        task::write_task(&running).unwrap();
        let before = std::fs::read_to_string(&running.path).unwrap();

        fake.human_edit(&a.id);
        fake.human_edit(&b.id);
        let items = pull_tracker(&config, &t, &fake, false).await.unwrap().items;
        assert_eq!(items[0].action, PullAction::Updated);
        assert_eq!(items[1].action, PullAction::SkippedRunning);
        let refreshed = task::load_task(&items[0].paths[0]).unwrap();
        assert_eq!(refreshed.frontmatter.external.unwrap().rev, Some(2));
        assert_eq!(std::fs::read_to_string(&running.path).unwrap(), before);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn remote_closed_marks_local_done() {
        let (root, config, t) = setup("closed");
        let fake = FakeTracker::new(t.clone());
        let a = fake.insert("New", "alpha", &[]);
        let items = pull_tracker(&config, &t, &fake, false).await.unwrap().items;
        fake.transition(&a.id, TaskStatus::Done, None).await.unwrap();
        let items2 = pull_tracker(&config, &t, &fake, false).await.unwrap().items;
        assert_eq!(items2[0].action, PullAction::MarkedDone);
        let doc = task::load_task(&items[0].paths[0]).unwrap();
        assert_eq!(doc.frontmatter.status, TaskStatus::Done);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn unacknowledged_or_partially_acknowledged_tracker_is_refused() {
        let (root, config, mut t) = setup("ack");
        let fake = FakeTracker::new(t.clone());
        fake.insert("New", "alpha", &[]);
        t.acknowledged = None;
        let err = pull_tracker(&config, &t, &fake, false).await.unwrap_err();
        assert!(format!("{err:#}").contains("tracker setup"), "{err:#}");

        t.acknowledged = Some(TrackerAck {
            by: "me".to_owned(),
            at: "now".to_owned(),
            agents: vec!["other".to_owned()],
        });
        let err = pull_tracker(&config, &t, &fake, false).await.unwrap_err();
        assert!(format!("{err:#}").contains("claude"), "{err:#}");
        assert_eq!(count_tasks(&root), 0);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn dry_run_writes_nothing() {
        let (root, config, t) = setup("dry");
        let fake = FakeTracker::new(t.clone());
        fake.insert("New", "alpha", &[]);
        let items = pull_tracker(&config, &t, &fake, true).await.unwrap().items;
        assert_eq!(items[0].action, PullAction::Created);
        assert_eq!(count_tasks(&root), 0);
        std::fs::remove_dir_all(root).ok();
    }

    fn make_varda_dir(root: &Path) {
        std::fs::create_dir_all(root.join("proj/.varda")).unwrap();
    }

    #[tokio::test]
    async fn pulled_body_is_visible_via_load_task_with_repo_definition() {
        let (root, config, t) = setup("defbody");
        make_varda_dir(&root);
        let fake = FakeTracker::new(t.clone());
        let a = fake.insert("New", "alpha", &["varda:ready"]);
        let items = pull_tracker(&config, &t, &fake, false).await.unwrap().items;
        assert_eq!(items[0].paths.len(), 2, "{:?}", items[0].paths);
        assert!(items[0].paths.iter().any(|p| p.starts_with(root.join("proj/.varda"))));
        let doc = task::load_task(&items[0].paths[0]).unwrap();
        assert!(doc.body.contains("https://fake.example/items/1"), "{}", doc.body);
        assert!(doc.body.starts_with("# alpha"), "{}", doc.body);

        fake.human_edit(&a.id);
        let items = pull_tracker(&config, &t, &fake, false).await.unwrap().items;
        assert_eq!(items[0].action, PullAction::Updated);
        assert_eq!(items[0].paths.len(), 2);
        let doc = task::load_task(&items[0].paths[0]).unwrap();
        let raw = std::fs::read_to_string(&items[0].paths[0]).unwrap();
        assert!(raw.contains("rev: 2"), "{raw}");
        let def = std::fs::read_to_string(&items[0].paths[1]).unwrap();
        assert!(def.trim_end().ends_with(doc.body.trim()), "{def}");
        assert!(def.starts_with("---\n"), "{def}");
        assert!(!def.contains("external"), "{def}");
        let fresh = RemoteIssue {
            body_markdown: "changed text".to_owned(),
            rev: 3,
            ..fake.get(&a.id).await.unwrap()
        };
        let (action, _) = apply_issue(&config, &t, &fresh, false).unwrap();
        assert_eq!(action, PullAction::Updated);
        let doc = task::load_task(&items[0].paths[0]).unwrap();
        assert_eq!(doc.body.trim(), task_body(&fresh).trim());
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn new_items_import_only_backlog_or_ready() {
        let (root, config, mut t) = setup("states");
        t.status_map.insert("running".to_owned(), "Active".to_owned());
        t.status_map.insert("review".to_owned(), "Resolved".to_owned());
        let fake = FakeTracker::new(t.clone());
        fake.insert("New", "backlogged", &[]);
        fake.insert("New", "readied", &["varda:ready"]);
        fake.insert("Active", "running", &[]);
        fake.insert("Resolved", "reviewing", &[]);
        fake.insert("Weird", "unmapped", &[]);
        let items = pull_tracker(&config, &t, &fake, false).await.unwrap().items;
        let actions: Vec<_> = items.iter().map(|i| i.action.clone()).collect();
        assert_eq!(
            actions,
            vec![
                PullAction::Created,
                PullAction::Created,
                PullAction::SkippedState,
                PullAction::SkippedState,
                PullAction::SkippedState,
            ]
        );
        assert_eq!(count_tasks(&root), 2);
        let b = task::load_task(&items[0].paths[0]).unwrap();
        let r = task::load_task(&items[1].paths[0]).unwrap();
        assert_eq!(b.frontmatter.status, TaskStatus::Backlog);
        assert_eq!(r.frontmatter.status, TaskStatus::Ready);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn partial_failure_keeps_earlier_paths() {
        let (root, config, t) = setup("partial");
        let fake = FakeTracker::new(t.clone());
        fake.insert("New", "alpha", &[]);
        fake.insert("New", "beta", &[]);
        // Squat on beta's filename so creating it fails after alpha succeeded.
        let squat = task::create_task(&config, "beta azdo-test-2", Path::new(&t.project_path), None, None, None)
            .unwrap();
        let mut squatter = task::load_task(&squat).unwrap();
        squatter.body = "# hand-written\n\nmine\n".to_owned();
        task::write_task(&squatter).unwrap();
        let outcome = pull_tracker(&config, &t, &fake, false).await.unwrap();
        assert_eq!(outcome.items.len(), 1);
        assert_eq!(outcome.items[0].action, PullAction::Created);
        assert_eq!(outcome.items[0].paths.len(), 1);
        assert!(outcome.items[0].paths[0].exists());
        assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
        assert!(outcome.errors[0].contains("item 2"), "{:?}", outcome.errors);

        // A retry sees alpha as unchanged: only the first run could report its path.
        let again = pull_tracker(&config, &t, &fake, false).await.unwrap();
        assert_eq!(again.items[0].action, PullAction::Unchanged);
        assert!(again.items[0].paths.is_empty());
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn two_trackers_same_title_and_id_both_import() {
        let (root, mut config, t1) = setup("twotrackers");
        let mut t2 = t1.clone();
        t2.name = "azdo-other".to_owned();
        config.trackers.push(t2.clone());
        let f1 = FakeTracker::new(t1.clone());
        let f2 = FakeTracker::new(t2.clone());
        f1.insert("New", "same", &[]);
        f2.insert("New", "same", &[]);
        let a = pull_tracker(&config, &t1, &f1, false).await.unwrap();
        let b = pull_tracker(&config, &t2, &f2, false).await.unwrap();
        assert!(a.errors.is_empty() && b.errors.is_empty(), "{:?} {:?}", a.errors, b.errors);
        assert_eq!(a.items[0].action, PullAction::Created);
        assert_eq!(b.items[0].action, PullAction::Created);
        assert_ne!(a.items[0].paths[0], b.items[0].paths[0]);
        assert_eq!(count_tasks(&root), 2);
        let a2 = pull_tracker(&config, &t1, &f1, false).await.unwrap();
        let b2 = pull_tracker(&config, &t2, &f2, false).await.unwrap();
        assert_eq!(a2.items[0].action, PullAction::Unchanged);
        assert_eq!(b2.items[0].action, PullAction::Unchanged);
        assert_eq!(count_tasks(&root), 2);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn default_agile_map_active_item_is_skipped_visibly_even_with_pickup_tag() {
        let (root, config, t) = setup("activepickup");
        let fake = FakeTracker::new(t.clone());
        fake.insert("Active", "tagged", &["varda"]);
        fake.insert("Active", "untagged", &[]);
        let items = pull_tracker(&config, &t, &fake, false).await.unwrap().items;
        assert_eq!(items.len(), 2);
        for item in &items {
            assert_eq!(item.action, PullAction::SkippedState);
            let note = item.note.as_deref().unwrap();
            assert!(note.contains("'Active'") && note.contains("status_map"), "{note}");
        }
        assert_eq!(count_tasks(&root), 0);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn definition_failure_does_not_advance_rev_and_retry_repairs() {
        let (root, config, t) = setup("defrepair");
        make_varda_dir(&root);
        let fake = FakeTracker::new(t.clone());
        let a = fake.insert("New", "alpha", &["varda:ready"]);
        let items = pull_tracker(&config, &t, &fake, false).await.unwrap().items;
        let (home, def) = (items[0].paths[0].clone(), items[0].paths[1].clone());
        let good = std::fs::read_to_string(&def).unwrap();

        std::fs::write(&def, "no frontmatter here\n").unwrap();
        fake.human_edit(&a.id);
        let outcome = pull_tracker(&config, &t, &fake, false).await.unwrap();
        assert_eq!(outcome.errors.len(), 1, "{:?}", outcome.errors);
        let raw = std::fs::read_to_string(&home).unwrap();
        assert!(raw.contains("rev: 1"), "{raw}");

        std::fs::write(&def, &good).unwrap();
        let items = pull_tracker(&config, &t, &fake, false).await.unwrap().items;
        assert_eq!(items[0].action, PullAction::Updated);
        assert!(std::fs::read_to_string(&home).unwrap().contains("rev: 2"));
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn failed_external_write_leaves_orphan_that_next_pull_adopts() {
        let (root, config, t) = setup("orphan");
        let fake = FakeTracker::new(t.clone());
        let a = fake.insert("New", "alpha", &["varda:ready"]);
        let issue = fake.get(&a.id).await.unwrap();

        // The write that records `external` fails after `create_task` succeeded.
        let err = apply_issue_with(&config, &t, &issue, false, |_| bail!("disk full")).unwrap_err();
        assert!(format!("{err:#}").contains("disk full"), "{err:#}");
        assert_eq!(count_tasks(&root), 1);
        assert!(task::find_task_by_external(&config, &t.name, &a.id).unwrap().is_none());

        // The next pull adopts the orphan: no duplicate, `external` now set.
        let items = pull_tracker(&config, &t, &fake, false).await.unwrap();
        assert!(items.errors.is_empty(), "{:?}", items.errors);
        assert_eq!(items.items[0].action, PullAction::Created);
        assert_eq!(count_tasks(&root), 1);
        let path = task::find_task_by_external(&config, &t.name, &a.id).unwrap().unwrap();
        let doc = task::load_task(&path).unwrap();
        assert_eq!(doc.frontmatter.status, TaskStatus::Ready);
        assert_eq!(doc.frontmatter.external.unwrap().rev, Some(1));

        let again = pull_tracker(&config, &t, &fake, false).await.unwrap();
        assert_eq!(again.items[0].action, PullAction::Unchanged);
        assert_eq!(count_tasks(&root), 1);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn crash_after_definition_write_is_adopted_by_next_pull() {
        let (root, config, t) = setup("defcrash");
        make_varda_dir(&root);
        let fake = FakeTracker::new(t.clone());
        let a = fake.insert("New", "alpha", &["varda:ready"]);
        let issue = fake.get(&a.id).await.unwrap();
        let store = root.join("proj/.varda/tasks");
        let err = apply_issue_with(&config, &t, &issue, false, |doc| {
            // The definition write lands, then the home write never happens.
            let def = std::fs::read_dir(&store).unwrap().next().unwrap().unwrap().path();
            let text = std::fs::read_to_string(&def).unwrap();
            let end = text[4..].find("---\n").unwrap() + 8;
            std::fs::write(&def, format!("{}\n{}", &text[..end], doc.body)).unwrap();
            bail!("crash")
        })
        .unwrap_err();
        assert!(format!("{err:#}").contains("crash"), "{err:#}");
        assert_eq!(count_tasks(&root), 1);

        let out = pull_tracker(&config, &t, &fake, false).await.unwrap();
        assert!(out.errors.is_empty(), "{:?}", out.errors);
        assert_eq!(out.items[0].action, PullAction::Created);
        assert_eq!(count_tasks(&root), 1);
        let path = task::find_task_by_external(&config, &t.name, &a.id).unwrap().unwrap();
        assert!(task::load_task(&path).unwrap().body.contains("fake.example"));
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn slow_credential_command_is_bounded_in_pull() {
        let (root, mut config, mut t) = setup("slowcred");
        t.credential = CredentialConfig {
            command: Some("sleep 30".to_owned()),
            ..CredentialConfig::default()
        };
        config.trackers = vec![t.clone()];
        let started = std::time::Instant::now();
        let res = pull_selected(&config, vec![&t], false, Some(Duration::from_secs(1))).await;
        assert!(res.is_err());
        assert!(started.elapsed() < Duration::from_secs(15), "{:?}", started.elapsed());
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn add_keeps_egress_and_sandbox_through_definition_sync() {
        let (root, config, t) = setup("addegress");
        make_varda_dir(&root);
        let fake = FakeTracker::new(t.clone());
        let local = LocalTaskOptions {
            assignee: Some("claude".to_owned()),
            sandbox: Some("worker".to_owned()),
            egress: vec!["example.com".to_owned()],
        };
        let path = add_tracker_task_via(&config, &t, &fake, "x", None, &local, Duration::from_secs(5))
            .await
            .unwrap();
        let mut doc = task::load_task(&path).unwrap();
        task::sync_definition_fields_from_repo(&mut doc).unwrap();
        assert_eq!(doc.frontmatter.bounds.egress, vec!["example.com"]);
        assert_eq!(doc.frontmatter.sandbox.as_deref(), Some("worker"));
        assert_eq!(doc.frontmatter.assignee.as_deref(), Some("claude"));
        std::fs::remove_dir_all(root).ok();
    }

    fn boxed_build(
        f: impl Fn(&TrackerConfig, Option<Duration>) -> Result<Box<dyn IssueTracker>> + Send + Sync + 'static,
    ) -> Arc<BuildFn> {
        Arc::new(f)
    }

    fn boxed_commit(f: impl Fn(&[&Path]) -> Result<()> + Send + Sync + 'static) -> Arc<CommitFn> {
        Arc::new(move |refs, _deadline| f(refs))
    }

    /// Delegates to a fake, optionally stalling `query` and recording drafts.
    struct Probe {
        inner: FakeTracker,
        stall: bool,
        drafts: std::sync::Mutex<Vec<super::super::IssueDraft>>,
    }

    #[async_trait::async_trait]
    impl IssueTracker for Probe {
        fn name(&self) -> &str {
            self.inner.name()
        }
        async fn query(
            &self,
            filter: &crate::config::TrackerFilter,
        ) -> super::super::Result<Vec<RemoteIssue>> {
            if self.stall {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            self.inner.query(filter).await
        }
        async fn get(&self, id: &str) -> super::super::Result<RemoteIssue> {
            self.inner.get(id).await
        }
        async fn create(&self, draft: &super::super::IssueDraft) -> super::super::Result<RemoteIssue> {
            self.drafts.lock().unwrap().push(draft.clone());
            self.inner.create(draft).await
        }
        async fn transition(
            &self,
            id: &str,
            to: TaskStatus,
            expected_rev: Option<u64>,
        ) -> super::super::Result<RemoteIssue> {
            self.inner.transition(id, to, expected_rev).await
        }
        async fn comment(&self, id: &str, markdown: &str) -> super::super::Result<()> {
            self.inner.comment(id, markdown).await
        }
        async fn assign(&self, id: &str, who: Option<&str>) -> super::super::Result<()> {
            self.inner.assign(id, who).await
        }
    }

    #[tokio::test]
    async fn stalled_tracker_does_not_discard_other_trackers_paths() {
        let (root, mut config, a) = setup("stall");
        let mut b = a.clone();
        b.name = "azdo-stall".to_owned();
        config.trackers = vec![a.clone(), b.clone()];
        config.git.auto_commit = true;
        let good = Probe { inner: FakeTracker::new(a.clone()), stall: false, drafts: Default::default() };
        good.inner.insert("New", "alpha", &[]);
        let slow = Probe { inner: FakeTracker::new(b.clone()), stall: true, drafts: Default::default() };
        let (good, slow) = (Arc::new(good), Arc::new(slow));
        struct Shared(Arc<Probe>);
        #[async_trait::async_trait]
        impl IssueTracker for Shared {
            fn name(&self) -> &str { self.0.name() }
            async fn query(&self, f: &crate::config::TrackerFilter) -> super::super::Result<Vec<RemoteIssue>> { self.0.query(f).await }
            async fn get(&self, id: &str) -> super::super::Result<RemoteIssue> { self.0.get(id).await }
            async fn create(&self, d: &super::super::IssueDraft) -> super::super::Result<RemoteIssue> { self.0.create(d).await }
            async fn transition(&self, id: &str, to: TaskStatus, r: Option<u64>) -> super::super::Result<RemoteIssue> { self.0.transition(id, to, r).await }
            async fn comment(&self, id: &str, m: &str) -> super::super::Result<()> { self.0.comment(id, m).await }
            async fn assign(&self, id: &str, w: Option<&str>) -> super::super::Result<()> { self.0.assign(id, w).await }
        }
        let build: Arc<BuildFn> = {
            let (good, slow) = (good.clone(), slow.clone());
            boxed_build(move |t, _| {
                let p = if t.name == "azdo-stall" { slow.clone() } else { good.clone() };
                Ok(Box::new(Shared(p)) as Box<dyn IssueTracker>)
            })
        };
        let committed: Arc<std::sync::Mutex<Vec<PathBuf>>> = Default::default();
        let commit: Arc<CommitFn> = {
            let committed = committed.clone();
            boxed_commit(move |refs| {
                committed.lock().unwrap().extend(refs.iter().map(|p| p.to_path_buf()));
                Ok(())
            })
        };
        let selected: Vec<&TrackerConfig> = config.trackers.iter().collect();
        let started = std::time::Instant::now();
        let res = pull_selected_with(&config, selected, false, Some(Duration::from_secs(1)), build, |_, _| Ok(PathBuf::from("/repo")), commit).await;
        assert!(res.is_err(), "the stalled tracker must be reported as failed");
        assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
        let done = committed.lock().unwrap().clone();
        assert!(!done.is_empty(), "completed tracker's paths must still be committed");
        assert!(done.iter().all(|p| p.exists()));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn stalled_discovery_puts_paths_on_the_uncommitted_list() {
        let paths = vec![PathBuf::from("/a/x.md"), PathBuf::from("/b/y.md")];
        let report = commit_in_groups(
            &paths,
            |p| {
                if p.starts_with("/a") {
                    Err(anyhow::anyhow!("git timed out and was stopped"))
                } else {
                    Ok(PathBuf::from("/b"))
                }
            },
            |_| Ok(()),
        );
        assert_eq!(report.uncommitted, vec![PathBuf::from("/a/x.md")]);
        assert_eq!(report.committed, 1);
        assert_eq!(report.errors.len(), 1);
    }

    #[tokio::test]
    async fn add_carries_filter_area_path_into_the_draft() {
        let (root, config, mut t) = setup("area");
        t.filter.area_path = Some("Proj\\Team".to_owned());
        let probe = Probe { inner: FakeTracker::new(t.clone()), stall: false, drafts: Default::default() };
        add_tracker_task_via(&config, &t, &probe, "x", None, &LocalTaskOptions::default(), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(probe.drafts.lock().unwrap()[0].area_path.as_deref(), Some("Proj\\Team"));
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn add_applies_every_filter_tag_on_create() {
        let (root, config, mut t) = setup("twotags");
        t.filter.tags = vec!["varda".to_owned(), "team".to_owned()];
        let fake = FakeTracker::new(t.clone());
        add_tracker_task_via(&config, &t, &fake, "x", None, &LocalTaskOptions::default(), Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(fake.get("1").await.unwrap().tags, vec!["varda", "team"]);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn add_creates_remote_item_with_pickup_tag_then_linked_local_task() {
        let (root, config, mut t) = setup("add");
        t.filter.tags = vec!["varda".to_owned()];
        let fake = FakeTracker::new(t.clone());
        let path = add_tracker_task_via(&config, &t, &fake, "write docs", Some("details"), &LocalTaskOptions::default(), Duration::from_secs(5))
            .await
            .unwrap();
        let remote = fake.get("1").await.unwrap();
        assert_eq!(remote.title, "write docs");
        assert_eq!(remote.body_markdown, "details");
        assert_eq!(remote.tags, vec!["varda"]);
        let doc = task::load_task(&path).unwrap();
        let ext = doc.frontmatter.external.unwrap();
        assert_eq!((ext.backend.as_str(), ext.id.as_str()), ("azdo-test", "1"));
        assert_eq!(doc.frontmatter.status, TaskStatus::Backlog);
        assert_eq!(doc.frontmatter.project.as_deref(), Some(t.project_path.as_str()));
        // The next pull recognises it: no duplicate.
        let again = pull_tracker(&config, &t, &fake, false).await.unwrap();
        assert_eq!(again.items[0].action, PullAction::Unchanged);
        assert_eq!(count_tasks(&root), 1);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn add_refuses_unknown_or_unacknowledged_tracker_before_creating_anything() {
        let (root, mut config, _) = setup("addack");
        let err = add_tracker_task(&config, "nope", "x", None, &LocalTaskOptions::default(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("no tracker named"), "{err:#}");
        config.trackers[0].acknowledged = None;
        let err = add_tracker_task(&config, "azdo-test", "x", None, &LocalTaskOptions::default(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("not been acknowledged"), "{err:#}");
        assert_eq!(count_tasks(&root), 0);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn pull_before_run_without_trackers_is_a_noop_and_with_trackers_imports() {
        let (root, mut config, t) = setup("runpull");
        let mut empty = config.clone();
        empty.trackers.clear();
        pull_before_run(&empty, Duration::from_secs(5)).await;
        assert_eq!(count_tasks(&root), 0);

        // An unbuildable tracker (credential env unset) is reported, not fatal.
        config.trackers = vec![t];
        pull_before_run(&config, Duration::from_secs(5)).await;
        assert_eq!(count_tasks(&root), 0);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn pull_every_has_a_floor_and_workspace_selects_covered_trackers() {
        assert!(check_pull_every(59).is_err());
        assert_eq!(check_pull_every(60).unwrap(), Duration::from_secs(60));

        let (root, mut config, t) = setup("cover");
        let mut other = t.clone();
        other.name = "elsewhere".to_owned();
        other.project_path = "/definitely/not/here".to_owned();
        config.trackers = vec![t.clone(), other];
        let project = PathBuf::from(&t.project_path);
        assert_eq!(trackers_covered_by(&config, &project), vec!["azdo-test"]);
        assert_eq!(trackers_covered_by(&config, &root), vec!["azdo-test"]);
        assert_eq!(trackers_covered_by(&config, &project.join("sub")), vec!["azdo-test"]);
        assert!(trackers_covered_by(&config, Path::new("/tmp/unrelated-ws")).is_empty());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn commit_groups_continue_past_failing_group_and_report_uncommitted() {
        let paths: Vec<PathBuf> = ["/a/1.md", "/b/2.md", "/c/3.md", "/nogit/4.md", "/a/5.md"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let root_for = |p: &Path| -> Result<PathBuf> {
            let top = p.components().nth(1).unwrap().as_os_str().to_owned();
            if top == "nogit" {
                bail!("not in a git repository");
            }
            Ok(PathBuf::from("/").join(top))
        };
        let mut attempted = Vec::new();
        let report = commit_in_groups(&paths, root_for, |refs| {
            attempted.push(refs.len());
            if refs[0].starts_with("/b") {
                bail!("commit boom");
            }
            Ok(())
        });
        assert_eq!(attempted, vec![2, 1, 1]);
        assert_eq!(report.committed, 3);
        assert_eq!(report.errors.len(), 2, "{:?}", report.errors);
        assert_eq!(
            report.uncommitted,
            vec![PathBuf::from("/nogit/4.md"), PathBuf::from("/b/2.md")]
        );
    }
}
