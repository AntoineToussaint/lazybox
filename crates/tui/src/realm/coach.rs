//! `Coach` — the onboarding coach rail (issue #1460), the successor to
//! the 14-card modal tour.
//!
//! Instead of a slide deck that narrates features, the coach is a slim
//! two-row strip docked above the footer. It gives the user *one*
//! objective at a time, points at the pane it's talking about, and
//! *waits* — confirming when the user actually performs the action,
//! verified against real model state, before advancing. The three panes
//! stay fully visible and usable the whole time: the rail is carved out
//! of the pane area, never drawn on top of it, so it can never occlude
//! the UI (the old deck's cardinal sin).
//!
//! Design properties carried forward from the deck:
//! - **Key hints render from the action catalog** (#602): every key the
//!   coach shows is the user's *effective* binding, including keymap-preset
//!   and `ui.action_keys` remaps. A step can never advertise a key the
//!   user doesn't have.
//!
//! Everything the coach teaches is data (`STEPS`): an objective, a
//! spotlight target, and a completion [`Goal`] evaluated against a
//! [`CoachSnapshot`] of real model state (or a real dispatched action).
//! A test walks every step and asserts each referenced action exists in
//! the catalog and each goal is reachable.

use lazybox_tui_core::action::{ActionKind, CatalogEntry};
use std::time::{Duration, Instant};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::buffer::Buffer;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::Paragraph;

/// How long a completed step's success line lingers before the coach
/// auto-advances, so the "that's it —" confirmation is legible.
pub(crate) const SUCCESS_DWELL: Duration = Duration::from_millis(2600);

/// How long a step may sit unsatisfied before the rail offers a hand
/// (#1460 stuck-detection) rather than sitting there being right.
pub(crate) const STUCK_AFTER: Duration = Duration::from_secs(25);

/// One segment of an objective/success line. `Key` resolves against the
/// live catalog at render time so the shown binding is always the user's
/// effective one; `TermExit` renders the terminal escape-leader exit
/// (`]]q` by default), which is owned by the terminal latch rather than
/// the catalog and so can't be a `Key`.
#[derive(Debug, Clone, Copy)]
enum Seg {
    Text(&'static str),
    Key(ActionKind),
    TermExit,
}

/// The pane a step is talking about — the spotlight target. The Model
/// accent-frames the matching pane while the step is live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Spot {
    Sidebar,
    Terminal,
    None,
}

/// How a step is completed. Every non-`Info` goal is a predicate over
/// real model state or a real dispatched action — never a bare "press
/// Enter to continue".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Goal {
    /// The user moved off the row they started on, or opened a workspace
    /// (focus left the sidebar).
    Opened,
    /// An agent terminal is now running — the worktree + agent came up.
    AgentRunning,
    /// Focus entered a live terminal this step, then returned to a
    /// navigation pane (stepped out without killing the session).
    SteppedOut,
    /// Focus is back in a live terminal (came back to the session).
    Returned,
    /// A jump-to-signal action was dispatched (asking / failing-CI /
    /// workspace) — reachable regardless of whether the inbox currently
    /// has such a row, so the step degrades instead of stalling.
    Jumped,
    /// Purely informational — the single allowed "read this" step. It
    /// never auto-completes; the user ends or skips off it.
    Info,
}

struct Step {
    objective: &'static [Seg],
    /// Shown, in the success color, once the goal fires.
    success: &'static str,
    spot: Spot,
    goal: Goal,
}

use Seg::{Key, TermExit, Text};

/// The curriculum — six objectives in the order a real first session
/// runs. Snippets, multi-repo fan-out, Spaces, focus layouts and the
/// GitHub actions are deliberately *not* here (#1460): that's the
/// mastery ledger's job, taught later when the user is doing the thing.
const STEPS: &[Step] = &[
    Step {
        objective: &[
            Text("Look at one thing — move with ↑/↓ and press "),
            Key(ActionKind::OpenWorkspace),
            Text(" to open it."),
        ],
        success: "That's a task — its activity and terminal fill the panes on the right.",
        spot: Spot::Sidebar,
        goal: Goal::Opened,
    },
    Step {
        objective: &[
            Text("Put an agent on it: press "),
            Key(ActionKind::Work),
            Text(". Watch a worktree appear."),
        ],
        success: "That's the aha — a git worktree and agent came up for you in the background.",
        spot: Spot::Sidebar,
        goal: Goal::AgentRunning,
    },
    Step {
        objective: &[
            Text("Talk to the agent, then press "),
            TermExit,
            Text(" to step back to the inbox."),
        ],
        success: "The session keeps running while you're away — nothing is lost.",
        spot: Spot::Terminal,
        goal: Goal::SteppedOut,
    },
    Step {
        objective: &[
            Text("Come back to it — "),
            Key(ActionKind::OpenWorkspace),
            Text(" or "),
            Key(ActionKind::JumpToWorkspace),
            Text(". Sessions persist."),
        ],
        success: "Same live agent, right where you left it. That's the whole product.",
        spot: Spot::Sidebar,
        goal: Goal::Returned,
    },
    Step {
        objective: &[
            Text("Notice work happening on its own — jump to it with "),
            Key(ActionKind::JumpToAsking),
            Text(" / "),
            Key(ActionKind::JumpToFailingCi),
            Text("."),
        ],
        success: "Straight to the row that needs you — an agent asking, or CI gone red.",
        spot: Spot::Sidebar,
        goal: Goal::Jumped,
    },
    Step {
        objective: &[
            Text("That's the loop. Quit on purpose with "),
            Key(ActionKind::Quit),
            Text(" — your sessions outlive it."),
        ],
        success: "",
        spot: Spot::None,
        goal: Goal::Info,
    },
];

/// Number of steps in the curriculum — exported so the Model can clamp a
/// persisted resume position without reaching into `STEPS`.
pub(crate) const STEP_COUNT: usize = STEPS.len();

/// Where keyboard focus sits, condensed to what the coach's goals care
/// about. Built from the Model's `PaneFocus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoachFocus {
    Sidebar,
    Activity,
    Terminal,
}

/// A snapshot of the real model state the coach's goals are evaluated
/// against, rebuilt each tick. Pure data so the goal logic is unit
/// testable without a live Model.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CoachSnapshot {
    pub focus: CoachFocus,
    pub cursor: usize,
    pub agent_running: bool,
}

impl CoachSnapshot {
    fn in_terminal(&self) -> bool {
        matches!(self.focus, CoachFocus::Terminal)
    }
    fn in_sidebar(&self) -> bool {
        matches!(self.focus, CoachFocus::Sidebar)
    }
}

/// What a click on the rail resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoachClick {
    SkipStep,
    End,
    None,
}

pub(crate) struct Coach {
    step: usize,
    catalog: Vec<CatalogEntry>,
    ascii: bool,
    /// `ui.terminal_escape_char` — the leader whose double-press (`]]`)
    /// escapes a live terminal, so the terminal-exit step can teach the
    /// gesture that actually works there rather than a PTY-swallowed key.
    escape_char: char,
    /// The sidebar cursor when the current step began, so `Opened` can
    /// detect movement.
    baseline_cursor: usize,
    /// Focus entered a live terminal during the current step.
    entered_terminal: bool,
    /// A jump-to-signal action was dispatched during the current step.
    jumped: bool,
    /// When the current step's goal fired; drives the success dwell.
    satisfied_at: Option<Instant>,
    /// When the current step began; drives the stuck hint.
    step_started_at: Instant,
    skip_btn: Option<Rect>,
    end_btn: Option<Rect>,
}

impl Coach {
    pub(crate) fn new(
        catalog: Vec<CatalogEntry>,
        step: usize,
        ascii: bool,
        escape_char: char,
        cursor: usize,
    ) -> Self {
        Self {
            step: step.min(STEP_COUNT - 1),
            catalog,
            ascii,
            escape_char,
            baseline_cursor: cursor,
            entered_terminal: false,
            jumped: false,
            satisfied_at: None,
            step_started_at: Instant::now(),
            skip_btn: None,
            end_btn: None,
        }
    }

    pub(crate) fn step_index(&self) -> usize {
        self.step
    }

    pub(crate) fn current_spot(&self) -> Spot {
        STEPS[self.step].spot
    }

    fn action_entry(&self, kind: ActionKind) -> Option<&CatalogEntry> {
        self.catalog
            .iter()
            .find(|e| e.kind == kind && e.param.is_none())
    }

    /// The effective key display for one action kind, or `None` when the
    /// user has unbound it (a step referencing it renders without the
    /// key — never a stale literal).
    fn key_display(&self, kind: ActionKind) -> Option<String> {
        self.action_entry(kind)
            .filter(|e| !e.keys_display.is_empty())
            .map(|e| e.keys_display.to_string())
    }

    /// Record that focus entered a terminal / a jump was dispatched —
    /// the two goals that can't be read off a single-frame snapshot.
    pub(crate) fn note_jump(&mut self) {
        self.jumped = true;
    }

    /// Fold a fresh snapshot in and report whether the current step's
    /// goal *just* became satisfied (transitioned false→true). Returns
    /// `false` on every later call for the same step, so the caller
    /// flashes the success line exactly once.
    pub(crate) fn observe(&mut self, snap: &CoachSnapshot) -> bool {
        if snap.in_terminal() {
            self.entered_terminal = true;
        }
        if self.satisfied_at.is_some() {
            return false;
        }
        if self.goal_met(snap) {
            self.satisfied_at = Some(Instant::now());
            return true;
        }
        false
    }

    fn goal_met(&self, snap: &CoachSnapshot) -> bool {
        match STEPS[self.step].goal {
            Goal::Opened => snap.cursor != self.baseline_cursor || !snap.in_sidebar(),
            Goal::AgentRunning => snap.agent_running,
            Goal::SteppedOut => self.entered_terminal && !snap.in_terminal(),
            Goal::Returned => snap.in_terminal(),
            Goal::Jumped => self.jumped,
            Goal::Info => false,
        }
    }

    pub(crate) fn is_satisfied(&self) -> bool {
        self.satisfied_at.is_some()
    }

    /// True once a satisfied step's success line has lingered long
    /// enough to auto-advance.
    pub(crate) fn ready_to_advance(&self) -> bool {
        self.satisfied_at
            .is_some_and(|at| at.elapsed() >= SUCCESS_DWELL)
    }

    /// Whether the current step has sat unsatisfied long enough to offer
    /// a hand (#1460 stuck-detection). Never fires on the informational
    /// step (nothing to be stuck on) or once the goal is met.
    pub(crate) fn stuck(&self) -> bool {
        !self.is_satisfied()
            && STEPS[self.step].goal != Goal::Info
            && self.step_started_at.elapsed() >= STUCK_AFTER
    }

    /// Move to the next step, re-baselining its per-step tracking to
    /// `cursor`. Returns `true` when there is no next step — the coach
    /// is finished and the caller should end it.
    pub(crate) fn advance(&mut self, cursor: usize) -> bool {
        if self.step + 1 >= STEP_COUNT {
            return true;
        }
        self.step += 1;
        self.baseline_cursor = cursor;
        self.entered_terminal = false;
        self.jumped = false;
        self.satisfied_at = None;
        self.step_started_at = Instant::now();
        false
    }

    /// Resolve a left-click at `(col, row)` against the rail's recorded
    /// hit-boxes.
    pub(crate) fn on_click(&self, col: u16, row: u16) -> CoachClick {
        let hit = |b: Option<Rect>| {
            b.is_some_and(|r| {
                col >= r.x && col < r.x + r.width && row >= r.y && row < r.y + r.height
            })
        };
        if hit(self.end_btn) {
            CoachClick::End
        } else if hit(self.skip_btn) {
            CoachClick::SkipStep
        } else {
            CoachClick::None
        }
    }

    fn glyph(&self, unicode: &'static str, ascii: &'static str) -> &'static str {
        if self.ascii { ascii } else { unicode }
    }

    /// Build the objective row's styled spans, resolving key hints from
    /// the catalog. A referenced key the user has unbound collapses to
    /// nothing rather than a stale literal.
    fn objective_spans(&self, theme: &crate::theme::Theme) -> Vec<Span<'static>> {
        let key = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let body = Style::default().fg(theme.text_strong);
        let mut spans = Vec::new();
        for seg in STEPS[self.step].objective {
            match seg {
                Text(t) => spans.push(Span::styled((*t).to_string(), body)),
                Key(kind) => {
                    if let Some(k) = self.key_display(*kind) {
                        spans.push(Span::styled(k, key));
                    }
                }
                TermExit => {
                    let c = self.escape_char;
                    spans.push(Span::styled(format!("{c}{c}q"), key));
                }
            }
        }
        spans
    }

    fn spot_label(&self) -> Option<&'static str> {
        match STEPS[self.step].spot {
            Spot::Sidebar => Some("inbox"),
            Spot::Terminal => Some("agent"),
            Spot::None => None,
        }
    }

    /// Render the two-row rail into `area` (already carved out of the
    /// pane region — this never overlaps a pane). Records the skip / end
    /// hit-boxes for `on_click`.
    pub(crate) fn render(&mut self, f: &mut Frame, area: Rect) {
        self.skip_btn = None;
        self.end_btn = None;
        if area.height == 0 || area.width < 12 {
            return;
        }
        let theme = crate::theme::current();
        let top = Rect { height: 1, ..area };

        // ── Objective / success row ─────────────────────────────────
        let badge_style = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);
        let badge = format!(
            " {} {}/{} ",
            self.glyph("◆", "*"),
            self.step + 1,
            STEP_COUNT,
        );
        let mut top_spans = vec![Span::styled(badge, badge_style)];
        if self.is_satisfied() {
            let mark = self.glyph("✓ ", "+ ");
            let text = STEPS[self.step].success;
            top_spans.push(Span::styled(
                format!("{mark}{text}"),
                Style::default()
                    .fg(theme.success)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            top_spans.push(Span::raw(" "));
            top_spans.extend(self.objective_spans(theme));
        }
        f.render_widget(Paragraph::new(Line::from(top_spans)), top);

        if area.height < 2 {
            return;
        }
        let bottom = Rect {
            y: area.y + 1,
            height: 1,
            ..area
        };

        // ── Controls / spotlight row ────────────────────────────────
        // Built span-by-span with a running x so the clickable skip /
        // end tokens can be recorded as hit-boxes.
        let dim = Style::default().fg(theme.text_dim);
        let accent = Style::default().fg(theme.accent);
        let mut spans: Vec<Span> = Vec::new();
        let mut x = bottom.x;
        let push = |spans: &mut Vec<Span<'static>>, x: &mut u16, text: String, style: Style| {
            let w = text.chars().count() as u16;
            let rect = Rect::new(*x, bottom.y, w, 1);
            spans.push(Span::styled(text, style));
            *x = x.saturating_add(w);
            rect
        };

        push(&mut spans, &mut x, " coach · ".to_string(), dim);
        self.skip_btn = Some(push(&mut spans, &mut x, "Ctrl-n skip".to_string(), accent));
        push(&mut spans, &mut x, " · ".to_string(), dim);
        self.end_btn = Some(push(&mut spans, &mut x, "Ctrl-e end".to_string(), accent));
        if self.is_satisfied() {
            push(
                &mut spans,
                &mut x,
                format!(" · advancing {}", self.glyph("→", "->")),
                Style::default().fg(theme.success),
            );
        } else if self.stuck() {
            let help = self.key_display(ActionKind::OpenHelp).unwrap_or_default();
            push(
                &mut spans,
                &mut x,
                format!(" · stuck? {help} to ask, or skip"),
                Style::default().fg(theme.error),
            );
        } else if let Some(label) = self.spot_label() {
            push(
                &mut spans,
                &mut x,
                format!("   {} {label}", self.glyph("▸", ">")),
                accent,
            );
        }
        f.render_widget(Paragraph::new(Line::from(spans)), bottom);
    }
}

/// Accent-frame a pane rect as the current step's spotlight target,
/// drawn over the pane after it renders. The panes lazybox draws have
/// no full border of their own (the sidebar's top row is its header, the
/// terminal paints PTY cells edge-to-edge), so a `Block`-border overlay
/// would *overwrite* that content. Instead this only ever recolors the
/// perimeter: a blank edge cell becomes a frame glyph in the accent, and
/// a cell that already holds content keeps its glyph and just gains an
/// accent background — the frame never occludes what the user is meant
/// to be reading. `ascii` picks box glyphs honoring `display.ascii_glyphs`.
pub(crate) fn spotlight(f: &mut Frame, rect: Rect, ascii: bool) {
    if rect.width < 2 || rect.height < 2 {
        return;
    }
    let theme = crate::theme::current();
    let (h, v, tl, tr, bl, br) = if ascii {
        ("-", "|", "+", "+", "+", "+")
    } else {
        ("─", "│", "┌", "┐", "└", "┘")
    };
    let x0 = rect.x;
    let x1 = rect.x + rect.width - 1;
    let y0 = rect.y;
    let y1 = rect.y + rect.height - 1;
    let buf = f.buffer_mut();
    let area = buf.area;
    let frame_cell = |buf: &mut Buffer, x: u16, y: u16, glyph: &str| {
        if x < area.left() || x >= area.right() || y < area.top() || y >= area.bottom() {
            return;
        }
        let cell = &mut buf[(x, y)];
        if cell.symbol().trim().is_empty() {
            cell.set_symbol(glyph);
            cell.set_fg(theme.accent);
        } else {
            cell.set_bg(theme.accent);
        }
    };
    frame_cell(buf, x0, y0, tl);
    frame_cell(buf, x1, y0, tr);
    frame_cell(buf, x0, y1, bl);
    frame_cell(buf, x1, y1, br);
    for x in (x0 + 1)..x1 {
        frame_cell(buf, x, y0, h);
        frame_cell(buf, x, y1, h);
    }
    for y in (y0 + 1)..y1 {
        frame_cell(buf, x0, y, v);
        frame_cell(buf, x1, y, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_tui_core::action::ActionDef;
    use std::collections::BTreeMap;

    fn catalog(agents: &[&str], overrides: &[(&str, &str)]) -> Vec<CatalogEntry> {
        let agents = agents.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let overrides = overrides
            .iter()
            .map(|(a, k)| (a.to_string(), k.to_string()))
            .collect::<BTreeMap<_, _>>();
        ActionDef::catalog_with_tiers(&agents, &overrides, &[])
    }

    fn default_catalog() -> Vec<CatalogEntry> {
        catalog(&["claude", "codex", "cursor"], &[])
    }

    fn coach() -> Coach {
        Coach::new(default_catalog(), 0, false, ']', 0)
    }

    fn snap(focus: CoachFocus, cursor: usize, agent_running: bool) -> CoachSnapshot {
        CoachSnapshot {
            focus,
            cursor,
            agent_running,
        }
    }

    /// Every step must reference only actions that exist in the catalog,
    /// so no step can advertise a key the user doesn't have (AC #3).
    #[test]
    fn every_referenced_action_exists_in_the_catalog() {
        let c = coach();
        let mut hints = 0;
        for step in STEPS {
            for seg in step.objective {
                match seg {
                    Key(kind) => {
                        assert!(
                            c.action_entry(*kind).is_some(),
                            "step references {kind:?} missing from catalog",
                        );
                        hints += 1;
                    }
                    // The terminal-exit gesture is the escape leader, not
                    // a catalog action; it still teaches a key.
                    TermExit => hints += 1,
                    Text(_) => {}
                }
            }
        }
        assert!(
            hints >= STEPS.len(),
            "each step should teach at least one key"
        );
    }

    /// Under every shipped keymap preset, every referenced action still
    /// resolves to an effective binding — so no preset leaves a step
    /// advertising a key the user doesn't have (AC #3, #6).
    #[test]
    fn every_step_has_effective_keys_under_each_preset() {
        for preset in ["default", "vim"] {
            let overrides = lazybox_tui_core::action::keymap_preset(preset)
                .unwrap_or_default()
                .into_iter()
                .collect::<Vec<_>>();
            let overrides = overrides
                .iter()
                .map(|(a, k)| (a.as_str(), k.as_str()))
                .collect::<Vec<_>>();
            let c = Coach::new(catalog(&["claude"], &overrides), 0, false, ']', 0);
            for step in STEPS {
                for seg in step.objective {
                    if let Key(kind) = seg {
                        assert!(
                            c.key_display(*kind).is_some(),
                            "{preset}: {kind:?} has no effective key",
                        );
                    }
                }
            }
        }
    }

    /// At most one step may be purely informational (AC #2).
    #[test]
    fn at_most_one_informational_step() {
        let info = STEPS.iter().filter(|s| s.goal == Goal::Info).count();
        assert!(info <= 1, "found {info} informational steps");
    }

    /// Each goal must be reachable by driving real snapshots/actions —
    /// the coach can always be completed to the end (AC #2, degradation).
    #[test]
    fn every_goal_is_reachable_and_the_coach_completes() {
        let mut c = coach();
        // 1 · Opened — move the cursor.
        assert!(c.observe(&snap(CoachFocus::Sidebar, 1, false)));
        assert!(!c.advance(1));
        // 2 · AgentRunning — an agent terminal comes up.
        assert!(c.observe(&snap(CoachFocus::Sidebar, 1, true)));
        assert!(!c.advance(1));
        // 3 · SteppedOut — enter a terminal, then leave it.
        assert!(!c.observe(&snap(CoachFocus::Terminal, 1, true)));
        assert!(c.observe(&snap(CoachFocus::Sidebar, 1, true)));
        assert!(!c.advance(1));
        // 4 · Returned — focus a terminal again.
        assert!(c.observe(&snap(CoachFocus::Terminal, 1, true)));
        assert!(!c.advance(1));
        // 5 · Jumped — dispatch a jump action.
        assert!(!c.observe(&snap(CoachFocus::Sidebar, 1, true)));
        c.note_jump();
        assert!(c.observe(&snap(CoachFocus::Sidebar, 1, true)));
        assert!(!c.advance(1));
        // 6 · Info — never auto-completes; advancing off it finishes.
        assert!(!c.observe(&snap(CoachFocus::Sidebar, 1, true)));
        assert!(c.advance(1));
    }

    /// A snapshot alone doesn't advance a step whose goal isn't met, and
    /// a satisfied step fires its success exactly once.
    #[test]
    fn goal_gates_advance_and_success_fires_once() {
        let mut c = coach();
        // Same row, still in sidebar → step 1 not met.
        assert!(!c.observe(&snap(CoachFocus::Sidebar, 0, false)));
        assert!(!c.is_satisfied());
        // Opening the workspace (focus leaves sidebar) meets it, once.
        assert!(c.observe(&snap(CoachFocus::Activity, 0, false)));
        assert!(c.is_satisfied());
        assert!(!c.observe(&snap(CoachFocus::Activity, 0, false)));
    }

    #[test]
    fn skip_advances_past_a_step_and_never_returns() {
        let mut c = coach();
        assert_eq!(c.step_index(), 0);
        assert!(!c.advance(0)); // skip step 1
        assert_eq!(c.step_index(), 1);
        // Re-observing step 1's goal must not drag us back.
        c.observe(&snap(CoachFocus::Sidebar, 5, false));
        assert_eq!(c.step_index(), 1);
    }

    #[test]
    fn resume_step_is_clamped_into_range() {
        let c = Coach::new(default_catalog(), 999, false, ']', 0);
        assert_eq!(c.step_index(), STEP_COUNT - 1);
    }

    #[test]
    fn key_hints_follow_remaps() {
        let c = Coach::new(
            catalog(&["claude"], &[("work", "Ctrl-y")]),
            1,
            false,
            ']',
            0,
        );
        assert_eq!(c.key_display(ActionKind::Work).as_deref(), Some("Ctrl-y"));
    }

    #[test]
    fn renders_at_minimum_size_and_with_ascii() {
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;
        // A 2-row rail on an 80-col, 22-row terminal (the old deck's
        // clip point, #600) plus ascii glyphs.
        for ascii in [false, true] {
            let mut c = Coach::new(default_catalog(), 0, ascii, ']', 0);
            let mut term = Terminal::new(TestBackend::new(80, 22)).unwrap();
            term.draw(|f| c.render(f, Rect::new(0, 20, 80, 2))).unwrap();
            let buf = term.backend().buffer().clone();
            let text: String = (0..buf.area.width)
                .map(|x| buf[(x, 20)].symbol().to_string())
                .collect();
            assert!(text.contains("1/6"), "progress badge missing: {text:?}");
        }
    }

    #[test]
    fn click_resolves_controls() {
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;
        let mut c = coach();
        let mut term = Terminal::new(TestBackend::new(80, 22)).unwrap();
        term.draw(|f| c.render(f, Rect::new(0, 20, 80, 2))).unwrap();
        let skip = c.skip_btn.expect("skip hit-box");
        let end = c.end_btn.expect("end hit-box");
        assert_eq!(c.on_click(skip.x, skip.y), CoachClick::SkipStep);
        assert_eq!(c.on_click(end.x, end.y), CoachClick::End);
        assert_eq!(c.on_click(0, 0), CoachClick::None);
    }

    /// The terminal-exit step must teach `]]q` (the escape leader, which
    /// actually leaves a live terminal), following the configured escape
    /// char — never `CyclePane`/Tab, which the PTY swallows once the user
    /// has typed to the agent (#1460 F1).
    #[test]
    fn terminal_exit_step_teaches_the_escape_leader_following_config() {
        let theme = crate::theme::current();
        for esc in [']', '\\'] {
            // Step index 2 = the "step back to the inbox" objective.
            let c = Coach::new(default_catalog(), 2, false, esc, 0);
            let text: String = c
                .objective_spans(theme)
                .iter()
                .map(|s| s.content.to_string())
                .collect();
            assert!(
                text.contains(&format!("{esc}{esc}q")),
                "exit gesture missing for esc {esc:?}: {text:?}",
            );
            assert!(
                !text.contains("Tab"),
                "must not teach the PTY-swallowed Tab: {text:?}",
            );
        }
    }

    /// The spotlight must never overwrite a pane's content — the panes it
    /// frames have no border of their own, so a `Block`-border overlay
    /// would eat the sidebar header / terminal edge cells (#1460 F2).
    #[test]
    fn spotlight_never_overwrites_pane_content() {
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;
        let mut term = Terminal::new(TestBackend::new(20, 6)).unwrap();
        term.draw(|f| {
            // Fill the top row edge-to-edge, like a sidebar header.
            f.render_widget(
                Paragraph::new("ABCDEFGHIJKLMNOPQRST"),
                Rect::new(0, 0, 20, 1),
            );
            spotlight(f, Rect::new(0, 0, 20, 6), false);
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let top: String = (0..20).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert_eq!(
            top, "ABCDEFGHIJKLMNOPQRST",
            "spotlight overwrote header content"
        );
        // A blank interior edge cell becomes a frame glyph instead.
        assert_eq!(buf[(0, 3)].symbol(), "│");
    }

    /// The spotlight frame honors `display.ascii_glyphs` (#1460 F3).
    #[test]
    fn spotlight_honors_ascii_glyphs() {
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;
        let mut term = Terminal::new(TestBackend::new(10, 5)).unwrap();
        term.draw(|f| spotlight(f, Rect::new(0, 0, 10, 5), true))
            .unwrap();
        let buf = term.backend().buffer().clone();
        assert_eq!(buf[(0, 0)].symbol(), "+", "ascii corner");
        assert_eq!(buf[(0, 2)].symbol(), "|", "ascii vertical edge");
        assert_eq!(buf[(4, 0)].symbol(), "-", "ascii horizontal edge");
    }

    /// A step that sits unsatisfied long enough offers a hand, resets on
    /// advance, and never fires on the informational step (#1460 F5).
    #[test]
    fn stuck_fires_after_idle_and_resets() {
        let mut c = coach();
        assert!(!c.stuck());
        c.step_started_at = Instant::now() - STUCK_AFTER;
        assert!(c.stuck());
        // Satisfying the goal clears stuck.
        assert!(c.observe(&snap(CoachFocus::Activity, 0, false)));
        assert!(!c.stuck());
        // Advancing re-baselines the timer.
        c.advance(0);
        assert!(!c.stuck());
        // The informational last step never reports stuck.
        let mut last = Coach::new(default_catalog(), STEP_COUNT - 1, false, ']', 0);
        last.step_started_at = Instant::now() - STUCK_AFTER;
        assert!(!last.stuck());
    }
}
