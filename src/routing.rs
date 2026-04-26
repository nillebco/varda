//! Task path routing.

use std::path::Path;

use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSetBuilder};

use crate::config::{Config, Route};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteMatch {
    pub agent: String,
    pub glob: String,
}

pub fn match_route(config: &Config, task_path: &Path) -> Result<RouteMatch> {
    let mut builder = GlobSetBuilder::new();

    for route in &config.routes {
        let glob = Glob::new(&route.glob)
            .with_context(|| format!("invalid route glob '{}'", route.glob))?;
        builder.add(glob);
    }

    let set = builder.build().context("failed to build route matcher")?;
    let matches = set.matches(task_path);
    let route = matches
        .first()
        .and_then(|index| config.routes.get(*index))
        .with_context(|| format!("no route matched {}", task_path.display()))?;

    ensure_agent_exists(config, route)?;

    Ok(RouteMatch {
        agent: route.agent.clone(),
        glob: route.glob.clone(),
    })
}

fn ensure_agent_exists(config: &Config, route: &Route) -> Result<()> {
    if !config.agents.contains_key(&route.agent) {
        bail!(
            "route '{}' references unknown agent '{}'",
            route.glob,
            route.agent
        );
    }

    Ok(())
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
                    glob: ".varda/operations/tasks/codex/**/*.md".to_owned(),
                    agent: "codex".to_owned(),
                },
                Route {
                    glob: ".varda/operations/tasks/**/*.md".to_owned(),
                    agent: "fallback".to_owned(),
                },
            ],
            agents: BTreeMap::from([
                (
                    "codex".to_owned(),
                    AgentConfig {
                        kind: AgentKind::Acp,
                        command: "codex".to_owned(),
                        args: vec!["--acp".to_owned()],
                    },
                ),
                (
                    "fallback".to_owned(),
                    AgentConfig {
                        kind: AgentKind::Acp,
                        command: "other".to_owned(),
                        args: vec![],
                    },
                ),
            ]),
            git: GitConfig { auto_commit: true },
        };

        let route = match_route(
            &config,
            Path::new(".varda/operations/tasks/codex/example.md"),
        )
        .expect("route should match");

        assert_eq!(route.agent, "codex");
    }
}
