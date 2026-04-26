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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskFrontmatter {
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recap: Option<String>,
    #[serde(default)]
    pub requires_user: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Ready,
    Running,
    Pending,
    NeedsUser,
    Failed,
}

impl TaskDocument {
    pub fn set_status(&mut self, status: TaskStatus) {
        self.frontmatter.status = status;
    }

    pub fn set_recap(&mut self, recap: impl Into<String>) {
        self.frontmatter.recap = Some(recap.into());
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

pub fn default_assignee(config: &Config) -> Result<String> {
    let route = config
        .routes
        .first()
        .context("config has no routes; cannot choose a default assignee")?;

    Ok(route.agent.clone())
}

pub fn create_task(config: &Config, taskname: &str, assignee: Option<&str>) -> Result<PathBuf> {
    let route = config
        .routes
        .first()
        .context("config has no routes; cannot choose a task directory")?;
    let task_dir = task_dir_from_glob(&route.glob)?;
    fs::create_dir_all(&task_dir)
        .with_context(|| format!("failed to create task directory {}", task_dir.display()))?;

    let filename = format!("{}.md", slugify_task_name(taskname)?);
    let path = task_dir.join(filename);

    if path.exists() {
        bail!("task {} already exists", path.display());
    }

    let task = TaskDocument {
        path: path.clone(),
        frontmatter: TaskFrontmatter {
            status: TaskStatus::Ready,
            assignee: assignee.map(str::to_owned),
            recap: None,
            requires_user: false,
        },
        body: format!("# {taskname}\n\n"),
    };

    write_task(&task)?;

    Ok(path)
}

fn parse_task(path: &Path, content: &str) -> Result<TaskDocument> {
    let matter = Matter::<YAML>::new();
    let parsed = matter
        .parse::<TaskFrontmatter>(content)
        .context("failed to parse markdown frontmatter")?;
    let frontmatter = parsed
        .data
        .with_context(|| format!("missing or invalid frontmatter in {}", path.display()))?;

    Ok(TaskDocument {
        path: path.to_path_buf(),
        frontmatter,
        body: parsed.content,
    })
}

fn task_dir_from_glob(glob: &str) -> Result<PathBuf> {
    let prefix = glob
        .split_once("**")
        .map(|(prefix, _)| prefix)
        .unwrap_or(glob);
    let prefix = prefix.trim_end_matches('/');
    let path = Path::new(prefix);

    if prefix.contains(['*', '?', '[']) {
        let stable_prefix = prefix
            .split(['*', '?', '['])
            .next()
            .unwrap_or("")
            .trim_end_matches('/');
        let stable_path = Path::new(stable_prefix);
        return stable_path
            .parent()
            .map(Path::to_path_buf)
            .context("route glob does not contain a usable task directory");
    }

    if path.extension().is_some() {
        return path
            .parent()
            .map(Path::to_path_buf)
            .context("route glob does not contain a usable task directory");
    }

    Ok(path.to_path_buf())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_task_frontmatter_and_body() {
        let task = parse_task(
            Path::new(".varda/operations/tasks/codex/example.md"),
            r#"---
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
        assert_eq!(task.frontmatter.assignee.as_deref(), Some("codex"));
        assert!(!task.frontmatter.requires_user);
        assert!(task.body.contains("Do the work."));
    }

    #[test]
    fn serializes_task_frontmatter() {
        let mut task = TaskDocument {
            path: PathBuf::from("task.md"),
            frontmatter: TaskFrontmatter {
                status: TaskStatus::Running,
                assignee: Some("codex".to_owned()),
                recap: None,
                requires_user: false,
            },
            body: "# Task\n\nDo the work.\n".to_owned(),
        };

        task.set_status(TaskStatus::Pending);
        task.set_recap(".varda/operations/recaps/run.md");

        let frontmatter =
            serde_yaml::to_string(&task.frontmatter).expect("frontmatter should serialize");

        assert!(frontmatter.contains("status: pending"));
        assert!(frontmatter.contains("recap: .varda/operations/recaps/run.md"));
    }

    #[test]
    fn writes_task_document() {
        let path = std::env::temp_dir().join(format!("varda-task-write-{}.md", std::process::id()));
        let task = TaskDocument {
            path: path.clone(),
            frontmatter: TaskFrontmatter {
                status: TaskStatus::Pending,
                assignee: Some("codex".to_owned()),
                recap: Some(".varda/operations/recaps/run.md".to_owned()),
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
        let task_glob = operations_dir
            .join("tasks/codex/**/*.md")
            .display()
            .to_string();
        let config = Config {
            defaults: crate::config::Defaults {
                timeout_seconds: 600,
                operations_dir: operations_dir.display().to_string(),
            },
            routes: vec![crate::config::Route {
                glob: task_glob,
                agent: "codex".to_owned(),
            }],
            agents: std::collections::BTreeMap::new(),
            git: crate::config::GitConfig { auto_commit: true },
        };

        let path = create_task(&config, "Write README Please", Some("codex"))
            .expect("task should be created");
        let content = fs::read_to_string(path).expect("task should be readable");

        assert!(content.contains("status: ready"));
        assert!(content.contains("assignee: codex"));
        assert!(content.contains("# Write README Please"));
    }

    #[test]
    fn derives_task_dir_from_glob() {
        let dir = task_dir_from_glob(".varda/operations/tasks/codex/**/*.md")
            .expect("glob should produce a task dir");

        assert_eq!(dir, PathBuf::from(".varda/operations/tasks/codex"));
    }

    #[test]
    fn slugifies_task_name() {
        assert_eq!(
            slugify_task_name("Write README Please").expect("name should slugify"),
            "write-readme-please"
        );
    }
}
