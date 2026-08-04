mod acp;
mod agent;
mod config;
mod git;
mod notify;
mod routing;
mod runner;
mod task;

use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;

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
        /// New status (backlog, ready, running, pending, needs_user, failed, done).
        status: String,
        /// Markdown task file or task id to update.
        task: PathBuf,
    },
    /// Print the resolved file path for a task ID or path.
    Resolve {
        /// Markdown task file or task id to resolve.
        task: PathBuf,
    },
    /// Update task properties for a single task or in bulk.
    Update {
        /// Task file or task id to update. Omit to use filter flags for bulk selection.
        task: Option<PathBuf>,
        /// Set the task status (backlog, ready, running, pending, needs_user, failed, done).
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
                exec,
                edit,
                background,
                interactive,
                quiet,
                ready,
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
        },
        Command::Skill { command } => match command {
            SkillCommand::Install { source, link } => {
                skill_install_command(source.as_deref(), link)?;
            }
        },
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
    let client = acp::AcpSubprocessClient::new(&planner_agent, agent_config);
    let timeout = std::time::Duration::from_secs(config.defaults.timeout_seconds);
    let request = agent::AgentRunRequest {
        agent_name: planner_agent.clone(),
        role_instructions: None,
        task_path: plan_path.display().to_string(),
        frontmatter: task::TaskFrontmatter {
            id: None,
            status: task::TaskStatus::Ready,
            project: None,
            assignee: Some(planner_agent),
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
        runs.spawn(async move { run_task_path_for_parallel(config, task_path).await });
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

async fn run_task_path_for_parallel(
    config: config::Config,
    task_path: PathBuf,
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
    let client = acp::AcpSubprocessClient::new(&display_name, agent_config);
    let outcome = runner::run_task(
        &config,
        &display_name,
        route.role_instructions.as_deref(),
        &task_path,
        &client,
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
        if let Some(project) = summary.project.as_ref() {
            if !projects.contains(project) {
                projects.push(project.clone());
            }
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
        tasks,
        projects,
        statuses: vec![
            "backlog",
            "ready",
            "running",
            "needs_user",
            "failed",
            "pending",
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
        if input[i] == b'%' && i + 2 < input.len() {
            if let (Some(high), Some(low)) = (hex_value(input[i + 1]), hex_value(input[i + 2])) {
                output.push((high << 4) | low);
                i += 3;
                continue;
            }
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
    <label>Project <select id="projectFilter"><option value="">All projects</option></select></label>
    <label>Status <select id="statusFilter"><option value="">All statuses</option></select></label>
  </section>
  <main>
    <section id="board" class="board"></section>
    <aside id="details" class="empty"></aside>
  </main>
  <script>
    const statuses = ["backlog", "ready", "running", "needs_user", "failed", "pending", "done"];
    let payload = { tasks: [], projects: [], statuses };
    let selectedPath = "";
    let selectedDetail = null;
    let detailLoadingPath = "";
    let initializedFilters = false;

    function label(value) {
      return value.replaceAll("_", " ");
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
      const project = document.getElementById("projectFilter").value;
      const status = document.getElementById("statusFilter").value;
      return (!project || task.project === project) && (!status || task.status === status);
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
      optionList(document.getElementById("projectFilter"), payload.projects, "All projects");
      optionList(document.getElementById("statusFilter"), payload.statuses || statuses, "All statuses");
      if (!initializedFilters) {
        const defaultProject = payload.default_project || "";
        if (payload.projects.includes(defaultProject)) {
          document.getElementById("projectFilter").value = defaultProject;
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
        task::TaskStatus::Pending,
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
    let exe = std::env::current_exe().context("failed to locate the varda executable")?;
    let child = ProcessCommand::new(&exe)
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
    println!(
        "task running in background: {} (pid: {})",
        resolved.display(),
        child.id()
    );
    Ok(())
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
    let client = acp::AcpSubprocessClient::new(&display_name, agent_config);
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
        &client,
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
    let client = acp::AcpSubprocessClient::new(&display_name, agent_config);
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
    let client = acp::AcpSubprocessClient::new(&display_name, agent_config);
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
                if let Some(agent) = filter_agent {
                    if t.assignee.as_deref() != Some(agent) {
                        return false;
                    }
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

    sessions.sort_by(|left, right| right.modified.cmp(&left.modified));
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

    #[test]
    fn task_list_active_statuses_exclude_backlog_and_done() {
        use task::TaskStatus;

        for status in [
            TaskStatus::Ready,
            TaskStatus::Running,
            TaskStatus::Pending,
            TaskStatus::NeedsUser,
            TaskStatus::Failed,
        ] {
            assert!(is_active_task_status(status));
        }

        assert!(!is_active_task_status(TaskStatus::Backlog));
        assert!(!is_active_task_status(TaskStatus::Done));
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

    fn test_task_document() -> task::TaskDocument {
        task::TaskDocument {
            path: PathBuf::from("task.md"),
            frontmatter: task::TaskFrontmatter {
                id: None,
                status: task::TaskStatus::Ready,
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
            },
            routes: vec![],
            agents: std::collections::BTreeMap::new(),
            roles: std::collections::BTreeMap::new(),
            git: config::GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
        };

        let sessions = task_sessions(&config, &task_path).expect("sessions should be found");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "session-1");
        assert_eq!(sessions[0].log_path, runs_dir.join("session-1.log"));

        fs::remove_dir_all(root).expect("test directory should be removable");
    }
}
