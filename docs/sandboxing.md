# Sandboxing

Sandbox providers, credentials, per-folder overrides, and the isolation invariants.

## Sandbox providers

Varda can run an agent inside an isolated sandbox instead of directly on your machine:

```toml
[defaults]
sandbox = "local"          # default provider for routes that don't override it

[[routes]]
glob = "/work/project/**"
agents = ["codex"]
sandbox = "docker"         # per-route override
env = { GCLOUD_PROJECT = "healthy-silo-31898" }  # per-project static, non-secret env

[sandboxes.docker]
image = "varda:latest"        # image/rootfs, or use `build` below instead
primitive = "docker"          # isolation kind; defaults to "docker" when omitted
egress = ["api.example.com"]  # optional egress allow-list (default-deny)
# egress_mode = "strict"      # docker: strict/proxy = forward-proxy sidecar; "dns-pin" = legacy
# egress_proxy_image = "vimagick/tinyproxy"  # override the forward-proxy image (proxy mode)
mounts = ["/opt/cache"]       # optional extra read-only host mounts
env = { TOOLCHAIN_HOME = "/opt/toolchain" }  # image-intrinsic static env
# memory = "4g"                # optional memory ceiling, docker `--memory` grammar
# cpus = "2"                   # optional CPU ceiling, docker `--cpus` grammar

[sandboxes.rustvm]
build = "./testdata/Dockerfile.rust"  # build the image from a Dockerfile at prepare()

[sandboxes.devc]
image_from = "devcontainer"   # take the image from the project's .devcontainer/
```

The effective provider for a route resolves as `route.sandbox` → `defaults.sandbox` → `"local"`. All keys are optional, so existing `config.toml` files parse and re-serialize unchanged. These keys are orthogonal to Codex's own `--sandbox workspace-write` CLI flag in the agent `args`.

Two knobs are deliberately separate. **`image`/`build`** decides *what tools are installed* (the OCI image or rootfs); **`primitive`** decides *what kind of boundary* runs it. The same image can run under `docker` (shared kernel) or an own-kernel microVM, so nested projects can pick both independently.

- **`image`** names a pre-existing image tag/reference.
- **`build`** points at a Dockerfile; the docker provider builds it at run time and uses the resulting content-addressed tag (an unchanged Dockerfile reuses the cached image). When both are set, `build` wins.
- **`image_from = "devcontainer"`** sources the image from the project's own [`.devcontainer/devcontainer.json`](https://containers.dev/) (falling back to a top-level `.devcontainer.json`) so Varda doesn't duplicate the environment definition. At `prepare()` it reads the devcontainer's `image` (used verbatim) or its `build.dockerfile` + `build.context` (built via docker, content-addressed like `build`), resolved relative to the `.devcontainer/` dir. The JSONC form (comments, trailing commas) is tolerated. `image_from` takes precedence over `image`/`build` when several are set.
  - **Isolation invariant (image source only).** A `devcontainer.json` may declare host `mounts` (`~/.aws`/SSH), docker-socket forwarding, access-widening `runArgs`, or lifecycle hooks (`postCreateCommand`, …). Varda takes **only the image/build** and **nothing else** — those fields are not even deserialized, so they can never leak into the run. Varda keeps sole control of mounts, egress, and credentials (the same project-only-mount, default-deny-egress, no-`$HOME` hardening as any other docker sandbox). A devcontainer that tries to bind-mount host `$HOME` does **not** make the host `$HOME`/`~/.aws` visible in the container.
- **`primitive`** is one of `"docker"` (default), `"local"`, or `"microsandbox"`. `microsandbox` is an own-kernel microVM runtime backed by the `msb` CLI (see below).
- **`memory`/`cpus`** are opt-in resource ceilings, absent by default (⇒ unbounded, exactly today's behavior — no surprise ceiling breaks a build or a large diff that works today). Both use **docker's own grammar**: `memory` is a size string (`"4g"`, `"512m"`); `cpus` is a decimal core count (`"2"`, `"1.5"`). Under `docker` they are emitted verbatim as `--memory`/`--cpus`, plus a `--memory-swap` equal to `--memory` (without it docker grants swap equal to the limit again, silently doubling the effective footprint) — so an oversized box is OOMKilled **alone**, as a cgroup kill docker reports on that one container, rather than the host kernel picking an unrelated victim by score. Under `microsandbox` the same two keys are translated onto `msb run`'s own `--memory` (MB integer) / `--cpus` (integer core count) flags rather than forking the config surface; a value that fails to parse against docker's grammar is dropped with a warning instead of failing the run. `local` does not support ceilings today; setting `memory`/`cpus` there has no effect.

**`local`** (the default provider name) runs the agent exactly as before — no isolation. Setting `primitive = "local"` on a named sandbox does the same, even if an `image` is present.

**`docker`** wraps the agent invocation in `docker run --rm --init -i` and executes it inside the resolved `image` (or the image built from `build`). The container's environment is built solely from the agent's configured `env` (passed as `-e K=V`); the host environment is not inherited.

- **Project-only mounts, plus opt-in context mounts.** Only the task's **project directory** is bind-mounted by default, at the same absolute path, so host secrets outside the project (e.g. `~/.aws`) are not reachable. Extra mounts may be declared at two trusted origins that **merge** (union, de-duplicated by target): `[sandboxes.X].mounts` (image-intrinsic, same for every project using that image) and `Route.mounts` (project context, e.g. a route for `**/dev/AsianDevBank/**` also mounting `~/dev/brain/AsianDevBank:ro`). Extra mounts are **read-only by default**.
- **Static env maps.** Non-secret static values may be declared at `[agents.X].env`, `[sandboxes.X].env`, `[[routes]].env`, and inline `.varda` `[sandbox].env`. They merge as `agent.env` → `sandbox.env` → `route.env` → `.varda` env, so the more-specific origin wins. Values support the same `{project}` and `~` expansion as agent env and mount paths. Use this for project constants such as `GCLOUD_PROJECT`; secrets and tokens belong in `auth_token_env`/credential injection, not static env.
- **Mount grammar (`source:target:mode`, docker-style).** `SOURCE` (target = same absolute path, `:ro`) · `SOURCE:ro|:w` · `SOURCE:TARGET` (absolute TARGET, `:ro`) · `SOURCE:TARGET:ro|:w`; a TOML table form `{ source, target, mode }` is also accepted. `~` and `{project}` expand; relative sources resolve against the project root.
- **Host mount visibility (VM-backed docker).** With a VM-backed daemon (Colima/Lima/Docker Desktop) only paths the VM actually shares are visible; a bind-mount whose **source is outside the VM's shared tree binds as an empty stub** (docker creates the mount point inside the VM). Keep mount sources — including the project and any context dirs — under a VM-mounted root (e.g. Colima's configured mount). See "Resume-capture" for how this affects the session store.
- **Egress modes are explicit.** With no `egress` hosts the sandbox gets `--network none` and is fully offline; this is strict/offline for every provider. For a non-empty Docker allow-list, `egress_mode` picks the enforcement:
  - **`strict` (default) / `proxy` — allow-listing forward-proxy sidecar.** Varda stands up a per-session **internal** docker network (no route to the internet) plus a small forward-proxy container that is dual-homed onto that internal network *and* the bridge. The sandbox joins only the internal network and receives `HTTP_PROXY`/`HTTPS_PROXY` pointing at the proxy (`http://egress-proxy:8888`); the proxy default-denies and forwards HTTP(S) CONNECT to the allow-listed hosts **only**. This is **real enforcement** — a denied host is genuinely unroutable, not just DNS-broken — and it works with apps that do their own DNS resolution (claude-code, codex), which the DNS-pin mode breaks. It needs no `NET_ADMIN`. Trade-off: it covers **proxy-aware HTTP(S)** traffic (the agent + git-over-https + npm/pip/cargo registries); raw non-proxy TCP is not forwarded. The proxy image is `vimagick/tinyproxy` by default and is overridable per sandbox via `egress_proxy_image` (any image running a tinyproxy-compatible proxy that reads `/etc/varda-proxy/tinyproxy.conf`).
  - **`dns-pin` — legacy name-pin (compat).** Varda attaches the container to the bridge network, disables ambient DNS (`--dns 0.0.0.0`), and pins only the allow-listed hostnames via `--add-host`. It blocks non-allow-listed **hostnames**, but an agent that already knows an IP can still make direct-IP connections, and apps doing their own DNS (bypassing `/etc/hosts`) break. Opt into it explicitly only for the legacy worker behavior; it is **not** microsandbox-equivalent firewalling.
- **Resident egress is stricter than worker egress.** `varda orchestrate` validates the long-lived resident against an agent-specific exact host inventory and a strict egress provider: Claude may use `api.anthropic.com` and `platform.claude.com` (the latter is a hard startup-connectivity requirement from Claude Code v2.x on); Codex/OpenAI may use `api.openai.com`, `chatgpt.com`, and `auth.openai.com`; Copilot resident mode currently fails closed until exact non-push Copilot auth/API endpoints are known. Do not add blanket `github.com` to resident egress. Ordinary worker sandboxes may still opt into broader route/user-approved egress where the workflow explicitly permits it.
- **Resume-capture without exposing `$HOME` (per-session volume + `docker cp`).** The container's `HOME` is a dedicated **per-session docker named volume** (not a host bind mount) — the host's real `$HOME` is never mounted, so credentials stay out. The agent writes its session store (claude/copilot/codex) under that HOME; after the run Varda `docker cp`s the store out of the container to a host directory (`~/.varda/sessions/{session_id}`) and reads it back to produce a working `resume_command`. Because the volume lives in daemon storage and `docker cp` streams through the daemon to the host, this works on **any** backend — including a VM-backed daemon whose share excludes `~/.varda` (e.g. a Colima profile mounting only `~/dev`), where a host bind of the session dir would silently bind an empty in-VM stub. The container drops `--rm` so it outlives its process long enough for the copy, then teardown removes both container and volume.
- **Fail-loud mounts.** A declared bind-mount whose host source does not exist is rejected with a clear error rather than silently mounting an empty stub (on a VM-backed daemon docker would otherwise create an empty in-VM mount point that *looks* successful).

**`microsandbox`** shells to the `msb` CLI (install with the microsandbox project; expects `msb` on `PATH`) and runs the agent inside an **own-kernel microVM** — a stronger inward boundary than docker's shared kernel, plus Windows coverage. It mirrors the docker provider: the same `image`/`build` inputs (a `build` Dockerfile is built via docker into a tag `msb` runs), the same project-only + opt-in merged mounts (`msb --mount HOST:GUEST`, read-only by default), the same resume-capture model (the guest `HOME` lives in VM storage and is `msb cp`-ed out to `~/.varda/sessions/{session_id}` after the run, so the host `$HOME`/credentials are never exposed), and default-deny egress (fully offline with no `egress`; `egress` hosts become per-host `msb` net allow-rules — enforced in-guest, so hostnames/CIDRs are passed directly rather than pre-resolved to IPs as docker requires). Because msb 0.6.x has no env-file option, Varda stages `env`-target credential values in a private read-only file with `--copy-file` and imports them inside the guest; secret values never appear in the ps-visible `msb run` argv. Ordinary non-secret environment settings continue to use `--env`. The keys never enter the VM, and an OCI image can bake in the agent CLI (e.g. the copilot CLI for the Windows path). *The `msb` argv spellings are centralized in `MicrosandboxSession::wrap`/`extract_session_store`; confirm them against your installed `msb --help` — see the M4 task notes on live verification.*

### Interactive sandbox (real agents): TTY, prompt staging, injected auth, and the docker lifecycle

`--interactive` runs now put the **real coding agents** (`claude`, `codex`, `copilot`) — not just a bare shell — inside `docker` and `microsandbox`, not just `local`. Each default agent ships an `interactive_command`/`interactive_args` that launches the agent through a login shell reading the staged prompt (`sh -c '<agent> "$(cat $VARDA_PROMPT_FILE)" …'`); when the resolved sandbox is not `local`, Varda attaches **your terminal** to that agent *inside the box*. The project is mounted, but `~/.aws`/`~/.ssh`/`~/.claude`/etc. stay invisible (the same isolation as a batch run), and teardown removes the container/microVM and its volume on every exit path.

- **The agent authenticates via injected identity, not a creds-dir mount.** The interactive path reuses the same `SandboxIdentity` channels as a batch run (M11/M11-ext): a scoped, host-minted token injected as an in-box env var (or staged as a read-only `0o400` file), the read-only git identity (`GIT_AUTHOR_*`/`GIT_COMMITTER_*`), and — when `forward_ssh_agent` is set and a live `$SSH_AUTH_SOCK` exists — the host **SSH agent socket forwarded** as a bind (`… :/ssh-agent`) with the in-guest `SSH_AUTH_SOCK` pointing at it, so `git push` signs on the host and **no private key ever enters the box**. `~/.aws`/`~/.ssh` and every credential dir remain unmounted; identity injection is mode-agnostic (the batch and interactive argv carry the exact same channels).
- **A real terminal is required.** A sandboxed interactive launch needs a TTY on stdin (`docker -it` / `msb -t` fail without one). Varda checks `stdin` up front and fails clearly if you pipe input or run headless — use a batch run (no `--interactive`) or `sandbox = "local"` in that case.
- **The prompt is staged *into* the guest.** The task prompt is copied to `/home/agent/.varda-prompt.txt` inside the box and exposed as `$VARDA_PROMPT_FILE`, so the in-guest agent can read the task even though the host temp file is not visible in the guest. (Under `local`, `$VARDA_PROMPT_FILE` points at a host temp, exactly as before.)
- **Docker interactive is a different lifecycle, not just `-it`.** Because the container `HOME` is a per-session named volume and a host bind of `~/.varda` hits the VM-visibility trap, Varda cannot `docker run` an interactive session directly. Instead it:
  1. `docker create … -it …` — create the container (do **not** run it),
  2. `docker cp` the staged prompt into it,
  3. `docker start -ai <container>` — attach *your* TTY,
  4. `docker cp <container>:/home/agent/. <host session store>` — extract the session store after you exit,
  5. `docker rm -f` the container and `docker volume rm -f` its volume.

  `microsandbox` needs no such dance: it stages the prompt with native pre-boot copy flags and runs the VM command with a TTY. Session capture and the `resume_command` are produced after the run by copying the guest HOME back to the host.
- **Ctrl-C** under `-it` propagates to the guest process; the `SessionTeardownGuard` still fires on the way out, so no `varda-sbx-*` container or volume leaks.
- **The interpretation pass stays local.** After the interactive session ends, Varda's post-session interpretation pass only reads the host session log to produce the recap and the captured `resume_command` (no untrusted exec), so it runs **un-sandboxed** on the host. An optional `interpreter_agent` on the agent config selects which agent runs that pass; when unset it defaults to the same agent that drove the session (a real agent re-reads its own transcript; a bare `sh` shell that can't emit a Varda recap should point `interpreter_agent` at a real agent).

> Resuming an interactive session under a sandbox is not yet supported (the fresh-shell launch is); resume runs remain `local`-only.

### Per-folder `.varda` (untrusted origin) and the hardening floor

A folder can commit its own sandbox choice in a **`.varda`** file. When resolving the sandbox for a task, Varda walks **up** from the task's project/target path to the routing root and uses the **nearest `.varda`**. Precedence:

```
task-pin (task add --sandbox)  →  nearest .varda  →  central route (glob)  →  defaults.sandbox  →  "local"
```

A task-pinned sandbox (`varda task add --sandbox <NAME>`, persisted as the
`sandbox:` task-frontmatter field) is a trusted operator origin, so it wins over
even the nearest `.varda` and is not subject to the `.varda` hardening floor.

`.varda` is TOML in one of two forms:

```toml
# Reference form — select a central [sandboxes.X]:
sandbox = "rust"
```

```toml
# Inline form — a self-contained sandbox:
[sandbox]
image = "rust:latest"
primitive = "docker"
mounts = ["ctx:/ctx"]        # relative to the .varda dir
egress = ["crates.io"]
env = { GCLOUD_PROJECT = "healthy-silo-31898" }
```

Inline `mounts` join the docker/microsandbox mount merge as a **third origin** (`MountOrigin::Varda`), unioned with the trusted `Sandbox`- and `Route`-origin mounts.

**Hardening floor (security).** Central `config.toml` (routes and `[sandboxes]`) stays **trusted**; a `.varda` is committed alongside possibly-untrusted code, so it is **clamped** — the floor applies **only** to the `.varda` origin (trust-by-origin: the same mount from a `Route` is allowed):

- **No escaping the box.** A `.varda` cannot select `primitive = "local"` unless `defaults.allow_local_varda = true` (default false).
- **In-tree, read-only mounts.** A `.varda` mount SOURCE must resolve **inside the project root** (out-of-tree / host paths are rejected), and is forced `:ro` unless `defaults.allow_varda_writable_mounts = true`.
- **Safe target.** A mount TARGET may not be `/`, a system dir (`/etc`, `/usr`, …), nor collide with / shadow the project mount.
- **Egress ceiling.** If `defaults.egress_ceiling` is set, a `.varda` may not widen egress beyond it.
- **Env key floor.** A `.varda` env map cannot set reserved process/control keys (`PATH`, `HOME`, `LD_PRELOAD`, `LD_LIBRARY_PATH`, `DYLD_*`, `VARDA_*`, `SSH_AUTH_SOCK`) and cannot override trusted agent/route env or a credential injection target.
- On any violation Varda **refuses to run** with an error naming the offending `.varda` and key.

**Credential-directory denylist (ALL origins, trusted config included).** Any mount whose SOURCE resolves (symlinks followed) into a known secret/identity store is refused regardless of origin: `~/.claude`, `~/.codex`, `~/.copilot`, `~/.aws`, `~/.azure`, `~/.terraform.d`, `~/.ssh`, `~/.config/gcloud`, `~/.config/fnox`, `~/.gnupg`, `~/.kube`, `~/.docker`, `~/.netrc`, `~/.git-credentials`. These carry live LLM tokens **and** cross-project history; mounting one defeats the sandbox and leaks other clients' data.

### Passing identity & auth into the box (three channels)

A sandboxed agent must be authenticated to run the LLM **and** "know who the user is" — but it must get there **without** mounting `~/.claude`/`~/.codex`/`~/.copilot`/`~/.aws`/`~/.ssh` (those carry live tokens + cross-project history; see the credential denylist above). Varda forwards identity through **three separable, opt-in channels**. Guiding principle: **share the minimum**.

**1. Auth-token injection (channel 1).** Instead of mounting a credential dir, inject a **dedicated, scoped, rotatable sandbox token** as an env var so the agent boots already signed in. The token is resolved from a **host env var / secret store at run time — never a raw secret in the repo or the task**. Prefer a purpose-provisioned token (own spend limit, lower privilege) over the user's primary credential.

```toml
[agents.claude]
# Name of a HOST env var holding the scoped sandbox token (resolved from the
# environment / a secret store like fnox — NOT a literal secret here).
auth_token_env = "CLAUDE_SANDBOX_TOKEN"
# In-box env var the agent reads its token from (defaults to auth_token_env).
auth_token_target = "ANTHROPIC_API_KEY"
```

At `prepare`, Varda reads `$CLAUDE_SANDBOX_TOKEN` on the host and re-exports its value into the box as `-e ANTHROPIC_API_KEY=…`. If the host var is unset/empty the run still boots (with a warning) — it just isn't authenticated. The stronger form — the agent never holds the key, LLM calls proxied through a broker — is the **M8 capability-broker** pattern (out of scope here).

**Multiple credentials per agent (`[[agents.X.credentials]]`).** The single `auth_token_env` pair is **one-entry sugar** over a general credential list. Each entry names exactly one **source** (where the scoped value is minted, on the host) and exactly one **target** (how it reaches the box). None of them ever mounts a credential dir — only the resolved, minimal value crosses the boundary.

| | key | meaning |
|---|---|---|
| **source** (one) | `from_env = "NAME"` | value of a HOST env var |
| | `from_secret = "name"` | a named secret from the host store (`fnox get name`) |
| | `from_fnox = "name"` | explicit alias of `from_secret` — same `fnox get name` resolution; prefer it when standardizing on fnox as the store |
| | `command = "…"` | run on the HOST at `prepare`; stdout (newline-trimmed) is the value — for host-minted, least-privilege, short-lived tokens |
| **target** (one) | `env = "IN_BOX_VAR"` | scoped `-e IN_BOX_VAR=…` (default) |
| | `file = "/guest/abs/path"` | the minimal value is staged as a **read-only** (`0o400`) file in the guest — in **both** batch and interactive runs (docker `cp` / msb `--copy-file`), and both the host temp and the guest copy are removed on teardown |

A `from_env`/`from_secret` source that is unset is skipped (the box still boots unauthenticated); a `command` that errors or prints nothing **fails the run loudly** — a broken mint must never silently degrade to an unauthenticated session. `refresh_seconds` is accepted for forward-compat but the value is currently minted **once** at `prepare` (periodic re-mint is a follow-up). Sources belong in the **trusted central `config.toml`** only; a `.varda` may reference a secret *name* (`from_secret`) but never a raw value or a `command`.

Recipes (each injects a scoped value — **no** `~/.config/gcloud`, `~/.azure`, `~/.aws`, `~/.terraform.d` mount):

```toml
# GCP deploy with a host-minted, impersonated, short-lived access token — no local Docker.
# Use with `gcloud run deploy --source` inside the box.
[[agents.claude.credentials]]
command = "gcloud auth print-access-token --impersonate-service-account=deployer@PROJECT.iam"
env = "CLOUDSDK_AUTH_ACCESS_TOKEN"
[[agents.claude.credentials]]
command = "gcloud auth print-access-token --impersonate-service-account=deployer@PROJECT.iam"
env = "GOOGLE_OAUTH_ACCESS_TOKEN"

# Terraform Cloud API token from the host secret store.
[[agents.claude.credentials]]
from_secret = "tfc-token"
env = "TF_TOKEN_app_terraform_io"

# Azure DevOps PAT from the host secret store.
[[agents.claude.credentials]]
from_secret = "azdo-pat"
env = "AZURE_DEVOPS_EXT_PAT"

# Azure CLI — service principal via env (NOT a ~/.azure mount)…
[[agents.claude.credentials]]
from_secret = "azure-client-id"
env = "AZURE_CLIENT_ID"
[[agents.claude.credentials]]
from_secret = "azure-client-secret"
env = "AZURE_CLIENT_SECRET"
[[agents.claude.credentials]]
from_secret = "azure-tenant-id"
env = "AZURE_TENANT_ID"
# …or a host-minted access token instead of the SP secret:
[[agents.claude.credentials]]
command = "az account get-access-token --query accessToken -o tsv"
env = "AZURE_TOKEN"
```

**fnox-bound static env vars.** A plain static env var — in `[agents.X].env`, `[sandboxes.X].env`, or a `[[routes]].env` — can be **bound to a fnox secret** instead of carrying a literal value, using the whole-value sentinel `"${fnox:NAME}"`. fnox lives on the **exterior**, side-by-side with Varda: at `prepare` Varda resolves the secret on the host (`fnox get NAME`) and injects **only the resolved value** into the box. The agent/sandbox never contacts fnox, never sees the sentinel, and `~/.config/fnox` is never mounted — same isolation model as credential injection, extended to the static env maps.

```toml
# The value is resolved from fnox on the host; the box sees only NPM_TOKEN=<value>.
[sandboxes.build.env]
NPM_TOKEN = "${fnox:npm-publish-token}"

[[routes]]
glob = "infra/**"
env = { HCLOUD_TOKEN = "${fnox:hcloud-token}" }
```

Only a **whole-value** `"${fnox:NAME}"` is a binding (never a substring of a larger literal), so the resolved secret is never embedded in — nor logged as part of — a bigger string. A missing/failed/empty fnox resolution **fails the run loudly** (redacted: only the key and secret *name* are surfaced, never the value). A fnox binding declared by an **untrusted origin** is **refused** — repo-committed config must not be able to bind an arbitrary host secret and exfiltrate it through the agent's env; use the trusted central `config.toml` for fnox-bound env. Untrusted origins are the per-folder `.varda` (see below) **and** any `[agents.X]`/`[sandboxes.X]`/`[[routes]]` merged in from an `include`d fragment rather than declared directly in the central config — a shared bundle is exactly as untrusted for this purpose as a repo-committed `.varda`, and the same refusal applies to that fragment's own `credentials`/`auth_token_env` (see above).

**2. Git identity via SSH-agent forwarding (channel 2).** Forward the host **SSH agent socket** so `git push` signs/authenticates on the host and **private keys never enter the box** (`ls ~/.ssh` in-guest stays empty). The read-only git identity is forwarded as `GIT_AUTHOR_*`/`GIT_COMMITTER_*` env so commits are attributed correctly without mounting `~/.gitconfig`.

```toml
[defaults]
forward_ssh_agent = true            # bind $SSH_AUTH_SOCK → /ssh-agent, set SSH_AUTH_SOCK
git_user_name  = "Ada Lovelace"
git_user_email = "ada@example.com"
```

(docker binds the socket file directly; on the own-kernel microVM the socket bind is best-effort at directory granularity.)

**3. Curated identity/context (channel 3).** `defaults.identity_context` mounts specific, **read-only FILES** so the agent learns "who the user is" without secrets:

```toml
[defaults]
identity_context = ["~/.claude/CLAUDE.md:/root/CLAUDE.md:ro", "~/profile.md:/root/profile.md:ro"]
```

Files only (never a whole dotdir, never `projects/` transcripts); the credential-**filename** denylist still applies, so a `.credentials.json` can never sneak in even when the file lives inside an otherwise-denylisted dir. All three channels are off by default — nothing is forwarded unless you opt in.

Current limitations:

- **Resume is `local`-only.** Fresh interactive sessions run under `docker`/`microsandbox` (the real `claude`/`codex`/`copilot` agents attach to your TTY inside the box — see [Interactive sandbox](#interactive-sandbox-real-agents-tty-prompt-staging-injected-auth-and-the-docker-lifecycle)). **Resuming** an interactive session under a non-`local` sandbox still returns a clear error and remains `local`-only.

## Per-folder `.varda` overrides

A project subtree can pin its own sandbox by placing a `.varda` file in (or above) the task's project directory. At launch Varda walks up from the project path to the git root and uses the **nearest** `.varda`, with precedence:

```
task-pin (task add --sandbox)  →  nearest .varda  →  central route sandbox  →  defaults.sandbox  →  "local"
```

A `.varda` is TOML in one of two forms:

```toml
# reference a central [sandboxes.X]
sandbox = "rustvm"
```
```toml
# or inline a self-contained sandbox
[sandbox]
image     = "rust:1.95"
primitive = "docker"
mounts    = ["../docs:/docs:ro"]
env       = { GCLOUD_PROJECT = "healthy-silo-31898" }
```

Because a `.varda` is repo-committed (attacker-influenceable on untrusted code), the inline form is clamped by a **hardening floor**: it cannot select `primitive = "local"` (unless `defaults.allow_local_varda`), cannot mount sources outside the project or any credential directory (see the invariants below), cannot target `/` or a system dir or shadow the project mount, cannot widen egress beyond `defaults.egress_ceiling`, and cannot set reserved env keys or override trusted agent/route env or credential injection targets. A floor violation refuses the run with a clear error. Central `config.toml` mounts and env are trusted and skip the `.varda` floor — except the credential denylist, which applies to every mount origin.

## Isolation invariants (never violate)

These rules are what make the sandbox meaningful; breaking any turns it into theatre:

1. **Never mount the docker socket** (`/var/run/docker.sock`) into an agent container — it is equivalent to host root.
2. **Never mount `~/.varda`** (the control-plane root: tasks, config, routes, all sessions) or install the `varda` binary into an agent container — it hands the agent the control plane. A single per-session scratch dir is not the root; the docker/microVM providers use a per-session volume + `cp`, not a host bind of `~/.varda`.
3. **No `--privileged` / no docker-in-docker** for agent containers. Sub-sandboxes are siblings spawned by the host, never nested.
4. **Never mount credential/identity directories** (`~/.claude`, `~/.codex`, `~/.copilot`, `~/.aws`, `~/.ssh`, `~/.config/gcloud`, `~/.config/fnox`, …) to "authenticate the agent" — they hold live tokens and cross-project history. Pass identity via an injected scoped token + SSH-agent forwarding + a curated read-only profile file instead. Enforced by `CREDENTIAL_DENYLIST` / `check_credential_denylist` across **all** mount origins.
5. Extra mounts default to read-only; the host `$HOME` is never mounted.

### Why these are defence-in-depth, not a guarantee

Trail of Bits, [*VMs won't contain cyber-capable agents*](https://blog.trailofbits.com/2026/08/26/vms-wont-contain-cyber-capable-agents/)
(August 2026), reports an agent autonomously finding three 0-days and escaping a VM three
times — via a host kernel bug, via **libslirp**, and via a chain against **QEMU/KVM**. Its
conclusion: "you can no longer assume a mere VM will contain a sufficiently advanced AI agent."

Two of those three vectors do not port to this codebase, and it is worth being precise about
why rather than claiming immunity. Varda's `microsandbox` primitive runs on **libkrun**, a
minimal VMM with a small device model, and microsandbox credits **smoltcp** — a Rust userspace
TCP/IP stack — rather than libslirp. So the specific libslirp bug and the QEMU device-model
chain describe a stack varda does not run. The *class* of risk transfers intact: smoltcp and
libkrun are a different attack surface, not an absent one, and libkrun sits on KVM (Linux) or
Hypervisor.framework (macOS), so a hypervisor-level escape remains conceivable.

The article's own recommendations line up with the invariants above and with choices already
made elsewhere in varda: least privilege on network (allow-listed egress per sandbox, no open
internet), pristine per-run environments (a fresh box and session store per run), and time
limits (`max_seconds`, the idle watchdog). Its one recommendation varda does **not** yet
implement is active monitoring of a running box.

Read the invariants in that light. They are not a proof of containment; they remove the cheap
escapes — the mounted docker socket, the handed-over control plane, the live credential
directory — so that escaping costs an actual exploit chain rather than a misconfiguration.
That is the property being defended, and it is why "the sandbox holds" is never a reason to
relax one of them.

[← back to the README](../README.md)
