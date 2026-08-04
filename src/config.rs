//! Varda configuration loading and initialization.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const VARDA_HOME_ENV: &str = "VARDA_HOME";
pub const CONFIG_FILENAME: &str = "config.toml";
pub const OPERATIONS_DIRNAME: &str = "operations";
pub const TASKS_DIRNAME: &str = "tasks";
pub const RECAPS_DIRNAME: &str = "recaps";
pub const RUNS_DIRNAME: &str = "runs";
pub const OPERATIONS_README: &str = "README.md";
/// Fallback sandbox provider when neither a route nor defaults specify one.
/// Wired into task execution in a later `SandboxProvider` milestone (M1).
#[allow(dead_code)]
pub const DEFAULT_SANDBOX_PROVIDER: &str = "local";

const DEFAULT_CONFIG: &str = r#"[defaults]
timeout_seconds = 600
operations_dir = "operations"

[[routes]]
glob = "**"
agents = ["codex"]

[agents.codex]
kind = "acp"
command = "codex"
args = ["exec", "--cd", ".", "--add-dir", "{varda_project}", "--add-dir", "{varda_home}", "--sandbox", "workspace-write", "-"]
interactive_command = "sh"
interactive_args = ["-c", "codex \"$(cat $VARDA_PROMPT_FILE)\" -C {project} --add-dir {varda_project} --add-dir {varda_home} -s workspace-write"]
resume_command_template = "codex resume -C {project} --add-dir {varda_project} --add-dir {varda_home} -s workspace-write {external_session_id}"

[agents.claude]
kind = "acp"
command = "claude"
args = ["-p", "--permission-mode", "acceptEdits", "--add-dir", "{project}", "--add-dir", "{varda_project}", "--add-dir", "{varda_home}"]
interactive_command = "sh"
interactive_args = ["-c", "claude \"$(cat $VARDA_PROMPT_FILE)\" --add-dir {project} --add-dir {varda_project} --add-dir {varda_home} --permission-mode acceptEdits"]
resume_command_template = "claude --resume {external_session_id} --add-dir {project} --add-dir {varda_project} --add-dir {varda_home} --permission-mode acceptEdits"

[agents.copilot]
kind = "acp"
command = "sh"
args = ["-c", "copilot -p \"$(cat)\" --allow-all-tools --add-dir {project} --add-dir {varda_project} --add-dir {varda_home} -s"]
resume_command_template = "copilot --resume={external_session_id} --add-dir {project} --add-dir {varda_project} --add-dir {varda_home} --allow-all-tools"

[roles.tester]
backend = "codex"
instructions = """
You are the tester agent. Your role is to verify an implementation after the implementation agent has finished.

Tester workflow:
- Read the task, any attached plan, existing recaps, and the current project state before deciding what to test.
- Define a concise test plan in your recap before or while executing it.
- Execute the practical checks needed to verify the implementation, using the project's existing verification commands when available.
- Decide explicitly whether the original task is complete.
- If verification succeeds, state that the implementation is verified and what evidence supports that decision.
- If verification fails, update the task with the failed checks and required follow-up when the task file is writable. In all cases, include the failed checks, exact follow-up work, and the suggested next agent to re-run the task.
- Only request user interaction when verification is blocked by missing information, credentials, environment access, or a decision that an agent cannot make."""

[git]
auto_commit = true
"#;

const OPERATIONS_README_CONTENT: &str = r#"# Varda Operations

This folder contains task files, agent recaps, and run records managed by Varda.

- `tasks/`: markdown tasks with YAML frontmatter, grouped by project folder.
- `recaps/`: end-user recaps produced by agents.
- `runs/`: run metadata and notification records.
"#;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    pub defaults: Defaults,
    #[serde(default)]
    pub routes: Vec<Route>,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentConfig>,
    #[serde(default)]
    pub roles: BTreeMap<String, RoleConfig>,
    #[serde(default)]
    pub git: GitConfig,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sandboxes: BTreeMap<String, SandboxConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoleConfig {
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Defaults {
    pub timeout_seconds: u64,
    pub operations_dir: String,
    /// Default sandbox provider applied to routes that do not set their own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Route {
    pub glob: String,
    #[serde(default)]
    pub agents: Vec<String>,
    /// Sandbox provider for this route; overrides the default when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SandboxConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Path to a Dockerfile to build the sandbox image from. When set, the
    /// docker provider builds it at `prepare()` and uses the resulting tag.
    /// Mutually exclusive-ish with `image` (build wins when both are set).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<String>,
    /// Isolation primitive: `"docker"` | `"microsandbox"` | `"clawk"` | `"local"`.
    /// Orthogonal to the image/rootfs: the same OCI image can run under docker
    /// (shared kernel) or an own-kernel microVM.
    #[serde(default = "default_primitive")]
    pub primitive: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub egress: Vec<String>,
}

/// Default isolation primitive when a `[sandboxes.<name>]` entry omits one.
pub fn default_primitive() -> String {
    "docker".to_owned()
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            image: None,
            build: None,
            primitive: default_primitive(),
            mounts: Vec::new(),
            egress: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentConfig {
    pub kind: AgentKind,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_prompt_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Command to use when running interactively (inherits terminal stdio).
    /// When set, the agent is spawned with all streams inherited from the terminal
    /// and $VARDA_PROMPT_FILE points to a file containing the task prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactive_command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactive_args: Option<Vec<String>>,
    /// Template used to build the resume command after an interactive session ends.
    /// `{external_session_id}` is replaced with the agent's own session id (discovered
    /// from the agent's session storage), and `{project}` with the task's project path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_command_template: Option<String>,
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

impl Config {
    /// Resolve the effective sandbox provider for a route.
    ///
    /// Precedence: `route.sandbox` → `defaults.sandbox` → [`DEFAULT_SANDBOX_PROVIDER`].
    pub fn effective_sandbox<'a>(&'a self, route: &'a Route) -> &'a str {
        route
            .sandbox
            .as_deref()
            .or(self.defaults.sandbox.as_deref())
            .unwrap_or(DEFAULT_SANDBOX_PROVIDER)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitResult {
    pub config_path: String,
    pub operations_dir: String,
}

pub fn init_workspace(force: bool) -> Result<InitResult> {
    let home = varda_home()?;
    let config_path = home.join(CONFIG_FILENAME);
    let operations_dir = home.join(OPERATIONS_DIRNAME);
    let tasks_dir = operations_dir.join(TASKS_DIRNAME);
    let recaps_dir = operations_dir.join(RECAPS_DIRNAME);
    let runs_dir = operations_dir.join(RUNS_DIRNAME);
    let operations_readme = operations_dir.join(OPERATIONS_README);

    if config_path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite it",
            config_path.display()
        );
    }

    fs::create_dir_all(&home)
        .with_context(|| format!("failed to create Varda home {}", home.display()))?;
    ensure_git_repo(&home)?;
    fs::create_dir_all(&tasks_dir).context("failed to create tasks directory")?;
    fs::create_dir_all(&recaps_dir).context("failed to create recaps directory")?;
    fs::create_dir_all(&runs_dir).context("failed to create runs directory")?;

    fs::write(&config_path, DEFAULT_CONFIG).context("failed to write default config")?;
    ensure_keep_file(&tasks_dir.join(".gitkeep"))?;
    ensure_keep_file(&recaps_dir.join(".gitkeep"))?;
    ensure_keep_file(&runs_dir.join(".gitkeep"))?;

    if !operations_readme.exists() || force {
        fs::write(&operations_readme, OPERATIONS_README_CONTENT)
            .context("failed to write operations README")?;
    }

    Ok(InitResult {
        config_path: config_path.display().to_string(),
        operations_dir: operations_dir.display().to_string(),
    })
}

fn ensure_git_repo(path: &Path) -> Result<()> {
    if path.join(".git").exists() {
        return Ok(());
    }

    let output = Command::new("git")
        .arg("init")
        .arg(path)
        .output()
        .with_context(|| format!("failed to start git init for {}", path.display()))?;

    if !output.status.success() {
        bail!(
            "git init {} failed with status {}; stderr: {}",
            path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

pub fn varda_home() -> Result<PathBuf> {
    if let Ok(home) = std::env::var(VARDA_HOME_ENV) {
        if !home.trim().is_empty() {
            return Ok(PathBuf::from(home));
        }
    }

    let home = std::env::var("HOME").context("HOME is not set and VARDA_HOME was not provided")?;
    Ok(PathBuf::from(home).join(".varda"))
}

pub fn config_file() -> Result<PathBuf> {
    Ok(varda_home()?.join(CONFIG_FILENAME))
}

pub fn load_config(path: impl AsRef<Path>) -> Result<Config> {
    let path = path.as_ref();
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    let mut config: Config = toml::from_str(&content)
        .with_context(|| format!("failed to parse config at {}", path.display()))?;
    resolve_config_paths(path, &mut config)?;
    remove_legacy_codex_exec_args(&mut config);
    add_varda_project_dir_to_default_agents(&mut config);

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

    let mut config = load_config_raw(&path)?;

    for agent in &agents {
        if !config.agents.contains_key(agent) && !config.roles.contains_key(agent) {
            bail!("unknown agent or role '{agent}'");
        }
    }

    config.routes.insert(
        0,
        Route {
            glob,
            agents,
            sandbox: None,
        },
    );
    save_config(path, &config)
}

fn load_config_raw(path: impl AsRef<Path>) -> Result<Config> {
    let path = path.as_ref();
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read config at {}", path.display()))?;
    let config = toml::from_str(&content)
        .with_context(|| format!("failed to parse config at {}", path.display()))?;

    Ok(config)
}

fn resolve_config_paths(path: &Path, config: &mut Config) -> Result<()> {
    let operations_dir = Path::new(&config.defaults.operations_dir);
    if operations_dir.is_absolute() {
        return Ok(());
    }

    let config_dir = path
        .parent()
        .with_context(|| format!("config path {} has no parent", path.display()))?;
    config.defaults.operations_dir = config_dir.join(operations_dir).display().to_string();

    Ok(())
}

fn remove_legacy_codex_exec_args(config: &mut Config) {
    for agent in config.agents.values_mut() {
        if agent.command != "codex" || !agent.args.iter().any(|arg| arg == "exec") {
            continue;
        }

        let mut cleaned = Vec::with_capacity(agent.args.len());
        let mut index = 0;

        while index < agent.args.len() {
            if agent.args[index] == "--ask-for-approval" {
                index += 1;
                if agent.args.get(index).is_some_and(|value| value == "never") {
                    index += 1;
                }
                continue;
            }

            cleaned.push(agent.args[index].clone());
            index += 1;
        }

        agent.args = cleaned;
    }
}

fn add_varda_project_dir_to_default_agents(config: &mut Config) {
    for agent in config.agents.values_mut() {
        match agent.command.as_str() {
            "codex" => add_codex_varda_project_dir(agent),
            "claude" => add_varda_dirs_as_arg_pairs(&mut agent.args),
            "sh" if agent.args.first().is_some_and(|arg| arg == "-c")
                && agent
                    .args
                    .get(1)
                    .is_some_and(|arg| arg.contains("copilot ")) =>
            {
                add_varda_dirs_to_shell_arg(&mut agent.args);
            }
            _ => {}
        }

        if agent
            .interactive_command
            .as_deref()
            .is_some_and(|command| command == "sh")
        {
            let is_wrapped_agent = agent
                .interactive_args
                .as_deref()
                .and_then(|args| args.get(1))
                .is_some_and(|arg| arg.contains("codex ") || arg.contains("claude "));
            if is_wrapped_agent {
                if let Some(args) = agent.interactive_args.as_mut() {
                    add_varda_dirs_to_shell_arg(args);
                }
            }
        }

        if let Some(template) = agent.resume_command_template.as_mut() {
            if template.contains("codex resume") && !template.contains(" -C ") {
                template.push_str(" -C {project}");
            }
            if template.contains("codex resume") && !template.contains(" -s ") {
                template.push_str(" -s workspace-write");
            }
            if template.contains("codex resume")
                || template.contains("claude --resume")
                || template.contains("copilot --resume=")
            {
                add_shell_fragment_once(template, "--add-dir {varda_project}", "{varda_project}");
                add_shell_fragment_once(template, "--add-dir {varda_home}", "{varda_home}");
            }
        }
    }
}

fn add_codex_varda_project_dir(agent: &mut AgentConfig) {
    let mut additions = Vec::new();
    if !agent.args.iter().any(|arg| arg == "{varda_project}") {
        additions.extend(["--add-dir".to_owned(), "{varda_project}".to_owned()]);
    }
    if !agent.args.iter().any(|arg| arg == "{varda_home}") {
        additions.extend(["--add-dir".to_owned(), "{varda_home}".to_owned()]);
    }
    if additions.is_empty() {
        return;
    }

    let insert_at = agent
        .args
        .iter()
        .position(|arg| arg == "--sandbox")
        .unwrap_or(agent.args.len());
    agent.args.splice(insert_at..insert_at, additions);
}

fn add_varda_dirs_as_arg_pairs(args: &mut Vec<String>) {
    add_arg_pair_once(args, "--add-dir", "{varda_project}");
    add_arg_pair_once(args, "--add-dir", "{varda_home}");
}

fn add_arg_pair_once(args: &mut Vec<String>, flag: &str, value: &str) {
    if args.iter().any(|arg| arg == value) {
        return;
    }
    args.push(flag.to_owned());
    args.push(value.to_owned());
}

fn add_varda_dirs_to_shell_arg(args: &mut [String]) {
    if let Some(shell_command) = args.get_mut(1) {
        add_shell_fragment_once(
            shell_command,
            "--add-dir {varda_project}",
            "{varda_project}",
        );
        add_shell_fragment_once(shell_command, "--add-dir {varda_home}", "{varda_home}");
    }
}

fn add_shell_fragment_once(command: &mut String, addition: &str, marker: &str) {
    if !command.contains(marker) {
        command.push(' ');
        command.push_str(addition);
    }
}

fn ensure_keep_file(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::write(path, "").with_context(|| format!("failed to write {}", path.display()))?;
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
        assert!(!config.agents.contains_key("tester"));
        assert_eq!(config.roles["tester"].backend, "codex");
        assert!(config.roles["tester"].instructions.is_some());
        assert_eq!(config.agents["claude"].command, "claude");
        assert_eq!(
            config.agents["claude"].args,
            vec![
                "-p",
                "--permission-mode",
                "acceptEdits",
                "--add-dir",
                "{project}",
                "--add-dir",
                "{varda_project}",
                "--add-dir",
                "{varda_home}"
            ]
        );
        assert_eq!(
            config.agents["claude"].interactive_command.as_deref(),
            Some("sh")
        );
        assert!(
            config.agents["claude"]
                .interactive_args
                .as_ref()
                .is_some_and(|a| a[0] == "-c")
        );
        assert_eq!(config.agents["copilot"].command, "sh");
        assert_eq!(
            config.agents["copilot"].args,
            vec![
                "-c",
                "copilot -p \"$(cat)\" --allow-all-tools --add-dir {project} --add-dir {varda_project} --add-dir {varda_home} -s"
            ]
        );
        assert_eq!(config.agents["codex"].max_prompt_tokens, None);
        assert!(
            !config.agents["codex"]
                .args
                .iter()
                .any(|arg| arg == "--ask-for-approval")
        );
        assert!(config.git.auto_commit);
    }

    #[test]
    fn strips_legacy_codex_exec_approval_args_on_load() {
        let path =
            std::env::temp_dir().join(format!("varda-legacy-codex-{}.toml", std::process::id()));
        let config = DEFAULT_CONFIG.replace(
            r#"args = ["exec", "--cd", ".", "--add-dir", "{varda_project}", "--add-dir", "{varda_home}", "--sandbox", "workspace-write", "-"]"#,
            r#"args = ["exec", "--cd", ".", "--sandbox", "workspace-write", "--ask-for-approval", "never", "-"]"#,
        );
        fs::write(&path, config).expect("config should be written");

        let config = load_config(&path).expect("legacy config should load");
        fs::remove_file(path).expect("config should be removed");

        assert_eq!(
            config.agents["codex"].args,
            vec![
                "exec",
                "--cd",
                ".",
                "--add-dir",
                "{varda_project}",
                "--add-dir",
                "{varda_home}",
                "--sandbox",
                "workspace-write",
                "-"
            ]
        );
    }

    #[test]
    fn prepends_project_route_before_catch_all() {
        let path = std::env::temp_dir().join(format!("varda-config-{}.toml", std::process::id()));
        fs::write(&path, DEFAULT_CONFIG).expect("config should be written");

        add_project_route(
            &path,
            "/work/project/**".to_owned(),
            vec!["codex".to_owned()],
        )
        .expect("project route should be prepended");

        let config = load_config(&path).expect("config should reload");
        fs::remove_file(path).expect("config should be removed");

        assert_eq!(config.routes.len(), 2);
        assert_eq!(config.routes[0].glob, "/work/project/**");
        assert_eq!(config.routes[0].agents, vec!["codex"]);
        assert_eq!(config.routes[1].glob, "**");
    }

    #[test]
    fn resolves_relative_operations_dir_against_config_dir() {
        let root = std::env::temp_dir().join(format!("varda-home-{}", std::process::id()));
        fs::create_dir_all(&root).expect("config directory should be created");
        let path = root.join("config.toml");
        fs::write(&path, DEFAULT_CONFIG).expect("config should be written");

        let config = load_config(&path).expect("config should load");

        assert_eq!(
            config.defaults.operations_dir,
            root.join("operations").display().to_string()
        );
    }

    #[test]
    fn default_config_round_trips_without_sandbox_keys() {
        let config: Config = toml::from_str(DEFAULT_CONFIG).expect("default config should parse");

        assert!(config.sandboxes.is_empty());
        assert!(config.defaults.sandbox.is_none());
        assert!(config.routes.iter().all(|route| route.sandbox.is_none()));

        let serialized = toml::to_string_pretty(&config).expect("config should serialize");
        // The only "sandbox" occurrences must be Codex's own `--sandbox` CLI args,
        // never our config keys (`sandbox = ` under [defaults]/[[routes]] or a
        // `[sandboxes]` table).
        assert!(
            !serialized.contains("sandbox = ") && !serialized.contains("[sandboxes"),
            "no sandbox config keys should be emitted when none are set: {serialized}"
        );

        let reparsed: Config = toml::from_str(&serialized).expect("serialized config should parse");
        assert_eq!(config, reparsed);
    }

    #[test]
    fn resolves_effective_sandbox_provider_precedence() {
        let mut config: Config =
            toml::from_str(DEFAULT_CONFIG).expect("default config should parse");

        let route = config.routes[0].clone();
        assert_eq!(config.effective_sandbox(&route), DEFAULT_SANDBOX_PROVIDER);

        config.defaults.sandbox = Some("docker".to_owned());
        assert_eq!(config.effective_sandbox(&route), "docker");

        let route_with_sandbox = Route {
            glob: "**".to_owned(),
            agents: vec!["codex".to_owned()],
            sandbox: Some("firejail".to_owned()),
        };
        assert_eq!(config.effective_sandbox(&route_with_sandbox), "firejail");
    }

    #[test]
    fn parses_sandboxes_table() {
        let toml = format!(
            "{DEFAULT_CONFIG}\n[sandboxes.docker]\nimage = \"varda:latest\"\nmounts = [\"/tmp\"]\negress = [\"api.example.com\"]\n"
        );
        let config: Config = toml::from_str(&toml).expect("config with sandboxes should parse");

        let docker = &config.sandboxes["docker"];
        assert_eq!(docker.image.as_deref(), Some("varda:latest"));
        assert_eq!(docker.mounts, vec!["/tmp"]);
        assert_eq!(docker.egress, vec!["api.example.com"]);
        // `primitive` defaults to "docker" when omitted, `build` to None.
        assert_eq!(docker.primitive, "docker");
        assert!(docker.build.is_none());
    }

    #[test]
    fn sandbox_config_round_trips_primitive_and_build() {
        let toml = format!(
            "{DEFAULT_CONFIG}\n[sandboxes.rustvm]\nbuild = \"./testdata/Dockerfile.rust\"\nprimitive = \"microsandbox\"\n"
        );
        let config: Config = toml::from_str(&toml).expect("config with build should parse");

        let rustvm = &config.sandboxes["rustvm"];
        assert!(rustvm.image.is_none());
        assert_eq!(rustvm.build.as_deref(), Some("./testdata/Dockerfile.rust"));
        assert_eq!(rustvm.primitive, "microsandbox");

        // Round-trip: serialize then reparse to an identical config.
        let serialized = toml::to_string_pretty(&config).expect("config should serialize");
        let reparsed: Config = toml::from_str(&serialized).expect("serialized config should parse");
        assert_eq!(config, reparsed);
        // The explicit primitive survives serialization.
        assert_eq!(reparsed.sandboxes["rustvm"].primitive, "microsandbox");
    }

    #[test]
    fn initializes_git_repo_when_needed() {
        let root = std::env::temp_dir().join(format!("varda-git-init-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).expect("old test directory should be removed");
        }
        fs::create_dir_all(&root).expect("test directory should be created");

        ensure_git_repo(&root).expect("git repo should initialize");

        assert!(root.join(".git").exists());
        fs::remove_dir_all(root).expect("test directory should be removed");
    }
}
