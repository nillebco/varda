//! ACP transport support.

use std::collections::BTreeMap;
use std::io::{IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
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
use crate::sandbox::{
    CommandSpec, LaunchMode, LocalProvider, SandboxContext, SandboxProvider, SandboxSession,
};

/// Guest-visible path the staged task prompt lands at inside a non-`local`
/// sandbox. Advertised to the agent via `VARDA_PROMPT_FILE` (M13a §5).
///
/// MUST live OUTSIDE the agent's `/home/agent` HOME: microsandbox's `--copy-file`
/// creates the destination's parent dir as **root-owned** in the guest overlay,
/// so staging into `$HOME` re-owns HOME to root and the uid-1001 `agent` can no
/// longer write `~/.claude` (Claude Code then fails every Bash call and its own
/// config/transcript writes with `EACCES: mkdir '/home/agent/.claude'`). Staging
/// under `/opt/varda` keeps HOME image-owned and writable; the file lands
/// root-owned `0644`, still world-readable so the agent can `cat` it.
const GUEST_PROMPT_FILE: &str = "/opt/varda/prompt.txt";

/// Guest-visible hostname that resolves to the HOST machine from inside an
/// own-kernel microVM guest (`microsandbox`/`clawk`). The broker BINDS to host
/// loopback (`127.0.0.1`), but the guest's own `127.0.0.1` is NOT the host — the
/// guest reaches the host's loopback service by dialing `host.microsandbox.internal`
/// instead. So for a VM-backed primitive the guest-visible broker host is this
/// name (paired with `VARDA_MCP_PORT`), distinct from the host bind address.
/// (msb DENIES host access by default, so the `host` net-rule group must also be
/// allowed — see `MicrosandboxSession::wrap`.)
const GUEST_BROKER_HOST: &str = "host.microsandbox.internal";

/// M11-ext — stage every file-target credential the identity resolved
/// ([`SandboxSession::identity_files`]) into the guest via `stage_credential_file`,
/// so the value lands as a READ-ONLY file inside the box in BOTH launch modes
/// (docker cp / msb `--copy-file`; cleaned on teardown). Env-target credentials
/// fold in via the provider's `guest_env()` at wrap and need no staging. Must run
/// after `prepare` and before `wrap` (providers bake staging into the argv).
fn stage_identity_files(session: &dyn SandboxSession, sandbox_name: &str) -> Result<()> {
    for (guest_path, value) in session.identity_files() {
        session
            .stage_credential_file(&value, &guest_path)
            .with_context(|| {
                format!(
                    "failed to stage credential file '{guest_path}' into '{sandbox_name}' sandbox"
                )
            })?;
    }
    Ok(())
}

/// Owns the prepared sandbox [`SandboxSession`] and guarantees `teardown()` runs
/// on EVERY exit path — including a cancel, where the M10 idle/budget watchdog
/// drops the in-flight `run_task` future before it reaches the inline teardown.
///
/// Rust has no async `Drop`, so the two paths differ:
/// - **Normal exit** — the caller invokes [`Self::teardown`], which takes the
///   session and awaits its teardown inline (a leak here would fail the run).
/// - **Cancel** — the future is dropped with the session still held, so [`Drop`]
///   detaches the teardown onto the current Tokio runtime. Sandbox teardown is
///   idempotent `docker rm -f` / `volume rm -f`, so fire-and-forget is enough to
///   stop `varda-sbx-*` containers/volumes from leaking on an idle/budget kill.
struct SessionTeardownGuard {
    session: Option<Box<dyn SandboxSession>>,
}

impl SessionTeardownGuard {
    fn new(session: Box<dyn SandboxSession>) -> Self {
        Self {
            session: Some(session),
        }
    }

    /// Borrow the live session for the `&self` calls (`wrap`, `validate_mounts`,
    /// `extract_session_store`, …) made while the run is in flight.
    fn session(&self) -> &dyn SandboxSession {
        self.session
            .as_deref()
            .expect("session is present until teardown() or Drop consumes it")
    }

    /// Normal-exit teardown: take the session and await its cleanup inline,
    /// disarming [`Drop`] so it does not double-tear-down.
    async fn teardown(mut self) -> Result<()> {
        match self.session.take() {
            Some(session) => session.teardown().await,
            None => Ok(()),
        }
    }
}

impl Drop for SessionTeardownGuard {
    fn drop(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        // Cancel path: the future was dropped before the inline teardown ran.
        // Detach teardown onto the runtime so the container/volume are still
        // reclaimed. Best-effort — nothing awaits it, but sandbox removal is
        // idempotent, so a fire-and-forget task is enough to prevent the leak.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    if let Err(error) = session.teardown().await {
                        eprintln!(
                            "warning: failed to tear down sandbox session on cancel: {error:#}"
                        );
                    }
                });
            }
            Err(_) => {
                eprintln!(
                    "warning: sandbox session dropped outside a Tokio runtime; teardown skipped"
                );
            }
        }
    }
}

#[derive(Clone)]
pub struct AcpSubprocessClient {
    agent_name: String,
    command: String,
    args: Vec<String>,
    working_dir: Option<String>,
    env: BTreeMap<String, String>,
    static_env: BTreeMap<String, String>,
    interactive_command: Option<String>,
    interactive_args: Option<Vec<String>>,
    resume_command_template: Option<String>,
    sandbox: Arc<dyn SandboxProvider>,
}

impl AcpSubprocessClient {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn new(agent_name: impl Into<String>, config: &AgentConfig) -> Self {
        // Default to the identity `local` provider so callers that do not opt
        // into sandboxing keep the exact pre-provider behavior.
        Self::with_sandbox(agent_name, config, Arc::new(LocalProvider))
    }

    pub fn with_sandbox(
        agent_name: impl Into<String>,
        config: &AgentConfig,
        sandbox: Arc<dyn SandboxProvider>,
    ) -> Self {
        Self::with_sandbox_env(agent_name, config, sandbox, BTreeMap::new())
    }

    pub fn with_sandbox_env(
        agent_name: impl Into<String>,
        config: &AgentConfig,
        sandbox: Arc<dyn SandboxProvider>,
        static_env: BTreeMap<String, String>,
    ) -> Self {
        Self {
            agent_name: agent_name.into(),
            command: config.command.clone(),
            args: config.args.clone(),
            working_dir: config.working_dir.clone(),
            env: config.env.clone(),
            static_env,
            interactive_command: config.interactive_command.clone(),
            interactive_args: config.interactive_args.clone(),
            resume_command_template: config.resume_command_template.clone(),
            sandbox,
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
        mut args: Vec<String>,
        request: &AgentRunRequest,
    ) -> Result<AgentRunResult> {
        if request.interactive {
            return self.execute_interactive(prompt, args, request).await;
        }
        let started_at = SystemTime::now();
        let command = expand_request_value(&self.command, request);

        // M12 per-task capability allowlist: for the Claude Code backend, a
        // headless (`-p`) run has no interactive approver, so any command not
        // pre-authorized is denied. Materialize the task's declared
        // `allow_commands` into a run-scoped settings file carrying
        // `permissions.allow` and inject it via `--settings` — deterministic,
        // scoped to exactly those commands, never a blanket bypass.
        if crate::capability::is_claude_backend(&command)
            && !request.frontmatter.allow_commands.is_empty()
        {
            if let Some(log_path) = request.session_log_path.as_deref() {
                match crate::capability::write_claude_run_settings(
                    Path::new(log_path),
                    &request.frontmatter.allow_commands,
                ) {
                    Ok(Some(settings_path)) => {
                        args.push("--settings".to_owned());
                        args.push(settings_path.display().to_string());
                        let _ = append_session_log(
                            log_path,
                            &format!(
                                "allow_commands={:?}\nrun_settings={}\n",
                                request.frontmatter.allow_commands,
                                settings_path.display()
                            ),
                        );
                    }
                    Ok(None) => {}
                    Err(error) => {
                        eprintln!(
                            "warning: failed to write run settings for allow_commands: {error:#}"
                        );
                    }
                }
            } else {
                eprintln!(
                    "warning: allow_commands declared but no session log path is set; \
                     the capability allowlist was not injected"
                );
            }
        }
        let working_dir = self
            .working_dir
            .as_deref()
            .map(|dir| expand_request_value(dir, request));
        let env = env_for_request(&self.env, &self.static_env, request);
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

        // Resolve the invocation, then let the sandbox provider rewrite it. For
        // the `local` provider this is the identity, so the spawn below is
        // byte-for-byte the same command as before providers existed.
        let spec = CommandSpec {
            program: command.clone(),
            args: args.clone(),
            env: env.clone(),
            cwd: working_dir.as_deref().map(PathBuf::from),
        };
        let sandbox_ctx = SandboxContext {
            project_root: Path::new(
                request
                    .frontmatter
                    .project
                    .as_deref()
                    .unwrap_or_else(|| working_dir.as_deref().unwrap_or(".")),
            ),
            route_glob: "",
            agent_kind: crate::config::AgentKind::Acp,
            session_id: &request.session_id,
        };
        let session = self
            .sandbox
            .prepare(&sandbox_ctx)
            .await
            .with_context(|| format!("failed to prepare '{}' sandbox", self.sandbox.name()))?;
        // Own the session in a guard so `teardown()` runs on EVERY exit path —
        // including a watchdog/budget cancel that drops this future mid-run
        // (otherwise the `varda-sbx-*` container/volume leak). All `&self` uses
        // below go through `guard.session()`; the final `guard.teardown()` awaits
        // cleanup inline on the normal path.
        let guard = SessionTeardownGuard::new(session);
        let session = guard.session();
        let session_store_root = session.session_store_root();
        if session_store_root.is_none() {
            eprintln!("WARN resume-command unavailable under sandbox");
        }
        // Fail loudly if a declared bind-mount source is unreachable on the host
        // (a would-be empty in-VM stub on a VM-backed daemon) before we run.
        session
            .validate_mounts()
            .with_context(|| format!("unusable mount for '{}' sandbox", self.sandbox.name()))?;
        // M11-ext — stage any file-target credentials as read-only guest files
        // before wrap bakes the argv. env-target credentials fold in via the
        // provider's `guest_env()` at wrap; file targets need the live session.
        stage_identity_files(session, self.sandbox.name())?;
        // Live stores (local) are polled while the agent runs; extracted stores
        // (docker volume + `docker cp`) are only discovered post-exit.
        let store_is_live = session.store_is_live();
        let spec = session.wrap(spec, LaunchMode::Batch).with_context(|| {
            format!(
                "failed to wrap command for '{}' sandbox",
                self.sandbox.name()
            )
        })?;
        // Provider-specific batch pre-start staging: docker `create` → `docker cp`
        // any file-target credentials → `start -ai` so they actually reach the
        // guest (its `docker run` streaming form cannot copy a file in first).
        // local/msb return the wrapped command unchanged.
        let spec = session.begin_batch(spec).await.with_context(|| {
            format!(
                "failed to begin batch session for '{}' sandbox",
                self.sandbox.name()
            )
        })?;

        let mut command_builder = Command::new(&spec.program);
        command_builder
            .args(&spec.args)
            .envs(&spec.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        protect_interpreter_from_terminal_sigint(&mut command_builder, request);
        if let Some(cwd) = spec.cwd.as_deref() {
            command_builder.current_dir(cwd);
        }

        let mut child = command_builder.spawn().with_context(|| {
            format!(
                "failed to start agent '{}' with command '{}'",
                self.agent_name, spec.program
            )
        })?;

        let mut stdin = child.stdin.take().context("failed to open agent stdin")?;
        stdin
            .write_all(prompt.as_bytes())
            .await
            .context("failed to write task prompt to agent stdin")?;
        drop(stdin);

        // Resume-capture depends on reaching the agent's session store on the
        // host. For a LIVE store (host `$HOME` for `local`) discovery polls while
        // the agent runs, exactly where it writes. For an EXTRACTED store
        // (`docker` volume) the store is not host-visible yet, so we defer
        // discovery until after the run has been `docker cp`-ed out (below).
        // Handle for the async session-store discovery task; awaited after the run
        // to build the resume command. Set for BOTH a live store (local: polled
        // while the agent runs, below) and an extracted store (docker: recorded
        // after `docker cp`). Previously the live handle was discarded, so `local`
        // headless runs never produced a resume_command and auto-resume never fired.
        let mut record_handle = None;
        if store_is_live && let Some(session_root) = session_store_root.as_deref() {
            record_handle = self.record_external_session(request, started_at, session_root);
        }

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

        // Extracted stores (docker): the agent has exited, so materialize its
        // session store on the host (`docker cp` from the volume) and THEN run a
        // single discovery pass — the files already exist, so the first poll hits
        // immediately. This is the container→host round-trip that makes
        // resume-capture work on a VM-backed daemon with a narrow share.
        if !store_is_live {
            match session.extract_session_store().await {
                Ok(()) => {
                    if let Some(session_root) = session_store_root.as_deref() {
                        record_handle =
                            self.record_external_session(request, started_at, session_root);
                    }
                }
                Err(error) => {
                    eprintln!("warning: failed to extract sandbox session store: {error:#}");
                }
            }
        }

        if let Err(error) = guard.teardown().await {
            eprintln!("warning: failed to tear down sandbox session: {error:#}");
        }

        // The external session id (hence resume command) comes from the discovery
        // task: for a live store it polled during the run; for an extracted store it
        // ran against the copied-out store above.
        let resume_command = match record_handle {
            Some(handle) => {
                let external_session_id = handle.await.ok().flatten();
                self.build_resume_command(request, external_session_id.as_deref())
            }
            None => None,
        };

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
            resume_command,
        })
    }

    async fn execute_interactive(
        &self,
        prompt: String,
        args: Vec<String>,
        request: &AgentRunRequest,
    ) -> Result<AgentRunResult> {
        // M13a: the interactive SHELL path (an `interactive_command` is set) now
        // runs inside docker/microsandbox too — see the sandboxed branch below.
        // The resume and pipe-fallback sub-paths remain local-only (guarded at
        // their own sites); `local` keeps its byte-for-byte pre-M13a behavior.
        let command = expand_request_value(&self.command, request);
        let working_dir = self
            .working_dir
            .as_deref()
            .map(|dir| expand_request_value(dir, request));
        let mut env = env_for_request(&self.env, &self.static_env, request);

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
            // Resuming an interactive session under a non-identity sandbox is not
            // part of M13a (the fresh-shell launch is); fail clearly rather than
            // silently resuming on the host.
            if self.sandbox.name() != "local" {
                bail!(
                    "resuming an interactive session under the '{}' sandbox is not supported yet; \
                     use sandbox=\"local\"",
                    self.sandbox.name()
                );
            }
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

        // M13a: route the interactive SHELL (an `interactive_command` is set)
        // through the sandbox provider when it isn't `local`. The prompt is staged
        // INTO the guest and the user's TTY is attached; teardown is guaranteed.
        if self.sandbox.name() != "local" {
            if let Some(interactive_cmd) = self.interactive_command.clone() {
                return self
                    .execute_interactive_sandboxed(
                        request,
                        &interactive_cmd,
                        &prompt,
                        working_dir,
                        env,
                    )
                    .await;
            }
            bail!(
                "the pipe-based interactive fallback is not supported under the '{}' sandbox; \
                 set `interactive_command` (e.g. \"sh\") or use sandbox=\"local\"",
                self.sandbox.name()
            );
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

            // Interactive runs are local-only (guarded above), so the agent's
            // session store lives under the host's real HOME.
            let record_handle =
                self.record_external_session(request, started_at, &host_session_root());

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

        // Local-only interactive fallback: store lives under the host's HOME.
        let record_handle = self.record_external_session(request, started_at, &host_session_root());

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

    /// Interactive SHELL launch inside a non-`local` sandbox (M13a §4–§6).
    ///
    /// Stages the task prompt INTO the guest, wraps the interactive command in
    /// [`LaunchMode::Interactive`] (docker `-it`/create-cp-start, `msb -t`),
    /// attaches the user's TTY with inherited stdio, then — mirroring the batch
    /// extracted-store path — extracts the session store, records the external
    /// session, and builds a resume command. Teardown is guaranteed via
    /// [`SessionTeardownGuard`] on every exit path.
    async fn execute_interactive_sandboxed(
        &self,
        request: &AgentRunRequest,
        interactive_cmd: &str,
        prompt: &str,
        working_dir: Option<String>,
        mut env: BTreeMap<String, String>,
    ) -> Result<AgentRunResult> {
        // A sandboxed interactive launch needs a real TTY: `docker -it` / `msb -t`
        // error out otherwise. Fail clearly BEFORE we prepare the box.
        use std::io::IsTerminal as _;
        if !std::io::stdin().is_terminal() {
            bail!(
                "an interactive sandboxed run needs a terminal on stdin; \
                 `{}` interactive requires a TTY (are you piping input or running headless?)",
                self.sandbox.name()
            );
        }

        let interactive_cmd = expand_request_value(interactive_cmd, request);
        let interactive_args =
            args_for_request(self.interactive_args.as_deref().unwrap_or(&[]), request);

        let spec = CommandSpec {
            program: interactive_cmd.clone(),
            args: interactive_args,
            env: env.clone(),
            cwd: working_dir.as_deref().map(PathBuf::from),
        };
        let sandbox_ctx = SandboxContext {
            project_root: Path::new(
                request
                    .frontmatter
                    .project
                    .as_deref()
                    .unwrap_or_else(|| working_dir.as_deref().unwrap_or(".")),
            ),
            route_glob: "",
            agent_kind: crate::config::AgentKind::Acp,
            session_id: &request.session_id,
        };
        let session = self
            .sandbox
            .prepare(&sandbox_ctx)
            .await
            .with_context(|| format!("failed to prepare '{}' sandbox", self.sandbox.name()))?;
        let guard = SessionTeardownGuard::new(session);
        let session = guard.session();
        session
            .validate_mounts()
            .with_context(|| format!("unusable mount for '{}' sandbox", self.sandbox.name()))?;

        // Stage the prompt into a GUEST-visible file and advertise its guest path,
        // so the interactive shell/agent can read the task without the host temp
        // (invisible in-guest) — M13a §5. env must carry VARDA_PROMPT_FILE BEFORE
        // wrap(), since providers bake env into the wrapped argv.
        let guest_prompt = session
            .stage_file(prompt, GUEST_PROMPT_FILE)
            .context("failed to stage the task prompt into the sandbox")?;
        env.insert("VARDA_PROMPT_FILE".to_owned(), guest_prompt.clone());
        // M11-ext — stage file-target credentials (env targets fold in via env above).
        stage_identity_files(session, self.sandbox.name())?;
        let spec = CommandSpec {
            env: env.clone(),
            ..spec
        };

        if let Some(log_path) = request.session_log_path.as_deref() {
            let _ = append_session_log(
                log_path,
                &format!(
                    "[interactive-sandboxed]\nsandbox={}\nVARDA_PROMPT_FILE={}\n",
                    self.sandbox.name(),
                    guest_prompt
                ),
            );
        }

        let started_at = SystemTime::now();
        let session_store_root = session.session_store_root();
        let store_is_live = session.store_is_live();

        let wrapped = session
            .wrap(spec, LaunchMode::Interactive)
            .with_context(|| {
                format!(
                    "failed to wrap interactive command for '{}' sandbox",
                    self.sandbox.name()
                )
            })?;
        // Provider-specific interactive lifecycle: docker create → cp → start -ai;
        // msb/local return the wrapped command directly.
        let launch = session.begin_interactive(wrapped).await.with_context(|| {
            format!(
                "failed to begin interactive session for '{}' sandbox",
                self.sandbox.name()
            )
        })?;

        // Live stores (would be local; not this path) poll during the run; the
        // extracted docker/msb store is discovered post-exit.
        let mut record_handle = None;
        if store_is_live && let Some(session_root) = session_store_root.as_deref() {
            record_handle = self.record_external_session(request, started_at, session_root);
        }

        let mut command_builder = Command::new(&launch.program);
        command_builder
            .args(&launch.args)
            .envs(&launch.env)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        if let Some(cwd) = launch.cwd.as_deref() {
            command_builder.current_dir(cwd);
        }

        set_terminal_title_for_agent(&self.agent_name);

        let mut child = command_builder.spawn().with_context(|| {
            format!(
                "failed to start interactive sandboxed agent '{}' with command '{}'",
                self.agent_name, launch.program
            )
        })?;
        let status = child
            .wait()
            .await
            .context("failed to wait for interactive sandboxed agent subprocess")?;

        // Extracted stores: materialize the guest session store on the host, THEN
        // discover once (mirrors the batch extracted-store path).
        if !store_is_live {
            match session.extract_session_store().await {
                Ok(()) => {
                    if let Some(session_root) = session_store_root.as_deref() {
                        record_handle =
                            self.record_external_session(request, started_at, session_root);
                    }
                }
                Err(error) => {
                    eprintln!("warning: failed to extract sandbox session store: {error:#}");
                }
            }
        }

        if let Err(error) = guard.teardown().await {
            eprintln!("warning: failed to tear down sandbox session: {error:#}");
        }

        if let Some(log_path) = request.session_log_path.as_deref() {
            let _ = append_session_log(log_path, &format!("\nstatus={status}\n"));
        }
        if !status.success() {
            bail!("agent '{}' exited with status {}", self.agent_name, status);
        }

        let external_session_id = match record_handle {
            Some(handle) => handle.await.ok().flatten(),
            None => None,
        };
        let resume_command = self.build_resume_command(request, external_session_id.as_deref());

        Ok(AgentRunResult {
            recap: "Interactive sandboxed session completed.\n\nrequires_user: false".to_owned(),
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
        session_root: &Path,
    ) -> Option<tokio::task::JoinHandle<Option<String>>> {
        let log_path = request.session_log_path.as_deref()?.to_owned();
        let session_root = session_root.to_path_buf();

        if self.command == "claude" {
            let project = request.frontmatter.project.as_deref()?.to_owned();
            let varda_session_id = request.session_id.clone();
            Some(tokio::spawn(async move {
                record_claude_external_session(
                    session_root,
                    log_path,
                    project,
                    varda_session_id,
                    started_at,
                )
                .await
            }))
        } else if self.uses_copilot() {
            Some(tokio::spawn(async move {
                record_copilot_external_session(session_root, log_path, started_at).await
            }))
        } else if self.command == "codex" {
            let project = request.frontmatter.project.clone();
            Some(tokio::spawn(async move {
                record_codex_external_session(session_root, log_path, started_at, project).await
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
    session_root: PathBuf,
    log_path: String,
    project: String,
    varda_session_id: String,
    started_at: SystemTime,
) -> Option<String> {
    for _ in 0..20 {
        if let Some(transcript) =
            find_claude_transcript(&session_root, &project, &varda_session_id, started_at)
        {
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

fn find_copilot_process_log(session_root: &Path, started_at: SystemTime) -> Option<PathBuf> {
    let logs_dir = session_root.join(".copilot/logs");
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
    session_root: PathBuf,
    log_path: String,
    started_at: SystemTime,
) -> Option<String> {
    for _ in 0..20 {
        if let Some(process_log) = find_copilot_process_log(&session_root, started_at)
            && let Some(workspace_id) = extract_copilot_workspace_id(&process_log)
        {
            let events_path = session_root
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
        time::sleep(Duration::from_millis(500)).await;
    }
    None
}

fn find_codex_session(
    session_root: &Path,
    started_at: SystemTime,
    project: Option<&str>,
) -> Option<PathBuf> {
    let sessions_base = session_root.join(".codex/sessions");
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
                    if let Some(project) = project
                        && let Ok(content) = std::fs::read_to_string(&path)
                        && let Some(first_line) = content.lines().next()
                        && let Ok(event) = serde_json::from_str::<serde_json::Value>(first_line)
                    {
                        let cwd = event["payload"]["cwd"].as_str().unwrap_or_default();
                        if !cwd.starts_with(project) && cwd != project {
                            continue;
                        }
                    }
                    matches.push((modified, path));
                }
            }
        }
    }

    matches.sort_by_key(|item| std::cmp::Reverse(item.0));
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
    session_root: PathBuf,
    log_path: String,
    started_at: SystemTime,
    project: Option<String>,
) -> Option<String> {
    for _ in 0..20 {
        if let Some(session_path) =
            find_codex_session(&session_root, started_at, project.as_deref())
        {
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
    let orchestration_section = request
        .orchestration_socket_path
        .as_deref()
        .map(|socket| {
            format!(
                "\n## Nested orchestration MCP\n\nA host-gated MCP broker is available at Unix socket `{socket}`. Speak newline-delimited JSON-RPC over that socket; `tools/list` advertises the available tools. The only spawn capability is `spawn_subtask`, and denials are hard errors.\n"
            )
        })
        .unwrap_or_default();

    format!(
        r#"{instructions}{role_section}{instructions_section}{plan_section}{orchestration_section}
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

        if arg == "--cd"
            && let Some(value) = args.get(index + 1)
        {
            resolved.push(if value == "." {
                project.to_owned()
            } else {
                expand_arg(value, request, project)
            });
            index += 2;
            continue;
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

/// Host `$HOME` as the session-store root for un-sandboxed (interactive) runs,
/// mirroring [`crate::sandbox::LocalSession::session_store_root`].
fn host_session_root() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn default_varda_home() -> String {
    if let Ok(home) = std::env::var("VARDA_HOME")
        && !home.trim().is_empty()
    {
        return home;
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
    std::path::Path::new(&home)
        .join(".varda")
        .display()
        .to_string()
}

fn env_for_request(
    env: &BTreeMap<String, String>,
    static_env: &BTreeMap<String, String>,
    request: &AgentRunRequest,
) -> BTreeMap<String, String> {
    let mut out: BTreeMap<String, String> = env
        .iter()
        .map(|(key, value)| (key.clone(), expand_env_value(value, request)))
        .collect();
    out.extend(
        static_env
            .iter()
            .map(|(key, value)| (key.clone(), expand_env_value(value, request))),
    );
    // M8-transport: expose the per-session MCP broker to the sandboxed agent. A
    // `local`/`docker` guest reaches it through the bind-mounted Unix socket
    // (`VARDA_MCP_SOCKET`); an own-kernel microVM guest (microsandbox/clawk) cannot
    // `connect()` that socket and instead dials the host over TCP, so it gets
    // `VARDA_MCP_ADDR` (host:port) plus `VARDA_MCP_PORT` (the port alone).
    if let Some(socket) = request.orchestration_socket_path.as_deref() {
        out.insert("VARDA_MCP_SOCKET".to_owned(), socket.to_owned());
    }
    if let Some(addr) = request.orchestration_addr.as_deref() {
        out.insert("VARDA_MCP_ADDR".to_owned(), addr.to_owned());
        if let Some((_host, port)) = addr.rsplit_once(':') {
            out.insert("VARDA_MCP_PORT".to_owned(), port.to_owned());
        }
        // Guest-visible broker HOST ≠ the host bind address. `orchestration_addr`
        // is set only for a VM-backed TCP broker (`primitive_needs_tcp_broker`),
        // whose broker binds host loopback — which is NOT the guest's own loopback.
        // The guest bridge dials `host.microsandbox.internal:$VARDA_MCP_PORT`, so
        // advertise that name as the connect host (the port stays the real bound
        // ephemeral port from `VARDA_MCP_ADDR`).
        out.insert("VARDA_MCP_HOST".to_owned(), GUEST_BROKER_HOST.to_owned());
    }
    out
}

fn expand_env_value(value: &str, request: &AgentRunRequest) -> String {
    let expanded = expand_request_value(value, request);
    expand_leading_tilde(&expanded)
}

fn expand_leading_tilde(value: &str) -> String {
    if value == "~" {
        return std::env::var("HOME").unwrap_or_else(|_| value.to_owned());
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}/{rest}");
    }
    value.to_owned()
}

fn find_claude_transcript(
    session_root: &Path,
    project: &str,
    varda_session_id: &str,
    started_at: SystemTime,
) -> Option<PathBuf> {
    let project_dir = claude_project_dir(session_root, project)?;
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

    matches.sort_by_key(|item| std::cmp::Reverse(item.0));
    matches.into_iter().map(|(_, path)| path).next()
}

fn claude_project_dir(session_root: &Path, project: &str) -> Option<PathBuf> {
    let slug = project.replace('/', "-");
    Some(session_root.join(".claude/projects").join(slug))
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

    /// A sandbox session that records whether `teardown()` ran, so tests can
    /// assert the M10 leak fix fires on the cancel path without spinning docker.
    struct RecordingSession {
        torn_down: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl SandboxSession for RecordingSession {
        fn wrap(&self, spec: CommandSpec, _mode: LaunchMode) -> Result<CommandSpec> {
            Ok(spec)
        }
        fn session_store_root(&self) -> Option<PathBuf> {
            None
        }
        async fn teardown(self: Box<Self>) -> Result<()> {
            self.torn_down
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    /// The guard's normal-exit path awaits teardown inline exactly once.
    #[tokio::test]
    async fn teardown_guard_runs_teardown_on_normal_exit() {
        let torn_down = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = SessionTeardownGuard::new(Box::new(RecordingSession {
            torn_down: torn_down.clone(),
        }));
        guard.teardown().await.expect("teardown should succeed");
        assert!(
            torn_down.load(std::sync::atomic::Ordering::SeqCst),
            "normal exit must tear the sandbox session down"
        );
    }

    /// LEAK FIX: when the future holding the guard is cancelled (dropped) before
    /// it can call `teardown()` inline — exactly what the idle/budget watchdog
    /// does — the guard's `Drop` still reclaims the sandbox. No `varda-sbx-*`
    /// container/volume is left behind on an idle-kill.
    #[tokio::test]
    async fn teardown_guard_runs_teardown_on_cancel() {
        let torn_down = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let torn_for_fut = torn_down.clone();
        let run = async move {
            let _guard = SessionTeardownGuard::new(Box::new(RecordingSession {
                torn_down: torn_for_fut,
            }));
            // Model an in-flight sandboxed run that never resolves; the watchdog
            // cancels it by dropping this future, so the inline teardown below is
            // never reached — only `Drop` can save us.
            std::future::pending::<()>().await;
            drop(_guard);
        };

        // Cancel the run the way `run_session_watched` does: the losing branch of
        // a `select!` is dropped in place.
        tokio::select! {
            _ = run => unreachable!("the pending run cannot complete"),
            _ = time::sleep(Duration::from_millis(20)) => {}
        }

        // Drop detaches teardown onto the runtime; yield so it gets polled.
        for _ in 0..10 {
            if torn_down.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            time::sleep(Duration::from_millis(10)).await;
        }
        assert!(
            torn_down.load(std::sync::atomic::Ordering::SeqCst),
            "a cancelled sandboxed run must still tear the session down (no varda-sbx-* leak)"
        );
    }

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
            auth_token_env: None,
            auth_token_target: None,
            credentials: Vec::new(),
            streams_output: None,
            resume_command_template: None,
            interpreter_agent: None,
        };
        let client = AcpSubprocessClient::new("echo", &config);

        let result = client
            .run_task(AgentRunRequest {
                agent_name: "echo".to_owned(),
                role_instructions: None,
                task_path: "task.md".to_owned(),
                frontmatter: TaskFrontmatter {
                    bounds: crate::task::TaskBounds::default(),
                    id: None,
                    status: TaskStatus::Ready,
                    project: Some("/work/project".to_owned()),
                    assignee: Some("echo".to_owned()),
                    sandbox: None,
                    recap: None,
                    recaps: vec![],
                    plan: None,
                    agent_session_id: None,
                    agent_session_log: None,
                    agent_session_ids: vec![],
                    agent_session_logs: vec![],
                    agent_resume_commands: vec![],
                    allow_commands: vec![],
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
                orchestration_socket_path: None,
                orchestration_addr: None,
            })
            .await
            .expect("subprocess should echo prompt");

        assert!(result.recap.contains("You have at most 10 minutes"));
        assert!(result.recap.contains("Do it."));
        assert!(!result.requires_user);
    }

    fn docker_request(project: &str, session_id: &str) -> AgentRunRequest {
        AgentRunRequest {
            agent_name: "shell".to_owned(),
            role_instructions: None,
            task_path: "task.md".to_owned(),
            frontmatter: TaskFrontmatter {
                bounds: crate::task::TaskBounds::default(),
                id: None,
                status: TaskStatus::Ready,
                project: Some(project.to_owned()),
                assignee: Some("shell".to_owned()),
                sandbox: None,
                recap: None,
                recaps: vec![],
                plan: None,
                agent_session_id: None,
                agent_session_log: None,
                agent_session_ids: vec![],
                agent_session_logs: vec![],
                agent_resume_commands: vec![],
                allow_commands: vec![],
                requires_user: false,
            },
            body: "# Task\n\nDo it.".to_owned(),
            timeout: Duration::from_secs(600),
            session_id: session_id.to_owned(),
            session_log_path: None,
            interactive: false,
            interpret: false,
            stream: false,
            resume_command: None,
            orchestration_socket_path: None,
            orchestration_addr: None,
        }
    }

    fn docker_client(command: &str, args: &[&str]) -> AcpSubprocessClient {
        docker_client_cfg(command, args, vec![], vec![])
    }

    fn docker_client_cfg(
        command: &str,
        args: &[&str],
        mounts: Vec<String>,
        egress: Vec<String>,
    ) -> AcpSubprocessClient {
        let config = AgentConfig {
            kind: crate::config::AgentKind::Acp,
            command: command.to_owned(),
            args: args.iter().map(|a| a.to_string()).collect(),
            max_prompt_tokens: None,
            working_dir: None,
            env: BTreeMap::new(),
            interactive_command: None,
            interactive_args: None,
            auth_token_env: None,
            auth_token_target: None,
            credentials: Vec::new(),
            streams_output: None,
            resume_command_template: None,
            interpreter_agent: None,
        };
        let sandbox_config = crate::config::SandboxConfig {
            image: Some("busybox:latest".to_owned()),
            mounts,
            egress,
            ..Default::default()
        };
        let merged = crate::sandbox::merge_mount_origins(&sandbox_config.mounts, &[], &[]);
        let provider = std::sync::Arc::new(
            crate::sandbox::DockerProvider::from_config("docker", &sandbox_config, merged)
                .expect("docker provider"),
        );
        AcpSubprocessClient::with_sandbox("shell", &config, provider)
    }

    #[test]
    fn env_for_request_merges_static_env_after_agent_env_and_expands_values() {
        let mut agent_env = BTreeMap::new();
        agent_env.insert("SHARED".to_owned(), "agent".to_owned());
        agent_env.insert("AGENT_ONLY".to_owned(), "{project}/agent".to_owned());
        let mut static_env = BTreeMap::new();
        static_env.insert("SHARED".to_owned(), "static".to_owned());
        static_env.insert("GCLOUD_PROJECT".to_owned(), "healthy-silo-31898".to_owned());
        static_env.insert("PROJECT_PATH".to_owned(), "{project}".to_owned());
        static_env.insert("CACHE_DIR".to_owned(), "~/cache".to_owned());

        let env = env_for_request(
            &agent_env,
            &static_env,
            &docker_request("/srv/app", "env-unit"),
        );

        assert_eq!(env["SHARED"], "static");
        assert_eq!(env["AGENT_ONLY"], "/srv/app/agent");
        assert_eq!(env["GCLOUD_PROJECT"], "healthy-silo-31898");
        assert_eq!(env["PROJECT_PATH"], "/srv/app");
        if let Ok(home) = std::env::var("HOME") {
            assert_eq!(env["CACHE_DIR"], format!("{home}/cache"));
        }
    }

    #[test]
    fn env_for_request_exports_socket_for_interactive_and_omits_it_without_socket() {
        // 461d: the interactive resident is env-wired exactly like a batch orchestrated
        // run — an `orchestration_socket_path` (regardless of `interactive`) exports
        // `VARDA_MCP_SOCKET`; a request without one carries no such key (non-regression).
        let mut interactive = docker_request("/srv/app", "interactive-broker");
        interactive.interactive = true;
        interactive.orchestration_socket_path = Some("/srv/app/.varda-mcp/root.sock".to_owned());
        let with_socket = env_for_request(&BTreeMap::new(), &BTreeMap::new(), &interactive);
        assert_eq!(
            with_socket["VARDA_MCP_SOCKET"],
            "/srv/app/.varda-mcp/root.sock"
        );

        let mut plain = docker_request("/srv/app", "no-broker");
        plain.interactive = true;
        assert_eq!(plain.orchestration_socket_path, None);
        let without_socket = env_for_request(&BTreeMap::new(), &BTreeMap::new(), &plain);
        assert!(!without_socket.contains_key("VARDA_MCP_SOCKET"));
    }

    #[test]
    fn env_for_request_exports_tcp_addr_for_vm_backed_broker() {
        // #541: an own-kernel microVM guest gets the TCP broker address as
        // `VARDA_MCP_ADDR` (host:port) plus `VARDA_MCP_PORT` (the port alone), and
        // no `VARDA_MCP_SOCKET` (the two transports are mutually exclusive).
        let mut vm = docker_request("/srv/app", "vm-broker");
        vm.interactive = true;
        vm.orchestration_addr = Some("172.16.0.177:54321".to_owned());
        let env = env_for_request(&BTreeMap::new(), &BTreeMap::new(), &vm);
        assert_eq!(env["VARDA_MCP_ADDR"], "172.16.0.177:54321");
        assert_eq!(env["VARDA_MCP_PORT"], "54321");
        assert!(!env.contains_key("VARDA_MCP_SOCKET"));
    }

    #[test]
    fn env_for_request_advertises_host_microsandbox_internal_for_vm_broker() {
        // #546 last-mile: the guest connect HOST must be `host.microsandbox.internal`
        // (which resolves to the HOST from inside the guest), NOT a loopback host —
        // the broker binds host loopback, but the guest's own loopback is not the
        // host. The port stays the real bound ephemeral port from `VARDA_MCP_ADDR`.
        let mut vm = docker_request("/srv/app", "vm-broker-host");
        vm.orchestration_addr = Some("127.0.0.1:54321".to_owned());
        let env = env_for_request(&BTreeMap::new(), &BTreeMap::new(), &vm);
        assert_eq!(env["VARDA_MCP_HOST"], "host.microsandbox.internal");
        assert_eq!(env["VARDA_MCP_PORT"], "54321");
        assert_ne!(env["VARDA_MCP_HOST"], "127.0.0.1");

        // A `local`/`docker` guest (socket transport) gets no `VARDA_MCP_HOST`.
        let mut sock = docker_request("/srv/app", "sock-broker");
        sock.orchestration_socket_path = Some("/srv/app/.varda-mcp/root.sock".to_owned());
        let sock_env = env_for_request(&BTreeMap::new(), &BTreeMap::new(), &sock);
        assert!(!sock_env.contains_key("VARDA_MCP_HOST"));
    }

    /// M5: build a sandbox image from `testdata/Dockerfile.rust` (a trivial
    /// `FROM busybox` — installs nothing heavy) and run a shell agent under it,
    /// asserting the recap is parsed. Proves the `build` knob is honoured end to
    /// end. Requires a running docker daemon; run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a running docker daemon"]
    async fn docker_build_sandbox_returns_parsed_recap() {
        let dockerfile = concat!(env!("CARGO_MANIFEST_DIR"), "/testdata/Dockerfile.rust");
        let config = AgentConfig {
            kind: crate::config::AgentKind::Acp,
            command: "cat".to_owned(),
            args: vec![],
            max_prompt_tokens: None,
            working_dir: None,
            env: BTreeMap::new(),
            interactive_command: None,
            interactive_args: None,
            auth_token_env: None,
            auth_token_target: None,
            credentials: Vec::new(),
            streams_output: None,
            resume_command_template: None,
            interpreter_agent: None,
        };
        let provider = std::sync::Arc::new(
            crate::sandbox::DockerProvider::from_config(
                "built",
                &crate::config::SandboxConfig {
                    build: Some(dockerfile.to_owned()),
                    ..Default::default()
                },
                Vec::new(),
            )
            .expect("docker provider from build config"),
        );
        let client = AcpSubprocessClient::with_sandbox("shell", &config, provider);
        let result = client
            .run_task(docker_request("/tmp", "docker-build-recap"))
            .await
            .expect("built docker agent should return a recap");
        assert!(result.recap.contains("Do it."));
        assert!(!result.requires_user);
    }

    /// Integration: a trivial shell agent under `sandbox="docker"` returns a
    /// parsed recap. Requires a running docker daemon and network to pull
    /// `busybox`. Run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a running docker daemon"]
    async fn docker_agent_returns_parsed_recap() {
        // `cat` echoes the prompt (delivered on stdin) straight back as the recap.
        let client = docker_client("cat", &[]);
        let result = client
            .run_task(docker_request("/tmp", "docker-recap"))
            .await
            .expect("docker agent should return a recap");
        assert!(result.recap.contains("Do it."));
        assert!(!result.requires_user);
    }

    /// Security assertion (the point of M1, preserved through M3): the agent
    /// container mounts only the project and a dedicated per-session HOME — never
    /// the host's real `$HOME` — so it CANNOT read the host's `~/.aws`. M3 sets
    /// the container `HOME` to the session store, so `$HOME/.aws` would only test
    /// that empty dir; probe the host's *absolute* `~/.aws` path instead to prove
    /// the real credential dir is not mounted. Run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a running docker daemon"]
    async fn docker_agent_cannot_read_host_aws_credentials() {
        // Absolute host credential path; must not be visible from inside.
        let host_aws = host_session_root().join(".aws");
        let host_aws = host_aws.display();
        // Probe for the host ~/.aws inside the container; print a sentinel + recap.
        let probe = format!(
            "if [ -e \"{host_aws}\" ]; then echo AWS_VISIBLE; else echo AWS_HIDDEN; fi; \
             echo; echo 'requires_user: false'"
        );
        let client = docker_client("sh", &["-c", probe.as_str()]);
        let result = client
            .run_task(docker_request("/tmp", "docker-aws"))
            .await
            .expect("docker agent should run the probe");
        assert!(
            result.recap.contains("AWS_HIDDEN"),
            "host ~/.aws must not be visible inside the sandbox; recap was: {}",
            result.recap
        );
        assert!(!result.recap.contains("AWS_VISIBLE"));
    }

    /// M2 egress exit criterion: with no allow-list the container is fully
    /// offline, so a DNS lookup of any external host must fail. Run with
    /// `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a running docker daemon"]
    async fn docker_agent_default_deny_blocks_egress() {
        // busybox `nslookup` fails (non-zero / no answer) with --network none.
        let client = docker_client(
            "sh",
            &[
                "-c",
                "if nslookup example.com >/dev/null 2>&1; then echo NET_OPEN; else echo NET_BLOCKED; fi; \
                 echo; echo 'requires_user: false'",
            ],
        );
        let result = client
            .run_task(docker_request("/tmp", "docker-egress-deny"))
            .await
            .expect("docker agent should run the egress probe");
        assert!(
            result.recap.contains("NET_BLOCKED"),
            "default-deny must block egress; recap was: {}",
            result.recap
        );
        assert!(!result.recap.contains("NET_OPEN"));
    }

    /// M2 egress exit criterion: an allow-listed host resolves (pinned) while a
    /// non-allow-listed host does not. Run with `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a running docker daemon and outbound DNS on the host"]
    async fn docker_agent_egress_allow_list_pins_only_listed_hosts() {
        let client = docker_client_cfg(
            "sh",
            &[
                "-c",
                // The allow-listed host is pinned into /etc/hosts via `--add-host`;
                // the other is absent because ambient DNS is disabled and it was
                // never pinned. We assert on the pin directly (grep is a busybox
                // builtin; `getent` is not, and `nslookup` bypasses /etc/hosts).
                "if grep -qw example.com /etc/hosts; then echo ALLOWED_OK; else echo ALLOWED_FAIL; fi; \
                 if grep -qw blocked.invalid /etc/hosts; then echo BLOCKED_OPEN; else echo BLOCKED_DENIED; fi; \
                 echo; echo 'requires_user: false'",
            ],
            vec![],
            vec!["example.com".to_owned()],
        );
        let result = client
            .run_task(docker_request("/tmp", "docker-egress-allow"))
            .await
            .expect("docker agent should run the allow-list probe");
        assert!(
            result.recap.contains("ALLOWED_OK"),
            "allow-listed host must be reachable; recap was: {}",
            result.recap
        );
        assert!(
            result.recap.contains("BLOCKED_DENIED"),
            "non-allow-listed host must be unreachable; recap was: {}",
            result.recap
        );
    }

    /// M15 static-env exit criterion: a static env map merged before wrapping is
    /// visible in the guest as a regular container env var. Run with
    /// `cargo test -- --ignored`.
    #[tokio::test]
    #[ignore = "requires a running docker daemon"]
    async fn docker_agent_static_env_visible_in_guest() {
        let config = AgentConfig {
            kind: crate::config::AgentKind::Acp,
            command: "sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "printf 'GCLOUD_PROJECT=%s\\n\\nrequires_user: false\\n' \"$GCLOUD_PROJECT\""
                    .to_owned(),
            ],
            max_prompt_tokens: None,
            working_dir: None,
            env: BTreeMap::new(),
            interactive_command: None,
            interactive_args: None,
            auth_token_env: None,
            auth_token_target: None,
            credentials: Vec::new(),
            streams_output: None,
            resume_command_template: None,
            interpreter_agent: None,
        };
        let sandbox_config = crate::config::SandboxConfig {
            image: Some("busybox:latest".to_owned()),
            ..Default::default()
        };
        let provider = std::sync::Arc::new(
            crate::sandbox::DockerProvider::from_config("docker", &sandbox_config, Vec::new())
                .expect("docker provider"),
        );
        let client = AcpSubprocessClient::with_sandbox_env(
            "shell",
            &config,
            provider,
            BTreeMap::from([("GCLOUD_PROJECT".to_owned(), "healthy-silo-31898".to_owned())]),
        );
        let result = client
            .run_task(docker_request("/tmp", "docker-static-env"))
            .await
            .expect("docker agent should receive static env");
        assert!(result.recap.contains("GCLOUD_PROJECT=healthy-silo-31898"));
    }

    /// Build a client whose interactive launch runs a REAL coding agent inside the
    /// sandbox, reading the task from the staged `VARDA_PROMPT_FILE` — exactly the
    /// M13b default-config shape (`sh -c '<agent> "$(cat $VARDA_PROMPT_FILE)" …'`).
    /// `command` stays the bare agent name so external-session capture (M13a) keys
    /// off the right transcript store. Caller supplies an image that has the agent
    /// installed + authenticated via injected identity.
    #[cfg(test)]
    fn interactive_agent_client(
        command: &str,
        interactive_shell: &str,
        image: &str,
    ) -> AcpSubprocessClient {
        let config = AgentConfig {
            kind: crate::config::AgentKind::Acp,
            command: command.to_owned(),
            args: vec![],
            max_prompt_tokens: None,
            working_dir: None,
            env: BTreeMap::new(),
            interactive_command: Some("sh".to_owned()),
            interactive_args: Some(vec!["-c".to_owned(), interactive_shell.to_owned()]),
            auth_token_env: None,
            auth_token_target: None,
            credentials: Vec::new(),
            streams_output: None,
            resume_command_template: None,
            interpreter_agent: None,
        };
        let sandbox_config = crate::config::SandboxConfig {
            image: Some(image.to_owned()),
            ..Default::default()
        };
        let merged = crate::sandbox::merge_mount_origins(&sandbox_config.mounts, &[], &[]);
        let provider = std::sync::Arc::new(
            crate::sandbox::DockerProvider::from_config("docker", &sandbox_config, merged)
                .expect("docker provider"),
        );
        AcpSubprocessClient::with_sandbox(command, &config, provider)
    }

    /// M13b live smoke — a REAL interactive agent session inside docker. These are
    /// `#[ignore]`d because they need (1) a real TTY on stdin — `cargo test`
    /// provides none, so run them from a terminal: `cargo test -- --ignored
    /// --nocapture`; (2) a running docker daemon; and (3) an image with the agent
    /// installed and authenticated via the injected identity (scoped token env /
    /// staged file), NOT a creds-dir mount. Each drives the sandboxed interactive
    /// launch through `run_task(interactive=true)`, attaches the user's TTY, and
    /// asserts the session completes and teardown leaves no `varda-sbx-*` container.
    async fn interactive_agent_smoke(command: &str, interactive_shell: &str, session: &str) {
        use std::io::IsTerminal as _;
        if !std::io::stdin().is_terminal() {
            eprintln!(
                "skipping {command} interactive smoke: no TTY on stdin (run from a terminal)"
            );
            return;
        }
        let client = interactive_agent_client(command, interactive_shell, "varda-agent:latest");
        let mut request = docker_request(".", session);
        request.interactive = true;
        let result = client
            .run_task(request)
            .await
            .unwrap_or_else(|e| panic!("{command} interactive session should complete: {e:#}"));
        assert!(!result.requires_user);
    }

    #[tokio::test]
    #[ignore = "live: real claude interactive under docker; needs a TTY + auth image"]
    async fn claude_interactive_under_sandbox_smoke() {
        interactive_agent_smoke(
            "claude",
            "claude \"$(cat $VARDA_PROMPT_FILE)\" --add-dir {project} --permission-mode acceptEdits",
            "m13b-claude-smoke",
        )
        .await;
    }

    #[tokio::test]
    #[ignore = "live: real codex interactive under docker; needs a TTY + auth image"]
    async fn codex_interactive_under_sandbox_smoke() {
        interactive_agent_smoke(
            "codex",
            "codex \"$(cat $VARDA_PROMPT_FILE)\" -C {project} -s workspace-write",
            "m13b-codex-smoke",
        )
        .await;
    }

    #[tokio::test]
    #[ignore = "live: real copilot interactive under docker; needs a TTY + auth image"]
    async fn copilot_interactive_under_sandbox_smoke() {
        interactive_agent_smoke(
            "copilot",
            "copilot \"$(cat $VARDA_PROMPT_FILE)\" --allow-all-tools --add-dir {project}",
            "m13b-copilot-smoke",
        )
        .await;
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
            auth_token_env: None,
            auth_token_target: None,
            credentials: Vec::new(),
            streams_output: None,
            resume_command_template: None,
            interpreter_agent: None,
        };
        let client = AcpSubprocessClient::new("shell", &config);

        let result = client
            .run_task(AgentRunRequest {
                agent_name: "shell".to_owned(),
                role_instructions: None,
                task_path: "task.md".to_owned(),
                frontmatter: TaskFrontmatter {
                    bounds: crate::task::TaskBounds::default(),
                    id: None,
                    status: TaskStatus::Ready,
                    project: Some("/work/project".to_owned()),
                    assignee: Some("shell".to_owned()),
                    sandbox: None,
                    recap: None,
                    recaps: vec![],
                    plan: None,
                    agent_session_id: None,
                    agent_session_log: None,
                    agent_session_ids: vec![],
                    agent_session_logs: vec![],
                    agent_resume_commands: vec![],
                    allow_commands: vec![],
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
                orchestration_socket_path: None,
                orchestration_addr: None,
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
            auth_token_env: None,
            auth_token_target: None,
            credentials: Vec::new(),
            streams_output: None,
            resume_command_template: None,
            interpreter_agent: None,
        };
        let client = AcpSubprocessClient::new("shell", &config);

        let result = client
            .run_task(AgentRunRequest {
                agent_name: "shell".to_owned(),
                role_instructions: None,
                task_path: "task.md".to_owned(),
                frontmatter: TaskFrontmatter {
                    bounds: crate::task::TaskBounds::default(),
                    id: None,
                    status: TaskStatus::Ready,
                    project: Some("/work/project".to_owned()),
                    assignee: Some("shell".to_owned()),
                    sandbox: None,
                    recap: None,
                    recaps: vec![],
                    plan: None,
                    agent_session_id: None,
                    agent_session_log: None,
                    agent_session_ids: vec![],
                    agent_session_logs: vec![],
                    agent_resume_commands: vec![],
                    allow_commands: vec![],
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
                orchestration_socket_path: None,
                orchestration_addr: None,
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
            auth_token_env: None,
            auth_token_target: None,
            credentials: Vec::new(),
            streams_output: None,
            resume_command_template: None,
            interpreter_agent: None,
        };
        let client = AcpSubprocessClient::new("shell", &config);

        let result = client
            .run_task(AgentRunRequest {
                agent_name: "shell".to_owned(),
                role_instructions: None,
                task_path: "task.md".to_owned(),
                frontmatter: TaskFrontmatter {
                    bounds: crate::task::TaskBounds::default(),
                    id: None,
                    status: TaskStatus::Ready,
                    project: Some(root.display().to_string()),
                    assignee: Some("shell".to_owned()),
                    sandbox: None,
                    recap: None,
                    recaps: vec![],
                    plan: None,
                    agent_session_id: None,
                    agent_session_log: None,
                    agent_session_ids: vec![],
                    agent_session_logs: vec![],
                    agent_resume_commands: vec![],
                    allow_commands: vec![],
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
                orchestration_socket_path: None,
                orchestration_addr: None,
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
                bounds: crate::task::TaskBounds::default(),
                id: None,
                status: TaskStatus::Ready,
                project: Some("/work/project".to_owned()),
                assignee: Some("codex".to_owned()),
                sandbox: None,
                recap: None,
                recaps: vec![],
                plan: None,
                agent_session_id: None,
                agent_session_log: None,
                agent_session_ids: vec![],
                agent_session_logs: vec![],
                agent_resume_commands: vec![],
                allow_commands: vec![],
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
            orchestration_socket_path: None,
            orchestration_addr: None,
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
                bounds: crate::task::TaskBounds::default(),
                id: None,
                status: TaskStatus::Ready,
                project: Some("/work/project".to_owned()),
                assignee: Some("claude".to_owned()),
                sandbox: None,
                recap: None,
                recaps: vec![],
                plan: None,
                agent_session_id: None,
                agent_session_log: None,
                agent_session_ids: vec![],
                agent_session_logs: vec![],
                agent_resume_commands: vec![],
                allow_commands: vec![],
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
            orchestration_socket_path: None,
            orchestration_addr: None,
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
            auth_token_env: None,
            auth_token_target: None,
            credentials: Vec::new(),
            streams_output: None,
            resume_command_template: Some(
                "claude --resume {external_session_id} --add-dir {project}".to_owned(),
            ),
            interpreter_agent: None,
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
            auth_token_env: None,
            auth_token_target: None,
            credentials: Vec::new(),
            streams_output: None,
            resume_command_template: None,
            interpreter_agent: None,
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
                bounds: crate::task::TaskBounds::default(),
                id: None,
                status: TaskStatus::Ready,
                project: Some(project.to_owned()),
                assignee: Some(agent.to_owned()),
                sandbox: None,
                recap: None,
                recaps: vec![],
                plan: None,
                agent_session_id: None,
                agent_session_log: None,
                agent_session_ids: vec![],
                agent_session_logs: vec![],
                agent_resume_commands: vec![],
                allow_commands: vec![],
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
            orchestration_socket_path: None,
            orchestration_addr: None,
        }
    }

    /// A non-`local` provider stub for the TTY-guard test. `prepare` is never
    /// reached — the guard fires first — so it just bails.
    struct FakeSandboxProvider;
    #[async_trait]
    impl SandboxProvider for FakeSandboxProvider {
        fn name(&self) -> &str {
            "docker"
        }
        async fn prepare(&self, _ctx: &SandboxContext<'_>) -> Result<Box<dyn SandboxSession>> {
            bail!("prepare should not be reached in the TTY-guard test")
        }
    }

    fn interactive_shell_config() -> AgentConfig {
        AgentConfig {
            kind: crate::config::AgentKind::Acp,
            command: "claude".to_owned(),
            args: vec![],
            max_prompt_tokens: None,
            working_dir: None,
            env: BTreeMap::new(),
            interactive_command: Some("sh".to_owned()),
            interactive_args: None,
            auth_token_env: None,
            auth_token_target: None,
            credentials: Vec::new(),
            streams_output: None,
            resume_command_template: None,
            interpreter_agent: None,
        }
    }

    /// M13a §4 TTY guard: a sandboxed interactive launch requires a real terminal
    /// on stdin (`docker -it`/`msb -t` fail otherwise). Under `cargo test` stdin is
    /// not a TTY, so the guard must bail with a clear message BEFORE preparing the
    /// box (the stub provider's `prepare` would otherwise panic).
    #[tokio::test]
    async fn sandboxed_interactive_requires_a_tty() {
        let config = interactive_shell_config();
        let client =
            AcpSubprocessClient::with_sandbox("sh", &config, Arc::new(FakeSandboxProvider));
        let request = sample_request("sh", "/work/project");
        let err = client
            .execute_interactive_sandboxed(&request, "sh", "prompt text", None, BTreeMap::new())
            .await
            .expect_err("must refuse to launch without a TTY");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("needs a terminal on stdin"),
            "unexpected error: {msg}"
        );
    }
}
