//! End-to-end task execution flow.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::time;
use uuid::Uuid;

use crate::agent::{AgentClient, AgentRunRequest, AgentRunResult};
use crate::config::Config;
use crate::task::{TaskStatus, load_task, write_task};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    pub status: TaskStatus,
    pub recap_path: PathBuf,
}

pub async fn run_task(
    config: &Config,
    agent_name: &str,
    task_path: &Path,
    client: &impl AgentClient,
) -> Result<RunOutcome> {
    let mut task = load_task(task_path)?;

    if task.frontmatter.status != TaskStatus::Ready {
        bail!(
            "task {} is not ready; current status is {:?}",
            task_path.display(),
            task.frontmatter.status
        );
    }

    task.set_status(TaskStatus::Running);
    write_task(&task)?;

    let timeout = Duration::from_secs(config.defaults.timeout_seconds);
    let request = AgentRunRequest {
        agent_name: agent_name.to_owned(),
        task_path: task_path.display().to_string(),
        frontmatter: task.frontmatter.clone(),
        body: task.body.clone(),
        timeout,
    };

    let result = match time::timeout(timeout, client.run_task(request)).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => AgentRunResult {
            recap: format!(
                "# Agent Run Failed\n\nThe agent failed while processing `{}`.\n\nError: {error}",
                task_path.display()
            ),
            requires_user: false,
            suggested_agent: None,
        },
        Err(_) => AgentRunResult {
            recap: format!(
                "# Agent Run Timed Out\n\nThe agent exceeded the configured {} second limit while processing `{}`.",
                config.defaults.timeout_seconds,
                task_path.display()
            ),
            requires_user: false,
            suggested_agent: None,
        },
    };

    let recap_path = write_recap(config, &result.recap)?;
    task.set_recap(recap_path.display().to_string());
    task.frontmatter.requires_user = result.requires_user;

    let status = if result.requires_user {
        TaskStatus::NeedsUser
    } else if result.recap.starts_with("# Agent Run Failed")
        || result.recap.starts_with("# Agent Run Timed Out")
    {
        TaskStatus::Failed
    } else {
        TaskStatus::Pending
    };

    task.set_status(status);
    write_task(&task)?;

    Ok(RunOutcome { status, recap_path })
}

fn write_recap(config: &Config, recap: &str) -> Result<PathBuf> {
    let recap_dir = Path::new(&config.defaults.operations_dir).join("recaps");
    fs::create_dir_all(&recap_dir)
        .with_context(|| format!("failed to create recap directory {}", recap_dir.display()))?;

    let recap_path = recap_dir.join(format!("{}.md", Uuid::new_v4()));
    fs::write(&recap_path, recap)
        .with_context(|| format!("failed to write recap at {}", recap_path.display()))?;

    Ok(recap_path)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::agent::AgentRunResult;
    use crate::agent::fake::FakeAgentClient;
    use crate::config::{AgentConfig, AgentKind, Defaults, GitConfig, Route};

    use super::*;

    #[tokio::test]
    async fn run_task_marks_successful_task_pending_and_writes_recap() {
        let root = std::env::temp_dir().join(format!("varda-run-{}", std::process::id()));
        let operations_dir = root.join("operations");
        let task_dir = operations_dir.join("tasks/codex");
        fs::create_dir_all(&task_dir).expect("task directory should be created");
        let task_path = task_dir.join("example.md");
        fs::write(
            &task_path,
            r#"---
status: ready
assignee: codex
requires_user: false
---

# Task

Do it.
"#,
        )
        .expect("task should be written");

        let config = test_config(operations_dir.display().to_string());
        let client = FakeAgentClient::new(AgentRunResult {
            recap: "# Recap\n\nCompleted.".to_owned(),
            requires_user: false,
            suggested_agent: None,
        });

        let outcome = run_task(&config, "codex", &task_path, &client)
            .await
            .expect("task should run");

        let updated = fs::read_to_string(&task_path).expect("task should be readable");
        let recap = fs::read_to_string(&outcome.recap_path).expect("recap should be readable");

        assert_eq!(outcome.status, TaskStatus::Pending);
        assert!(updated.contains("status: pending"));
        assert!(updated.contains("recap:"));
        assert!(recap.contains("Completed."));
    }

    fn test_config(operations_dir: String) -> Config {
        Config {
            defaults: Defaults {
                timeout_seconds: 600,
                operations_dir,
            },
            routes: vec![Route {
                glob: ".varda/operations/tasks/codex/**/*.md".to_owned(),
                agent: "codex".to_owned(),
            }],
            agents: BTreeMap::from([(
                "codex".to_owned(),
                AgentConfig {
                    kind: AgentKind::Acp,
                    command: "codex".to_owned(),
                    args: vec![],
                },
            )]),
            git: GitConfig { auto_commit: true },
        }
    }
}
