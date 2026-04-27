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
            TaskCommand::Add { taskname, project } => {
                let config_path = config::config_file()?;
                let config = config::load_config(&config_path)?;
                let project_path = task::resolve_project_path(project.as_deref())?;
                let default_route = routing::match_route(&config, &project_path, None)?;
                let default_assignee = default_route.agent;
                let assignee = prompt_assignee(&default_assignee)?;
                if let Some(assignee) = assignee.as_deref() {
                    routing::match_route(&config, &project_path, Some(assignee))?;
                }
                let task_path =
                    task::create_task(&config, &taskname, &project_path, assignee.as_deref())?;
                let task_id = task::load_task(&task_path)?.frontmatter.id;
                if let Some(task_id) = task_id {
                    println!("created task #{task_id} {}", task_path.display());
                } else {
                    println!("created task {}", task_path.display());
                }
                open_editor(&task_path)?;
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
    let (scope, considered_tasks) = if project_tasks.is_empty() {
        ("global", task::list_all_tasks(&config)?)
    } else {
        ("project", project_tasks)
    };
    let ready_tasks: Vec<_> = considered_tasks
        .iter()
        .filter(|task| task.status == task::TaskStatus::Ready)
        .cloned()
        .collect();
    let plan_path = write_execution_plan(&config, scope, &project_path, &ready_tasks)?;

    println!("scope: {scope}");
    if scope == "project" {
        println!("project: {}", project_path.display());
    } else {
        println!("project: none detected for current directory; considered all tasks");
    }
    println!("ready_tasks: {}", ready_tasks.len());
    println!("plan: {}", plan_path.display());
    println!("review the plan, edit if needed, then confirm before running tasks");

    Ok(())
}

fn write_execution_plan(
    config: &config::Config,
    scope: &str,
    project_path: &Path,
    ready_tasks: &[task::TaskSummary],
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
    let content = render_execution_plan(scope, project_path, timestamp, ready_tasks);

    fs::write(&plan_path, content)
        .with_context(|| format!("failed to write plan at {}", plan_path.display()))?;

    Ok(plan_path)
}

fn render_execution_plan(
    scope: &str,
    project_path: &Path,
    timestamp: u64,
    ready_tasks: &[task::TaskSummary],
) -> String {
    let title_scope = if scope == "project" {
        "Project"
    } else {
        "Global"
    };
    let mut content = format!(
        "# {title_scope} Ready Task Execution Plan\n\n- Scope: {scope}\n- Generated timestamp: {timestamp}\n- Project: {}\n- Ready tasks considered: {}\n- Planner agent: codex\n\n",
        project_path.display(),
        ready_tasks.len()
    );

    content.push_str("## Ready Tasks\n\n");
    if ready_tasks.is_empty() {
        content.push_str("No ready tasks were found.\n\n");
    } else {
        for task in ready_tasks {
            let id = task
                .id
                .map(|id| format!("#{id}"))
                .unwrap_or_else(|| "unversioned".to_owned());
            let assignee = task.assignee.as_deref().unwrap_or("route default");
            content.push_str(&format!(
                "- {id} `{}`: {} (agent: {assignee})\n",
                task.path.display(),
                task.title
            ));
        }
        content.push('\n');
    }

    content.push_str("## Priority And Dependencies\n\n");
    content.push_str("- First: `version-the-task`, because stable task IDs and captured starting state reduce ambiguity for every later run.\n");
    content.push_str("- Next: `assign-a-task-ID` if still not complete, because several ready tasks do not have IDs and task references are easier to review and run when numbered.\n");
    content.push_str("- Then: command surface fixes (`run-a-specific-task`, `global-run`, `global-runner`, `planner-agent`) because they define how work is selected and executed.\n");
    content.push_str("- Then: session support (`support-sessions`, `resume-session-interactively`) because resume behavior depends on run/session metadata.\n");
    content.push_str("- Then: visibility work (`tasks-dashboard-cli`, `tasks-dashboard`, `show-task`) after task identity and run semantics are stable.\n");
    content.push_str("- Optional/independent: `support-claude` can run after routing/session assumptions are clear; it may be parallel with dashboard work if agent configuration is isolated.\n\n");

    content.push_str("## Execution Stages\n\n");
    content.push_str("1. Stage 1, sequential: complete task identity/versioning work.\n");
    content.push_str("2. Stage 2, sequential: implement planning and run command semantics.\n");
    content.push_str("3. Stage 3, parallel candidates: session tracking/resume work and dashboard/listing work, provided they touch disjoint modules.\n");
    content.push_str("4. Stage 4, sequential validation: run the CLI against representative tasks, update docs, and confirm the plan before executing the ready set.\n\n");

    content.push_str("## User Review\n\n");
    content.push_str("Review and edit this plan before executing it. Execution should wait for explicit user confirmation.\n");

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
        git::commit_task_update(&task_path, &outcome.recap_path, notification.as_deref())?;
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
