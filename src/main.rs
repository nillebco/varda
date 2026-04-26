mod acp;
mod agent;
mod config;
mod git;
mod notify;
mod routing;
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

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Init { force } => {
            println!("varda init is not implemented yet; force={force}");
        }
        Command::Run { task } => {
            println!("varda run is not implemented yet; task={}", task.display());
        }
    }

    Ok(())
}
