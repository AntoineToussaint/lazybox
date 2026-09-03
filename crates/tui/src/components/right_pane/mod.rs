//! RightPane — the right column of the TUI. Shows the session
//! currently selected in the Sidebar: header (title, branch, CI,
//! reviewers) on top, comment list below.
//!
//! ## Why one component instead of Header + Comments
//!
//! Header and Comments share one state source (the current session)
//! and one focus (there's no "tab between header and comments"). Two
//! components with a shared source would open a desync surface
//! between them. The render is split into sub-functions for
//! readability; the *component* stays one unit.
//!
//! Dashboard tiles (task #70) are a separate case — they each have
//! an independent data source and can make sense as individual
//! components in a TileStack container.
//!
//! ## Data flow
//!
//! AppRoot reads `sidebar.selected_workspace()` after every key event
//! and calls `right_pane.set_workspace(...)`. The RightPane doesn't
//! track every workspace the daemon knows about — only the currently
//! selected one. This keeps the component simple and its event
//! handler a no-op.

mod card;
mod markdown;

#[cfg(test)]
mod tests;

pub(crate) use card::{CardState, render_card_body, render_card_header};
pub(crate) use markdown::teaser_text;

use crate::{PaneId, PaneOutcome};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use lazybox_core::Workspace;
use lazybox_ipc::{Command, Event};
use ratatui::Frame;
use ratatui::prelude::*;
use ratatui::widgets::*;
use std::rc::Rc;

/// Width the layout-sizing render (`task_body_content_rows`) uses — a
/// conservative default column count, chosen before the real pane width
/// is known (see `task_body_content_rows`).
const TASK_BODY_SIZE_WIDTH: u16 = 80;

/// Content-addressed cache of the rendered description body (#1031).
/// `comment_render::render_body` is O(body size) with heavy per-word
/// allocation and was previously run twice per frame — once to size the
/// layout, once to draw — with no memoization, so a large PR/issue body
/// with the description expanded was a per-frame render stall. The cache
/// is keyed by the body text itself, so it can never render stale
/// content; it holds the uncapped render per column width the layout
/// asks for (the width-80 sizing pass plus the live pane width).
#[derive(Default)]
struct TaskBodyCache {
    body: String,
    /// `body_wants_rich_modal(&body)`, computed once per body change so
    /// the per-frame header glyph doesn't rescan the whole body.
    rich_modal: bool,
    by_width: Vec<(u16, Rc<Vec<Line<'static>>>)>,
}

/// Cheap scan for markdown the inline teaser degrades badly enough that
/// the full reader is worth offering even for a short body: fenced code
/// blocks, GFM tables, and images. Pure over the body text, so the
/// result is memoized per body in [`TaskBodyCache`] (#1031).
fn body_wants_rich_modal(body: &str) -> bool {
    if body.contains("![") {
        return true; // markdown image
    }
    for line in body.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            return true; // fenced code block
        }
    }
    // A GFM table delimiter row — `| --- | :--: |` — is unambiguous and
    // is what the teaser flattens into an unreadable single line. Skip
    // lines indented into an (unfenced) code block: 4+ leading spaces or
    // a leading tab makes it indented code, where a `| --- |`-shaped
    // line is literal text, not a table delimiter.
    body.lines().any(|line| {
        let indent = line.len() - line.trim_start_matches(' ').len();
        if indent >= 4 || line.starts_with('\t') {
            return false;
        }
        let t = line.trim();
        t.contains('|') && t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
    })
}

pub struct RightPane {
    id: PaneId,
    workspace: Option<Workspace>,
    /// Stacked-PR position of the focused workspace's PR (issue #969),
    /// set alongside [`RightPane::set_workspace`] from the sidebar's
    /// cross-workspace index. Drives the `Stacked on #A · k/N` header
    /// line. `None` when the focused PR is standalone.
    stack: Option<lazybox_core::StackPosition>,
    /// Scroll offset into the comment list (top-of-viewport index).
    comment_scroll: usize,
    /// Headless navigation state for the activity feed: cursor +
    /// expanded set + selected set. Pulled into its own struct so
    /// the navigation logic (j/k, toggle expand, multi-select) is
    /// unit-testable without rendering a `Frame`. See
    /// [`crate::components::activity_feed::ActivityFeed`].
    feed: crate::components::activity_feed::ActivityFeed,
    /// Per-source authenticated logins. Populated from
    /// `IpcEvent::ViewerIdentities`. Activity bylines authored by
    /// the local user render as `@me` instead of their bare login.
    viewer_logins: std::collections::HashMap<String, String>,
    /// Visible *lines* in the activity viewport (not cards — render
    /// builds a virtual line buffer so an expanded card may span
    /// many lines). Updated during `render`; consumed by
    /// `clamp_scroll_to_cursor` (as a conservative lower bound on
    /// cards) and `scroll_activity` (as the line denominator). 1 is
    /// a conservative default before the first render so the cursor
    /// never gets stranded.
    last_visible_cards: usize,
    /// Total *lines* the virtual buffer rendered last frame —
    /// i.e. `cards.len()` after every card (collapsed + expanded)
    /// was laid out. `last_visible_cards` is the slice we drew;
    /// this is the whole strip. `scroll_activity` needs both to
    /// compute the line-offset scroll cap; without it, an expanded
    /// card meant `activity.len() − visible_lines` underflowed to 0
    /// and the wheel did nothing.
    last_total_lines: usize,
    /// Whether the activity section is collapsed to its header row.
    /// Defaults to expanded; auto-collapses when the workspace has no
    /// activity (the empty pane is just visual noise — keeping the
    /// header alone tells the user where it would land).
    activity_collapsed: bool,
    /// User-driven override of the collapse state. Once the user
    /// explicitly toggles, we stop auto-collapsing on empty: their
    /// intent wins.
    activity_collapse_user_set: bool,
    /// Auto-mark-read timer. Armed when the cursor lands on an
    /// unread row while the pane has focus; on the next `tick`
    /// past `auto_mark_delay` the activity flips to read and we
    /// remember it in `last_marked_read` so `z` can undo it.
    /// Backed by the generic `TimerLatch` so the
    /// "arm-on-event, fire-on-elapsed" contract is one place.
    mark_timer: crate::confirm_latch::TimerLatch,
    /// Fingerprint of the row the auto-mark timer was ARMED on. New
    /// activities insert at the top of the feed and shift every
    /// index, so firing on the raw `feed.cursor` index can mark a
    /// comment the user never dwelt on. At fire time the cursor row
    /// is checked against this fingerprint; on mismatch the
    /// fingerprint is re-resolved to its shifted index (or the fire
    /// is skipped if the row vanished). Cleared whenever the timer
    /// disarms.
    mark_timer_target: Option<ActivityFingerprint>,
    /// Fingerprint of the most recently auto-marked activity, plus
    /// its last-known index. On `z` we resolve the fingerprint back
    /// to the CURRENT index — a poll that introduced a new comment
    /// at the top shifts every older row down by one, and using the
    /// raw cached index would un-read the wrong comment. See
    /// `AutoMarkRecord::resolve` for the lookup.
    last_marked_read: Option<AutoMarkRecord>,
    /// Agent the `f` (fix) shortcut spawns. Configurable via YAML
    /// (`setup.default_agent`); defaults to `"claude"`.
    default_agent: String,
    /// Commit/PR conventions injected into the `w` work brief. Wired
    /// from YAML (`conventions:`) at startup; defaults to Conventional
    /// Commits. Mirrors the sidebar so `w` from either pane is
    /// convention-aware.
    conventions: lazybox_core::Conventions,
    /// Visibility of the task-body / description teaser.
    /// Default `Collapsed`: the activity feed is what users come to
    /// the inbox for; the body is reference material they can pop
    /// open with `d`. `d` toggles `Collapsed ⇄ Preview`, where Preview
    /// caps the inline height at `task_body_max_rows`. A body longer
    /// than the cap (or rich markdown the teaser degrades) is read in
    /// the scrollable reader modal (#448), not by growing the pane —
    /// see [`TaskBodyView`] and [`Self::toggle_task_body`].
    task_body_view: TaskBodyView,
    /// Resolved cap on the task-body expanded height, sourced from
    /// `~/.lazybox/config.yaml::ui.task_body_max_rows` (default 8).
    task_body_max_rows: u16,
    /// Resolved auto-mark-read timer, sourced from
    /// `ui.auto_mark_delay` (default 1 s).
    auto_mark_delay: std::time::Duration,
    /// Hit-test rectangles cached during render so a click in the
    /// pane can be mapped back to (activity_index | section_header)
    /// without a re-layout. Refreshed on every render.
    click_hits: ClickHits,
    /// Notice text queued by `handle_mouse_click` when a click
    /// toggles an activity row's selection. The orchestrator drains
    /// this after dispatching the click and surfaces it as a
    /// footer Hint — pure `✓` visual was too subtle on its own.
    pending_selection_notice: Option<String>,
    /// URL queued by `handle_mouse_click` when a click lands on the
    /// header title or the originating-issue line. The orchestrator
    /// drains it after dispatching the click and hands it to the
    /// browser launcher (#567).
    pending_open_url: Option<String>,
    /// Set when a click lands on the header `Reviewers:` line. The
    /// orchestrator drains it and kicks off the reviewer picker — the
    /// same `g r` flow — since the pane can't reach `Model` itself
    /// (#1092).
    pending_request_reviewers: bool,
    /// Set when the user asks to read the full description (a second
    /// `d`, or a click on the `+N more lines` trailer). The orchestrator
    /// drains it after dispatching the key/click and mounts the reader
    /// modal (#448) — the pane can't reach `Model::modal_stack` itself.
    pending_open_description: bool,
    /// Whether the last Preview render truncated the body (there's more
    /// than the pane shows). Refreshed every `render_task_body`; the `d`
    /// / header-click path reads it to decide "read full vs. collapse".
    /// Reaching that decision always follows a render (you can't be in
    /// Preview without one), so the flag is current when it's consulted.
    body_overflows: bool,
    /// Memoized activity virtual-line buffer. Rebuilt only when an input
    /// that actually affects the rendered cards changes; scrolling
    /// (which only moves `comment_scroll`) reuses it untouched. See
    /// [`ActivityBuffer`] for why this is what keeps scroll responsive.
    activity_buffer: Option<ActivityBuffer>,
    /// Content-addressed memo for the description-body render (#1031).
    /// See [`TaskBodyCache`].
    task_body_cache: TaskBodyCache,
    /// Revision counter for the workspace's activity *content*. Bumped
    /// by [`RightPane::refresh_activity_rev`] whenever the stored
    /// workspace's activity set actually mutates (new / edited /
    /// removed entries, or a different workspace entirely). The buffer
    /// cache key hashes this single u64 instead of re-hashing every
    /// activity body's bytes on every frame — with a long feed of
    /// large comment bodies, the per-frame SipHash over all bodies was
    /// itself a measurable render cost.
    activity_rev: u64,
    /// Cheap fingerprint backing `activity_rev`: workspace key +
    /// per-activity (author, body *length*, timestamp, kind, node id).
    /// Deliberately length-not-bytes for bodies — `set_workspace`
    /// receives a fresh `Workspace` clone on every pane sync, so this
    /// runs often and must stay O(n) over tiny fields.
    activity_identity: u64,
    /// Count of activity-buffer rebuilds, for the encapsulation
    /// regression test that pins "scrolling never rebuilds the feed."
    #[cfg(test)]
    activity_rebuilds: u32,
    /// Repo / Space **overview** shown when the sidebar cursor rests on
    /// a group header instead of a workspace (issue #1442). Set from the
    /// model alongside `set_workspace(None)`; when present and
    /// `workspace` is `None`, `render` paints the overview across the
    /// whole pane instead of the `(no session selected)` placeholder.
    overview: Option<crate::components::repo_overview::RepoOverview>,
    /// Absolute screen rows of the overview roster, paired with the
    /// workspace each row addresses — refreshed on every overview render
    /// so a click can move the sidebar cursor onto that workspace.
    overview_hits: Vec<(u16, lazybox_core::SessionKey)>,
    /// Queued by an overview roster click; drained by the orchestrator,
    /// which selects the workspace in the sidebar (the pane can't reach
    /// `Model` itself, mirroring the other `pending_*` handoffs).
    pending_select_workspace: Option<lazybox_core::SessionKey>,
}

/// Click-target geometry captured during render. Three regions are
/// tracked: the Description section's toggle row, the Activity
/// section's toggle row, and each visible activity card. The
/// orchestrator's mouse handler reads these and dispatches; no
/// re-layout / re-measure happens in the click path.
#[derive(Debug, Default)]
struct ClickHits {
    /// Row of the header's title (state-pill) line, paired with the
    /// primary task's URL. Clicking it opens the PR / issue in the
    /// browser — the title is a live link, not just a label (#567).
    header_title: Option<(u16, String)>,
    /// Row of the header's `Issue: #N` originating-issue line, paired
    /// with that issue's URL. `None` unless the workspace is a PR with
    /// a tracked originating issue (#567).
    header_issue: Option<(u16, String)>,
    /// Row of the header's `Reviewers:` line. Clicking it opens the
    /// reviewer picker — the same `g r` flow (#1092). `None` unless the
    /// workspace is a PR (only PRs have reviewers).
    header_reviewers: Option<u16>,
    /// Row containing the `▶ Description` / `▼ Description` header,
    /// or `None` when the section isn't being rendered (no body).
    body_header_row: Option<u16>,
    /// Row containing the `+N more lines …` trailer that closes a
    /// truncated Preview. Clicking it jumps straight to `Full`.
    /// `None` unless the body is capped and there's more to show.
    body_more_row: Option<u16>,
    /// Row containing the `▸ Activity` / `▾ Activity` header.
    activity_header_row: Option<u16>,
    /// One `(activity_index, row_range)` entry per visible card.
    /// `row_range` is inclusive on both ends — header through last
    /// body line of the card. Empty when the section is collapsed
    /// or there's no activity.
    activity_cards: Vec<(usize, std::ops::RangeInclusive<u16>)>,
}

/// Memoized activity virtual-line buffer.
///
/// Building this buffer is the expensive part of painting the feed:
/// one `Line` (≈10 allocated `Span`s) per card header, plus a full
/// markdown→ratatui render of every *expanded* card's body. Rebuilding
/// it on every frame is what made wheel-scrolling lock up the UI — a
/// scroll only changes `comment_scroll`, a windowing offset, yet the
/// old render path re-laid-out the entire feed on every wheel tick.
///
/// The buffer is a pure function of the inputs folded into `key`
/// (`RightPane::activity_buffer_key`). `comment_scroll` is *deliberately
/// absent* from that key: scrolling cannot change the key, so it can
/// never miss the cache, so it can never trigger a rebuild. The window
/// the scroll selects (`skip`/`take` in `render_activity`) runs against
/// this cached buffer in O(viewport). That decoupling is the
/// encapsulation guarantee — a future change can only make scroll
/// expensive again by adding `comment_scroll` to the key, which would
/// be obviously wrong on sight.
struct ActivityBuffer {
    /// Fingerprint of every input that determines the buffer's content.
    key: u64,
    /// One entry per virtual line: collapsed cards are 1 line, expanded
    /// cards are header + wrapped body lines.
    cards: Vec<Line<'static>>,
    /// `(activity_index, start_line, end_line)` — inclusive line span of
    /// each card within `cards`, for click hit-testing after windowing.
    card_spans: Vec<(usize, u16, u16)>,
}

// `MARK_READ_DELAY` retired — value lives on `self.auto_mark_delay`
// now, sourced from `~/.lazybox/config.yaml::ui.auto_mark_delay`.

/// Pure predicate: should the auto-mark timer be armed for the
/// current state? The old `rearm_mark_timer` mixed this decision
/// with the `&mut self` mutation; pulling it out lets the cell
/// tests pin every truth-table cell (focused × workspace ×
/// cursor-unread) directly without going through the rest of the
/// pane's state.
/// Two-state visibility for the PR/issue description *teaser* (#448).
/// `d` toggles Collapsed ⇄ Preview; a genuinely long body is read in
/// the scrollable [`markdown_modal`](crate::realm::components::markdown_modal)
/// (opened from the `+N more lines` trailer or a second `d`), not by
/// growing the pane. Stored on `RightPane::task_body_view`; the renderer
/// + constraint math fan out from this single field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskBodyView {
    /// Header-only row, body hidden. Default for fresh workspaces.
    #[default]
    Collapsed,
    /// Body visible but capped at `task_body_max_rows`. The trailer
    /// shows `+N more lines` when content exceeds the cap; opening the
    /// reader modal is the way to see the rest.
    Preview,
}

impl TaskBodyView {
    /// Toggle between collapsed and the teaser preview.
    pub fn cycle(self) -> Self {
        match self {
            Self::Collapsed => Self::Preview,
            Self::Preview => Self::Collapsed,
        }
    }

    pub fn is_visible(self) -> bool {
        !matches!(self, Self::Collapsed)
    }
}

/// The trailer row that closes a truncated description preview. `dropped`
/// is the number of hidden lines; the affordance spells out that the
/// full body opens in the reader modal, so the previously-dead-end
/// `+N more lines` reads as a live control.
fn more_lines_trailer(dropped: usize, theme: &crate::theme::Theme) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("+{dropped} more lines"),
            Style::default()
                .fg(theme.text_dim)
                .add_modifier(Modifier::ITALIC),
        ),
        Span::styled(
            "  —  click or press d to read full",
            Style::default().fg(theme.accent),
        ),
    ])
}

/// A short clickable-link label for a task in the reader modal —
/// `#123` when the number is known, else the full task key.
fn task_ref_label(task: &lazybox_core::Task) -> String {
    match crate::components::task_label::pr_number(task) {
        Some(n) => format!("#{n}"),
        None => task.id.key.clone(),
    }
}

/// Stable enough identifier for one activity row, used by the `z`
/// undo flow to find the row's *current* index after a poll that
/// reshuffled the list (a new top-of-feed comment shifts every
/// older row down by one), and shipped inside
/// `Command::MarkActivityRead` so the daemon resolves the row
/// against ITS current list instead of trusting a possibly-shifted
/// raw index. Canonical definition lives in core (next to the
/// `merge_activity` identity logic) so client and daemon can't
/// drift.
use lazybox_core::ActivityFingerprint;

#[derive(Debug, Clone)]
struct AutoMarkRecord {
    /// Hint at the position we last saw this activity at — first
    /// thing `resolve` checks before scanning the list. Stale after
    /// a reshuffle but the scan covers the falls-out-of-place case.
    last_index: usize,
    fingerprint: ActivityFingerprint,
}

impl AutoMarkRecord {
    /// Find the activity matching this record in `activity` and
    /// return its current index. None when the row is gone (e.g.
    /// removed upstream between the mark and the undo).
    fn resolve(&self, activity: &[lazybox_core::Activity]) -> Option<usize> {
        self.fingerprint.resolve(activity, self.last_index)
    }
}

pub fn should_arm_mark_timer(
    focused: bool,
    workspace: Option<&lazybox_core::Workspace>,
    cursor: usize,
) -> bool {
    // `focused` is now ignored — see `tick` for rationale. Kept in
    // the signature so existing call sites compile without churn
    // and so a future opt-in (user wants focus-gating back) is
    // one boolean away. Net effect: as long as the cursor sits
    // on an unread row, the timer arms regardless of which pane
    // owns keyboard focus.
    let _ = focused;
    workspace
        .map(|w| w.is_activity_unread(cursor))
        .unwrap_or(false)
}

impl RightPane {
    pub fn new(id: PaneId) -> Self {
        Self {
            id,
            workspace: None,
            stack: None,
            comment_scroll: 0,
            feed: crate::components::activity_feed::ActivityFeed::new(),
            viewer_logins: std::collections::HashMap::new(),
            last_visible_cards: 1,
            last_total_lines: 0,
            // Empty workspace → collapsed; cleared on first non-empty
            // workspace landing in `set_workspace`.
            activity_collapsed: true,
            activity_collapse_user_set: false,
            mark_timer: crate::confirm_latch::TimerLatch::new(),
            mark_timer_target: None,
            last_marked_read: None,
            default_agent: "claude".to_string(),
            conventions: lazybox_core::Conventions::default(),
            task_body_view: TaskBodyView::Collapsed,
            task_body_max_rows: lazybox_config::UiDefaults::default().task_body_max_rows,
            auto_mark_delay: lazybox_config::UiDefaults::default().auto_mark_delay,
            click_hits: ClickHits::default(),
            pending_selection_notice: None,
            pending_open_url: None,
            pending_request_reviewers: false,
            pending_open_description: false,
            body_overflows: false,
            activity_buffer: None,
            task_body_cache: TaskBodyCache::default(),
            activity_rev: 0,
            activity_identity: 0,
            #[cfg(test)]
            activity_rebuilds: 0,
            overview: None,
            overview_hits: Vec::new(),
            pending_select_workspace: None,
        }
    }

    /// Apply resolved `UiDefaults` once at startup. Subsequent
    /// hot-reload of the YAML would call this again; idempotent.
    pub fn apply_ui_defaults(&mut self, ui: &lazybox_config::UiDefaults) {
        self.task_body_max_rows = ui.task_body_max_rows;
        self.auto_mark_delay = ui.auto_mark_delay;
    }

    /// Override the agent the `f` (fix) shortcut spawns. AppRoot
    /// wires this from `setup.default_agent` in YAML.
    pub fn with_default_agent(mut self, agent: impl Into<String>) -> Self {
        self.default_agent = agent.into();
        self
    }

    /// Replace the default agent at runtime (used by `apply_config`).
    pub fn set_default_agent(&mut self, agent: impl Into<String>) {
        self.default_agent = agent.into();
    }

    /// Wire the YAML-configured `conventions:` block at startup so a
    /// `w` from the activity pane builds the same convention-aware brief
    /// as the sidebar. Mirrors [`Self::set_default_agent`].
    pub fn set_conventions(&mut self, conventions: lazybox_core::Conventions) {
        self.conventions = conventions;
    }

    /// Install the daemon-announced viewer identities (source →
    /// login). Activity bylines whose author matches one of these
    /// logins render as `@me` instead of the bare username. Called
    /// from the orchestrator on `IpcEvent::ViewerIdentities`.
    pub fn set_viewer_logins(&mut self, logins: std::collections::HashMap<String, String>) {
        self.viewer_logins = logins;
    }

    /// Returns the auto-mark progress as `(elapsed_ratio, label)` if
    /// armed, else None. The status footer reads this to render a
    /// progress bar. `elapsed_ratio` is clamped to [0.0, 1.0].
    pub fn auto_mark_progress(&self) -> Option<f32> {
        self.mark_timer.progress(self.auto_mark_delay)
    }

    /// Whether `z` would do something useful right now. Drives the
    /// hint footer's "z undo" entry — we only show the hint when
    /// there's actually something to undo.
    pub fn can_undo_mark_read(&self) -> bool {
        self.last_marked_read.is_some()
    }

    /// Arm the auto-mark timer iff the cursor is currently on an
    /// unread activity. Called whenever cursor or workspace state
    /// Keep the focused row on-screen as the cursor walks the
    /// activity list. Without this, `comment_scroll` is frozen at 0
    /// and `j` past the visible window lets the cursor disappear
    /// off the bottom of the pane. `last_visible_cards` is set
    /// during the previous render — conservative default of 1
    /// avoids a stranded cursor before the first frame.
    fn clamp_scroll_to_cursor(&mut self) {
        if self.feed.cursor < self.comment_scroll {
            self.comment_scroll = self.feed.cursor;
        } else if self.feed.cursor >= self.comment_scroll + self.last_visible_cards {
            // `(cursor + 1) - last_visible_cards` underflows when
            // `last_visible_cards > cursor + 1` (tiny viewport at
            // pre-first-render state). Saturating subtraction
            // clamps to 0, which means "top of buffer" — the
            // safest fallback that doesn't strand the cursor.
            self.comment_scroll = (self.feed.cursor + 1).saturating_sub(self.last_visible_cards);
        }
    }

    /// changes in a way that might affect the answer (j/k/g/G,
    /// set_workspace, focus enter). Idempotent on re-arm — relies on
    /// `TimerLatch::arm` preserving an existing `armed_at`, so a
    /// `WorkspaceUpserted` arriving every poll interval doesn't
    /// reset the countdown out from under a user reading a row.
    fn rearm_mark_timer(&mut self, focused: bool) {
        if should_arm_mark_timer(focused, self.workspace.as_ref(), self.feed.cursor) {
            // Stash the row's fingerprint only on a FRESH arm — an
            // idempotent re-arm keeps counting toward the row the
            // timer started on, so the fingerprint must keep naming
            // that original row even if the feed reshuffled since.
            if !self.mark_timer.is_armed() {
                self.mark_timer_target = self.cursor_fingerprint();
            }
            self.mark_timer.arm();
        } else {
            self.disarm_mark_timer();
        }
    }

    /// Cursor moved to a new row — disarm + rearm fresh so the
    /// previous row's elapsed time doesn't carry over. Idempotent
    /// `arm` is correct for the "same row, refresh on tick" case
    /// but wrong here: if the user was 0.9s into row A and j's to
    /// row B, plain `arm` would leave the timer at 0.9s and fire
    /// almost immediately on B (which the user hasn't dwelt on).
    fn rearm_mark_timer_for_new_row(&mut self, focused: bool) {
        if should_arm_mark_timer(focused, self.workspace.as_ref(), self.feed.cursor) {
            self.mark_timer.rearm_fresh();
            self.mark_timer_target = self.cursor_fingerprint();
        } else {
            self.disarm_mark_timer();
        }
    }

    /// Disarm the auto-mark timer AND forget which row it was armed
    /// on — the two always travel together.
    fn disarm_mark_timer(&mut self) {
        self.mark_timer.disarm();
        self.mark_timer_target = None;
    }

    /// Fingerprint of the activity row currently under the cursor,
    /// if any.
    fn cursor_fingerprint(&self) -> Option<ActivityFingerprint> {
        self.workspace
            .as_ref()?
            .activity
            .get(self.feed.cursor)
            .map(ActivityFingerprint::of)
    }

    /// Flip the cursor's activity to read and remember the index for
    /// undo. Returns `(session_key, index, fingerprint)` so the
    /// caller can persist via `Command::MarkActivityRead` — the
    /// fingerprint rides along so the daemon re-resolves the row
    /// against its own (possibly newer) list.
    fn fire_auto_mark(&mut self) -> Option<(lazybox_core::SessionKey, usize, ActivityFingerprint)> {
        let cursor = self.feed.cursor;
        // Resolve the row the timer was ARMED on. New activities
        // insert at the TOP of the feed and shift every index, so
        // the raw cursor index may now point at a comment the user
        // never dwelt on. The fingerprint stashed at arm time is the
        // ground truth: if the cursor row no longer matches it, find
        // the armed row's new index and mark THAT — or skip entirely
        // when the row is gone.
        let i = {
            let workspace = self.workspace.as_ref()?;
            match &self.mark_timer_target {
                Some(fp) => {
                    if workspace
                        .activity
                        .get(cursor)
                        .is_some_and(|a| fp.matches(a))
                    {
                        cursor
                    } else {
                        match workspace.activity.iter().position(|a| fp.matches(a)) {
                            Some(shifted) => shifted,
                            None => {
                                tracing::debug!(
                                    cursor,
                                    "auto-mark fire: armed row vanished — skipping",
                                );
                                self.disarm_mark_timer();
                                return None;
                            }
                        }
                    }
                }
                // Defensive: timer armed without a stashed target
                // (shouldn't happen) — fall back to the cursor row.
                None => cursor,
            }
        };
        let workspace = self.workspace.as_mut()?;
        let total = workspace.activity.len();
        let was_unread = workspace.is_activity_unread(i);
        if !was_unread {
            tracing::debug!(
                index = i,
                total,
                "auto-mark fire: armed row not unread — skipping",
            );
            return None;
        }
        let fingerprint = workspace.activity.get(i).map(ActivityFingerprint::of)?;
        workspace.mark_activity_read(i);
        let unread_after = workspace.unread_count();
        tracing::info!(
            workspace = %workspace.key,
            index = i,
            unread_after,
            "auto-mark fire: flipped row to read",
        );
        let session_key = lazybox_core::SessionKey::from(&workspace.key);
        self.last_marked_read = Some(AutoMarkRecord {
            last_index: i,
            fingerprint: fingerprint.clone(),
        });
        self.disarm_mark_timer();
        Some((session_key, i, fingerprint))
    }

    /// Mark the activity row under the cursor read — the explicit
    /// per-row counterpart to the auto-mark-on-hover timer, fired by
    /// `m` while this pane is focused (via the catalog's
    /// `MarkAllRead` dispatch). Records the undo target so `z` can
    /// revert, exactly like the timer path.
    ///
    /// Returns `true` when the cursor is on an actionable activity
    /// row (the per-row semantics apply, even if the row was already
    /// read and nothing was pushed); `false` when there is no row to
    /// act on — collapsed section, empty feed, or no workspace — so
    /// the caller can fall back to the workspace-wide mark.
    pub fn mark_cursor_row_read(&mut self, cmds: &mut Vec<Command>) -> bool {
        let Some(workspace) = self.workspace.as_mut() else {
            return false;
        };
        if self.activity_collapsed || workspace.activity.is_empty() {
            return false;
        }
        let cursor = self.feed.cursor;
        if workspace.is_activity_unread(cursor) {
            let fingerprint = workspace.activity.get(cursor).map(ActivityFingerprint::of);
            cmds.push(Command::MarkActivityRead {
                session_key: workspace.key.clone().into(),
                index: cursor,
                fingerprint: fingerprint.clone(),
            });
            if let Some(fingerprint) = fingerprint {
                self.last_marked_read = Some(AutoMarkRecord {
                    last_index: cursor,
                    fingerprint,
                });
            }
            // Local echo, same as the auto-mark timer path: flip the
            // row read immediately so `m` updates on this frame
            // instead of waiting for the daemon's next
            // `WorkspaceUpserted` round-trip.
            workspace.mark_activity_read(cursor);
        }
        true
    }

    /// Activity row under the cursor when per-row mark-read semantics apply.
    pub fn activity_cursor_target(&self) -> Option<usize> {
        let workspace = self.workspace.as_ref()?;
        if self.activity_collapsed || workspace.activity.is_empty() {
            None
        } else {
            Some(self.feed.cursor)
        }
    }

    /// Apply the optimistic local echo for a planned per-row mark.
    pub fn mark_activity_read_locally(&mut self, index: usize) {
        let Some(workspace) = self.workspace.as_mut() else {
            return;
        };
        let Some(fingerprint) = workspace.activity.get(index).map(ActivityFingerprint::of) else {
            return;
        };
        self.last_marked_read = Some(AutoMarkRecord {
            last_index: index,
            fingerprint,
        });
        workspace.mark_activity_read(index);
    }

    /// Undo the most recent auto-mark, if any. Returns
    /// `(session_key, index)` for the caller to persist via
    /// `Command::UnmarkActivityRead`.
    ///
    /// Resolves the stored fingerprint to the activity's *current*
    /// position — a poll that introduced a new top-of-feed comment
    /// shifts every older row down by one, and a raw cached index
    /// would un-read the wrong comment.
    fn undo_auto_mark(&mut self) -> Option<(lazybox_core::SessionKey, usize)> {
        let record = self.last_marked_read.take()?;
        let workspace = self.workspace.as_mut()?;
        let index = match record.resolve(&workspace.activity) {
            Some(i) => i,
            None => {
                tracing::debug!(
                    workspace = %workspace.key,
                    last_index = record.last_index,
                    "z undo: activity gone — refusing to unmark a stranger",
                );
                self.mark_timer.disarm();
                self.mark_timer_target = None;
                return None;
            }
        };
        workspace.unmark_activity_read(index);
        // Re-arm if the cursor is still on this row — otherwise the
        // timer would re-fire on the next tick and undo the undo.
        // Simpler: just clear; user can re-arm by moving.
        self.mark_timer.disarm();
        self.mark_timer_target = None;
        Some((lazybox_core::SessionKey::from(&workspace.key), index))
    }

    /// Drive the auto-mark timer. Called from the App's per-tick
    /// path. Returns `(session_key, index, fingerprint)` when the
    /// timer fired and an activity was just marked, so the App can
    /// persist via IPC.
    pub fn tick(
        &mut self,
        focused: bool,
    ) -> Option<(lazybox_core::SessionKey, usize, ActivityFingerprint)> {
        // `focused` parameter kept for API compatibility but no
        // longer gates the fire. The reasoning was originally "user
        // navigated away, stop the countdown" but in practice the
        // user often KEEPS the sidebar focused while looking at the
        // activity that's already rendered in the right pane (the
        // right pane shows the sidebar's selected workspace either
        // way). The result was auto-mark only firing if the user
        // explicitly Tab-ed into the right pane, which most users
        // didn't do. Now: the timer ticks regardless of which pane
        // owns keyboard focus.
        let _ = focused;
        if !self.mark_timer.ready(self.auto_mark_delay) {
            return None;
        }
        self.fire_auto_mark()
    }

    /// Called by the App when this pane's focus state flips. On focus
    /// gain we re-arm the auto-mark timer (so the user just landing
    /// on the activity feed kicks off the countdown). On focus loss
    /// we disarm and clear the undo target — switching panes is a
    /// natural "I'm done here" boundary.
    pub fn notify_focus_changed(&mut self, focused: bool) {
        if focused {
            self.rearm_mark_timer(true);
        } else {
            self.disarm_mark_timer();
            self.last_marked_read = None;
        }
    }

    /// Collapse / expand the activity section. Visible for tests and
    /// for click-to-toggle (mouse handler can call this directly).
    pub fn set_activity_collapsed(&mut self, collapsed: bool) {
        self.activity_collapsed = collapsed;
        self.activity_collapse_user_set = true;
    }

    pub fn activity_collapsed(&self) -> bool {
        self.activity_collapsed
    }

    /// Collapse the cursor's activity row if it's currently expanded.
    /// Returns `true` when a row was actually collapsed — the caller
    /// (directional `←`/`h` focus movement) treats `false` as "nothing
    /// left to collapse" and steps focus back to the sidebar instead.
    pub fn collapse_focused_row(&mut self) -> bool {
        let Some(workspace) = &self.workspace else {
            return false;
        };
        if self.activity_collapsed || workspace.activity.is_empty() {
            return false;
        }
        let cursor = self.feed.cursor;
        if self.feed.is_expanded(cursor) {
            self.feed.set_expanded(cursor, false);
            true
        } else {
            false
        }
    }

    /// Whether the pane has anything worth showing for the current
    /// workspace: a non-empty activity feed, or a PR / issue
    /// description body. Drives the orchestrator's decision to hide
    /// the whole Activity pane (giving its space to the terminal)
    /// when a workspace is empty. Mirrors the `has_activity` /
    /// `has_body` predicates `contextual_bindings` already uses.
    pub fn has_visible_content(&self) -> bool {
        let Some(w) = self.workspace.as_ref() else {
            return false;
        };
        !w.activity.is_empty() || self.has_task_body()
    }

    /// Apply the auto-collapse-on-empty rule. Honours the user
    /// override: once they've toggled explicitly we don't fight them.
    fn auto_collapse_for_workspace(&mut self) {
        if self.activity_collapse_user_set {
            return;
        }
        let empty = self
            .workspace
            .as_ref()
            .map(|w| w.activity.is_empty())
            .unwrap_or(true);
        self.activity_collapsed = empty;
    }

    /// AppRoot calls this whenever the Sidebar's selection changes.
    /// Resets the comment cursor because what was "row 3" on the
    /// previous workspace is meaningless on the new one.
    /// Set the focused PR's stacked-PR position (issue #969). Fed from
    /// the sidebar's index by the model whenever the selection changes,
    /// paired with [`RightPane::set_workspace`].
    pub fn set_stack(&mut self, stack: Option<lazybox_core::StackPosition>) {
        self.stack = stack;
    }

    /// Set the repo / Space overview shown on a group-header row (issue
    /// #1442). Paired with `set_workspace(None)`; ignored visually while
    /// a workspace is selected.
    pub fn set_overview(
        &mut self,
        overview: Option<crate::components::repo_overview::RepoOverview>,
    ) {
        self.overview = overview;
    }

    /// Whether the pane is currently showing a group overview — the
    /// model reads this to give the overview the whole right column
    /// (there's no terminal stack to share with on a header row).
    pub fn showing_overview(&self) -> bool {
        self.workspace.is_none() && self.overview.is_some()
    }

    /// Drain a roster-click workspace selection queued by the overview.
    pub fn take_select_workspace(&mut self) -> Option<lazybox_core::SessionKey> {
        self.pending_select_workspace.take()
    }

    pub fn set_workspace(&mut self, workspace: Option<Workspace>) {
        let same = match (&self.workspace, &workspace) {
            (Some(a), Some(b)) => a.key == b.key,
            _ => false,
        };
        self.workspace = workspace;
        if !same {
            self.comment_scroll = 0;
            self.feed.cursor = 0;
            // New workspace selection — drop the user's collapse
            // override so we re-apply the empty-aware default. Without
            // this, toggling once would stick across every workspace.
            self.activity_collapse_user_set = false;
            // Auto-mark state belongs to whatever workspace was last
            // displayed. A stale `last_marked_read` would point at an
            // index in workspace A; pressing `z` on workspace B would
            // un-read a different activity entirely. Disarm + forget.
            self.disarm_mark_timer();
            self.last_marked_read = None;
            // Indices are workspace-relative; an "expanded row 3" on
            // PR A points at a wholly different comment on PR B.
            // `on_workspace_change` resets cursor + expanded + selected
            // atomically — see `ActivityFeed`.
            self.feed.on_workspace_change();
            // Click hits reference the PREVIOUS workspace's render —
            // an "activity_cards[3] at row 14..17" doesn't mean
            // anything for the new workspace. Clear so a click after
            // the switch hits the freshly-rendered card index.
            self.click_hits = ClickHits::default();
            // Footer's "selected activity #N of M" must not leak —
            // the M was from the old workspace.
            self.pending_selection_notice = None;
            // The description-teaser toggle is per-workspace too: a
            // Preview left open on PR A must not silently expand PR B's
            // description the moment it's selected. Fold back to the
            // Collapsed default (and clear the render-derived overflow
            // flag; it's recomputed on the next `render_task_body`).
            self.task_body_view = TaskBodyView::Collapsed;
            self.body_overflows = false;
        }
        self.auto_collapse_for_workspace();
        // Bump the activity revision iff the content actually changed —
        // `set_workspace` runs on every pane sync with a fresh clone,
        // and most of those carry byte-identical activity.
        self.refresh_activity_rev();
        // Arm the auto-mark timer if the new workspace has an unread
        // row under the (fresh) cursor. Without this the badge count
        // stuck on the sidebar even as the user navigated past every
        // comment — the timer only re-armed on Tab focus change /
        // explicit click, which most users never did from the
        // sidebar. The `focused` flag passed here is ignored by
        // `should_arm_mark_timer` (kept in the signature for opt-in
        // future use); arming is purely "is the cursor on an unread
        // row right now."
        self.rearm_mark_timer(true);
    }

    /// Cheap fingerprint of the current workspace's activity *set*
    /// identity — everything that determines the rendered card content
    /// except body bytes (length + timestamp + author + node id stand
    /// in; a body edit that keeps all of those identical is not
    /// detectable, an accepted trade for not re-hashing every body on
    /// every pane sync). Read/unread bits are deliberately excluded:
    /// the buffer key folds those per-entry itself (they mutate
    /// in-place without going through `set_workspace`).
    fn compute_activity_identity(workspace: Option<&Workspace>) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let Some(w) = workspace else {
            return 0;
        };
        let mut h = DefaultHasher::new();
        w.key.as_str().hash(&mut h);
        w.activity.len().hash(&mut h);
        for a in &w.activity {
            a.author.hash(&mut h);
            a.body.len().hash(&mut h);
            a.created_at.timestamp_millis().hash(&mut h);
            (a.kind as u8).hash(&mut h);
            a.node_id.hash(&mut h);
        }
        h.finish()
    }

    /// Recompute the activity-identity fingerprint and bump
    /// `activity_rev` when it moved. Called wherever `self.workspace`
    /// is replaced (`set_workspace`, the `WorkspaceUpserted` handler).
    fn refresh_activity_rev(&mut self) {
        let identity = Self::compute_activity_identity(self.workspace.as_ref());
        if identity != self.activity_identity {
            self.activity_identity = identity;
            self.activity_rev = self.activity_rev.wrapping_add(1);
        }
    }

    /// Map a mouse click to a pane action. Returns `true` when the
    /// click hit a known target and the caller should redraw. The
    /// orchestrator wires this from its mouse handler so:
    ///
    /// - clicking the `▶/▼ Description` row toggles the body
    /// - clicking the `▸/▾ Activity` row toggles the activity section
    /// - clicking an activity card moves the cursor onto it AND
    ///   toggles its expand state (the natural "open this one" gesture)
    ///
    /// All targets are populated during render via `click_hits`; this
    /// function does pure lookup, no re-layout.
    pub fn handle_mouse_click(&mut self, _col: u16, row: u16) -> bool {
        tracing::debug!(
            click_row = row,
            body_header_row = ?self.click_hits.body_header_row,
            activity_header_row = ?self.click_hits.activity_header_row,
            num_cards = self.click_hits.activity_cards.len(),
            cards = ?self.click_hits.activity_cards,
            "right_pane.handle_mouse_click",
        );
        // Overview roster click (issue #1442): move the sidebar cursor
        // onto the clicked workspace. Handled first — in overview mode
        // none of the workspace-shaped click regions below are live.
        if self.showing_overview() {
            if let Some((_, key)) = self.overview_hits.iter().find(|(r, _)| *r == row) {
                self.pending_select_workspace = Some(key.clone());
                return true;
            }
            return false;
        }
        if let Some((r, url)) = &self.click_hits.header_title
            && *r == row
        {
            // The title is a live link — open the PR / issue itself.
            self.pending_open_url = Some(url.clone());
            return true;
        }
        if let Some((r, url)) = &self.click_hits.header_issue
            && *r == row
        {
            // Open the originating issue directly (#567).
            self.pending_open_url = Some(url.clone());
            return true;
        }
        if Some(row) == self.click_hits.header_reviewers {
            // Click the Reviewers line to open the reviewer picker —
            // same effect as pressing `g r` (#1092).
            self.pending_request_reviewers = true;
            return true;
        }
        if Some(row) == self.click_hits.body_header_row {
            // Click the description header to toggle the teaser — same
            // effect as pressing `d`.
            self.toggle_task_body();
            return true;
        }
        if Some(row) == self.click_hits.body_more_row {
            // The `+N more lines` trailer opens the whole body in the
            // reader modal — clicking the thing that says "there's more"
            // should reveal all of it (#448).
            self.request_open_description();
            return true;
        }
        if Some(row) == self.click_hits.activity_header_row {
            self.activity_collapsed = !self.activity_collapsed;
            self.activity_collapse_user_set = true;
            return true;
        }
        if let Some((idx, _)) = self
            .click_hits
            .activity_cards
            .iter()
            .find(|(_, range)| range.contains(&row))
        {
            let target = *idx;
            self.feed.cursor = target;
            // Single click on a card = "select THIS row" (radio).
            // Replaces the previous toggle-into-set behaviour, which
            // meant clicking row B while row A was selected
            // deselected A (toggle off… no wait, A stayed; the bug
            // was clicking the SAME row twice in a row deselected it
            // visually without re-selecting on next click — and
            // accumulating selection across clicks confused users
            // who expected mailer-style "click moves the highlight").
            // Explicit multi-select stays available via `v`
            // (keyboard).
            self.feed.select_only(target);
            self.pending_selection_notice = Some(format!(
                "selected activity #{} of {}",
                target + 1,
                self.workspace
                    .as_ref()
                    .map(|w| w.activity.len())
                    .unwrap_or(0),
            ));
            self.rearm_mark_timer(true);
            return true;
        }
        false
    }

    /// Drain the queued selection notice (if any). Orchestrator
    /// calls this after handling a click and forwards the string
    /// to its footer Notice.
    pub fn drain_selection_notice(&mut self) -> Option<String> {
        self.pending_selection_notice.take()
    }

    /// Drain a URL queued by a click on the header title or the
    /// originating-issue line. The orchestrator opens it in the
    /// browser (#567).
    pub fn take_open_url(&mut self) -> Option<String> {
        self.pending_open_url.take()
    }

    /// Drain the "open the reviewer picker" request queued by a click
    /// on the header `Reviewers:` line (#1092). The orchestrator runs
    /// the `g r` flow when this returns `true`.
    pub fn take_request_reviewers(&mut self) -> bool {
        std::mem::take(&mut self.pending_request_reviewers)
    }

    /// Double-click on an activity card → toggle its expanded state.
    /// Returns `true` when the click landed on a card (caller redraws).
    /// Header rows (description / activity) ignore double-click;
    /// their single-click toggle already does what the user wants.
    pub fn handle_mouse_double_click(&mut self, _col: u16, row: u16) -> bool {
        if let Some((idx, _)) = self
            .click_hits
            .activity_cards
            .iter()
            .find(|(_, range)| range.contains(&row))
        {
            let target = *idx;
            self.feed.cursor = target;
            self.feed.toggle_expand(target);
            self.rearm_mark_timer(true);
            return true;
        }
        false
    }

    /// Scroll the activity list by `delta` rows. Negative = scroll up
    /// (older comments — these are the newer ones, since the feed is
    /// newest-first); positive = scroll down. Used by the mouse-wheel
    /// handler. Returns `true` when the scroll actually moved so the
    /// caller can flag a redraw.
    pub fn scroll_activity(&mut self, delta: isize) -> bool {
        if self.activity_collapsed {
            return false;
        }
        // `comment_scroll` is a *line* offset into the virtual card
        // buffer — the same units as `last_total_lines` and
        // `last_visible_cards` (which actually holds line counts;
        // see field doc). Computing `max_scroll` in activity-count
        // units (the previous shape) underflowed to 0 whenever any
        // card was expanded, which is exactly when wheel-scroll
        // matters most.
        let total_lines = self.last_total_lines;
        let visible = self.last_visible_cards.max(1);
        let max_scroll = total_lines.saturating_sub(visible);
        if max_scroll == 0 {
            return false;
        }
        let before = self.comment_scroll;
        let new = if delta < 0 {
            before.saturating_sub((-delta) as usize)
        } else {
            (before + delta as usize).min(max_scroll)
        };
        if new == before {
            return false;
        }
        self.comment_scroll = new;
        self.rearm_mark_timer(true);
        true
    }

    pub fn selected_workspace(&self) -> Option<&Workspace> {
        self.workspace.as_ref()
    }

    /// Optimistic local flip mirroring `Sidebar::mark_workspace_merged`:
    /// when `Event::PrMerged` confirms a merge, flip the header pill to
    /// MERGED now if this pane is showing that workspace, instead of
    /// waiting for the confirming poll. The Model's merge latch keeps it
    /// MERGED across any interim `Open` poll.
    pub fn mark_workspace_merged(&mut self, key: &lazybox_core::WorkspaceKey) {
        if let Some(ws) = self.workspace.as_mut()
            && ws.key == *key
            && let Some(pr) = ws.pr.as_mut()
        {
            pr.state = lazybox_core::TaskState::Merged;
        }
    }

    pub fn comment_cursor(&self) -> usize {
        self.feed.cursor
    }

    /// Jump the activity row cursor to the first row (`g` — the
    /// catalog's `ActivityTop`). Safe no-op semantics on an empty /
    /// absent feed: cursor and scroll both pin to 0.
    pub fn activity_cursor_top(&mut self) {
        self.feed.cursor = 0;
        self.comment_scroll = 0;
    }

    /// Jump the activity row cursor to the last row (`Shift-G` — the
    /// catalog's `ActivityBottom`). No-op when there's no activity.
    pub fn activity_cursor_bottom(&mut self) {
        let Some(ws) = &self.workspace else {
            return;
        };
        if ws.activity.is_empty() {
            return;
        }
        self.feed.cursor = ws.activity.len() - 1;
        self.clamp_scroll_to_cursor();
    }

    /// Test accessor — top-of-viewport index into the activity
    /// list. Tests use this to verify the scroll-follows-cursor
    /// behavior.
    pub fn comment_scroll(&self) -> usize {
        self.comment_scroll
    }

    /// Activity indices the user explicitly multi-selected (via `v`
    /// or click). Empty when nothing's selected. Used by the
    /// orchestrator's `m` (mark) dispatch to decide between
    /// "mark only the selected activities" and "mark all" — same
    /// `m` key, smarter semantics based on whether the user has
    /// a per-row selection in flight.
    pub fn selected_activity_indices(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self.feed.selected().iter().copied().collect();
        v.sort_unstable();
        v
    }

    /// Fingerprint of the activity row at `index` in the current
    /// snapshot, for shipping alongside the raw index in
    /// `Command::MarkActivityRead` (the daemon re-resolves it against
    /// its own list). None when the index is out of range.
    pub fn activity_fingerprint_at(&self, index: usize) -> Option<ActivityFingerprint> {
        self.workspace
            .as_ref()?
            .activity
            .get(index)
            .map(ActivityFingerprint::of)
    }

    /// Drop the multi-select set. Called after `w` consumes the
    /// selection so the rows don't stay highlighted once the agent
    /// has been spawned with them.
    pub fn clear_activity_selection(&mut self) {
        self.feed.clear_selection();
    }

    /// The counterpart-link row for the header — the task this workspace
    /// is paired with — as `(prefix, label, url)`. Bidirectional and
    /// provider-aware (#567, #922):
    ///
    /// - **PR primary** → its originating issue / ticket. A folded-in
    ///   `Task` (`gh_issues` / `linear_issues`, carrying a real provider
    ///   URL) wins; a GitHub issue reads `Issue: #N`, a Linear ticket
    ///   `Linear: ENG-123`. Falls back to the PR's `closes_issues`
    ///   reference, whose `owner/repo#N` key yields a derived GitHub issue
    ///   URL.
    /// - **Linear ticket primary** → its linked GitHub PR (from the
    ///   ticket's attachment, before any collapse), reading `PR: #N` with
    ///   a derived GitHub PR URL.
    ///
    /// Known limitation (#922): a PR whose Linear ticket has been matched
    /// only by identifier (`pr.linked_tasks`), not yet fetched/folded,
    /// shows no counterpart row — a Linear URL can't be derived from the
    /// `ENG-123` identifier alone (it needs the workspace slug the fetched
    /// ticket carries). The row appears once the ticket is folded in.
    ///
    /// `None` for a workspace with no tracked counterpart.
    fn originating_issue(&self) -> Option<(String, String, String)> {
        let ws = self.workspace.as_ref()?;
        if let Some(pr) = ws.pr.as_ref() {
            if let Some(issue) = ws.gh_issues.iter().chain(ws.linear_issues.iter()).next() {
                let prefix = if issue.id.source == "linear" {
                    "Linear"
                } else {
                    "Issue"
                };
                return Some((prefix.into(), task_ref_label(issue), issue.url.clone()));
            }
            let issue_id = pr.closes_issues.first()?;
            let (repo, number) = issue_id.key.rsplit_once('#')?;
            return Some((
                "Issue".into(),
                format!("#{number}"),
                format!("https://github.com/{repo}/issues/{number}"),
            ));
        }
        // Ticket / issue primary (no PR yet): surface its linked GitHub PR.
        let primary = ws.primary_task()?;
        let pr_id = primary
            .linked_tasks
            .iter()
            .find(|id| id.source == "github")?;
        let (repo, number) = pr_id.key.rsplit_once('#')?;
        Some((
            "PR".into(),
            format!("#{number}"),
            format!("https://github.com/{repo}/pull/{number}"),
        ))
    }

    fn render_header(
        &mut self,
        area: Rect,
        frame: &mut Frame,
        origin: Option<(String, String, String)>,
        show_diffstat: bool,
    ) {
        self.click_hits.header_title = None;
        self.click_hits.header_issue = None;
        self.click_hits.header_reviewers = None;
        let theme = crate::theme::current();
        let Some(workspace) = &self.workspace else {
            let line = Line::from(Span::styled(" (no session selected) ", theme.hint()));
            let placeholder = Paragraph::new(crate::components::table::truncate_line(
                line,
                area.width as usize,
            ));
            frame.render_widget(placeholder, area);
            return;
        };
        let Some(task) = workspace.primary_task() else {
            // Workspace exists but no task attached yet (created from
            // scratch). Show a minimal header so the user knows where
            // they are and what branch the next agent will spawn into.
            let lines = vec![Line::from(vec![
                Span::styled(
                    " EMPTY ",
                    Style::default()
                        .bg(theme.chrome)
                        .fg(theme.text_strong)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
                Span::styled(&workspace.name, Style::default().bold()),
            ])];
            // Same truncation treatment as the populated header
            // path below — wrap can't break inside a workspace
            // name like `tensorzero/nanogateway`, so a narrow pane
            // would clip the trailing chars silently.
            let width = area.width as usize;
            let truncated: Vec<Line> = lines
                .into_iter()
                .map(|l| crate::components::table::truncate_line(l, width))
                .collect();
            frame.render_widget(Paragraph::new(truncated), area);
            return;
        };

        let mut lines: Vec<Line> = Vec::new();

        use crate::components::icons;
        use crate::lazybox_theme::{self, StatePill};

        // Breadcrumb above the title: `repo · #1234`. Dim, separated
        // by a `›`. Orients the user to "where am I?" before they
        // read the title — yazi's cwd line plays the same role. The
        // item's creator rides the far right of this top row
        // (`opened by @author`) so "who opened this" reads as primary
        // top-right context for PRs, issues, and Linear alike (#849) —
        // this row is always present and short, unlike the Branch row
        // (which is filler `Branch: -` for issues).
        let mut crumbs: Vec<Span> = Vec::new();
        if let Some(repo) = task.repo.as_deref() {
            crumbs.push(Span::styled(
                format!("{} ", icons::REPO),
                Style::default().fg(theme.warn),
            ));
            crumbs.push(Span::styled(
                repo.to_string(),
                Style::default().fg(theme.text_strong),
            ));
            if let Some(n) = crate::components::task_label::pr_number(task) {
                crumbs.push(Span::styled("  ›  ", Style::default().fg(theme.chrome)));
                crumbs.push(Span::styled(
                    format!("#{n}"),
                    Style::default()
                        .fg(crate::components::task_label::pr_number_color(n))
                        .add_modifier(Modifier::BOLD),
                ));
            }
        }
        // Skip the creator when the viewer is the author — you don't need
        // "opened by @you" on your own PR/issue. #849 is about seeing who
        // opened *someone else's* item, so the byline only earns its space
        // when the author isn't the viewer.
        if !task.author.is_empty() && task.role != lazybox_core::TaskRole::Author {
            let opened_by = "opened by ";
            let handle = format!("@{}", task.author);
            let left: usize = crumbs
                .iter()
                .map(|s| crate::util::visual_width(s.content.as_ref()))
                .sum();
            let right = crate::util::visual_width(opened_by) + crate::util::visual_width(&handle);
            // Right-align when the row has slack; otherwise keep a single
            // space so the crumb and creator never glue together as the
            // pane narrows.
            let gap = (area.width as usize).saturating_sub(left + right).max(1);
            crumbs.push(Span::raw(" ".repeat(gap)));
            crumbs.push(Span::styled(opened_by, Style::default().fg(theme.text_dim)));
            crumbs.push(Span::styled(handle, Style::default().fg(theme.hover)));
        }
        if !crumbs.is_empty() {
            lines.push(Line::from(crumbs));
        }

        // State pill — yazi-style powerline segment: solid block in
        // state color, prefixed by a Nerd-Font glyph, closed by a
        // triangle that "flows" into the page background.
        let (icon, label, bucket) = match task.state {
            lazybox_core::TaskState::Open => (icons::PR_OPEN, "OPEN", StatePill::Open),
            lazybox_core::TaskState::Draft => (icons::PR_DRAFT, "DRAFT", StatePill::Draft),
            lazybox_core::TaskState::Merged => (icons::PR_MERGED, "MERGED", StatePill::Merged),
            lazybox_core::TaskState::Closed => (icons::PR_CLOSED, "CLOSED", StatePill::Closed),
            lazybox_core::TaskState::InProgress => (icons::PR_WIP, "WIP", StatePill::InProgress),
            lazybox_core::TaskState::InReview => (icons::PR_REVIEW, "REVIEW", StatePill::InReview),
        };
        let (bg, fg) = lazybox_theme::state_pill(theme, bucket);
        // The title line is a live link: clicking it opens the task
        // in the browser (#567). Underline the title and trail an `↗`
        // so it reads as clickable to a mouse user without knowing the
        // `g o` chord (#590) — the whole row is the hit region, so the
        // glyph is clickable too. Only register the click target when
        // the row actually falls inside the (possibly squished) header
        // area — otherwise a clipped row would record a hit region over
        // whatever paints below it.
        let title_row = area.y + lines.len() as u16;
        if title_row < area.bottom() {
            self.click_hits.header_title = Some((title_row, task.url.clone()));
        }
        // The `↗` is pinned to the row's end so a long title truncates
        // ahead of the affordance instead of dropping it — the whole
        // row is the hit region, so it must stay legible as a link even
        // when the pane is narrow.
        let title_body = Line::from(vec![
            Span::styled(
                format!(" {icon} {label} "),
                Style::default().bg(bg).fg(fg).add_modifier(Modifier::BOLD),
            ),
            Span::styled(icons::POWERLINE_RIGHT, Style::default().fg(bg)),
            Span::raw(" "),
            Span::styled(
                &task.title,
                Style::default()
                    .fg(theme.text_strong)
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::UNDERLINED),
            ),
        ]);
        lines.push(crate::components::table::truncate_line_keep_suffix(
            title_body,
            Span::styled(
                format!(" {}", icons::EXTERNAL_LINK),
                Style::default().fg(theme.accent),
            ),
            area.width as usize,
        ));

        // Branch line — confirms which worktree lazybox will spawn an
        // agent into. Cyan accent on a "Branch:" dim label.
        let branch = task.branch.as_deref().unwrap_or("-");
        lines.push(Line::from(vec![
            Span::styled("Branch: ", Style::default().fg(theme.text_dim)),
            Span::styled(branch, Style::default().fg(theme.accent)),
        ]));

        // Stacked-PR relationship (issue #969). A PR whose base is
        // another open PR's head sits mid-stack; spell out the parent it
        // rides on and its `k/N` position so the chain reads as a stack,
        // not unrelated rows. The bottom PR (nothing beneath it) shows
        // only its position — there's no parent to name.
        if let Some(stack) = &self.stack {
            let position = format!("{}/{}", stack.position, stack.depth);
            let mut spans = vec![Span::styled("Stack: ", Style::default().fg(theme.text_dim))];
            match stack.parent.as_ref().and_then(|p| p.number()) {
                // Mid-stack: name the parent, then the position after a
                // separator.
                Some(parent) => {
                    spans.push(Span::styled(
                        format!("stacked on #{parent} "),
                        Style::default().fg(theme.accent),
                    ));
                    spans.push(Span::styled(
                        format!("· {position}"),
                        Style::default().fg(theme.text_dim),
                    ));
                }
                // Bottom of the stack: no parent to name, so show the
                // position alone — no dangling separator.
                None => spans.push(Span::styled(position, Style::default().fg(theme.text_dim))),
            }
            lines.push(Line::from(spans));
        }

        // Diffstat — a one-line at-a-glance sense of a PR's size/shape.
        // PRs only; issues have no diff. Additions green, deletions red,
        // file count dim, matching the desktop `detailSignals` summary.
        // `show_diffstat` is computed once in `render` and threaded in so
        // the height reservation and this line read the same value.
        if show_diffstat {
            let files = task.changed_files;
            let files_noun = if files == 1 { "file" } else { "files" };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("+{}", task.additions),
                    Style::default().fg(theme.success),
                ),
                Span::raw(" "),
                Span::styled(
                    format!("−{}", task.deletions),
                    Style::default().fg(theme.error),
                ),
                Span::styled(
                    format!(" · {files} {files_noun} changed"),
                    Style::default().fg(theme.text_dim),
                ),
            ]));
        }

        // Originating issue — the Issue this PR was created from /
        // closes. An explicit, clickable path to it so the user doesn't
        // have to expand the description and hunt for the reference
        // (#567). Its own row so the click handler can tell it apart
        // from the title link above (row-granular hit-testing). The
        // click target is only registered when the row is inside the
        // header area (see the title line above).
        if let Some((prefix, label, url)) = origin {
            let issue_row = area.y + lines.len() as u16;
            if issue_row < area.bottom() {
                self.click_hits.header_issue = Some((issue_row, url));
            }
            lines.push(Line::from(vec![
                Span::styled(format!("{prefix}: "), Style::default().fg(theme.text_dim)),
                Span::styled(
                    label,
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::UNDERLINED),
                ),
            ]));
        }

        // Why this ticket is in your inbox (#1015). Linear only ever
        // returns issues assigned to or created by the token's viewer,
        // so name that reason outright — and when a ticket matches
        // neither (which the default `[assigned, created]` scope should
        // make impossible) say so plainly rather than letting it read
        // like any other "yours" row: a ticket you didn't create or get
        // assigned is the tell that the token resolves to the wrong
        // Linear identity, or that a subscribed scope is pulling it in.
        if task.id.source == lazybox_core::LINEAR_SOURCE {
            let (glyph_color, reason) = match task.role {
                lazybox_core::TaskRole::Assignee => (theme.accent, "assigned to you"),
                lazybox_core::TaskRole::Author => (theme.accent, "created by you"),
                _ => (theme.warn, "not assigned to or created by you"),
            };
            lines.push(Line::from(vec![
                Span::styled("◆ ", Style::default().fg(glyph_color)),
                Span::styled(reason, Style::default().fg(theme.text_dim)),
            ]));
        }

        // Reviewers + assignees. When populated, render the @logins.
        // When empty AND the task is a PR, surface a discoverable
        // hint with the keyboard shortcut so the user doesn't have
        // to dig through `?` to learn how to add them. Empty
        // *issues* don't get the hint for reviewers (issues have no
        // reviewers on GitHub), but they DO get the assignees hint.
        let is_pr = task.is_pr();
        let reviewer_summary = task.reviewer_summary();
        if !reviewer_summary.is_empty() {
            use lazybox_core::ReviewState;
            let mut spans: Vec<Span> = Vec::with_capacity(reviewer_summary.len() * 3 + 1);
            spans.push(Span::styled(
                "Reviewers: ",
                Style::default().fg(theme.text_dim),
            ));
            for (i, reviewer) in reviewer_summary.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" ", Style::default()));
                }
                // A state glyph precedes each reviewer so an approved PR
                // reads as approved at a glance — pending reviewers keep
                // the bare `@login` styling for continuity with the old
                // requested-only line.
                let (glyph, color) = match reviewer.state {
                    ReviewState::Approved => (Some("✓"), theme.success),
                    ReviewState::ChangesRequested => (Some("✗"), theme.error),
                    ReviewState::Commented => (Some(icons::COMMENT), theme.text_dim),
                    ReviewState::Pending => (None, theme.hover),
                };
                if let Some(glyph) = glyph {
                    spans.push(Span::styled(
                        format!("{glyph} "),
                        Style::default().fg(color),
                    ));
                }
                spans.push(Span::styled(
                    format!("@{}", reviewer.login),
                    Style::default().fg(color),
                ));
                // Distinguish a bot approval (e.g. `claude[bot]`) from a
                // human one — a repo on the `approval: human` policy
                // treats a bot-only approval as not-yet-ready.
                if reviewer.is_bot {
                    spans.push(Span::styled(" (bot)", Style::default().fg(theme.text_dim)));
                }
            }
            let reviewers_row = area.y + lines.len() as u16;
            if reviewers_row < area.bottom() {
                self.click_hits.header_reviewers = Some(reviewers_row);
            }
            lines.push(Line::from(spans));
        } else if is_pr {
            let reviewers_row = area.y + lines.len() as u16;
            if reviewers_row < area.bottom() {
                self.click_hits.header_reviewers = Some(reviewers_row);
            }
            lines.push(Line::from(vec![
                Span::styled("Reviewers: ", Style::default().fg(theme.text_dim)),
                Span::styled(
                    "none ",
                    Style::default()
                        .fg(theme.text_dim)
                        .add_modifier(Modifier::ITALIC),
                ),
                Span::styled("— g r to request", Style::default().fg(theme.text_dim)),
            ]));
        }
        if !task.assignees.is_empty() {
            let mut spans: Vec<Span> = Vec::with_capacity(task.assignees.len() * 2 + 1);
            spans.push(Span::styled(
                "Assignees: ",
                Style::default().fg(theme.text_dim),
            ));
            for (i, login) in task.assignees.iter().enumerate() {
                if i > 0 {
                    spans.push(Span::styled(" ", Style::default()));
                }
                spans.push(Span::styled(
                    format!("@{login}"),
                    Style::default().fg(theme.accent),
                ));
            }
            lines.push(Line::from(spans));
        } else {
            lines.push(Line::from(vec![
                Span::styled("Assignees: ", Style::default().fg(theme.text_dim)),
                Span::styled(
                    "none ",
                    Style::default()
                        .fg(theme.text_dim)
                        .add_modifier(Modifier::ITALIC),
                ),
                Span::styled("— g a to change", Style::default().fg(theme.text_dim)),
            ]));
        }

        // Truncate each line to the pane width with `…` instead of
        // relying on `Wrap`. The wrap can only break at whitespace,
        // and the lines this header produces are mostly single
        // identifiers — `tensorzero/nanogateway`, branch names,
        // `@logins` — that have no break point and just clipped
        // silently when the pane got narrow. truncate_line preserves
        // every span's style and emits `…` at the cut.
        let width = area.width as usize;
        let truncated: Vec<Line> = lines
            .into_iter()
            .map(|l| crate::components::table::truncate_line(l, width))
            .collect();
        let para = Paragraph::new(truncated);
        frame.render_widget(para, area);
    }

    /// Test accessor — how many times the activity buffer has been
    /// rebuilt. The encapsulation regression test asserts this stays
    /// flat across scrolls.
    #[cfg(test)]
    pub(super) fn activity_rebuilds(&self) -> u32 {
        self.activity_rebuilds
    }

    /// Test accessor — the activity content revision. The memo-key
    /// regression tests pin "identical content doesn't bump, mutated
    /// content does."
    #[cfg(test)]
    pub(super) fn activity_rev(&self) -> u64 {
        self.activity_rev
    }

    /// Fingerprint every input that determines the activity virtual-line
    /// buffer, so [`render_activity`](Self::render_activity) can skip the
    /// rebuild when nothing relevant changed.
    ///
    /// Crucially, `comment_scroll` is *not* hashed: scrolling cannot
    /// alter this key, so it can never invalidate the cached buffer.
    /// That is the encapsulation that makes "scrolling rebuilds the whole
    /// feed" — the unresponsiveness this guards against — impossible by
    /// construction rather than merely tuned away.
    ///
    /// `now` (the timestamp feeding relative ages like "2m") is also
    /// excluded: folding it in would defeat the cache on every frame.
    /// Relative ages refresh whenever any real change rebuilds the
    /// buffer, and a poll lands at least once a minute, so staleness is
    /// bounded and invisible.
    fn activity_buffer_key(
        activity_rev: u64,
        workspace: &Workspace,
        feed: &crate::components::activity_feed::ActivityFeed,
        viewer_logins: &std::collections::HashMap<String, String>,
        theme_name: &str,
        body_width: u16,
        focused: bool,
    ) -> u64 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        // XOR-fold sets/maps so the result is independent of iteration
        // order — two logically-equal sets MUST hash equal, otherwise an
        // unrelated mutation could silently invalidate the cache.
        fn fold_indices(set: &std::collections::HashSet<usize>) -> u64 {
            // `+1` so index 0 doesn't multiply to zero, which would make
            // `{0}` fold identically to the empty set.
            set.iter().fold(0u64, |acc, &i| {
                acc ^ (i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)
            })
        }

        let mut h = DefaultHasher::new();
        body_width.hash(&mut h);
        focused.hash(&mut h);
        theme_name.hash(&mut h);
        feed.cursor.hash(&mut h);
        fold_indices(feed.expanded()).hash(&mut h);
        fold_indices(feed.selected()).hash(&mut h);
        let viewer_fold = viewer_logins.iter().fold(0u64, |acc, (k, v)| {
            let mut e = DefaultHasher::new();
            k.hash(&mut e);
            v.hash(&mut e);
            acc ^ e.finish()
        });
        viewer_fold.hash(&mut h);
        // Activity *content* rides the revision counter — bumped by
        // `refresh_activity_rev` whenever the set actually mutates —
        // instead of re-hashing every body's bytes on every frame
        // (which made an idle repaint O(total comment bytes)). The
        // per-entry read/unread bits are still folded directly: they
        // mutate in-place (auto-mark, `m`, `z`) without a workspace
        // swap, so the rev can't see them — and they're one bool each.
        activity_rev.hash(&mut h);
        workspace.activity.len().hash(&mut h);
        for i in 0..workspace.activity.len() {
            workspace.is_activity_unread(i).hash(&mut h);
        }
        h.finish()
    }

    /// Render the activity section. Always renders the header row;
    /// only renders the list body when `!activity_collapsed`. The
    /// header carries: collapse glyph, "Activity", total count, and
    /// an "● N new" badge when there's unread content.
    ///
    /// Returns the row count consumed (header + optional body), so
    /// future sections stacked below can offset themselves.
    fn render_activity(&mut self, area: Rect, frame: &mut Frame, focused: bool) -> u16 {
        let theme = crate::theme::current();
        let title_color = if focused { theme.accent } else { theme.chrome };
        self.click_hits.activity_header_row = if area.height > 0 { Some(area.y) } else { None };
        // Cards list stays empty while collapsed; the click handler
        // checks the header row independently.
        if self.activity_collapsed {
            self.click_hits.activity_cards.clear();
        }

        let total = self
            .workspace
            .as_ref()
            .map(|w| w.activity.len())
            .unwrap_or(0);
        let unread = self
            .workspace
            .as_ref()
            .map(|w| w.unread_count())
            .unwrap_or(0);
        // Triangle glyph mirrors the sidebar selection caret. ▸ when
        // collapsed (points right, "expand into me"), ▾ when expanded.
        let glyph = if self.activity_collapsed {
            "▸"
        } else {
            "▾"
        };

        let mut header_spans: Vec<Span> = vec![
            Span::styled(format!("{glyph} "), Style::default().fg(title_color)),
            Span::styled("Activity", theme.title(focused)),
        ];
        // Count goes in next to the label only when there IS
        // activity. `Activity 0` reads like a broken counter; bare
        // `Activity` reads naturally as "no activity yet" — which
        // is the truthful state for fresh PRs with no comments.
        if total > 0 {
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled(
                format!("{total}"),
                Style::default().fg(theme.text_dim),
            ));
            // Position indicator — only when not everything fits
            // on screen. Tells the user there's more to scroll into
            // and where they currently are. `last_visible_cards`
            // is set during the prior render; on first paint it
            // defaults to 1 which produces a reasonable hint.
            let visible = self.last_visible_cards.max(1);
            if total > visible && !self.activity_collapsed {
                let start = self.comment_scroll + 1;
                let end = (self.comment_scroll + visible).min(total);
                header_spans.push(Span::raw("  "));
                header_spans.push(Span::styled(
                    format!("[{start}–{end}]"),
                    Style::default().fg(theme.text_dim),
                ));
            }
        }
        if unread > 0 {
            header_spans.push(Span::raw("  "));
            header_spans.push(Span::styled(
                format!("● {unread} new"),
                theme.badge_unread(),
            ));
        }

        let title_area = Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(2),
            1.min(area.height),
        );
        frame.render_widget(
            Paragraph::new(crate::components::table::truncate_line(
                Line::from(header_spans),
                title_area.width as usize,
            )),
            title_area,
        );

        if self.activity_collapsed {
            return 1;
        }

        if area.height < 2 {
            return 1;
        }

        let div_area = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), 1);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(div_area.width as usize),
                Style::default().fg(Color::DarkGray),
            ))),
            div_area,
        );

        let inner = Rect {
            x: area.x + 1,
            y: area.y + 3,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(3),
        };

        let Some(workspace) = &self.workspace else {
            return 3;
        };

        if workspace.activity.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled("(no activity)", theme.hint()))),
                inner,
            );
            return area.height;
        }

        // Each activity renders as one card. Default shape (collapsed):
        //
        //     │ [review] alice  ›  one-line teaser of the body
        //
        // Expanded (`→` on the focused row) extends the card with up
        // to 12 wrapped body lines beneath the header, indented past
        // the marker bar so the cluster reads as one block.
        //
        // The leading `│` is the unread/cursor indicator (1 colored
        // cell, no full-row bg flood — yazi's marker_symbol pattern).
        // Indent of body content past the marker bar. 1 cell for the
        // bar itself + 3 spaces of breathing room.
        const BODY_INDENT: u16 = 4;
        // How many cells of the body to inline next to the header for
        // a collapsed row. Anything past this gets the `…` ellipsis;
        // expanding (`→`) shows the full body.
        const TEASER_CELLS: usize = 60;

        let body_width = inner.width.saturating_sub(BODY_INDENT);
        // Build EVERY card into a single virtual line buffer first —
        // no skip, no break — then apply `comment_scroll` as a
        // **line offset** when we clip to `inner.height`. The older
        // shape skipped whole activities and broke at the viewport
        // boundary, which made wheel-scrolling useless once even one
        // activity was expanded: the body lines past the viewport
        // were never rendered AND the wheel only ever advanced by
        // an entire activity, so you'd skip past the very lines you
        // wanted to read.
        //
        // Building that buffer is expensive (a markdown render per
        // expanded card), so it's memoized in `self.activity_buffer`
        // keyed on every input that affects it — but NOT on
        // `comment_scroll`. Scrolling reuses the cached buffer and only
        // re-windows it below; that's what keeps the feed responsive at
        // any list size. See [`ActivityBuffer`].
        let key = Self::activity_buffer_key(
            self.activity_rev,
            workspace,
            &self.feed,
            &self.viewer_logins,
            theme.name,
            body_width,
            focused,
        );
        if self.activity_buffer.as_ref().map(|b| b.key) != Some(key) {
            self.activity_buffer = Some(build_activity_buffer(
                key,
                workspace,
                &self.feed,
                &self.viewer_logins,
                theme,
                focused,
                body_width,
                BODY_INDENT,
                TEASER_CELLS,
            ));
            #[cfg(test)]
            {
                self.activity_rebuilds += 1;
            }
        }
        // Clamp `comment_scroll` to a valid line offset (the buffer
        // may have shrunk since the last render, e.g. a card was
        // collapsed). `last_visible_lines` is the height of the
        // window we're about to draw — used by `scroll_activity`
        // and `clamp_scroll_to_cursor` to bound the scroll.
        let total_lines = self
            .activity_buffer
            .as_ref()
            .expect("activity_buffer set above")
            .cards
            .len();
        let window = (inner.height as usize).min(total_lines);
        let max_scroll = total_lines.saturating_sub(window);
        if self.comment_scroll > max_scroll {
            self.comment_scroll = max_scroll;
        }
        let scroll = self.comment_scroll;

        // Window the virtual buffer down to what fits, and translate
        // each card's line span into absolute on-screen y-range for
        // hit-testing. Cards fully above / below the window are
        // dropped from the click index — they're not visible, no
        // click target. This is O(viewport) — the only per-frame work
        // a scroll triggers; the buffer itself is reused.
        let buffer = self
            .activity_buffer
            .as_ref()
            .expect("activity_buffer set above");
        let visible: Vec<Line<'static>> = buffer
            .cards
            .iter()
            .skip(scroll)
            .take(window)
            .cloned()
            .collect();
        let mut hits: Vec<(usize, std::ops::RangeInclusive<u16>)> = Vec::new();
        for &(i, start, end) in &buffer.card_spans {
            let s = start as usize;
            let e = end as usize;
            if e < scroll || s >= scroll + window {
                continue;
            }
            let visible_start = s.max(scroll) - scroll;
            let visible_end = e.min(scroll + window - 1) - scroll;
            let abs_start = inner.y.saturating_add(visible_start as u16);
            let abs_end = inner.y.saturating_add(visible_end as u16);
            hits.push((i, abs_start..=abs_end));
        }
        self.click_hits.activity_cards = hits;

        frame.render_widget(Paragraph::new(visible), inner);
        // Scroll-position indicator in the right padding column —
        // auto-hides when everything fits.
        crate::components::scrollbar::render_vertical(
            frame,
            Rect::new(inner.x + inner.width, inner.y, 1, inner.height),
            total_lines,
            window,
            scroll,
        );
        self.last_visible_cards = window.max(1);
        self.last_total_lines = total_lines;
        area.height
    }
}

/// Lay every activity out into one virtual line buffer: one header
/// `Line` per card, plus the wrapped markdown body for expanded cards.
/// Pure — no scroll, no `&mut self`, no `Frame`. This is the expensive
/// step `render_activity` memoizes behind [`ActivityBuffer`]; keeping it
/// a free function of explicit inputs (never `comment_scroll`) is what
/// stops a scroll from ever reaching it.
#[allow(clippy::too_many_arguments)]
fn build_activity_buffer(
    key: u64,
    workspace: &Workspace,
    feed: &crate::components::activity_feed::ActivityFeed,
    viewer_logins: &std::collections::HashMap<String, String>,
    theme: &crate::theme::Theme,
    focused: bool,
    body_width: u16,
    body_indent: u16,
    teaser_cells: usize,
) -> ActivityBuffer {
    let mut cards: Vec<Line<'static>> = Vec::new();
    let mut card_spans: Vec<(usize, u16, u16)> = Vec::new();
    let now = chrono::Utc::now();
    for (i, activity) in workspace.activity.iter().enumerate() {
        let start_line = cards.len() as u16;
        let state = CardState {
            is_cursor: i == feed.cursor,
            is_unread: workspace.is_activity_unread(i),
            is_expanded: feed.is_expanded(i),
            is_selected: feed.is_selected(i),
            focused,
        };
        cards.push(render_card_header(
            &state,
            activity,
            theme,
            now,
            teaser_cells,
            viewer_logins,
        ));
        if state.is_expanded {
            cards.extend(render_card_body(
                activity,
                theme,
                &state,
                body_width,
                body_indent,
            ));
        }
        let end_line = cards.len().saturating_sub(1) as u16;
        card_spans.push((i, start_line, end_line));
    }
    ActivityBuffer {
        key,
        cards,
        card_spans,
    }
}

/// Flatten a comment body into a single-line teaser. Strips Markdown
/// noise (HTML comments, leading hashes/quotes/bullets), collapses
/// whitespace, and clips to `max_cells` cells with `…`. Used in the
/// Strip inline markdown / HTML noise from `s` so the activity
/// teaser doesn't end up showing literal `<sub><sub>![Badge](url)`
/// soup. Targets four shapes:
///
/// - `<sub>` / `</sub>` (GitHub renders these as smaller text; in the
///   teaser they're pure clutter)
/// - `![alt text](url)` image references → `alt text`
/// - `[link text](url)` links → `link text`
/// - `<!-- comments -->` (already done by `strip_html_comments`
///   upstream, but pass-through safe).
///
/// Doesn't try to be a full markdown parser — these four are the
/// ones that show up in PR descriptions enough to ruin teasers.

/// Inherent methods. Lifted from the legacy `tui_kit::Pane` trait
/// impl so the realm wrappers can call them directly without UFCS.
impl RightPane {
    /// Stable pane id.
    pub fn id(&self) -> PaneId {
        self.id
    }

    /// Border title.
    pub fn title(&self) -> &str {
        "Activity"
    }

    /// State-aware short list for the footer hint bar.
    ///
    /// Catalog-driven: build the relevant `Action`s, then ask the
    /// catalog for the effective key (override-aware) + state-aware
    /// label. The `v select row` / `toggle row` flip is the one
    /// pane-local label override — selection state isn't in
    /// `workspace`, so it can't go through `contextual_label`.
    ///
    /// `overrides` carries the user's `ui.action_keys` map; pass an
    /// empty map to use catalog defaults.
    pub fn contextual_bindings(
        &self,
        overrides: &std::collections::BTreeMap<String, String>,
    ) -> Vec<crate::Binding> {
        use crate::Binding;
        use lazybox_tui_core::action::{Action, ActionDef, contextual_label};

        let workspace = self.workspace.as_ref();
        let has_activity = workspace.map(|w| !w.activity.is_empty()).unwrap_or(false);
        let selected: Vec<usize> = self.feed.selected().iter().copied().collect();
        let has_selection = !selected.is_empty();
        let has_body = self.has_task_body();

        let mut actions: Vec<Action> = Vec::with_capacity(6);
        if has_activity {
            actions.push(Action::Reply);
        }
        if crate::intent::classify_work(workspace, &selected).is_some() {
            actions.push(Action::Work);
        }
        if has_activity {
            actions.push(Action::SelectRow);
        }
        if has_body {
            actions.push(Action::ToggleDescription);
        }

        actions
            .into_iter()
            .map(|a| {
                let def = ActionDef::for_action(&a);
                let label: std::borrow::Cow<'static, str> = match &a {
                    Action::SelectRow if has_selection => std::borrow::Cow::Borrowed("toggle row"),
                    _ => std::borrow::Cow::Borrowed(contextual_label(&a, workspace)),
                };
                Binding {
                    keys: def.effective_keys_display(overrides),
                    label,
                }
            })
            .collect()
    }

    pub fn handle_key(&mut self, key: KeyEvent, cmds: &mut Vec<Command>) -> PaneOutcome {
        // Toggle keys work without a workspace too — collapse state is
        // owned by the pane, not the workspace.
        // `Enter` toggles the activity section. `Space` is reserved
        // for per-row multi-select (the list-UI convention) and
        // handled further down once we know a workspace exists and
        // the section is expanded.
        if key.code == KeyCode::Enter {
            self.set_activity_collapsed(!self.activity_collapsed);
            return PaneOutcome::Consumed;
        }

        // `z` undoes the most recent auto-mark-read, regardless of
        // collapse state. Idempotent: pressing it twice in a row only
        // un-reads once. Disarms the auto-mark timer too — otherwise
        // it would just re-fire on the next tick. Persists via
        // `Command::UnmarkActivityRead` so a restart sees the row
        // back in the unread set.
        if key.code == KeyCode::Char('z') && key.modifiers == KeyModifiers::NONE {
            if let Some((session_key, index)) = self.undo_auto_mark() {
                cmds.push(Command::UnmarkActivityRead { session_key, index });
            }
            return PaneOutcome::Consumed;
        }

        // Comment navigation requires a workspace AND an expanded
        // section — collapsed bodies don't render rows.
        let Some(workspace) = &self.workspace else {
            return PaneOutcome::Pass;
        };
        if self.activity_collapsed {
            return PaneOutcome::Pass;
        }
        let last = workspace.activity.len().saturating_sub(1);

        let result = match (key.code, key.modifiers) {
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                if workspace.activity.is_empty() {
                    return PaneOutcome::Consumed;
                }
                if self.feed.cursor < last {
                    self.feed.cursor += 1;
                }
                self.clamp_scroll_to_cursor();
                PaneOutcome::Consumed
            }
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                self.feed.cursor = self.feed.cursor.saturating_sub(1);
                self.clamp_scroll_to_cursor();
                PaneOutcome::Consumed
            }
            (KeyCode::PageDown, _) => {
                if !workspace.activity.is_empty() {
                    let jump = self.last_visible_cards;
                    self.feed.cursor = (self.feed.cursor + jump).min(last);
                    self.clamp_scroll_to_cursor();
                }
                PaneOutcome::Consumed
            }
            (KeyCode::PageUp, _) => {
                let jump = self.last_visible_cards;
                self.feed.cursor = self.feed.cursor.saturating_sub(jump);
                self.clamp_scroll_to_cursor();
                PaneOutcome::Consumed
            }
            // `→`/`l` expand the focused comment, `←`/`h` collapse it.
            // Per-row state lives in `self.feed` (see ActivityFeed)
            // so other rows stay condensed while one is being read.
            (KeyCode::Right, _) | (KeyCode::Char('l'), KeyModifiers::NONE) => {
                if !workspace.activity.is_empty() {
                    self.feed.set_expanded(self.feed.cursor, true);
                }
                PaneOutcome::Consumed
            }
            (KeyCode::Left, _) | (KeyCode::Char('h'), KeyModifiers::NONE) => {
                self.feed.set_expanded(self.feed.cursor, false);
                PaneOutcome::Consumed
            }
            // `g` / `Shift-G` normally arrive through the catalog
            // dispatch (`ActivityTop` / `ActivityBottom` →
            // `activity_cursor_top` / `_bottom`), which resolves first
            // under Right focus; these arms remain for direct
            // pane-driving callers (tests, tuirealm event path).
            (KeyCode::Char('g'), KeyModifiers::NONE) => {
                self.activity_cursor_top();
                PaneOutcome::Consumed
            }
            (KeyCode::Char('G'), m) if m.contains(KeyModifiers::SHIFT) => {
                self.activity_cursor_bottom();
                PaneOutcome::Consumed
            }
            // `Space` (and the vim-style `v`) toggle the focused
            // activity row into / out of the selection set. `Space`
            // is the discoverable convention surfaced in the hint
            // bar; `v` stays for muscle memory. `w` then consumes the
            // set (or the cursor row when it's empty) and spawns the
            // default agent with a pre-built "address these comments"
            // prompt.
            (KeyCode::Char(' '), KeyModifiers::NONE) | (KeyCode::Char('v'), KeyModifiers::NONE) => {
                if !workspace.activity.is_empty() {
                    let c = self.feed.cursor;
                    self.feed.toggle_select(c);
                }
                PaneOutcome::Consumed
            }
            // `d` (description) toggles the PR / issue body section
            // between collapsed (1-row header only) and expanded.
            // Renamed from `b` after the user flagged it as opaque
            // ("b for what — body, brief?") — `d` matches the label
            // shown in the help panel.
            (KeyCode::Char('d'), KeyModifiers::NONE) => {
                self.toggle_task_body();
                PaneOutcome::Consumed
            }
            // `w` (work-on-this). All decision logic — comments
            // selected vs. fix-CI vs. implement-issue vs. nothing —
            // lives in `crate::intent::resolve_work`, a pure function
            // with full (state, key) → Intent coverage in its own
            // tests. The handler just executes whichever Intent the
            // resolver hands back.
            (KeyCode::Char('w'), KeyModifiers::NONE) => {
                let mut selected: Vec<usize> = self.feed.selected().iter().copied().collect();
                selected.sort();
                let intent = crate::intent::resolve_work(
                    Some(workspace),
                    &selected,
                    &self.default_agent,
                    &self.conventions,
                );
                if let crate::intent::Intent::SpawnAgent {
                    workspace_key,
                    agent_id,
                    prompt,
                } = intent
                {
                    cmds.push(Command::Spawn {
                        model_alias: None,
                        access: lazybox_ipc::AgentRunAccess::Default,
                        session_key: workspace_key,
                        session_id: None,
                        client_request_id: None,
                        kind: lazybox_ipc::TerminalKind::Agent(agent_id),
                        cwd: None,
                        initial_prompt: prompt,
                        initial_snippet: None,
                        on_main: false,
                        // Activity-pane `w` continues a live conversation.
                        force_new: false,
                    });
                    self.feed.clear_selection();
                }
                PaneOutcome::Consumed
            }
            // `m` (mark the focused row read) is dispatched through
            // the catalog — see `Model::dispatch_action(MarkAllRead)`,
            // which calls `mark_cursor_row_read` when this pane is
            // focused and falls back to the workspace-wide mark
            // otherwise.
            _ => PaneOutcome::Pass,
        };

        // Cursor moves invalidate the previous undo target (you don't
        // get to undo a mark-read after navigating elsewhere — saves a
        // surprising "z reverts the comment two rows up" footgun) and
        // re-arm the timer with a fresh countdown so the previous
        // row's elapsed time doesn't carry over to the new one.
        if result == PaneOutcome::Consumed {
            self.last_marked_read = None;
            self.rearm_mark_timer_for_new_row(true);
        }
        result
    }

    pub fn on_event(&mut self, event: &Event) {
        // When the currently-selected workspace is upserted, refresh
        // our local copy so comment-cursor offsets stay in range.
        let Event::WorkspaceUpserted(workspace) = event else {
            return;
        };
        let Some(current) = self.workspace.as_ref() else {
            return;
        };
        if current.key == workspace.key {
            let prev_len = current.activity.len();
            let new_len = workspace.activity.len();
            let last = new_len.saturating_sub(1);
            self.workspace = Some((**workspace).clone());
            // Content may have changed (new comment, edited body) —
            // bump the buffer revision so the cache invalidates.
            self.refresh_activity_rev();
            self.feed.cursor = self.feed.cursor.min(last);
            if self.comment_scroll > last {
                self.comment_scroll = last;
            }
            // Activity is sorted newest-first; an inserted comment
            // shifts every existing index. Shift the cursor +
            // expanded + selected sets in lockstep so an expanded
            // card stays expanded across the 60s poll cycle.
            self.feed.adjust_for_length_change(prev_len, new_len);
        }
    }

    pub fn render(&mut self, area: Rect, frame: &mut Frame, focused: bool) {
        // Group-header row (issue #1442): no workspace, but a repo/Space
        // overview to paint. Takes the whole pane — the workspace-shaped
        // header/activity scaffold is meaningless here.
        if self.showing_overview() {
            self.render_overview(area, frame);
            return;
        }
        // Refresh the description-body render memo (#1031) before the
        // layout constraint and the body render both read it — otherwise
        // each re-parses the raw markdown every frame.
        self.refresh_task_body_cache();
        // Use ratatui's native layout solver to share the vertical
        // budget between the description (collapsible) and the
        // activity feed. `Constraint::Max(...)` lets the body shrink
        // gracefully when the right pane is short; `Constraint::Min(3)`
        // guarantees the activity feed always has at least its header
        // + 2 rows visible, no matter how long the PR description is.
        let body_constraint = self.task_body_constraint();
        // Baseline header is crumbs + pill/title + branch + reviewers;
        // an originating-issue line (#567) adds one row when present, and
        // a PR's diffstat line (#997) adds one more. Computed once and
        // threaded into `render_header` so the height reservation and the
        // emitted lines can't disagree.
        let origin = self.originating_issue();
        let show_diffstat = self
            .workspace
            .as_ref()
            .and_then(|w| w.primary_task())
            .is_some_and(|t| t.is_pr());
        let header_height = 4 + u16::from(origin.is_some()) + u16::from(show_diffstat);
        let chunks = Layout::vertical([
            Constraint::Length(header_height), // header (crumbs, pill, branch, [issue])
            Constraint::Length(1),             // separator
            body_constraint,                   // 0 / 1 / Max(N) for the body
            Constraint::Min(3),                // activity — never below 3 rows
        ])
        .split(area);

        self.render_header(chunks[0], frame, origin, show_diffstat);

        // Thin separator; accent-tinted while this pane has focus so
        // the active pane reads at a glance (#286) — same cue as the
        // sidebar / terminal dividers.
        let sep = Block::default()
            .borders(Borders::TOP)
            .border_style(crate::theme::current().pane_divider(focused));
        frame.render_widget(sep, chunks[1]);

        if chunks[2].height > 0 {
            self.render_task_body(chunks[2], frame);
        }

        let _ = self.render_activity(chunks[3], frame, focused);
    }

    /// Paint the repo / Space overview across the whole pane (issue
    /// #1442). Records each roster row's absolute screen row in
    /// `overview_hits` so a click can move the sidebar cursor onto that
    /// workspace.
    fn render_overview(&mut self, area: Rect, frame: &mut Frame) {
        let Some((lines, hits)) = self.overview.as_ref().map(|o| o.lines(area.width)) else {
            return;
        };
        self.overview_hits = hits
            .into_iter()
            .filter(|(idx, _)| (*idx as u16) < area.height)
            .map(|(idx, key)| (area.y + idx as u16, key))
            .collect();
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// Render the collapsed one-line summary shown in the pane's
    /// `Summary` mode: a slim count of the signals worth keeping in
    /// view (new activity, failing CI) plus how recently the task
    /// moved. The rest of the pane's rows go to the terminal below.
    pub fn render_summary(
        &self,
        area: Rect,
        frame: &mut Frame,
        now: chrono::DateTime<chrono::Utc>,
    ) {
        let line = self.summary_line(now);
        let para = Paragraph::new(crate::components::table::truncate_line(
            line,
            area.width as usize,
        ));
        frame.render_widget(para, area);
    }

    /// Build the `Summary`-mode line. Pure (takes `now`) so a render
    /// test can pin the clock and assert the text without a `Frame`.
    /// The leading `▸` echoes the collapsed-section glyph — it reads
    /// as "expand me". Counts come straight off the workspace; the
    /// relative-time trailer reuses the sidebar's `relative_time`.
    fn summary_line(&self, now: chrono::DateTime<chrono::Utc>) -> Line<'static> {
        use crate::components::sidebar::pills::relative_time;
        let theme = crate::theme::current();
        let sep = Span::styled("  ·  ", Style::default().fg(theme.chrome));
        let mut spans: Vec<Span<'static>> =
            vec![Span::styled("▸ ", Style::default().fg(theme.text_dim))];
        let Some(workspace) = self.workspace.as_ref() else {
            spans.push(Span::styled("no activity", theme.hint()));
            return Line::from(spans);
        };
        let unread = workspace.unread_count();
        if unread > 0 {
            spans.push(Span::styled(
                format!("{unread} new"),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                "no new activity",
                Style::default().fg(theme.text_dim),
            ));
        }
        if let Some(task) = workspace.primary_task() {
            if matches!(
                task.ci,
                lazybox_core::CiStatus::Failure | lazybox_core::CiStatus::Mixed
            ) {
                spans.push(sep.clone());
                spans.push(Span::styled(
                    "CI failing",
                    Style::default()
                        .fg(theme.error)
                        .add_modifier(Modifier::BOLD),
                ));
            }
            let rel = relative_time(task.updated_at, now);
            let trailer = if rel == "now" {
                "updated just now".to_string()
            } else {
                format!("updated {rel} ago")
            };
            spans.push(sep);
            spans.push(Span::styled(trailer, Style::default().fg(theme.text_dim)));
        }
        Line::from(spans)
    }

    /// Layout constraint for the task-body section. Three cases:
    /// - no body on the focused task → 0 rows.
    /// - body collapsed → 1 row (the `▶ Description` toggle hint).
    /// - body expanded → `Max(content + 1)` so ratatui's solver can
    ///   trim it when the pane is short (`Constraint::Min(3)` on the
    ///   activity row gets priority).
    fn task_body_constraint(&self) -> Constraint {
        if !self.has_task_body() {
            return Constraint::Length(0);
        }
        if !self.task_body_view.is_visible() {
            return Constraint::Length(1);
        }
        // Content + 1 for the `▼ Description` header line.
        let want = self.task_body_content_rows().saturating_add(1);
        Constraint::Max(want.max(2))
    }

    /// Estimate of the rendered body height (excluding the section
    /// header). Used by `task_body_constraint` to size the
    /// `Constraint::Max(...)` upper bound. The renderer itself caps
    /// at the same number so the body never overflows.
    fn task_body_content_rows(&self) -> u16 {
        let Some(body) = self.task_body_str() else {
            return 0;
        };
        // Preview caps at `task_body_max_rows`; a longer body is read
        // in the modal, not by growing the pane.
        let cap = match self.task_body_view {
            TaskBodyView::Collapsed => 0,
            TaskBodyView::Preview => self.task_body_max_rows as usize,
        };
        if cap == 0 {
            return 0;
        }
        // Width-aware count so wrapping affects it. `TASK_BODY_SIZE_WIDTH`
        // is a conservative default — actual render uses `area.width`,
        // which is always ≥ this for any practical terminal. `render`
        // primes this width in the cache (see `refresh_task_body_cache`),
        // so this normally reads the memo; the direct render is a
        // fallback for a `&self` caller that didn't run through `render`.
        let len = self
            .task_body_cache
            .by_width
            .iter()
            .find(|(w, _)| *w == TASK_BODY_SIZE_WIDTH)
            .map(|(_, lines)| lines.len())
            .unwrap_or_else(|| {
                crate::components::comment_render::render_body(body, TASK_BODY_SIZE_WIDTH, cap)
                    .len()
            });
        (len as u16).min(self.task_body_max_rows)
    }

    /// Rebuild the description-body render memo when the body changed,
    /// and pre-render the width-80 sizing pass the layout constraint
    /// reads — but only while the body is actually shown, so a collapsed
    /// description costs nothing. Content-addressed by the body text, so
    /// the cache can never serve stale content.
    fn refresh_task_body_cache(&mut self) {
        let changed = match self.task_body_str() {
            Some(body) => self.task_body_cache.body != body,
            None => !self.task_body_cache.body.is_empty(),
        };
        if changed {
            let body = self.task_body_str().map(str::to_owned).unwrap_or_default();
            self.task_body_cache.rich_modal = !body.is_empty() && body_wants_rich_modal(&body);
            self.task_body_cache.by_width.clear();
            self.task_body_cache.body = body;
        }
        if self.task_body_view.is_visible() && !self.task_body_cache.body.is_empty() {
            self.cache_body_at_width(TASK_BODY_SIZE_WIDTH);
        }
    }

    /// Fetch (or render + memoize) the uncapped body render at `width`.
    /// The cache holds at most the sizing width plus the current pane
    /// width — a resize drops the prior pane-width entry rather than
    /// letting `by_width` grow.
    fn cache_body_at_width(&mut self, width: u16) -> Rc<Vec<Line<'static>>> {
        if let Some((_, lines)) = self
            .task_body_cache
            .by_width
            .iter()
            .find(|(w, _)| *w == width)
        {
            return lines.clone();
        }
        self.task_body_cache
            .by_width
            .retain(|(w, _)| *w == TASK_BODY_SIZE_WIDTH || *w == width);
        let body = self.task_body_cache.body.clone();
        let lines = Rc::new(crate::components::comment_render::render_body(
            &body,
            width,
            usize::MAX,
        ));
        self.task_body_cache.by_width.push((width, lines.clone()));
        lines
    }

    fn has_task_body(&self) -> bool {
        self.task_body_str().is_some()
    }

    /// The primary task's own description, trimmed, when non-empty.
    fn primary_body_str(&self) -> Option<&str> {
        self.workspace
            .as_ref()?
            .primary_task()?
            .body
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// The description the inline teaser renders. The primary task's
    /// own body normally; but a PR with no description of its own falls
    /// back to a folded-in issue / Linear ticket's body (#1037) so the
    /// section — and its `d` reader affordance — never vanishes just
    /// because the PR body is blank. Without this, a Linear ticket
    /// collapsed into a PR workspace (#922) whose PR carried no body
    /// showed an empty Description even though the ticket's description
    /// was fetched and stored. The reader modal still lists every
    /// linked issue in full (see [`Self::task_body`]).
    fn task_body_str(&self) -> Option<&str> {
        self.primary_body_str()
            .or_else(|| self.first_linked_issue_body())
    }

    /// The first folded-in issue / ticket description on a PR workspace,
    /// in `gh_issues` → `linear_issues` order — the teaser fallback for a
    /// PR with no body of its own. Mirrors [`Self::linked_issue_tasks`]'s
    /// PR-gate and non-empty-body predicate, but short-circuits on the
    /// first match instead of allocating the full list: `task_body_str`
    /// (hence `has_task_body` / `has_visible_content` / the footer hints)
    /// runs it every frame.
    fn first_linked_issue_body(&self) -> Option<&str> {
        let ws = self.workspace.as_ref()?;
        ws.pr.as_ref()?;
        ws.gh_issues
            .iter()
            .chain(ws.linear_issues.iter())
            .find_map(|t| t.body.as_deref().map(str::trim).filter(|s| !s.is_empty()))
    }

    /// Render the focused task's body using the same lightweight
    /// markdown pipeline the activity feed uses. The first row is a
    /// `▼/▶ Description` header indicating collapse state; when
    /// expanded the body content follows, sized to whatever the
    /// layout solver gave us (so the activity feed always wins when
    /// the pane is short). Issues benefit most (the body IS the
    /// work brief); PR descriptions get the same treatment.
    fn render_task_body(&mut self, area: Rect, frame: &mut Frame) {
        let theme = crate::theme::current();
        if self.task_body_str().is_none() {
            self.click_hits.body_header_row = None;
            self.click_hits.body_more_row = None;
            self.body_overflows = false;
            return;
        }
        // First row of the section is the toggle header — clicks
        // here toggle `task_body_view` (Collapsed ⇄ Preview).
        self.click_hits.body_header_row = if area.height > 0 { Some(area.y) } else { None };

        // Build the body rows first so we know whether the reader modal
        // is the right next action *before* labelling the header — the
        // header's `(d · read full)` vs. `(d · collapse)` hint must match
        // what `d` actually does this frame.
        self.click_hits.body_more_row = None;
        self.body_overflows = false;
        let mut body_lines: Vec<Line<'static>> = Vec::new();
        if self.task_body_view.is_visible() {
            // Render at most `area.height - 1` body rows — anything
            // more would overflow the rect ratatui carved out for us.
            let body_rows = area.height.saturating_sub(1) as usize;
            if body_rows > 0 {
                // Uncapped render from the memo, then truncate here so we
                // control the trailer — `render_body`'s own `+N more
                // lines` row is inert; ours carries the read-full
                // affordance and gets registered as a click target. (The
                // cap only trims the tail, so rendering uncapped costs no
                // more work.)
                let rendered = self.cache_body_at_width(area.width.saturating_sub(2));
                if rendered.len() > body_rows {
                    self.body_overflows = true;
                    let kept = body_rows.saturating_sub(1);
                    let dropped = rendered.len() - kept;
                    body_lines.extend(rendered.iter().take(kept).cloned());
                    // The trailer is the next row (header + body so far) —
                    // clicking it (or a second `d`) opens the reader modal.
                    self.click_hits.body_more_row = Some(area.y + 1 + body_lines.len() as u16);
                    body_lines.push(more_lines_trailer(dropped, theme));
                } else {
                    body_lines.extend(rendered.iter().cloned());
                }
            }
        }

        // Two-state glyph: ▶ collapsed, ▼ preview. The reader modal is
        // the "read it all" path (via the `+N more` trailer or a second
        // `d`), so there's no inline "full" state. In Preview the `d`
        // hint reflects whether `d` will open the reader (body truncated,
        // or rich markdown the teaser degrades) or simply collapse. The
        // richness scan is read from the memo (#1031) rather than
        // rescanned per frame; `body_overflows` was just set above.
        let reader = self.body_overflows
            || self.task_body_cache.rich_modal
            || !self.linked_issue_tasks().is_empty();
        let (glyph, suffix) = match self.task_body_view {
            TaskBodyView::Collapsed => ("▶", "  (d · expand)"),
            TaskBodyView::Preview if reader => ("▼", "  (d · read full)"),
            TaskBodyView::Preview => ("▼", "  (d · collapse)"),
        };
        let header = Line::from(vec![
            Span::styled(
                format!("{glyph} Description"),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(suffix.to_string(), Style::default().fg(theme.text_dim)),
        ]);
        let mut lines = vec![header];
        lines.extend(body_lines);
        // No `Wrap`: `render_body` already wrapped every body line to
        // `area.width - 2`, so each `Line` maps to exactly one screen
        // row. That 1:1 mapping is what makes `body_more_row` above a
        // sound click target — a wrapping Paragraph re-measures by
        // display column (vs. `render_body`'s char count) and can re-wrap
        // a wide-unicode line or the un-pre-wrapped header, silently
        // pushing the trailer off its recorded row. Overlong lines clip
        // instead; the body is already width-fitted, so only the header /
        // trailer decoration clips, and only in a pane too narrow for it.
        let para = Paragraph::new(lines);
        // One-cell left margin so the body indents past the header's
        // crumbs + state pill — reads as "this belongs to the task
        // above" rather than as another full-width section.
        let inner = Rect {
            x: area.x.saturating_add(1),
            y: area.y,
            width: area.width.saturating_sub(1),
            height: area.height,
        };
        frame.render_widget(para, inner);
    }

    /// Toggle the description teaser (Collapsed ⇄ Preview), bound to
    /// `d` and the description-header click. From a Preview where the
    /// reader is the right next step — the body is truncated, or it's
    /// rich markdown the one-line teaser degrades — this instead opens
    /// the full body in the reader modal (#448) and folds the teaser
    /// away. A short, plain Preview that's fully shown just collapses.
    pub fn toggle_task_body(&mut self) {
        if self.task_body_view == TaskBodyView::Preview && self.wants_full_modal() {
            self.request_open_description();
            return;
        }
        self.task_body_view = self.task_body_view.cycle();
    }

    /// Whether the reader modal is the right action for the current body:
    /// it overflows the inline preview, or it contains block markdown the
    /// teaser renders poorly (fenced code, tables, images) and is worth
    /// reading in the full renderer even when it isn't truncated.
    ///
    /// `body_overflows` is render-derived (last frame); the richness scan
    /// is a pure function of the body, so a rich body is offered the
    /// reader even before a truncating render has run.
    fn wants_full_modal(&self) -> bool {
        // A PR with a linked issue always earns the reader modal: that's
        // where both descriptions are shown together (#462), even when
        // the PR's own body is short and plain.
        self.body_overflows
            || self.task_body_str().is_some_and(body_wants_rich_modal)
            || !self.linked_issue_tasks().is_empty()
    }

    /// Linked issues whose descriptions the reader modal appends after
    /// the PR's own (#462). Only for a PR workspace — the `x j` join /
    /// auto-collapse folds them into `gh_issues` / `linear_issues`, so
    /// they ride the same workspace and need no cross-workspace lookup.
    /// Issue-only workspaces return nothing: their primary *is* the
    /// issue, already shown.
    fn linked_issue_tasks(&self) -> Vec<&lazybox_core::Task> {
        let Some(ws) = self.workspace.as_ref() else {
            return Vec::new();
        };
        if ws.pr.is_none() {
            return Vec::new();
        }
        ws.gh_issues
            .iter()
            .chain(ws.linear_issues.iter())
            .filter(|t| {
                t.body
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|b| !b.is_empty())
            })
            .collect()
    }

    /// Queue the reader modal for the focused body and fold the inline
    /// teaser away — after the modal closes, the pane reads compact.
    fn request_open_description(&mut self) {
        if self.task_body().is_none() {
            return;
        }
        self.pending_open_description = true;
        self.task_body_view = TaskBodyView::Collapsed;
    }

    /// Drain a queued "open the full description" request. The
    /// orchestrator calls this after dispatching a key/click and, when
    /// set, mounts the reader modal with [`Self::task_body`] /
    /// [`Self::task_body_title`].
    pub fn take_open_description(&mut self) -> bool {
        std::mem::take(&mut self.pending_open_description)
    }

    /// The reader-modal source for the focused workspace. Normally the
    /// primary task's raw markdown body; when the primary is a PR with
    /// linked issue(s), each linked issue's description is appended as
    /// its own section, and clickable links to the PR and every linked
    /// issue are embedded so the modal surfaces both without leaving it
    /// (#462). The modal renders markdown links as click targets
    /// (`Msg::OpenUrl`), so the embedded `[#N ↗](url)` lines open in the
    /// browser.
    pub fn task_body(&self) -> Option<String> {
        // The primary's OWN body here (not `task_body_str`, which may
        // borrow a linked issue's body as a teaser fallback) — the
        // linked descriptions are appended as their own sections below,
        // so pulling the fallback in as the primary would double them.
        let primary_body = self.primary_body_str();
        let linked = self.linked_issue_tasks();
        if linked.is_empty() {
            return primary_body.map(str::to_string);
        }
        let primary = self.workspace.as_ref()?.primary_task()?;
        let mut out = format!(
            "[Open {} in browser ↗]({})\n",
            task_ref_label(primary),
            primary.url,
        );
        if let Some(body) = primary_body {
            out.push('\n');
            out.push_str(body);
            out.push('\n');
        }
        for issue in linked {
            let body = issue.body.as_deref().map(str::trim).unwrap_or_default();
            out.push_str(&format!(
                "\n---\n\n## Linked issue {label}\n\n[Open {label} in browser ↗]({url})\n\n{body}\n",
                label = task_ref_label(issue),
                url = issue.url,
            ));
        }
        Some(out.trim_end().to_string())
    }

    /// A concise title for the reader modal: the task key (e.g.
    /// `owner/repo#123`) plus its title when they differ.
    ///
    /// Normally the primary task. But when a PR contributes no body of
    /// its own and folds in exactly one issue / ticket, the reader's
    /// content *is* that issue's description (#1037) — so the header
    /// names it, not the empty-bodied PR that merely roots the
    /// workspace. A PR with its own body, or more than one linked issue,
    /// keeps the PR title: the reader is then a genuine composite the PR
    /// heads.
    pub fn task_body_title(&self) -> Option<String> {
        let ws = self.workspace.as_ref()?;
        let linked = self.linked_issue_tasks();
        let subject = match (self.primary_body_str(), linked.as_slice()) {
            (None, [only]) => *only,
            _ => ws.primary_task()?,
        };
        let key = subject.id.key.as_str();
        let title = subject.title.trim();
        if title.is_empty() || title == key {
            Some(key.to_string())
        } else {
            Some(format!("{key} · {title}"))
        }
    }
}

// `build_address_comments_prompt` retired — moved into
// `crate::intent` so the `w` resolver owns the prompt text. The
// right pane's `w` handler now calls `intent::resolve_work` and
// just executes the returned `Intent`.
