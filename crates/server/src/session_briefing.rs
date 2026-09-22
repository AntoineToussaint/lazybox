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
}
