//! `varda tracker list|pull` (milestone T1, CLI half): the acknowledgement
//! gate and the idempotent import of tracker work items into home tasks.
//!
//! A task is matched to its work item by `external.{backend,id}`, so a second
//! pull never duplicates. Refreshes never touch a `running` task.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use super::{IssueTracker, RemoteIssue, status_from_remote};
use crate::config::{Config, TrackerConfig};
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
    fn label(&self) -> &'static str {
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
    let setup_hint = format!("run `varda tracker setup --tracker {}`", tracker.name);
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
            let project = Path::new(&tracker.project_path);
            // The tracker name keeps two trackers that return the same title and
            // native id for one project from colliding on the task filename.
            let name = format!("{} {}-{}", issue.title, tracker.name, issue.id);
            let path = task::create_task(config, &name, project, None, None, None)?;
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
            let paths = task::write_task_and_definition_body(&doc)?;
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
    let mut failed = 0usize;
    let mut commit_failures = 0usize;
    let mut written: Vec<PathBuf> = Vec::new();
    for t in selected {
        let result = match super::build_tracker(t, config) {
            Ok(backend) => pull_tracker(config, t, backend.as_ref(), dry_run).await,
            Err(e) => Err(e),
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
        let report = commit_in_groups(
            &written,
            crate::git::repo_root_for_path,
            |refs| crate::git::commit_task_files(refs, "Pull tracker work items"),
        );
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

#[cfg(test)]
mod tests {
    use super::super::fake::FakeTracker;
    use super::super::{AZDO_AGILE_STATUS_MAP, status_map_from};
    use super::*;
    use crate::config::{CredentialConfig, Route, TrackerAck, TrackerFilter, TrackerKind};
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
        task::create_task(&config, "beta azdo-test-2", Path::new(&t.project_path), None, None, None)
            .unwrap();
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
