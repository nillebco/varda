//! User notification support.

use std::fs;
use std::path::{Path, PathBuf};
#[cfg(all(target_os = "macos", not(test)))]
use std::process::Command;

use anyhow::{Context, Result};
use serde::Serialize;
use uuid::Uuid;

use crate::config::Config;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NotificationRecord {
    pub kind: NotificationKind,
    pub task_path: String,
    pub recap_path: String,
    pub message: String,
    pub signal: NotificationSignal,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    NeedsUser,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct NotificationSignal {
    pub title: String,
    pub body: String,
}

pub fn notify_user_interaction(
    config: &Config,
    task_path: &Path,
    recap_path: &Path,
) -> Result<PathBuf> {
    let signal = NotificationSignal {
        title: "Varda needs user input".to_string(),
        body: format!(
            "Task {} requires user interaction. See recap {}.",
            task_path.display(),
            recap_path.display()
        ),
    };
    let record = NotificationRecord {
        kind: NotificationKind::NeedsUser,
        task_path: task_path.display().to_string(),
        recap_path: recap_path.display().to_string(),
        message: signal.body.clone(),
        signal,
    };

    let path = write_notification(config, &record)?;
    emit_signal(&record.signal);

    Ok(path)
}

fn write_notification(config: &Config, record: &NotificationRecord) -> Result<PathBuf> {
    let runs_dir = Path::new(&config.defaults.operations_dir).join("runs");
    fs::create_dir_all(&runs_dir)
        .with_context(|| format!("failed to create runs directory {}", runs_dir.display()))?;

    let path = runs_dir.join(format!("{}.notification.json", Uuid::new_v4()));
    let json = serde_json::to_string_pretty(record).context("failed to serialize notification")?;
    fs::write(&path, format!("{json}\n"))
        .with_context(|| format!("failed to write notification at {}", path.display()))?;

    Ok(path)
}

fn emit_signal(signal: &NotificationSignal) {
    if let Err(error) = emit_platform_signal(signal) {
        eprintln!("failed to send notification signal: {error:#}");
    }
}

#[cfg(all(target_os = "macos", not(test)))]
fn emit_platform_signal(signal: &NotificationSignal) -> Result<()> {
    let script = format!(
        "display notification {} with title {}",
        applescript_string(&signal.body),
        applescript_string(&signal.title)
    );
    let status = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .status()
        .context("failed to run osascript")?;

    if !status.success() {
        anyhow::bail!("osascript exited with status {status}");
    }

    Ok(())
}

#[cfg(any(not(target_os = "macos"), test))]
fn emit_platform_signal(_signal: &NotificationSignal) -> Result<()> {
    Ok(())
}

#[cfg(all(target_os = "macos", not(test)))]
fn applescript_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::config::{Defaults, GitConfig};

    use super::*;

    #[test]
    fn writes_user_interaction_notification() {
        let root = std::env::temp_dir().join(format!("varda-notify-{}", std::process::id()));
        let config = Config {
            defaults: Defaults {
                timeout_seconds: 600,
                operations_dir: root.display().to_string(),
                sandbox: None,
                ..Default::default()
            },
            routes: vec![],
            agents: BTreeMap::new(),
            roles: BTreeMap::new(),
            git: GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
            orchestration: crate::orchestration::OrchestrationPolicy::default(),
            include: Vec::new(),
            requires_commands: Vec::new(),
            requires_secrets: Vec::new(),
        };

        let notification = notify_user_interaction(
            &config,
            Path::new(".varda/operations/tasks/example.md"),
            Path::new(".varda/operations/recaps/run.md"),
        )
        .expect("notification should write");

        let content = fs::read_to_string(notification).expect("notification should be readable");

        assert!(content.contains("\"kind\": \"needs_user\""));
        assert!(content.contains("requires user interaction"));
        assert!(content.contains("\"title\": \"Varda needs user input\""));
    }
}
