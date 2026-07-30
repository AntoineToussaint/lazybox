use lazybox_agents::{Agent, Registry, SpawnCtx};
use lazybox_core::{SessionId, SessionKey};
use lazybox_ipc::{AgentRunAccess, SpawnOrigin, TerminalId, TerminalKind};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default)]
pub struct SpawnOptions {
    pub cwd: Option<String>,
    pub initial_prompt: Option<String>,
    pub autonomous: bool,
    pub on_main: bool,
    pub model_alias: Option<String>,
    pub resume: bool,
    pub provider_session_id: Option<String>,
    pub no_permission_override: Option<bool>,
    pub access: AgentRunAccess,
    pub client_request_id: Option<String>,
    pub origin: SpawnOrigin,
}

#[derive(Debug)]
pub(crate) struct SpawnPlanInput {
    pub session_key: SessionKey,
    pub kind: TerminalKind,
    pub cwd: PathBuf,
    pub agent_worktree: PathBuf,
    pub owning_session: Option<SessionId>,
    pub initial_prompt: Option<String>,
    pub terminal_id: TerminalId,
    pub hook_settings: Option<PathBuf>,
    pub hook_command: Option<String>,
    pub repo_env: Vec<(String, String)>,
    pub priority_model_alias: Option<String>,
    pub autonomous: bool,
    pub landed_on_main: bool,
    pub model_alias: Option<String>,
    pub resume: bool,
    pub provider_session_id: Option<String>,
    pub no_permission_override: Option<bool>,
    pub access: AgentRunAccess,
    pub shell_command: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SpawnFlags {
    pub autonomous: bool,
    pub no_permission: bool,
    pub on_main: bool,
    pub resume: bool,
    pub uses_argv_hooks: bool,
}

pub(crate) struct SpawnPlan {
    pub session_key: SessionKey,
    pub kind: TerminalKind,
    pub argv: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub hint: String,
    pub persist_key: Option<String>,
    pub owning_session: Option<SessionId>,
    pub initial_prompt: Option<String>,
    pub terminal_id: TerminalId,
    pub hook_settings: Option<PathBuf>,
    pub model_label: Option<String>,
    pub model_alias: Option<String>,
    pub provider_session_id: Option<String>,
    pub access: AgentRunAccess,
    pub flags: SpawnFlags,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub(crate) enum SpawnPlanError {
    #[error("no agent registered for id {0}")]
    UnknownAgent(String),
}

pub(crate) fn build_spawn_plan(
    input: SpawnPlanInput,
    cfg: &lazybox_config::Config,
    agents: &Registry,
) -> Result<SpawnPlan, SpawnPlanError> {
    let SpawnPlanInput {
        session_key,
        kind,
        cwd,
        agent_worktree,
        owning_session,
        initial_prompt,
        terminal_id,
        hook_settings,
        hook_command,
        repo_env,
        priority_model_alias,
        autonomous,
        landed_on_main,
        model_alias,
        resume,
        provider_session_id,
        no_permission_override,
        access,
        shell_command,
    } = input;
    let no_permission = access != AgentRunAccess::ReadOnly
        && no_permission_override.unwrap_or_else(|| skip_permissions_for(autonomous, cfg));
    let agent = match &kind {
        TerminalKind::Agent(id) => Some(
            agents
                .get(id)
                .ok_or_else(|| SpawnPlanError::UnknownAgent(id.clone()))?,
        ),
        _ => None,
    };
    let resolved_model_alias = model_alias.clone().or_else(|| priority_model_alias.clone());
    let (model_args, model_label) = match &kind {
        TerminalKind::Agent(agent_id) => {
            let models = cfg.agent_models(agent_id);
            let alias = resolved_model_alias.as_deref();
            let label = alias
                .or(models.default.as_deref())
                .and_then(|alias| models.tier(alias))
                .map(|tier| tier.label.clone());
            (models.resolve_args(alias), label)
        }
        _ => (Vec::new(), None),
    };
    let argv = argv_for(
        agents,
        &kind,
        &agent_worktree,
        || shell_command,
        no_permission,
        hook_settings.clone(),
        hook_command.as_deref(),
        &model_args,
        resume,
        provider_session_id.as_deref(),
        access,
    )?;
    let uses_argv_hooks = agent
        .as_deref()
        .zip(hook_command.as_deref())
        .is_some_and(|(agent, command)| !agent.hook_command_args(command).is_empty());
    let mut env = repo_env;
    for (key, value) in gateway_env_for_agent(cfg, agent.as_deref()) {
        if !env.iter().any(|(existing, _)| existing == &key) {
            env.push((key, value));
        }
    }
    let env = with_agent_spawn_defaults(env, agent.as_deref());
    let env = with_agent_pty_spawn_env(env, agent.as_deref());
    let env = with_worktree_cargo_target(env, Some(&cwd));
    let kind_label = match &kind {
        TerminalKind::Agent(id) => id.clone(),
        TerminalKind::Shell => "shell".into(),
        TerminalKind::LogTail { path } => {
            let base = path.rsplit('/').next().unwrap_or(path);
            format!("log-{base}")
        }
    };
    let hint = format!("{}-{kind_label}", session_key.as_str());
    let persist_key = match owning_session {
        Some(id) if !landed_on_main => Some(id.to_string()),
        _ => None,
    };

    Ok(SpawnPlan {
        session_key,
        kind,
        argv,
        cwd,
        env,
        hint,
        persist_key,
        owning_session,
        initial_prompt,
        terminal_id,
        hook_settings,
        model_label,
        model_alias: resolved_model_alias,
        provider_session_id,
        access,
        flags: SpawnFlags {
            autonomous,
            no_permission,
            on_main: landed_on_main,
            resume,
            uses_argv_hooks,
        },
    })
}

pub(crate) fn argv_for(
    agents: &Registry,
    kind: &TerminalKind,
    agent_worktree: &Path,
    resolve_shell: impl FnOnce() -> String,
    skip_permissions: bool,
    hook_settings_path: Option<PathBuf>,
    hook_command: Option<&str>,
    model_args: &[String],
    resume: bool,
    provider_session_id: Option<&str>,
    access: AgentRunAccess,
) -> Result<Vec<String>, SpawnPlanError> {
    match kind {
        TerminalKind::Agent(agent_id) => {
            let agent = agents
                .get(agent_id)
                .ok_or_else(|| SpawnPlanError::UnknownAgent(agent_id.clone()))?;
            let ctx = SpawnCtx {
                session_key: String::new(),
                worktree: agent_worktree.to_path_buf(),
                repo: None,
                pr_number: None,
                env: Default::default(),
                skip_permissions,
                access,
                hook_settings_path,
            };
            let mut argv = if resume {
                agent.resume_session(&ctx, provider_session_id)
            } else {
                agent.spawn(&ctx)
            };
            if let Some(command) = hook_command {
                argv.extend(agent.hook_command_args(command));
            }
            argv.extend(model_args.iter().cloned());
            Ok(argv)
        }
        TerminalKind::Shell => Ok(vec![resolve_shell()]),
        TerminalKind::LogTail { path } => Ok(vec!["tail".into(), "-F".into(), path.clone()]),
    }
}

pub(crate) fn gateway_env_for_agent(
    cfg: &lazybox_config::Config,
    agent: Option<&dyn Agent>,
) -> Vec<(String, String)> {
    let Some(provider) = agent.and_then(Agent::llm_provider) else {
        return Vec::new();
    };
    cfg.agent
        .gateway_url()
        .map(|url| vec![(provider.base_url_env().to_string(), url.to_string())])
        .unwrap_or_default()
}

pub(crate) fn with_agent_spawn_defaults(
    mut env: Vec<(String, String)>,
    agent: Option<&dyn Agent>,
) -> Vec<(String, String)> {
    let Some(agent) = agent else {
        return env;
    };
    for (key, value) in agent.spawn_env() {
        if !env.iter().any(|(existing, _)| existing == &key) {
            env.push((key, value));
        }
    }
    env
}

pub(crate) fn with_agent_pty_spawn_env(
    mut env: Vec<(String, String)>,
    agent: Option<&dyn Agent>,
) -> Vec<(String, String)> {
    let Some(agent) = agent else {
        return env;
    };
    for (key, value) in agent.pty_spawn_env() {
        if let Some((_, existing)) = env.iter_mut().find(|(existing, _)| existing == &key) {
            *existing = value;
        } else {
            env.push((key, value));
        }
    }
    env
}

pub(crate) fn with_worktree_cargo_target(
    mut env: Vec<(String, String)>,
    cwd: Option<&Path>,
) -> Vec<(String, String)> {
    let Some(cwd) = cwd else {
        return env;
    };
    if env.iter().any(|(key, _)| key == "CARGO_TARGET_DIR") {
        return env;
    }
    env.push((
        "CARGO_TARGET_DIR".to_string(),
        cwd.join("target").to_string_lossy().into_owned(),
    ));
    env
}

pub(crate) fn skip_permissions_for(autonomous: bool, cfg: &lazybox_config::Config) -> bool {
    if autonomous {
        cfg.agent.autonomous_skip_permissions
    } else {
        cfg.agent.skip_permissions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(kind: TerminalKind) -> SpawnPlanInput {
        SpawnPlanInput {
            session_key: SessionKey::from("github-acme-widget-657"),
            kind,
            cwd: PathBuf::from("/worktrees/widget-657"),
            agent_worktree: PathBuf::from("/worktrees/widget-657"),
            owning_session: Some(SessionId::new()),
            initial_prompt: Some("extract the spawn plan".into()),
            terminal_id: TerminalId(42),
            hook_settings: None,
            hook_command: None,
            repo_env: Vec::new(),
            priority_model_alias: None,
            autonomous: false,
            landed_on_main: false,
            model_alias: None,
            resume: false,
            provider_session_id: None,
            no_permission_override: None,
            access: AgentRunAccess::Default,
            shell_command: String::new(),
        }
    }

    #[test]
    fn agent_plan_resolves_argv_env_and_flags_without_io() {
        let mut cfg = lazybox_config::Config::default();
        cfg.agent.llm_gateway_url = Some("http://gateway.internal".into());
        let mut input = input(TerminalKind::Agent("claude".into()));
        input.autonomous = true;
        input.model_alias = Some("M".into());
        input.hook_settings = Some(PathBuf::from("/run/lazybox/settings-42.json"));
        input.repo_env = vec![("PROJECT_ENV".into(), "test".into())];

        let plan =
            build_spawn_plan(input, &cfg, &Registry::default_builtins()).expect("valid plan");

        assert_eq!(plan.session_key, SessionKey::from("github-acme-widget-657"));
        assert!(matches!(&plan.kind, TerminalKind::Agent(id) if id == "claude"));
        assert_eq!(
            plan.argv,
            vec![
                "claude",
                "--dangerously-skip-permissions",
                "--strict-mcp-config",
                "--settings",
                "/run/lazybox/settings-42.json",
                "--model",
                "claude-sonnet-5",
            ]
        );
        assert_eq!(
            plan.env,
            vec![
                ("PROJECT_ENV".into(), "test".into()),
                (
                    "ANTHROPIC_BASE_URL".into(),
                    "http://gateway.internal".into()
                ),
                ("CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN".into(), "1".into()),
                (
                    "CARGO_TARGET_DIR".into(),
                    "/worktrees/widget-657/target".into()
                ),
            ]
        );
        assert_eq!(plan.hint, "github-acme-widget-657-claude");
        assert!(plan.persist_key.is_some());
        assert_eq!(
            plan.flags,
            SpawnFlags {
                autonomous: true,
                no_permission: true,
                on_main: false,
                resume: false,
                uses_argv_hooks: false,
            }
        );
        assert_eq!(plan.model_label.as_deref(), Some("Sonnet"));
    }

    #[test]
    fn explicit_model_alias_wins_over_priority_fallback() {
        let cfg = lazybox_config::Config::default();
        let mut input = input(TerminalKind::Agent("claude".into()));
        input.model_alias = Some("S".into());
        input.priority_model_alias = Some("L".into());

        let plan =
            build_spawn_plan(input, &cfg, &Registry::default_builtins()).expect("valid plan");

        assert!(
            plan.argv
                .ends_with(&["--model".to_string(), "claude-haiku-4-5".to_string()])
        );
        assert_eq!(plan.model_label.as_deref(), Some("Haiku"));
    }

    #[test]
    fn codex_plan_uses_the_resolved_agent_worktree_without_filesystem_lookup() {
        let cfg = lazybox_config::Config::default();
        let temp = tempfile::tempdir().expect("tempdir");
        let resolved = temp.path().join("resolved");
        let alias = temp.path().join("alias");
        std::fs::create_dir(&resolved).expect("resolved directory");
        std::os::unix::fs::symlink(&resolved, &alias).expect("worktree symlink");
        let mut input = input(TerminalKind::Agent("codex".into()));
        input.autonomous = true;
        input.cwd = PathBuf::from("/worktrees/widget-link");
        input.agent_worktree = alias.clone();

        let plan =
            build_spawn_plan(input, &cfg, &Registry::default_builtins()).expect("valid plan");

        let path = serde_json::to_string(&alias.to_string_lossy()).expect("serialize path");
        assert_eq!(plan.cwd, PathBuf::from("/worktrees/widget-link"));
        assert!(
            plan.argv
                .contains(&format!("projects={{{path}={{trust_level=\"trusted\"}}}}"))
        );
    }

    #[test]
    fn shell_plan_uses_resolved_command_and_main_flags() {
        let cfg = lazybox_config::Config::default();
        let mut input = input(TerminalKind::Shell);
        input.shell_command = "/bin/fish".into();
        input.landed_on_main = true;

        let plan =
            build_spawn_plan(input, &cfg, &Registry::default_builtins()).expect("valid plan");

        assert_eq!(plan.argv, vec!["/bin/fish"]);
        assert_eq!(plan.hint, "github-acme-widget-657-shell");
        assert_eq!(plan.persist_key, None);
        assert!(plan.flags.on_main);
        assert!(!plan.flags.no_permission);
    }

    #[test]
    fn unknown_agent_fails_during_planning() {
        let error = build_spawn_plan(
            input(TerminalKind::Agent("missing".into())),
            &lazybox_config::Config::default(),
            &Registry::default_builtins(),
        )
        .err()
        .expect("unknown agent");

        assert_eq!(error, SpawnPlanError::UnknownAgent("missing".into()));
    }

    #[test]
    fn read_only_agent_plan_never_enables_unattended_bypass() {
        let mut cfg = lazybox_config::Config::default();
        cfg.agent.autonomous_skip_permissions = true;
        let mut input = input(TerminalKind::Agent("codex".into()));
        input.autonomous = true;
        input.access = AgentRunAccess::ReadOnly;

        let plan =
            build_spawn_plan(input, &cfg, &Registry::default_builtins()).expect("valid plan");

        assert!(!plan.flags.no_permission);
        assert_eq!(plan.access, AgentRunAccess::ReadOnly);
        assert!(
            plan.argv
                .windows(2)
                .any(|args| args == ["--sandbox", "read-only"])
        );
    }
}
