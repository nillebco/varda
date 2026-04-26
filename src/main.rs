mod acp;
mod agent;
mod config;
mod git;
mod notify;
mod routing;
mod runner;
mod task;

use std::path::PathBuf;

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
        }
    }

    Ok(())
}
