//! `varda doctor`: which isolation backends work on THIS host, and whether every
//! configured `[sandboxes.*]` entry can actually launch here.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;

use crate::config::{self, Config, SandboxConfig};
use crate::sandbox;

/// One host probe can hang on a wedged daemon or VM; never let it hold the command.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// Backends `varda doctor` probes, in report order. The `docker` primitive is
/// split by engine because docker and podman are installed and fail independently.
const BACKENDS: &[&str] = &[
    "local",
    "docker",
    "podman",
    "microsandbox",
    "processcontainer",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Available(String),
    Unavailable(String),
}

impl Availability {
    fn render(&self) -> String {
        match self {
            Self::Available(detail) => format!("available — {detail}"),
            Self::Unavailable(reason) => format!("unavailable — {reason}"),
        }
    }
}

/// The backend a configured sandbox launches through: its primitive, or for the
/// `docker` primitive, its engine CLI.
pub fn sandbox_backend(name: &str, config: &SandboxConfig) -> Result<String> {
    if config.primitive == "docker" {
        sandbox::docker_engine(name, config)
    } else {
        Ok(config.primitive.clone())
    }
}

/// Per-sandbox verdicts against the probed backends. `Err` marks a sandbox that
/// cannot launch here (bad config or missing backend).
pub fn sandbox_verdicts(
    sandboxes: &BTreeMap<String, SandboxConfig>,
    backends: &BTreeMap<String, Availability>,
) -> Vec<(String, String, std::result::Result<(), String>)> {
    sandboxes
        .iter()
        .map(
            |(name, sandbox_config)| match sandbox_backend(name, sandbox_config) {
                Err(error) => (
                    name.clone(),
                    sandbox_config.primitive.clone(),
                    Err(format!("{error:#}")),
                ),
                Ok(backend) => {
                    let verdict = match backends.get(&backend) {
                        Some(Availability::Available(_)) => Ok(()),
                        Some(Availability::Unavailable(reason)) => Err(reason.clone()),
                        None => Err(format!("unknown primitive '{backend}'")),
                    };
                    (name.clone(), backend, verdict)
                }
            },
        )
        .collect()
}

pub async fn doctor_host_command() -> Result<()> {
    let mut backends = BTreeMap::new();
    println!("isolation backends on this host:");
    for backend in BACKENDS {
        let availability = probe_backend(backend).await;
        println!("  {backend:<17} {}", availability.render());
        backends.insert((*backend).to_owned(), availability);
    }

    let config = match config::config_file().and_then(|path| config::resolve_config(&path)) {
        Ok(config) => config,
        Err(error) => {
            println!();
            println!("configured sandboxes: not checked — {error:#}");
            return Ok(());
        }
    };
    let sandboxes = report_sandboxes(&config, &backends);
    let tracker_failures = crate::tracker::doctor::report(&config).await;
    sandboxes?;
    if tracker_failures > 0 {
        anyhow::bail!("{tracker_failures} tracker check(s) failed");
    }
    Ok(())
}

fn report_sandboxes(config: &Config, backends: &BTreeMap<String, Availability>) -> Result<()> {
    println!();
    if config.sandboxes.is_empty() {
        println!("configured sandboxes: none");
        return Ok(());
    }
    println!("configured sandboxes:");
    let verdicts = sandbox_verdicts(&config.sandboxes, backends);
    let mut broken = 0;
    for (name, backend, verdict) in &verdicts {
        match verdict {
            Ok(()) => println!("  {name:<17} ok ({backend})"),
            Err(reason) => {
                broken += 1;
                println!("  {name:<17} cannot run ({backend}): {reason}");
            }
        }
    }
    if broken > 0 {
        anyhow::bail!(
            "{broken} of {} configured sandboxes cannot run on this host",
            verdicts.len()
        );
    }
    Ok(())
}

async fn probe_backend(backend: &str) -> Availability {
    match backend {
        "local" => Availability::Available("no isolation".to_owned()),
        "docker" => {
            probe_engine(
                "docker",
                &["info", "--format", "{{.ServerVersion}}"],
                "server",
            )
            .await
        }
        "podman" => {
            probe_engine(
                "podman",
                &["info", "--format", "{{.Version.Version}}"],
                "podman",
            )
            .await
        }
        "microsandbox" => probe_microsandbox().await,
        "processcontainer" => probe_processcontainer().await,
        other => Availability::Unavailable(format!("unknown backend '{other}'")),
    }
}

/// Run `program args` with a bounded wait. `Ok(stdout)` on success, `Err(reason)`
/// otherwise; a missing binary is reported as "not installed".
async fn run_probe(program: &str, args: &[&str]) -> std::result::Result<String, String> {
    let child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(PROBE_TIMEOUT, child).await {
        Err(_) => Err(format!(
            "`{program}` did not answer within {}s",
            PROBE_TIMEOUT.as_secs()
        )),
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(format!("`{program}` is not installed (not on PATH)"))
        }
        Ok(Err(error)) => Err(format!("failed to run `{program}`: {error}")),
        Ok(Ok(output)) if output.status.success() => {
            Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
        }
        Ok(Ok(output)) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let first = stderr
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("")
                .trim();
            Err(format!(
                "`{program} {}` failed: {first}",
                args.first().unwrap_or(&"")
            ))
        }
    }
}

async fn probe_engine(program: &str, args: &[&str], label: &str) -> Availability {
    match run_probe(program, args).await {
        Ok(version) => Availability::Available(format!("{label} {version}")),
        Err(reason) => Availability::Unavailable(reason),
    }
}

async fn probe_microsandbox() -> Availability {
    if cfg!(windows) {
        return Availability::Unavailable("microsandbox does not run on Windows".to_owned());
    }
    match run_probe("msb", &["--version"]).await {
        Ok(version) => Availability::Available(version),
        Err(reason) => Availability::Unavailable(reason),
    }
}

#[cfg(windows)]
async fn probe_processcontainer() -> Availability {
    let probe = tokio::task::spawn_blocking(|| mxc_sdk::v1::probe(None)).await;
    let output = match probe {
        Err(error) => return Availability::Unavailable(format!("MXC probe panicked: {error}")),
        Ok(Err(error)) => return Availability::Unavailable(format!("MXC probe failed: {error}")),
        Ok(Ok(output)) => output,
    };
    match serde_json::to_value(&output) {
        Ok(value) => processcontainer_verdict(&value),
        Err(error) => Availability::Unavailable(format!("unreadable MXC probe: {error}")),
    }
}

#[cfg(not(windows))]
async fn probe_processcontainer() -> Availability {
    Availability::Unavailable("Windows only (MXC)".to_owned())
}

/// Interpret MXC's probe JSON. varda needs a BaseContainer tier (no admin) and,
/// for egress through its loopback proxy, the identity-less loopback proxy
/// (microsoft/mxc#1490); without the latter only fully offline sandboxes work.
#[cfg_attr(not(windows), allow(dead_code))]
fn processcontainer_verdict(value: &serde_json::Value) -> Availability {
    if let Some(error) = value.get("error").and_then(|e| e.as_str()) {
        return Availability::Unavailable(format!("MXC: {error}"));
    }
    let probes = value.get("probes");
    let fact = |key: &str| probes.and_then(|p| p.get(key)).and_then(|v| v.as_bool());
    if fact("baseContainerApiPresent") == Some(false) {
        return Availability::Unavailable(
            "BaseContainer API not present on this Windows build".to_owned(),
        );
    }
    let tier = value
        .get("tier")
        .and_then(|t| t.as_str())
        .unwrap_or("unknown tier");
    let proxy = match fact("baseContainerSupportsIdentitylessLoopbackProxy") {
        Some(true) => "egress proxy supported",
        Some(false) => "no identity-less loopback proxy: only offline sandboxes (egress = []) work",
        None => "egress proxy support not reported",
    };
    Availability::Available(format!("tier {tier}, {proxy}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(primitive: &str, engine: Option<&str>) -> SandboxConfig {
        SandboxConfig {
            primitive: primitive.to_owned(),
            engine: engine.map(str::to_owned),
            ..SandboxConfig::default()
        }
    }

    #[test]
    fn docker_sandboxes_resolve_to_their_engine() {
        assert_eq!(
            sandbox_backend("a", &sandbox("docker", None)).unwrap(),
            "docker"
        );
        assert_eq!(
            sandbox_backend("a", &sandbox("docker", Some("podman"))).unwrap(),
            "podman"
        );
        assert_eq!(
            sandbox_backend("a", &sandbox("microsandbox", None)).unwrap(),
            "microsandbox"
        );
        assert!(sandbox_backend("a", &sandbox("docker", Some("nerdctl"))).is_err());
    }

    #[test]
    fn verdicts_follow_backend_availability() {
        let backends = BTreeMap::from([
            (
                "docker".to_owned(),
                Availability::Unavailable("daemon down".to_owned()),
            ),
            (
                "podman".to_owned(),
                Availability::Available("podman 5.2".to_owned()),
            ),
        ]);
        let sandboxes = BTreeMap::from([
            ("d".to_owned(), sandbox("docker", None)),
            ("p".to_owned(), sandbox("docker", Some("podman"))),
            ("x".to_owned(), sandbox("firecracker", None)),
        ]);
        let verdicts = sandbox_verdicts(&sandboxes, &backends);
        assert_eq!(
            verdicts[0],
            (
                "d".to_owned(),
                "docker".to_owned(),
                Err("daemon down".to_owned())
            )
        );
        assert_eq!(verdicts[1], ("p".to_owned(), "podman".to_owned(), Ok(())));
        assert!(
            verdicts[2]
                .2
                .as_ref()
                .unwrap_err()
                .contains("unknown primitive")
        );
    }

    #[test]
    fn processcontainer_verdict_reads_mxc_probe() {
        let ok = serde_json::json!({
            "tier": "baseContainer",
            "probes": {"baseContainerApiPresent": true, "baseContainerSupportsIdentitylessLoopbackProxy": true}
        });
        assert_eq!(
            processcontainer_verdict(&ok),
            Availability::Available("tier baseContainer, egress proxy supported".to_owned())
        );
        let old = serde_json::json!({
            "tier": "baseContainer",
            "probes": {"baseContainerApiPresent": true, "baseContainerSupportsIdentitylessLoopbackProxy": false}
        });
        assert!(
            matches!(processcontainer_verdict(&old), Availability::Available(d) if d.contains("offline"))
        );
        let missing = serde_json::json!({"probes": {"baseContainerApiPresent": false}});
        assert!(matches!(
            processcontainer_verdict(&missing),
            Availability::Unavailable(_)
        ));
        let failed = serde_json::json!({"error": "detector failed", "probes": {}});
        assert!(matches!(
            processcontainer_verdict(&failed),
            Availability::Unavailable(_)
        ));
    }
}
