//! Where the spawn briefing's **standing rules** come from.
//!
//! `lazybox_agents::lazybox_session_context` takes the rules as prose — the
//! agents crate deliberately depends on no config crate. The daemon is the
//! one that holds a `Config`, so the global→per-repo resolution
//! ([`lazybox_config::Config::agent_policies`]) happens here, once, and every
//! channel the briefing rides reads it through these two helpers.
//!
//! There are four such channels — Claude's `SessionStart` hook, Codex's
//! native `developer_instructions`, a prompt prefix for hookless PTY agents,
//! and the provider-boundary injection for structured/headless runs. They
//! must all state the same rules, so none of them may render the block
//! itself.

/// The rendered standing-rules block for work in `repo` (`owner/name`), or
/// box-wide when the repo is unknown. Empty when the user turned every rule
/// off, which the briefing renders as no section at all.
pub fn standing_rules(cfg: &lazybox_config::Config, repo: Option<&str>) -> String {
    cfg.agent_policies(repo).render()
}

/// [`standing_rules`] for a caller with no `Config` in hand — the hook
/// process, which runs out-of-band of the daemon and reads config off disk
/// like every other `lazybox` subcommand. A config that will not parse falls
/// back to lazybox's built-in rules rather than silently briefing the agent
/// with none: a broken YAML file is a reason to warn, never a reason to drop
/// the user's house rules on the floor.
pub fn standing_rules_from_disk(repo: Option<&str>) -> String {
    match lazybox_config::Config::load() {
        Ok(cfg) => standing_rules(&cfg, repo),
        Err(error) => {
            tracing::warn!(
                %error,
                "session briefing: config did not load — briefing with lazybox's built-in \
                 standing rules"
            );
            lazybox_core::AgentPolicies::builtin().render()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_resolve_globally_and_per_repo() {
        let cfg = lazybox_config::Config::parse(
            r#"
policies:
  one-self-contained-pr: false
repos:
  acme/api:
    policies:
      house-rule: Rebase, never merge.
"#,
        )
        .expect("parse");
        let global = standing_rules(&cfg, None);
        assert!(global.contains("explicit go-ahead"));
        assert!(!global.contains("self-contained pull request"));
        assert!(!global.contains("Rebase, never merge."));

        let repo = standing_rules(&cfg, Some("acme/api"));
        assert!(repo.contains("Rebase, never merge."));
        // The repo layer adds to the global one; it does not restart from
        // the built-ins.
        assert!(!repo.contains("self-contained pull request"));
    }

    #[test]
    fn a_default_config_renders_lazyboxs_own_rules() {
        let rendered = standing_rules(&lazybox_config::Config::default(), None);
        assert_eq!(rendered, lazybox_core::AgentPolicies::builtin().render());
        assert!(!rendered.is_empty());
    }

    /// Every agent lazybox can spawn must actually *receive* the rules —
    /// the regression this whole mechanism exists for. An agent gets the
    /// briefing through one of two channels, decided by the agent itself:
    /// a native startup flag (`session_context_args`, which Claude and Codex
    /// implement) or, when it has none, a prefix on its first prompt. Cursor
    /// and any YAML-declared `GenericCli` take the second. Walk every kind
    /// and follow whichever channel it actually uses.
    #[test]
    fn both_standing_rules_reach_every_agent_kind() {
        let cfg = lazybox_config::Config::default();
        let rules = standing_rules(&cfg, None);
        let briefing = lazybox_agents::lazybox_session_context(&rules);

        let mut registry = lazybox_agents::registry();
        registry.register(std::sync::Arc::new(
            lazybox_agents::agent::builtins::GenericCli {
                id: "house-cli".into(),
                display_name: "House CLI".into(),
                spawn_cmd: vec!["house-cli".into()],
                resume_cmd: None,
                asking_patterns: vec![],
            },
        ));

        let ids: Vec<String> = registry.ids().map(str::to_string).collect();
        assert!(
            ids.len() >= 4,
            "expected claude/codex/cursor plus the GenericCli: {ids:?}"
        );
        for id in ids {
            let agent = registry.get(&id).expect("registered");
            let native = agent.session_context_args(&briefing);
            let delivered = if native.is_empty() {
                lazybox_agents::lazybox_session_prompt(&rules, "do the work")
            } else {
                native.join(" ")
            };
            for needle in [
                "without the user's explicit go-ahead",
                "one self-contained pull request",
            ] {
                assert!(
                    delivered.contains(needle),
                    "agent `{id}` must be told {needle:?}: {delivered}"
                );
            }
        }
    }

    /// ...and an override reaches them by the same route: turning a rule off
    /// removes it from every kind, not just the one whose channel was tested.
    #[test]
    fn an_override_reaches_every_agent_kind_too() {
        let cfg = lazybox_config::Config::parse(
            "policies:\n  one-self-contained-pr: false\n  ask-before-filing-a-record: Ask Antoine.\n",
        )
        .expect("parse");
        let rules = standing_rules(&cfg, None);
        let briefing = lazybox_agents::lazybox_session_context(&rules);
        let registry = lazybox_agents::registry();
        for id in registry.ids().map(str::to_string).collect::<Vec<_>>() {
            let agent = registry.get(&id).expect("registered");
            let native = agent.session_context_args(&briefing);
            let delivered = if native.is_empty() {
                lazybox_agents::lazybox_session_prompt(&rules, "do the work")
            } else {
                native.join(" ")
            };
            assert!(
                delivered.contains("Ask Antoine."),
                "agent `{id}` must get the replacement wording: {delivered}"
            );
            assert!(
                !delivered.contains("one self-contained pull request"),
                "agent `{id}` must not get the rule the user turned off: {delivered}"
            );
        }
    }
}
