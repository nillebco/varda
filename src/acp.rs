//! ACP transport support.

use std::process::Stdio;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::agent::{AgentClient, AgentRunRequest, AgentRunResult, build_agent_instructions};
use crate::config::AgentConfig;

#[derive(Debug, Clone)]
pub struct AcpSubprocessClient {
    agent_name: String,
    command: String,
    args: Vec<String>,
}

impl AcpSubprocessClient {
    pub fn new(agent_name: impl Into<String>, config: &AgentConfig) -> Self {
        Self {
            agent_name: agent_name.into(),
            command: config.command.clone(),
            args: config.args.clone(),
        }
    }
}

#[async_trait]
impl AgentClient for AcpSubprocessClient {
    async fn run_task(&self, request: AgentRunRequest) -> Result<AgentRunResult> {
        let prompt = build_prompt(&request);
        let args = args_for_request(&self.args, &request);
        let mut child = Command::new(&self.command)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "failed to start agent '{}' with command '{}'",
                    self.agent_name, self.command
                )
            })?;

        let mut stdin = child.stdin.take().context("failed to open agent stdin")?;
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("failed to write task prompt to agent stdin")?;
        drop(stdin);

        let output = child
            .wait_with_output()
            .await
            .context("failed to wait for agent subprocess")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!(
                "agent '{}' exited with status {}; stderr: {}",
                self.agent_name,
                output.status,
                stderr.trim()
            );
        }

        let recap = String::from_utf8(output.stdout)
            .context("agent stdout was not valid UTF-8")?
            .trim()
            .to_owned();

        if recap.is_empty() {
            bail!("agent '{}' produced an empty recap", self.agent_name);
        }

        Ok(AgentRunResult {
            requires_user: recap_contains_user_request(&recap),
            suggested_agent: None,
            recap,
        })
    }
}

fn build_prompt(request: &AgentRunRequest) -> String {
    format!(
        r#"{instructions}

Agent: {agent}
Task path: {task_path}
Task frontmatter:
{frontmatter}

Task markdown:
{body}
"#,
        instructions = build_agent_instructions(request.timeout),
        agent = request.agent_name,
        task_path = request.task_path,
        frontmatter = serde_yaml::to_string(&request.frontmatter)
            .unwrap_or_else(|_| "<frontmatter serialization failed>".to_owned()),
        body = request.body,
    )
}

fn args_for_request(args: &[String], request: &AgentRunRequest) -> Vec<String> {
    let Some(project) = request.frontmatter.project.as_deref() else {
        return args.to_vec();
    };

    let mut resolved = Vec::with_capacity(args.len());
    let mut index = 0;

    while index < args.len() {
        let arg = &args[index];
        resolved.push(expand_arg(arg, request, project));

        if arg == "--cd" {
            if let Some(value) = args.get(index + 1) {
                resolved.push(if value == "." {
                    project.to_owned()
                } else {
                    expand_arg(value, request, project)
                });
                index += 2;
                continue;
            }
        }

        index += 1;
    }

    resolved
}

fn expand_arg(arg: &str, request: &AgentRunRequest, project: &str) -> String {
    arg.replace("{project}", project)
        .replace("{task}", &request.task_path)
}

fn recap_contains_user_request(recap: &str) -> bool {
    recap
        .lines()
        .any(|line| line.trim().eq_ignore_ascii_case("requires_user: true"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::task::{TaskFrontmatter, TaskStatus};

    use super::*;

    #[tokio::test]
    async fn subprocess_client_sends_prompt_and_captures_recap() {
        let config = AgentConfig {
            kind: crate::config::AgentKind::Acp,
            command: "cat".to_owned(),
            args: vec![],
        };
        let client = AcpSubprocessClient::new("echo", &config);

        let result = client
            .run_task(AgentRunRequest {
                agent_name: "echo".to_owned(),
                task_path: "task.md".to_owned(),
                frontmatter: TaskFrontmatter {
                    id: None,
                    status: TaskStatus::Ready,
                    project: Some("/work/project".to_owned()),
                    assignee: Some("echo".to_owned()),
                    recap: None,
                    requires_user: false,
                },
                body: "# Task\n\nDo it.".to_owned(),
                timeout: Duration::from_secs(600),
            })
            .await
            .expect("subprocess should echo prompt");

        assert!(result.recap.contains("You have at most 10 minutes"));
        assert!(result.recap.contains("Do it."));
        assert!(!result.requires_user);
    }

    #[test]
    fn detects_requires_user_marker() {
        assert!(recap_contains_user_request(
            "Completed nothing.\nrequires_user: true"
        ));
        assert!(!recap_contains_user_request("requires_user: false"));
    }

    #[test]
    fn replaces_dot_cd_with_task_project_path() {
        let request = AgentRunRequest {
            agent_name: "codex".to_owned(),
            task_path: "/home/user/.varda/operations/tasks/task.md".to_owned(),
            frontmatter: TaskFrontmatter {
                id: None,
                status: TaskStatus::Ready,
                project: Some("/work/project".to_owned()),
                assignee: Some("codex".to_owned()),
                recap: None,
                requires_user: false,
            },
            body: "# Task".to_owned(),
            timeout: Duration::from_secs(600),
        };

        let args = args_for_request(
            &[
                "exec".to_owned(),
                "--cd".to_owned(),
                ".".to_owned(),
                "--sandbox".to_owned(),
                "workspace-write".to_owned(),
                "-".to_owned(),
            ],
            &request,
        );

        assert_eq!(
            args,
            vec![
                "exec",
                "--cd",
                "/work/project",
                "--sandbox",
                "workspace-write",
                "-"
            ]
        );
    }
}
