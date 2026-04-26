//! Markdown task parsing and updates.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use gray_matter::{Matter, engine::YAML};
use serde::{Deserialize, Serialize};

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
}
