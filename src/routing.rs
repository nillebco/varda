//! Task path routing.

use std::fs;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use globset::{Glob, GlobSetBuilder};

use crate::agent::{build_agent_instructions, build_planning_instructions};
use crate::config::{Config, Route};
use crate::task::TaskDocument;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteMatch {
    pub agent: String,
    pub glob: String,
    pub allowed_agents: Vec<String>,
    pub estimated_prompt_tokens: usize,
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
        estimated_prompt_tokens: 0,
    })
}

pub fn match_route_for_task(
    config: &Config,
    task: &TaskDocument,
    planning: bool,
) -> Result<RouteMatch> {
    let project_path = task
        .frontmatter
        .project
        .as_deref()
        .map(Path::new)
        .context("task frontmatter is missing project")?;
    let route = find_route(config, project_path)?;
    ensure_agents_exist(config, route)?;

    let estimated_prompt_tokens = estimate_task_prompt_tokens(config, task, planning);
    let agent = select_agent_with_budget(
        config,
        route,
        task.frontmatter.assignee.as_deref(),
        estimated_prompt_tokens,
    )?;

    Ok(RouteMatch {
        agent,
        glob: route.glob.clone(),
        allowed_agents: route.agents.clone(),
        estimated_prompt_tokens,
    })
}

fn find_route<'a>(config: &'a Config, project_path: &Path) -> Result<&'a Route> {
    let mut builder = GlobSetBuilder::new();

    for route in &config.routes {
        let glob = Glob::new(&route.glob)
            .with_context(|| format!("invalid route glob '{}'", route.glob))?;
        builder.add(glob);
    }

    let set = builder.build().context("failed to build route matcher")?;
    let matches = set.matches(project_path);
    matches
        .first()
        .and_then(|index| config.routes.get(*index))
        .with_context(|| format!("no project route matched {}", project_path.display()))
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

fn select_agent_with_budget(
    config: &Config,
    route: &Route,
    requested_agent: Option<&str>,
    estimated_prompt_tokens: usize,
) -> Result<String> {
    if let Some(agent) = requested_agent {
        if !route.agents.iter().any(|allowed| allowed == agent) {
            bail!(
                "agent '{}' is not allowed for project route '{}'; allowed agents: {}",
                agent,
                route.glob,
                route.agents.join(", ")
            );
        }

        if agent_fits_prompt_budget(config, agent, estimated_prompt_tokens) {
            return Ok(agent.to_owned());
        }

        bail!(
            "agent '{}' prompt budget is too small for this task: estimated {} tokens, max_prompt_tokens {}; allowed agents with enough budget: {}",
            agent,
            estimated_prompt_tokens,
            describe_agent_budget(config, agent),
            agents_with_enough_budget(config, route, estimated_prompt_tokens).join(", ")
        );
    }

    route
        .agents
        .iter()
        .find(|agent| agent_fits_prompt_budget(config, agent, estimated_prompt_tokens))
        .cloned()
        .with_context(|| {
            format!(
                "no allowed agent has enough prompt budget for this task: estimated {} tokens; allowed agents: {}",
                estimated_prompt_tokens,
                describe_route_budgets(config, route)
            )
        })
}

fn agent_fits_prompt_budget(config: &Config, agent: &str, estimated_prompt_tokens: usize) -> bool {
    config
        .agents
        .get(agent)
        .and_then(|agent| agent.max_prompt_tokens)
        .is_none_or(|max_prompt_tokens| estimated_prompt_tokens <= max_prompt_tokens)
}

fn describe_agent_budget(config: &Config, agent: &str) -> String {
    config
        .agents
        .get(agent)
        .and_then(|agent| agent.max_prompt_tokens)
        .map(|budget| budget.to_string())
        .unwrap_or_else(|| "unlimited".to_owned())
}

fn agents_with_enough_budget(
    config: &Config,
    route: &Route,
    estimated_prompt_tokens: usize,
) -> Vec<String> {
    route
        .agents
        .iter()
        .filter(|agent| agent_fits_prompt_budget(config, agent, estimated_prompt_tokens))
        .cloned()
        .collect()
}

fn describe_route_budgets(config: &Config, route: &Route) -> String {
    route
        .agents
        .iter()
        .map(|agent| format!("{agent}={}", describe_agent_budget(config, agent)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn estimate_task_prompt_tokens(config: &Config, task: &TaskDocument, planning: bool) -> usize {
    let timeout = Duration::from_secs(config.defaults.timeout_seconds);
    let mut characters = if planning {
        build_planning_instructions(timeout).len()
    } else {
        build_agent_instructions(timeout).len()
    };

    characters += task.path.display().to_string().len();
    characters += task.body.len();
    characters += serde_yaml::to_string(&task.frontmatter)
        .map(|frontmatter| frontmatter.len())
        .unwrap_or_default();

    if let Some(project) = task.frontmatter.project.as_deref() {
        characters += project_instructions_len(project);
    }
    if !planning {
        if let Some(plan_path) = task.frontmatter.plan.as_deref() {
            characters += fs::read_to_string(plan_path)
                .map(|content| content.len())
                .unwrap_or_default();
        }
    }

    characters.div_ceil(4)
}

fn project_instructions_len(project: &str) -> usize {
    ["CLAUDE.md", "AGENTS.md", "copilot-instructions.md"]
        .iter()
        .filter_map(|name| fs::read_to_string(Path::new(project).join(name)).ok())
        .filter(|content| !content.trim().is_empty())
        .map(|content| content.len())
        .sum()
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
                        max_prompt_tokens: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        interactive_command: None,
                        interactive_args: None,
                    },
                ),
                (
                    "fallback".to_owned(),
                    AgentConfig {
                        kind: AgentKind::Acp,
                        command: "other".to_owned(),
                        args: vec![],
                        max_prompt_tokens: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        interactive_command: None,
                        interactive_args: None,
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
                        max_prompt_tokens: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        interactive_command: None,
                        interactive_args: None,
                    },
                ),
                (
                    "claude".to_owned(),
                    AgentConfig {
                        kind: AgentKind::Acp,
                        command: "claude".to_owned(),
                        args: vec![],
                        max_prompt_tokens: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        interactive_command: None,
                        interactive_args: None,
                    },
                ),
                (
                    "copilot".to_owned(),
                    AgentConfig {
                        kind: AgentKind::Acp,
                        command: "gh".to_owned(),
                        args: vec![],
                        max_prompt_tokens: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        interactive_command: None,
                        interactive_args: None,
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

        let other_route = match_route(&config, Path::new("/Users/nilleb/dev/nillebco/varda"), None)
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
                    max_prompt_tokens: None,
                    working_dir: None,
                    env: BTreeMap::new(),
                    interactive_command: None,
                    interactive_args: None,
                },
            )]),
            git: GitConfig { auto_commit: true },
        };

        let error = match_route(&config, Path::new("/work/project"), Some("claude"))
            .expect_err("agent should be rejected");

        assert!(error.to_string().contains("not allowed"));
    }
}
