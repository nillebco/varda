//! End-to-end task execution flow.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::time;
use uuid::Uuid;

use crate::agent::{
    AgentClient, AgentRunRequest, AgentRunResult, parse_blocked_commands, parse_files_touched,
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
    /// M12: commands the agent reported under the `Blocked commands` heading of
    /// its recap — denied by its permission layer because they were not on the
    /// task's `allow_commands`. Surfaced so an orchestrator can widen the
    /// allowlist and re-run without an interactive approver.
    pub blocked_commands: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanOutcome {
    pub plan_path: PathBuf,
}

/// Run `fut` under the resolved execution ceiling. `Some(secs)` enforces a hard
/// `time::timeout`; `None` (`max_seconds = "none"`, or `timeout_seconds = 0`)
/// runs it unbounded. `Err(Elapsed)` is returned only when a finite ceiling is
/// hit, so callers keep treating that as the timeout path.
///
/// M10 increment: this makes `defaults.effective_max_seconds()` actually honored
/// so a run can opt out of the hard kill. The full cooperative model (idle
/// watchdog, auto-resume loop, operation budget) builds on top of this seam.
async fn run_under_ceiling<F, T>(
    max_seconds: Option<u64>,
    fut: F,
) -> Result<T, tokio::time::error::Elapsed>
where
    F: std::future::Future<Output = T>,
{
    match max_seconds {
        Some(secs) => time::timeout(Duration::from_secs(secs), fut).await,
        None => Ok(fut.await),
    }
}

/// How often the idle watchdog samples the session log for growth. Small enough
/// that sub-second-precision tests observe a cancel promptly; the sampled work
/// is a single `stat`, so the cadence is cheap.
const IDLE_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Why a watched session stopped early (i.e. before the agent future resolved).
/// Both variants cancel the in-flight session, but they carry different intent:
/// an idle stall is a wedged/hung child (hard cancel), while a budget stop is a
/// graceful checkpoint against the soft total ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionKill {
    /// The session produced no output for `idle_secs` seconds — the M10 idle
    /// watchdog fired. This is what catches `PendingAgentClient`-style hangs
    /// cheaply, where the old duration kill would have waited out the full ceiling.
    Idle { idle_secs: u64 },
    /// The soft total ceiling (`effective_max_seconds`) was reached. A graceful
    /// checkpoint: the loop stops and the task is marked `needs_user`, never killed
    /// mid-edit.
    Budget { max_secs: u64 },
}

/// Run an agent session `fut` under the M10 idle watchdog.
///
/// The agent's acp streaming path appends every stdout/stderr chunk to the
/// session log, so a growing log is a direct proxy for a productive run. This
/// polls the log's size on [`IDLE_POLL_INTERVAL`]; a run is cancelled ONLY after
/// `idle_timeout` of no growth — a productive long run never trips it, a wedged
/// loop/hung process does. `max_seconds` layers the soft total ceiling on top:
/// on reaching it the session stops with [`SessionKill::Budget`] (a graceful
/// checkpoint), distinct from the idle stall.
async fn run_session_watched<F>(
    idle_timeout: Duration,
    max_seconds: Option<u64>,
    log_path: &Path,
    fut: F,
) -> Result<Result<AgentRunResult>, SessionKill>
where
    F: std::future::Future<Output = Result<AgentRunResult>>,
{
    tokio::pin!(fut);

    let log_size = || fs::metadata(log_path).map(|m| m.len()).unwrap_or(0);
    let deadline = max_seconds.map(|secs| Instant::now() + Duration::from_secs(secs));
    let mut last_size = log_size();
    let mut last_activity = Instant::now();
    let mut ticker = time::interval(IDLE_POLL_INTERVAL);
    // The first tick fires immediately; consume it so the loop paces on real waits.
    ticker.tick().await;

    loop {
        tokio::select! {
            output = &mut fut => return Ok(output),
            _ = ticker.tick() => {
                let now = Instant::now();
                let size = log_size();
                if size != last_size {
                    last_size = size;
                    last_activity = now;
                }
                if let Some(deadline) = deadline
                    && now >= deadline {
                        return Err(SessionKill::Budget {
                            max_secs: max_seconds.unwrap_or_default(),
                        });
                    }
                if now.duration_since(last_activity) >= idle_timeout {
                    return Err(SessionKill::Idle {
                        idle_secs: idle_timeout.as_secs(),
                    });
                }
            }
        }
    }
}

/// The M10 cooperative bounds resolved once per run so the idle watchdog, the
/// soft total ceiling, the auto-resume loop, and the tool-call budget share a
/// single source of truth.
///
/// Precedence: a per-task frontmatter override wins over the corresponding
/// `defaults.*` value; an unset override falls back to the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OperationBounds {
    idle_timeout: Duration,
    max_seconds: Option<u64>,
    /// Max auto-resume hops (fresh continuation sessions) before stopping with
    /// `needs_user`. `0` disables auto-resume (a single session only).
    max_continuations: u32,
    /// Tool-call budget across the whole task. `0` = unlimited.
    max_tool_calls: u64,
}

impl OperationBounds {
    /// Resolve the bounds for this run, letting the task frontmatter override any
    /// of the four defaults. `frontmatter` is the task being run.
    fn resolve(config: &Config, frontmatter: &crate::task::TaskFrontmatter) -> Self {
        let bounds = &frontmatter.bounds;
        let idle_timeout = Duration::from_secs(
            bounds
                .idle_timeout
                .unwrap_or(config.defaults.idle_timeout_seconds),
        );
        let max_seconds = match &bounds.max_seconds {
            Some(over) => crate::config::effective_max_seconds(over, config.defaults.timeout_seconds),
            None => config.defaults.effective_max_seconds(),
        };
        Self {
            idle_timeout,
            max_seconds,
            max_continuations: bounds
                .max_continuations
                .unwrap_or(config.defaults.max_continuations),
            max_tool_calls: bounds.max_tool_calls.unwrap_or(config.defaults.max_tool_calls),
        }
    }
}

/// Normalize one watched session's raw result into the run's `session_outcome`:
/// `Ok(result)` on a natural end (carrying any resume command), or `Err(recap)`
/// for the idle-kill / budget-stop / agent-error terminal paths. Extracted so
/// the interactive single-shot and the headless auto-resume loop share the exact
/// same recap wording and downstream status semantics.
fn single_session_outcome(
    agent_result: Result<Result<AgentRunResult>, SessionKill>,
    session_id: &str,
    session_log_path: &Path,
    task_path: &Path,
) -> Result<Result<AgentRunResult, AgentRunResult>> {
    Ok(match agent_result {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(error)) => {
            append_session_log(session_log_path, &format!("\nerror:\n{error:#}\n"))?;
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
        Err(SessionKill::Idle { idle_secs }) => {
            append_session_log(
                session_log_path,
                &format!(
                    "\nidle_watchdog:\nno output for {idle_secs} seconds; session cancelled\nlong_running_task_requested=true\n"
                ),
            )?;
            Err(AgentRunResult {
                recap: format!(
                    "# Agent Run Timed Out\n\nThe agent produced no output for {idle_secs} seconds while processing `{}`, so Varda's idle watchdog cancelled the wedged session.\n\nWhat completed: the session log was preserved for inspection.\n\nWhat remains: delegate the unfinished work to a Varda long-running runner task, then resume the agent after the complete runner output is available.\n\nBlockers: the session stalled with no output.\n\nUser interaction required: no.\n\nSuggested next agent: runner.\n\nSession ID: `{session_id}`\n\nSession log: [{}]({})",
                    task_path.display(),
                    session_log_path.display(),
                    session_log_path.display()
                ),
                requires_user: false,
                suggested_agent: Some("runner".to_owned()),
                resume_command: None,
            })
        }
        Err(SessionKill::Budget { max_secs }) => {
            append_session_log(
                session_log_path,
                &format!(
                    "\nbudget:\nsoft ceiling of {max_secs} seconds reached; stopping gracefully for user review\n"
                ),
            )?;
            // A graceful checkpoint, NOT a failure: mark the task needs_user with the
            // accumulated recap rather than killing it mid-work.
            Err(AgentRunResult {
                recap: format!(
                    "# Operation Budget Reached\n\nThe agent reached the configured {max_secs} second soft ceiling while processing `{}`. Varda stopped the run gracefully so no edit was interrupted mid-flight.\n\nWhat remains: review the partial progress in the session log and re-run to continue.\n\nUser interaction required: yes.\n\nSession ID: `{session_id}`\n\nSession log: [{}]({})",
                    task_path.display(),
                    session_log_path.display(),
                    session_log_path.display()
                ),
                requires_user: true,
                suggested_agent: None,
                resume_command: None,
            })
        }
    })
}

/// Join accumulated hop recaps in order, then the final hop's recap last.
fn combine_recaps(prior: &[String], last: &str) -> String {
    let mut parts: Vec<&str> = prior.iter().map(String::as_str).collect();
    parts.push(last);
    parts.join("\n\n---\n\n")
}

/// M10 multi-hop auto-resume loop (headless runs only).
///
/// Runs the first (already-built) watched session, then — while the agent hands
/// back a resume command AND does not ask for the user AND the `max_continuations`
/// ceiling is not yet reached — dispatches a FRESH continuation session seeded
/// with that command, looping until the agent reports done or a bound stops it.
///
/// Semantics preserved for the single-hop case: with no resume command the very
/// first session's outcome is returned verbatim, so pre-M10 behavior (and every
/// existing test) is unchanged. Recaps from each hop are preserved in order;
/// hitting `max_continuations` with work still remaining stops gracefully with
/// `needs_user` rather than silently dropping the tail.
#[allow(clippy::too_many_arguments)]
async fn run_auto_resume_loop(
    config: &Config,
    client: &impl AgentClient,
    agent_name: &str,
    role_instructions: Option<&str>,
    task_path: &Path,
    task: &mut TaskDocument,
    bounds: OperationBounds,
    timeout: Duration,
    stream: bool,
    first_session_id: String,
    first_session_log: PathBuf,
    first_request: AgentRunRequest,
) -> Result<Result<AgentRunResult, AgentRunResult>> {
    let mut prior_recaps: Vec<String> = Vec::new();
    let mut sid = first_session_id;
    let mut log_path = first_session_log;
    let mut request = first_request;
    let mut hop: u32 = 0;

    loop {
        let agent_result = run_session_watched(
            bounds.idle_timeout,
            bounds.max_seconds,
            &log_path,
            client.run_task(request),
        )
        .await;

        match single_session_outcome(agent_result, &sid, &log_path, task_path)? {
            Ok(result) => {
                let more_work = result.resume_command.is_some() && !result.requires_user;
                if more_work && hop < bounds.max_continuations {
                    // Continue: dispatch a FRESH continuation seeded with the
                    // captured resume command; preserve this hop's recap in order.
                    let resume = result
                        .resume_command
                        .clone()
                        .expect("more_work implies a resume command");
                    prior_recaps.push(result.recap);
                    hop += 1;
                    sid = Uuid::new_v4().to_string();
                    log_path = session_log_path(config, &sid);
                    task.frontmatter.agent_session_ids.push(sid.clone());
                    task.frontmatter
                        .agent_session_logs
                        .push(log_path.display().to_string());
                    write_task(task)?;
                    write_session_log(
                        &log_path,
                        &format!(
                            "session_id={sid}\nagent={agent_name}\ntask={}\nresume_command={resume}\n[auto_resume hop={hop}/{}]\n",
                            task_path.display(),
                            bounds.max_continuations,
                        ),
                    )?;
                    request = AgentRunRequest {
                        agent_name: agent_name.to_owned(),
                        role_instructions: role_instructions.map(str::to_owned),
                        task_path: task_path.display().to_string(),
                        frontmatter: task.frontmatter.clone(),
                        body: String::new(),
                        timeout,
                        session_id: sid.clone(),
                        session_log_path: Some(log_path.display().to_string()),
                        interactive: false,
                        interpret: false,
                        stream,
                        resume_command: Some(resume),
                    };
                    continue;
                }

                if more_work {
                    // Hit the max_continuations ceiling with work still remaining:
                    // stop gracefully for the user rather than dropping the tail.
                    append_session_log(
                        &log_path,
                        &format!(
                            "\nauto_resume:\nreached max_continuations={} with work remaining; stopping for user review\n",
                            bounds.max_continuations
                        ),
                    )?;
                    let combined = combine_recaps(&prior_recaps, &result.recap);
                    return Ok(Err(AgentRunResult {
                        recap: format!(
                            "# Auto-Resume Limit Reached\n\nVarda stitched {} continuation session(s) for `{}` but the agent still reported more work after the max_continuations={} ceiling. Stopping for user review.\n\nUser interaction required: yes.\n\n## Accumulated recaps\n\n{combined}",
                            hop + 1,
                            task_path.display(),
                            bounds.max_continuations,
                        ),
                        requires_user: true,
                        suggested_agent: result.suggested_agent,
                        resume_command: result.resume_command,
                    }));
                }

                // Done. Single-hop returns verbatim (pre-M10 behavior); a
                // multi-hop run stitches every hop's recap in order.
                if prior_recaps.is_empty() {
                    return Ok(Ok(result));
                }
                let combined = combine_recaps(&prior_recaps, &result.recap);
                return Ok(Ok(AgentRunResult {
                    recap: combined,
                    requires_user: result.requires_user,
                    suggested_agent: result.suggested_agent,
                    resume_command: None,
                }));
            }
            Err(failure) => {
                // Terminal kill/error. Single-hop returns verbatim so the status
                // mapping is unchanged; a multi-hop run keeps the failure recap's
                // marker PREFIX (so Failed/needs_user still resolves) and appends
                // the earlier hops for context.
                if prior_recaps.is_empty() {
                    return Ok(Err(failure));
                }
                let earlier = prior_recaps.join("\n\n---\n\n");
                let recap =
                    format!("{}\n\n## Earlier continuation recaps\n\n{earlier}", failure.recap);
                return Ok(Err(AgentRunResult { recap, ..failure }));
            }
        }
    }
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

    // Hint reflects the effective ceiling (not the legacy 600s) so the agent
    // plans against its real budget instead of self-scoping early.
    let timeout = Duration::from_secs(
        config
            .defaults
            .effective_max_seconds()
            .unwrap_or(config.defaults.timeout_seconds),
    );
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

    // M10 cooperative model: an interactive run stays fully user-driven (no
    // watchdog); a headless run is guarded by the idle watchdog + soft ceiling
    // and may stitch multiple auto-resume continuations into one task.
    let bounds = OperationBounds::resolve(config, &task.frontmatter);
    let (session_outcome, _interactive_finalization_guard) = if interactive {
        let agent_result = Ok(client.run_task(request).await);
        let guard = InteractiveFinalizationGuard::activate()?;
        (
            single_session_outcome(agent_result, &session_id, &session_log_path, task_path)?,
            Some(guard),
        )
    } else {
        let outcome = run_auto_resume_loop(
            config,
            client,
            agent_name,
            role_instructions,
            task_path,
            &mut task,
            bounds,
            timeout,
            stream,
            session_id.clone(),
            session_log_path.clone(),
            request,
        )
        .await?;
        (outcome, None)
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
    let blocked_commands = parse_blocked_commands(&result.recap);
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
        blocked_commands,
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

    // Hint reflects the effective ceiling (not the legacy 600s) so the agent
    // plans against its real budget instead of self-scoping early.
    let timeout = Duration::from_secs(
        config
            .defaults
            .effective_max_seconds()
            .unwrap_or(config.defaults.timeout_seconds),
    );
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
    let blocked_commands = parse_blocked_commands(&result.recap);
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
        blocked_commands,
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

    // Hint reflects the effective ceiling (not the legacy 600s) so the agent
    // plans against its real budget instead of self-scoping early.
    let timeout = Duration::from_secs(
        config
            .defaults
            .effective_max_seconds()
            .unwrap_or(config.defaults.timeout_seconds),
    );
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

    let result = match run_under_ceiling(
        config.defaults.effective_max_seconds(),
        client.plan_task(request),
    )
    .await
    {
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

    /// M10: `None` ceiling (`max_seconds = "none"`) runs unbounded — no hard kill;
    /// a finite ceiling that elapses returns `Err`, the timeout path.
    #[tokio::test]
    async fn run_under_ceiling_none_is_unbounded_and_finite_elapses() {
        // No ceiling: the future runs to completion even though it out-waits any
        // small limit; returns Ok.
        let out = super::run_under_ceiling(None, async {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            7u32
        })
        .await;
        assert_eq!(out, Ok(7));

        // Finite ceiling that elapses before the future completes: Err (timeout).
        let elapsed = super::run_under_ceiling(Some(0), async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            7u32
        })
        .await;
        assert!(
            elapsed.is_err(),
            "a 0s ceiling must elapse to the timeout path"
        );
    }

    use crate::agent::fake::FakeAgentClient;
    use crate::agent::{AgentRunRequest, AgentRunResult};
    use crate::config::{AgentConfig, AgentKind, Defaults, GitConfig, MaxSeconds, Route};

    use super::*;

    /// M10 idle watchdog: a session that appends to its log within the idle window
    /// survives, while a silent one is cancelled after the window elapses. These
    /// exercise the primitive directly (no agent client) so they stay fast and
    /// self-bounding.
    #[tokio::test]
    async fn idle_watchdog_cancels_silent_session_but_not_a_chatty_one() {
        let dir = std::env::temp_dir().join(format!("varda-idle-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).expect("temp dir");
        let log_path = dir.join("session.log");
        fs::write(&log_path, "header\n").expect("seed log");

        // Silent: the future never resolves and the log never grows → Idle kill.
        let silent = run_session_watched(
            Duration::from_millis(250),
            None,
            &log_path,
            std::future::pending::<Result<AgentRunResult>>(),
        )
        .await;
        assert!(
            matches!(silent, Err(SessionKill::Idle { .. })),
            "a silent session must be idle-cancelled, got {silent:?}"
        );

        // Chatty: a background task grows the log every 50ms for ~600ms, out-living
        // the 250ms idle window, then the session future resolves normally.
        let chatty_log = log_path.clone();
        let fut = async move {
            for i in 0..12u32 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let _ = append_session_log(&chatty_log, &format!("chunk {i}\n"));
            }
            Ok(AgentRunResult {
                recap: "# Recap\n\nDone.".to_owned(),
                requires_user: false,
                suggested_agent: None,
                resume_command: None,
            })
        };
        let chatty = run_session_watched(Duration::from_millis(250), None, &log_path, fut).await;
        assert!(
            matches!(chatty, Ok(Ok(_))),
            "a session that keeps emitting output must not be idle-cancelled, got {chatty:?}"
        );
    }

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
        // Disambiguate the idle path from the soft ceiling: no budget deadline
        // (`max_seconds = "none"`) and a 1s idle window. A never-returning client
        // that writes nothing trips the idle watchdog, not the budget.
        config.defaults.idle_timeout_seconds = 1;
        config.defaults.max_seconds = Some(MaxSeconds::Keyword("none".to_owned()));
        let client = PendingAgentClient;

        let outcome = run_task(&config, "codex", None, &task_path, &client, false, false)
            .await
            .expect("task should idle-cancel cleanly");

        let recap = fs::read_to_string(&outcome.recap_path).expect("recap should be readable");
        let log = fs::read_to_string(&outcome.session_log_path).expect("log should be readable");

        assert_eq!(outcome.status, TaskStatus::Failed);
        assert!(recap.contains("Agent Run Timed Out"));
        assert!(recap.contains("idle watchdog"));
        assert!(recap.contains("long-running runner task"));
        assert!(recap.contains("Suggested next agent: runner"));
        assert!(log.contains("long_running_task_requested=true"));
    }

    /// M10 operation budget: reaching the soft total ceiling (`max_seconds`) is a
    /// graceful checkpoint — the task is marked `needs_user` with the accumulated
    /// recap, never `failed` and never a mid-work kill.
    #[tokio::test]
    async fn run_task_budget_ceiling_marks_needs_user_not_failed() {
        let root =
            std::env::temp_dir().join(format!("varda-run-budget-{}", std::process::id()));
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
        // A 1s soft ceiling with a large idle window: the budget fires first.
        config.defaults.max_seconds = Some(MaxSeconds::Seconds(1));
        config.defaults.idle_timeout_seconds = 60;
        let client = PendingAgentClient;

        let outcome = run_task(&config, "codex", None, &task_path, &client, false, false)
            .await
            .expect("task should stop at the budget cleanly");

        let recap = fs::read_to_string(&outcome.recap_path).expect("recap should be readable");

        assert_eq!(
            outcome.status,
            TaskStatus::NeedsUser,
            "a budget stop is a graceful checkpoint, not a failure"
        );
        assert!(recap.contains("Operation Budget Reached"));
        assert!(recap.contains("soft ceiling"));
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

    /// A client that replays a scripted list of responses, one per session call,
    /// recording each request. Used to drive the multi-hop auto-resume loop:
    /// early responses carry a `resume_command` ("more work"), the last does not.
    #[derive(Clone)]
    struct ScriptedResumeClient {
        requests: std::sync::Arc<std::sync::Mutex<Vec<AgentRunRequest>>>,
        responses: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<AgentRunResult>>>,
        /// Response handed back once the scripted queue is exhausted.
        fallback: AgentRunResult,
    }

    #[async_trait]
    impl AgentClient for ScriptedResumeClient {
        async fn run_task(&self, request: AgentRunRequest) -> Result<AgentRunResult> {
            self.requests.lock().unwrap().push(request);
            let next = self.responses.lock().unwrap().pop_front();
            Ok(next.unwrap_or_else(|| self.fallback.clone()))
        }
    }

    fn ready_task(dir_tag: &str) -> (PathBuf, Config) {
        let root = std::env::temp_dir().join(format!("varda-{dir_tag}-{}", Uuid::new_v4()));
        let operations_dir = root.join("operations");
        let task_dir = operations_dir.join("tasks/codex");
        fs::create_dir_all(&task_dir).expect("task directory should be created");
        let task_path = task_dir.join("example.md");
        fs::write(
            &task_path,
            "---\nstatus: ready\nproject: /work/project\nassignee: codex\nrequires_user: false\n---\n\n# Task\n\nDo it.\n",
        )
        .expect("task should be written");
        let config = test_config(operations_dir.display().to_string());
        (task_path, config)
    }

    /// M10 auto-resume: a resume-command-then-done script stitches ≥2 sessions
    /// into one COMPLETED task, preserving each hop's recap in order.
    #[tokio::test]
    async fn auto_resume_stitches_resume_then_done_into_completed_task() {
        let (task_path, mut config) = ready_task("run-autoresume");
        config.defaults.max_continuations = 8;
        let client = ScriptedResumeClient {
            requests: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            responses: std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from(
                vec![
                    AgentRunResult {
                        recap: "# Hop one\n\nStarted the work.".to_owned(),
                        requires_user: false,
                        suggested_agent: None,
                        resume_command: Some("codex resume hop-1".to_owned()),
                    },
                    AgentRunResult {
                        recap: "# Hop two\n\nFinished the work.".to_owned(),
                        requires_user: false,
                        suggested_agent: None,
                        resume_command: None,
                    },
                ],
            ))),
            fallback: AgentRunResult {
                recap: "unexpected extra call".to_owned(),
                requires_user: false,
                suggested_agent: None,
                resume_command: None,
            },
        };

        let outcome = run_task(&config, "codex", None, &task_path, &client, false, false)
            .await
            .expect("auto-resume task should run");

        let recorded = client.requests.lock().unwrap().clone();
        assert_eq!(recorded.len(), 2, "expected two stitched sessions");
        assert!(recorded[0].resume_command.is_none(), "first hop is fresh");
        assert_eq!(
            recorded[1].resume_command.as_deref(),
            Some("codex resume hop-1"),
            "second hop must be seeded with the captured resume command"
        );

        let recap = fs::read_to_string(&outcome.recap_path).expect("recap readable");
        let hop_one = recap.find("Hop one").expect("hop one recap preserved");
        let hop_two = recap.find("Hop two").expect("hop two recap preserved");
        assert!(hop_one < hop_two, "recaps must be preserved in hop order");

        assert_eq!(outcome.status, TaskStatus::Pending, "a stitched run completes");
        let task = crate::task::load_task(&task_path).expect("task loads");
        assert_eq!(
            task.frontmatter.agent_session_ids.len(),
            2,
            "each hop records its own session id"
        );
    }

    /// M10 auto-resume cap: an agent that never reports done is stopped after
    /// `max_continuations` hops and marked `needs_user` (never an infinite loop).
    #[tokio::test]
    async fn auto_resume_is_capped_by_max_continuations() {
        let (task_path, mut config) = ready_task("run-autoresume-cap");
        config.defaults.max_continuations = 2;
        let client = ScriptedResumeClient {
            requests: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            responses: std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
            // Every call reports more work: the loop must stop on the cap, not here.
            fallback: AgentRunResult {
                recap: "# Still working\n\nMore to do.".to_owned(),
                requires_user: false,
                suggested_agent: None,
                resume_command: Some("codex resume again".to_owned()),
            },
        };

        let outcome = run_task(&config, "codex", None, &task_path, &client, false, false)
            .await
            .expect("capped auto-resume should stop cleanly");

        let recorded = client.requests.lock().unwrap().clone();
        assert_eq!(
            recorded.len(),
            3,
            "first session + max_continuations(2) continuations = 3 calls"
        );
        assert_eq!(
            outcome.status,
            TaskStatus::NeedsUser,
            "hitting the continuation cap with work remaining is a graceful checkpoint"
        );
        let recap = fs::read_to_string(&outcome.recap_path).expect("recap readable");
        assert!(recap.contains("Auto-Resume Limit Reached"));
    }

    /// M10 per-task overrides: a task frontmatter bound wins over the config default.
    #[test]
    fn per_task_frontmatter_overrides_bounds() {
        let config = test_config("/tmp/unused".to_owned());
        let mut fm = crate::task::TaskFrontmatter {
            bounds: crate::task::TaskBounds::default(),
            ..default_frontmatter()
        };
        // Defaults come from config first.
        let base = OperationBounds::resolve(&config, &fm);
        assert_eq!(base.idle_timeout.as_secs(), config.defaults.idle_timeout_seconds);
        assert_eq!(base.max_continuations, config.defaults.max_continuations);

        // A per-task override wins for each of the four bounds.
        fm.bounds.idle_timeout = Some(7);
        fm.bounds.max_continuations = Some(1);
        fm.bounds.max_tool_calls = Some(42);
        fm.bounds.max_seconds = Some(crate::config::MaxSeconds::Seconds(123));
        let over = OperationBounds::resolve(&config, &fm);
        assert_eq!(over.idle_timeout.as_secs(), 7);
        assert_eq!(over.max_continuations, 1);
        assert_eq!(over.max_tool_calls, 42);
        assert_eq!(over.max_seconds, Some(123));
    }

    fn default_frontmatter() -> crate::task::TaskFrontmatter {
        crate::task::TaskFrontmatter {
            id: None,
            status: TaskStatus::Ready,
            project: None,
            assignee: None,
            recap: None,
            recaps: vec![],
            plan: None,
            agent_session_id: None,
            agent_session_log: None,
            agent_session_ids: vec![],
            agent_session_logs: vec![],
            agent_resume_commands: vec![],
            allow_commands: vec![],
            bounds: crate::task::TaskBounds::default(),
            requires_user: false,
        }
    }

    fn test_config(operations_dir: String) -> Config {
        Config {
            defaults: Defaults {
                timeout_seconds: 600,
                operations_dir,
                sandbox: None,
                // A generous idle window so the instant fake clients used by most
                // tests are never idle-limited; stall/budget tests set their own.
                idle_timeout_seconds: 30,
                ..Default::default()
            },
            routes: vec![Route {
                glob: "**".to_owned(),
                agents: vec!["codex".to_owned()],
                sandbox: None,
                mounts: Vec::new(),
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
                    auth_token_env: None,
                    auth_token_target: None,
                    resume_command_template: None,
                },
            )]),
            roles: std::collections::BTreeMap::new(),
            git: GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
        }
    }
}
