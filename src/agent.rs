//! Agent execution abstractions.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;

use crate::task::TaskFrontmatter;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRunRequest {
    pub agent_name: String,
    pub role_instructions: Option<String>,
    pub task_path: String,
    pub frontmatter: TaskFrontmatter,
    pub body: String,
    pub timeout: Duration,
    pub session_id: String,
    pub session_log_path: Option<String>,
    /// When true, tee stdout to the terminal and forward terminal stdin to the agent.
    pub interactive: bool,
    /// When true, the agent should only interpret a prior session log into a Varda recap
    /// without performing any new work. Mutually exclusive with `interactive`.
    pub interpret: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRunResult {
    pub recap: String,
    pub requires_user: bool,
    pub suggested_agent: Option<String>,
    /// Command the user can run to resume the agent session, when one was discovered.
    /// Populated for interactive runs from the per-agent `resume_command_template`
    /// in the config, with the discovered external session id substituted in.
    pub resume_command: Option<String>,
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

Do NOT run `git add`, `git commit`, or any other git history-modifying command. Leave your changes in the working tree, unstaged. Varda stages and commits exactly the files you list under `Files touched` after the run finishes.

At the end of the recap, include exactly one bare machine-readable marker line whose content is either `requires_user: true` or `requires_user: false`.

If you need user input, stop and use the true marker."#
    )
}

pub fn build_interactive_instructions() -> String {
    r#"You are running an interactive task session managed by Varda.

Read and follow all project instructions from CLAUDE.md, AGENTS.md, and copilot-instructions.md found in the project folder. When those files are present, Varda includes their contents below as Project instructions; treat them as mandatory task requirements.

Help the user accomplish the task. Collaborate with them as you normally would in an interactive shell session: ask clarifying questions when needed, take actions, and report results conversationally. There is no time limit.

Do NOT run `git add`, `git commit`, or any other git history-modifying command. Leave the changes in the working tree, unstaged. Varda commits the files reported by the interpreter pass after the session ends.

Do NOT produce a structured Varda recap, file list, or `requires_user` marker yourself. Once the session ends, Varda will pass the session log to a separate interpreter pass that produces those artifacts. Just focus on doing the work with the user."#.to_owned()
}

pub fn build_interpretation_instructions() -> String {
    r#"You are interpreting a completed interactive Varda session and producing the post-session recap.

A previous interactive session for this task has already ended. Your only job is to read the session log (and any referenced external transcripts you can access) and produce the recap that Varda needs.

Do NOT perform any new work. Do not edit, create, or delete files. Do not run commands beyond what is needed to read the session log or transcripts. Only summarize what already happened.

Produce a concise recap for the end user that includes:
- what was completed during the session
- what remains
- any blockers encountered
- whether user interaction is required to continue
- suggested next agent, if applicable

Include a section listing every file that was created, modified, or deleted during the session, based on what the session log shows.
Add a markdown heading called Files touched and list one absolute file path per line below it.
If no files were changed, write (none) under that heading.

Varda will use this list to stage and commit the changes after the run, so be precise: list only files that were actually touched and use absolute paths. Do not run `git add`, `git commit`, or any other git history-modifying command yourself.

At the end of the recap, include exactly one bare machine-readable marker line whose content is either `requires_user: true` or `requires_user: false`."#.to_owned()
}

/// Parse the `Files touched` section of a recap into a list of absolute paths.
///
/// The recap convention is documented in `build_agent_instructions`: a markdown
/// heading whose text reads "Files touched" (any heading level), followed by one
/// path per line until the next heading or end of input. List markers (`-`, `*`,
/// `•`), backticks, and surrounding whitespace are stripped. Lines that read
/// `(none)` are ignored, as are non-absolute paths (which Varda cannot stage
/// safely without guessing the repository root).
pub fn parse_files_touched(recap: &str) -> Vec<PathBuf> {
    let mut in_section = false;
    let mut files = Vec::new();

    for line in recap.lines() {
        let trimmed = line.trim();

        if let Some(rest) = trimmed.strip_prefix('#') {
            let heading_text = rest.trim_start_matches('#').trim();
            in_section = heading_text.eq_ignore_ascii_case("files touched");
            continue;
        }

        if !in_section || trimmed.is_empty() {
            continue;
        }

        let path_str = trimmed
            .trim_start_matches(|c: char| c == '-' || c == '*' || c == '•')
            .trim()
            .trim_matches('`')
            .trim();

        if path_str.is_empty() || path_str.eq_ignore_ascii_case("(none)") {
            continue;
        }

        let path = PathBuf::from(path_str);
        if path.is_absolute() {
            files.push(path);
        }
    }

    files
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
        assert!(instructions.contains("Do NOT run `git add`"));
        assert!(!instructions.contains("Agent field below is `tester`"));
    }

    #[test]
    fn parse_files_touched_extracts_absolute_paths() {
        let recap = "# Recap\n\n## Files touched\n\n- /tmp/a.txt\n- /tmp/b.txt\n* `/tmp/c.txt`\n\nrequires_user: false\n";
        let files = parse_files_touched(recap);
        assert_eq!(
            files,
            vec![
                PathBuf::from("/tmp/a.txt"),
                PathBuf::from("/tmp/b.txt"),
                PathBuf::from("/tmp/c.txt"),
            ]
        );
    }

    #[test]
    fn parse_files_touched_handles_none_marker_and_relative_paths() {
        let recap = "# Recap\n\n### Files touched\n\n(none)\n- relative/path.txt\n\n## Next steps\n\nNothing.\n";
        let files = parse_files_touched(recap);
        assert!(files.is_empty());
    }

    #[test]
    fn parse_files_touched_stops_at_next_heading() {
        let recap = "## Files touched\n\n/tmp/x\n\n## Other\n\n/tmp/should-not-be-included\n";
        let files = parse_files_touched(recap);
        assert_eq!(files, vec![PathBuf::from("/tmp/x")]);
    }

    #[test]
    fn interactive_instructions_omit_recap_requirements() {
        let instructions = build_interactive_instructions();

        assert!(instructions.contains("interactive task session"));
        assert!(!instructions.contains("at most"));
        assert!(!instructions.contains("minutes."));
        assert!(!instructions.contains("Files touched"));
        assert!(instructions.contains("Do NOT produce a structured Varda recap"));
        assert!(instructions.contains("interpreter pass"));
    }

    #[test]
    fn interpretation_instructions_request_recap_from_log() {
        let instructions = build_interpretation_instructions();

        assert!(instructions.contains("interpreting a completed interactive Varda session"));
        assert!(instructions.contains("Do NOT perform any new work"));
        assert!(instructions.contains("Files touched"));
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
            resume_command: None,
        });

        let result = client
            .run_task(AgentRunRequest {
                agent_name: "codex".to_owned(),
                role_instructions: None,
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
                    agent_resume_commands: vec![],
                    requires_user: false,
                },
                body: "# Task".to_owned(),
                timeout: Duration::from_secs(600),
                session_id: "session-1".to_owned(),
                session_log_path: None,
                interactive: false,
                interpret: false,
            })
            .await
            .expect("fake agent should return a result");

        assert_eq!(result.recap, "done");
        assert!(!result.requires_user);
        assert_eq!(result.suggested_agent.as_deref(), Some("codex"));
    }
}
