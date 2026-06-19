//! `Tour` — the in-app onboarding walkthrough (issue #146, #112).
//!
//! A skippable, stepped overlay card built around the workflows a
//! first-time user actually runs: starting a worktree-backed session
//! from scratch, triaging the inbox, putting an agent on a task,
//! juggling several sessions, and shipping. Each card is a short
//! user story rather than a feature dump, and the flow works even
//! from an empty inbox (a fresh user has no row to act on yet).
//!
//! Launched automatically the first time lazybox boots into the panes
//! (tracked by `~/.lazybox/config.yaml::ui.tour_seen`) and re-invocable
//! on demand via the tour shortcut. It is a plain modal: it owns
//! input while mounted but every key either advances, retreats, or
//! exits, so it never traps focus (mind #90). Any exit — finishing
//! the last step OR pressing Esc/q — returns [`Msg::TourFinished`],
//! which marks the tour seen and pops it.

use crate::realm::Msg;
use crate::realm::UserEvent;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{
    Event, Key, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use tuirealm::state::State;

/// One tour card: a heading plus the body lines under it. Body keeps
/// inline key hints (`w`, `Tab`, `Shift-N`) as plain text — the
/// default bindings are stable and a card reads cleaner than a table.
/// Keys mirror the action catalog (`lazybox_tui_core::action`); when a
/// binding moves there, update the matching hint here.
struct TourStep {
    title: &'static str,
    body: &'static [&'static str],
}

const STEPS: &[TourStep] = &[
    TourStep {
        title: "Welcome to lazybox",
        body: &[
            "lazybox turns work into sessions: every task gets its own",
            "git worktree with an agent (Claude Code) or a shell running",
            "in it — and lazybox hides the worktree juggling for you.",
            "",
            "Track GitHub repos and their PRs/issues flow into your",
            "inbox, but you don't need any of that to start — a",
            "worktree-backed session works from an empty inbox too.",
            "",
            "Keyboard or mouse — lazybox drives fully with either.",
            "Step with → / Enter (or click below); ← goes back,",
            "Esc skips. Re-open this tour any time with Shift-T.",
        ],
    },
    TourStep {
        title: "1 · Start from scratch",
        body: &[
            "Empty inbox? Here's a first move that needs no PRs:",
            "",
            "  Shift-N   new project (register a repo or local space)",
            "  n         new workspace inside it",
            "  c / s     start Claude Code, or a plain shell in it",
            "",
            "You land in a fresh git worktree + session, zero setup.",
            "",
            "In a hurry? Shift-W does project → workspace → agent in",
            "one step, from any pane.",
        ],
    },
    TourStep {
        title: "2 · Your inbox",
        body: &[
            "Once you track GitHub repos, PRs and issues flow in,",
            "grouped by repo and sorted by what needs you.",
            "",
            "Rows carry attention signals so you triage at a glance:",
            "  • CI failing            • review pending",
            "  • agent asking          • unread activity",
            "",
            "↑ / ↓ move    Enter opens    / searches",
            "Shift-S cycles mailboxes: Inbox → Inactive → Snoozed.",
            "",
            "Prefer the mouse? lazybox is fully clickable:",
            "  • click a pane to focus it, a row to select it",
            "  • drag the splitters to resize, wheel scrolls back",
            "  • right-click a link in a terminal to open it",
        ],
    },
    TourStep {
        title: "3 · Put an agent on it",
        body: &[
            "Press w on any row and lazybox opens a worktree, then",
            "launches Claude Code with a prompt fit to the task. A few",
            "ways that plays out:",
            "",
            "  • A PR you review has failing CI → Shift-F jumps to it,",
            "    then w lets the agent fix the build.",
            "  • An open issue → w starts an agent to implement it.",
            "  • A scratch idea on a repo you track → n then c, done.",
            "",
            "c / x / u pick the agent (Claude / Codex / Cursor); s is a",
            "plain shell; e opens the worktree in your editor.",
        ],
    },
    TourStep {
        title: "4 · Juggle many sessions",
        body: &[
            "Every task is its own worktree-backed session, so you can",
            "run several at once without minding the git plumbing.",
            "",
            "  !          jump to the next agent waiting on your input",
            "  Shift-A    adopt worktrees/agents you started elsewhere",
            "  Tab        cycle the sidebar, activity and terminal panes",
            "",
            "The agent runs live in the terminal pane on the right while",
            "new events keep flowing into the sidebar.",
        ],
    },
    TourStep {
        title: "5 · Ship it & make it yours",
        body: &[
            "When a PR is ready, press g — a which-key menu pops up",
            "showing everything you can do to this PR:",
            "",
            "  g m  merge PR    g v  reviewers   g a  assignees",
            "  g l  labels      g o  open in browser",
            "",
            "g shows the menu; the second key picks. The old",
            "Shift-M/V/G/L/O keys still work as direct aliases.",
            "",
            "? shows the full keymap, , Settings, q q quits. In a",
            "terminal keys go to the agent — press ]] to return first.",
            "",
            "It's all plain YAML in ~/.lazybox/config.yaml: scopes,",
            "agents, keybindings.",
            "",
            "That's the tour — Enter to finish, Shift-T to re-open it.",
        ],
    },
];

pub struct Tour {
    /// Index into [`STEPS`]. Always in range — navigation clamps.
    cursor: usize,
    /// Footer hit-boxes recorded by `view`, consumed by `on_mouse` so
    /// an all-mouse user can click through the tour. `None` until the
    /// first render.
    back_btn: Option<Rect>,
    next_btn: Option<Rect>,
    skip_btn: Option<Rect>,
}

impl Tour {
    pub fn new() -> Self {
        Self {
            cursor: 0,
            back_btn: None,
            next_btn: None,
            skip_btn: None,
        }
    }

    fn is_last(&self) -> bool {
        self.cursor + 1 >= STEPS.len()
    }

    /// Step forward, finishing once we move past the last card.
    fn advance(&mut self) -> Option<Msg> {
        if self.is_last() {
            Some(Msg::TourFinished)
        } else {
            self.cursor += 1;
            None
        }
    }

    /// Pure key handler — kept a method so tests can drive it without
    /// a tuirealm `Application`.
    pub fn on_key(&mut self, key: &KeyEvent) -> Option<Msg> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if matches!(key.code, Key::Esc) || (ctrl && matches!(key.code, Key::Char('c'))) {
            return Some(Msg::TourFinished);
        }
        match key.code {
            // Arrow keys lead; j/k-style aliases stay for muscle memory.
            Key::Right | Key::Enter | Key::Char(' ' | 'n' | 'l') => self.advance(),
            Key::Left | Key::Backspace | Key::Char('p' | 'h') => {
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            Key::Char('q') => Some(Msg::TourFinished),
            _ => None,
        }
    }

    /// Pure mouse handler — only left button-down reaches a mounted
    /// modal (drag/scroll are filtered upstream). Skip/Back hit their
    /// footer boxes; any other click advances, so a mouse-only user can
    /// click anywhere on the card to step forward.
    pub fn on_mouse(&mut self, ev: &MouseEvent) -> Option<Msg> {
        if !matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
            return None;
        }
        let hit = |b: Option<Rect>| {
            b.is_some_and(|r| {
                ev.column >= r.x
                    && ev.column < r.x + r.width
                    && ev.row >= r.y
                    && ev.row < r.y + r.height
            })
        };
        if hit(self.skip_btn) {
            return Some(Msg::TourFinished);
        }
        if hit(self.back_btn) {
            self.cursor = self.cursor.saturating_sub(1);
            return None;
        }
        self.advance()
    }
}

impl Default for Tour {
    fn default() -> Self {
        Self::new()
    }
}

impl Component for Tour {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let step = &STEPS[self.cursor];

        let modal_w = 68u16.min(area.width.saturating_sub(4));
        let modal_h = 22u16.min(area.height.saturating_sub(2));
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Span::styled(" Feature tour ", theme.modal_title()))
            .border_style(Style::default().fg(theme.accent));
        let inner = block.inner(modal);
        frame.render_widget(block, modal);
        if inner.height < 4 {
            return;
        }

        let body_rect = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height - 1,
        };
        let help_rect = Rect {
            x: inner.x,
            y: inner.y + inner.height - 1,
            width: inner.width,
            height: 1,
        };

        let mut lines: Vec<Line> = vec![
            Line::raw(""),
            Line::from(Span::styled(
                format!("  {}", step.title),
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::raw(""),
        ];
        for &l in step.body {
            lines.push(Line::from(Span::styled(
                format!("  {l}"),
                Style::default().fg(theme.text_strong),
            )));
        }
        frame.render_widget(Paragraph::new(lines), body_rect);

        // Footer buttons. Each is rendered AND recorded as a hit-box so
        // `on_mouse` can route clicks — the tour is fully mouse-drivable.
        let mut spans: Vec<Span> = Vec::new();
        let mut x = help_rect.x;
        let push = |spans: &mut Vec<Span>, x: &mut u16, text: String, style: Style| -> Rect {
            let w = text.chars().count() as u16;
            let rect = Rect::new(*x, help_rect.y, w, 1);
            spans.push(Span::styled(text, style));
            *x += w;
            rect
        };

        push(
            &mut spans,
            &mut x,
            format!("  step {}/{}   ", self.cursor + 1, STEPS.len()),
            Style::default().fg(theme.text_dim),
        );
        self.back_btn = Some(push(
            &mut spans,
            &mut x,
            " ← Back ".to_string(),
            Style::default().fg(theme.accent).bold(),
        ));
        push(&mut spans, &mut x, "  ".to_string(), Style::default());
        let next_label = if self.is_last() {
            " Finish "
        } else {
            " Next → "
        };
        self.next_btn = Some(push(
            &mut spans,
            &mut x,
            next_label.to_string(),
            Style::default().fg(theme.success).bold(),
        ));
        push(&mut spans, &mut x, "  ".to_string(), Style::default());
        self.skip_btn = Some(push(
            &mut spans,
            &mut x,
            " Esc Skip ".to_string(),
            Style::default().fg(theme.error).bold(),
        ));

        frame.render_widget(Paragraph::new(Line::from(spans)), help_rect);
    }

    fn query(&self, _: Attribute) -> Option<QueryResult<'_>> {
        None
    }
    fn attr(&mut self, _: Attribute, _: AttrValue) {}
    fn state(&self) -> State {
        State::None
    }
    fn perform(&mut self, _: Cmd) -> CmdResult {
        CmdResult::NoChange
    }
}

impl AppComponent<Msg, UserEvent> for Tour {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        match ev {
            Event::Keyboard(key) => self.on_key(key),
            Event::Mouse(m) => self.on_mouse(m),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ke(code: Key) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn click(col: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// Render a tour at `cursor` and hand back the instance — the
    /// footer hit-boxes are only populated after a `view` pass.
    fn rendered_tour(cursor: usize) -> Tour {
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;
        let mut t = Tour::new();
        t.cursor = cursor;
        let (w, h) = (90u16, 30u16);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| t.view(f, Rect::new(0, 0, w, h))).unwrap();
        t
    }

    fn center(r: Rect) -> (u16, u16) {
        (r.x + r.width / 2, r.y)
    }

    /// Render the card at `cursor` into a throwaway backend and return
    /// the visible text — the snapshot surface for the step content.
    fn render_step(cursor: usize) -> String {
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;
        let mut t = Tour::new();
        t.cursor = cursor;
        let (w, h) = (90u16, 30u16);
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| t.view(f, Rect::new(0, 0, w, h))).unwrap();
        let buf = term.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                let mut row = String::new();
                for x in 0..buf.area.width {
                    row.push_str(buf[(x, y)].symbol());
                }
                row.trim_end().to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The whole tour rendered top to bottom — one string to scan for
    /// content invariants that span steps.
    fn render_all() -> String {
        (0..STEPS.len())
            .map(render_step)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn steps_are_non_empty() {
        assert!(!STEPS.is_empty());
        for s in STEPS {
            assert!(!s.title.is_empty());
            assert!(!s.body.is_empty());
        }
    }

    #[test]
    fn body_lines_fit_the_modal_width() {
        // The card is 68 cols wide with a 2-space gutter, so a body
        // line over ~62 chars would clip. Guard the new copy.
        for s in STEPS {
            for l in s.body {
                assert!(
                    l.chars().count() <= 62,
                    "step {:?} line too wide ({}): {l:?}",
                    s.title,
                    l.chars().count(),
                );
            }
        }
    }

    #[test]
    fn covers_from_scratch_entry_points() {
        // A fresh user with an empty inbox must see a first move that
        // needs no pre-existing row: new project / new workspace.
        let all = render_all();
        assert!(all.contains("Shift-N"), "new-project key missing");
        assert!(all.contains("new workspace"), "new-workspace step missing");
        assert!(
            all.contains("Start from scratch"),
            "from-scratch step missing",
        );
    }

    #[test]
    fn mentions_adopt_sessions() {
        assert!(
            render_all().contains("Shift-A"),
            "adopt-sessions key missing"
        );
    }

    #[test]
    fn snippets_step_is_gone() {
        // Snippets are a power-user feature; onboarding shouldn't carry
        // them. Guard against the step creeping back in.
        let all = render_all().to_lowercase();
        assert!(
            !all.contains("snippet"),
            "snippets leaked back into the tour"
        );
        assert!(!all.contains("]<key>"), "snippet leader hint still present");
    }

    #[test]
    fn key_hints_match_the_action_catalog() {
        use lazybox_tui_core::action::{ActionDef, ActionKind};
        // Each hint shown in the tour must be the catalog's current
        // default for that action — the catalog is the source of truth.
        let all = render_all();
        for (kind, hint) in [
            (ActionKind::NewProject, "Shift-N"),
            (ActionKind::NewWorkspace, "n"),
            (ActionKind::AdoptSessions, "Shift-A"),
            (ActionKind::JumpToAsking, "!"),
            (ActionKind::JumpToFailingCi, "Shift-F"),
            (ActionKind::StartAgent, "Shift-W"),
            (ActionKind::CyclePane, "Tab"),
        ] {
            assert_eq!(
                ActionDef::for_kind(kind).default_keys,
                hint,
                "catalog default for {kind:?} drifted from the tour hint",
            );
            assert!(all.contains(hint), "tour no longer shows {hint}");
        }
    }

    #[test]
    fn github_leader_demo_matches_the_catalog() {
        use lazybox_tui_core::action::{ActionDef, ActionGroup};
        // The ship-it step demonstrates the g leader as a menu, and its
        // chords + labels must be the catalog's — so the demo can't
        // drift from the live keymap (issue #145, #114).
        let all = render_all();
        assert!(
            all.contains("press g"),
            "tour no longer frames g as a which-key menu"
        );
        let group = ActionGroup::Github;
        for (key, kind) in group.members() {
            let chord = format!("{} {key}", group.leader());
            assert!(all.contains(&chord), "tour missing leader chord {chord}");
            let label = ActionDef::for_kind(*kind).label;
            assert!(
                all.contains(label),
                "tour missing label {label:?} for {chord}"
            );
        }
    }

    #[test]
    fn advances_then_finishes_on_last_step() {
        let mut t = Tour::new();
        // Walk forward through every step but the last — no Msg.
        for _ in 0..STEPS.len() - 1 {
            assert_eq!(t.on_key(&ke(Key::Right)), None);
        }
        assert!(t.is_last());
        // One more advance off the last card finishes the tour.
        assert_eq!(t.on_key(&ke(Key::Enter)), Some(Msg::TourFinished));
    }

    #[test]
    fn back_clamps_at_first_step() {
        let mut t = Tour::new();
        assert_eq!(t.on_key(&ke(Key::Left)), None);
        assert_eq!(t.cursor, 0);
        let _ = t.on_key(&ke(Key::Right));
        assert_eq!(t.cursor, 1);
        let _ = t.on_key(&ke(Key::Left));
        assert_eq!(t.cursor, 0);
    }

    #[test]
    fn esc_and_q_finish_immediately() {
        let mut t = Tour::new();
        assert_eq!(t.on_key(&ke(Key::Esc)), Some(Msg::TourFinished));
        let mut t = Tour::new();
        assert_eq!(t.on_key(&ke(Key::Char('q'))), Some(Msg::TourFinished));
    }

    #[test]
    fn ctrl_c_finishes() {
        let mut t = Tour::new();
        let ev = KeyEvent::new(Key::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(t.on_key(&ev), Some(Msg::TourFinished));
    }

    #[test]
    fn leads_with_arrow_keys_not_jk() {
        // The inbox step taught navigation with `j / k`; onboarding now
        // leads with the arrow keys most newcomers reach for.
        let all = render_all();
        assert!(all.contains("↑ / ↓ move"), "arrow-key nav hint missing");
        assert!(
            !all.contains("j / k"),
            "j/k is still the primary nav instruction"
        );
    }

    #[test]
    fn advertises_mouse_support() {
        // Mouse support is a first-class point: focus, select, resize,
        // scroll, and right-click-to-open links.
        let all = render_all().to_lowercase();
        for needle in [
            "click a pane",
            "row to select",
            "resize",
            "scrolls",
            "right-click",
        ] {
            assert!(all.contains(needle), "mouse hint {needle:?} missing");
        }
    }

    #[test]
    fn click_advances_then_finishes_on_last_step() {
        let mut t = rendered_tour(0);
        // A click anywhere that isn't Back/Skip steps forward.
        assert_eq!(t.on_mouse(&click(1, 1)), None);
        assert_eq!(t.cursor, 1);
        // The Next button advances too.
        let (nx, ny) = center(t.next_btn.unwrap());
        assert_eq!(t.on_mouse(&click(nx, ny)), None);
        assert_eq!(t.cursor, 2);

        let mut last = rendered_tour(STEPS.len() - 1);
        let (nx, ny) = center(last.next_btn.unwrap());
        assert_eq!(last.on_mouse(&click(nx, ny)), Some(Msg::TourFinished));
    }

    #[test]
    fn clicking_back_button_retreats() {
        let mut t = rendered_tour(2);
        let (bx, by) = center(t.back_btn.unwrap());
        assert_eq!(t.on_mouse(&click(bx, by)), None);
        assert_eq!(t.cursor, 1);
    }

    #[test]
    fn clicking_skip_button_finishes() {
        let mut t = rendered_tour(0);
        let (sx, sy) = center(t.skip_btn.unwrap());
        assert_eq!(t.on_mouse(&click(sx, sy)), Some(Msg::TourFinished));
    }

    #[test]
    fn non_left_clicks_are_ignored() {
        let mut t = rendered_tour(1);
        let ev = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Right),
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        };
        assert_eq!(t.on_mouse(&ev), None);
        assert_eq!(t.cursor, 1, "right-click must not navigate");
    }
}
