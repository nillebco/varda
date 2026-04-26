//! Varda configuration loading and initialization.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

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
glob = "**"
agents = ["codex"]

[agents.codex]
kind = "acp"
command = "codex"
args = ["exec", "--cd", ".", "--sandbox", "workspace-write", "--ask-for-approval", "never", "-"]

[git]
auto_commit = true
"#;

const OPERATIONS_README_CONTENT: &str = r#"# Varda Operations

This folder contains task files, agent recaps, and run records managed by Varda.

- `tasks/`: markdown tasks with YAML frontmatter.
- `recaps/`: end-user recaps produced by agents.
- `runs/`: run metadata and notification records.
"#;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub defaults: Defaults,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub agents: std::collections::BTreeMap<String, AgentConfig>,
    #[serde(default)]
    pub git: GitConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Defaults {
    pub timeout_seconds: u64,
    pub operations_dir: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Route {
    pub glob: String,
    #[serde(default)]
    pub agents: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentConfig {
    pub kind: AgentKind,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    Acp,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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

pub fn save_config(path: impl AsRef<Path>, config: &Config) -> Result<()> {
    let path = path.as_ref();
    let content = toml::to_string_pretty(config).context("failed to serialize config")?;
    fs::write(path, content)
        .with_context(|| format!("failed to write config at {}", path.display()))?;

    Ok(())
}

pub fn add_project_route(path: impl AsRef<Path>, glob: String, agents: Vec<String>) -> Result<()> {
    if agents.is_empty() {
        bail!("project route must allow at least one agent");
    }

    let mut config = load_config(&path)?;

    for agent in &agents {
        if !config.agents.contains_key(agent) {
            bail!("unknown agent '{agent}'");
        }
    }

    config.routes.push(Route { glob, agents });
    save_config(path, &config)
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
        assert_eq!(config.routes[0].agents, vec!["codex"]);
        assert_eq!(config.agents["codex"].command, "codex");
        assert!(config.git.auto_commit);
    }

    #[test]
    fn appends_project_route() {
        let path = std::env::temp_dir().join(format!("varda-config-{}.toml", std::process::id()));
        fs::write(&path, DEFAULT_CONFIG).expect("config should be written");

        add_project_route(
            &path,
            "/work/project/**".to_owned(),
            vec!["codex".to_owned()],
        )
        .expect("project route should be appended");

        let config = load_config(&path).expect("config should reload");
        fs::remove_file(path).expect("config should be removed");

        assert_eq!(config.routes.len(), 2);
        assert_eq!(config.routes[1].glob, "/work/project/**");
        assert_eq!(config.routes[1].agents, vec!["codex"]);
    }
}
