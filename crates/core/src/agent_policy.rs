//! Standing **agent policies** — the named house rules lazybox states in
//! every spawned agent's briefing, each one individually overridable.
//!
//! The briefing an agent reads at spawn has two kinds of content. Most of
//! it is *mechanics*: what the `working` label means, that `@lazybox`
//! spawns an agent, where the tracker record is on disk. Those are facts
//! about lazybox, and a user who "turned one off" would simply be lied to.
//!
//! A **policy** is the other kind: a rule about how the user wants work
//! done here. "Ask before filing an issue" and "prefer one PR over a
//! stack" are opinions, not facts, and a different user — or a different
//! repo — is entitled to a different one. So they are modelled as an
//! ordered set of `(id, text)` rules rather than baked into the blurb's
//! string literal, and the id is the handle an override names.
//!
//! ## Overriding
//!
//! `policies:` in `~/.lazybox/config.yaml` is a map from policy id to
//! either a bool (`false` drops the rule, `true` re-asserts the built-in
//! wording) or a string (replacement prose). An id the built-ins do not
//! define **adds** a rule, so a user's own house rule rides the same
//! channel as lazybox's. `repos.<owner/name>.policies:` layers on top of
//! the global map for work in that repo.
//!
//! ```yaml
//! policies:
//!   one-self-contained-pr: false                  # drop it entirely
//!   ask-before-filing-a-record: "Ask me first."   # reword it
//!   house-rule: "Never touch `main` directly."    # add your own
//! repos:
//!   acme/api:
//!     policies:
//!       one-self-contained-pr: true               # ...but keep it here
//! ```
//!
//! ## Rendering
//!
//! [`AgentPolicies::render`] emits prose an LLM reads — a titled block of
//! bullets — not a config dump. The set is rendered once per session, in
//! the spawn-intrinsic briefing that reaches *every* agent kind (Claude,
//! Codex, Cursor, `GenericCli`), so a rule holds for a bare `a c` start
//! as surely as for a `w` work prompt.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One standing rule: a stable `id` (the override handle) and the prose
/// the agent actually reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentPolicy {
    /// Stable handle an override names. Kebab-case by convention;
    /// never renamed, because a rename silently drops a user's override.
    pub id: String,
    /// The rule, as one paragraph of prose. Rendered as a bullet.
    pub text: String,
}

/// How a config entry overrides one policy. Untagged so YAML can spell
/// it either way: `id: false` toggles, `id: "text"` rewords.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentPolicyOverride {
    /// `false` drops the rule from the briefing; `true` keeps the
    /// built-in wording (useful to re-assert, per repo, a rule the
    /// global config turned off).
    Enabled(bool),
    /// Replacement prose. Also the spelling that *adds* a rule the
    /// built-ins do not define. A blank string is treated as `false`:
    /// an empty bullet is noise, not a policy.
    Text(String),
}

/// The `policies:` map as written in config — global, or under one repo.
/// A newtype rather than a bare map so `Config` and `RepoConfig` name
/// the same type and the layering lives in one place.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentPolicyOverrides(pub BTreeMap<String, AgentPolicyOverride>);

impl AgentPolicyOverrides {
    /// True when nothing is declared — the common case, and the one
    /// `skip_serializing_if` keeps out of a written config file.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// `ask-before-filing-a-record`: lazybox's default used to be the
/// opposite — the work preamble told an agent that a filed issue *was*
/// the deliverable for any follow-up it noticed. That produced tracker
/// noise nobody asked for, so the default is now to propose and wait.
/// Filing itself is unchanged once permission is given, including the
/// rule that a tracked item never gets a second workspace beside it.
pub const ASK_BEFORE_FILING: &str = "ask-before-filing-a-record";

/// `one-self-contained-pr`: prefer one PR, even a large one, over a
/// stack. A stack moves work onto the reviewer and, when a child lands
/// by squash, can strand its parent's commits off the default branch.
pub const ONE_SELF_CONTAINED_PR: &str = "one-self-contained-pr";

/// The built-in rules, in the order they are rendered. Deliberately
/// short: every rule here is paid for in context on every spawn, and a
/// list long enough to skim past is a list that steers nothing.
fn builtin_policies() -> Vec<AgentPolicy> {
    vec![
        AgentPolicy {
            id: ASK_BEFORE_FILING.to_string(),
            text: "Never open a GitHub issue or a Linear ticket without the user's explicit \
                   go-ahead — not for a follow-up you noticed, not for a slice you carved out, \
                   not because a prompt said to file one \"if needed\". Say what you would file \
                   and wait for a yes. Once you have it, the filed record is the deliverable: \
                   report its URL."
                .to_string(),
        },
        AgentPolicy {
            id: ONE_SELF_CONTAINED_PR.to_string(),
            text: "Prefer one self-contained pull request, even a large one, over a stack of \
                   dependent PRs. A stack moves merge-order work onto the reviewer, and a \
                   stacked child that lands by squash can strand its parent's commits off the \
                   default branch. Split only when the user asks you to."
                .to_string(),
        },
    ]
}

/// The resolved, ordered set of standing rules for one spawn.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentPolicies {
    policies: Vec<AgentPolicy>,
}

impl AgentPolicies {
    /// lazybox's shipped rules, with no override applied.
    pub fn builtin() -> Self {
        Self {
            policies: builtin_policies(),
        }
    }

    /// Ids of the shipped rules, in render order — what a user names to
    /// override one, and what the config reference documents.
    pub fn builtin_ids() -> Vec<String> {
        builtin_policies().into_iter().map(|p| p.id).collect()
    }

    /// Built-ins with `layers` applied in order, each layer overriding
    /// the ones before it (global config, then the per-repo map).
    ///
    /// Built-in order is preserved; a rule an override *adds* appends in
    /// id order after them, so a user's own rules read as a block rather
    /// than interleaving with lazybox's.
    pub fn resolve<'a>(layers: impl IntoIterator<Item = &'a AgentPolicyOverrides>) -> Self {
        let mut merged: BTreeMap<String, AgentPolicyOverride> = BTreeMap::new();
        for layer in layers {
            for (id, value) in &layer.0 {
                merged.insert(id.clone(), value.clone());
            }
        }

        let mut policies = Vec::new();
        for builtin in builtin_policies() {
            match merged.remove(&builtin.id) {
                None | Some(AgentPolicyOverride::Enabled(true)) => policies.push(builtin),
                Some(AgentPolicyOverride::Enabled(false)) => {}
                Some(AgentPolicyOverride::Text(text)) => {
                    if let Some(text) = non_blank(&text) {
                        policies.push(AgentPolicy {
                            id: builtin.id,
                            text,
                        });
                    }
                }
            }
        }
        // What is left names no built-in, so it is a rule the user is
        // adding. `true` adds nothing — there is no shipped wording to
        // re-assert — and is ignored rather than rendered as an empty
        // bullet.
        for (id, value) in merged {
            if let AgentPolicyOverride::Text(text) = value
                && let Some(text) = non_blank(&text)
            {
                policies.push(AgentPolicy { id, text });
            }
        }
        Self { policies }
    }

    /// The rules, in render order.
    pub fn iter(&self) -> impl Iterator<Item = &AgentPolicy> {
        self.policies.iter()
    }

    /// True when every rule was turned off — the briefing then omits the
    /// section header rather than printing an empty one.
    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }

    /// The prose of the rule `id`, or `None` when it is not in force.
    pub fn text(&self, id: &str) -> Option<&str> {
        self.policies
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.text.as_str())
    }

    /// The briefing block: a titled paragraph followed by one bullet per
    /// rule, or the empty string when no rule is in force. Ends without
    /// a trailing newline so the caller owns the joining.
    pub fn render(&self) -> String {
        if self.policies.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "Standing rules — how this user wants work done here. They hold for the whole \
             session unless the user says otherwise in it, and they outrank a habit or a \
             default you brought with you:",
        );
        for policy in &self.policies {
            out.push_str("\n  - ");
            out.push_str(&policy.text);
        }
        out
    }
}

/// `Some(trimmed)` for text with content, `None` for blank — so a
/// blank override reads as "drop this rule" instead of rendering a
/// bullet with nothing after the dash.
fn non_blank(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overrides(pairs: &[(&str, AgentPolicyOverride)]) -> AgentPolicyOverrides {
        AgentPolicyOverrides(
            pairs
                .iter()
                .map(|(id, value)| ((*id).to_string(), value.clone()))
                .collect(),
        )
    }

    #[test]
    fn builtins_carry_both_standing_rules() {
        let policies = AgentPolicies::builtin();
        assert_eq!(
            AgentPolicies::builtin_ids(),
            vec![
                ASK_BEFORE_FILING.to_string(),
                ONE_SELF_CONTAINED_PR.to_string()
            ]
        );
        let ask = policies.text(ASK_BEFORE_FILING).expect("the filing rule");
        assert!(
            ask.contains("without the user's explicit go-ahead"),
            "the filing rule must state the permission gate: {ask}"
        );
        let pr = policies.text(ONE_SELF_CONTAINED_PR).expect("the PR rule");
        assert!(
            pr.contains("one self-contained pull request") && pr.contains("stack"),
            "the PR rule must prefer one PR over a stack: {pr}"
        );
    }

    #[test]
    fn an_override_can_turn_one_rule_off() {
        let config = overrides(&[(ONE_SELF_CONTAINED_PR, AgentPolicyOverride::Enabled(false))]);
        let policies = AgentPolicies::resolve([&config]);
        assert!(policies.text(ONE_SELF_CONTAINED_PR).is_none());
        // ...without taking the other rule with it.
        assert!(policies.text(ASK_BEFORE_FILING).is_some());
        assert!(!policies.render().contains("self-contained pull request"));
    }

    #[test]
    fn an_override_can_reword_one_rule() {
        let config = overrides(&[(
            ASK_BEFORE_FILING,
            AgentPolicyOverride::Text("File whatever you like.".into()),
        )]);
        let policies = AgentPolicies::resolve([&config]);
        assert_eq!(
            policies.text(ASK_BEFORE_FILING),
            Some("File whatever you like.")
        );
        let rendered = policies.render();
        assert!(rendered.contains("File whatever you like."));
        assert!(
            !rendered.contains("explicit go-ahead"),
            "the replacement must replace, not append: {rendered}"
        );
    }

    #[test]
    fn an_unknown_id_adds_a_rule_of_the_users_own() {
        let config = overrides(&[(
            "house-rule",
            AgentPolicyOverride::Text("Never touch `main` directly.".into()),
        )]);
        let policies = AgentPolicies::resolve([&config]);
        assert_eq!(
            policies.text("house-rule"),
            Some("Never touch `main` directly.")
        );
        // Added rules render after the built-ins, not between them.
        let ids: Vec<&str> = policies.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![ASK_BEFORE_FILING, ONE_SELF_CONTAINED_PR, "house-rule"]
        );
    }

    #[test]
    fn a_later_layer_wins_and_can_re_assert_a_dropped_rule() {
        let global = overrides(&[(ONE_SELF_CONTAINED_PR, AgentPolicyOverride::Enabled(false))]);
        let repo = overrides(&[(ONE_SELF_CONTAINED_PR, AgentPolicyOverride::Enabled(true))]);
        // Global alone drops it; the repo layer puts the shipped wording
        // back, which is the only reason `true` is a meaningful value.
        assert!(
            AgentPolicies::resolve([&global])
                .text(ONE_SELF_CONTAINED_PR)
                .is_none()
        );
        let both = AgentPolicies::resolve([&global, &repo]);
        assert_eq!(
            both.text(ONE_SELF_CONTAINED_PR),
            AgentPolicies::builtin().text(ONE_SELF_CONTAINED_PR)
        );
    }

    #[test]
    fn a_blank_override_drops_the_rule_rather_than_rendering_an_empty_bullet() {
        for blank in ["", "   ", "\n\t "] {
            let config = overrides(&[(
                ASK_BEFORE_FILING,
                AgentPolicyOverride::Text(blank.to_string()),
            )]);
            let policies = AgentPolicies::resolve([&config]);
            assert!(
                policies.text(ASK_BEFORE_FILING).is_none(),
                "blank {blank:?}"
            );
            assert!(!policies.render().contains("-  "), "blank {blank:?}");
        }
    }

    #[test]
    fn turning_every_rule_off_renders_nothing_at_all() {
        let config = overrides(&[
            (ASK_BEFORE_FILING, AgentPolicyOverride::Enabled(false)),
            (ONE_SELF_CONTAINED_PR, AgentPolicyOverride::Enabled(false)),
        ]);
        let policies = AgentPolicies::resolve([&config]);
        assert!(policies.is_empty());
        // No header, no stray bullet — the section disappears whole.
        assert_eq!(policies.render(), "");
    }

    #[test]
    fn render_is_prose_with_one_bullet_per_rule() {
        let rendered = AgentPolicies::builtin().render();
        assert!(rendered.starts_with("Standing rules"));
        assert_eq!(rendered.matches("\n  - ").count(), 2);
        assert!(!rendered.ends_with('\n'), "the caller owns the joining");
    }

    #[test]
    fn the_builtin_block_stays_inside_its_own_budget() {
        // The rendered block rides in every agent's context on every
        // launch, alongside the mechanics blurb that has its own cap in
        // `lazybox-agents`. Each half guards its own bytes: this one is
        // the prose lazybox ships, so it is the half a change *here*
        // can grow. ~900 bytes is two paragraph-length rules with room
        // for a third; a set that needs more than that has stopped
        // being a set of standing rules and become a manual.
        let rendered = AgentPolicies::builtin().render();
        assert!(
            rendered.len() <= 900,
            "the built-in standing rules should stay tight: {} bytes",
            rendered.len()
        );
    }

    #[test]
    fn overrides_parse_in_both_spellings_and_round_trip() {
        // The YAML spelling is covered where `serde_yaml` lives
        // (`lazybox-config`); this pins the untagged shape itself —
        // a bool is a toggle, a string is replacement prose — which is
        // what makes both spellings legal in one map.
        let json = r#"{"one-self-contained-pr": false,
                       "ask-before-filing-a-record": "Ask me first."}"#;
        let parsed: AgentPolicyOverrides = serde_json::from_str(json).expect("parse");
        assert_eq!(
            parsed.0.get(ONE_SELF_CONTAINED_PR),
            Some(&AgentPolicyOverride::Enabled(false))
        );
        assert_eq!(
            parsed.0.get(ASK_BEFORE_FILING),
            Some(&AgentPolicyOverride::Text("Ask me first.".into()))
        );
        let round: AgentPolicyOverrides =
            serde_json::from_str(&serde_json::to_string(&parsed).expect("write")).expect("reparse");
        assert_eq!(round, parsed);
    }
}
