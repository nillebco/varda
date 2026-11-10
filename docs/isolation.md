# What each sandbox primitive actually isolates

This page states, per `primitive`, what an agent inside the sandbox can and cannot reach:
files, network, environment and credentials, the desktop, and resources. It describes what
the code does today, and marks each claim **verified** (tested on a real host), **by
construction** (follows from the arguments varda passes; no dedicated test), or **unverified**.
Update it in the same change as any code that alters a boundary.

[sandboxing.md](./sandboxing.md) explains how to configure the sandboxes. This page explains what
those settings actually guarantee.

## At a glance

| | `local` | `docker` / `podman` | `microsandbox` | `processcontainer` (Windows MXC) |
|---|---|---|---|---|
| Boundary | none | Linux namespaces + cgroups; shared kernel (on macOS, the Colima/Docker VM's kernel) | own-kernel microVM (`msb`) | Tier 1 BaseContainer: OS-built restricted token and ACLs, same kernel and same desktop session |
| Admin needed | no | daemon access (≈ root on Linux) | no | no |
| Project | rw, in place | rw bind at the same path | rw bind at the same path | rw grant at the same path |
| Rest of the host filesystem | everything | invisible unless mounted | invisible unless mounted | denied except granted paths and OS dirs (`C:\Windows`, `Program Files`: read/execute) |
| `HOME` | the user's | per-session named volume | per-session guest home | per-session dir under `VARDA_HOME` |
| Egress, empty list | open | `--network none` | `--net-default deny` | MXC `egress: deny`, no proxy |
| Egress, with a list | open | proxy sidecar on an `--internal` network (`proxy`/`strict`), or DNS pins (`dns-pin`) | in-guest net rules per host | one loopback proxy in the varda relay |
| Host env inherited | yes | no | no | no (`inheritDefaultEnv: false`) |
| Desktop / UI | full | none (no display) | none | **screen capture open**; clipboard, other windows and keystroke delivery blocked (`ui.disable: false`) |
| CPU / memory caps | no | opt-in `memory`/`cpus` | opt-in `memory`/`cpus` | none set |

## Shared rules (every non-`local` primitive)

- **Mount denylists, for every config origin including the central config** (`src/sandbox/mod.rs`):
  `CREDENTIAL_DENYLIST` (`.claude`, `.codex`, `.copilot`, `.aws`, `.azure`, `.terraform.d`, `.ssh`,
  `.config/gcloud`, `.config/fnox`, `.gnupg`, `.kube`, `.docker`, `.netrc`, `.git-credentials`),
  `CONTROL_PLANE_DENYLIST` (`.varda`), and the docker/podman socket. A mount that resolves into any
  of them is refused before launch. *By construction, unit-tested.*
- **Extra mounts are read-only by default**; `:rw` must be explicit.
- **Credentials** reach the box only through declared channels: env-target values, files staged
  read-only in the guest, a curated `identity_context` file, `GIT_AUTHOR_*`/`GIT_COMMITTER_*`
  env, and opt-in SSH-agent forwarding. Whatever is injected is fully readable by the agent; the
  sandbox limits *which* secrets go in, not what the agent does with them.
- **The project is read-write and shared with the host.** Anything the host later executes from it
  — git hooks and `.git/config` (when the host runs git there), `pre-commit`/`prek`, build scripts,
  editor tasks — is effectively agent-authored. This is the main path out of every primitive.
  Host-side tools that run git in a project (an MCP broker, a launcher's pre-session `git pull`)
  must disable hooks and fsmonitor and refuse repo config that names a command.
- **Egress allow-list grammar** (docker proxy and processcontainer proxy): `host` matches exactly,
  `*.host` matches exactly one extra label; CONNECT to 443/563 and plain HTTP to 80, plus any port
  an entry names (`127.0.0.1:8765`). Filtering is by hostname only: an allowed host is allowed for
  every path and account on it (e.g. any repository on `github.com`). TLS is not inspected.

## `local`

No isolation. The agent runs as you, with your environment, files, network, keychain and desktop.
Use it only for trusted, interactive work.

## `docker` / `podman`

**Files.** The project is bind-mounted read-write at its host path; `HOME` is a per-session named
volume (the host `HOME` is never mounted); extra mounts come only from `[sandboxes.X].mounts` and
`Route.mounts`, read-only unless `:rw`. A `devcontainer.json` contributes its image only — never its
mounts, `runArgs`, socket forwarding or hooks. *By construction; the denylist is unit-tested.*

**Network.**
- Empty list: `--network none`. *By construction.*
- `proxy`/`strict` (default when a list is set): the box joins an `--internal` network with no
  route out; its only peer is a tinyproxy sidecar that is also on the bridge and forwards to
  allow-listed hosts. `HTTP(S)_PROXY` points clients at it; `NO_PROXY` covers loopback and the
  proxy. Only proxy-aware HTTP(S) leaves the box; raw TCP, SSH and DNS to the internet do not.
  Each request is checked against the hostname. *Verified in day-to-day use.*
- `dns-pin` (legacy, opt-in): bridge network, `--dns 0.0.0.0`, `--add-host` for each allowed host.
  Blocks lookups of other names, **not** traffic to IP literals. Not real enforcement.
- If the proxy sidecar dies, the box loses all egress (fail-closed).

**Environment.** Only the configured env is passed (`-e`); the host env is not inherited.

**Not limited.** varda passes no `--cap-drop`, `--security-opt no-new-privileges`, `--read-only`,
`--pids-limit` or `--user`: the box runs as the image's `USER` (`Dockerfile.agents` uses a non-root
`agent`) with docker's default capabilities. A kernel or runtime escape reaches the daemon's host:
on macOS that is the Colima/Docker Desktop VM, which by default mounts `$HOME` (narrow its
`mounts:` to what the projects need). SSH-agent forwarding, when enabled, lets the agent authenticate as you to any
reachable SSH host (no key leaves the host, but signing is available).

## `microsandbox`

**Files.** Project bound read-write with `--mount-dir`; extra mounts as for docker; guest `/tmp`
can be a sized tmpfs (`tmpfs`); credentials staged with pre-boot `--copy-file` (read-only).
*Verified: a `:ro` mount blocks writes and host `~/.aws` is not visible.*

**Network.** Empty list and no broker: `--net-default deny`. Otherwise `--net-default-egress deny`
and one `--net-rule allow@<host>` per entry; when the varda MCP broker is active, also
`allow@host` (msb's group for the host machine); the rule names no port, so every host port the
host exposes is reachable from the guest, not just the broker's. Rules are hostname or CIDR, enforced by `msb`. *Unverified: whether a root
process in the guest can alter that enforcement; check against msb's design before relying on it
for hostile code.*

**Boundary.** Own kernel: a guest kernel exploit stays in the VM. This is the strongest primitive.

## `processcontainer` (Windows MXC BaseContainer)

`src/sandbox/processcontainer.rs`, `src/sandbox/egress_proxy.rs`. varda requests Tier 1 and sets
`fallback.allowDaclMutation: false`, so MXC fails rather than fall back to Tier 3 (which edits host
ACLs and needs admin preparation).

**Files.** MXC gets two lists; everything else is refused by the container's ACLs.
- Read-write: project root, per-task session home (becomes `USERPROFILE`, `HOME`, `APPDATA`,
  `LOCALAPPDATA`, `TEMP`), `:rw` mounts.
- Read-only: the staged dir, the agent program's dir, Node's dir, varda's own dir (PTY host),
  configured mounts.
- Implicit: `C:\Windows` and `Program Files` are readable/executable, as for any AppContainer-style
  process; without that nothing runs.
- Denied: the real `%USERPROFILE%` (`Documents` is not visible), `lstat C:\`, writes to read-only
  paths. *Verified, spike #1170.* Credential Manager is unreachable; named pipes fail with `EPERM`.
  *Verified, spikes #1170/#1181.* Registry access: *unverified.*
- Paths are not remapped (`source:target` mounts are refused).

**Network.** MXC filters by CIDR only, so varda uses MXC "Model 2": `egress.default: deny`, and the
only reachable endpoint is a loopback HTTP proxy run by the `varda sandbox-exec` relay on the host
(MXC sets `HTTP(S)_PROXY` and the WinHTTP proxy). That proxy applies the allow-list above and logs
every decision to `egress-proxy.log`. Name resolution happens host-side. Empty list: no proxy, fully
offline. *Verified 2026-10-16 on a Windows 365 Cloud PC: allow-listed host reachable, other hosts denied,
direct egress blocked, a second loopback service unreachable.* Requires `mxc-sdk >= 1.0.1` and
build 26100.9711 / 26200.9711 or later (`varda doctor` checks both). *Unverified:* whether the
proxy refuses an allowed hostname that resolves to a private or loopback address.

**Environment.** Built from scratch: a few Windows basics (`SYSTEMROOT`, `WINDIR`, `PATHEXT`,
`COMSPEC`, processor vars), the sandbox and agent env, env-target credentials, then the profile
variables pinned to the session home. File-target credentials and SSH-agent forwarding are not
supported.

**Not limited.**
- **Desktop.** `ui.disable: false`, because Node and PowerShell fail Win32k initialisation
  (0xC0000142) without UI access, so the job-object UI limits are off and the agent runs on your
  interactive desktop. Measured on a Windows 365 Cloud PC (2026-11-10) with a
  clipboard/UI probe run first on the host as a baseline, then in the container:

  | Capability | Container |
  |---|---|
  | read the clipboard | blocked (verified) |
  | write the clipboard | blocked (verified) |
  | see other windows' titles | blocked (verified) |
  | rename another process's window | blocked (verified) |
  | capture the screen | **open** (verified: the capture contains a marker window the driver paints on the user's desktop) |
  | synthetic keystrokes reaching another window | blocked (verified: `SendInput` accepts the events, none arrive) |

  The restricted token cuts the clipboard and cross-process window access, and Windows drops the
  container's synthetic input before it reaches a normal-integrity window (the probe types into a
  focused, topmost window of the user's; the host baseline receives the text, the container's does
  not), so the agent cannot type into your terminal. It does not stop a screen capture: anything
  visible on the desktop, in any application, is readable by the agent. Until varda can run the
  container on a separate desktop, keep sensitive windows closed or minimised while a session runs.
  Probe: a host-baseline-then-container clipboard/UI script (`ui-probe.ps1` with its driver).
- **Resources.** No CPU, memory or process caps are set, and `timeout: 0` (MXC supports job-object
  caps; spike #1170 confirmed they work without admin).
- **Same kernel and session.** A Windows privilege escalation from the restricted token reaches
  the user's session.

## Keeping this page true

- Change a flag in `wrap()`, a denylist, or an egress mode → update the matching section and the
  table in the same commit.
- Promote a claim from *unverified* to *verified* only with a reproducible test (a probe script,
  a spike runbook, or an integration test), and link it.
