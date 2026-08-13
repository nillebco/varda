---
id: 521
project: /Users/nilleb/dev/nillebco/varda
assignee: claude
---

# Add codex to orchestrate on ChatGPT subscription (staged auth.json, no API spend)

# Add codex to `varda orchestrate` on the ChatGPT SUBSCRIPTION (staged auth.json, no API spend)

Enable codex as a resident/worker agent inside the orchestrate sandbox, authenticated via the user's **ChatGPT subscription** — NOT an OpenAI API key (which bills pay-per-token). Unlocks the claude↔codex cross-review inside the self-hosting orchestrator, on subscriptions only.

## Context / prior state
- The `varda-agents:latest` image already has `codex-cli` + `socat`.
- claude is already wired as `claude-resident` (subscription via `CLAUDE_CODE_OAUTH_TOKEN` from `claude setup-token`) and validated end-to-end to the token line.
- codex uses ChatGPT OAuth: `~/.codex/auth.json` (JWT `id_token` + long-lived `refresh_token`, `OPENAI_API_KEY: null`). There is NO scoped-token middle ground for subscription codex — subscription auth = the OAuth credential itself.

## Do
1. **`[agents.codex-resident]` agent** in `~/.varda/config.toml`: mirror the codex agent (`command="codex"`, interactive form reading `$VARDA_PROMPT_FILE`), plus:
   - **Auth via M11 file-target credential**: stage `~/.codex/auth.json` into the box at the guest path codex reads (`~/.codex/auth.json` / `$CODEX_HOME/auth.json`), read-only `0o400`. Use `from_secret`/`from_env` pointing at the host file, or a `file` target that copies the host auth.json. Confirm the guest HOME/`CODEX_HOME` is where codex looks.
   - **MCP wiring**: codex registers MCP servers in its OWN config, not `--mcp-config`. Add `[mcp_servers.varda]` (command `socat`, args `["-","UNIX-CONNECT:$VARDA_MCP_SOCKET"]`) to a `~/.codex/config.toml` staged into the box (or codex's flag if one exists — verify `codex --help` / `codex mcp`). This exposes `spawn_subtask`/`await_subtask`/`subtask_result` to codex.
2. **Egress**: ensure the resident egress allowlist includes the hosts codex needs — `api.openai.com`, and likely `chatgpt.com` + `auth.openai.com` for the OAuth refresh (the `id_token` expires ~1h; codex refreshes using the refresh_token, which needs those hosts). Use the configurable `resident_egress_allowlist` once merged.
3. **Route**: option (a) resident=codex-resident; option (b) KEEP resident=claude and let it spawn **codex workers** (claude↔codex cross-review). Wire whichever the operator wants; (b) is the nicer end state.

## Verify (the real unknowns)
- Does codex run **headless in a fresh-HOME container** from a staged `auth.json` (does it accept the token, and can it refresh the expired `id_token` via the allowed egress hosts)? This is the make-or-break — test it in isolation first: `docker run` the image with the staged auth + egress and a trivial `codex exec` prompt.
- Does the socat bridge expose the spawn tools to codex (codex actually lists/calls `spawn_subtask`)?

## Security notes (record in the setup + WORKFLOW/README)
- Staging `auth.json` puts the ChatGPT **refresh_token** in the box — powerful + long-lived. It is BOUNDED here by the box's containment: egress restricted to OpenAI/Anthropic hosts + no push, so a compromised resident can burn ChatGPT quota but **cannot exfiltrate the token to an attacker-controlled host**. Residual risk = quota abuse, not credential theft.
- **Time-boxing is weak** for this path (refresh_token is long-lived; cutting it off means revoking the ChatGPT session/device). If true short-TTL is wanted, prefer the **host-proxy** model (box holds no credential; proxy holds the OAuth and is killed to revoke) — see the separate host-proxy follow-up.

## Depends on
- The first claude-only dogfood working (proven baseline).
- The configurable `resident_egress_allowlist` (#510) merged+installed (to add chatgpt.com/auth.openai.com).

## Exit criteria
- codex runs under `varda orchestrate` on the ChatGPT subscription (no OPENAI_API_KEY / no API spend), reaches its provider via the restricted egress, and sees the spawn broker tools — enabling a claude↔codex orchestrated wave.
