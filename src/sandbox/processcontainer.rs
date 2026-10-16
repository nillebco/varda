//! `processcontainer` primitive: Windows Microsoft eXecution Containers (MXC).
//!
//! MXC's Tier 1 BaseContainer confines a process without administrator rights
//! (Windows 11 24H2/25H2 with the Aug 2026 CU). The MXC SDK spawns the process
//! itself, so instead of rewriting the command into a CLI argv (docker, msb),
//! [`ProcessContainerSession::wrap`] writes a relay request file and returns
//! `varda sandbox-exec --request <file>`. That hidden subcommand
//! ([`run_relay`]) starts the allow-listing egress proxy, spawns the container
//! through `mxc-sdk`, relays stdio (or a ConPTY for interactive launches) and
//! exits with the agent's exit code. The runner and ACP code are unchanged.
//!
//! Network: MXC filters egress by CIDR only, so a non-empty `egress` list uses
//! MXC "Model 2": the container may reach exactly one loopback endpoint, the
//! relay's [`egress_proxy`](super::egress_proxy), which allow-lists hostnames.
//! That needs the identity-less loopback proxy (`hostLoopback: allow`), which
//! MXC 1.0.0 rejected on PSEC 1.0 hosts and MXC 1.0.1 (microsoft/mxc#1490)
//! supports. An empty `egress` is fully offline.
//!
//! Filesystem: the container sees only its read-write paths (the project, the
//! per-task session home) and read-only paths (the agent program's directory,
//! Node, staged files, configured mounts). MXC never remaps paths, so a mount
//! is a host path visible at the same path; `source:target` remapping is
//! refused.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{
    CommandSpec, LaunchMode, MountOrigin, SandboxContext, SandboxIdentity, SandboxProvider,
    SandboxSession, sanitize_docker_name, session_store_key, staged_temp_suffix,
    varda_sessions_root,
};
use crate::config::SandboxConfig;

/// Hidden CLI subcommand that runs a relay request (see [`run_relay`]).
pub const RELAY_SUBCOMMAND: &str = "sandbox-exec";

/// Contents of the relay request file written by `wrap()` and read by
/// `varda sandbox-exec`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RelayRequest {
    pub interactive: bool,
    /// MXC 1.0.0 container request (JSON), WITHOUT `runtimeConfig`: the relay
    /// adds `runtimeConfig.networkProxy` once its proxy is bound.
    pub mxc: serde_json::Value,
    /// Hostname allow-list for the egress proxy. Empty ⇒ offline, no proxy.
    pub egress: Vec<String>,
    /// Where the proxy logs its ALLOW/DENY decisions.
    pub proxy_log: Option<PathBuf>,
}

/// One `mounts` entry for this primitive: a host path, read-only unless it
/// ends in `:rw`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostMount {
    path: PathBuf,
    writable: bool,
}

/// Parse `C:\path`, `C:\path:ro` or `C:\path:rw`. A drive letter's colon is
/// not a separator. Anything else after the path (a guest target) is refused:
/// MXC cannot remap paths.
fn parse_host_mount(raw: &str) -> Result<HostMount> {
    let raw = raw.trim();
    let (body, writable) = if let Some(body) = raw.strip_suffix(":rw") {
        (body, true)
    } else if let Some(body) = raw.strip_suffix(":ro") {
        (body, false)
    } else {
        (raw, false)
    };
    // Skip a leading drive letter (`C:`) when looking for a target separator.
    let after_drive = match body.as_bytes() {
        [letter, b':', ..] if letter.is_ascii_alphabetic() => &body[2..],
        _ => body,
    };
    if after_drive.contains(':') {
        bail!(
            "mount '{raw}': processcontainer mounts are host paths seen at the same path inside the \
             container; `source:target` remapping is not supported (use `PATH`, `PATH:ro` or `PATH:rw`)"
        );
    }
    if body.is_empty() {
        bail!("mount '{raw}' has an empty path");
    }
    Ok(HostMount {
        path: PathBuf::from(strip_verbatim(body)),
        writable,
    })
}

/// Drop the `\\?\` verbatim prefix `std::fs::canonicalize` puts on Windows
/// paths (`\\?\C:\x` → `C:\x`, `\\?\UNC\srv\share` → `\\srv\share`).
/// `cmd.exe` treats a verbatim cwd as an unsupported UNC path and falls back
/// to `C:\Windows`, and MXC path policies are written in the plain form.
fn strip_verbatim(text: &str) -> String {
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_owned(),
        _ => text.to_owned(),
    }
}

fn plain_path(path: &Path) -> PathBuf {
    PathBuf::from(strip_verbatim(&path.display().to_string()))
}

/// Quote one argument for `CommandLineToArgvW` / the MSVC CRT parser.
fn quote_windows_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '\n', '\u{b}', '"']) {
        return arg.to_owned();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// How the agent program is launched inside the container.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedProgram {
    /// The full command line handed to MXC.
    command_line: String,
    /// Directory holding the program; added read-only and prepended to PATH.
    dir: Option<PathBuf>,
}

/// Find `program` on `path_var` (entries split on `separator`) trying each
/// `pathext` extension, the way `cmd.exe` does. An absolute or relative path
/// with a directory component is used as-is.
fn find_on_path(
    program: &str,
    path_var: &str,
    separator: char,
    pathext: &[String],
    is_file: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let has_ext = Path::new(program).extension().is_some();
    let candidates = |base: PathBuf| -> Vec<PathBuf> {
        let mut out = Vec::new();
        if has_ext {
            out.push(base.clone());
        }
        for ext in pathext {
            let mut with_ext = base.clone().into_os_string();
            with_ext.push(ext);
            out.push(PathBuf::from(with_ext));
        }
        out
    };
    if program.contains(['\\', '/']) {
        return candidates(PathBuf::from(program))
            .into_iter()
            .find(|c| is_file(c));
    }
    path_var
        .split(separator)
        .filter(|entry| !entry.trim().is_empty())
        .flat_map(|entry| candidates(Path::new(entry.trim()).join(program)))
        .find(|c| is_file(c))
}

/// Build the container command line. A real `.exe` is launched directly by
/// MXC by absolute path. A `.cmd`/`.bat` shim (npm installs `copilot.cmd`) goes
/// through `cmd.exe /d /c` by BARE name with its directory on PATH: inside a
/// BaseContainer, `cmd` refuses to launch an absolute path ("Access is denied",
/// spike #1181), while the same file found through PATH runs.
fn resolve_program(program: &str, args: &[String], found: Option<PathBuf>) -> ResolvedProgram {
    let args_line: String = args
        .iter()
        .map(|a| format!(" {}", quote_windows_arg(a)))
        .collect();
    let Some(found) = found else {
        // Unresolved: let cmd search the container PATH.
        return ResolvedProgram {
            command_line: format!("cmd.exe /d /c {}{args_line}", quote_windows_arg(program)),
            dir: None,
        };
    };
    let dir = found.parent().map(Path::to_path_buf);
    let ext = found
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    if ext == "cmd" || ext == "bat" {
        let stem = found
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(program)
            .to_owned();
        ResolvedProgram {
            command_line: format!("cmd.exe /d /c {}{args_line}", quote_windows_arg(&stem)),
            dir,
        }
    } else {
        ResolvedProgram {
            command_line: format!(
                "{}{args_line}",
                quote_windows_arg(&found.display().to_string())
            ),
            dir,
        }
    }
}

/// Inputs to [`build_mxc_request`], separated from the filesystem so the
/// request shape is unit-testable on any OS.
#[derive(Debug, Clone)]
struct RequestInputs {
    container_id: String,
    command_line: String,
    cwd: PathBuf,
    env: BTreeMap<String, String>,
    readwrite: Vec<PathBuf>,
    readonly: Vec<PathBuf>,
    online: bool,
}

fn path_strings(paths: &[PathBuf]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    paths
        .iter()
        .map(|p| p.display().to_string())
        .filter(|p| seen.insert(p.to_ascii_lowercase()))
        .collect()
}

/// The MXC 1.0.0 request. `allowDaclMutation: false` pins Tier 1: MXC must
/// fail rather than fall back to Tier 3, which edits host ACLs and needs an
/// admin host-prep. `ui.disable: false` because Node and PowerShell die at
/// Win32k init (0xC0000142) without UI access (spike #1170).
fn build_mxc_request(inputs: &RequestInputs) -> serde_json::Value {
    let env: Vec<String> = inputs.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let network = if inputs.online {
        // Model 2, identity-less proxy: only the relay's loopback proxy is reachable.
        serde_json::json!({
            "egress": { "default": "deny" },
            "ingress": { "default": "allow", "hostLoopback": "allow" }
        })
    } else {
        serde_json::json!({
            "egress": { "default": "deny" },
            "ingress": { "default": "deny", "hostLoopback": "deny" }
        })
    };
    serde_json::json!({
        "version": "1.0.0",
        "containerId": inputs.container_id,
        "containment": "processcontainer",
        "fallback": { "allowDaclMutation": false },
        "ui": { "disable": false },
        "process": {
            "commandLine": inputs.command_line,
            "cwd": inputs.cwd.display().to_string(),
            "env": env,
            "inheritDefaultEnv": false,
            "timeout": 0
        },
        "filesystem": {
            "readwritePaths": path_strings(&inputs.readwrite),
            "readonlyPaths": path_strings(&inputs.readonly)
        },
        "network": network
    })
}

pub struct ProcessContainerProvider {
    name: String,
    mounts: Vec<HostMount>,
    egress: Vec<String>,
    env: BTreeMap<String, String>,
    identity: SandboxIdentity,
}

impl ProcessContainerProvider {
    pub fn from_config(
        name: &str,
        config: &SandboxConfig,
        mounts: Vec<(MountOrigin, String)>,
    ) -> Result<Self> {
        if config.image.is_some() || config.build.is_some() || config.image_from.is_some() {
            bail!(
                "sandbox '{name}' uses primitive 'processcontainer', which runs host binaries: \
                 remove `image`/`build`/`image_from`"
            );
        }
        if config.egress_mode == crate::config::EgressMode::DnsPin && !config.egress.is_empty() {
            bail!(
                "sandbox '{name}': `egress_mode = \"dns-pin\"` has no meaning for processcontainer; \
                 its egress always goes through the allow-listing proxy (use strict or proxy)"
            );
        }
        let mounts = mounts
            .iter()
            .map(|(_, raw)| {
                parse_host_mount(raw).with_context(|| format!("sandbox '{name}'"))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Self {
            name: name.to_owned(),
            mounts,
            egress: config
                .egress
                .iter()
                .filter_map(|entry| super::expand_egress_entry(entry))
                .collect(),
            env: config.env.clone(),
            identity: SandboxIdentity::default(),
        })
    }

    pub fn with_identity(mut self, identity: SandboxIdentity) -> Self {
        self.identity = identity;
        self
    }
}

#[async_trait]
impl SandboxProvider for ProcessContainerProvider {
    fn name(&self) -> &str {
        &self.name
    }

    /// The session home is a live host directory keyed on the task, so a rerun
    /// sees its predecessor's store without any copy-in.
    fn supports_interactive_resume(&self) -> bool {
        true
    }

    async fn prepare(&self, ctx: &SandboxContext<'_>) -> Result<Box<dyn SandboxSession>> {
        if !cfg!(windows) {
            bail!(
                "sandbox '{}' uses primitive 'processcontainer', which is Windows-only (MXC)",
                self.name
            );
        }
        if !self.identity.auth_files.is_empty() {
            bail!(
                "sandbox '{}': file-target credentials are not supported by processcontainer yet; \
                 use an env-target credential",
                self.name
            );
        }
        if self.identity.ssh_auth_sock.is_some() {
            eprintln!(
                "warning: sandbox '{}': SSH agent forwarding is not supported by processcontainer; ignored",
                self.name
            );
        }
        let root = varda_sessions_root().join(session_store_key(ctx));
        let home = root.join("home");
        let staged = root.join("staged");
        let relay = root.join("relay");
        for dir in [
            home.join("AppData").join("Roaming"),
            home.join("AppData").join("Local"),
            home.join("tmp"),
            staged.clone(),
            relay.clone(),
        ] {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
        }
        let handle = sanitize_docker_name(ctx.session_id);
        let handle: String = handle.chars().take(48).collect();
        Ok(Box::new(ProcessContainerSession {
            project_root: plain_path(ctx.project_root),
            mounts: self.mounts.clone(),
            egress: self.egress.clone(),
            env: self.env.clone(),
            identity: self.identity.clone(),
            container_id: format!("varda-{handle}"),
            home,
            staged,
            relay,
            relay_files: std::sync::Mutex::new(Vec::new()),
        }))
    }
}

pub struct ProcessContainerSession {
    project_root: PathBuf,
    mounts: Vec<HostMount>,
    egress: Vec<String>,
    env: BTreeMap<String, String>,
    identity: SandboxIdentity,
    container_id: String,
    /// Live session home: USERPROFILE/HOME/APPDATA/LOCALAPPDATA/TEMP.
    home: PathBuf,
    /// Read-only dir for staged prompts and files.
    staged: PathBuf,
    /// Dir for relay request files and the proxy log.
    relay: PathBuf,
    relay_files: std::sync::Mutex<Vec<PathBuf>>,
}

impl ProcessContainerSession {
    fn host_env(name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    }

    /// The container env: a minimal Windows base, the session home as the
    /// profile, then the sandbox's static env, identity env and the agent's
    /// own env (later wins), with the profile variables re-pinned last so the
    /// agent cannot point HOME back at the real profile.
    fn container_env(
        &self,
        spec_env: &BTreeMap<String, String>,
        extra_path: &[PathBuf],
    ) -> BTreeMap<String, String> {
        let system_root = Self::host_env("SYSTEMROOT").unwrap_or_else(|| r"C:\Windows".to_owned());
        let mut env = BTreeMap::new();
        env.insert("SYSTEMROOT".to_owned(), system_root.clone());
        env.insert("WINDIR".to_owned(), system_root.clone());
        for name in ["PATHEXT", "COMSPEC", "NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE", "OS"] {
            if let Some(value) = Self::host_env(name) {
                env.insert(name.to_owned(), value);
            }
        }
        env.insert("NODE_USE_ENV_PROXY".to_owned(), "1".to_owned());
        for (k, v) in &self.env {
            env.insert(k.clone(), v.clone());
        }
        for (k, v) in self.identity.guest_env() {
            env.insert(k, v);
        }
        for (k, v) in spec_env {
            env.insert(k.clone(), v.clone());
        }
        let mut path: Vec<String> = extra_path.iter().map(|p| p.display().to_string()).collect();
        path.push(format!(r"{system_root}\System32"));
        path.push(system_root.clone());
        path.push(format!(r"{system_root}\System32\WindowsPowerShell\v1.0"));
        env.insert("PATH".to_owned(), path.join(";"));
        let home = self.home.display().to_string();
        let tmp = self.home.join("tmp").display().to_string();
        env.insert("USERPROFILE".to_owned(), home.clone());
        env.insert("HOME".to_owned(), home.clone());
        env.insert(
            "APPDATA".to_owned(),
            self.home.join("AppData").join("Roaming").display().to_string(),
        );
        env.insert(
            "LOCALAPPDATA".to_owned(),
            self.home.join("AppData").join("Local").display().to_string(),
        );
        env.insert("TEMP".to_owned(), tmp.clone());
        env.insert("TMP".to_owned(), tmp);
        // Where staged files (the task prompt) live, so an agent can be given
        // read access to that folder (`copilot --add-dir %VARDA_STAGED_DIR%`).
        env.insert(
            "VARDA_STAGED_DIR".to_owned(),
            self.staged.display().to_string(),
        );
        env
    }

    fn write_relay(&self, request: &RelayRequest) -> Result<PathBuf> {
        let body = serde_json::to_string_pretty(request)?;
        let path = self
            .relay
            .join(format!("request-{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&path, body)
            .with_context(|| format!("failed to write relay request {}", path.display()))?;
        self.relay_files
            .lock()
            .expect("relay_files mutex poisoned")
            .push(path.clone());
        Ok(path)
    }
}

#[async_trait]
impl SandboxSession for ProcessContainerSession {
    fn wrap(&self, spec: CommandSpec, mode: LaunchMode) -> Result<CommandSpec> {
        // Host paths reach the agent through `{project}`-style args, env values
        // and the cwd; give the container the plain form of all of them.
        let spec = CommandSpec {
            args: spec.args.iter().map(|a| strip_verbatim(a)).collect(),
            env: spec
                .env
                .iter()
                .map(|(k, v)| (k.clone(), strip_verbatim(v)))
                .collect(),
            cwd: spec.cwd.as_deref().map(plain_path),
            ..spec
        };
        let host_path = spec
            .env
            .get("PATH")
            .cloned()
            .or_else(|| Self::host_env("PATH"))
            .unwrap_or_default();
        let pathext: Vec<String> = Self::host_env("PATHEXT")
            .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_owned())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(str::to_owned)
            .collect();
        let is_file = |p: &Path| p.is_file();
        let found = find_on_path(&spec.program, &host_path, ';', &pathext, &is_file);
        let resolved = resolve_program(&spec.program, &spec.args, found);
        // Node: npm shims (copilot.cmd) call `node`, which lives elsewhere.
        let node_dir = find_on_path("node", &host_path, ';', &pathext, &is_file)
            .and_then(|p| p.parent().map(Path::to_path_buf));

        let mut extra_path = Vec::new();
        let mut readonly = vec![self.staged.clone()];
        for dir in resolved.dir.iter().chain(node_dir.iter()) {
            extra_path.push(dir.clone());
            readonly.push(dir.clone());
        }
        let mut readwrite = vec![self.project_root.clone(), self.home.clone()];
        for mount in &self.mounts {
            if mount.writable {
                readwrite.push(mount.path.clone());
            } else {
                readonly.push(mount.path.clone());
            }
        }

        let inputs = RequestInputs {
            container_id: self.container_id.clone(),
            command_line: resolved.command_line,
            cwd: spec.cwd.clone().unwrap_or_else(|| self.project_root.clone()),
            env: self.container_env(&spec.env, &extra_path),
            readwrite,
            readonly,
            online: !self.egress.is_empty(),
        };
        let request = RelayRequest {
            interactive: mode == LaunchMode::Interactive,
            mxc: build_mxc_request(&inputs),
            egress: self.egress.clone(),
            proxy_log: Some(self.relay.join("egress-proxy.log")),
        };
        let path = self.write_relay(&request)?;
        let exe = std::env::current_exe().context("failed to locate the varda executable")?;
        Ok(CommandSpec {
            program: exe.display().to_string(),
            args: vec![
                RELAY_SUBCOMMAND.to_owned(),
                "--request".to_owned(),
                path.display().to_string(),
            ],
            env: BTreeMap::new(),
            cwd: Some(self.project_root.clone()),
        })
    }

    /// Staged files land in the read-only `staged` dir (the default host temp
    /// is outside the container's view).
    fn stage_file(&self, content: &str, guest_path: &str) -> Result<String> {
        let name = Path::new(guest_path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("staged");
        let path = self
            .staged
            .join(format!("{}-{name}", staged_temp_suffix(content)));
        std::fs::write(&path, content)
            .with_context(|| format!("failed to stage {}", path.display()))?;
        Ok(path.display().to_string())
    }

    fn session_store_root(&self) -> Option<PathBuf> {
        Some(self.home.clone())
    }

    fn validate_mounts(&self) -> Result<()> {
        for mount in &self.mounts {
            if !mount.path.exists() {
                bail!("mount source {} does not exist", mount.path.display());
            }
        }
        Ok(())
    }

    fn mount_targets(&self) -> Option<Vec<PathBuf>> {
        let mut targets = vec![self.project_root.clone(), self.staged.clone()];
        targets.extend(self.mounts.iter().map(|m| m.path.clone()));
        Some(targets)
    }

    async fn teardown(self: Box<Self>) -> Result<()> {
        let files = std::mem::take(&mut *self.relay_files.lock().expect("relay_files mutex poisoned"));
        for file in files {
            let _ = std::fs::remove_file(file);
        }
        Ok(())
    }
}

/// Hidden CLI subcommand run INSIDE the container for interactive launches.
pub const PTY_HOST_SUBCOMMAND: &str = "pty-host";

/// Rewrite an interactive request so the container runs
/// `varda pty-host --rows R --cols C --command-line <original>` (the same
/// varda executable, made readable inside the container) instead of the agent
/// directly. See [`super::pty_host`].
fn wrap_for_pty_host(
    mxc: &mut serde_json::Value,
    exe: &Path,
    rows: u16,
    cols: u16,
) -> Result<()> {
    let original = mxc["process"]["commandLine"]
        .as_str()
        .context("relay request has no process.commandLine")?
        .to_owned();
    mxc["process"]["commandLine"] = serde_json::Value::String(format!(
        "{} {PTY_HOST_SUBCOMMAND} --rows {rows} --cols {cols} --command-line {}",
        quote_windows_arg(&exe.display().to_string()),
        quote_windows_arg(&original)
    ));
    if let Some(dir) = exe.parent() {
        let readonly = &mut mxc["filesystem"]["readonlyPaths"];
        if !readonly.is_array() {
            *readonly = serde_json::json!([]);
        }
        readonly
            .as_array_mut()
            .expect("readonlyPaths is an array")
            .push(serde_json::Value::String(dir.display().to_string()));
    }
    Ok(())
}

/// `varda sandbox-exec --request <file>`: run a relay request and return the
/// agent's exit code.
pub async fn run_relay(request_path: &Path) -> Result<i32> {
    let body = std::fs::read_to_string(request_path)
        .with_context(|| format!("failed to read relay request {}", request_path.display()))?;
    let request: RelayRequest = serde_json::from_str(&body).context("malformed relay request")?;
    let mut mxc = request.mxc.clone();
    if !request.egress.is_empty() {
        let proxy = super::egress_proxy::start(
            super::egress_proxy::AllowList::parse(&request.egress),
            request.proxy_log.clone(),
        )
        .await?;
        mxc["runtimeConfig"] = serde_json::json!({
            "networkProxy": format!("http://{}", proxy.addr)
        });
    }
    let json = serde_json::to_string(&mxc)?;
    let interactive = request.interactive;
    tokio::task::spawn_blocking(move || relay::run(&json, interactive))
        .await
        .context("relay thread panicked")?
}

#[cfg(windows)]
mod relay {
    //! Windows side of the relay: spawn through `mxc-sdk` and pump stdio.

    use std::io::{Read, Write};
    use std::thread;
    use std::time::Duration;

    use anyhow::{Context, Result, anyhow};
    use mxc_sdk::v1::MxcPtySize;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Console::{
        CONSOLE_MODE, CONSOLE_SCREEN_BUFFER_INFO, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT,
        ENABLE_PROCESSED_INPUT, ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_INPUT,
        ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetConsoleScreenBufferInfo,
        GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleMode,
    };

    fn mxc_error(error: mxc_sdk::v1::Error) -> anyhow::Error {
        anyhow!("MXC: {error}")
    }

    fn exit_code(status: mxc_sdk::v1::WaitResult) -> i32 {
        match status {
            mxc_sdk::v1::WaitResult::Exited(code) => code,
            mxc_sdk::v1::WaitResult::TimedOut => 124,
        }
    }

    pub fn run(json: &str, interactive: bool) -> Result<i32> {
        if interactive {
            run_pty(json)
        } else {
            run_pipes(json)
        }
    }

    fn run_pipes(json: &str) -> Result<i32> {
        let mut process =
            mxc_sdk::__ffi::spawn_container_json(json, false).map_err(mxc_error)?;
        for warning in process.warnings() {
            eprintln!("warning: MXC: {warning}");
        }
        if let Some(mut stdin) = process.take_stdin() {
            thread::spawn(move || {
                let _ = std::io::copy(&mut std::io::stdin().lock(), &mut stdin);
            });
        }
        let out = process.take_stdout().map(|mut stdout| {
            thread::spawn(move || {
                let _ = std::io::copy(&mut stdout, &mut std::io::stdout().lock());
            })
        });
        let err = process.take_stderr().map(|mut stderr| {
            thread::spawn(move || {
                let _ = std::io::copy(&mut stderr, &mut std::io::stderr().lock());
            })
        });
        let status = process.wait().context("failed to wait for the container")?;
        for handle in [out, err].into_iter().flatten() {
            let _ = handle.join();
        }
        Ok(exit_code(status))
    }

    struct ConsoleModeGuard {
        input: HANDLE,
        output: HANDLE,
        input_mode: CONSOLE_MODE,
        output_mode: CONSOLE_MODE,
    }

    impl ConsoleModeGuard {
        /// Raw VT input (keys reach the agent's TUI, Ctrl-C included) and VT
        /// output processing (ConPTY emits escape sequences).
        fn enter() -> Option<Self> {
            unsafe {
                let input = GetStdHandle(STD_INPUT_HANDLE);
                let output = GetStdHandle(STD_OUTPUT_HANDLE);
                let mut input_mode: CONSOLE_MODE = 0;
                let mut output_mode: CONSOLE_MODE = 0;
                if GetConsoleMode(input, &mut input_mode) == 0
                    || GetConsoleMode(output, &mut output_mode) == 0
                {
                    return None;
                }
                let raw_in = (input_mode
                    & !(ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT))
                    | ENABLE_VIRTUAL_TERMINAL_INPUT;
                SetConsoleMode(input, raw_in);
                SetConsoleMode(
                    output,
                    output_mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING,
                );
                Some(Self {
                    input,
                    output,
                    input_mode,
                    output_mode,
                })
            }
        }
    }

    impl Drop for ConsoleModeGuard {
        fn drop(&mut self) {
            unsafe {
                SetConsoleMode(self.input, self.input_mode);
                SetConsoleMode(self.output, self.output_mode);
            }
        }
    }

    fn console_size() -> Option<MxcPtySize> {
        unsafe {
            let output = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(output, &mut info) == 0 {
                return None;
            }
            let cols = (info.srWindow.Right - info.srWindow.Left + 1).max(1) as u16;
            let rows = (info.srWindow.Bottom - info.srWindow.Top + 1).max(1) as u16;
            Some(MxcPtySize {
                rows,
                cols,
                ..MxcPtySize::default()
            })
        }
    }

    /// Interactive: MXC cannot attach a ConPTY to a BaseContainer, so the
    /// container runs `varda pty-host` over plain pipes (see
    /// [`super::super::pty_host`]) and this side relays the real console to
    /// those pipes in raw VT mode, sending resizes in-band.
    fn run_pty(json: &str) -> Result<i32> {
        let size = console_size();
        let (rows, cols) = size.map_or((30, 120), |s| (s.rows, s.cols));
        let exe = super::plain_path(
            &std::env::current_exe().context("failed to locate the varda executable")?,
        );
        let mut mxc: serde_json::Value = serde_json::from_str(json)?;
        super::wrap_for_pty_host(&mut mxc, &exe, rows, cols)?;
        let json = serde_json::to_string(&mxc)?;

        let mut process =
            mxc_sdk::__ffi::spawn_container_json(&json, false).map_err(mxc_error)?;
        for warning in process.warnings() {
            eprintln!("warning: MXC: {warning}");
        }
        let _console = ConsoleModeGuard::enter();
        let writer = std::sync::Arc::new(std::sync::Mutex::new(
            process.take_stdin().context("container stdin")?,
        ));
        let mut stdout = process.take_stdout().context("container stdout")?;
        let mut stderr = process.take_stderr();

        let output = thread::spawn(move || {
            let mut out = std::io::stdout();
            let mut buf = [0u8; 8192];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if out.write_all(&buf[..n]).is_err() {
                            break;
                        }
                        let _ = out.flush();
                    }
                }
            }
        });
        let errors = stderr.take().map(|mut stderr| {
            thread::spawn(move || {
                let _ = std::io::copy(&mut stderr, &mut std::io::stderr().lock());
            })
        });
        // stdin → container. Detached: a blocking console read cannot be
        // cancelled, and the process exits right after the agent does.
        {
            let writer = writer.clone();
            thread::spawn(move || {
                let mut stdin = std::io::stdin();
                let mut buf = [0u8; 1024];
                loop {
                    let n = match stdin.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    };
                    let Ok(mut w) = writer.lock() else { break };
                    if w.write_all(&buf[..n]).is_err() {
                        break;
                    }
                    let _ = w.flush();
                }
            });
        }
        // Follow console resizes (in-band, one write per sequence).
        {
            let writer = writer.clone();
            thread::spawn(move || {
                let mut last = console_size();
                loop {
                    thread::sleep(Duration::from_millis(250));
                    let now = console_size();
                    if now != last {
                        if let Some(size) = now {
                            let sequence =
                                super::super::pty_host::resize_sequence(size.rows, size.cols);
                            let Ok(mut w) = writer.lock() else { break };
                            if w.write_all(&sequence).is_err() {
                                break;
                            }
                            let _ = w.flush();
                        }
                        last = now;
                    }
                }
            });
        }
        let status = process.wait().context("failed to wait for the container")?;
        let _ = output.join();
        if let Some(errors) = errors {
            let _ = errors.join();
        }
        Ok(exit_code(status))
    }
}

#[cfg(not(windows))]
mod relay {
    pub fn run(_json: &str, _interactive: bool) -> anyhow::Result<i32> {
        anyhow::bail!("`varda sandbox-exec` runs processcontainer sandboxes and is Windows-only")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_mounts_parse_drive_paths_and_modes() {
        assert_eq!(
            parse_host_mount(r"C:\Users\me\brain").unwrap(),
            HostMount {
                path: PathBuf::from(r"C:\Users\me\brain"),
                writable: false
            }
        );
        assert!(parse_host_mount(r"C:\data:rw").unwrap().writable);
        assert!(!parse_host_mount(r"C:\data:ro").unwrap().writable);
        assert!(parse_host_mount(r"C:\data:/workspace/data").is_err());
        assert!(parse_host_mount(r"C:\data:D:\other:ro").is_err());
    }

    #[test]
    fn verbatim_prefixes_are_stripped() {
        assert_eq!(strip_verbatim(r"\\?\C:\Users\IB7\proj"), r"C:\Users\IB7\proj");
        assert_eq!(strip_verbatim(r"\\?\UNC\srv\share\x"), r"\\srv\share\x");
        assert_eq!(strip_verbatim(r"C:\plain"), r"C:\plain");
        assert_eq!(strip_verbatim("--add-dir"), "--add-dir");
        // Not a drive path after the prefix: left alone.
        assert_eq!(strip_verbatim(r"\\?\Volume{x}\a"), r"\\?\Volume{x}\a");
    }

    #[test]
    fn windows_args_are_quoted_for_argv_parsing() {
        assert_eq!(quote_windows_arg("plain"), "plain");
        assert_eq!(quote_windows_arg("two words"), "\"two words\"");
        assert_eq!(quote_windows_arg(""), "\"\"");
        assert_eq!(quote_windows_arg(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(quote_windows_arg(r"C:\dir with space\"), r#""C:\dir with space\\""#);
    }

    #[test]
    fn programs_resolve_through_path_and_pathext() {
        let files = [
            PathBuf::from("/npm").join("copilot.cmd"),
            PathBuf::from("/node").join("node.exe"),
        ];
        let is_file = |p: &Path| files.iter().any(|f| f == p);
        let pathext = vec![".EXE".to_owned(), ".cmd".to_owned()];
        assert_eq!(
            find_on_path("copilot", "/nope;/npm;/node", ';', &pathext, &is_file),
            Some(PathBuf::from("/npm").join("copilot.cmd"))
        );
        assert_eq!(
            find_on_path("node", "/npm;/node", ';', &[".exe".to_owned()], &is_file),
            Some(PathBuf::from("/node").join("node.exe"))
        );
        assert_eq!(find_on_path("missing", "/npm", ';', &pathext, &is_file), None);
    }

    #[test]
    fn cmd_shims_run_by_bare_name_and_exes_by_absolute_path() {
        let args = vec!["--resume".to_owned(), "a b".to_owned()];
        let shim = resolve_program(
            "copilot",
            &args,
            Some(PathBuf::from(r"C:\npm").join("copilot.cmd")),
        );
        assert_eq!(shim.command_line, "cmd.exe /d /c copilot --resume \"a b\"");
        assert!(shim.dir.is_some());
        let exe = resolve_program(
            "node",
            &[],
            Some(PathBuf::from(r"C:\Program Files\nodejs\node.exe")),
        );
        assert!(exe.command_line.starts_with('"'), "{}", exe.command_line);
        assert!(exe.command_line.ends_with("node.exe\""), "{}", exe.command_line);
        let unresolved = resolve_program("gh", &[], None);
        assert_eq!(unresolved.command_line, "cmd.exe /d /c gh");
    }

    fn inputs(online: bool) -> RequestInputs {
        RequestInputs {
            container_id: "varda-abc".to_owned(),
            command_line: "cmd.exe /d /c copilot".to_owned(),
            cwd: PathBuf::from(r"C:\proj"),
            env: BTreeMap::from([("A".to_owned(), "1".to_owned())]),
            readwrite: vec![PathBuf::from(r"C:\proj"), PathBuf::from(r"C:\Proj")],
            readonly: vec![PathBuf::from(r"C:\npm")],
            online,
        }
    }

    #[test]
    fn mxc_request_pins_tier1_and_ui() {
        let request = build_mxc_request(&inputs(false));
        assert_eq!(request["version"], "1.0.0");
        assert_eq!(request["containment"], "processcontainer");
        assert_eq!(request["fallback"]["allowDaclMutation"], false);
        assert_eq!(request["ui"]["disable"], false);
        assert_eq!(request["process"]["inheritDefaultEnv"], false);
        assert_eq!(request["process"]["env"][0], "A=1");
        assert_eq!(request["process"]["timeout"], 0);
        // Case-insensitive de-duplication of Windows paths.
        assert_eq!(
            request["filesystem"]["readwritePaths"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert!(request.get("runtimeConfig").is_none());
    }

    #[test]
    fn offline_denies_all_and_online_uses_the_loopback_proxy_contract() {
        let offline = build_mxc_request(&inputs(false));
        assert_eq!(offline["network"]["ingress"]["hostLoopback"], "deny");
        assert_eq!(offline["network"]["egress"]["default"], "deny");
        let online = build_mxc_request(&inputs(true));
        assert_eq!(online["network"]["egress"]["default"], "deny");
        assert_eq!(online["network"]["ingress"]["default"], "allow");
        assert_eq!(online["network"]["ingress"]["hostLoopback"], "allow");
    }

    #[test]
    fn interactive_requests_run_the_agent_under_pty_host() {
        let mut mxc = build_mxc_request(&inputs(true));
        let exe = PathBuf::from(r"C:\Users\me\AppData\Local\varda\bin").join("varda.exe");
        wrap_for_pty_host(&mut mxc, &exe, 40, 120).unwrap();
        let line = mxc["process"]["commandLine"].as_str().unwrap();
        assert!(line.contains(" pty-host --rows 40 --cols 120 --command-line "), "{line}");
        assert!(line.ends_with("\"cmd.exe /d /c copilot\""), "{line}");
        let readonly = mxc["filesystem"]["readonlyPaths"].as_array().unwrap();
        assert!(readonly.iter().any(|p| p.as_str().unwrap().ends_with("bin")), "{readonly:?}");
    }

    #[test]
    fn relay_request_round_trips() {
        let request = RelayRequest {
            interactive: true,
            mxc: build_mxc_request(&inputs(true)),
            egress: vec!["api.githubcopilot.com".to_owned()],
            proxy_log: Some(PathBuf::from(r"C:\log")),
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<RelayRequest>(&json).unwrap(), request);
    }

    #[test]
    fn from_config_rejects_images_and_remapped_mounts() {
        let mut config = SandboxConfig {
            primitive: "processcontainer".to_owned(),
            ..SandboxConfig::default()
        };
        config.image = Some("ubuntu".to_owned());
        assert!(ProcessContainerProvider::from_config("pc", &config, vec![]).is_err());
        config.image = None;
        let remapped = vec![(MountOrigin::Sandbox, r"C:\a:/guest".to_owned())];
        assert!(ProcessContainerProvider::from_config("pc", &config, remapped).is_err());
        let ok = vec![(MountOrigin::Sandbox, r"C:\a:ro".to_owned())];
        assert!(ProcessContainerProvider::from_config("pc", &config, ok).is_ok());
    }
}
