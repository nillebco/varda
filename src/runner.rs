//! End-to-end task execution flow.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use tokio::time;
use uuid::Uuid;

use crate::agent::{
    AgentClient, AgentRunRequest, AgentRunResult, parse_files_touched,
    recap_requires_user_interaction,
};
use crate::config::Config;
use crate::task::{TaskDocument, TaskStatus, load_task, write_task};

/// Maximum bytes of session log content embedded in the interpretation prompt body.
/// Larger logs are truncated, keeping the tail (most recent activity).
const INTERPRETATION_LOG_BUDGET: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunOutcome {
    pub status: TaskStatus,
    pub recap_path: PathBuf,
    pub session_log_path: PathBuf,
    /// Absolute paths the agent reported under the `Files touched` heading of
    /// its recap. Varda commits these in the project repo; the agent does not.
    pub files_touched: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanOutcome {
    pub plan_path: PathBuf,
}

pub async fn run_task(
    config: &Config,
    agent_name: &str,
    role_instructions: Option<&str>,
    task_path: &Path,
    client: &impl AgentClient,
    interactive: bool,
    stream: bool,
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
    task.frontmatter.agent_session_ids.push(session_id.clone());
    task.frontmatter
        .agent_session_logs
        .push(session_log_path.display().to_string());
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
        role_instructions: role_instructions.map(str::to_owned),
        task_path: task_path.display().to_string(),
        frontmatter: task.frontmatter.clone(),
        body: task.body.clone(),
        timeout,
        session_id: session_id.clone(),
        session_log_path: Some(session_log_path.display().to_string()),
        interactive,
        interpret: false,
        stream,
        resume_command: None,
    };

    let agent_result = if interactive {
        Ok(client.run_task(request).await)
    } else {
        time::timeout(timeout, client.run_task(request)).await
    };

    let _interactive_finalization_guard = if interactive {
        Some(InteractiveFinalizationGuard::activate()?)
    } else {
        None
    };

    let session_outcome = match agent_result {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(error)) => {
            append_session_log(&session_log_path, &format!("\nerror:\n{error:#}\n"))?;
            Err(AgentRunResult {
                recap: format!(
                    "# Agent Run Failed\n\nThe agent failed while processing `{}`.\n\nError: {error}\n\nSession ID: `{session_id}`\n\nSession log: [{}]({})",
                    task_path.display(),
                    session_log_path.display(),
                    session_log_path.display()
                ),
                requires_user: false,
                suggested_agent: None,
                resume_command: None,
            })
        }
        Err(_) => {
            append_session_log(
                &session_log_path,
                &format!(
                    "\ntimeout:\nexceeded {} second limit\nlong_running_task_requested=true\n",
                    config.defaults.timeout_seconds
                ),
            )?;
            Err(AgentRunResult {
                recap: format!(
                    "# Agent Run Timed Out\n\nThe agent exceeded the configured {} second limit while processing `{}`.\n\nWhat completed: the session log was preserved for inspection.\n\nWhat remains: delegate the unfinished work to a Varda long-running runner task, then resume the agent after the complete runner output is available.\n\nBlockers: the single-session time limit was reached.\n\nUser interaction required: no.\n\nSuggested next agent: runner.\n\nSession ID: `{session_id}`\n\nSession log: [{}]({})",
                    config.defaults.timeout_seconds,
                    task_path.display(),
                    session_log_path.display(),
                    session_log_path.display()
                ),
                requires_user: false,
                suggested_agent: Some("runner".to_owned()),
                resume_command: None,
            })
        }
    };

    let result = match session_outcome {
        Err(failure) => failure,
        Ok(session_result) if interactive => {
            // The interactive session carries the resume command; persist it on the task
            // before running the interpreter pass so it survives even if the interpreter
            // pass fails or returns its own (None) resume_command.
            if let Some(resume) = session_result.resume_command.as_deref() {
                task.frontmatter
                    .agent_resume_commands
                    .push(resume.to_owned());
                write_task(&task)?;
                eprintln!("captured resume command: {resume}");
            }
            match interpret_interactive_session(
                config,
                client,
                agent_name,
                role_instructions,
                task_path,
                &task,
                &session_id,
                &session_log_path,
                timeout,
            )
            .await
            {
                Ok(interpreted) => interpreted,
                Err(error) => {
                    append_session_log(
                        &session_log_path,
                        &format!("\ninterpretation_error:\n{error:#}\n"),
                    )?;
                    AgentRunResult {
                        recap: format!(
                            "# Interactive Session Completed\n\nThe interactive session ended successfully but Varda's interpreter pass failed: {error}\n\nFalling back to the agent's session-end output.\n\n{}\n\nSession log: [{}]({})\n\nrequires_user: false",
                            session_result.recap,
                            session_log_path.display(),
                            session_log_path.display()
                        ),
                        requires_user: false,
                        suggested_agent: None,
                        resume_command: None,
                    }
                }
            }
        }
        Ok(session_result) => session_result,
    };

    let requires_user = result.requires_user || recap_requires_user_interaction(&result.recap);
    let files_touched = parse_files_touched(&result.recap);
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
        files_touched,
    })
}

pub async fn resume_interactive_task(
    config: &Config,
    agent_name: &str,
    role_instructions: Option<&str>,
    task_path: &Path,
    client: &impl AgentClient,
    resume_command: String,
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
    task.frontmatter.agent_session_ids.push(session_id.clone());
    task.frontmatter
        .agent_session_logs
        .push(session_log_path.display().to_string());
    write_task(&task)?;

    write_session_log(
        &session_log_path,
        &format!(
            "session_id={session_id}\nagent={agent_name}\ntask={}\nresume_command={resume_command}\n[interactive_resume]\n",
            task_path.display()
        ),
    )?;

    let request = AgentRunRequest {
        agent_name: agent_name.to_owned(),
        role_instructions: role_instructions.map(str::to_owned),
        task_path: task_path.display().to_string(),
        frontmatter: task.frontmatter.clone(),
        body: String::new(),
        timeout,
        session_id: session_id.clone(),
        session_log_path: Some(session_log_path.display().to_string()),
        interactive: true,
        interpret: false,
        stream: false,
        resume_command: Some(resume_command),
    };

    let session_result = client.run_task(request).await.with_context(|| {
        format!(
            "failed to resume interactive agent session for {}",
            task_path.display()
        )
    })?;

    let _interactive_finalization_guard = InteractiveFinalizationGuard::activate()?;

    if let Some(resume) = session_result.resume_command.as_deref() {
        task.frontmatter
            .agent_resume_commands
            .push(resume.to_owned());
        write_task(&task)?;
        eprintln!("captured resume command: {resume}");
    }

    let result = match interpret_interactive_session(
        config,
        client,
        agent_name,
        role_instructions,
        task_path,
        &task,
        &session_id,
        &session_log_path,
        timeout,
    )
    .await
    {
        Ok(interpreted) => interpreted,
        Err(error) => {
            append_session_log(
                &session_log_path,
                &format!("\ninterpretation_error:\n{error:#}\n"),
            )?;
            AgentRunResult {
                recap: format!(
                    "# Interactive Session Completed\n\nThe resumed interactive session ended successfully but Varda's interpreter pass failed: {error}\n\nFalling back to the agent's session-end output.\n\n{}\n\nSession log: [{}]({})\n\nrequires_user: false",
                    session_result.recap,
                    session_log_path.display(),
                    session_log_path.display()
                ),
                requires_user: false,
                suggested_agent: None,
                resume_command: None,
            }
        }
    };

    let requires_user = result.requires_user || recap_requires_user_interaction(&result.recap);
    let files_touched = parse_files_touched(&result.recap);
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
        files_touched,
    })
}

pub async fn plan_task(
    config: &Config,
    agent_name: &str,
    role_instructions: Option<&str>,
    task_path: &Path,
    client: &(impl AgentClient + Sync),
) -> Result<PlanOutcome> {
    let mut task = load_task(task_path)?;

    let timeout = Duration::from_secs(config.defaults.timeout_seconds);
    let request = AgentRunRequest {
        agent_name: agent_name.to_owned(),
        role_instructions: role_instructions.map(str::to_owned),
        task_path: task_path.display().to_string(),
        frontmatter: task.frontmatter.clone(),
        body: task.body.clone(),
        timeout,
        session_id: Uuid::new_v4().to_string(),
        session_log_path: None,
        interactive: false,
        interpret: false,
        stream: false,
        resume_command: None,
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
            resume_command: None,
        },
        Err(_) => AgentRunResult {
            recap: format!(
                "# Planning Timed Out\n\nThe agent exceeded the configured {} second limit while planning `{}`.",
                config.defaults.timeout_seconds,
                task_path.display()
            ),
            requires_user: false,
            suggested_agent: None,
            resume_command: None,
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

#[allow(clippy::too_many_arguments)]
async fn interpret_interactive_session(
    _config: &Config,
    client: &impl AgentClient,
    agent_name: &str,
    role_instructions: Option<&str>,
    task_path: &Path,
    task: &TaskDocument,
    parent_session_id: &str,
    session_log_path: &Path,
    timeout: Duration,
) -> Result<AgentRunResult> {
    let log_excerpt = read_session_log_excerpt(session_log_path)?;
    let body = format!(
        "An interactive Varda session for this task just finished. Read the session log content below \
        (and any referenced external transcripts at the listed paths if your tools allow you to open them) \
        and produce the Varda recap. Do not perform any new work.\n\n\
        ## Original task body\n\n{task_body}\n\n\
        ## Session log\n\nPath: {log_path}\n\n```\n{log_excerpt}\n```\n",
        task_body = task.body,
        log_path = session_log_path.display(),
        log_excerpt = log_excerpt,
    );

    let request = AgentRunRequest {
        agent_name: agent_name.to_owned(),
        role_instructions: role_instructions.map(str::to_owned),
        task_path: task_path.display().to_string(),
        frontmatter: task.frontmatter.clone(),
        body,
        timeout,
        session_id: format!("{parent_session_id}-interpret"),
        session_log_path: Some(session_log_path.display().to_string()),
        interactive: false,
        interpret: true,
        stream: false,
        resume_command: None,
    };

    eprintln!();
    eprintln!(
        "Interactive session ended. Varda is now running the {agent_name} interpreter pass to produce the recap and Files touched list."
    );
    eprintln!(
        "This non-interactive agent run may take up to {} seconds — please wait, do not kill the process.",
        timeout.as_secs()
    );

    append_session_log(session_log_path, "\ninterpretation_pass: starting\n")?;

    let result = time::timeout(timeout, client.run_task(request))
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "interpretation pass exceeded {} second limit",
                timeout.as_secs()
            )
        })?
        .context("interpretation pass failed")?;

    append_session_log(session_log_path, "\ninterpretation_pass: completed\n")?;
    eprintln!("Interpreter pass completed; writing recap.");

    Ok(result)
}

#[cfg(unix)]
struct InteractiveFinalizationGuard {
    previous_sigint_action: libc::sigaction,
}

#[cfg(unix)]
impl InteractiveFinalizationGuard {
    fn activate() -> Result<Self> {
        let mut ignore_action: libc::sigaction = unsafe { std::mem::zeroed() };
        ignore_action.sa_sigaction = libc::SIG_IGN;
        unsafe {
            libc::sigemptyset(&mut ignore_action.sa_mask);
        }
        ignore_action.sa_flags = 0;

        let mut previous_sigint_action: libc::sigaction = unsafe { std::mem::zeroed() };
        let result =
            unsafe { libc::sigaction(libc::SIGINT, &ignore_action, &mut previous_sigint_action) };
        if result == -1 {
            return Err(std::io::Error::last_os_error())
                .context("failed to protect interactive finalization from Ctrl-C");
        }

        eprintln!(
            "Ctrl-C is temporarily disabled while Varda stores the interactive session result."
        );

        Ok(Self {
            previous_sigint_action,
        })
    }
}

#[cfg(unix)]
impl Drop for InteractiveFinalizationGuard {
    fn drop(&mut self) {
        let result = unsafe {
            libc::sigaction(
                libc::SIGINT,
                &self.previous_sigint_action,
                std::ptr::null_mut(),
            )
        };
        if result == -1 {
            eprintln!(
                "warning: failed to restore Ctrl-C handling after interactive finalization: {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

#[cfg(not(unix))]
struct InteractiveFinalizationGuard;

#[cfg(not(unix))]
impl InteractiveFinalizationGuard {
    fn activate() -> Result<Self> {
        eprintln!(
            "Please wait while Varda stores the interactive session result before closing this process."
        );
        Ok(Self)
    }
}

fn read_session_log_excerpt(path: &Path) -> Result<String> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read session log at {}", path.display()))?;
    if content.len() <= INTERPRETATION_LOG_BUDGET {
        return Ok(content);
    }
    let truncated_at = content.len() - INTERPRETATION_LOG_BUDGET;
    let mut start = truncated_at;
    while start < content.len() && !content.is_char_boundary(start) {
        start += 1;
    }
    Ok(format!(
        "[truncated: showing last {} bytes of {} byte log]\n\n{}",
        content.len() - start,
        content.len(),
        &content[start..]
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use async_trait::async_trait;

    use crate::agent::fake::FakeAgentClient;
    use crate::agent::{AgentRunRequest, AgentRunResult};
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
            resume_command: None,
        });

        let outcome = run_task(&config, "codex", None, &task_path, &client, false, false)
            .await
            .expect("task should run");

        let updated = fs::read_to_string(&task_path).expect("task should be readable");
        let recap = fs::read_to_string(&outcome.recap_path).expect("recap should be readable");

        assert_eq!(outcome.status, TaskStatus::Pending);
        assert!(updated.contains("status: pending"));
        assert!(updated.contains("recaps:"));
        assert!(updated.contains("agent_session_ids:"));
        assert!(updated.contains("agent_session_logs:"));
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
            resume_command: None,
        });

        let outcome = run_task(&config, "codex", None, &task_path, &client, false, false)
            .await
            .expect("task should run");

        let updated = fs::read_to_string(&task_path).expect("task should be readable");

        assert_eq!(outcome.status, TaskStatus::NeedsUser);
        assert!(updated.contains("status: needs_user"));
        assert!(updated.contains("requires_user: true"));
    }

    #[tokio::test]
    async fn run_task_timeout_requests_long_running_runner_task() {
        let root = std::env::temp_dir().join(format!(
            "varda-run-timeout-long-running-{}",
            std::process::id()
        ));
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

        let mut config = test_config(operations_dir.display().to_string());
        config.defaults.timeout_seconds = 0;
        let client = PendingAgentClient;

        let outcome = run_task(&config, "codex", None, &task_path, &client, false, false)
            .await
            .expect("task should time out cleanly");

        let recap = fs::read_to_string(&outcome.recap_path).expect("recap should be readable");
        let log = fs::read_to_string(&outcome.session_log_path).expect("log should be readable");

        assert_eq!(outcome.status, TaskStatus::Failed);
        assert!(recap.contains("Agent Run Timed Out"));
        assert!(recap.contains("long-running runner task"));
        assert!(recap.contains("Suggested next agent: runner"));
        assert!(log.contains("long_running_task_requested=true"));
    }

    struct PendingAgentClient;

    #[async_trait]
    impl AgentClient for PendingAgentClient {
        async fn run_task(&self, _request: AgentRunRequest) -> Result<AgentRunResult> {
            std::future::pending::<Result<AgentRunResult>>().await
        }
    }

    #[derive(Clone)]
    struct RecordingAgentClient {
        requests: std::sync::Arc<std::sync::Mutex<Vec<AgentRunRequest>>>,
        session_response: AgentRunResult,
        interpretation_response: AgentRunResult,
    }

    #[async_trait]
    impl AgentClient for RecordingAgentClient {
        async fn run_task(&self, request: AgentRunRequest) -> Result<AgentRunResult> {
            let interpret = request.interpret;
            self.requests.lock().unwrap().push(request);
            Ok(if interpret {
                self.interpretation_response.clone()
            } else {
                self.session_response.clone()
            })
        }
    }

    #[tokio::test]
    async fn interactive_run_invokes_interpreter_pass() {
        let root = std::env::temp_dir().join(format!(
            "varda-run-interactive-interpret-{}",
            std::process::id()
        ));
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

Help interactively.
"#,
        )
        .expect("task should be written");

        let mut config = test_config(operations_dir.display().to_string());
        config.git.auto_commit = false;
        let client = RecordingAgentClient {
            requests: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            session_response: AgentRunResult {
                recap: "Interactive session completed.".to_owned(),
                requires_user: false,
                suggested_agent: None,
                resume_command: None,
            },
            interpretation_response: AgentRunResult {
                recap: "# Interpreted Recap\n\nDid the work.\n\nrequires_user: false".to_owned(),
                requires_user: false,
                suggested_agent: None,
                resume_command: None,
            },
        };

        let outcome = run_task(&config, "codex", None, &task_path, &client, true, false)
            .await
            .expect("interactive task should run and be interpreted");

        let recap = fs::read_to_string(&outcome.recap_path).expect("recap should be readable");
        let recorded = client.requests.lock().unwrap().clone();

        assert_eq!(
            recorded.len(),
            2,
            "expected one session call and one interpretation call"
        );
        assert!(
            recorded[0].interactive,
            "first call should be the interactive session"
        );
        assert!(!recorded[0].interpret);
        assert!(
            !recorded[1].interactive,
            "second call should not be interactive"
        );
        assert!(
            recorded[1].interpret,
            "second call should be the interpretation pass"
        );
        assert!(recorded[1].body.contains("Session log"));
        assert!(recorded[1].body.contains("interactive Varda session"));
        assert_eq!(outcome.status, TaskStatus::Pending);
        assert!(recap.contains("Interpreted Recap"));
        assert!(recap.contains("Did the work."));
    }

    #[tokio::test]
    async fn interactive_run_persists_resume_command_in_frontmatter() {
        let root = std::env::temp_dir().join(format!(
            "varda-run-interactive-resume-{}",
            std::process::id()
        ));
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

Help interactively.
"#,
        )
        .expect("task should be written");

        let mut config = test_config(operations_dir.display().to_string());
        config.git.auto_commit = false;
        let client = RecordingAgentClient {
            requests: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            session_response: AgentRunResult {
                recap: "Interactive session completed.".to_owned(),
                requires_user: false,
                suggested_agent: None,
                resume_command: Some("codex resume abc-123".to_owned()),
            },
            interpretation_response: AgentRunResult {
                recap: "# Interpreted Recap\n\nDid the work.\n\nrequires_user: false".to_owned(),
                requires_user: false,
                suggested_agent: None,
                resume_command: None,
            },
        };

        run_task(&config, "codex", None, &task_path, &client, true, false)
            .await
            .expect("interactive task should run");

        let task = crate::task::load_task(&task_path).expect("task should load");
        assert_eq!(
            task.frontmatter.agent_resume_commands,
            vec!["codex resume abc-123".to_owned()],
            "interactive resume command should be persisted in the task frontmatter"
        );

        let raw = fs::read_to_string(&task_path).expect("task file should be readable");
        assert!(raw.contains("agent_resume_commands:"));
        assert!(raw.contains("codex resume abc-123"));
    }

    #[tokio::test]
    async fn resume_interactive_task_uses_captured_command_then_interprets() {
        let root =
            std::env::temp_dir().join(format!("varda-run-captured-resume-{}", std::process::id()));
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
agent_resume_commands:
- codex resume abc-123
---

# Task

Help interactively.
"#,
        )
        .expect("task should be written");

        let mut config = test_config(operations_dir.display().to_string());
        config.git.auto_commit = false;
        let client = RecordingAgentClient {
            requests: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            session_response: AgentRunResult {
                recap: "Interactive resume session completed.".to_owned(),
                requires_user: false,
                suggested_agent: None,
                resume_command: None,
            },
            interpretation_response: AgentRunResult {
                recap: "# Interpreted Recap\n\nResumed the work.\n\nrequires_user: false"
                    .to_owned(),
                requires_user: false,
                suggested_agent: None,
                resume_command: None,
            },
        };

        let outcome = resume_interactive_task(
            &config,
            "codex",
            None,
            &task_path,
            &client,
            "codex resume abc-123".to_owned(),
        )
        .await
        .expect("captured resume command should run and be interpreted");

        let recorded = client.requests.lock().unwrap().clone();
        assert_eq!(recorded.len(), 2);
        assert!(recorded[0].interactive);
        assert!(!recorded[0].interpret);
        assert_eq!(
            recorded[0].resume_command.as_deref(),
            Some("codex resume abc-123")
        );
        assert!(recorded[0].body.is_empty());
        assert!(!recorded[1].interactive);
        assert!(recorded[1].interpret);
        assert!(recorded[1].body.contains("Session log"));
        assert_eq!(outcome.status, TaskStatus::Pending);
    }

    fn test_config(operations_dir: String) -> Config {
        Config {
            defaults: Defaults {
                timeout_seconds: 600,
                operations_dir,
                sandbox: None,
            },
            routes: vec![Route {
                glob: "**".to_owned(),
                agents: vec!["codex".to_owned()],
                sandbox: None,
            }],
            agents: BTreeMap::from([(
                "codex".to_owned(),
                AgentConfig {
                    kind: AgentKind::Acp,
                    command: "codex".to_owned(),
                    args: vec![],
                    max_prompt_tokens: None,
                    working_dir: None,
                    env: BTreeMap::new(),
                    interactive_command: None,
                    interactive_args: None,
                    resume_command_template: None,
                },
            )]),
            roles: std::collections::BTreeMap::new(),
            git: GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
        }
    }
}
