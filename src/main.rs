mod acp;
mod agent;
mod capability;
mod config;
mod git;
mod mcp_transport;
mod notify;
mod orchestration;
mod routing;
mod runner;
mod sandbox;
mod task;

use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use uuid::Uuid;

use crate::agent::AgentClient;

#[derive(Debug, Parser)]
#[command(name = "varda")]
#[command(about = "Drive ACP agents from markdown operations tasks")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Initialize the Varda operations folder.
    Init {
        /// Overwrite an existing Varda config.
        #[arg(long)]
        force: bool,
    },
    /// Run tasks through configured agents.
    Run {
        /// Markdown task file or task id to process.
        #[arg(long)]
        task: Option<PathBuf>,
        /// Execution plan to transform to JSON and run. For compatibility, a positional task still runs as a task.
        plan: Option<PathBuf>,
        /// Skip the confirmation prompt and run immediately.
        #[arg(long)]
        yes: bool,
    },
    /// Create a reviewable execution plan for ready tasks.
    Plan,
    /// Manage Varda configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Manage project routes.
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Manage markdown tasks.
    Task {
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Manage Claude Code skills.
    Skill {
        #[command(subcommand)]
        command: SkillCommand,
    },
    /// Launch the self-hosting orchestrator: run the RESIDENT as a sandboxed
    /// interactive agent with the spawn broker wired, so Varda can drive its own
    /// dev loop. The loop logic lives in the workspace's `.varda/WORKFLOW.md`; this
    /// command only routes the resident into an isolating, net-denied sandbox with a
    /// dedicated rw workspace mount and NO push credential (all asserted before launch).
    Orchestrate {
        /// Attach a TTY and put the operator in the conversation (M13b interactive
        /// path). Without this flag the resident runs headless until it terminates
        /// or signals `needs_user`.
        #[arg(long)]
        interactive: bool,
        /// Dedicated workspace directory mounted rw into the resident sandbox.
        /// Defaults to `<varda_home>/orchestrate/workspace`. Never `$HOME`/`~/dev`.
        #[arg(long)]
        workspace: Option<PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum TaskCommand {
    /// Create a new markdown task.
    Add {
        /// Human-readable task name. Omit when using --file.
        taskname: Option<String>,
        /// Optional task description (body text). Reads from stdin if stdin is not a terminal.
        description: Option<String>,
        /// Read task name (filename stem) and description (file content) from a file.
        #[arg(long)]
        file: Option<PathBuf>,
        /// Project path this task belongs to. Defaults to the current directory.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Agent to assign the task to. Skips the interactive assignee prompt.
        #[arg(long)]
        agent: Option<String>,
        /// Pin this task to a named central sandbox (`[sandboxes.<NAME>]`),
        /// overriding route/`.varda`/defaults resolution at run time. Use `local`
        /// for the identity provider. Composes with `--exec --interactive`.
        #[arg(long, value_name = "NAME")]
        sandbox: Option<String>,
        /// Treat the task name as a complete one-line task and run it immediately.
        #[arg(long)]
        exec: bool,
        /// Open the task in $EDITOR after creation (or before running with --exec).
        #[arg(long)]
        edit: bool,
        /// Spawn the agent in the background and return immediately (only meaningful with --exec).
        #[arg(long)]
        background: bool,
        /// Surface the agent output in the current shell and forward stdin for interaction (only meaningful with --exec).
        #[arg(long)]
        interactive: bool,
        /// Suppress live streaming of the agent's stdout (only meaningful with --exec).
        #[arg(long)]
        quiet: bool,
        /// Set the task status to ready after creation (skips backlog).
        #[arg(long)]
        ready: bool,
        /// Resume-or-create: if a task with this name already exists for the project,
        /// resume it (fresh session) instead of erroring. Only acts with `--exec`;
        /// intended for the interactive shell aliases (vadc/vtgg/…) so repeated
        /// launches reuse one task per (project, name) rather than colliding.
        #[arg(long)]
        reuse: bool,
    },
    /// List markdown tasks for a project.
    List {
        /// Project path to list tasks for. Defaults to the current directory.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Include backlog and done tasks.
        #[arg(long)]
        all: bool,
    },
    /// Run a markdown task through the configured agent.
    Run {
        /// Markdown task file or task id to process.
        task: PathBuf,
        /// Spawn the agent in the background and return immediately.
        #[arg(long)]
        background: bool,
        /// Surface the agent output in the current shell and forward stdin for interaction.
        #[arg(long)]
        interactive: bool,
        /// Suppress live streaming of the agent's stdout to the terminal (output is still
        /// written to the session log and the recap is printed after the run).
        #[arg(long)]
        quiet: bool,
    },
    /// Generate an agent-driven plan for a task and open it in $EDITOR.
    Plan {
        /// Markdown task file or task id to plan.
        task: PathBuf,
    },
    /// Resume a task that is waiting for user input, then run it.
    Resume {
        /// Markdown task file or task id to resume.
        task: PathBuf,
        /// Start a fresh agent session instead of using a captured agent resume command.
        #[arg(long)]
        fresh: bool,
        /// Surface the agent output in the current shell and forward stdin for interaction.
        #[arg(long)]
        interactive: bool,
    },
    /// Choose a past run session for a task and move it back to ready.
    ResumeSession {
        /// Markdown task file or task id whose session should be resumed.
        task: PathBuf,
    },
    /// Display a markdown task and its associated recap.
    Show {
        /// Markdown task file or task id to display.
        task: PathBuf,
    },
    /// Show a kanban dashboard and optionally open task details.
    Dashboard {
        /// Project path to show tasks for. Defaults to the current directory.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Show tasks across all projects instead of only one project.
        #[arg(long)]
        all: bool,
        /// Serve a browser-based kanban dashboard.
        #[arg(long)]
        web: bool,
        /// Local port for --web.
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// Detach the --web server from the terminal so it survives shell exit.
        #[arg(long)]
        daemon: bool,
        /// Task file or task id to display after the board.
        #[arg(long)]
        task: Option<PathBuf>,
    },
    /// Open a markdown task in $EDITOR.
    Edit {
        /// Markdown task file or task id to open.
        task: PathBuf,
    },
    /// Show runtime diagnostics: agent config, route, session logs, and live processes.
    Inspect {
        /// Markdown task file or task id to inspect.
        task: PathBuf,
    },
    /// Set the status of a task directly.
    SetStatus {
        /// New status (backlog, ready, running, review, needs_user, failed, done). Legacy `pending` is accepted as an alias for `review`.
        status: String,
        /// Markdown task file or task id to update.
        task: PathBuf,
    },
    /// Print the resolved file path for a task ID or path.
    Resolve {
        /// Markdown task file or task id to resolve.
        task: PathBuf,
    },
    /// Delete a task's runtime state file (and its recaps) from the home store.
    Delete {
        /// Markdown task file or task id to delete.
        task: PathBuf,
        /// Delete without prompting for confirmation.
        #[arg(long)]
        yes: bool,
        /// Keep the task's recap files instead of removing them alongside the task.
        #[arg(long)]
        keep_recaps: bool,
    },
    /// Update task properties for a single task or in bulk.
    Update {
        /// Task file or task id to update. Omit to use filter flags for bulk selection.
        task: Option<PathBuf>,
        /// Set the task status (backlog, ready, running, review, needs_user, failed, done). Legacy `pending` is accepted as an alias for `review`.
        #[arg(long, value_name = "STATUS")]
        set_status: Option<String>,
        /// Set the task assignee.
        #[arg(long, value_name = "AGENT")]
        set_agent: Option<String>,
        /// Only update tasks with this status (repeatable for OR logic).
        #[arg(long, value_name = "STATUS")]
        filter_status: Vec<String>,
        /// Only update tasks assigned to this agent.
        #[arg(long, value_name = "AGENT")]
        filter_agent: Option<String>,
        /// Project path to scope task selection. Defaults to the current directory.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Operate across all projects instead of only one project.
        #[arg(long)]
        all: bool,
        /// Apply changes without prompting for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Rewrite legacy `status: pending` task state files to `status: review`.
    ///
    /// Operates on the configured Varda operations task directory, preserves all
    /// other frontmatter and task body, is idempotent, and reports how many files
    /// were changed. Legacy `pending` parsing stays supported afterwards, so task
    /// files from another machine or branch still load.
    MigrateStatus,
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Open the global Varda config in $EDITOR.
    Edit,
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// Add a project/path route and its allowed agents.
    Add {
        /// Project path or glob pattern.
        glob: String,
        /// Allowed agents for this project route. Accepts comma-separated values.
        #[arg(long, value_delimiter = ',', required = true)]
        agents: Vec<String>,
    },
    /// Fold a finished workspace's tasks into a mother project (post-merge cleanup).
    ///
    /// Re-keys every task whose `project` is WORKSPACE to the mother and relocates
    /// the records into the mother's store, so a merged worktree stops being a
    /// separate dashboard project. WORKSPACE is matched as a path string — the
    /// worktree may already be removed.
    Fold {
        /// The workspace/worktree project path whose tasks to fold.
        workspace: String,
        /// The mother project to fold into (must exist).
        #[arg(long)]
        into: PathBuf,
        /// Preview what would move without writing.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SkillCommand {
    /// Install the Varda skill into the user-level Claude Code skills directory.
    Install {
        /// Path to the SKILL.md file. Defaults to skills/varda/SKILL.md in the current directory.
        source: Option<PathBuf>,
        /// Create a symbolic link instead of copying the skill file.
        #[arg(long)]
        link: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Init { force } => {
            let result = config::init_workspace(force)?;
            println!(
                "initialized Varda config at {} and operations folder at {}",
                result.config_path, result.operations_dir
            );
        }
        Command::Run { task, plan, yes } => {
            run_command(task.as_deref(), plan.as_deref(), yes).await?;
        }
        Command::Plan => {
            plan_command()?;
        }
        Command::Config { command } => match command {
            ConfigCommand::Edit => {
                let config_path = config::config_file()?;
                open_editor(&config_path)?;
            }
        },
        Command::Task { command } => match command {
            TaskCommand::Add {
                taskname,
                description,
                file,
                project,
                agent,
                sandbox,
                exec,
                edit,
                background,
                interactive,
                quiet,
                ready,
                reuse,
            } => {
                use std::io::IsTerminal as _;
                let (taskname, description) = if let Some(file_path) = file {
                    let stem = file_path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .with_context(|| {
                            format!("cannot derive task name from {}", file_path.display())
                        })?
                        .to_owned();
                    let content = fs::read_to_string(&file_path)
                        .with_context(|| format!("failed to read {}", file_path.display()))?;
                    let trimmed = content.trim_end().to_owned();
                    (
                        stem,
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed)
                        },
                    )
                } else {
                    let name = taskname.context("taskname is required when --file is not used")?;
                    let desc = if description.is_some() {
                        description
                    } else if !std::io::stdin().is_terminal() {
                        let mut buf = String::new();
                        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)?;
                        let trimmed = buf.trim_end().to_owned();
                        if trimmed.is_empty() {
                            None
                        } else {
                            Some(trimmed)
                        }
                    } else {
                        None
                    };
                    (name, desc)
                };
                let config_path = config::config_file()?;
                let config = config::load_config(&config_path)?;
                let project_path = task::resolve_project_path(project.as_deref())?;
                // --reuse (with --exec): if this (project, name) task already exists,
                // resume it with a fresh interactive session instead of erroring on the
                // collision. Lets the shell aliases be launched repeatedly without a
                // stuck fixed-name task blocking the next run. Skips creation entirely;
                // the existing task keeps its stored assignee/sandbox.
                if reuse && exec {
                    let existing = task::task_file_path(&config, &project_path, &taskname)?;
                    if existing.exists() {
                        println!(
                            "task {} already exists — resuming (fresh, interactive)",
                            existing.display()
                        );
                        return resume_task_command(&existing, true, interactive).await;
                    }
                }
                // Validate the pinned sandbox up front so `--sandbox typo` fails at
                // creation time rather than only at run time. `local` is always valid.
                if let Some(name) = sandbox.as_deref() {
                    let known = name == config::DEFAULT_SANDBOX_PROVIDER
                        || config.sandboxes.contains_key(name);
                    if !known {
                        anyhow::bail!(
                            "sandbox '{name}' is not configured; \
                             add a `[sandboxes.{name}]` entry or use `local`"
                        );
                    }
                }
                let assignee = if let Some(ref agent_name) = agent {
                    routing::match_route(&config, &project_path, Some(agent_name))?;
                    Some(agent_name.clone())
                } else {
                    let default_route = routing::match_route(&config, &project_path, None)?;
                    let default_assignee = default_route.display_name().to_owned();
                    let assignee = prompt_assignee(&default_assignee)?;
                    if let Some(assignee) = assignee.as_deref() {
                        routing::match_route(&config, &project_path, Some(assignee))?;
                    }
                    assignee
                };
                let task_path = task::create_task(
                    &config,
                    &taskname,
                    &project_path,
                    assignee.as_deref(),
                    description.as_deref(),
                    sandbox.as_deref(),
                )?;
                let mut task_doc = task::load_task(&task_path)?;
                if ready || exec {
                    task_doc.set_status(task::TaskStatus::Ready);
                    task::write_task(&task_doc)?;
                }
                if let Some(task_id) = task_doc.frontmatter.id {
                    println!("created task #{task_id} {}", task_path.display());
                } else {
                    println!("created task {}", task_path.display());
                }
                if exec {
                    if edit {
                        open_editor(&task_path)?;
                    }
                    if background {
                        spawn_task_in_background(&task_path)?;
                    } else {
                        run_task_command(&task_path, interactive, quiet).await?;
                    }
                } else if edit {
                    open_editor(&task_path)?;
                }
            }
            TaskCommand::List { project, all } => {
                let config_path = config::config_file()?;
                let config = config::load_config(&config_path)?;
                let project_path = task::resolve_project_path(project.as_deref())?;
                let mut tasks = task::list_tasks(&config, &project_path)?;
                if !all {
                    tasks.retain(|task| is_active_task_status(task.status));
                }
                print_task_list(&project_path, &tasks);
            }
            TaskCommand::Run {
                task,
                background,
                interactive,
                quiet,
            } => {
                if background {
                    spawn_task_in_background(&task)?;
                } else {
                    run_task_command(&task, interactive, quiet).await?;
                }
            }
            TaskCommand::Plan { task } => {
                plan_task_command(&task).await?;
            }
            TaskCommand::Resume {
                task,
                fresh,
                interactive,
            } => {
                resume_task_command(&task, fresh, interactive).await?;
            }
            TaskCommand::ResumeSession { task } => {
                resume_task_session_command(&task)?;
            }
            TaskCommand::Show { task } => {
                show_task_command(&task)?;
            }
            TaskCommand::Dashboard {
                project,
                all,
                web,
                port,
                daemon,
                task,
            } => {
                dashboard_task_command(
                    project.as_deref(),
                    all,
                    web,
                    port,
                    daemon,
                    task.as_deref(),
                )?;
            }
            TaskCommand::Edit { task } => {
                let config_path = config::config_file()?;
                let config = config::load_config(&config_path)?;
                let resolved = task::resolve_task_reference(&config, &task)?;
                open_editor(&resolved)?;
            }
            TaskCommand::Inspect { task } => {
                inspect_task_command(&task)?;
            }
            TaskCommand::SetStatus { status, task } => {
                update_tasks_command(
                    Some(&task),
                    Some(&status),
                    None,
                    &[],
                    None,
                    None,
                    false,
                    true,
                )?;
            }
            TaskCommand::Resolve { task } => {
                let config_path = config::config_file()?;
                let config = config::load_config(&config_path)?;
                let resolved = task::resolve_task_reference(&config, &task)?;
                println!("{}", resolved.display());
            }
            TaskCommand::Delete {
                task,
                yes,
                keep_recaps,
            } => {
                delete_task_command(&task, yes, keep_recaps)?;
            }
            TaskCommand::Update {
                task,
                set_status,
                set_agent,
                filter_status,
                filter_agent,
                project,
                all,
                yes,
            } => {
                update_tasks_command(
                    task.as_deref(),
                    set_status.as_deref(),
                    set_agent.as_deref(),
                    &filter_status,
                    filter_agent.as_deref(),
                    project.as_deref(),
                    all,
                    yes,
                )?;
            }
            TaskCommand::MigrateStatus => {
                let config = config::load_config(&config::config_file()?)?;
                let changed = task::migrate_pending_status(&config)?;
                println!("migrated {changed} task file(s) from `pending` to `review`");
            }
        },
        Command::Project { command } => match command {
            ProjectCommand::Add { glob, agents } => {
                let config_path = config::config_file()?;
                config::add_project_route(&config_path, glob.clone(), agents.clone())?;
                println!(
                    "added project route glob={} allowed_agents={}",
                    glob,
                    agents.join(",")
                );
            }
            ProjectCommand::Fold {
                workspace,
                into,
                dry_run,
            } => {
                let config = config::load_config(&config::config_file()?)?;
                let report = task::fold_project(&config, &workspace, &into, dry_run)?;
                let verb = if dry_run { "would fold" } else { "folded" };
                println!(
                    "{verb} {} task(s) from {} into {}",
                    report.moved.len(),
                    workspace,
                    into.display()
                );
                if !report.collisions.is_empty() {
                    println!(
                        "  left in place (name already in mother store): {}",
                        report.collisions.join(", ")
                    );
                }
                for dir in &report.removed_dirs {
                    println!("  removed empty store folder {dir}");
                }
            }
        },
        Command::Skill { command } => match command {
            SkillCommand::Install { source, link } => {
                skill_install_command(source.as_deref(), link)?;
            }
        },
        Command::Orchestrate {
            interactive,
            workspace,
        } => {
            orchestrate_command(interactive, workspace.as_deref()).await?;
        }
    }

    Ok(())
}

fn plan_command() -> Result<()> {
    let config_path = config::config_file()?;
    let config = config::load_config(&config_path)?;
    let project_path = task::resolve_project_path(None)?;
    let project_tasks = task::list_tasks(&config, &project_path)?;
    let (scope, considered_tasks, selection_reason) = if project_tasks.is_empty() {
        (
            "global",
            task::list_all_tasks(&config)?,
            format!(
                "the current folder is not known as a Varda project because no tasks reference {}; all tasks across all projects were considered",
                project_path.display()
            ),
        )
    } else {
        (
            "project",
            project_tasks,
            format!(
                "the current folder is known as a Varda project because tasks in the Varda task store reference {}; only this project's tasks were considered",
                project_path.display()
            ),
        )
    };
    let ready_tasks: Vec<_> = considered_tasks
        .iter()
        .filter(|task| task.status == task::TaskStatus::Ready)
        .cloned()
        .collect();
    let plan_tasks = plan_tasks(&config, &ready_tasks)?;
    let plan_path = write_execution_plan(
        &config,
        scope,
        &project_path,
        &selection_reason,
        considered_tasks.len(),
        &plan_tasks,
    )?;

    println!("scope: {scope}");
    println!("selection: {selection_reason}");
    if scope == "project" {
        println!("project: {}", project_path.display());
    } else {
        println!("project: none detected for current directory");
    }
    println!("considered_tasks: {}", considered_tasks.len());
    println!("ready_tasks: {}", plan_tasks.len());
    println!("plan: {}", plan_path.display());
    println!("review the plan, edit if needed, then confirm before running tasks");

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlanTask {
    summary: task::TaskSummary,
    agent: String,
    route_glob: String,
    dependency_hint: DependencyHint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum DependencyHint {
    Identity,
    Planning,
    Execution,
    Sessions,
    Visibility,
    AgentSupport,
    General,
}

impl DependencyHint {
    fn label(self) -> &'static str {
        match self {
            Self::Identity => "identity/versioning",
            Self::Planning => "planning",
            Self::Execution => "execution",
            Self::Sessions => "sessions/resume",
            Self::Visibility => "visibility/dashboard",
            Self::AgentSupport => "agent support",
            Self::General => "general",
        }
    }

    fn stage(self) -> &'static str {
        match self {
            Self::Identity => "Stage 1, sequential",
            Self::Planning => "Stage 2, sequential",
            Self::Execution => "Stage 3, sequential",
            Self::Sessions => "Stage 4, parallel candidate",
            Self::Visibility => "Stage 4, parallel candidate",
            Self::AgentSupport => "Stage 5, optional parallel candidate",
            Self::General => "Stage 6, sequential validation",
        }
    }

    fn reason(self) -> &'static str {
        match self {
            Self::Identity => {
                "stable task IDs and captured task state reduce ambiguity for later runs"
            }
            Self::Planning => "planning semantics should be reviewed before automated execution",
            Self::Execution => {
                "run command behavior depends on task selection and routing semantics"
            }
            Self::Sessions => "session resume behavior depends on run metadata and task lifecycle",
            Self::Visibility => {
                "dashboards and task views depend on stable task and recap metadata"
            }
            Self::AgentSupport => {
                "additional agent support can be isolated after routing assumptions are clear"
            }
            Self::General => "work should run after the higher-level command semantics are stable",
        }
    }
}

fn plan_tasks(config: &config::Config, ready_tasks: &[task::TaskSummary]) -> Result<Vec<PlanTask>> {
    let mut plan_tasks = Vec::new();
    for summary in ready_tasks {
        let task_document = task::load_task(&summary.path)?;
        let route = routing::match_route_for_task(config, &task_document, false)?;
        plan_tasks.push(PlanTask {
            summary: summary.clone(),
            agent: route.display_name().to_owned(),
            route_glob: route.glob,
            dependency_hint: dependency_hint(summary),
        });
    }

    plan_tasks.sort_by(|left, right| {
        left.dependency_hint
            .cmp(&right.dependency_hint)
            .then_with(|| {
                left.summary
                    .id
                    .unwrap_or(u64::MAX)
                    .cmp(&right.summary.id.unwrap_or(u64::MAX))
            })
            .then_with(|| left.summary.path.cmp(&right.summary.path))
    });

    Ok(plan_tasks)
}

fn dependency_hint(summary: &task::TaskSummary) -> DependencyHint {
    let text = format!(
        "{} {}",
        summary.title.to_ascii_lowercase(),
        summary.path.display()
    );

    if text.contains("version") || text.contains(" id") || text.contains("-id") {
        DependencyHint::Identity
    } else if text.contains("plan") || text.contains("planner") {
        DependencyHint::Planning
    } else if text.contains("run") || text.contains("exec") {
        DependencyHint::Execution
    } else if text.contains("session") || text.contains("resume") {
        DependencyHint::Sessions
    } else if text.contains("dashboard") || text.contains("show") || text.contains("list") {
        DependencyHint::Visibility
    } else if text.contains("claude") || text.contains("agent") {
        DependencyHint::AgentSupport
    } else {
        DependencyHint::General
    }
}

fn write_execution_plan(
    config: &config::Config,
    scope: &str,
    project_path: &Path,
    selection_reason: &str,
    considered_tasks_count: usize,
    ready_tasks: &[PlanTask],
) -> Result<PathBuf> {
    let timestamp = unix_timestamp()?;
    let plan_dir = Path::new(&config.defaults.operations_dir).join("plans");
    fs::create_dir_all(&plan_dir)
        .with_context(|| format!("failed to create plan directory {}", plan_dir.display()))?;
    let project_name = project_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace");
    let plan_path = plan_dir.join(format!(
        "{scope}-{project_name}-ready-task-plan-{timestamp}.md"
    ));
    let content = render_execution_plan(
        scope,
        project_path,
        timestamp,
        selection_reason,
        considered_tasks_count,
        ready_tasks,
    );

    fs::write(&plan_path, content)
        .with_context(|| format!("failed to write plan at {}", plan_path.display()))?;

    Ok(plan_path)
}

fn render_execution_plan(
    scope: &str,
    project_path: &Path,
    timestamp: u64,
    selection_reason: &str,
    considered_tasks_count: usize,
    ready_tasks: &[PlanTask],
) -> String {
    let title_scope = if scope == "project" {
        "Project"
    } else {
        "Global"
    };
    let project = project_path.display().to_string();
    let frontmatter = PlanFrontmatter {
        plan_type: "ready_task_execution_plan",
        scope,
        project: &project,
        generated_timestamp: timestamp,
        tasks_evaluated: considered_tasks_count,
        ready_tasks: ready_tasks.len(),
        planner_agent: "codex",
        selection_reason,
        requires_user_confirmation: true,
    };
    let frontmatter =
        serde_yaml::to_string(&frontmatter).expect("plan frontmatter should serialize");
    let frontmatter = frontmatter.trim_start_matches("---\n").trim_end();
    let mut content = format!(
        "---\n{frontmatter}\n---\n\n# {title_scope} Ready Task Execution Plan\n\n- Scope: {scope}\n- Generated timestamp: {timestamp}\n- Project: `{}`\n- Selection rule: {selection_reason}.\n- Tasks evaluated: {considered_tasks_count}\n- Ready tasks: {}\n- Planner agent: codex\n- Execution should wait for explicit user confirmation.\n\n",
        project_path.display(),
        ready_tasks.len()
    );

    content.push_str("## Ready Tasks\n\n");
    if ready_tasks.is_empty() {
        content.push_str("No ready tasks were found.\n\n");
    } else {
        for task in ready_tasks {
            let id = task
                .summary
                .id
                .map(|id| format!("#{id}"))
                .unwrap_or_else(|| "unversioned".to_owned());
            let project = task.summary.project.as_deref().unwrap_or("no project");
            content.push_str(&format!(
                "- {id} `{}`: {} (project: `{project}`, agent: {}, route: `{}`, category: {})\n",
                task.summary.path.display(),
                task.summary.title,
                task.agent,
                task.route_glob,
                task.dependency_hint.label()
            ));
        }
        content.push('\n');
    }

    content.push_str("## Priority And Dependencies\n\n");
    if ready_tasks.is_empty() {
        content.push_str("- No ready work is available to prioritize.\n\n");
    } else {
        for task in ready_tasks {
            content.push_str(&format!(
                "- `{}`: {}.\n",
                task.summary.title,
                task.dependency_hint.reason()
            ));
        }
        content.push('\n');
    }

    content.push_str("## Execution Stages\n\n");
    if ready_tasks.is_empty() {
        content.push_str("No execution stages are needed until a task is ready.\n\n");
    } else {
        let mut last_stage = "";
        for task in ready_tasks {
            let stage = task.dependency_hint.stage();
            if stage != last_stage {
                content.push_str(&format!("- {stage}: "));
                last_stage = stage;
            } else {
                content.push_str("- Same stage: ");
            }
            content.push_str(&format!(
                "`{}` assigned to `{}`.\n",
                task.summary.title, task.agent
            ));
        }
        content.push_str("- Final gate: review this plan, edit it if needed, and confirm before executing the ready set.\n\n");
    }

    content.push_str("## Review Gate\n\n");
    content.push_str("The next step is user review. The plan should be confirmed or edited before Varda executes the ready tasks.\n");

    content
}

#[derive(Debug, Serialize)]
struct PlanFrontmatter<'a> {
    plan_type: &'a str,
    scope: &'a str,
    project: &'a str,
    generated_timestamp: u64,
    tasks_evaluated: usize,
    ready_tasks: usize,
    planner_agent: &'a str,
    selection_reason: &'a str,
    requires_user_confirmation: bool,
}

#[derive(Debug, Deserialize)]
struct OwnedPlanFrontmatter {
    #[serde(default)]
    planner_agent: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct JsonExecutionPlan {
    schema: String,
    source_plan: String,
    tasks: Vec<JsonExecutionTask>,
}

#[derive(Debug, Serialize, Deserialize)]
struct JsonExecutionTask {
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    parallel_group: Option<String>,
}

fn unix_timestamp() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs())
}

fn file_mtime_seconds(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs())
}

async fn run_command(task_arg: Option<&Path>, plan_arg: Option<&Path>, yes: bool) -> Result<()> {
    match (task_arg, plan_arg) {
        (Some(task), None) => run_task_command(task, false, false).await,
        (Some(_), Some(_)) => {
            anyhow::bail!("pass either --task <TASK> or a plan path, not both")
        }
        (None, Some(path)) => {
            let config_path = config::config_file()?;
            let config = config::load_config(&config_path)?;
            if looks_like_task(&config, path) {
                run_task_command(path, false, false).await
            } else {
                run_plan_command(path, yes).await
            }
        }
        (None, None) => run_ready_tasks_command(yes).await,
    }
}

fn looks_like_task(config: &config::Config, path: &Path) -> bool {
    task::resolve_task_reference(config, path)
        .ok()
        .and_then(|resolved| task::load_task(resolved).ok())
        .is_some()
}

async fn run_plan_command(plan_path: &Path, yes: bool) -> Result<()> {
    let config_path = config::config_file()?;
    let config = config::load_config(&config_path)?;
    let plan_path = if plan_path.exists() {
        plan_path.to_path_buf()
    } else {
        anyhow::bail!("plan does not exist: {}", plan_path.display());
    };
    let json_path = transform_plan_to_json(&config, &plan_path).await?;
    let json_plan = load_json_execution_plan(&json_path)?;
    let task_paths: Vec<PathBuf> = json_plan
        .tasks
        .iter()
        .map(|task| PathBuf::from(&task.path))
        .collect();

    println!("plan_json: {}", json_path.display());
    println!("tasks: {}", task_paths.len());
    for path in &task_paths {
        println!("  {}", path.display());
    }
    if !yes && !prompt_yes_no("Proceed with running these tasks?", true)? {
        println!("aborted");
        return Ok(());
    }
    run_task_paths_in_parallel(config, task_paths).await
}

async fn transform_plan_to_json(config: &config::Config, plan_path: &Path) -> Result<PathBuf> {
    let content = fs::read_to_string(plan_path)
        .with_context(|| format!("failed to read plan at {}", plan_path.display()))?;
    let planner_agent = plan_planner_agent(&content)
        .or_else(|| config.agents.keys().next().cloned())
        .context("no configured agent is available to transform the plan")?;
    let agent_config = config
        .agents
        .get(&planner_agent)
        .with_context(|| format!("plan transformer agent '{planner_agent}' is not configured"))?;
    let client = build_client(
        config,
        &planner_agent,
        agent_config,
        config::DEFAULT_SANDBOX_PROVIDER,
        &[],
        &std::collections::BTreeMap::new(),
        None,
        None,
        None,
    )?;
    let timeout = std::time::Duration::from_secs(config.defaults.timeout_seconds);
    let request = agent::AgentRunRequest {
        agent_name: planner_agent.clone(),
        role_instructions: None,
        task_path: plan_path.display().to_string(),
        frontmatter: task::TaskFrontmatter {
            bounds: crate::task::TaskBounds::default(),
            id: None,
            sandbox: None,
            status: task::TaskStatus::Ready,
            project: None,
            mother_project: None,
            assignee: Some(planner_agent),
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
        body: format!(
            "# Transform Execution Plan To JSON\n\nReturn only JSON using schema `varda.execution_plan.v1`.\n\nThe JSON shape is:\n\n```json\n{{\"schema\":\"varda.execution_plan.v1\",\"source_plan\":\"{}\",\"tasks\":[{{\"path\":\"/absolute/or/resolvable/task.md\",\"id\":1,\"title\":\"Task title\",\"agent\":\"codex\",\"project\":\"/project\",\"stage\":\"Stage 1\",\"parallel_group\":\"group-1\"}}]}}\n```\n\nInclude every task that should be executed from this plan. Omit optional fields when unknown.\n\nPlan markdown:\n\n{}",
            plan_path.display(),
            content
        ),
        timeout,
        session_id: uuid::Uuid::new_v4().to_string(),
        session_log_path: None,
        interactive: false,
        interpret: false,
        stream: false,
        resume_command: None,
        orchestration_socket_path: None,
        orchestration_addr: None,
    };
    let result = client.run_task(request).await?;
    let json = extract_json_object(&result.recap)?;
    let mut json_plan: JsonExecutionPlan =
        serde_json::from_str(json).context("agent did not return a valid execution plan JSON")?;
    if json_plan.schema != "varda.execution_plan.v1" {
        anyhow::bail!(
            "unsupported execution plan JSON schema '{}'",
            json_plan.schema
        );
    }
    json_plan.source_plan = plan_path.display().to_string();
    let rendered = serde_json::to_string_pretty(&json_plan)?;
    let json_path = plan_path.with_extension("json");
    fs::write(&json_path, format!("{rendered}\n"))
        .with_context(|| format!("failed to write plan JSON at {}", json_path.display()))?;

    Ok(json_path)
}

fn plan_planner_agent(content: &str) -> Option<String> {
    let matter = gray_matter::Matter::<gray_matter::engine::YAML>::new();
    matter
        .parse::<OwnedPlanFrontmatter>(content)
        .ok()
        .and_then(|parsed| parsed.data)
        .and_then(|frontmatter| frontmatter.planner_agent)
}

fn extract_json_object(output: &str) -> Result<&str> {
    let start = output
        .find('{')
        .context("agent output did not contain JSON")?;
    let end = output
        .rfind('}')
        .context("agent output did not contain a complete JSON object")?;
    if end < start {
        anyhow::bail!("agent output did not contain a complete JSON object");
    }
    Ok(&output[start..=end])
}

fn load_json_execution_plan(path: &Path) -> Result<JsonExecutionPlan> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read plan JSON at {}", path.display()))?;
    let plan: JsonExecutionPlan =
        serde_json::from_str(&content).context("failed to parse execution plan JSON")?;
    if plan.schema != "varda.execution_plan.v1" {
        anyhow::bail!("unsupported execution plan JSON schema '{}'", plan.schema);
    }
    Ok(plan)
}

async fn run_ready_tasks_command(yes: bool) -> Result<()> {
    let config_path = config::config_file()?;
    let config = config::load_config(&config_path)?;
    let ready_summaries: Vec<task::TaskSummary> = task::list_all_tasks(&config)?
        .into_iter()
        .filter(|summary| summary.status == task::TaskStatus::Ready)
        .collect();

    if ready_summaries.is_empty() {
        println!("ready_tasks: 0");
        return Ok(());
    }

    let plan_tasks = plan_tasks(&config, &ready_summaries)?;
    println!("ready_tasks: {}", plan_tasks.len());
    for task in &plan_tasks {
        let id = task
            .summary
            .id
            .map(|id| format!("#{id}"))
            .unwrap_or_else(|| "unversioned".to_owned());
        println!(
            "  {} {} (agent: {}, category: {})",
            id,
            task.summary.title,
            task.agent,
            task.dependency_hint.label()
        );
    }
    if !yes && !prompt_yes_no("Proceed with running these tasks?", true)? {
        println!("aborted");
        return Ok(());
    }
    let task_paths: Vec<PathBuf> = plan_tasks.into_iter().map(|t| t.summary.path).collect();
    run_task_paths_in_parallel(config, task_paths).await
}

async fn run_task_paths_in_parallel(
    config: config::Config,
    task_paths: Vec<PathBuf>,
) -> Result<()> {
    if config.git.auto_commit {
        for task_path in &task_paths {
            git::commit_task_file(
                task_path,
                &format!("Snapshot task {} before run", task_path.display()),
            )?;
        }
        println!("committed task snapshots");
    }

    let mut runs = JoinSet::new();
    for task_path in task_paths {
        let config = config.clone();
        runs.spawn(async move { run_task_path_for_parallel(config, task_path, None).await });
    }

    let mut failures = 0usize;
    while let Some(joined) = runs.join_next().await {
        match joined.context("task runner join failed")? {
            Ok(report) => {
                println!(
                    "processed task={} agent={} glob={} status={:?} recap={}",
                    report.task_path.display(),
                    report.agent,
                    report.glob,
                    report.outcome.status,
                    report.outcome.recap_path.display()
                );
                let notification = if report.outcome.status == task::TaskStatus::NeedsUser {
                    let notification = notify::notify_user_interaction(
                        &config,
                        &report.task_path,
                        &report.outcome.recap_path,
                    )?;
                    println!(
                        "user interaction required for {}; notification={}",
                        report.task_path.display(),
                        notification.display()
                    );
                    Some(notification)
                } else {
                    None
                };
                if config.git.auto_commit {
                    if let Some(project) = report.project.as_deref() {
                        commit_agent_files_for_task(
                            &report.task_path,
                            project,
                            &report.outcome.files_touched,
                        );
                    }
                    git::commit_task_update(
                        &report.task_path,
                        &report.outcome.recap_path,
                        &report.outcome.session_log_path,
                        notification.as_deref(),
                    )?;
                }
            }
            Err(error) => {
                failures += 1;
                eprintln!("task failed: {error:#}");
            }
        }
    }

    if config.git.auto_commit {
        println!("committed task updates");
    }
    if failures > 0 {
        anyhow::bail!("{failures} task(s) failed");
    }
    Ok(())
}

struct ParallelRunReport {
    task_path: PathBuf,
    agent: String,
    glob: String,
    outcome: runner::RunOutcome,
    project: Option<String>,
}

#[derive(Clone)]
struct VardaSubtaskLauncher {
    config: config::Config,
    project_path: PathBuf,
    fallback_agent: String,
    spawn_state: orchestration::SharedSpawnState,
    /// Registry of isolated per-worker checkouts (subtask id → worktree/branch),
    /// shared with the broker's `integrate_subtasks` tool. The launcher records
    /// each worktree it creates here; the resident harvests them back at merge
    /// time. A subtask absent from the map ran on the shared mount (non-git
    /// mother → degrade).
    worker_registry: orchestration::WorkerRegistry,
}

impl orchestration::SubtaskLauncher for VardaSubtaskLauncher {
    fn launch(
        &mut self,
        req: &orchestration::SpawnRequest,
        grant: &orchestration::SpawnGrant,
    ) -> anyhow::Result<orchestration::SubtaskId> {
        let handle = tokio::runtime::Handle::try_current()
            .context("spawn_subtask launch requires a Tokio runtime")?;
        if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread {
            anyhow::bail!("detached spawn_subtask launch requires Tokio's multi-thread runtime");
        }
        let project = req
            .route
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.project_path.clone());
        let assignee = req.agent.as_deref().unwrap_or(&self.fallback_agent);

        // Preflight route resolution BEFORE creating/spawning the subtask. A
        // spawned worker is run through normal route resolution, which enforces
        // the route's `agents` allowlist and sandbox. If the requested agent
        // isn't runnable at the target path, fail the spawn LOUDLY here so the
        // master's `spawn_subtask` call returns an error it can react to —
        // otherwise the subtask sits at `ready` forever and an `await_subtasks`
        // on it deadlocks (the resident route listing only `claude-resident`
        // while the broker spawns `claude` workers did exactly this).
        routing::match_route(&self.config, &project, Some(assignee)).with_context(|| {
            format!(
                "cannot spawn subtask: agent '{assignee}' is not runnable at '{}'. \
                 Add it to that route's `agents`, or request a permitted agent.",
                project.display()
            )
        })?;

        // Resolve + preflight the worker sandbox BEFORE creating anything, so a
        // misconfigured sandbox fails the spawn LOUDLY (like the agent check above)
        // instead of stranding a task/worktree. An explicit `sandbox=` request wins;
        // else the route's `default_worker_sandbox`; else `None` (route resolution).
        // Pinning it onto the subtask's frontmatter (below) takes highest precedence
        // in `resolve_sandbox_for`, so the worker lands in a box that can actually
        // build/reach its model API rather than the route's LLM-only resident box —
        // the failure that stranded #642/#649/#636.
        let orch_policy = self.config.resolve_orchestration_for(&project);
        let spawn_sandbox =
            orchestration::spawn_sandbox_override(req.sandbox.as_deref(), &orch_policy);

        // `authorize_and_record` (in `gated_launch`) only re-checks `allow_agents`/
        // `deny_agents`/`allow_sandboxes`/`deny_sandboxes` when the caller passed
        // `req.agent`/`req.sandbox` explicitly. `assignee`/`spawn_sandbox` above may
        // instead be a FALLBACK (`self.fallback_agent`, `default_worker_sandbox`)
        // that policy never saw — re-run the same allow/deny check against the
        // EFFECTIVE values so a fallback can never silently bypass `deny_sandboxes`
        // (which defaults to denying `local`) or `deny_agents`.
        orchestration::check_effective_placement(
            &orch_policy,
            assignee,
            spawn_sandbox.as_deref(),
        )
        .map_err(|denied| anyhow::anyhow!("cannot spawn subtask: {denied}"))?;

        if let Some(sandbox) = &spawn_sandbox
            && !self.config.sandboxes.contains_key(sandbox)
        {
            anyhow::bail!(
                "cannot spawn subtask: worker sandbox '{sandbox}' is not defined as a central \
                 [sandboxes.{sandbox}] (from the spawn request or \
                 [routes.orchestration].default_worker_sandbox). Define it, or request a sandbox \
                 that exists."
            );
        }

        let short = uuid::Uuid::new_v4().to_string();
        let task_name = format!("spawned-subtask-{}", &short[..8]);
        // `create_task` is ALWAYS called with the MOTHER path (`project`): it uses
        // this both for the home-store folder and to locate the repo DEFINITION
        // store. If it were called with the worktree, every subtask would write
        // its definition INTO the worktree, where it would be committed onto that
        // worker's own `wip/` branch and self-replicate through merge-back. So we
        // create against the mother, then (below) point the LOADED doc's `project`
        // at the isolated worktree while recording the mother in `mother_project`.
        let task_path = task::create_task(
            &self.config,
            &task_name,
            &project,
            Some(assignee),
            Some(&req.brief),
            None,
        )
        .context("failed to create spawned subtask")?;
        let mut task_doc = task::load_task(&task_path)?;

        // Pin the resolved sandbox onto the subtask (highest precedence in
        // `resolve_sandbox_for`) so it overrides the route's sandbox. Preflighted
        // above, so this name is known to resolve. See #636.
        if let Some(sandbox) = spawn_sandbox {
            task_doc.frontmatter.sandbox = Some(sandbox);
        }

        // §2 — per-worker isolation. Create a `git worktree add -b wip/<slug>` off
        // the mother's HEAD at a distinct out-of-tree host path, mount THAT into
        // the worker (via its `project`), and record the mother in `mother_project`
        // so POLICY (route/sandbox/orchestration) still keys on the mother while
        // the worker edits files in its own worktree/branch. DEGRADE gracefully:
        // `create_worker_worktree` fails outside a git repo, so a non-git mother
        // falls back to today's shared-mount behaviour (no worktree, no override).
        let worktree_slug = format!("{}-{}", task_name, &short[..8]);
        let worker_checkout = match worker_worktree_path(&worktree_slug) {
            Ok(worktree_path) => {
                match git::create_worker_worktree(&project, &worktree_slug, &worktree_path) {
                    Ok(checkout) => {
                        task_doc.frontmatter.project = Some(checkout.path.display().to_string());
                        task_doc.frontmatter.mother_project =
                            Some(project.display().to_string());
                        Some(checkout)
                    }
                    Err(error) => {
                        eprintln!(
                            "warning: worker isolation unavailable for '{}' \
                             (falling back to the shared mount): {error:#}",
                            project.display()
                        );
                        None
                    }
                }
            }
            Err(error) => {
                eprintln!(
                    "warning: could not allocate a worker worktree path \
                     (falling back to the shared mount): {error:#}"
                );
                None
            }
        };

        task_doc.set_status(task::TaskStatus::Ready);
        task::write_task(&task_doc)?;
        let subtask_id = task_doc
            .frontmatter
            .id
            .map(|id| id.to_string())
            .unwrap_or_else(|| task_path.display().to_string());

        // Record the isolated checkout so the resident's `integrate_subtasks` tool
        // can harvest and merge this worker's branch back later. Keyed by the same
        // subtask id the broker returns and the master `await`s on.
        if let Some(checkout) = worker_checkout {
            self.worker_registry.record(subtask_id.clone(), checkout);
        }

        let lineage = SpawnLineage {
            root_id: subtask_id.clone(),
            root_depth: grant.child_depth,
            state: self.spawn_state.clone(),
        };
        let config = self.config.clone();
        let path = task_path.clone();
        let child_id = subtask_id.clone();
        let child_handle = tokio::spawn(run_spawned_subtask_settling(
            config, path, lineage, child_id, "spawned",
        ));
        self.spawn_state
            .insert_handle(subtask_id.clone(), child_handle);

        Ok(subtask_id)
    }

    fn run_existing(
        &mut self,
        task_id: &str,
        grant: &orchestration::SpawnGrant,
    ) -> anyhow::Result<orchestration::SubtaskId> {
        let handle = tokio::runtime::Handle::try_current()
            .context("run_subtask launch requires a Tokio runtime")?;
        if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::CurrentThread {
            anyhow::bail!("detached run_subtask launch requires Tokio's multi-thread runtime");
        }

        // Resolve the caller-supplied id to an EXISTING task's STATE file. A numeric
        // id maps through the home store; anything that resolves to no task is a
        // spoofed / out-of-scope id and fails LOUDLY here (never trusted).
        let task_path = task::resolve_task_reference(&self.config, Path::new(task_id))
            .with_context(|| format!("cannot run subtask: no task resolves to id '{task_id}'"))?;
        let mut task_doc = task::load_task(&task_path)?;

        // Preflight route resolution BEFORE running, exactly like `launch`: this
        // re-validates the EXISTING task's own agent/sandbox against the route's
        // `agents` allowlist and isolating-sandbox requirement, so an unrunnable
        // placement surfaces as an error the master can react to instead of a
        // subtask wedged at `ready`. The id is caller-supplied — its stored
        // frontmatter is never trusted without this re-validation.
        routing::match_route_for_task(&self.config, &task_doc, false).with_context(|| {
            format!(
                "cannot run subtask '{task_id}': its agent/sandbox is not runnable. \
                 Fix the task's route/agent, or run a permitted task."
            )
        })?;

        let subtask_id = task_doc
            .frontmatter
            .id
            .map(|id| id.to_string())
            .unwrap_or_else(|| task_path.display().to_string());

        // Runtime collision guard: refuse to re-run a task that is CURRENTLY
        // executing in this process — a second run would clobber its `JoinHandle`
        // and orphan the live child. A finished/leftover handle (or a task merely
        // marked `running` in frontmatter by a crashed prior run) is NOT live and
        // may be re-run. Frontmatter status is otherwise irrelevant: it is
        // normalized to `Ready` below, so the caller never needs a separate
        // "prepare the task" tool — any state is runnable.
        if self.spawn_state.has_live_handle(&subtask_id) {
            anyhow::bail!(
                "task '{subtask_id}' is already running in this session; refusing to double-run it"
            );
        }

        task_doc.set_status(task::TaskStatus::Ready);
        task::write_task(&task_doc)?;

        let lineage = SpawnLineage {
            root_id: subtask_id.clone(),
            root_depth: grant.child_depth,
            state: self.spawn_state.clone(),
        };
        let config = self.config.clone();
        let path = task_path.clone();
        let child_id = subtask_id.clone();
        let child_handle = tokio::spawn(run_spawned_subtask_settling(
            config, path, lineage, child_id, "run",
        ));
        self.spawn_state
            .insert_handle(subtask_id.clone(), child_handle);

        Ok(subtask_id)
    }
}

async fn join_spawned_subtasks(spawn_state: &orchestration::SharedSpawnState) {
    loop {
        let handles = spawn_state.drain_handles();
        if handles.is_empty() {
            break;
        }
        for (subtask_id, handle) in handles {
            match handle.await {
                Ok(()) => {}
                Err(error) if error.is_cancelled() => {
                    eprintln!("warning: spawned subtask {subtask_id} was cancelled");
                }
                Err(error) => {
                    eprintln!("warning: spawned subtask {subtask_id} join failed: {error:#}");
                }
            }
        }
    }
}

async fn abort_spawned_subtasks(spawn_state: &orchestration::SharedSpawnState) {
    for (subtask_id, handle) in spawn_state.drain_handles() {
        handle.abort();
        if let Err(error) = handle.await {
            if !error.is_cancelled() {
                eprintln!(
                    "warning: spawned subtask {subtask_id} join failed after abort: {error:#}"
                );
            }
        }
    }
}

async fn finish_spawned_subtasks(
    spawn_state: &orchestration::SharedSpawnState,
    is_root_run: bool,
    run_succeeded: bool,
) {
    if !is_root_run {
        return;
    }

    if run_succeeded {
        join_spawned_subtasks(spawn_state).await;
    } else {
        abort_spawned_subtasks(spawn_state).await;
    }
}

/// Host-side collect channel: resolves a spawned subtask id to its STATE
/// (status + recap text) from the `~/.varda` home store. Injected into the
/// [`orchestration::SpawnBroker`] so a sandboxed master's `await_subtask*` /
/// `subtask_result` calls can harvest a child's result without any host
/// capability crossing the boundary. The resident (un-sandboxed) host reuses
/// this SAME impl directly, so it is public and holds only the config it needs.
#[derive(Clone)]
pub struct VardaSubtaskResults {
    config: config::Config,
}

impl VardaSubtaskResults {
    pub fn new(config: config::Config) -> Self {
        Self { config }
    }

    /// Shared id→STATE resolution: a subtask id is the numeric task id the
    /// launcher assigned. A non-numeric id (fallback path form) never matches.
    fn state(&self, id: &str) -> Option<(task::TaskStatus, Option<String>)> {
        let num = id.parse::<u64>().ok()?;
        task::lookup_task_state(&self.config, num).ok().flatten()
    }
}

impl orchestration::SubtaskResults for VardaSubtaskResults {
    /// Resolves `id` and distinguishes "found" from a genuine resolution
    /// failure (unknown id, non-numeric id, ambiguous duplicate, or a failed
    /// state load) instead of collapsing all of those into `None` the way the
    /// old `Option`-returning signature did — that conflation is what let
    /// `await_subtask` poll an unresolvable id to its 30-minute ceiling (#653).
    fn status(&self, id: &str) -> orchestration::SubtaskStatus {
        let Ok(num) = id.parse::<u64>() else {
            return orchestration::SubtaskStatus::Unresolved(format!(
                "subtask id '{id}' is not a resolvable numeric task id"
            ));
        };
        match task::lookup_task_state(&self.config, num) {
            Ok(Some((status, _))) => orchestration::SubtaskStatus::Found(status),
            Ok(None) => {
                orchestration::SubtaskStatus::Unresolved(format!("no task found with id {num}"))
            }
            Err(error) => orchestration::SubtaskStatus::Unresolved(format!(
                "failed to resolve task {num}: {error:#}"
            )),
        }
    }

    fn recap(&self, id: &str) -> Option<String> {
        let (_, recap_path) = self.state(id)?;
        std::fs::read_to_string(recap_path?).ok()
    }

    /// Unified diff of the subtask's worktree (its uncommitted output). Resolves the
    /// subtask's `project` (its isolated worktree) and diffs it HOST-side, where git
    /// and the worktree are local — so a reviewer never needs git or a mount of the
    /// reviewed worker's box.
    fn diff(&self, id: &str) -> Option<String> {
        let num = id.parse::<u64>().ok()?;
        let path = task::find_task_by_id(&self.config, num).ok().flatten()?;
        let fm = task::load_task(&path).ok()?.frontmatter;
        subtask_worktree_diff(fm.project.as_deref()?, fm.mother_project.as_deref())
    }
}

/// Compute the unified diff of a worker's full contribution — everything its
/// worktree differs from the mother branch-point — INCLUDING new files. Diffs
/// against `merge-base(worktree HEAD, mother HEAD)` so it captures the changes
/// whether they are still uncommitted OR already committed onto the wip/ branch
/// (integrate commits `files_touched` there); falls back to `HEAD` (uncommitted
/// only) when the mother/merge-base can't be resolved. `add -N` renders untracked
/// files; `reset` restores the index so nothing is left mutated for integration.
fn subtask_worktree_diff(project: &str, mother: Option<&str>) -> Option<String> {
    let git_out = |dir: &str, args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    let base = mother
        .and_then(|m| git_out(m, &["rev-parse", "HEAD"]))
        .and_then(|head| git_out(project, &["merge-base", "HEAD", &head]))
        .unwrap_or_else(|| "HEAD".to_owned());
    let output = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "git add -A -N >/dev/null 2>&1; git diff {base}; git reset -q >/dev/null 2>&1"
        ))
        .current_dir(project)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Host-side task control-plane seam (task #640): resolves `list_tasks` /
/// `get_task` / `set_task_status` against the `~/.varda` home store through
/// the `task::` helpers. The broker calls this ONLY with the caller's own
/// `project_path` (never attacker-supplied — see
/// `orchestration::SpawnBroker::with_task_control_plane`), so scoping is
/// enforced by construction: every method reuses `task::list_tasks`, which
/// already filters by project.
#[derive(Clone)]
pub struct VardaTaskControlPlane {
    config: config::Config,
}

impl VardaTaskControlPlane {
    pub fn new(config: config::Config) -> Self {
        Self { config }
    }

    fn find(&self, project_path: &Path, id: u64) -> Option<task::TaskSummary> {
        task::list_tasks(&self.config, project_path)
            .ok()?
            .into_iter()
            .find(|t| t.id == Some(id))
    }
}

/// The task-file basename minus its extension, used as the caller-facing
/// `slug` for a task (the home store names STATE files `<slug>.md`, with no
/// id prefix — unlike the repo-local `<id>-<slug>.md` DEFINITION naming).
fn task_slug(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

impl orchestration::TaskControlPlane for VardaTaskControlPlane {
    fn list_tasks(
        &self,
        project_path: &Path,
        status: Option<task::TaskStatus>,
    ) -> Vec<orchestration::TaskListEntry> {
        let Ok(summaries) = task::list_tasks(&self.config, project_path) else {
            return Vec::new();
        };
        summaries
            .into_iter()
            .filter(|t| t.id.is_some())
            .filter(|t| status.is_none_or(|status| t.status == status))
            .map(|t| orchestration::TaskListEntry {
                id: t.id.expect("filtered to Some above"),
                slug: task_slug(&t.path),
                status: t.status.as_str().to_owned(),
                title: t.title,
                assignee: t.assignee,
            })
            .collect()
    }

    fn get_task(&self, project_path: &Path, id: u64) -> Option<orchestration::TaskDetail> {
        let summary = self.find(project_path, id)?;
        let doc = task::load_task(&summary.path).ok()?;
        Some(orchestration::TaskDetail {
            id,
            slug: task_slug(&summary.path),
            status: doc.frontmatter.status.as_str().to_owned(),
            title: doc.title(),
            assignee: doc.frontmatter.assignee.clone(),
            body: doc.body.clone(),
        })
    }

    fn set_task_status(
        &self,
        project_path: &Path,
        id: u64,
        status: task::TaskStatus,
    ) -> Result<(), String> {
        let summary = self
            .find(project_path, id)
            .ok_or_else(|| format!("task '{id}' not found in this project"))?;
        let mut doc = task::load_task(&summary.path).map_err(|error| error.to_string())?;
        doc.set_status(status);
        task::write_task(&doc).map_err(|error| error.to_string())
    }
}

struct OrchestratedAgentClient<C: AgentClient = acp::AcpSubprocessClient> {
    inner: C,
    config: config::Config,
    policy: orchestration::OrchestrationPolicy,
    project_path: PathBuf,
    fallback_agent: String,
    lineage: Option<SpawnLineage>,
    /// Resolved sandbox primitive for this run (`local`/`docker`/`microsandbox`/
    /// `clawk`). Selects the broker transport: own-kernel microVMs cannot reach a
    /// bind-mounted Unix socket, so they get the TCP transport
    /// ([`config::primitive_needs_tcp_broker`]); everything else keeps the socket.
    sandbox_primitive: String,
}

/// Host IP the broker TCP listener binds to for a microVM guest. Loopback by
/// default (host-only, never a public interface); override with
/// `VARDA_BROKER_BIND_IP` to the per-sandbox gateway the guest actually reaches
/// (e.g. an msb bridge gateway `172.16.0.x`). Kept as a host-only bind per the
/// orchestration isolation invariants — the broker is capability-gated, so the
/// port grants no capability, but it is still never exposed on `0.0.0.0`.
fn broker_bind_ip() -> std::net::IpAddr {
    let loopback = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
    std::env::var("VARDA_BROKER_BIND_IP")
        .ok()
        .and_then(|raw| raw.trim().parse::<std::net::IpAddr>().ok())
        // REJECT wildcard/unspecified addresses (`0.0.0.0`, `::`): binding there
        // would expose the capability-gated broker on ALL interfaces (public/LAN),
        // violating the host-only invariant. A misconfigured env var must not widen
        // exposure — fall back to loopback.
        .filter(|ip| {
            if ip.is_unspecified() {
                eprintln!(
                    "warning: VARDA_BROKER_BIND_IP={ip} is a wildcard address; refusing to bind the broker on all interfaces — falling back to loopback. Set it to the per-sandbox gateway IP instead."
                );
                false
            } else {
                true
            }
        })
        .unwrap_or(loopback)
}

/// Resolve the effective sandbox primitive for the run at `project_path`, used to
/// select the broker transport. Falls back to `local` (the Unix-socket transport)
/// if resolution fails — a safe default that never mis-serves a microVM guest over
/// an unreachable socket without at least the broker being harmless on loopback.
fn resolve_sandbox_primitive(
    config: &config::Config,
    project_path: &Path,
    pinned: Option<&str>,
) -> String {
    let routing_root = routing_root_for(project_path);
    config
        .resolve_sandbox_for(project_path, &routing_root, pinned)
        .map(|resolved| resolved.config.primitive)
        .unwrap_or_else(|_| "local".to_owned())
}

#[derive(Clone)]
struct SpawnLineage {
    root_id: orchestration::SubtaskId,
    root_depth: u32,
    state: orchestration::SharedSpawnState,
}

#[async_trait]
impl<C: AgentClient> AgentClient for OrchestratedAgentClient<C> {
    async fn run_task(&self, mut request: agent::AgentRunRequest) -> Result<agent::AgentRunResult> {
        // An interactive orchestrated run (the long-lived resident) DOES get a broker:
        // the socket is served for the whole session and torn down root-only after
        // outstanding children join (see the teardown below). Only `interpret` (a
        // read-only recap pass) and a policy-disabled run short-circuit with no broker.
        if !self.policy.enabled || request.interpret {
            return self.inner.run_task(request).await;
        }

        let socket_dir = self.project_path.join(".varda-mcp");
        let socket_path = socket_dir.join(format!("{}.sock", request.session_id));
        let default_root_id = request
            .frontmatter
            .id
            .map(|id| id.to_string())
            .unwrap_or_else(|| request.session_id.clone());
        let (root_id, root_depth, spawn_state) = self.lineage.as_ref().map_or_else(
            || (default_root_id, 0, orchestration::SharedSpawnState::new()),
            |lineage| {
                (
                    lineage.root_id.clone(),
                    lineage.root_depth,
                    lineage.state.clone(),
                )
            },
        );
        // Shared worker registry: the launcher records each isolated worktree it
        // creates, and the broker's `integrate_subtasks` tool harvests them back
        // for the host-side merge. One instance threaded through both halves so
        // they observe the same map.
        let worker_registry = orchestration::WorkerRegistry::new();
        let launcher = VardaSubtaskLauncher {
            config: self.config.clone(),
            project_path: self.project_path.clone(),
            fallback_agent: self.fallback_agent.clone(),
            spawn_state: spawn_state.clone(),
            worker_registry: worker_registry.clone(),
        };
        let broker = std::sync::Arc::new(
            orchestration::SpawnBroker::with_shared_state(
                self.policy.clone(),
                root_id.clone(),
                root_depth,
                spawn_state.clone(),
                launcher,
            )
            .with_results(VardaSubtaskResults::new(self.config.clone()))
            // The resident's own mounted workspace is the integration worktree each
            // worker branch is merged onto (WORKFLOW.md step 5). Merge-back is
            // local-only: the resident has no push credentials (G2/G3).
            .with_integration(worker_registry, self.project_path.clone())
            // Task control-plane tools (task #640): scoped to this run's OWN
            // project, exactly like the integration worktree above.
            .with_task_control_plane(
                VardaTaskControlPlane::new(self.config.clone()),
                self.project_path.clone(),
            ),
        );
        // Transport selection by primitive. Own-kernel microVMs (microsandbox/clawk)
        // share the project tree over virtio-fs, which exposes the socket FILE but
        // not its AF_UNIX endpoint, so an in-guest connect() is refused; those guests
        // reach the host over TCP instead. `local`/`docker` see the real socket
        // through the bind mount and keep the Unix transport.
        let use_tcp = config::primitive_needs_tcp_broker(&self.sandbox_primitive);
        let server = if use_tcp {
            let (addr, listener) = match mcp_transport::bind_tcp(broker_bind_ip()).await {
                Ok(bound) => bound,
                Err(error) => {
                    eprintln!("warning: MCP broker TCP bind failed: {error:#}");
                    return self.inner.run_task(request).await;
                }
            };
            request.orchestration_addr = Some(addr.to_string());
            tokio::spawn(async move {
                if let Err(error) = mcp_transport::serve_tcp(listener, root_id, broker).await {
                    eprintln!("warning: MCP broker transport exited: {error:#}");
                }
            })
        } else {
            let server_path = socket_path.clone();
            request.orchestration_socket_path = Some(socket_path.display().to_string());
            tokio::spawn(async move {
                if let Err(error) =
                    mcp_transport::serve_unix_socket(&server_path, root_id, broker).await
                {
                    eprintln!("warning: MCP broker transport exited: {error:#}");
                }
            })
        };

        let is_root_run = self.lineage.is_none();
        let result = self.inner.run_task(request).await;
        finish_spawned_subtasks(&spawn_state, is_root_run, result.is_ok()).await;
        server.abort();
        if !use_tcp {
            let _ = std::fs::remove_file(&socket_path);
            let _ = std::fs::remove_dir(&socket_dir);
        }
        result
    }
}

/// Build an ACP client for `display_name`, injecting the resolved sandbox
/// provider.
///
/// When `project_path` is `Some`, the sandbox is resolved through the live
/// `.varda` path ([`config::Config::resolve_sandbox_for`]): the nearest `.varda`
/// (walked up to the routing root) wins over the central route, the untrusted
/// `.varda` origin is clamped by the hardening floor, and the three mount origins
/// (`Sandbox`/`Route`/`Varda`) are merged into the provider. When `project_path`
/// is `None` (no project context, e.g. plan transformation) the trusted-only
/// by-name path is used with `sandbox_name`/`route_mounts`. `local` yields the
/// identity provider; any other name must have a matching `[sandboxes.<name>]`.
#[allow(clippy::too_many_arguments)]
fn build_client(
    config: &config::Config,
    display_name: &str,
    agent_config: &config::AgentConfig,
    sandbox_name: &str,
    route_mounts: &[String],
    route_env: &std::collections::BTreeMap<String, String>,
    project_path: Option<&Path>,
    policy_path: Option<&Path>,
    pinned_sandbox: Option<&str>,
) -> Result<acp::AcpSubprocessClient> {
    // M11 — resolve the three identity/auth channels (curated identity files,
    // SSH-agent + git identity, scoped auth token) once and inject them into
    // whichever provider is selected. `local` ignores them (no boundary to cross).
    let identity = resolve_sandbox_identity(config, agent_config)?;
    let mut static_env = std::collections::BTreeMap::new();
    // Keys from the UNTRUSTED `.varda` origin, retained so `resolve_env_secrets`
    // refuses a fnox binding from repo-committed config (see its doc).
    let mut untrusted_env_keys: Vec<String> = Vec::new();
    let provider = match project_path {
        Some(project_path) => {
            // POLICY vs MOUNT split (task #598): the sandbox/route/`.varda` policy
            // is resolved against `policy_path` — the MOTHER repo root for an
            // isolated worker whose `project_path` is an out-of-tree worktree —
            // while the returned mounts are later `{project}`-expanded against the
            // worktree (`project_path`). When there is no separate mother
            // (`policy_path` is `None`), policy resolution keys on `project_path`,
            // so a non-orchestrated run behaves exactly as before. The mother must
            // be threaded explicitly: deriving it from the worktree via
            // `routing_root_for` would return the WORKTREE root, not the mother.
            let policy_path = policy_path.unwrap_or(project_path);
            let routing_root = routing_root_for(policy_path);
            let resolved =
                config.resolve_sandbox_for(policy_path, &routing_root, pinned_sandbox)?;
            enforce_varda_env_credential_floor(agent_config, &resolved)?;
            if let Some(varda_file) = &resolved.varda_file {
                eprintln!(
                    "sandbox: '{}' selected via {}",
                    resolved.name,
                    varda_file.display()
                );
            }
            let mounts = sandbox::merge_mount_origins(
                &resolved.config.mounts,
                &resolved.route_mounts,
                &resolved.varda_mounts,
            );
            untrusted_env_keys = resolved.varda_env_keys.clone();
            static_env = resolved.env;
            sandbox::provider_from_config(&resolved.name, &resolved.config, mounts, &identity)?
        }
        None => {
            if let Some(sandbox_config) = config.sandboxes.get(sandbox_name) {
                static_env.extend(sandbox_config.env.clone());
            }
            static_env.extend(route_env.clone());
            sandbox::provider_for(sandbox_name, &config.sandboxes, route_mounts, &identity)?
        }
    };
    // Resolve `${fnox:NAME}` bindings on the HOST at prepare time, injecting only the
    // resolved value. Static env carries the (possibly untrusted `.varda`) sandbox/route
    // origins; agent env is always a trusted central origin, so no key is untrusted.
    resolve_env_secrets(&mut static_env, &untrusted_env_keys)?;
    let mut agent_config = agent_config.clone();
    resolve_env_secrets(&mut agent_config.env, &[])?;
    Ok(acp::AcpSubprocessClient::with_sandbox_env(
        display_name,
        &agent_config,
        provider,
        static_env,
    ))
}

fn enforce_varda_env_credential_floor(
    agent_config: &config::AgentConfig,
    resolved: &config::ResolvedSandbox,
) -> Result<()> {
    // Every credential ENV target (the legacy pair folds into this list) is a
    // credential-injection sink; a `.varda` may not shadow one. File targets are
    // guest paths, not env keys, so they cannot collide with `varda_env_keys`.
    for cred in agent_config.effective_credentials() {
        let Ok(config::CredentialTarget::Env(target)) = cred.target() else {
            continue;
        };
        if resolved.varda_env_keys.iter().any(|key| key == target) {
            let origin = resolved
                .varda_file
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| ".varda".to_owned());
            anyhow::bail!(
                "`.varda` at {origin} declares env key '{target}', which would override credential injection"
            );
        }
    }
    for key in &resolved.varda_env_keys {
        if agent_config.env.contains_key(key) {
            let origin = resolved
                .varda_file
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| ".varda".to_owned());
            anyhow::bail!(
                "`.varda` at {origin} declares env key '{key}', which would override trusted agent env"
            );
        }
    }
    Ok(())
}

/// M11 — assemble the sandbox identity/auth bundle from central config + the host
/// environment. Three separable, opt-in channels; nothing is forwarded by default.
///
/// - **Auth token** (`[agents.X].auth_token_env`): the NAME of a host env var
///   holding a dedicated, scoped, rotatable sandbox token. Its value is read from
///   the environment (never a repo secret) and re-exported into the box as
///   `auth_token_target` (defaulting to the same name). Missing/empty ⇒ skipped
///   with a warning, so the sandbox still boots (it just won't be authenticated).
/// - **SSH-agent + git identity** (`defaults.forward_ssh_agent`,
///   `defaults.git_user_name`/`git_user_email`): forward `$SSH_AUTH_SOCK` (only
///   when a live socket exists) and the read-only git identity.
/// - **Curated identity files** (`defaults.identity_context`): passed through
///   verbatim; validated read-only + credential-denylisted at wrap time.
fn resolve_sandbox_identity(
    config: &config::Config,
    agent_config: &config::AgentConfig,
) -> Result<sandbox::SandboxIdentity> {
    let defaults = &config.defaults;
    // M11-ext — resolve the effective credential list (explicit `credentials`
    // entries plus the legacy `auth_token_env`/`auth_token_target` sugar) into the
    // two injection channels: scoped in-box env vars and read-only staged files.
    // Everything is minted HOST-side; only the scoped value crosses the boundary.
    let (auth_env, auth_files) = resolve_agent_credentials(agent_config)?;

    // Forward the SSH agent socket only when forwarding is enabled AND a live
    // socket exists on the host; otherwise the mount source would be missing.
    let ssh_auth_sock = if defaults.forward_ssh_agent {
        match std::env::var("SSH_AUTH_SOCK") {
            Ok(sock) if !sock.is_empty() && Path::new(&sock).exists() => Some(sock),
            _ => {
                eprintln!(
                    "sandbox: forward_ssh_agent is set but no live $SSH_AUTH_SOCK on the host; \
                     git push over SSH will not work in the box"
                );
                None
            }
        }
    } else {
        None
    };

    let identity = sandbox::SandboxIdentity {
        identity_context: defaults.identity_context.clone(),
        ssh_auth_sock,
        git_name: defaults.git_user_name.clone(),
        git_email: defaults.git_user_email.clone(),
        auth_env,
        auth_files,
    };
    if !identity.is_empty() {
        eprintln!(
            "sandbox: identity channels active — cred_env:{} cred_files:{} ssh_agent:{} git_identity:{} identity_files:{}",
            identity.auth_env.len(),
            identity.auth_files.len(),
            identity.ssh_auth_sock.is_some(),
            identity.git_name.is_some() || identity.git_email.is_some(),
            identity.identity_context.len(),
        );
    }
    Ok(identity)
}

/// M11-ext — resolve an agent's effective credential list into the two injection
/// channels: `(auth_env, auth_files)` where `auth_env` maps in-box env var names to
/// scoped values and `auth_files` maps absolute guest paths to scoped values. Each
/// entry is validated (exactly one source, exactly one target) and minted HOST-side.
fn resolve_agent_credentials(
    agent_config: &config::AgentConfig,
) -> Result<(
    std::collections::BTreeMap<String, String>,
    std::collections::BTreeMap<String, String>,
)> {
    let mut auth_env = std::collections::BTreeMap::new();
    let mut auth_files = std::collections::BTreeMap::new();
    for cred in agent_config.effective_credentials() {
        let source = cred.source()?;
        let target = cred.target()?;
        let Some(value) = resolve_credential_value(&source)? else {
            continue;
        };
        match target {
            config::CredentialTarget::Env(name) => {
                auth_env.insert(name.to_owned(), value);
            }
            config::CredentialTarget::File(path) => {
                auth_files.insert(path.to_owned(), value);
            }
        }
    }
    Ok((auth_env, auth_files))
}

/// Mint a single credential value on the HOST. Returns `Ok(None)` when a
/// `from_env`/`from_secret` source is unset/empty (skip the injection so the box
/// still boots, matching the legacy `auth_token_env` behavior). `command` and
/// secret-store failures fail loudly — the minting identity never reaches the box,
/// so a broken mint must not silently degrade to an unauthenticated run.
fn resolve_credential_value(source: &config::CredentialSource<'_>) -> Result<Option<String>> {
    match source {
        config::CredentialSource::Env(name) => match std::env::var(name) {
            Ok(value) if !value.is_empty() => Ok(Some(value)),
            _ => {
                eprintln!(
                    "sandbox: credential source env '{name}' is unset/empty on the host; skipping \
                     this injection (set a dedicated, scoped sandbox token)"
                );
                Ok(None)
            }
        },
        config::CredentialSource::Secret(name) => {
            let value = run_host_credential_command("fnox", &["get", name]).with_context(|| {
                format!("failed to resolve secret '{name}' from the host secret store (`fnox get {name}`)")
            })?;
            if value.is_empty() {
                anyhow::bail!("secret '{name}' resolved to an empty value on the host");
            }
            Ok(Some(value))
        }
        config::CredentialSource::Command(cmd) => {
            let value = run_host_credential_command("sh", &["-c", cmd])
                .with_context(|| format!("credential command failed on the host: {cmd}"))?;
            if value.is_empty() {
                anyhow::bail!("credential command produced empty output on the host: {cmd}");
            }
            Ok(Some(value))
        }
    }
}

/// Resolve `${fnox:NAME}` bindings in a static env map on the HOST at prepare time,
/// replacing each sentinel value in place with the value `fnox get NAME` returns. Only
/// the resolved VALUE crosses the boundary; the agent/sandbox never contacts fnox and
/// never sees the sentinel. Non-sentinel values are left untouched.
///
/// `untrusted_keys` names env keys that originate from the repo-committed (UNTRUSTED)
/// `.varda` origin. A fnox binding on one of those is REFUSED: untrusted config must not
/// be able to bind an arbitrary host secret and exfiltrate it through the agent's env.
/// Trusted origins (central `[sandboxes.X].env`/`[[routes]].env`, `[agents.X].env`) may
/// bind freely. Missing/failed/empty fnox resolution fails the run loudly (redacted:
/// only the key and secret NAME are surfaced, never the value).
fn resolve_env_secrets(
    env: &mut std::collections::BTreeMap<String, String>,
    untrusted_keys: &[String],
) -> Result<()> {
    for (key, value) in env.iter_mut() {
        let Some(secret) = config::fnox_env_ref(value) else {
            continue;
        };
        let secret = secret.to_owned();
        if untrusted_keys.iter().any(|k| k == key) {
            anyhow::bail!(
                "env key '{key}' from an untrusted `.varda` binds fnox secret '{secret}'; \
                 fnox env bindings are only allowed from trusted central config"
            );
        }
        let resolved =
            run_host_credential_command("fnox", &["get", &secret]).with_context(|| {
                format!(
                    "failed to resolve env '{key}' from the host secret store (`fnox get {secret}`)"
                )
            })?;
        if resolved.is_empty() {
            anyhow::bail!(
                "env '{key}' fnox secret '{secret}' resolved to an empty value on the host"
            );
        }
        *value = resolved;
    }
    Ok(())
}

/// Run a host-side credential minting command and return its stdout with the
/// trailing newline trimmed. Never logs stdout (it is a secret); errors surface
/// only the command and its stderr.
fn run_host_credential_command(program: &str, args: &[&str]) -> Result<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn host credential command `{program}`"))?;
    if !output.status.success() {
        anyhow::bail!(
            "`{program} {}` exited with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim_end_matches(['\n', '\r'])
        .to_owned())
}

/// Bound for the upward `.varda` walk: the git repository root of `project_path`,
/// falling back to the project path itself when it is not inside a git repo.
fn routing_root_for(project_path: &Path) -> PathBuf {
    git::repo_root_for_path(project_path).unwrap_or_else(|_| project_path.to_path_buf())
}

/// Distinct out-of-tree host path for a per-worker isolated worktree, under
/// `<varda_home>/worktrees/wip-<slug>/`. Kept out of the mother tree so the
/// worktree never shadows or collides with the project checkout. The path must
/// NOT already exist (`create_worker_worktree` requires a fresh path); the `slug`
/// carries a per-spawn uuid suffix, so collisions are not expected, but a
/// lingering directory from a crashed prior run is removed first.
fn worker_worktree_path(slug: &str) -> Result<PathBuf> {
    let base = config::varda_home()?.join("worktrees");
    std::fs::create_dir_all(&base)
        .with_context(|| format!("failed to create worktree base {}", base.display()))?;
    let path = base.join(format!("wip-{slug}"));
    if path.exists() {
        std::fs::remove_dir_all(&path).with_context(|| {
            format!("failed to clear stale worktree path {}", path.display())
        })?;
    }
    Ok(path)
}

async fn run_task_path_for_parallel(
    config: config::Config,
    task_path: PathBuf,
    lineage: Option<SpawnLineage>,
) -> Result<ParallelRunReport> {
    let task_path = task::resolve_task_reference(&config, &task_path)?;
    let task_document = task::load_task(&task_path)?;
    let route = routing::match_route_for_task(&config, &task_document, false)?;
    let id = task_document
        .frontmatter
        .id
        .map(|id| format!("#{id}"))
        .unwrap_or_else(|| "unversioned".to_owned());
    println!(
        "dispatching {} {} → agent={}",
        id,
        task_document.title(),
        route.display_name()
    );
    let agent_config = config
        .agents
        .get(&route.agent)
        .expect("routing ensures the selected agent exists");
    let display_name = route.display_name().to_owned();
    let client = build_client(
        &config,
        &display_name,
        agent_config,
        &route.sandbox,
        &route.route_mounts,
        &route.route_env,
        task_document.frontmatter.project.as_deref().map(Path::new),
        task_document.frontmatter.mother_project.as_deref().map(Path::new),
        task_document.frontmatter.sandbox.as_deref(),
    )?;
    // POLICY reads (orchestration policy + sandbox-primitive/transport selection)
    // key on the MOTHER repo root via `policy_project`; MOUNT/cwd (the socket dir,
    // the launcher's project root) keeps `project`. For a non-orchestrated task
    // `policy_project` == `project`, so this is backward-compatible.
    let policy = task_document
        .frontmatter
        .policy_project()
        .map(|p| Path::new(p.as_str()))
        .map(|path| config.resolve_orchestration_for(path))
        .unwrap_or_else(|| config.orchestration.clone());
    let policy_path = task_document
        .frontmatter
        .policy_project()
        .map(|p| PathBuf::from(p.as_str()));
    let orchestrated_client = task_document
        .frontmatter
        .project
        .as_deref()
        .map(PathBuf::from)
        .filter(|_| policy.enabled)
        .map(|project_path| OrchestratedAgentClient {
            inner: client.clone(),
            config: config.clone(),
            policy,
            sandbox_primitive: resolve_sandbox_primitive(
                &config,
                policy_path.as_deref().unwrap_or(&project_path),
                task_document.frontmatter.sandbox.as_deref(),
            ),
            project_path,
            fallback_agent: route.agent.clone(),
            lineage,
        });
    let outcome = runner::run_task(
        &config,
        &display_name,
        route.role_instructions.as_deref(),
        &task_path,
        orchestrated_client
            .as_ref()
            .map_or(&client as &dyn AgentClient, |c| c as &dyn AgentClient),
        false,
        false,
    )
    .await?;

    Ok(ParallelRunReport {
        task_path,
        agent: route.display_name().to_owned(),
        glob: route.glob,
        outcome,
        project: task_document.frontmatter.project.clone(),
    })
}

/// Hard ceiling for a broker-spawned subtask's ENTIRE run (agent + interpreter +
/// teardown). A backstop far above the agent's own budget/idle-watchdog, so a wedged
/// box or a teardown that never returns cannot strand the subtask — and the master's
/// `await_subtask` — forever.
const SPAWNED_SUBTASK_HARD_CEILING: Duration = Duration::from_secs(60 * 60);

/// Run a broker-spawned subtask to completion and GUARANTEE it settles on a terminal
/// status, so an awaiting master never hangs. `runner::run_task` writes a terminal
/// status on a clean finish; this backstops every path that never reaches that write:
/// an uncaught `Err`, a PANIC (which unwinds past a plain `if let Err`), an abort, or a
/// wedged run/teardown that returns nothing. On any non-success it forces `Failed` when
/// the status is still non-terminal (never clobbering a real Done/Review/NeedsUser).
/// Observed live: spawned reviews/checks whose agent had already exited (box `stopped`)
/// yet stayed `running`, wedging the resident's `await_subtask` until hand-reconciled.
async fn run_spawned_subtask_settling(
    config: config::Config,
    path: PathBuf,
    lineage: SpawnLineage,
    child_id: String,
    label: &'static str,
) {
    let run = tokio::spawn(run_task_path_for_parallel(config, path.clone(), Some(lineage)));
    let abort = run.abort_handle();
    let settled_ok = match tokio::time::timeout(SPAWNED_SUBTASK_HARD_CEILING, run).await {
        Ok(Ok(Ok(_report))) => true, // run_task wrote a terminal status on a clean finish
        Ok(Ok(Err(error))) => {
            eprintln!("warning: {label} subtask {child_id} failed: {error:#}");
            false
        }
        Ok(Err(join)) => {
            eprintln!("warning: {label} subtask {child_id} panicked/aborted: {join}");
            false
        }
        Err(_elapsed) => {
            abort.abort(); // stop the wedged run so it can't leak in the background
            eprintln!(
                "warning: {label} subtask {child_id} exceeded the {}s hard ceiling; forcing terminal",
                SPAWNED_SUBTASK_HARD_CEILING.as_secs()
            );
            false
        }
    };
    if !settled_ok
        && let Ok(mut doc) = task::load_task(&path)
        && !doc.frontmatter.status.is_terminal()
    {
        doc.set_status(task::TaskStatus::Failed);
        let _ = task::write_task(&doc);
    }
}

fn show_task_command(task_path: &Path) -> Result<()> {
    let task_path = resolve_task_for_show(task_path)?;
    let task_content = fs::read_to_string(&task_path)
        .with_context(|| format!("failed to read task at {}", task_path.display()))?;
    let task_document = task::load_task(&task_path)?;

    println!("# Task {}", task_path.display());
    println!();
    print!("{task_content}");
    if !task_content.ends_with('\n') {
        println!();
    }

    println!();
    println!("---");
    println!();

    if task_document.frontmatter.recaps.is_empty() {
        println!("# Recap");
        println!();
        println!("No recap is associated with this task.");
        return Ok(());
    }

    for recap_path_str in &task_document.frontmatter.recaps {
        let recap_path = resolve_recap_path(recap_path_str, &task_path);
        let recap_content = fs::read_to_string(&recap_path)
            .with_context(|| format!("failed to read recap at {}", recap_path.display()))?;

        println!("# Recap {}", recap_path.display());
        println!();
        print!("{recap_content}");
        if !recap_content.ends_with('\n') {
            println!();
        }
        println!();
        println!("---");
        println!();
    }

    Ok(())
}

fn inspect_task_command(task_path: &Path) -> Result<()> {
    let config_path = config::config_file()?;
    let config = config::load_config(&config_path)?;
    let task_path = task::resolve_task_reference(&config, task_path)?;
    let task = task::load_task(&task_path)?;
    let fm = &task.frontmatter;

    println!("# Task {}", task_path.display());
    println!();
    println!(
        "status: {:?}  assignee: {}  project: {}",
        fm.status,
        fm.assignee.as_deref().unwrap_or("(none)"),
        fm.project.as_deref().unwrap_or("(none)")
    );
    println!();

    // Agent config
    let assignee = fm.assignee.as_deref().unwrap_or("(none)");
    if let Some(agent_cfg) = config.agents.get(assignee) {
        println!("## Agent config: {assignee}");
        println!("  command:     {} {:?}", agent_cfg.command, agent_cfg.args);
        match &agent_cfg.interactive_command {
            Some(cmd) => println!(
                "  interactive: {} {:?}  [configured]",
                cmd,
                agent_cfg.interactive_args.as_deref().unwrap_or(&[])
            ),
            None => println!("  interactive: (not configured — will fall back to pipe mode)"),
        }
        println!();
    }

    // Route
    match routing::match_route_for_task(&config, &task, false) {
        Ok(route) => {
            println!("## Route");
            println!("  glob:   {}", route.glob);
            println!("  agent:  {}", route.agent);
            println!();
        }
        Err(e) => {
            println!("## Route");
            println!("  (could not resolve: {e})");
            println!();
        }
    }

    // Sessions
    let n = fm.agent_session_ids.len();
    println!("## Sessions ({n})");
    if n == 0 {
        println!("  none");
    }
    let ps_output = std::process::Command::new("ps")
        .args(["aux"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();

    for (i, session_id) in fm.agent_session_ids.iter().enumerate() {
        let log_path = fm.agent_session_logs.get(i).map(|s| s.as_str());

        // Detect whether a process holding this session's working context is running.
        // We match on the task path appearing in ps output as a heuristic.
        let task_path_str = task_path.display().to_string();
        let live_pids: Vec<&str> = ps_output
            .lines()
            .filter(|l| l.contains(&task_path_str))
            .filter_map(|l| l.split_whitespace().nth(1))
            .collect();

        let live = if live_pids.is_empty() {
            "not running".to_owned()
        } else {
            format!("running (pid {})", live_pids.join(", "))
        };

        println!();
        println!("### Session {} — {live}", &session_id[..8]);
        if let Some(log) = log_path {
            println!("  log: {log}");
            match fs::read_to_string(log) {
                Ok(content) => {
                    println!("  ---");
                    for line in content.lines() {
                        println!("  {line}");
                    }
                    println!("  ---");
                }
                Err(e) => println!("  (could not read log: {e})"),
            }
        } else {
            println!("  (no log path recorded)");
        }
    }

    Ok(())
}

fn dashboard_task_command(
    project: Option<&Path>,
    all_projects: bool,
    web: bool,
    port: u16,
    daemon: bool,
    selected_task: Option<&Path>,
) -> Result<()> {
    let config_path = config::config_file()?;
    let config = config::load_config(&config_path)?;

    if daemon && !web {
        anyhow::bail!("--daemon requires --web");
    }

    if web {
        if daemon {
            spawn_dashboard_daemon(project, all_projects, port)?;
            return Ok(());
        }
        serve_task_dashboard(config, project.map(Path::to_path_buf), true, port)?;
        return Ok(());
    }

    let (scope, tasks) = if all_projects {
        ("all projects".to_owned(), task::list_all_tasks(&config)?)
    } else {
        let project_path = task::resolve_project_path(project)?;
        (
            project_path.display().to_string(),
            task::list_tasks(&config, &project_path)?,
        )
    };

    print_task_dashboard(&scope, &tasks);

    let selected = if let Some(selected_task) = selected_task {
        Some(selected_task.to_path_buf())
    } else {
        prompt_task_selection()?
    };

    if let Some(selected) = selected {
        println!();
        println!("---");
        println!();
        show_task_command(&selected)?;
    }

    Ok(())
}

#[derive(Debug, Serialize)]
struct DashboardPayload {
    scope: String,
    generated_at: u64,
    default_project: Option<String>,
    /// Host `$HOME`, so the UI can abbreviate it to `~` in displayed paths while
    /// keeping absolute paths as the data/API keys.
    home: Option<String>,
    tasks: Vec<DashboardTask>,
    projects: Vec<String>,
    statuses: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct DashboardTask {
    id: Option<u64>,
    status: &'static str,
    project: Option<String>,
    assignee: Option<String>,
    title: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<u64>,
}

#[derive(Debug, Serialize)]
struct DashboardTaskDetail {
    path: String,
    markdown: String,
    recaps: Vec<DashboardRecap>,
}

#[derive(Debug, Serialize)]
struct DashboardRecap {
    path: String,
    markdown: String,
}

#[derive(Debug, Deserialize)]
struct DashboardStatusUpdate {
    path: String,
    status: task::TaskStatus,
}

fn spawn_dashboard_daemon(project: Option<&Path>, all_projects: bool, port: u16) -> Result<()> {
    let exe = std::env::current_exe().context("failed to locate the varda executable")?;
    let mut command = ProcessCommand::new(&exe);
    command.args(["task", "dashboard", "--web"]);
    command.args(["--port", &port.to_string()]);
    if all_projects {
        command.arg("--all");
    }
    if let Some(project) = project {
        command.arg("--project").arg(project);
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    detach_command(&mut command);

    let child = command
        .spawn()
        .with_context(|| format!("failed to spawn dashboard daemon on port {port}"))?;
    println!(
        "dashboard daemon started on http://127.0.0.1:{port}/ (pid: {})",
        child.id()
    );
    println!("stop it with: kill {}", child.id());
    Ok(())
}

#[cfg(unix)]
fn detach_command(command: &mut ProcessCommand) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn detach_command(_command: &mut ProcessCommand) {}

fn serve_task_dashboard(
    config: config::Config,
    project: Option<PathBuf>,
    all_projects: bool,
    port: u16,
) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .with_context(|| format!("failed to bind dashboard server to 127.0.0.1:{port}"))?;
    println!("serving task dashboard at http://127.0.0.1:{port}/");
    println!("press Ctrl-C to stop");

    for stream in listener.incoming() {
        let mut stream = stream.context("failed to accept dashboard connection")?;
        if let Err(error) =
            handle_dashboard_connection(&mut stream, &config, project.as_deref(), all_projects)
        {
            eprintln!("dashboard request failed: {error:#}");
        }
    }

    Ok(())
}

fn handle_dashboard_connection(
    stream: &mut TcpStream,
    config: &config::Config,
    project: Option<&Path>,
    all_projects: bool,
) -> Result<()> {
    let mut buffer = [0; 4096];
    let read = stream
        .read(&mut buffer)
        .context("failed to read dashboard request")?;
    let request = String::from_utf8_lossy(&buffer[..read]);
    let request_line = request.lines().next().unwrap_or("");
    let method = request_line.split_whitespace().next().unwrap_or("");
    let target = request_line.split_whitespace().nth(1).unwrap_or("/");
    let body = request
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("");

    match (method, target.split('?').next().unwrap_or("/")) {
        ("GET", "/" | "/index.html") => {
            write_http_response(stream, "200 OK", "text/html", DASHBOARD_HTML)
        }
        ("GET", "/api/tasks") => {
            let payload = load_dashboard_payload(config, project, all_projects)?;
            let json =
                serde_json::to_string(&payload).context("failed to encode dashboard JSON")?;
            write_http_response(stream, "200 OK", "application/json", &json)
        }
        ("GET", "/api/tasks/detail") => {
            let path =
                query_param(target, "path").context("missing required query parameter 'path'")?;
            let detail = load_dashboard_task_detail(config, Path::new(&path))?;
            let json = serde_json::to_string(&detail)
                .context("failed to encode dashboard task detail JSON")?;
            write_http_response(stream, "200 OK", "application/json", &json)
        }
        ("POST", "/api/tasks/status") => {
            let update: DashboardStatusUpdate =
                serde_json::from_str(body).context("failed to decode dashboard status update")?;
            update_dashboard_task_status(config, update)?;
            write_http_response(stream, "200 OK", "application/json", "{\"ok\":true}")
        }
        ("GET", "/healthz") => write_http_response(stream, "200 OK", "text/plain", "ok\n"),
        _ => write_http_response(stream, "404 Not Found", "text/plain", "not found\n"),
    }
}

fn update_dashboard_task_status(
    config: &config::Config,
    update: DashboardStatusUpdate,
) -> Result<()> {
    use task::TaskStatus;
    match update.status {
        TaskStatus::Done | TaskStatus::Backlog | TaskStatus::Ready => {}
        other => anyhow::bail!(
            "dashboard status updates do not support status '{}'",
            other.as_str()
        ),
    }

    let task_path = task::resolve_task_reference(config, Path::new(&update.path))?;
    let mut task_document = task::load_task(&task_path)?;
    if task_document.frontmatter.status == update.status {
        return Ok(());
    }

    task_document.set_status(update.status);
    if update.status == TaskStatus::Done {
        task_document.frontmatter.requires_user = false;
    }
    task::write_task(&task_document)?;

    if config.git.auto_commit {
        git::commit_task_file(
            &task_path,
            &format!(
                "Mark task {} {}",
                task_path.display(),
                update.status.as_str()
            ),
        )?;
    }

    Ok(())
}

fn write_http_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &str,
) -> Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(response.as_bytes())
        .context("failed to write dashboard response")
}

fn load_dashboard_payload(
    config: &config::Config,
    project: Option<&Path>,
    all_projects: bool,
) -> Result<DashboardPayload> {
    let default_project = task::resolve_project_path(project)?;
    let default_project = default_project.display().to_string();
    let (scope, summaries) = if all_projects {
        ("all projects".to_owned(), task::list_all_tasks(config)?)
    } else {
        (
            default_project.clone(),
            task::list_tasks(config, Path::new(&default_project))?,
        )
    };

    let mut projects = Vec::new();
    let mut tasks = Vec::new();
    for summary in summaries {
        if let Some(project) = summary.project.as_ref()
            && !projects.contains(project)
        {
            projects.push(project.clone());
        }

        let completed_at = file_mtime_seconds(&summary.path);

        tasks.push(DashboardTask {
            id: summary.id,
            status: summary.status.as_str(),
            project: summary.project,
            assignee: summary.assignee,
            title: summary.title,
            path: summary.path.display().to_string(),
            completed_at,
        });
    }
    projects.sort();

    Ok(DashboardPayload {
        scope,
        generated_at: unix_timestamp()?,
        default_project: Some(default_project),
        home: std::env::var("HOME").ok(),
        tasks,
        projects,
        statuses: vec![
            "backlog",
            "ready",
            "running",
            "needs_user",
            "failed",
            "review",
            "done",
        ],
    })
}

fn load_dashboard_task_detail(
    config: &config::Config,
    task_path: &Path,
) -> Result<DashboardTaskDetail> {
    let task_path = task::resolve_task_reference(config, task_path)?;
    let document = task::load_task(&task_path)?;
    let markdown = fs::read_to_string(&task_path)
        .with_context(|| format!("failed to read task at {}", task_path.display()))?;
    let recaps = document
        .frontmatter
        .recaps
        .iter()
        .map(|recap_path| {
            let resolved = resolve_recap_path(recap_path, &task_path);
            let markdown = fs::read_to_string(&resolved).unwrap_or_else(|error| {
                format!(
                    "# Recap Unavailable\n\nFailed to read {}: {error}",
                    resolved.display()
                )
            });
            DashboardRecap {
                path: resolved.display().to_string(),
                markdown,
            }
        })
        .collect();

    Ok(DashboardTaskDetail {
        path: task_path.display().to_string(),
        markdown,
        recaps,
    })
}

fn query_param(target: &str, name: &str) -> Option<String> {
    let query = target.split_once('?')?.1;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key == name {
            Some(percent_decode(value.replace('+', " ").as_bytes()))
        } else {
            None
        }
    })
}

fn percent_decode(input: &[u8]) -> String {
    let mut output = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%'
            && i + 2 < input.len()
            && let (Some(high), Some(low)) = (hex_value(input[i + 1]), hex_value(input[i + 2]))
        {
            output.push((high << 4) | low);
            i += 3;
            continue;
        }
        output.push(input[i]);
        i += 1;
    }

    String::from_utf8_lossy(&output).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

const DASHBOARD_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>✨ Varda Tasks</title>
  <link rel="icon" href="data:image/svg+xml,<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'><text y='.9em' font-size='90'>✨</text></svg>">
  <style>
    :root { color-scheme: light; --bg: #f6f7f9; --panel: #ffffff; --line: #d9dee7; --text: #1e2430; --muted: #687386; --accent: #1f7a5a; }
    * { box-sizing: border-box; }
    body { margin: 0; font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; background: var(--bg); color: var(--text); }
    header { display: flex; align-items: center; justify-content: space-between; gap: 16px; padding: 18px 24px; border-bottom: 1px solid var(--line); background: var(--panel); }
    h1 { margin: 0; font-size: 20px; font-weight: 700; }
    .meta { color: var(--muted); font-size: 13px; }
    .filters { display: flex; gap: 10px; align-items: center; flex-wrap: wrap; padding: 12px 24px; border-bottom: 1px solid var(--line); background: #eef1f5; }
    label { display: grid; gap: 4px; font-size: 12px; color: var(--muted); }
    select { min-width: 160px; border: 1px solid var(--line); border-radius: 6px; background: var(--panel); padding: 8px 10px; color: var(--text); }
    main { display: grid; grid-template-columns: minmax(0, 1fr); gap: 0; min-height: calc(100vh - 113px); }
    main.details-open { grid-template-columns: minmax(0, 1fr) minmax(360px, 34vw); }
    .board { display: grid; grid-template-columns: repeat(7, minmax(200px, 1fr)); gap: 12px; overflow-x: auto; padding: 16px; }
    .column { min-width: 220px; border: 1px solid transparent; border-radius: 8px; padding: 4px; }
    .column.drop-target { border-color: var(--accent); background: #edf7f2; }
    .column h2 { display: flex; justify-content: space-between; align-items: center; margin: 0 0 10px; font-size: 13px; text-transform: uppercase; color: var(--muted); letter-spacing: 0; }
    .count { border: 1px solid var(--line); border-radius: 999px; padding: 1px 8px; background: var(--panel); color: var(--muted); }
    .task { width: 100%; text-align: left; border: 1px solid var(--line); border-radius: 8px; background: var(--panel); padding: 10px; margin-bottom: 10px; cursor: pointer; box-shadow: 0 1px 2px rgba(25, 32, 44, 0.05); }
    .task[draggable="true"] { cursor: grab; }
    .task.dragging { opacity: 0.55; }
    .task:hover, .task.selected { border-color: var(--accent); }
    .task-title { font-weight: 650; line-height: 1.3; overflow-wrap: anywhere; }
    .task-row { display: flex; gap: 8px; flex-wrap: wrap; margin-top: 8px; color: var(--muted); font-size: 12px; }
    .badge { border: 1px solid var(--line); border-radius: 999px; padding: 2px 7px; background: #f9fafb; max-width: 100%; overflow-wrap: anywhere; }
    aside { display: none; border-left: 1px solid var(--line); background: var(--panel); padding: 18px; overflow: auto; }
    main.details-open aside { display: block; }
    .empty { color: var(--muted); padding: 20px; }
    .details-header { display: flex; align-items: start; justify-content: space-between; gap: 12px; }
    .details h2 { margin: 0 0 6px; font-size: 18px; overflow-wrap: anywhere; }
    .close { border: 1px solid var(--line); border-radius: 6px; background: var(--panel); color: var(--muted); cursor: pointer; font-size: 18px; line-height: 1; padding: 4px 8px; }
    .close:hover { border-color: var(--accent); color: var(--text); }
    .details .path { color: var(--muted); font-size: 12px; overflow-wrap: anywhere; }
    pre { white-space: pre-wrap; overflow-wrap: anywhere; background: #f6f7f9; border: 1px solid var(--line); border-radius: 8px; padding: 12px; font-size: 13px; line-height: 1.45; }
    h3 { margin: 18px 0 8px; font-size: 14px; }
    @media (max-width: 980px) { main.details-open { grid-template-columns: 1fr; } aside { border-left: 0; border-top: 1px solid var(--line); } .board { grid-template-columns: repeat(7, 220px); } }
  </style>
</head>
<body>
  <header>
    <div>
      <h1>✨ Varda Tasks</h1>
      <div id="scope" class="meta"></div>
    </div>
    <div id="updated" class="meta"></div>
  </header>
  <section class="filters">
    <label>Project <input id="projectFilter" list="projectOptions" placeholder="All projects — type to search" autocomplete="off" /><datalist id="projectOptions"></datalist></label>
    <label>Status <select id="statusFilter"><option value="">All statuses</option></select></label>
  </section>
  <main>
    <section id="board" class="board"></section>
    <aside id="details" class="empty"></aside>
  </main>
  <script>
    const statuses = ["backlog", "ready", "running", "needs_user", "failed", "review", "done"];
    let payload = { tasks: [], projects: [], statuses };
    let selectedPath = "";
    let selectedDetail = null;
    let detailLoadingPath = "";
    let initializedFilters = false;

    function label(value) {
      return value.replaceAll("_", " ");
    }

    function homePrefix() {
      const h = payload.home || "";
      return h.endsWith("/") ? h.slice(0, -1) : h;
    }
    // Display paths under $HOME as ~/… ; keep absolute paths as the data/API keys.
    function abbreviate(p) {
      if (!p) return p;
      const h = homePrefix();
      if (h && (p === h || p.startsWith(h + "/"))) return "~" + p.slice(h.length);
      return p;
    }

    function projectHue(name) {
      let h = 0;
      for (let i = 0; i < name.length; i++) h = (h * 31 + name.charCodeAt(i)) & 0xffff;
      return h % 360;
    }

    function optionList(select, values, emptyLabel) {
      const current = select.value;
      select.innerHTML = "";
      const empty = document.createElement("option");
      empty.value = "";
      empty.textContent = emptyLabel;
      select.appendChild(empty);
      for (const value of values) {
        const option = document.createElement("option");
        option.value = value;
        option.textContent = label(value);
        select.appendChild(option);
      }
      select.value = values.includes(current) ? current : "";
    }

    function taskMatches(task) {
      // Case-insensitive SUBSTRING search over the ~-abbreviated project path, so
      // "461", "sandbox", or a full "~/dev/…" path all filter as expected.
      const query = document.getElementById("projectFilter").value.trim().toLowerCase();
      const status = document.getElementById("statusFilter").value;
      const projectOk = !query || (task.project && abbreviate(task.project).toLowerCase().includes(query));
      return projectOk && (!status || task.status === status);
    }

    function closeDetails() {
      selectedPath = "";
      selectedDetail = null;
      detailLoadingPath = "";
      document.querySelector("main").classList.remove("details-open");
      document.getElementById("details").className = "empty";
      document.getElementById("details").innerHTML = "";
      renderBoard();
    }

    function renderBoard() {
      // Project filter is a searchable input backed by a datalist of ~-abbreviated
      // paths; status stays a plain select.
      const projectOptions = document.getElementById("projectOptions");
      projectOptions.innerHTML = "";
      for (const project of payload.projects) {
        const option = document.createElement("option");
        option.value = abbreviate(project);
        projectOptions.appendChild(option);
      }
      optionList(document.getElementById("statusFilter"), payload.statuses || statuses, "All statuses");
      if (!initializedFilters) {
        const defaultProject = payload.default_project || "";
        if (payload.projects.includes(defaultProject)) {
          document.getElementById("projectFilter").value = abbreviate(defaultProject);
        }
        initializedFilters = true;
      }
      document.getElementById("scope").textContent = `Scope: ${payload.scope || "unknown"} | Tasks: ${payload.tasks.length}`;
      document.getElementById("updated").textContent = payload.generated_at ? `Updated ${new Date(payload.generated_at * 1000).toLocaleTimeString()}` : "";

      const board = document.getElementById("board");
      board.innerHTML = "";
      for (const status of statuses) {
        const column = document.createElement("section");
        column.className = "column";
        column.dataset.status = status;
        const droppable = status === "done" || status === "backlog" || status === "ready";
        if (droppable) {
          column.ondragover = event => {
            const path = event.dataTransfer.getData("text/plain");
            const task = payload.tasks.find(t => t.path === path);
            if (Array.from(event.dataTransfer.types).includes("text/plain") && (!task || task.status !== status)) {
              event.preventDefault();
              column.classList.add("drop-target");
            }
          };
          column.ondragleave = () => column.classList.remove("drop-target");
          column.ondrop = async event => {
            event.preventDefault();
            column.classList.remove("drop-target");
            const path = event.dataTransfer.getData("text/plain");
            if (path) {
              try {
                await updateTaskStatus(path, status);
              } catch (error) {
                document.getElementById("details").textContent = `Failed to update task: ${error}`;
              }
            }
          };
        }
        const tasks = payload.tasks.filter(task => task.status === status && taskMatches(task));
        const heading = document.createElement("h2");
        heading.innerHTML = `<span>${label(status)}</span><span class="count">${tasks.length}</span>`;
        column.appendChild(heading);
        for (const task of tasks) {
          const button = document.createElement("button");
          button.className = `task${task.path === selectedPath ? " selected" : ""}`;
          button.type = "button";
          button.draggable = task.status !== "done";
          button.ondragstart = event => {
            event.dataTransfer.setData("text/plain", task.path);
            event.dataTransfer.effectAllowed = "move";
            button.classList.add("dragging");
          };
          button.ondragend = () => button.classList.remove("dragging");
          button.onclick = () => { selectedPath = task.path; selectedDetail = null; detailLoadingPath = ""; renderBoard(); renderDetails(task); };
          const id = task.id === null || task.id === undefined ? "unversioned" : `#${task.id}`;
          const projectChip = (payload.scope === "all projects" && task.project)
            ? task.project.replace(/\\/g, "/").split("/").filter(Boolean).pop() || task.project
            : null;
          let projectBadgeHtml = "";
          if (projectChip) {
            const hue = projectHue(projectChip);
            const style = `background:hsl(${hue},60%,92%);border-color:hsl(${hue},45%,68%);color:hsl(${hue},55%,28%)`;
            projectBadgeHtml = `<span class="badge" style="${style}"></span>`;
          }
          button.innerHTML = `<div class="task-title"></div><div class="task-row"><span class="badge"></span><span class="badge"></span>${projectBadgeHtml}</div>`;
          button.querySelector(".task-title").textContent = task.title;
          const badges = button.querySelectorAll(".badge");
          badges[0].textContent = id;
          badges[1].textContent = task.assignee || "-";
          if (projectChip) badges[2].textContent = projectChip;
          column.appendChild(button);
        }
        board.appendChild(column);
      }
      const selected = payload.tasks.find(task => task.path === selectedPath && taskMatches(task));
      if (selected) {
        renderDetails(selected);
      } else if (selectedPath) {
        closeDetails();
      }
    }

    async function updateTaskStatus(path, status) {
      const response = await fetch("/api/tasks/status", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ path, status })
      });
      if (!response.ok) {
        throw new Error(await response.text());
      }
      const task = payload.tasks.find(task => task.path === path);
      if (task) {
        task.status = status;
      }
      renderBoard();
      await refresh();
    }

    function renderDetails(task) {
      const details = document.getElementById("details");
      document.querySelector("main").classList.add("details-open");
      details.className = "details";
      details.innerHTML = "";
      const header = document.createElement("div");
      header.className = "details-header";
      const headingGroup = document.createElement("div");
      const title = document.createElement("h2");
      title.textContent = task.title;
      headingGroup.appendChild(title);
      const path = document.createElement("div");
      path.className = "path";
      path.textContent = task.path;
      headingGroup.appendChild(path);
      const close = document.createElement("button");
      close.type = "button";
      close.className = "close";
      close.title = "Close";
      close.textContent = "x";
      close.onclick = closeDetails;
      header.appendChild(headingGroup);
      header.appendChild(close);
      details.appendChild(header);
      const taskHeading = document.createElement("h3");
      taskHeading.textContent = "Task";
      details.appendChild(taskHeading);
      if (!selectedDetail || selectedDetail.path !== task.path) {
        const loading = document.createElement("p");
        loading.className = "meta";
        loading.textContent = "Loading task details...";
        details.appendChild(loading);
        if (detailLoadingPath !== task.path) {
          detailLoadingPath = task.path;
          fetchTaskDetail(task.path).catch(error => {
            if (selectedPath === task.path) {
              details.textContent = `Failed to load task details: ${error}`;
            }
          });
        }
        return;
      }
      const taskPre = document.createElement("pre");
      taskPre.textContent = selectedDetail.markdown;
      details.appendChild(taskPre);
      const recapHeading = document.createElement("h3");
      recapHeading.textContent = "Recaps";
      details.appendChild(recapHeading);
      if (!selectedDetail.recaps.length) {
        const empty = document.createElement("p");
        empty.className = "meta";
        empty.textContent = "No recaps are associated with this task.";
        details.appendChild(empty);
      }
      for (const recap of selectedDetail.recaps) {
        const recapPath = document.createElement("div");
        recapPath.className = "path";
        recapPath.textContent = recap.path;
        details.appendChild(recapPath);
        const recapPre = document.createElement("pre");
        recapPre.textContent = recap.markdown;
        details.appendChild(recapPre);
      }
    }

    async function fetchTaskDetail(path) {
      const response = await fetch(`/api/tasks/detail?path=${encodeURIComponent(path)}`, { cache: "no-store" });
      if (!response.ok) {
        throw new Error(await response.text());
      }
      const detail = await response.json();
      if (selectedPath === path) {
        detailLoadingPath = "";
        selectedDetail = detail;
        const task = payload.tasks.find(task => task.path === path);
        if (task) renderDetails(task);
      }
    }

    function sortTasksByCompletionDesc(tasks) {
      tasks.sort((a, b) => {
        const aTime = a.completed_at ?? -Infinity;
        const bTime = b.completed_at ?? -Infinity;
        if (aTime !== bTime) return bTime - aTime;
        const aId = a.id ?? -Infinity;
        const bId = b.id ?? -Infinity;
        return bId - aId;
      });
    }

    async function refresh() {
      const response = await fetch("/api/tasks", { cache: "no-store" });
      payload = await response.json();
      sortTasksByCompletionDesc(payload.tasks);
      renderBoard();
    }

    document.getElementById("projectFilter").addEventListener("change", renderBoard);
    document.getElementById("projectFilter").addEventListener("input", renderBoard);
    document.getElementById("statusFilter").addEventListener("change", renderBoard);
    refresh().catch(error => {
      document.getElementById("details").textContent = `Failed to load dashboard: ${error}`;
    });
    setInterval(refresh, 30000);
  </script>
</body>
</html>
"##;

fn project_chip(project: Option<&str>) -> String {
    project
        .and_then(|p| Path::new(p).file_name())
        .and_then(|s| s.to_str())
        .map(|s| format!(" [{s}]"))
        .unwrap_or_default()
}

fn print_task_dashboard(scope: &str, tasks: &[task::TaskSummary]) {
    let show_project = scope == "all projects";

    println!("# Tasks Dashboard");
    println!();
    println!("scope: {scope}");
    println!("tasks: {}", tasks.len());
    println!();

    for status in [
        task::TaskStatus::Backlog,
        task::TaskStatus::Ready,
        task::TaskStatus::Running,
        task::TaskStatus::NeedsUser,
        task::TaskStatus::Failed,
        task::TaskStatus::Review,
        task::TaskStatus::Done,
    ] {
        println!("## {}", status.as_str());
        let mut found = false;
        for task in tasks.iter().filter(|task| task.status == status) {
            found = true;
            let id = task
                .id
                .map(|id| format!("#{id}"))
                .unwrap_or_else(|| "unversioned".to_owned());
            let assignee = task.assignee.as_deref().unwrap_or("-");
            let chip = if show_project {
                project_chip(task.project.as_deref())
            } else {
                String::new()
            };
            println!(
                "- {id}{chip} {} (assignee: {assignee}, path: {})",
                task.title,
                task.path.display()
            );
        }
        if !found {
            println!("- none");
        }
        println!();
    }
}

fn prompt_task_selection() -> Result<Option<PathBuf>> {
    print!("Task to inspect [id/path, blank to skip]: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let input = input.trim();

    if input.is_empty() {
        Ok(None)
    } else {
        Ok(Some(PathBuf::from(input.trim_start_matches('#'))))
    }
}

fn resolve_task_for_show(task_path: &Path) -> Result<PathBuf> {
    if task_path.exists() {
        return Ok(task_path.to_path_buf());
    }

    let config_path = config::config_file()?;
    let config = config::load_config(&config_path)?;
    task::resolve_task_reference(&config, task_path)
}

fn resolve_recap_path(recap_path: &str, task_path: &Path) -> PathBuf {
    let path = PathBuf::from(recap_path);
    if path.is_absolute() || path.exists() {
        return path;
    }

    task_path
        .parent()
        .map(|parent| parent.join(&path))
        .unwrap_or(path)
}

fn print_task_list(project_path: &Path, tasks: &[task::TaskSummary]) {
    println!("project: {}", project_path.display());

    if tasks.is_empty() {
        println!("no tasks found");
        return;
    }

    println!(
        "{:<5} {:<10} {:<12} {:<32} PATH",
        "ID", "STATUS", "ASSIGNEE", "TITLE"
    );
    for task in tasks {
        let id = task
            .id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "-".to_owned());
        let assignee = task.assignee.as_deref().unwrap_or("-");
        println!(
            "{:<5} {:<10} {:<12} {:<32} {}",
            id,
            task.status.as_str(),
            assignee,
            truncate_for_table(&task.title, 32),
            task.path.display()
        );
    }
}

fn is_active_task_status(status: task::TaskStatus) -> bool {
    !matches!(status, task::TaskStatus::Backlog | task::TaskStatus::Done)
}

fn truncate_for_table(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_owned();
    }

    let mut truncated: String = value.chars().take(width.saturating_sub(1)).collect();
    truncated.push('.');
    truncated
}

fn spawn_task_in_background(task_path: &Path) -> Result<()> {
    let config_path = config::config_file()?;
    let config = config::load_config(&config_path)?;
    let resolved = task::resolve_task_reference(&config, task_path)?;
    let initial_task = task::load_task(&resolved)?;
    if initial_task.frontmatter.status != task::TaskStatus::Ready {
        anyhow::bail!(
            "task {} is not ready; current status is {:?}",
            resolved.display(),
            initial_task.frontmatter.status
        );
    }
    let initial_session_count = initial_task.frontmatter.agent_session_ids.len();
    let exe = std::env::current_exe().context("failed to locate the varda executable")?;
    let mut child = ProcessCommand::new(&exe)
        .args(["task", "run"])
        .arg(&resolved)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| {
            format!(
                "failed to spawn background agent for {}",
                resolved.display()
            )
        })?;
    wait_for_background_launch(&config, &resolved, initial_session_count, &mut child)?;
    println!(
        "task running in background: {} (pid: {})",
        resolved.display(),
        child.id()
    );
    Ok(())
}

fn wait_for_background_launch(
    config: &config::Config,
    task_path: &Path,
    initial_session_count: usize,
    child: &mut std::process::Child,
) -> Result<()> {
    const BACKGROUND_LAUNCH_TIMEOUT: Duration = Duration::from_secs(10);
    const BACKGROUND_LAUNCH_POLL: Duration = Duration::from_millis(100);

    let started = Instant::now();
    loop {
        let task = task::load_task(task_path)?;
        let recorded_launch = task.frontmatter.agent_session_ids.len() > initial_session_count
            && task.frontmatter.agent_session_logs.len() > initial_session_count
            && Path::new(&task.frontmatter.agent_session_logs[initial_session_count]).exists();
        if recorded_launch && task.frontmatter.status != task::TaskStatus::Ready {
            return Ok(());
        }

        if let Some(status) = child
            .try_wait()
            .context("failed to inspect background agent process")?
        {
            if recorded_launch {
                return Ok(());
            }
            record_background_launch_failure(
                config,
                task_path,
                &format!(
                    "background child pid {} exited before recording a session (status: {status})",
                    child.id()
                ),
            )?;
            anyhow::bail!(
                "background agent for {} exited before recording a session (status: {status})",
                task_path.display()
            );
        }

        if started.elapsed() >= BACKGROUND_LAUNCH_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            record_background_launch_failure(
                config,
                task_path,
                &format!(
                    "background child pid {} did not record a running session within {} seconds",
                    child.id(),
                    BACKGROUND_LAUNCH_TIMEOUT.as_secs()
                ),
            )?;
            anyhow::bail!(
                "background agent for {} did not record a session within {} seconds",
                task_path.display(),
                BACKGROUND_LAUNCH_TIMEOUT.as_secs()
            );
        }

        std::thread::sleep(BACKGROUND_LAUNCH_POLL);
    }
}

fn record_background_launch_failure(
    config: &config::Config,
    task_path: &Path,
    reason: &str,
) -> Result<()> {
    let mut task = task::load_task(task_path)?;
    if !matches!(
        task.frontmatter.status,
        task::TaskStatus::Ready | task::TaskStatus::Running
    ) {
        return Ok(());
    }

    let session_id = Uuid::new_v4().to_string();
    let session_log_path = Path::new(&config.defaults.operations_dir)
        .join(config::RUNS_DIRNAME)
        .join(format!("{session_id}.log"));
    if let Some(parent) = session_log_path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create session log directory {}",
                parent.display()
            )
        })?;
    }
    fs::write(
        &session_log_path,
        format!(
            "session_id={session_id}\nagent=background-launcher\ntask={}\nlaunch_failure={reason}\n",
            task_path.display()
        ),
    )
    .with_context(|| format!("failed to write session log at {}", session_log_path.display()))?;

    let recap_dir = Path::new(&config.defaults.operations_dir).join(config::RECAPS_DIRNAME);
    fs::create_dir_all(&recap_dir)
        .with_context(|| format!("failed to create recap directory {}", recap_dir.display()))?;
    let recap_path = recap_dir.join(format!("{}.md", Uuid::new_v4()));
    let recap = format!(
        "---\ntask: {}\n---\n\n# Agent Run Failed\n\nThe background launch failed before Varda could record a running agent session.\n\nReason: {reason}\n\nSession ID: `{session_id}`\n\nSession log: [{}]({})\n",
        task_path.display(),
        session_log_path.display(),
        session_log_path.display()
    );
    fs::write(&recap_path, recap)
        .with_context(|| format!("failed to write recap at {}", recap_path.display()))?;

    task.frontmatter.agent_session_ids.push(session_id);
    task.frontmatter
        .agent_session_logs
        .push(session_log_path.display().to_string());
    task.set_recap(recap_path.display().to_string());
    task.frontmatter.requires_user = false;
    task.set_status(task::TaskStatus::Failed);
    task::write_task(&task)?;

    Ok(())
}

/// The validated placement of the sandboxed RESIDENT: which agent drives it, the
/// isolating sandbox it lands in, the dedicated rw workspace, and whether the
/// spawn broker is wired. Produced by [`resolve_resident_launch`] only AFTER every
/// security gate ([`config::enforce_resident_launch`]) has passed, so holding one
/// is proof the launch is safe.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResidentLaunch {
    /// Agent (or role) that drives the resident.
    agent: String,
    /// Effective isolating sandbox name (never `local`).
    sandbox: String,
    /// Dedicated host workspace mounted rw into the box.
    workspace: PathBuf,
    /// True when the nested-orchestration spawn broker is wired for this route
    /// (461d interactive path) — always true after enforcement, since a disabled
    /// policy is rejected.
    broker_wired: bool,
}

/// Resolve the resident's placement for `workspace` and assert every load-bearing
/// gate before returning. Errors (loudly) when the route would launch the resident
/// un-sandboxed, with network, with a push credential, or with an unsafe workspace
/// mount. Pure over `config` + the host `$HOME`/filesystem, so it is exercised
/// directly by the deterministic tests.
fn resolve_resident_launch(config: &config::Config, workspace: &Path) -> Result<ResidentLaunch> {
    let route = routing::match_route(config, workspace, None).with_context(|| {
        format!(
            "no route matches the orchestration workspace {}; add a `[[routes]]` whose glob covers it \
             (see the sandboxed-resident example in the default config)",
            workspace.display()
        )
    })?;
    let agent_config = config
        .agents
        .get(&route.agent)
        .expect("routing ensures the selected agent exists");
    let routing_root = routing_root_for(workspace);
    // Orchestrate/resident launch has no task frontmatter → no task-pinned
    // override. Route/`.varda`/defaults precedence is unchanged, keeping
    // `enforce_resident_launch` intact.
    let resolved = config.resolve_sandbox_for(workspace, &routing_root, None)?;
    let mounts: Vec<String> = sandbox::merge_mount_origins(
        &resolved.config.mounts,
        &resolved.route_mounts,
        &resolved.varda_mounts,
    )
    .into_iter()
    .map(|(_origin, spec)| spec)
    .collect();
    let credentials = agent_config.effective_credentials();
    let orchestration = config.resolve_orchestration_for(workspace);

    // The resident's EFFECTIVE env, merged with the same precedence as the real
    // launch (`build_client` → `env_for_request`): agent env is the base, the
    // resolved sandbox+route+`.varda` static env overrides on collision. Presence of
    // a push-enabling key — from ANY origin — is what `enforce_resident_launch`
    // rejects, so this union is the full env-channel surface to scan.
    let mut effective_env = agent_config.env.clone();
    effective_env.extend(resolved.env.clone());

    config::enforce_resident_launch(
        &route.agent,
        &resolved.name,
        &resolved.config,
        &mounts,
        workspace,
        &credentials,
        &effective_env,
        config.defaults.forward_ssh_agent,
        &orchestration,
    )?;

    Ok(ResidentLaunch {
        agent: route.display_name().to_owned(),
        sandbox: resolved.name,
        workspace: workspace.to_path_buf(),
        broker_wired: orchestration.enabled,
    })
}

/// Body of a scaffolded resident task. It points the agent at the workspace's
/// `.varda/WORKFLOW.md` as the contract — the loop intelligence is authored there
/// (a separate task), not here.
const RESIDENT_TASK_BODY: &str = "\
# Resident orchestrator

You are the Varda self-hosting RESIDENT. You run inside an isolating sandbox whose \
egress is restricted to your LLM provider API ONLY (no `github.com`, no general hosts) \
with this workspace mounted read-write. Your contract — the dev loop you execute — is \
defined in `.varda/WORKFLOW.md` in this workspace. Read it and follow it.

Spawn workers through the Varda spawn broker (`spawn_subtask`); merge their branches \
in-box against the mounted workspace. You can reach ONLY your LLM API and have NO push \
credential and NO route to a remote: pushing back out is a separate, human-gated step \
performed on the host. \
Stop and signal `needs_user` when the workflow calls for a human decision.
";

/// Ensure the resident task at `task_path` is runnable before `run_task_command`
/// hands it to `runner::run_task` (which refuses anything but `Ready`). A prior
/// `orchestrate` launch can leave the resident task in a terminal state
/// (Failed/Review/Done/NeedsUser/Running); reset it to `Ready` so each new
/// `varda orchestrate` invocation can relaunch the same resident task without a
/// manual `varda task set-status ready` workaround. Recap/session history in the
/// frontmatter and the task body are left untouched — only the status field flips.
fn ensure_resident_task_ready(task_path: &Path) -> Result<()> {
    let mut task_doc = task::load_task(task_path)?;
    if task_doc.frontmatter.status != task::TaskStatus::Ready
        && task_doc.frontmatter.status != task::TaskStatus::Backlog
    {
        task_doc.set_status(task::TaskStatus::Ready);
        task::write_task(&task_doc)?;
    }
    Ok(())
}

/// Resolve or scaffold the RESIDENT task for `workspace`. Reuses an existing
/// `resident-orchestrator` task under the workspace project when present; otherwise
/// scaffolds a minimal one whose body points at `.varda/WORKFLOW.md`.
fn resolve_or_scaffold_resident_task(
    config: &config::Config,
    workspace: &Path,
    agent: &str,
) -> Result<PathBuf> {
    const RESIDENT_TASK_NAME: &str = "resident-orchestrator";
    if let Ok(existing) = task::list_tasks(config, workspace) {
        if let Some(found) = existing.into_iter().find(|t| {
            t.path
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|stem| stem.contains(RESIDENT_TASK_NAME))
                .unwrap_or(false)
        }) {
            return Ok(found.path);
        }
    }
    let task_path = task::create_task(
        config,
        RESIDENT_TASK_NAME,
        workspace,
        Some(agent),
        Some(RESIDENT_TASK_BODY),
        None,
    )?;
    let mut task_doc = task::load_task(&task_path)?;
    task_doc.set_status(task::TaskStatus::Ready);
    task::write_task(&task_doc)?;
    Ok(task_path)
}

async fn orchestrate_command(interactive: bool, workspace: Option<&Path>) -> Result<()> {
    let config_path = config::config_file()?;
    let config = config::load_config(&config_path)?;

    // Default to a dedicated workspace under the Varda home — never $HOME/~/dev.
    let workspace = match workspace {
        Some(dir) => dir.to_path_buf(),
        None => config::varda_home()?.join("orchestrate").join("workspace"),
    };
    fs::create_dir_all(&workspace).with_context(|| {
        format!(
            "failed to create orchestration workspace {}",
            workspace.display()
        )
    })?;
    let workspace = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.clone());

    // Assert every sandboxed-resident gate BEFORE any task is launched. A route
    // that would place the resident un-sandboxed / with network / with a push
    // credential / with an unsafe workspace mount is refused here, loudly.
    let launch = resolve_resident_launch(&config, &workspace)?;

    println!("orchestrate: resident launch validated");
    println!("  workspace:  {} (mounted rw)", launch.workspace.display());
    println!("  sandbox:    {} (isolating, net-denied)", launch.sandbox);
    println!("  agent:      {}", launch.agent);
    println!("  push creds: none (push is a separate, human-gated host step)");
    println!(
        "  broker:     {}",
        if launch.broker_wired {
            "wired (spawn_subtask available to the resident)"
        } else {
            "off"
        }
    );
    println!(
        "  mode:       {}",
        if interactive {
            "interactive (TTY attached, operator in the conversation)"
        } else {
            "headless (autonomous until terminal / needs_user)"
        }
    );

    let task_path = resolve_or_scaffold_resident_task(&config, &workspace, &launch.agent)?;
    ensure_resident_task_ready(&task_path)?;
    println!("orchestrate: resident task {}", task_path.display());
    println!();

    // Delegate to the standard run path, which (for an orchestration-enabled route)
    // wraps the session in the 461d interactive broker so `spawn_subtask` is served
    // for the whole session. `--interactive` attaches the TTY (M13b); headless runs
    // the resident autonomously until it terminates or signals needs_user.
    run_task_command(&task_path, interactive, false).await
}

async fn run_task_command(task_path: &Path, interactive: bool, quiet: bool) -> Result<()> {
    let config_path = config::config_file()?;
    let config = config::load_config(&config_path)?;
    let task_path = task::resolve_task_reference(&config, task_path)?;
    let task_document = task::load_task(&task_path)?;
    let id_str = task_document
        .frontmatter
        .id
        .map(|id| format!(" #{id}"))
        .unwrap_or_default();
    println!("Running task{}: {}", id_str, task_document.title());
    println!();
    print!("{}", task_document.body.trim());
    println!();
    println!();
    println!("---");
    println!();
    let route = routing::match_route_for_task(&config, &task_document, false)?;
    println!("estimated_prompt_tokens: {}", route.estimated_prompt_tokens);
    let agent_config = config
        .agents
        .get(&route.agent)
        .expect("routing ensures the selected agent exists");
    let display_name = route.display_name().to_owned();
    let client = build_client(
        &config,
        &display_name,
        agent_config,
        &route.sandbox,
        &route.route_mounts,
        &route.route_env,
        task_document.frontmatter.project.as_deref().map(Path::new),
        task_document.frontmatter.mother_project.as_deref().map(Path::new),
        task_document.frontmatter.sandbox.as_deref(),
    )?;
    // Wire the nested-orchestration broker onto the interactive resident: when
    // orchestration is enabled for this project, wrap the session in
    // `OrchestratedAgentClient` so it serves the MCP socket and exports
    // `VARDA_MCP_SOCKET`, exactly as the batch path does. A non-orchestrated run
    // stays a plain client (no broker, no overhead). The socket lives for the whole
    // interactive session and is torn down root-only after children join (461b).
    let policy = task_document
        .frontmatter
        .project
        .as_deref()
        .map(Path::new)
        .map(|path| config.resolve_orchestration_for(path))
        .unwrap_or_else(|| config.orchestration.clone());
    let orchestrated_client = task_document
        .frontmatter
        .project
        .as_deref()
        .map(PathBuf::from)
        .filter(|_| policy.enabled)
        .map(|project_path| OrchestratedAgentClient {
            inner: client.clone(),
            config: config.clone(),
            policy,
            sandbox_primitive: resolve_sandbox_primitive(
                &config,
                &project_path,
                task_document.frontmatter.sandbox.as_deref(),
            ),
            project_path,
            fallback_agent: route.agent.clone(),
            lineage: None,
        });
    if config.git.auto_commit {
        git::commit_task_file(
            &task_path,
            &format!("Snapshot task {} before run", task_path.display()),
        )?;
        println!("committed task snapshot");
    }
    let stream = !quiet && !interactive;
    let outcome = runner::run_task(
        &config,
        &display_name,
        route.role_instructions.as_deref(),
        &task_path,
        orchestrated_client
            .as_ref()
            .map_or(&client as &dyn AgentClient, |c| c as &dyn AgentClient),
        interactive,
        stream,
    )
    .await?;
    println!(
        "processed task={} agent={} glob={} status={:?} recap={}",
        task_path.display(),
        display_name,
        route.glob,
        outcome.status,
        outcome.recap_path.display()
    );

    let recap_content = fs::read_to_string(&outcome.recap_path)
        .with_context(|| format!("failed to read recap at {}", outcome.recap_path.display()))?;
    println!();
    println!("---");
    println!();
    print!("{recap_content}");
    if !recap_content.ends_with('\n') {
        println!();
    }

    if !outcome.blocked_commands.is_empty() {
        println!();
        println!("blocked_commands: {}", outcome.blocked_commands.join(", "));
        println!(
            "hint: add these to the task's `allow_commands` frontmatter and re-run to authorize them headlessly"
        );
    }

    let notification = if outcome.status == task::TaskStatus::NeedsUser {
        let notification =
            notify::notify_user_interaction(&config, &task_path, &outcome.recap_path)?;
        println!(
            "user interaction required; notification={}",
            notification.display()
        );
        Some(notification)
    } else {
        None
    };
    if config.git.auto_commit {
        if let Some(project) = task_document.frontmatter.project.as_deref() {
            commit_agent_files_for_task(&task_path, project, &outcome.files_touched);
        }
        git::commit_task_update(
            &task_path,
            &outcome.recap_path,
            &outcome.session_log_path,
            notification.as_deref(),
        )?;
        println!("committed task update");
    }

    Ok(())
}

fn commit_agent_files_for_task(task_path: &Path, project: &str, files_touched: &[PathBuf]) {
    if files_touched.is_empty() {
        return;
    }
    let project_path = Path::new(project);
    let id = task_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("task");
    let message = format!("Apply task {id} agent changes");
    if let Err(error) = git::commit_agent_files(project_path, files_touched, &message) {
        eprintln!(
            "warning: failed to commit agent files for task {}: {error:#}",
            task_path.display()
        );
    } else {
        println!(
            "committed {} agent file(s) in {}",
            files_touched.len(),
            project_path.display()
        );
    }
}

async fn plan_task_command(task_path: &Path) -> Result<()> {
    let config_path = config::config_file()?;
    let config = config::load_config(&config_path)?;
    let task_path = task::resolve_task_reference(&config, task_path)?;
    let task_document = task::load_task(&task_path)?;
    let route = routing::match_route_for_task(&config, &task_document, true)?;
    let agent_config = config
        .agents
        .get(&route.agent)
        .expect("routing ensures the selected agent exists");
    let display_name = route.display_name().to_owned();
    let client = build_client(
        &config,
        &display_name,
        agent_config,
        &route.sandbox,
        &route.route_mounts,
        &route.route_env,
        task_document.frontmatter.project.as_deref().map(Path::new),
        task_document.frontmatter.mother_project.as_deref().map(Path::new),
        task_document.frontmatter.sandbox.as_deref(),
    )?;
    let outcome = runner::plan_task(
        &config,
        &display_name,
        route.role_instructions.as_deref(),
        &task_path,
        &client,
    )
    .await?;
    println!(
        "plan generated task={} agent={} plan={}",
        task_path.display(),
        display_name,
        outcome.plan_path.display()
    );
    open_editor(&outcome.plan_path)?;
    if config.git.auto_commit {
        git::commit_task_plan(&task_path, &outcome.plan_path)?;
        println!("committed task plan");
    }
    Ok(())
}

async fn resume_task_command(task_path: &Path, fresh: bool, interactive: bool) -> Result<()> {
    let config_path = config::config_file()?;
    let config = config::load_config(&config_path)?;
    let task_path = task::resolve_task_reference(&config, task_path)?;
    let mut task_document = task::load_task(&task_path)?;
    let was_needs_user = task_document.frontmatter.status == task::TaskStatus::NeedsUser;
    let captured_resume_command = if fresh {
        None
    } else {
        latest_agent_resume_command(&task_document)
    };

    task_document.set_status(task::TaskStatus::Ready);
    task_document.frontmatter.requires_user = false;
    task::write_task(&task_document)?;

    if was_needs_user && prompt_yes_no("Open editor to complete the task update?", true)? {
        open_editor(&task_path)?;
    }

    if let Some(resume_command) = captured_resume_command {
        println!("Captured agent resume command found:");
        println!("  {resume_command}");
        if prompt_yes_no("Resume the previous agent session?", true)? {
            return run_captured_resume_command(
                &config,
                &task_path,
                &task_document,
                resume_command,
            )
            .await;
        }
        println!("Starting a fresh agent session.");
    }

    run_task_command(&task_path, interactive, false).await
}

fn latest_agent_resume_command(task_document: &task::TaskDocument) -> Option<String> {
    task_document
        .frontmatter
        .agent_resume_commands
        .iter()
        .rev()
        .find(|command| !command.trim().is_empty())
        .cloned()
}

async fn run_captured_resume_command(
    config: &config::Config,
    task_path: &Path,
    task_document: &task::TaskDocument,
    resume_command: String,
) -> Result<()> {
    let route = routing::match_route_for_task(config, task_document, false)?;
    println!("estimated_prompt_tokens: {}", route.estimated_prompt_tokens);
    let agent_config = config
        .agents
        .get(&route.agent)
        .expect("routing ensures the selected agent exists");
    let display_name = route.display_name().to_owned();
    let client = build_client(
        config,
        &display_name,
        agent_config,
        &route.sandbox,
        &route.route_mounts,
        &route.route_env,
        task_document.frontmatter.project.as_deref().map(Path::new),
        task_document.frontmatter.mother_project.as_deref().map(Path::new),
        task_document.frontmatter.sandbox.as_deref(),
    )?;
    if config.git.auto_commit {
        git::commit_task_file(
            task_path,
            &format!("Snapshot task {} before resume", task_path.display()),
        )?;
        println!("committed task snapshot");
    }
    let outcome = runner::resume_interactive_task(
        config,
        &display_name,
        route.role_instructions.as_deref(),
        task_path,
        &client,
        resume_command,
    )
    .await?;
    println!(
        "processed task={} agent={} glob={} status={:?} recap={}",
        task_path.display(),
        display_name,
        route.glob,
        outcome.status,
        outcome.recap_path.display()
    );

    let recap_content = fs::read_to_string(&outcome.recap_path)
        .with_context(|| format!("failed to read recap at {}", outcome.recap_path.display()))?;
    println!();
    println!("---");
    println!();
    print!("{recap_content}");
    if !recap_content.ends_with('\n') {
        println!();
    }

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn update_tasks_command(
    task_ref: Option<&Path>,
    set_status: Option<&str>,
    set_agent: Option<&str>,
    filter_status: &[String],
    filter_agent: Option<&str>,
    project: Option<&Path>,
    all: bool,
    yes: bool,
) -> Result<()> {
    if set_status.is_none() && set_agent.is_none() {
        anyhow::bail!("nothing to update: specify --set-status and/or --set-agent");
    }

    let new_status: Option<task::TaskStatus> = set_status.map(|s| s.parse()).transpose()?;
    let filter_statuses: Vec<task::TaskStatus> = filter_status
        .iter()
        .map(|s| s.parse())
        .collect::<Result<Vec<_>>>()?;

    let config_path = config::config_file()?;
    let config = config::load_config(&config_path)?;

    let task_paths: Vec<PathBuf> = if let Some(task_ref) = task_ref {
        vec![task::resolve_task_reference(&config, task_ref)?]
    } else {
        let summaries: Vec<task::TaskSummary> = if all {
            task::list_all_tasks(&config)?
        } else {
            let project_path = task::resolve_project_path(project)?;
            task::list_tasks(&config, &project_path)?
        };
        summaries
            .into_iter()
            .filter(|t| {
                if !filter_statuses.is_empty() && !filter_statuses.contains(&t.status) {
                    return false;
                }
                if let Some(agent) = filter_agent
                    && t.assignee.as_deref() != Some(agent)
                {
                    return false;
                }
                true
            })
            .map(|t| t.path)
            .collect()
    };

    if task_paths.is_empty() {
        println!("no matching tasks");
        return Ok(());
    }

    if !yes {
        println!("will update {} task(s):", task_paths.len());
        for path in &task_paths {
            println!("  {}", path.display());
        }
        if let Some(s) = set_status {
            println!("  set status → {s}");
        }
        if let Some(a) = set_agent {
            println!("  set agent  → {a}");
        }
        if !prompt_yes_no("Proceed?", true)? {
            println!("aborted");
            return Ok(());
        }
    }

    for path in &task_paths {
        let mut doc = task::load_task(path)?;
        if let Some(status) = new_status {
            doc.set_status(status);
        }
        if let Some(agent) = set_agent {
            doc.set_assignee(agent);
        }
        task::write_task(&doc)?;
        println!("updated {}", path.display());
    }

    println!("updated {} task(s)", task_paths.len());

    if config.git.auto_commit {
        let paths_ref: Vec<&Path> = task_paths.iter().map(|p| p.as_path()).collect();
        git::commit_task_files(&paths_ref, "Update tasks")?;
        println!("committed changes");
    }

    Ok(())
}

fn delete_task_command(task_ref: &Path, yes: bool, keep_recaps: bool) -> Result<()> {
    let config_path = config::config_file()?;
    let config = config::load_config(&config_path)?;
    let task_path = task::resolve_task_reference(&config, task_ref)?;
    let doc = task::load_task(&task_path)?;

    // Collect the recap artifacts that belong to this task so they don't linger
    // in the home store after the task record is gone.
    let recap_paths: Vec<PathBuf> = if keep_recaps {
        Vec::new()
    } else {
        doc.frontmatter
            .recaps
            .iter()
            .map(|recap| resolve_recap_path(recap, &task_path))
            .filter(|path| path.exists())
            .collect()
    };

    if !yes {
        println!("will delete task {}:", task_path.display());
        println!("  title  → {}", doc.title());
        println!("  status → {}", doc.frontmatter.status.as_str());
        for recap in &recap_paths {
            println!("  recap  → {}", recap.display());
        }
        if !prompt_yes_no("Proceed?", false)? {
            println!("aborted");
            return Ok(());
        }
    }

    let mut removed: Vec<PathBuf> = Vec::new();
    for recap in &recap_paths {
        fs::remove_file(recap)
            .with_context(|| format!("failed to remove recap {}", recap.display()))?;
        removed.push(recap.clone());
    }
    fs::remove_file(&task_path)
        .with_context(|| format!("failed to remove task {}", task_path.display()))?;
    removed.push(task_path.clone());

    println!("deleted task {}", task_path.display());
    if !recap_paths.is_empty() {
        println!("removed {} recap file(s)", recap_paths.len());
    }

    if config.git.auto_commit {
        let paths_ref: Vec<&Path> = removed.iter().map(|p| p.as_path()).collect();
        git::commit_task_deletions(&paths_ref, "Delete task")?;
        println!("committed changes");
    }

    Ok(())
}

fn resume_task_session_command(task_path: &Path) -> Result<()> {
    let config_path = config::config_file()?;
    let config = config::load_config(&config_path)?;
    let task_path = task::resolve_task_reference(&config, task_path)?;
    let mut task_document = task::load_task(&task_path)?;
    let sessions = task_sessions(&config, &task_path)?;

    if sessions.is_empty() {
        anyhow::bail!("no previous sessions found for {}", task_path.display());
    }

    let selected = prompt_task_session(&sessions)?.context("session resume cancelled")?;
    task_document.set_status(task::TaskStatus::Ready);
    task_document.frontmatter.requires_user = false;
    task_document
        .frontmatter
        .agent_session_ids
        .push(selected.session_id.clone());
    task_document
        .frontmatter
        .agent_session_logs
        .push(selected.log_path.display().to_string());
    task::write_task(&task_document)?;

    if config.git.auto_commit {
        git::commit_task_file(
            &task_path,
            &format!("Resume task session {}", task_path.display()),
        )?;
        println!("committed task session resume");
    }

    println!(
        "task ready; selected session={} log={}",
        selected.session_id,
        selected.log_path.display()
    );

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskSession {
    session_id: String,
    log_path: PathBuf,
    modified: Option<SystemTime>,
}

fn task_sessions(config: &config::Config, task_path: &Path) -> Result<Vec<TaskSession>> {
    let runs_dir = Path::new(&config.defaults.operations_dir).join(config::RUNS_DIRNAME);
    if !runs_dir.exists() {
        return Ok(Vec::new());
    }

    let mut sessions = Vec::new();
    for entry in fs::read_dir(&runs_dir)
        .with_context(|| format!("failed to read runs directory {}", runs_dir.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", runs_dir.display()))?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("log") {
            continue;
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read session log {}", path.display()))?;
        let Some(log_task) = session_log_value(&content, "task") else {
            continue;
        };
        if !same_task_path(Path::new(&log_task), task_path) {
            continue;
        }

        let session_id = session_log_value(&content, "session_id").unwrap_or_else(|| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("unknown")
                .to_owned()
        });
        let modified = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok());
        sessions.push(TaskSession {
            session_id,
            log_path: path,
            modified,
        });
    }

    sessions.sort_by_key(|session| std::cmp::Reverse(session.modified));
    Ok(sessions)
}

fn session_log_value(content: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    content.lines().find_map(|line| {
        line.strip_prefix(&prefix)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    })
}

fn same_task_path(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }

    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn prompt_task_session(sessions: &[TaskSession]) -> Result<Option<TaskSession>> {
    println!("Past sessions:");
    for (index, session) in sessions.iter().enumerate() {
        println!(
            "{}. {} {}",
            index + 1,
            session.session_id,
            session.log_path.display()
        );
    }
    print!("Session to resume [1, q to cancel]: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim();
    if answer.eq_ignore_ascii_case("q") || answer.eq_ignore_ascii_case("quit") {
        return Ok(None);
    }

    let index = if answer.is_empty() {
        0
    } else {
        answer
            .parse::<usize>()
            .with_context(|| format!("invalid session selection '{answer}'"))?
            .checked_sub(1)
            .context("session selection must be at least 1")?
    };

    sessions
        .get(index)
        .cloned()
        .with_context(|| format!("session selection {} is out of range", index + 1))
        .map(Some)
}

fn prompt_assignee(default_assignee: &str) -> Result<Option<String>> {
    print!("Assignee [{default_assignee}]: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let assignee = input.trim();

    if assignee.is_empty() {
        Ok(Some(default_assignee.to_owned()))
    } else {
        Ok(Some(assignee.to_owned()))
    }
}

fn prompt_yes_no(prompt: &str, default: bool) -> Result<bool> {
    let suffix = if default { "Y/n" } else { "y/N" };
    print!("{prompt} [{suffix}]: ");
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let answer = input.trim().to_ascii_lowercase();

    if answer.is_empty() {
        return Ok(default);
    }

    Ok(matches!(answer.as_str(), "y" | "yes"))
}

fn open_editor(path: &Path) -> Result<()> {
    let path_str = path.to_str().unwrap_or_default();

    if let Ok(nvim_socket) = std::env::var("NVIM") {
        // Running inside Neovim's terminal — open in the parent instance and wait
        let status = ProcessCommand::new("nvim")
            .args(["--server", &nvim_socket, "--remote-wait", path_str])
            .status()?;
        if !status.success() {
            anyhow::bail!("nvim --remote-wait exited with status {status}");
        }
    } else if std::env::var("ZED_TERM").is_ok() {
        let status = ProcessCommand::new("zed")
            .args(["--wait", path_str])
            .status()?;
        if !status.success() {
            anyhow::bail!("zed --wait exited with status {status}");
        }
    } else if std::env::var("VSCODE_GIT_IPC_HANDLE").is_ok() {
        // VS Code or Cursor (a VS Code fork) — prefer Cursor if it's on PATH
        let cli = if which_exists("cursor") {
            "cursor"
        } else {
            "code"
        };
        let status = ProcessCommand::new(cli)
            .args(["--reuse-window", "--wait", path_str])
            .status()?;
        if !status.success() {
            anyhow::bail!("'{cli} --reuse-window --wait' exited with status {status}");
        }
    } else {
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_owned());
        let status = ProcessCommand::new(&editor).arg(path).status()?;
        if !status.success() {
            anyhow::bail!("editor '{editor}' exited with status {status}");
        }
    }

    Ok(())
}

fn which_exists(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(bin).is_file()))
        .unwrap_or(false)
}

fn skill_install_command(source: Option<&Path>, link: bool) -> Result<()> {
    let source = if let Some(s) = source {
        s.to_path_buf()
    } else {
        std::env::current_dir()?.join("skills/varda/SKILL.md")
    };

    if !source.exists() {
        anyhow::bail!(
            "skill source not found at {}; run this command from the varda project directory or pass the path explicitly",
            source.display()
        );
    }

    let home = std::env::var("HOME").context("HOME environment variable not set")?;
    let dest_dir = PathBuf::from(format!("{home}/.claude/skills/varda"));
    let dest = dest_dir.join("SKILL.md");

    fs::create_dir_all(&dest_dir)
        .with_context(|| format!("failed to create skills directory {}", dest_dir.display()))?;

    if dest.symlink_metadata().is_ok() {
        fs::remove_file(&dest)
            .with_context(|| format!("failed to remove existing skill at {}", dest.display()))?;
    }

    if link {
        let source = source
            .canonicalize()
            .with_context(|| format!("failed to resolve {}", source.display()))?;
        std::os::unix::fs::symlink(&source, &dest).with_context(|| {
            format!(
                "failed to create symlink {} -> {}",
                dest.display(),
                source.display()
            )
        })?;
        println!("linked {} -> {}", dest.display(), source.display());
    } else {
        fs::copy(&source, &dest).with_context(|| {
            format!("failed to copy {} to {}", source.display(), dest.display())
        })?;
        println!("installed {} -> {}", source.display(), dest.display());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gray_matter::{Matter, engine::YAML};
    use serde::Deserialize;

    #[test]
    fn resolve_env_secrets_leaves_non_sentinel_values_untouched() {
        // No `${fnox:...}` binding ⇒ nothing is resolved (fnox is never invoked), and
        // literal values pass through verbatim regardless of trusted/untrusted origin.
        let mut env = std::collections::BTreeMap::new();
        env.insert("PLAIN".to_owned(), "literal".to_owned());
        env.insert("TILDE".to_owned(), "~/path".to_owned());
        resolve_env_secrets(&mut env, &["PLAIN".to_owned()]).expect("no bindings must pass");
        assert_eq!(env.get("PLAIN").unwrap(), "literal");
        assert_eq!(env.get("TILDE").unwrap(), "~/path");
    }

    #[test]
    fn resolve_env_secrets_refuses_untrusted_varda_binding() {
        // A fnox binding on a key from the untrusted `.varda` origin is refused BEFORE
        // any host resolution — repo config must not exfiltrate arbitrary host secrets.
        let mut env = std::collections::BTreeMap::new();
        env.insert("EXFIL".to_owned(), "${fnox:aws-prod-key}".to_owned());
        let err = resolve_env_secrets(&mut env, &["EXFIL".to_owned()])
            .expect_err("untrusted fnox binding must error");
        let msg = err.to_string();
        assert!(msg.contains("untrusted"), "error must name the untrusted origin: {msg}");
        assert!(msg.contains("EXFIL"), "error must name the key: {msg}");
        // The sentinel is left in place; no value was resolved.
        assert_eq!(env.get("EXFIL").unwrap(), "${fnox:aws-prod-key}");
    }

    #[derive(Debug, Deserialize)]
    struct PlanMetadata {
        plan_type: String,
        scope: String,
        project: String,
        generated_timestamp: u64,
        tasks_evaluated: usize,
        ready_tasks: usize,
        planner_agent: String,
        selection_reason: String,
        requires_user_confirmation: bool,
    }

    #[test]
    fn rendered_execution_plan_includes_review_frontmatter() {
        let content = render_execution_plan(
            "project",
            Path::new("/tmp/example"),
            1_775_000_000,
            "the current folder is known as a Varda project",
            3,
            &[],
        );

        let matter = Matter::<YAML>::new();
        let parsed = matter
            .parse::<PlanMetadata>(&content)
            .expect("plan should parse");
        let metadata = parsed.data.expect("plan should include valid frontmatter");

        assert_eq!(metadata.plan_type, "ready_task_execution_plan");
        assert_eq!(metadata.scope, "project");
        assert_eq!(metadata.project, "/tmp/example");
        assert_eq!(metadata.generated_timestamp, 1_775_000_000);
        assert_eq!(metadata.tasks_evaluated, 3);
        assert_eq!(metadata.ready_tasks, 0);
        assert_eq!(metadata.planner_agent, "codex");
        assert_eq!(
            metadata.selection_reason,
            "the current folder is known as a Varda project"
        );
        assert!(metadata.requires_user_confirmation);
        assert!(
            parsed
                .content
                .starts_with("# Project Ready Task Execution Plan")
        );
        assert!(parsed.content.contains("- Tasks evaluated: 3"));
        assert!(parsed.content.contains("- Ready tasks: 0"));
    }

    #[test]
    fn extracts_json_object_from_agent_output() {
        let output = "```json\n{\"schema\":\"varda.execution_plan.v1\",\"source_plan\":\"plan.md\",\"tasks\":[]}\n```";

        let json = extract_json_object(output).expect("json object should be extracted");

        assert_eq!(
            json,
            "{\"schema\":\"varda.execution_plan.v1\",\"source_plan\":\"plan.md\",\"tasks\":[]}"
        );
    }

    #[test]
    fn reads_planner_agent_from_plan_frontmatter() {
        let content = r#"---
planner_agent: codex
---

# Plan
"#;

        assert_eq!(plan_planner_agent(content).as_deref(), Some("codex"));
    }

    fn resident_tmp(name: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("varda-orchestrate-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    /// Build a config whose only route places `ws` in an isolating, net-denied
    /// sandbox with `ws` mounted rw and the spawn broker enabled — the canonical
    /// sandboxed-resident setup. `egress`/`primitive` are tweakable so tests can
    /// prove the gates reject unsafe variants.
    fn resident_config(ws: &Path, primitive: &str, egress: &str) -> config::Config {
        let toml = format!(
            r#"[defaults]
timeout_seconds = 600
operations_dir = "operations"

[[routes]]
glob = "{ws}"
agents = ["claude"]
sandbox = "orchestration"
mounts = ["{ws}:/workspace:rw"]

[agents.claude]
kind = "acp"
command = "claude"
args = []

[sandboxes.orchestration]
image = "dev:latest"
primitive = "{primitive}"
egress = [{egress}]

[orchestration]
enabled = true
deny_sandboxes = ["local"]
"#,
            ws = ws.display(),
        );
        toml::from_str(&toml).expect("resident test config should parse")
    }

    #[test]
    fn resolve_resident_launch_resolves_isolating_sandbox_with_broker() {
        let ws = resident_tmp("ok");
        let config = resident_config(&ws, "docker", "");
        let launch = resolve_resident_launch(&config, &ws)
            .expect("a well-formed sandboxed-resident route resolves");
        assert_eq!(launch.sandbox, "orchestration");
        assert_ne!(launch.sandbox, "local", "must not be un-sandboxed");
        assert_eq!(launch.agent, "claude");
        assert!(launch.broker_wired, "the spawn broker must be wired");
        assert_eq!(launch.workspace, ws);
    }

    #[test]
    fn resolve_resident_launch_rejects_non_llm_egress() {
        // A non-LLM egress host (here `api.example.com`) is refused; only the fixed
        // LLM-endpoint allowlist may be reached.
        let ws = resident_tmp("net");
        let config = resident_config(&ws, "microsandbox", "\"api.example.com\"");
        let err = resolve_resident_launch(&config, &ws)
            .expect_err("a non-LLM-endpoint egress host must be rejected");
        let msg = err.to_string();
        assert!(msg.contains("api.example.com"), "must name the host: {msg}");
        assert!(msg.contains("LLM"), "must state the LLM-only policy: {msg}");
    }

    #[test]
    fn resolve_resident_launch_allows_llm_egress() {
        // The resident may reach its LLM provider API when the provider can enforce
        // strict egress semantics.
        let ws = resident_tmp("llm");
        let config = resident_config(&ws, "microsandbox", "\"api.anthropic.com\"");
        let launch = resolve_resident_launch(&config, &ws)
            .expect("egress limited to an LLM endpoint must resolve");
        assert_eq!(launch.sandbox, "orchestration");
    }

    #[test]
    fn resolve_resident_launch_allows_docker_proxy_egress() {
        // Docker strict egress is now enforced by the forward-proxy sidecar, so a
        // docker resident limited to its LLM endpoint resolves rather than being
        // refused as an unenforceable downgrade.
        let ws = resident_tmp("docker-net");
        let config = resident_config(&ws, "docker", "\"api.anthropic.com\"");
        let launch = resolve_resident_launch(&config, &ws)
            .expect("docker proxy-enforced egress to an LLM endpoint must resolve");
        assert_eq!(launch.sandbox, "orchestration");
    }

    #[test]
    fn resolve_resident_launch_rejects_unsandboxed_resident() {
        let ws = resident_tmp("local");
        let config = resident_config(&ws, "local", "");
        let err = resolve_resident_launch(&config, &ws)
            .expect_err("an un-sandboxed resident must be rejected");
        assert!(err.to_string().contains("isolating sandbox"), "{err}");
    }

    /// #522 regression: a resident task left in a terminal state (Failed / Review /
    /// Done / NeedsUser / Running) by a prior `orchestrate` launch must be reset to
    /// `Ready` so the next launch does not hit `run_task_command`'s "task ... is not
    /// ready" bail — without a manual `varda task set-status ready` workaround. Prior
    /// recap history must survive the reset; only the status field flips.
    #[test]
    fn ensure_resident_task_ready_resets_terminal_statuses() {
        let ws = resident_tmp("ensure-ready");
        let mut config = resident_config(&ws, "docker", "");
        config.defaults.operations_dir = ws.join("operations").display().to_string();

        let task_path = resolve_or_scaffold_resident_task(&config, &ws, "claude")
            .expect("resident task should scaffold");

        for status in [
            task::TaskStatus::Failed,
            task::TaskStatus::Review,
            task::TaskStatus::Done,
            task::TaskStatus::NeedsUser,
            task::TaskStatus::Running,
        ] {
            let mut task_doc = task::load_task(&task_path).unwrap();
            task_doc.set_status(status);
            task_doc.set_recap(format!("prior run left task {status:?}"));
            task::write_task(&task_doc).unwrap();

            ensure_resident_task_ready(&task_path)
                .unwrap_or_else(|err| panic!("resetting {status:?} should succeed: {err}"));

            let reloaded = task::load_task(&task_path).unwrap();
            assert_eq!(
                reloaded.frontmatter.status,
                task::TaskStatus::Ready,
                "status {status:?} should have been reset to ready"
            );
            assert_eq!(
                reloaded.frontmatter.recaps.last(),
                Some(&format!("prior run left task {status:?}")),
                "recap history must be preserved across the reset"
            );
        }
    }

    /// #522 regression: a fresh first-time scaffold (already `Ready`) must be left
    /// unchanged by `ensure_resident_task_ready`.
    #[test]
    fn ensure_resident_task_ready_leaves_fresh_scaffold_unchanged() {
        let ws = resident_tmp("ensure-ready-fresh");
        let mut config = resident_config(&ws, "docker", "");
        config.defaults.operations_dir = ws.join("operations").display().to_string();

        let task_path = resolve_or_scaffold_resident_task(&config, &ws, "claude")
            .expect("resident task should scaffold");
        let before = task::load_task(&task_path).unwrap();
        assert_eq!(before.frontmatter.status, task::TaskStatus::Ready);

        ensure_resident_task_ready(&task_path).expect("ready check should succeed");

        let after = task::load_task(&task_path).unwrap();
        assert_eq!(after.frontmatter.status, task::TaskStatus::Ready);
        assert_eq!(after.body, before.body, "task body must be untouched");
        assert!(
            after.frontmatter.recaps.is_empty(),
            "a fresh scaffold has no recap history to preserve"
        );
    }

    /// #522 end-to-end: simulates two `varda orchestrate` launches back to back. The
    /// second launch must reuse (not duplicate) the same resident task and must
    /// recover it from `Failed` to `Ready` without operator intervention.
    #[test]
    fn orchestrate_resident_relaunch_recovers_from_failed_status() {
        let ws = resident_tmp("relaunch");
        let mut config = resident_config(&ws, "docker", "");
        config.defaults.operations_dir = ws.join("operations").display().to_string();

        let first_path = resolve_or_scaffold_resident_task(&config, &ws, "claude")
            .expect("first launch should scaffold the resident task");
        ensure_resident_task_ready(&first_path).expect("first launch should be ready to run");
        assert_eq!(
            task::load_task(&first_path).unwrap().frontmatter.status,
            task::TaskStatus::Ready
        );

        // Simulate a prior run that failed (e.g. the sandbox lost network mid-run).
        let mut failed = task::load_task(&first_path).unwrap();
        failed.set_status(task::TaskStatus::Failed);
        failed.set_recap("first run failed: network egress denied");
        task::write_task(&failed).unwrap();

        let second_path = resolve_or_scaffold_resident_task(&config, &ws, "claude")
            .expect("second launch should resolve the existing resident task");
        assert_eq!(
            second_path, first_path,
            "the resident task must be reused, not duplicated, across launches"
        );
        ensure_resident_task_ready(&second_path)
            .expect("second launch should recover the task to ready");

        let reloaded = task::load_task(&second_path).unwrap();
        assert_eq!(reloaded.frontmatter.status, task::TaskStatus::Ready);
        assert_eq!(
            reloaded.frontmatter.recaps,
            vec!["first run failed: network egress denied".to_string()],
            "the failure recap must survive the automatic reset"
        );
    }

    /// Live end-to-end of the sandboxed-resident model. Requires a working docker
    /// daemon, so it is `#[ignore]` in the deterministic suite; run with
    /// `cargo test -- --ignored orchestrate_live_resident`.
    ///
    /// Scenario (driven manually / by the WORKFLOW.md resident contract):
    ///   1. A resident boots in a docker box with `ws` mounted rw and `--network none`.
    ///   2. It spawns ONE worker (via `spawn_subtask`) that edits a file on a branch.
    ///   3. The resident merges that branch IN-BOX against the mounted workspace.
    /// Assertions:
    ///   - the merged change is visible on the HOST through the `ws` mount;
    ///   - `~/.aws` and the host `$HOME` were never visible inside the box
    ///     (credential-denylist + no home mount);
    ///   - NO push occurred (net-deny + no push credential in the resident identity).
    ///
    /// What this harness verifies offline before the box ever boots: the launch
    /// contract those assertions depend on — an isolating, net-denied sandbox with a
    /// dedicated rw workspace mount and no push credential — actually holds for a
    /// docker-backed config, and the inverse (a push credential) is refused.
    #[test]
    #[ignore = "requires docker"]
    fn orchestrate_live_resident() {
        let ws = resident_tmp("live");
        // Real workspace shape: a git repo the resident merges worker branches into.
        std::process::Command::new("git")
            .arg("init")
            .arg(&ws)
            .status()
            .expect("git init");
        fs::create_dir_all(ws.join(".varda")).unwrap();
        fs::write(ws.join(".varda/WORKFLOW.md"), "# resident contract\n").unwrap();

        // The launch contract holds for the docker-backed resident route.
        let config = resident_config(&ws, "docker", "");
        let launch = resolve_resident_launch(&config, &ws)
            .expect("docker-backed sandboxed resident must pass every gate");
        assert_eq!(launch.sandbox, "orchestration");
        assert!(launch.broker_wired);

        // Injecting a git push credential into the same route is refused, so the box
        // can never authenticate a push to a remote.
        let mut with_push = config.clone();
        if let Some(agent) = with_push.agents.get_mut("claude") {
            agent.credentials = vec![config::CredentialConfig {
                from_env: Some("GH_HOST_TOKEN".to_owned()),
                env: Some("GITHUB_TOKEN".to_owned()),
                ..Default::default()
            }];
        }
        let err = resolve_resident_launch(&with_push, &ws)
            .expect_err("a resident carrying a push credential must be refused");
        assert!(err.to_string().contains("push credential"), "{err}");

        // NOTE: the full in-box spawn→edit→merge flow and the host-visibility /
        // no-home-mount / no-push assertions above are exercised by driving the real
        // `orchestrate` command against this workspace with a live docker daemon.
    }

    #[test]
    fn task_list_active_statuses_exclude_backlog_and_done() {
        use task::TaskStatus;

        for status in [
            TaskStatus::Ready,
            TaskStatus::Running,
            TaskStatus::Review,
            TaskStatus::NeedsUser,
            TaskStatus::Failed,
        ] {
            assert!(is_active_task_status(status));
        }

        assert!(!is_active_task_status(TaskStatus::Backlog));
        assert!(!is_active_task_status(TaskStatus::Done));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn join_spawned_subtasks_drains_descendant_handles_registered_during_join() {
        use std::sync::{
            Arc,
            atomic::{AtomicU32, Ordering},
        };

        let state = orchestration::SharedSpawnState::new();
        let completed = Arc::new(AtomicU32::new(0));
        let child_state = state.clone();
        let child_completed = completed.clone();
        state.insert_handle(
            "child".to_owned(),
            tokio::spawn(async move {
                let grand_completed = child_completed.clone();
                child_state.insert_handle(
                    "grandchild".to_owned(),
                    tokio::spawn(async move {
                        grand_completed.fetch_add(1, Ordering::SeqCst);
                    }),
                );
                child_completed.fetch_add(1, Ordering::SeqCst);
            }),
        );

        join_spawned_subtasks(&state).await;

        assert_eq!(completed.load(Ordering::SeqCst), 2);
        assert_eq!(state.handle_count(), 0);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn nested_orchestrated_child_returns_without_self_joining_shared_state() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        use std::time::Duration;
        use tokio::time::timeout;

        let state = orchestration::SharedSpawnState::new();
        let child_state = state.clone();
        let terminal_written = Arc::new(AtomicBool::new(false));
        let child_terminal_written = terminal_written.clone();

        let child_handle = tokio::spawn(async move {
            finish_spawned_subtasks(&child_state, false, true).await;
            child_terminal_written.store(true, Ordering::SeqCst);
        });
        state.insert_handle("child".to_owned(), child_handle);

        timeout(Duration::from_millis(100), async {
            while !terminal_written.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("child run should not self-join");

        timeout(
            Duration::from_millis(100),
            finish_spawned_subtasks(&state, true, true),
        )
        .await
        .expect("root cleanup should join the completed child");
        assert_eq!(state.handle_count(), 0);
    }

    /// Test double for `OrchestratedAgentClient`'s inner agent: it never spawns a
    /// real subprocess. It records what the broker wiring handed it (the injected
    /// `orchestration_socket_path`, whether the request was interactive) and probes
    /// the served Unix socket so a test can assert the broker was live for the whole
    /// inner run — at entry and, after a brief hold, at exit.
    #[derive(Clone)]
    struct RecordingInnerClient {
        interactive_during_run: std::sync::Arc<std::sync::atomic::AtomicBool>,
        observed_socket: std::sync::Arc<std::sync::Mutex<Option<Option<String>>>>,
        observed_addr: std::sync::Arc<std::sync::Mutex<Option<String>>>,
        tcp_live_at_start: std::sync::Arc<std::sync::atomic::AtomicBool>,
        socket_live_at_start: std::sync::Arc<std::sync::atomic::AtomicBool>,
        socket_live_at_end: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl RecordingInnerClient {
        fn new() -> Self {
            Self {
                interactive_during_run: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                    false,
                )),
                observed_socket: std::sync::Arc::new(std::sync::Mutex::new(None)),
                observed_addr: std::sync::Arc::new(std::sync::Mutex::new(None)),
                tcp_live_at_start: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                socket_live_at_start: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
                    false,
                )),
                socket_live_at_end: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            }
        }

        fn observed_socket(&self) -> Option<String> {
            self.observed_socket
                .lock()
                .unwrap()
                .clone()
                .expect("inner run should have executed")
        }

        fn observed_addr(&self) -> Option<String> {
            self.observed_addr.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl AgentClient for RecordingInnerClient {
        async fn run_task(&self, request: agent::AgentRunRequest) -> Result<agent::AgentRunResult> {
            use std::sync::atomic::Ordering::SeqCst;
            self.interactive_during_run
                .store(request.interactive, SeqCst);
            *self.observed_socket.lock().unwrap() = Some(request.orchestration_socket_path.clone());
            *self.observed_addr.lock().unwrap() = request.orchestration_addr.clone();
            if let Some(addr) = request.orchestration_addr.as_deref() {
                // The TCP broker binds before the request is threaded in, so a connect
                // must succeed immediately for the whole session.
                if tokio::net::TcpStream::connect(addr).await.is_ok() {
                    self.tcp_live_at_start.store(true, SeqCst);
                }
            }
            if let Some(path) = request.orchestration_socket_path.as_deref() {
                // serve_unix_socket binds asynchronously; poll briefly for it to appear.
                for _ in 0..100 {
                    if Path::new(path).exists() {
                        self.socket_live_at_start.store(true, SeqCst);
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                // Hold the session open, then confirm the broker is STILL served — the
                // socket must span the whole session, not be torn down per-child.
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                self.socket_live_at_end
                    .store(Path::new(path).exists(), SeqCst);
            }
            Ok(agent::AgentRunResult {
                recap: "recap".to_owned(),
                requires_user: false,
                suggested_agent: None,
                resume_command: None,
            })
        }
    }

    fn broker_test_config() -> config::Config {
        config::Config {
            defaults: config::Defaults::default(),
            routes: Vec::new(),
            agents: std::collections::BTreeMap::new(),
            roles: std::collections::BTreeMap::new(),
            git: config::GitConfig { auto_commit: false },
            sandboxes: std::collections::BTreeMap::new(),
            orchestration: orchestration::OrchestrationPolicy::default(),
        }
    }

    /// Short `/tmp`-rooted project dir so the derived Unix socket path stays under
    /// the ~104-char `sockaddr_un` limit on macOS.
    fn broker_test_project(tag: &str) -> PathBuf {
        let path = PathBuf::from("/tmp").join(format!(
            "v461d-{tag}-{}-{}",
            std::process::id(),
            &uuid::Uuid::new_v4().to_string()[..8]
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn interactive_broker_request(session_id: &str) -> agent::AgentRunRequest {
        let doc = test_task_document();
        // Mirrors runner::run_task's interactive build: socket path starts unset and is
        // threaded in by OrchestratedAgentClient when orchestration is enabled.
        agent::AgentRunRequest {
            agent_name: "codex".to_owned(),
            role_instructions: None,
            task_path: "task.md".to_owned(),
            frontmatter: doc.frontmatter,
            body: doc.body,
            timeout: std::time::Duration::from_secs(600),
            session_id: session_id.to_owned(),
            session_log_path: None,
            interactive: true,
            interpret: false,
            stream: false,
            resume_command: None,
            orchestration_socket_path: None,
            orchestration_addr: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interactive_orchestrated_run_serves_broker_socket() {
        use std::sync::atomic::Ordering::SeqCst;

        let project = broker_test_project("int");
        let inner = RecordingInnerClient::new();
        let client = OrchestratedAgentClient {
            inner: inner.clone(),
            config: broker_test_config(),
            sandbox_primitive: "local".to_owned(),
            policy: orchestration::OrchestrationPolicy {
                enabled: true,
                ..Default::default()
            },
            project_path: project.clone(),
            fallback_agent: "codex".to_owned(),
            lineage: None,
        };

        let session_id = "root";
        let result = client
            .run_task(interactive_broker_request(session_id))
            .await
            .unwrap();
        assert_eq!(result.recap, "recap");

        // Interactive flowed THROUGH the broker instead of short-circuiting.
        assert!(inner.interactive_during_run.load(SeqCst));
        // The per-session socket path was threaded into the request (so env_for_request
        // will export VARDA_MCP_SOCKET) and the broker served it live during the run.
        let expected = project
            .join(".varda-mcp")
            .join(format!("{session_id}.sock"));
        assert_eq!(
            inner.observed_socket(),
            Some(expected.display().to_string())
        );
        assert!(
            inner.socket_live_at_start.load(SeqCst),
            "broker socket must be served during the interactive session"
        );
        // Root-only teardown removes the socket and its dir after the session ends.
        assert!(
            !expected.exists(),
            "socket must be torn down after the session"
        );
        assert!(!project.join(".varda-mcp").exists());

        let _ = std::fs::remove_dir_all(&project);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_orchestrated_interactive_run_gets_no_broker() {
        use std::sync::atomic::Ordering::SeqCst;

        let project = broker_test_project("plain");
        let inner = RecordingInnerClient::new();
        let client = OrchestratedAgentClient {
            inner: inner.clone(),
            config: broker_test_config(),
            sandbox_primitive: "local".to_owned(),
            // Orchestration disabled ⇒ the interactive path must stay exactly as today.
            policy: orchestration::OrchestrationPolicy::default(),
            project_path: project.clone(),
            fallback_agent: "codex".to_owned(),
            lineage: None,
        };

        client
            .run_task(interactive_broker_request("interactive-plain"))
            .await
            .unwrap();

        assert!(inner.interactive_during_run.load(SeqCst));
        // No socket threaded in ⇒ env_for_request exports no VARDA_MCP_SOCKET.
        assert_eq!(inner.observed_socket(), None);
        assert!(!inner.socket_live_at_start.load(SeqCst));
        // And no broker directory was ever created under the project.
        assert!(!project.join(".varda-mcp").exists());

        let _ = std::fs::remove_dir_all(&project);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn interactive_broker_socket_spans_whole_session_then_tears_down() {
        use std::sync::atomic::Ordering::SeqCst;

        // Lifetime guarantee: the interactive resident is the root run (lineage None),
        // so teardown is root-only — finish_spawned_subtasks joins any detached children
        // BEFORE the server is aborted and the socket removed (461b ordering, exercised
        // by join_spawned_subtasks_drains_* and nested_orchestrated_child_* above). Here
        // we assert the complementary half: the socket is served for the ENTIRE inner
        // session (start AND end), never torn down mid-session, and only removed after.
        let project = broker_test_project("life");
        let inner = RecordingInnerClient::new();
        let client = OrchestratedAgentClient {
            inner: inner.clone(),
            config: broker_test_config(),
            sandbox_primitive: "local".to_owned(),
            policy: orchestration::OrchestrationPolicy {
                enabled: true,
                ..Default::default()
            },
            project_path: project.clone(),
            fallback_agent: "codex".to_owned(),
            lineage: None,
        };

        let expected = project.join(".varda-mcp").join("resident.sock");
        client
            .run_task(interactive_broker_request("resident"))
            .await
            .unwrap();

        assert!(
            inner.socket_live_at_start.load(SeqCst),
            "live at session start"
        );
        assert!(
            inner.socket_live_at_end.load(SeqCst),
            "broker must still be served at the end of the session (not per-child teardown)"
        );
        assert!(
            !expected.exists(),
            "socket removed only after the session ends"
        );

        let _ = std::fs::remove_dir_all(&project);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn vm_backed_primitive_serves_broker_over_tcp_not_socket() {
        use std::sync::atomic::Ordering::SeqCst;

        // An own-kernel microVM primitive cannot reach a bind-mounted Unix socket,
        // so the broker is served over TCP and the guest env carries VARDA_MCP_ADDR
        // (host:port) instead of a socket path — and no `.varda-mcp` socket dir is
        // created under the project.
        let project = broker_test_project("tcp");
        let inner = RecordingInnerClient::new();
        let client = OrchestratedAgentClient {
            inner: inner.clone(),
            config: broker_test_config(),
            policy: orchestration::OrchestrationPolicy {
                enabled: true,
                ..Default::default()
            },
            sandbox_primitive: "microsandbox".to_owned(),
            project_path: project.clone(),
            fallback_agent: "codex".to_owned(),
            lineage: None,
        };

        client
            .run_task(interactive_broker_request("root"))
            .await
            .unwrap();

        // TCP addr threaded in (host:port on the loopback default bind), no socket path.
        let addr = inner.observed_addr().expect("VARDA_MCP_ADDR must be set");
        assert_eq!(inner.observed_socket(), None);
        let socket_addr: std::net::SocketAddr =
            addr.parse().expect("orchestration_addr must be host:port");
        assert!(socket_addr.ip().is_loopback(), "host-only bind by default");
        assert_ne!(socket_addr.port(), 0, "ephemeral port assigned");
        // The listener was live and reachable for the whole session.
        assert!(
            inner.tcp_live_at_start.load(SeqCst),
            "broker TCP listener must be reachable during the session"
        );
        // The TCP transport never creates the project socket dir.
        assert!(!project.join(".varda-mcp").exists());

        let _ = std::fs::remove_dir_all(&project);
    }

    #[test]
    fn primitive_selects_tcp_only_for_microvm() {
        // Transport predicate: own-kernel microVMs need TCP; shared-kernel / local do not.
        assert!(config::primitive_needs_tcp_broker("microsandbox"));
        assert!(config::primitive_needs_tcp_broker("clawk"));
        assert!(!config::primitive_needs_tcp_broker("local"));
        assert!(!config::primitive_needs_tcp_broker("docker"));
    }

    #[test]
    fn broker_socket_dir_is_gitignored() {
        // The broker socket dir lives under the resident's repo (the varda mother),
        // so an orchestrate session must not dirty the worktree it is about to merge.
        // Bind the ignore entry to the exact dir name the run path creates.
        let gitignore =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/.gitignore")).unwrap();
        assert!(
            gitignore.lines().any(|line| line.trim() == ".varda-mcp/"),
            "'.varda-mcp/' must be gitignored so the broker socket dir stays untracked"
        );
    }

    #[test]
    fn extracts_values_from_session_log() {
        let content = "session_id=session-1\nagent=codex\ntask=/tmp/task.md\n";

        assert_eq!(
            session_log_value(content, "session_id").as_deref(),
            Some("session-1")
        );
        assert_eq!(
            session_log_value(content, "task").as_deref(),
            Some("/tmp/task.md")
        );
        assert_eq!(session_log_value(content, "missing"), None);
    }

    #[test]
    fn decodes_dashboard_query_params() {
        let target = "/api/tasks/detail?path=%2Ftmp%2Ftask%20one.md&unused=true";

        assert_eq!(
            query_param(target, "path").as_deref(),
            Some("/tmp/task one.md")
        );
        assert_eq!(query_param(target, "missing"), None);
    }

    #[test]
    fn latest_agent_resume_command_uses_latest_non_empty_entry() {
        let mut task = test_task_document();
        task.frontmatter.agent_resume_commands = vec![
            "codex resume old".to_owned(),
            "   ".to_owned(),
            "codex resume new".to_owned(),
        ];

        assert_eq!(
            latest_agent_resume_command(&task).as_deref(),
            Some("codex resume new")
        );
    }

    #[test]
    fn latest_agent_resume_command_returns_none_when_empty() {
        let mut task = test_task_document();
        task.frontmatter.agent_resume_commands = vec!["".to_owned(), "  ".to_owned()];

        assert_eq!(latest_agent_resume_command(&task), None);
    }

    #[test]
    fn varda_env_floor_rejects_agent_env_and_credential_target_collisions() {
        let mut agent = config::AgentConfig {
            kind: config::AgentKind::Acp,
            command: "codex".to_owned(),
            args: Vec::new(),
            max_prompt_tokens: None,
            working_dir: None,
            env: std::collections::BTreeMap::from([("TRUSTED_AGENT".to_owned(), "x".to_owned())]),
            auth_token_env: Some("HOST_TOKEN".to_owned()),
            auth_token_target: Some("SANDBOX_TOKEN".to_owned()),
            credentials: Vec::new(),
            interactive_command: None,
            interactive_args: None,
            streams_output: None,
            resume_command_template: None,
            interpreter_agent: None,
            skip_recap: false,
        };
        let mut resolved = config::ResolvedSandbox {
            name: "inline".to_owned(),
            config: config::SandboxConfig::default(),
            route_mounts: Vec::new(),
            varda_mounts: Vec::new(),
            env: std::collections::BTreeMap::new(),
            varda_env_keys: vec!["TRUSTED_AGENT".to_owned()],
            varda_file: Some(PathBuf::from("/repo/.varda")),
        };

        let err = enforce_varda_env_credential_floor(&agent, &resolved).unwrap_err();
        assert!(err.to_string().contains("TRUSTED_AGENT"), "{err}");

        agent.env.clear();
        resolved.varda_env_keys = vec!["SANDBOX_TOKEN".to_owned()];
        let err = enforce_varda_env_credential_floor(&agent, &resolved).unwrap_err();
        assert!(err.to_string().contains("SANDBOX_TOKEN"), "{err}");
    }

    fn agent_for_credentials(credentials: Vec<config::CredentialConfig>) -> config::AgentConfig {
        config::AgentConfig {
            kind: config::AgentKind::Acp,
            command: "claude".to_owned(),
            args: Vec::new(),
            max_prompt_tokens: None,
            working_dir: None,
            env: std::collections::BTreeMap::new(),
            streams_output: None,
            auth_token_env: None,
            auth_token_target: None,
            credentials,
            interactive_command: None,
            interactive_args: None,
            resume_command_template: None,
            interpreter_agent: None,
            skip_recap: false,
        }
    }

    /// Exit criterion: several credentials inject per run — multiple env targets and
    /// ≥1 file target, minted at prepare from a `command` source (the FAKE mint) plus
    /// a `from_env` source. The scoped values reach BOTH channels.
    #[test]
    fn resolve_agent_credentials_mints_env_and_file_targets() {
        // SAFETY: a uniquely-named var this test owns; set and removed within it.
        unsafe { std::env::set_var("VARDA_TEST_CRED_HOST", "sk-host-token") };
        let agent = agent_for_credentials(vec![
            // command source → env target (host-minted short-lived token).
            config::CredentialConfig {
                command: Some("printf scoped-access-token".to_owned()),
                env: Some("CLOUDSDK_AUTH_ACCESS_TOKEN".to_owned()),
                ..Default::default()
            },
            // from_env source → env target.
            config::CredentialConfig {
                from_env: Some("VARDA_TEST_CRED_HOST".to_owned()),
                env: Some("ANTHROPIC_API_KEY".to_owned()),
                ..Default::default()
            },
            // command source → file target (staged read-only in the guest).
            config::CredentialConfig {
                command: Some("printf scoped-file-token".to_owned()),
                file: Some("/home/agent/.config/gcloud-token".to_owned()),
                ..Default::default()
            },
        ]);

        let (auth_env, auth_files) = resolve_agent_credentials(&agent).unwrap();
        unsafe { std::env::remove_var("VARDA_TEST_CRED_HOST") };

        assert_eq!(
            auth_env
                .get("CLOUDSDK_AUTH_ACCESS_TOKEN")
                .map(String::as_str),
            Some("scoped-access-token")
        );
        assert_eq!(
            auth_env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-host-token")
        );
        assert_eq!(
            auth_files
                .get("/home/agent/.config/gcloud-token")
                .map(String::as_str),
            Some("scoped-file-token")
        );
    }

    /// Back-compat: the legacy `auth_token_env`/`auth_token_target` pair still injects
    /// as a single env-target credential (one-entry sugar over the list).
    #[test]
    fn resolve_agent_credentials_back_compat_single_token() {
        unsafe { std::env::set_var("VARDA_TEST_LEGACY_TOKEN", "sk-legacy") };
        let mut agent = agent_for_credentials(vec![]);
        agent.auth_token_env = Some("VARDA_TEST_LEGACY_TOKEN".to_owned());
        agent.auth_token_target = Some("ANTHROPIC_API_KEY".to_owned());

        let (auth_env, auth_files) = resolve_agent_credentials(&agent).unwrap();
        unsafe { std::env::remove_var("VARDA_TEST_LEGACY_TOKEN") };

        assert_eq!(
            auth_env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-legacy")
        );
        assert!(auth_files.is_empty());
    }

    /// A missing `from_env` source is skipped (box still boots unauthenticated); a
    /// failing or empty `command` source fails loudly (a broken mint must not silently
    /// degrade to an unauthenticated run).
    #[test]
    fn resolve_agent_credentials_missing_env_skips_but_bad_command_fails() {
        let agent = agent_for_credentials(vec![config::CredentialConfig {
            from_env: Some("VARDA_TEST_UNSET_CRED_9x".to_owned()),
            env: Some("SHOULD_NOT_APPEAR".to_owned()),
            ..Default::default()
        }]);
        let (auth_env, _) = resolve_agent_credentials(&agent).unwrap();
        assert!(
            auth_env.is_empty(),
            "missing env source must be skipped: {auth_env:?}"
        );

        // Empty output fails loudly.
        let empty = agent_for_credentials(vec![config::CredentialConfig {
            command: Some("true".to_owned()),
            env: Some("X".to_owned()),
            ..Default::default()
        }]);
        assert!(
            resolve_agent_credentials(&empty).is_err(),
            "empty command output must fail"
        );

        // Non-zero exit fails loudly.
        let failing = agent_for_credentials(vec![config::CredentialConfig {
            command: Some("exit 3".to_owned()),
            env: Some("X".to_owned()),
            ..Default::default()
        }]);
        assert!(
            resolve_agent_credentials(&failing).is_err(),
            "failed command must fail"
        );
    }

    fn test_task_document() -> task::TaskDocument {
        task::TaskDocument {
            path: PathBuf::from("task.md"),
            frontmatter: task::TaskFrontmatter {
                bounds: crate::task::TaskBounds::default(),
                id: None,
                status: task::TaskStatus::Ready,
                project: None,
                mother_project: None,
                assignee: None,
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
            body: "# Task\n".to_owned(),
        }
    }

    #[test]
    fn finds_sessions_for_task() {
        let root = std::env::temp_dir().join(format!("varda-task-sessions-{}", std::process::id()));
        let operations_dir = root.join("operations");
        let runs_dir = operations_dir.join(config::RUNS_DIRNAME);
        let tasks_dir = operations_dir.join("tasks");
        fs::create_dir_all(&runs_dir).expect("runs directory should be created");
        fs::create_dir_all(&tasks_dir).expect("tasks directory should be created");
        let task_path = tasks_dir.join("mine.md");
        fs::write(&task_path, "# Mine\n").expect("task should be written");
        fs::write(
            runs_dir.join("session-1.log"),
            format!(
                "session_id=session-1\nagent=codex\ntask={}\n",
                task_path.display()
            ),
        )
        .expect("matching session should be written");
        fs::write(
            runs_dir.join("session-2.log"),
            "session_id=session-2\nagent=codex\ntask=/tmp/other.md\n",
        )
        .expect("other session should be written");

        let config = config::Config {
            defaults: config::Defaults {
                timeout_seconds: 600,
                operations_dir: operations_dir.display().to_string(),
                sandbox: None,
                ..Default::default()
            },
            routes: vec![],
            agents: std::collections::BTreeMap::new(),
            roles: std::collections::BTreeMap::new(),
            git: config::GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
            orchestration: crate::orchestration::OrchestrationPolicy::default(),
        };

        let sessions = task_sessions(&config, &task_path).expect("sessions should be found");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "session-1");
        assert_eq!(sessions[0].log_path, runs_dir.join("session-1.log"));

        fs::remove_dir_all(root).expect("test directory should be removable");
    }

    #[test]
    fn background_launch_failure_marks_task_failed_with_recap_and_log() {
        let root = std::env::temp_dir().join(format!(
            "varda-background-launch-failure-{}",
            uuid::Uuid::new_v4()
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
        let config = config::Config {
            defaults: config::Defaults {
                timeout_seconds: 600,
                operations_dir: operations_dir.display().to_string(),
                sandbox: None,
                ..Default::default()
            },
            routes: vec![],
            agents: std::collections::BTreeMap::new(),
            roles: std::collections::BTreeMap::new(),
            git: config::GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
            orchestration: crate::orchestration::OrchestrationPolicy::default(),
        };

        record_background_launch_failure(&config, &task_path, "agent binary was not found")
            .expect("failure should be recorded");

        let task = task::load_task(&task_path).expect("task should load");
        assert_eq!(task.frontmatter.status, task::TaskStatus::Failed);
        assert_eq!(task.frontmatter.agent_session_ids.len(), 1);
        assert_eq!(task.frontmatter.agent_session_logs.len(), 1);
        assert_eq!(task.frontmatter.recaps.len(), 1);

        let log = fs::read_to_string(&task.frontmatter.agent_session_logs[0])
            .expect("session log should be readable");
        let recap =
            fs::read_to_string(&task.frontmatter.recaps[0]).expect("recap should be readable");
        assert!(log.contains("launch_failure=agent binary was not found"));
        assert!(recap.contains("Agent Run Failed"));
        assert!(recap.contains("agent binary was not found"));

        fs::remove_dir_all(root).expect("test directory should be removable");
    }
}
