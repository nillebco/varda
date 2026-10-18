//! `varda doctor` tracker checks: per configured tracker, does the credential
//! resolve, does auth work, is every `status_map` value a valid state for the
//! work-item type, and is the acknowledgement present.

use std::time::Duration;

use super::IssueTracker;
use crate::config::{Config, TrackerConfig};

const CREDENTIAL_TIMEOUT: Duration = Duration::from_secs(30);

/// `status_map` entries whose target state is not in `valid` (case-insensitive).
pub fn invalid_status_map_entries(
    tracker: &TrackerConfig,
    valid: &[String],
) -> Vec<(String, String)> {
    tracker
        .status_map
        .iter()
        .filter(|(_, state)| !valid.iter().any(|v| v.eq_ignore_ascii_case(state)))
        .map(|(status, state)| (status.clone(), state.clone()))
        .collect()
}

/// One named check and its verdict (`Err` ⇒ failed, with the reason).
pub type Check = (&'static str, Result<String, String>);

/// The checks that need a built backend: auth and status-map validity.
pub async fn backend_checks(tracker: &TrackerConfig, backend: &dyn IssueTracker) -> Vec<Check> {
    let auth = backend
        .check_auth()
        .await
        .map(|()| "ok".to_owned())
        .map_err(|e| e.to_string());
    let auth_ok = auth.is_ok();
    let mut checks = vec![("auth", auth)];
    if auth_ok {
        let map = match backend.valid_states().await {
            Err(e) => Err(format!("could not list valid states: {e}")),
            Ok(None) => Ok("not checked (backend cannot list states)".to_owned()),
            Ok(Some(valid)) => {
                let bad = invalid_status_map_entries(tracker, &valid);
                if bad.is_empty() {
                    Ok("every value is a valid state".to_owned())
                } else {
                    let bad: Vec<String> =
                        bad.iter().map(|(k, v)| format!("{k} = '{v}'")).collect();
                    Err(format!(
                        "not valid states ({}): {}",
                        valid.join(", "),
                        bad.join(", ")
                    ))
                }
            }
        };
        checks.push(("status_map", map));
    }
    checks
}

/// Print the tracker section of `varda doctor`; returns how many checks failed.
pub async fn report(config: &Config) -> usize {
    println!();
    if config.trackers.is_empty() {
        println!("configured trackers: none");
        return 0;
    }
    println!("configured trackers:");
    let mut failed = 0;
    for tracker in &config.trackers {
        let mut checks: Vec<Check> = vec![(
            "acknowledged",
            super::sync::check_acknowledged(config, tracker)
                .map(|()| "yes".to_owned())
                .map_err(|e| format!("{e:#}")),
        )];
        let for_build = tracker.clone();
        let built = tokio::task::spawn_blocking(move || {
            super::build_tracker_within(&for_build, Some(CREDENTIAL_TIMEOUT))
        })
        .await;
        match built {
            Ok(Ok(backend)) => {
                checks.insert(0, ("credential", Ok("resolves".to_owned())));
                checks.extend(backend_checks(tracker, backend.as_ref()).await);
            }
            Ok(Err(e)) => checks.insert(0, ("credential", Err(format!("{e:#}")))),
            Err(e) => checks.insert(0, ("credential", Err(e.to_string()))),
        }
        println!("  {} ({:?})", tracker.name, tracker.kind);
        for (name, verdict) in checks {
            match verdict {
                Ok(detail) => println!("    {name:<13} ok — {detail}"),
                Err(reason) => {
                    failed += 1;
                    println!("    {name:<13} FAILED — {reason}");
                }
            }
        }
    }
    failed
}

#[cfg(test)]
mod tests {
    use super::super::fake::FakeTracker;
    use super::super::{AZDO_AGILE_STATUS_MAP, status_map_from};
    use super::*;
    use crate::config::{CredentialConfig, TrackerFilter, TrackerKind};
    use std::collections::BTreeMap;

    fn tracker(map: &[(&str, &str)]) -> TrackerConfig {
        TrackerConfig {
            name: "t".to_owned(),
            kind: TrackerKind::Azdo,
            project_path: "/tmp".to_owned(),
            credential: CredentialConfig::default(),
            filter: TrackerFilter::default(),
            status_map: status_map_from(map),
            acknowledged: None,
            extra: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn status_map_values_are_validated_against_backend_states() {
        let t = tracker(AZDO_AGILE_STATUS_MAP);
        let fake = FakeTracker::new(t.clone());
        // Not enumerable: reported, not failed.
        let checks = backend_checks(&t, &fake).await;
        assert!(checks.iter().all(|(_, v)| v.is_ok()), "{checks:?}");

        fake.set_valid_states(&["new", "Active", "Resolved", "Closed"]);
        let checks = backend_checks(&t, &fake).await;
        assert!(checks.iter().all(|(_, v)| v.is_ok()), "{checks:?}");

        fake.set_valid_states(&["New", "Active", "Done"]);
        let checks = backend_checks(&t, &fake).await;
        let (_, verdict) = checks.iter().find(|(n, _)| *n == "status_map").unwrap();
        let err = verdict.as_ref().unwrap_err();
        assert!(err.contains("review = 'Resolved'") && err.contains("done = 'Closed'"), "{err}");
    }
}
