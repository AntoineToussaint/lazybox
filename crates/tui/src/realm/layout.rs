//! `LayoutCtx` — split percentages, drag state, and the splitter math
//! the run loop hands keys + mouse events into. Extracted from
//! `Model` to keep that struct focused on orchestration; `LayoutCtx`
//! is pure data + arithmetic with no IPC, modal, or pane coupling.
//!
//! The two percentages drive the same three-rect layout the rest of
//! the TUI consumes (`pane_areas`). Callers mutate via `update_drag`
//! / `nudge_splits` and read `(sidebar_pct, right_top_pct)` straight
//! off the struct.

use lazybox_config::ActivityPaneMode;
use tuirealm::ratatui::layout::{Constraint, Direction, Layout, Rect};

/// Initial split percentages. Match the legacy defaults so users
/// don't see a jumpy first frame after the migration.
pub(crate) const DEFAULT_SIDEBAR_PCT: u16 = 40;
pub(crate) const DEFAULT_RIGHT_TOP_PCT: u16 = 25;
/// Min/max for either splitter (percentage). Keeps every pane
/// usable — no zero-height activity feed, no sliver sidebar.
pub(crate) const SPLIT_MIN: u16 = 15;
pub(crate) const SPLIT_MAX: u16 = 80;
/// Default step size per Shift-arrow tap. Picked so 4-5 taps cover
/// a useful range and a single tap is visibly more than a shimmer.
/// Live value reads from `ui.split_step_percent` (via
/// `lazybox_config::UiDefaults`) — kept here so the tests below stay
/// readable.
#[cfg(test)]
pub(crate) const SPLIT_STEP: i16 = 3;

/// Which splitter the user is currently dragging.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DragTarget {
    /// The vertical line between sidebar and the right column.
    SidebarRight,
    /// The horizontal line between activity and terminal stack.
    ActivityTerminals,
}

/// Splitter percentages + last-viewport snapshot + active drag, in
/// one place. Methods that mutate the percentages return `bool` so
/// the caller can flip its `redraw` flag without LayoutCtx knowing
/// about the wider model's redraw bookkeeping.
pub(crate) struct LayoutCtx {
    pub sidebar_pct: u16,
    pub right_top_pct: u16,
    pub last_area: Rect,
    pub active_drag: Option<DragTarget>,
    /// True iff the user has explicitly set the sidebar width
    /// (via persisted YAML or a runtime nudge / drag). When true,
    /// the absolute-column cap (`SIDEBAR_MAX_COLS`) is lifted —
    /// "default" and "user-chosen" are different things, and the
    /// user's deliberate choice always wins. When false, the cap
    /// is still applied so a fresh user on a wide monitor doesn't
    /// get a 160-col sidebar staring back at them.
    pub sidebar_user_resized: bool,
    /// True iff the user has explicitly chosen the Activity row's
    /// height (a splitter drag or Shift-↑/↓). When true, the
    /// content-fit shrink (`fit_activity_height`) is skipped and the
    /// percentage is honored verbatim, blank rows and all — the same
    /// "default ≠ user-chosen" rule `sidebar_user_resized` applies to
    /// the sidebar width.
    pub activity_user_resized: bool,
}

impl LayoutCtx {
    pub fn new() -> Self {
        Self {
            sidebar_pct: DEFAULT_SIDEBAR_PCT,
            right_top_pct: DEFAULT_RIGHT_TOP_PCT,
            last_area: Rect::default(),
            active_drag: None,
            sidebar_user_resized: false,
            activity_user_resized: false,
        }
    }

    /// Apply persisted splits from `~/.lazybox/config.yaml::ui`. `None`
    /// leaves the default in place.
    ///
    /// Does NOT flip `sidebar_user_resized` — a persisted percentage
    /// is just the percent knob, not a "user wants the cap lifted"
    /// declaration. On a 250-cell ultrawide, 40% × 250 = 100 cells
    /// (cap hits cleanly); without this reassertion an old persisted
    /// 40% launched into an uncapped 100-cell sidebar that on smaller
    /// terminals looked correct but on ultrawides bloomed to ~40%
    /// real estate before the cap could clamp it. The cap is now
    /// reasserted on every launch; users who want a deliberately
    /// wider sidebar nudge it at runtime (Shift-Right or drag), which
    /// flips the flag in-session.
    pub fn apply_persisted(
        &mut self,
        sidebar_pct: Option<u16>,
        right_top_pct: Option<u16>,
        activity_user_resized: Option<bool>,
    ) {
        if let Some(s) = sidebar_pct {
            self.sidebar_pct = clamp_pct(s as i16);
        }
        if let Some(t) = right_top_pct {
            self.right_top_pct = clamp_pct(t as i16);
        }
        // Unlike the sidebar's absolute-column cap (which is deliberately
        // reasserted every launch), a deliberately-resized activity row
        // must survive a restart: the persisted percentage alone would be
        // re-shrunk by the content-fit.
        if let Some(resized) = activity_user_resized {
            self.activity_user_resized = resized;
        }
    }

    /// Test whether `(col, row)` lands within tolerance of one of the
    /// two splitter lines. Tolerance: ±1 cell so users don't have to
    /// land pixel-perfect on the divider.
    ///
    /// `horizontal_active` gates only the horizontal (activity ↔
    /// terminal) splitter: it's a real resize handle just for the
    /// *full* activity pane. The slim `Summary` line still has a
    /// positive height, so the height check alone would synthesize a
    /// dead splitter there that drags `right_top_pct` with no visible
    /// effect — pass `false` in Summary / Hidden to suppress it. The
    /// vertical sidebar splitter is unaffected and always live.
    pub fn hit_test_splitter(
        &self,
        col: u16,
        row: u16,
        sidebar_rect: Rect,
        right_top_rect: Rect,
        horizontal_active: bool,
    ) -> Option<DragTarget> {
        // Vertical splitter sits between sidebar and the right column.
        let v_x = sidebar_rect.x + sidebar_rect.width;
        if col + 1 >= v_x
            && col <= v_x + 1
            && row >= self.last_area.y
            && row < self.last_area.y + self.last_area.height
        {
            return Some(DragTarget::SidebarRight);
        }
        // Horizontal splitter sits between right-top and right-bottom.
        // Suppressed unless the activity pane is a full, resizable pane
        // (a zero-height hidden row, or the slim summary line, has no
        // splitter to grab at the top edge of the terminal stack).
        let h_y = right_top_rect.y + right_top_rect.height;
        if horizontal_active
            && right_top_rect.height > 0
            && row + 1 >= h_y
            && row <= h_y + 1
            && col >= right_top_rect.x
            && col < right_top_rect.x + right_top_rect.width
        {
            return Some(DragTarget::ActivityTerminals);
        }
        None
    }

    /// Translate a drag's `(col, row)` into a new percentage for the
    /// active splitter and apply it. Returns `true` if the percentage
    /// actually changed so the caller can redraw.
    pub fn update_drag(&mut self, target: DragTarget, col: u16, row: u16) -> bool {
        match target {
            DragTarget::SidebarRight => {
                if self.last_area.width == 0 {
                    return false;
                }
                let rel = col.saturating_sub(self.last_area.x) as i32;
                let pct = (rel * 100 / self.last_area.width as i32)
                    .clamp(SPLIT_MIN as i32, SPLIT_MAX as i32) as u16;
                if pct != self.sidebar_pct {
                    self.sidebar_pct = pct;
                    self.sidebar_user_resized = true;
                    return true;
                }
                false
            }
            DragTarget::ActivityTerminals => {
                // Grabbing the splitter is a deliberate height choice —
                // mark it on the first movement so the content-fit shrink
                // stops fighting the pointer (the pane tracks the drag
                // rather than snapping back to its fitted height).
                self.activity_user_resized = true;
                let (_, right_top_rect, right_bottom_rect) = pane_areas(
                    self.last_area,
                    self.sidebar_pct,
                    self.right_top_pct,
                    self.sidebar_user_resized,
                );
                let right_height = right_top_rect.height + right_bottom_rect.height;
                if right_height == 0 {
                    return false;
                }
                let rel = row.saturating_sub(right_top_rect.y) as i32;
                let pct = (rel * 100 / right_height as i32)
                    .clamp(SPLIT_MIN as i32, SPLIT_MAX as i32) as u16;
                if pct != self.right_top_pct {
                    self.right_top_pct = pct;
                    return true;
                }
                false
            }
        }
    }

    /// Adjust the split percentages. `dx > 0` widens the sidebar;
    /// `dy > 0` grows the activity row at the terminal stack's
    /// expense. Persists to YAML on change. Returns `true` if any
    /// percentage actually changed.
    pub fn nudge_splits(&mut self, dx: i16, dy: i16) -> bool {
        let new_sidebar = clamp_pct(self.sidebar_pct as i16 + dx);
        let new_top = clamp_pct(self.right_top_pct as i16 + dy);
        if new_sidebar != self.sidebar_pct || new_top != self.right_top_pct {
            if new_sidebar != self.sidebar_pct {
                // Mark user-resized so the absolute-column cap is
                // lifted — Shift-arrow is an explicit "I want this
                // width, default-cap doesn't apply" signal.
                self.sidebar_user_resized = true;
            }
            if new_top != self.right_top_pct {
                // Shift-↑/↓ is an explicit height choice — honor it
                // verbatim and stop fitting the Activity row to content.
                self.activity_user_resized = true;
            }
            self.sidebar_pct = new_sidebar;
            self.right_top_pct = new_top;
            self.persist();
            return true;
        }
        false
    }

    /// Adopt the height the content-fit is currently *displaying* as the
    /// manual height: snap `right_top_pct` to it, mark the row user-set,
    /// and persist. Called when a Shift-↑/↓ nudge first takes manual
    /// control, so the nudge that follows grows / shrinks from what's on
    /// screen instead of jumping back to the stored percentage the fit had
    /// been overriding — without this, a Shift-↑ meant to shrink a fitted
    /// pane would instead *grow* it to the full percentage. No-op (returns
    /// `false`) once the user already took manual control, or before the
    /// viewport is known. Returns `true` when it adopted a new height.
    pub fn adopt_fitted_activity_height(&mut self, natural: u16) -> bool {
        if self.activity_user_resized {
            return false;
        }
        let (_, right_top, right_bottom) = pane_areas(
            self.last_area,
            self.sidebar_pct,
            self.right_top_pct,
            self.sidebar_user_resized,
        );
        let column = right_top.height + right_bottom.height;
        if column == 0 {
            return false;
        }
        let fitted =
            fit_activity_height((Rect::default(), right_top, right_bottom), natural, false)
                .1
                .height;
        self.right_top_pct = clamp_pct((fitted as i32 * 100 / column as i32) as i16);
        self.activity_user_resized = true;
        self.persist();
        true
    }

    /// Best-effort save of the current split percentages. The activity
    /// row's user-resized flag rides along so a deliberate height survives
    /// a restart — persisting the percentage without it would let the
    /// content-fit re-cap the value the user chose.
    pub fn persist(&self) {
        let s = self.sidebar_pct;
        let t = self.right_top_pct;
        let activity_resized = self.activity_user_resized;
        lazybox_config::Config::save_with_async(move |c| {
            c.ui.sidebar_pct = Some(s);
            c.ui.right_top_pct = Some(t);
            c.ui.activity_user_resized = Some(activity_resized);
        });
    }
}

/// Clamp a candidate percentage into the legal split range.
pub(crate) fn clamp_pct(raw: i16) -> u16 {
    raw.clamp(SPLIT_MIN as i16, SPLIT_MAX as i16) as u16
}

/// Hard cap on the sidebar's column count. Past this, no matter
/// what `sidebar_pct` says, extra horizontal space goes to the
/// right pane. The sidebar's longest natural row (`⇄1234 A ●
/// long title here    C CONFLICT  1d`) is around 90 cols; 100
/// gives a small margin without leaving the sidebar dominating an
/// ultra-wide monitor.
///
/// User can manually nudge the percentage up via `Shift-Right` —
/// but the absolute cap stays in force. To override, future work:
/// expose `ui.sidebar_max_cols` in `config.yaml`.
pub(crate) const SIDEBAR_MAX_COLS: u16 = 100;

/// Minimum sidebar width even on a narrow terminal — below this
/// the row content is unreadable. Picked to fit the
/// `⇄NNN A …` prefix plus a meaningful slice of the title.
pub(crate) const SIDEBAR_MIN_COLS: u16 = 30;

/// Compute the three pane rects (sidebar / right-top / right-bottom).
/// `sidebar_pct` is the sidebar's share of the total width;
/// `right_top_pct` is the activity row's share of the right column's
/// height. Both should already be clamped to `[SPLIT_MIN, SPLIT_MAX]`.
///
/// `user_resized` controls the absolute-column cap:
/// - `false` (default state) → cap at `SIDEBAR_MAX_COLS` so a fresh
///   user on a wide monitor doesn't get a 160-col sidebar.
/// - `true` (user nudged / drag-resized / persisted choice) → no
///   cap. The user's deliberate choice wins. They can grow the
///   sidebar to whatever Shift-Right takes them, all the way to
///   `SPLIT_MAX = 80%`.
///
/// `SIDEBAR_MIN_COLS` always applies — even a deliberate "make it
/// tiny" choice shouldn't collapse the rows into unreadable noise.
pub(crate) fn pane_areas(
    area: Rect,
    sidebar_pct: u16,
    right_top_pct: u16,
    user_resized: bool,
) -> (Rect, Rect, Rect) {
    let preferred = (area.width as u32 * sidebar_pct as u32 / 100) as u16;
    let upper = if user_resized {
        // No cap — honor the user's percentage. Still bounded by
        // the available width and SPLIT_MAX (which `sidebar_pct`
        // is already pre-clamped to).
        area.width
    } else {
        SIDEBAR_MAX_COLS
    };
    let sidebar_cols = preferred.clamp(SIDEBAR_MIN_COLS, upper).min(area.width);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(sidebar_cols), Constraint::Min(0)])
        .split(area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(right_top_pct), Constraint::Min(0)])
        .split(cols[1]);
    (cols[0], rows[0], rows[1])
}

/// Rows the Activity pane keeps in `Summary` mode: a single slim line
/// carrying the counts that matter (new activity / failing CI) above
/// the terminal.
pub(crate) const ACTIVITY_SUMMARY_HEIGHT: u16 = 1;

/// Cap on the Activity row's share of the right column when it's being
/// fit to content (#1469). Even a chatty feed keeps the majority of the
/// column with the agent's terminal.
pub(crate) const ACTIVITY_MAX_PCT: u16 = 40;

/// Shrink the Activity row (`right_top`) to the rows its content would
/// actually fill (`natural`), handing what it gives up to the terminal
/// stack (`right_bottom`) below. Runs before [`apply_activity_mode`], so
/// `Summary` / `Hidden` (which replace the height outright) are
/// unaffected.
///
/// - **Only ever shrinks.** A pane whose content overflows its
///   percentage keeps the percentage — a `natural` above the current
///   height is ignored.
/// - **Capped at [`ACTIVITY_MAX_PCT`]** of the right column, so a chatty
///   feed can't crowd the terminal even when the percentage is set high.
/// - **Skipped once the user has chosen a height** (`user_set`): a
///   deliberate drag / Shift-↑↓ is honored verbatim, blank rows and all.
pub(crate) fn fit_activity_height(
    rects: (Rect, Rect, Rect),
    natural: u16,
    user_set: bool,
) -> (Rect, Rect, Rect) {
    let (sidebar, right_top, right_bottom) = rects;
    if user_set {
        return rects;
    }
    let column = right_top.height + right_bottom.height;
    let cap = (column as u32 * ACTIVITY_MAX_PCT as u32 / 100) as u16;
    let fitted = natural.min(right_top.height).min(cap);
    let top = Rect {
        height: fitted,
        ..right_top
    };
    let bottom = Rect {
        y: right_top.y + fitted,
        height: right_top.height + right_bottom.height - fitted,
        ..right_bottom
    };
    (sidebar, top, bottom)
}

/// Resize the activity row for the pane's [`ActivityPaneMode`], handing
/// whatever it gives up to the terminal stack below it:
///
/// - `Full` — rects unchanged; the whole feed renders in `right_top`.
/// - `Summary` — `right_top` shrinks to [`ACTIVITY_SUMMARY_HEIGHT`] and
///   the reclaimed rows fold into `right_bottom`.
/// - `Hidden` — `right_top` collapses to zero height (the renderer
///   skips it and mouse hit-tests can't land on it) and `right_bottom`
///   spans the full right column.
pub(crate) fn apply_activity_mode(
    rects: (Rect, Rect, Rect),
    mode: ActivityPaneMode,
) -> (Rect, Rect, Rect) {
    let (sidebar, right_top, right_bottom) = rects;
    let kept = match mode {
        ActivityPaneMode::Full => return (sidebar, right_top, right_bottom),
        ActivityPaneMode::Summary => ACTIVITY_SUMMARY_HEIGHT.min(right_top.height),
        ActivityPaneMode::Hidden => 0,
    };
    let top = Rect {
        height: kept,
        ..right_top
    };
    let bottom = Rect {
        y: right_top.y + kept,
        height: right_top.height + right_bottom.height - kept,
        ..right_bottom
    };
    (sidebar, top, bottom)
}

/// Rows reserved for the focus-mode event header (issue #156).
pub(crate) const FOCUS_HEADER_HEIGHT: u16 = 1;

/// Split the pane area for focus mode into `(header, terminal_body)`.
/// The header is a slim strip at the top; the terminal takes the rest.
/// When the area is too short for both, the header wins and the body
/// collapses to empty (the caller renders nothing into a zero rect).
pub(crate) fn focus_mode_areas(pane_area: Rect) -> (Rect, Rect) {
    if pane_area.height <= FOCUS_HEADER_HEIGHT {
        return (pane_area, Rect::default());
    }
    let header = Rect {
        height: FOCUS_HEADER_HEIGHT,
        ..pane_area
    };
    let body = Rect {
        y: pane_area.y + FOCUS_HEADER_HEIGHT,
        height: pane_area.height - FOCUS_HEADER_HEIGHT,
        ..pane_area
    };
    (header, body)
}

/// Partition the focus-mode body (everything under the event header)
/// into the workspace-pane rects of a [`FocusLayout`] (#1258). Pane
/// order is the pane index the Model tracks focus by: `SplitV` is
/// left→right, `SplitH` top→bottom, `Grid` reads top-left, top-right,
/// bottom-left, bottom-right. `Single` returns the body untouched so
/// the historical one-fullscreen-terminal render stays pixel-identical.
/// Panes butt directly against each other — each multi-pane rect is
/// drawn with its own border, which provides the visual seam.
pub(crate) fn focus_layout_areas(body: Rect, layout: lazybox_config::FocusLayout) -> Vec<Rect> {
    use lazybox_config::FocusLayout as L;
    let halves_h = |r: Rect| -> (Rect, Rect) {
        let left_w = r.width / 2;
        (
            Rect { width: left_w, ..r },
            Rect {
                x: r.x + left_w,
                width: r.width - left_w,
                ..r
            },
        )
    };
    let halves_v = |r: Rect| -> (Rect, Rect) {
        let top_h = r.height / 2;
        (
            Rect { height: top_h, ..r },
            Rect {
                y: r.y + top_h,
                height: r.height - top_h,
                ..r
            },
        )
    };
    match layout {
        L::Single => vec![body],
        L::SplitV => {
            let (l, r) = halves_h(body);
            vec![l, r]
        }
        L::SplitH => {
            let (t, b) = halves_v(body);
            vec![t, b]
        }
        L::Grid => {
            let (top, bottom) = halves_v(body);
            let (tl, tr) = halves_h(top);
            let (bl, br) = halves_h(bottom);
            vec![tl, tr, bl, br]
        }
    }
}

/// Move focus-mode pane focus one step in `dir` (#1258). Pure pane
/// geometry over the index order [`focus_layout_areas`] defines;
/// motion clamps at the edges (no wrap) so an arrow is always a
/// spatial move, never a surprise teleport. Returns the new pane
/// index — unchanged when the direction has no neighbor.
pub(crate) fn focus_pane_move(
    layout: lazybox_config::FocusLayout,
    from: usize,
    dir: lazybox_core::TileDirection,
) -> usize {
    use lazybox_config::FocusLayout as L;
    use lazybox_core::TileDirection as D;
    match layout {
        L::Single => 0,
        L::SplitV => match dir {
            D::Left => 0,
            D::Right => 1,
            _ => from.min(1),
        },
        L::SplitH => match dir {
            D::Up => 0,
            D::Down => 1,
            _ => from.min(1),
        },
        L::Grid => {
            let from = from.min(3);
            let (row, col) = (from / 2, from % 2);
            let (row, col) = match dir {
                D::Left => (row, 0),
                D::Right => (row, 1),
                D::Up => (0, col),
                D::Down => (1, col),
            };
            row * 2 + col
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect::new(0, 0, 100, 50)
    }

    fn ctx() -> LayoutCtx {
        let mut c = LayoutCtx::new();
        c.last_area = area();
        c
    }

    #[test]
    fn defaults_are_in_range() {
        let c = LayoutCtx::new();
        assert_eq!(c.sidebar_pct, DEFAULT_SIDEBAR_PCT);
        assert_eq!(c.right_top_pct, DEFAULT_RIGHT_TOP_PCT);
        assert!(c.sidebar_pct >= SPLIT_MIN && c.sidebar_pct <= SPLIT_MAX);
        assert!(c.right_top_pct >= SPLIT_MIN && c.right_top_pct <= SPLIT_MAX);
        assert!(c.active_drag.is_none());
    }

    #[test]
    fn apply_persisted_clamps_into_legal_range() {
        let mut c = LayoutCtx::new();
        // Below min → clamped up.
        c.apply_persisted(Some(0), Some(0), None);
        assert_eq!(c.sidebar_pct, SPLIT_MIN);
        assert_eq!(c.right_top_pct, SPLIT_MIN);
        // Above max → clamped down.
        c.apply_persisted(Some(99), Some(99), None);
        assert_eq!(c.sidebar_pct, SPLIT_MAX);
        assert_eq!(c.right_top_pct, SPLIT_MAX);
        // None leaves the existing value alone.
        c.apply_persisted(None, None, None);
        assert_eq!(c.sidebar_pct, SPLIT_MAX);
    }

    #[test]
    fn apply_persisted_does_not_lift_the_column_cap() {
        // Regression: previously, loading any persisted sidebar_pct
        // flipped `sidebar_user_resized = true`, which removed the
        // SIDEBAR_MAX_COLS cap. On an ultrawide terminal that turned
        // the default 40% into a ~250-cell sidebar dominating the
        // screen. The cap must reassert across launches; only an
        // explicit runtime nudge / drag lifts it.
        let mut c = LayoutCtx::new();
        c.apply_persisted(Some(40), None, None);
        assert!(!c.sidebar_user_resized);
        // On a 250-cell ultrawide, 40% × 250 = 100 cells, which the
        // cap allows; the test for the actual cap kicking in lives
        // in pane_areas — here we just verify the flag stays clean.
        let area = Rect::new(0, 0, 250, 50);
        let (sidebar, _, _) =
            pane_areas(area, c.sidebar_pct, c.right_top_pct, c.sidebar_user_resized);
        assert!(
            sidebar.width <= SIDEBAR_MAX_COLS,
            "sidebar {} exceeded cap {} after apply_persisted",
            sidebar.width,
            SIDEBAR_MAX_COLS,
        );
    }

    #[test]
    fn nudge_widens_and_narrows_sidebar() {
        let mut c = LayoutCtx::new();
        let start = c.sidebar_pct;
        // We don't assert the persisted side-effect — that's a YAML
        // write that's tested elsewhere.
        let _ = c.nudge_splits(SPLIT_STEP, 0);
        assert_eq!(c.sidebar_pct, start + SPLIT_STEP as u16);
        let _ = c.nudge_splits(-SPLIT_STEP, 0);
        assert_eq!(c.sidebar_pct, start);
    }

    #[test]
    fn nudge_returns_false_when_clamped_against_a_wall() {
        let mut c = LayoutCtx::new();
        c.sidebar_pct = SPLIT_MAX;
        c.right_top_pct = SPLIT_MAX;
        // Already at the ceiling on both axes — no change, no
        // redraw, no YAML write.
        assert!(!c.nudge_splits(SPLIT_STEP, SPLIT_STEP));
    }

    #[test]
    fn hit_test_finds_the_vertical_splitter() {
        let c = ctx();
        let (sidebar, right_top, _) = pane_areas(
            area(),
            c.sidebar_pct,
            c.right_top_pct,
            c.sidebar_user_resized,
        );
        // Hover one cell right of the sidebar's right edge → vertical splitter.
        let v_x = sidebar.x + sidebar.width;
        assert_eq!(
            c.hit_test_splitter(v_x, 10, sidebar, right_top, true),
            Some(DragTarget::SidebarRight)
        );
    }

    #[test]
    fn hit_test_finds_the_horizontal_splitter() {
        let c = ctx();
        let (sidebar, right_top, _) = pane_areas(
            area(),
            c.sidebar_pct,
            c.right_top_pct,
            c.sidebar_user_resized,
        );
        let h_y = right_top.y + right_top.height;
        assert_eq!(
            c.hit_test_splitter(right_top.x + 5, h_y, sidebar, right_top, true),
            Some(DragTarget::ActivityTerminals)
        );
    }

    #[test]
    fn summary_seam_has_no_horizontal_splitter() {
        // The slim summary line has a positive height, so only the
        // explicit `horizontal_active = false` keeps its seam from
        // synthesizing a dead splitter.
        let c = ctx();
        let (sidebar, right_top, right_bottom) = pane_areas(
            area(),
            c.sidebar_pct,
            c.right_top_pct,
            c.sidebar_user_resized,
        );
        let (_, summary_top, _) = apply_activity_mode(
            (sidebar, right_top, right_bottom),
            ActivityPaneMode::Summary,
        );
        let h_y = summary_top.y + summary_top.height;
        assert_eq!(
            c.hit_test_splitter(summary_top.x + 5, h_y, sidebar, summary_top, false),
            None,
            "no draggable splitter at the summary / terminal seam"
        );
        // The vertical sidebar splitter still works in Summary mode.
        let v_x = sidebar.x + sidebar.width;
        assert_eq!(
            c.hit_test_splitter(v_x, 10, sidebar, summary_top, false),
            Some(DragTarget::SidebarRight),
        );
    }

    #[test]
    fn hit_test_misses_inside_a_pane() {
        let c = ctx();
        let (sidebar, right_top, _) = pane_areas(
            area(),
            c.sidebar_pct,
            c.right_top_pct,
            c.sidebar_user_resized,
        );
        // Middle of the sidebar — not on any splitter.
        assert_eq!(c.hit_test_splitter(2, 10, sidebar, right_top, true), None);
    }

    #[test]
    fn update_drag_moves_sidebar_to_drop_column() {
        let mut c = ctx();
        // Drop at column 25 out of 100 → ~25% sidebar.
        let changed = c.update_drag(DragTarget::SidebarRight, 25, 10);
        assert!(changed);
        assert_eq!(c.sidebar_pct, 25);
    }

    #[test]
    fn update_drag_clamps_to_split_max() {
        let mut c = ctx();
        // Way past the right edge — clamps to SPLIT_MAX.
        let changed = c.update_drag(DragTarget::SidebarRight, 95, 10);
        assert!(changed);
        assert_eq!(c.sidebar_pct, SPLIT_MAX);
    }

    #[test]
    fn update_drag_returns_false_when_pct_unchanged() {
        let mut c = ctx();
        let start = c.sidebar_pct;
        // Drop at the column already corresponding to the current pct.
        let target_col = (start as u32 * c.last_area.width as u32 / 100) as u16;
        let _ = c.update_drag(DragTarget::SidebarRight, target_col, 10);
        // Second drag at the same column → no change → false.
        let changed = c.update_drag(DragTarget::SidebarRight, target_col, 10);
        assert!(!changed);
    }

    #[test]
    fn apply_activity_mode_keeps_rects_when_full() {
        let c = ctx();
        let rects = pane_areas(
            area(),
            c.sidebar_pct,
            c.right_top_pct,
            c.sidebar_user_resized,
        );
        assert_eq!(apply_activity_mode(rects, ActivityPaneMode::Full), rects);
    }

    #[test]
    fn apply_activity_mode_folds_top_into_bottom_when_hidden() {
        let c = ctx();
        let (sidebar, right_top, right_bottom) = pane_areas(
            area(),
            c.sidebar_pct,
            c.right_top_pct,
            c.sidebar_user_resized,
        );
        let (s, top, bottom) =
            apply_activity_mode((sidebar, right_top, right_bottom), ActivityPaneMode::Hidden);
        assert_eq!(s, sidebar, "sidebar is untouched");
        assert_eq!(
            top.height, 0,
            "hidden activity row collapses to zero height"
        );
        assert_eq!(top.y, right_top.y);
        // The terminal stack reclaims the full right column.
        assert_eq!(bottom.y, right_top.y);
        assert_eq!(bottom.height, right_top.height + right_bottom.height);
        assert_eq!(bottom.width, right_top.width);
    }

    #[test]
    fn apply_activity_mode_keeps_one_summary_row() {
        let c = ctx();
        let (sidebar, right_top, right_bottom) = pane_areas(
            area(),
            c.sidebar_pct,
            c.right_top_pct,
            c.sidebar_user_resized,
        );
        let (s, top, bottom) = apply_activity_mode(
            (sidebar, right_top, right_bottom),
            ActivityPaneMode::Summary,
        );
        assert_eq!(s, sidebar, "sidebar is untouched");
        assert_eq!(
            top.height, ACTIVITY_SUMMARY_HEIGHT,
            "summary keeps a single slim row"
        );
        assert_eq!(top.y, right_top.y);
        // The terminal reclaims everything the summary gave up.
        assert_eq!(bottom.y, right_top.y + ACTIVITY_SUMMARY_HEIGHT);
        assert_eq!(
            bottom.height,
            right_top.height + right_bottom.height - ACTIVITY_SUMMARY_HEIGHT
        );
    }

    #[test]
    fn focus_layout_areas_partition_the_body_exactly() {
        use lazybox_config::FocusLayout as L;
        let body = Rect::new(0, 1, 121, 39); // odd width: remainder must not be lost
        for layout in [L::Single, L::SplitV, L::SplitH, L::Grid] {
            let rects = focus_layout_areas(body, layout);
            assert_eq!(rects.len(), layout.pane_count(), "{layout:?}");
            let cells: u32 = rects.iter().map(|r| r.width as u32 * r.height as u32).sum();
            assert_eq!(
                cells,
                body.width as u32 * body.height as u32,
                "{layout:?} panes must tile the body with no gaps or overlap"
            );
            for r in &rects {
                assert!(r.x >= body.x && r.x + r.width <= body.x + body.width);
                assert!(r.y >= body.y && r.y + r.height <= body.y + body.height);
            }
        }
        // Single is byte-identical to the body — the pixel-identity
        // guarantee starts here.
        assert_eq!(focus_layout_areas(body, L::Single), vec![body]);
        // Grid order is TL, TR, BL, BR.
        let grid = focus_layout_areas(body, L::Grid);
        assert!(grid[0].x < grid[1].x && grid[0].y == grid[1].y);
        assert!(grid[2].y > grid[0].y && grid[2].x == grid[0].x);
        assert!(grid[3].x > grid[2].x && grid[3].y == grid[2].y);
    }

    #[test]
    fn focus_pane_move_is_spatial_and_clamps_at_edges() {
        use lazybox_config::FocusLayout as L;
        use lazybox_core::TileDirection as D;
        // SplitV: left/right move, up/down inert, edges clamp.
        assert_eq!(focus_pane_move(L::SplitV, 0, D::Right), 1);
        assert_eq!(focus_pane_move(L::SplitV, 1, D::Right), 1);
        assert_eq!(focus_pane_move(L::SplitV, 1, D::Left), 0);
        assert_eq!(focus_pane_move(L::SplitV, 0, D::Up), 0);
        // SplitH: up/down move, left/right inert.
        assert_eq!(focus_pane_move(L::SplitH, 0, D::Down), 1);
        assert_eq!(focus_pane_move(L::SplitH, 1, D::Up), 0);
        assert_eq!(focus_pane_move(L::SplitH, 0, D::Right), 0);
        // Grid: 2D moves between quadrants, clamping at edges.
        assert_eq!(focus_pane_move(L::Grid, 0, D::Right), 1);
        assert_eq!(focus_pane_move(L::Grid, 1, D::Down), 3);
        assert_eq!(focus_pane_move(L::Grid, 3, D::Left), 2);
        assert_eq!(focus_pane_move(L::Grid, 2, D::Up), 0);
        assert_eq!(focus_pane_move(L::Grid, 0, D::Left), 0);
        assert_eq!(focus_pane_move(L::Grid, 3, D::Down), 3);
        // Single always resolves to the only pane.
        assert_eq!(focus_pane_move(L::Single, 0, D::Right), 0);
    }

    #[test]
    fn hidden_activity_row_has_no_horizontal_splitter() {
        let c = ctx();
        let (sidebar, right_top, right_bottom) = pane_areas(
            area(),
            c.sidebar_pct,
            c.right_top_pct,
            c.sidebar_user_resized,
        );
        let (_, hidden_top, _) =
            apply_activity_mode((sidebar, right_top, right_bottom), ActivityPaneMode::Hidden);
        // The old splitter sat at `right_top.y + right_top.height`. With
        // the row hidden (zero height) nothing there should hit-test as
        // a draggable splitter.
        let h_y = right_top.y + right_top.height;
        assert_eq!(
            c.hit_test_splitter(right_top.x + 5, h_y, sidebar, hidden_top, false),
            None,
        );
        // The vertical sidebar splitter is unaffected even with the
        // horizontal splitter inactive.
        let v_x = sidebar.x + sidebar.width;
        assert_eq!(
            c.hit_test_splitter(v_x, 10, sidebar, hidden_top, false),
            Some(DragTarget::SidebarRight),
        );
    }

    #[test]
    fn fit_activity_shrinks_a_nearly_empty_pane_to_content() {
        // 100×50 area, 25% default → right_top ≈ 12 rows. Content only
        // needs 6: shrink to 6 and hand the other rows to the terminal.
        let c = ctx();
        let rects = pane_areas(
            area(),
            c.sidebar_pct,
            c.right_top_pct,
            c.sidebar_user_resized,
        );
        let (_, right_top, right_bottom) = rects;
        let column = right_top.height + right_bottom.height;
        let (sidebar, top, bottom) = fit_activity_height(rects, 6, false);
        assert_eq!(sidebar, rects.0, "sidebar untouched");
        assert_eq!(top.height, 6, "activity row fits its content");
        assert_eq!(top.y, right_top.y);
        assert_eq!(bottom.y, right_top.y + 6, "terminal takes over below");
        assert_eq!(
            top.height + bottom.height,
            column,
            "column height is conserved"
        );
    }

    #[test]
    fn fit_activity_only_ever_shrinks() {
        // Content that overflows the percentage keeps the percentage —
        // the fit never grows the row into the terminal.
        let c = ctx();
        let rects = pane_areas(
            area(),
            c.sidebar_pct,
            c.right_top_pct,
            c.sidebar_user_resized,
        );
        let unfitted = rects.1.height;
        let (_, top, bottom) = fit_activity_height(rects, 999, false);
        assert_eq!(top.height, unfitted, "row keeps its percentage");
        assert_eq!(bottom, rects.2, "terminal is untouched");
    }

    #[test]
    fn fit_activity_caps_at_the_max_pct() {
        // A tall persisted percentage (60%) with a chatty feed is still
        // capped at ACTIVITY_MAX_PCT of the column so the terminal keeps
        // the majority.
        let mut c = ctx();
        c.right_top_pct = 60;
        let rects = pane_areas(
            area(),
            c.sidebar_pct,
            c.right_top_pct,
            c.sidebar_user_resized,
        );
        let (_, right_top, right_bottom) = rects;
        let column = right_top.height + right_bottom.height;
        let cap = column * ACTIVITY_MAX_PCT / 100;
        let (_, top, _) = fit_activity_height(rects, 999, false);
        assert_eq!(
            top.height, cap,
            "capped at {ACTIVITY_MAX_PCT}% of the column"
        );
        assert!(top.height < right_top.height, "the cap shrank the 60% row");
    }

    #[test]
    fn fit_activity_is_skipped_once_the_user_chose_a_height() {
        // With `user_set`, the percentage is honored verbatim — blank
        // rows and all — regardless of how little content there is.
        let c = ctx();
        let rects = pane_areas(
            area(),
            c.sidebar_pct,
            c.right_top_pct,
            c.sidebar_user_resized,
        );
        assert_eq!(fit_activity_height(rects, 3, true), rects);
    }

    #[test]
    fn dragging_the_horizontal_splitter_marks_the_activity_row_user_set() {
        let mut c = ctx();
        assert!(!c.activity_user_resized);
        c.update_drag(DragTarget::ActivityTerminals, 60, 30);
        assert!(
            c.activity_user_resized,
            "a splitter drag opts the row out of content-fit"
        );
    }

    #[test]
    fn shift_up_down_marks_the_activity_row_user_set() {
        let mut c = ctx();
        assert!(!c.activity_user_resized);
        // A vertical nudge changes right_top_pct → user-set.
        assert!(c.nudge_splits(0, SPLIT_STEP));
        assert!(c.activity_user_resized);
        // A horizontal-only nudge leaves the activity flag alone.
        let mut c2 = ctx();
        assert!(c2.nudge_splits(SPLIT_STEP, 0));
        assert!(!c2.activity_user_resized);
    }

    #[test]
    fn adopt_fitted_activity_height_snaps_pct_to_the_displayed_height() {
        // 100×50 area, 25% default → right_top ≈ 12 rows. Content only
        // needs 10 rows, so the fit displays 10 = 20% of the 50-row
        // column. Adopting must set the stored pct to 20, not leave it at
        // the overridden 25, and mark the row user-set.
        let mut c = ctx();
        assert!(c.adopt_fitted_activity_height(10));
        assert_eq!(c.right_top_pct, 20);
        assert!(c.activity_user_resized);
        // Once the user has manual control it's a no-op — a later fit
        // measurement can't move the height they chose.
        c.right_top_pct = 33;
        assert!(!c.adopt_fitted_activity_height(5));
        assert_eq!(c.right_top_pct, 33, "manual height is left untouched");
    }

    #[test]
    fn first_shift_up_shrinks_a_fitted_pane_instead_of_growing_it() {
        // Regression (#1469 review): the stored percentage (25%) was
        // decoupled from the fitted display height (20% for 10 rows of a
        // 50-row column). A Shift-↑ meant to shrink nudged 25→22, and
        // turning the fit off then *grew* the pane from the fitted 20% to
        // 22%. Adopting the fitted height first makes the nudge shrink from
        // what's on screen: 20 → 17.
        let mut c = ctx();
        c.adopt_fitted_activity_height(10); // mirrors the key handler
        assert_eq!(c.right_top_pct, 20);
        assert!(c.nudge_splits(0, -SPLIT_STEP)); // Shift-Up
        assert_eq!(
            c.right_top_pct,
            20 - SPLIT_STEP as u16,
            "shrinks below the fitted height rather than growing past it"
        );
    }

    #[test]
    fn apply_persisted_restores_the_activity_user_resized_flag() {
        // A deliberately-resized activity row must survive a restart: the
        // persisted percentage alone would be re-shrunk by the content-fit
        // (unlike the sidebar's cap, which is reasserted each launch).
        let mut c = LayoutCtx::new();
        assert!(!c.activity_user_resized);
        c.apply_persisted(Some(50), Some(50), Some(true));
        assert!(c.activity_user_resized, "restored user-resized intent");
        // None leaves the current flag untouched (mid-session reloads).
        c.apply_persisted(None, None, None);
        assert!(c.activity_user_resized);
        // An explicit false clears it.
        c.apply_persisted(None, None, Some(false));
        assert!(!c.activity_user_resized);
    }
}
