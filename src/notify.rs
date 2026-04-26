//! User notification support.

use std::fs;
use std::path::{Path, PathBuf};

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
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NotificationKind {
    NeedsUser,
}

pub fn notify_user_interaction(
    config: &Config,
    task_path: &Path,
    recap_path: &Path,
) -> Result<PathBuf> {
    let record = NotificationRecord {
        kind: NotificationKind::NeedsUser,
        task_path: task_path.display().to_string(),
        recap_path: recap_path.display().to_string(),
        message: format!(
            "Task {} requires user interaction. See recap {}.",
            task_path.display(),
            recap_path.display()
        ),
    };

    write_notification(config, &record)
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
            },
            routes: vec![],
            agents: BTreeMap::new(),
            git: GitConfig { auto_commit: true },
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
    }
}
