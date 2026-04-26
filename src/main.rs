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
            let route = routing::match_route(&config, &task)?;
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
            TaskCommand::Add { taskname } => {
                let config = config::load_config(config::CONFIG_FILE)?;
                let default_assignee = task::default_assignee(&config)?;
                let assignee = prompt_assignee(&default_assignee)?;
                let task_path = task::create_task(&config, &taskname, assignee.as_deref())?;
                println!("created task {}", task_path.display());
                open_editor(&task_path)?;
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
