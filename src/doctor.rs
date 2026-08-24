use std::path::Path;
use std::process::{Command, Output};

use anyhow::Result;

use crate::{config, sandbox, task};

#[derive(Debug, PartialEq, Eq)]
enum Probe {
    Yes(String),
    No(String),
    Unknown(String),
}

impl Probe {
    fn render(&self) -> String {
        match self {
            Self::Yes(detail) => format!("yes — {detail}"),
            Self::No(detail) => format!("no — {detail}"),
            Self::Unknown(reason) => format!("unknown — {reason}"),
        }
    }
}

trait LogAuthority {
    fn logs(&self, source: &str, box_name: &str) -> std::io::Result<Output>;
}

struct Msb;
impl LogAuthority for Msb {
    fn logs(&self, source: &str, box_name: &str) -> std::io::Result<Output> {
        Command::new("msb")
            .args(["logs", "--source", source, box_name])
            .output()
    }
}

fn source_log(
    authority: &dyn LogAuthority,
    source: &str,
    box_name: &str,
) -> Result<String, String> {
    let output = authority
        .logs(source, box_name)
        .map_err(|error| format!("could not invoke msb: {error}"))?;
    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if reason.is_empty() {
            format!("msb logs exited with {}", output.status)
        } else {
            format!("msb logs failed: {reason}")
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn boot_probe(authority: &dyn LogAuthority, box_name: &str) -> Probe {
    match source_log(authority, "system", box_name) {
        Ok(log) if log.contains("agent relay: client connected") => {
            Probe::Yes("sandbox agent relay connected".to_owned())
        }
        Ok(log) if log.contains("entering VM") => Probe::Unknown(
            "sandbox entered the VM but the relay has not connected; it may still be booting"
                .to_owned(),
        ),
        Ok(_) => Probe::Unknown("system log has no recognized boot marker".to_owned()),
        Err(reason) => Probe::Unknown(reason),
    }
}

fn output_probe(authority: &dyn LogAuthority, box_name: &str) -> Probe {
    match (
        source_log(authority, "stdout", box_name),
        source_log(authority, "stderr", box_name),
    ) {
        (Ok(stdout), Ok(stderr)) if stdout.is_empty() && stderr.is_empty() => {
            Probe::No("sandbox stdout and stderr are empty".to_owned())
        }
        (Ok(stdout), Ok(stderr)) => Probe::Yes(format!(
            "{} stdout bytes, {} stderr bytes",
            stdout.len(),
            stderr.len()
        )),
        (Err(a), Err(b)) if a == b => Probe::Unknown(a),
        (Err(a), Err(b)) => Probe::Unknown(format!("stdout: {a}; stderr: {b}")),
        (Err(reason), _) => Probe::Unknown(format!("stdout unavailable: {reason}")),
        (_, Err(reason)) => Probe::Unknown(format!("stderr unavailable: {reason}")),
    }
}

fn end_cause(log: Result<&str, &str>) -> Probe {
    let log = match log {
        Ok(log) => log,
        Err(reason) => return Probe::Unknown(reason.to_owned()),
    };
    if log.contains("\nbudget:\n") {
        Probe::Yes("budget-expired".to_owned())
    } else if log.contains("\nidle_watchdog:\n") {
        Probe::Yes("truncated by idle watchdog".to_owned())
    } else if log.lines().any(|line| line == "status=exit status: 0") {
        Probe::Yes("completed (sandbox command exited successfully)".to_owned())
    } else if log
        .lines()
        .any(|line| line.starts_with("status=exit status:"))
    {
        Probe::Unknown("sandbox command exited unsuccessfully; the log does not identify whether the box or agent failed".to_owned())
    } else {
        Probe::Unknown("run has no terminal status yet".to_owned())
    }
}

pub fn doctor_task_command(task_ref: &Path) -> Result<()> {
    let config = config::load_config(&config::config_file()?)?;
    let task_path = task::resolve_task_reference(&config, task_ref)?;
    let document = task::load_task(&task_path)?;
    let Some(session_id) = document.frontmatter.agent_session_ids.last() else {
        for name in ["booted", "agent output", "end cause"] {
            println!("{name}: unknown — task has no recorded run");
        }
        return Ok(());
    };
    let box_name = format!("varda-sbx-{}", sandbox::sanitize_session_handle(session_id));
    let project = document
        .frontmatter
        .project
        .as_deref()
        .map(Path::new)
        .unwrap_or(&task_path);
    let policy_path = document
        .frontmatter
        .mother_project
        .as_deref()
        .map(Path::new)
        .unwrap_or(project);
    let routing_root = crate::routing_root_for(policy_path);
    let provider = config.resolve_sandbox_for(
        policy_path,
        &routing_root,
        document.frontmatter.sandbox.as_deref(),
    );
    let log = document
        .frontmatter
        .agent_session_logs
        .last()
        .ok_or_else(|| "task has no session log path".to_owned())
        .and_then(|path| std::fs::read_to_string(path).map_err(|error| error.to_string()));
    println!("task: {}", task_path.display());
    println!("session: {session_id}");
    match provider {
        Ok(provider) if provider.config.primitive == "microsandbox" => {
            let authority = Msb;
            println!("box: {box_name}");
            println!("booted: {}", boot_probe(&authority, &box_name).render());
            println!(
                "agent output: {}",
                output_probe(&authority, &box_name).render()
            );
        }
        Ok(provider) => {
            println!(
                "box: unknown — provider `{}` has no log probe",
                provider.config.primitive
            );
            println!("booted: unknown — provider authority is not microsandbox");
            println!("agent output: unknown — provider authority is not microsandbox");
        }
        Err(error) => {
            println!("box: unknown — could not resolve provider: {error}");
            println!("booted: unknown — could not resolve provider authority");
            println!("agent output: unknown — could not resolve provider authority");
        }
    }
    let log_ref = log.as_ref().map(String::as_str).map_err(String::as_str);
    println!("end cause: {}", end_cause(log_ref).render());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    struct Fake(Option<(&'static str, &'static str)>);
    impl LogAuthority for Fake {
        fn logs(&self, source: &str, _box_name: &str) -> std::io::Result<Output> {
            let Some((wanted, text)) = self.0 else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "msb unavailable",
                ));
            };
            Ok(Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: if wanted == source {
                    text.as_bytes().to_vec()
                } else {
                    vec![]
                },
                stderr: vec![],
            })
        }
    }

    #[test]
    fn boot_states_and_unavailable_negative_control() {
        assert!(matches!(
            boot_probe(
                &Fake(Some(("system", "agent relay: client connected"))),
                "box"
            ),
            Probe::Yes(_)
        ));
        assert!(matches!(
            boot_probe(&Fake(Some(("system", "entering VM"))), "box"),
            Probe::Unknown(_)
        ));
        assert!(matches!(boot_probe(&Fake(None), "box"), Probe::Unknown(_)));
    }

    #[test]
    fn output_states_and_unavailable_negative_control() {
        assert!(matches!(
            output_probe(&Fake(Some(("stdout", "hello"))), "box"),
            Probe::Yes(_)
        ));
        assert!(matches!(
            output_probe(&Fake(Some(("stdout", ""))), "box"),
            Probe::No(_)
        ));
        assert!(matches!(
            output_probe(&Fake(None), "box"),
            Probe::Unknown(_)
        ));
    }

    #[test]
    fn end_cause_is_conservative() {
        assert!(matches!(
            end_cause(Ok("\nstatus=exit status: 0\n")),
            Probe::Yes(_)
        ));
        assert!(matches!(
            end_cause(Ok("\nstatus=exit status: 1\n")),
            Probe::Unknown(_)
        ));
        assert!(matches!(end_cause(Err("unreadable")), Probe::Unknown(_)));
        assert_eq!(
            end_cause(Ok("\nbudget:\nsoft ceiling")),
            Probe::Yes("budget-expired".to_owned())
        );
    }
}
