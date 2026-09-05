//! Declarative workspace-row layout for the sidebar.
//!
//! Replaces ~200 LoC of hand-rolled span-stitching + width tracking
//! that used to live inline in `Sidebar::render`. Defines the
//! workspace row as a sequence of typed columns + per-piece cell
//! builders; the table primitive (`components::table`) handles
//! geometry (column widths, padding, right-alignment, cursor fill).
//!
//! Each cell builder is a pure function of `&WorkspaceRowCtx` so
//! callers can unit-test individual pieces (the PR-number cell's
//! padding behavior, the status pill's row-style fallback, the
//! asking glyph's reserved width) without rendering a whole
//! sidebar.

use crate::components::sidebar::{
    badge_pill_style, role_badge, status_pills, workspace_type_label,
};
use crate::components::table::{Cell, Column, Row};
use crate::theme::Theme;
use lazybox_core::{Task, Workspace};
use ratatui::style::{Modifier, Style};
use ratatui::text::Span;

/// All state needed to render one workspace row. Built once per
/// row by the sidebar's render fn from `self` + `(visible_row, i)`.
/// Borrowed everywhere so we don't allocate for the typical case.
pub struct WorkspaceRowCtx<'a> {
    pub workspace: Option<&'a Workspace>,
    pub task: Option<&'a Task>,
    pub theme: &'a Theme,
    pub now: chrono::DateTime<chrono::Utc>,
    pub focused: bool,
    pub is_cursor: bool,
    /// This workspace is in the broadcast multi-select set (`v`).
    /// Renders a `✓` in the shared selection gutter; on the cursor row
    /// the cursor marker keeps the slot but stays accent so the row
    /// still reads as selected.
    pub is_selected: bool,
    /// Widest `#NNN` across all visible workspace rows in this
    /// render pass. Every row's pr-number cell pads to this width
    /// so the role / asking columns line up across rows.
    pub max_pr_num_width: usize,
    /// Any agent in this workspace is in `AgentState::InputNeeded`.
    /// Renders the `?` pill in the shared state slot. Mutually
    /// exclusive with `working` — input-needed wins if both were ever
    /// set (they can't be, by the disjoint asking/working sets).
    pub asking: bool,
    /// Any agent in this workspace is in `AgentState::LimitReached` — a
    /// provider usage / rate-limit block (#847). Renders the `⏳` pill in
    /// the shared state slot. Highest precedence: it's the most urgent
    /// "act (externally) before this moves" signal.
    pub limit_reached: bool,
    /// Any agent in this workspace is in `AgentState::AwaitingReset` — the
    /// calm auto-waiting block (lazybox pressed Wait; it's parked until the
    /// limit resets). Renders the quiet `💤` glyph. Lower precedence than
    /// the alerting states and than `working`: it's handled, nothing to act
    /// on, so an actively working sibling wins the slot.
    pub awaiting_reset: bool,
    /// Any agent in this workspace is waiting on a provider credit chooser.
    pub credit_exhausted: bool,
    /// Any agent in this workspace is in `AgentState::Working`
    /// (streaming / running a tool). Renders the animated spinner in
    /// the same slot the `?` pill uses.
    pub working: bool,
    /// Any agent in this workspace is in `AgentState::Done` — finished
    /// its turn, waiting to be looked at (#80). Renders `✓` in the same
    /// slot. Mutually exclusive with `asking`/`working` upstream;
    /// asking and working take precedence defensively if both were set.
    pub done: bool,
    /// The workspace's agent process has exited (`AgentState::Exited` —
    /// clean or crash; #356/#357). Renders `✗` in the same slot so a dead
    /// agent reads as "ended, restart it" instead of blanking to nothing.
    /// Lowest precedence: a live signal (asking/working/done) always wins.
    pub exited: bool,
    /// Current spinner glyph for the `working` slot. Shared across all
    /// rows in a render pass — the sidebar advances a single frame
    /// counter on a low-rate tick (see `Sidebar::tick_working`), so
    /// the animation costs one glyph lookup per working row, no
    /// per-tick row rebuild.
    pub working_glyph: &'static str,
    /// This workspace is provisioning its first spawn (cloning, worktree,
    /// setup, launching the agent) and no terminal has reported an
    /// `AgentState` yet (#1069). Renders the animated "spawning" arc in
    /// the shared state slot so the row reads as *coming up* rather than
    /// blank until the agent is live. Yields to every live agent signal
    /// (`working` / `done` / `asking` / `limit_reached`) — a second
    /// session running beside the spawn keeps its glyph — and outranks
    /// only the terminal `exited` marker: re-spawning a crashed agent,
    /// whose sticky `Exited` (#356) lingers with no live terminal, shows
    /// the arc rather than a stale ✗. See `cell_state`.
    pub spawning: bool,
    /// Current glyph for the `spawning` slot — a rotating arc, sharing
    /// the same frame counter as `working_glyph` but a distinct frame set
    /// so a starting-up row reads differently from a running one.
    pub spawning_glyph: &'static str,
    /// `Sidebar::runner_badges(key)` — `[('C', n), ('S', m)]` etc.
    pub badges: Vec<(char, usize)>,
    /// `Sidebar::agent_models(key)` — the model + effort label to show
    /// beside a single agent badge (`[('C', "Opus")]`,
    /// `[('X', "gpt-5.5 · xhigh")]`). The label is abbreviated to a `◆O`
    /// glyph at render (#1068). Empty when `ui.show_agent_model` is off,
    /// when no model is known, or when a badge collapses two agents.
    pub agent_models: Vec<(char, String)>,
    /// This workspace's 1-based jump number — its slot in the
    /// sidebar-order focused roster (`Sidebar::numbered_workspace_keys`).
    /// `Some` only for focused (starred) workspaces; rendered as a small
    /// badge ahead of the agent pill so the user can see which
    /// `]]<digit>` lands here. `None` for unfocused rows (and for the
    /// 10th focused workspace onward, which has no single-digit jump).
    pub agent_number: Option<usize>,
    /// Render the type indicator as plain ASCII (`p`/`i`/`l`) instead
    /// of the default unicode glyphs (`⇄`/`○`/`◆`). Wired from
    /// `display.ascii_glyphs` in `~/.lazybox/config.yaml`.
    pub ascii_glyphs: bool,
    /// This workspace has "auto-merge on green" armed
    /// (`Workspace::auto_merge_on_green`). Renders a distinct `⚡` glyph
    /// (#1046) ahead of the status glyphs so the user can see, at a
    /// glance, which rows will merge themselves once CI goes green.
    pub auto_merge_armed: bool,
    /// GitHub-native auto-merge is enabled on the PR
    /// (`Task::auto_merge_enabled`). Renders a distinct `◆` policy glyph
    /// alongside `⚡` — it's a standing automation *policy*, not a task
    /// status, so it lives here instead of the status column and never
    /// hides the `✗` CI-fail glyph on an armed PR (#778).
    pub auto_merge_enabled: bool,
    /// This workspace has CI-failure auto-fix explicitly armed.
    pub auto_fix_ci_armed: bool,
    /// This workspace has merge-conflict auto-fix explicitly armed.
    pub auto_fix_conflict_armed: bool,
    /// This workspace has "track main" armed (`Workspace::track_main` —
    /// issue #535). Renders a `⤓` glyph so the user can see which rows the
    /// daemon keeps fast-forwarded to the default branch.
    pub track_main: bool,
    /// The tracked workspace is behind `origin/<default>` and couldn't be
    /// auto-synced (`Workspace::track_main_behind`). Flips the track-main
    /// `⤓` glyph to its warn color so a stuck (dirty/diverged) worktree
    /// reads at a glance. Only meaningful when `track_main`.
    pub track_main_behind: bool,
    /// This workspace is metered (`Workspace::metered`, toggled with
    /// `x $`): its agent spawns are routed through lazybox's local metering
    /// proxy so cost and tokens accrue per session (#1488). Renders a `$` in
    /// the passive badge cluster — the *durable* cue that a canary is armed.
    /// Before this, the only per-workspace signal was a ` $ METER ` pill in
    /// the sidebar header, drawn from the focused row alone: you could not
    /// see which workspaces were metered without visiting each one.
    pub metered: bool,
    /// This workspace carries a non-empty local note
    /// (`Workspace::has_notes` — issue #458). Renders a small ` ✎ ` pill
    /// so the user can see, at a glance, which rows have a scratchpad.
    pub has_notes: bool,
    /// Total snippets delivered to this workspace's agent
    /// (`Workspace::sent_snippets.total()` — issue #463): a monotonic
    /// count of every delivery, not the size of the capped MRU. Renders a
    /// dim ` ]N ` pill; `0` renders nothing.
    pub sent_snippet_count: usize,
    /// Visible ticket-tree placement. `None` for rows outside a hierarchy;
    /// roots with children still carry metadata so they get a disclosure.
    /// The row's source is Quiet / Digest / Muted and the row does NOT
    /// punch through (#scale): ambient badges (the unread pill) are
    /// suppressed so a demoted source stops shouting.
    pub source_quiet: bool,
    /// An event-conditional snooze fired within `WOKE_WINDOW` (#scale,
    /// B4): render the wake glyph in the shared state slot so the
    /// re-entry is announced. Yields to every live agent signal.
    pub recently_woken: bool,
    pub ticket_tree: Option<lazybox_tui_core::inbox::TicketTreeMeta>,
    /// This workspace's PR is part of a detected stack (issue #969) — its
    /// [`StackPosition`](lazybox_core::StackPosition). Renders a ` ⇗k/N `
    /// badge so a chain of stacked PRs reads as an ordered stack at a
    /// glance rather than unrelated rows. `None` for standalone PRs.
    pub stack: Option<&'a lazybox_core::StackPosition>,
    /// Tier `(badge_letter, label) → short` map for the model badge
    /// (`('C', "Opus") → "O"`), aggregated from every agent's model menu.
    /// The badge reads a declared short here and falls back to the label's
    /// first character when a key is absent (#1068). Keyed by the agent's
    /// badge letter so two agents sharing a tier label keep distinct
    /// shorts. Sourced from `Sidebar::model_shorts`.
    pub model_shorts: &'a std::collections::HashMap<(char, String), String>,
    /// Active search term to highlight within this row's title, or `None`
    /// when no search touches this row. `Some` underlines the matched span
    /// so the user can see *what* matched — the vim `/pattern` cue (#1099).
    /// Already `#`-stripped and trimmed by the caller.
    pub highlight_query: Option<&'a str>,
    /// Source group label to render as a dim `repo · ` prefix ahead of the
    /// title (#1450). `Some` only for rows in the synthetic `★ Focused`
    /// section, which are lifted out of their repo group and so carry no
    /// repo header to say where they came from; the label is the same one
    /// [`group_label`](lazybox_tui_core::inbox::group_label) gives the row's
    /// repo header elsewhere. `None` for rows shown under their own header.
    pub repo_prefix: Option<String>,
}

impl<'a> WorkspaceRowCtx<'a> {
    /// Cursor row background. Drives `Row::fill_style` so every
    /// column's padding inherits the highlight bg — without this
    /// the cursor row looked broken (highlight stopping mid-row).
    pub fn row_style(&self) -> Style {
        if self.is_cursor && self.focused {
            self.theme.row_focused()
        } else if self.is_cursor {
            self.theme.row_unfocused()
        } else {
            Style::default()
        }
    }

    fn raw_title(&self) -> &'a str {
        self.task
            .map(|t| t.title.as_str())
            .unwrap_or_else(|| self.workspace.map(|w| w.name.as_str()).unwrap_or("?"))
    }

    /// An issue that's been open long enough to read in months (`Nmo`
    /// in the time column). Age is measured from when it was opened, not
    /// last touched, so an old issue with recent chatter still reads as
    /// old. PRs are excluded — they carry their own staleness cues (CI,
    /// review, conflict pills) and a fade would fight those. Stale issues
    /// get a dim title so active rows stand out and old ones don't waste
    /// a second glance (issue #274).
    fn is_stale_issue(&self) -> bool {
        self.task.is_some_and(|t| {
            !t.is_pr() && lazybox_core::time::is_stale_at(&t.opened_at(), self.now)
        })
    }
}

/// Column spec for every workspace row in the current render pass.
/// Built once (with `max_pr_num_width` from the pre-pass), and shared by
/// a SINGLE `render_table` call that takes every visible workspace row
/// at once — that's what lets `Column::max(0)` line up across rows
/// (each Max column expands to the widest natural cell across the whole
/// table, and collapses to 0 when no row has content).
///
/// Order (left → right):
///
/// 0. Prefix — `▶` (cursor) / ` ` (no cursor). A single shared
///    selection gutter: the marker occupies one column reused across
///    every row type, instead of a 2-col marker re-added at each depth
///    (issue #231). Rows sit one column in from the repo header's
///    disclosure arrow, so the tree nesting still reads.
/// 1. Type glyph — `⇄` / `○` / `◆` (or ASCII `p`/`i`/`l`) / blank,
///    followed by a single space separator (2 cells total) so the
///    row reads `⇄ 312` rather than the cramped `⇄312` — see issues
///    #42 and #94.
/// 2. PR number — `NNN` (no `#` prefix; the glyph carries the type —
///    issue #67), left-aligned and padded to `max_pr_num_width` so the
///    digits sit one space off the type glyph (`⇄ 312`, `○ 7`) on every
///    row. Right-aligning instead pushed shorter numbers off the glyph
///    with leading padding (`⇄ 312` vs `○   7`) — the inconsistent
///    post-glyph spacing of issue #65. Trailing padding still aligns
///    the role column to a fixed x across rows.
/// 3. Role badge — ` R` colored marker, or blank.
/// 4. State slot — ` ? ` (input-needed, warn) / ` ⠋ ` (working,
///    animated accent spinner) / blank (idle). One slot, three
///    mutually-exclusive states; reserved width so the title to the
///    right doesn't jitter as a row changes state.
/// 5. Title — flex, absorbs the remaining width. Truncates with `…`.
///    Conventional-commit / bracket tags like `[CI]` stay inline at
///    the front of the title rather than being hoisted into a
///    reserved column that every tag-less row would pay for (#80). The
///    task's labels (` [bug] [ci] +2`) ride at the tail of this same
///    cell — as an atomic droppable group — instead of a reserved
///    column. A global `Max` label column made every row, tag-less
///    ones included, reserve the widest label cell anywhere in the
///    sidebar, so one Dependabot repo's ` [deps] [go]` chips truncated
///    unrelated tag-less titles (#329). Inline, a label-less row hands
///    all that width to its title; the chips are excluded from the
///    flex's protected floor, so under width pressure they shed whole
///    (after the status pill — #328) before the title elides.
/// 6. Unread pill — ` ●N `, right-aligned. Max so the column collapses
///    when no row has unread, and lines up at a consistent x when any
///    row does.
/// 7. Badge: agent slot — ` C ` / ` C×2 ` / ` CX ` / blank. Same
///    Max semantics. A single agent's model rides here as a compact
///    `◆O` tier badge (#803, abbreviated to one glyph — #1068), so even
///    a verbose `gpt-5.6-sol · xhigh` shrinks to `◆g ·xhi` and can't
///    anchor this Max column table-wide.
/// 8. Badge: shell slot — ` S ` / blank. Cell carries a leading space
///    so the two badges visually separate when both present.
/// 9. Passive-info badge cluster — one right-aligned, Max-collapsing
///    column packing the low-signal badges: `⎇ local` (linked checkout),
///    `✎` (has notes), `]N` (snippet count), `⤓main`/`behind` (track-main,
///    #535), `FIX` (auto-fix armed). These used to own one anchored column
///    each (#524), which reserved the *sum* of every badge type's widest
///    cell on every row — a wide, gap-ridden trailer even though the badges
///    rarely co-occur. Packed into one cluster (#813) the column only
///    reserves the widest single row's cluster, reclaiming the interior
///    gaps for the title. Vertical alignment of individual badges is traded
///    away for that density.
/// 10. Merge-arm badge cluster — a second right-aligned, Max-collapsing
///    column for the two "merge when green" arms: `ARM` (lazybox
///    merge-on-green) and `AUTO` (GitHub-native auto-merge, #778). Split
///    from the passive-info cluster so it keeps a *higher* drop priority
///    (`P_ARMS` > `P_BADGES`): the arms that decide whether the PR merges
///    itself survive under width pressure after the low-signal decoration
///    has shed, exactly as the per-badge priorities did before the pack
///    (#813). Sits rightmost of the badges, nearest the status pill.
/// 11. Status pill — ` MERGED ` / ` REVIEW  CI FAIL ` / blank.
///    Right-aligned, sized to the pills actually present (each pill is
///    trimmed to its own ` LABEL ` block — no blank-slot filler), so a
///    lone CI pill sits one clean gap off the time. Cell is empty
///    (width 0) when both review + CI pills are None, so the column
///    collapses for an all-empty table.
/// 12. Time — ` Xm` / ` Xh` / ` Xd`, right-aligned. Leading space is
///    baked into the cell so a 1-cell gap separates time from
///    whatever sits to its left (status pill or, when status is
///    empty, the title flex padding).
pub fn build_columns(max_pr_num_width: usize) -> Vec<Column> {
    // Drop order when the sidebar is too narrow to fit every column:
    // lower priority sheds first. The issue number + title (and the
    // type glyph that tells issue-from-PR) are kept. Labels are the
    // least important thing on the row; they ride in the title cell as
    // an atomic tail (excluded from the flex floor), so they shed
    // before any of these columns — and, crucially, before the status
    // pill (CI / CONFLICT — the actionable signal), which is kept
    // nearly as long as the title (issue #328).
    const P_TIME: u8 = 10;
    // The badges pack into two priority-tiered clusters (#813) rather than
    // seven per-badge columns (#524), keeping graduated shedding without the
    // per-column reserved gaps. The passive-info cluster (linked / notes /
    // snippet / track / fix) is low-signal decoration, so it sheds first —
    // right after the timestamp (which #328 keeps as the first trailer to
    // go, `P_TIME` below `P_UNREAD`).
    const P_BADGES: u8 = 20;
    // The merge-arm cluster (`⚡`/`◆`) is now a one-glyph icon per arm
    // (#1046), not the old five-cell ` ARM ` / ` AUTO ` blocks — so it costs
    // almost nothing to keep. #813 had it at 21 (just above the passive
    // decoration), where on a normal-width row carrying an agent badge, a
    // model label and a CI pill the five-cell block was among the first
    // columns shed: the "this PR will merge itself" signal silently vanished
    // (#1046). Raised above the unread count so the arm icons outlive the
    // columns that were squeezing them out; they still yield to the higher
    // signals (shell / agent / role / state) and the CI-status pill.
    const P_ARMS: u8 = 35;
    const P_UNREAD: u8 = 30;
    const P_BADGE_SHELL: u8 = 40;
    const P_BADGE_AGENT: u8 = 50;
    const P_ROLE: u8 = 60;
    const P_STATE: u8 = 70;
    const P_STATUS: u8 = 80;
    // The title should keep at least this many cells before any secondary
    // column is allowed to crowd it out — below this a title is just a word
    // fragment + `…` and tells you nothing. On a normal-width row the title
    // reads in full because its flex absorbs all the slack the capped model
    // label (#813) and the packed badge cluster (#813) no longer reserve;
    // this floor only governs the narrow-width fight, where it stays low
    // enough that the agent badge (which agent is running — a primary
    // signal) survives rather than being evicted to show a longer title.
    const TITLE_MIN: usize = 20;
    vec![
        Column::fixed(1),                          // 0: prefix (shared 1-col caret gutter)
        Column::fixed(2),                          // 1: type glyph + trailing space separator
        Column::fixed(max_pr_num_width), // 2: pr_num (left-aligned, one space off the glyph)
        Column::fixed(2).priority(P_ROLE), // 3: role (" R" or blank)
        Column::fixed(3).priority(P_STATE), // 4: state slot (" ? "/" ⠋ "/blank, reserved)
        Column::flex(TITLE_MIN),         // 5: title (labels ride inline at its tail)
        Column::max(0).right().priority(P_UNREAD), // 6: unread
        Column::max(0).priority(P_BADGE_AGENT), // 7: badge_agent (+ capped model label)
        Column::max(0).priority(P_BADGE_SHELL), // 8: badge_shell (carries its own leading space)
        Column::max(0).right().priority(P_BADGES), // 9: passive-info badge cluster (#813)
        Column::max(0).right().priority(P_ARMS), // 10: merge-arm badge cluster (#813)
        Column::max(0).right().priority(P_STATUS), // 11: status (CI / review pills)
        Column::max(0).right().priority(P_TIME), // 12: time (carries its own leading space)
    ]
}

/// Build the `Row<Cell>` for a single workspace row. Fill style is
/// the row's cursor highlight (or unstyled when not under cursor),
/// applied via `Row::fill` so every column's padding inherits the
/// row's bg.
pub fn build_row(ctx: &WorkspaceRowCtx<'_>) -> Row {
    let cells = vec![
        cell_prefix(ctx),
        cell_type(ctx),
        cell_pr_num(ctx),
        cell_role(ctx),
        cell_state(ctx),
        cell_title(ctx),
        cell_unread(ctx),
        cell_badge_agent(ctx),
        cell_badge_shell(ctx),
        cell_badges(ctx),
        cell_merge_arms(ctx),
        cell_status(ctx),
        cell_time(ctx),
    ];
    Row::new(cells).fill(ctx.row_style())
}

fn cell_prefix(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    // A selected row shows `✓` even under the cursor. The cursor already
    // reads through the full-row highlight (`row_style`), so letting the
    // caret win this single-cell gutter would hide the mark on the very
    // row you just pressed `v` on — the "no immediate feedback" bug
    // (issue #786). Selection wins the glyph; cursor keeps the highlight.
    let s = if ctx.is_selected {
        "✓"
    } else if ctx.is_cursor {
        "▶"
    } else {
        " "
    };
    let style = if ctx.is_cursor || ctx.is_selected {
        ctx.row_style()
            .fg(ctx.theme.accent)
            .add_modifier(Modifier::BOLD)
    } else {
        ctx.row_style()
    };
    Cell::from_span(Span::styled(s, style))
}

fn cell_type(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let Some(workspace) = ctx.workspace else {
        return Cell::empty();
    };
    let Some(glyph) = workspace_type_label(workspace, ctx.ascii_glyphs) else {
        return Cell::empty();
    };
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        // Color the glyph by source so PR / GitHub issue / Linear are
        // distinguishable at a glance — they used to share one dim grey,
        // which hid the Linear `◆` entirely. Mirrors the section-header
        // markers (PR → success, issue → hover) and gives Linear the
        // accent tone. The branch order matches `workspace_type_label`,
        // so if a glyph rendered, exactly one arm matches; the final
        // arm is Linear (the only other glyph-bearing kind).
        let color = if workspace.pr.is_some() {
            ctx.theme.success
        } else if !workspace.gh_issues.is_empty() {
            ctx.theme.hover
        } else {
            ctx.theme.accent
        };
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    };
    // Glyph + a single trailing space so the row reads `⇄ 312`
    // instead of the cramped `⇄312` (issue #94); the space separator
    // also keeps the `#`-less number readable (issues #42, #67).
    // Both spans borrow `&'static str` (no per-frame allocation on the
    // hot path); the trailing space takes the row fill style like the
    // other inter-cell separators.
    Cell::new(vec![
        Span::styled(glyph, style),
        Span::styled(" ", ctx.row_style()),
    ])
}

fn cell_pr_num(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let Some(task) = ctx.task else {
        return Cell::empty();
    };
    // A GitHub key shows its `NNN`; a tracker key (Linear `ENG-123`,
    // Jira `PROJ-42`) shows the identifier itself — the only handle a
    // user has on those rows.
    let Some(label) = crate::components::task_label::task_identifier(task) else {
        return Cell::empty();
    };
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default()
            .fg(crate::components::task_label::identifier_color(task))
            .add_modifier(Modifier::BOLD)
    };
    // No `#` prefix: the type glyph in the column to the left already
    // says "issue" (`○`) or "PR" (`⇄`), so the `#` was redundant and
    // cost a column on every row (issue #67). We emit just the `NNN`
    // span; the column is Fixed(max_pr_num_width) and LEFT-aligned, so
    // the renderer pads the deficit on the RIGHT with the row's
    // fill_style. Left alignment keeps the number one space off the
    // type glyph on every row (issue #65) — a right-aligned column
    // padded shorter numbers on the left, opening an inconsistent gap
    // after the glyph. `pr_number_color` colors the digits.
    Cell::from_span(Span::styled(label, style))
}

fn cell_role(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let Some(task) = ctx.task else {
        return Cell::empty();
    };
    // A Linear ticket only reaches the inbox when it's assigned to or
    // created by the token's viewer (`A`/`@`), so anything else is the
    // "why is this here?" anomaly (#1015) — a wrong token identity, a
    // stale daemon, or a subscribed scope. Flag it in warn (`?`) so it
    // stands out while scanning, instead of the quiet dim `·` a benign
    // GitHub mention gets; mirrors the detail pane's "not assigned to or
    // created by you" line. GitHub roles keep their usual badge.
    let (letter, color) = if task.id.source == lazybox_core::LINEAR_SOURCE
        && !matches!(
            task.role,
            lazybox_core::TaskRole::Author | lazybox_core::TaskRole::Assignee
        ) {
        ('?', ctx.theme.warn)
    } else {
        role_badge(ctx.theme, task.role)
    };
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    };
    // " R" — leading space separator + colored letter. Reads
    // cleaner than `7204R` (which scanned as one weird token).
    Cell::new(vec![
        Span::styled(" ", ctx.row_style()),
        Span::styled(letter.to_string(), style),
    ])
}

/// The shared per-session state slot: a single 3-cell column that
/// renders the agent's current `AgentState` with a distinct visual
/// per state, so it's one thing to scan:
///   - `InputNeeded` → ` ? ` (warn, bold) — a static glyph: the
///     agent is paused waiting on me.
///   - `Working`     → ` <spinner> ` (accent, bold) — an animated
///     glyph: the agent is making progress right now.
///   - `Done`        → ` ✓ ` (success, bold) — a static glyph: the
///     agent finished its turn and is waiting to be looked at (#80).
///   - `LimitReached`→ ` ⏳ ` (warn, bold) — a static glyph: the agent
///     hit its provider usage limit and is waiting to be resumed (#847).
///   - `CreditExhausted` → ` ¢ ` (warn, bold) — the provider credit
///     recovery transaction has not completed yet.
///   - `Spawning`    → ` <arc> ` (dim) — an animated glyph: the workspace
///     is provisioning (clone / worktree / launch) and the agent is
///     *coming*, before any terminal reports state (#1069). A distinct
///     spinner from `Working` so "starting up" doesn't read as "running".
///   - `Exited`      → ` ✗ ` (dim) — a static glyph: the agent process
///     ended (clean or crash; #356/#357). Not an alert color — a dead
///     agent is a fact to notice, not an emergency.
///   - `Idle`        → blank.
///   - `AwaitingReset` → ` 💤 ` (dim) — a static glyph: lazybox pressed
///     Wait and the agent is parked, sleeping until its limit resets. Calm,
///     not an alert — nothing for you to do.
/// Reserved width either way so the kind/title to the right don't
/// jitter as a row moves between states. Precedence credit-exhausted >
/// limit-reached > asking > working > awaiting-reset > done > spawning > exited. `spawning` yields to
/// every *live* signal and outranks only the terminal `exited` marker.
/// That split is exact, not defensive: a terminal's `Working` / `Done` /
/// `InputNeeded` / `LimitReached` / `CreditExhausted` entry is dropped when it exits (only
/// `Exited` is retained — the sidebar's `TerminalExited` handler), so any
/// of those present while `spawning` is set belongs to a genuinely live
/// *sibling* terminal (a second session running alongside this spawn) and
/// rightly wins. `Exited` is the lone exception: retained as the #356
/// restart affordance, it lingers with no live terminal after a crash, so
/// on a cold re-provision `spawning > exited` shows the "coming up" arc
/// instead of stranding a stale ✗ over an agent that is restarting.
fn cell_state(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let (glyph, fg) = if ctx.credit_exhausted {
        ("¢", ctx.theme.warn)
    } else if ctx.limit_reached {
        ("⏳", ctx.theme.warn)
    } else if ctx.asking {
        ("?", ctx.theme.warn)
    } else if ctx.working {
        (ctx.working_glyph, ctx.theme.accent)
    } else if ctx.awaiting_reset {
        // The calm auto-waiting block: parked until reset, handled — a quiet
        // 💤 in the dim text color, NOT an alert. Below `working` so a live
        // sibling's spinner wins; above `done` so a still-parked agent shows
        // over a merely-finished one.
        ("💤", ctx.theme.text_dim)
    } else if ctx.done {
        ("✓", ctx.theme.success)
    } else if ctx.spawning {
        (ctx.spawning_glyph, ctx.theme.text_dim)
    } else if ctx.exited {
        ("✗", ctx.theme.text_dim)
    } else if ctx.recently_woken {
        // Announced re-entry (#scale, B4): the snooze's wake condition
        // fired. Single-width glyph on purpose — emoji here would
        // shear the column grid. Lowest precedence: any live agent
        // signal outranks the announcement.
        (if ctx.ascii_glyphs { "w" } else { "↺" }, ctx.theme.accent)
    } else {
        return Cell::empty();
    };
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default().fg(fg).add_modifier(Modifier::BOLD)
    };
    // Reserved 3 cells: " G " (leading + glyph + trailing space).
    // `glyph` is `&'static str` and the spaces are literals, so every
    // span borrows static data — no per-row, per-frame allocation on
    // the render hot path.
    Cell::new(vec![
        Span::styled(" ", style),
        Span::styled(glyph, style),
        Span::styled(" ", ctx.row_style()),
    ])
}

/// Spinner frames for the "working" state slot. A small braille
/// cycle — visually distinct from the static `?` input-needed glyph
/// and cheap to render. `working_glyph` indexes this by the sidebar's
/// shared frame counter.
pub(crate) const WORKING_SPINNER_FRAMES: &[&str] =
    &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Resolve the spinner glyph for a given frame index. Wraps, so the
/// caller's counter can grow unbounded.
pub(crate) fn working_glyph(frame: usize) -> &'static str {
    WORKING_SPINNER_FRAMES[frame % WORKING_SPINNER_FRAMES.len()]
}

/// Spinner frames for the "spawning" state slot (#1069) — a rotating
/// arc, deliberately distinct from the `Working` braille cycle so a row
/// that is *coming up* (cloning / worktree / launching the agent) reads
/// differently from one actively running.
pub(crate) const SPAWNING_SPINNER_FRAMES: &[&str] = &["◜", "◠", "◝", "◞", "◡", "◟"];

/// Resolve the spawning arc glyph for a given frame index. Wraps, so the
/// shared frame counter can grow unbounded.
pub(crate) fn spawning_glyph(frame: usize) -> &'static str {
    SPAWNING_SPINNER_FRAMES[frame % SPAWNING_SPINNER_FRAMES.len()]
}

fn cell_title(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    // The full title, tags and all. Bracketed tags like `[CI]` stay
    // where they originated instead of being hoisted into a reserved
    // column (#80). No truncation here — the table renderer trims with
    // `…` when the flex column ends up smaller than the cell's natural
    // width.
    //
    // Stale issues fade (DIM) so the eye skips them — but never on the
    // cursor row, whose highlight fill must stay legible.
    let mut style = ctx.row_style();
    if !ctx.is_cursor
        && (ctx.is_stale_issue() || ctx.ticket_tree.is_some_and(|meta| meta.context_only))
    {
        style = style.add_modifier(Modifier::DIM);
    }
    // Labels ride at the tail of the title cell rather than in a
    // reserved column (#329): a tag-less row hands all that width to
    // its title. Marked as the cell's atomic tail so they shed as one
    // unit — after the status pill (#328), never sliced mid-chip —
    // when the row is too narrow (see `Cell::atomic_tail`).
    let labels = label_spans(ctx);
    let tail = labels.len();
    // A `★ Focused` row is lifted out of its repo group, so it has no repo
    // header to say where it came from — name the source inline (#1450).
    // Dim so it reads as a cue rather than competing with the title, but
    // legible (no forced dim) on the cursor row, mirroring the title and
    // the tree prefix. It leads the cell as an atomic head so a narrow
    // pane sheds it whole rather than truncating the title behind it.
    let mut spans: Vec<Span<'static>> = Vec::new();
    let head = if let Some(repo) = &ctx.repo_prefix {
        let prefix_style = if ctx.is_cursor {
            ctx.row_style()
        } else {
            ctx.row_style().fg(ctx.theme.text_dim)
        };
        spans.push(Span::styled(format!("{repo} · "), prefix_style));
        1
    } else {
        0
    };
    spans.extend(ticket_tree_prefix(ctx));
    spans.extend(title_spans(
        ctx.raw_title(),
        ctx.highlight_query,
        style,
        ctx.theme,
    ));
    spans.extend(labels);
    Cell::new(spans).atomic_tail(tail).atomic_head(head)
}

/// Split a title into styled spans, underlining the first case-insensitive
/// occurrence of the active search term so the user can see what matched
/// (the vim `/pattern` cue, #1099). With no query — or no contiguous match
/// (a purely fuzzy/metadata hit) — the title is one plain span, unchanged.
fn title_spans(
    title: &str,
    query: Option<&str>,
    style: Style,
    theme: &Theme,
) -> Vec<Span<'static>> {
    let Some(range) = query.and_then(|q| ci_match_range(title, q)) else {
        return vec![Span::styled(title.to_string(), style)];
    };
    let hl = style
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
    let mut spans = Vec::with_capacity(3);
    if range.start > 0 {
        spans.push(Span::styled(title[..range.start].to_string(), style));
    }
    spans.push(Span::styled(title[range.clone()].to_string(), hl));
    if range.end < title.len() {
        spans.push(Span::styled(title[range.end..].to_string(), style));
    }
    spans
}

/// Byte range of the first case-insensitive occurrence of `needle` in
/// `hay`, expressed in `hay`'s ORIGINAL byte offsets, or `None`.
///
/// The match is found in lowercased space, but `to_lowercase()` can shift
/// byte offsets (non-ASCII case-folds grow or shrink, and per-char shifts
/// can even cancel to an equal total length while skewing interior
/// boundaries). So the lowercased offsets are validated against the
/// original before use: they must land on real char boundaries AND the
/// original slice must itself case-fold back to the needle. When they
/// don't, we skip the highlight rather than slice mid-codepoint — which
/// would panic in this render path on an attacker-chosen title.
fn ci_match_range(hay: &str, needle: &str) -> Option<std::ops::Range<usize>> {
    if needle.is_empty() {
        return None;
    }
    let needle_lower = needle.to_lowercase();
    let hay_lower = hay.to_lowercase();
    let start = hay_lower.find(&needle_lower)?;
    let end = start + needle_lower.len();
    (hay.is_char_boundary(start)
        && hay.is_char_boundary(end)
        && hay[start..end].to_lowercase() == needle_lower)
        .then_some(start..end)
}

/// Compact tree prefix inside the flexible title column. Depth is capped so
/// malformed or unusually deep provider data cannot consume the entire
/// title; deeper descendants retain an ellipsis marker and the final three
/// indentation steps.
fn ticket_tree_prefix(ctx: &WorkspaceRowCtx<'_>) -> Vec<Span<'static>> {
    let Some(meta) = ctx.ticket_tree else {
        return Vec::new();
    };
    if meta.depth == 0 && !meta.has_children {
        return Vec::new();
    }
    const MAX_VISIBLE_DEPTH: usize = 4;
    let mut spans = Vec::with_capacity(3);
    let dim = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default().fg(ctx.theme.text_dim)
    };
    if meta.depth > MAX_VISIBLE_DEPTH {
        spans.push(Span::styled("… ", dim));
        spans.push(Span::styled(
            "  ".repeat(MAX_VISIBLE_DEPTH.saturating_sub(1)),
            dim,
        ));
    } else if meta.depth > 0 {
        spans.push(Span::styled("  ".repeat(meta.depth), dim));
    }
    let glyph = if meta.has_children {
        if meta.collapsed { "▸ " } else { "▾ " }
    } else {
        "· "
    };
    let glyph_style = if ctx.is_cursor {
        ctx.row_style()
    } else if meta.has_children {
        Style::default().fg(ctx.theme.accent)
    } else {
        dim
    };
    spans.push(Span::styled(glyph, glyph_style));
    spans
}

/// Hard cap on a single chip's text (before the `…`). A verbose
/// label like `github_actions` otherwise eats a big slice of the row;
/// past this we truncate with an ellipsis so no one chip dominates.
const MAX_CHIP_LEN: usize = 10;

/// Shorten a label name for its chip: a small alias table for the
/// common verbose GitHub labels (`dependencies` → `deps`), then a hard
/// per-chip length cap with a trailing `…` for everything else
/// (issue #328). Case-insensitive on the alias lookup so `Dependencies`
/// and `dependencies` collapse the same way.
fn abbreviate_label(name: &str) -> String {
    let alias = match name.to_ascii_lowercase().as_str() {
        "dependencies" => Some("deps"),
        "documentation" => Some("docs"),
        "enhancement" => Some("enhance"),
        _ => None,
    };
    if let Some(short) = alias {
        return short.to_string();
    }
    if name.chars().count() > MAX_CHIP_LEN {
        let head: String = name.chars().take(MAX_CHIP_LEN - 1).collect();
        format!("{head}…")
    } else {
        name.to_string()
    }
}

/// Render the task's labels as compact chips: ` [name] [name] +N`.
/// Caps at 3 chips with a `+N` overflow indicator so the row layout
/// stays predictable when a PR has many labels. Each chip's text
/// adopts the GitHub label color (parsed from the hex string) as
/// the foreground; falls back to `text_dim` for the bracket
/// delimiters so the bracket framing reads consistently across the
/// rainbow. Empty when the row has no labels — the caller
/// (`cell_title`) then emits nothing at the title's tail.
///
/// Now that the chips ride in the title cell (#329), a stale issue's
/// fade covers them too: the title dims to send the eye elsewhere, so
/// full-color chips beside it would fight that cue. Suppressed on the
/// cursor row, matching `cell_title`.
fn label_spans(ctx: &WorkspaceRowCtx<'_>) -> Vec<Span<'static>> {
    const MAX_CHIPS: usize = 3;
    let is_internal_claim = |label: &lazybox_core::Label| {
        ctx.task.is_some_and(|task| {
            task.id.source == lazybox_core::GITHUB_SOURCE
                && lazybox_core::is_working_claim_label_name(&label.name)
        })
    };
    let labels = match ctx.task.map(|task| task.labels.as_slice()) {
        Some(labels) if labels.iter().any(|label| !is_internal_claim(label)) => labels,
        _ => return Vec::new(),
    };
    let dim = !ctx.is_cursor && ctx.is_stale_issue();
    let maybe_dim = |style: Style| {
        if dim {
            style.add_modifier(Modifier::DIM)
        } else {
            style
        }
    };
    let visible = labels.iter().filter(|label| !is_internal_claim(label));
    let total = visible.clone().count();
    let shown = visible.take(MAX_CHIPS);
    // Upper bound: MAX_CHIPS chips × (space + `[` + name + `]`) +
    // one optional overflow span. Sized to the visible rendering,
    // not the input length — a PR with 50 labels still only emits
    // 13 spans worth of buffer here.
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(MAX_CHIPS * 4 + 1);
    for label in shown {
        spans.push(Span::styled(" ", ctx.row_style()));
        let bracket_style = if ctx.is_cursor {
            ctx.row_style()
        } else {
            maybe_dim(Style::default().fg(ctx.theme.text_dim))
        };
        let text_style = if ctx.is_cursor {
            ctx.row_style()
        } else {
            maybe_dim(label_text_style(ctx.theme, &label.color))
        };
        spans.push(Span::styled("[", bracket_style));
        spans.push(Span::styled(abbreviate_label(&label.name), text_style));
        spans.push(Span::styled("]", bracket_style));
    }
    if total > MAX_CHIPS {
        let overflow_style = if ctx.is_cursor {
            ctx.row_style()
        } else {
            maybe_dim(Style::default().fg(ctx.theme.text_dim))
        };
        spans.push(Span::styled(
            format!(" +{}", total - MAX_CHIPS),
            overflow_style,
        ));
    }
    spans
}

/// Translate GitHub's hex color (e.g. `"d73a4a"`) into a ratatui
/// `Style`. Empty / unparseable → `text_dim`. The hex string may
/// arrive with or without a leading `#`; both shapes are handled.
///
/// ASCII-gated before byte-slicing: `.len()` is byte length, not
/// char count, so without the gate a 2-byte UTF-8 char that happens
/// to fit in 6 bytes would slice through a code point and panic.
/// GitHub never returns that, but providers are external input.
fn label_text_style(theme: &Theme, hex: &str) -> Style {
    let cleaned = hex.trim_start_matches('#');
    if !cleaned.is_ascii() || cleaned.len() != 6 {
        return Style::default().fg(theme.text_dim);
    }
    let parse = |s: &str| u8::from_str_radix(s, 16).ok();
    match (
        parse(&cleaned[0..2]),
        parse(&cleaned[2..4]),
        parse(&cleaned[4..6]),
    ) {
        (Some(r), Some(g), Some(b)) => Style::default().fg(ratatui::style::Color::Rgb(r, g, b)),
        _ => Style::default().fg(theme.text_dim),
    }
}

fn cell_unread(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    // Quiet/Digest/Muted sources suppress the ambient unread badge
    // (#scale); punch-through rows keep it (the ctx flag is already
    // punch-through-aware).
    if ctx.source_quiet {
        return Cell::empty();
    }
    let unread = ctx.workspace.map(|w| w.unread_count()).unwrap_or(0);
    if unread == 0 {
        return Cell::empty();
    }
    let text = if unread < 10 {
        format!(" ●{unread} ")
    } else if unread < 100 {
        format!(" ●{unread}")
    } else {
        " ●99+".to_string()
    };
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default()
            .fg(ctx.theme.hover)
            .add_modifier(Modifier::BOLD)
    };
    Cell::from_span(Span::styled(text, style))
}

/// Agent-letter badges — one for every non-`S` entry in `ctx.badges`.
/// A single agent keeps its padded pill; multiple agents share one
/// compact group (` C×2X `) so the complete set remains visible in a
/// narrow sidebar. A dim jump number prefixes the group when present.
fn cell_badge_agent(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let agent_count = ctx
        .badges
        .iter()
        .filter(|(letter, _)| *letter != 'S')
        .count();
    if agent_count == 0 {
        return Cell::empty();
    }

    let mut spans = Vec::with_capacity(agent_count + usize::from(ctx.agent_number.is_some()));
    if let Some(num) = ctx.agent_number {
        let num_style = if ctx.is_cursor {
            ctx.row_style()
        } else {
            Style::default()
                .fg(ctx.theme.text_dim)
                .add_modifier(Modifier::BOLD)
        };
        spans.push(Span::styled(format!(" {num}"), num_style));
    }
    spans.extend(
        ctx.badges
            .iter()
            .filter(|(letter, _)| *letter != 'S')
            .enumerate()
            .map(|(index, &(letter, n))| {
                let leading_space = agent_count == 1 || (ctx.agent_number.is_none() && index == 0);
                let trailing_space =
                    agent_count == 1 || (ctx.agent_number.is_none() && index + 1 == agent_count);
                let count = if n > 1 {
                    format!("×{n}")
                } else {
                    String::new()
                };
                let label = format!(
                    "{}{letter}{count}{}",
                    if leading_space { " " } else { "" },
                    if trailing_space { " " } else { "" },
                );
                Span::styled(label, badge_pill_style(ctx.theme, letter))
            }),
    );
    // A single agent shows its model right after the badge as a compact
    // `◆O` / `◆g ·xhi` tier badge — the `◆ tier` language of the terminal
    // tab (#803), abbreviated to a single glyph (#1068) so the model reads
    // above the agent letter without eating the row. The full tier word
    // stays in the tab and the `?` markers legend. Multiple agents collapse
    // to the compact `C×2X` group with no room for a label, so it's
    // suppressed there.
    if agent_count == 1
        && let Some((letter, model)) = ctx
            .badges
            .iter()
            .find(|(letter, _)| *letter != 'S')
            .and_then(|(letter, _)| {
                ctx.agent_models
                    .iter()
                    .find(|(l, _)| l == letter)
                    .map(|(_, model)| (*letter, model))
            })
    {
        spans.extend(model_badge_spans(ctx, letter, model));
    }
    Cell::new(spans)
}

/// Styled spans for a single agent's model, rendered as a compact `◆O`
/// tier badge (the `◆ tier` language of the terminal tab, #803, shrunk to
/// one glyph — #1068). The model name is abbreviated to a single short
/// glyph ([`model_short`]) that keeps the accent badge tone; a Codex-style
/// `<model> · <effort>` label keeps the abbreviated effort as a dimmer
/// suffix (`◆g ·xhi`) so "how hard it's thinking" still reads. Leads with
/// the `◆` glyph (the agent pill's trailing space supplies the gap) and
/// closes with a trailing space before the next column. `letter` is the
/// agent's badge letter, keying the short lookup so two agents that share
/// a tier label keep distinct shorts.
fn model_badge_spans(ctx: &WorkspaceRowCtx<'_>, letter: char, model: &str) -> Vec<Span<'static>> {
    let (badge_style, effort_style) = if ctx.is_cursor {
        (ctx.row_style(), ctx.row_style())
    } else {
        (
            Style::default().fg(ctx.theme.accent),
            Style::default().fg(ctx.theme.text_dim),
        )
    };
    // Split a `<model> · <effort>` reading into a dim effort suffix — but
    // only when the whole string isn't itself a declared tier label. A
    // best tier whose own label carries an effort (`"Opus · max"`, #748)
    // must resolve its declared short verbatim, not be split at the `·`
    // and have the effort mistaken for a Codex reasoning suffix.
    match model.split_once(" · ") {
        Some((name, effort)) if !ctx.model_shorts.contains_key(&(letter, model.to_string())) => {
            vec![
                Span::styled(format!("◆{}", model_short(ctx, letter, name)), badge_style),
                Span::styled(format!(" ·{} ", abbreviate_effort(effort)), effort_style),
            ]
        }
        _ => vec![Span::styled(
            format!("◆{} ", model_short(ctx, letter, model)),
            badge_style,
        )],
    }
}

/// The compact one-glyph form of a model name for the `◆O` badge (#1068):
/// the agent-declared `short` from that agent's tier menu when `name`
/// matches a tier label, else the name's first character. Keyed by the
/// agent's badge `letter` so two agents declaring the same tier label keep
/// their own shorts. Keeps the sidebar badge to a single glyph (`◆O`,
/// `◆g`) instead of the full model word — the full name stays in the
/// terminal tab and the `?` markers legend.
fn model_short(ctx: &WorkspaceRowCtx<'_>, letter: char, name: &str) -> String {
    if let Some(short) = ctx.model_shorts.get(&(letter, name.to_string())) {
        return short.clone();
    }
    // Digit-lookalike guard: a lone O/o/I/l after the ◆ reads as 0/1 in
    // monospace fonts. Widen those to two characters so the badge stays
    // legible; everything else keeps the single-glyph form (#1068).
    let mut chars = name.chars();
    match chars.next() {
        Some(first @ ('O' | 'o' | 'I' | 'l')) => match chars.next() {
            Some(second) => format!("{first}{second}"),
            None => first.to_string(),
        },
        Some(first) => first.to_string(),
        None => String::new(),
    }
}

/// Abbreviate a reasoning-effort token to a compact form. Covers every
/// token Codex emits (`CODEX_EFFORT_TOKENS`): the verbose ones shorten and
/// the already-short ones (`max`, `none`) pass through, as does any unknown
/// token a future provider might introduce.
fn abbreviate_effort(effort: &str) -> &str {
    match effort {
        "xhigh" => "xhi",
        "high" => "hi",
        "medium" => "med",
        "low" => "lo",
        "minimal" => "min",
        "default" => "def",
        other => other,
    }
}

fn cell_badge_shell(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let shell = ctx.badges.iter().find(|(c, _)| *c == 'S').copied();
    badge_slot_cell(ctx, shell)
}

fn badge_slot_cell(ctx: &WorkspaceRowCtx<'_>, badge: Option<(char, usize)>) -> Cell {
    match badge {
        Some((letter, n)) => {
            let label = if n > 1 {
                format!(" {letter}×{n} ")
            } else {
                format!(" {letter} ")
            };
            Cell::from_span(Span::styled(label, badge_pill_style(ctx.theme, letter)))
        }
        None => Cell::empty(),
    }
}

/// Concatenate a set of single-badge sub-cells into one packed cluster
/// cell, or [`Cell::empty`] when none are present. Each sub-cell already
/// carries its own padding, so concatenating their spans keeps the badges
/// visually separated; the shared `Column::max(0)` collapses to 0 when no
/// row in the pass carries any of them.
fn pack_badges(cells: impl IntoIterator<Item = Cell>) -> Cell {
    let mut spans = Vec::new();
    for cell in cells {
        spans.extend(cell.spans);
    }
    if spans.is_empty() {
        return Cell::empty();
    }
    Cell::new(spans)
}

/// The passive-info badge cluster (#813): the low-signal badges the row
/// carries, packed into one right-aligned cell instead of five anchored
/// columns (#524). Left → right, least → most consequential: `⎇ local` →
/// `✎` → `]N` → `⤓main`/`behind` → `FIX`. The two merge-when-green arms
/// live in [`cell_merge_arms`] instead, at a higher drop priority, so this
/// decoration sheds first under width pressure while the arms survive —
/// the graduated shedding the per-badge priorities gave before the pack.
///
/// The pack trades per-badge vertical alignment for horizontal density:
/// the column reserves only the widest single row's cluster rather than
/// the sum of every badge type's widest cell across the whole table.
fn cell_badges(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    pack_badges([
        cell_remote(ctx),
        cell_stack(ctx),
        cell_linked(ctx),
        cell_notes(ctx),
        cell_snippet(ctx),
        cell_track_main(ctx),
        cell_fix(ctx),
        cell_metered(ctx),
    ])
}

/// The `⇅ <remote>` badge: this workspace's sessions run on a remote box
/// (the `sandbox:` box, spawned via the `r`-prefix), not the local
/// in-process daemon. The network glyph + box name make "this runs on the
/// box" legible at a glance. Passive info, packed into the shared badge
/// cluster like `⎇ local`; renders nothing when the workspace is local.
fn cell_remote(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let Some(name) = ctx.workspace.and_then(|w| w.remote.as_deref()) else {
        return Cell::empty();
    };
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default()
            .fg(ctx.theme.warn)
            .add_modifier(Modifier::BOLD)
    };
    Cell::from_span(Span::styled(format!(" ⇅ {name} "), style))
}

/// The ` ⇗k/N ` stacked-PR badge (issue #969): this workspace's PR sits
/// at position `k` of a stack `N` deep. A fg-only accent glyph — passive
/// structural info, so it reads like the other decorations rather than an
/// urgent arm. The parent PR number is spelled out in the right pane;
/// here the position alone signals "part of a chain." Packs into the
/// shared badge cluster (#813).
fn cell_stack(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    let Some(stack) = ctx.stack else {
        return Cell::empty();
    };
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default()
            .fg(ctx.theme.accent)
            .add_modifier(Modifier::BOLD)
    };
    Cell::from_span(Span::styled(
        format!(" ⇗{}/{} ", stack.position, stack.depth),
        style,
    ))
}

/// The merge-arm badge cluster (#813): `⚡` (lazybox client-side
/// merge-on-green) then `◆` (GitHub-native, durable), packed into one
/// right-aligned cell. Kept out of [`cell_badges`] so its column carries a
/// higher drop priority (`P_ARMS`): the arms that decide whether the PR
/// merges itself outlive the low-signal decoration under width pressure,
/// preserving the shed order (`… → track → arm → auto`) the per-badge
/// columns had. As one-glyph icons (#1046) they almost never need to shed.
/// Sits rightmost of the badges, nearest the status glyph.
fn cell_merge_arms(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    pack_badges([cell_arm(ctx), cell_auto(ctx)])
}

/// The `⎇ local` badge for a linked (no-worktree) checkout — the
/// sidebar counterpart of the `⎇ main` tab badge, so the user is always
/// reminded this workspace's sessions run in their real checkout, not an
/// isolated worktree. Renders even on a task-less linked row. Packed into
/// the shared right-aligned badge cluster (#813) when present, so it
/// steals no title width when absent.
fn cell_linked(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    if !ctx.workspace.is_some_and(|w| w.is_linked()) {
        return Cell::empty();
    }
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default()
            .fg(ctx.theme.warn)
            .add_modifier(Modifier::BOLD)
    };
    Cell::from_span(Span::styled(" ⎇ local ", style))
}

/// The `✎` has-notes badge (issue #458). Passive info, not an urgent
/// arm — a dim fg-only glyph rather than the filled ARM/FIX blocks. Packs
/// into the shared badge cluster (#813).
fn cell_notes(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    if !ctx.has_notes {
        return Cell::empty();
    }
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default().fg(ctx.theme.text_dim)
    };
    Cell::from_span(Span::styled(" ✎ ", style))
}

/// The `]N` sent-snippet badge (issue #463) — a dim count of the
/// workspace's bounded recent-distinct history. Packs into the shared
/// badge cluster (#813).
fn cell_snippet(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    if ctx.sent_snippet_count == 0 {
        return Cell::empty();
    }
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default().fg(ctx.theme.text_dim)
    };
    Cell::from_span(Span::styled(
        format!(" ]{} ", ctx.sent_snippet_count),
        style,
    ))
}

/// The `◆` GitHub-native auto-merge glyph (#778, iconized #1046) — an
/// accent-colored marker in the same slot family as `⚡`/`🔧`. It's a
/// standing automation *policy*, so it lives here rather than in the
/// status column, where it used to hide the `✗` CI-fail glyph on exactly
/// the armed PRs that most need it. Packs into the merge-arm cluster.
///
/// Accent-colored, deliberately *not* the same as `⚡` (#794): `◆` is
/// GitHub's server-side merge, so it lands the PR even while lazybox is
/// closed. The accent reads as the durable, "handled by GitHub" state; `⚡`
/// carries the softer green of the lazybox-local arm that only fires while
/// the client runs.
fn cell_auto(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    if !ctx.auto_merge_enabled {
        return Cell::empty();
    }
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default()
            .fg(ctx.theme.accent)
            .add_modifier(Modifier::BOLD)
    };
    Cell::from_span(Span::styled(
        format!(" {} ", crate::components::sidebar::AUTO_GLYPH),
        style,
    ))
}

/// The `⚡` auto-merge-on-green glyph (iconized #1046) so the "this row
/// will merge itself once CI goes green" signal reads at a glance. Packs
/// into the merge-arm cluster (#813).
///
/// Colored `success` (green), not the accent of `◆` (#794), so the two
/// merge-on-green arms never blur into one marker: `⚡` is lazybox's
/// *client-side* merge, fired by the daemon only while lazybox is running
/// (quit lazybox and nothing merges), whereas `◆` is GitHub's durable
/// server-side merge. Green doubles as a mnemonic — this is the arm that
/// lands the PR "on green."
fn cell_arm(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    if !ctx.auto_merge_armed {
        return Cell::empty();
    }
    // Filled `success` block, not a fg-only glyph: `⚡` is the *actionable*
    // arm — the one merge-on-green state the user drives with `g g` — so it
    // reads at a glance the way the old ` ARM ` block did, while staying one
    // glyph wide (keeping #1046's column budget). Its passive siblings `◆`
    // (GitHub-native AUTO) and `🔧` (FIX) stay fg-only, so the block draws
    // the eye to the arm you toggled.
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default()
            .bg(ctx.theme.success)
            .fg(ratatui::style::Color::Black)
            .add_modifier(Modifier::BOLD)
    };
    Cell::from_span(Span::styled(
        format!(" {} ", crate::components::sidebar::ARM_GLYPH),
        style,
    ))
}

/// The compact `🔧` auto-fix glyph (iconized #1046). Packs into the shared
/// badge cluster (#813); the focused workspace's full trigger description
/// lives in the sidebar header.
/// The ` $ ` metering badge (#1488): this workspace's spawns route through
/// the metering proxy, so its cost and tokens are being priced.
///
/// Accent, not warn — metering is *observation*, not an automation that will
/// act on the PR (`FIX` / `ARM` earn warn). One glyph, packed into the shared
/// passive cluster like `✎` / `]N` / `⤓main`, so an armed canary is legible
/// across the whole sidebar rather than only on the focused row.
fn cell_metered(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    if !ctx.metered {
        return Cell::empty();
    }
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default()
            .fg(ctx.theme.accent)
            .add_modifier(Modifier::BOLD)
    };
    Cell::from_span(Span::styled(" $ ".to_string(), style))
}

fn cell_fix(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    if !ctx.auto_fix_ci_armed && !ctx.auto_fix_conflict_armed {
        return Cell::empty();
    }
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default()
            .fg(ctx.theme.warn)
            .add_modifier(Modifier::BOLD)
    };
    Cell::from_span(Span::styled(
        format!(" {} ", crate::components::sidebar::FIX_GLYPH),
        style,
    ))
}

/// The track-main badge (issue #535). A synced tracked workspace shows a
/// calm accent ` ⤓main `; one that's behind `origin/<default>` and can't
/// auto-sync (dirty / diverged) flips to a filled warn ` behind ` block
/// so a stuck worktree reads at a glance. Packs into the shared badge
/// cluster (#813).
fn cell_track_main(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    if !ctx.track_main {
        return Cell::empty();
    }
    if ctx.track_main_behind {
        let style = if ctx.is_cursor {
            ctx.row_style()
        } else {
            Style::default()
                .fg(ctx.theme.warn)
                .add_modifier(Modifier::BOLD)
        };
        return Cell::from_span(Span::styled(
            format!(" {} ", crate::components::sidebar::TRACK_GLYPH),
            style,
        ));
    }
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else {
        Style::default().fg(ctx.theme.accent)
    };
    Cell::from_span(Span::styled(
        format!(" {} ", crate::components::sidebar::TRACK_GLYPH),
        style,
    ))
}

fn cell_status(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    // CI/review pills only exist for a workspace with an upstream task.
    // The passive badges (`⎇ local` / `✎` / `]N` / `ARM` / `FIX` / `⤓main`) that
    // used to share this cell now own their own columns (#524), so this
    // is back to just the actionable status pills.
    let (primary, secondary) = match ctx.task {
        Some(task) => status_pills(task),
        None => (None, None),
    };
    // Empty cell when there's nothing to show — `Column::max(0)`
    // collapses the column across the whole table when NO row has a
    // pill, handing the slack back to the title flex.
    let claimed = ctx.workspace.is_some_and(Workspace::is_claimed);
    if !claimed && primary.is_none() && secondary.is_none() {
        return Cell::empty();
    }
    // Emit only the pills that are actually present, each trimmed to
    // its own ` LABEL ` block (the padding lives in the label). No
    // blank-slot filler: a pill-less side would just stack dead space
    // between the visible pill and the time trailer (issue #328).
    // Right-aligned by the column, so the rightmost pill sits one clean
    // gap off the duration — its block's trailing space plus the time
    // cell's leading space, nothing more.
    let mut spans = Vec::with_capacity(3);
    if claimed {
        let style = if ctx.is_cursor {
            ctx.row_style().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(ctx.theme.warn)
                .add_modifier(Modifier::BOLD)
        };
        // The row keeps the claim glyph QUIET: `⚑` for the ordinary
        // single-owner claim (raw device/session hex told a human
        // nothing at a glance — "what is 6eb7/64ce?"), `⚑×N` only when
        // several owners genuinely hold it. Full owner provenance
        // (device/session) still surfaces where there is room and
        // context: the claimed-spawn confirm names each owner, and the
        // labels carry it for debugging.
        let active = ctx
            .task
            .map(|task| task.active_qualified_working_claims(chrono::Utc::now()))
            .unwrap_or_default();
        let glyph = crate::components::sidebar::CLAIM_GLYPH;
        let label = match active.len() {
            n if n > 1 => format!(" {glyph}×{n} "),
            _ => format!(" {glyph} "),
        };
        spans.push(Span::styled(label, style));
    }
    if let Some(p) = primary {
        spans.push(Span::styled(p.label, p.style));
    }
    if let Some(p) = secondary {
        spans.push(Span::styled(p.label, p.style));
    }
    Cell::new(spans)
}

fn cell_time(ctx: &WorkspaceRowCtx<'_>) -> Cell {
    // A snoozed row shows WHEN IT WAKES instead of its last activity —
    // in the Snoozed mailbox and under the `snoozed` filter lens alike
    // (#scale: the Snoozed mailbox used to be an undifferentiated list;
    // "wakes in 3d" is the datum that makes un-snoozing an informed
    // choice). `⏾` (ascii: `z`) marks the number as a wake time, not an
    // age.
    if let Some(w) = ctx.workspace
        && let Some(until) = w.snoozed_until
        && until > ctx.now
    {
        let glyph = if ctx.ascii_glyphs { "z" } else { "⏾" };
        let text = format!(
            "{glyph}{}",
            crate::components::sidebar::relative_time(ctx.now, until)
        );
        let style = if ctx.is_cursor {
            ctx.row_style()
        } else {
            Style::default().fg(ctx.theme.text_dim)
        };
        return Cell::new(vec![
            Span::styled(" ", ctx.row_style()),
            Span::styled(text, style),
        ]);
    }
    let Some(task) = ctx.task else {
        return Cell::empty();
    };
    // A stale issue shows its age (time since opened) so it reads as old
    // even after recent chatter. Everything else — PRs, and recent issues —
    // keeps the last-activity timestamp: PRs because their CI/review pills
    // key off activity, recent issues because the issue asked to leave
    // their existing display untouched (issue #274).
    let anchor = if ctx.is_stale_issue() {
        task.opened_at()
    } else {
        task.updated_at
    };
    let text = crate::components::sidebar::relative_time(anchor, ctx.now);
    let style = if ctx.is_cursor {
        ctx.row_style()
    } else if ctx.is_stale_issue() {
        // Already `Nmo` here — fade it further so a stale issue's age
        // reads as "old, ignore me" rather than just another timestamp.
        Style::default()
            .fg(ctx.theme.text_dim)
            .add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(ctx.theme.text_dim)
    };
    // Time text may be `now` (3), `5m` (2), `12h` (3), `2d` (2),
    // `12mo` (4). Leading space is part of the cell so there's
    // always a 1-cell gap between time and whatever sits to its
    // left (status pill, or — when status collapses to 0 — the
    // title flex padding).
    Cell::new(vec![
        Span::styled(" ", ctx.row_style()),
        Span::styled(text, style),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use lazybox_core::{CiStatus, ReviewStatus, Task, TaskId, TaskRole, TaskState, Workspace};

    fn fixed_time() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 4, 1, 12, 0, 0).unwrap()
    }

    fn make_task(key: &str, title: &str) -> Task {
        Task {
            author: String::new(),
            id: TaskId {
                source: "github".into(),
                key: key.into(),
            },
            title: title.into(),
            body: None,
            state: TaskState::Open,
            role: TaskRole::Author,
            ci: CiStatus::None,
            review: ReviewStatus::None,
            checks: vec![],
            unread_count: 0,
            url: format!("https://github.com/{key}"),
            repo: Some("owner/repo".into()),
            branch: Some("main".into()),
            base_branch: None,
            updated_at: fixed_time(),
            created_at: None,
            closed_at: None,
            labels: vec![],
            reviewers: vec![],
            reviews: vec![],
            assignees: vec![],
            auto_merge_enabled: false,
            is_in_merge_queue: false,
            mergeable: lazybox_core::Mergeable::Mergeable,
            is_behind_base: false,
            merge_blocked: false,
            approval_policy: Default::default(),
            node_id: None,
            needs_reply: false,
            last_commenter: None,
            recent_activity: vec![],
            additions: 0,
            deletions: 0,
            changed_files: 0,
            kind: None,
            closes_issues: vec![],
            linked_tasks: vec![],
            parent: None,
            priority: None,
            state_label: None,
        }
    }

    /// A shared empty `(letter, label) → short` map for the ctx literals
    /// that don't exercise the model badge. `&'static`, so it satisfies any
    /// ctx lifetime; the badge's first-letter fallback still yields `◆O`.
    fn empty_shorts() -> &'static std::collections::HashMap<(char, String), String> {
        static EMPTY: std::sync::OnceLock<std::collections::HashMap<(char, String), String>> =
            std::sync::OnceLock::new();
        EMPTY.get_or_init(std::collections::HashMap::new)
    }

    fn ctx_for<'a>(
        workspace: &'a Workspace,
        task: &'a Task,
        theme: &'a Theme,
    ) -> WorkspaceRowCtx<'a> {
        WorkspaceRowCtx {
            recently_woken: false,
            source_quiet: false,
            workspace: Some(workspace),
            task: Some(task),
            theme,
            now: fixed_time(),
            focused: true,
            is_cursor: false,
            is_selected: false,
            max_pr_num_width: 4,
            asking: false,
            limit_reached: false,
            awaiting_reset: false,
            credit_exhausted: false,
            working: false,
            done: false,
            exited: false,
            working_glyph: working_glyph(0),
            spawning: false,
            spawning_glyph: spawning_glyph(0),
            badges: vec![],
            agent_models: vec![],
            agent_number: None,
            ascii_glyphs: false,
            auto_merge_armed: false,
            auto_merge_enabled: false,
            auto_fix_ci_armed: false,
            auto_fix_conflict_armed: false,
            track_main: false,
            track_main_behind: false,
            metered: false,
            has_notes: false,
            sent_snippet_count: 0,
            ticket_tree: None,
            stack: None,
            model_shorts: empty_shorts(),
            highlight_query: None,
            repo_prefix: None,
        }
    }

    fn theme() -> Theme {
        crate::theme::current().clone()
    }

    #[test]
    fn build_columns_have_expected_count_and_order() {
        let cols = build_columns(5);
        // 13 columns: the labels column retired into the title cell
        // (#329), and the seven per-badge passive columns (#524) packed
        // into two priority-tiered clusters (#813): passive-info and
        // merge-arms.
        assert_eq!(cols.len(), 13);
        // Title column (idx 5) is the only Flex one.
        let flex_indices: Vec<_> = cols
            .iter()
            .enumerate()
            .filter(|(_, c)| matches!(c.width, crate::components::table::ColumnWidth::Flex { .. }))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(flex_indices, vec![5]);
    }

    /// Regression for issue #231: the row prefix is a single shared
    /// 1-cell selection gutter (`▶` / ` `), not a 2-cell marker re-added
    /// at every depth. Reclaims one column of title room on every
    /// workspace row (and #121's earlier 4→2 cut goes the rest of the
    /// way to 1).
    #[test]
    fn cell_prefix_is_single_cell_selection_gutter() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);

        ctx.is_cursor = false;
        let cell = cell_prefix(&ctx);
        assert_eq!(cell.width(), 1);
        assert_eq!(cell_text(&cell), " ");

        ctx.is_cursor = true;
        let cell = cell_prefix(&ctx);
        assert_eq!(cell.width(), 1);
        assert_eq!(cell_text(&cell), "▶");

        // The fixed prefix column matches the cell width so the table
        // doesn't pad the inset back out.
        match build_columns(4)[0].width {
            crate::components::table::ColumnWidth::Fixed(w) => assert_eq!(w, 1),
            other => panic!("expected Fixed(1) prefix column, got {other:?}"),
        }
    }

    /// Broadcast multi-select: a selected row shows `✓` in the shared
    /// gutter — including under the cursor, so pressing `v` marks the
    /// current row visibly and immediately (issue #786). The slot stays
    /// a single cell either way.
    #[test]
    fn cell_prefix_shows_check_for_selected_rows() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);

        ctx.is_selected = true;
        let cell = cell_prefix(&ctx);
        assert_eq!(cell.width(), 1);
        assert_eq!(cell_text(&cell), "✓");
        assert_eq!(cell.spans[0].style.fg, Some(theme.accent));

        // Cursor + selected: the `✓` still shows (selection wins the
        // glyph); the cursor is carried by the full-row highlight, not
        // the caret.
        ctx.is_cursor = true;
        let cell = cell_prefix(&ctx);
        assert_eq!(cell.width(), 1);
        assert_eq!(cell_text(&cell), "✓");
        assert_eq!(cell.spans[0].style.fg, Some(theme.accent));
    }

    #[test]
    fn cursor_prefix_is_bold_accent_across_built_in_themes() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());

        for theme in crate::theme::BUILT_IN_THEMES {
            let mut ctx = ctx_for(&ws, &task, theme);
            ctx.is_cursor = true;
            let cell = cell_prefix(&ctx);

            assert_eq!(cell.width(), 1, "theme: {}", theme.name);
            assert_eq!(cell_text(&cell), "▶", "theme: {}", theme.name);
            assert_eq!(
                cell.spans[0].style.fg,
                Some(theme.accent),
                "theme: {}",
                theme.name
            );
            assert!(
                cell.spans[0].style.add_modifier.contains(Modifier::BOLD),
                "theme: {}",
                theme.name
            );
        }
    }

    #[test]
    fn build_columns_pr_num_uses_max_pr_num_width() {
        let cols = build_columns(7);
        match cols[2].width {
            crate::components::table::ColumnWidth::Fixed(w) => assert_eq!(w, 7),
            other => panic!("expected Fixed, got {other:?}"),
        }
    }

    /// PR-number cell prints `NNN` (no `#` prefix — issue #67) with no
    /// padding; the column width supplies the padding so every row
    /// aligns. The type glyph to the left now carries the
    /// issue-vs-PR signal the `#` used to.
    #[test]
    fn cell_pr_num_emits_bare_number_only() {
        let task = make_task("owner/repo#42", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_pr_num(&ctx);
        assert_eq!(cell.spans.len(), 1);
        assert_eq!(cell.spans[0].content.as_ref(), "42");
    }

    /// State slot: idle (neither asking nor working) → empty cell, so
    /// the column's reserved width fills with row-style spaces (no
    /// jitter as rows change state).
    #[test]
    fn cell_state_empty_when_idle() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_state(&ctx);
        assert_eq!(cell.width(), 0);
    }

    /// Concatenate a cell's span contents — the rendered text,
    /// independent of how it's split into styled spans.
    fn cell_text(cell: &Cell) -> String {
        cell.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// State slot: input-needed → 3 cells (" ? " — leading space,
    /// glyph, trailing space).
    #[test]
    fn cell_state_three_cells_when_asking() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.asking = true;
        let cell = cell_state(&ctx);
        assert_eq!(cell.width(), 3);
        assert_eq!(cell_text(&cell), " ? ");
    }

    #[test]
    fn cell_state_three_cells_when_credit_exhausted() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.credit_exhausted = true;
        let cell = cell_state(&ctx);
        assert_eq!(cell.width(), 3);
        assert_eq!(cell_text(&cell), " ¢ ");
    }

    /// State slot: working → 3 cells with the current spinner glyph.
    /// Same reserved width as the asking pill so the slot never jitters.
    #[test]
    fn cell_state_three_cells_with_spinner_when_working() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.working = true;
        ctx.working_glyph = working_glyph(3);
        let cell = cell_state(&ctx);
        assert_eq!(cell.width(), 3);
        assert_eq!(cell_text(&cell), format!(" {} ", working_glyph(3)));
    }

    /// State slot precedence: input-needed wins over working if both
    /// flags are somehow set (they can't be, by the disjoint sets,
    /// but the slot must still render exactly one thing).
    #[test]
    fn cell_state_input_needed_wins_over_working() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.asking = true;
        ctx.working = true;
        let cell = cell_state(&ctx);
        assert_eq!(cell_text(&cell), " ? ");
    }

    /// State slot: done → 3 cells with a `✓`, same reserved width as
    /// the asking pill and working spinner (#80).
    #[test]
    fn cell_state_three_cells_with_check_when_done() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.done = true;
        let cell = cell_state(&ctx);
        assert_eq!(cell.width(), 3);
        assert_eq!(cell_text(&cell), " ✓ ");
    }

    /// State slot: exited → 3 cells with a `✗`, same reserved width as the
    /// other pills (#356/#357).
    #[test]
    fn cell_state_three_cells_with_cross_when_exited() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.exited = true;
        let cell = cell_state(&ctx);
        assert_eq!(cell.width(), 3);
        assert_eq!(cell_text(&cell), " ✗ ");
    }

    /// State slot precedence: a live signal (here `done`) wins over the
    /// terminal `exited` marker if both are somehow set.
    #[test]
    fn cell_state_done_wins_over_exited() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.done = true;
        ctx.exited = true;
        let cell = cell_state(&ctx);
        assert_eq!(cell_text(&cell), " ✓ ");
    }

    /// State slot precedence: working wins over done if both flags are
    /// somehow set (disjoint upstream, but the slot renders one thing).
    #[test]
    fn cell_state_working_wins_over_done() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.working = true;
        ctx.working_glyph = working_glyph(2);
        ctx.done = true;
        let cell = cell_state(&ctx);
        assert_eq!(cell_text(&cell), format!(" {} ", working_glyph(2)));
    }

    /// #1069: re-spawning a crashed agent. The prior session's sticky
    /// `Exited` (#356 keeps it across the reap) lingers in the state map
    /// while the new cold provision runs — so `spawning` and `exited` are
    /// both set. The arc must win, or the row shows a stale ✗ over an
    /// agent that is actively restarting.
    #[test]
    fn cell_state_spawning_wins_over_exited() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.spawning = true;
        ctx.spawning_glyph = spawning_glyph(1);
        ctx.exited = true;
        let cell = cell_state(&ctx);
        assert_eq!(cell_text(&cell), format!(" {} ", spawning_glyph(1)));
    }

    /// A *live* `done` wins over the arc: a second session that finished
    /// (terminal still alive, → alert #80) beside a sibling session's
    /// cold spawn shows `✓`, not the arc. A stale `done` never reaches
    /// this arm — `Done` is dropped when its terminal exits (only `Exited`
    /// is retained), so a `done` seen while spawning is a live sibling.
    #[test]
    fn cell_state_done_wins_over_spawning() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.done = true;
        ctx.spawning = true;
        ctx.spawning_glyph = spawning_glyph(2);
        let cell = cell_state(&ctx);
        assert_eq!(cell_text(&cell), " ✓ ");
    }

    /// But a genuinely live signal still wins: a second session
    /// provisioning behind an already-`working` agent shows the working
    /// spinner, not the arc — the workspace really is working.
    #[test]
    fn cell_state_working_wins_over_spawning() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.working = true;
        ctx.working_glyph = working_glyph(4);
        ctx.spawning = true;
        ctx.spawning_glyph = spawning_glyph(1);
        let cell = cell_state(&ctx);
        assert_eq!(cell_text(&cell), format!(" {} ", working_glyph(4)));
    }

    /// The spinner frame index wraps so an unbounded counter is safe.
    #[test]
    fn working_glyph_wraps_frame_index() {
        let n = WORKING_SPINNER_FRAMES.len();
        assert_eq!(working_glyph(0), working_glyph(n));
        assert_eq!(working_glyph(1), working_glyph(n + 1));
    }

    /// Build a PR-shaped task — `make_task` fills `url` from `key`,
    /// so the `/pull/` segment is what makes `Workspace::attach_task`
    /// classify it as a PR (not an issue). The two `cell_type` tests
    /// need this; the existing `make_task` keys (`owner/repo#1`)
    /// would land in the gh_issues slot and render `○`.
    fn pr_task(repo: &str, n: u64) -> Task {
        let mut task = make_task(&format!("{repo}#{n}"), "x");
        task.url = format!("https://github.com/{repo}/pull/{n}");
        task
    }

    /// Type cell renders the unicode glyph plus a trailing space by
    /// default. Anchors the layout contract for issues #42 and #94:
    /// type column is 2 cells (glyph + separator) so the number sits
    /// one space off the glyph.
    #[test]
    fn cell_type_emits_glyph_then_space_for_pr() {
        let task = pr_task("owner/repo", 1);
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_type(&ctx);
        assert_eq!(cell.width(), 2);
        assert_eq!(cell.spans[0].content.as_ref(), "⇄");
        assert_eq!(cell.spans[1].content.as_ref(), " ");
    }

    /// `ascii_glyphs = true` (config opt-in) swaps the unicode glyph
    /// for the plain letter so fonts that don't render the unicode
    /// reliably still get a usable, single-cell marker.
    #[test]
    fn cell_type_honors_ascii_fallback() {
        let task = pr_task("owner/repo", 1);
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.ascii_glyphs = true;
        let cell = cell_type(&ctx);
        assert_eq!(cell.width(), 2);
        assert_eq!(cell.spans[0].content.as_ref(), "p");
        assert_eq!(cell.spans[1].content.as_ref(), " ");
    }

    /// Issue workspace (no PR slot) renders the `○` glyph — pins
    /// the per-variant routing through `workspace_type_label`.
    #[test]
    fn cell_type_emits_circle_for_issue() {
        let task = make_task("owner/repo#1", "x"); // make_task URL → issue
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_type(&ctx);
        assert_eq!(cell.width(), 2);
        assert_eq!(cell.spans[0].content.as_ref(), "○");
        assert_eq!(cell.spans[1].content.as_ref(), " ");
    }

    /// Empty workspace (no PR, no issues) → no type cell, so the
    /// glyph column collapses to nothing rather than rendering a
    /// stray character.
    #[test]
    fn cell_type_empty_for_scratch_workspace() {
        let ws = Workspace::empty(
            lazybox_core::WorkspaceKey("scratch".into()),
            "main",
            fixed_time(),
        );
        let theme = theme();
        let ctx = WorkspaceRowCtx {
            recently_woken: false,
            source_quiet: false,
            workspace: Some(&ws),
            task: None,
            theme: &theme,
            now: fixed_time(),
            focused: false,
            is_cursor: false,
            is_selected: false,
            max_pr_num_width: 2,
            asking: false,
            limit_reached: false,
            awaiting_reset: false,
            credit_exhausted: false,
            working: false,
            done: false,
            exited: false,
            working_glyph: working_glyph(0),
            spawning: false,
            spawning_glyph: spawning_glyph(0),
            badges: vec![],
            agent_models: vec![],
            agent_number: None,
            ascii_glyphs: false,
            auto_merge_armed: false,
            auto_merge_enabled: false,
            auto_fix_ci_armed: false,
            auto_fix_conflict_armed: false,
            track_main: false,
            track_main_behind: false,
            metered: false,
            has_notes: false,
            sent_snippet_count: 0,
            ticket_tree: None,
            stack: None,
            model_shorts: empty_shorts(),
            highlight_query: None,
            repo_prefix: None,
        };
        assert_eq!(cell_type(&ctx).width(), 0);
    }

    /// Title cell keeps a bracketed `[CI]`-style tag inline instead of
    /// hoisting it into a reserved column (#80).
    #[test]
    fn cell_title_keeps_bracket_tag_inline() {
        let task = make_task("owner/repo#1", "[CI] cache post-job upload");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_title(&ctx);
        assert_eq!(cell.spans[0].content.as_ref(), "[CI] cache post-job upload");
    }

    #[test]
    fn cell_title_prepends_dim_repo_prefix_when_set() {
        let task = make_task("owner/repo#1", "Fix the thing");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.repo_prefix = Some("owner/repo".into());
        let cell = cell_title(&ctx);
        assert_eq!(cell.spans[0].content.as_ref(), "owner/repo · ");
        assert_eq!(cell.spans[0].style.fg, Some(theme.text_dim));
        // The title itself still follows, unchanged.
        assert_eq!(cell.spans[1].content.as_ref(), "Fix the thing");
    }

    #[test]
    fn cell_title_has_no_repo_prefix_by_default() {
        let task = make_task("owner/repo#1", "Fix the thing");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_title(&ctx);
        assert_eq!(cell.spans[0].content.as_ref(), "Fix the thing");
    }

    /// #1450 regression: the original fix put the `repo · ` prefix ahead
    /// of the title in the same cell, and right-edge truncation then ate
    /// the title and left only the prefix on a narrow pane. The prefix is
    /// now a droppable atomic head, so it sheds whole and the title stays.
    #[test]
    fn focused_prefix_never_evicts_the_title_on_a_narrow_pane() {
        let task = make_task("owner/repo#1", "Fix the thing");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        // A long owner/repo that, ahead of the title, would have shoved it
        // off the row entirely.
        ctx.repo_prefix = Some("AntoineToussaint/lazybox".into());
        let columns = build_columns(4);
        let lines = crate::components::table::render_table(&[build_row(&ctx)], &columns, 30);
        let text = line_text(&lines[0]);
        assert!(
            text.contains("Fix the thing"),
            "the title must stay whole, not be crowded out by the prefix: {text:?}",
        );
        assert!(
            !text.contains("AntoineToussaint"),
            "the long repo prefix must shed, not swallow the title: {text:?}",
        );
    }

    /// #1450: on the cursor row the prefix must stay legible — no forced
    /// dim fg, which reads as low-contrast grey over the highlight fill.
    /// Mirrors how `cell_title`/`ticket_tree_prefix` suppress dimming on
    /// the cursor row.
    #[test]
    fn focused_prefix_is_legible_not_dimmed_on_the_cursor_row() {
        let task = make_task("owner/repo#1", "Fix the thing");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.repo_prefix = Some("owner/repo".into());
        ctx.is_cursor = true;
        let cell = cell_title(&ctx);
        assert_eq!(cell.spans[0].content.as_ref(), "owner/repo · ");
        assert_ne!(
            cell.spans[0].style.fg,
            Some(theme.text_dim),
            "cursor-row prefix must not be forced to the dim fg",
        );
    }

    #[test]
    fn ticket_tree_prefix_distinguishes_parent_child_and_folded_state() {
        let theme = theme();
        let task = make_task("owner/repo#1", "Parent ticket");
        let workspace = Workspace::from_task(task.clone(), fixed_time());
        let mut ctx = ctx_for(&workspace, &task, &theme);

        ctx.ticket_tree = Some(lazybox_tui_core::inbox::TicketTreeMeta {
            depth: 0,
            has_children: true,
            collapsed: false,
            context_only: false,
        });
        let expanded: String = cell_title(&ctx)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(expanded.starts_with("▾ "));

        ctx.ticket_tree = Some(lazybox_tui_core::inbox::TicketTreeMeta {
            depth: 1,
            has_children: false,
            collapsed: false,
            context_only: false,
        });
        let child: String = cell_title(&ctx)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(child.starts_with("  · "));

        ctx.ticket_tree = Some(lazybox_tui_core::inbox::TicketTreeMeta {
            depth: 0,
            has_children: true,
            collapsed: true,
            context_only: false,
        });
        let collapsed: String = cell_title(&ctx)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(collapsed.starts_with("▸ "));
    }

    /// Title cell keeps a conventional-commit prefix inline rather than
    /// stripping it into a separate kind column (#80).
    #[test]
    fn cell_title_keeps_conventional_prefix_inline() {
        let task = make_task("owner/repo#1", "feat: add login");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_title(&ctx);
        assert_eq!(cell.spans[0].content.as_ref(), "feat: add login");
    }

    /// Cursor row gets `row_focused` style and propagates via the
    /// Row's fill_style.
    #[test]
    fn build_row_cursor_gets_focused_fill_style() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.is_cursor = true;
        ctx.focused = true;
        let row = build_row(&ctx);
        assert_eq!(row.fill_style, Some(theme.row_focused()));
    }

    /// Unfocused cursor row uses `row_unfocused` not `row_focused`.
    #[test]
    fn build_row_cursor_unfocused_gets_unfocused_fill_style() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.is_cursor = true;
        ctx.focused = false;
        let row = build_row(&ctx);
        assert_eq!(row.fill_style, Some(theme.row_unfocused()));
    }

    /// A linked (no-worktree) workspace shows the `⎇ local` badge in its
    /// own slot even when it has no task, so the user always sees it
    /// points at their real checkout (#524 moved it out of the status
    /// cell into `cell_linked`).
    #[test]
    fn cell_linked_shows_local_badge_for_linked_workspace() {
        let theme = theme();
        let mut ws = Workspace::empty(
            lazybox_core::WorkspaceKey::new("acme-widget"),
            "main",
            fixed_time(),
        );
        ws.linked_checkout = Some(std::path::PathBuf::from("/home/dev/code/acme/widget"));
        let task = make_task("owner/repo#1", "x");
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.task = None; // linked tracking row, no attached task
        let cell = cell_linked(&ctx);
        assert!(
            cell.width() > 0,
            "linked workspace must render a non-empty linked cell"
        );
        let text: String = cell.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("⎇ local"), "got {text:?}");
        // The status cell no longer carries the linked badge.
        assert_eq!(cell_status(&ctx).width(), 0);

        // A plain workspace with no task renders nothing in either slot.
        let plain = Workspace::empty(
            lazybox_core::WorkspaceKey::new("plain"),
            "main",
            fixed_time(),
        );
        let mut plain_ctx = ctx_for(&plain, &task, &theme);
        plain_ctx.task = None;
        assert_eq!(cell_linked(&plain_ctx).width(), 0);
    }

    /// No badges, no agent cell content.
    #[test]
    fn cell_badge_agent_empty_when_no_agent() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        assert_eq!(cell_badge_agent(&ctx).width(), 0);
    }

    /// Single agent: ` C ` (3 cells), shell slot picks up `S` too.
    #[test]
    fn cell_badge_agent_renders_single_letter_pill() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.badges = vec![('C', 1), ('S', 1)];
        assert_eq!(cell_badge_agent(&ctx).width(), 3);
        assert_eq!(cell_badge_shell(&ctx).width(), 3);
    }

    /// Distinct agents share a compact group while shells stay in the
    /// separate shell slot.
    #[test]
    fn cell_badge_agent_renders_every_distinct_agent() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.badges = vec![('C', 1), ('X', 1), ('S', 1)];

        let cell = cell_badge_agent(&ctx);
        let text: String = cell.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(cell.width(), 4);
        assert_eq!(text, " CX ");
        assert!(
            !text.contains('S'),
            "shell leaked into agent slot: {text:?}"
        );
        assert_eq!(cell_badge_shell(&ctx).width(), 3);
    }

    /// Multi-instance agent widens the slot to 5 cells (` C×2 `).
    #[test]
    fn cell_badge_agent_widens_for_multi_instance() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.badges = vec![('C', 2)];
        assert_eq!(cell_badge_agent(&ctx).width(), 5);
    }

    /// Counts stay attached to their agent without hiding the other
    /// distinct agent badges.
    #[test]
    fn cell_badge_agent_renders_mixed_counts() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.badges = vec![('C', 2), ('X', 1)];

        let cell = cell_badge_agent(&ctx);
        let text: String = cell.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(cell.width(), 6);
        assert_eq!(text, " C×2X ");
    }

    /// A jump number prefixes the agent pill (` 2 C `, 5 cells) so the
    /// `]]<digit>` target is visible; the digit is the `agent_number`.
    #[test]
    fn cell_badge_agent_shows_jump_number() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.badges = vec![('C', 1)];
        ctx.agent_number = Some(2);
        let cell = cell_badge_agent(&ctx);
        // ` 2`(2) + ` C `(3) = 5 cells, vs. 3 for the bare pill.
        assert_eq!(cell.width(), 5);
        let text: String = cell.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains('2'), "jump number missing: {text:?}");
        assert!(text.contains('C'), "agent letter missing: {text:?}");
    }

    /// Without a jump number the agent pill stays at its bare ` C ` —
    /// non-agent rows and agents past the ninth get no badge.
    #[test]
    fn cell_badge_agent_no_number_stays_bare() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.badges = vec![('C', 1)];
        ctx.agent_number = None;
        assert_eq!(cell_badge_agent(&ctx).width(), 3);
    }

    /// #779/#803/#1068: a single agent shows its model after the pill as a
    /// compact `◆O` tier badge — the tier word abbreviated to one glyph —
    /// matched to the badge letter and leaning on the pill's trailing
    /// space for the gap.
    #[test]
    fn cell_badge_agent_appends_model_for_single_agent() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.badges = vec![('C', 1)];
        ctx.agent_models = vec![('C', "Opus".to_string())];
        let cell = cell_badge_agent(&ctx);
        let text: String = cell.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            text, " C ◆Op ",
            "the model rides after the pill as a compact ◆ badge (Op, not O — a lone O reads as zero)"
        );
    }

    /// #1068: the model name abbreviates to its agent-declared `short`
    /// when the tier menu supplies one — even when that differs from the
    /// first letter (disambiguating two tiers that share it) — and to the
    /// label's first character otherwise.
    #[test]
    fn model_badge_uses_declared_short_then_first_letter() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let shorts =
            std::collections::HashMap::from([(('C', "Sonnet".to_string()), "Sn".to_string())]);

        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.model_shorts = &shorts;
        ctx.badges = vec![('C', 1)];
        ctx.agent_models = vec![('C', "Sonnet".to_string())];
        let text: String = cell_badge_agent(&ctx)
            .spans
            .iter()
            .map(|s| s.content.as_ref().to_string())
            .collect();
        assert_eq!(text, " C ◆Sn ", "declared short wins over first-letter");

        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.badges = vec![('C', 1)];
        ctx.agent_models = vec![('C', "Haiku".to_string())];
        let text: String = cell_badge_agent(&ctx)
            .spans
            .iter()
            .map(|s| s.content.as_ref().to_string())
            .collect();
        assert_eq!(
            text, " C ◆H ",
            "no declared short → the label's first letter"
        );
    }

    /// #1068 review: a best tier whose own label carries an effort
    /// (`"Opus · max"`, #748) must resolve its declared short verbatim —
    /// not be split at the `·` with `max` mistaken for a Codex reasoning
    /// suffix and the short dropped to a first letter.
    #[test]
    fn model_badge_honors_declared_short_on_a_model_dot_effort_label() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let shorts =
            std::collections::HashMap::from([(('C', "Opus · max".to_string()), "B".to_string())]);

        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.model_shorts = &shorts;
        ctx.badges = vec![('C', 1)];
        ctx.agent_models = vec![('C', "Opus · max".to_string())];
        let text: String = cell_badge_agent(&ctx)
            .spans
            .iter()
            .map(|s| s.content.as_ref().to_string())
            .collect();
        assert_eq!(
            text, " C ◆B ",
            "the whole-label short wins over splitting at the ·"
        );
    }

    /// #1068 review: two agents can declare the same tier label with
    /// different shorts; the badge is keyed by the agent's letter, so each
    /// resolves its own short instead of colliding on the bare label.
    #[test]
    fn model_badge_disambiguates_shared_label_by_agent_letter() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let shorts = std::collections::HashMap::from([
            (('C', "Fast".to_string()), "F".to_string()),
            (('X', "Fast".to_string()), "⚡".to_string()),
        ]);

        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.model_shorts = &shorts;
        ctx.badges = vec![('X', 1)];
        ctx.agent_models = vec![('X', "Fast".to_string())];
        let text: String = cell_badge_agent(&ctx)
            .spans
            .iter()
            .map(|s| s.content.as_ref().to_string())
            .collect();
        assert_eq!(
            text, " X ◆⚡ ",
            "codex's `Fast` short must not pick up claude's"
        );
    }

    /// #803/#1068: a Codex-style `<model> · <effort>` label keeps its
    /// abbreviated effort as a dimmer suffix while the model shrinks to
    /// one glyph — the accent `◆g` above the dim ` ·xhi `.
    #[test]
    fn cell_badge_agent_model_is_diamond_badge_with_dim_effort() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.badges = vec![('X', 1)];
        ctx.agent_models = vec![('X', "gpt-5.5 · xhigh".to_string())];
        let cell = cell_badge_agent(&ctx);
        let text: String = cell.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, " X ◆g ·xhi ");
        // The `◆g` model glyph is accent; the ` ·xhi ` effort span is dim.
        let diamond = cell
            .spans
            .iter()
            .find(|s| s.content.as_ref() == "◆g")
            .expect("a ◆ badge span");
        assert_eq!(diamond.style.fg, Some(theme.accent));
        let effort = cell
            .spans
            .iter()
            .find(|s| s.content.as_ref() == " ·xhi ")
            .expect("a dim effort span");
        assert_eq!(effort.style.fg, Some(theme.text_dim));
    }

    /// #813/#1068: the abbreviated one-glyph badge keeps the agent column
    /// narrow — one row's verbose `gpt-5.6-sol · xhigh` no longer widens
    /// the column on a sibling row that just reads `Opus`.
    #[test]
    fn long_model_does_not_widen_agent_column_table_wide() {
        let theme = theme();
        let task0 = make_task("owner/repo#1", "row with verbose model");
        let task1 = make_task("owner/repo#2", "row with short model");
        let ws0 = Workspace::from_task(task0.clone(), fixed_time());
        let ws1 = Workspace::from_task(task1.clone(), fixed_time());
        let mut ctx0 = ctx_for(&ws0, &task0, &theme);
        ctx0.badges = vec![('X', 1)];
        ctx0.agent_models = vec![('X', "gpt-5.6-sol · xhigh".to_string())];
        let mut ctx1 = ctx_for(&ws1, &task1, &theme);
        ctx1.badges = vec![('C', 1)];
        ctx1.agent_models = vec![('C', "Opus".to_string())];

        // The verbose row's agent cell stays bounded — the raw
        // `gpt-5.6-sol · xhigh` (19) would have anchored the column that
        // wide across the table; abbreviated to `◆g ·xhi` it's a handful
        // of cells regardless of how long the raw model string is.
        let verbose = cell_badge_agent(&ctx0);
        assert!(
            verbose.width() <= 14,
            "abbreviated agent cell should stay narrow: {} cells",
            verbose.width(),
        );
        let text: String = verbose.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !text.contains("xhigh") && !text.contains("gpt-5.6-sol"),
            "the model and effort should be abbreviated: {text:?}"
        );
    }

    /// #779: the label is matched to its own agent letter — a model
    /// recorded for a letter that isn't present must not leak onto the row.
    #[test]
    fn cell_badge_agent_model_matches_its_own_letter() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.badges = vec![('C', 1)];
        // A stale label keyed to a codex badge that isn't on this row.
        ctx.agent_models = vec![('X', "gpt-5.5 · xhigh".to_string())];
        let cell = cell_badge_agent(&ctx);
        let text: String = cell.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, " C ", "an unmatched label must not render");
    }

    /// #779: two distinct agents collapse to the compact ` CX ` group with
    /// no room for labels, so the model is suppressed even when known.
    #[test]
    fn cell_badge_agent_suppresses_model_for_multiple_agents() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.badges = vec![('C', 1), ('X', 1)];
        ctx.agent_models = vec![
            ('C', "Opus".to_string()),
            ('X', "gpt-5.5 · xhigh".to_string()),
        ];
        let cell = cell_badge_agent(&ctx);
        let text: String = cell.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, " CX ", "multi-agent rows drop the models");
    }

    /// Empty workspace (no task): title falls back to workspace name.
    #[test]
    fn cell_title_falls_back_to_workspace_name_when_no_task() {
        let ws = Workspace::empty(
            lazybox_core::WorkspaceKey("lonely".into()),
            "main",
            fixed_time(),
        );
        let theme = theme();
        let ctx = WorkspaceRowCtx {
            recently_woken: false,
            source_quiet: false,
            workspace: Some(&ws),
            task: None,
            theme: &theme,
            now: fixed_time(),
            focused: false,
            is_cursor: false,
            is_selected: false,
            max_pr_num_width: 3,
            asking: false,
            limit_reached: false,
            awaiting_reset: false,
            credit_exhausted: false,
            working: false,
            done: false,
            exited: false,
            working_glyph: working_glyph(0),
            spawning: false,
            spawning_glyph: spawning_glyph(0),
            badges: vec![],
            agent_models: vec![],
            agent_number: None,
            ascii_glyphs: false,
            auto_merge_armed: false,
            auto_merge_enabled: false,
            auto_fix_ci_armed: false,
            auto_fix_conflict_armed: false,
            track_main: false,
            track_main_behind: false,
            metered: false,
            has_notes: false,
            sent_snippet_count: 0,
            ticket_tree: None,
            stack: None,
            model_shorts: empty_shorts(),
            highlight_query: None,
            repo_prefix: None,
        };
        assert_eq!(cell_title(&ctx).spans[0].content.as_ref(), "lonely");
    }

    /// `cell_status` returns an empty cell when neither review nor CI
    /// has anything to surface — so the table's `Column::max(0)` for
    /// status collapses to 0 when no row in the visible list has a
    /// pill, handing the slack back to the title flex.
    #[test]
    fn cell_status_empty_when_no_pills() {
        let mut task = make_task("owner/repo#1", "x");
        task.review = ReviewStatus::None;
        task.ci = CiStatus::None;
        task.state = TaskState::Open;
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        assert_eq!(cell_status(&ctx).width(), 0);
    }

    #[test]
    fn working_label_renders_as_a_dedicated_claim_pill_not_a_generic_chip() {
        let mut task = make_task("owner/repo#1", "x");
        task.review = ReviewStatus::None;
        task.ci = CiStatus::None;
        task.state = TaskState::Open;
        task.labels = vec![
            lazybox_core::Label::new("Working"),
            lazybox_core::Label::new("bug"),
        ];
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);

        let status: String = cell_status(&ctx)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(
            status,
            format!(" {} ", crate::components::sidebar::CLAIM_GLYPH)
        );
        let labels: String = label_spans(&ctx)
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(labels, " [bug]");
    }

    /// A single qualified owner renders the QUIET glyph — raw
    /// device/session hex on the row read as line noise ("what is
    /// 6eb7/64ce?"); owner detail lives in the claimed-spawn confirm.
    #[test]
    fn qualified_working_claim_pill_stays_quiet_for_one_owner() {
        let mut task = make_task("owner/repo#1", "x");
        task.review = ReviewStatus::None;
        task.ci = CiStatus::None;
        task.state = TaskState::Open;
        task.labels = vec![lazybox_core::Label::new(
            lazybox_core::qualified_working_claim_label(
                "0123456789abcdef0123456789abcdef",
                uuid::Uuid::parse_str("12345678-90ab-cdef-1234-567890abcdef").unwrap(),
                chrono::Utc::now() + chrono::Duration::hours(1),
            )
            .unwrap(),
        )];
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);

        let status: String = cell_status(&ctx)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(
            status,
            format!(" {} ", crate::components::sidebar::CLAIM_GLYPH),
            "one owner is the ordinary case — bare glyph, no hex",
        );
        assert!(label_spans(&ctx).is_empty());
    }

    #[test]
    fn multiple_qualified_owners_collapse_to_a_counted_claim_glyph() {
        let mut task = make_task("owner/repo#1", "x");
        task.review = ReviewStatus::None;
        task.ci = CiStatus::None;
        task.state = TaskState::Open;
        let expires = chrono::Utc::now() + chrono::Duration::hours(1);
        task.labels = vec![
            lazybox_core::Label::new(
                lazybox_core::qualified_working_claim_label(
                    "0123456789abcdef0123456789abcdef",
                    uuid::Uuid::from_u128(1),
                    expires,
                )
                .unwrap(),
            ),
            lazybox_core::Label::new(
                lazybox_core::qualified_working_claim_label(
                    "fedcba9876543210fedcba9876543210",
                    uuid::Uuid::from_u128(2),
                    expires,
                )
                .unwrap(),
            ),
        ];
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);

        let status: String = cell_status(&ctx)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(
            status,
            format!(" {}×2 ", crate::components::sidebar::CLAIM_GLYPH)
        );
    }

    #[test]
    fn non_github_working_label_remains_an_ordinary_chip() {
        let mut task = make_task("ENG-42", "x");
        task.id.source = "linear".to_string();
        task.review = ReviewStatus::None;
        task.ci = CiStatus::None;
        task.state = TaskState::Open;
        task.labels = vec![lazybox_core::Label::new("Working")];
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);

        assert_eq!(cell_status(&ctx).width(), 0);
        let labels: String = label_spans(&ctx)
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(labels, " [Working]");
    }

    /// A lone CI signal is sized to just its own `✗` glyph (#1046) — no
    /// blank review-slot filler stacking dead space before the time
    /// trailer (issue #328).
    #[test]
    fn cell_status_is_trimmed_to_the_present_pill() {
        let mut task = make_task("owner/repo#1", "x");
        task.ci = CiStatus::Failure;
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_status(&ctx);
        assert_eq!(cell.width(), 2);
        assert_eq!(cell.spans.len(), 1);
        assert_eq!(cell.spans[0].content.as_ref(), " ✗");
    }

    /// #1079: a merged PR must render a glyph distinct from the
    /// actionable `✓` shared by ready / approved / CI-green, in a dimmed
    /// terminal-state style — so "done and gone" can't be mistaken for
    /// "act on me now" at the real rendered-cell level (not just in the
    /// `status_pill` map). Renders all three rows through `cell_status`
    /// and pins glyph + color for each.
    #[test]
    fn merged_row_renders_a_distinct_dim_glyph_from_ready_and_approved() {
        let theme = theme();
        let rendered = |task: &Task| {
            let ws = Workspace::from_task(task.clone(), fixed_time());
            let ctx = ctx_for(&ws, task, &theme);
            let cell = cell_status(&ctx);
            let span = &cell.spans[0];
            (span.content.as_ref().to_string(), span.style.fg)
        };

        let mut merged = make_task("owner/repo#1", "x");
        merged.state = TaskState::Merged;

        let mut ready = make_task("owner/repo#2", "x");
        ready.review = ReviewStatus::Approved;
        ready.ci = CiStatus::Success;

        let mut approved = make_task("owner/repo#3", "x");
        approved.review = ReviewStatus::Approved;
        approved.ci = CiStatus::Running;

        let (merged_glyph, merged_fg) = rendered(&merged);
        let (ready_glyph, ready_fg) = rendered(&ready);
        let (approved_glyph, approved_fg) = rendered(&approved);

        // Distinct glyph, not the shared `✓`.
        assert_eq!(merged_glyph, " ⋈");
        assert_eq!(ready_glyph, " ✓");
        assert_eq!(approved_glyph, " ✓");
        assert_ne!(merged_glyph, ready_glyph);
        assert_ne!(merged_glyph, approved_glyph);

        // Terminal / past-tense styling: dimmed, so the distinction holds
        // even without color, and unlike the bright actionable `✓`s.
        assert_eq!(merged_fg, Some(theme.text_dim));
        assert_ne!(merged_fg, ready_fg);
        assert_ne!(merged_fg, approved_fg);
    }

    /// An armed workspace surfaces its `⚡` merge-on-green marker in its
    /// own slot even when the PR has no CI / review pill yet — so a
    /// freshly-armed row is visibly distinct before CI even starts (#524).
    #[test]
    fn cell_arm_shows_pill_when_armed_without_ci() {
        let mut task = make_task("owner/repo#1", "x");
        task.review = ReviewStatus::None;
        task.ci = CiStatus::None;
        task.state = TaskState::Open;
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        assert_eq!(cell_arm(&ctx).width(), 0, "unarmed row has no ARM slot");
        ctx.auto_merge_armed = true;
        let cell = cell_arm(&ctx);
        assert_eq!(cell.spans[0].content.as_ref(), " ⚡ ");
        // The status cell stays empty — no CI/review pill here.
        assert_eq!(cell_status(&ctx).width(), 0);
    }

    /// #778: GitHub-native auto-merge is a policy, not a status. An
    /// armed PR with failing CI must show BOTH the `◆` policy glyph
    /// (its own column) AND the `✗` CI-fail status glyph — the whole
    /// point of the fix is that AUTO no longer hides the red CI it's
    /// blocked on.
    #[test]
    fn auto_merge_pill_and_ci_fail_render_together() {
        let mut task = make_task("owner/repo#1", "x");
        task.state = TaskState::Open;
        task.auto_merge_enabled = true;
        task.ci = CiStatus::Failure;
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        assert_eq!(cell_auto(&ctx).width(), 0, "auto-merge off → no AUTO slot");
        ctx.auto_merge_enabled = true;
        assert_eq!(
            cell_auto(&ctx).spans[0].content.as_ref(),
            " ◆ ",
            "armed PR shows its AUTO policy glyph",
        );
        assert_eq!(
            cell_status(&ctx).spans[0].content.as_ref(),
            " ✗",
            "…and the failing-CI status glyph is no longer hidden",
        );
    }

    /// #794: `⚡` (lazybox client-side merge-on-green) and `◆`
    /// (GitHub-native, durable) must not render as the same marker. They
    /// sit in adjacent columns and both mean "merges itself on green," so
    /// they carry distinct glyphs AND distinct colors — `⚡` in lazybox
    /// green (dies when lazybox closes), `◆` in GitHub accent. Pin both so
    /// they never collapse back to one look.
    #[test]
    fn arm_and_auto_pills_are_visually_distinct() {
        let mut task = make_task("owner/repo#1", "x");
        task.state = TaskState::Open;
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.auto_merge_armed = true;
        ctx.auto_merge_enabled = true;
        let arm = cell_arm(&ctx);
        let auto = cell_auto(&ctx);
        assert_ne!(
            arm.spans[0].content.as_ref(),
            auto.spans[0].content.as_ref(),
            "ARM and AUTO must not share a glyph"
        );
        let arm_style = arm.spans[0].style;
        let auto_style = auto.spans[0].style;
        // `⚡` is the actionable arm, so it's a filled lazybox-green block
        // (green background, black glyph) — reads at a glance like the old
        // ` ARM ` pill; `◆` (AUTO) stays a fg-only GitHub-accent glyph.
        assert_eq!(
            arm_style.bg,
            Some(theme.success),
            "ARM is a filled lazybox-green block"
        );
        assert_eq!(auto_style.fg, Some(theme.accent), "AUTO is GitHub-accent");
        assert_ne!(
            (arm_style.fg, arm_style.bg),
            (auto_style.fg, auto_style.bg),
            "ARM and AUTO must not share a look"
        );
    }

    /// The shared auto-fix column stays compact even on the cursor row.
    /// #1488: a metered workspace carries a durable `$` on its row. Before
    /// this the only per-workspace cue was a header pill drawn from the
    /// focused row, so you couldn't tell which workspaces were metered
    /// without visiting each one.
    #[test]
    fn cell_metered_marks_a_metered_workspace() {
        let task = make_task("owner/repo#1", "x");
        let mut ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();

        // Not metered → nothing, so the column collapses for a sidebar
        // where no row is metered.
        let ctx = ctx_for(&ws, &task, &theme);
        assert_eq!(cell_metered(&ctx).width(), 0);

        ws.metered = true;
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.metered = true;
        let cell = cell_metered(&ctx);
        assert_eq!(cell_text(&cell), " $ ");
        assert_eq!(
            cell.spans[0].style.fg,
            Some(theme.accent),
            "metering observes; it doesn't act on the PR the way FIX/ARM do",
        );

        // On the cursor row the badge inherits the row highlight so the
        // fill stays legible — same rule every other badge follows.
        ctx.is_cursor = true;
        assert_eq!(cell_metered(&ctx).spans[0].style, ctx.row_style());
    }

    /// The badge rides the shared passive cluster, so it packs with the
    /// other decorations instead of reserving its own column.
    #[test]
    fn metered_badge_packs_into_the_passive_cluster() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.metered = true;
        ctx.has_notes = true;

        let text = cell_text(&cell_badges(&ctx));
        assert!(text.contains('$'), "metered badge missing: {text:?}");
        assert!(text.contains('✎'), "notes badge missing: {text:?}");
    }

    #[test]
    fn cell_fix_stays_compact_on_the_cursor_row() {
        let mut task = make_task("owner/repo#1", "x");
        task.review = ReviewStatus::None;
        task.ci = CiStatus::None;
        task.state = TaskState::Open;
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        let fix = format!(" {} ", crate::components::sidebar::FIX_GLYPH);
        assert_eq!(cell_fix(&ctx).width(), 0, "unarmed row has no FIX slot");
        ctx.is_cursor = true;
        ctx.auto_fix_ci_armed = true;
        assert_eq!(cell_fix(&ctx).spans[0].content.as_ref(), fix);
        ctx.auto_fix_conflict_armed = true;
        assert_eq!(cell_fix(&ctx).spans[0].content.as_ref(), fix);
        ctx.auto_fix_ci_armed = false;
        assert_eq!(cell_fix(&ctx).spans[0].content.as_ref(), fix);
        ctx.is_cursor = false;
        assert_eq!(cell_fix(&ctx).spans[0].content.as_ref(), fix);
    }

    #[test]
    fn focused_auto_fix_keeps_its_column_at_default_sidebar_width() {
        let theme = theme();
        let task0 = make_task("owner/repo#1", "Focused workspace");
        let task1 = make_task("owner/repo#2", "Another readable workspace");
        let ws0 = Workspace::from_task(task0.clone(), fixed_time());
        let ws1 = Workspace::from_task(task1.clone(), fixed_time());
        let mut focused = ctx_for(&ws0, &task0, &theme);
        focused.is_cursor = true;
        focused.auto_fix_ci_armed = true;
        focused.auto_fix_conflict_armed = true;
        let other = ctx_for(&ws1, &task1, &theme);

        let lines = crate::components::table::render_table(
            &[build_row(&focused), build_row(&other)],
            &build_columns(4),
            38,
        );
        let focused_line = line_text(&lines[0]);
        let other_line = line_text(&lines[1]);

        assert!(
            focused_line.contains(crate::components::sidebar::FIX_GLYPH),
            "{focused_line:?}"
        );
        assert!(
            other_line.contains("Another readable"),
            "focused auto-fix must not reserve a long blank column on sibling rows: {other_line:?}"
        );
    }

    /// The track-main badge (issue #535): empty when untracked, and a
    /// `⤓` glyph when tracked — calm accent when synced, warn-colored
    /// when the worktree fell behind and couldn't auto-sync (#1046).
    #[test]
    fn cell_track_main_reflects_tracked_and_behind_state() {
        let mut task = make_task("owner/repo#1", "x");
        task.review = ReviewStatus::None;
        task.ci = CiStatus::None;
        task.state = TaskState::Open;
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let track = format!(" {} ", crate::components::sidebar::TRACK_GLYPH);
        let mut ctx = ctx_for(&ws, &task, &theme);
        assert_eq!(
            cell_track_main(&ctx).width(),
            0,
            "untracked row has no track slot"
        );
        ctx.track_main = true;
        let synced = cell_track_main(&ctx);
        assert_eq!(synced.spans[0].content.as_ref(), track);
        assert_eq!(synced.spans[0].style.fg, Some(theme.accent));
        ctx.track_main_behind = true;
        let behind = cell_track_main(&ctx);
        assert_eq!(behind.spans[0].content.as_ref(), track);
        assert_eq!(
            behind.spans[0].style.fg,
            Some(theme.warn),
            "a stuck (behind) track-main flips to the warn color"
        );
    }

    /// A workspace carrying a local note surfaces a ` ✎ ` badge (issue
    /// #458) in its own slot even when it has no CI/review pill and no
    /// task at all — a session-less scratchpad still reads as noted
    /// (#524).
    #[test]
    fn cell_notes_shows_badge_without_task() {
        let ws = Workspace::empty(
            lazybox_core::WorkspaceKey("scratch".into()),
            "main",
            fixed_time(),
        );
        let theme = theme();
        let placeholder = make_task("owner/repo#1", "x");
        let mut ctx = ctx_for(&ws, &placeholder, &theme);
        // Task-less workspace: no CI/review pills possible.
        ctx.task = None;
        assert_eq!(cell_notes(&ctx).width(), 0, "no note, no badge");
        ctx.has_notes = true;
        let cell = cell_notes(&ctx);
        assert_eq!(cell.spans[0].content.as_ref(), " ✎ ");
    }

    /// A PR that's part of a stack surfaces a ` ⇗k/N ` badge (issue
    /// #969); a standalone PR (no `stack`) shows nothing.
    #[test]
    fn cell_stack_shows_position_badge() {
        let task = make_task("owner/repo#2", "child");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        assert_eq!(cell_stack(&ctx).width(), 0, "no stack, no badge");
        let stack = lazybox_core::StackPosition {
            parent: Some(lazybox_core::TaskId {
                source: "github".into(),
                key: "owner/repo#1".into(),
            }),
            children: vec![],
            position: 2,
            depth: 3,
        };
        ctx.stack = Some(&stack);
        let cell = cell_stack(&ctx);
        assert_eq!(cell.spans[0].content.as_ref(), " ⇗2/3 ");
    }

    /// A workspace that's been sent snippets surfaces a ` ]N ` badge
    /// (issue #463) in its own slot; a row with none shows nothing
    /// (#524).
    #[test]
    fn cell_snippet_shows_badge() {
        let mut ws = Workspace::empty(
            lazybox_core::WorkspaceKey("scratch".into()),
            "main",
            fixed_time(),
        );
        let theme = theme();
        let placeholder = make_task("owner/repo#1", "x");
        let mut ctx = ctx_for(&ws, &placeholder, &theme);
        ctx.task = None;
        assert_eq!(cell_snippet(&ctx).width(), 0, "no snippets, no badge");
        ctx.sent_snippet_count = 3;
        let cell = cell_snippet(&ctx);
        assert_eq!(cell.spans[0].content.as_ref(), " ]3 ");

        // Deliver more DISTINCT snippets than the MRU can hold: the MRU
        // saturates at SENT_SNIPPETS_MAX but the honest count keeps
        // climbing, so the badge shows every delivery — not the cap.
        let deliveries = lazybox_core::SENT_SNIPPETS_MAX + 1;
        for index in 0..deliveries {
            ws.record_snippet_delivery(format!("workflow-{index}"));
        }
        let mut capped = ctx_for(&ws, &placeholder, &theme);
        capped.sent_snippet_count = ws.sent_snippets.total();
        assert_eq!(
            cell_snippet(&capped).spans[0].content.as_ref(),
            format!(" ]{deliveries} ").as_str(),
            "the badge counts every delivery, past the MRU cap",
        );
    }

    /// The `⚡` merge-on-green badge rides in its own column ahead of the
    /// live CI glyph rather than replacing it — an armed PR with running/red
    /// CI shows both, in separate cells (#524, #1046).
    #[test]
    fn arm_badge_coexists_with_ci_pill() {
        let mut task = make_task("owner/repo#1", "x");
        task.ci = CiStatus::Failure;
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.auto_merge_armed = true;
        assert_eq!(cell_arm(&ctx).spans[0].content.as_ref(), " ⚡ ");
        assert!(
            cell_status(&ctx)
                .spans
                .iter()
                .any(|s| s.content.as_ref().contains('✗')),
            "live CI glyph still present alongside the arm"
        );
    }

    /// `cell_time` carries its own leading space, so when the status
    /// column collapses (no pills anywhere) the time still reads as
    /// `<title flex padding>` + 1-cell gap + `5m`, not jammed against
    /// the title's last character.
    #[test]
    fn snoozed_row_time_cell_shows_wake_time() {
        let task = make_task("owner/repo#1", "x");
        let mut ws = Workspace::from_task(task.clone(), fixed_time());
        ws.snoozed_until = Some(fixed_time() + chrono::Duration::hours(4));
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_time(&ctx);
        let text: String = cell.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            text.trim(),
            "⏾4h",
            "a snoozed row's time column is its wake time, not its age"
        );

        // Expired snooze → the normal activity timestamp comes back.
        ws.snoozed_until = Some(fixed_time() - chrono::Duration::hours(1));
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_time(&ctx);
        let text: String = cell.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            !text.contains('⏾'),
            "an expired snooze must not render a wake glyph"
        );
    }

    #[test]
    fn cell_time_emits_leading_space() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let cell = cell_time(&ctx);
        // First span is a single-cell row-style space.
        assert_eq!(cell.spans[0].content.as_ref(), " ");
    }

    fn make_stale_task(key: &str, title: &str) -> Task {
        let mut t = make_task(key, title);
        t.updated_at = fixed_time() - chrono::Duration::days(40);
        t
    }

    /// Issue #274, finding 1: age is measured from when the issue was
    /// opened, not last touched. An issue opened months ago but commented
    /// on today still reads as old (`Nmo`) and fades — `updated_at` alone
    /// would have shown a misleading `now`.
    #[test]
    fn old_issue_with_recent_activity_still_reads_as_stale() {
        let mut task = make_task("owner/repo#1", "old but chatty");
        task.created_at = Some(fixed_time() - chrono::Duration::days(90));
        task.updated_at = fixed_time(); // touched just now
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);

        assert!(ctx.is_stale_issue());
        assert_eq!(cell_text(&cell_time(&ctx)).trim(), "3mo");
        assert!(
            cell_title(&ctx).spans[0]
                .style
                .add_modifier
                .contains(Modifier::DIM)
        );
    }

    /// Issue #274: a recent issue keeps its last-activity display — the
    /// age anchor only kicks in once the issue is stale, so a 3-day-old
    /// issue commented on an hour ago still reads `1h`, not `3d`.
    #[test]
    fn recent_issue_keeps_activity_display() {
        let mut task = make_task("owner/repo#1", "young and active");
        task.created_at = Some(fixed_time() - chrono::Duration::days(3));
        task.updated_at = fixed_time() - chrono::Duration::hours(1);
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);

        assert!(!ctx.is_stale_issue());
        assert_eq!(cell_text(&cell_time(&ctx)).trim(), "1h");
        assert!(
            !cell_title(&ctx).spans[0]
                .style
                .add_modifier
                .contains(Modifier::DIM)
        );
    }

    /// Issue #274: an issue old enough to read in months fades — its
    /// title and age carry `DIM` so active rows draw the eye.
    #[test]
    fn stale_issue_title_and_time_are_dimmed() {
        let task = make_stale_task("owner/repo#1", "old issue");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);

        assert!(ctx.is_stale_issue());
        // Age renders in months.
        assert_eq!(cell_text(&cell_time(&ctx)).trim(), "1mo");

        let title = cell_title(&ctx);
        assert!(title.spans[0].style.add_modifier.contains(Modifier::DIM));
        let time = cell_time(&ctx);
        // The age span (after the leading space) is dimmed.
        assert!(time.spans[1].style.add_modifier.contains(Modifier::DIM));
    }

    /// A recent issue is untouched — no fade.
    #[test]
    fn fresh_issue_is_not_dimmed() {
        let task = make_task("owner/repo#1", "new issue");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);

        assert!(!ctx.is_stale_issue());
        let title = cell_title(&ctx);
        assert!(!title.spans[0].style.add_modifier.contains(Modifier::DIM));
    }

    /// PRs keep their own staleness cues (CI / review pills) — an old
    /// PR must NOT fade, or the fade would fight those signals.
    #[test]
    fn stale_pr_is_not_dimmed() {
        let mut task = make_stale_task("owner/repo#7", "old pr");
        task.url = "https://github.com/owner/repo/pull/7".into();
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);

        assert!(task.is_pr());
        assert!(!ctx.is_stale_issue());
        let title = cell_title(&ctx);
        assert!(!title.spans[0].style.add_modifier.contains(Modifier::DIM));
    }

    /// A PR shows recency of activity, not age: an old PR pushed to just
    /// now reads as `now`, keeping the time column aligned with the
    /// CI/review pills that key off activity.
    #[test]
    fn pr_time_tracks_activity_not_age() {
        let mut task = make_task("owner/repo#7", "old but active pr");
        task.url = "https://github.com/owner/repo/pull/7".into();
        task.created_at = Some(fixed_time() - chrono::Duration::days(90));
        task.updated_at = fixed_time();
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);

        assert!(task.is_pr());
        assert_eq!(cell_text(&cell_time(&ctx)).trim(), "now");
    }

    /// The cursor row's highlight fill must stay legible — the fade is
    /// suppressed there even when the issue is stale.
    #[test]
    fn cursor_row_suppresses_stale_fade() {
        let task = make_stale_task("owner/repo#1", "old issue");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.is_cursor = true;

        let title = cell_title(&ctx);
        assert!(!title.spans[0].style.add_modifier.contains(Modifier::DIM));
    }

    /// Regression for issue #22, part 1: title flex reclaims the
    /// 36+ trailing cells when no row has unread / badge / status
    /// content. The pre-fix table reserved 5 (unread) + 7 (badges) +
    /// 19 (status) + 1 (gutter) = 32 cells PER ROW even when every
    /// cell was empty, so a 100-cell sidebar effectively gave the
    /// title flex only ~42 cells and truncated long titles with `…`
    /// while leaving a huge gap to the right.
    #[test]
    fn title_flex_expands_when_trailing_columns_are_all_empty() {
        // Two open issues, both with NO unread / badges / CI / review.
        let task_a = make_task(
            "owner/repo#1",
            "Round-robin per-repo sync to reduce query overhead",
        );
        let task_b = make_task(
            "owner/repo#2",
            "Switch from polling-driven sync to notifications",
        );
        let ws_a = Workspace::from_task(task_a.clone(), fixed_time());
        let ws_b = Workspace::from_task(task_b.clone(), fixed_time());
        let theme = theme();
        let ctx_a = ctx_for(&ws_a, &task_a, &theme);
        let ctx_b = ctx_for(&ws_b, &task_b, &theme);
        let columns = build_columns(4);
        let rows = vec![build_row(&ctx_a), build_row(&ctx_b)];
        // Generous row budget (mimics a wide terminal's sidebar).
        let lines = crate::components::table::render_table(&rows, &columns, 100);
        // Both lines render. The title text should make it into the
        // line (truncated or not) — the bug was the title getting
        // chopped to ~42 cells with 30+ empty cells trailing it.
        let joined_a: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let joined_b: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        // Title body should appear in full when there's room. With
        // no trailing-column content, the longest natural title here
        // (`Round-robin per-repo sync to reduce query overhead`,
        // 50 cells) fits comfortably inside the 100-cell budget.
        assert!(
            joined_a.contains("Round-robin per-repo sync to reduce query overhead"),
            "title was truncated despite empty trailing columns: {joined_a:?}",
        );
        assert!(
            joined_b.contains("Switch from polling-driven sync to notifications"),
            "title was truncated despite empty trailing columns: {joined_b:?}",
        );
    }

    /// Regression for issue #22, part 2: a row WITHOUT a `C` badge
    /// (and no other right-side content) does not leave the badge
    /// column as a ragged gap. When at least one row has a `C`
    /// badge, every other row pads to the same column width so the
    /// `C` letters line up at the same x position across rows.
    #[test]
    fn badge_column_lines_up_across_rows() {
        // Row A: has a Claude agent badge. Row B: no badge.
        let task_a = make_task("owner/repo#1", "A");
        let task_b = make_task("owner/repo#2", "B");
        let ws_a = Workspace::from_task(task_a.clone(), fixed_time());
        let ws_b = Workspace::from_task(task_b.clone(), fixed_time());
        let theme = theme();
        let mut ctx_a = ctx_for(&ws_a, &task_a, &theme);
        ctx_a.badges = vec![('C', 1)];
        let ctx_b = ctx_for(&ws_b, &task_b, &theme);
        let columns = build_columns(4);
        let rows = vec![build_row(&ctx_a), build_row(&ctx_b)];
        let lines = crate::components::table::render_table(&rows, &columns, 80);
        let row_a: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let row_b: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        // Same total visible width across rows — that's what makes
        // every fixed-position column (incl. the trailing time
        // column) align across rows.
        let width_a = crate::util::visual_width(&row_a);
        let width_b = crate::util::visual_width(&row_b);
        assert_eq!(
            width_a, width_b,
            "rows must render to the same total width or trailing columns drift: {row_a:?} vs {row_b:?}",
        );
        // Find the `C` glyph in row A; row B must have a space at
        // the SAME char/cell offset, not anything else. The type
        // glyph (`⇄` / `○`) is multi-byte UTF-8, so anchor by char
        // position — `str::find` is byte-based and would drift.
        let row_a_chars: Vec<char> = row_a.chars().collect();
        let c_pos = row_a_chars
            .windows(3)
            .position(|w| w == [' ', 'C', ' '])
            .expect("row A should have a ` C ` badge");
        let same_window: String = row_b.chars().skip(c_pos).take(3).collect();
        assert_eq!(
            same_window, "   ",
            "row B did not reserve the ` C ` column at the same x as row A",
        );
    }

    /// #1015: a Linear ticket that is neither assigned to nor created
    /// by the viewer is the "why is this here?" anomaly, so its row is
    /// flagged with a warn `?` rather than the quiet dim `·` a benign
    /// GitHub mention gets. Legitimate Linear rows keep `A` / `@`, and
    /// the flag is Linear-only — a GitHub mention is untouched.
    #[test]
    fn linear_neither_assigned_nor_created_flags_the_row() {
        let theme = theme();
        let columns = build_columns(4);

        let mut anomaly = make_task("OBI-9", "linear anomaly");
        anomaly.id.source = "linear".into();
        anomaly.role = TaskRole::Mentioned;
        let ws_a = Workspace::from_task(anomaly.clone(), fixed_time());
        let ctx_a = ctx_for(&ws_a, &anomaly, &theme);

        let mut assigned = make_task("OBI-10", "linear assigned");
        assigned.id.source = "linear".into();
        assigned.role = TaskRole::Assignee;
        let ws_b = Workspace::from_task(assigned.clone(), fixed_time());
        let ctx_b = ctx_for(&ws_b, &assigned, &theme);

        // A GitHub mention is a legitimate, benign state — it must keep
        // the dim `·`, not get flagged.
        let mut gh_mention = make_task("owner/repo#3", "gh mention");
        gh_mention.role = TaskRole::Mentioned;
        let ws_c = Workspace::from_task(gh_mention.clone(), fixed_time());
        let ctx_c = ctx_for(&ws_c, &gh_mention, &theme);

        let rows = vec![build_row(&ctx_a), build_row(&ctx_b), build_row(&ctx_c)];
        let lines = crate::components::table::render_table(&rows, &columns, 80);
        let joined =
            |i: usize| -> String { lines[i].spans.iter().map(|s| s.content.as_ref()).collect() };
        let (line_a, line_b, line_c) = (joined(0), joined(1), joined(2));

        assert!(
            line_a.contains('?'),
            "linear anomaly row must be flagged with `?`: {line_a:?}"
        );
        assert!(
            line_b.contains('@') && !line_b.contains('?'),
            "assigned linear ticket keeps `@`, not the flag: {line_b:?}"
        );
        assert!(
            line_c.contains('·') && !line_c.contains('?'),
            "github mention keeps the dim `·` and is never flagged: {line_c:?}"
        );
    }

    /// Regression for issue #130: a wide pane keeps every column —
    /// the narrow-width shedding must NOT kick in when everything
    /// fits. Status pill, time, and the full title all render.
    #[test]
    fn wide_width_keeps_status_time_and_title() {
        let mut task = make_task("owner/repo#42", "Fix the broken sidebar layout");
        task.ci = CiStatus::Failure; // `✗` CI-fail status glyph
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let columns = build_columns(2);
        let rows = vec![build_row(&ctx)];
        let lines = crate::components::table::render_table(&rows, &columns, 100);
        let line: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line.contains("42"), "number missing on wide pane: {line:?}");
        assert!(
            line.contains("Fix the broken sidebar layout"),
            "title truncated on wide pane: {line:?}",
        );
        assert!(
            line.contains('✗'),
            "status glyph missing on wide pane: {line:?}"
        );
    }

    /// Regression for issue #328: at a narrow width the CI status is
    /// KEPT — it's the actionable signal, so it sheds nearly last —
    /// while the timestamp is the first column to go. (Before the
    /// shed-priority swap the status pill dropped out ahead of the
    /// less-important columns, exactly backwards.) With the compact icon
    /// (#1046) the status barely costs anything, so the squeeze only
    /// bites at a tighter width than the old 9-cell pill needed.
    #[test]
    fn narrow_width_keeps_status_and_sheds_time_first() {
        let mut task = make_task("owner/repo#42", "Fix the broken sidebar layout");
        task.ci = CiStatus::Failure;
        task.updated_at = fixed_time() - chrono::Duration::minutes(5); // " 5m"
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let columns = build_columns(2);
        let rows = vec![build_row(&ctx)];
        let lines = crate::components::table::render_table(&rows, &columns, 30);
        let line: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            line.contains("42"),
            "item number dropped at narrow width: {line:?}"
        );
        assert!(
            line.contains("Fix the broken"),
            "title squeezed to nothing at narrow width: {line:?}",
        );
        assert!(
            line.contains('✗'),
            "status must survive — it now sheds nearly last: {line:?}",
        );
        assert!(
            !line.contains("5m"),
            "the timestamp should shed first at this width: {line:?}",
        );
    }

    /// Regression for issue #269: narrowing the sidebar must not drop
    /// the CI status column while the title is short enough to leave
    /// room for it. The old budgeter reserved the title's full 20-cell
    /// `min` and shed the status column to fund padding it never filled
    /// — losing the CI signal AND leaving a blank gap. A 7-cell title
    /// at width 44 has ample room for the 19-cell status column.
    #[test]
    fn short_title_keeps_status_instead_of_leaving_empty_gap() {
        let mut task = make_task("owner/repo#7", "Fix bug");
        task.ci = CiStatus::Failure; // `✗` CI-fail status glyph
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let columns = build_columns(2);
        let rows = vec![build_row(&ctx)];
        let lines = crate::components::table::render_table(&rows, &columns, 44);
        let line: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(line.contains('7'), "item number dropped: {line:?}");
        assert!(
            line.contains("Fix bug"),
            "short title should render in full: {line:?}",
        );
        assert!(
            line.contains('✗'),
            "status must survive when the short title leaves room: {line:?}",
        );
    }

    /// Regression for issue #130: at a tiny width even the role /
    /// state indicators shed, and the title finally elides with `…` —
    /// but the item number is still there, so the row is identifiable
    /// (the bug rendered `⬤ … C` with no number/title at all).
    #[test]
    fn tiny_width_preserves_number_and_elides_title_last() {
        let mut task = make_task("owner/repo#42", "Fix the broken sidebar layout");
        task.ci = CiStatus::Failure;
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.asking = true; // state slot wants width too
        let columns = build_columns(2);
        let rows = vec![build_row(&ctx)];
        let lines = crate::components::table::render_table(&rows, &columns, 22);
        let line: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            line.contains("42"),
            "number must survive at tiny width: {line:?}"
        );
        assert!(
            line.contains("Fix the"),
            "title head must survive: {line:?}"
        );
        assert!(
            line.contains('…'),
            "title should elide at tiny width: {line:?}"
        );
        assert!(!line.contains('✗'), "status glyph must be gone: {line:?}");
        assert!(
            !line.contains('?'),
            "state slot must shed before the title: {line:?}"
        );
    }

    /// Labels render as bracketed chips with one leading space per
    /// chip. No labels → no spans, so a tag-less title cell is just
    /// the title with no reserved label width (#329).
    #[test]
    fn label_spans_empty_for_taskless_or_unlabeled_row() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        assert!(label_spans(&ctx).is_empty());
        // Tag-less title cell: one span, no atomic tail.
        let title = cell_title(&ctx);
        assert_eq!(title.spans.len(), 1);
        assert_eq!(title.atomic_tail, 0);
    }

    #[test]
    fn label_spans_render_bracketed_chips() {
        let mut task = make_task("owner/repo#1", "x");
        task.labels = vec![
            lazybox_core::Label::new("bug"),
            lazybox_core::Label::new("ci"),
        ];
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let joined: String = label_spans(&ctx)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(joined, " [bug] [ci]");
        // They ride at the tail of the title cell, tagged as the atomic
        // drop unit — 2 chips × (space, `[`, name, `]`) = 8 spans.
        let title = cell_title(&ctx);
        let joined: String = title.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "x [bug] [ci]");
        assert_eq!(title.atomic_tail, 8);
    }

    /// More than 3 labels collapses extras into a `+N` overflow
    /// indicator — the issue's "graceful truncation" requirement.
    #[test]
    fn label_spans_truncate_with_overflow_indicator() {
        let mut task = make_task("owner/repo#1", "x");
        task.labels = vec![
            lazybox_core::Label::new("bug"),
            lazybox_core::Label::new("ci"),
            lazybox_core::Label::new("backend"),
            lazybox_core::Label::new("priority"),
            lazybox_core::Label::new("docs"),
        ];
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let joined: String = label_spans(&ctx)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(joined, " [bug] [ci] [backend] +2");
    }

    /// A stale issue's fade now reaches its inline chips (#329): with
    /// the chips in the title cell, a full-color label beside a dimmed
    /// title would fight the "old, skip me" cue — so the visible chip
    /// spans dim too, but not on the cursor row.
    #[test]
    fn stale_issue_labels_are_dimmed_off_cursor() {
        let mut task = make_stale_task("owner/repo#1", "old issue");
        task.labels = vec![lazybox_core::Label::new("bug")];
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);

        assert!(ctx.is_stale_issue());
        // The inter-chip separator keeps the plain row style (it's
        // invisible); every visible chip span carries DIM.
        assert!(
            label_spans(&ctx)
                .iter()
                .filter(|s| !s.content.trim().is_empty())
                .all(|s| s.style.add_modifier.contains(Modifier::DIM)),
            "stale issue's label chips should be dimmed",
        );

        ctx.is_cursor = true;
        assert!(
            label_spans(&ctx)
                .iter()
                .all(|s| !s.style.add_modifier.contains(Modifier::DIM)),
            "cursor row must not dim its label chips",
        );
    }

    /// A fresh (non-stale) labelled row keeps full-color chips.
    #[test]
    fn fresh_issue_labels_are_not_dimmed() {
        let mut task = make_task("owner/repo#1", "new issue");
        task.labels = vec![lazybox_core::Label::new("bug")];
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);

        assert!(!ctx.is_stale_issue());
        assert!(
            label_spans(&ctx)
                .iter()
                .all(|s| !s.style.add_modifier.contains(Modifier::DIM)),
            "fresh issue's label chips should not be dimmed",
        );
    }

    /// Regression for issue #329: a tag-less row must NOT lose title
    /// width to another row's labels. Two rows go through one
    /// `render_table` call (as the sidebar does): one carries long
    /// `[dependencies] [go]` chips, the other has none. The label-less
    /// row's title has to render in full — the pre-fix global `Max`
    /// label column reserved the widest label cell on every row, so
    /// the tag-less title elided with `…` for no visible reason.
    #[test]
    fn tagless_title_not_truncated_by_another_rows_labels() {
        let theme = theme();
        let long = "Round-robin per-repo sync to reduce overhead";
        let mut labelled = make_task("owner/repo#1", "chore: bump deps");
        labelled.labels = vec![
            lazybox_core::Label::new("dependencies"),
            lazybox_core::Label::new("go"),
        ];
        let tagless = make_task("owner/repo#2", long);
        let ws_a = Workspace::from_task(labelled.clone(), fixed_time());
        let ws_b = Workspace::from_task(tagless.clone(), fixed_time());
        let ctx_a = ctx_for(&ws_a, &labelled, &theme);
        let ctx_b = ctx_for(&ws_b, &tagless, &theme);
        let columns = build_columns(4);
        let rows = vec![build_row(&ctx_a), build_row(&ctx_b)];
        let lines = crate::components::table::render_table(&rows, &columns, 62);
        let tagless_line: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            tagless_line.contains(long),
            "tag-less title truncated by another row's labels: {tagless_line:?}",
        );
        assert!(
            !tagless_line.contains('…'),
            "tag-less title should not elide: {tagless_line:?}",
        );
    }

    /// Issue #328, part 1: common verbose label names are aliased to a
    /// short form, and anything else past `MAX_CHIP_LEN` is capped with
    /// a trailing `…` so no one chip dominates the row.
    #[test]
    fn abbreviate_label_aliases_and_caps() {
        assert_eq!(abbreviate_label("dependencies"), "deps");
        assert_eq!(abbreviate_label("Dependencies"), "deps"); // case-insensitive
        assert_eq!(abbreviate_label("documentation"), "docs");
        assert_eq!(abbreviate_label("go"), "go"); // short, untouched
        // A long, non-aliased name caps at MAX_CHIP_LEN cells incl. `…`.
        let capped = abbreviate_label("github_actions");
        assert_eq!(capped, "github_ac…");
        assert_eq!(capped.chars().count(), MAX_CHIP_LEN);
    }

    /// Issue #328, part 1: the row from the screenshot — `[dependencies]
    /// [go]` — collapses to `[deps] [go]` so a Dependabot-heavy list
    /// stops eating the title's width.
    #[test]
    fn label_spans_abbreviate_long_names() {
        let mut task = make_task("owner/repo#1", "x");
        task.labels = vec![
            lazybox_core::Label::new("dependencies"),
            lazybox_core::Label::new("go"),
        ];
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let joined: String = label_spans(&ctx)
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(joined, " [deps] [go]");
    }

    /// Issue #328, part 2: under width pressure the labels shed before
    /// the conflict / CI status — the tags are the least important thing
    /// on the row, the merge-conflict signal is the most. A width that
    /// can't fit both keeps the `⚠` conflict glyph and drops the chips.
    #[test]
    fn narrow_width_sheds_labels_before_status() {
        let mut task = make_task("owner/repo#42", "Fix bug");
        task.mergeable = lazybox_core::Mergeable::Conflicting; // `⚠`
        task.labels = vec![
            lazybox_core::Label::new("bug"),
            lazybox_core::Label::new("ci"),
        ];
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let columns = build_columns(2);
        let rows = vec![build_row(&ctx)];
        let lines = crate::components::table::render_table(&rows, &columns, 28);
        let line: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            line.contains('⚠'),
            "the actionable status must survive: {line:?}",
        );
        assert!(
            !line.contains("[bug]") && !line.contains("[ci]"),
            "labels should shed before the status glyph: {line:?}",
        );
        assert!(line.contains("Fix bug"), "title dropped: {line:?}");
    }

    /// #813 / #1046 regression: the badges pack into two priority tiers,
    /// not one atomic cluster, so they shed *graduated* under width
    /// pressure — the low-signal passive-info badges (`⤓`) drop first, the
    /// merge-when-green arms (`⚡`/`◆`) outlive them, and the actionable CI
    /// status glyph outlives every badge. #813 packed all seven into one
    /// cell (regressing the graduated order); #1046 additionally raised the
    /// merge-arm tier above the unread count so the "will it merge itself"
    /// signal reliably shows. Asserted via a width sweep — the compact
    /// icons make the exact shed widths glyph-width-dependent, so we pin the
    /// ORDER (which token needs the most room to appear, hence sheds first)
    /// rather than three hardcoded widths.
    #[test]
    fn merge_arms_outlive_passive_badges_then_status_survives() {
        let mut task = make_task("owner/repo#1", "Readable title text here");
        task.state = TaskState::Open;
        task.ci = CiStatus::Failure; // `✗` status glyph
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.track_main = true; // ⤓ — passive-info tier
        ctx.auto_merge_armed = true; // ⚡ — merge-arm tier
        ctx.auto_merge_enabled = true; // ◆ — merge-arm tier

        let columns = build_columns(4);
        let render = |w: usize| -> String {
            let rows = vec![build_row(&ctx)];
            let lines = crate::components::table::render_table(&rows, &columns, w);
            lines[0].spans.iter().map(|s| s.content.as_ref()).collect()
        };
        // Narrowest width at which `needle` still renders. A token that
        // needs a wider pane to appear sheds first under pressure.
        let present_from = |needle: char| -> usize {
            (10..=120)
                .find(|&w| render(w).contains(needle))
                .unwrap_or(usize::MAX)
        };
        let passive = present_from('⤓');
        let arm = present_from('⚡');
        let auto = present_from('◆');
        let status = present_from('✗');

        // Everything shows on a wide pane.
        assert!(
            render(120).contains('⤓')
                && render(120).contains('⚡')
                && render(120).contains('◆')
                && render(120).contains('✗'),
            "all badges + status visible when wide: {:?}",
            render(120),
        );
        // The two merge arms shed together.
        assert_eq!(arm, auto, "ARM and AUTO shed at the same width");
        // Passive decoration sheds before the merge arms…
        assert!(
            passive > arm,
            "passive `⤓` must shed before the merge arms (passive_from={passive}, arm_from={arm})",
        );
        // …and the merge arms shed before the actionable CI status.
        assert!(
            arm > status,
            "merge arms must outlive nothing below status but shed before it \
             (arm_from={arm}, status_from={status})",
        );
    }

    /// Issue #328, part 3: the status pill is trimmed to its ` LABEL `
    /// block, so a single clean gap — the pill's own trailing space plus
    /// the time cell's leading space — separates the CI status from the
    /// duration, instead of the old baked-in trailing padding stacking
    /// with the column gap and the time's leading space.
    #[test]
    fn status_pill_sits_one_clean_gap_off_the_time() {
        let mut task = make_task("owner/repo#42", "Fix bug");
        task.ci = CiStatus::Failure;
        task.updated_at = fixed_time() - chrono::Duration::minutes(5); // "5m"
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let ctx = ctx_for(&ws, &task, &theme);
        let columns = build_columns(4);
        let rows = vec![build_row(&ctx)];
        let lines = crate::components::table::render_table(&rows, &columns, 80);
        let line: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            line.contains("✗ 5m"),
            "expected a single clean gap between status and time: {line:?}",
        );
        assert!(
            !line.contains("✗  5m"),
            "status↔time gap is still oversized: {line:?}",
        );
    }

    /// Multi-instance badge (` C×2 `, 5 cells) no longer gets
    /// truncated to `… ` because the badge column is `Column::max(0)`
    /// and expands to the widest natural cell across the table.
    #[test]
    fn multi_instance_badge_is_not_truncated() {
        let task = make_task("owner/repo#1", "x");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.badges = vec![('C', 2)];
        let columns = build_columns(4);
        let rows = vec![build_row(&ctx)];
        let lines = crate::components::table::render_table(&rows, &columns, 80);
        let line: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            line.contains(" C×2 "),
            "multi-instance badge was truncated: {line:?}",
        );
    }

    /// A persisted multi-agent workspace keeps its complete badge set
    /// and jump number at the default 40-column sidebar width (38 cells
    /// inside the border).
    #[test]
    fn mixed_agent_badges_are_not_truncated_under_width_pressure() {
        let task = make_task("owner/repo#1", "Readable workspace title");
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let theme = theme();
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.badges = vec![('C', 2), ('X', 1)];
        ctx.agent_number = Some(1);
        let columns = build_columns(4);
        let rows = vec![build_row(&ctx)];
        let lines = crate::components::table::render_table(&rows, &columns, 38);
        let line: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            line.contains(" 1C×2X"),
            "mixed agent badges or jump number were truncated: {line:?}",
        );
        assert!(
            line.contains("Readable workspace"),
            "agent width made the title unreadable: {line:?}",
        );
    }

    /// Regression for issues #65 and #94: the type glyph must sit one
    /// space off the `NNN` on every row, regardless of how wide that
    /// row's number is. The bug was a right-aligned pr-number column:
    /// it padded shorter numbers on the LEFT, so `⇄ 312` rendered tight
    /// while `○ 7` picked up extra leading spaces (`○   7`) —
    /// inconsistent post-glyph spacing across rows. Left alignment moves
    /// the padding to the right, keeping the glyph→number gap a constant
    /// single space everywhere. (The `#` prefix itself was dropped in
    /// issue #67; the glyph now carries the issue-vs-PR signal.)
    #[test]
    fn type_glyph_has_single_space_before_number_for_mixed_widths() {
        let theme = theme();
        // Mixed glyph types AND mixed number widths (1/2/3 digits) so
        // `max_pr_num_width` is driven by `312` (3 cells). The narrower
        // rows are the ones the old right-align padded on the left; the
        // issue (`○`) row exercises the non-PR glyph path too, so a
        // regression can't hide on one glyph variant.
        let cases: [(Task, &str); 3] = [
            (make_task("owner/repo#7", "x"), "○ 7"), // issue, 1 digit
            (pr_task("owner/repo", 42), "⇄ 42"),     // PR, 2 digits
            (pr_task("owner/repo", 312), "⇄ 312"),   // PR, 3 digits
        ];
        let workspaces: Vec<Workspace> = cases
            .iter()
            .map(|(task, _)| Workspace::from_task(task.clone(), fixed_time()))
            .collect();
        let ctxs: Vec<WorkspaceRowCtx<'_>> = workspaces
            .iter()
            .zip(cases.iter())
            .map(|(ws, (task, _))| ctx_for(ws, task, &theme))
            .collect();
        let columns = build_columns(3); // width of "312"
        let rows: Vec<Row> = ctxs.iter().map(build_row).collect();
        let lines = crate::components::table::render_table(&rows, &columns, 80);
        for (line, (_, expected)) in lines.iter().zip(cases.iter()) {
            let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                joined.contains(expected),
                "type glyph must sit one space off the number; \
                 wanted {expected:?} in {joined:?}",
            );
        }
    }

    /// Companion to the above for the "without a task" case in the
    /// acceptance criteria: a scratch workspace (no PR / issue / linear
    /// ticket) renders the same total width as task rows so the title
    /// and trailing columns stay aligned across the mixed list — the
    /// empty type / number cells are padded by their fixed columns, not
    /// dropped.
    #[test]
    fn taskless_row_keeps_alignment_with_task_rows() {
        let theme = theme();
        let task = pr_task("owner/repo", 312);
        let ws_task = Workspace::from_task(task.clone(), fixed_time());
        let ctx_task = ctx_for(&ws_task, &task, &theme);

        let ws_scratch = Workspace::empty(
            lazybox_core::WorkspaceKey("scratch-branch".into()),
            "main",
            fixed_time(),
        );
        let ctx_scratch = WorkspaceRowCtx {
            recently_woken: false,
            source_quiet: false,
            workspace: Some(&ws_scratch),
            task: None,
            theme: &theme,
            now: fixed_time(),
            focused: false,
            is_cursor: false,
            is_selected: false,
            max_pr_num_width: 4,
            asking: false,
            limit_reached: false,
            awaiting_reset: false,
            credit_exhausted: false,
            working: false,
            done: false,
            exited: false,
            working_glyph: working_glyph(0),
            spawning: false,
            spawning_glyph: spawning_glyph(0),
            badges: vec![],
            agent_models: vec![],
            agent_number: None,
            ascii_glyphs: false,
            auto_merge_armed: false,
            auto_merge_enabled: false,
            auto_fix_ci_armed: false,
            auto_fix_conflict_armed: false,
            track_main: false,
            track_main_behind: false,
            metered: false,
            has_notes: false,
            sent_snippet_count: 0,
            ticket_tree: None,
            stack: None,
            model_shorts: empty_shorts(),
            highlight_query: None,
            repo_prefix: None,
        };
        let columns = build_columns(4);
        let rows = vec![build_row(&ctx_task), build_row(&ctx_scratch)];
        let lines = crate::components::table::render_table(&rows, &columns, 80);
        let task_line: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        let scratch_line: String = lines[1].spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(
            crate::util::visual_width(&task_line),
            crate::util::visual_width(&scratch_line),
            "taskless row must render to the same width as a task row: {task_line:?} vs {scratch_line:?}",
        );
    }

    /// Build a linked (no-worktree) workspace so `cell_linked` renders
    /// its `⎇ local` badge.
    fn linked_ws(name: &str) -> Workspace {
        let mut ws = Workspace::empty(lazybox_core::WorkspaceKey::new(name), "main", fixed_time());
        ws.linked_checkout = Some(std::path::PathBuf::from("/home/dev/code/local"));
        ws
    }

    /// Concatenate a rendered line into its visible string.
    fn line_text(line: &ratatui::text::Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// #813: the seven per-badge columns (#524) are packed into two
    /// clusters. The passive-info cell packs linked → notes → snippet →
    /// track → fix contiguously; the merge-arm cell packs arm → auto. A
    /// badge-less row carries none of them — no per-type slot is reserved
    /// on rows that don't use it.
    #[test]
    fn passive_badges_pack_into_two_clusters() {
        let theme = theme();
        // Row 0: linked / notes / snippet / fix / arm. Row 1: just notes.
        // Row 2: no passive badges.
        let task0 = make_task("owner/repo#1", "all badges");
        let task1 = make_task("owner/repo#2", "one badge");
        let task2 = make_task("owner/repo#3", "no badges");
        let ws0 = linked_ws("all");
        let ws1 = Workspace::from_task(task1.clone(), fixed_time());
        let ws2 = Workspace::from_task(task2.clone(), fixed_time());

        let mut ctx0 = ctx_for(&ws0, &task0, &theme);
        ctx0.has_notes = true;
        ctx0.sent_snippet_count = 2;
        ctx0.auto_merge_armed = true;
        ctx0.auto_fix_ci_armed = true;
        ctx0.auto_fix_conflict_armed = true;
        let mut ctx1 = ctx_for(&ws1, &task1, &theme);
        ctx1.has_notes = true;
        let ctx2 = ctx_for(&ws2, &task2, &theme);

        // Passive-info cluster packs the low-signal badges; the merge arms
        // live in their own cell so they can outlive the decoration.
        let info: String = cell_badges(&ctx0)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(info, " ⎇ local  ✎  ]2  🔧 ");
        let arms: String = cell_merge_arms(&ctx0)
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(arms, " ⚡ ");

        let columns = build_columns(4);
        let rows = vec![build_row(&ctx0), build_row(&ctx1), build_row(&ctx2)];
        let lines = crate::components::table::render_table(&rows, &columns, 100);
        let l0 = line_text(&lines[0]);
        let l1 = line_text(&lines[1]);
        let l2 = line_text(&lines[2]);

        // Uniform cluster columns → rows still render to equal total width,
        // so the status / time trailer stays aligned across rows.
        let w0 = crate::util::visual_width(&l0);
        assert_eq!(w0, crate::util::visual_width(&l1), "{l0:?} vs {l1:?}");
        assert_eq!(w0, crate::util::visual_width(&l2), "{l0:?} vs {l2:?}");

        // The all-badges row shows both clusters, arms right of the info;
        // the badge-less row shows none of them.
        assert!(l0.contains(" ⎇ local  ✎  ]2  🔧  ⚡ "), "{l0:?}");
        assert!(l1.contains('✎'), "{l1:?}");
        assert!(
            !l2.contains('✎') && !l2.contains('⎇') && !l2.contains('⚡'),
            "badge-less row must carry no passive badges: {l2:?}",
        );
    }

    /// #813: both packed clusters are right-aligned, so a row with fewer
    /// badges pads on the left and its badges hug the same right edge —
    /// nearest the status / time trailer — as a row carrying more.
    #[test]
    fn passive_badge_cluster_is_right_aligned() {
        let theme = theme();
        let task0 = make_task("owner/repo#1", "a");
        let task1 = make_task("owner/repo#2", "b");
        let ws0 = linked_ws("wide");
        let ws1 = Workspace::from_task(task1.clone(), fixed_time());
        let mut ctx0 = ctx_for(&ws0, &task0, &theme);
        ctx0.has_notes = true; // wider info cluster: ⎇ local + ✎
        ctx0.auto_merge_armed = true;
        let mut ctx1 = ctx_for(&ws1, &task1, &theme);
        ctx1.auto_merge_armed = true; // no info cluster, just the ⚡ arm

        let columns = build_columns(4);
        let rows = vec![build_row(&ctx0), build_row(&ctx1)];
        let lines = crate::components::table::render_table(&rows, &columns, 100);
        let l0 = line_text(&lines[0]);
        let l1 = line_text(&lines[1]);

        // Both rows end their trailing `⚡` at the same offset: the
        // narrower cluster is padded on the LEFT to the shared column
        // width, keeping the arm flush-right.
        let arm_end = |s: &str| s.find('⚡').map(|b| s[..b].chars().count());
        assert_eq!(
            arm_end(&l0),
            arm_end(&l1),
            "cluster not right-aligned: {l0:?} vs {l1:?}",
        );
    }

    /// Issue #524: a badge type with zero occupants collapses to 0 width
    /// (matching `cell_status`), handing the slack back to the title —
    /// unused badge slots steal no room. Adding a snippet badge to one
    /// row is what makes another row's long title lose that slot's width.
    #[test]
    fn unused_badge_column_collapses_and_frees_title_width() {
        let theme = theme();
        let long = "Round-robin per-repo sync to reduce query overhead"; // 50 cells
        let short = make_task("owner/repo#1", "x");
        let titled = make_task("owner/repo#2", long);
        let ws_short = Workspace::from_task(short.clone(), fixed_time());
        let ws_titled = Workspace::from_task(titled.clone(), fixed_time());

        // Width tuned so the long title fits exactly while every passive
        // badge column is collapsed, but a single 4-cell snippet slot
        // tips it into truncation.
        const BUDGET: usize = 68;

        // No passive badges anywhere → snippet column collapses, title fits.
        let ctx_short = ctx_for(&ws_short, &short, &theme);
        let ctx_titled = ctx_for(&ws_titled, &titled, &theme);
        let columns = build_columns(4);
        let rows = vec![build_row(&ctx_short), build_row(&ctx_titled)];
        let lines = crate::components::table::render_table(&rows, &columns, BUDGET);
        assert!(
            line_text(&lines[1]).contains(long),
            "long title should fit when badge columns collapse: {:?}",
            line_text(&lines[1]),
        );

        // Give the OTHER row a snippet badge: its column now reserves 4
        // cells for every row, so the long title loses that width.
        let mut ctx_short_snip = ctx_for(&ws_short, &short, &theme);
        ctx_short_snip.sent_snippet_count = 2;
        let rows = vec![build_row(&ctx_short_snip), build_row(&ctx_titled)];
        let lines = crate::components::table::render_table(&rows, &columns, BUDGET);
        assert!(
            !line_text(&lines[1]).contains(long),
            "occupied snippet column must steal width from the title: {:?}",
            line_text(&lines[1]),
        );
    }

    #[test]
    fn ci_match_range_is_case_insensitive_and_byte_correct() {
        assert_eq!(ci_match_range("Add Search bar", "search"), Some(4..10));
        assert_eq!(ci_match_range("Add Search bar", "ADD"), Some(0..3));
        assert_eq!(ci_match_range("Add Search bar", "bar"), Some(11..14));
        assert_eq!(ci_match_range("Add Search bar", "xyz"), None);
        assert_eq!(ci_match_range("anything", ""), None);
    }

    #[test]
    fn ci_match_range_stays_correct_and_panic_free_under_case_fold_skew() {
        // A grow-on-fold (İ → i̇, +1 byte) skews later offsets: naively
        // mapping the lowercased match back would highlight "tanb", so the
        // helper must decline instead.
        assert_eq!(ci_match_range("İstanbul", "stan"), None);
        // A net-zero fold (ẞ→ß shrinks −1, İ→i̇ grows +1) leaves total length
        // equal but skews an interior boundary — the lowercased offset lands
        // mid-codepoint in the original. Must return None, never panic.
        assert_eq!(ci_match_range("ẞxİy", "x"), None);
        // A length-preserving non-ASCII fold still highlights the right span.
        assert_eq!(ci_match_range("Café", "café"), Some(0..5));
    }

    #[test]
    fn title_spans_underlines_only_the_matched_substring() {
        let theme = theme();
        let base = Style::default().fg(theme.text_dim);
        let spans = title_spans("Add Search bar", Some("search"), base, &theme);
        assert_eq!(spans.len(), 3, "before / match / after: {spans:?}");
        assert_eq!(spans[0].content, "Add ");
        assert_eq!(spans[1].content, "Search");
        assert!(
            spans[1].style.add_modifier.contains(Modifier::UNDERLINED),
            "the matched span is underlined",
        );
        assert_eq!(spans[1].style.fg, Some(theme.accent));
        assert_eq!(spans[2].content, " bar");
        // The unmatched flanks keep the base style untouched.
        assert_eq!(spans[0].style.fg, base.fg);
        assert_eq!(spans[2].style.fg, base.fg);
    }

    #[test]
    fn title_spans_is_one_plain_span_without_a_match() {
        let theme = theme();
        let base = Style::default().fg(theme.text_dim);
        assert_eq!(
            title_spans("Add Search bar", None, base, &theme).len(),
            1,
            "no query → no split",
        );
        assert_eq!(
            title_spans("Add Search bar", Some("zzz"), base, &theme).len(),
            1,
            "a non-matching query → no split",
        );
    }

    /// A highlighted (multi-span) title must survive the table's column
    /// truncation: at a wide budget the whole match is underlined, and at a
    /// narrow budget the row elides with `…` and stays within budget —
    /// never panics or overflows (#1099).
    #[test]
    fn highlighted_title_truncates_within_budget() {
        let theme = theme();
        let title = "Refactor the search indexer and cache eviction policy";
        let task = make_task("owner/repo#1", title);
        let ws = Workspace::from_task(task.clone(), fixed_time());
        let mut ctx = ctx_for(&ws, &task, &theme);
        ctx.highlight_query = Some("search");
        let columns = build_columns(4);
        let rows = vec![build_row(&ctx)];

        let underlined = |line: &ratatui::text::Line<'_>| -> String {
            line.spans
                .iter()
                .filter(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
                .map(|s| s.content.to_string())
                .collect()
        };

        // Wide: the whole match is underlined and the title is intact.
        let wide = crate::components::table::render_table(&rows, &columns, 100);
        assert!(
            line_text(&wide[0]).contains(title),
            "full title at width 100"
        );
        assert_eq!(underlined(&wide[0]), "search", "the match is underlined");

        // Narrow: the title elides, the row stays within budget, and the
        // underlined fragment (if any) is still a prefix of the match — the
        // multi-span head truncated cleanly rather than panicking.
        let narrow = crate::components::table::render_table(&rows, &columns, 22);
        let text = line_text(&narrow[0]);
        assert!(
            text.contains('…'),
            "narrow budget elides the title: {text:?}"
        );
        assert!(
            crate::util::visual_width(&text) <= 22,
            "row must not exceed budget: {text:?}",
        );
        let frag = underlined(&narrow[0]);
        assert!(
            "search".starts_with(frag.as_str()),
            "the underlined fragment is a clean prefix of the match: {frag:?}",
        );
    }
}
