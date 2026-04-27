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
        /// Markdown task file to resume.
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
        /// Markdown task file to display.
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
    let mut task_document = task::load_task(task_path)?;
    let was_needs_user = task_document.frontmatter.status == task::TaskStatus::NeedsUser;

    task_document.set_status(task::TaskStatus::Ready);
    task_document.frontmatter.requires_user = false;
    task::write_task(&task_document)?;

    if was_needs_user && prompt_yes_no("Open editor to complete the task update?", true)? {
        open_editor(task_path)?;
    }

    git::commit_task_file(task_path, &format!("Resume task {}", task_path.display()))?;
    println!("committed resume update");

    run_task_command(task_path).await
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
