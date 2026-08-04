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
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
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
    #[serde(default)]
    pub requires_user: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Backlog,
    Ready,
    Running,
    Pending,
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
            Self::Pending => "pending",
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
            "pending" => Ok(Self::Pending),
            "needs_user" => Ok(Self::NeedsUser),
            "failed" => Ok(Self::Failed),
            "done" => Ok(Self::Done),
            _ => anyhow::bail!(
                "unknown status '{}'; expected one of: backlog, ready, running, pending, needs_user, failed, done",
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

pub fn create_task(
    config: &Config,
    taskname: &str,
    project_path: &Path,
    assignee: Option<&str>,
    description: Option<&str>,
) -> Result<PathBuf> {
    let task_root = Path::new(&config.defaults.operations_dir).join("tasks");
    let task_dir = task_root.join(project_task_folder(project_path)?);
    fs::create_dir_all(&task_dir)
        .with_context(|| format!("failed to create task directory {}", task_dir.display()))?;
    let id = next_task_id(&task_root)?;

    let filename = format!("{}.md", slugify_task_name(taskname)?);
    let path = task_dir.join(filename);

    if path.exists() {
        bail!("task {} already exists", path.display());
    }

    let task = TaskDocument {
        path: path.clone(),
        frontmatter: TaskFrontmatter {
            id: Some(id),
            status: TaskStatus::Backlog,
            project: Some(project_path.display().to_string()),
            assignee: assignee.map(str::to_owned),
            recap: None,
            recaps: vec![],
            plan: None,
            agent_session_id: None,
            agent_session_log: None,
            agent_session_ids: vec![],
            agent_session_logs: vec![],
            agent_resume_commands: vec![],
            requires_user: false,
        },
        body: if let Some(desc) = description {
            format!("# {taskname}\n\n{desc}\n")
        } else {
            format!("# {taskname}\n\n")
        },
    };

    write_task(&task)?;

    Ok(path)
}

pub fn list_tasks(config: &Config, project_path: &Path) -> Result<Vec<TaskSummary>> {
    let task_dir = Path::new(&config.defaults.operations_dir).join("tasks");
    if !task_dir.exists() {
        return Ok(Vec::new());
    }

    let project_path = normalize_project_path(project_path)?;
    let mut tasks = Vec::new();
    collect_tasks(&task_dir, &project_path, &mut tasks)?;
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

    find_task_by_id(config, id)?.with_context(|| format!("no task found with id {id}"))
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

        if !entry_path
            .extension()
            .is_some_and(|extension| extension == "md")
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

        if !entry_path
            .extension()
            .is_some_and(|extension| extension == "md")
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

        if !entry_path
            .extension()
            .is_some_and(|extension| extension == "md")
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

fn next_task_id(task_dir: &Path) -> Result<u64> {
    Ok(max_task_id(task_dir)?.unwrap_or(0) + 1)
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
    if let Some(recap) = frontmatter.recap.take() {
        if frontmatter.recaps.is_empty() {
            frontmatter.recaps.push(recap);
        }
    }

    // Migrate legacy single agent_session fields to the list variants.
    if let Some(id) = frontmatter.agent_session_id.take() {
        if frontmatter.agent_session_ids.is_empty() {
            frontmatter.agent_session_ids.push(id);
        }
    }
    if let Some(log) = frontmatter.agent_session_log.take() {
        if frontmatter.agent_session_logs.is_empty() {
            frontmatter.agent_session_logs.push(log);
        }
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
    }

    #[test]
    fn serializes_task_frontmatter() {
        let mut task = TaskDocument {
            path: PathBuf::from("task.md"),
            frontmatter: TaskFrontmatter {
                id: None,
                status: TaskStatus::Running,
                project: None,
                assignee: Some("codex".to_owned()),
                recap: None,
                recaps: vec![],
                plan: None,
                agent_session_id: None,
                agent_session_log: None,
                agent_session_ids: vec![],
                agent_session_logs: vec![],
                agent_resume_commands: vec![],
                requires_user: false,
            },
            body: "# Task\n\nDo the work.\n".to_owned(),
        };

        task.set_status(TaskStatus::Pending);
        task.set_recap(".varda/operations/recaps/run.md");
        task.frontmatter
            .agent_session_ids
            .push("session-123".to_owned());
        task.frontmatter
            .agent_session_logs
            .push(".varda/operations/runs/session-123.log".to_owned());

        let frontmatter =
            serde_yaml::to_string(&task.frontmatter).expect("frontmatter should serialize");

        assert!(frontmatter.contains("status: pending"));
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
                id: Some(7),
                status: TaskStatus::Pending,
                project: None,
                assignee: Some("codex".to_owned()),
                recap: None,
                recaps: vec![".varda/operations/recaps/run.md".to_owned()],
                plan: None,
                agent_session_id: None,
                agent_session_log: None,
                agent_session_ids: vec![],
                agent_session_logs: vec![],
                agent_resume_commands: vec![],
                requires_user: false,
            },
            body: "# Task\n\nDo the work.\n".to_owned(),
        };

        write_task(&task).expect("task should write");
        let written = fs::read_to_string(&path).expect("task file should be readable");
        fs::remove_file(path).expect("task file should be removable");

        assert!(written.starts_with("---\n"));
        assert!(written.contains("status: pending"));
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
            },
            routes: vec![crate::config::Route {
                glob: "**".to_owned(),
                agents: vec!["codex".to_owned()],
                sandbox: None,
                mounts: Vec::new(),
            }],
            agents: std::collections::BTreeMap::new(),
            roles: std::collections::BTreeMap::new(),
            git: crate::config::GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
        };

        let project_path = Path::new("/work/project");
        let path = create_task(
            &config,
            "Write README Please",
            project_path,
            Some("codex"),
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
            },
            routes: vec![],
            agents: std::collections::BTreeMap::new(),
            roles: std::collections::BTreeMap::new(),
            git: crate::config::GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
        };

        let path = create_task(&config, "Next Task", Path::new("/work/project"), None, None)
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
            },
            routes: vec![],
            agents: std::collections::BTreeMap::new(),
            roles: std::collections::BTreeMap::new(),
            git: crate::config::GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
        };

        let first = create_task(&config, "Project Task", &first_project, None, None)
            .expect("first task should be created");
        let second = create_task(&config, "Project Task", &second_project, None, None)
            .expect("second task should be created");

        assert_ne!(first.parent(), second.parent());
        assert_eq!(first.file_name(), second.file_name());
        assert!(first.starts_with(operations_dir.join("tasks")));
        assert!(second.starts_with(operations_dir.join("tasks")));
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
            },
            routes: vec![],
            agents: std::collections::BTreeMap::new(),
            roles: std::collections::BTreeMap::new(),
            git: crate::config::GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
        };

        let tasks = list_tasks(&config, &project_path).expect("tasks should list");

        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, Some(2));
        assert_eq!(tasks[0].status, TaskStatus::Pending);
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
            },
            routes: vec![],
            agents: std::collections::BTreeMap::new(),
            roles: std::collections::BTreeMap::new(),
            git: crate::config::GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
        };

        let resolved =
            resolve_task_reference(&config, Path::new("42")).expect("task id should resolve");

        assert_eq!(resolved, task_path);
        fs::remove_dir_all(root).expect("test directory should be removable");
    }
}
