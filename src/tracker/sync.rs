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
        }
    }
}

#[derive(Debug, Clone)]
pub struct PullItem {
    pub id: String,
    pub title: String,
    pub action: PullAction,
    pub path: Option<PathBuf>,
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
) -> Result<Vec<PullItem>> {
    check_acknowledged(config, tracker)?;
    let issues = source
        .query(&tracker.filter)
        .await
        .map_err(|e| anyhow::anyhow!("tracker '{}': {e}", tracker.name))?;
    let mut items = Vec::new();
    for issue in issues {
        let (action, path) = apply_issue(config, tracker, &issue, dry_run)
            .with_context(|| format!("tracker '{}' item {}", tracker.name, issue.id))?;
        items.push(PullItem {
            id: issue.id,
            title: issue.title,
            action,
            path,
        });
    }
    Ok(items)
}

fn apply_issue(
    config: &Config,
    tracker: &TrackerConfig,
    issue: &RemoteIssue,
    dry_run: bool,
) -> Result<(PullAction, Option<PathBuf>)> {
    match task::find_task_by_external(config, &tracker.name, &issue.id)? {
        None => {
            if is_done(tracker, issue) {
                return Ok((PullAction::SkippedClosed, None));
            }
            if dry_run {
                return Ok((PullAction::Created, None));
            }
            let status = match status_from_remote(tracker, &issue.remote_state, &issue.tags) {
                Some(TaskStatus::Backlog) => TaskStatus::Backlog,
                _ => TaskStatus::Ready,
            };
            let project = Path::new(&tracker.project_path);
            let name = format!("{} {}", issue.title, issue.id);
            let path = task::create_task(config, &name, project, None, None, None)?;
            let mut doc = task::load_task(&path)?;
            doc.frontmatter.status = status;
            doc.frontmatter.external = Some(ExternalRef {
                backend: tracker.name.clone(),
                id: issue.id.clone(),
                url: issue.url.clone(),
                rev: Some(issue.rev),
                synced_status: None,
                sync_error: None,
            });
            doc.body = task_body(issue);
            task::write_task(&doc)?;
            Ok((PullAction::Created, Some(path)))
        }
        Some(path) => {
            let mut doc = task::load_task(&path)?;
            if doc.frontmatter.status == TaskStatus::Running {
                return Ok((PullAction::SkippedRunning, None));
            }
            let seen_rev = doc.frontmatter.external.as_ref().and_then(|e| e.rev);
            if seen_rev == Some(issue.rev) {
                return Ok((PullAction::Unchanged, None));
            }
            let close = is_done(tracker, issue) && doc.frontmatter.status != TaskStatus::Done;
            if !dry_run {
                if let Some(ext) = doc.frontmatter.external.as_mut() {
                    ext.rev = Some(issue.rev);
                    ext.url = issue.url.clone();
                }
                doc.body = task_body(issue);
                if close {
                    doc.frontmatter.status = TaskStatus::Done;
                }
                task::write_task(&doc)?;
            }
            let action = if close {
                PullAction::MarkedDone
            } else {
                PullAction::Updated
            };
            Ok((action, Some(path)))
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
    let mut written: Vec<PathBuf> = Vec::new();
    for t in selected {
        let result = match super::build_tracker(t, config) {
            Ok(backend) => pull_tracker(config, t, backend.as_ref(), dry_run).await,
            Err(e) => Err(e),
        };
        match result {
            Ok(items) => {
                for item in &items {
                    let prefix = if dry_run { "would " } else { "" };
                    println!("{}: {prefix}{} {} {}", t.name, item.action.label(), item.id, item.title);
                    written.extend(item.path.clone());
                }
            }
            Err(e) => {
                failed += 1;
                eprintln!("{}: {e:#}", t.name);
            }
        }
    }
    if !dry_run && config.git.auto_commit && !written.is_empty() {
        let refs: Vec<&Path> = written.iter().map(PathBuf::as_path).collect();
        crate::git::commit_task_files(&refs, "Pull tracker work items")?;
        println!("committed {} task file(s)", refs.len());
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
        let items = pull_tracker(&config, &t, &fake, false).await.unwrap();
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.action == PullAction::Created));
        assert_eq!(count_tasks(&root), 2);
        let alpha = task::load_task(items[0].path.as_ref().unwrap()).unwrap();
        assert_eq!(alpha.frontmatter.status, TaskStatus::Ready);
        assert_eq!(alpha.frontmatter.project.as_deref(), Some(t.project_path.as_str()));
        assert!(alpha.body.contains("https://fake.example/items/1"));
        let beta = task::load_task(items[1].path.as_ref().unwrap()).unwrap();
        assert_eq!(beta.frontmatter.status, TaskStatus::Backlog);

        let again = pull_tracker(&config, &t, &fake, false).await.unwrap();
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
        let items = pull_tracker(&config, &t, &fake, false).await.unwrap();
        let mut running = task::load_task(items[1].path.as_ref().unwrap()).unwrap();
        running.frontmatter.status = TaskStatus::Running;
        task::write_task(&running).unwrap();
        let before = std::fs::read_to_string(&running.path).unwrap();

        fake.human_edit(&a.id);
        fake.human_edit(&b.id);
        let items = pull_tracker(&config, &t, &fake, false).await.unwrap();
        assert_eq!(items[0].action, PullAction::Updated);
        assert_eq!(items[1].action, PullAction::SkippedRunning);
        let refreshed = task::load_task(items[0].path.as_ref().unwrap()).unwrap();
        assert_eq!(refreshed.frontmatter.external.unwrap().rev, Some(2));
        assert_eq!(std::fs::read_to_string(&running.path).unwrap(), before);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn remote_closed_marks_local_done() {
        let (root, config, t) = setup("closed");
        let fake = FakeTracker::new(t.clone());
        let a = fake.insert("New", "alpha", &[]);
        let items = pull_tracker(&config, &t, &fake, false).await.unwrap();
        fake.transition(&a.id, TaskStatus::Done, None).await.unwrap();
        let items2 = pull_tracker(&config, &t, &fake, false).await.unwrap();
        assert_eq!(items2[0].action, PullAction::MarkedDone);
        let doc = task::load_task(items[0].path.as_ref().unwrap()).unwrap();
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
        let items = pull_tracker(&config, &t, &fake, true).await.unwrap();
        assert_eq!(items[0].action, PullAction::Created);
        assert_eq!(count_tasks(&root), 0);
        std::fs::remove_dir_all(root).ok();
    }
}
