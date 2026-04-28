//! Task path routing.

use std::path::Path;

use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSetBuilder};

use crate::config::{Config, Route};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteMatch {
    pub agent: String,
    pub glob: String,
    pub allowed_agents: Vec<String>,
}

pub fn match_route(
    config: &Config,
    project_path: &Path,
    requested_agent: Option<&str>,
) -> Result<RouteMatch> {
    let mut builder = GlobSetBuilder::new();

    for route in &config.routes {
        let glob = Glob::new(&route.glob)
            .with_context(|| format!("invalid route glob '{}'", route.glob))?;
        builder.add(glob);
    }

    let set = builder.build().context("failed to build route matcher")?;
    let matches = set.matches(project_path);
    let route = matches
        .first()
        .and_then(|index| config.routes.get(*index))
        .with_context(|| format!("no project route matched {}", project_path.display()))?;

    ensure_agents_exist(config, route)?;
    let agent = select_agent(route, requested_agent)?;

    Ok(RouteMatch {
        agent,
        glob: route.glob.clone(),
        allowed_agents: route.agents.clone(),
    })
}

fn ensure_agents_exist(config: &Config, route: &Route) -> Result<()> {
    if route.agents.is_empty() {
        bail!("route '{}' does not allow any agents", route.glob);
    }

    for agent in &route.agents {
        if !config.agents.contains_key(agent) {
            bail!(
                "route '{}' references unknown agent '{}'",
                route.glob,
                agent
            );
        }
    }

    Ok(())
}

fn select_agent(route: &Route, requested_agent: Option<&str>) -> Result<String> {
    if let Some(agent) = requested_agent {
        if route.agents.iter().any(|allowed| allowed == agent) {
            return Ok(agent.to_owned());
        }

        bail!(
            "agent '{}' is not allowed for project route '{}'; allowed agents: {}",
            agent,
            route.glob,
            route.agents.join(", ")
        );
    }

    route
        .agents
        .first()
        .cloned()
        .with_context(|| format!("route '{}' does not allow any agents", route.glob))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::config::{AgentConfig, AgentKind, Defaults, GitConfig};

    use super::*;

    #[test]
    fn matches_first_configured_route() {
        let config = Config {
            defaults: Defaults {
                timeout_seconds: 600,
                operations_dir: ".varda/operations".to_owned(),
            },
            routes: vec![
                Route {
                    glob: "/work/special/**".to_owned(),
                    agents: vec!["codex".to_owned()],
                },
                Route {
                    glob: "**".to_owned(),
                    agents: vec!["fallback".to_owned()],
                },
            ],
            agents: BTreeMap::from([
                (
                    "codex".to_owned(),
                    AgentConfig {
                        kind: AgentKind::Acp,
                        command: "codex".to_owned(),
                        args: vec!["--acp".to_owned()],
                        working_dir: None,
                        env: BTreeMap::new(),
                    },
                ),
                (
                    "fallback".to_owned(),
                    AgentConfig {
                        kind: AgentKind::Acp,
                        command: "other".to_owned(),
                        args: vec![],
                        working_dir: None,
                        env: BTreeMap::new(),
                    },
                ),
            ]),
            git: GitConfig { auto_commit: true },
        };

        let route = match_route(&config, Path::new("/work/special/project"), None)
            .expect("route should match");

        assert_eq!(route.agent, "codex");
    }

    #[test]
    fn specific_route_beats_catch_all_when_listed_first() {
        let config = Config {
            defaults: Defaults {
                timeout_seconds: 600,
                operations_dir: ".varda/operations".to_owned(),
            },
            routes: vec![
                Route {
                    glob: "**/AsianDevBank/**".to_owned(),
                    agents: vec!["copilot".to_owned()],
                },
                Route {
                    glob: "**".to_owned(),
                    agents: vec!["codex".to_owned(), "claude".to_owned()],
                },
            ],
            agents: BTreeMap::from([
                (
                    "codex".to_owned(),
                    AgentConfig {
                        kind: AgentKind::Acp,
                        command: "codex".to_owned(),
                        args: vec![],
                        working_dir: None,
                        env: BTreeMap::new(),
                    },
                ),
                (
                    "claude".to_owned(),
                    AgentConfig {
                        kind: AgentKind::Acp,
                        command: "claude".to_owned(),
                        args: vec![],
                        working_dir: None,
                        env: BTreeMap::new(),
                    },
                ),
                (
                    "copilot".to_owned(),
                    AgentConfig {
                        kind: AgentKind::Acp,
                        command: "gh".to_owned(),
                        args: vec![],
                        working_dir: None,
                        env: BTreeMap::new(),
                    },
                ),
            ]),
            git: GitConfig { auto_commit: true },
        };

        let adb_route = match_route(
            &config,
            Path::new("/Users/nilleb/dev/AsianDevBank/dpt-activity-log"),
            None,
        )
        .expect("ADB path should match");
        assert_eq!(adb_route.agent, "copilot");

        let other_route =
            match_route(&config, Path::new("/Users/nilleb/dev/nillebco/varda"), None)
                .expect("non-ADB path should match");
        assert_eq!(other_route.agent, "codex");
    }

    #[test]
    fn rejects_requested_agent_that_is_not_allowed() {
        let config = Config {
            defaults: Defaults {
                timeout_seconds: 600,
                operations_dir: ".varda/operations".to_owned(),
            },
            routes: vec![Route {
                glob: "**".to_owned(),
                agents: vec!["codex".to_owned()],
            }],
            agents: BTreeMap::from([(
                "codex".to_owned(),
                AgentConfig {
                    kind: AgentKind::Acp,
                    command: "codex".to_owned(),
                    args: vec![],
                    working_dir: None,
                    env: BTreeMap::new(),
                },
            )]),
            git: GitConfig { auto_commit: true },
        };

        let error = match_route(&config, Path::new("/work/project"), Some("claude"))
            .expect_err("agent should be rejected");

        assert!(error.to_string().contains("not allowed"));
    }
}
