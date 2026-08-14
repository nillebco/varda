//! Markdown task parsing and updates.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use gray_matter::{Matter, engine::YAML};
use serde::{Deserialize, Serialize};

use crate::config::Config;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskDocument {
    pub path: PathBuf,
    pub frontmatter: TaskFrontmatter,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSummary {
    pub path: PathBuf,
    pub id: Option<u64>,
    pub status: TaskStatus,
    pub project: Option<String>,
    pub assignee: Option<String>,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskFrontmatter {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u64>,
    // Runtime STATE. A repo-local `.varda/tasks/<id>-<slug>.md` DEFINITION omits
    // this field (status is control-plane state, never committed to the code
    // repo), so it defaults to `backlog` when such a definition is loaded.
    #[serde(default = "default_status")]
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    /// Task-pinned sandbox override. When set (via `varda task add --sandbox
    /// <NAME>`), the run path resolves the sandbox to the central
    /// `[sandboxes.<NAME>]` with HIGHEST precedence — above the nearest `.varda`,
    /// the matched route, and `defaults.sandbox`. `"local"` selects the identity
    /// provider. See `config::Config::resolve_sandbox_for`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    // Legacy single-recap field; kept for backward-compat deserialization only.
    // Migrated to `recaps` on load. Not serialized.
    #[serde(default, skip_serializing)]
    pub recap: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recaps: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    // Legacy single-value fields; kept for backward-compat deserialization only.
    // Migrated to `agent_session_ids`/`agent_session_logs` on load. Not serialized.
    #[serde(default, skip_serializing)]
    pub agent_session_id: Option<String>,
    #[serde(default, skip_serializing)]
    pub agent_session_log: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_session_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_session_logs: Vec<String>,
    /// Resume commands captured from interactive runs, parallel to `agent_session_ids`.
    /// Populated from the agent's `resume_command_template` after the interactive session
    /// ends and the agent's own session id has been discovered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_resume_commands: Vec<String>,
    /// M12 per-task capability allowlist. Each entry is either a bare command
    /// name (e.g. `"msb"`, `"docker"`, `"cargo"`) or a full agent tool pattern
    /// (e.g. `"Bash(cargo test:*)"`). Varda translates these into the agent
    /// backend's permission config for the run (Claude Code
    /// `permissions.allow`), so a headless run — which has no interactive
    /// approver — can execute exactly these commands without a prompt. It is
    /// NOT a blanket bypass: only the declared commands are pre-authorized.
    /// See the `capability` module and the README "Per-task capability
    /// allowlist" section.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_commands: Vec<String>,
    /// M10 per-task overrides for the four cooperative-execution bounds. Each is
    /// optional and, when set, wins over the corresponding `defaults.*` value
    /// (see `runner::OperationBounds::resolve`). Flattened so the keys appear at
    /// the top level of the task frontmatter (e.g. `idle_timeout: 300`).
    #[serde(flatten)]
    pub bounds: TaskBounds,
    #[serde(default)]
    pub requires_user: bool,
}

impl TaskFrontmatter {
    /// A copy with varda's internal run-bookkeeping cleared, for injection into an
    /// agent prompt. `recaps` / `agent_session_logs` are HOST filesystem paths
    /// (under `<varda_home>/operations/…`) that do not exist inside a sandbox —
    /// injecting them lures the agent into `ls`/`cat` of unreachable paths (and
    /// grows the prompt unboundedly as a task accumulates runs). The agent gets the
    /// task BODY and the fields that actually describe the work (id/status/project/
    /// assignee/sandbox/plan/allow_commands/bounds); its own prior recaps are not
    /// its concern. Applied uniformly (sandboxed or not) since these paths are
    /// never useful handed to the agent.
    pub fn sanitized_for_prompt(&self) -> Self {
        let mut fm = self.clone();
        fm.recap = None;
        fm.recaps.clear();
        fm.agent_session_id = None;
        fm.agent_session_log = None;
        fm.agent_session_ids.clear();
        fm.agent_session_logs.clear();
        fm.agent_resume_commands.clear();
        fm
    }
}

/// M10 per-task overrides for the cooperative-execution bounds. An unset field
/// falls back to the matching `defaults.*` config value.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskBounds {
    /// Override `defaults.idle_timeout_seconds` for this task (seconds).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_timeout: Option<u64>,
    /// Override `defaults.max_seconds` (soft total ceiling) for this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_seconds: Option<crate::config::MaxSeconds>,
    /// Override `defaults.max_continuations` (auto-resume hop cap) for this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_continuations: Option<u32>,
    /// Override `defaults.max_tool_calls` (tool-call budget) for this task.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tool_calls: Option<u64>,
}

/// Default status for a task whose frontmatter omits `status` — namely a
/// repo-local DEFINITION (`.varda/tasks/<id>-<slug>.md`), which never carries
/// runtime state. Home-store STATE files always spell `status` out.
fn default_status() -> TaskStatus {
    TaskStatus::Backlog
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Backlog,
    Ready,
    Running,
    // The post-agent review state: a run completed, produced a recap, and the
    // task is waiting for human review / follow-up. Serialized as `review`; the
    // `alias` keeps legacy `status: pending` STATE files (from another machine or
    // branch) loading without error. See `migrate_pending_status` for the one-shot
    // rewrite of existing control-plane files.
    #[serde(alias = "pending")]
    Review,
    NeedsUser,
    Failed,
    Done,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Backlog => "backlog",
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Review => "review",
            Self::NeedsUser => "needs_user",
            Self::Failed => "failed",
            Self::Done => "done",
        }
    }
}

impl std::str::FromStr for TaskStatus {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "backlog" => Ok(Self::Backlog),
            "ready" => Ok(Self::Ready),
            "running" => Ok(Self::Running),
            "review" => Ok(Self::Review),
            // Legacy alias: accept the old `pending` spelling and map it to `review`.
            "pending" => Ok(Self::Review),
            "needs_user" => Ok(Self::NeedsUser),
            "failed" => Ok(Self::Failed),
            "done" => Ok(Self::Done),
            _ => anyhow::bail!(
                "unknown status '{}'; expected one of: backlog, ready, running, review, needs_user, failed, done (legacy alias: pending -> review)",
                s
            ),
        }
    }
}

impl TaskDocument {
    pub fn set_status(&mut self, status: TaskStatus) {
        self.frontmatter.status = status;
    }

    pub fn set_recap(&mut self, recap: impl Into<String>) {
        self.frontmatter.recaps.push(recap.into());
    }

    pub fn set_plan(&mut self, plan: impl Into<String>) {
        self.frontmatter.plan = Some(plan.into());
    }

    pub fn set_assignee(&mut self, assignee: impl Into<String>) {
        self.frontmatter.assignee = Some(assignee.into());
    }

    pub fn title(&self) -> String {
        task_title(&self.body)
    }
}

pub fn load_task(path: impl AsRef<Path>) -> Result<TaskDocument> {
    let path = path.as_ref();
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read task at {}", path.display()))?;
    parse_task(path, &content)
}

pub fn write_task(task: &TaskDocument) -> Result<()> {
    let frontmatter =
        serde_yaml::to_string(&task.frontmatter).context("failed to serialize task frontmatter")?;
    let frontmatter = frontmatter.trim_start_matches("---\n").trim_end();
    let content = format!("---\n{frontmatter}\n---\n\n{}", task.body.trim_start());

    fs::write(&task.path, content)
        .with_context(|| format!("failed to write task at {}", task.path.display()))?;

    Ok(())
}

/// Outcome of [`fold_project`].
#[derive(Debug, Default)]
pub struct FoldReport {
    /// Task records re-keyed and relocated into the mother's store folder.
    pub moved: Vec<u64>,
    /// Filenames skipped because the mother store already had that name.
    pub collisions: Vec<String>,
    /// Source store folders removed because they became empty.
    pub removed_dirs: Vec<String>,
}

/// Fold every task whose `project` equals `workspace` into the `mother` project:
/// rewrite the record's `project` to the mother's canonical path and relocate the
/// file into the mother's store folder. A merged worktree's tasks thus become the
/// mother's history instead of a separate dashboard project.
///
/// `workspace` is matched as a path STRING (trailing slash ignored) — the worktree
/// may already be deleted after a merge, so it is deliberately NOT canonicalized.
/// `mother` must exist (it is canonicalized). With `dry_run`, nothing is written.
pub fn fold_project(
    config: &Config,
    workspace: &str,
    mother: &Path,
    dry_run: bool,
) -> Result<FoldReport> {
    let task_root = Path::new(&config.defaults.operations_dir).join("tasks");
    let mother = normalize_project_path(mother)?;
    let mother_str = mother.to_string_lossy().into_owned();
    let mother_dir = task_root.join(project_task_folder(&mother)?);
    let want = workspace.trim_end_matches('/');
    if want == mother_str.trim_end_matches('/') {
        bail!("refusing to fold a project into itself: {want}");
    }

    let mut summaries = Vec::new();
    if task_root.exists() {
        collect_all_tasks(&task_root, &mut summaries)?;
    }
    let mut report = FoldReport::default();
    let mut source_dirs = std::collections::BTreeSet::new();
    for summary in summaries {
        if summary.project.as_deref().map(|p| p.trim_end_matches('/')) != Some(want) {
            continue;
        }
        let src = summary.path.clone();
        if let Some(parent) = src.parent() {
            source_dirs.insert(parent.to_path_buf());
        }
        let filename = src.file_name().unwrap_or_default().to_owned();
        let dst = mother_dir.join(&filename);
        if dst != src && dst.exists() {
            report.collisions.push(filename.to_string_lossy().into_owned());
            continue;
        }
        report.moved.extend(summary.id);
        if dry_run {
            continue;
        }
        let mut doc = load_task(&src)?;
        doc.frontmatter.project = Some(mother_str.clone());
        doc.path = dst.clone();
        fs::create_dir_all(&mother_dir).with_context(|| {
            format!("failed to create mother store {}", mother_dir.display())
        })?;
        write_task(&doc)?;
        if dst != src {
            fs::remove_file(&src)
                .with_context(|| format!("failed to remove folded record {}", src.display()))?;
        }
    }
    if !dry_run {
        for dir in source_dirs {
            if dir != mother_dir
                && fs::read_dir(&dir).map(|mut e| e.next().is_none()).unwrap_or(false)
            {
                let _ = fs::remove_dir(&dir);
                report.removed_dirs.push(dir.to_string_lossy().into_owned());
            }
        }
    }
    Ok(report)
}

/// Directory name a repository uses to carry its own committed task DEFINITIONS
/// and workflow rules (`<repo>/.varda/`). Distinct from the untrusted sandbox
/// `.varda` FILE handled in `config.rs`: a directory named `.varda` and a file
/// named `.varda` cannot coexist, and `config::find_nearest_varda` only matches
/// the FILE form (`candidate.is_file()`), so the two features never collide.
pub const REPO_VARDA_DIRNAME: &str = ".varda";
/// Subdirectory of the repo `.varda/` holding task DEFINITION markdown files.
pub const REPO_TASKS_DIRNAME: &str = "tasks";

/// The repo-local task DEFINITION store for `project_path`, i.e.
/// `<project>/.varda/tasks`, but ONLY when `<project>/.varda` exists as a
/// directory. Returns `None` for repos without a `.varda/` directory (the
/// back-compat, home-store-only case) — including the legacy `.varda` sandbox
/// FILE, which is not a directory.
pub fn repo_task_store(project_path: &Path) -> Option<PathBuf> {
    let varda_dir = project_path.join(REPO_VARDA_DIRNAME);
    if varda_dir.is_dir() {
        Some(varda_dir.join(REPO_TASKS_DIRNAME))
    } else {
        None
    }
}

/// Durable, committable subset of a task's frontmatter. A DEFINITION carries the
/// spec (id, project, assignee, capability allowlist, bounds, requires_user) and
/// the brief body — but NEVER runtime STATE (status, recaps, session ids/logs,
/// plan, resume commands), which lives only in the `~/.varda` control plane.
#[derive(Debug, Serialize)]
struct TaskDefinition<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    assignee: Option<&'a str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    allow_commands: &'a Vec<String>,
    #[serde(flatten)]
    bounds: &'a TaskBounds,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    requires_user: bool,
    // Operator sandbox pin (`--sandbox`) is part of the DEFINITION (not runtime
    // state): a clone/worktree materializing this task must preserve the pin.
    #[serde(skip_serializing_if = "Option::is_none")]
    sandbox: Option<&'a str>,
}

/// Serialize a task's DEFINITION-only frontmatter + brief to
/// `<repo>/.varda/tasks/<id>-<slug>.md`. Runtime state is deliberately excluded
/// so the file is safe to commit to the code repository.
fn write_definition(store: &Path, slug: &str, task: &TaskDocument) -> Result<PathBuf> {
    fs::create_dir_all(store)
        .with_context(|| format!("failed to create repo task store {}", store.display()))?;
    let filename = match task.frontmatter.id {
        Some(id) => format!("{id}-{slug}.md"),
        None => format!("{slug}.md"),
    };
    let path = store.join(filename);
    if path.exists() {
        bail!("task definition {} already exists", path.display());
    }

    let definition = TaskDefinition {
        id: task.frontmatter.id,
        project: task.frontmatter.project.as_deref(),
        assignee: task.frontmatter.assignee.as_deref(),
        allow_commands: &task.frontmatter.allow_commands,
        bounds: &task.frontmatter.bounds,
        requires_user: task.frontmatter.requires_user,
        sandbox: task.frontmatter.sandbox.as_deref(),
    };
    let frontmatter =
        serde_yaml::to_string(&definition).context("failed to serialize task definition")?;
    let frontmatter = frontmatter.trim_end();
    let content = format!("---\n{frontmatter}\n---\n\n{}", task.body.trim_start());
    fs::write(&path, content)
        .with_context(|| format!("failed to write task definition at {}", path.display()))?;

    Ok(path)
}

pub fn create_task(
    config: &Config,
    taskname: &str,
    project_path: &Path,
    assignee: Option<&str>,
    description: Option<&str>,
    sandbox: Option<&str>,
) -> Result<PathBuf> {
    let task_root = Path::new(&config.defaults.operations_dir).join("tasks");
    let task_dir = task_root.join(project_task_folder(project_path)?);
    fs::create_dir_all(&task_dir)
        .with_context(|| format!("failed to create task directory {}", task_dir.display()))?;

    // Allocate ids across BOTH the home STATE store and the repo DEFINITION store
    // (when present), so a clone that already carries definitions never collides.
    let repo_store = repo_task_store(project_path);
    let mut max_id = max_task_id(&task_root)?;
    if let Some(store) = repo_store.as_deref() {
        max_id = max_id.max(max_task_id(store)?);
    }
    let id = max_id.unwrap_or(0) + 1;

    let slug = slugify_task_name(taskname)?;
    let filename = format!("{slug}.md");
    let path = task_dir.join(filename);

    if path.exists() {
        bail!("task {} already exists", path.display());
    }

    let task = TaskDocument {
        path: path.clone(),
        frontmatter: TaskFrontmatter {
            bounds: crate::task::TaskBounds::default(),
            id: Some(id),
            status: TaskStatus::Backlog,
            project: Some(project_path.display().to_string()),
            assignee: assignee.map(str::to_owned),
            sandbox: sandbox.map(str::to_owned),
            recap: None,
            recaps: vec![],
            plan: None,
            agent_session_id: None,
            agent_session_log: None,
            agent_session_ids: vec![],
            agent_session_logs: vec![],
            agent_resume_commands: vec![],
            allow_commands: vec![],
            requires_user: false,
        },
        body: if let Some(desc) = description {
            format!("# {taskname}\n\n{desc}\n")
        } else {
            format!("# {taskname}\n\n")
        },
    };

    // Always write the home-store STATE file: it is the run-time authority and
    // keeps status/recaps/logs OUT of the code repository.
    write_task(&task)?;

    // When the repo opts in with a `.varda/` directory, also drop the durable
    // DEFINITION (frontmatter spec + brief) into `<repo>/.varda/tasks/`, so the
    // task travels with the code. Runtime state stays home.
    if let Some(store) = repo_store {
        write_definition(&store, &slug, &task)?;
    }

    Ok(path)
}

pub fn list_tasks(config: &Config, project_path: &Path) -> Result<Vec<TaskSummary>> {
    let normalized = normalize_project_path(project_path)?;
    let task_dir = Path::new(&config.defaults.operations_dir).join("tasks");
    let mut tasks = Vec::new();
    if task_dir.exists() {
        collect_tasks(&task_dir, &normalized, &mut tasks)?;
    }

    // Augment the home STATE store with repo-local DEFINITIONS the code repo
    // carries. Home state takes precedence for an id already present (it holds the
    // live status); repo-only definitions surface with their default status so a
    // fresh clone still sees every task the code ships.
    if let Some(store) = repo_task_store(project_path).filter(|store| store.exists()) {
        let seen: std::collections::HashSet<u64> =
            tasks.iter().filter_map(|task| task.id).collect();
        let mut definitions = Vec::new();
        collect_all_tasks(&store, &mut definitions)?;
        for definition in definitions {
            match definition.id {
                Some(id) if seen.contains(&id) => continue,
                _ => tasks.push(definition),
            }
        }
    }

    tasks.sort_by(|left, right| {
        left.id
            .unwrap_or(u64::MAX)
            .cmp(&right.id.unwrap_or(u64::MAX))
            .then_with(|| left.path.cmp(&right.path))
    });

    Ok(tasks)
}

pub fn list_all_tasks(config: &Config) -> Result<Vec<TaskSummary>> {
    let task_dir = Path::new(&config.defaults.operations_dir).join("tasks");
    if !task_dir.exists() {
        return Ok(Vec::new());
    }

    let mut tasks = Vec::new();
    collect_all_tasks(&task_dir, &mut tasks)?;
    tasks.sort_by(|left, right| {
        left.id
            .unwrap_or(u64::MAX)
            .cmp(&right.id.unwrap_or(u64::MAX))
            .then_with(|| left.path.cmp(&right.path))
    });

    Ok(tasks)
}

pub fn resolve_task_reference(config: &Config, task_ref: &Path) -> Result<PathBuf> {
    if task_ref.exists() {
        return Ok(task_ref.to_path_buf());
    }

    let Some(task_ref) = task_ref.to_str() else {
        bail!("task reference is not valid UTF-8: {}", task_ref.display());
    };
    let id = task_ref.parse::<u64>().with_context(|| {
        format!("task reference '{task_ref}' is neither an existing path nor a numeric id")
    })?;

    if let Some(path) = find_task_by_id(config, id)? {
        return Ok(path);
    }

    // Clone/worktree flow: the home STATE store has no record of this id, but the
    // current repo may carry its DEFINITION in `.varda/tasks/`. Materialize a home
    // STATE file from that definition and run against it, so state is written to
    // `~/.varda` and never committed back into the code repo. Walk up to the repo
    // root first so a `run` issued from a SUBDIRECTORY still finds `.varda/tasks`.
    let cwd = std::env::current_dir().context("failed to determine current directory")?;
    let repo_root = find_repo_root(&cwd).unwrap_or(cwd);
    if let Some(path) = materialize_from_repo_definition(config, id, &repo_root)? {
        return Ok(path);
    }

    bail!("no task found with id {id}")
}

/// Nearest ancestor of `start` (inclusive) that opts into the repo-local task
/// store, i.e. carries a `.varda/` DIRECTORY. This lets `run`/lookups issued from
/// a subdirectory resolve against the repo root's `.varda/tasks`. Returns `None`
/// when no ancestor carries a `.varda/` directory.
fn find_repo_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|ancestor| ancestor.join(REPO_VARDA_DIRNAME).is_dir())
        .map(Path::to_path_buf)
}

/// If `repo_root`'s repo `.varda/tasks/` carries a DEFINITION with `id`, copy it
/// into the home STATE store (keyed by its `project` + id) and return the home
/// path. Returns `None` when no such definition exists.
fn materialize_from_repo_definition(
    config: &Config,
    id: u64,
    repo_root: &Path,
) -> Result<Option<PathBuf>> {
    let Some(store) = repo_task_store(repo_root) else {
        return Ok(None);
    };
    if !store.exists() {
        return Ok(None);
    }

    let mut matches = Vec::new();
    collect_task_id_matches(&store, id, &mut matches)?;
    let definition_path = match matches.len() {
        0 => return Ok(None),
        1 => matches.remove(0),
        _ => bail!(
            "multiple task definitions found with id {id} in {}",
            store.display()
        ),
    };

    let definition = load_task(&definition_path)?;
    // Bind the materialized STATE to the CURRENT checkout, never the (possibly
    // stale) absolute `project` the definition was committed with. A clone or
    // worktree lives at a different path than the author's machine, so routing
    // and client-build must target the repo we are actually running from.
    let project = repo_root.display().to_string();

    let task_root = Path::new(&config.defaults.operations_dir).join("tasks");
    let task_dir = task_root.join(project_task_folder(Path::new(&project))?);
    fs::create_dir_all(&task_dir)
        .with_context(|| format!("failed to create task directory {}", task_dir.display()))?;

    let filename = definition_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("{id}.md"));
    let state_path = task_dir.join(filename);

    let mut frontmatter = TaskFrontmatter {
        project: Some(project),
        ..definition.frontmatter
    };
    // Definitions never carry runtime state, so `status` loads as the default
    // `Backlog`. The runner only accepts `Ready` tasks, so a freshly
    // materialized repo definition would be rejected on its advertised first
    // `run`. Promote it to `Ready` here so the first run of a repo-defined task
    // succeeds; an explicit non-default status (e.g. a hand-authored `pending`)
    // is left untouched.
    if frontmatter.status == TaskStatus::Backlog {
        frontmatter.status = TaskStatus::Ready;
    }

    let state = TaskDocument {
        path: state_path.clone(),
        frontmatter,
        body: definition.body,
    };
    write_task(&state)?;

    Ok(Some(state_path))
}

/// Resolve a numeric task id to its current STATE: the terminal-or-in-flight
/// `TaskStatus` and the path of its most recent recap (if any). Reads the home
/// STATE store — the runtime authority for status/recaps — via
/// [`find_task_by_id`]. Returns `None` when no task carries `id`. The collect
/// channel ([`crate::orchestration::SubtaskResults`]) polls this to know when a
/// spawned subtask has finished and where to read its recap. Kept here so recap
/// path resolution is not duplicated across call sites.
pub fn lookup_task_state(config: &Config, id: u64) -> Result<Option<(TaskStatus, Option<String>)>> {
    let Some(path) = find_task_by_id(config, id)? else {
        return Ok(None);
    };
    let doc = load_task(&path)?;
    let recap_path = doc.frontmatter.recaps.last().cloned();
    Ok(Some((doc.frontmatter.status, recap_path)))
}

/// Rewrite every control-plane task STATE file under the configured operations
/// task directory whose `status` is the legacy `pending` spelling to the new
/// `review`. All other frontmatter and the task body are preserved. Idempotent:
/// a file already at `review` (or any other status) is left untouched, so a
/// second run reports 0 changes. Legacy read compatibility (the `pending` serde
/// alias and `FromStr` alias) stays in place afterwards, so task files from
/// another machine or branch that still say `pending` keep loading. Returns the
/// number of files rewritten.
pub fn migrate_pending_status(config: &Config) -> Result<usize> {
    let task_root = Path::new(&config.defaults.operations_dir).join("tasks");
    if !task_root.exists() {
        return Ok(0);
    }
    let mut changed = 0;
    migrate_pending_in_dir(&task_root, &mut changed)?;
    Ok(changed)
}

fn migrate_pending_in_dir(path: &Path, changed: &mut usize) -> Result<()> {
    for entry in fs::read_dir(path)
        .with_context(|| format!("failed to read task directory {}", path.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read task directory {}", path.display()))?;
        let entry_path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry_path.display()))?;

        if file_type.is_dir() {
            migrate_pending_in_dir(&entry_path, changed)?;
            continue;
        }

        if entry_path
            .extension()
            .is_none_or(|extension| extension != "md")
        {
            continue;
        }

        let content = fs::read_to_string(&entry_path)
            .with_context(|| format!("failed to read task at {}", entry_path.display()))?;
        // Byte-preserving edit: flip only the legacy `status: pending` value on
        // its frontmatter line. Every other byte — unknown keys, comments, key
        // ordering, quoting, and the body — is preserved exactly, so the pass is
        // idempotent and never reformats unrelated task files. Deliberately does
        // NOT round-trip through `TaskDocument`/`write_task`, which reserializes
        // typed frontmatter and would drop unknown keys, comments, and formatting.
        let Some(migrated) = rewrite_legacy_pending_status(&content) else {
            continue;
        };
        fs::write(&entry_path, migrated)
            .with_context(|| format!("failed to write task at {}", entry_path.display()))?;
        *changed += 1;
    }

    Ok(())
}

/// Byte-preserving migration of a legacy `status: pending` frontmatter value to
/// `review`. Returns `Some(new_content)` when exactly the value token on the
/// matching frontmatter line was rewritten, or `None` when the file must be left
/// byte-for-byte unchanged: no frontmatter, no `status:` key, a `status:` value
/// other than `pending`, or an already-migrated file. Only the frontmatter block
/// (between the first two `---` fences) is scanned, so a stray `pending` in the
/// body never triggers a rewrite. A trailing inline comment on the status line is
/// preserved; bare and quoted (`"pending"`/`'pending'`) values are handled.
fn rewrite_legacy_pending_status(content: &str) -> Option<String> {
    let mut start = 0usize;
    let mut first = true;
    loop {
        let rel_newline = content[start..].find('\n');
        let end = match rel_newline {
            Some(index) => start + index,
            None => content.len(),
        };
        let line = &content[start..end];

        if first {
            // The opening `---` fence must be the very first line.
            if line.trim() != "---" {
                return None;
            }
            first = false;
        } else if line.trim() == "---" {
            // Closing fence reached with no legacy status line: leave unchanged.
            return None;
        } else if let Some(replaced) = replace_pending_status_value(line) {
            let mut result = String::with_capacity(content.len());
            result.push_str(&content[..start]);
            result.push_str(&replaced);
            result.push_str(&content[end..]);
            return Some(result);
        }

        match rel_newline {
            Some(index) => start += index + 1,
            // No newline and no closing fence found: not a migratable file.
            None => return None,
        }
    }
}

/// If `line` is a frontmatter `status:` entry whose value is the legacy
/// `pending` (bare or quoted), return the line with only that value token
/// rewritten to `review`, preserving indentation, quoting, surrounding
/// whitespace, and any trailing inline comment. Otherwise return `None`.
fn replace_pending_status_value(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    let after_key = trimmed.strip_prefix("status:")?;

    // Split off an optional trailing YAML comment (a `#` preceded by
    // whitespace) so it survives the rewrite untouched.
    let (value_part, comment_part) = split_trailing_comment(after_key);
    let value = value_part.trim();
    let is_legacy_pending =
        value == "pending" || value == "\"pending\"" || value == "'pending'";
    if !is_legacy_pending {
        return None;
    }

    // Replace only the `pending` token; quotes and whitespace in `value_part`
    // are preserved as-is.
    let new_value_part = value_part.replacen("pending", "review", 1);
    Some(format!("{indent}status:{new_value_part}{comment_part}"))
}

/// Split `s` into its value region and an optional trailing YAML comment. The
/// comment starts at the first `#` that is at the start of `s` or preceded by
/// whitespace; the returned comment slice includes that leading whitespace.
fn split_trailing_comment(s: &str) -> (&str, &str) {
    let bytes = s.as_bytes();
    for index in 0..bytes.len() {
        if bytes[index] == b'#' && (index == 0 || bytes[index - 1].is_ascii_whitespace()) {
            return (&s[..index], &s[index..]);
        }
    }
    (s, "")
}

fn find_task_by_id(config: &Config, id: u64) -> Result<Option<PathBuf>> {
    let task_dir = Path::new(&config.defaults.operations_dir).join("tasks");
    if !task_dir.exists() {
        return Ok(None);
    }

    let mut matches = Vec::new();
    collect_task_id_matches(&task_dir, id, &mut matches)?;

    match matches.len() {
        0 => Ok(None),
        1 => Ok(matches.pop()),
        _ => bail!("multiple tasks found with id {id}"),
    }
}

fn collect_task_id_matches(path: &Path, id: u64, matches: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(path)
        .with_context(|| format!("failed to read task directory {}", path.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read task directory {}", path.display()))?;
        let entry_path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry_path.display()))?;

        if file_type.is_dir() {
            collect_task_id_matches(&entry_path, id, matches)?;
            continue;
        }

        if entry_path
            .extension()
            .is_none_or(|extension| extension != "md")
        {
            continue;
        }

        let task = match load_task(&entry_path) {
            Ok(task) => task,
            Err(error) => {
                eprintln!("warning: skipped {}: {error:#}", entry_path.display());
                continue;
            }
        };

        if task.frontmatter.id == Some(id) {
            matches.push(entry_path);
        }
    }

    Ok(())
}

fn collect_tasks(path: &Path, project_path: &Path, tasks: &mut Vec<TaskSummary>) -> Result<()> {
    for entry in fs::read_dir(path)
        .with_context(|| format!("failed to read task directory {}", path.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read task directory {}", path.display()))?;
        let entry_path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry_path.display()))?;

        if file_type.is_dir() {
            collect_tasks(&entry_path, project_path, tasks)?;
            continue;
        }

        if entry_path
            .extension()
            .is_none_or(|extension| extension != "md")
        {
            continue;
        }

        let task = match load_task(&entry_path) {
            Ok(task) => task,
            Err(error) => {
                eprintln!("warning: skipped {}: {error:#}", entry_path.display());
                continue;
            }
        };
        let Some(task_project) = task.frontmatter.project.as_deref() else {
            continue;
        };
        if normalize_project_path(Path::new(task_project))? != project_path {
            continue;
        }

        tasks.push(TaskSummary {
            path: task.path,
            id: task.frontmatter.id,
            status: task.frontmatter.status,
            project: task.frontmatter.project,
            assignee: task.frontmatter.assignee,
            title: task_title(&task.body),
        });
    }

    Ok(())
}

fn collect_all_tasks(path: &Path, tasks: &mut Vec<TaskSummary>) -> Result<()> {
    for entry in fs::read_dir(path)
        .with_context(|| format!("failed to read task directory {}", path.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read task directory {}", path.display()))?;
        let entry_path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry_path.display()))?;

        if file_type.is_dir() {
            collect_all_tasks(&entry_path, tasks)?;
            continue;
        }

        if entry_path
            .extension()
            .is_none_or(|extension| extension != "md")
        {
            continue;
        }

        let task = match load_task(&entry_path) {
            Ok(task) => task,
            Err(error) => {
                eprintln!("warning: skipped {}: {error:#}", entry_path.display());
                continue;
            }
        };

        tasks.push(TaskSummary {
            path: task.path,
            id: task.frontmatter.id,
            status: task.frontmatter.status,
            project: task.frontmatter.project,
            assignee: task.frontmatter.assignee,
            title: task_title(&task.body),
        });
    }

    Ok(())
}

fn normalize_project_path(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return path
            .canonicalize()
            .with_context(|| format!("failed to canonicalize project path {}", path.display()));
    }

    Ok(path.to_path_buf())
}

fn max_task_id(path: &Path) -> Result<Option<u64>> {
    if !path.exists() {
        return Ok(None);
    }

    let mut max_id = None;
    for entry in fs::read_dir(path)
        .with_context(|| format!("failed to read task directory {}", path.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read task directory {}", path.display()))?;
        let entry_path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", entry_path.display()))?;

        let entry_max = if file_type.is_dir() {
            max_task_id(&entry_path)?
        } else if entry_path
            .extension()
            .is_some_and(|extension| extension == "md")
        {
            match load_task(&entry_path) {
                Ok(task) => Some(task.frontmatter.id.unwrap_or(0)),
                Err(error) => {
                    eprintln!("warning: skipped {}: {error:#}", entry_path.display());
                    None
                }
            }
        } else {
            None
        };

        max_id = max_id.max(entry_max);
    }

    Ok(max_id)
}

pub fn resolve_project_path(project_path: Option<&Path>) -> Result<PathBuf> {
    let path = match project_path {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir().context("failed to determine current project directory")?,
    };

    if path.exists() {
        return path
            .canonicalize()
            .with_context(|| format!("failed to canonicalize project path {}", path.display()));
    }

    Ok(path)
}

fn task_title(body: &str) -> String {
    body.lines()
        .find_map(|line| line.strip_prefix("# ").map(str::trim))
        .filter(|title| !title.is_empty())
        .unwrap_or("(untitled)")
        .to_owned()
}

fn parse_task(path: &Path, content: &str) -> Result<TaskDocument> {
    let matter = Matter::<YAML>::new();
    let parsed = matter
        .parse::<TaskFrontmatter>(content)
        .context("failed to parse markdown frontmatter")?;
    let mut frontmatter = parsed
        .data
        .with_context(|| format!("missing or invalid frontmatter in {}", path.display()))?;

    // Migrate legacy single-recap field to the recaps list.
    if let Some(recap) = frontmatter.recap.take()
        && frontmatter.recaps.is_empty()
    {
        frontmatter.recaps.push(recap);
    }

    // Migrate legacy single agent_session fields to the list variants.
    if let Some(id) = frontmatter.agent_session_id.take()
        && frontmatter.agent_session_ids.is_empty()
    {
        frontmatter.agent_session_ids.push(id);
    }
    if let Some(log) = frontmatter.agent_session_log.take()
        && frontmatter.agent_session_logs.is_empty()
    {
        frontmatter.agent_session_logs.push(log);
    }

    Ok(TaskDocument {
        path: path.to_path_buf(),
        frontmatter,
        body: parsed.content,
    })
}

fn slugify_task_name(taskname: &str) -> Result<String> {
    let mut slug = String::new();
    let mut last_was_dash = false;

    for character in taskname.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }

    let slug = slug.trim_matches('-').to_owned();

    if slug.is_empty() {
        bail!("task name must contain at least one ASCII letter or digit");
    }

    Ok(slug)
}

fn project_task_folder(project_path: &Path) -> Result<String> {
    let normalized = normalize_project_path(project_path)?;
    let project = normalized
        .to_str()
        .with_context(|| format!("project path is not valid UTF-8: {}", normalized.display()))?;

    slugify_path(project)
}

fn slugify_path(path: &str) -> Result<String> {
    let mut slug = String::new();
    let mut last_was_dash = false;

    for character in path.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            slug.push(character);
            last_was_dash = false;
        } else if !last_was_dash {
            slug.push('-');
            last_was_dash = true;
        }
    }

    let slug = slug.trim_matches('-').to_owned();

    if slug.is_empty() {
        bail!("project path must contain at least one ASCII letter or digit");
    }

    Ok(slug)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_task_frontmatter_and_body() {
        let task = parse_task(
            Path::new(".varda/operations/tasks/codex/example.md"),
            r#"---
id: 42
status: ready
assignee: codex
recap: null
requires_user: false
---

# Task

Do the work.
"#,
        )
        .expect("task should parse");

        assert_eq!(task.frontmatter.status, TaskStatus::Ready);
        assert_eq!(task.frontmatter.id, Some(42));
        assert_eq!(task.frontmatter.assignee.as_deref(), Some("codex"));
        assert!(task.frontmatter.agent_session_ids.is_empty());
        assert!(!task.frontmatter.requires_user);
        assert!(task.body.contains("Do the work."));
        // A task without the M12 key defaults to an empty allowlist.
        assert!(task.frontmatter.allow_commands.is_empty());
    }

    #[test]
    fn parses_and_round_trips_allow_commands() {
        let task = parse_task(
            Path::new(".varda/operations/tasks/claude/example.md"),
            r#"---
id: 7
status: ready
assignee: claude
allow_commands:
  - msb
  - docker
requires_user: false
---

# Task

Verify the sandbox.
"#,
        )
        .expect("task should parse");

        assert_eq!(task.frontmatter.allow_commands, vec!["msb", "docker"]);

        // The declared allowlist survives a serialize round-trip; an empty list
        // is omitted (skip_serializing_if) so untouched tasks stay clean.
        let frontmatter =
            serde_yaml::to_string(&task.frontmatter).expect("frontmatter should serialize");
        assert!(frontmatter.contains("allow_commands:"));
        assert!(frontmatter.contains("- msb"));

        let empty = TaskFrontmatter {
            bounds: crate::task::TaskBounds::default(),
            allow_commands: vec![],
            ..task.frontmatter.clone()
        };
        let empty_yaml = serde_yaml::to_string(&empty).expect("frontmatter should serialize");
        assert!(!empty_yaml.contains("allow_commands"));
    }

    #[test]
    fn sanitized_for_prompt_drops_host_bookkeeping_paths() {
        let task = parse_task(
            Path::new(".varda/operations/tasks/claude/resident.md"),
            r#"---
id: 523
status: ready
project: /Users/x/dev/ws
assignee: claude-resident
recaps:
  - /Users/x/.varda/operations/recaps/aaa.md
  - /Users/x/.varda/operations/recaps/bbb.md
agent_session_ids:
  - sess-1
agent_session_logs:
  - /Users/x/.varda/operations/runs/aaa.log
agent_resume_commands:
  - "claude --resume sess-1"
allow_commands:
  - msb
---

# Resident

Body.
"#,
        )
        .expect("task should parse");

        let clean = task.frontmatter.sanitized_for_prompt();
        // Host-path bookkeeping is gone…
        assert!(clean.recaps.is_empty());
        assert!(clean.agent_session_ids.is_empty());
        assert!(clean.agent_session_logs.is_empty());
        assert!(clean.agent_resume_commands.is_empty());
        // …but the fields that describe the work survive.
        assert_eq!(clean.id, Some(523));
        assert_eq!(clean.assignee.as_deref(), Some("claude-resident"));
        assert_eq!(clean.allow_commands, vec!["msb"]);

        let yaml = serde_yaml::to_string(&clean).expect("serialize");
        assert!(!yaml.contains("operations/recaps"), "yaml: {yaml}");
        assert!(!yaml.contains("agent_session_logs"), "yaml: {yaml}");
        // The original is untouched (we cloned).
        assert_eq!(task.frontmatter.recaps.len(), 2);
    }

    #[test]
    fn serializes_task_frontmatter() {
        let mut task = TaskDocument {
            path: PathBuf::from("task.md"),
            frontmatter: TaskFrontmatter {
                bounds: crate::task::TaskBounds::default(),
                id: None,
                status: TaskStatus::Running,
                project: None,
                assignee: Some("codex".to_owned()),
                sandbox: None,
                recap: None,
                recaps: vec![],
                plan: None,
                agent_session_id: None,
                agent_session_log: None,
                agent_session_ids: vec![],
                agent_session_logs: vec![],
                agent_resume_commands: vec![],
                allow_commands: vec![],
                requires_user: false,
            },
            body: "# Task\n\nDo the work.\n".to_owned(),
        };

        task.set_status(TaskStatus::Review);
        task.set_recap(".varda/operations/recaps/run.md");
        task.frontmatter
            .agent_session_ids
            .push("session-123".to_owned());
        task.frontmatter
            .agent_session_logs
            .push(".varda/operations/runs/session-123.log".to_owned());

        let frontmatter =
            serde_yaml::to_string(&task.frontmatter).expect("frontmatter should serialize");

        assert!(frontmatter.contains("status: review"));
        assert!(frontmatter.contains("recaps:"));
        assert!(frontmatter.contains(".varda/operations/recaps/run.md"));
        assert!(frontmatter.contains("agent_session_ids:"));
        assert!(frontmatter.contains("session-123"));
        assert!(frontmatter.contains(".varda/operations/runs/session-123.log"));
    }

    #[test]
    fn writes_task_document() {
        let path = std::env::temp_dir().join(format!("varda-task-write-{}.md", std::process::id()));
        let task = TaskDocument {
            path: path.clone(),
            frontmatter: TaskFrontmatter {
                bounds: crate::task::TaskBounds::default(),
                id: Some(7),
                status: TaskStatus::Review,
                project: None,
                assignee: Some("codex".to_owned()),
                sandbox: None,
                recap: None,
                recaps: vec![".varda/operations/recaps/run.md".to_owned()],
                plan: None,
                agent_session_id: None,
                agent_session_log: None,
                agent_session_ids: vec![],
                agent_session_logs: vec![],
                agent_resume_commands: vec![],
                allow_commands: vec![],
                requires_user: false,
            },
            body: "# Task\n\nDo the work.\n".to_owned(),
        };

        write_task(&task).expect("task should write");
        let written = fs::read_to_string(&path).expect("task file should be readable");
        fs::remove_file(path).expect("task file should be removable");

        assert!(written.starts_with("---\n"));
        assert!(written.contains("status: review"));
        assert!(written.contains("# Task"));
    }

    #[test]
    fn creates_task_from_first_route() {
        let root = std::env::temp_dir().join(format!("varda-task-add-{}", std::process::id()));
        let operations_dir = root.join("operations");
        let config = Config {
            defaults: crate::config::Defaults {
                timeout_seconds: 600,
                operations_dir: operations_dir.display().to_string(),
                sandbox: None,
                ..Default::default()
            },
            routes: vec![crate::config::Route {
                glob: "**".to_owned(),
                agents: vec!["codex".to_owned()],
                sandbox: None,
                mounts: Vec::new(),
                orchestration: None,
                env: std::collections::BTreeMap::new(),
            }],
            agents: std::collections::BTreeMap::new(),
            roles: std::collections::BTreeMap::new(),
            git: crate::config::GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
            orchestration: crate::orchestration::OrchestrationPolicy::default(),
        };

        let project_path = Path::new("/work/project");
        let path = create_task(
            &config,
            "Write README Please",
            project_path,
            Some("codex"),
            None,
            None,
        )
        .expect("task should be created");
        let content = fs::read_to_string(&path).expect("task should be readable");

        assert!(content.contains("id: 1"));
        assert_eq!(
            path.parent(),
            Some(operations_dir.join("tasks/work-project").as_path())
        );
        assert!(content.contains("status: backlog"));
        assert!(content.contains("project: /work/project"));
        assert!(content.contains("assignee: codex"));
        assert!(content.contains("# Write README Please"));
    }

    #[test]
    fn creates_task_with_next_id() {
        let root =
            std::env::temp_dir().join(format!("varda-task-add-next-id-{}", std::process::id()));
        let task_dir = root.join("operations/tasks/old-project");
        fs::create_dir_all(&task_dir).expect("task directory should be created");
        fs::write(
            task_dir.join("old.md"),
            r#"---
id: 41
status: ready
requires_user: false
---

# Old
"#,
        )
        .expect("existing task should be written");

        let config = Config {
            defaults: crate::config::Defaults {
                timeout_seconds: 600,
                operations_dir: root.join("operations").display().to_string(),
                sandbox: None,
                ..Default::default()
            },
            routes: vec![],
            agents: std::collections::BTreeMap::new(),
            roles: std::collections::BTreeMap::new(),
            git: crate::config::GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
            orchestration: crate::orchestration::OrchestrationPolicy::default(),
        };

        let path = create_task(&config, "Next Task", Path::new("/work/project"), None, None, None)
            .expect("task should be created");
        let content = fs::read_to_string(&path).expect("task should be readable");

        assert!(content.contains("id: 42"));
        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    #[test]
    fn creates_separate_task_folders_per_project() {
        let root = std::env::temp_dir().join(format!(
            "varda-task-add-project-folders-{}",
            std::process::id()
        ));
        let operations_dir = root.join("operations");
        let first_project = root.join("first project");
        let second_project = root.join("second/project");
        fs::create_dir_all(&first_project).expect("first project should be created");
        fs::create_dir_all(&second_project).expect("second project should be created");

        let config = Config {
            defaults: crate::config::Defaults {
                timeout_seconds: 600,
                operations_dir: operations_dir.display().to_string(),
                sandbox: None,
                ..Default::default()
            },
            routes: vec![],
            agents: std::collections::BTreeMap::new(),
            roles: std::collections::BTreeMap::new(),
            git: crate::config::GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
            orchestration: crate::orchestration::OrchestrationPolicy::default(),
        };

        let first = create_task(&config, "Project Task", &first_project, None, None, None)
            .expect("first task should be created");
        let second = create_task(&config, "Project Task", &second_project, None, None, None)
            .expect("second task should be created");

        assert_ne!(first.parent(), second.parent());
        assert_eq!(first.file_name(), second.file_name());
        assert!(first.starts_with(operations_dir.join("tasks")));
        assert!(second.starts_with(operations_dir.join("tasks")));
        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    fn test_config(operations_dir: &Path) -> Config {
        Config {
            defaults: crate::config::Defaults {
                timeout_seconds: 600,
                operations_dir: operations_dir.display().to_string(),
                sandbox: None,
                ..Default::default()
            },
            routes: vec![],
            agents: std::collections::BTreeMap::new(),
            roles: std::collections::BTreeMap::new(),
            git: crate::config::GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
            orchestration: crate::orchestration::OrchestrationPolicy::default(),
        }
    }

    #[test]
    fn repo_task_store_requires_a_varda_directory() {
        let root =
            std::env::temp_dir().join(format!("varda-repo-store-detect-{}", std::process::id()));
        let project = root.join("repo");
        fs::create_dir_all(&project).expect("project should be created");

        // No `.varda` at all: home-store-only, back-compat.
        assert!(repo_task_store(&project).is_none());

        // A legacy `.varda` sandbox FILE must NOT be treated as a task store.
        fs::write(project.join(".varda"), "sandbox = \"rust\"\n").expect("file should write");
        assert!(repo_task_store(&project).is_none());

        // A `.varda/` DIRECTORY opts the repo into the local task store.
        fs::remove_file(project.join(".varda")).expect("file should remove");
        fs::create_dir_all(project.join(".varda")).expect("dir should create");
        assert_eq!(
            repo_task_store(&project),
            Some(project.join(".varda").join("tasks"))
        );

        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    #[test]
    fn create_task_writes_repo_definition_without_state() {
        let root =
            std::env::temp_dir().join(format!("varda-repo-def-write-{}", std::process::id()));
        let operations_dir = root.join("operations");
        let project = root.join("repo");
        fs::create_dir_all(project.join(".varda")).expect("repo .varda should be created");
        let config = test_config(&operations_dir);

        let state_path = create_task(&config, "Repo Local Task", &project, Some("claude"), None, None)
            .expect("task should be created");

        // The returned path is the HOME state file (run-time authority), which
        // carries runtime state such as `status`.
        assert!(state_path.starts_with(operations_dir.join("tasks")));
        let state = fs::read_to_string(&state_path).expect("state should be readable");
        assert!(state.contains("status: backlog"));

        // The repo carries an id-prefixed DEFINITION that omits runtime state.
        let definition_path = project.join(".varda/tasks/1-repo-local-task.md");
        let definition = fs::read_to_string(&definition_path).expect("definition should exist");
        assert!(definition.contains("id: 1"));
        assert!(definition.contains("project:"));
        assert!(definition.contains("assignee: claude"));
        assert!(!definition.contains("status:"));
        assert!(!definition.contains("recaps"));
        assert!(!definition.contains("agent_session"));
        assert!(definition.contains("# Repo Local Task"));

        // The definition round-trips through the loader with a default status.
        let loaded = load_task(&definition_path).expect("definition should load");
        assert_eq!(loaded.frontmatter.id, Some(1));
        assert_eq!(loaded.frontmatter.status, TaskStatus::Backlog);

        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    #[test]
    fn resolve_reference_materializes_home_state_from_repo_definition() {
        let root =
            std::env::temp_dir().join(format!("varda-repo-materialize-{}", std::process::id()));
        let operations_dir = root.join("operations");
        let project = root.join("clone");
        let store = project.join(".varda/tasks");
        fs::create_dir_all(&store).expect("repo store should be created");
        // Simulate a fresh clone: only the repo DEFINITION exists, no home state.
        fs::write(
            store.join("5-shipped.md"),
            format!(
                "---\nid: 5\nproject: {}\nassignee: codex\n---\n\n# Shipped\n\nDo it.\n",
                project.display()
            ),
        )
        .expect("definition should write");

        let config = test_config(&operations_dir);

        // Exercise the repo-root-scoped helper directly to avoid mutating the
        // process-wide cwd (which would be flaky under parallel test execution).
        let resolved = materialize_from_repo_definition(&config, 5, &project)
            .expect("materialization should not fail")
            .expect("id should resolve via repo definition");

        // State was materialized into the home store, NOT the repo.
        assert!(resolved.starts_with(operations_dir.join("tasks")));
        assert!(!resolved.starts_with(&project));
        let state = load_task(&resolved).expect("materialized state should load");
        assert_eq!(state.frontmatter.id, Some(5));
        assert_eq!(state.frontmatter.assignee.as_deref(), Some("codex"));
        assert!(state.body.contains("Do it."));

        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    #[test]
    fn materialized_repo_task_is_runnable_not_backlog() {
        // Finding #1: a definition omits runtime state, so it loads as `Backlog`;
        // the runner rejects anything that is not `Ready`. Materializing a fresh
        // repo definition must land it in a runnable state so its first `run`
        // (the advertised flow) is accepted rather than bailed out on.
        let root = std::env::temp_dir().join(format!("varda-repo-runnable-{}", std::process::id()));
        let operations_dir = root.join("operations");
        let project = root.join("clone");
        let store = project.join(".varda/tasks");
        fs::create_dir_all(&store).expect("repo store should be created");
        fs::write(
            store.join("7-ship-it.md"),
            "---\nid: 7\nassignee: claude\n---\n\n# Ship It\n\nGo.\n",
        )
        .expect("definition should write");
        // A definition loads with the default status the runner would reject.
        let definition = load_task(&store.join("7-ship-it.md")).expect("definition should load");
        assert_eq!(definition.frontmatter.status, TaskStatus::Backlog);

        let config = test_config(&operations_dir);
        let resolved = materialize_from_repo_definition(&config, 7, &project)
            .expect("materialization should not fail")
            .expect("id should resolve via repo definition");

        // The materialized STATE is `Ready` — exactly the precondition
        // `runner::run_task` enforces before it will start the agent.
        let state = load_task(&resolved).expect("materialized state should load");
        assert_eq!(state.frontmatter.status, TaskStatus::Ready);

        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    #[test]
    fn materialization_binds_to_current_checkout_not_stale_path() {
        // Finding #2: a cloned/worktree definition may carry the AUTHOR's absolute
        // project path, which does not exist on this machine. Materialization must
        // bind the runtime project to the checkout we are running from so routing
        // and client-build target a real repo, not the committed stale path.
        let root = std::env::temp_dir().join(format!("varda-repo-rebind-{}", std::process::id()));
        let operations_dir = root.join("operations");
        let checkout = root.join("real-checkout");
        let store = checkout.join(".varda/tasks");
        fs::create_dir_all(&store).expect("repo store should be created");
        let bogus = "/nonexistent/author/machine/repo";
        fs::write(
            store.join("11-portable.md"),
            format!("---\nid: 11\nproject: {bogus}\nassignee: codex\n---\n\n# Portable\n"),
        )
        .expect("definition should write");

        let config = test_config(&operations_dir);
        let resolved = materialize_from_repo_definition(&config, 11, &checkout)
            .expect("materialization should not fail")
            .expect("id should resolve via repo definition");

        let state = load_task(&resolved).expect("materialized state should load");
        // The runtime project is the checkout, NOT the stale committed path.
        assert_eq!(
            state.frontmatter.project.as_deref(),
            Some(checkout.display().to_string().as_str())
        );
        assert_ne!(state.frontmatter.project.as_deref(), Some(bogus));
        // The state file is filed under the checkout's project folder, so `run`
        // routes against a repo that actually exists.
        assert_eq!(
            resolved.parent(),
            Some(
                operations_dir
                    .join("tasks")
                    .join(project_task_folder(&checkout).expect("folder should derive"))
                    .as_path()
            )
        );

        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    #[test]
    fn repo_lookup_resolves_from_a_subdirectory() {
        // Finding #3: a `run` issued from a SUBDIRECTORY of the repo must still
        // find `.varda/tasks` at the repo root. `find_repo_root` walks up to the
        // nearest ancestor that opts into the repo-local store.
        let root = std::env::temp_dir().join(format!("varda-repo-subdir-{}", std::process::id()));
        let operations_dir = root.join("operations");
        let repo = root.join("repo");
        let store = repo.join(".varda/tasks");
        fs::create_dir_all(&store).expect("repo store should be created");
        let subdir = repo.join("crates").join("inner").join("src");
        fs::create_dir_all(&subdir).expect("subdirectory should be created");
        fs::write(
            store.join("13-deep.md"),
            "---\nid: 13\nassignee: claude\n---\n\n# Deep\n",
        )
        .expect("definition should write");

        // From deep inside the repo, the walk-up locates the repo root.
        assert_eq!(find_repo_root(&subdir), Some(repo.clone()));

        // And materialization against that resolved root surfaces the task,
        // which a naive exact-`current_dir()` lookup from the subdir would miss.
        let config = test_config(&operations_dir);
        let repo_root = find_repo_root(&subdir).expect("repo root should resolve");
        let resolved = materialize_from_repo_definition(&config, 13, &repo_root)
            .expect("materialization should not fail")
            .expect("id should resolve via repo definition from a subdirectory");
        let state = load_task(&resolved).expect("materialized state should load");
        assert_eq!(state.frontmatter.id, Some(13));

        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    #[test]
    fn list_tasks_surfaces_repo_definitions() {
        let root = std::env::temp_dir().join(format!("varda-repo-list-{}", std::process::id()));
        let operations_dir = root.join("operations");
        let project = root.join("repo");
        let store = project.join(".varda/tasks");
        fs::create_dir_all(&store).expect("repo store should be created");
        fs::write(
            store.join("9-fresh.md"),
            format!(
                "---\nid: 9\nproject: {}\nassignee: claude\n---\n\n# Fresh Clone Task\n",
                project.display()
            ),
        )
        .expect("definition should write");

        let config = test_config(&operations_dir);
        let tasks = list_tasks(&config, &project).expect("tasks should list");

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, Some(9));
        assert_eq!(tasks[0].status, TaskStatus::Backlog);
        assert_eq!(tasks[0].title, "Fresh Clone Task");

        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    #[test]
    fn parses_review_status_and_legacy_pending_alias() {
        use std::str::FromStr;

        // The new canonical spelling.
        assert_eq!(TaskStatus::from_str("review").unwrap(), TaskStatus::Review);
        // The legacy alias still parses, mapping to the same variant.
        assert_eq!(TaskStatus::from_str("pending").unwrap(), TaskStatus::Review);
        // An unknown status still errors, and the message now names `review`.
        let err = TaskStatus::from_str("bogus").unwrap_err().to_string();
        assert!(err.contains("review"), "message: {err}");

        // `review` round-trips through serde as `review`…
        assert_eq!(TaskStatus::Review.as_str(), "review");
        let yaml = serde_yaml::to_string(&TaskStatus::Review).unwrap();
        assert!(yaml.contains("review"), "yaml: {yaml}");
        // …and a legacy `pending` value still deserializes via the serde alias.
        let from_legacy: TaskStatus = serde_yaml::from_str("pending").unwrap();
        assert_eq!(from_legacy, TaskStatus::Review);
    }

    #[test]
    fn migrates_legacy_pending_state_files_idempotently() {
        let root = std::env::temp_dir().join(format!("varda-migrate-review-{}", std::process::id()));
        let operations_dir = root.join("operations");
        let task_dir = operations_dir.join("tasks/project");
        fs::create_dir_all(&task_dir).expect("task directory should be created");

        // A legacy `pending` STATE file that must be rewritten.
        fs::write(
            task_dir.join("legacy.md"),
            "---\nid: 1\nstatus: pending\nproject: /work/p\nassignee: claude\nrequires_user: false\n---\n\n# Legacy\n\nBody stays.\n",
        )
        .expect("legacy task should write");
        // An already-`review` file that must be left untouched (idempotency).
        fs::write(
            task_dir.join("fresh.md"),
            "---\nid: 2\nstatus: review\nrequires_user: false\n---\n\n# Fresh\n",
        )
        .expect("fresh task should write");
        // A `ready` file that must not be affected.
        fs::write(
            task_dir.join("ready.md"),
            "---\nid: 3\nstatus: ready\nrequires_user: false\n---\n\n# Ready\n",
        )
        .expect("ready task should write");

        let config = test_config(&operations_dir);

        let changed = migrate_pending_status(&config).expect("migration should run");
        assert_eq!(changed, 1, "only the one legacy file should be rewritten");

        let migrated =
            fs::read_to_string(task_dir.join("legacy.md")).expect("legacy should be readable");
        assert!(migrated.contains("status: review"), "content: {migrated}");
        assert!(!migrated.contains("status: pending"), "content: {migrated}");
        // Other frontmatter and body survive the rewrite.
        assert!(migrated.contains("assignee: claude"));
        assert!(migrated.contains("Body stays."));

        // Idempotent: a second pass rewrites nothing.
        let changed_again = migrate_pending_status(&config).expect("migration should run again");
        assert_eq!(changed_again, 0);

        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    #[test]
    fn migration_preserves_unknown_frontmatter_keys_and_comments() {
        let root = std::env::temp_dir().join(format!(
            "varda-migrate-preserve-{}",
            std::process::id()
        ));
        let operations_dir = root.join("operations");
        let task_dir = operations_dir.join("tasks/project");
        fs::create_dir_all(&task_dir).expect("task directory should be created");

        // Frontmatter with an unknown key, a comment, an inline comment on the
        // status line, and a body line that mentions `status: pending` (which
        // must NOT be rewritten because it is outside the frontmatter block).
        let original = "---\n# do not drop\nid: 7\ncustom_field: keep-me\nstatus: pending # legacy\nassignee: claude\nrequires_user: false\n---\n\n# Body\n\nThe old value was status: pending here.\n";
        let path = task_dir.join("legacy.md");
        fs::write(&path, original).expect("legacy task should write");

        let config = test_config(&operations_dir);
        let changed = migrate_pending_status(&config).expect("migration should run");
        assert_eq!(changed, 1, "the one legacy file should be rewritten");

        let migrated = fs::read_to_string(&path).expect("migrated file should be readable");
        // Only the frontmatter status value flipped; the inline comment survives.
        assert!(
            migrated.contains("status: review # legacy"),
            "content: {migrated}"
        );
        // Unknown key and the comment survive byte-for-byte.
        assert!(migrated.contains("# do not drop"), "content: {migrated}");
        assert!(migrated.contains("custom_field: keep-me"), "content: {migrated}");
        // The body mention of the old value is left untouched.
        assert!(
            migrated.contains("The old value was status: pending here."),
            "content: {migrated}"
        );
        // The expected byte-for-byte result: exactly `pending` -> `review` on
        // the frontmatter status line, nothing else changed.
        let expected = original.replacen("status: pending # legacy", "status: review # legacy", 1);
        assert_eq!(migrated, expected, "migration must be byte-preserving");

        // Idempotent: a second pass changes nothing.
        let changed_again = migrate_pending_status(&config).expect("migration reruns");
        assert_eq!(changed_again, 0);

        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    #[test]
    fn slugifies_task_name() {
        assert_eq!(
            slugify_task_name("Write README Please").expect("name should slugify"),
            "write-readme-please"
        );
    }

    #[test]
    fn lists_tasks_for_project() {
        let root = std::env::temp_dir().join(format!("varda-task-list-{}", std::process::id()));
        let task_dir = root.join("operations/tasks");
        fs::create_dir_all(&task_dir).expect("task directory should be created");
        let project_path = root.join("project");
        fs::create_dir_all(&project_path).expect("project directory should be created");
        let other_project_path = root.join("other-project");
        fs::create_dir_all(&other_project_path).expect("other project directory should be created");

        fs::write(
            task_dir.join("mine.md"),
            format!(
                r#"---
id: 2
status: pending
project: {}
assignee: codex
requires_user: false
---

# Mine
"#,
                project_path.display()
            ),
        )
        .expect("task should be written");
        fs::write(
            task_dir.join("other.md"),
            format!(
                r#"---
id: 1
status: ready
project: {}
requires_user: false
---

# Other
"#,
                other_project_path.display()
            ),
        )
        .expect("other task should be written");

        let config = Config {
            defaults: crate::config::Defaults {
                timeout_seconds: 600,
                operations_dir: root.join("operations").display().to_string(),
                sandbox: None,
                ..Default::default()
            },
            routes: vec![],
            agents: std::collections::BTreeMap::new(),
            roles: std::collections::BTreeMap::new(),
            git: crate::config::GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
            orchestration: crate::orchestration::OrchestrationPolicy::default(),
        };

        let tasks = list_tasks(&config, &project_path).expect("tasks should list");

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, Some(2));
        // The task file was written with the legacy `status: pending`; the serde
        // alias must load it as `Review` without error.
        assert_eq!(tasks[0].status, TaskStatus::Review);
        assert_eq!(tasks[0].title, "Mine");
        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    #[test]
    fn resolves_task_reference_by_numeric_id() {
        let root = std::env::temp_dir().join(format!("varda-task-resolve-{}", std::process::id()));
        let task_dir = root.join("operations/tasks");
        fs::create_dir_all(&task_dir).expect("task directory should be created");
        let task_path = task_dir.join("mine.md");
        fs::write(
            &task_path,
            r#"---
id: 42
status: ready
project: /work/project
requires_user: false
---

# Mine
"#,
        )
        .expect("task should be written");

        let config = Config {
            defaults: crate::config::Defaults {
                timeout_seconds: 600,
                operations_dir: root.join("operations").display().to_string(),
                sandbox: None,
                ..Default::default()
            },
            routes: vec![],
            agents: std::collections::BTreeMap::new(),
            roles: std::collections::BTreeMap::new(),
            git: crate::config::GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
            orchestration: crate::orchestration::OrchestrationPolicy::default(),
        };

        let resolved =
            resolve_task_reference(&config, Path::new("42")).expect("task id should resolve");

        assert_eq!(resolved, task_path);
        fs::remove_dir_all(root).expect("test directory should be removable");
    }
}
