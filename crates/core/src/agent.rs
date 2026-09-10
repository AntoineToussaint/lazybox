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
    fn capability_aliases_is_unset_only_when_all_empty() {
        assert!(CapabilityAliases::default().is_unset());
        let set = CapabilityAliases {
            high: Some("L".into()),
            ..Default::default()
        };
        assert!(!set.is_unset());
    }

    #[test]
    fn overlay_replaces_set_fields_and_keeps_the_rest() {
        let mut base = CapabilityAliases {
            best: None,
            high: Some("L".into()),
            medium: Some("M".into()),
            low: Some("S".into()),
        };
        base.overlay(&CapabilityAliases {
            best: Some("B".into()),
            high: Some("X".into()),
            ..Default::default()
        });
        // `best` and `high` came from the overlay; `medium`/`low` the
        // overlay left unset, so the base mappings survive.
        assert_eq!(base.best.as_deref(), Some("B"));
        assert_eq!(base.high.as_deref(), Some("X"));
        assert_eq!(base.medium.as_deref(), Some("M"));
        assert_eq!(base.low.as_deref(), Some("S"));
    }

    #[test]
    fn best_capability_maps_to_its_alias() {
        use crate::CapabilityTier;
        let m = AgentModels {
            tiers: vec![ModelTier {
                alias: "B".into(),
                label: "Opus · max".into(),
                short: None,
                args: vec![
                    "--model".into(),
                    "opus".into(),
                    "--reasoning-effort".into(),
                    "max".into(),
                ],
            }],
            capability: CapabilityAliases {
                best: Some("B".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(m.alias_for_capability(CapabilityTier::Best), Some("B"));
        assert_eq!(
            m.resolve_args(m.alias_for_capability(CapabilityTier::Best)),
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
        // Default names an absent alias, and each capability tier points
        // at a tier the (empty) menu doesn't define.
        let m = AgentModels {
            default: Some("L".into()),
            replace: false,
            tiers: vec![],
            capability: CapabilityAliases {
                best: Some("B".into()),
                high: Some("H".into()),
                ..Default::default()
            },
            deprecated_priority: CapabilityAliases::default(),
            unknown: Default::default(),
        };
        assert_eq!(
            m.dangling_aliases(),
            vec![
                ("default".to_string(), "L".to_string()),
                ("capability.best".to_string(), "B".to_string()),
                ("capability.high".to_string(), "H".to_string()),
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
            vec!["--model".to_string(), "claude-opus-5".to_string()]
        );
        let default_tier = m.tier(m.default.as_deref().unwrap()).unwrap();
        assert!(!default_tier.excluded_from_default());
    }

    /// The pinned ids are bare — a `[1m]` long-context suffix bills at a
    /// premium past 200k tokens, which a bare spawn must not opt into.
    #[test]
    fn builtin_claude_tiers_pin_bare_model_ids() {
        let m = AgentModels::builtin("claude").unwrap();
        assert_eq!(
            m.tiers
                .iter()
                .map(|t| t.model_id().expect("every built-in tier pins a model"))
                .collect::<Vec<_>>(),
            vec![
                "claude-haiku-4-5",
                "claude-sonnet-5",
                "claude-opus-5",
                "claude-fable-5-1",
            ]
        );
    }

    #[test]
    fn model_id_reads_both_flag_spellings() {
        let tier = |args: &[&str]| ModelTier {
            alias: "X".into(),
            label: "X".into(),
            short: None,
            args: args.iter().map(|a| (*a).to_string()).collect(),
        };
        assert_eq!(
            tier(&["--model", "claude-opus-5"]).model_id(),
            Some("claude-opus-5")
        );
        assert_eq!(
            tier(&["--model=claude-opus-5"]).model_id(),
            Some("claude-opus-5")
        );
        assert_eq!(tier(&["-m", "gpt-5"]).model_id(), Some("gpt-5"));
        // Attached short form is not parsed — a wrong id is worse than none.
        assert_eq!(tier(&["-mgpt-5"]).model_id(), None);
        assert_eq!(
            tier(&["--reasoning-effort", "max", "--model", "opus"]).model_id(),
            Some("opus")
        );
        assert_eq!(tier(&["--reasoning-effort", "max"]).model_id(), None);
    }

    #[test]
    fn overlay_tiers_replaces_in_place_and_appends_the_rest() {
        let mut m = AgentModels::builtin("claude").unwrap();
        m.overlay_tiers(&[
            ModelTier {
                alias: "L".into(),
                label: "Opus".into(),
                short: Some("Op".into()),
                args: vec!["--model".into(), "claude-opus-5[1m]".into()],
            },
            ModelTier {
                alias: "B".into(),
                label: "Opus · max".into(),
                short: None,
                args: vec!["--model".into(), "claude-opus-5".into()],
            },
        ]);
        // `L` kept its slot (menu order is display order); `B` appended.
        assert_eq!(
            m.tiers.iter().map(|t| t.alias.as_str()).collect::<Vec<_>>(),
            vec!["S", "M", "L", "XL", "B"]
        );
        assert_eq!(m.tier("L").unwrap().model_id(), Some("claude-opus-5[1m]"));
        // The tiers the overlay didn't mention survive untouched.
        assert_eq!(m.tier("M").unwrap().model_id(), Some("claude-sonnet-5"));
    }

    #[test]
    fn fable_tiers_are_excluded_from_default() {
        let fable = ModelTier {
            alias: "F".into(),
            label: "Fable".into(),
            short: None,
            args: vec!["--model".into(), "claude-fable-5".into()],
        };
        assert!(fable.excluded_from_default());
        // A coding tier whose unrelated args merely contain the
        // substring is not a Fable tier — before #1598 this lost both
        // its default eligibility and its capability mapping.
        let opus_with_a_fable_path = ModelTier {
            alias: "L".into(),
            label: "Opus".into(),
            short: None,
            args: vec![
                "--model".into(),
                "claude-opus-5".into(),
                "--settings".into(),
                "/Users/me/fable/settings.json".into(),
            ],
        };
        assert!(!opus_with_a_fable_path.excluded_from_default());
        // A tier that names no model is still judged on its argv — the
        // only evidence available.
        let implicit_fable = ModelTier {
            alias: "F".into(),
            label: "Fable".into(),
            short: None,
            args: vec!["--profile".into(), "fable-writing".into()],
        };
        assert!(implicit_fable.excluded_from_default());
        // The built-in menu now ships a Fable tier of its own (#1600).
        // It must be the only ineligible one, and never the default.
        let m = AgentModels::builtin("claude").unwrap();
        assert!(m.tier("XL").unwrap().excluded_from_default());
        assert_ne!(m.default.as_deref(), Some("XL"));
        for tier in m.tiers.iter().filter(|t| t.alias != "XL") {
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
    fn builtin_claude_maps_capability_to_tier_and_model() {
        use crate::CapabilityTier;
        let m = AgentModels::builtin("claude").unwrap();
        // high → Opus, medium → Sonnet, low → Haiku.
        assert_eq!(m.alias_for_capability(CapabilityTier::High), Some("L"));
        assert_eq!(m.alias_for_capability(CapabilityTier::Medium), Some("M"));
        assert_eq!(m.alias_for_capability(CapabilityTier::Low), Some("S"));
        // And each alias resolves to that tier's model args.
        assert_eq!(
            m.resolve_args(m.alias_for_capability(CapabilityTier::High)),
            vec!["--model".to_string(), "claude-opus-5".to_string()]
        );
        assert_eq!(
            m.resolve_args(m.alias_for_capability(CapabilityTier::Low)),
            vec!["--model".to_string(), "claude-haiku-4-5".to_string()]
        );
    }

    #[test]
    fn unmapped_capability_yields_no_alias() {
        use crate::CapabilityTier;
        // An agent menu with no capability map (the default) never routes
        // a declared tier to a model — the spawn keeps the agent's default.
        let m = AgentModels {
            tiers: AgentModels::builtin("claude").unwrap().tiers,
            ..Default::default()
        };
        assert_eq!(m.alias_for_capability(CapabilityTier::High), None);
    }

    /// #1598 refused to route *any* capability tier onto a Fable-class
    /// model, at every lookup. That also blocked the built-in menu's own
    /// deliberate `best → XL`, which #1600 exists to wire — so the guard
    /// moved to where it can tell the two cases apart:
    /// `Config::agent_models` strips an **inherited** mapping, while a
    /// menu that declares both the tier and the mapping is honored here.
    #[test]
    fn a_declared_capability_mapping_onto_fable_is_honored() {
        use crate::CapabilityTier;
        let m = AgentModels {
            tiers: vec![ModelTier {
                alias: "F".into(),
                label: "Fable".into(),
                short: None,
                args: vec!["--model".into(), "claude-fable-5".into()],
            }],
            capability: CapabilityAliases {
                best: Some("F".into()),
                high: Some("F".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(m.alias_for_capability(CapabilityTier::Best), Some("F"));
        assert_eq!(m.alias_for_capability(CapabilityTier::High), Some("F"));
        assert_eq!(
            m.resolve_args(m.alias_for_capability(CapabilityTier::High)),
            vec!["--model".to_string(), "claude-fable-5".to_string()]
        );
        // The raw mapping still reads the same — nothing filters it now.
        assert_eq!(m.capability.alias_for(CapabilityTier::High), Some("F"));
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
    /// Compact one-glyph form for the sidebar model badge (`◆O` for
    /// `"Opus"`). Optional — the badge falls back to the label's first
    /// character when unset, so a tier only declares this to override
    /// that default or disambiguate two tiers sharing a first letter.
    #[serde(default)]
    pub short: Option<String>,
    /// Args appended to the agent's spawn argv to select this model.
    #[serde(default)]
    pub args: Vec<String>,
}

impl ModelTier {
    /// True when this tier pins a creative/writing-class model (Fable)
    /// that must never be a coding agent's *default*, nor the target a
    /// declared capability tier routes to. The tier stays spawnable
    /// through an explicit chord; only the default-tier resolution, the
    /// default-model picker, and capability routing exclude it.
    ///
    /// Judged on the model id this tier actually pins
    /// ([`Self::model_id`]), not on the whole argv: a tier is identified
    /// by the model it names, and scanning every arg misread any tier
    /// whose unrelated flags happened to contain the substring — a
    /// `--settings /home/me/fable/x.json` on an Opus tier read as Fable
    /// and lost its mapping. A tier that names no `--model` at all still
    /// falls back to the argv scan, because there the args are the only
    /// evidence of which model it selects.
    pub fn excluded_from_default(&self) -> bool {
        match self.model_id() {
            Some(id) => id.to_ascii_lowercase().contains("fable"),
            None => self
                .args
                .iter()
                .any(|a| a.to_ascii_lowercase().contains("fable")),
        }
    }

    /// The model id this tier pins, read out of its own args — the value
    /// after `--model` / `-m`, or the `--model=<id>` long-option spelling.
    /// `None` for a tier that selects a model some other way (or not at
    /// all), which callers render as "no id to show". Surfaced so the
    /// *decision* a tier encodes is visible where a user picks it,
    /// instead of hiding behind a label like "Opus" (#1568).
    ///
    /// The attached short form (`-mgpt-5`) is deliberately not parsed: a
    /// short flag glued to its value can't be told from a different flag
    /// without knowing the agent's own option table, and guessing would
    /// print a wrong model id — worse than printing none.
    pub fn model_id(&self) -> Option<&str> {
        let mut args = self.args.iter();
        while let Some(arg) = args.next() {
            if let Some(id) = arg.strip_prefix("--model=") {
                return Some(id);
            }
            if arg == "--model" || arg == "-m" {
                return args.next().map(String::as_str);
            }
        }
        None
    }
}

/// Which tier alias each declared capability tier (`best` / `high` /
/// `medium` / `low`) maps to for an **autonomous** or bare-`w` spawn.
/// This is the config-driven bridge between the tier a task declares
/// (a label or an `@best`/`@high`/`@medium`/`@low` body marker; see
/// [`resolve_capability_tier`](crate::resolve_capability_tier)) and this
/// agent's own alias menu — so `high` can mean `L` (Opus) for Claude
/// but a different alias for another agent. An unset tier (or one
/// pointing at an alias the menu doesn't define) picks no model, so the
/// spawn falls back to the agent's default tier / default model.
///
/// The tokens are model-capability names, not priorities: they choose
/// which model runs the task and nothing else — no ranking, no queue,
/// no ordering (#1598).
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct CapabilityAliases {
    #[serde(default)]
    pub best: Option<String>,
    #[serde(default)]
    pub high: Option<String>,
    #[serde(default)]
    pub medium: Option<String>,
    #[serde(default)]
    pub low: Option<String>,
}

impl CapabilityAliases {
    /// True when no tier maps to an alias — the map is absent from
    /// config and every tier falls through to the default tier.
    pub fn is_unset(&self) -> bool {
        self.best.is_none() && self.high.is_none() && self.medium.is_none() && self.low.is_none()
    }

    /// Drop `tier`'s mapping, so it resolves to no model tier. Used to
    /// strip an *inherited* mapping a user's own menu never asked for.
    pub fn clear(&mut self, tier: crate::CapabilityTier) {
        match tier {
            crate::CapabilityTier::Best => self.best = None,
            crate::CapabilityTier::High => self.high = None,
            crate::CapabilityTier::Medium => self.medium = None,
            crate::CapabilityTier::Low => self.low = None,
        }
    }

    /// The alias `tier` maps to, verbatim — no eligibility filtering.
    /// [`AgentModels::alias_for_capability`] is the resolver callers
    /// want; this is the raw mapping, for diagnostics that need to say
    /// *what* a tier pointed at even when it isn't routable.
    pub fn alias_for(&self, tier: crate::CapabilityTier) -> Option<&str> {
        match tier {
            crate::CapabilityTier::Best => self.best.as_deref(),
            crate::CapabilityTier::High => self.high.as_deref(),
            crate::CapabilityTier::Medium => self.medium.as_deref(),
            crate::CapabilityTier::Low => self.low.as_deref(),
        }
    }

    /// The alias mapped to the tier named by `name` (`"best"` /
    /// `"high"` / `"medium"` / `"low"`), for callers that hold the token
    /// rather than the enum. Any other name maps to nothing.
    pub fn alias_for_name(&self, name: &str) -> Option<&str> {
        match name {
            "best" => self.best.as_deref(),
            "high" => self.high.as_deref(),
            "medium" => self.medium.as_deref(),
            "low" => self.low.as_deref(),
            _ => None,
        }
    }

    /// Each `(tier-token, mapped-alias)` pair the user actually set,
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

    /// Overlay `other`'s set fields onto `self`, per tier: a tier
    /// `other` maps replaces `self`'s mapping for it; one `other` leaves
    /// unset keeps `self`'s. Used to layer a user's partial `capability:`
    /// map onto an inherited built-in map without wiping the tiers
    /// the user didn't mention.
    pub fn overlay(&mut self, other: &CapabilityAliases) {
        if other.best.is_some() {
            self.best = other.best.clone();
        }
        if other.high.is_some() {
            self.high = other.high.clone();
        }
        if other.medium.is_some() {
            self.medium = other.medium.clone();
        }
        if other.low.is_some() {
            self.low = other.low.clone();
        }
    }
}

/// What an agent's menu makes of a task's model declarations — the
/// outcome of [`AgentModels::choose_model`].
#[derive(Debug, PartialEq, Eq)]
pub enum ModelChoice<'a> {
    /// Nothing the task declared names a tier this menu defines. The
    /// spawn keeps the agent's default tier / model.
    Unresolved,
    /// `request` (the highest-precedence declaration that resolved)
    /// selects the tier `alias`.
    Resolved {
        request: &'a crate::ModelRequest,
        alias: &'a str,
    },
    /// Equally authoritative declarations named different tiers. Left
    /// unselected on purpose: the alternative is letting an order the
    /// provider never promised decide which model spends the money.
    Conflict(Vec<&'a str>),
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
    /// Capability-tier → tier-alias map used when a spawn declares no
    /// explicit tier chord but the task carries a `best`/`high`/`medium`/
    /// `low` label or body marker.
    #[serde(default)]
    pub capability: CapabilityAliases,
    /// Deprecated spelling of [`Self::capability`], still accepted so a
    /// config written before the rename keeps routing (#1598). Folded
    /// under `capability` — which wins where both map the same tier —
    /// by `Config::agent_models`, and warned about at daemon start.
    /// Serialized only when the user actually wrote it, so a save
    /// never invents the dead key.
    #[serde(
        default,
        rename = "priority",
        skip_serializing_if = "CapabilityAliases::is_unset"
    )]
    pub deprecated_priority: CapabilityAliases,
    /// Keys under `models:` that lazybox does not recognize, kept
    /// verbatim so a save never drops what the user wrote.
    ///
    /// `deny_unknown_fields` would be the obvious guard and is the wrong
    /// one: a parse error propagates to `Config::load()`, whose callers
    /// `unwrap_or_default()`, so one typo would silently replace the
    /// user's ENTIRE config with defaults. Capturing instead lets config
    /// load name the stray key while everything else keeps working — the
    /// difference between "your `capabilty:` block does nothing" and a
    /// `high` label silently routing to Opus because the block it was
    /// renamed into never parsed (#1598).
    #[serde(
        flatten,
        default,
        skip_serializing_if = "std::collections::BTreeMap::is_empty"
    )]
    pub unknown: std::collections::BTreeMap<String, serde_json::Value>,
    /// Take this block as the whole menu instead of layering it over the
    /// agent's built-in one. Overlay is the default because retuning one
    /// tier shouldn't cost you the rest of the menu (#1568) — but overlay
    /// alone can only *add* to the built-in menu, so a user who wants a
    /// deliberately restricted set (say Sonnet only, with no `L` chord and
    /// no `high` → Opus routing) has no way to say so. This is that way.
    #[serde(default)]
    pub replace: bool,
}

impl AgentModels {
    /// The tier matching `alias`, if any.
    pub fn tier(&self, alias: &str) -> Option<&ModelTier> {
        self.tiers.iter().find(|t| t.alias == alias)
    }

    /// The capability map this block declares: the deprecated `priority`
    /// key folded under the current `capability` one, which wins where
    /// both name the same tier (#1598). Config's menu merge reads this
    /// so no downstream reader ever has to know the old key existed.
    pub fn declared_capability(&self) -> CapabilityAliases {
        let mut folded = self.deprecated_priority.clone();
        folded.overlay(&self.capability);
        folded
    }

    /// The tier alias this agent maps a declared capability tier to, if
    /// any. Feeds [`Self::resolve_args`] on the autonomous / bare-`w`
    /// spawn path (an explicit `w S` chord bypasses it).
    ///
    /// A mapping onto a tier that [`ModelTier::excluded_from_default`]
    /// rejects — a creative-class model like Fable — resolves to `None`:
    /// a label on a coding task must never land it on a writing model,
    /// however the capability map is configured. The tier stays reachable
    /// through an explicit chord, the same escape hatch the default
    /// resolution leaves open.
    pub fn alias_for_capability(&self, tier: crate::CapabilityTier) -> Option<&str> {
        self.capability.alias_for(tier)
    }

    /// The tier a `model:<token>` declaration names: matched against
    /// each tier's alias, then its label, then the model id it pins —
    /// case-insensitively and exactly, in that order. So `model:l`,
    /// `model:opus` and `model:claude-opus-5` all reach the same tier,
    /// while a substring rule (which would make `model:s` match
    /// "Sonnet" as readily as the `S` tier) is avoided (#1600).
    pub fn tier_for_token(&self, token: &str) -> Option<&ModelTier> {
        let by = |f: fn(&ModelTier) -> Option<&str>| {
            self.tiers
                .iter()
                .find(|t| f(t).is_some_and(|v| v.eq_ignore_ascii_case(token)))
        };
        by(|t| Some(t.alias.as_str()))
            .or_else(|| by(|t| Some(t.label.as_str())))
            .or_else(|| by(ModelTier::model_id))
    }

    /// The tier alias one declared [`ModelRequest`](crate::ModelRequest)
    /// resolves to on this menu, if any.
    pub fn alias_for_request(&self, request: &crate::ModelRequest) -> Option<&str> {
        match request {
            crate::ModelRequest::Tier(token) => {
                self.tier_for_token(token).map(|t| t.alias.as_str())
            }
            crate::ModelRequest::Capability(tier) => self.alias_for_capability(*tier),
        }
    }

    /// Walk `ranks` (from
    /// [`resolve_model_requests`](crate::resolve_model_requests),
    /// highest precedence first) and pick the tier the task asks for.
    ///
    /// The first rank that resolves against this menu wins, so a
    /// declaration this agent has no tier for falls through to the next
    /// rank instead of consuming the decision — an unrelated `model:*`
    /// label must not swallow a `high` label that would have resolved.
    ///
    /// A rank whose members resolve to *different* tiers is a
    /// contradiction the menu cannot break, and the provider gives no
    /// stable order to break it with, so nothing is selected and the
    /// caller reports the conflict. Members that agree (`model:l` and
    /// `model:opus` naming one tier) are not a conflict.
    pub fn choose_model<'a>(&'a self, ranks: &'a [Vec<crate::ModelRequest>]) -> ModelChoice<'a> {
        for rank in ranks {
            let mut chosen: Option<(&crate::ModelRequest, &str)> = None;
            let mut conflict: Vec<&str> = Vec::new();
            for request in rank {
                let Some(alias) = self.alias_for_request(request) else {
                    continue;
                };
                match chosen {
                    None => chosen = Some((request, alias)),
                    Some((_, picked)) if picked == alias => {}
                    Some((_, picked)) => {
                        if conflict.is_empty() {
                            conflict.push(picked);
                        }
                        if !conflict.contains(&alias) {
                            conflict.push(alias);
                        }
                    }
                }
            }
            if !conflict.is_empty() {
                return ModelChoice::Conflict(conflict);
            }
            if let Some((request, alias)) = chosen {
                return ModelChoice::Resolved { request, alias };
            }
        }
        ModelChoice::Unresolved
    }

    /// Aliases named by `default` or any `capability.*` that no tier in the
    /// menu defines. Each is a dangling reference that resolves to no
    /// args — the spawn silently keeps the agent's own hard-coded model
    /// instead of the tier the config appears to request. Config load
    /// surfaces these as warnings so the no-op is discoverable.
    ///
    /// Returns `(source, alias)` pairs where `source` is `"default"` or a
    /// `"capability.<tier>"` token, in a stable order (`default` first,
    /// then capability tiers strongest-first).
    pub fn dangling_aliases(&self) -> Vec<(String, String)> {
        let sources = self
            .default
            .as_deref()
            .map(|a| ("default".to_string(), a))
            .into_iter()
            .chain(
                self.capability
                    .declared()
                    .map(|(name, alias)| (format!("capability.{name}"), alias)),
            );
        sources
            .filter(|(_, alias)| self.tier(alias).is_none())
            .map(|(source, alias)| (source, alias.to_string()))
            .collect()
    }

    /// Each `(tier-token, alias)` the capability map points at a tier
    /// [`ModelTier::excluded_from_default`] rejects. The mapping parses,
    /// names a tier that really exists, and is then refused at spawn —
    /// so [`Self::dangling_aliases`] (which only knows about aliases no
    /// tier *defines*) says nothing about it. Config load surfaces these
    /// so the refusal is discoverable at startup rather than only in a
    /// footer notice at spawn time (#1598).
    pub fn excluded_capability_aliases(&self) -> Vec<(&'static str, String)> {
        self.capability
            .declared()
            .filter(|(_, alias)| {
                self.tier(alias)
                    .is_some_and(ModelTier::excluded_from_default)
            })
            .map(|(name, alias)| (name, alias.to_string()))
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

    /// Layer `tiers` onto this menu by alias: a declared tier replaces
    /// the same-alias tier in place (keeping menu order), an unknown
    /// alias appends. So a user can retune one tier without re-declaring
    /// the built-in menu around it — and without silently dropping the
    /// tiers and capability mappings they didn't mention (#1568).
    pub fn overlay_tiers(&mut self, tiers: &[ModelTier]) {
        for tier in tiers {
            match self.tiers.iter_mut().find(|t| t.alias == tier.alias) {
                Some(existing) => *existing = tier.clone(),
                None => self.tiers.push(tier.clone()),
            }
        }
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
            // can resolve to a non-coding model (Fable). The pin wins
            // over the user's `~/.claude/settings.json` `model`, so
            // config load warns when the two disagree
            // (`Config::pinned_model_warnings`).
            //
            // The ids stay bare — no `[1m]` long-context suffix. The 1M
            // window bills at a premium past 200k tokens, which a bare
            // spawn must not opt into silently; a user who wants it
            // declares it as a tier of their own.
            "claude" => Some(AgentModels {
                default: Some("L".into()),
                replace: false,
                tiers: vec![
                    ModelTier {
                        alias: "S".into(),
                        label: "Haiku".into(),
                        short: Some("H".into()),
                        args: vec!["--model".into(), "claude-haiku-4-5".into()],
                    },
                    ModelTier {
                        alias: "M".into(),
                        label: "Sonnet".into(),
                        short: Some("S".into()),
                        args: vec!["--model".into(), "claude-sonnet-5".into()],
                    },
                    ModelTier {
                        alias: "L".into(),
                        label: "Opus".into(),
                        // "Op", not "O": a lone capital O reads as the
                        // digit zero in most monospace fonts ("◆0??").
                        short: Some("Op".into()),
                        args: vec!["--model".into(), "claude-opus-5".into()],
                    },
                    // The top of the ladder. Two chars, so it claims no
                    // chord — Fable is reached deliberately, by a
                    // `model:xl` / `model:fable` label or the `best`
                    // capability word, never by a stray keystroke — and
                    // `excluded_from_default` keeps it off every bare
                    // spawn (#1600).
                    ModelTier {
                        alias: "XL".into(),
                        label: "Fable".into(),
                        short: Some("F".into()),
                        args: vec!["--model".into(), "claude-fable-5-1".into()],
                    },
                ],
                // A declared capability tier routes to the matching model
                // tier: best → Fable, high → Opus, medium → Sonnet,
                // low → Haiku. `best` used to map to nothing, so a task
                // asking for the strongest model got whatever `default`
                // happened to be — masked while `default` was `L`, a
                // silent downgrade to Haiku under `default: S` (#1600).
                //
                // Reaching a Fable-class tier from a capability word is
                // allowed only because this mapping is *declared*, here
                // and deliberately. `Config::agent_models` clears an
                // *inherited* one, so a user menu is never routed to a
                // creative-class model it never named (#1598's guard,
                // narrowed rather than dropped).
                capability: CapabilityAliases {
                    best: Some("XL".into()),
                    high: Some("L".into()),
                    medium: Some("M".into()),
                    low: Some("S".into()),
                },
                deprecated_priority: CapabilityAliases::default(),
                unknown: Default::default(),
            }),
            _ => None,
        }
    }
}
