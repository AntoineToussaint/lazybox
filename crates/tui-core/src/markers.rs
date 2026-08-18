//! Generated registry of the sidebar's on-screen markers — status
//! pills, the per-session agent-state glyph, and the passive row badges
//! — each defined once as `{ label, meaning, when it shows }` and fed
//! into the Ask Lazybox context (#883).
//!
//! The point is that these stay in sync with the code the way the action
//! catalog already does for keybindings: a new status pill or agent
//! state can't reach the UI without a documented meaning here, because
//! the enum-backed sources ([`StatusTag`], [`AgentState`]) are mapped
//! through exhaustive matches — adding a variant is a compile error until
//! it has an entry. The help context (`crate::help::agent_context`)
//! renders whatever this module returns, so Ask Lazybox can then explain
//! any pill the user sees.
//!
//! `StatusTag` is a superset of the pills the sidebar actually paints —
//! the visible pills come from `status_pills`/`lifecycle_pill` in
//! `lazybox_tui::components::sidebar::pills`, not from `StatusTag`
//! directly (e.g. `Behind` is a `StatusTag` variant but renders no row
//! pill). So the *documented set* is pinned to the real renderer by the
//! `documented_status_pills_match_the_renderer` test in `tui`, which
//! lives there because only `tui` can see both the renderer and this
//! registry — that is the drift guard that would have caught a
//! documented-but-unrendered pill.
//!
//! Not every marker is enum-backed: the passive row badges (` ARM `,
//! ` FIX `, `⎇ local`, role letters, …) are rendered ad hoc from
//! `Workspace` fields in `lazybox_tui::components::workspace_row`, so
//! they're a hand-curated list here. They gain no compile-time drift
//! guard — only the completeness test that each still appears in the
//! generated context.

use lazybox_core::StatusTag;
use lazybox_ipc::AgentState;

/// One documented marker: the on-screen `label`, a one-line `meaning`,
/// and `when` it appears. Rendered verbatim into the help context.
pub struct MarkerDoc {
    /// The text (or glyph) as it appears on screen, e.g. `CHANGES`.
    pub label: &'static str,
    /// One-line explanation of what the marker signals.
    pub meaning: &'static str,
    /// The condition under which the marker shows.
    pub when: &'static str,
}

/// Documentation for a single status pill, or `None` for the variants
/// the sidebar renders no pill for (`Behind`, `None`). Exhaustive over
/// `StatusTag`, so a new variant can't ship without a decision here; the
/// *set* of documented pills is pinned to what the real renderer emits
/// by `documented_status_pills_match_the_renderer` in `tui` (#883).
fn status_pill_doc(tag: StatusTag) -> Option<MarkerDoc> {
    let doc = |label, meaning, when| {
        Some(MarkerDoc {
            label,
            meaning,
            when,
        })
    };
    // The `label` is the on-screen glyph (#1046) — several of the
    // actionable states share one (`✓` = CI ok / approved / ready,
    // distinguished by color and column), so each `meaning` leads with the
    // state name to keep the generated Ask-Lazybox docs unambiguous. Merged
    // is deliberately NOT one of them: it carries its own terminal `⋈` join
    // glyph so a done-and-gone PR can't be mistaken for a ready-to-act one
    // (#1079). The glyph set is pinned to the renderer by
    // `documented_status_pills_match_the_renderer` in `tui`.
    match tag {
        StatusTag::Merged => doc(
            "⋈",
            "Merged — the pull request has been merged; a terminal, past-tense state.",
            "Shows on a PR whose branch was merged into its base.",
        ),
        StatusTag::Closed => doc(
            "⊘",
            "Closed — the pull request or issue was closed without merging.",
            "Shows on a closed PR or issue.",
        ),
        StatusTag::Conflict => doc(
            // Trailing U+FE0E pins `⚠` to one cell (see `pills::G_CONFLICT`);
            // the label must match the renderer for the drift guard.
            "⚠\u{fe0e}",
            "Conflict — the PR has merge conflicts with its base branch and can't merge until they're resolved.",
            "Shows when the PR conflicts with its base branch.",
        ),
        StatusTag::CiFailed => doc(
            "✗",
            "CI failing — continuous-integration checks are failing.",
            "Shows when one or more required checks failed.",
        ),
        StatusTag::CiMixed => doc(
            "±",
            "CI mixed — partly green and partly failing.",
            "Shows when some checks passed while others failed.",
        ),
        StatusTag::ChangesRequested => doc(
            "✗",
            "Changes requested — a reviewer requested changes; the PR needs edits before it can be approved.",
            "Shows when a review left the \"changes requested\" verdict.",
        ),
        StatusTag::Queued => doc(
            "⧖",
            "Queued — the PR is sitting in GitHub's merge queue.",
            "Shows once the PR has entered GitHub's merge queue.",
        ),
        StatusTag::Draft => doc(
            "◇",
            "Draft — the PR is a draft and not yet ready for review.",
            "Shows while the PR is marked draft, even with green CI.",
        ),
        StatusTag::Ready => doc(
            "✓",
            "Ready — approved with green (or no) CI, ready to merge now.",
            "Shows when the PR is approved and CI is green or unset.",
        ),
        StatusTag::Approved => doc(
            "✓",
            "Approved — a reviewer approved the PR, but CI isn't green yet.",
            "Shows when the PR is approved but CI is still pending or failing.",
        ),
        StatusTag::ReviewPending => doc(
            "◌",
            "Review pending — the PR is waiting on review; a review was requested or is pending.",
            "Shows while review is requested or pending with no verdict yet.",
        ),
        StatusTag::CiRunning => doc(
            "◔",
            "CI running — checks are still in progress.",
            "Shows while checks are queued or in progress.",
        ),
        StatusTag::CiOk => doc(
            "✓",
            "CI passing — CI is green and nothing more pressing applies.",
            "Shows when all checks passed and nothing more pressing applies.",
        ),
        // `Behind` and `None` render no status pill: the two-column
        // renderer (`lazybox_tui::components::sidebar::status_pills`)
        // has no arm for either. Behind-ness surfaces instead as the
        // `⤓main`→`behind` track-main badge and the header tally, so
        // documenting a "BEHIND" pill would describe something the row
        // never shows. The `documented_status_pills_match_the_renderer`
        // test in `tui` pins this set to what actually renders.
        StatusTag::Behind | StatusTag::None => None,
    }
}

/// Documentation for the per-session agent-state glyph (the row's state
/// slot and the terminal tab badge). Exhaustive over [`AgentState`], so
/// a new state can't ship without a meaning (#883).
fn agent_state_doc(state: &AgentState) -> MarkerDoc {
    let doc = |label, meaning, when| MarkerDoc {
        label,
        meaning,
        when,
    };
    match state {
        AgentState::Working => doc(
            "Working (an animated spinner)",
            "The agent is actively producing output or running a tool right now.",
            "Shows while the agent is mid-turn.",
        ),
        AgentState::InputNeeded => doc(
            "? InputNeeded",
            "The agent is paused waiting on you at a prompt — a permission gate, chooser, or Y/N question. Jump to it with the ask-agent jump.",
            "Shows when the agent hit a structural prompt that needs an answer.",
        ),
        AgentState::Idle => doc(
            "Idle (no glyph)",
            "The agent is idle and hasn't started a task yet.",
            "Shows on a freshly launched agent that has never run a turn.",
        ),
        AgentState::Done => doc(
            "✓ Done",
            "The agent finished its turn and is waiting to be looked at.",
            "Shows once the agent came to rest after working.",
        ),
        AgentState::Exited { .. } => doc(
            "✗ Exited",
            "The agent's process ended — a clean exit or a crash.",
            "Shows after the agent's process terminated.",
        ),
        AgentState::LimitReached => doc(
            "⏳ LimitReached",
            "The agent hit its provider usage/rate limit and is paused until you resume it.",
            "Shows while the agent is parked on a usage-limit prompt.",
        ),
    }
}

/// The row-level "spawning" glyph (#1069). Not an [`AgentState`] variant
/// — it's a client-side provisioning indicator driven by the
/// `WorktreeStep` events, shown in the same row slot as the agent-state
/// glyphs from the moment a spawn starts until the agent reports its
/// first state (or setup fails). Documented here by hand, alongside the
/// enum-backed states, so `spawning` is looked-up-able in the `?` help
/// legend and via Ask Lazybox even though no `AgentState` backs it.
pub fn spawning_doc() -> MarkerDoc {
    MarkerDoc {
        label: "◜ Spawning (an animated arc)",
        meaning: "The workspace is provisioning — cloning, creating the worktree, running setup, launching the agent — before any terminal exists to report a state.",
        when: "Shows from the moment a spawn starts until the agent reports its first state, or setup fails.",
    }
}

/// The passive row badges rendered from `Workspace` fields (not an
/// enum), left to right. Hand-curated: keep this in step with
/// `lazybox_tui::components::workspace_row`'s badge cells.
const ROW_BADGES: &[MarkerDoc] = &[
    MarkerDoc {
        label: "WORK",
        meaning: "This GitHub issue or PR is claimed by an agent, including agents running from another lazybox machine.",
        when: "Shows while the task carries the shared `working` label; starting another agent asks for confirmation.",
    },
    MarkerDoc {
        label: "⚡",
        meaning: "ARM — this PR will auto-merge once CI goes green; lazybox's client-side merge, which only fires while lazybox is running.",
        when: "Shows once you arm merge-on-green (`g g`).",
    },
    MarkerDoc {
        label: "◆",
        meaning: "AUTO — GitHub-native auto-merge is enabled; GitHub merges the PR server-side once checks pass, even with lazybox closed.",
        when: "Shows when auto-merge is enabled on the PR on GitHub.",
    },
    MarkerDoc {
        label: "🔧",
        meaning: "FIX — auto-fix is armed; lazybox spawns an agent to fix failing CI and/or merge conflicts on this PR.",
        when: "Shows once you arm auto-fix for this workspace (`g p`).",
    },
    MarkerDoc {
        label: "⎇ local",
        meaning: "This workspace runs its sessions in your real checkout, not an isolated worktree.",
        when: "Shows on a linked (no-worktree) workspace.",
    },
    MarkerDoc {
        label: "⤓",
        meaning: "Track-main is on: the worktree auto-syncs with the default branch. In its warn color it's stuck behind and can't auto-sync (dirty or diverged).",
        when: "Shows when the workspace has track-main armed.",
    },
    MarkerDoc {
        label: "✎",
        meaning: "The workspace carries a local note.",
        when: "Shows once you save a note on the workspace.",
    },
    MarkerDoc {
        label: "]N",
        meaning: "Count of distinct snippets/prompts you've recently sent to this workspace's agent.",
        when: "Shows once at least one snippet was sent to the workspace.",
    },
    MarkerDoc {
        label: "★ Focused",
        meaning: "Starred workspaces are lifted into a synthetic \"★ Focused\" group pinned at the top of the sidebar.",
        when: "Appears when the sidebar has one or more starred workspaces.",
    },
    MarkerDoc {
        label: "Role letter (A / R / @ / ·)",
        meaning: "Your role on the PR/issue: `A` author, `R` reviewer, `@` assignee, dim `·` mentioned.",
        when: "Always present, as the row's leading badge.",
    },
    MarkerDoc {
        label: "Runner letter (C / X / U / S), optionally jump-numbered",
        meaning: "The agent or shell running in the workspace: `C` claude, `X` codex, `U` cursor, `S` shell. On the first nine agent workspaces it's prefixed by a 1–9 jump number (` 2 C `) that `]]<digit>` jumps to.",
        when: "Shows when the workspace has a live session.",
    },
    MarkerDoc {
        label: "Type glyph (⇄ / ○ / ◆)",
        meaning: "The workspace's source: `⇄` pull request, `○` GitHub issue, `◆` Linear ticket.",
        when: "Sits before the number on each workspace row.",
    },
    MarkerDoc {
        label: "Model badge (◆O / ◆S / ◆H)",
        meaning: "The agent's model/tier, abbreviated to one glyph: `◆O` Opus, `◆S` Sonnet, `◆H` Haiku for Claude; other agents show their model's first letter (or a declared short) with a dim effort suffix (`◆g ·xhi`). The full model name shows on the terminal tab.",
        when: "Rides after a single agent's runner letter when it has a known model.",
    },
];

/// Every status pill that renders (all [`StatusTag`] variants but
/// `None`), in severity order.
pub fn status_pill_docs() -> Vec<MarkerDoc> {
    StatusTag::ALL
        .into_iter()
        .filter_map(status_pill_doc)
        .collect()
}

/// The documented agent-state glyph for every [`AgentState`].
pub fn agent_state_docs() -> Vec<MarkerDoc> {
    AgentState::ALL.iter().map(agent_state_doc).collect()
}

/// The passive row badges.
pub fn row_badge_docs() -> &'static [MarkerDoc] {
    ROW_BADGES
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every documented status pill has a non-empty meaning and
    /// when-clause. The exact *set* (which `StatusTag` variants render a
    /// pill) is pinned to the renderer by
    /// `documented_status_pills_match_the_renderer` in `tui`; here we
    /// only guard that each entry is filled in and the #883 quick win is
    /// present. `Behind` and `None` render no pill, so this is a subset
    /// of `StatusTag::ALL`.
    #[test]
    fn every_status_pill_is_documented() {
        let docs = status_pill_docs();
        assert!(!docs.is_empty());
        assert!(
            docs.len() < StatusTag::ALL.len(),
            "not every tag renders a pill"
        );
        for doc in &docs {
            assert!(!doc.label.is_empty());
            assert!(!doc.meaning.is_empty(), "{}: empty meaning", doc.label);
            assert!(!doc.when.is_empty(), "{}: empty when", doc.label);
        }
        // The quick win from #883: changes-requested must be explainable.
        // Its label is the on-screen `✗` glyph (#1046); the meaning names
        // the state.
        assert!(
            docs.iter()
                .any(|d| d.label == "✗" && d.meaning.starts_with("Changes requested"))
        );
        // Regression for the review finding: Behind is not a rendered row
        // pill, so it must not be documented as one — no glyph gets a
        // "behind" meaning here.
        assert!(
            !docs.iter().any(|d| d.meaning.contains("behind its base")),
            "Behind renders no status pill; documenting it reintroduces the drift #883 fixes"
        );
    }

    /// #1068: the compact model/tier badge (`◆O`) is documented in the
    /// row-badge legend, so a user can look up what the glyph means.
    #[test]
    fn model_tier_badge_is_documented() {
        let doc = row_badge_docs()
            .iter()
            .find(|d| d.label.contains("◆O"))
            .expect("the model badge must be documented");
        assert!(doc.meaning.contains("Opus"), "names the full tier word");
    }

    #[test]
    fn fleet_claim_badge_is_documented() {
        let doc = row_badge_docs()
            .iter()
            .find(|doc| doc.label == "WORK")
            .expect("the cross-machine working claim must be explainable");
        assert!(doc.meaning.contains("another lazybox machine"));
        assert!(doc.when.contains("confirmation"));
    }

    /// The pre-terminal "spawning" arc (#1069) is documented like the
    /// enum-backed states, with a filled-in meaning + when-clause.
    #[test]
    fn spawning_glyph_is_documented() {
        let doc = spawning_doc();
        assert!(doc.label.contains("Spawning"));
        assert!(doc.meaning.contains("provisioning"));
        assert!(!doc.when.is_empty());
    }

    /// Every agent state has a non-empty meaning, one doc per variant.
    #[test]
    fn every_agent_state_is_documented() {
        let docs = agent_state_docs();
        assert_eq!(docs.len(), AgentState::ALL.len());
        for doc in &docs {
            assert!(!doc.meaning.is_empty(), "{}: empty meaning", doc.label);
            assert!(!doc.when.is_empty(), "{}: empty when", doc.label);
        }
    }
}
