//! Varda configuration loading and initialization.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

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

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub defaults: Defaults,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub agents: std::collections::BTreeMap<String, AgentConfig>,
    #[serde(default)]
    pub git: GitConfig,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Defaults {
    pub timeout_seconds: u64,
    pub operations_dir: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Route {
    pub glob: String,
    pub agent: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AgentConfig {
    pub kind: AgentKind,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Acp,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct GitConfig {
    #[serde(default = "default_auto_commit")]
    pub auto_commit: bool,
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            auto_commit: default_auto_commit(),
        }
    }
}

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

pub fn load_config(path: impl AsRef<Path>) -> Result<Config> {
    let path = path.as_ref();
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    let config = toml::from_str(&content)
        .with_context(|| format!("failed to parse config at {}", path.display()))?;

    Ok(config)
}

fn ensure_keep_file(path: &str) -> Result<()> {
    if !Path::new(path).exists() {
        fs::write(path, "").with_context(|| format!("failed to write {path}"))?;
    }

    Ok(())
}

fn default_auto_commit() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_config() {
        let config: Config = toml::from_str(DEFAULT_CONFIG).expect("default config should parse");

        assert_eq!(config.defaults.timeout_seconds, 600);
        assert_eq!(config.routes[0].agent, "codex");
        assert_eq!(config.agents["codex"].command, "codex");
        assert!(config.git.auto_commit);
    }
}
