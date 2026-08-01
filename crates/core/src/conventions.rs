//! Commit / PR naming conventions the agent-work brief injects.
//!
//! The shared work preamble (`prompts/agent-work.md`) already bakes in
//! the default house style — Conventional Commits, a `(#N)` PR-title
//! suffix, a `Closes #N.` body line. This type lets a user override
//! that default from `~/.lazybox/config.yaml`; the prompt builders in
//! [`crate::prompts`] read it and inject the matching guidance. An
//! unset `conventions:` block resolves to [`Conventions::default`],
//! which is byte-for-byte the historical behavior.

use serde::{Deserialize, Serialize};

/// How commit messages (and the matching PR-title prefix) should be
/// styled in agent-authored work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CommitStyle {
    /// No particular convention; the agent writes plain, descriptive
    /// messages.
    None,
    /// A house style described in [`Conventions::custom_instruction`].
    Custom,
    /// [Conventional Commits](https://www.conventionalcommits.org/)
    /// (`feat:`, `fix:`, `chore:`, `refactor:`, …) — the default the
    /// shared work preamble already instructs. An unrecognized
    /// `commit_style` value also lands here (`#[serde(other)]` — must
    /// be the last variant) so a typo degrades to the safe default
    /// instead of failing the whole config load.
    #[default]
    #[serde(other)]
    Conventional,
}

/// Commit/PR conventions the work brief tells spawned agents to
/// follow. Defaults match what the shared preamble already bakes in
/// (Conventional Commits, the `Closes #N.` body line), so an unset
/// `conventions:` block changes nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Conventions {
    /// Commit-message / PR-title-prefix style. See [`CommitStyle`].
    pub commit_style: CommitStyle,
    /// Free-text house style injected verbatim when
    /// `commit_style: custom`. Ignored for other styles. A blank value
    /// falls back to the preamble's default guidance.
    pub custom_instruction: Option<String>,
    /// Keep the `Closes #N.` body line that collapses an issue and its
    /// PR into a single lazybox row. Default `true`; set `false` to
    /// drop it (e.g. repos that close issues manually).
    pub include_closes: bool,
}

impl Default for Conventions {
    fn default() -> Self {
        Self {
            commit_style: CommitStyle::Conventional,
            custom_instruction: None,
            include_closes: true,
        }
    }
}

impl Conventions {
    /// Directive that countermands the preamble's standing "start the
    /// PR body with a `Closes #N.` line" guidance when the user has
    /// disabled the issue↔PR auto-close. `None` when `include_closes`
    /// is true (the preamble stands unchallenged). Needed because the
    /// preamble is a static, unconditional instruction — dropping the
    /// concrete `Closes #N.` clause from the task text alone leaves the
    /// agent still told to close the issue.
    pub fn closes_override(&self) -> Option<&'static str> {
        (!self.include_closes).then_some(
            "Do NOT add a `Closes #N.` line to the PR body — this repository closes \
             issues manually. Disregard the auto-close guidance in the principles above.",
        )
    }

    /// Guidance to append to a work brief when the configured style
    /// *differs* from the preamble's Conventional-Commits default.
    /// `None` when the default is in effect (the preamble already
    /// covers it) — so a default config appends nothing and the brief
    /// is unchanged from the historical output.
    pub fn commit_override(&self) -> Option<String> {
        match self.commit_style {
            CommitStyle::Conventional => None,
            CommitStyle::None => Some(
                "Commit-message convention: none required. Disregard the Conventional \
                 Commits guidance in the principles above and write plain, descriptive \
                 commit messages and PR title."
                    .to_string(),
            ),
            CommitStyle::Custom => {
                let text = self
                    .custom_instruction
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())?;
                Some(format!(
                    "Commit-message convention (house style — overrides the Conventional \
                     Commits guidance in the principles above): {text}"
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_conventional_with_closes() {
        let c = Conventions::default();
        assert_eq!(c.commit_style, CommitStyle::Conventional);
        assert!(c.include_closes);
        // The default matches the preamble, so nothing is appended.
        assert_eq!(c.commit_override(), None);
    }

    #[test]
    fn none_style_emits_an_opt_out_override() {
        let c = Conventions {
            commit_style: CommitStyle::None,
            ..Default::default()
        };
        let text = c.commit_override().expect("override present");
        assert!(text.contains("none required"));
        assert!(text.contains("Disregard the Conventional"));
    }

    #[test]
    fn custom_style_injects_the_instruction() {
        let c = Conventions {
            commit_style: CommitStyle::Custom,
            custom_instruction: Some("  Gitmoji prefixes on every commit  ".to_string()),
            ..Default::default()
        };
        let text = c.commit_override().expect("override present");
        assert!(text.contains("Gitmoji prefixes on every commit"));
        // Trimmed, not verbatim with surrounding whitespace.
        assert!(!text.contains("  Gitmoji"));
    }

    #[test]
    fn closes_override_only_when_disabled() {
        // Default (include_closes: true) leaves the preamble unchallenged.
        assert_eq!(Conventions::default().closes_override(), None);
        let off = Conventions {
            include_closes: false,
            ..Default::default()
        };
        let text = off.closes_override().expect("override present");
        assert!(text.contains("Do NOT add a `Closes #N.` line"));
        assert!(text.contains("Disregard the auto-close guidance"));
    }

    #[test]
    fn blank_custom_instruction_falls_back_to_default() {
        let c = Conventions {
            commit_style: CommitStyle::Custom,
            custom_instruction: Some("   ".to_string()),
            ..Default::default()
        };
        assert_eq!(c.commit_override(), None);
    }

    #[test]
    fn unknown_commit_style_deserializes_to_conventional() {
        // A typo must not fail the deserialize — it degrades to the
        // safe default (`#[serde(other)]`).
        let c: Conventions = serde_json::from_str(r#"{"commit_style":"bogus"}"#).unwrap();
        assert_eq!(c.commit_style, CommitStyle::Conventional);
    }

    #[test]
    fn known_styles_round_trip() {
        for (raw, style) in [
            (
                r#"{"commit_style":"conventional"}"#,
                CommitStyle::Conventional,
            ),
            (r#"{"commit_style":"none"}"#, CommitStyle::None),
            (r#"{"commit_style":"custom"}"#, CommitStyle::Custom),
        ] {
            let c: Conventions = serde_json::from_str(raw).unwrap();
            assert_eq!(c.commit_style, style);
        }
    }
}
