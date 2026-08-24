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
    /// Provider-reported lifecycle state for the box, e.g. `running` / `stopped` /
    /// `crashed`, or `None` when the provider no longer has a record. Needed to turn
    /// "the relay never connected" from a maybe into a verdict: a box that is still
    /// running may simply be slow to boot, while one the provider has already settled
    /// will never connect now.
    fn box_state(&self, box_name: &str) -> std::io::Result<Output>;
}

struct Msb;
impl LogAuthority for Msb {
    fn logs(&self, source: &str, box_name: &str) -> std::io::Result<Output> {
        Command::new("msb")
            .args(["logs", "--source", source, box_name])
            .output()
    }

    fn box_state(&self, _box_name: &str) -> std::io::Result<Output> {
        Command::new("msb").arg("ls").output()
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

/// Provider lifecycle state for `box_name`, parsed from the provider listing.
/// `Ok(None)` means the provider has no record of the box at all.
fn box_state(authority: &dyn LogAuthority, box_name: &str) -> Result<Option<String>, String> {
    let output = authority
        .box_state(box_name)
        .map_err(|error| format!("could not invoke msb: {error}"))?;
    if !output.status.success() {
        let reason = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if reason.is_empty() {
            format!("msb ls exited with {}", output.status)
        } else {
            format!("msb ls failed: {reason}")
        });
    }
    let listing = String::from_utf8_lossy(&output.stdout).into_owned();
    Ok(listing
        .lines()
        .find(|line| line.split_whitespace().next() == Some(box_name))
        .and_then(|line| line.split_whitespace().nth(2).map(str::to_owned)))
}

/// True once the provider has settled the box: it will never connect its relay now.
fn is_terminal_box_state(state: &str) -> bool {
    matches!(state, "stopped" | "crashed" | "failed" | "exited")
}

fn boot_probe(authority: &dyn LogAuthority, box_name: &str) -> Probe {
    match source_log(authority, "system", box_name) {
        Ok(log) if log.contains("agent relay: client connected") => {
            Probe::Yes("sandbox agent relay connected".to_owned())
        }
        Ok(log) if log.contains("entering VM") => {
            // The relay never connected. Whether that is "not yet" or "never" is not
            // knowable from the log alone — ask the provider whether the box has
            // already settled. Getting this wrong is expensive: a box that never
            // booted produces no stdout, no stderr and no exit status, which reads
            // exactly like an agent that ran and said nothing.
            match box_state(authority, box_name) {
                Ok(Some(state)) if is_terminal_box_state(&state) => Probe::No(format!(
                    "sandbox entered the VM but the relay never connected and the box is \
                     already {state}; nothing ran inside it"
                )),
                Ok(Some(state)) => Probe::Unknown(format!(
                    "sandbox entered the VM but the relay has not connected; box is {state}, \
                     so it may still be booting"
                )),
                Ok(None) => Probe::Unknown(
                    "sandbox entered the VM but the relay has not connected, and the provider \
                     no longer has a record of the box"
                        .to_owned(),
                ),
                Err(reason) => Probe::Unknown(format!(
                    "sandbox entered the VM but the relay has not connected; box state \
                     unavailable: {reason}"
                )),
            }
        }
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

    /// Fake with a provider listing too, so the boot probe's terminal-state branch is
    /// exercised rather than inferred.
    struct FakeWithState(Option<(&'static str, &'static str)>, Option<&'static str>);

    impl LogAuthority for FakeWithState {
        fn logs(&self, source: &str, box_name: &str) -> std::io::Result<Output> {
            Fake(self.0).logs(source, box_name)
        }

        fn box_state(&self, _box_name: &str) -> std::io::Result<Output> {
            let listing = match self.1 {
                Some(state) => format!("NAME IMAGE STATUS CREATED\nbox img {state} now\n"),
                None => "NAME IMAGE STATUS CREATED\n".to_owned(),
            };
            Ok(Output {
                status: std::process::ExitStatus::from_raw(0),
                stdout: listing.into_bytes(),
                stderr: vec![],
            })
        }
    }

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

        fn box_state(&self, _box_name: &str) -> std::io::Result<Output> {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "msb unavailable",
            ))
        }
    }

    #[test]
    fn a_settled_box_that_never_connected_its_relay_is_a_definite_no() {
        // The case that matters: no stdout, no stderr, no exit status — identical to an
        // agent that ran and said nothing, unless the provider state is consulted.
        let never_booted = FakeWithState(Some(("system", "entering VM")), Some("crashed"));
        match boot_probe(&never_booted, "box") {
            Probe::No(detail) => {
                assert!(detail.contains("crashed"), "verdict names the state: {detail}");
                assert!(detail.contains("nothing ran"), "verdict is actionable: {detail}");
            }
            other => panic!("a settled box must be a definite no, got {other:?}"),
        }

        // Still running: genuinely unknowable, so it must NOT harden into a verdict.
        assert!(matches!(
            boot_probe(
                &FakeWithState(Some(("system", "entering VM")), Some("running")),
                "box"
            ),
            Probe::Unknown(_)
        ));

        // Provider has no record, and provider unavailable: both stay unknown (P2).
        assert!(matches!(
            boot_probe(&FakeWithState(Some(("system", "entering VM")), None), "box"),
            Probe::Unknown(_)
        ));
        assert!(matches!(
            boot_probe(&Fake(Some(("system", "entering VM"))), "box"),
            Probe::Unknown(_)
        ));
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
