mod acp;
mod agent;
mod config;
mod git;
mod notify;
mod routing;
mod runner;
mod task;

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "varda")]
#[command(about = "Drive ACP agents from markdown operations tasks")]
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
    /// Run a markdown task through the configured agent.
    Run {
        /// Markdown task file or task id to process.
        task: PathBuf,
    },
    /// Create a reviewable execution plan for ready tasks.
    Plan,
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
    /// Show stored operation records.
    Show {
        #[command(subcommand)]
        command: ShowCommand,
    },
}

#[derive(Debug, Subcommand)]
enum TaskCommand {
    /// Create a new markdown task and open it in $EDITOR.
    Add {
        /// Human-readable task name.
        taskname: String,
        /// Project path this task belongs to. Defaults to the current directory.
        #[arg(long)]
        project: Option<PathBuf>,
        /// Agent to assign the task to. Skips the interactive assignee prompt.
        #[arg(long)]
        agent: Option<String>,
        /// Treat the task name as a complete one-line task and run it immediately.
        #[arg(long)]
        exec: bool,
    },
    /// List markdown tasks for a project.
    List {
        /// Project path to list tasks for. Defaults to the current directory.
        #[arg(long)]
        project: Option<PathBuf>,
    },
    /// Run a markdown task through the configured agent.
    Run {
        /// Markdown task file or task id to process.
        task: PathBuf,
    },
    /// Resume a task that is waiting for user input, then run it.
    Resume {
        /// Markdown task file or task id to resume.
        task: PathBuf,
    },
    /// Display a markdown task and its associated recap.
    Show {
        /// Markdown task file or task id to display.
        task: PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum ShowCommand {
    /// Display a markdown task and its associated recap.
    Task {
        /// Markdown task file or task id to display.
        task: PathBuf,
    },
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
        Command::Run { task } => {
            run_task_command(&task).await?;
        }
        Command::Plan => {
            plan_command()?;
        }
        Command::Task { command } => match command {
            TaskCommand::Add {
                taskname,
                project,
                agent,
                exec,
            } => {
                let config_path = config::config_file()?;
                let config = config::load_config(&config_path)?;
                let project_path = task::resolve_project_path(project.as_deref())?;
                let assignee = if let Some(ref agent_name) = agent {
                    routing::match_route(&config, &project_path, Some(agent_name))?;
                    Some(agent_name.clone())
                } else {
                    let default_route = routing::match_route(&config, &project_path, None)?;
                    let default_assignee = default_route.agent;
                    let assignee = prompt_assignee(&default_assignee)?;
                    if let Some(assignee) = assignee.as_deref() {
                        routing::match_route(&config, &project_path, Some(assignee))?;
                    }
                    assignee
                };
                let task_path =
                    task::create_task(&config, &taskname, &project_path, assignee.as_deref())?;
                let task_id = task::load_task(&task_path)?.frontmatter.id;
                if let Some(task_id) = task_id {
                    println!("created task #{task_id} {}", task_path.display());
                } else {
                    println!("created task {}", task_path.display());
                }
                if exec {
                    run_task_command(&task_path).await?;
                } else {
                    open_editor(&task_path)?;
                }
            }
            TaskCommand::List { project } => {
                let config_path = config::config_file()?;
                let config = config::load_config(&config_path)?;
                let project_path = task::resolve_project_path(project.as_deref())?;
                let tasks = task::list_tasks(&config, &project_path)?;
                print_task_list(&project_path, &tasks);
            }
            TaskCommand::Run { task } => {
                run_task_command(&task).await?;
            }
            TaskCommand::Resume { task } => {
                resume_task_command(&task).await?;
            }
            TaskCommand::Show { task } => {
                show_task_command(&task)?;
            }
        },
        Command::Show { command } => match command {
            ShowCommand::Task { task } => {
                show_task_command(&task)?;
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
        let project_path = summary
            .project
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let route = routing::match_route(config, &project_path, summary.assignee.as_deref())?;
        plan_tasks.push(PlanTask {
            summary: summary.clone(),
            agent: route.agent,
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
    ready_tasks: &[PlanTask],
) -> String {
    let title_scope = if scope == "project" {
        "Project"
    } else {
        "Global"
    };
    let mut content = format!(
        "# {title_scope} Ready Task Execution Plan\n\n- Scope: {scope}\n- Generated timestamp: {timestamp}\n- Project: `{}`\n- Selection rule: {selection_reason}.\n- Ready tasks considered: {}\n- Planner agent: codex\n- Execution should wait for explicit user confirmation.\n\n",
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

fn unix_timestamp() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before the Unix epoch")?
        .as_secs())
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

    let Some(recap_path) = task_document.frontmatter.recap.as_deref() else {
        println!("# Recap");
        println!();
        println!("No recap is associated with this task.");
        return Ok(());
    };

    let recap_path = resolve_recap_path(recap_path, &task_path);
    let recap_content = fs::read_to_string(&recap_path)
        .with_context(|| format!("failed to read recap at {}", recap_path.display()))?;

    println!("# Recap {}", recap_path.display());
    println!();
    print!("{recap_content}");
    if !recap_content.ends_with('\n') {
        println!();
    }

    Ok(())
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

fn truncate_for_table(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        return value.to_owned();
    }

    let mut truncated: String = value.chars().take(width.saturating_sub(1)).collect();
    truncated.push('.');
    truncated
}

async fn run_task_command(task_path: &Path) -> Result<()> {
    let config_path = config::config_file()?;
    let config = config::load_config(&config_path)?;
    let task_path = task::resolve_task_reference(&config, task_path)?;
    let task_document = task::load_task(&task_path)?;
    let project_path = task::task_project_path(&task_document)?;
    let route = routing::match_route(
        &config,
        &project_path,
        task_document.frontmatter.assignee.as_deref(),
    )?;
    let agent_config = config
        .agents
        .get(&route.agent)
        .expect("routing ensures the selected agent exists");
    let client = acp::AcpSubprocessClient::new(&route.agent, agent_config);
    if config.git.auto_commit {
        git::commit_task_file(
            &task_path,
            &format!("Snapshot task {} before run", task_path.display()),
        )?;
        println!("committed task snapshot");
    }
    let outcome = runner::run_task(&config, &route.agent, &task_path, &client).await?;
    println!(
        "processed task={} agent={} glob={} status={:?} recap={}",
        task_path.display(),
        route.agent,
        route.glob,
        outcome.status,
        outcome.recap_path.display()
    );
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
        git::commit_task_update(
            &task_path,
            &outcome.recap_path,
            notification.as_deref(),
        )?;
        println!("committed task update");
    }

    Ok(())
}

async fn resume_task_command(task_path: &Path) -> Result<()> {
    let config_path = config::config_file()?;
    let config = config::load_config(&config_path)?;
    let task_path = task::resolve_task_reference(&config, task_path)?;
    let mut task_document = task::load_task(&task_path)?;
    let was_needs_user = task_document.frontmatter.status == task::TaskStatus::NeedsUser;

    task_document.set_status(task::TaskStatus::Ready);
    task_document.frontmatter.requires_user = false;
    task::write_task(&task_document)?;

    if was_needs_user && prompt_yes_no("Open editor to complete the task update?", true)? {
        open_editor(&task_path)?;
    }

    git::commit_task_file(&task_path, &format!("Resume task {}", task_path.display()))?;
    println!("committed resume update");

    run_task_command(&task_path).await
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
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_owned());
    let status = ProcessCommand::new(&editor).arg(path).status()?;

    if !status.success() {
        anyhow::bail!("editor '{editor}' exited with status {status}");
    }

    Ok(())
}
