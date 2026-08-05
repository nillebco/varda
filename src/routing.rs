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
    pub role: Option<String>,
    pub role_instructions: Option<String>,
    pub glob: String,
    pub allowed_agents: Vec<String>,
    pub estimated_prompt_tokens: usize,
    /// Effective sandbox provider for this route (`route` → `defaults` → `local`).
    pub sandbox: String,
    /// Project-context mounts declared on the matched route (M6a). Composed with
    /// the sandbox's image-intrinsic mounts by the docker provider.
    pub route_mounts: Vec<String>,
}

impl RouteMatch {
    pub fn display_name(&self) -> &str {
        self.role.as_deref().unwrap_or(&self.agent)
    }
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
    let (agent, role, role_instructions) = select_agent(config, route, requested_agent)?;

    Ok(RouteMatch {
        agent,
        role,
        role_instructions,
        glob: route.glob.clone(),
        allowed_agents: route.agents.clone(),
        estimated_prompt_tokens: 0,
        sandbox: config.effective_sandbox(route).to_owned(),
        route_mounts: route.mounts.clone(),
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
    let (agent, role, role_instructions) = select_agent_with_budget(
        config,
        route,
        task.frontmatter.assignee.as_deref(),
        estimated_prompt_tokens,
    )?;

    Ok(RouteMatch {
        agent,
        role,
        role_instructions,
        glob: route.glob.clone(),
        allowed_agents: route.agents.clone(),
        estimated_prompt_tokens,
        sandbox: config.effective_sandbox(route).to_owned(),
        route_mounts: route.mounts.clone(),
    })
}

/// Public glob-route lookup used by [`crate::config::Config::resolve_sandbox_for`]
/// to read the central (trusted) route's sandbox name and mounts.
pub fn find_route_public<'a>(config: &'a Config, project_path: &Path) -> Result<&'a Route> {
    find_route(config, project_path)
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

    for name in &route.agents {
        if config.agents.contains_key(name) {
            continue;
        }
        if let Some(role) = config.roles.get(name) {
            if !config.agents.contains_key(&role.backend) {
                bail!(
                    "route '{}' role '{}' references unknown backend agent '{}'",
                    route.glob,
                    name,
                    role.backend
                );
            }
            continue;
        }
        bail!(
            "route '{}' references unknown agent or role '{}'",
            route.glob,
            name
        );
    }

    Ok(())
}

fn select_agent(
    config: &Config,
    route: &Route,
    requested_agent: Option<&str>,
) -> Result<(String, Option<String>, Option<String>)> {
    let name = if let Some(agent) = requested_agent {
        if route.agents.iter().any(|allowed| allowed == agent) {
            agent.to_owned()
        } else {
            bail!(
                "agent '{}' is not allowed for project route '{}'; allowed agents: {}",
                agent,
                route.glob,
                route.agents.join(", ")
            );
        }
    } else {
        route
            .agents
            .first()
            .cloned()
            .with_context(|| format!("route '{}' does not allow any agents", route.glob))?
    };

    Ok(resolve_selection(config, &name))
}

fn select_agent_with_budget(
    config: &Config,
    route: &Route,
    requested_agent: Option<&str>,
    estimated_prompt_tokens: usize,
) -> Result<(String, Option<String>, Option<String>)> {
    if let Some(name) = requested_agent {
        if !route.agents.iter().any(|allowed| allowed == name) {
            bail!(
                "agent '{}' is not allowed for project route '{}'; allowed agents: {}",
                name,
                route.glob,
                route.agents.join(", ")
            );
        }

        let backend = resolve_backend(config, name);
        if agent_fits_prompt_budget(config, backend, estimated_prompt_tokens) {
            return Ok(resolve_selection(config, name));
        }

        bail!(
            "agent '{}' prompt budget is too small for this task: estimated {} tokens, max_prompt_tokens {}; allowed agents with enough budget: {}",
            name,
            estimated_prompt_tokens,
            describe_agent_budget(config, backend),
            agents_with_enough_budget(config, route, estimated_prompt_tokens).join(", ")
        );
    }

    for name in &route.agents {
        let backend = resolve_backend(config, name);
        if agent_fits_prompt_budget(config, backend, estimated_prompt_tokens) {
            return Ok(resolve_selection(config, name));
        }
    }

    bail!(
        "no allowed agent has enough prompt budget for this task: estimated {} tokens; allowed agents: {}",
        estimated_prompt_tokens,
        describe_route_budgets(config, route)
    )
}

fn resolve_backend<'a>(config: &'a Config, name: &'a str) -> &'a str {
    config
        .roles
        .get(name)
        .map(|r| r.backend.as_str())
        .unwrap_or(name)
}

fn resolve_selection(config: &Config, name: &str) -> (String, Option<String>, Option<String>) {
    if let Some(role) = config.roles.get(name) {
        (
            role.backend.clone(),
            Some(name.to_owned()),
            role.instructions.clone(),
        )
    } else {
        (name.to_owned(), None, None)
    }
}

fn agent_fits_prompt_budget(
    config: &Config,
    backend: &str,
    estimated_prompt_tokens: usize,
) -> bool {
    config
        .agents
        .get(backend)
        .and_then(|a| a.max_prompt_tokens)
        .is_none_or(|max| estimated_prompt_tokens <= max)
}

fn describe_agent_budget(config: &Config, backend: &str) -> String {
    config
        .agents
        .get(backend)
        .and_then(|a| a.max_prompt_tokens)
        .map(|b| b.to_string())
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
        .filter(|name| {
            let backend = resolve_backend(config, name);
            agent_fits_prompt_budget(config, backend, estimated_prompt_tokens)
        })
        .cloned()
        .collect()
}

fn describe_route_budgets(config: &Config, route: &Route) -> String {
    route
        .agents
        .iter()
        .map(|name| {
            let backend = resolve_backend(config, name);
            format!("{name}={}", describe_agent_budget(config, backend))
        })
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

    if !planning {
        if let Some(assignee) = task.frontmatter.assignee.as_deref() {
            if let Some(role) = config.roles.get(assignee) {
                characters += role.instructions.as_deref().map(str::len).unwrap_or(0);
            }
        }
    }

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
                sandbox: None,
                ..Default::default()
            },
            routes: vec![
                Route {
                    glob: "/work/special/**".to_owned(),
                    agents: vec!["codex".to_owned()],
                    sandbox: None,
                    mounts: Vec::new(),
                },
                Route {
                    glob: "**".to_owned(),
                    agents: vec!["fallback".to_owned()],
                    sandbox: None,
                    mounts: Vec::new(),
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
                        auth_token_env: None,
                        auth_token_target: None,
                        resume_command_template: None,
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
                        auth_token_env: None,
                        auth_token_target: None,
                        resume_command_template: None,
                    },
                ),
            ]),
            roles: BTreeMap::new(),
            git: GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
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
                sandbox: None,
                ..Default::default()
            },
            routes: vec![
                Route {
                    glob: "**/AsianDevBank/**".to_owned(),
                    agents: vec!["copilot".to_owned()],
                    sandbox: None,
                    mounts: Vec::new(),
                },
                Route {
                    glob: "**".to_owned(),
                    agents: vec!["codex".to_owned(), "claude".to_owned()],
                    sandbox: None,
                    mounts: Vec::new(),
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
                        auth_token_env: None,
                        auth_token_target: None,
                        resume_command_template: None,
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
                        auth_token_env: None,
                        auth_token_target: None,
                        resume_command_template: None,
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
                        auth_token_env: None,
                        auth_token_target: None,
                        resume_command_template: None,
                    },
                ),
            ]),
            roles: BTreeMap::new(),
            git: GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
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
                sandbox: None,
                ..Default::default()
            },
            routes: vec![Route {
                glob: "**".to_owned(),
                agents: vec!["codex".to_owned()],
                sandbox: None,
                mounts: Vec::new(),
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
                    auth_token_env: None,
                    auth_token_target: None,
                    resume_command_template: None,
                },
            )]),
            roles: BTreeMap::new(),
            git: GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
        };

        let error = match_route(&config, Path::new("/work/project"), Some("claude"))
            .expect_err("agent should be rejected");

        assert!(error.to_string().contains("not allowed"));
    }

    #[test]
    fn selects_first_allowed_agent_that_fits_prompt_budget() {
        let config = Config {
            defaults: Defaults {
                timeout_seconds: 600,
                operations_dir: ".varda/operations".to_owned(),
                sandbox: None,
                ..Default::default()
            },
            routes: vec![Route {
                glob: "**".to_owned(),
                agents: vec!["small".to_owned(), "large".to_owned()],
                sandbox: None,
                mounts: Vec::new(),
            }],
            agents: BTreeMap::from([
                (
                    "small".to_owned(),
                    AgentConfig {
                        kind: AgentKind::Acp,
                        command: "small".to_owned(),
                        args: vec![],
                        max_prompt_tokens: Some(1),
                        working_dir: None,
                        env: BTreeMap::new(),
                        interactive_command: None,
                        interactive_args: None,
                        auth_token_env: None,
                        auth_token_target: None,
                        resume_command_template: None,
                    },
                ),
                (
                    "large".to_owned(),
                    AgentConfig {
                        kind: AgentKind::Acp,
                        command: "large".to_owned(),
                        args: vec![],
                        max_prompt_tokens: Some(10_000),
                        working_dir: None,
                        env: BTreeMap::new(),
                        interactive_command: None,
                        interactive_args: None,
                        auth_token_env: None,
                        auth_token_target: None,
                        resume_command_template: None,
                    },
                ),
            ]),
            roles: BTreeMap::new(),
            git: GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
        };
        let task = TaskDocument {
            path: Path::new("/tmp/task.md").to_path_buf(),
            frontmatter: crate::task::TaskFrontmatter {
                id: Some(1),
                status: crate::task::TaskStatus::Ready,
                project: Some("/work/project".to_owned()),
                assignee: None,
                recap: None,
                recaps: vec![],
                plan: None,
                agent_session_id: None,
                agent_session_log: None,
                agent_session_ids: vec![],
                agent_session_logs: vec![],
                agent_resume_commands: vec![],
                allow_commands: vec![],
                requires_user: false,
            },
            body: "# Task\n\nDo it.".to_owned(),
        };

        let route =
            match_route_for_task(&config, &task, false).expect("large agent should be selected");

        assert_eq!(route.agent, "large");
        assert!(route.estimated_prompt_tokens > 1);
    }

    #[test]
    fn rejects_requested_agent_that_exceeds_prompt_budget() {
        let config = Config {
            defaults: Defaults {
                timeout_seconds: 600,
                operations_dir: ".varda/operations".to_owned(),
                sandbox: None,
                ..Default::default()
            },
            routes: vec![Route {
                glob: "**".to_owned(),
                agents: vec!["small".to_owned(), "large".to_owned()],
                sandbox: None,
                mounts: Vec::new(),
            }],
            agents: BTreeMap::from([
                (
                    "small".to_owned(),
                    AgentConfig {
                        kind: AgentKind::Acp,
                        command: "small".to_owned(),
                        args: vec![],
                        max_prompt_tokens: Some(1),
                        working_dir: None,
                        env: BTreeMap::new(),
                        interactive_command: None,
                        interactive_args: None,
                        auth_token_env: None,
                        auth_token_target: None,
                        resume_command_template: None,
                    },
                ),
                (
                    "large".to_owned(),
                    AgentConfig {
                        kind: AgentKind::Acp,
                        command: "large".to_owned(),
                        args: vec![],
                        max_prompt_tokens: None,
                        working_dir: None,
                        env: BTreeMap::new(),
                        interactive_command: None,
                        interactive_args: None,
                        auth_token_env: None,
                        auth_token_target: None,
                        resume_command_template: None,
                    },
                ),
            ]),
            roles: BTreeMap::new(),
            git: GitConfig { auto_commit: true },
            sandboxes: std::collections::BTreeMap::new(),
        };
        let task = TaskDocument {
            path: Path::new("/tmp/task.md").to_path_buf(),
            frontmatter: crate::task::TaskFrontmatter {
                id: Some(1),
                status: crate::task::TaskStatus::Ready,
                project: Some("/work/project".to_owned()),
                assignee: Some("small".to_owned()),
                recap: None,
                recaps: vec![],
                plan: None,
                agent_session_id: None,
                agent_session_log: None,
                agent_session_ids: vec![],
                agent_session_logs: vec![],
                agent_resume_commands: vec![],
                allow_commands: vec![],
                requires_user: false,
            },
            body: "# Task\n\nDo it.".to_owned(),
        };

        let error = match_route_for_task(&config, &task, false)
            .expect_err("small agent should be rejected");

        assert!(error.to_string().contains("prompt budget is too small"));
        assert!(error.to_string().contains("large"));
    }
}
