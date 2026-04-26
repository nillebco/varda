//! Varda configuration loading and initialization.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};

pub const VARDA_DIR: &str = ".varda";
pub const CONFIG_FILE: &str = ".varda/config.toml";
pub const OPERATIONS_DIR: &str = ".varda/operations";
pub const TASKS_DIR: &str = ".varda/operations/tasks";
pub const RECAPS_DIR: &str = ".varda/operations/recaps";
pub const RUNS_DIR: &str = ".varda/operations/runs";
pub const OPERATIONS_README: &str = ".varda/operations/README.md";
const TASKS_KEEP: &str = ".varda/operations/tasks/.gitkeep";
const RECAPS_KEEP: &str = ".varda/operations/recaps/.gitkeep";
const RUNS_KEEP: &str = ".varda/operations/runs/.gitkeep";

const DEFAULT_CONFIG: &str = r#"[defaults]
timeout_seconds = 600
operations_dir = ".varda/operations"

[[routes]]
glob = ".varda/operations/tasks/codex/**/*.md"
agent = "codex"

[agents.codex]
kind = "acp"
command = "codex"
args = ["--acp"]

[git]
auto_commit = true
"#;

const OPERATIONS_README_CONTENT: &str = r#"# Varda Operations

This folder contains task files, agent recaps, and run records managed by Varda.

- `tasks/`: markdown tasks with YAML frontmatter.
- `recaps/`: end-user recaps produced by agents.
- `runs/`: run metadata and notification records.
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitResult {
    pub config_path: String,
    pub operations_dir: String,
}

pub fn init_workspace(force: bool) -> Result<InitResult> {
    let config_path = Path::new(CONFIG_FILE);

    if config_path.exists() && !force {
        bail!("{CONFIG_FILE} already exists; pass --force to overwrite it");
    }

    fs::create_dir_all(VARDA_DIR).context("failed to create .varda directory")?;
    fs::create_dir_all(TASKS_DIR).context("failed to create tasks directory")?;
    fs::create_dir_all(RECAPS_DIR).context("failed to create recaps directory")?;
    fs::create_dir_all(RUNS_DIR).context("failed to create runs directory")?;

    fs::write(CONFIG_FILE, DEFAULT_CONFIG).context("failed to write default config")?;
    ensure_keep_file(TASKS_KEEP)?;
    ensure_keep_file(RECAPS_KEEP)?;
    ensure_keep_file(RUNS_KEEP)?;

    if !Path::new(OPERATIONS_README).exists() || force {
        fs::write(OPERATIONS_README, OPERATIONS_README_CONTENT)
            .context("failed to write operations README")?;
    }

    Ok(InitResult {
        config_path: CONFIG_FILE.to_owned(),
        operations_dir: OPERATIONS_DIR.to_owned(),
    })
}

fn ensure_keep_file(path: &str) -> Result<()> {
    if !Path::new(path).exists() {
        fs::write(path, "").with_context(|| format!("failed to write {path}"))?;
    }

    Ok(())
}
