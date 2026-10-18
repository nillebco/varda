//! Push Varda status transitions to the issue tracker (milestone T2) and the
//! `varda tracker sync` retry.
//!
//! A push never fails or slows a run: every error lands in
//! `external.sync_error` (and stderr), and the whole push — tracker construction
//! including host credential resolution, plus the HTTP calls — is bounded.
//!
//! Ownership rule: Varda only moves an item whose remote state is still the one
//! it last saw (`external.remote_state`, recorded at import / pull refresh /
//! successful push). A different state means a human acted, so the push is
//! REFUSED, `rev` is deliberately left stale (a later push re-detects the same
//! conflict until a human resolves it, e.g. by re-pulling) and `sync_error`
//! says why.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};

use super::{IssueTracker, RemoteIssue, TrackerError, map_status, status_from_remote};
use crate::config::{Config, TrackerConfig};
use crate::task::{self, ExternalRef, TaskDocument, TaskStatus};

/// Upper bound for one whole push (build tracker + all HTTP calls).
pub const PUSH_TIMEOUT: Duration = Duration::from_secs(60);
/// Tracker comment size cap, in bytes, notice included.
pub const COMMENT_CAP: usize = 20_000;
const TRUNCATION_NOTICE: &str = "\n\n…(truncated by Varda)";

/// Cut `text` to at most `cap` bytes INCLUDING the truncation notice.
pub fn truncate_comment(text: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_owned();
    }
    let mut end = cap.saturating_sub(TRUNCATION_NOTICE.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{TRUNCATION_NOTICE}", &text[..end])
}

fn strip_front_matter(raw: &str) -> &str {
    raw.strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n").map(|(_, body)| body))
        .unwrap_or(raw)
        .trim()
}

/// The body of the first markdown section whose heading contains `name`
/// (case-insensitive), heading line included, up to the next heading of the
/// same or a higher level.
fn section(recap: &str, name: &str) -> Option<String> {
    let level = |line: &str| line.chars().take_while(|c| *c == '#').count();
    let mut out: Vec<&str> = Vec::new();
    let mut base = None;
    for line in recap.lines() {
        let l = level(line);
        let is_heading = l > 0 && line[l..].starts_with(' ');
        match base {
            None if is_heading && line.to_ascii_lowercase().contains(name) => {
                base = Some(l);
                out.push(line);
            }
            Some(b) if is_heading && l <= b => break,
            Some(_) => out.push(line),
            None => {}
        }
    }
    (!out.is_empty()).then(|| out.join("\n").trim_end().to_owned())
}

fn latest_recap(doc: &TaskDocument) -> Option<String> {
    let path = doc.frontmatter.recaps.last()?;
    let path = PathBuf::from(path);
    let path = if path.is_absolute() {
        path
    } else {
        doc.path.parent().unwrap_or_else(|| Path::new(".")).join(path)
    };
    std::fs::read_to_string(path)
        .ok()
        .map(|raw| strip_front_matter(&raw).to_owned())
}

/// The comment accompanying a transition into `doc`'s status, if any.
fn comment_for(doc: &TaskDocument) -> Option<String> {
    let fm = &doc.frontmatter;
    let text = match fm.status {
        TaskStatus::Running => {
            let run = fm.agent_session_ids.last().map_or("(unknown)", String::as_str);
            let agent = fm.assignee.as_deref().unwrap_or("an agent");
            format!("Varda started run {run} with {agent}")
        }
        TaskStatus::Review => {
            let recap = latest_recap(doc).unwrap_or_default();
            let parts: Vec<String> = ["files touched", "verification"]
                .iter()
                .filter_map(|name| section(&recap, name))
                .collect();
            let body = if parts.is_empty() {
                if recap.is_empty() {
                    "(no recap was recorded)".to_owned()
                } else {
                    recap
                }
            } else {
                parts.join("\n\n")
            };
            format!("Varda finished the run; ready for review.\n\n{body}")
        }
        TaskStatus::NeedsUser => format!(
            "Varda needs your input to continue:\n\n{}",
            latest_recap(doc).unwrap_or_else(|| "(no question was recorded)".to_owned())
        ),
        TaskStatus::Failed => format!(
            "Varda's run failed:\n\n{}",
            latest_recap(doc).unwrap_or_else(|| "(no failure summary was recorded)".to_owned())
        ),
        _ => return None,
    };
    Some(truncate_comment(&text, COMMENT_CAP))
}

enum Moved {
    /// Remote already closed and the task is done: nothing to push.
    AlreadyDone,
    Issue(RemoteIssue),
}

enum Check {
    Proceed,
    AlreadyDone,
    Refuse(String),
}

fn ownership_check(
    tracker: &TrackerConfig,
    ext: &ExternalRef,
    remote: &RemoteIssue,
    status: TaskStatus,
) -> Check {
    let remote_done =
        status_from_remote(tracker, &remote.remote_state, &remote.tags) == Some(TaskStatus::Done);
    if remote_done {
        return if status == TaskStatus::Done {
            Check::AlreadyDone
        } else {
            Check::Refuse(format!(
                "remote item is closed ('{}'); Varda will not reopen it (re-run `varda tracker pull`)",
                remote.remote_state
            ))
        };
    }
    // Baseline: the state recorded at import / last sync; older tasks fall back
    // to what the last pushed status maps to.
    let baseline = ext.remote_state.clone().or_else(|| {
        ext.synced_status
            .and_then(|s| map_status(tracker, s).state)
    });
    match baseline {
        Some(b) if !b.eq_ignore_ascii_case(&remote.remote_state) => Check::Refuse(format!(
            "remote state changed from '{b}' to '{}' since Varda last synced; not overwriting a human change (re-run `varda tracker pull` to accept it)",
            remote.remote_state
        )),
        _ => Check::Proceed,
    }
}

/// Push `doc`'s current status through `backend`, updating `doc.external`.
/// Never returns an error: failures are recorded in `sync_error`.
pub async fn push_via(
    tracker: &TrackerConfig,
    backend: &dyn IssueTracker,
    doc: &mut TaskDocument,
) {
    push_via_tracked(tracker, backend, doc, None).await
}

/// Bookkeeping confirmed by a successful transition, readable by a caller whose
/// timeout drops the push future before it finishes (see [`push_via_tracked`]).
type Progress = std::sync::Mutex<Option<ExternalRef>>;

/// [`push_via`], publishing the bookkeeping to `progress` right after the
/// transition succeeds and before the comment phase, so a timeout in the
/// comment cannot discard it.
async fn push_via_tracked(
    tracker: &TrackerConfig,
    backend: &dyn IssueTracker,
    doc: &mut TaskDocument,
    progress: Option<&Progress>,
) {
    let Some(mut ext) = doc.frontmatter.external.clone() else {
        return;
    };
    let status = doc.frontmatter.status;
    let mut attempt = 0;
    let moved: Result<Moved, String> = loop {
        let remote = match backend.get(&ext.id).await {
            Ok(remote) => remote,
            Err(e) => break Err(e.to_string()),
        };
        match ownership_check(tracker, &ext, &remote, status) {
            Check::AlreadyDone => break Ok(Moved::AlreadyDone),
            Check::Refuse(why) => break Err(format!("conflict: {why}")),
            Check::Proceed => {}
        }
        // The rev just fetched: the state is unchanged, so edits to other fields
        // do not matter; only a race after this `get` can still conflict.
        match backend.transition(&ext.id, status, Some(remote.rev)).await {
            Ok(issue) => break Ok(Moved::Issue(issue)),
            Err(TrackerError::Conflict { .. }) if attempt == 0 => attempt += 1,
            Err(e @ TrackerError::Conflict { .. }) => {
                break Err(format!("conflict: {e} (retried once)"));
            }
            Err(e) => break Err(e.to_string()),
        }
    };
    match moved {
        Err(error) => ext.sync_error = Some(error),
        Ok(Moved::AlreadyDone) => {
            ext.synced_status = Some(status);
            ext.sync_error = None;
        }
        Ok(Moved::Issue(issue)) => {
            ext.rev = Some(issue.rev);
            ext.remote_state = Some(issue.remote_state);
            ext.synced_status = Some(status);
            ext.sync_error = None;
            if let Some(progress) = progress {
                *progress.lock().unwrap_or_else(|p| p.into_inner()) = Some(ext.clone());
            }
            if let Some(text) = comment_for(doc) {
                match backend.comment(&ext.id, &text).await {
                    // A comment bumps the rev; refresh it so the next
                    // transition does not start from a stale one.
                    Ok(()) => {
                        if let Ok(fresh) = backend.get(&ext.id).await {
                            ext.rev = Some(fresh.rev);
                        }
                    }
                    Err(e) => ext.sync_error = Some(format!("comment failed: {e}")),
                }
            }
        }
    }
    if let Some(error) = &ext.sync_error {
        eprintln!(
            "tracker sync: {} {}: {error}",
            ext.backend, ext.id
        );
    }
    doc.frontmatter.external = Some(ext);
}

/// Record a failure that happened outside [`push_via`] (timeout, build error).
fn record_error(doc: &mut TaskDocument, error: String) {
    if let Some(ext) = doc.frontmatter.external.as_mut() {
        eprintln!("tracker sync: {} {}: {error}", ext.backend, ext.id);
        ext.sync_error = Some(error);
    }
}

/// Push the task's current status to its tracker. No-op (no tracker built) for a
/// task without `external`. The whole push is bounded by `bound`.
pub async fn push_transition_within(config: &Config, doc: &mut TaskDocument, bound: Duration) {
    let Some(ext) = doc.frontmatter.external.as_ref() else {
        return;
    };
    let Some(tracker) = config.trackers.iter().find(|t| t.name == ext.backend).cloned() else {
        let error = format!("tracker '{}' is not configured", ext.backend);
        return record_error(doc, error);
    };
    if let Err(e) = super::sync::check_acknowledged(config, &tracker) {
        return record_error(doc, format!("{e:#}"));
    }
    let for_build = tracker.clone();
    let build = async move {
        // Credential resolution can block (fnox / user command): run it on a
        // blocking thread, with its own deadline so the child is killed.
        tokio::task::spawn_blocking(move || super::build_tracker_within(&for_build, Some(bound)))
            .await
            .map_err(|e| format!("building tracker failed: {e}"))?
            .map_err(|e| format!("building tracker failed: {e:#}"))
    };
    push_bounded(&tracker, doc, bound, build).await
}

/// Build the backend and push, all within `bound`. If the timeout fires after a
/// successful transition, the transitioned bookkeeping is kept and only the
/// comment phase is reported as timed out.
async fn push_bounded(
    tracker: &TrackerConfig,
    doc: &mut TaskDocument,
    bound: Duration,
    build: impl std::future::Future<Output = std::result::Result<Box<dyn IssueTracker>, String>>,
) {
    let progress: Progress = std::sync::Mutex::new(None);
    let work = async {
        let backend = build.await?;
        let mut pushed = doc.clone();
        push_via_tracked(tracker, backend.as_ref(), &mut pushed, Some(&progress)).await;
        Ok::<_, String>(pushed.frontmatter.external)
    };
    match tokio::time::timeout(bound, work).await {
        Ok(Ok(external)) => doc.frontmatter.external = external,
        Ok(Err(error)) => record_error(doc, error),
        Err(_) => {
            let confirmed = progress.lock().unwrap_or_else(|p| p.into_inner()).take();
            match confirmed {
                // The transition landed remotely; keep its bookkeeping so the
                // next push sees the new baseline, and retry the comment later.
                Some(ext) => {
                    doc.frontmatter.external = Some(ext);
                    record_error(
                        doc,
                        format!(
                            "comment timed out after {}s (state already transitioned)",
                            bound.as_secs_f32()
                        ),
                    );
                }
                None => {
                    record_error(doc, format!("push timed out after {}s", bound.as_secs_f32()))
                }
            }
        }
    }
}

pub async fn push_transition(config: &Config, doc: &mut TaskDocument) {
    push_transition_within(config, doc, PUSH_TIMEOUT).await
}

/// [`push_transition`] then persist the task if its tracker bookkeeping changed.
/// The runner's single hook per status settle.
pub async fn push_and_save(config: &Config, doc: &mut TaskDocument) {
    // Never fails a run; a save failure is already printed.
    let _ = push_and_save_within(config, doc, PUSH_TIMEOUT).await;
}

/// Returns `false` if the bookkeeping changed but could not be saved.
async fn push_and_save_within(config: &Config, doc: &mut TaskDocument, bound: Duration) -> bool {
    let before = doc.frontmatter.external.clone();
    push_transition_within(config, doc, bound).await;
    if doc.frontmatter.external != before
        && let Err(e) = task::write_task(doc)
    {
        eprintln!(
            "tracker sync: could not save task {}: {e:#}",
            doc.path.display()
        );
        return false;
    }
    true
}

fn needs_sync(doc: &TaskDocument) -> bool {
    doc.frontmatter
        .external
        .as_ref()
        .is_some_and(|e| e.synced_status != Some(doc.frontmatter.status) || e.sync_error.is_some())
}

fn collect_task_paths(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_task_paths(&path, out);
        } else if path.extension().is_some_and(|x| x == "md") {
            out.push(path);
        }
    }
}

/// Retry every task whose `synced_status` differs from `status` or that carries
/// a `sync_error`. Returns the number of tasks still failing.
pub async fn sync_tasks(config: &Config, only: Option<&str>, bound: Duration) -> usize {
    let mut paths = Vec::new();
    collect_task_paths(&Path::new(&config.defaults.operations_dir).join("tasks"), &mut paths);
    paths.sort();
    let mut failing = 0;
    for path in paths {
        let Ok(mut doc) = task::load_task(&path) else {
            continue;
        };
        let backend = match doc.frontmatter.external.as_ref() {
            Some(e) if only.is_none_or(|n| e.backend == n) => e.backend.clone(),
            _ => continue,
        };
        if !needs_sync(&doc) {
            continue;
        }
        let saved = push_and_save_within(config, &mut doc, bound).await;
        let ext = doc.frontmatter.external.as_ref().expect("external");
        if !saved {
            failing += 1;
            println!("{backend}: {} pushed but the task file could not be saved", ext.id);
            continue;
        }
        match &ext.sync_error {
            None => println!("{backend}: synced {} -> {:?}", ext.id, doc.frontmatter.status),
            Some(e) => {
                failing += 1;
                println!("{backend}: {} still failing: {e}", ext.id);
            }
        }
    }
    failing
}

/// `varda tracker sync [--tracker NAME]`.
pub async fn sync_command(config: &Config, only: Option<&str>) -> Result<()> {
    if let Some(name) = only
        && !config.trackers.iter().any(|t| t.name == name)
    {
        bail!("no tracker named '{name}' is configured");
    }
    let failing = sync_tasks(config, only, PUSH_TIMEOUT).await;
    if failing > 0 {
        bail!("{failing} task(s) could not be synced");
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
        let root = std::env::temp_dir().join(format!("varda-push-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let project = root.join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let tracker = TrackerConfig {
            name: "azdo-test".to_owned(),
            kind: TrackerKind::Azdo,
            project_path: project.display().to_string(),
            credential: CredentialConfig {
                from_env: Some("VARDA_PUSH_TEST_PAT_UNSET".to_owned()),
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

    /// Import `remote` as a task the way `tracker pull` does.
    async fn imported(
        config: &Config,
        t: &TrackerConfig,
        fake: &FakeTracker,
    ) -> TaskDocument {
        let outcome = super::super::sync::pull_tracker(config, t, fake, false)
            .await
            .unwrap();
        task::load_task(&outcome.items[0].paths[0]).unwrap()
    }

    fn write_recap_file(root: &Path, text: &str) -> String {
        let path = root.join("recap.md");
        std::fs::write(&path, format!("---\ntask: x\n---\n\n{text}")).unwrap();
        path.display().to_string()
    }

    fn ext(doc: &TaskDocument) -> &ExternalRef {
        doc.frontmatter.external.as_ref().unwrap()
    }

    #[tokio::test]
    async fn each_transition_pushes_state_tags_and_comment() {
        let (root, config, t) = setup("transitions");
        let fake = FakeTracker::new(t.clone());
        let item = fake.insert("New", "alpha", &["varda:ready"]);
        let mut doc = imported(&config, &t, &fake).await;
        assert_eq!(ext(&doc).remote_state.as_deref(), Some("New"));

        doc.frontmatter.status = TaskStatus::Running;
        doc.frontmatter.assignee = Some("claude".to_owned());
        doc.frontmatter.agent_session_ids.push("run-42".to_owned());
        push_via(&t, &fake, &mut doc).await;
        let remote = fake.get(&item.id).await.unwrap();
        assert_eq!(remote.remote_state, "Active");
        assert!(remote.tags.is_empty(), "{:?}", remote.tags);
        assert_eq!(
            fake.comments()[0].1,
            "Varda started run run-42 with claude"
        );
        assert_eq!(ext(&doc).synced_status, Some(TaskStatus::Running));
        assert_eq!(ext(&doc).rev, Some(remote.rev));
        assert_eq!(ext(&doc).sync_error, None);

        doc.frontmatter.status = TaskStatus::Review;
        doc.frontmatter.recaps.push(write_recap_file(
            &root,
            "# Recap\n\nIntro text.\n\n## Files touched\n/abs/a.rs\n\n## Verification\ncargo test: ok\n\n## Next\nnothing\n",
        ));
        push_via(&t, &fake, &mut doc).await;
        assert_eq!(fake.get(&item.id).await.unwrap().remote_state, "Resolved");
        let review = &fake.comments()[1].1;
        assert!(review.contains("/abs/a.rs") && review.contains("cargo test: ok"), "{review}");
        assert!(!review.contains("nothing"), "{review}");

        doc.frontmatter.status = TaskStatus::NeedsUser;
        doc.frontmatter.recaps.push(write_recap_file(&root, "Which branch should I use?"));
        push_via(&t, &fake, &mut doc).await;
        let remote = fake.get(&item.id).await.unwrap();
        assert_eq!(remote.remote_state, "Active");
        assert!(remote.tags.contains(&"varda:needs-user".to_owned()));
        assert!(fake.comments()[2].1.contains("Which branch should I use?"));

        doc.frontmatter.status = TaskStatus::Failed;
        doc.frontmatter.recaps.push(write_recap_file(&root, "# Agent Run Failed\n\nboom"));
        push_via(&t, &fake, &mut doc).await;
        let remote = fake.get(&item.id).await.unwrap();
        assert_eq!(remote.tags, vec!["varda:failed"]);
        assert!(fake.comments()[3].1.contains("boom"));
        assert_eq!(ext(&doc).sync_error, None);
        assert_eq!(ext(&doc).synced_status, Some(TaskStatus::Failed));
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn human_closed_item_is_not_overwritten_and_refusal_is_sticky() {
        let (root, config, t) = setup("closed");
        let fake = FakeTracker::new(t.clone());
        let item = fake.insert("New", "alpha", &["varda:ready"]);
        let mut doc = imported(&config, &t, &fake).await;
        let rev_before = ext(&doc).rev;
        // A human closes the item.
        fake.set_state(&item.id, "Closed");

        doc.frontmatter.status = TaskStatus::Running;
        push_via(&t, &fake, &mut doc).await;
        let err = ext(&doc).sync_error.clone().unwrap();
        assert!(err.starts_with("conflict:"), "{err}");
        assert_eq!(fake.get(&item.id).await.unwrap().remote_state, "Closed");
        // Finding 1: the refusal must NOT adopt the fresh rev ...
        assert_eq!(ext(&doc).rev, rev_before);
        assert_eq!(ext(&doc).synced_status, None);

        // ... so a second push (e.g. `tracker sync`) still refuses.
        push_via(&t, &fake, &mut doc).await;
        assert!(ext(&doc).sync_error.as_deref().unwrap().starts_with("conflict:"));
        assert_eq!(fake.get(&item.id).await.unwrap().remote_state, "Closed");
        assert!(fake.comments().is_empty());
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn imported_task_changed_by_human_before_first_push_is_not_overwritten() {
        let (root, config, t) = setup("baseline");
        let fake = FakeTracker::new(t.clone());
        let item = fake.insert("New", "alpha", &["varda:ready"]);
        let mut doc = imported(&config, &t, &fake).await;
        assert_eq!(ext(&doc).synced_status, None);
        // New -> Resolved by a human, not Varda, and not a done state.
        fake.set_state(&item.id, "Resolved");

        doc.frontmatter.status = TaskStatus::Running;
        push_via(&t, &fake, &mut doc).await;
        let err = ext(&doc).sync_error.clone().unwrap();
        assert!(err.starts_with("conflict:") && err.contains("'New'"), "{err}");
        assert_eq!(fake.get(&item.id).await.unwrap().remote_state, "Resolved");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn repull_accepts_human_change_and_unblocks_push() {
        let (root, config, t) = setup("repull");
        let fake = FakeTracker::new(t.clone());
        let item = fake.insert("New", "alpha", &[]);
        let mut doc = imported(&config, &t, &fake).await;
        fake.set_state(&item.id, "Resolved");
        doc.frontmatter.status = TaskStatus::Running;
        push_via(&t, &fake, &mut doc).await;
        assert!(ext(&doc).sync_error.is_some());
        // Persist (as the runner would) and re-pull.
        doc.frontmatter.status = TaskStatus::Ready;
        task::write_task(&doc).unwrap();
        super::super::sync::pull_tracker(&config, &t, &fake, false).await.unwrap();
        let mut doc = task::load_task(&doc.path).unwrap();
        assert_eq!(ext(&doc).remote_state.as_deref(), Some("Resolved"));
        doc.frontmatter.status = TaskStatus::Running;
        push_via(&t, &fake, &mut doc).await;
        assert_eq!(ext(&doc).sync_error, None);
        assert_eq!(fake.get(&item.id).await.unwrap().remote_state, "Active");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn conflict_without_state_change_retries_once_with_fresh_rev() {
        let (root, config, t) = setup("retry");
        let fake = FakeTracker::new(t.clone());
        let item = fake.insert("New", "alpha", &[]);
        let mut doc = imported(&config, &t, &fake).await;
        fake.fail_next_transition_with_conflict();
        doc.frontmatter.status = TaskStatus::Running;
        push_via(&t, &fake, &mut doc).await;
        assert_eq!(ext(&doc).sync_error, None);
        assert_eq!(fake.get(&item.id).await.unwrap().remote_state, "Active");
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn push_failure_records_sync_error_and_sync_retries_and_clears_it() {
        let (root, config, t) = setup("sync");
        let fake = FakeTracker::new(t.clone());
        let item = fake.insert("New", "alpha", &[]);
        let mut doc = imported(&config, &t, &fake).await;
        // Remote vanished: the push fails, the task keeps its own status.
        let gone = FakeTracker::new(t.clone());
        doc.frontmatter.status = TaskStatus::Running;
        push_via(&t, &gone, &mut doc).await;
        assert_eq!(doc.frontmatter.status, TaskStatus::Running);
        assert!(ext(&doc).sync_error.is_some());
        assert_eq!(ext(&doc).synced_status, None);
        task::write_task(&doc).unwrap();

        // `sync` retries through the real build path; the credential env is unset
        // here, so it fails visibly and keeps the error.
        let failing = sync_tasks(&config, Some("azdo-test"), Duration::from_secs(5)).await;
        assert_eq!(failing, 1);
        let saved = task::load_task(&doc.path).unwrap();
        assert!(ext(&saved).sync_error.as_deref().unwrap().contains("credential"), "{:?}", ext(&saved));

        // With a working backend the retry succeeds and clears the error.
        let mut saved = saved;
        push_via(&t, &fake, &mut saved).await;
        assert_eq!(ext(&saved).sync_error, None);
        assert_eq!(ext(&saved).synced_status, Some(TaskStatus::Running));
        assert_eq!(fake.get(&item.id).await.unwrap().remote_state, "Active");
        assert!(!needs_sync(&saved));
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn task_without_external_is_untouched_and_builds_no_tracker() {
        let (root, mut config, t) = setup("noext");
        // A tracker whose credential would hang if anything tried to build it.
        config.trackers[0].credential = CredentialConfig {
            command: Some("sleep 30".to_owned()),
            ..CredentialConfig::default()
        };
        let path = task::create_task(&config, "plain", Path::new(&t.project_path), None, None, None)
            .unwrap();
        let mut doc = task::load_task(&path).unwrap();
        doc.frontmatter.status = TaskStatus::Running;
        let before = doc.clone();
        let started = std::time::Instant::now();
        push_and_save(&config, &mut doc).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(doc.frontmatter, before.frontmatter);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn hanging_credential_command_times_out_within_the_bound() {
        let (root, mut config, t) = setup("hang");
        config.trackers[0].credential = CredentialConfig {
            command: Some("sleep 30".to_owned()),
            ..CredentialConfig::default()
        };
        let fake = FakeTracker::new(t.clone());
        fake.insert("New", "alpha", &[]);
        let mut doc = {
            let mut cfg = config.clone();
            cfg.trackers[0].credential = t.credential.clone();
            imported(&cfg, &t, &fake).await
        };
        doc.frontmatter.status = TaskStatus::Running;
        let started = std::time::Instant::now();
        push_transition_within(&config, &mut doc, Duration::from_millis(500)).await;
        assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
        let err = ext(&doc).sync_error.clone().unwrap();
        assert!(err.contains("timed out"), "{err}");
        assert_eq!(doc.frontmatter.status, TaskStatus::Running);
        std::fs::remove_dir_all(root).ok();
    }

    /// Wraps a backend; `comment` sleeps past any sane bound.
    struct SlowComment(std::sync::Arc<FakeTracker>);

    #[async_trait::async_trait]
    impl IssueTracker for SlowComment {
        fn name(&self) -> &str {
            self.0.name()
        }
        async fn query(&self, f: &crate::config::TrackerFilter) -> super::super::Result<Vec<RemoteIssue>> {
            self.0.query(f).await
        }
        async fn get(&self, id: &str) -> super::super::Result<RemoteIssue> {
            self.0.get(id).await
        }
        async fn create(&self, d: &super::super::IssueDraft) -> super::super::Result<RemoteIssue> {
            self.0.create(d).await
        }
        async fn transition(
            &self,
            id: &str,
            to: TaskStatus,
            rev: Option<u64>,
        ) -> super::super::Result<RemoteIssue> {
            self.0.transition(id, to, rev).await
        }
        async fn comment(&self, _id: &str, _m: &str) -> super::super::Result<()> {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        }
        async fn assign(&self, id: &str, who: Option<&str>) -> super::super::Result<()> {
            self.0.assign(id, who).await
        }
    }

    #[tokio::test]
    async fn comment_timeout_after_transition_keeps_transitioned_bookkeeping() {
        let (root, config, t) = setup("commenttimeout");
        let fake = std::sync::Arc::new(FakeTracker::new(t.clone()));
        let item = fake.insert("New", "alpha", &[]);
        let mut doc = imported(&config, &t, &fake).await;
        doc.frontmatter.status = TaskStatus::Running;
        doc.frontmatter.assignee = Some("claude".to_owned());
        doc.frontmatter.agent_session_ids.push("run-1".to_owned());

        let slow = SlowComment(fake.clone());
        let boxed: Box<dyn IssueTracker> = Box::new(slow);
        push_bounded(&t, &mut doc, Duration::from_millis(300), async { Ok(boxed) }).await;
        assert_eq!(fake.get(&item.id).await.unwrap().remote_state, "Active");
        assert_eq!(ext(&doc).remote_state.as_deref(), Some("Active"));
        assert_eq!(ext(&doc).synced_status, Some(TaskStatus::Running));
        assert!(ext(&doc).sync_error.as_deref().unwrap().contains("comment timed out"));

        // The retry is not treated as a human change.
        push_via(&t, &*fake, &mut doc).await;
        assert_eq!(ext(&doc).sync_error, None);
        std::fs::remove_dir_all(root).ok();
    }

    #[tokio::test]
    async fn save_failure_is_reported_and_counted_by_sync() {
        let (root, config, t) = setup("savefail");
        let fake = FakeTracker::new(t.clone());
        fake.insert("New", "alpha", &[]);
        let mut doc = imported(&config, &t, &fake).await;
        let dir = doc.path.parent().unwrap().to_path_buf();
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        use std::os::unix::fs::PermissionsExt;
        perms.set_mode(0o555);
        std::fs::set_permissions(&dir, perms.clone()).unwrap();
        let writable = std::fs::write(dir.join(".probe"), "x").is_ok();
        if writable {
            // Running as root: cannot make the directory unwritable.
            std::fs::remove_file(dir.join(".probe")).ok();
            perms.set_mode(0o755);
            std::fs::set_permissions(&dir, perms).unwrap();
            std::fs::remove_dir_all(root).ok();
            return;
        }
        // Bookkeeping change only, no remote needed: unconfigured tracker.
        doc.frontmatter.status = TaskStatus::Running;
        let mut cfg = config.clone();
        cfg.trackers.clear();
        let saved = push_and_save_within(&cfg, &mut doc, Duration::from_secs(1)).await;
        perms.set_mode(0o755);
        std::fs::set_permissions(&dir, perms).unwrap();
        assert!(!saved);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn truncated_comment_never_exceeds_the_cap() {
        for text in ["x".repeat(25_000), "é".repeat(15_000), "é".repeat(10_001)] {
            let out = truncate_comment(&text, COMMENT_CAP);
            assert!(out.len() <= COMMENT_CAP, "{}", out.len());
            assert!(out.ends_with(TRUNCATION_NOTICE));
        }
        assert_eq!(truncate_comment("short", COMMENT_CAP), "short");
        let exact = "y".repeat(COMMENT_CAP);
        assert_eq!(truncate_comment(&exact, COMMENT_CAP), exact);
    }
}
