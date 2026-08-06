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

/// Documentation for a single status pill, or `None` for the empty
/// [`StatusTag::None`] (no pill rendered). Exhaustive over `StatusTag`,
/// so a new variant can't ship without a meaning (#883).
fn status_pill_doc(tag: StatusTag) -> Option<MarkerDoc> {
    let doc = |label, meaning, when| {
        Some(MarkerDoc {
            label,
            meaning,
            when,
        })
    };
    match tag {
        StatusTag::Merged => doc(
            "MERGED",
            "The pull request has been merged.",
            "Shows on a PR whose branch was merged into its base.",
        ),
        StatusTag::Closed => doc(
            "CLOSED",
            "The pull request or issue was closed without merging.",
            "Shows on a closed PR or issue.",
        ),
        StatusTag::Conflict => doc(
            "CONFLICT",
            "The PR has merge conflicts with its base branch and can't merge until they're resolved.",
            "Shows when the PR conflicts with its base branch.",
        ),
        StatusTag::CiFailed => doc(
            "CI FAIL",
            "Continuous-integration checks are failing.",
            "Shows when one or more required checks failed.",
        ),
        StatusTag::CiMixed => doc(
            "CI MIX",
            "CI is partly green and partly failing.",
            "Shows when some checks passed while others failed.",
        ),
        StatusTag::ChangesRequested => doc(
            "CHANGES",
            "A reviewer requested changes — the PR needs edits before it can be approved.",
            "Shows when a review left the \"changes requested\" verdict.",
        ),
        StatusTag::Queued => doc(
            "QUEUED",
            "The PR is sitting in GitHub's merge queue.",
            "Shows once the PR has entered GitHub's merge queue.",
        ),
        StatusTag::Draft => doc(
            "DRAFT",
            "The PR is a draft and not yet ready for review.",
            "Shows while the PR is marked draft, even with green CI.",
        ),
        StatusTag::Ready => doc(
            "READY",
            "Approved with green (or no) CI — ready to merge now.",
            "Shows when the PR is approved and CI is green or unset.",
        ),
        StatusTag::Approved => doc(
            "APPROVED",
            "A reviewer approved the PR, but CI isn't green yet.",
            "Shows when the PR is approved but CI is still pending or failing.",
        ),
        StatusTag::ReviewPending => doc(
            "REVIEW",
            "The PR is waiting on review — a review was requested or is pending.",
            "Shows while review is requested or pending with no verdict yet.",
        ),
        StatusTag::CiRunning => doc(
            "CI RUN",
            "CI is still running.",
            "Shows while checks are queued or in progress.",
        ),
        StatusTag::CiOk => doc(
            "CI OK",
            "CI is green and nothing more pressing applies.",
            "Shows when all checks passed and nothing more pressing applies.",
        ),
        StatusTag::Behind => doc(
            "BEHIND",
            "The branch is behind its base — informational; update it to catch up.",
            "Shows when the PR's branch is behind its base branch.",
        ),
        StatusTag::None => None,
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
            "⟳ Working (animated spinner)",
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

/// The passive row badges rendered from `Workspace` fields (not an
/// enum), left to right. Hand-curated: keep this in step with
/// `lazybox_tui::components::workspace_row`'s badge cells.
const ROW_BADGES: &[MarkerDoc] = &[
    MarkerDoc {
        label: "ARM",
        meaning: "This PR will auto-merge once CI goes green — lazybox's client-side merge, which only fires while lazybox is running.",
        when: "Shows once you arm merge-on-green (`g g`).",
    },
    MarkerDoc {
        label: "AUTO",
        meaning: "GitHub-native auto-merge is enabled — GitHub merges the PR server-side once checks pass, even with lazybox closed.",
        when: "Shows when auto-merge is enabled on the PR on GitHub.",
    },
    MarkerDoc {
        label: "FIX",
        meaning: "Auto-fix is armed — lazybox spawns an agent to fix failing CI and/or merge conflicts on this PR.",
        when: "Shows once you arm auto-fix for this workspace (`g p`).",
    },
    MarkerDoc {
        label: "⎇ local",
        meaning: "This workspace runs its sessions in your real checkout, not an isolated worktree.",
        when: "Shows on a linked (no-worktree) workspace.",
    },
    MarkerDoc {
        label: "⤓main / behind",
        meaning: "Track-main is on: the worktree auto-syncs with the default branch. `behind` warns it's stuck behind and can't auto-sync (dirty or diverged).",
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
        label: "Runner letter (C / X / U / S)",
        meaning: "The agent or shell running in the workspace: `C` claude, `X` codex, `U` cursor, `S` shell.",
        when: "Shows when the workspace has a live session.",
    },
    MarkerDoc {
        label: "Type glyph (⇄ / ○ / ◆)",
        meaning: "The workspace's source: `⇄` pull request, `○` GitHub issue, `◆` Linear ticket.",
        when: "Sits before the number on each workspace row.",
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

    /// Every rendered status pill has a non-empty meaning and when-clause,
    /// and the count matches the enum (minus the no-pill `None`).
    #[test]
    fn every_status_pill_is_documented() {
        let docs = status_pill_docs();
        assert_eq!(docs.len(), StatusTag::ALL.len() - 1);
        for doc in &docs {
            assert!(!doc.label.is_empty());
            assert!(!doc.meaning.is_empty(), "{}: empty meaning", doc.label);
            assert!(!doc.when.is_empty(), "{}: empty when", doc.label);
        }
        // The quick win from #883: CHANGES must now be explainable.
        assert!(docs.iter().any(|d| d.label == "CHANGES"));
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
