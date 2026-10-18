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
    IssueTracker, status_map_from,
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
    let who: Vec<String> = agents
        .iter()
        .map(|a| format!("`{a}`, {}", agent_provider(config, a)))
        .collect();
    format!(
        "Work item titles, descriptions, acceptance criteria and comments from\n\
         `{org}/{project}` will be sent to the configured agent ({}) as\n\
         prompt content, and run recaps will be posted back to those work items.\n\
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
    out.push_str(&format!("name = {}\nkind = \"azdo\"\n", toml_str(&t.name)));
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
        None => {
            io.say("Where does the Azure DevOps PAT live?");
            io.say("  1) fnox key   2) environment variable   3) command (e.g. az account get-access-token ...)");
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

    // 4. Dry run (reuses the pull dry-run). A provisional acknowledgement lets
    // the pull gate pass; it is not persisted unless confirmed below.
    let ack = TrackerAck {
        by: opts.by.clone(),
        at: opts.at.clone(),
        agents: agents.clone(),
    };
    let mut dry = draft.clone();
    dry.acknowledged = Some(ack.clone());
    match super::sync::pull_tracker(config, &dry, backend.as_ref(), true).await {
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

    // 5. Disclaimer + acknowledgement.
    io.say("");
    io.say(&disclaimer_text(config, &opts.org, &opts.project, &agents));
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
            // The raw reason may carry response bodies; never print it here.
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
}
