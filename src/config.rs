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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Defaults {
    pub timeout_seconds: u64,
    pub operations_dir: String,
    /// Default sandbox provider applied to routes that do not set their own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// M6b hardening floor knobs — all clamp the UNTRUSTED `.varda` origin only;
    /// the central `config.toml` (routes/sandboxes) stays trusted.
    ///
    /// Allow a `.varda` to select `primitive = "local"` (escape the box). Default
    /// false: an attacker-influenceable `.varda` must never opt out of isolation.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_local_varda: bool,
    /// Allow a `.varda` mount to be writable. Default false: `.varda` mounts are
    /// forced `:ro`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_varda_writable_mounts: bool,
    /// Egress ceiling: if set, a `.varda` may not widen egress beyond this host
    /// allow-list. `None` ⇒ no ceiling clamp (still bounded by the trusted route).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub egress_ceiling: Option<Vec<String>>,
    /// Curated, READ-ONLY identity/context mounts — the sanctioned way to tell the
    /// agent "who the user is" WITHOUT mounting credential dirs. Each entry is a
    /// specific FILE (never a dir) following the `source[:target][:mode]` grammar,
    /// e.g. `"~/.claude/CLAUDE.md:/root/CLAUDE.md:ro"`. The credential-file denylist
    /// still applies so a `.credentials.json` can never sneak in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identity_context: Vec<String>,
}

/// serde `skip_serializing_if` helper: omit `false` booleans so the default
/// config round-trips without emitting the new hardening keys.
fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Route {
    pub glob: String,
    #[serde(default)]
    pub agents: Vec<String>,
    /// Sandbox provider for this route; overrides the default when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    /// Project-context mounts tied to the code↔context mapping (M6a). These
    /// compose with the image-intrinsic `[sandboxes.X].mounts` (effective set =
    /// their union). Each entry follows the `source[:target][:mode]` grammar
    /// parsed by [`crate::sandbox::parse_mount`]; both are trusted origins in
    /// M6a, so no hardening floor applies yet (that arrives with the untrusted
    /// `.varda` origin in M6b).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<String>,
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

/// Filename of the folder-local, repo-committed (UNTRUSTED) sandbox config.
/// Resolved into the live run path by [`Config::resolve_sandbox_for`], which is
/// invoked from `build_client` when a task carries a project path.
pub const VARDA_FILE: &str = ".varda";

/// A parsed `.varda` file. It carries a single `sandbox` key that is EITHER a
/// reference to a central `[sandboxes.X]` (string) OR an inline, self-contained
/// `[sandbox]` block (table). UNTRUSTED — always clamped by the M6b hardening
/// floor via [`resolve_sandbox_for`] before use.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct VardaFile {
    pub sandbox: VardaSandbox,
}

/// The two forms a `.varda` `sandbox` value may take.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum VardaSandbox {
    /// `sandbox = "rust"` — select a central `[sandboxes.rust]`.
    Reference(String),
    /// `[sandbox]` block — a self-contained sandbox definition.
    Inline(SandboxConfig),
}

/// The fully-resolved sandbox for a task path, after walk-up + precedence +
/// (for the untrusted `.varda` origin) the hardening floor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSandbox {
    /// Effective sandbox name: a central name, `"inline"` for an inline `.varda`,
    /// or `"local"`.
    pub name: String,
    /// Effective sandbox config (central table entry, inline `.varda`, or a
    /// synthetic `local`).
    pub config: SandboxConfig,
    /// Trusted project-context route mounts (origin `Route`).
    pub route_mounts: Vec<String>,
    /// UNTRUSTED, already-hardened `.varda` inline mounts (origin `Varda`), each a
    /// `source:target:mode` string ready to apply (source made absolute, forced
    /// `:ro` unless allowed).
    pub varda_mounts: Vec<String>,
    /// Path of the `.varda` that supplied the config, when one was used.
    pub varda_file: Option<PathBuf>,
}

/// Walk UP from `start` (inclusive) to `routing_root` (inclusive) and return the
/// nearest existing `.varda` file. `None` when none is found in range.
pub fn find_nearest_varda(start: &Path, routing_root: &Path) -> Option<PathBuf> {
    let mut dir = if start.is_file() {
        start.parent()?
    } else {
        start
    };
    loop {
        let candidate = dir.join(VARDA_FILE);
        if candidate.is_file() {
            return Some(candidate);
        }
        if dir == routing_root {
            return None;
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return None,
        }
    }
}

impl Config {
    /// Resolve the effective sandbox for a task at `project_path`, honoring the
    /// precedence `nearest .varda → central route (glob) → defaults.sandbox →
    /// "local"` and clamping the untrusted `.varda` origin with the M6b hardening
    /// floor. `routing_root` bounds the upward `.varda` walk.
    pub fn resolve_sandbox_for(
        &self,
        project_path: &Path,
        routing_root: &Path,
    ) -> Result<ResolvedSandbox> {
        // Trusted baseline from the central route: its glob-selected sandbox name
        // and project-context mounts. Used directly when no `.varda` applies.
        let route = crate::routing::find_route_public(self, project_path).ok();
        let route_mounts = route.map(|r| r.mounts.clone()).unwrap_or_default();
        let central_name = route
            .and_then(|r| r.sandbox.clone())
            .or_else(|| self.defaults.sandbox.clone())
            .unwrap_or_else(|| DEFAULT_SANDBOX_PROVIDER.to_owned());

        let Some(varda_path) = find_nearest_varda(project_path, routing_root) else {
            let config = self.sandbox_config_by_name(&central_name);
            return Ok(ResolvedSandbox {
                name: central_name,
                config,
                route_mounts,
                varda_mounts: Vec::new(),
                varda_file: None,
            });
        };

        let text = fs::read_to_string(&varda_path)
            .with_context(|| format!("failed to read `.varda` at {}", varda_path.display()))?;
        let parsed: VardaFile = toml::from_str(&text)
            .with_context(|| format!("failed to parse `.varda` at {}", varda_path.display()))?;
        let varda_dir = varda_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        match parsed.sandbox {
            VardaSandbox::Reference(name) => {
                let config = self.sandbox_config_by_name(&name);
                self.enforce_varda_primitive_floor(&config.primitive, &varda_path)?;
                Ok(ResolvedSandbox {
                    name,
                    config,
                    route_mounts,
                    varda_mounts: Vec::new(),
                    varda_file: Some(varda_path),
                })
            }
            VardaSandbox::Inline(config) => {
                self.enforce_varda_primitive_floor(&config.primitive, &varda_path)?;
                self.enforce_egress_ceiling(&config.egress, &varda_path)?;
                let varda_mounts =
                    self.harden_inline_varda_mounts(&config.mounts, project_path, &varda_dir, &varda_path)?;
                let config = SandboxConfig {
                    mounts: Vec::new(),
                    ..config
                };
                Ok(ResolvedSandbox {
                    name: "inline".to_owned(),
                    config,
                    route_mounts,
                    varda_mounts,
                    varda_file: Some(varda_path),
                })
            }
        }
    }

    fn sandbox_config_by_name(&self, name: &str) -> SandboxConfig {
        if name == DEFAULT_SANDBOX_PROVIDER {
            return SandboxConfig {
                primitive: DEFAULT_SANDBOX_PROVIDER.to_owned(),
                ..SandboxConfig::default()
            };
        }
        self.sandboxes.get(name).cloned().unwrap_or_default()
    }

    /// Floor: an untrusted `.varda` may not select `primitive = "local"` (escape
    /// the box) unless `defaults.allow_local_varda`.
    fn enforce_varda_primitive_floor(&self, primitive: &str, varda_file: &Path) -> Result<()> {
        if primitive == "local" && !self.defaults.allow_local_varda {
            bail!(
                "`.varda` at {} selects `primitive = \"local\"` (escapes the sandbox); \
                 refused unless `defaults.allow_local_varda = true`",
                varda_file.display()
            );
        }
        Ok(())
    }

    /// Floor: an untrusted `.varda` may not widen egress beyond
    /// `defaults.egress_ceiling` (when set).
    fn enforce_egress_ceiling(&self, egress: &[String], varda_file: &Path) -> Result<()> {
        if let Some(ceiling) = &self.defaults.egress_ceiling {
            for host in egress {
                if !ceiling.iter().any(|allowed| allowed == host) {
                    bail!(
                        "`.varda` at {} requests egress host '{host}' beyond `defaults.egress_ceiling`",
                        varda_file.display()
                    );
                }
            }
        }
        Ok(())
    }

    /// Floor: harden each inline `.varda` mount (in-tree SOURCE, credential
    /// denylist, forced `:ro` unless allowed, safe TARGET) and return them as
    /// ready-to-apply `source:target:mode` strings.
    fn harden_inline_varda_mounts(
        &self,
        mounts: &[String],
        project_root: &Path,
        varda_dir: &Path,
        varda_file: &Path,
    ) -> Result<Vec<String>> {
        let mut out = Vec::with_capacity(mounts.len());
        for raw in mounts {
            let spec = crate::sandbox::parse_mount(raw)
                .with_context(|| format!("invalid `.varda` mount '{raw}' in {}", varda_file.display()))?;
            // `.varda` mount paths are relative to the `.varda` dir, not the
            // project root; make SOURCE absolute against `varda_dir` first.
            let source = if spec.source.is_absolute() {
                spec.source.clone()
            } else {
                varda_dir.join(&spec.source)
            };
            crate::sandbox::check_credential_denylist(&source)?;
            let abs_spec = crate::sandbox::MountSpec {
                source,
                target: spec.target.clone(),
                writable: spec.writable,
            };
            let hardened = crate::sandbox::harden_varda_mount(
                &abs_spec,
                project_root,
                self.defaults.allow_varda_writable_mounts,
                varda_file,
            )?;
            let mode = if hardened.writable { "rw" } else { "ro" };
            out.push(format!(
                "{}:{}:{mode}",
                hardened.source.display(),
                hardened.target.display()
            ));
        }
        Ok(out)
    }
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
            mounts: Vec::new(),
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
            mounts: Vec::new(),
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

#[cfg(test)]
mod m6b_tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("varda-m6b-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    fn base_config() -> Config {
        let mut c: Config = toml::from_str(DEFAULT_CONFIG).unwrap();
        c.routes = vec![Route {
            glob: "**".to_owned(),
            agents: vec!["codex".to_owned()],
            sandbox: None,
            mounts: vec![],
        }];
        c
    }

    #[test]
    fn walk_up_finds_nearest_varda() {
        let root = tmp("walkup");
        let nested = root.join("a/b/c");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("a").join(VARDA_FILE), "sandbox = \"rust\"\n").unwrap();
        fs::write(nested.join(VARDA_FILE), "sandbox = \"go\"\n").unwrap();

        let found = find_nearest_varda(&nested, &root).unwrap();
        assert_eq!(found, nested.join(VARDA_FILE));

        // From the middle dir the higher `.varda` is the nearest.
        let mid = root.join("a/b");
        assert_eq!(
            find_nearest_varda(&mid, &root).unwrap(),
            root.join("a").join(VARDA_FILE)
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reference_and_central_precedence() {
        let root = tmp("ref");
        let proj = root.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let mut config = base_config();
        config.sandboxes.insert(
            "rust".to_owned(),
            SandboxConfig {
                image: Some("rust:latest".to_owned()),
                ..SandboxConfig::default()
            },
        );

        // No `.varda` ⇒ central route/defaults ⇒ "local".
        let r = config.resolve_sandbox_for(&proj, &root).unwrap();
        assert_eq!(r.name, "local");
        assert!(r.varda_file.is_none());

        // Reference `.varda` selects the central sandbox by name.
        fs::write(proj.join(VARDA_FILE), "sandbox = \"rust\"\n").unwrap();
        let r = config.resolve_sandbox_for(&proj, &root).unwrap();
        assert_eq!(r.name, "rust");
        assert_eq!(r.config.image.as_deref(), Some("rust:latest"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn inline_varda_mount_is_hardened_and_ro() {
        let root = tmp("inline");
        let proj = root.join("proj");
        fs::create_dir_all(proj.join("ctx")).unwrap();
        let config = base_config();
        fs::write(
            proj.join(VARDA_FILE),
            "[sandbox]\nimage = \"x:1\"\nmounts = [\"ctx:/ctx\"]\n",
        )
        .unwrap();

        let r = config.resolve_sandbox_for(&proj, &root).unwrap();
        assert_eq!(r.name, "inline");
        assert_eq!(r.varda_mounts.len(), 1);
        // Forced :ro (writable not allowed by default) and source made absolute.
        assert!(r.varda_mounts[0].ends_with(":/ctx:ro"));
        assert!(r.varda_mounts[0].starts_with(proj.join("ctx").to_str().unwrap()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn floor_rejects_local_primitive() {
        let root = tmp("local");
        let proj = root.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let config = base_config();
        fs::write(
            proj.join(VARDA_FILE),
            "[sandbox]\nimage = \"x:1\"\nprimitive = \"local\"\n",
        )
        .unwrap();
        let err = config.resolve_sandbox_for(&proj, &root).unwrap_err();
        assert!(err.to_string().contains("primitive"), "{err}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn floor_rejects_out_of_tree_source() {
        let root = tmp("outoftree");
        let proj = root.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let config = base_config();
        fs::write(
            proj.join(VARDA_FILE),
            "[sandbox]\nimage = \"x:1\"\nmounts = [\"/etc:/data\"]\n",
        )
        .unwrap();
        let err = config.resolve_sandbox_for(&proj, &root).unwrap_err();
        assert!(err.to_string().contains("outside the project root"), "{err}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn floor_rejects_system_target_and_egress_over_ceiling() {
        let root = tmp("systgt");
        let proj = root.join("proj");
        fs::create_dir_all(proj.join("ctx")).unwrap();
        let config = base_config();
        fs::write(
            proj.join(VARDA_FILE),
            "[sandbox]\nimage = \"x:1\"\nmounts = [\"ctx:/etc\"]\n",
        )
        .unwrap();
        let err = config.resolve_sandbox_for(&proj, &root).unwrap_err();
        assert!(err.to_string().contains("system dir"), "{err}");

        // Egress ceiling clamp.
        let mut config = base_config();
        config.defaults.egress_ceiling = Some(vec!["api.ok.com".to_owned()]);
        fs::write(
            proj.join(VARDA_FILE),
            "[sandbox]\nimage = \"x:1\"\negress = [\"evil.example.com\"]\n",
        )
        .unwrap();
        let err = config.resolve_sandbox_for(&proj, &root).unwrap_err();
        assert!(err.to_string().contains("egress_ceiling"), "{err}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn writable_varda_mount_allowed_when_opted_in() {
        let root = tmp("writable");
        let proj = root.join("proj");
        fs::create_dir_all(proj.join("ctx")).unwrap();
        let mut config = base_config();
        config.defaults.allow_varda_writable_mounts = true;
        fs::write(
            proj.join(VARDA_FILE),
            "[sandbox]\nimage = \"x:1\"\nmounts = [\"ctx:/ctx:rw\"]\n",
        )
        .unwrap();
        let r = config.resolve_sandbox_for(&proj, &root).unwrap();
        assert!(r.varda_mounts[0].ends_with(":/ctx:rw"), "{:?}", r.varda_mounts);
        let _ = fs::remove_dir_all(&root);
    }

    /// M6b-wire: the LIVE run path (resolve → merge origins → build provider), not
    /// `resolve_sandbox_for` in isolation. A reference `.varda` in a project
    /// SUBFOLDER selects that central sandbox's provider at run time.
    #[test]
    fn run_path_reference_varda_selects_provider() {
        let root = tmp("runref");
        let sub = root.join("service");
        fs::create_dir_all(&sub).unwrap();
        let mut config = base_config();
        config.sandboxes.insert(
            "rust".to_owned(),
            SandboxConfig {
                image: Some("rust:latest".to_owned()),
                primitive: "docker".to_owned(),
                ..SandboxConfig::default()
            },
        );
        fs::write(sub.join(VARDA_FILE), "sandbox = \"rust\"\n").unwrap();

        let resolved = config.resolve_sandbox_for(&sub, &root).unwrap();
        let mounts = crate::sandbox::merge_mount_origins(
            &resolved.config.mounts,
            &resolved.route_mounts,
            &resolved.varda_mounts,
        );
        let provider =
            crate::sandbox::provider_from_config(&resolved.name, &resolved.config, mounts).unwrap();
        assert_eq!(provider.name(), "rust");
        let _ = fs::remove_dir_all(&root);
    }

    /// M6b-wire: an inline `.varda` mount flows through the run path as a
    /// `MountOrigin::Varda` in the merged set handed to the provider (hardened,
    /// `:ro`), so it is applied rather than dropped.
    #[test]
    fn run_path_inline_varda_produces_varda_origin() {
        let root = tmp("runinline");
        let proj = root.join("proj");
        fs::create_dir_all(proj.join("ctx")).unwrap();
        let config = base_config();
        fs::write(
            proj.join(VARDA_FILE),
            "[sandbox]\nimage = \"x:1\"\nmounts = [\"ctx:/ctx\"]\n",
        )
        .unwrap();

        let resolved = config.resolve_sandbox_for(&proj, &root).unwrap();
        let mounts = crate::sandbox::merge_mount_origins(
            &resolved.config.mounts,
            &resolved.route_mounts,
            &resolved.varda_mounts,
        );
        let varda: Vec<_> = mounts
            .iter()
            .filter(|(origin, _)| *origin == crate::sandbox::MountOrigin::Varda)
            .collect();
        assert_eq!(varda.len(), 1, "expected one Varda-origin mount: {mounts:?}");
        assert!(varda[0].1.ends_with(":/ctx:ro"), "{:?}", varda[0].1);
        // The provider builds from the inline config.
        let provider =
            crate::sandbox::provider_from_config(&resolved.name, &resolved.config, mounts).unwrap();
        assert_eq!(provider.name(), "inline");
        let _ = fs::remove_dir_all(&root);
    }

    /// M6b-wire: a floor-violating inline `.varda` (primitive = "local", i.e. an
    /// escape from the box) refuses the run at resolution with a clear error,
    /// before any provider is built.
    #[test]
    fn run_path_floor_violation_refuses_before_provider() {
        let root = tmp("runfloor");
        let proj = root.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let config = base_config();
        fs::write(
            proj.join(VARDA_FILE),
            "[sandbox]\nimage = \"x:1\"\nprimitive = \"local\"\n",
        )
        .unwrap();
        let err = config.resolve_sandbox_for(&proj, &root).unwrap_err();
        assert!(err.to_string().contains("primitive"), "{err}");
        let _ = fs::remove_dir_all(&root);
    }
}
