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

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::Deserialize;

use crate::config::{AgentKind, SandboxConfig};

/// Where a mount declaration came from. Mounts compose across origins (effective
/// set = their union). In M6a both origins are the trusted central
/// `config.toml`, so no clamping is applied; this enum is the seam M6b extends
/// with a `Varda` (untrusted `.varda`) origin plus a hardening floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MountOrigin {
    /// Image-intrinsic `[sandboxes.X].mounts` — same for every project using
    /// the image.
    Sandbox,
    /// Project-context `Route.mounts` — tied to the code↔context mapping.
    Route,
    // M6b: Varda — untrusted `.varda` origin; will be clamped by a floor.
}

/// A parsed mount request: a host `source` bind-mounted at `target` inside the
/// container, read-only unless `writable`.
///
/// `source`/`target` are stored as written (they may still contain `~` or
/// `{project}` and may be relative to the project root); expansion to absolute
/// host/container paths happens at wrap time via [`expand_mount_path`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountSpec {
    pub source: PathBuf,
    pub target: PathBuf,
    pub writable: bool,
}

/// Interpret a mode segment (`ro`/`rw`/`w`) as a writability flag.
fn mount_mode_writable(segment: &str) -> Option<bool> {
    match segment {
        "ro" => Some(false),
        "rw" | "w" => Some(true),
        _ => None,
    }
}

/// Parse the `source[:target][:mode]` string grammar into a [`MountSpec`].
///
/// Forms (aligned with docker `src:dst:mode`):
/// - `SOURCE` — target defaults to SOURCE, read-only.
/// - `SOURCE:ro|:rw|:w` — target defaults to SOURCE, mode as given.
/// - `SOURCE:TARGET` — TARGET must be absolute (`/…`); read-only.
/// - `SOURCE:TARGET:ro|:rw|:w` — explicit target and mode.
///
/// Disambiguation of the segment after SOURCE: a value equal to `ro`/`rw`/`w`
/// is the MODE; a value starting with `/` is the TARGET. Anything else is an
/// error.
pub fn parse_mount(raw: &str) -> Result<MountSpec> {
    let parts: Vec<&str> = raw.split(':').collect();
    match parts.as_slice() {
        [source] if !source.is_empty() => Ok(MountSpec {
            source: PathBuf::from(source),
            target: PathBuf::from(source),
            writable: false,
        }),
        [source, second] if !source.is_empty() => {
            if let Some(writable) = mount_mode_writable(second) {
                Ok(MountSpec {
                    source: PathBuf::from(source),
                    target: PathBuf::from(source),
                    writable,
                })
            } else if second.starts_with('/') {
                Ok(MountSpec {
                    source: PathBuf::from(source),
                    target: PathBuf::from(second),
                    writable: false,
                })
            } else {
                bail!(
                    "invalid mount '{raw}': expected SOURCE, SOURCE:MODE, SOURCE:/ABS_TARGET, or SOURCE:/ABS_TARGET:MODE (MODE = ro|rw|w)"
                )
            }
        }
        [source, target, mode] if !source.is_empty() => {
            let writable = mount_mode_writable(mode).with_context(|| {
                format!("invalid mount '{raw}': trailing segment '{mode}' is not a mode (ro|rw|w)")
            })?;
            if !target.starts_with('/') {
                bail!("invalid mount '{raw}': TARGET '{target}' must be an absolute path");
            }
            Ok(MountSpec {
                source: PathBuf::from(source),
                target: PathBuf::from(target),
                writable,
            })
        }
        _ => bail!("invalid mount '{raw}': malformed (empty or too many ':'-separated segments)"),
    }
}

impl<'de> Deserialize<'de> for MountSpec {
    /// Serde helper accepting BOTH the string shorthand (parsed by
    /// [`parse_mount`]) and the canonical table form
    /// `{ source, target?, mode? }`.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Str(String),
            Table {
                source: String,
                #[serde(default)]
                target: Option<String>,
                #[serde(default)]
                mode: Option<String>,
            },
        }
        match Raw::deserialize(deserializer)? {
            Raw::Str(s) => parse_mount(&s).map_err(serde::de::Error::custom),
            Raw::Table {
                source,
                target,
                mode,
            } => {
                let writable = match mode.as_deref() {
                    None | Some("ro") => false,
                    Some("rw") | Some("w") => true,
                    Some(other) => {
                        return Err(serde::de::Error::custom(format!(
                            "invalid mount mode '{other}' (expected ro|rw|w)"
                        )));
                    }
                };
                let target = target
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from(&source));
                Ok(MountSpec {
                    source: PathBuf::from(source),
                    target,
                    writable,
                })
            }
        }
    }
}

/// Expand `~` (HOME), `{project}` (the matched project root), and
/// project-root-relative paths into an absolute host/container path.
fn expand_mount_path(raw: &Path, project_root: &Path) -> PathBuf {
    let text = raw.to_string_lossy();
    let text = text.replace("{project}", &project_root.to_string_lossy());
    let expanded = if text == "~" {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(text.clone()))
    } else if let Some(rest) = text.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(|home| Path::new(&home).join(rest))
            .unwrap_or_else(|| PathBuf::from(text.clone()))
    } else {
        PathBuf::from(text)
    };
    if expanded.is_absolute() {
        expanded
    } else {
        project_root.join(expanded)
    }
}

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
    ///
    /// For live stores this path is written during the run; for extracted stores
    /// (docker volume + `docker cp`) it is only populated by
    /// [`extract_session_store`](Self::extract_session_store) after the run.
    fn session_store_root(&self) -> Option<PathBuf>;
    /// `true` when the session store is written directly to a host-visible path
    /// during the run (so resume-discovery can poll live). `false` when the store
    /// is materialized only after the run (docker volume + `docker cp`); callers
    /// must call [`extract_session_store`](Self::extract_session_store) and then
    /// discover once, rather than polling during the run.
    fn store_is_live(&self) -> bool {
        true
    }
    /// Fail loudly when a declared bind-mount SOURCE is not reachable on the host
    /// (a would-be empty in-VM stub on a VM-backed daemon), naming the path.
    /// No-op for providers that do not bind host paths.
    fn validate_mounts(&self) -> Result<()> {
        Ok(())
    }
    /// Materialize the agent's session store on the host after the run and before
    /// teardown (e.g. `docker cp` from a per-session volume into
    /// [`session_store_root`](Self::session_store_root)). No-op for live stores.
    async fn extract_session_store(&self) -> Result<()> {
        Ok(())
    }
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
/// inside a container whose only bind mount is the project directory and whose
/// only reachable network hosts are an explicit egress allow-list.
pub struct DockerProvider {
    name: String,
    /// A pre-existing image tag/reference. `None` when `build` is set.
    image: Option<String>,
    /// Path to a Dockerfile to build at `prepare()`. Takes precedence over
    /// `image` when both are set; the resulting content-addressed tag is used.
    build: Option<String>,
    /// Extra host paths (beyond the project root) the sandbox may see, tagged by
    /// origin and following the `source[:target][:mode]` grammar. The effective
    /// set is the union of the image-intrinsic (`Sandbox`) and project-context
    /// (`Route`) mounts, de-duplicated by target at wrap time.
    mounts: Vec<(MountOrigin, String)>,
    /// Egress allow-list of hostnames. Empty ⇒ the container is fully offline
    /// (`--network none`); non-empty ⇒ default-deny with only these hosts
    /// resolvable inside the container.
    egress: Vec<String>,
}

impl DockerProvider {
    /// Build a docker provider named `name` from its `[sandboxes.<name>]` entry.
    ///
    /// Either `image` or `build` must be supplied. The effective mount set is
    /// the union of the image-intrinsic `config.mounts` (origin `Sandbox`) and
    /// the project-context `route_mounts` (origin `Route`); `egress` is threaded
    /// through from the config. All are applied by [`DockerSession`] at wrap
    /// time.
    pub fn from_config(
        name: &str,
        config: &SandboxConfig,
        route_mounts: &[String],
    ) -> Result<Self> {
        let image = config.image.clone().filter(|image| !image.is_empty());
        let build = config.build.clone().filter(|build| !build.is_empty());
        if image.is_none() && build.is_none() {
            bail!(
                "sandbox '{name}' needs an `image` or a `build` path (required for the docker provider)"
            );
        }
        let mounts = config
            .mounts
            .iter()
            .map(|m| (MountOrigin::Sandbox, m.clone()))
            .chain(route_mounts.iter().map(|m| (MountOrigin::Route, m.clone())))
            .collect();
        Ok(Self {
            name: name.to_owned(),
            image,
            build,
            mounts,
            egress: config.egress.clone(),
        })
    }

    /// Resolve the concrete image tag to run: build the Dockerfile when `build`
    /// is set (content-addressed, cached), otherwise use the configured image.
    async fn resolve_image(&self) -> Result<String> {
        if let Some(build) = &self.build {
            return build_image(&self.name, build).await;
        }
        self.image
            .clone()
            .with_context(|| format!("sandbox '{}' has neither image nor build", self.name))
    }

    #[cfg(test)]
    fn new_for_test(name: &str, image: &str, mounts: &[&str], egress: &[&str]) -> Self {
        Self {
            name: name.to_owned(),
            image: Some(image.to_owned()),
            build: None,
            mounts: mounts
                .iter()
                .map(|m| (MountOrigin::Sandbox, m.to_string()))
                .collect(),
            egress: egress.iter().map(|e| e.to_string()).collect(),
        }
    }
}

/// Build the Dockerfile at `dockerfile` and return a content-addressed image tag.
///
/// The tag encodes a hash of the Dockerfile's contents, so an unchanged
/// Dockerfile reuses the cached image (we skip the build when the tag already
/// exists locally). The build context is the Dockerfile's parent directory.
async fn build_image(name: &str, dockerfile: &str) -> Result<String> {
    use std::hash::{Hash as _, Hasher as _};

    let contents = std::fs::read(dockerfile)
        .with_context(|| format!("failed to read Dockerfile '{dockerfile}' for sandbox '{name}'"))?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    contents.hash(&mut hasher);
    let tag = format!("varda-sandbox:{:016x}", hasher.finish());

    let context_dir = Path::new(dockerfile)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));

    // Skip the build when an image with this content-addressed tag already exists.
    let exists = tokio::process::Command::new("docker")
        .args(["image", "inspect", &tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false);
    if exists {
        return Ok(tag);
    }

    let output = tokio::process::Command::new("docker")
        .arg("build")
        .arg("-q")
        .arg("-t")
        .arg(&tag)
        .arg("-f")
        .arg(dockerfile)
        .arg(&context_dir)
        .output()
        .await
        .with_context(|| format!("failed to spawn `docker build` for sandbox '{name}'"))?;
    if !output.status.success() {
        bail!(
            "`docker build` for sandbox '{name}' failed with status {}; stderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(tag)
}

/// Placeholder provider for isolation primitives whose runtime is not yet
/// wired up (microsandbox, clawk). Config parses and routing resolves, but
/// `prepare()` fails with a clear "runtime not installed" error so the missing
/// capability surfaces at launch rather than being silently ignored.
pub struct StubProvider {
    name: String,
    primitive: String,
}

#[async_trait]
impl SandboxProvider for StubProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn prepare(&self, _ctx: &SandboxContext<'_>) -> Result<Box<dyn SandboxSession>> {
        bail!(
            "sandbox primitive '{}' not yet available: runtime not installed",
            self.primitive
        )
    }
}

#[async_trait]
impl SandboxProvider for DockerProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn prepare(&self, ctx: &SandboxContext<'_>) -> Result<Box<dyn SandboxSession>> {
        // Resolve each allow-listed hostname to a concrete IP on the host so the
        // container can be pinned to exactly those addresses via `--add-host`
        // while ambient DNS is disabled (default-deny by name resolution).
        let mut egress_pins = Vec::with_capacity(self.egress.len());
        for host in &self.egress {
            let ip = resolve_host(host).await.with_context(|| {
                format!(
                    "failed to resolve egress-allow-listed host '{host}' for sandbox '{}'",
                    self.name
                )
            })?;
            egress_pins.push((host.clone(), ip));
        }
        // Give the container a dedicated HOME backed by a PER-SESSION DOCKER
        // NAMED VOLUME (not a host bind mount). The volume lives in the daemon/VM
        // storage, so it works on a VM-backed daemon whose shared tree excludes
        // `~/.varda` (e.g. a Colima profile sharing only `~/dev`) — a host bind of
        // `~/.varda/sessions/{id}` there silently mounts an empty in-VM stub and
        // the agent's session store never reaches the host. After the run we
        // `docker cp` the store out of the container into `session_store` (a
        // host-side dir we create, never mounted), which resume-capture reads.
        let session_store = varda_sessions_root().join(ctx.session_id);
        std::fs::create_dir_all(&session_store).with_context(|| {
            format!(
                "failed to create sandbox session store {}",
                session_store.display()
            )
        })?;
        let handle = sanitize_docker_name(ctx.session_id);
        // Resolve the concrete image only now: a `build` sandbox builds its
        // Dockerfile here (once, content-addressed) rather than at config load.
        let image = self.resolve_image().await?;
        Ok(Box::new(DockerSession {
            image,
            project_root: ctx.project_root.to_path_buf(),
            mounts: self.mounts.clone(),
            egress_pins,
            session_store,
            volume: format!("varda-sbx-{handle}"),
            container: format!("varda-sbx-{handle}"),
            home: "/home/agent".to_owned(),
        }))
    }
}

/// Host directory under which per-session sandbox HOME dirs are created.
///
/// Mirrors `acp::default_varda_home`: honours `VARDA_HOME`, else `$HOME/.varda`,
/// then appends `sessions`.
fn varda_sessions_root() -> PathBuf {
    let base = std::env::var_os("VARDA_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".varda")))
        .unwrap_or_else(|| PathBuf::from(".varda"));
    base.join("sessions")
}

/// Sanitize a session id into a docker-safe name/volume component.
///
/// Docker names must match `[a-zA-Z0-9][a-zA-Z0-9_.-]*`; every other character
/// is folded to `-`. The `varda-sbx-` prefix guarantees a valid leading char, so
/// the result is always usable even for an empty or exotic session id.
fn sanitize_docker_name(session_id: &str) -> String {
    session_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Resolve `host` to a single IP address using the host's blocking resolver.
///
/// Runs on the blocking pool so we don't need tokio's optional `net` feature.
async fn resolve_host(host: &str) -> Result<String> {
    use std::net::ToSocketAddrs as _;
    let host = host.to_owned();
    tokio::task::spawn_blocking(move || {
        let ip = (host.as_str(), 0u16)
            .to_socket_addrs()?
            .next()
            .with_context(|| format!("host '{host}' resolved to no addresses"))?
            .ip()
            .to_string();
        anyhow::Ok(ip)
    })
    .await
    .context("egress DNS resolution task panicked")?
}

pub struct DockerSession {
    image: String,
    project_root: PathBuf,
    /// Extra host mounts (origin-tagged, `source[:target][:mode]` grammar),
    /// merged and de-duplicated by target at wrap time.
    mounts: Vec<(MountOrigin, String)>,
    /// Resolved `(hostname, ip)` egress allow-list. Empty ⇒ `--network none`.
    egress_pins: Vec<(String, String)>,
    /// Host dir into which the agent's session store is `docker cp`-ed AFTER the
    /// run (never bind-mounted). Resume-capture reads this back from the host.
    session_store: PathBuf,
    /// Per-session docker named volume backing the container `HOME`. Lives in
    /// daemon/VM storage, independent of any host-share configuration.
    volume: String,
    /// Per-session container name (the run drops `--rm` so the container
    /// survives its process exit long enough to `docker cp` the store out).
    container: String,
    /// In-container mount point for `volume`, used as the agent's `HOME`.
    home: String,
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
            // A stable per-session name so we can `docker cp` the session store
            // out and `docker rm` the container in teardown. NOTE: we deliberately
            // do NOT pass `--rm` — the container must outlive its process exit so
            // the store can be extracted before removal.
            "--name".to_owned(),
            self.container.clone(),
            // `--init` reaps the container's PID 1 so a killed/dropped `docker`
            // client (timeout, kill_on_drop) still stops the container cleanly.
            "--init".to_owned(),
            // `-i` keeps stdin open so the prompt-then-EOF still reaches the agent.
            "-i".to_owned(),
            "--network".to_owned(),
        ];
        if self.egress_pins.is_empty() {
            // No allow-list ⇒ fully offline: nothing outbound is reachable.
            args.push("none".to_owned());
        } else {
            // Default-deny by name: attach to the bridge for connectivity, break
            // ambient DNS (`--dns 0.0.0.0` ⇒ no working resolver), then re-add
            // exactly the allow-listed hosts pinned to their resolved IPs. A
            // non-allow-listed hostname cannot resolve and is therefore
            // unreachable, while allow-listed hosts stay reachable. NOTE: this is
            // a name-resolution allow-list; IP-level firewalling of raw egress is
            // a later milestone.
            args.push("bridge".to_owned());
            args.push("--dns".to_owned());
            args.push("0.0.0.0".to_owned());
            for (host, ip) in &self.egress_pins {
                args.push("--add-host".to_owned());
                args.push(format!("{host}:{ip}"));
            }
        }
        // Project root is always mounted at the same absolute path; nothing else
        // is unless explicitly allow-listed via `mounts` (read-only).
        args.push("-v".to_owned());
        args.push(format!("{proj}:{proj}"));
        // The container's HOME is a per-session DOCKER NAMED VOLUME (not a host
        // bind). It lives in daemon/VM storage, so it works on a VM-backed daemon
        // whose share excludes `~/.varda`; the store is `docker cp`-ed to the
        // host after the run. The host's real HOME is never exposed.
        args.push("-v".to_owned());
        args.push(format!("{}:{}", self.volume, self.home));
        // Effective extra mounts = union(sandbox, route), de-duplicated by the
        // (expanded, absolute) target so a later origin does not double-mount a
        // target an earlier one already claimed. First declaration wins.
        let mut seen_targets: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for (_origin, raw) in &self.mounts {
            let spec = parse_mount(raw)
                .with_context(|| format!("invalid mount '{raw}' for sandbox '{}'", self.image))?;
            let source = expand_mount_path(&spec.source, &self.project_root);
            let target = expand_mount_path(&spec.target, &self.project_root);
            if !seen_targets.insert(target.clone()) {
                continue;
            }
            let source = source.display().to_string();
            let target = target.display().to_string();
            args.push("-v".to_owned());
            if spec.writable {
                args.push(format!("{source}:{target}"));
            } else {
                args.push(format!("{source}:{target}:ro"));
            }
        }
        args.push("-w".to_owned());
        args.push(cwd);
        // Move the resolved env into `-e K=V`; the container starts with a clean
        // base env, so host secrets are never inherited implicitly. Force HOME to
        // the mounted per-session store so the agent writes its session there.
        // BTreeMap iteration is sorted, keeping the produced argv deterministic.
        let mut env = spec.env;
        env.insert("HOME".to_owned(), self.home.clone());
        for (key, value) in &env {
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
        // The host dir that `extract_session_store` populates via `docker cp`;
        // resume-capture reads it back after the run.
        Some(self.session_store.clone())
    }

    fn store_is_live(&self) -> bool {
        // The store lives in a docker volume during the run; it only reaches the
        // host after `extract_session_store`, so discovery must run post-exit.
        false
    }

    fn validate_mounts(&self) -> Result<()> {
        // Fail loudly when a bind SOURCE is absent on the host: on a VM-backed
        // daemon docker would otherwise create an empty in-VM stub and the mount
        // would silently look successful. (The session store is a volume, not a
        // bind, so it is exempt.) A non-existent source is the host-observable
        // proxy for "not reachable inside the VM".
        if !self.project_root.exists() {
            bail!(
                "sandbox project mount source '{}' does not exist on the host; \
                 docker would mount an empty stub (check the path / VM share)",
                self.project_root.display()
            );
        }
        for (origin, raw) in &self.mounts {
            let spec = parse_mount(raw)
                .with_context(|| format!("invalid mount '{raw}' for sandbox '{}'", self.image))?;
            let source = expand_mount_path(&spec.source, &self.project_root);
            if !source.exists() {
                bail!(
                    "sandbox {origin:?} mount source '{}' does not exist on the host; \
                     docker would mount an empty stub (check the path / VM share)",
                    source.display()
                );
            }
        }
        Ok(())
    }

    async fn extract_session_store(&self) -> Result<()> {
        // Copy the container HOME *contents* into the host session-store dir. The
        // trailing `/.` copies what is inside `home` into the (existing) host dir
        // rather than nesting it. Runs after the agent exits and before teardown.
        let src = format!("{}:{}/.", self.container, self.home);
        let dst = self.session_store.display().to_string();
        let output = tokio::process::Command::new("docker")
            .args(["cp", &src, &dst])
            .output()
            .await
            .with_context(|| {
                format!(
                    "failed to spawn `docker cp` from sandbox container '{}'",
                    self.container
                )
            })?;
        if !output.status.success() {
            bail!(
                "`docker cp {src} {dst}` failed with status {}; stderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    async fn teardown(self: Box<Self>) -> Result<()> {
        // Without `--rm` the container and its per-session volume persist; remove
        // both here. Best-effort: a cleanup failure must not fail the run.
        let _ = tokio::process::Command::new("docker")
            .args(["rm", "-f", &self.container])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        let _ = tokio::process::Command::new("docker")
            .args(["volume", "rm", "-f", &self.volume])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        Ok(())
    }
}

/// microVM provider backed by the `msb` (microsandbox) CLI.
///
/// Mirrors [`DockerProvider`] but shells to `msb` instead of `docker`, so the
/// spawn / stdin-prompt / stdout-capture / kill-on-drop path in `acp.rs` is
/// unchanged. The isolation is stronger (own-kernel microVM rather than a
/// shared-kernel container); the session-store handling reuses the docker
/// "materialize after the run via `cp`" model, since a microVM's guest HOME is
/// not a host bind mount.
///
/// CLI GRAMMAR (confirmed against `msb 0.6.8`, 2026-08-05): `msb run [OPTS]
/// <IMAGE> -- <COMMAND>` (image positional before `--`); `--name`, `--workdir`,
/// `--env K=V`; host binds via `--mount-dir SOURCE:DEST[:ro|:rw]`; egress via
/// `--net-default[-egress] deny` + `--net-rule allow@<target>`; extraction via
/// `msb cp <sandbox>:/abs/path <host>` (the sandbox persists in `stopped` state
/// after a foreground run, so cp works before `msb rm`). Verified live: a
/// `--mount-dir …:ro` blocks writes and the host `~/.aws` is not visible in the
/// microVM. Spellings are centralized in [`MicrosandboxSession::wrap`] /
/// [`MicrosandboxSession::extract_session_store`].
pub struct MicrosandboxProvider {
    name: String,
    /// Pre-existing OCI image reference `msb` can pull, or the tag produced by a
    /// docker build of `build`. `None` when `build` is set.
    image: Option<String>,
    /// Path to a Dockerfile built (via docker, content-addressed) into an image
    /// `msb` then runs. Lets a project bake its agent CLI into the microVM.
    build: Option<String>,
    /// Extra host mounts (origin-tagged, `source[:target][:mode]` grammar),
    /// unioned and de-duplicated by target at wrap time — same as docker.
    mounts: Vec<(MountOrigin, String)>,
    /// Egress allow-list of hostnames. Empty ⇒ fully offline; non-empty ⇒
    /// default-deny with only these hosts permitted outbound.
    egress: Vec<String>,
}

impl MicrosandboxProvider {
    /// Build a microsandbox provider from its `[sandboxes.<name>]` entry. Same
    /// shape as [`DockerProvider::from_config`]: `image` or `build` is required,
    /// the mount set is the union of sandbox- and route-origin mounts.
    pub fn from_config(
        name: &str,
        config: &SandboxConfig,
        route_mounts: &[String],
    ) -> Result<Self> {
        let image = config.image.clone().filter(|image| !image.is_empty());
        let build = config.build.clone().filter(|build| !build.is_empty());
        if image.is_none() && build.is_none() {
            bail!(
                "sandbox '{name}' needs an `image` or a `build` path (required for the microsandbox provider)"
            );
        }
        let mounts = config
            .mounts
            .iter()
            .map(|m| (MountOrigin::Sandbox, m.clone()))
            .chain(route_mounts.iter().map(|m| (MountOrigin::Route, m.clone())))
            .collect();
        Ok(Self {
            name: name.to_owned(),
            image,
            build,
            mounts,
            egress: config.egress.clone(),
        })
    }

    /// Resolve the concrete image reference to run: a `build` sandbox builds its
    /// Dockerfile (content-addressed, cached, via docker) into a tag `msb` runs;
    /// otherwise the configured OCI reference is used verbatim.
    async fn resolve_image(&self) -> Result<String> {
        if let Some(build) = &self.build {
            return build_image(&self.name, build).await;
        }
        self.image
            .clone()
            .with_context(|| format!("sandbox '{}' has neither image nor build", self.name))
    }
}

#[async_trait]
impl SandboxProvider for MicrosandboxProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn prepare(&self, ctx: &SandboxContext<'_>) -> Result<Box<dyn SandboxSession>> {
        // Host-side dir the guest session store is `msb cp`-ed into after the run
        // (never a bind mount — the microVM guest HOME lives in VM storage).
        let session_store = varda_sessions_root().join(ctx.session_id);
        std::fs::create_dir_all(&session_store).with_context(|| {
            format!(
                "failed to create sandbox session store {}",
                session_store.display()
            )
        })?;
        let handle = sanitize_docker_name(ctx.session_id);
        let image = self.resolve_image().await?;
        Ok(Box::new(MicrosandboxSession {
            image,
            project_root: ctx.project_root.to_path_buf(),
            mounts: self.mounts.clone(),
            egress: self.egress.clone(),
            session_store,
            sandbox: format!("varda-sbx-{handle}"),
            home: "/home/agent".to_owned(),
        }))
    }
}

pub struct MicrosandboxSession {
    image: String,
    project_root: PathBuf,
    mounts: Vec<(MountOrigin, String)>,
    /// Egress allow-list of hostnames (unresolved — `msb` net rules take names /
    /// CIDRs directly). Empty ⇒ fully offline.
    egress: Vec<String>,
    /// Host dir the guest session store is `msb cp`-ed into AFTER the run.
    session_store: PathBuf,
    /// Per-session microVM sandbox name (for `exec`/`cp`/`stop`/`rm`).
    sandbox: String,
    /// In-guest HOME path; the agent writes its session store here.
    home: String,
}

#[async_trait]
impl SandboxSession for MicrosandboxSession {
    fn wrap(&self, spec: CommandSpec) -> Result<CommandSpec> {
        // Mount the project at the SAME absolute path inside the guest so
        // `{project}`-style expansions stay valid, and run there.
        let proj = self.project_root.display().to_string();
        let cwd = spec
            .cwd
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| proj.clone());

        // `msb run` creates a sandbox from an image and runs a command in it,
        // streaming stdio like `docker run -i` (stdin reaches the guest, stdout
        // is captured, exit code propagates). A stable `--name` lets teardown
        // `msb cp`/`stop`/`rm` the same sandbox.
        let mut args = vec![
            "run".to_owned(),
            "--name".to_owned(),
            self.sandbox.clone(),
        ];

        // Egress: empty ⇒ fully offline; non-empty ⇒ default-deny plus an allow
        // rule per host. `msb` enforces net policy in-guest (own-kernel), so
        // unlike docker we hand it hostnames/CIDRs rather than pre-resolved IPs.
        if self.egress.is_empty() {
            args.push("--net-default".to_owned());
            args.push("deny".to_owned());
        } else {
            args.push("--net-default-egress".to_owned());
            args.push("deny".to_owned());
            for host in &self.egress {
                args.push("--net-rule".to_owned());
                // msb net-rule token grammar: `<action>[:<direction>]@<target>`
                // (confirmed against msb 0.6.8; e.g. `allow@example.com`).
                args.push(format!("allow@{host}"));
            }
        }

        // Project root always mounted at its absolute path; nothing else unless
        // explicitly allow-listed. `msb --mount-dir SOURCE:DEST[:ro|:rw]` binds a
        // host directory into the guest (confirmed against msb 0.6.8).
        args.push("--mount-dir".to_owned());
        args.push(format!("{proj}:{proj}"));
        // Effective extra mounts = union(sandbox, route), de-duplicated by the
        // expanded absolute target; first declaration wins. Read-only unless the
        // spec says otherwise (msb appends `:ro`/`:rw` like docker).
        let mut seen_targets: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for (_origin, raw) in &self.mounts {
            let mspec = parse_mount(raw)
                .with_context(|| format!("invalid mount '{raw}' for sandbox '{}'", self.image))?;
            let source = expand_mount_path(&mspec.source, &self.project_root);
            let target = expand_mount_path(&mspec.target, &self.project_root);
            if !seen_targets.insert(target.clone()) {
                continue;
            }
            let source = source.display().to_string();
            let target = target.display().to_string();
            args.push("--mount-dir".to_owned());
            if mspec.writable {
                args.push(format!("{source}:{target}:rw"));
            } else {
                args.push(format!("{source}:{target}:ro"));
            }
        }

        args.push("--workdir".to_owned());
        args.push(cwd);
        // Clean base env in the guest; force HOME to the guest session store so
        // the agent writes its resume state there. Sorted for a deterministic argv.
        let mut env = spec.env;
        env.insert("HOME".to_owned(), self.home.clone());
        for (key, value) in &env {
            args.push("--env".to_owned());
            args.push(format!("{key}={value}"));
        }

        // Image is positional, then the command after `--` so its flags are not
        // parsed by `msb`.
        args.push(self.image.clone());
        args.push("--".to_owned());
        args.push(spec.program);
        args.extend(spec.args);

        Ok(CommandSpec {
            program: "msb".to_owned(),
            args,
            // The `msb` CLI inherits the host env (PATH etc.); the guest env is
            // fully specified by the `--env` flags above.
            env: BTreeMap::new(),
            cwd: None,
        })
    }

    fn session_store_root(&self) -> Option<PathBuf> {
        // `msb cp <sandbox>:<home> <session_store>` lands the guest HOME as a
        // SUBDIRECTORY named after HOME's basename — msb has no docker-style `/.`
        // contents-copy (confirmed: it rejects a `.` path component). So the
        // agent's `.claude/…` ends up at `<session_store>/<basename>/.claude/…`
        // and discovery must read that nested dir.
        let root = Path::new(&self.home)
            .file_name()
            .map(|name| self.session_store.join(name))
            .unwrap_or_else(|| self.session_store.clone());
        Some(root)
    }

    fn store_is_live(&self) -> bool {
        // Guest HOME lives in VM storage during the run; it only reaches the host
        // after `extract_session_store`, so discovery must run post-exit.
        false
    }

    fn validate_mounts(&self) -> Result<()> {
        // Fail loudly when a bind SOURCE is absent on the host — same rationale
        // as docker: a missing source would otherwise mount an empty guest stub.
        if !self.project_root.exists() {
            bail!(
                "sandbox project mount source '{}' does not exist on the host (check the path / VM share)",
                self.project_root.display()
            );
        }
        for (origin, raw) in &self.mounts {
            let mspec = parse_mount(raw)
                .with_context(|| format!("invalid mount '{raw}' for sandbox '{}'", self.image))?;
            let source = expand_mount_path(&mspec.source, &self.project_root);
            if !source.exists() {
                bail!(
                    "sandbox {origin:?} mount source '{}' does not exist on the host (check the path / VM share)",
                    source.display()
                );
            }
        }
        Ok(())
    }

    async fn extract_session_store(&self) -> Result<()> {
        // Copy the guest HOME dir into the host session-store dir. Runs after the
        // agent exits and before teardown, so resume-capture can read it back from
        // the host even though the guest HOME was never bind-mounted. NB: msb has
        // no docker `/.` contents-copy, so this nests the HOME under its basename;
        // `session_store_root()` accounts for that.
        let src = format!("{}:{}", self.sandbox, self.home);
        let dst = self.session_store.display().to_string();
        let output = tokio::process::Command::new("msb")
            .args(["cp", &src, &dst])
            .output()
            .await
            .with_context(|| {
                format!("failed to spawn `msb cp` from sandbox '{}'", self.sandbox)
            })?;
        if !output.status.success() {
            bail!(
                "`msb cp {src} {dst}` failed with status {}; stderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    async fn teardown(self: Box<Self>) -> Result<()> {
        // Best-effort: stop then remove the per-session sandbox; a cleanup
        // failure must not fail the run.
        let _ = tokio::process::Command::new("msb")
            .args(["stop", &self.sandbox])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        let _ = tokio::process::Command::new("msb")
            .args(["rm", "-f", &self.sandbox])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
        Ok(())
    }
}

/// Resolve the effective provider for a sandbox name against the config.
///
/// Dispatch is driven by the resolved [`SandboxConfig::primitive`] (the "what
/// kind of boundary" knob), which is orthogonal to the image/rootfs ("what
/// tools are installed"):
/// - `"local"` → [`LocalProvider`] (no isolation)
/// - `"docker"` → [`DockerProvider`] (shared-kernel container)
/// - `"microsandbox"` → [`MicrosandboxProvider`] (own-kernel microVM via `msb`)
/// - `"clawk"` → [`StubProvider`] (own-kernel microVM; `prepare()` errors until
///   the runtime is wired up in a later milestone)
///
/// The bare name `"local"` with no `[sandboxes.local]` entry stays a shortcut
/// for the identity provider; any other name must have a `[sandboxes.<name>]`
/// entry (whose `primitive` defaults to `"docker"`).
pub fn provider_for(
    name: &str,
    sandboxes: &BTreeMap<String, SandboxConfig>,
    route_mounts: &[String],
) -> Result<std::sync::Arc<dyn SandboxProvider>> {
    match sandboxes.get(name) {
        Some(config) => match config.primitive.as_str() {
            "local" => Ok(std::sync::Arc::new(LocalProvider)),
            "docker" => Ok(std::sync::Arc::new(DockerProvider::from_config(
                name,
                config,
                route_mounts,
            )?)),
            "microsandbox" => Ok(std::sync::Arc::new(MicrosandboxProvider::from_config(
                name,
                config,
                route_mounts,
            )?)),
            "clawk" => Ok(std::sync::Arc::new(StubProvider {
                name: name.to_owned(),
                primitive: config.primitive.clone(),
            })),
            other => bail!(
                "sandbox '{name}' has unknown primitive '{other}' (expected local, docker, microsandbox, or clawk)"
            ),
        },
        None if name == "local" => Ok(std::sync::Arc::new(LocalProvider)),
        None => bail!("sandbox '{name}' is not defined under [sandboxes]"),
    }
}

#[cfg(test)]
impl DockerSession {
    /// Construct a session directly for wrap/argv/mount unit tests, bypassing
    /// `prepare()` (which would need a live docker daemon). The volume/container
    /// names and HOME are fixed so argv assertions stay deterministic.
    fn for_test(
        image: &str,
        project_root: &str,
        mounts: Vec<(MountOrigin, String)>,
        egress_pins: Vec<(String, String)>,
        session_store: &str,
    ) -> Self {
        Self {
            image: image.to_owned(),
            project_root: PathBuf::from(project_root),
            mounts,
            egress_pins,
            session_store: PathBuf::from(session_store),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ctx<'a>(project_root: &'a Path) -> SandboxContext<'a> {
        ctx_with_id(project_root, "session-1")
    }

    /// Like [`ctx`] but with an explicit session id. Docker integration tests
    /// that actually launch a container must each use a UNIQUE id: `prepare`
    /// derives the container/volume name (`varda-sbx-<id>`) from it, and the
    /// docker path drops `--rm` (the container must outlive its process so the
    /// session store can be `docker cp`-ed out), so a shared id collides.
    fn ctx_with_id<'a>(project_root: &'a Path, session_id: &'a str) -> SandboxContext<'a> {
        SandboxContext {
            project_root,
            route_glob: "**",
            agent_kind: AgentKind::Acp,
            session_id,
        }
    }

    /// Best-effort removal of a `varda-sbx-<id>` container + its volume, so a
    /// test is robust to a leftover from a prior panicked run (teardown skipped).
    async fn docker_cleanup(session_id: &str) {
        let name = format!("varda-sbx-{}", sanitize_docker_name(session_id));
        let quiet = |mut c: tokio::process::Command| async move {
            let _ = c
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;
        };
        let mut rm = tokio::process::Command::new("docker");
        rm.args(["rm", "-f", &name]);
        quiet(rm).await;
        let mut vol = tokio::process::Command::new("docker");
        vol.args(["volume", "rm", "-f", &name]);
        quiet(vol).await;
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
        let provider = DockerProvider::new_for_test("docker", "varda:latest", &[], &[]);
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
        // HOME is a per-session named volume mounted at a fixed in-container path;
        // its name derives from the (sanitized) session id "session-1".
        assert_eq!(wrapped.program, "docker");
        assert_eq!(
            wrapped.args,
            vec![
                "run",
                "--name",
                "varda-sbx-session-1",
                "--init",
                "-i",
                "--network",
                "none",
                "-v",
                "/home/me/project:/home/me/project",
                "-v",
                "varda-sbx-session-1:/home/agent",
                "-w",
                "/home/me/project",
                // sorted env: ALPHA before FOO before the injected HOME
                "-e",
                "ALPHA=beta",
                "-e",
                "FOO=bar",
                "-e",
                "HOME=/home/agent",
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
        let provider = DockerProvider::new_for_test("docker", "img", &[], &[]);
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

    /// M3: the container's HOME is a host-visible per-session dir, so the store
    /// root is reachable (Some) and resume-capture can read it back.
    #[test]
    fn docker_session_store_root_is_the_session_mount() {
        let session = DockerSession {
            image: "img".to_owned(),
            project_root: PathBuf::from("/proj"),
            mounts: vec![],
            egress_pins: vec![],
            session_store: PathBuf::from("/var/varda/sessions/s1"),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
        };
        assert_eq!(
            session.session_store_root(),
            Some(PathBuf::from("/var/varda/sessions/s1"))
        );
    }

    /// M9: the docker store is NOT live during the run — it lives in a volume and
    /// is only materialized on the host by `extract_session_store` afterwards.
    #[test]
    fn docker_store_is_not_live() {
        let session =
            DockerSession::for_test("img", "/proj", vec![], vec![], "/var/varda/sessions/s1");
        assert!(!session.store_is_live());
    }

    /// M9 fail-loud guard: a bind-mount SOURCE that does not exist on the host
    /// (a would-be empty in-VM stub on a VM-backed daemon) is rejected loudly,
    /// naming the offending path — both for the project root and for extra mounts.
    #[test]
    fn docker_validate_mounts_errors_on_unreachable_source() {
        // Bogus project root.
        let bad_project = DockerSession::for_test(
            "img",
            "/nonexistent/varda-m9-project",
            vec![],
            vec![],
            "/var/varda/sessions/s1",
        );
        let err = bad_project
            .validate_mounts()
            .expect_err("nonexistent project mount source must fail loudly");
        assert!(
            err.to_string().contains("/nonexistent/varda-m9-project")
                && err.to_string().contains("empty stub"),
            "unexpected error: {err:#}"
        );

        // Existing project, bogus extra mount source.
        let real_project = std::env::temp_dir();
        let bad_mount = DockerSession::for_test(
            "img",
            &real_project.display().to_string(),
            vec![(
                MountOrigin::Route,
                "/nonexistent/varda-m9-context:/ctx".to_owned(),
            )],
            vec![],
            "/var/varda/sessions/s1",
        );
        let err = bad_mount
            .validate_mounts()
            .expect_err("nonexistent extra mount source must fail loudly");
        assert!(
            err.to_string().contains("/nonexistent/varda-m9-context"),
            "unexpected error: {err:#}"
        );
    }

    /// M9: `validate_mounts` passes when every bind source exists on the host.
    #[test]
    fn docker_validate_mounts_ok_when_sources_exist() {
        let real = std::env::temp_dir();
        let real = real.display().to_string();
        let session = DockerSession::for_test(
            "img",
            &real,
            vec![(MountOrigin::Sandbox, format!("{real}:/ctx:ro"))],
            vec![],
            "/var/varda/sessions/s1",
        );
        assert!(session.validate_mounts().is_ok());
    }

    /// M2 mount allow-list: the project root is always mounted; extra `mounts`
    /// are added read-only and nothing else appears. With no extra mounts, only
    /// the single project bind mount is present.
    #[test]
    fn docker_wrap_mounts_only_project_root_by_default() {
        let session = DockerSession {
            image: "img".to_owned(),
            project_root: PathBuf::from("/srv/app"),
            mounts: vec![],
            egress_pins: vec![],
            session_store: PathBuf::from("/var/varda/sessions/s1"),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
        };
        let wrapped = session
            .wrap(CommandSpec {
                program: "sh".to_owned(),
                args: vec![],
                env: BTreeMap::new(),
                cwd: None,
            })
            .unwrap();
        let mounts: Vec<&String> = wrapped
            .args
            .iter()
            .enumerate()
            .filter(|(i, _)| *i > 0 && wrapped.args[i - 1] == "-v")
            .map(|(_, v)| v)
            .collect();
        // Project root plus the per-session HOME mount; nothing else.
        assert_eq!(
            mounts,
            vec![
                "/srv/app:/srv/app",
                "varda-sbx-s1:/home/agent"
            ]
        );
    }

    /// M2 mount allow-list: an explicit extra mount is bind-mounted read-only.
    #[test]
    fn docker_wrap_adds_extra_mounts_read_only() {
        let session = DockerSession {
            image: "img".to_owned(),
            project_root: PathBuf::from("/srv/app"),
            mounts: vec![(MountOrigin::Sandbox, "/opt/cache".to_owned())],
            egress_pins: vec![],
            session_store: PathBuf::from("/var/varda/sessions/s1"),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
        };
        let wrapped = session
            .wrap(CommandSpec {
                program: "sh".to_owned(),
                args: vec![],
                env: BTreeMap::new(),
                cwd: None,
            })
            .unwrap();
        let mounts: Vec<&String> = wrapped
            .args
            .iter()
            .enumerate()
            .filter(|(i, _)| *i > 0 && wrapped.args[i - 1] == "-v")
            .map(|(_, v)| v)
            .collect();
        // Project root, then the per-session HOME mount, then the read-only extra.
        assert_eq!(
            mounts,
            vec![
                "/srv/app:/srv/app",
                "varda-sbx-s1:/home/agent",
                "/opt/cache:/opt/cache:ro"
            ]
        );
    }

    /// M2 egress: no allow-list ⇒ fully offline (`--network none`, no DNS/host
    /// overrides).
    #[test]
    fn docker_wrap_no_egress_is_network_none() {
        let session = DockerSession {
            image: "img".to_owned(),
            project_root: PathBuf::from("/srv/app"),
            mounts: vec![],
            egress_pins: vec![],
            session_store: PathBuf::from("/var/varda/sessions/s1"),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
        };
        let wrapped = session
            .wrap(CommandSpec {
                program: "sh".to_owned(),
                args: vec![],
                env: BTreeMap::new(),
                cwd: None,
            })
            .unwrap();
        let n = wrapped.args.iter().position(|a| a == "--network").unwrap();
        assert_eq!(wrapped.args[n + 1], "none");
        assert!(!wrapped.args.iter().any(|a| a == "--add-host"));
        assert!(!wrapped.args.iter().any(|a| a == "--dns"));
    }

    /// M2 egress allow-list: declared hosts switch the container to `bridge`,
    /// disable ambient DNS, and pin exactly the allow-listed hosts to their IPs.
    #[test]
    fn docker_wrap_egress_pins_allow_listed_hosts() {
        let session = DockerSession {
            image: "img".to_owned(),
            project_root: PathBuf::from("/srv/app"),
            mounts: vec![],
            egress_pins: vec![
                ("api.example.com".to_owned(), "93.184.216.34".to_owned()),
                ("cdn.example.com".to_owned(), "203.0.113.7".to_owned()),
            ],
            session_store: PathBuf::from("/var/varda/sessions/s1"),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
        };
        let wrapped = session
            .wrap(CommandSpec {
                program: "sh".to_owned(),
                args: vec![],
                env: BTreeMap::new(),
                cwd: None,
            })
            .unwrap();
        let n = wrapped.args.iter().position(|a| a == "--network").unwrap();
        assert_eq!(wrapped.args[n + 1], "bridge");
        let d = wrapped.args.iter().position(|a| a == "--dns").unwrap();
        assert_eq!(wrapped.args[d + 1], "0.0.0.0");
        let add_hosts: Vec<&String> = wrapped
            .args
            .iter()
            .enumerate()
            .filter(|(i, _)| *i > 0 && wrapped.args[i - 1] == "--add-host")
            .map(|(_, v)| v)
            .collect();
        assert_eq!(
            add_hosts,
            vec!["api.example.com:93.184.216.34", "cdn.example.com:203.0.113.7"]
        );
    }

    #[test]
    fn provider_for_local_and_docker() {
        let mut sandboxes: BTreeMap<String, SandboxConfig> = BTreeMap::new();
        assert_eq!(provider_for("local", &sandboxes, &[]).unwrap().name(), "local");

        // Unknown sandbox errors.
        assert!(provider_for("docker", &sandboxes, &[]).is_err());

        sandboxes.insert(
            "docker".to_owned(),
            SandboxConfig {
                image: Some("varda:latest".to_owned()),
                ..Default::default()
            },
        );
        assert_eq!(provider_for("docker", &sandboxes, &[]).unwrap().name(), "docker");

        // Missing image AND build errors under the docker primitive.
        sandboxes.insert(
            "broken".to_owned(),
            SandboxConfig {
                image: None,
                ..Default::default()
            },
        );
        assert!(provider_for("broken", &sandboxes, &[]).is_err());
    }

    /// M5: `primitive` selects the boundary kind independently of the image.
    /// An explicit `primitive = "local"` yields the identity provider even with
    /// an image set; microsandbox/clawk yield the stub.
    #[test]
    fn provider_for_dispatches_on_primitive() {
        let mut sandboxes: BTreeMap<String, SandboxConfig> = BTreeMap::new();
        sandboxes.insert(
            "isolated".to_owned(),
            SandboxConfig {
                image: Some("busybox".to_owned()),
                primitive: "local".to_owned(),
                ..Default::default()
            },
        );
        assert_eq!(
            provider_for("isolated", &sandboxes, &[]).unwrap().name(),
            "local"
        );

        for primitive in ["microsandbox", "clawk"] {
            sandboxes.insert(
                "vm".to_owned(),
                SandboxConfig {
                    image: Some("busybox".to_owned()),
                    primitive: primitive.to_owned(),
                    ..Default::default()
                },
            );
            // The stub resolves at provider_for time but errors at prepare().
            let provider = provider_for("vm", &sandboxes, &[]).unwrap();
            assert_eq!(provider.name(), "vm");
        }

        // An unknown primitive is rejected outright.
        sandboxes.insert(
            "weird".to_owned(),
            SandboxConfig {
                image: Some("busybox".to_owned()),
                primitive: "qemu".to_owned(),
                ..Default::default()
            },
        );
        assert!(provider_for("weird", &sandboxes, &[]).is_err());
    }

    /// M4: `msb run` argv shape — image positional after flags, command after
    /// `--`, project bound at its absolute path, offline by default, HOME forced
    /// to the guest session store. Pure unit test; needs no `msb`.
    #[test]
    fn microsandbox_wrap_produces_expected_argv() {
        let session = MicrosandboxSession {
            image: "busybox".to_owned(),
            project_root: PathBuf::from("/proj"),
            mounts: Vec::new(),
            egress: Vec::new(),
            session_store: PathBuf::from("/host/store"),
            sandbox: "varda-sbx-abc".to_owned(),
            home: "/home/agent".to_owned(),
        };
        let spec = CommandSpec {
            program: "claude".to_owned(),
            args: vec!["--acp".to_owned()],
            env: BTreeMap::new(),
            cwd: None,
        };
        let wrapped = session.wrap(spec).unwrap();
        assert_eq!(wrapped.program, "msb");
        assert_eq!(
            wrapped.args,
            vec![
                "run",
                "--name",
                "varda-sbx-abc",
                "--net-default",
                "deny",
                "--mount-dir",
                "/proj:/proj",
                "--workdir",
                "/proj",
                "--env",
                "HOME=/home/agent",
                "busybox",
                "--",
                "claude",
                "--acp",
            ]
        );
        assert!(wrapped.env.is_empty());
        assert!(!session.store_is_live());
        // `msb cp` nests the guest HOME under its basename on the host, so the
        // discovery root is the nested dir, not the raw session_store.
        assert_eq!(
            session.session_store_root(),
            Some(PathBuf::from("/host/store/agent"))
        );
    }

    /// M4 live exit criteria — verified against a real `msb` microVM: a
    /// read-only mount blocks writes, host `~/.aws` is NOT visible (own-kernel
    /// isolation), and the session store round-trips guest→host via `msb cp`.
    #[tokio::test]
    #[ignore = "requires the msb (microsandbox) runtime"]
    async fn microsandbox_isolates_and_round_trips_live() {
        use tokio::process::Command as TokioCommand;

        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("msb-it-proj");
        std::fs::create_dir_all(&root).unwrap();

        let provider = MicrosandboxProvider::from_config(
            "microsandbox",
            &SandboxConfig {
                image: Some("busybox".to_owned()),
                primitive: "microsandbox".to_owned(),
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        let session = provider
            .prepare(&ctx_with_id(&root, "msb-it-1"))
            .await
            .unwrap();
        let store = session.session_store_root().expect("store root");

        // The agent writes a session-store file under HOME and probes host creds.
        let spec = session
            .wrap(CommandSpec {
                program: "sh".to_owned(),
                args: vec![
                    "-c".to_owned(),
                    "mkdir -p \"$HOME/.claude\" && echo hi > \"$HOME/.claude/transcript.jsonl\"; \
                     ls /Users/nilleb/.aws >/dev/null 2>&1 && echo AWS_VISIBLE || echo AWS_HIDDEN"
                        .to_owned(),
                ],
                env: BTreeMap::new(),
                cwd: Some(root.clone()),
            })
            .unwrap();
        let out = TokioCommand::new(&spec.program)
            .args(&spec.args)
            .output()
            .await
            .expect("failed to run msb");
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();

        let extracted = if out.status.success() {
            session.extract_session_store().await
        } else {
            Ok(())
        };
        let got = std::fs::read_to_string(store.join(".claude/transcript.jsonl"));
        session.teardown().await.ok();

        assert!(out.status.success(), "msb run should succeed: {stdout}");
        assert!(
            stdout.contains("AWS_HIDDEN"),
            "host ~/.aws must NOT be visible in the microVM; got: {stdout}"
        );
        extracted.expect("msb cp extraction should succeed");
        assert_eq!(
            got.expect("session store must round-trip guest→host via msb cp")
                .trim(),
            "hi"
        );
    }

    /// M5: the clawk stub parses and resolves, but `prepare()` fails with a
    /// clear "runtime not installed" message (microsandbox is now a real
    /// provider; clawk stays stubbed).
    #[tokio::test]
    async fn stub_provider_prepare_errors_clearly() {
        let provider = StubProvider {
            name: "vm".to_owned(),
            primitive: "clawk".to_owned(),
        };
        let root = Path::new("/proj");
        let err = match provider.prepare(&ctx(root)).await {
            Ok(_) => panic!("stub prepare should not succeed"),
            Err(err) => err.to_string(),
        };
        assert!(
            err.contains("not yet available") && err.contains("clawk"),
            "unexpected stub error: {err}"
        );
    }

    /// M5: a `build` sandbox defers image resolution to `prepare()`; a missing
    /// Dockerfile surfaces there as a read error rather than at config time.
    #[tokio::test]
    async fn docker_build_missing_dockerfile_errors_at_prepare() {
        let provider = DockerProvider::from_config(
            "built",
            &SandboxConfig {
                build: Some("/nonexistent/Dockerfile.does-not-exist".to_owned()),
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        let root = Path::new("/proj");
        let err = match provider.prepare(&ctx(root)).await {
            Ok(_) => panic!("build prepare with a missing Dockerfile should not succeed"),
            Err(err) => err.to_string(),
        };
        assert!(
            err.contains("Dockerfile") || err.contains("failed to read"),
            "unexpected build error: {err}"
        );
    }

    // ---- M6a: mount grammar + three-origin (two in M6a) merge ----

    /// Collect the `-v` mount values (the arg after each `-v`) from a wrapped
    /// docker argv.
    fn mount_values(wrapped: &CommandSpec) -> Vec<String> {
        wrapped
            .args
            .iter()
            .enumerate()
            .filter(|(i, _)| *i > 0 && wrapped.args[i - 1] == "-v")
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// M6a grammar: source-only ⇒ same-path, read-only.
    #[test]
    fn parse_mount_source_only_is_same_path_ro() {
        let spec = parse_mount("/data").unwrap();
        assert_eq!(
            spec,
            MountSpec {
                source: PathBuf::from("/data"),
                target: PathBuf::from("/data"),
                writable: false,
            }
        );
    }

    /// M6a grammar: `SOURCE:ro|:w` sets the mode, target defaults to source.
    #[test]
    fn parse_mount_source_with_mode() {
        assert!(!parse_mount("/data:ro").unwrap().writable);
        assert!(parse_mount("/data:w").unwrap().writable);
        assert!(parse_mount("/data:rw").unwrap().writable);
        let w = parse_mount("~/ctx:w").unwrap();
        assert_eq!(w.source, PathBuf::from("~/ctx"));
        assert_eq!(w.target, PathBuf::from("~/ctx"));
        assert!(w.writable);
    }

    /// M6a grammar: `SOURCE:TARGET` (absolute target) is an explicit target, ro.
    #[test]
    fn parse_mount_explicit_target() {
        let spec = parse_mount("~/dev/brain/AsianDevBank:/context/adb").unwrap();
        assert_eq!(spec.source, PathBuf::from("~/dev/brain/AsianDevBank"));
        assert_eq!(spec.target, PathBuf::from("/context/adb"));
        assert!(!spec.writable);
    }

    /// M6a grammar: full `SOURCE:TARGET:mode` form.
    #[test]
    fn parse_mount_explicit_target_and_mode() {
        let spec = parse_mount("/src:/dst:w").unwrap();
        assert_eq!(spec.source, PathBuf::from("/src"));
        assert_eq!(spec.target, PathBuf::from("/dst"));
        assert!(spec.writable);
    }

    /// M6a grammar: malformed forms error (non-mode/non-abs middle segment,
    /// relative target in the 3-segment form, and empty source).
    #[test]
    fn parse_mount_malformed_errors() {
        assert!(parse_mount("/src:relative").is_err());
        assert!(parse_mount("/src:relative:ro").is_err());
        assert!(parse_mount("/src:/dst:bogus").is_err());
        assert!(parse_mount("").is_err());
    }

    /// M6a: the canonical TOML table form deserializes into a `MountSpec`
    /// (source-only ⇒ same-path ro; explicit target + mode honoured).
    #[test]
    fn mount_spec_table_form_deserializes() {
        #[derive(Deserialize)]
        struct Wrap {
            mount: MountSpec,
        }
        let only: Wrap = toml::from_str("mount = { source = \"/data\" }").unwrap();
        assert_eq!(
            only.mount,
            MountSpec {
                source: PathBuf::from("/data"),
                target: PathBuf::from("/data"),
                writable: false,
            }
        );
        let full: Wrap = toml::from_str(
            "mount = { source = \"~/dev/brain/AsianDevBank\", target = \"/context/adb\", mode = \"ro\" }",
        )
        .unwrap();
        assert_eq!(full.mount.target, PathBuf::from("/context/adb"));
        assert!(!full.mount.writable);
        // The string shorthand also flows through the same Deserialize helper.
        let shorthand: Wrap = toml::from_str("mount = \"/opt/cache:w\"").unwrap();
        assert!(shorthand.mount.writable);
    }

    /// M6a exit criterion: a `Route.mounts` entry in the string form reaches the
    /// docker argv as `-v SOURCE:SOURCE:ro` (target defaults to the expanded
    /// source). Uses an absolute source so the assertion is host-independent.
    #[test]
    fn route_mount_reaches_argv_same_path_ro() {
        let session = DockerSession {
            image: "img".to_owned(),
            project_root: PathBuf::from("/srv/app"),
            mounts: vec![(MountOrigin::Route, "/ctx/adb:ro".to_owned())],
            egress_pins: vec![],
            session_store: PathBuf::from("/var/varda/sessions/s1"),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
        };
        let wrapped = session
            .wrap(CommandSpec {
                program: "sh".to_owned(),
                args: vec![],
                env: BTreeMap::new(),
                cwd: None,
            })
            .unwrap();
        assert_eq!(
            mount_values(&wrapped),
            vec![
                "/srv/app:/srv/app".to_owned(),
                "varda-sbx-s1:/home/agent".to_owned(),
                "/ctx/adb:/ctx/adb:ro".to_owned(),
            ]
        );
    }

    /// M6a exit criterion: an explicit-target route mount maps to
    /// `-v SOURCE:/context/adb:ro`.
    #[test]
    fn route_mount_explicit_target_reaches_argv() {
        let session = DockerSession {
            image: "img".to_owned(),
            project_root: PathBuf::from("/srv/app"),
            mounts: vec![(MountOrigin::Route, "/host/adb:/context/adb".to_owned())],
            egress_pins: vec![],
            session_store: PathBuf::from("/var/varda/sessions/s1"),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
        };
        let wrapped = session
            .wrap(CommandSpec {
                program: "sh".to_owned(),
                args: vec![],
                env: BTreeMap::new(),
                cwd: None,
            })
            .unwrap();
        assert!(
            mount_values(&wrapped).contains(&"/host/adb:/context/adb:ro".to_owned()),
            "expected explicit-target mount, got {:?}",
            mount_values(&wrapped)
        );
    }

    /// M6a: `{project}` expansion and project-root-relative sources (no env
    /// mutation, so this test is isolated). `~` expansion is covered directly
    /// against the ambient HOME below.
    #[test]
    fn route_mount_expands_project_and_relative() {
        let session = DockerSession {
            image: "img".to_owned(),
            project_root: PathBuf::from("/srv/app"),
            mounts: vec![
                (MountOrigin::Route, "{project}/vendor:/vendor:ro".to_owned()),
                (MountOrigin::Route, "subdir:ro".to_owned()),
            ],
            egress_pins: vec![],
            session_store: PathBuf::from("/var/varda/sessions/s1"),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
        };
        let wrapped = session
            .wrap(CommandSpec {
                program: "sh".to_owned(),
                args: vec![],
                env: BTreeMap::new(),
                cwd: None,
            })
            .unwrap();
        let values = mount_values(&wrapped);
        assert!(values.contains(&"/srv/app/vendor:/vendor:ro".to_owned()));
        assert!(values.contains(&"/srv/app/subdir:/srv/app/subdir:ro".to_owned()));
    }

    /// M6a: `~` expands against HOME. Read (never mutate) the ambient HOME so
    /// this test does not perturb the shared test process env.
    #[test]
    fn expand_mount_path_expands_tilde_against_home() {
        let Some(home) = std::env::var_os("HOME") else {
            return; // No HOME in this environment; nothing to assert.
        };
        let home = PathBuf::from(home);
        let expanded = expand_mount_path(Path::new("~/notes"), Path::new("/srv/app"));
        assert_eq!(expanded, home.join("notes"));
    }

    /// M6a: effective mounts = union(sandbox, route), de-duplicated by target.
    /// A sandbox and a route mount claiming the same target collapse to one
    /// (first origin — sandbox — wins), while distinct targets both appear.
    #[test]
    fn effective_mounts_union_dedups_by_target() {
        let session = DockerSession {
            image: "img".to_owned(),
            project_root: PathBuf::from("/srv/app"),
            mounts: vec![
                (MountOrigin::Sandbox, "/shared:/context:ro".to_owned()),
                (MountOrigin::Route, "/other:/context:w".to_owned()),
                (MountOrigin::Route, "/extra:ro".to_owned()),
            ],
            egress_pins: vec![],
            session_store: PathBuf::from("/var/varda/sessions/s1"),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
        };
        let wrapped = session
            .wrap(CommandSpec {
                program: "sh".to_owned(),
                args: vec![],
                env: BTreeMap::new(),
                cwd: None,
            })
            .unwrap();
        let values = mount_values(&wrapped);
        // Same target `/context` de-duped to the sandbox origin's ro mount.
        assert!(values.contains(&"/shared:/context:ro".to_owned()));
        assert!(!values.iter().any(|v| v == "/other:/context"));
        // The distinct target still appears.
        assert!(values.contains(&"/extra:/extra:ro".to_owned()));
    }

    /// M6a: `from_config` composes sandbox + route mounts into one origin-tagged
    /// set (union), in that order.
    #[test]
    fn from_config_unions_sandbox_and_route_mounts() {
        let provider = DockerProvider::from_config(
            "docker",
            &SandboxConfig {
                image: Some("img".to_owned()),
                mounts: vec!["/img/cache".to_owned()],
                ..Default::default()
            },
            &["~/dev/brain/AsianDevBank:ro".to_owned()],
        )
        .unwrap();
        assert_eq!(
            provider.mounts,
            vec![
                (MountOrigin::Sandbox, "/img/cache".to_owned()),
                (MountOrigin::Route, "~/dev/brain/AsianDevBank:ro".to_owned()),
            ]
        );
    }

    /// M6a docker integration: a route context mount is visible READ-ONLY inside
    /// the container — a write attempt fails. Requires a docker daemon.
    #[tokio::test]
    #[ignore = "requires docker"]
    async fn docker_route_mount_is_read_only() {
        use tokio::process::Command as TokioCommand;

        // A host dir to expose read-only as a route context mount. It must live
        // under a path the docker VM can actually see: on a hardened Colima that
        // mounts only ~/dev, sources outside it bind as empty stubs. The repo
        // (CARGO_MANIFEST_DIR) is under the developer's mounted tree, so use its
        // target/ dir rather than ~/.varda (which may be unmounted in the VM).
        let ctx_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("m6a-ro-probe");
        std::fs::create_dir_all(&ctx_dir).unwrap();
        std::fs::write(ctx_dir.join("seed.txt"), b"seed").unwrap();
        let ctx_str = ctx_dir.display().to_string();

        let provider = DockerProvider::from_config(
            "docker",
            &SandboxConfig {
                image: Some("busybox".to_owned()),
                ..Default::default()
            },
            &[format!("{ctx_str}:/context:ro")],
        )
        .unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("m6a-proj");
        std::fs::create_dir_all(&root).unwrap();
        docker_cleanup("ro-mount-1").await;
        let session = provider.prepare(&ctx_with_id(&root, "ro-mount-1")).await.unwrap();

        // Reading the mounted file succeeds; writing into it fails (ro).
        let spec = session
            .wrap(CommandSpec {
                program: "sh".to_owned(),
                args: vec![
                    "-c".to_owned(),
                    "cat /context/seed.txt && ! (echo x > /context/probe.txt)".to_owned(),
                ],
                env: BTreeMap::new(),
                cwd: Some(root.clone()),
            })
            .unwrap();
        let status = TokioCommand::new(&spec.program)
            .args(&spec.args)
            .status()
            .await
            .expect("failed to run docker");
        // Tear down (remove the named container/volume) before asserting so a
        // failure never leaks docker resources for the next run.
        session.teardown().await.ok();
        assert!(
            status.success(),
            "read should succeed and write should fail under a read-only route mount"
        );
    }

    /// M9 exit criterion — the REAL host round-trip the earlier tests never
    /// checked. An agent writes its session store under `$HOME` (a per-session
    /// docker VOLUME, not a host bind), and Varda must read it back FROM THE HOST
    /// via `docker cp`. This works even on a VM-backed daemon whose share excludes
    /// `~/.varda` (the Colima `~/dev`-only case that silently broke the M3 bind).
    #[tokio::test]
    #[ignore = "requires docker"]
    async fn docker_session_store_round_trips_to_host_via_volume() {
        use tokio::process::Command as TokioCommand;

        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("m9-rt-proj");
        std::fs::create_dir_all(&root).unwrap();
        docker_cleanup("store-rt-1").await;

        let provider = DockerProvider::from_config(
            "docker",
            &SandboxConfig {
                image: Some("busybox".to_owned()),
                ..Default::default()
            },
            &[],
        )
        .unwrap();
        let session = provider
            .prepare(&ctx_with_id(&root, "store-rt-1"))
            .await
            .unwrap();
        let store = session.session_store_root().expect("docker store root");
        std::fs::create_dir_all(&store).unwrap();

        // Simulate the agent writing a session-store file under HOME (the volume).
        let spec = session
            .wrap(CommandSpec {
                program: "sh".to_owned(),
                args: vec![
                    "-c".to_owned(),
                    "mkdir -p \"$HOME/.claude\" && echo hi > \"$HOME/.claude/transcript.jsonl\""
                        .to_owned(),
                ],
                env: BTreeMap::new(),
                cwd: Some(root.clone()),
            })
            .unwrap();
        let status = TokioCommand::new(&spec.program)
            .args(&spec.args)
            .status()
            .await
            .expect("failed to run docker");

        // Extract the store from the volume to the HOST, capture the outcome, then
        // ALWAYS tear down before asserting so a failure never leaks resources.
        let extracted = if status.success() {
            session.extract_session_store().await
        } else {
            Ok(())
        };
        let host_file = store.join(".claude/transcript.jsonl");
        let got = std::fs::read_to_string(&host_file);
        session.teardown().await.ok();

        assert!(status.success(), "container write should succeed");
        extracted.expect("docker cp extraction should succeed");
        assert_eq!(
            got.expect("session-store file must exist on the host after extraction")
                .trim(),
            "hi",
            "the session store must round-trip container→host via the volume + docker cp"
        );
    }
}
