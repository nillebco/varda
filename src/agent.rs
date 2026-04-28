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
    pub session_id: String,
    pub session_log_path: Option<String>,
    /// When true, tee stdout to the terminal and forward terminal stdin to the agent.
    pub interactive: bool,
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

    async fn plan_task(&self, request: AgentRunRequest) -> Result<AgentRunResult> {
        self.run_task(request).await
    }
}

pub fn build_planning_instructions(timeout: Duration) -> String {
    let minutes = timeout.as_secs() / 60;

    format!(
        r#"You are producing an execution plan for a task managed by Varda.

You have at most {minutes} minutes.

Analyze the task and produce a structured plan. Do NOT execute the task.

Your plan must include:
- A brief summary of what the task requires
- Numbered implementation steps in execution order
- Potential blockers or risks for each step
- Any open questions or ambiguities that should be resolved before starting

Format the plan as markdown starting with a `# Plan` heading."#
    )
}

pub fn build_agent_instructions(timeout: Duration) -> String {
    let minutes = timeout.as_secs() / 60;

    format!(
        r#"You are processing a task managed by Varda.

You have at most {minutes} minutes.

Read and follow all project instructions from CLAUDE.md, AGENTS.md, and copilot-instructions.md found in the project folder. When those files are present, Varda includes their contents below as Project instructions; treat them as mandatory task requirements.

Before the time limit expires, produce a concise recap for the end user.
The recap must include:
- what you completed
- what remains
- any blockers
- whether user interaction is required
- suggested next agent, if applicable

In your recap, include a section listing every file you created, modified, or deleted during this session.
Add a markdown heading called Files touched and list one absolute file path per line below it.
If no files were changed, write (none) under that heading.

At the end of the recap, include exactly one bare machine-readable marker line whose content is either `requires_user: true` or `requires_user: false`.

If you need user input, stop and use the true marker."#
    )
}

pub fn recap_requires_user_interaction(recap: &str) -> bool {
    let mut previous_line_was_user_interaction_heading = false;

    for line in recap.lines() {
        let normalized = normalize_recap_line(line);

        if normalized.eq_ignore_ascii_case("requires_user: true") {
            return true;
        }

        if let Some(answer) = normalized
            .strip_prefix("user interaction required:")
            .or_else(|| normalized.strip_prefix("user interaction required -"))
        {
            if answer_starts_yes(answer) {
                return true;
            }
            continue;
        }

        if previous_line_was_user_interaction_heading {
            if normalized.is_empty() {
                continue;
            }
            if answer_starts_yes(&normalized) {
                return true;
            }
        }

        previous_line_was_user_interaction_heading =
            normalized.eq_ignore_ascii_case("user interaction required");
    }

    false
}

fn normalize_recap_line(line: &str) -> String {
    line.trim()
        .trim_start_matches('#')
        .trim()
        .trim_matches('*')
        .trim()
        .to_ascii_lowercase()
}

fn answer_starts_yes(answer: &str) -> bool {
    let answer = answer.trim_start();
    answer == "yes"
        || answer.strip_prefix("yes").is_some_and(|rest| {
            rest.starts_with(':')
                || rest.starts_with('.')
                || rest.starts_with('-')
                || rest.starts_with(',')
        })
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
        assert!(instructions.contains("Project instructions"));
        assert!(instructions.contains("Files touched"));
        assert!(instructions.contains("absolute file path"));
        assert!(instructions.contains("requires_user"));
    }

    #[test]
    fn detects_requires_user_recap_markers() {
        assert!(recap_requires_user_interaction(
            "Completed nothing.\nrequires_user: true"
        ));
        assert!(recap_requires_user_interaction(
            "**User Interaction Required**\nYes: run the smoke suite locally."
        ));
        assert!(recap_requires_user_interaction(
            "User interaction required: yes, provide credentials."
        ));
        assert!(!recap_requires_user_interaction("requires_user: false"));
        assert!(!recap_requires_user_interaction(
            "User interaction required: no.\nContinue with codex."
        ));
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
                    id: None,
                    status: TaskStatus::Ready,
                    project: Some("/work/project".to_owned()),
                    assignee: Some("codex".to_owned()),
                    recap: None,
                    recaps: vec![],
                    plan: None,
                    agent_session_id: None,
                    agent_session_log: None,
                    agent_session_ids: vec![],
                    agent_session_logs: vec![],
                    requires_user: false,
                },
                body: "# Task".to_owned(),
                timeout: Duration::from_secs(600),
                session_id: "session-1".to_owned(),
                session_log_path: None,
                interactive: false,
            })
            .await
            .expect("fake agent should return a result");

        assert_eq!(result.recap, "done");
        assert!(!result.requires_user);
        assert_eq!(result.suggested_agent.as_deref(), Some("codex"));
    }
}
