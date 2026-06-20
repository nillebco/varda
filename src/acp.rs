//! ACP transport support.

use std::collections::BTreeMap;
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time;

use crate::agent::{
    AgentClient, AgentRunRequest, AgentRunResult, build_agent_instructions,
    build_interactive_instructions, build_interpretation_instructions, build_planning_instructions,
    recap_requires_user_interaction,
};
use crate::config::AgentConfig;

#[derive(Debug, Clone)]
pub struct AcpSubprocessClient {
    agent_name: String,
    command: String,
    args: Vec<String>,
    working_dir: Option<String>,
    env: BTreeMap<String, String>,
    interactive_command: Option<String>,
    interactive_args: Option<Vec<String>>,
    resume_command_template: Option<String>,
}

impl AcpSubprocessClient {
    pub fn new(agent_name: impl Into<String>, config: &AgentConfig) -> Self {
        Self {
            agent_name: agent_name.into(),
            command: config.command.clone(),
            args: config.args.clone(),
            working_dir: config.working_dir.clone(),
            env: config.env.clone(),
            interactive_command: config.interactive_command.clone(),
            interactive_args: config.interactive_args.clone(),
            resume_command_template: config.resume_command_template.clone(),
        }
    }
}

#[async_trait]
impl AgentClient for AcpSubprocessClient {
    async fn run_task(&self, request: AgentRunRequest) -> Result<AgentRunResult> {
        let prompt = build_prompt(&request);
        let args = args_for_request(&self.args, &request);
        self.execute(prompt, args, &request).await
    }

    async fn plan_task(&self, request: AgentRunRequest) -> Result<AgentRunResult> {
        let prompt = build_planning_prompt(&request);
        let args = args_for_request(&self.args, &request);
        self.execute(prompt, args, &request).await
    }
}

impl AcpSubprocessClient {
    async fn execute(
        &self,
        prompt: String,
        args: Vec<String>,
        request: &AgentRunRequest,
    ) -> Result<AgentRunResult> {
        if request.interactive {
            return self.execute_interactive(prompt, args, request).await;
        }
        let started_at = SystemTime::now();
        let command = expand_request_value(&self.command, request);
        let working_dir = self
            .working_dir
            .as_deref()
            .map(|dir| expand_request_value(dir, request));
        let env = env_for_request(&self.env, request);
        if let Some(log_path) = request.session_log_path.as_deref() {
            let _ = append_session_log(
                log_path,
                &format!(
                    "session_id={}\nagent={}\ntask={}\ncommand={} args={:?}\nworking_dir={:?}\n",
                    request.session_id,
                    self.agent_name,
                    request.task_path,
                    command,
                    args,
                    working_dir
                ),
            );
        }

        let mut command_builder = Command::new(&command);
        command_builder
            .args(&args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        protect_interpreter_from_terminal_sigint(&mut command_builder, request);
        if let Some(working_dir) = working_dir.as_deref() {
            command_builder.current_dir(working_dir);
        }

        let mut child = command_builder.spawn().with_context(|| {
            format!(
                "failed to start agent '{}' with command '{}'",
                self.agent_name, command
            )
        })?;

        let mut stdin = child.stdin.take().context("failed to open agent stdin")?;
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("failed to write task prompt to agent stdin")?;
        drop(stdin);

        self.record_external_session(request, started_at);

        let stdout = child.stdout.take().context("failed to open agent stdout")?;
        let stderr = child.stderr.take().context("failed to open agent stderr")?;
        let log_path = request.session_log_path.clone();
        let stdout_log_path = log_path.clone();
        let stderr_log_path = log_path.clone();

        use std::io::IsTerminal as _;
        let stream_to_terminal = request.stream && std::io::stdout().is_terminal();
        let stdout_task = async move {
            if stream_to_terminal {
                collect_stream_tee(stdout, stdout_log_path, "stdout").await
            } else {
                collect_stream(stdout, stdout_log_path, "stdout").await
            }
        };
        let stderr_task = collect_stream(stderr, stderr_log_path, "stderr");
        let wait_task = async {
            child
                .wait()
                .await
                .context("failed to wait for agent subprocess")
        };
        let (stdout, stderr, status) = tokio::try_join!(stdout_task, stderr_task, wait_task)
            .context("failed while waiting for agent subprocess")?;

        if let Some(log_path) = request.session_log_path.as_deref() {
            let _ = append_session_log(log_path, &format!("\nstatus={status}\n"));
        }

        if !status.success() {
            let stderr = String::from_utf8_lossy(&stderr);
            bail!(
                "agent '{}' exited with status {}; stderr: {}",
                self.agent_name,
                status,
                stderr.trim()
            );
        }

        let recap = String::from_utf8(stdout)
            .context("agent stdout was not valid UTF-8")?
            .trim()
            .to_owned();

        if recap.is_empty() {
            bail!("agent '{}' produced an empty recap", self.agent_name);
        }

        Ok(AgentRunResult {
            requires_user: recap_requires_user_interaction(&recap),
            suggested_agent: None,
            recap,
            resume_command: None,
        })
    }

    async fn execute_interactive(
        &self,
        prompt: String,
        args: Vec<String>,
        request: &AgentRunRequest,
    ) -> Result<AgentRunResult> {
        let command = expand_request_value(&self.command, request);
        let working_dir = self
            .working_dir
            .as_deref()
            .map(|dir| expand_request_value(dir, request));
        let mut env = env_for_request(&self.env, request);

        if let Some(log_path) = request.session_log_path.as_deref() {
            let _ = append_session_log(
                log_path,
                &format!(
                    "session_id={}\nagent={}\ntask={}\ncommand={} args={:?}\nworking_dir={:?}\n[interactive]\n",
                    request.session_id,
                    self.agent_name,
                    request.task_path,
                    command,
                    args,
                    working_dir
                ),
            );
        }

        if let Some(resume_command) = request.resume_command.as_deref() {
            let interactive_cmd = self
                .interactive_command
                .as_deref()
                .map(|cmd| expand_request_value(cmd, request))
                .unwrap_or_else(|| "sh".to_owned());
            let interactive_args = resume_args_for_command(&interactive_cmd, resume_command);

            if let Some(log_path) = request.session_log_path.as_deref() {
                let _ = append_session_log(
                    log_path,
                    &format!(
                        "resume_command={resume_command}\nresume_invocation={} {:?}\n",
                        interactive_cmd, interactive_args
                    ),
                );
            }

            let mut command_builder = Command::new(&interactive_cmd);
            command_builder
                .args(&interactive_args)
                .envs(env)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            if let Some(working_dir) = working_dir.as_deref() {
                command_builder.current_dir(working_dir);
            }

            set_terminal_title_for_agent(&self.agent_name);

            let mut child = command_builder.spawn().with_context(|| {
                format!(
                    "failed to resume interactive agent '{}' with command '{}'",
                    self.agent_name, interactive_cmd
                )
            })?;

            let status = child
                .wait()
                .await
                .context("failed to wait for resumed interactive agent subprocess")?;

            if let Some(log_path) = request.session_log_path.as_deref() {
                let _ = append_session_log(log_path, &format!("\nstatus={status}\n"));
            }

            if !status.success() {
                bail!("agent '{}' exited with status {}", self.agent_name, status,);
            }

            return Ok(AgentRunResult {
                recap: "Interactive resume session completed.\n\nrequires_user: false".to_owned(),
                requires_user: false,
                suggested_agent: None,
                resume_command: None,
            });
        }

        // Write the task prompt to a temp file so truly-interactive agents can read it.
        let prompt_file =
            std::env::temp_dir().join(format!("varda-prompt-{}.txt", request.session_id));
        std::fs::write(&prompt_file, prompt.as_bytes())
            .context("failed to write prompt to temp file")?;
        env.insert(
            "VARDA_PROMPT_FILE".to_owned(),
            prompt_file.display().to_string(),
        );

        if let Some(interactive_cmd) = &self.interactive_command {
            // Truly interactive: inherit all terminal streams so the user can interact directly.
            let interactive_cmd = expand_request_value(interactive_cmd, request);
            let interactive_args =
                args_for_request(self.interactive_args.as_deref().unwrap_or(&[]), request);

            let started_at = SystemTime::now();
            let mut command_builder = Command::new(&interactive_cmd);
            command_builder
                .args(&interactive_args)
                .envs(env)
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
            if let Some(working_dir) = working_dir.as_deref() {
                command_builder.current_dir(working_dir);
            }

            set_terminal_title_for_agent(&self.agent_name);

            let mut child = command_builder.spawn().with_context(|| {
                format!(
                    "failed to start interactive agent '{}' with command '{}'",
                    self.agent_name, interactive_cmd
                )
            })?;

            let record_handle = self.record_external_session(request, started_at);

            let status = child
                .wait()
                .await
                .context("failed to wait for interactive agent subprocess")?;
            let _ = std::fs::remove_file(&prompt_file);

            if let Some(log_path) = request.session_log_path.as_deref() {
                let _ = append_session_log(log_path, &format!("\nstatus={status}\n"));
            }

            if !status.success() {
                bail!("agent '{}' exited with status {}", self.agent_name, status,);
            }

            let external_session_id = match record_handle {
                Some(handle) => handle.await.ok().flatten(),
                None => None,
            };
            let resume_command = self.build_resume_command(request, external_session_id.as_deref());

            return Ok(AgentRunResult {
                recap: "Interactive session completed.\n\nrequires_user: false".to_owned(),
                requires_user: false,
                suggested_agent: None,
                resume_command,
            });
        }

        // Fallback: pipe-based interactive mode (prompt written to stdin, terminal forwarded).
        let started_at = SystemTime::now();
        let prompt = std::fs::read_to_string(&prompt_file)
            .context("failed to read prompt from temp file")?;
        let _ = std::fs::remove_file(&prompt_file);

        let mut command_builder = Command::new(&command);
        command_builder
            .args(&args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if let Some(working_dir) = working_dir.as_deref() {
            command_builder.current_dir(working_dir);
        }

        set_terminal_title_for_agent(&self.agent_name);

        let mut child = command_builder.spawn().with_context(|| {
            format!(
                "failed to start agent '{}' with command '{}'",
                self.agent_name, command
            )
        })?;

        let mut stdin = child.stdin.take().context("failed to open agent stdin")?;
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("failed to write task prompt to agent stdin")?;
        // Forward terminal stdin to the agent so the user can interact.
        tokio::spawn(async move {
            let mut terminal_stdin = tokio::io::stdin();
            let _ = tokio::io::copy(&mut terminal_stdin, &mut stdin).await;
        });

        let record_handle = self.record_external_session(request, started_at);

        let stdout = child.stdout.take().context("failed to open agent stdout")?;
        let log_path = request.session_log_path.clone();
        let tee_task = collect_stream_tee(stdout, log_path, "stdout");

        let (stdout_bytes, status) = tokio::try_join!(tee_task, async {
            child
                .wait()
                .await
                .context("failed to wait for agent subprocess")
        })
        .context("failed while waiting for agent subprocess")?;

        if let Some(log_path) = request.session_log_path.as_deref() {
            let _ = append_session_log(log_path, &format!("\nstatus={status}\n"));
        }

        if !status.success() {
            bail!("agent '{}' exited with status {}", self.agent_name, status,);
        }

        // Interactive sessions don't produce a structured Varda recap on stdout;
        // the runner runs a separate interpretation pass over the session log.
        // We still consume stdout above so it gets logged via the tee.
        drop(stdout_bytes);

        let external_session_id = match record_handle {
            Some(handle) => handle.await.ok().flatten(),
            None => None,
        };
        let resume_command = self.build_resume_command(request, external_session_id.as_deref());

        Ok(AgentRunResult {
            recap: "Interactive session completed.".to_owned(),
            requires_user: false,
            suggested_agent: None,
            resume_command,
        })
    }

    fn uses_copilot(&self) -> bool {
        self.command == "copilot"
            || self
                .args
                .iter()
                .any(|a| a == "copilot" || a.starts_with("copilot "))
    }

    /// Spawns the per-agent external-session discovery task and returns a handle whose
    /// output is the agent's own session id (when found). The spawned task also writes
    /// `external_session_id=...` to the Varda session log as a side effect.
    fn record_external_session(
        &self,
        request: &AgentRunRequest,
        started_at: SystemTime,
    ) -> Option<tokio::task::JoinHandle<Option<String>>> {
        let log_path = request.session_log_path.as_deref()?.to_owned();

        if self.command == "claude" {
            let project = request.frontmatter.project.as_deref()?.to_owned();
            let varda_session_id = request.session_id.clone();
            Some(tokio::spawn(async move {
                record_claude_external_session(log_path, project, varda_session_id, started_at)
                    .await
            }))
        } else if self.uses_copilot() {
            Some(tokio::spawn(async move {
                record_copilot_external_session(log_path, started_at).await
            }))
        } else if self.command == "codex" {
            let project = request.frontmatter.project.clone();
            Some(tokio::spawn(async move {
                record_codex_external_session(log_path, started_at, project).await
            }))
        } else {
            None
        }
    }

    /// Build a resume command from the configured template by substituting the
    /// agent's external session id and the task's project path. Returns None when
    /// no template is configured or no session id was discovered.
    fn build_resume_command(
        &self,
        request: &AgentRunRequest,
        external_session_id: Option<&str>,
    ) -> Option<String> {
        let template = self.resume_command_template.as_deref()?;
        let session_id = external_session_id?;
        let project = request.frontmatter.project.as_deref().unwrap_or("");
        Some(
            template
                .replace("{external_session_id}", session_id)
                .replace("{project}", project),
        )
    }
}

async fn record_claude_external_session(
    log_path: String,
    project: String,
    varda_session_id: String,
    started_at: SystemTime,
) -> Option<String> {
    for _ in 0..20 {
        if let Some(transcript) = find_claude_transcript(&project, &varda_session_id, started_at) {
            let session_id = transcript
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("unknown")
                .to_owned();
            let _ = append_session_log(
                &log_path,
                &format!(
                    "external_session_id={session_id}\nexternal_session_log={}\n",
                    transcript.display()
                ),
            );
            return Some(session_id);
        }
        time::sleep(Duration::from_millis(250)).await;
    }
    None
}

fn find_copilot_process_log(started_at: SystemTime) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let logs_dir = Path::new(&home).join(".copilot/logs");
    find_copilot_process_log_in(&logs_dir, started_at)
}

fn find_copilot_process_log_in(logs_dir: &Path, started_at: SystemTime) -> Option<PathBuf> {
    let mut candidates: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(logs_dir)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let path = e.path();
            let name = path.file_name()?.to_str()?.to_owned();
            if !name.starts_with("process-") || !name.ends_with(".log") {
                return None;
            }
            let mtime = e.metadata().ok()?.modified().ok()?;
            if mtime < started_at {
                return None;
            }
            Some((mtime, path))
        })
        .collect();
    // Pick the file whose mtime is closest to (and at or after) started_at.
    candidates.sort_by_key(|(mtime, _)| *mtime);
    candidates.into_iter().map(|(_, p)| p).next()
}

fn extract_copilot_workspace_id(log_path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(log_path).ok()?;
    for line in content.lines() {
        if let Some(rest) = line.split("Workspace initialized: ").nth(1) {
            let id = rest.split_whitespace().next()?;
            return Some(id.to_owned());
        }
    }
    None
}

async fn record_copilot_external_session(
    log_path: String,
    started_at: SystemTime,
) -> Option<String> {
    for _ in 0..20 {
        if let Some(process_log) = find_copilot_process_log(started_at) {
            if let Some(workspace_id) = extract_copilot_workspace_id(&process_log) {
                let home = std::env::var_os("HOME")?;
                let events_path = Path::new(&home)
                    .join(".copilot/session-state")
                    .join(&workspace_id)
                    .join("events.jsonl");
                let _ = append_session_log(
                    &log_path,
                    &format!(
                        "external_session_id={workspace_id}\nexternal_session_log={}\n",
                        events_path.display()
                    ),
                );
                return Some(workspace_id);
            }
        }
        time::sleep(Duration::from_millis(500)).await;
    }
    None
}

fn find_codex_session(started_at: SystemTime, project: Option<&str>) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let sessions_base = Path::new(&home).join(".codex/sessions");
    let mut matches: Vec<(SystemTime, PathBuf)> = Vec::new();

    let years = std::fs::read_dir(&sessions_base).ok()?;
    for year in years.flatten() {
        for month in std::fs::read_dir(year.path())
            .ok()
            .into_iter()
            .flatten()
            .flatten()
        {
            for day in std::fs::read_dir(month.path())
                .ok()
                .into_iter()
                .flatten()
                .flatten()
            {
                for file in std::fs::read_dir(day.path())
                    .ok()
                    .into_iter()
                    .flatten()
                    .flatten()
                {
                    let path = file.path();
                    if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let Ok(meta) = file.metadata() else { continue };
                    let Ok(modified) = meta.modified() else {
                        continue;
                    };
                    if modified < started_at {
                        continue;
                    }
                    if let Some(project) = project {
                        if let Ok(content) = std::fs::read_to_string(&path) {
                            if let Some(first_line) = content.lines().next() {
                                if let Ok(event) =
                                    serde_json::from_str::<serde_json::Value>(first_line)
                                {
                                    let cwd = event["payload"]["cwd"].as_str().unwrap_or_default();
                                    if !cwd.starts_with(project) && cwd != project {
                                        continue;
                                    }
                                }
                            }
                        }
                    }
                    matches.push((modified, path));
                }
            }
        }
    }

    matches.sort_by(|a, b| b.0.cmp(&a.0));
    matches.into_iter().map(|(_, p)| p).next()
}

fn extract_codex_session_id(path: &Path) -> Option<String> {
    // Filename stem: "rollout-YYYY-MM-DDTHH-MM-SS-<uuid>" where uuid is 36 chars.
    let stem = path.file_stem()?.to_str()?;
    if stem.len() >= 36 {
        Some(stem[stem.len() - 36..].to_owned())
    } else {
        None
    }
}

async fn record_codex_external_session(
    log_path: String,
    started_at: SystemTime,
    project: Option<String>,
) -> Option<String> {
    for _ in 0..20 {
        if let Some(session_path) = find_codex_session(started_at, project.as_deref()) {
            let session_id =
                extract_codex_session_id(&session_path).unwrap_or_else(|| "unknown".to_owned());
            let _ = append_session_log(
                &log_path,
                &format!(
                    "external_session_id={session_id}\nexternal_session_log={}\n",
                    session_path.display()
                ),
            );
            return Some(session_id);
        }
        time::sleep(Duration::from_millis(500)).await;
    }
    None
}

async fn collect_stream<R>(
    mut stream: R,
    log_path: Option<String>,
    label: &'static str,
) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut wrote_header = false;

    loop {
        let bytes_read = stream
            .read(&mut buffer)
            .await
            .with_context(|| format!("failed to read agent {label}"))?;
        if bytes_read == 0 {
            break;
        }

        output.extend_from_slice(&buffer[..bytes_read]);
        if let Some(log_path) = log_path.as_deref() {
            if !wrote_header {
                let _ = append_session_log(log_path, &format!("\n{label}:\n"));
                wrote_header = true;
            }
            let _ = append_session_log(log_path, &String::from_utf8_lossy(&buffer[..bytes_read]));
        }
    }

    Ok(output)
}

async fn collect_stream_tee<R>(
    mut stream: R,
    log_path: Option<String>,
    label: &'static str,
) -> Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    use std::io::Write as _;

    let mut output = Vec::new();
    let mut buffer = [0_u8; 8192];
    let mut wrote_header = false;

    loop {
        let bytes_read = stream
            .read(&mut buffer)
            .await
            .with_context(|| format!("failed to read agent {label}"))?;
        if bytes_read == 0 {
            break;
        }

        let chunk = &buffer[..bytes_read];
        output.extend_from_slice(chunk);

        let _ = std::io::stdout().write_all(chunk);
        let _ = std::io::stdout().flush();

        if let Some(log_path) = log_path.as_deref() {
            if !wrote_header {
                let _ = append_session_log(log_path, &format!("\n{label}:\n"));
                wrote_header = true;
            }
            let _ = append_session_log(log_path, &String::from_utf8_lossy(chunk));
        }
    }

    Ok(output)
}

fn load_project_instructions(project: &str) -> String {
    let files = ["CLAUDE.md", "AGENTS.md", "copilot-instructions.md"];
    let sections: Vec<String> = files
        .iter()
        .filter_map(|name| {
            let path = std::path::Path::new(project).join(name);
            let content = std::fs::read_to_string(&path).ok()?;
            if content.trim().is_empty() {
                return None;
            }
            Some(format!("### {name}\n{content}"))
        })
        .collect();
    sections.join("\n\n")
}

fn build_prompt(request: &AgentRunRequest) -> String {
    let project_instructions = request
        .frontmatter
        .project
        .as_deref()
        .map(load_project_instructions)
        .unwrap_or_default();

    let role_section = request
        .role_instructions
        .as_deref()
        .map(|instr| format!("\n## Role\n\n{instr}\n"))
        .unwrap_or_default();

    let instructions_section = if project_instructions.is_empty() {
        String::new()
    } else {
        format!("\n## Project instructions\n\n{project_instructions}\n")
    };

    let plan_section = request
        .frontmatter
        .plan
        .as_deref()
        .and_then(|plan_path| std::fs::read_to_string(plan_path).ok())
        .map(|content| format!("\n## Task plan\n\n{content}\n"))
        .unwrap_or_default();

    format!(
        r#"{instructions}{role_section}{instructions_section}{plan_section}
Agent: {agent}
Task path: {task_path}
Task frontmatter:
{frontmatter}

Task markdown:
{body}
"#,
        instructions = if request.interactive {
            build_interactive_instructions()
        } else if request.interpret {
            build_interpretation_instructions()
        } else {
            build_agent_instructions(request.timeout)
        },
        agent = request.agent_name,
        task_path = request.task_path,
        frontmatter = serde_yaml::to_string(&request.frontmatter)
            .unwrap_or_else(|_| "<frontmatter serialization failed>".to_owned()),
        body = request.body,
    )
}

fn build_planning_prompt(request: &AgentRunRequest) -> String {
    let project_instructions = request
        .frontmatter
        .project
        .as_deref()
        .map(load_project_instructions)
        .unwrap_or_default();

    let instructions_section = if project_instructions.is_empty() {
        String::new()
    } else {
        format!("\n## Project instructions\n\n{project_instructions}\n")
    };

    format!(
        r#"{instructions}{instructions_section}
Agent: {agent}
Task path: {task_path}
Task frontmatter:
{frontmatter}

Task markdown:
{body}
"#,
        instructions = build_planning_instructions(request.timeout),
        agent = request.agent_name,
        task_path = request.task_path,
        frontmatter = serde_yaml::to_string(&request.frontmatter)
            .unwrap_or_else(|_| "<frontmatter serialization failed>".to_owned()),
        body = request.body,
    )
}

fn args_for_request(args: &[String], request: &AgentRunRequest) -> Vec<String> {
    let Some(project) = request.frontmatter.project.as_deref() else {
        return args
            .iter()
            .map(|arg| expand_request_value(arg, request))
            .collect();
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
    expand_varda_project(arg)
        .replace("{project}", project)
        .replace("{task}", &request.task_path)
}

fn resume_args_for_command(command: &str, resume_command: &str) -> Vec<String> {
    if command == "sh" || command.ends_with("/sh") {
        vec!["-c".to_owned(), resume_command.to_owned()]
    } else {
        vec![resume_command.to_owned()]
    }
}

#[cfg(unix)]
fn protect_interpreter_from_terminal_sigint(
    command_builder: &mut Command,
    request: &AgentRunRequest,
) {
    if request.interpret {
        command_builder.process_group(0);
    }
}

#[cfg(not(unix))]
fn protect_interpreter_from_terminal_sigint(
    _command_builder: &mut Command,
    _request: &AgentRunRequest,
) {
}

fn expand_request_value(value: &str, request: &AgentRunRequest) -> String {
    let Some(project) = request.frontmatter.project.as_deref() else {
        return expand_varda_project(value).replace("{task}", &request.task_path);
    };
    expand_arg(value, request, project)
}

fn expand_varda_project(value: &str) -> String {
    value
        .replace("{varda_project}", env!("CARGO_MANIFEST_DIR"))
        .replace("{varda_home}", &default_varda_home())
}

fn default_varda_home() -> String {
    if let Ok(home) = std::env::var("VARDA_HOME") {
        if !home.trim().is_empty() {
            return home;
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    std::path::Path::new(&home)
        .join(".varda")
        .display()
        .to_string()
}

fn env_for_request(
    env: &BTreeMap<String, String>,
    request: &AgentRunRequest,
) -> BTreeMap<String, String> {
    env.iter()
        .map(|(key, value)| (key.clone(), expand_request_value(value, request)))
        .collect()
}

fn find_claude_transcript(
    project: &str,
    varda_session_id: &str,
    started_at: SystemTime,
) -> Option<PathBuf> {
    let project_dir = claude_project_dir(project)?;
    let entries = std::fs::read_dir(project_dir).ok()?;
    let mut matches = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("jsonl") {
            continue;
        }

        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if modified < started_at {
            continue;
        }

        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        if content.contains(varda_session_id) {
            matches.push((modified, path));
        }
    }

    matches.sort_by(|left, right| right.0.cmp(&left.0));
    matches.into_iter().map(|(_, path)| path).next()
}

fn claude_project_dir(project: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let slug = project.replace('/', "-");
    Some(Path::new(&home).join(".claude/projects").join(slug))
}

fn append_session_log(path: &str, content: &str) -> Result<()> {
    use std::io::Write;

    let path = std::path::Path::new(path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create session log directory {}",
                parent.display()
            )
        })?;
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open session log {}", path.display()))?;
    file.write_all(content.as_bytes())
        .with_context(|| format!("failed to write session log {}", path.display()))?;
    Ok(())
}

fn set_terminal_title_for_agent(agent_name: &str) {
    if !std::io::stdout().is_terminal() {
        return;
    }

    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(terminal_title_sequence(agent_name).as_bytes());
    let _ = stdout.flush();
}

fn terminal_title_sequence(agent_name: &str) -> String {
    format!("\x1b]0;{}\x07", terminal_title(agent_name))
}

fn terminal_title(agent_name: &str) -> String {
    let agent_name = agent_name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>();
    let agent_name = agent_name.trim();
    if agent_name.is_empty() {
        "varda + agent".to_owned()
    } else {
        format!("varda + {agent_name}")
    }
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
            max_prompt_tokens: None,
            working_dir: None,
            env: BTreeMap::new(),
            interactive_command: None,
            interactive_args: None,
            resume_command_template: None,
        };
        let client = AcpSubprocessClient::new("echo", &config);

        let result = client
            .run_task(AgentRunRequest {
                agent_name: "echo".to_owned(),
                role_instructions: None,
                task_path: "task.md".to_owned(),
                frontmatter: TaskFrontmatter {
                    id: None,
                    status: TaskStatus::Ready,
                    project: Some("/work/project".to_owned()),
                    assignee: Some("echo".to_owned()),
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
                body: "# Task\n\nDo it.".to_owned(),
                timeout: Duration::from_secs(600),
                session_id: "session-1".to_owned(),
                session_log_path: None,
                interactive: false,
                interpret: false,
                stream: false,
                resume_command: None,
            })
            .await
            .expect("subprocess should echo prompt");

        assert!(result.recap.contains("You have at most 10 minutes"));
        assert!(result.recap.contains("Do it."));
        assert!(!result.requires_user);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn interpreter_subprocess_uses_separate_process_group() {
        let config = AgentConfig {
            kind: crate::config::AgentKind::Acp,
            command: "sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "printf '%s' \"$(ps -o pgid= -p $$ | tr -d ' ')\"".to_owned(),
            ],
            max_prompt_tokens: None,
            working_dir: None,
            env: BTreeMap::new(),
            interactive_command: None,
            interactive_args: None,
            resume_command_template: None,
        };
        let client = AcpSubprocessClient::new("shell", &config);

        let result = client
            .run_task(AgentRunRequest {
                agent_name: "shell".to_owned(),
                role_instructions: None,
                task_path: "task.md".to_owned(),
                frontmatter: TaskFrontmatter {
                    id: None,
                    status: TaskStatus::Ready,
                    project: Some("/work/project".to_owned()),
                    assignee: Some("shell".to_owned()),
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
                body: "# Task\n\nInterpret it.".to_owned(),
                timeout: Duration::from_secs(600),
                session_id: "session-1-interpret".to_owned(),
                session_log_path: None,
                interactive: false,
                interpret: true,
                stream: false,
                resume_command: None,
            })
            .await
            .expect("interpreter subprocess should run");

        let child_process_group: libc::pid_t = result
            .recap
            .parse()
            .expect("subprocess should print its process group");
        let parent_process_group = unsafe { libc::getpgrp() };

        assert_ne!(
            child_process_group, parent_process_group,
            "interpreter subprocess should not share Varda's foreground process group"
        );
    }

    #[tokio::test]
    async fn subprocess_client_streams_stdout_and_stderr_to_session_log() {
        let root = std::env::temp_dir().join(format!("varda-acp-log-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("temp directory should be created");
        let log_path = root.join("session.log");
        let config = AgentConfig {
            kind: crate::config::AgentKind::Acp,
            command: "sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "printf 'recap line\\n'; printf 'diagnostic line\\n' >&2".to_owned(),
            ],
            max_prompt_tokens: None,
            working_dir: None,
            env: BTreeMap::new(),
            interactive_command: None,
            interactive_args: None,
            resume_command_template: None,
        };
        let client = AcpSubprocessClient::new("shell", &config);

        let result = client
            .run_task(AgentRunRequest {
                agent_name: "shell".to_owned(),
                role_instructions: None,
                task_path: "task.md".to_owned(),
                frontmatter: TaskFrontmatter {
                    id: None,
                    status: TaskStatus::Ready,
                    project: Some("/work/project".to_owned()),
                    assignee: Some("shell".to_owned()),
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
                body: "# Task\n\nDo it.".to_owned(),
                timeout: Duration::from_secs(600),
                session_id: "session-1".to_owned(),
                session_log_path: Some(log_path.display().to_string()),
                interactive: false,
                interpret: false,
                stream: false,
                resume_command: None,
            })
            .await
            .expect("subprocess should run");

        let log = std::fs::read_to_string(&log_path).expect("session log should be readable");
        assert_eq!(result.recap, "recap line");
        assert!(log.contains("stdout:\nrecap line"));
        assert!(log.contains("stderr:\ndiagnostic line"));
        assert!(log.contains("status=exit status: 0"));
    }

    #[tokio::test]
    async fn subprocess_client_applies_agent_env_and_working_dir() {
        let root = std::env::temp_dir().join(format!("varda-acp-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&root).expect("temp directory should be created");
        let root = std::fs::canonicalize(root).expect("temp directory should canonicalize");
        let config = AgentConfig {
            kind: crate::config::AgentKind::Acp,
            command: "sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "printf '%s\\n%s\\n' \"$VARDA_TEST_PROJECT\" \"$PWD\"".to_owned(),
            ],
            max_prompt_tokens: None,
            working_dir: Some("{project}".to_owned()),
            env: BTreeMap::from([("VARDA_TEST_PROJECT".to_owned(), "{project}".to_owned())]),
            interactive_command: None,
            interactive_args: None,
            resume_command_template: None,
        };
        let client = AcpSubprocessClient::new("shell", &config);

        let result = client
            .run_task(AgentRunRequest {
                agent_name: "shell".to_owned(),
                role_instructions: None,
                task_path: "task.md".to_owned(),
                frontmatter: TaskFrontmatter {
                    id: None,
                    status: TaskStatus::Ready,
                    project: Some(root.display().to_string()),
                    assignee: Some("shell".to_owned()),
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
                body: "# Task\n\nDo it.".to_owned(),
                timeout: Duration::from_secs(600),
                session_id: "session-1".to_owned(),
                session_log_path: None,
                interactive: false,
                interpret: false,
                stream: false,
                resume_command: None,
            })
            .await
            .expect("subprocess should run");

        let expected_project = root.display().to_string();
        assert_eq!(
            result.recap,
            format!("{expected_project}\n{expected_project}")
        );
        std::fs::remove_dir_all(root).expect("temp directory should be removed");
    }

    #[test]
    fn replaces_dot_cd_with_task_project_path() {
        let request = AgentRunRequest {
            agent_name: "codex".to_owned(),
            role_instructions: None,
            task_path: "/home/user/.varda/operations/tasks/task.md".to_owned(),
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
            stream: false,
            resume_command: None,
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

    #[test]
    fn expands_task_project_placeholders_in_args() {
        let request = AgentRunRequest {
            agent_name: "claude".to_owned(),
            role_instructions: None,
            task_path: "/home/user/.varda/operations/tasks/task.md".to_owned(),
            frontmatter: TaskFrontmatter {
                id: None,
                status: TaskStatus::Ready,
                project: Some("/work/project".to_owned()),
                assignee: Some("claude".to_owned()),
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
            stream: false,
            resume_command: None,
        };

        let args = args_for_request(
            &[
                "-p".to_owned(),
                "--permission-mode".to_owned(),
                "acceptEdits".to_owned(),
                "--add-dir".to_owned(),
                "{project}".to_owned(),
            ],
            &request,
        );

        assert_eq!(
            args,
            vec![
                "-p",
                "--permission-mode",
                "acceptEdits",
                "--add-dir",
                "/work/project",
            ]
        );
    }

    #[test]
    fn expands_varda_project_placeholders_in_args() {
        let request = sample_request("codex", "/work/project");

        let args = args_for_request(
            &[
                "exec".to_owned(),
                "--cd".to_owned(),
                ".".to_owned(),
                "--add-dir".to_owned(),
                "{varda_project}".to_owned(),
                "--add-dir".to_owned(),
                "{varda_home}".to_owned(),
                "-".to_owned(),
            ],
            &request,
        );

        assert_eq!(
            args,
            vec![
                "exec".to_owned(),
                "--cd".to_owned(),
                "/work/project".to_owned(),
                "--add-dir".to_owned(),
                env!("CARGO_MANIFEST_DIR").to_owned(),
                "--add-dir".to_owned(),
                default_varda_home(),
                "-".to_owned()
            ]
        );
    }

    #[test]
    fn build_resume_command_substitutes_session_id_and_project() {
        let config = AgentConfig {
            kind: crate::config::AgentKind::Acp,
            command: "claude".to_owned(),
            args: vec![],
            max_prompt_tokens: None,
            working_dir: None,
            env: BTreeMap::new(),
            interactive_command: None,
            interactive_args: None,
            resume_command_template: Some(
                "claude --resume {external_session_id} --add-dir {project}".to_owned(),
            ),
        };
        let client = AcpSubprocessClient::new("claude", &config);
        let request = sample_request("claude", "/work/project");

        let resume = client.build_resume_command(&request, Some("abc-123"));
        assert_eq!(
            resume.as_deref(),
            Some("claude --resume abc-123 --add-dir /work/project")
        );

        assert!(client.build_resume_command(&request, None).is_none());
    }

    #[test]
    fn build_resume_command_returns_none_without_template() {
        let config = AgentConfig {
            kind: crate::config::AgentKind::Acp,
            command: "claude".to_owned(),
            args: vec![],
            max_prompt_tokens: None,
            working_dir: None,
            env: BTreeMap::new(),
            interactive_command: None,
            interactive_args: None,
            resume_command_template: None,
        };
        let client = AcpSubprocessClient::new("claude", &config);
        let request = sample_request("claude", "/work/project");

        assert!(
            client
                .build_resume_command(&request, Some("abc-123"))
                .is_none()
        );
    }

    #[test]
    fn resume_args_run_stored_command_through_shell_when_available() {
        assert_eq!(
            resume_args_for_command("sh", "codex resume abc-123"),
            vec!["-c".to_owned(), "codex resume abc-123".to_owned()]
        );
        assert_eq!(
            resume_args_for_command("/bin/sh", "claude --resume abc-123"),
            vec!["-c".to_owned(), "claude --resume abc-123".to_owned()]
        );
        assert_eq!(
            resume_args_for_command("agent", "opaque resume string"),
            vec!["opaque resume string".to_owned()]
        );
    }

    #[test]
    fn terminal_title_sequence_names_varda_and_agent() {
        assert_eq!(terminal_title_sequence("codex"), "\x1b]0;varda + codex\x07");
    }

    #[test]
    fn terminal_title_sanitizes_control_characters() {
        assert_eq!(terminal_title("co\x1bdex\x07"), "varda + co dex");
        assert_eq!(terminal_title("\n\t"), "varda + agent");
    }

    #[test]
    fn find_copilot_process_log_in_picks_earliest_after_start() {
        let dir = std::env::temp_dir().join(format!("varda-copilot-logs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Create a file before started_at.
        std::fs::write(dir.join("process-11111.log"), b"old").unwrap();
        // Non-process file should always be ignored.
        std::fs::write(dir.join("other.log"), b"decoy").unwrap();
        std::thread::sleep(Duration::from_millis(15));

        let started_at = SystemTime::now();
        std::thread::sleep(Duration::from_millis(15));

        // First valid file after started_at — should be chosen.
        std::fs::write(dir.join("process-22222.log"), b"current").unwrap();
        std::thread::sleep(Duration::from_millis(15));

        // A later file — should not be chosen because 22222 has an earlier mtime.
        std::fs::write(dir.join("process-33333.log"), b"later").unwrap();

        let result = find_copilot_process_log_in(&dir, started_at);
        assert!(result.is_some(), "expected a match after started_at");
        let name = result
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(name, "process-22222.log");

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn find_copilot_process_log_in_returns_none_when_no_log_after_start() {
        let dir =
            std::env::temp_dir().join(format!("varda-copilot-logs-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        std::fs::write(dir.join("process-00000.log"), b"old").unwrap();
        std::thread::sleep(Duration::from_millis(15));

        let started_at = SystemTime::now();

        let result = find_copilot_process_log_in(&dir, started_at);
        assert!(
            result.is_none(),
            "expected no match when all logs predate started_at"
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    fn sample_request(agent: &str, project: &str) -> AgentRunRequest {
        AgentRunRequest {
            agent_name: agent.to_owned(),
            role_instructions: None,
            task_path: "task.md".to_owned(),
            frontmatter: TaskFrontmatter {
                id: None,
                status: TaskStatus::Ready,
                project: Some(project.to_owned()),
                assignee: Some(agent.to_owned()),
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
            stream: false,
            resume_command: None,
        }
    }
}
