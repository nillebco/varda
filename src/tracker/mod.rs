//! Issue-tracker seam (milestone T0): the [`IssueTracker`] trait, the status
//! mapping between Varda's `TaskStatus` and a tracker's states/tags, and an
//! in-memory [`FakeTracker`] for tests. No network code lives here; concrete
//! backends (Azure DevOps first) implement the trait in later milestones.

// Nothing outside tests consumes this seam yet (T1 wires the first backend).
#![allow(dead_code)]

pub mod azdo;
pub mod doctor;
pub mod github;
pub mod push;
pub mod setup;
pub mod sync;

use std::collections::BTreeMap;
use std::fmt;

use anyhow::Context as _;
use async_trait::async_trait;

use crate::config::{Config, TrackerConfig, TrackerFilter, TrackerKind};
use crate::task::TaskStatus;

/// A work item as read from a tracker.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteIssue {
    pub id: String,
    pub url: String,
    pub rev: u64,
    pub title: String,
    pub body_markdown: String,
    pub remote_state: String,
    pub assignee: Option<String>,
    pub tags: Vec<String>,
    pub fields: BTreeMap<String, serde_json::Value>,
}

/// What `create` needs to open a new work item.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IssueDraft {
    pub title: String,
    pub body_markdown: String,
    pub tags: Vec<String>,
    pub assignee: Option<String>,
    /// Backend work-item type (Azure DevOps: `Task`, `Bug`, ...); `None` ⇒ the
    /// tracker's configured default.
    pub work_item_type: Option<String>,
}

/// Tracker failures. A revision conflict is distinguished from every other
/// failure so callers can refresh and re-decide instead of clobbering a human edit.
#[derive(Debug)]
pub enum TrackerError {
    /// The item changed since `expected` (optimistic-concurrency check failed).
    Conflict {
        id: String,
        expected: Option<u64>,
        actual: Option<u64>,
    },
    NotFound(String),
    Other(anyhow::Error),
}

impl TrackerError {
    pub fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict { .. })
    }
}

impl fmt::Display for TrackerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict {
                id,
                expected,
                actual,
            } => write!(
                f,
                "revision conflict on '{id}': expected rev {expected:?}, remote is at {actual:?}"
            ),
            Self::NotFound(id) => write!(f, "work item '{id}' not found"),
            Self::Other(error) => write!(f, "{error:#}"),
        }
    }
}

impl std::error::Error for TrackerError {}

impl From<anyhow::Error> for TrackerError {
    fn from(error: anyhow::Error) -> Self {
        Self::Other(error)
    }
}

pub type Result<T> = std::result::Result<T, TrackerError>;

#[async_trait]
pub trait IssueTracker: Send + Sync {
    fn name(&self) -> &str;
    /// Candidate work items matching `filter`, for import.
    async fn query(&self, filter: &TrackerFilter) -> Result<Vec<RemoteIssue>>;
    async fn get(&self, id: &str) -> Result<RemoteIssue>;
    async fn create(&self, draft: &IssueDraft) -> Result<RemoteIssue>;
    /// Move the item to the state mapped for `to`. With `expected_rev`, fails
    /// with [`TrackerError::Conflict`] if the item changed in between.
    async fn transition(
        &self,
        id: &str,
        to: TaskStatus,
        expected_rev: Option<u64>,
    ) -> Result<RemoteIssue>;
    async fn comment(&self, id: &str, markdown: &str) -> Result<()>;
    async fn assign(&self, id: &str, who: Option<&str>) -> Result<()>;
    /// Cheap authenticated call proving the credential works (`tracker list`).
    /// Backends without a dedicated endpoint accept by default.
    async fn check_auth(&self) -> Result<()> {
        Ok(())
    }
    /// State names valid for the configured work-item type, so `doctor` can
    /// validate the `status_map`. `None` ⇒ the backend cannot enumerate them.
    async fn valid_states(&self) -> Result<Option<Vec<String>>> {
        Ok(None)
    }
    /// Name of the project's process template (`tracker setup` proposes a
    /// `status_map` from it). `None` ⇒ the backend has no such notion.
    async fn process_template(&self) -> Result<Option<String>> {
        Ok(None)
    }
    /// Which of the `wanted` labels do not exist on the remote yet (`tracker
    /// setup`). Backends without a label catalogue report none missing.
    async fn missing_labels(&self, _wanted: &[String]) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
    /// Create the given labels on the remote; already-existing ones are fine.
    async fn create_labels(&self, _names: &[String]) -> Result<()> {
        Err(TrackerError::Other(anyhow::anyhow!(
            "this tracker cannot create labels"
        )))
    }
}

/// Build the backend for a (central-config) `[[trackers]]` entry, resolving its
/// credential on the HOST. The token never leaves this process.
pub fn build_tracker(
    tracker: &TrackerConfig,
    _config: &Config,
) -> anyhow::Result<Box<dyn IssueTracker>> {
    build_tracker_within(tracker, None)
}

/// [`build_tracker`] whose credential command (if any) is killed after `timeout`.
pub fn build_tracker_within(
    tracker: &TrackerConfig,
    timeout: Option<std::time::Duration>,
) -> anyhow::Result<Box<dyn IssueTracker>> {
    match tracker.kind {
        TrackerKind::Azdo => {
            let pat = resolve_tracker_credential(tracker, timeout)?;
            Ok(Box::new(azdo::AzdoTracker::from_config(tracker, pat)?))
        }
        TrackerKind::Github => {
            let token = resolve_tracker_credential(tracker, timeout)?;
            Ok(Box::new(github::GithubTracker::from_config(tracker, token)?))
        }
        other => anyhow::bail!("tracker kind {other:?} is not implemented yet"),
    }
}

fn resolve_tracker_credential(
    tracker: &TrackerConfig,
    timeout: Option<std::time::Duration>,
) -> anyhow::Result<String> {
    let source = tracker.credential.source()?;
    crate::resolve_credential_value_within(&source, tracker.credential.optional, timeout)
        .with_context(|| format!("resolving credential for tracker '{}'", tracker.name))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "credential for tracker '{}' resolved to nothing (unset/empty)",
                tracker.name
            )
        })
}

// ---------------------------------------------------------------------------
// Status mapping
// ---------------------------------------------------------------------------

/// Helper tags disambiguating Varda statuses that share one tracker state.
const HELPER_TAGS: &[(TaskStatus, &str)] = &[
    (TaskStatus::Ready, "varda:ready"),
    (TaskStatus::NeedsUser, "varda:needs-user"),
    (TaskStatus::Failed, "varda:failed"),
];

fn helper_tag(status: TaskStatus) -> Option<&'static str> {
    HELPER_TAGS
        .iter()
        .find(|(s, _)| *s == status)
        .map(|(_, tag)| *tag)
}

/// The remote side of a Varda status change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedState {
    /// Remote state to move to; `None` when the tracker's `status_map` has no
    /// entry for the status (leave the remote state alone).
    pub state: Option<String>,
    pub add_tags: Vec<String>,
    pub remove_tags: Vec<String>,
}

/// Map a Varda status onto the tracker. `ready`/`needs_user`/`failed` add their
/// `varda:<status>` helper tag; any other status removes stale helper tags, and
/// a helper status removes the other helpers.
pub fn map_status(tracker: &TrackerConfig, status: TaskStatus) -> MappedState {
    let own = helper_tag(status);
    MappedState {
        state: tracker.status_map.get(status.as_str()).cloned(),
        add_tags: own.map(str::to_owned).into_iter().collect(),
        remove_tags: HELPER_TAGS
            .iter()
            .map(|(_, tag)| *tag)
            .filter(|tag| Some(*tag) != own)
            .map(str::to_owned)
            .collect(),
    }
}

/// Reverse of [`map_status`]: which Varda status does a remote state (plus its
/// tags) denote? A `varda:<status>` helper tag wins when the state matches that
/// status's mapped state. Otherwise the untagged candidates are tried in order
/// backlog, ready, running, review, done — `ready` comes AFTER `backlog` so a
/// shared state (Agile's `New`) resolves to backlog, yet a state mapped only by
/// `ready` (Scrum's `Approved`) still resolves to ready. State comparison is
/// case-insensitive.
pub fn status_from_remote(
    tracker: &TrackerConfig,
    remote_state: &str,
    tags: &[String],
) -> Option<TaskStatus> {
    let state_is = |status: TaskStatus| {
        tracker
            .status_map
            .get(status.as_str())
            .is_some_and(|mapped| {
                mapped.eq_ignore_ascii_case(remote_state)
                    // GitHub: a bare `closed` accepts any `closed:<reason>`.
                    || (mapped.eq_ignore_ascii_case("closed")
                        && remote_state.to_ascii_lowercase().starts_with("closed:"))
            })
    };
    for (status, tag) in HELPER_TAGS {
        if tags.iter().any(|t| t.eq_ignore_ascii_case(tag)) && state_is(*status) {
            return Some(*status);
        }
    }
    [
        TaskStatus::Backlog,
        TaskStatus::Ready,
        TaskStatus::Running,
        TaskStatus::Review,
        TaskStatus::Done,
    ]
    .into_iter()
    .find(|status| state_is(*status))
}

/// Built-in `status_map` defaults per Azure DevOps process template, keyed by
/// `TaskStatus::as_str()`. Used by `tracker setup` later. In Scrum `review` shares
/// `Committed` with `running` (the process has no spare state), and in Basic
/// `review` shares `Doing`; the reverse mapping resolves those to `running`.
pub const AZDO_AGILE_STATUS_MAP: &[(&str, &str)] = &[
    ("backlog", "New"),
    ("ready", "New"),
    ("running", "Active"),
    ("review", "Resolved"),
    ("needs_user", "Active"),
    ("failed", "Active"),
    ("done", "Closed"),
];
pub const AZDO_SCRUM_STATUS_MAP: &[(&str, &str)] = &[
    ("backlog", "New"),
    ("ready", "Approved"),
    ("running", "Committed"),
    ("review", "Committed"),
    ("needs_user", "Committed"),
    ("failed", "Committed"),
    ("done", "Done"),
];
pub const AZDO_BASIC_STATUS_MAP: &[(&str, &str)] = &[
    ("backlog", "To Do"),
    ("ready", "To Do"),
    ("running", "Doing"),
    ("review", "Doing"),
    ("needs_user", "Doing"),
    ("failed", "Doing"),
    ("done", "Done"),
];
pub const AZDO_CMMI_STATUS_MAP: &[(&str, &str)] = &[
    ("backlog", "Proposed"),
    ("ready", "Proposed"),
    ("running", "Active"),
    ("review", "Resolved"),
    ("needs_user", "Active"),
    ("failed", "Active"),
    ("done", "Closed"),
];

/// Default GitHub Issues `status_map`: issues only open/close, so everything but
/// `done` stays open and the `varda:*` helper labels carry the rest.
pub const GITHUB_STATUS_MAP: &[(&str, &str)] = &[
    ("backlog", "open"),
    ("ready", "open"),
    ("running", "open"),
    ("review", "open"),
    ("needs_user", "open"),
    ("failed", "open"),
    ("done", "closed:completed"),
];

/// Owned form of one of the default maps above.
pub fn status_map_from(defaults: &[(&str, &str)]) -> BTreeMap<String, String> {
    defaults
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

// ---------------------------------------------------------------------------
// In-memory fake
// ---------------------------------------------------------------------------

#[cfg(test)]
pub mod fake {
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct State {
        issues: BTreeMap<String, RemoteIssue>,
        comments: Vec<(String, String)>,
        next_id: u64,
        /// The next `transition` fails with a conflict regardless of the rev,
        /// and bumps the remote rev as a concurrent human edit would.
        conflict_next: bool,
        valid_states: Option<Vec<String>>,
    }

    /// In-memory tracker with rev bumping and conflict simulation.
    pub struct FakeTracker {
        config: TrackerConfig,
        state: Mutex<State>,
    }

    impl FakeTracker {
        pub fn new(config: TrackerConfig) -> Self {
            Self {
                config,
                state: Mutex::new(State {
                    next_id: 1,
                    ..State::default()
                }),
            }
        }

        pub fn insert(&self, state: &str, title: &str, tags: &[&str]) -> RemoteIssue {
            let mut s = self.state.lock().unwrap();
            let id = s.next_id.to_string();
            s.next_id += 1;
            let issue = RemoteIssue {
                url: format!("https://fake.example/items/{id}"),
                id: id.clone(),
                rev: 1,
                title: title.to_owned(),
                body_markdown: String::new(),
                remote_state: state.to_owned(),
                assignee: None,
                tags: tags.iter().map(|t| (*t).to_owned()).collect(),
                fields: BTreeMap::new(),
            };
            s.issues.insert(id, issue.clone());
            issue
        }

        /// Simulate a human editing the item: bumps its rev.
        pub fn human_edit(&self, id: &str) {
            let mut s = self.state.lock().unwrap();
            if let Some(issue) = s.issues.get_mut(id) {
                issue.rev += 1;
            }
        }

        /// Simulate a human moving the item to `state` (bumps the rev).
        pub fn set_state(&self, id: &str, state: &str) {
            let mut s = self.state.lock().unwrap();
            if let Some(issue) = s.issues.get_mut(id) {
                issue.remote_state = state.to_owned();
                issue.rev += 1;
            }
        }

        pub fn set_valid_states(&self, states: &[&str]) {
            self.state.lock().unwrap().valid_states =
                Some(states.iter().map(|s| (*s).to_owned()).collect());
        }

        pub fn fail_next_transition_with_conflict(&self) {
            self.state.lock().unwrap().conflict_next = true;
        }

        pub fn comments(&self) -> Vec<(String, String)> {
            self.state.lock().unwrap().comments.clone()
        }
    }

    #[async_trait]
    impl IssueTracker for FakeTracker {
        fn name(&self) -> &str {
            &self.config.name
        }

        async fn query(&self, filter: &TrackerFilter) -> Result<Vec<RemoteIssue>> {
            let s = self.state.lock().unwrap();
            Ok(s.issues
                .values()
                .filter(|i| {
                    filter.tags.iter().all(|t| i.tags.contains(t))
                        && (filter.states.is_empty() || filter.states.contains(&i.remote_state))
                })
                .cloned()
                .collect())
        }

        async fn get(&self, id: &str) -> Result<RemoteIssue> {
            let s = self.state.lock().unwrap();
            s.issues
                .get(id)
                .cloned()
                .ok_or_else(|| TrackerError::NotFound(id.to_owned()))
        }

        async fn create(&self, draft: &IssueDraft) -> Result<RemoteIssue> {
            let backlog = map_status(&self.config, TaskStatus::Backlog)
                .state
                .unwrap_or_default();
            let tags: Vec<&str> = draft.tags.iter().map(String::as_str).collect();
            let created = self.insert(&backlog, &draft.title, &tags);
            let mut s = self.state.lock().unwrap();
            let issue = s.issues.get_mut(&created.id).unwrap();
            issue.body_markdown = draft.body_markdown.clone();
            issue.assignee = draft.assignee.clone();
            Ok(issue.clone())
        }

        async fn transition(
            &self,
            id: &str,
            to: TaskStatus,
            expected_rev: Option<u64>,
        ) -> Result<RemoteIssue> {
            let mapped = map_status(&self.config, to);
            let mut s = self.state.lock().unwrap();
            let forced = std::mem::take(&mut s.conflict_next);
            let issue = s
                .issues
                .get_mut(id)
                .ok_or_else(|| TrackerError::NotFound(id.to_owned()))?;
            if forced {
                issue.rev += 1;
            }
            if forced || expected_rev.is_some_and(|rev| rev != issue.rev) {
                return Err(TrackerError::Conflict {
                    id: id.to_owned(),
                    expected: expected_rev,
                    actual: Some(issue.rev),
                });
            }
            if let Some(state) = mapped.state {
                issue.remote_state = state;
            }
            // Helper tags match case-insensitively, like `status_from_remote`.
            issue.tags.retain(|t| {
                !mapped
                    .remove_tags
                    .iter()
                    .any(|r| r.eq_ignore_ascii_case(t))
            });
            for tag in mapped.add_tags {
                if !issue.tags.iter().any(|t| t.eq_ignore_ascii_case(&tag)) {
                    issue.tags.push(tag);
                }
            }
            issue.rev += 1;
            Ok(issue.clone())
        }

        async fn comment(&self, id: &str, markdown: &str) -> Result<()> {
            let mut s = self.state.lock().unwrap();
            if !s.issues.contains_key(id) {
                return Err(TrackerError::NotFound(id.to_owned()));
            }
            s.comments.push((id.to_owned(), markdown.to_owned()));
            Ok(())
        }

        async fn valid_states(&self) -> Result<Option<Vec<String>>> {
            Ok(self.state.lock().unwrap().valid_states.clone())
        }

        async fn assign(&self, id: &str, who: Option<&str>) -> Result<()> {
            let mut s = self.state.lock().unwrap();
            let issue = s
                .issues
                .get_mut(id)
                .ok_or_else(|| TrackerError::NotFound(id.to_owned()))?;
            issue.assignee = who.map(str::to_owned);
            issue.rev += 1;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::FakeTracker;
    use super::*;
    use crate::config::CredentialConfig;

    pub(crate) fn tracker_with(map: &[(&str, &str)]) -> TrackerConfig {
        TrackerConfig {
            name: "azdo-test".to_owned(),
            kind: crate::config::TrackerKind::Azdo,
            project_path: "/tmp/proj".to_owned(),
            credential: CredentialConfig {
                from_env: Some("PAT".to_owned()),
                ..CredentialConfig::default()
            },
            filter: TrackerFilter::default(),
            status_map: status_map_from(map),
            acknowledged: None,
            extra: BTreeMap::new(),
        }
    }

    const ALL: [TaskStatus; 7] = [
        TaskStatus::Backlog,
        TaskStatus::Ready,
        TaskStatus::Running,
        TaskStatus::Review,
        TaskStatus::NeedsUser,
        TaskStatus::Failed,
        TaskStatus::Done,
    ];

    #[test]
    fn map_status_adds_helper_tag_and_removes_stale_ones() {
        let t = tracker_with(AZDO_AGILE_STATUS_MAP);
        let m = map_status(&t, TaskStatus::NeedsUser);
        assert_eq!(m.state.as_deref(), Some("Active"));
        assert_eq!(m.add_tags, vec!["varda:needs-user"]);
        assert_eq!(m.remove_tags, vec!["varda:ready", "varda:failed"]);

        let m = map_status(&t, TaskStatus::Review);
        assert_eq!(m.state.as_deref(), Some("Resolved"));
        assert!(m.add_tags.is_empty());
        assert_eq!(m.remove_tags.len(), 3);

        let m = map_status(&t, TaskStatus::Ready);
        assert_eq!(m.add_tags, vec!["varda:ready"]);
        assert!(!m.remove_tags.contains(&"varda:ready".to_owned()));
    }

    #[test]
    fn map_status_unmapped_status_leaves_state_alone() {
        let t = tracker_with(&[("done", "Closed")]);
        assert_eq!(map_status(&t, TaskStatus::Running).state, None);
    }

    #[test]
    fn default_maps_round_trip_through_status_from_remote() {
        for map in [
            AZDO_AGILE_STATUS_MAP,
            AZDO_SCRUM_STATUS_MAP,
            AZDO_BASIC_STATUS_MAP,
        ] {
            let t = tracker_with(map);
            for status in ALL {
                let m = map_status(&t, status);
                let back = status_from_remote(&t, m.state.as_deref().unwrap(), &m.add_tags);
                match status {
                    // Helper-tagged statuses always round-trip exactly (Ready
                    // shares a state with Backlog on Agile/Basic; its tag wins).
                    TaskStatus::Ready | TaskStatus::NeedsUser | TaskStatus::Failed => {
                        assert_eq!(back, Some(status), "{map:?}")
                    }
                    // Untagged: the first candidate sharing the state wins, so
                    // the result maps to the same remote state.
                    _ => {
                        let back = back.expect("mapped state resolves");
                        assert_eq!(
                            map_status(&t, back).state,
                            m.state,
                            "{status:?} via {map:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn status_from_remote_agile_shared_new_is_backlog() {
        let t = tracker_with(AZDO_AGILE_STATUS_MAP);
        assert_eq!(status_from_remote(&t, "New", &[]), Some(TaskStatus::Backlog));
        // The helper tag promotes it to ready.
        assert_eq!(
            status_from_remote(&t, "New", &["varda:ready".to_owned()]),
            Some(TaskStatus::Ready)
        );
        assert_eq!(
            status_from_remote(&t, "active", &["varda:failed".to_owned()]),
            Some(TaskStatus::Failed)
        );
        assert_eq!(
            status_from_remote(&t, "Active", &[]),
            Some(TaskStatus::Running)
        );
        assert_eq!(status_from_remote(&t, "Removed", &[]), None);
    }

    #[test]
    fn status_from_remote_scrum_approved_is_untagged_ready() {
        let t = tracker_with(AZDO_SCRUM_STATUS_MAP);
        assert_eq!(
            status_from_remote(&t, "Approved", &[]),
            Some(TaskStatus::Ready)
        );
        assert_eq!(status_from_remote(&t, "New", &[]), Some(TaskStatus::Backlog));
        assert_eq!(
            status_from_remote(&t, "Committed", &[]),
            Some(TaskStatus::Running)
        );
    }

    #[test]
    fn status_from_remote_basic() {
        let t = tracker_with(AZDO_BASIC_STATUS_MAP);
        assert_eq!(status_from_remote(&t, "To Do", &[]), Some(TaskStatus::Backlog));
        assert_eq!(status_from_remote(&t, "Doing", &[]), Some(TaskStatus::Running));
        assert_eq!(status_from_remote(&t, "Done", &[]), Some(TaskStatus::Done));
    }

    #[tokio::test]
    async fn fake_tracker_bumps_rev_and_reports_conflict() {
        let t = FakeTracker::new(tracker_with(AZDO_AGILE_STATUS_MAP));
        let item = t.insert("New", "do it", &["varda"]);
        assert_eq!(item.rev, 1);

        let moved = t
            .transition(&item.id, TaskStatus::NeedsUser, Some(1))
            .await
            .expect("matching rev succeeds");
        assert_eq!(moved.rev, 2);
        assert_eq!(moved.remote_state, "Active");
        assert!(moved.tags.contains(&"varda:needs-user".to_owned()));

        // Moving on removes the stale helper tag.
        let moved = t
            .transition(&item.id, TaskStatus::Review, Some(2))
            .await
            .unwrap();
        assert!(!moved.tags.iter().any(|t| t.starts_with("varda:")));

        // A stale rev is a Conflict, distinct from NotFound.
        t.human_edit(&item.id);
        let err = t
            .transition(&item.id, TaskStatus::Done, Some(3))
            .await
            .unwrap_err();
        assert!(err.is_conflict(), "{err}");
        assert_eq!(t.get(&item.id).await.unwrap().remote_state, "Resolved");

        // Forced conflict simulation.
        t.fail_next_transition_with_conflict();
        let err = t.transition(&item.id, TaskStatus::Done, None).await.unwrap_err();
        assert!(err.is_conflict());
        assert!(!t.transition("nope", TaskStatus::Done, None).await.unwrap_err().is_conflict());

        t.comment(&item.id, "recap").await.unwrap();
        assert_eq!(t.comments(), vec![(item.id.clone(), "recap".to_owned())]);
    }

    #[tokio::test]
    async fn fake_tracker_removes_helper_tags_case_insensitively() {
        let t = FakeTracker::new(tracker_with(AZDO_AGILE_STATUS_MAP));
        let item = t.insert("Active", "x", &["VARDA:NEEDS-USER"]);
        assert_eq!(
            status_from_remote(&tracker_with(AZDO_AGILE_STATUS_MAP), "Active", &item.tags),
            Some(TaskStatus::NeedsUser)
        );
        let moved = t
            .transition(&item.id, TaskStatus::Running, None)
            .await
            .unwrap();
        assert!(moved.tags.is_empty(), "{:?}", moved.tags);
        assert_ne!(
            status_from_remote(
                &tracker_with(AZDO_AGILE_STATUS_MAP),
                &moved.remote_state,
                &moved.tags
            ),
            Some(TaskStatus::NeedsUser)
        );

        // Dedup is case-insensitive too.
        let item = t.insert("New", "y", &["VARDA:READY"]);
        let moved = t.transition(&item.id, TaskStatus::Ready, None).await.unwrap();
        assert_eq!(moved.tags.len(), 1, "{:?}", moved.tags);
    }

    #[tokio::test]
    async fn fake_tracker_query_filters_and_create_works() {
        let t = FakeTracker::new(tracker_with(AZDO_AGILE_STATUS_MAP));
        t.insert("New", "a", &["varda"]);
        t.insert("New", "b", &[]);
        t.insert("Closed", "c", &["varda"]);
        let filter = TrackerFilter {
            tags: vec!["varda".to_owned()],
            states: vec!["New".to_owned()],
            ..TrackerFilter::default()
        };
        let found = t.query(&filter).await.unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "a");

        let created = t
            .create(&IssueDraft {
                title: "new".to_owned(),
                body_markdown: "body".to_owned(),
                ..IssueDraft::default()
            })
            .await
            .unwrap();
        assert_eq!(created.remote_state, "New");
        assert_eq!(created.body_markdown, "body");
        t.assign(&created.id, Some("me")).await.unwrap();
        assert_eq!(t.get(&created.id).await.unwrap().assignee.as_deref(), Some("me"));
    }
}
