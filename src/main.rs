mod acp;
mod agent;
mod config;
mod git;
mod notify;
mod routing;
mod runner;
mod task;

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

use anyhow::Result;
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
        /// Markdown task file to process.
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
            let config = config::load_config(config::CONFIG_FILE)?;
            let task_document = task::load_task(&task)?;
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
            let outcome = runner::run_task(&config, &route.agent, &task, &client).await?;
            println!(
                "processed task={} agent={} glob={} status={:?} recap={}",
                task.display(),
                route.agent,
                route.glob,
                outcome.status,
                outcome.recap_path.display()
            );
            let notification = if outcome.status == task::TaskStatus::NeedsUser {
                let notification =
                    notify::notify_user_interaction(&config, &task, &outcome.recap_path)?;
                println!(
                    "user interaction required; notification={}",
                    notification.display()
                );
                Some(notification)
            } else {
                None
            };
            if config.git.auto_commit {
                git::commit_task_update(&task, &outcome.recap_path, notification.as_deref())?;
                println!("committed task update");
            }
        }
        Command::Task { command } => match command {
            TaskCommand::Add { taskname, project } => {
                let config = config::load_config(config::CONFIG_FILE)?;
                let project_path = task::resolve_project_path(project.as_deref())?;
                let default_route = routing::match_route(&config, &project_path, None)?;
                let default_assignee = default_route.agent;
                let assignee = prompt_assignee(&default_assignee)?;
                if let Some(assignee) = assignee.as_deref() {
                    routing::match_route(&config, &project_path, Some(assignee))?;
                }
                let task_path =
                    task::create_task(&config, &taskname, &project_path, assignee.as_deref())?;
                println!("created task {}", task_path.display());
                open_editor(&task_path)?;
            }
        },
        Command::Project { command } => match command {
            ProjectCommand::Add { glob, agents } => {
                config::add_project_route(config::CONFIG_FILE, glob.clone(), agents.clone())?;
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

fn open_editor(path: &Path) -> Result<()> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_owned());
    let status = ProcessCommand::new(&editor).arg(path).status()?;

    if !status.success() {
        anyhow::bail!("editor '{editor}' exited with status {status}");
    }

    Ok(())
}
