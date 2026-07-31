//! Agent abstraction. Any coding agent (Claude Code, Aider, Cursor, etc.)
//! implements this trait to integrate with lazybox.

use serde::{Deserialize, Serialize};

/// Configuration for a coding agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Display name (e.g., "Claude Code").
    #[serde(default = "default_name")]
    pub name: String,
    /// Command to spawn (e.g., "claude").
    #[serde(default = "default_command")]
    pub command: String,
    /// Additional args for first launch.
    #[serde(default)]
    pub args: Vec<String>,
    /// Args to resume a previous session (e.g., ["--continue"]).
    #[serde(default)]
    pub resume_args: Vec<String>,
    /// Patterns in terminal output that indicate the agent is asking a question.
    /// Used for notification detection.
    #[serde(default = "default_asking_patterns")]
    pub asking_patterns: Vec<String>,
}

fn default_name() -> String {
    "Claude Code".into()
}

fn default_command() -> String {
    "claude".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_aliases_is_unset_only_when_all_empty() {
        assert!(PriorityAliases::default().is_unset());
        let set = PriorityAliases {
            high: Some("L".into()),
            ..Default::default()
        };
        assert!(!set.is_unset());
    }

    #[test]
    fn best_priority_maps_to_its_alias() {
        use crate::PriorityTier;
        let m = AgentModels {
            tiers: vec![ModelTier {
                alias: "B".into(),
                label: "Opus · max".into(),
                args: vec![
                    "--model".into(),
                    "opus".into(),
                    "--reasoning-effort".into(),
                    "max".into(),
                ],
            }],
            priority: PriorityAliases {
                best: Some("B".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(m.alias_for_priority(PriorityTier::Best), Some("B"));
        assert_eq!(
            m.resolve_args(m.alias_for_priority(PriorityTier::Best)),
            vec![
                "--model".to_string(),
                "opus".to_string(),
                "--reasoning-effort".to_string(),
                "max".to_string(),
            ]
        );
    }

    #[test]
    fn dangling_aliases_flags_every_undefined_reference() {
        // Default names an absent alias, and each priority points at a
        // tier the (empty) menu doesn't define.
        let m = AgentModels {
            default: Some("L".into()),
            tiers: vec![],
            priority: PriorityAliases {
                best: Some("B".into()),
                high: Some("H".into()),
                ..Default::default()
            },
        };
        assert_eq!(
            m.dangling_aliases(),
            vec![
                ("default".to_string(), "L".to_string()),
                ("priority.best".to_string(), "B".to_string()),
                ("priority.high".to_string(), "H".to_string()),
            ]
        );
    }

    #[test]
    fn dangling_aliases_empty_when_everything_resolves() {
        let m = AgentModels::builtin("claude").unwrap();
        assert!(m.dangling_aliases().is_empty());
    }

    #[test]
    fn resolve_args_uses_named_alias() {
        let m = AgentModels::builtin("claude").unwrap();
        assert_eq!(
            m.resolve_args(Some("M")),
            vec!["--model".to_string(), "claude-sonnet-5".to_string()]
        );
        assert_eq!(
            m.resolve_args(Some("S")),
            vec!["--model".to_string(), "claude-haiku-4-5".to_string()]
        );
    }

    #[test]
    fn resolve_args_falls_back_to_configured_default() {
        let m = AgentModels {
            default: Some("M".into()),
            tiers: AgentModels::builtin("claude").unwrap().tiers,
            ..Default::default()
        };
        assert_eq!(
            m.resolve_args(None),
            vec!["--model".to_string(), "claude-sonnet-5".to_string()]
        );
    }

    #[test]
    fn resolve_args_empty_when_no_default_and_no_alias() {
        let m = AgentModels {
            tiers: AgentModels::builtin("claude").unwrap().tiers,
            ..Default::default()
        };
        assert!(m.resolve_args(None).is_empty());
    }

    /// A bare Claude spawn must always carry an explicit `--model` for
    /// a coding tier — leaving the default unset hands the choice to
    /// Claude Code's ambient default, which can be Fable.
    #[test]
    fn builtin_claude_default_pins_an_explicit_coding_model() {
        let m = AgentModels::builtin("claude").unwrap();
        assert_eq!(m.default.as_deref(), Some("L"));
        assert_eq!(
            m.resolve_args(None),
            vec!["--model".to_string(), "claude-opus-4-8".to_string()]
        );
        let default_tier = m.tier(m.default.as_deref().unwrap()).unwrap();
        assert!(!default_tier.excluded_from_default());
    }

    #[test]
    fn fable_tiers_are_excluded_from_default() {
        let fable = ModelTier {
            alias: "F".into(),
            label: "Fable".into(),
            args: vec!["--model".into(), "claude-fable-5".into()],
        };
        assert!(fable.excluded_from_default());
        for tier in &AgentModels::builtin("claude").unwrap().tiers {
            assert!(
                !tier.excluded_from_default(),
                "{} must stay eligible",
                tier.label
            );
        }
    }

    #[test]
    fn resolve_args_empty_for_unknown_alias() {
        let m = AgentModels::builtin("claude").unwrap();
        assert!(m.resolve_args(Some("ZZ")).is_empty());
    }

    #[test]
    fn only_claude_has_builtin_tiers() {
        assert!(AgentModels::builtin("claude").is_some());
        assert!(AgentModels::builtin("codex").is_none());
        assert!(AgentModels::builtin("cursor-agent").is_none());
    }

    #[test]
    fn builtin_claude_maps_priority_to_tier_and_model() {
        use crate::PriorityTier;
        let m = AgentModels::builtin("claude").unwrap();
        // high → Opus, medium → Sonnet, low → Haiku.
        assert_eq!(m.alias_for_priority(PriorityTier::High), Some("L"));
        assert_eq!(m.alias_for_priority(PriorityTier::Medium), Some("M"));
        assert_eq!(m.alias_for_priority(PriorityTier::Low), Some("S"));
        // And each alias resolves to that tier's model args.
        assert_eq!(
            m.resolve_args(m.alias_for_priority(PriorityTier::High)),
            vec!["--model".to_string(), "claude-opus-4-8".to_string()]
        );
        assert_eq!(
            m.resolve_args(m.alias_for_priority(PriorityTier::Low)),
            vec!["--model".to_string(), "claude-haiku-4-5".to_string()]
        );
    }

    #[test]
    fn unmapped_priority_yields_no_alias() {
        use crate::PriorityTier;
        // An agent menu with no priority map (the default) never routes
        // a priority to a tier — the spawn keeps the agent's default.
        let m = AgentModels {
            tiers: AgentModels::builtin("claude").unwrap().tiers,
            ..Default::default()
        };
        assert_eq!(m.alias_for_priority(PriorityTier::High), None);
    }
}

fn default_asking_patterns() -> Vec<String> {
    vec![
        "(y/n)".into(),
        "(yes/no)".into(),
        "allow ".into(),
        "do you want".into(),
        "would you like".into(),
        "press enter".into(),
    ]
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            name: "Claude Code".into(),
            command: "claude".into(),
            args: vec![],
            resume_args: vec!["--continue".into()],
            asking_patterns: default_asking_patterns(),
        }
    }
}

impl AgentConfig {
    /// Build the command + args to spawn the agent.
    pub fn spawn_command(&self, resume: bool) -> Vec<String> {
        let mut cmd = vec![self.command.clone()];
        if resume && !self.resume_args.is_empty() {
            cmd.extend(self.resume_args.iter().cloned());
        } else {
            cmd.extend(self.args.iter().cloned());
        }
        cmd
    }
}

/// One selectable model tier for an agent: a short alias the user types,
/// a human-readable label shown in the which-key popup / help / tab
/// badge, and the CLI args appended to the agent's spawn command to
/// select that model.
///
/// The `alias` doubles as the second keystroke of the tier chord
/// (`w S`, `a M`), so a tier that wants a chord must use a single
/// character — multi-character aliases still configure a model but
/// won't get a chord. Aliases are case-sensitive (`S` = Shift-S), which
/// keeps the tier keys out of the lowercase agent keys (`c`/`x`/`u`)
/// that share the same leader namespace.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelTier {
    /// Short alias — the chord key and config handle (`"S"`, `"M"`, `"L"`).
    pub alias: String,
    /// Display name of the model (`"Haiku"`, `"Sonnet"`, `"Opus"`).
    pub label: String,
    /// Args appended to the agent's spawn argv to select this model.
    #[serde(default)]
    pub args: Vec<String>,
}

impl ModelTier {
    /// True when this tier pins a creative/writing-class model (Fable)
    /// that must never be a coding agent's *default*. The tier stays
    /// spawnable through an explicit chord; only the default-tier
    /// resolution and the default-model picker exclude it.
    pub fn excluded_from_default(&self) -> bool {
        self.args
            .iter()
            .any(|a| a.to_ascii_lowercase().contains("fable"))
    }
}

/// Which tier alias each declared task priority (`best` / `high` /
/// `medium` / `low`) maps to for an **autonomous** or bare-`w` spawn.
/// This is the config-driven bridge between the priority a task declares
/// (a label or an `@best`/`@high`/`@medium`/`@low` body marker; see
/// [`resolve_priority_tier`](crate::resolve_priority_tier)) and this
/// agent's own alias menu — so `high` can mean `L` (Opus) for Claude
/// but a different alias for another agent. An unset priority (or one
/// pointing at an alias the menu doesn't define) picks no model, so the
/// spawn falls back to the agent's default tier / default model.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct PriorityAliases {
    #[serde(default)]
    pub best: Option<String>,
    #[serde(default)]
    pub high: Option<String>,
    #[serde(default)]
    pub medium: Option<String>,
    #[serde(default)]
    pub low: Option<String>,
}

impl PriorityAliases {
    /// True when no priority maps to an alias — the map is absent from
    /// config and every priority falls through to the default tier.
    pub fn is_unset(&self) -> bool {
        self.best.is_none() && self.high.is_none() && self.medium.is_none() && self.low.is_none()
    }

    /// Each `(priority-token, mapped-alias)` pair the user actually set,
    /// in strongest-first order. Feeds config-load validation that warns
    /// on an alias the tier menu doesn't define.
    pub fn declared(&self) -> impl Iterator<Item = (&'static str, &str)> {
        [
            ("best", &self.best),
            ("high", &self.high),
            ("medium", &self.medium),
            ("low", &self.low),
        ]
        .into_iter()
        .filter_map(|(name, alias)| alias.as_deref().map(|a| (name, a)))
    }
}

/// Per-agent model menu — the ordered tiers a spawn chord can pick from
/// plus which tier a bare spawn (no chord) uses.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct AgentModels {
    /// Alias of the tier a bare spawn resolves to. `None` → the agent's
    /// own hard-coded default model (no extra args).
    #[serde(default)]
    pub default: Option<String>,
    /// Ordered tier list (`alias → { label, args }`). Order is the
    /// which-key popup / help display order.
    #[serde(default)]
    pub tiers: Vec<ModelTier>,
    /// Priority → tier-alias map used when a spawn declares no explicit
    /// tier chord but the task carries a `high`/`medium`/`low` priority.
    #[serde(default)]
    pub priority: PriorityAliases,
}

impl AgentModels {
    /// The tier matching `alias`, if any.
    pub fn tier(&self, alias: &str) -> Option<&ModelTier> {
        self.tiers.iter().find(|t| t.alias == alias)
    }

    /// The tier alias this agent maps a declared task priority to, if
    /// any. Feeds [`Self::resolve_args`] on the autonomous / bare-`w`
    /// spawn path (an explicit `w S` chord bypasses it).
    pub fn alias_for_priority(&self, tier: crate::PriorityTier) -> Option<&str> {
        match tier {
            crate::PriorityTier::Best => self.priority.best.as_deref(),
            crate::PriorityTier::High => self.priority.high.as_deref(),
            crate::PriorityTier::Medium => self.priority.medium.as_deref(),
            crate::PriorityTier::Low => self.priority.low.as_deref(),
        }
    }

    /// Aliases named by `default` or any `priority.*` that no tier in the
    /// menu defines. Each is a dangling reference that resolves to no
    /// args — the spawn silently keeps the agent's own hard-coded model
    /// instead of the tier the config appears to request. Config load
    /// surfaces these as warnings so the no-op is discoverable.
    ///
    /// Returns `(source, alias)` pairs where `source` is `"default"` or a
    /// `"priority.<tier>"` token, in a stable order (`default` first,
    /// then priorities strongest-first).
    pub fn dangling_aliases(&self) -> Vec<(String, String)> {
        let sources = self
            .default
            .as_deref()
            .map(|a| ("default".to_string(), a))
            .into_iter()
            .chain(
                self.priority
                    .declared()
                    .map(|(name, alias)| (format!("priority.{name}"), alias)),
            );
        sources
            .filter(|(_, alias)| self.tier(alias).is_none())
            .map(|(source, alias)| (source, alias.to_string()))
            .collect()
    }

    /// Resolve the spawn args for a chosen `alias`, or for the
    /// configured `default` tier when `alias` is `None`. An unknown
    /// alias (or an unset / dangling default) yields no args, so the
    /// agent falls back to its own default model.
    pub fn resolve_args(&self, alias: Option<&str>) -> Vec<String> {
        let want = alias.or(self.default.as_deref());
        want.and_then(|a| self.tier(a))
            .map(|t| t.args.clone())
            .unwrap_or_default()
    }

    /// Built-in tier menu for a known agent id, or `None` for an agent
    /// lazybox ships no model presets for. Only Claude ships presets —
    /// its model flag (`--model`) takes stable aliases; Codex / Cursor
    /// name their models differently and are left to per-agent YAML.
    pub fn builtin(agent_id: &str) -> Option<AgentModels> {
        match agent_id {
            // Claude's default tier is pinned so a bare spawn always
            // passes an explicit `--model`. With no flag, Claude Code
            // falls back to its own ambient account/CLI default, which
            // can resolve to a non-coding model (Fable).
            "claude" => Some(AgentModels {
                default: Some("L".into()),
                tiers: vec![
                    ModelTier {
                        alias: "S".into(),
                        label: "Haiku".into(),
                        args: vec!["--model".into(), "claude-haiku-4-5".into()],
                    },
                    ModelTier {
                        alias: "M".into(),
                        label: "Sonnet".into(),
                        args: vec!["--model".into(), "claude-sonnet-5".into()],
                    },
                    ModelTier {
                        alias: "L".into(),
                        label: "Opus".into(),
                        args: vec!["--model".into(), "claude-opus-4-8".into()],
                    },
                ],
                // A declared priority routes to the matching tier:
                // high → Opus, medium → Sonnet, low → Haiku. `best` is
                // left unmapped: the built-in menu has no max-reasoning
                // tier, so a `best` run is opt-in — define a best tier
                // (model + effort) and `priority.best` in YAML (#748).
                priority: PriorityAliases {
                    best: None,
                    high: Some("L".into()),
                    medium: Some("M".into()),
                    low: Some("S".into()),
                },
            }),
            _ => None,
        }
    }
}
