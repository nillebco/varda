//! End-to-end task execution flow.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::time;
use uuid::Uuid;

use crate::agent::{AgentClient, AgentRunRequest, AgentRunResult, recap_requires_user_interaction};
use crate::config::Config;
use crate::task::{TaskStatus, load_task, write_task};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    pub status: TaskStatus,
    pub recap_path: PathBuf,
    pub session_log_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanOutcome {
    pub plan_path: PathBuf,
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

    let timeout = Duration::from_secs(config.defaults.timeout_seconds);
    let session_id = Uuid::new_v4().to_string();
    let session_log_path = session_log_path(config, &session_id);
    task.set_status(TaskStatus::Running);
    task.frontmatter.agent_session_id = Some(session_id.clone());
    task.frontmatter.agent_session_log = Some(session_log_path.display().to_string());
    write_task(&task)?;

    write_session_log(
        &session_log_path,
        &format!(
            "session_id={session_id}\nagent={agent_name}\ntask={}\n",
            task_path.display()
        ),
    )?;
    let request = AgentRunRequest {
        agent_name: agent_name.to_owned(),
        task_path: task_path.display().to_string(),
        frontmatter: task.frontmatter.clone(),
        body: task.body.clone(),
        timeout,
        session_id: session_id.clone(),
        session_log_path: Some(session_log_path.display().to_string()),
    };

    let result = match time::timeout(timeout, client.run_task(request)).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            append_session_log(&session_log_path, &format!("\nerror:\n{error:#}\n"))?;
            AgentRunResult {
                recap: format!(
                    "# Agent Run Failed\n\nThe agent failed while processing `{}`.\n\nError: {error}\n\nSession ID: `{session_id}`\n\nSession log: [{}]({})",
                    task_path.display(),
                    session_log_path.display(),
                    session_log_path.display()
                ),
                requires_user: false,
                suggested_agent: None,
            }
        }
        Err(_) => {
            append_session_log(
                &session_log_path,
                &format!(
                    "\ntimeout:\nexceeded {} second limit\n",
                    config.defaults.timeout_seconds
                ),
            )?;
            AgentRunResult {
                recap: format!(
                    "# Agent Run Timed Out\n\nThe agent exceeded the configured {} second limit while processing `{}`.\n\nSession ID: `{session_id}`\n\nSession log: [{}]({})",
                    config.defaults.timeout_seconds,
                    task_path.display(),
                    session_log_path.display(),
                    session_log_path.display()
                ),
                requires_user: false,
                suggested_agent: None,
            }
        }
    };

    let requires_user = result.requires_user || recap_requires_user_interaction(&result.recap);
    let recap_path = write_recap(config, task_path, &result.recap)?;
    task.set_recap(recap_path.display().to_string());
    task.frontmatter.requires_user = requires_user;

    let status = if requires_user {
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

    Ok(RunOutcome {
        status,
        recap_path,
        session_log_path,
    })
}

pub async fn plan_task(
    config: &Config,
    agent_name: &str,
    task_path: &Path,
    client: &(impl AgentClient + Sync),
) -> Result<PlanOutcome> {
    let mut task = load_task(task_path)?;

    let timeout = Duration::from_secs(config.defaults.timeout_seconds);
    let request = AgentRunRequest {
        agent_name: agent_name.to_owned(),
        task_path: task_path.display().to_string(),
        frontmatter: task.frontmatter.clone(),
        body: task.body.clone(),
        timeout,
        session_id: Uuid::new_v4().to_string(),
        session_log_path: None,
    };

    let result = match time::timeout(timeout, client.plan_task(request)).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => AgentRunResult {
            recap: format!(
                "# Planning Failed\n\nThe agent failed while planning `{}`.\n\nError: {error}",
                task_path.display()
            ),
            requires_user: false,
            suggested_agent: None,
        },
        Err(_) => AgentRunResult {
            recap: format!(
                "# Planning Timed Out\n\nThe agent exceeded the configured {} second limit while planning `{}`.",
                config.defaults.timeout_seconds,
                task_path.display()
            ),
            requires_user: false,
            suggested_agent: None,
        },
    };

    let plan_path = write_plan(config, task_path, &result.recap)?;
    task.set_plan(plan_path.display().to_string());
    write_task(&task)?;

    Ok(PlanOutcome { plan_path })
}

fn write_plan(config: &Config, task_path: &Path, plan: &str) -> Result<PathBuf> {
    let plan_dir = Path::new(&config.defaults.operations_dir).join("plans");
    fs::create_dir_all(&plan_dir)
        .with_context(|| format!("failed to create plan directory {}", plan_dir.display()))?;
    let stem = task_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("task");
    let plan_path = plan_dir.join(format!("{stem}-plan.md"));
    fs::write(&plan_path, plan)
        .with_context(|| format!("failed to write plan at {}", plan_path.display()))?;
    Ok(plan_path)
}

fn write_recap(config: &Config, task_path: &Path, recap: &str) -> Result<PathBuf> {
    let recap_dir = Path::new(&config.defaults.operations_dir).join("recaps");
    fs::create_dir_all(&recap_dir)
        .with_context(|| format!("failed to create recap directory {}", recap_dir.display()))?;

    let recap_path = recap_dir.join(format!("{}.md", Uuid::new_v4()));
    let content = format!("---\ntask: {}\n---\n\n{}", task_path.display(), recap);
    fs::write(&recap_path, &content)
        .with_context(|| format!("failed to write recap at {}", recap_path.display()))?;

    Ok(recap_path)
}

fn session_log_path(config: &Config, session_id: &str) -> PathBuf {
    Path::new(&config.defaults.operations_dir)
        .join("runs")
        .join(format!("{session_id}.log"))
}

fn write_session_log(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create session log directory {}",
                parent.display()
            )
        })?;
    }
    fs::write(path, content)
        .with_context(|| format!("failed to write session log at {}", path.display()))
}

fn append_session_log(path: &Path, content: &str) -> Result<()> {
    use std::io::Write;

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open session log at {}", path.display()))?;
    file.write_all(content.as_bytes())
        .with_context(|| format!("failed to append session log at {}", path.display()))
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
project: /work/project
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
        assert!(updated.contains("recaps:"));
        assert!(updated.contains("agent_session_id:"));
        assert!(updated.contains("agent_session_log:"));
        assert!(outcome.session_log_path.exists());
        assert!(recap.contains("Completed."));
    }

    #[tokio::test]
    async fn run_task_marks_needs_user_from_recap_wording() {
        let root =
            std::env::temp_dir().join(format!("varda-run-needs-user-{}", std::process::id()));
        let operations_dir = root.join("operations");
        let task_dir = operations_dir.join("tasks/codex");
        fs::create_dir_all(&task_dir).expect("task directory should be created");
        let task_path = task_dir.join("example.md");
        fs::write(
            &task_path,
            r#"---
status: ready
project: /work/project
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
            recap: "# Recap\n\n**User Interaction Required**\nYes: run this locally.".to_owned(),
            requires_user: false,
            suggested_agent: None,
        });

        let outcome = run_task(&config, "codex", &task_path, &client)
            .await
            .expect("task should run");

        let updated = fs::read_to_string(&task_path).expect("task should be readable");

        assert_eq!(outcome.status, TaskStatus::NeedsUser);
        assert!(updated.contains("status: needs_user"));
        assert!(updated.contains("requires_user: true"));
    }

    fn test_config(operations_dir: String) -> Config {
        Config {
            defaults: Defaults {
                timeout_seconds: 600,
                operations_dir,
            },
            routes: vec![Route {
                glob: "**".to_owned(),
                agents: vec!["codex".to_owned()],
            }],
            agents: BTreeMap::from([(
                "codex".to_owned(),
                AgentConfig {
                    kind: AgentKind::Acp,
                    command: "codex".to_owned(),
                    args: vec![],
                    working_dir: None,
                    env: BTreeMap::new(),
                },
            )]),
            git: GitConfig { auto_commit: true },
        }
    }
}
