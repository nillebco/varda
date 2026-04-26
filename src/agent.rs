//! Agent execution abstractions.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::task::TaskFrontmatter;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRunRequest {
    pub agent_name: String,
    pub task_path: String,
    pub frontmatter: TaskFrontmatter,
    pub body: String,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRunResult {
    pub recap: String,
    pub requires_user: bool,
    pub suggested_agent: Option<String>,
}

#[async_trait]
pub trait AgentClient {
    async fn run_task(&self, request: AgentRunRequest) -> Result<AgentRunResult>;
}

pub fn build_agent_instructions(timeout: Duration) -> String {
    let minutes = timeout.as_secs() / 60;

    format!(
        r#"You are processing a task managed by Varda.

You have at most {minutes} minutes.

Before the time limit expires, produce a concise recap for the end user.
The recap must include:
- what you completed
- what remains
- any blockers
- whether user interaction is required
- suggested next agent, if applicable

If you need user input, stop and mark the result as requires_user."#
    )
}

#[cfg(test)]
pub mod fake {
    use super::*;

    #[derive(Debug, Clone)]
    pub struct FakeAgentClient {
        result: AgentRunResult,
    }

    impl FakeAgentClient {
        pub fn new(result: AgentRunResult) -> Self {
            Self { result }
        }
    }

    #[async_trait]
    impl AgentClient for FakeAgentClient {
        async fn run_task(&self, _request: AgentRunRequest) -> Result<AgentRunResult> {
            Ok(self.result.clone())
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::task::TaskStatus;

    use super::fake::FakeAgentClient;
    use super::*;

    #[test]
    fn builds_time_limited_agent_instructions() {
        let instructions = build_agent_instructions(Duration::from_secs(600));

        assert!(instructions.contains("at most 10 minutes"));
        assert!(instructions.contains("produce a concise recap"));
        assert!(instructions.contains("requires_user"));
    }

    #[tokio::test]
    async fn fake_agent_returns_configured_result() {
        let client = FakeAgentClient::new(AgentRunResult {
            recap: "done".to_owned(),
            requires_user: false,
            suggested_agent: Some("codex".to_owned()),
        });

        let result = client
            .run_task(AgentRunRequest {
                agent_name: "codex".to_owned(),
                task_path: "task.md".to_owned(),
                frontmatter: TaskFrontmatter {
                    status: TaskStatus::Ready,
                    project: Some("/work/project".to_owned()),
                    assignee: Some("codex".to_owned()),
                    recap: None,
                    requires_user: false,
                },
                body: "# Task".to_owned(),
                timeout: Duration::from_secs(600),
            })
            .await
            .expect("fake agent should return a result");

        assert_eq!(result.recap, "done");
        assert!(!result.requires_user);
        assert_eq!(result.suggested_agent.as_deref(), Some("codex"));
    }
}
