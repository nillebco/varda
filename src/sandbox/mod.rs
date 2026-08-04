//! Sandbox provider abstraction.
//!
//! A [`SandboxProvider`] decides *how* an agent subprocess is launched. The
//! spawn itself lives in [`crate::acp::AcpSubprocessClient`]; before building
//! the `Command`, the client runs the fully-resolved [`CommandSpec`] through a
//! [`SandboxSession::wrap`] so the provider can rewrite the invocation (e.g.
//! wrap it in `docker run`). The [`LocalProvider`] is the identity provider:
//! `wrap` returns the spec unchanged, so the un-sandboxed path is provably the
//! same behavior as before providers existed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::config::{AgentKind, SandboxConfig};

/// A fully-resolved subprocess invocation, before it is handed to the OS.
///
/// Providers rewrite this in [`SandboxSession::wrap`]. For `local` it is the
/// literal command; for `docker` the whole thing is folded into `docker run …`
/// arguments and `env` is emptied (moved into `-e K=V` flags).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
}

/// Read-only context describing the task being launched.
///
/// `route_glob`, `agent_kind`, and `session_id` are threaded through for
/// providers that will key on them in later milestones (image selection,
/// per-session container names); the M1 providers don't read them yet.
#[allow(dead_code)]
pub struct SandboxContext<'a> {
    pub project_root: &'a Path,
    pub route_glob: &'a str,
    pub agent_kind: AgentKind,
    pub session_id: &'a str,
}

#[async_trait]
pub trait SandboxProvider: Send + Sync {
    fn name(&self) -> &str;
    async fn prepare(&self, ctx: &SandboxContext<'_>) -> Result<Box<dyn SandboxSession>>;
}

#[async_trait]
pub trait SandboxSession: Send + Sync {
    /// Rewrite a resolved command for execution inside this sandbox.
    fn wrap(&self, spec: CommandSpec) -> Result<CommandSpec>;
    /// Filesystem root under which the agent's own session store lives, when it
    /// is reachable from the host. `None` degrades resume-capture (M1 docker).
    fn session_store_root(&self) -> Option<PathBuf>;
    async fn teardown(self: Box<Self>) -> Result<()>;
}

/// Identity provider: no isolation, `wrap` returns the spec unchanged.
pub struct LocalProvider;

#[async_trait]
impl SandboxProvider for LocalProvider {
    fn name(&self) -> &str {
        "local"
    }

    async fn prepare(&self, _ctx: &SandboxContext<'_>) -> Result<Box<dyn SandboxSession>> {
        Ok(Box::new(LocalSession))
    }
}

pub struct LocalSession;

#[async_trait]
impl SandboxSession for LocalSession {
    fn wrap(&self, spec: CommandSpec) -> Result<CommandSpec> {
        Ok(spec)
    }

    fn session_store_root(&self) -> Option<PathBuf> {
        std::env::var_os("HOME").map(PathBuf::from)
    }

    async fn teardown(self: Box<Self>) -> Result<()> {
        Ok(())
    }
}

/// Docker provider: wraps the agent invocation in `docker run` so it executes
/// inside a container whose only bind mount is the project directory.
pub struct DockerProvider {
    name: String,
    image: String,
    network: String,
}

impl DockerProvider {
    /// Build a docker provider named `name` from its `[sandboxes.<name>]` entry.
    ///
    /// The image is required. Network defaults to `none` (fully isolated) unless
    /// the sandbox declares egress hosts, in which case `bridge` is used.
    pub fn from_config(name: &str, config: &SandboxConfig) -> Result<Self> {
        let image = match config.image.as_deref() {
            Some(image) if !image.is_empty() => image.to_owned(),
            _ => bail!("sandbox '{name}' is missing an `image` (required for the docker provider)"),
        };
        let network = if config.egress.is_empty() {
            "none".to_owned()
        } else {
            "bridge".to_owned()
        };
        Ok(Self {
            name: name.to_owned(),
            image,
            network,
        })
    }

    #[cfg(test)]
    fn new_for_test(name: &str, image: &str, network: &str) -> Self {
        Self {
            name: name.to_owned(),
            image: image.to_owned(),
            network: network.to_owned(),
        }
    }
}

#[async_trait]
impl SandboxProvider for DockerProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn prepare(&self, ctx: &SandboxContext<'_>) -> Result<Box<dyn SandboxSession>> {
        Ok(Box::new(DockerSession {
            image: self.image.clone(),
            network: self.network.clone(),
            project_root: ctx.project_root.to_path_buf(),
        }))
    }
}

pub struct DockerSession {
    image: String,
    network: String,
    project_root: PathBuf,
}

#[async_trait]
impl SandboxSession for DockerSession {
    fn wrap(&self, spec: CommandSpec) -> Result<CommandSpec> {
        // Mount the project at the SAME absolute path inside the container so
        // that `{project}`-style path expansions stay valid, and run there.
        let proj = self.project_root.display().to_string();
        let cwd = spec
            .cwd
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| proj.clone());

        let mut args = vec![
            "run".to_owned(),
            "--rm".to_owned(),
            // `--init` reaps the container's PID 1 so a killed/dropped `docker`
            // client (timeout, kill_on_drop) tears the container down cleanly.
            "--init".to_owned(),
            // `-i` keeps stdin open so the prompt-then-EOF still reaches the agent.
            "-i".to_owned(),
            "--network".to_owned(),
            self.network.clone(),
            "-v".to_owned(),
            format!("{proj}:{proj}"),
            "-w".to_owned(),
            cwd,
        ];
        // Move the resolved env into `-e K=V`; the container starts with a clean
        // base env, so host secrets are never inherited implicitly. BTreeMap
        // iteration is sorted, keeping the produced argv deterministic.
        for (key, value) in &spec.env {
            args.push("-e".to_owned());
            args.push(format!("{key}={value}"));
        }
        args.push(self.image.clone());
        args.push(spec.program);
        args.extend(spec.args);

        Ok(CommandSpec {
            program: "docker".to_owned(),
            args,
            // The docker CLI itself inherits the host env (needs PATH, DOCKER_HOST…);
            // the container env is fully specified by the `-e` flags above.
            env: BTreeMap::new(),
            cwd: None,
        })
    }

    fn session_store_root(&self) -> Option<PathBuf> {
        // The agent's session store lives inside the container in M1, so it is
        // not reachable from the host: resume-capture is degraded until M3.
        None
    }

    async fn teardown(self: Box<Self>) -> Result<()> {
        // `--rm` removes the container on exit; nothing extra to tear down.
        Ok(())
    }
}

/// Resolve the effective provider for a sandbox name against the config.
///
/// `local` is always available; any other name must have a `[sandboxes.<name>]`
/// entry that supplies a docker `image`.
pub fn provider_for(
    name: &str,
    sandboxes: &BTreeMap<String, SandboxConfig>,
) -> Result<std::sync::Arc<dyn SandboxProvider>> {
    if name == "local" {
        return Ok(std::sync::Arc::new(LocalProvider));
    }
    match sandboxes.get(name) {
        Some(config) => Ok(std::sync::Arc::new(DockerProvider::from_config(name, config)?)),
        None => bail!("sandbox '{name}' is not defined under [sandboxes]"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ctx<'a>(project_root: &'a Path) -> SandboxContext<'a> {
        SandboxContext {
            project_root,
            route_glob: "**",
            agent_kind: AgentKind::Acp,
            session_id: "session-1",
        }
    }

    #[tokio::test]
    async fn local_wrap_is_identity() {
        let provider = LocalProvider;
        let root = Path::new("/proj");
        let session = provider.prepare(&ctx(root)).await.unwrap();
        let mut env = BTreeMap::new();
        env.insert("A".to_owned(), "1".to_owned());
        let spec = CommandSpec {
            program: "claude".to_owned(),
            args: vec!["--foo".to_owned()],
            env: env.clone(),
            cwd: Some(PathBuf::from("/proj")),
        };
        let wrapped = session.wrap(spec.clone()).unwrap();
        assert_eq!(wrapped, spec);
    }

    #[tokio::test]
    async fn docker_wrap_produces_exact_argv() {
        let provider = DockerProvider::new_for_test("docker", "varda:latest", "none");
        let root = Path::new("/home/me/project");
        let session = provider.prepare(&ctx(root)).await.unwrap();

        let mut env = BTreeMap::new();
        env.insert("FOO".to_owned(), "bar".to_owned());
        env.insert("ALPHA".to_owned(), "beta".to_owned());
        let spec = CommandSpec {
            program: "claude".to_owned(),
            args: vec!["--print".to_owned(), "-".to_owned()],
            env,
            cwd: Some(PathBuf::from("/home/me/project")),
        };

        let wrapped = session.wrap(spec).unwrap();
        assert_eq!(wrapped.program, "docker");
        assert_eq!(
            wrapped.args,
            vec![
                "run",
                "--rm",
                "--init",
                "-i",
                "--network",
                "none",
                "-v",
                "/home/me/project:/home/me/project",
                "-w",
                "/home/me/project",
                // sorted env: ALPHA before FOO
                "-e",
                "ALPHA=beta",
                "-e",
                "FOO=bar",
                "varda:latest",
                "claude",
                "--print",
                "-",
            ]
        );
        assert!(wrapped.env.is_empty());
        assert!(wrapped.cwd.is_none());
    }

    #[tokio::test]
    async fn docker_wrap_defaults_cwd_to_project_root() {
        let provider = DockerProvider::new_for_test("docker", "img", "none");
        let root = Path::new("/srv/app");
        let session = provider.prepare(&ctx(root)).await.unwrap();
        let spec = CommandSpec {
            program: "sh".to_owned(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        };
        let wrapped = session.wrap(spec).unwrap();
        // `-w` should fall back to the project root when no cwd is set.
        let w = wrapped.args.iter().position(|a| a == "-w").unwrap();
        assert_eq!(wrapped.args[w + 1], "/srv/app");
    }

    #[test]
    fn docker_session_store_root_is_none() {
        let session = DockerSession {
            image: "img".to_owned(),
            network: "none".to_owned(),
            project_root: PathBuf::from("/proj"),
        };
        assert!(session.session_store_root().is_none());
    }

    #[test]
    fn provider_for_local_and_docker() {
        let mut sandboxes: BTreeMap<String, SandboxConfig> = BTreeMap::new();
        assert_eq!(provider_for("local", &sandboxes).unwrap().name(), "local");

        // Unknown sandbox errors.
        assert!(provider_for("docker", &sandboxes).is_err());

        sandboxes.insert(
            "docker".to_owned(),
            SandboxConfig {
                image: Some("varda:latest".to_owned()),
                mounts: vec![],
                egress: vec![],
            },
        );
        assert_eq!(provider_for("docker", &sandboxes).unwrap().name(), "docker");

        // Missing image errors.
        sandboxes.insert(
            "broken".to_owned(),
            SandboxConfig {
                image: None,
                mounts: vec![],
                egress: vec![],
            },
        );
        assert!(provider_for("broken", &sandboxes).is_err());
    }
}
