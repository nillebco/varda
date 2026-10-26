# Issue trackers

Varda can take its work from an issue tracker instead of a folder of markdown files, and report the result back to it. The loop is the same as for local tasks — route → run in a sandbox → bound → recap → commit — the tracker just supplies the work and receives the outcome.

Supported backends: **Azure DevOps Boards** (`azdo`), **GitHub Issues** (`github`), **Jira Cloud** (`jira`).

## Concepts

- **The tracker is the source of truth for the definition and the lifecycle.** Title, description, and the done/closed state live in the tracker. A human closing the item there marks the local task `done` only if the closed state matches the configured `done` mapping (on GitHub the default accepts only `closed:completed`, so closing as "not planned" does not mark it done).
- **The local task file is the run record.** Session ids, logs, resume commands, recap paths and budget bookkeeping stay in `~/.varda` — they do not belong in a tracker. A tracker-linked task is an ordinary home task with an extra `external:` frontmatter block (backend name, remote id, url, last revision seen, last synced status, `sync_error`). A task without `external` behaves exactly as before.
- **Sync happens at the edges.** `pull` imports/refreshes items; Varda pushes status and recaps back when a task changes status. The runner, orchestrator and MCP broker keep reading the local board.
- **One tracker → one local repo.** Each `[[trackers]]` entry has a `project_path`; imported tasks land there and go through the normal route/sandbox resolution.
- **Pickup is opt-in.** Varda only touches items carrying the pickup tag/label (default `varda`), so it can live inside a project that holds other tickets.

A tracker-linked task can be addressed by its external id: `varda task run <tracker-name>:<id>` (e.g. `azdo-acme:12345`).

## Quickstart: Azure DevOps

```sh
varda tracker setup azdo --org acme --project Platform --repo ~/dev/acme/platform \
  --credential-env AZDO_PAT
```

Setup prints each step as it runs:

1. **Credential** — resolves the PAT on the host (`--credential-fnox KEY`, `--credential-env VAR`, or `--credential-command CMD`; with none given it prompts) and proves it works against the org. Only the *reference* is written to config, never the token.
2. **Process discovery** — reads the project's process template and states and proposes a `status_map` (built-in defaults for Agile / Scrum / Basic / CMMI; a custom process gets a best-effort guess you confirm).
3. **Pickup tag** — shows any items already tagged `varda` (`--tag` changes the name). AzDO has no "create tag" call: a tag exists once applied, e.g. `az boards work-item update --id N --fields System.Tags=varda` or in the web UI.
4. **Dry run** — prints what `pull` would import, before anything is written.
5. **Data disclaimer + acknowledgement** — see below.
6. **Write config** — appends the `[[trackers]]` block to `~/.varda/config.toml`, then runs the tracker checks of `varda doctor`.

`--yes` skips the prompts (the acknowledgement is still recorded); `--force` replaces an existing tracker of the same name; `--name` overrides the default name `azdo-<project>`; `--work-item-type` (default `Task`) sets the type used when Varda creates items.

Then tag a work item `varda`. `varda run` only runs `ready` tasks, and readiness is decided when the item is **first imported**: the item must be in the `ready` state of your `status_map` and, where `ready` shares that state with `backlog` (Agile's `New`, and GitHub/Jira equivalents), also carry the `varda:ready` tag. So apply `varda` and `varda:ready` **before** the first `varda tracker pull`. Under Scrum, `ready` maps to `Approved`, so a `New` item cannot become ready by tagging; move it to `Approved` first.

Pulling an item that is already imported never promotes it: an existing `backlog` task only gets its body, URL and remote state refreshed (only a remote close is propagated, as `done`). Adding `varda:ready` afterwards therefore has no effect on the local task; promote it with `varda task set-status ready <task>`.


```sh
varda tracker list            # configured trackers + auth check
varda tracker pull            # import matching items as tasks
varda run                     # run everything ready
```

### GitHub Issues

```sh
varda tracker setup github --repo owner/name --project-path ~/dev/owner/name \
  --credential-command "gh auth token"
```

Checks the token, offers to create the pickup label (`--label`, default `varda`) and the `varda:*` helper labels, previews the pickup, records the acknowledgement. `--api-base URL` targets GitHub Enterprise. Issues only have open/closed, so every status except `done` maps to `open` and the helper labels carry the rest; `done` maps to `closed:completed`.

### Jira Cloud

```sh
varda tracker setup jira --site https://acme.atlassian.net --project-key VAR \
  --email me@example.com --repo ~/dev/acme/app --credential-env JIRA_TOKEN
```

Proves the API token, reads the project's workflow statuses and proposes a `status_map` from them (positional guess flagged for confirmation on a custom workflow), previews the pickup label (`--label`), records the acknowledgement. Status moves use workflow *transitions*; the `status_map` value names the target status. `--issue-type` (default `Task`) sets the type used when Varda creates issues.

## Configuration reference

Trackers are declared in the central `~/.varda/config.toml`. `varda tracker setup` writes the block for you; this is what it looks like.

```toml
[[trackers]]
name = "azdo-acme"                  # unique; used by --tracker and <name>:<id> refs
kind = "azdo"                       # azdo | github | jira
organization = "acme"               # azdo
project = "Platform"                # azdo
project_path = "/Users/me/dev/acme/platform"   # where the code lives; feeds route/sandbox resolution
work_item_type = "Task"             # azdo, optional: type for items Varda creates
credential = { from_env = "AZDO_PAT" }         # from_env | from_secret | from_fnox | command

[trackers.filter]
tags = ["varda"]                    # the pickup tag
states = ["New", "Active"]          # optional remote states to include
area_path = "Platform\\Backend"     # azdo, optional
# wiql = "SELECT [System.Id] FROM WorkItems WHERE ..."   # azdo: replaces the simple filter
# jql  = "project = VAR AND labels = varda"              # jira: replaces the simple filter

[trackers.status_map]               # keyed by Varda status, value = remote state
backlog    = "New"
ready      = "New"                  # + tag "varda:ready"
running    = "Active"
review     = "Resolved"
needs_user = "Active"               # + tag "varda:needs-user" + comment
failed     = "Active"               # + tag "varda:failed" + comment
done       = "Closed"

[trackers.acknowledged]             # written by `tracker setup`
by = "Ada Lovelace"
at = "2026-10-20T09:00:00Z"
agents = ["claude"]
```

Other kind-specific keys: GitHub uses `repo = "owner/name"` and optional `api_base`; Jira uses `site`, `project_key`, `email` and optional `issue_type`. Unknown keys must be strings. `status_map` keys must be real task statuses (`backlog`, `ready`, `running`, `review`, `needs_user`, `failed`, `done`); names must be unique and `project_path` non-empty — config load rejects violations.

### `status_map` defaults

Several Varda statuses collapse onto one remote state, so helper tags/labels (`varda:ready`, `varda:needs-user`, `varda:failed`) disambiguate. They appear the first time Varda applies them; Varda only ever touches `varda:*` helper tags, never your own.

| Varda status | Agile | Scrum | Basic | CMMI | GitHub |
|---|---|---|---|---|---|
| `backlog` | New | New | To Do | Proposed | open |
| `ready` | New | Approved | To Do | Proposed | open |
| `running` | Active | Committed | Doing | Active | open |
| `review` | Resolved | Committed | Doing | Resolved | open |
| `needs_user` | Active | Committed | Doing | Active | open |
| `failed` | Active | Committed | Doing | Active | open |
| `done` | Closed | Done | Done | Closed | closed:completed |

Jira has no fixed default: setup derives the map from the project's workflow (looking for names such as To Do/Open, In Progress, In Review, Done/Closed).

### `acknowledged`

`pull` (and `task add --tracker`) refuse a tracker with no acknowledgement, and also when the route covering `project_path` allows an agent that is not listed in `acknowledged.agents`. Re-run `varda tracker setup … --force` to re-acknowledge.

## Lifecycle

| Event | Tracker effect |
|---|---|
| `varda tracker pull` | create/refresh the home task (`external` set); never duplicates |
| task → `running` | remote state per `status_map`; comment "Varda started run …" |
| task → `review` | remote state per `status_map`; comment with the recap's *Files touched* and *Verification* sections |
| task → `needs_user` | `varda:needs-user` tag + comment with the agent's question |
| task → `failed` | `varda:failed` tag + comment with the failure summary |
| after the commit | follow-up comment with the commit hash(es) and branch; on Azure DevOps with an Azure Repos origin, artifact links too |
| human moves the item to a state matching the `done` mapping | next `pull` marks the local task `done` (GitHub default: only `closed:completed`; "not planned" does not) |
| human edits title/description | next `pull` refreshes the task body — never while the task is `running` |

Comments are capped at 20,000 bytes (truncated with a notice). A new remote item that is already closed, in an in-flight state, or in a state the `status_map` doesn't know is not imported.

## Commands

```sh
varda tracker list                     # configured trackers + authentication check
varda tracker pull [--tracker NAME] [--dry-run]   # import/refresh matching items
varda tracker sync [--tracker NAME]    # retry pushes that failed / are out of sync
varda tracker setup azdo|github|jira …  # guided setup
varda task add "…" --tracker NAME      # create the work item (tagged for pickup), then the linked local task
varda task run NAME:ID                 # run by external id
varda run --pull                       # pull every tracker, then run what is ready
varda orchestrate --pull-every SECS    # pull in the background while the resident runs (min 60)
```

`run --pull` is best-effort: a failing tracker is reported and the run continues. `--pull-every` only pulls trackers whose `project_path` the orchestration workspace covers, and runs host-side — the MCP broker surface is unchanged.

## Conflicts

Varda never overwrites a human change. Before each push it re-reads the item; if the remote state differs from the one Varda last saw (or the item is closed — including terminal states the `status_map` doesn't cover: GitHub "not planned", Azure DevOps `Removed`, any Jira status in the *Done* category such as "Won't Do"), the push is **refused** and `external.sync_error` explains why. Azure DevOps additionally sends a `test /rev` operation, so an edit landing between the read and the write fails with a conflict; Varda re-reads and retries once. Jira and GitHub have no atomic compare-and-set, so there the check is read-then-write and a narrow race remains. Re-run `varda tracker pull` to accept the human's change.

A push failure never fails a run: the error is stored in `external.sync_error`, shown by `varda task show`, and retried by `varda tracker sync`. Every outbound request has a timeout, and each push is bounded at 60 s. The runner awaits every push, so a run that pushes several times can be delayed by up to 60 s per push.

## Data disclaimer

Setup shows a one-screen disclaimer and records your confirmation (who, when, which agents):

> Work item titles, descriptions, acceptance criteria and comments from the tracker will be sent to the configured agent as prompt content, and run recaps will be posted back to those work items. Confirm that this is allowed for this organisation.

Descriptions are converted to markdown (HTML for Azure DevOps, ADF for Jira) before reaching the agent; the conversion is lossy (tables, embedded images are dropped).

## Security

- **Trackers are central-config only.** A repo-committed `.varda` that declares a tracker is refused at load: it could otherwise redirect the token or the recaps to an attacker's org. The same trust rule as credentials.
- **Tokens stay on the host.** The credential is resolved host-side, sent only in the request's `Authorization` header, scrubbed from errors, and never written to config or handed to the sandboxed agent. Grant the minimum scope (AzDO: *Work Items — Read & Write*; GitHub: issues read/write on the one repo).

## Troubleshooting

`varda doctor` includes a `configured trackers` section: per tracker, the acknowledgement/route check, whether the credential resolves and authenticates, and whether every `status_map` state exists for the work item type (AzDO) or workflow (Jira).

| Symptom | Likely cause |
|---|---|
| `has not been acknowledged` | run the `tracker setup` command the error prints |
| `acknowledgement does not cover agent(s) …` | the route for `project_path` allows an agent added after setup; re-run setup |
| `project_path has no route` | add a route with `varda project add` for that path |
| `sync_error` on a task | see `varda task show`; `conflict:` means a human changed the item — `varda tracker pull`, then `varda tracker sync` |
| nothing imported | the item lacks the pickup tag, or its state is filtered out — try `varda tracker pull --dry-run` |
| `status_map` state reported missing by doctor | the process template was customised; edit `[trackers.status_map]` |
