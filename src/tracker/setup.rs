//! `varda tracker setup azdo`: guided setup plus the data-disclaimer
//! acknowledgement.
//!
//! Each step prints a ✓/✗ line. Nothing is written to the central config until
//! the disclaimer is confirmed (or `--yes` is given). The block is APPENDED as
//! text so existing content and comments survive; only the credential
//! *reference* is ever written, never the token.

use std::collections::VecDeque;
use std::io::{BufRead as _, IsTerminal as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};

use super::{
    AZDO_AGILE_STATUS_MAP, AZDO_BASIC_STATUS_MAP, AZDO_CMMI_STATUS_MAP, AZDO_SCRUM_STATUS_MAP,
    GITHUB_STATUS_MAP, IssueTracker, status_map_from,
};
use crate::config::{Config, CredentialConfig, TrackerAck, TrackerConfig, TrackerFilter, TrackerKind};

/// Interactive I/O behind a trait so tests drive setup with scripted answers.
pub trait SetupIo {
    /// Whether prompts can be answered (stdin is a terminal).
    fn is_tty(&self) -> bool;
    fn say(&mut self, line: &str);
    /// Print `prompt` and read one trimmed line.
    fn ask(&mut self, prompt: &str) -> Result<String>;
}

/// stdin/stdout implementation.
pub struct StdioIo;

impl SetupIo for StdioIo {
    fn is_tty(&self) -> bool {
        std::io::stdin().is_terminal()
    }

    fn say(&mut self, line: &str) {
        println!("{line}");
    }

    fn ask(&mut self, prompt: &str) -> Result<String> {
        print!("{prompt} ");
        std::io::stdout().flush().ok();
        let mut line = String::new();
        let n = std::io::stdin().lock().read_line(&mut line)?;
        if n == 0 {
            bail!("unexpected end of input while waiting for an answer");
        }
        Ok(line.trim().to_owned())
    }
}

/// Scripted answers, for tests.
#[derive(Default)]
pub struct ScriptedIo {
    pub tty: bool,
    pub answers: VecDeque<String>,
    pub out: Vec<String>,
}

impl SetupIo for ScriptedIo {
    fn is_tty(&self) -> bool {
        self.tty
    }

    fn say(&mut self, line: &str) {
        self.out.push(line.to_owned());
    }

    fn ask(&mut self, prompt: &str) -> Result<String> {
        self.out.push(prompt.to_owned());
        self.answers
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("no scripted answer for: {prompt}"))
    }
}

fn confirm(io: &mut dyn SetupIo, prompt: &str, default: bool) -> Result<bool> {
    let hint = if default { "[Y/n]" } else { "[y/N]" };
    let answer = io.ask(&format!("{prompt} {hint}"))?;
    Ok(match answer.to_ascii_lowercase().as_str() {
        "" => default,
        "y" | "yes" => true,
        _ => false,
    })
}

#[derive(Debug, Clone)]
pub struct SetupOptions {
    pub org: String,
    pub project: String,
    pub repo: PathBuf,
    pub name: Option<String>,
    pub tag: String,
    pub credential: Option<CredentialConfig>,
    pub work_item_type: String,
    pub yes: bool,
    pub force: bool,
    /// Who acknowledges (git `user.email` or `$USER`).
    pub by: String,
    /// RFC 3339 timestamp of the acknowledgement.
    pub at: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SetupOutcome {
    /// Block written; `doctor_failures` post-write checks failed.
    Written { doctor_failures: usize },
    /// The pilot declined at the disclaimer; nothing was written.
    Declined,
}

pub type BackendFactory<'a> = &'a dyn Fn(&TrackerConfig) -> Result<Box<dyn IssueTracker>>;

fn ok(io: &mut dyn SetupIo, text: &str) {
    io.say(&format!("✓ {text}"));
}

fn fail(io: &mut dyn SetupIo, text: &str) {
    io.say(&format!("✗ {text}"));
}

fn slug(s: &str) -> String {
    let slug: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect();
    slug.trim_matches('-').to_owned()
}

/// Provider behind an agent, from the CLI it execs.
fn agent_provider(config: &Config, agent: &str) -> &'static str {
    use crate::config::{AgentCli, agent_cli_kind};
    match config
        .agents
        .get(agent)
        .and_then(|a| agent_cli_kind(&a.command, &a.args))
    {
        Some(AgentCli::Claude) => "Anthropic",
        Some(AgentCli::Copilot) => "GitHub Copilot",
        Some(AgentCli::Codex) => "OpenAI",
        None => "unknown provider",
    }
}

/// The disclaimer screen. Wording is from the plan, filled in.
pub fn disclaimer_text(config: &Config, org: &str, project: &str, agents: &[String]) -> String {
    disclaimer_for(config, &format!("{org}/{project}"), "Work item", "work items", agents)
}

/// [`disclaimer_text`] for an arbitrary `scope` (`org/project`, `owner/repo`).
fn disclaimer_for(config: &Config, scope: &str, noun: &str, plural: &str, agents: &[String]) -> String {
    let who: Vec<String> = agents
        .iter()
        .map(|a| format!("`{a}`, {}", agent_provider(config, a)))
        .collect();
    format!(
        "{noun} titles, descriptions, acceptance criteria and comments from\n\
         `{scope}` will be sent to the configured agent ({}) as\n\
         prompt content, and run recaps will be posted back to those {plural}.\n\
         Confirm that this is allowed for this organisation.",
        who.join("; ")
    )
}

/// Best default `status_map` for `process`; `exact` is false when the process is
/// not one of the built-ins and the pick is a guess from state names.
fn propose_status_map(
    process: Option<&str>,
    states: Option<&[String]>,
) -> (&'static str, &'static [(&'static str, &'static str)], bool) {
    let defaults: [(&str, &[(&str, &str)]); 4] = [
        ("Agile", AZDO_AGILE_STATUS_MAP),
        ("Scrum", AZDO_SCRUM_STATUS_MAP),
        ("Basic", AZDO_BASIC_STATUS_MAP),
        ("CMMI", AZDO_CMMI_STATUS_MAP),
    ];
    if let Some(p) = process
        && let Some((n, m)) = defaults.iter().find(|(n, _)| n.eq_ignore_ascii_case(p.trim()))
    {
        return (n, m, true);
    }
    let score = |map: &[(&str, &str)]| {
        let mut values: Vec<&str> = map.iter().map(|(_, v)| *v).collect();
        values.sort_unstable();
        values.dedup();
        states.map_or(0, |st| {
            values.iter().filter(|v| st.iter().any(|s| s.eq_ignore_ascii_case(v))).count()
        })
    };
    // max_by_key keeps the LAST maximum; iterate reversed so ties favour Agile.
    let (n, m) = defaults
        .iter()
        .rev()
        .max_by_key(|(_, m)| score(m))
        .copied()
        .expect("non-empty");
    (n, m, false)
}

/// Whether `line` is the table header `header`, allowing surrounding
/// whitespace and a trailing `# comment`.
fn header_is(line: &str, header: &str) -> bool {
    let t = line.trim();
    t.strip_prefix(header)
        .is_some_and(|rest| rest.trim().is_empty() || rest.trim_start().starts_with('#'))
}

/// Remove the `[[trackers]]` block named `name` (with its `[trackers.*]`
/// subtables). Returns `None` when there is no such block.
fn remove_tracker_block(text: &str, name: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        if header_is(lines[i], "[[trackers]]") {
            let mut end = i + 1;
            while end < lines.len() {
                let t = lines[end].trim_start();
                if t.starts_with('[') && !t.starts_with("[trackers.") {
                    break;
                }
                end += 1;
            }
            let block = lines[i..end].join("\n");
            let is_it = toml::from_str::<toml::Value>(&block)
                .ok()
                .and_then(|v| v.get("trackers")?.as_array()?.first()?.get("name")?.as_str().map(str::to_owned))
                .is_some_and(|n| n == name);
            if is_it {
                let mut out: Vec<&str> = lines[..i].to_vec();
                out.extend(&lines[end..]);
                let mut s = out.join("\n");
                if text.ends_with('\n') {
                    s.push('\n');
                }
                return Some(s);
            }
            i = end;
        } else {
            i += 1;
        }
    }
    None
}

fn toml_str(s: &str) -> String {
    toml::Value::String(s.to_owned()).to_string()
}

fn toml_list(items: &[String]) -> String {
    format!("[{}]", items.iter().map(|s| toml_str(s)).collect::<Vec<_>>().join(", "))
}

/// Render the `[[trackers]]` block for `t` (reference-only credential).
pub fn render_block(t: &TrackerConfig) -> String {
    let mut out = String::from("[[trackers]]\n");
    let kind = match t.kind {
        TrackerKind::Azdo => "azdo",
        TrackerKind::Github => "github",
        TrackerKind::Jira => "jira",
    };
    out.push_str(&format!("name = {}\nkind = \"{kind}\"\n", toml_str(&t.name)));
    for (k, v) in &t.extra {
        out.push_str(&format!("{k} = {}\n", toml_str(v)));
    }
    out.push_str(&format!("project_path = {}\n", toml_str(&t.project_path)));
    let c = &t.credential;
    let cred = if let Some(v) = &c.from_fnox {
        format!("from_fnox = {}", toml_str(v))
    } else if let Some(v) = &c.from_secret {
        format!("from_secret = {}", toml_str(v))
    } else if let Some(v) = &c.from_env {
        format!("from_env = {}", toml_str(v))
    } else {
        format!("command = {}", toml_str(c.command.as_deref().unwrap_or_default()))
    };
    out.push_str(&format!("credential = {{ {cred} }}\n"));
    let mut filter = Vec::new();
    if !t.filter.tags.is_empty() {
        filter.push(format!("tags = {}", toml_list(&t.filter.tags)));
    }
    if !t.filter.states.is_empty() {
        filter.push(format!("states = {}", toml_list(&t.filter.states)));
    }
    if !filter.is_empty() {
        out.push_str(&format!("filter = {{ {} }}\n", filter.join(", ")));
    }
    if let Some(a) = &t.acknowledged {
        out.push_str(&format!(
            "acknowledged = {{ by = {}, at = {}, agents = {} }}\n",
            toml_str(&a.by),
            toml_str(&a.at),
            toml_list(&a.agents)
        ));
    }
    if !t.status_map.is_empty() {
        out.push_str("\n[trackers.status_map]\n");
        for (k, v) in &t.status_map {
            out.push_str(&format!("{k} = {}\n", toml_str(v)));
        }
    }
    out
}

/// Append `block` to the config text (replacing the same-named block when
/// `force`), checking that the result still parses with valid trackers.
fn merged_config_text(existing: &str, t: &TrackerConfig, force: bool) -> Result<String> {
    let mut text = if force {
        remove_tracker_block(existing, &t.name).unwrap_or_else(|| existing.to_owned())
    } else {
        existing.to_owned()
    };
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    if !text.is_empty() {
        text.push('\n');
    }
    text.push_str(&render_block(t));
    let parsed: toml::Value = toml::from_str(&text).context("the resulting config does not parse")?;
    let trackers: Vec<TrackerConfig> = parsed
        .get("trackers")
        .cloned()
        .map(|v| v.try_into())
        .transpose()
        .context("the resulting [[trackers]] do not deserialize")?
        .unwrap_or_default();
    crate::config::validate_trackers(&trackers)?;
    Ok(text)
}

/// Ask where the credential lives; only a reference is ever returned.
fn prompt_credential(io: &mut dyn SetupIo, question: &str, command_hint: &str) -> Result<CredentialConfig> {
    io.say(question);
    io.say(&format!("  1) fnox key   2) environment variable   3) {command_hint}"));
    let kind = io.ask("Choice [1-3]:")?;
    let value = io.ask("Key / variable name / command:")?;
    if value.is_empty() {
        bail!("empty credential reference");
    }
    let mut c = CredentialConfig::default();
    match kind.as_str() {
        "1" => c.from_fnox = Some(value),
        "2" => c.from_env = Some(value),
        "3" => c.command = Some(value),
        other => bail!("unknown credential choice '{other}'"),
    }
    Ok(c)
}

/// Run the guided setup. `config` is the loaded central config (routes,
/// agents, existing trackers); `config_path` is where the block is appended.
pub async fn run_setup(
    config: &Config,
    config_path: &Path,
    opts: &SetupOptions,
    io: &mut dyn SetupIo,
    build: BackendFactory<'_>,
) -> Result<SetupOutcome> {
    let name = opts
        .name
        .clone()
        .unwrap_or_else(|| format!("azdo-{}", slug(&opts.project)));
    if name.trim().is_empty() {
        bail!("could not derive a tracker name; pass --name");
    }
    if config.trackers.iter().any(|t| t.name == name) && !opts.force {
        bail!("tracker '{name}' already exists in {}; pass --force to replace it, or --name to pick another", config_path.display());
    }

    // The repo must be routed: the route decides which agents see the data.
    let repo = std::fs::canonicalize(&opts.repo)
        .with_context(|| format!("--repo {} does not exist", opts.repo.display()))?;
    let route = crate::routing::find_route_public(config, &repo)
        .with_context(|| format!("--repo {} matches no route in the central config", repo.display()))?;
    let agents = route.agents.clone();

    // 1. Credential.
    let credential = match &opts.credential {
        Some(c) => c.clone(),
        None if opts.yes || !io.is_tty() => {
            bail!("no credential given; pass --credential-fnox, --credential-env or --credential-command")
        }
        None => prompt_credential(
            io,
            "Where does the Azure DevOps PAT live?",
            "command (e.g. az account get-access-token ...)",
        )?,
    };
    credential.source().context("invalid credential reference")?;

    let mut extra = std::collections::BTreeMap::new();
    extra.insert("organization".to_owned(), opts.org.clone());
    extra.insert("project".to_owned(), opts.project.clone());
    if opts.work_item_type != "Task" {
        extra.insert("work_item_type".to_owned(), opts.work_item_type.clone());
    }
    let mut draft = TrackerConfig {
        name: name.clone(),
        kind: TrackerKind::Azdo,
        project_path: repo.display().to_string(),
        credential,
        filter: TrackerFilter {
            tags: vec![opts.tag.clone()],
            ..TrackerFilter::default()
        },
        status_map: Default::default(),
        acknowledged: None,
        extra,
    };

    let backend = match build(&draft) {
        Ok(b) => b,
        Err(_) => {
            // The chain may embed credential-command stderr; never print it.
            fail(io, "credential could not be resolved (see `varda tracker doctor`)");
            bail!("credential could not be resolved");
        }
    };
    ok(io, "credential reference resolves");
    if backend.check_auth().await.is_err() {
        fail(io, "connectionData: authentication failed (check the credential and its scope)");
        bail!("authentication failed");
    }
    ok(io, "authenticated (connectionData)");
    let probe = TrackerFilter {
        wiql: Some("SELECT [System.Id] FROM WorkItems WHERE [System.TeamProject] = @project AND [System.Id] = 0".to_owned()),
        ..TrackerFilter::default()
    };
    match backend.query(&probe).await {
        Ok(items) if items.len() <= 1 => ok(io, "read access (WIQL query)"),
        Ok(_) => ok(io, "read access (WIQL query)"),
        Err(_) => {
            fail(io, "read access: the WIQL probe failed (check the `Work Items (Read)` scope)");
            bail!("cannot read work items with this credential");
        }
    }
    io.say("  (write access is not probed: it would modify a work item; `Work Items (Read & Write)` scope is required)");

    // 2. Process template → status_map.
    let process = backend.process_template().await.unwrap_or(None);
    let states = backend.valid_states().await.unwrap_or(None);
    let (kind, defaults, exact) = propose_status_map(process.as_deref(), states.as_deref());
    match (&process, exact) {
        (Some(p), true) => ok(io, &format!("process template: {p}")),
        (Some(p), false) => ok(io, &format!("process template: {p} (custom; closest built-in: {kind})")),
        (None, _) => ok(io, &format!("process template: unknown (closest built-in: {kind})")),
    }
    draft.status_map = status_map_from(defaults);
    io.say(&format!("  {:<12} -> {}", "varda status", "state"));
    for (status, state) in &draft.status_map {
        io.say(&format!("  {status:<12} -> {state}"));
    }
    if !exact && !opts.yes && !confirm(io, &format!("Use the {kind} mapping above?"), true)? {
        bail!("status_map not confirmed; nothing written (set `status_map` by hand after adding the tracker)");
    }
    // `filter.states` stays empty: pull must also see items closed remotely so
    // it can mark the local task done (a new closed item is skipped by pull).

    // 3. Pickup tag.
    io.say(&format!(
        "Pickup tag: Varda only touches work items tagged `{}`; this scopes it inside a project that holds other tickets.",
        opts.tag
    ));
    let tagged = TrackerFilter {
        tags: vec![opts.tag.clone()],
        ..TrackerFilter::default()
    };
    match backend.query(&tagged).await {
        Ok(items) if items.is_empty() => {
            ok(io, &format!("no work items carry `{}` yet", opts.tag));
        }
        Ok(items) => {
            ok(io, &format!("{} work item(s) already carry `{}`:", items.len(), opts.tag));
            for i in items.iter().take(10) {
                io.say(&format!("  #{} [{}] {}", i.id, i.remote_state, i.title));
            }
        }
        Err(_) => {
            fail(io, "could not list tagged items");
            bail!("the tagged-items query failed; nothing written");
        }
    }
    io.say(&format!(
        "  To tag one: az boards work-item update --id N --fields System.Tags={}   (or use the web UI)",
        opts.tag
    ));
    io.say("  The varda:ready / varda:needs-user / varda:failed helper tags appear on first use.");

    let agents_for_text = agents.clone();
    finalize(
        config,
        config_path,
        FinalizeArgs {
            draft,
            agents,
            disclaimer: disclaimer_text(config, &opts.org, &opts.project, &agents_for_text),
            yes: opts.yes,
            force: opts.force,
            by: &opts.by,
            at: &opts.at,
        },
        backend.as_ref(),
        io,
    )
    .await
}

/// Everything the shared setup tail needs.
struct FinalizeArgs<'a> {
    draft: TrackerConfig,
    agents: Vec<String>,
    disclaimer: String,
    yes: bool,
    force: bool,
    by: &'a str,
    at: &'a str,
}

/// Dry-run pull, disclaimer + acknowledgement, append the block, doctor checks.
async fn finalize(
    config: &Config,
    config_path: &Path,
    args: FinalizeArgs<'_>,
    backend: &dyn IssueTracker,
    io: &mut dyn SetupIo,
) -> Result<SetupOutcome> {
    let FinalizeArgs { mut draft, agents, disclaimer, yes, force, by, at } = args;
    let name = draft.name.clone();
    // Dry run (reuses the pull dry-run). A provisional acknowledgement lets
    // the pull gate pass; it is not persisted unless confirmed below.
    let ack = TrackerAck {
        by: by.to_owned(),
        at: at.to_owned(),
        agents: agents.clone(),
    };
    let mut dry = draft.clone();
    dry.acknowledged = Some(ack.clone());
    match super::sync::pull_tracker(config, &dry, backend, true).await {
        Ok(outcome) => {
            ok(io, &format!("dry run: pull would process {} work item(s)", outcome.items.len()));
            for item in &outcome.items {
                io.say(&format!("  {} {} {}", item.action.label(), item.id, item.title));
            }
            if !outcome.errors.is_empty() {
                for e in &outcome.errors {
                    fail(io, e);
                }
                bail!("the pull dry run reported errors; nothing written");
            }
        }
        Err(_) => {
            fail(io, "dry run failed");
            bail!("the pull dry run failed; nothing written");
        }
    }

    // Disclaimer + acknowledgement.
    io.say("");
    io.say(&disclaimer);
    if yes {
        io.say("(--yes: acknowledgement recorded without prompting)");
    } else if !confirm(io, "Do you confirm?", false)? {
        io.say("Not acknowledged; nothing written.");
        return Ok(SetupOutcome::Declined);
    }
    draft.acknowledged = Some(ack);

    // Write config (append) and run the doctor checks.
    let existing = match std::fs::read_to_string(config_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", config_path.display())),
    };
    let text = merged_config_text(&existing, &draft, force)?;
    if let Some(dir) = config_path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(config_path, text)
        .with_context(|| format!("writing {}", config_path.display()))?;
    ok(io, &format!("tracker '{name}' written to {}", config_path.display()));

    let mut config_after = config.clone();
    config_after.trackers.retain(|t| t.name != name);
    config_after.trackers.push(draft.clone());
    let mut failures = 0;
    let mut checks = vec![(
        "acknowledged",
        super::sync::check_acknowledged(&config_after, &draft)
            .map(|()| "yes".to_owned())
            .map_err(|_| String::new()),
    )];
    checks.extend(super::doctor::backend_checks(&draft, backend).await);
    for (check, verdict) in checks {
        match verdict {
            Ok(detail) => ok(io, &format!("doctor {check}: {detail}")),
            // The raw reason may carry response bodies; never print it here.
            Err(_) => {
                failures += 1;
                fail(io, &format!("doctor {check}: failed (run `varda tracker doctor` for details)"));
            }
        }
    }
    Ok(SetupOutcome::Written { doctor_failures: failures })
}

#[derive(Debug, Clone)]
pub struct GithubSetupOptions {
    /// `owner/name` of the GitHub repository holding the issues.
    pub github_repo: String,
    /// REST API root for GitHub Enterprise; `None` ⇒ `https://api.github.com`.
    pub api_base: Option<String>,
    /// Local repository the issues are worked on in.
    pub project_path: PathBuf,
    pub name: Option<String>,
    /// Pickup label: only issues carrying it are imported.
    pub label: String,
    pub credential: Option<CredentialConfig>,
    /// Create missing labels without asking.
    pub yes: bool,
    pub force: bool,
    pub by: String,
    pub at: String,
}

/// `varda tracker setup github`: same shape as [`run_setup`] — credential proof
/// (`GET /repos/{o}/{r}`), pickup/helper label check with an offer to create
/// them (GitHub can), dry run, disclaimer, write config.
pub async fn run_setup_github(
    config: &Config,
    config_path: &Path,
    opts: &GithubSetupOptions,
    io: &mut dyn SetupIo,
    build: BackendFactory<'_>,
) -> Result<SetupOutcome> {
    if !super::github::valid_repo_slug(&opts.github_repo) {
        bail!("--repo must look like `owner/name`, got '{}'", opts.github_repo);
    }
    let name = opts
        .name
        .clone()
        .unwrap_or_else(|| format!("github-{}", slug(&opts.github_repo)));
    if name.trim().is_empty() {
        bail!("could not derive a tracker name; pass --name");
    }
    if config.trackers.iter().any(|t| t.name == name) && !opts.force {
        bail!("tracker '{name}' already exists in {}; pass --force to replace it, or --name to pick another", config_path.display());
    }
    let repo = std::fs::canonicalize(&opts.project_path)
        .with_context(|| format!("--project-path {} does not exist", opts.project_path.display()))?;
    let route = crate::routing::find_route_public(config, &repo)
        .with_context(|| format!("--project-path {} matches no route in the central config", repo.display()))?;
    let agents = route.agents.clone();

    // 1. Credential.
    let credential = match &opts.credential {
        Some(c) => c.clone(),
        None if opts.yes || !io.is_tty() => {
            bail!("no credential given; pass --credential-fnox, --credential-env or --credential-command")
        }
        None => prompt_credential(
            io,
            "Where does the GitHub token live? (fine-grained PAT with Issues read/write)",
            "command (e.g. gh auth token)",
        )?,
    };
    credential.source().context("invalid credential reference")?;

    let mut extra = std::collections::BTreeMap::new();
    extra.insert("repo".to_owned(), opts.github_repo.clone());
    if let Some(base) = opts.api_base.as_deref().filter(|b| !b.trim().is_empty()) {
        extra.insert("api_base".to_owned(), base.trim().to_owned());
    }
    let mut draft = TrackerConfig {
        name: name.clone(),
        kind: TrackerKind::Github,
        project_path: repo.display().to_string(),
        credential,
        filter: TrackerFilter {
            tags: vec![opts.label.clone()],
            ..TrackerFilter::default()
        },
        status_map: status_map_from(GITHUB_STATUS_MAP),
        acknowledged: None,
        extra,
    };

    let backend = match build(&draft) {
        Ok(b) => b,
        Err(_) => {
            // The chain may embed credential-command stderr; never print it.
            fail(io, "credential could not be resolved (see `varda tracker doctor`)");
            bail!("credential could not be resolved");
        }
    };
    ok(io, "credential reference resolves");
    if backend.check_auth().await.is_err() {
        fail(io, &format!("GET /repos/{}: authentication failed (check the token, its Issues scope and the repo name)", opts.github_repo));
        bail!("authentication failed");
    }
    ok(io, &format!("authenticated (GET /repos/{})", opts.github_repo));
    io.say("  (write access is not probed: it would modify an issue; Issues read/write permission is required)");

    // 2. status_map.
    ok(io, "status_map: GitHub issues only open/close; the varda:* labels carry the finer statuses");
    io.say(&format!("  {:<12} -> {}", "varda status", "state"));
    for (status, state) in &draft.status_map {
        io.say(&format!("  {status:<12} -> {state}"));
    }

    // 3. Pickup + helper labels.
    let mut wanted = vec![opts.label.clone()];
    wanted.extend(["varda:ready", "varda:needs-user", "varda:failed"].map(str::to_owned));
    io.say(&format!(
        "Pickup label: Varda only touches issues labelled `{}`; this scopes it inside a repository that holds other issues.",
        opts.label
    ));
    match backend.missing_labels(&wanted).await {
        Ok(missing) if missing.is_empty() => ok(io, "pickup and helper labels exist"),
        Ok(missing) => {
            io.say(&format!("  missing labels: {}", missing.join(", ")));
            let create = opts.yes
                || (io.is_tty() && confirm(io, "Create them on the repository now?", true)?);
            if create {
                if backend.create_labels(&missing).await.is_err() {
                    fail(io, "could not create labels (needs Issues write permission; create them by hand)");
                    bail!("label creation failed; nothing written");
                }
                ok(io, &format!("created {} label(s)", missing.len()));
            } else {
                io.say("  (not created; add them in the web UI, or re-run with --yes. Helper labels are also created on first use.)");
            }
        }
        Err(_) => {
            fail(io, "could not list labels");
            bail!("the label check failed; nothing written");
        }
    }
    let tagged = TrackerFilter {
        tags: vec![opts.label.clone()],
        ..TrackerFilter::default()
    };
    match backend.query(&tagged).await {
        Ok(items) if items.is_empty() => ok(io, &format!("read access; no issues carry `{}` yet", opts.label)),
        Ok(items) => {
            ok(io, &format!("read access; {} issue(s) already carry `{}`:", items.len(), opts.label));
            for i in items.iter().take(10) {
                io.say(&format!("  #{} [{}] {}", i.id, i.remote_state, i.title));
            }
        }
        Err(_) => {
            fail(io, "could not list labelled issues");
            bail!("the labelled-issues query failed; nothing written");
        }
    }
    // `filter.states` stays empty: pull must also see issues closed remotely so
    // it can mark the local task done.

    let ack_agents = agents.clone();
    draft.acknowledged = None;
    finalize(
        config,
        config_path,
        FinalizeArgs {
            draft,
            agents,
            disclaimer: disclaimer_for(config, &opts.github_repo, "Issue", "issues", &ack_agents),
            yes: opts.yes,
            force: opts.force,
            by: &opts.by,
            at: &opts.at,
        },
        backend.as_ref(),
        io,
    )
    .await
}

// ---------------------------------------------------------------------------
// Jira
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct JiraSetupOptions {
    /// `https://x.atlassian.net`
    pub site: String,
    pub project_key: String,
    /// Atlassian account email (the Basic-auth user; the token is the password).
    pub email: String,
    pub repo: PathBuf,
    pub name: Option<String>,
    /// Pickup label: only issues carrying it are imported.
    pub label: String,
    pub credential: Option<CredentialConfig>,
    pub issue_type: String,
    pub yes: bool,
    pub force: bool,
    pub by: String,
    pub at: String,
}

/// Propose a `status_map` (values are target STATUS names; Jira moves by
/// transition) from the project's workflow statuses. The flag is `true` when
/// backlog, running and done all matched a well-known name; otherwise the map
/// is a positional guess the pilot must confirm.
pub fn propose_jira_status_map(
    statuses: &[String],
) -> (std::collections::BTreeMap<String, String>, bool) {
    let pick = |candidates: &[&str]| -> Option<String> {
        candidates.iter().find_map(|c| {
            statuses.iter().find(|s| s.eq_ignore_ascii_case(c)).cloned()
        })
    };
    let backlog = pick(&["Backlog", "To Do", "Open", "Selected for Development"]);
    let running = pick(&["In Progress", "In Development", "Doing"]);
    let review = pick(&["In Review", "Review", "Code Review", "In QA", "Testing"]);
    let done = pick(&["Done", "Closed", "Resolved", "Complete", "Completed"]);
    let exact = backlog.is_some() && running.is_some() && done.is_some();
    let first = statuses.first().cloned().unwrap_or_default();
    let last = statuses.last().cloned().unwrap_or_default();
    let middle = statuses.get(statuses.len() / 2).cloned().unwrap_or_default();
    let backlog = backlog.unwrap_or(first);
    let running = running.unwrap_or(middle);
    let done = done.unwrap_or(last);
    let review = review.unwrap_or_else(|| running.clone());
    let map = [
        ("backlog", &backlog),
        ("ready", &backlog),
        ("running", &running),
        ("review", &review),
        ("needs_user", &running),
        ("failed", &running),
        ("done", &done),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.clone()))
    .collect();
    (map, exact)
}

/// `varda tracker setup jira`: credential proof (`/myself`), workflow statuses
/// to propose the `status_map`, label explanation, dry run, disclaimer, write.
pub async fn run_setup_jira(
    config: &Config,
    config_path: &Path,
    opts: &JiraSetupOptions,
    io: &mut dyn SetupIo,
    build: BackendFactory<'_>,
) -> Result<SetupOutcome> {
    let name = opts
        .name
        .clone()
        .unwrap_or_else(|| format!("jira-{}", slug(&opts.project_key)));
    if name.trim().is_empty() {
        bail!("could not derive a tracker name; pass --name");
    }
    if config.trackers.iter().any(|t| t.name == name) && !opts.force {
        bail!("tracker '{name}' already exists in {}; pass --force to replace it, or --name to pick another", config_path.display());
    }
    let repo = std::fs::canonicalize(&opts.repo)
        .with_context(|| format!("--repo {} does not exist", opts.repo.display()))?;
    let route = crate::routing::find_route_public(config, &repo)
        .with_context(|| format!("--repo {} matches no route in the central config", repo.display()))?;
    let agents = route.agents.clone();

    // 1. Credential.
    let credential = match &opts.credential {
        Some(c) => c.clone(),
        None if opts.yes || !io.is_tty() => {
            bail!("no credential given; pass --credential-fnox, --credential-env or --credential-command")
        }
        None => {
            io.say("Where does the Jira API token live?");
            io.say("  1) fnox key   2) environment variable   3) command");
            let kind = io.ask("Choice [1-3]:")?;
            let value = io.ask("Key / variable name / command:")?;
            if value.is_empty() {
                bail!("empty credential reference");
            }
            let mut c = CredentialConfig::default();
            match kind.as_str() {
                "1" => c.from_fnox = Some(value),
                "2" => c.from_env = Some(value),
                "3" => c.command = Some(value),
                other => bail!("unknown credential choice '{other}'"),
            }
            c
        }
    };
    credential.source().context("invalid credential reference")?;

    let mut extra = std::collections::BTreeMap::new();
    extra.insert("site".to_owned(), opts.site.trim_end_matches('/').to_owned());
    extra.insert("project_key".to_owned(), opts.project_key.clone());
    extra.insert("email".to_owned(), opts.email.clone());
    if opts.issue_type != "Task" {
        extra.insert("issue_type".to_owned(), opts.issue_type.clone());
    }
    let mut draft = TrackerConfig {
        name: name.clone(),
        kind: TrackerKind::Jira,
        project_path: repo.display().to_string(),
        credential,
        filter: TrackerFilter {
            tags: vec![opts.label.clone()],
            ..TrackerFilter::default()
        },
        status_map: Default::default(),
        acknowledged: None,
        extra,
    };
    let backend = match build(&draft) {
        Ok(b) => b,
        Err(_) => {
            fail(io, "credential could not be resolved or the Jira settings are invalid (see `varda tracker doctor`)");
            bail!("credential could not be resolved");
        }
    };
    ok(io, "credential reference resolves");
    if backend.check_auth().await.is_err() {
        fail(io, "/rest/api/3/myself: authentication failed (check the email, the token and the site)");
        bail!("authentication failed");
    }
    ok(io, "authenticated (/rest/api/3/myself)");

    // 2. Workflow statuses -> status_map.
    let statuses = match backend.valid_states().await {
        Ok(Some(s)) if !s.is_empty() => s,
        _ => {
            fail(io, "could not read the project's statuses (check the project key and the `Browse projects` permission)");
            bail!("cannot read project statuses; nothing written");
        }
    };
    ok(io, &format!("project statuses: {}", statuses.join(", ")));
    let (map, exact) = propose_jira_status_map(&statuses);
    draft.status_map = map;
    io.say("  Jira moves issues by workflow TRANSITION; each value below is the target status Varda asks for.");
    io.say(&format!("  {:<12} -> {}", "varda status", "target status"));
    for (status, target) in &draft.status_map {
        io.say(&format!("  {status:<12} -> {target}"));
    }
    if !exact && !opts.yes && !confirm(io, "These statuses are a guess; use the mapping above?", true)? {
        bail!("status_map not confirmed; nothing written (set `status_map` by hand after adding the tracker)");
    }

    // 3. Pickup label.
    io.say(&format!(
        "Pickup label: Varda only touches issues labelled `{}`; this scopes it inside a project that holds other tickets.",
        opts.label
    ));
    let tagged = TrackerFilter {
        tags: vec![opts.label.clone()],
        ..TrackerFilter::default()
    };
    match backend.query(&tagged).await {
        Ok(items) if items.is_empty() => ok(io, &format!("no issues carry `{}` yet", opts.label)),
        Ok(items) => {
            ok(io, &format!("{} issue(s) already carry `{}`:", items.len(), opts.label));
            for i in items.iter().take(10) {
                io.say(&format!("  {} [{}] {}", i.id, i.remote_state, i.title));
            }
        }
        Err(_) => {
            fail(io, "could not list labelled issues (check the `Browse projects` permission)");
            bail!("the labelled-issues query failed; nothing written");
        }
    }
    io.say(&format!("  To label one: open the issue and add the label `{}`.", opts.label));
    io.say("  The varda:ready / varda:needs-user / varda:failed helper labels appear on first use.");
    io.say("  (write access is not probed: it would modify an issue; the token's user needs `Edit issues`, `Transition issues` and `Add comments`)");

    // 4. Dry run.
    let ack = TrackerAck {
        by: opts.by.clone(),
        at: opts.at.clone(),
        agents: agents.clone(),
    };
    let mut dry = draft.clone();
    dry.acknowledged = Some(ack.clone());
    match super::sync::pull_tracker(config, &dry, backend.as_ref(), true).await {
        Ok(outcome) => {
            ok(io, &format!("dry run: pull would process {} issue(s)", outcome.items.len()));
            for item in &outcome.items {
                io.say(&format!("  {} {} {}", item.action.label(), item.id, item.title));
            }
            if !outcome.errors.is_empty() {
                for e in &outcome.errors {
                    fail(io, e);
                }
                bail!("the pull dry run reported errors; nothing written");
            }
        }
        Err(_) => {
            fail(io, "dry run failed");
            bail!("the pull dry run failed; nothing written");
        }
    }

    // 5. Disclaimer + acknowledgement.
    io.say("");
    io.say(&disclaimer_text(config, &opts.site, &opts.project_key, &agents));
    if opts.yes {
        io.say("(--yes: acknowledgement recorded without prompting)");
    } else if !confirm(io, "Do you confirm?", false)? {
        io.say("Not acknowledged; nothing written.");
        return Ok(SetupOutcome::Declined);
    }
    draft.acknowledged = Some(ack);

    // 6. Write config (append) and run the doctor checks.
    let existing = match std::fs::read_to_string(config_path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", config_path.display())),
    };
    let text = merged_config_text(&existing, &draft, opts.force)?;
    if let Some(dir) = config_path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    std::fs::write(config_path, text)
        .with_context(|| format!("writing {}", config_path.display()))?;
    ok(io, &format!("tracker '{name}' written to {}", config_path.display()));

    let mut config_after = config.clone();
    config_after.trackers.retain(|t| t.name != name);
    config_after.trackers.push(draft.clone());
    let mut failures = 0;
    let mut checks = vec![(
        "acknowledged",
        super::sync::check_acknowledged(&config_after, &draft)
            .map(|()| "yes".to_owned())
            .map_err(|_| String::new()),
    )];
    checks.extend(super::doctor::backend_checks(&draft, backend.as_ref()).await);
    for (check, verdict) in checks {
        match verdict {
            Ok(detail) => ok(io, &format!("doctor {check}: {detail}")),
            Err(_) => {
                failures += 1;
                fail(io, &format!("doctor {check}: failed (run `varda tracker doctor` for details)"));
            }
        }
    }
    Ok(SetupOutcome::Written { doctor_failures: failures })
}

/// Current UTC time as RFC 3339 (`2026-10-18T09:30:00Z`).
pub fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Who is acknowledging: `git config user.email`, else `$USER`.
pub fn acknowledging_user() -> String {
    std::process::Command::new("git")
        .args(["config", "user.email"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("USER").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use super::super::fake::FakeTracker;
    use super::*;
    use crate::config::Route;
    use async_trait::async_trait;
    use std::collections::BTreeMap;

    /// FakeTracker plus a process template.
    struct SetupFake {
        inner: FakeTracker,
        process: Option<String>,
        states: Vec<String>,
    }

    #[async_trait]
    impl IssueTracker for SetupFake {
        fn name(&self) -> &str {
            self.inner.name()
        }
        async fn query(&self, f: &TrackerFilter) -> super::super::Result<Vec<super::super::RemoteIssue>> {
            if f.wiql.is_some() {
                return Ok(Vec::new());
            }
            self.inner.query(f).await
        }
        async fn get(&self, id: &str) -> super::super::Result<super::super::RemoteIssue> {
            self.inner.get(id).await
        }
        async fn create(&self, d: &super::super::IssueDraft) -> super::super::Result<super::super::RemoteIssue> {
            self.inner.create(d).await
        }
        async fn transition(
            &self,
            id: &str,
            to: crate::task::TaskStatus,
            rev: Option<u64>,
        ) -> super::super::Result<super::super::RemoteIssue> {
            self.inner.transition(id, to, rev).await
        }
        async fn comment(&self, id: &str, m: &str) -> super::super::Result<()> {
            self.inner.comment(id, m).await
        }
        async fn assign(&self, id: &str, w: Option<&str>) -> super::super::Result<()> {
            self.inner.assign(id, w).await
        }
        async fn valid_states(&self) -> super::super::Result<Option<Vec<String>>> {
            Ok(Some(self.states.clone()))
        }
        async fn process_template(&self) -> super::super::Result<Option<String>> {
            Ok(self.process.clone())
        }
    }

    struct Env {
        root: PathBuf,
        config: Config,
        config_path: PathBuf,
        opts: SetupOptions,
    }

    fn env(name: &str) -> Env {
        let root = std::env::temp_dir().join(format!("varda-setup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let repo = root.join("repo");
        let config = Config {
            defaults: crate::config::Defaults {
                operations_dir: root.join("ops").display().to_string(),
                sandbox: None,
                ..Default::default()
            },
            routes: vec![Route {
                glob: format!("{}/**", root.display()),
                agents: vec!["claude".to_owned()],
                sandbox: None,
                mounts: vec![],
                env: BTreeMap::new(),
                orchestration: None,
                verify: vec![],
                untrusted: false,
            }],
            agents: BTreeMap::new(),
            roles: BTreeMap::new(),
            git: crate::config::GitConfig { auto_commit: false },
            sandboxes: BTreeMap::new(),
            orchestration: crate::orchestration::OrchestrationPolicy::default(),
            include: Vec::new(),
            requires_commands: Vec::new(),
            requires_secrets: Vec::new(),
            trackers: Vec::new(),
        };
        let config_path = root.join("config.toml");
        std::fs::write(&config_path, "# my comment\n[defaults]\noperations_dir = \"ops\"\n").unwrap();
        let opts = SetupOptions {
            org: "acme".to_owned(),
            project: "Proj".to_owned(),
            repo,
            name: None,
            tag: "varda".to_owned(),
            credential: Some(CredentialConfig {
                from_fnox: Some("azdo/pat".to_owned()),
                ..CredentialConfig::default()
            }),
            work_item_type: "Task".to_owned(),
            yes: true,
            force: false,
            by: "me@example.com".to_owned(),
            at: "2026-10-18T09:30:00Z".to_owned(),
        };
        Env { root, config, config_path, opts }
    }

    fn factory(process: Option<&str>) -> impl Fn(&TrackerConfig) -> Result<Box<dyn IssueTracker>> {
        let process = process.map(str::to_owned);
        move |t: &TrackerConfig| {
            let fake = FakeTracker::new(t.clone());
            fake.insert("New", "tagged item", &["varda"]);
            fake.insert("New", "other item", &[]);
            Ok(Box::new(SetupFake {
                inner: fake,
                process: process.clone(),
                states: ["New", "Active", "Resolved", "Closed", "Removed"].map(str::to_owned).to_vec(),
            }) as Box<dyn IssueTracker>)
        }
    }

    #[tokio::test]
    async fn happy_path_with_yes_appends_block_and_keeps_comments() {
        let e = env("happy");
        let mut io = ScriptedIo::default();
        let f = factory(Some("Agile"));
        let out = run_setup(&e.config, &e.config_path, &e.opts, &mut io, &f).await.unwrap();
        assert_eq!(out, SetupOutcome::Written { doctor_failures: 0 }, "{:#?}", io.out);
        let text = std::fs::read_to_string(&e.config_path).unwrap();
        assert!(text.starts_with("# my comment\n[defaults]"), "{text}");
        assert!(text.contains("from_fnox = \"azdo/pat\""));
        assert!(text.contains("by = \"me@example.com\""));
        assert!(text.contains("agents = [\"claude\"]"));
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        let trackers: Vec<TrackerConfig> = parsed["trackers"].clone().try_into().unwrap();
        assert_eq!(trackers.len(), 1);
        assert_eq!(trackers[0].name, "azdo-proj");
        assert_eq!(trackers[0].status_map["done"], "Closed");
        assert_eq!(trackers[0].filter.tags, vec!["varda"]);
        assert_eq!(trackers[0].extra_str("organization"), Some("acme"));
        let joined = io.out.join("\n");
        assert!(joined.contains("1 work item(s) already carry `varda`"), "{joined}");
        assert!(joined.contains("dry run: pull would process 1 work item(s)"), "{joined}");
        assert!(joined.contains("`acme/Proj`"), "{joined}");
        std::fs::remove_dir_all(e.root).ok();
    }

    #[tokio::test]
    async fn declining_the_disclaimer_writes_nothing() {
        let mut e = env("decline");
        e.opts.yes = false;
        let before = std::fs::read_to_string(&e.config_path).unwrap();
        let mut io = ScriptedIo {
            answers: ["n".to_owned()].into(),
            ..ScriptedIo::default()
        };
        let f = factory(Some("Agile"));
        let out = run_setup(&e.config, &e.config_path, &e.opts, &mut io, &f).await.unwrap();
        assert_eq!(out, SetupOutcome::Declined);
        assert_eq!(std::fs::read_to_string(&e.config_path).unwrap(), before);
        std::fs::remove_dir_all(e.root).ok();
    }

    #[tokio::test]
    async fn existing_name_is_refused_unless_forced() {
        let mut e = env("exists");
        let f = factory(Some("Agile"));
        let mut io = ScriptedIo::default();
        run_setup(&e.config, &e.config_path, &e.opts, &mut io, &f).await.unwrap();
        let written = std::fs::read_to_string(&e.config_path).unwrap();
        let parsed: toml::Value = toml::from_str(&written).unwrap();
        e.config.trackers = parsed["trackers"].clone().try_into().unwrap();

        let err = run_setup(&e.config, &e.config_path, &e.opts, &mut io, &f).await.unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err:#}");
        assert_eq!(std::fs::read_to_string(&e.config_path).unwrap(), written);

        e.opts.force = true;
        e.opts.tag = "other".to_owned();
        run_setup(&e.config, &e.config_path, &e.opts, &mut io, &f).await.unwrap();
        let text = std::fs::read_to_string(&e.config_path).unwrap();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        let trackers: Vec<TrackerConfig> = parsed["trackers"].clone().try_into().unwrap();
        assert_eq!(trackers.len(), 1, "{text}");
        assert_eq!(trackers[0].filter.tags, vec!["other"]);
        assert!(text.starts_with("# my comment"));
        std::fs::remove_dir_all(e.root).ok();
    }

    #[tokio::test]
    async fn token_never_appears_in_written_config() {
        let mut e = env("token");
        e.opts.credential = Some(CredentialConfig {
            command: Some("az account get-access-token --query accessToken -o tsv".to_owned()),
            ..CredentialConfig::default()
        });
        let secret = "s3cret-pat-value";
        let f = factory(Some("Agile"));
        let mut io = ScriptedIo::default();
        run_setup(&e.config, &e.config_path, &e.opts, &mut io, &f).await.unwrap();
        let text = std::fs::read_to_string(&e.config_path).unwrap();
        assert!(!text.contains(secret));
        assert!(text.contains("command = \"az account get-access-token"));
        assert!(!io.out.join("\n").contains(secret));
        std::fs::remove_dir_all(e.root).ok();
    }

    #[tokio::test]
    async fn no_credential_with_yes_is_an_error_and_tty_asks() {
        let mut e = env("cred");
        e.opts.credential = None;
        let f = factory(Some("Agile"));
        let mut io = ScriptedIo::default();
        let err = run_setup(&e.config, &e.config_path, &e.opts, &mut io, &f).await.unwrap_err();
        assert!(err.to_string().contains("no credential given"));

        e.opts.yes = false;
        let mut io = ScriptedIo {
            tty: true,
            answers: ["2", "AZDO_PAT", "y"].map(str::to_owned).into(),
            ..ScriptedIo::default()
        };
        run_setup(&e.config, &e.config_path, &e.opts, &mut io, &f).await.unwrap();
        let text = std::fs::read_to_string(&e.config_path).unwrap();
        assert!(text.contains("from_env = \"AZDO_PAT\""), "{text}");
        std::fs::remove_dir_all(e.root).ok();
    }

    #[test]
    fn process_proposals() {
        let st = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(propose_status_map(Some("scrum"), None).0, "Scrum");
        assert_eq!(propose_status_map(Some("CMMI"), None).0, "CMMI");
        let (name, _, exact) = propose_status_map(Some("Inherited"), Some(&st(&["Proposed", "Active", "Resolved", "Closed"])));
        assert_eq!((name, exact), ("CMMI", false));
        let (name, _, exact) = propose_status_map(None, Some(&st(&["To Do", "Doing", "Done"])));
        assert_eq!((name, exact), ("Basic", false));
        assert_eq!(propose_status_map(None, None).0, "Agile");
    }

    #[test]
    fn rfc3339_shape_and_epoch_math() {
        let s = now_rfc3339();
        assert_eq!(s.len(), 20);
        assert!(s.ends_with('Z') && s.starts_with("20"));
    }

    #[test]
    fn remove_block_keeps_other_trackers_and_text() {
        let text = "# c\n[[trackers]]\nname = \"a\"\n[trackers.status_map]\nbacklog = \"New\"\n\n[[trackers]]\nname = \"b\"\n\n[defaults]\nx = 1\n";
        let out = remove_tracker_block(text, "a").unwrap();
        assert!(out.contains("name = \"b\"") && !out.contains("name = \"a\"") && !out.contains("backlog"));
        assert!(out.contains("[defaults]"));
        assert!(remove_tracker_block(text, "zzz").is_none());
    }

    #[tokio::test]
    async fn filter_states_do_not_exclude_done() {
        let e = env("states");
        let mut io = ScriptedIo::default();
        let f = factory(Some("Agile"));
        run_setup(&e.config, &e.config_path, &e.opts, &mut io, &f).await.unwrap();
        let text = std::fs::read_to_string(&e.config_path).unwrap();
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        let trackers: Vec<TrackerConfig> = parsed["trackers"].clone().try_into().unwrap();
        let t = &trackers[0];
        assert!(t.filter.states.is_empty(), "{:?}", t.filter.states);
        // A remotely Closed item still matches the filter, so pull can see it.
        let fake = FakeTracker::new(t.clone());
        fake.insert("Closed", "finished", &["varda"]);
        assert_eq!(fake.query(&t.filter).await.unwrap().len(), 1);
        std::fs::remove_dir_all(e.root).ok();
    }

    const SECRET: &str = "s3cret-from-stderr";

    #[tokio::test]
    async fn credential_failure_does_not_print_the_error_chain() {
        let e = env("credfail");
        let mut io = ScriptedIo::default();
        let f = |_: &TrackerConfig| -> Result<Box<dyn IssueTracker>> {
            Err(anyhow::anyhow!("command failed: PAT={SECRET}"))
        };
        let err = run_setup(&e.config, &e.config_path, &e.opts, &mut io, &f).await.unwrap_err();
        assert!(!format!("{err:#}").contains(SECRET));
        assert!(!io.out.join("\n").contains(SECRET), "{:#?}", io.out);
        std::fs::remove_dir_all(e.root).ok();
    }

    /// Backend whose selected calls fail with an error containing SECRET.
    struct Failing {
        inner: SetupFake,
        auth: bool,
        wiql: bool,
        tagged: bool,
    }

    #[async_trait]
    impl IssueTracker for Failing {
        fn name(&self) -> &str {
            self.inner.name()
        }
        async fn check_auth(&self) -> super::super::Result<()> {
            if self.auth {
                return Err(super::super::TrackerError::Other(anyhow::anyhow!("401 body {SECRET}")));
            }
            Ok(())
        }
        async fn query(&self, f: &TrackerFilter) -> super::super::Result<Vec<super::super::RemoteIssue>> {
            if (f.wiql.is_some() && self.wiql) || (f.wiql.is_none() && self.tagged) {
                return Err(super::super::TrackerError::Other(anyhow::anyhow!("boom {SECRET}")));
            }
            self.inner.query(f).await
        }
        async fn get(&self, id: &str) -> super::super::Result<super::super::RemoteIssue> {
            self.inner.get(id).await
        }
        async fn create(&self, d: &super::super::IssueDraft) -> super::super::Result<super::super::RemoteIssue> {
            self.inner.create(d).await
        }
        async fn transition(
            &self,
            id: &str,
            to: crate::task::TaskStatus,
            rev: Option<u64>,
        ) -> super::super::Result<super::super::RemoteIssue> {
            self.inner.transition(id, to, rev).await
        }
        async fn comment(&self, id: &str, m: &str) -> super::super::Result<()> {
            self.inner.comment(id, m).await
        }
        async fn assign(&self, id: &str, w: Option<&str>) -> super::super::Result<()> {
            self.inner.assign(id, w).await
        }
        async fn valid_states(&self) -> super::super::Result<Option<Vec<String>>> {
            self.inner.valid_states().await
        }
        async fn process_template(&self) -> super::super::Result<Option<String>> {
            self.inner.process_template().await
        }
    }

    fn failing(auth: bool, wiql: bool, tagged: bool) -> impl Fn(&TrackerConfig) -> Result<Box<dyn IssueTracker>> {
        move |t: &TrackerConfig| {
            let fake = FakeTracker::new(t.clone());
            fake.insert("New", "tagged item", &["varda"]);
            Ok(Box::new(Failing {
                inner: SetupFake {
                    inner: fake,
                    process: Some("Agile".to_owned()),
                    states: ["New", "Active", "Closed"].map(str::to_owned).to_vec(),
                },
                auth,
                wiql,
                tagged,
            }) as Box<dyn IssueTracker>)
        }
    }

    #[tokio::test]
    async fn auth_and_read_failures_do_not_print_error_bodies() {
        for (auth, wiql) in [(true, false), (false, true)] {
            let e = env("authfail");
            let mut io = ScriptedIo::default();
            let f = failing(auth, wiql, false);
            let err = run_setup(&e.config, &e.config_path, &e.opts, &mut io, &f).await.unwrap_err();
            assert!(!format!("{err:#}").contains(SECRET));
            assert!(!io.out.join("\n").contains(SECRET), "{:#?}", io.out);
            std::fs::remove_dir_all(e.root).ok();
        }
    }

    /// `check_auth` succeeds on the first call (setup's own probe) and fails
    /// afterwards (the post-write doctor) with an error containing SECRET.
    struct LateAuth {
        inner: SetupFake,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl IssueTracker for LateAuth {
        fn name(&self) -> &str {
            self.inner.name()
        }
        async fn check_auth(&self) -> super::super::Result<()> {
            if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 1 {
                return Err(super::super::TrackerError::Other(anyhow::anyhow!("401 body {SECRET}")));
            }
            Ok(())
        }
        async fn query(&self, f: &TrackerFilter) -> super::super::Result<Vec<super::super::RemoteIssue>> {
            self.inner.query(f).await
        }
        async fn get(&self, id: &str) -> super::super::Result<super::super::RemoteIssue> {
            self.inner.get(id).await
        }
        async fn create(&self, d: &super::super::IssueDraft) -> super::super::Result<super::super::RemoteIssue> {
            self.inner.create(d).await
        }
        async fn transition(
            &self,
            id: &str,
            to: crate::task::TaskStatus,
            rev: Option<u64>,
        ) -> super::super::Result<super::super::RemoteIssue> {
            self.inner.transition(id, to, rev).await
        }
        async fn comment(&self, id: &str, m: &str) -> super::super::Result<()> {
            self.inner.comment(id, m).await
        }
        async fn assign(&self, id: &str, w: Option<&str>) -> super::super::Result<()> {
            self.inner.assign(id, w).await
        }
        async fn valid_states(&self) -> super::super::Result<Option<Vec<String>>> {
            self.inner.valid_states().await
        }
        async fn process_template(&self) -> super::super::Result<Option<String>> {
            self.inner.process_template().await
        }
    }

    #[tokio::test]
    async fn late_doctor_failure_does_not_print_error_body() {
        let e = env("lateauth");
        let mut io = ScriptedIo::default();
        let f = |t: &TrackerConfig| -> Result<Box<dyn IssueTracker>> {
            let fake = FakeTracker::new(t.clone());
            fake.insert("New", "tagged item", &["varda"]);
            Ok(Box::new(LateAuth {
                inner: SetupFake {
                    inner: fake,
                    process: Some("Agile".to_owned()),
                    states: ["New", "Active", "Closed"].map(str::to_owned).to_vec(),
                },
                calls: std::sync::atomic::AtomicUsize::new(0),
            }) as Box<dyn IssueTracker>)
        };
        let out = run_setup(&e.config, &e.config_path, &e.opts, &mut io, &f).await.unwrap();
        match out {
            SetupOutcome::Written { doctor_failures } => assert!(doctor_failures >= 1, "{:#?}", io.out),
            other => panic!("unexpected outcome {other:?}"),
        }
        assert!(!io.out.join("\n").contains(SECRET), "{:#?}", io.out);
        assert!(io.out.iter().any(|l| l.contains("doctor auth: failed")), "{:#?}", io.out);
        std::fs::remove_dir_all(e.root).ok();
    }

    #[tokio::test]
    async fn dry_run_errors_abort_without_writing() {
        let e = env("dryerrors");
        let before = std::fs::read(&e.config_path).unwrap();
        let mut io = ScriptedIo::default();
        let config = e.config.clone();
        // Two local tasks claim the tagged item's external id, so the dry run
        // reports a per-item error instead of failing outright.
        let f = move |t: &TrackerConfig| -> Result<Box<dyn IssueTracker>> {
            let fake = FakeTracker::new(t.clone());
            let item = fake.insert("New", "tagged item", &["varda"]);
            for name in ["dup a", "dup b"] {
                let path = crate::task::create_task(&config, name, Path::new(&t.project_path), None, None, None)
                    .unwrap();
                let mut doc = crate::task::load_task(&path).unwrap();
                doc.frontmatter.external = Some(crate::task::ExternalRef {
                    backend: t.name.clone(),
                    id: item.id.clone(),
                    url: String::new(),
                    rev: Some(0),
                    synced_status: None,
                    remote_state: None,
                    sync_error: None,
                });
                crate::task::write_task(&doc).unwrap();
            }
            Ok(Box::new(SetupFake {
                inner: fake,
                process: Some("Agile".to_owned()),
                states: ["New", "Active", "Closed"].map(str::to_owned).to_vec(),
            }) as Box<dyn IssueTracker>)
        };
        let err = run_setup(&e.config, &e.config_path, &e.opts, &mut io, &f).await.unwrap_err();
        assert!(err.to_string().contains("dry run reported errors"), "{err:#}");
        assert_eq!(std::fs::read(&e.config_path).unwrap(), before);
        std::fs::remove_dir_all(e.root).ok();
    }

    #[tokio::test]
    async fn failed_tagged_query_aborts_without_writing() {
        let e = env("tagfail");
        let before = std::fs::read(&e.config_path).unwrap();
        let mut io = ScriptedIo::default();
        let f = failing(false, false, true);
        let err = run_setup(&e.config, &e.config_path, &e.opts, &mut io, &f).await.unwrap_err();
        assert!(err.to_string().contains("nothing written"), "{err:#}");
        assert!(!io.out.join("\n").contains(SECRET));
        assert_eq!(std::fs::read(&e.config_path).unwrap(), before);
        std::fs::remove_dir_all(e.root).ok();
    }

    #[tokio::test]
    async fn unknown_process_asks_and_declining_aborts() {
        let mut e = env("unknown");
        e.opts.yes = false;
        let before = std::fs::read(&e.config_path).unwrap();
        let mut io = ScriptedIo {
            answers: ["n".to_owned()].into(),
            ..ScriptedIo::default()
        };
        let f = factory(Some("Inherited"));
        let err = run_setup(&e.config, &e.config_path, &e.opts, &mut io, &f).await.unwrap_err();
        assert!(err.to_string().contains("not confirmed"), "{err:#}");
        assert!(io.out.iter().any(|l| l.contains("mapping above?")), "{:#?}", io.out);
        assert_eq!(std::fs::read(&e.config_path).unwrap(), before);
        std::fs::remove_dir_all(e.root).ok();
    }

    #[test]
    fn remove_block_accepts_commented_headers() {
        let text = "[[trackers]] # production\nname = \"a\"\n[trackers.status_map] # map\nbacklog = \"New\"\n\n[[trackers]]   # other\nname = \"b\"\n";
        let out = remove_tracker_block(text, "a").unwrap();
        assert!(!out.contains("name = \"a\"") && !out.contains("backlog"), "{out}");
        assert!(out.contains("name = \"b\""), "{out}");
        assert!(!header_is("[[trackers]]x", "[[trackers]]"));
    }
    /// FakeTracker plus a label catalogue.
    struct LabelFake {
        inner: FakeTracker,
        labels: std::sync::Mutex<Vec<String>>,
        deny_create: bool,
    }

    #[async_trait]
    impl IssueTracker for LabelFake {
        fn name(&self) -> &str {
            self.inner.name()
        }
        async fn query(&self, f: &TrackerFilter) -> super::super::Result<Vec<super::super::RemoteIssue>> {
            self.inner.query(f).await
        }
        async fn get(&self, id: &str) -> super::super::Result<super::super::RemoteIssue> {
            self.inner.get(id).await
        }
        async fn create(&self, d: &super::super::IssueDraft) -> super::super::Result<super::super::RemoteIssue> {
            self.inner.create(d).await
        }
        async fn transition(
            &self,
            id: &str,
            to: crate::task::TaskStatus,
            rev: Option<u64>,
        ) -> super::super::Result<super::super::RemoteIssue> {
            self.inner.transition(id, to, rev).await
        }
        async fn comment(&self, id: &str, m: &str) -> super::super::Result<()> {
            self.inner.comment(id, m).await
        }
        async fn assign(&self, id: &str, w: Option<&str>) -> super::super::Result<()> {
            self.inner.assign(id, w).await
        }
        async fn valid_states(&self) -> super::super::Result<Option<Vec<String>>> {
            Ok(Some(["open", "closed", "closed:completed", "closed:not_planned"].map(str::to_owned).to_vec()))
        }
        async fn missing_labels(&self, wanted: &[String]) -> super::super::Result<Vec<String>> {
            let have = self.labels.lock().unwrap();
            Ok(wanted.iter().filter(|w| !have.contains(w)).cloned().collect())
        }
        async fn create_labels(&self, names: &[String]) -> super::super::Result<()> {
            if self.deny_create {
                return Err(super::super::TrackerError::Other(anyhow::anyhow!("403 secret-body")));
            }
            self.labels.lock().unwrap().extend(names.iter().cloned());
            Ok(())
        }
    }

    fn gh_factory(existing: &[&str], deny_create: bool) -> impl Fn(&TrackerConfig) -> Result<Box<dyn IssueTracker>> {
        let existing: Vec<String> = existing.iter().map(|s| (*s).to_owned()).collect();
        move |t: &TrackerConfig| {
            let mut gh = t.clone();
            gh.status_map = status_map_from(GITHUB_STATUS_MAP);
            let fake = FakeTracker::new(gh);
            fake.insert("open", "tagged item", &["varda"]);
            let labels = LabelFake {
                inner: fake,
                labels: std::sync::Mutex::new(existing.clone()),
                deny_create,
            };
            Ok(Box::new(labels) as Box<dyn IssueTracker>)
        }
    }

    fn gh_opts(e: &Env) -> GithubSetupOptions {
        GithubSetupOptions {
            github_repo: "acme/widgets".to_owned(),
            api_base: None,
            project_path: e.opts.repo.clone(),
            name: None,
            label: "varda".to_owned(),
            credential: Some(CredentialConfig {
                command: Some("gh auth token".to_owned()),
                ..CredentialConfig::default()
            }),
            yes: true,
            force: false,
            by: "me@example.com".to_owned(),
            at: "2026-10-18T09:30:00Z".to_owned(),
        }
    }

    #[tokio::test]
    async fn github_happy_path_with_yes_creates_labels_and_writes_block() {
        let e = env("gh-happy");
        let mut io = ScriptedIo::default();
        let f = gh_factory(&["bug"], false);
        let out = run_setup_github(&e.config, &e.config_path, &gh_opts(&e), &mut io, &f).await.unwrap();
        assert_eq!(out, SetupOutcome::Written { doctor_failures: 0 }, "{:#?}", io.out);
        let text = std::fs::read_to_string(&e.config_path).unwrap();
        assert!(text.starts_with("# my comment\n[defaults]"), "{text}");
        assert!(text.contains("kind = \"github\"") && text.contains("repo = \"acme/widgets\""), "{text}");
        assert!(text.contains("command = \"gh auth token\""), "{text}");
        assert!(!text.contains("api_base"), "{text}");
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        let trackers: Vec<TrackerConfig> = parsed["trackers"].clone().try_into().unwrap();
        assert_eq!(trackers[0].name, "github-acme-widgets");
        assert_eq!(trackers[0].kind, TrackerKind::Github);
        assert_eq!(trackers[0].status_map["done"], "closed:completed");
        assert_eq!(trackers[0].status_map["running"], "open");
        assert_eq!(trackers[0].filter.tags, vec!["varda"]);
        assert!(trackers[0].filter.states.is_empty());
        assert_eq!(trackers[0].acknowledged.as_ref().unwrap().agents, vec!["claude"]);
        let joined = io.out.join("\n");
        assert!(joined.contains("created 4 label(s)"), "{joined}");
        assert!(joined.contains("dry run: pull would process 1 work item(s)"), "{joined}");
        assert!(joined.contains("`acme/widgets`") && joined.contains("Issue titles"), "{joined}");
        std::fs::remove_dir_all(e.root).ok();
    }

    #[tokio::test]
    async fn github_records_api_base_for_enterprise() {
        let e = env("gh-ghe");
        let mut opts = gh_opts(&e);
        opts.api_base = Some("https://ghe.example/api/v3".to_owned());
        let mut io = ScriptedIo::default();
        let f = gh_factory(&["varda", "varda:ready", "varda:needs-user", "varda:failed"], false);
        run_setup_github(&e.config, &e.config_path, &opts, &mut io, &f).await.unwrap();
        let text = std::fs::read_to_string(&e.config_path).unwrap();
        assert!(text.contains("api_base = \"https://ghe.example/api/v3\""), "{text}");
        assert!(io.out.join("\n").contains("pickup and helper labels exist"));
        std::fs::remove_dir_all(e.root).ok();
    }

    #[tokio::test]
    async fn github_without_yes_offers_labels_and_decline_leaves_them() {
        let e = env("gh-offer");
        let mut opts = gh_opts(&e);
        opts.yes = false;
        // tty: create-labels? n; disclaimer? y
        let mut io = ScriptedIo {
            tty: true,
            answers: ["n", "y"].map(str::to_owned).into(),
            ..ScriptedIo::default()
        };
        let f = gh_factory(&[], false);
        let out = run_setup_github(&e.config, &e.config_path, &opts, &mut io, &f).await.unwrap();
        assert!(matches!(out, SetupOutcome::Written { .. }));
        let joined = io.out.join("\n");
        assert!(joined.contains("missing labels: varda, varda:ready"), "{joined}");
        assert!(joined.contains("not created"), "{joined}");
        std::fs::remove_dir_all(e.root).ok();
    }

    #[tokio::test]
    async fn github_label_creation_failure_aborts_without_echoing_the_body() {
        let e = env("gh-deny");
        let mut io = ScriptedIo::default();
        let f = gh_factory(&[], true);
        let err = run_setup_github(&e.config, &e.config_path, &gh_opts(&e), &mut io, &f).await.unwrap_err();
        assert!(err.to_string().contains("label creation failed"), "{err}");
        assert!(!io.out.join("\n").contains("secret-body"));
        assert!(!std::fs::read_to_string(&e.config_path).unwrap().contains("trackers"));
        std::fs::remove_dir_all(e.root).ok();
    }

    #[tokio::test]
    async fn github_rejects_bad_repo_and_existing_name() {
        let e = env("gh-bad");
        let mut opts = gh_opts(&e);
        opts.github_repo = "not-a-slug".to_owned();
        let mut io = ScriptedIo::default();
        let f = gh_factory(&[], false);
        let err = run_setup_github(&e.config, &e.config_path, &opts, &mut io, &f).await.unwrap_err();
        assert!(err.to_string().contains("owner/name"), "{err}");

        let mut first = ScriptedIo::default();
        run_setup_github(&e.config, &e.config_path, &gh_opts(&e), &mut first, &f).await.unwrap();
        let mut config = e.config.clone();
        let parsed: toml::Value = toml::from_str(&std::fs::read_to_string(&e.config_path).unwrap()).unwrap();
        config.trackers = parsed["trackers"].clone().try_into().unwrap();
        let err = run_setup_github(&config, &e.config_path, &gh_opts(&e), &mut io, &f).await.unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");
        std::fs::remove_dir_all(e.root).ok();
    }

    #[test]
    fn jira_status_map_proposals() {
        let names = |v: &[&str]| v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let (m, exact) = propose_jira_status_map(&names(&["To Do", "in progress", "In Review", "Done"]));
        assert!(exact);
        assert_eq!(m["backlog"], "To Do");
        assert_eq!(m["running"], "in progress");
        assert_eq!(m["review"], "In Review");
        assert_eq!(m["done"], "Done");
        // No review status: review shares the running one.
        let (m, exact) = propose_jira_status_map(&names(&["Open", "In Progress", "Closed"]));
        assert!(exact);
        assert_eq!((m["backlog"].as_str(), m["review"].as_str(), m["done"].as_str()), ("Open", "In Progress", "Closed"));
        // Custom workflow: positional guess, flagged.
        let (m, exact) = propose_jira_status_map(&names(&["Intake", "Building", "Shipped"]));
        assert!(!exact);
        assert_eq!((m["backlog"].as_str(), m["running"].as_str(), m["done"].as_str()), ("Intake", "Building", "Shipped"));
    }

    #[tokio::test]
    async fn jira_setup_happy_path_writes_a_jira_block_without_the_token() {
        let e = env("jira-happy");
        let opts = JiraSetupOptions {
            site: "https://acme.atlassian.net/".to_owned(),
            project_key: "VAR".to_owned(),
            email: "me@example.com".to_owned(),
            repo: e.opts.repo.clone(),
            name: None,
            label: "varda".to_owned(),
            credential: Some(CredentialConfig {
                from_env: Some("JIRA_TOKEN".to_owned()),
                ..CredentialConfig::default()
            }),
            issue_type: "Task".to_owned(),
            yes: true,
            force: false,
            by: "me@example.com".to_owned(),
            at: "2026-10-18T09:30:00Z".to_owned(),
        };
        let build = |t: &TrackerConfig| -> Result<Box<dyn IssueTracker>> {
            let fake = FakeTracker::new(t.clone());
            fake.insert("To Do", "labelled", &["varda"]);
            Ok(Box::new(SetupFake {
                inner: fake,
                process: None,
                states: ["To Do", "In Progress", "In Review", "Done"].map(str::to_owned).to_vec(),
            }) as Box<dyn IssueTracker>)
        };
        let mut io = ScriptedIo::default();
        let outcome = run_setup_jira(&e.config, &e.config_path, &opts, &mut io, &build).await.unwrap();
        assert_eq!(outcome, SetupOutcome::Written { doctor_failures: 0 });
        let written = std::fs::read_to_string(&e.config_path).unwrap();
        assert!(written.contains("# my comment"), "{written}");
        assert!(written.contains("kind = \"jira\"") && written.contains("name = \"jira-var\""), "{written}");
        assert!(written.contains("site = \"https://acme.atlassian.net\""), "{written}");
        assert!(written.contains("project_key = \"VAR\"") && written.contains("email = \"me@example.com\""), "{written}");
        assert!(written.contains("from_env = \"JIRA_TOKEN\""), "{written}");
        assert!(written.contains("review = \"In Review\""), "{written}");
        assert!(written.contains("acknowledged"), "{written}");
        let _ = std::fs::remove_dir_all(&e.root);
    }
}
