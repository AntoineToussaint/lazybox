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
use lazybox_tui_core::action::{ActionDef, ActionKind};
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

#[derive(Debug, Clone, Copy)]
enum TourSegment {
    Text(&'static str),
    Hint(ActionKind),
    Leader(ActionKind),
}

type TourLine = &'static [TourSegment];

macro_rules! line {
    ($text:literal) => {
        &[TourSegment::Text($text)]
    };
    ($($segment:expr),+ $(,)?) => {
        &[$($segment),+]
    };
}

struct TourStep {
    title: &'static str,
    body: &'static [TourLine],
}

fn render_line(line: TourLine) -> String {
    let mut rendered = String::new();
    for segment in line {
        match segment {
            TourSegment::Text(text) => rendered.push_str(text),
            TourSegment::Hint(kind) => {
                rendered.push_str(ActionDef::for_kind(*kind).default_keys);
            }
            TourSegment::Leader(kind) => {
                let keys = ActionDef::for_kind(*kind).default_keys;
                rendered.push_str(keys.split_whitespace().next().unwrap_or(keys));
            }
        }
    }
    rendered
}

const STEPS: &[TourStep] = &[
    TourStep {
        title: "Welcome to lazybox",
        body: &[
            line!("lazybox turns work into sessions: every task gets its own"),
            line!("git worktree with an agent (Claude Code) or a shell running"),
            line!("in it — and lazybox hides the worktree juggling for you."),
            line!(""),
            line!("Track GitHub repos and their PRs/issues flow into your"),
            line!("inbox, but you don't need any of that to start — a"),
            line!("worktree-backed session works from an empty inbox too."),
            line!(""),
            line!("Keyboard or mouse — lazybox drives fully with either."),
            line!("Step with → / Enter (or click below); ← goes back,"),
            line!(
                TourSegment::Text("Esc skips. Re-open this tour any time with "),
                TourSegment::Hint(ActionKind::OpenTour),
                TourSegment::Text("."),
            ),
        ],
    },
    TourStep {
        title: "1 · Start from scratch",
        body: &[
            line!("Empty inbox? Here's a first move that needs no PRs:"),
            line!(""),
            line!(
                TourSegment::Text("  "),
                TourSegment::Hint(ActionKind::NewProject),
                TourSegment::Text("       new project (register a repo or local space)"),
            ),
            line!(
                TourSegment::Text("  "),
                TourSegment::Hint(ActionKind::NewWorkspace),
                TourSegment::Text("       new workspace inside it"),
            ),
            line!(
                TourSegment::Text("  "),
                TourSegment::Hint(ActionKind::SpawnAgent),
                TourSegment::Text(" / "),
                TourSegment::Hint(ActionKind::SpawnShell),
                TourSegment::Text("   start an agent or shell in it"),
            ),
            line!(""),
            line!("You land in a fresh git worktree + session, zero setup."),
            line!(""),
            line!(
                TourSegment::Text("In a hurry? "),
                TourSegment::Hint(ActionKind::StartAgent),
                TourSegment::Text(" does project → workspace → agent in"),
            ),
            line!("one step, from any pane."),
        ],
    },
    TourStep {
        title: "2 · Your inbox",
        body: &[
            line!("Once you track GitHub repos, PRs and issues flow in,"),
            line!("grouped by repo and sorted by what needs you."),
            line!(""),
            line!("Rows carry attention signals so you triage at a glance:"),
            line!("  • CI failing            • review pending"),
            line!("  • agent asking          • unread activity"),
            line!(""),
            line!(
                TourSegment::Text("↑ / ↓ move    "),
                TourSegment::Hint(ActionKind::OpenWorkspace),
                TourSegment::Text(" opens    "),
                TourSegment::Hint(ActionKind::OpenSearch),
                TourSegment::Text(" searches"),
            ),
            line!(
                TourSegment::Hint(ActionKind::CycleMailbox),
                TourSegment::Text(" cycles mailboxes: Inbox → Inactive → Snoozed."),
            ),
            line!(""),
            line!("Prefer the mouse? lazybox is fully clickable:"),
            line!("  • click a pane to focus it, a row to select it"),
            line!("  • drag the splitters to resize, wheel scrolls back"),
            line!("  • right-click a link in a terminal to open it"),
        ],
    },
    TourStep {
        title: "3 · Put an agent on it",
        body: &[
            line!(
                TourSegment::Text("Press "),
                TourSegment::Hint(ActionKind::Work),
                TourSegment::Text(" on any row and lazybox opens a worktree, then"),
            ),
            line!("launches Claude Code with a prompt fit to the task. A few"),
            line!("ways that plays out:"),
            line!(""),
            line!(
                TourSegment::Text("  • A PR you review has failing CI → "),
                TourSegment::Hint(ActionKind::JumpToFailingCi),
                TourSegment::Text(" jumps to it,"),
            ),
            line!(
                TourSegment::Text("    then "),
                TourSegment::Hint(ActionKind::Work),
                TourSegment::Text(" lets the agent fix the build."),
            ),
            line!(
                TourSegment::Text("  • An open issue → "),
                TourSegment::Hint(ActionKind::Work),
                TourSegment::Text(" starts an agent to implement it."),
            ),
            line!(
                TourSegment::Text("  • A scratch idea on a repo → "),
                TourSegment::Hint(ActionKind::NewWorkspace),
                TourSegment::Text(", "),
                TourSegment::Hint(ActionKind::SpawnAgent),
                TourSegment::Text(", done."),
            ),
            line!(""),
            line!(
                TourSegment::Leader(ActionKind::SpawnAgent),
                TourSegment::Text(" opens the agent menu: "),
                TourSegment::Hint(ActionKind::SpawnAgent),
                TourSegment::Text(" pick Claude /"),
            ),
            line!(
                TourSegment::Text("Codex / Cursor; "),
                TourSegment::Hint(ActionKind::SpawnShell),
                TourSegment::Text(" is a plain shell; "),
                TourSegment::Hint(ActionKind::OpenEditor),
                TourSegment::Text(" opens the worktree"),
            ),
            line!("in your editor."),
        ],
    },
    TourStep {
        title: "4 · Juggle many sessions",
        body: &[
            line!("Every task is its own worktree-backed session, so you can"),
            line!("run several at once without minding the git plumbing."),
            line!(""),
            line!(
                TourSegment::Text("  "),
                TourSegment::Hint(ActionKind::JumpToWorkspace),
                TourSegment::Text("          jump to any workspace (fuzzy picker, all repos)"),
            ),
            line!(
                TourSegment::Text("  "),
                TourSegment::Hint(ActionKind::JumpToAsking),
                TourSegment::Text("          jump to the next agent waiting on your input"),
            ),
            line!(
                TourSegment::Text("  "),
                TourSegment::Hint(ActionKind::JumpToFailingCi),
                TourSegment::Text("    jump to the next PR with failing CI"),
            ),
            line!(
                TourSegment::Text("  "),
                TourSegment::Hint(ActionKind::AdoptSessions),
                TourSegment::Text("        adopt worktrees/agents started elsewhere"),
            ),
            line!(
                TourSegment::Text("  "),
                TourSegment::Hint(ActionKind::CyclePane),
                TourSegment::Text("        cycle the sidebar, activity and terminal panes"),
            ),
            line!(""),
            line!(
                TourSegment::Text("Jump works from anywhere — inside an agent, press ]] then "),
                TourSegment::Hint(ActionKind::JumpToWorkspace),
                TourSegment::Text("."),
            ),
        ],
    },
    TourStep {
        title: "5 · Ship it & make it yours",
        body: &[
            line!(
                TourSegment::Text("When a PR is ready, press "),
                TourSegment::Leader(ActionKind::MergePr),
                TourSegment::Text(" — a which-key menu pops up"),
            ),
            line!("showing everything you can do to this PR:"),
            line!(""),
            line!(
                TourSegment::Text("  "),
                TourSegment::Hint(ActionKind::MergePr),
                TourSegment::Text("  merge PR    "),
                TourSegment::Hint(ActionKind::RequestReviewers),
                TourSegment::Text("  reviewers   "),
                TourSegment::Hint(ActionKind::AddAssignees),
                TourSegment::Text("  assignees"),
            ),
            line!(
                TourSegment::Text("  "),
                TourSegment::Hint(ActionKind::ManageLabels),
                TourSegment::Text("  labels      "),
                TourSegment::Hint(ActionKind::OpenInBrowser),
                TourSegment::Text("  open in browser"),
            ),
            line!(""),
            line!(
                TourSegment::Leader(ActionKind::MergePr),
                TourSegment::Text(" shows the menu; the second key picks. Grouped actions"),
            ),
            line!("live behind their leader only — re-add direct aliases via"),
            line!("ui.action_keys if you miss them."),
            line!(""),
            line!(
                TourSegment::Hint(ActionKind::OpenHelp),
                TourSegment::Text(" opens Ask Lazybox (press "),
                TourSegment::Hint(ActionKind::OpenHelp),
                TourSegment::Text(" again for all keys); "),
                TourSegment::Hint(ActionKind::OpenSettings),
                TourSegment::Text(" opens"),
            ),
            line!(
                TourSegment::Text("Settings, "),
                TourSegment::Hint(ActionKind::OpenThemePicker),
                TourSegment::Text(" themes, "),
                TourSegment::Hint(ActionKind::Quit),
                TourSegment::Text(" quits. In a terminal, press ]]q"),
            ),
            line!("to return first—the embedded program owns other keys."),
            line!(""),
            line!(
                TourSegment::Hint(ActionKind::OpenThemePicker),
                TourSegment::Text(" opens the theme picker: light + dark, live preview,"),
            ),
            line!("and your pick persists across restarts."),
            line!(""),
            line!("It's all plain YAML in ~/.lazybox/config.yaml: scopes,"),
            line!("agents, keybindings, theme."),
            line!(""),
            line!(
                TourSegment::Text("That's the tour — Enter to finish, "),
                TourSegment::Hint(ActionKind::OpenTour),
                TourSegment::Text(" to re-open it."),
            ),
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
        for &line in step.body {
            lines.push(Line::from(Span::styled(
                format!("  {}", render_line(line)),
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
            for &line in s.body {
                let rendered = render_line(line);
                assert!(
                    rendered.chars().count() <= 62,
                    "step {:?} line too wide ({}): {rendered:?}",
                    s.title,
                    rendered.chars().count(),
                );
            }
        }
    }

    #[test]
    fn covers_from_scratch_entry_points() {
        // A fresh user with an empty inbox must see a first move that
        // needs no pre-existing row: new project / new workspace.
        let all = render_all();
        assert!(all.contains("x p"), "new-project key missing");
        assert!(all.contains("new workspace"), "new-workspace step missing");
        assert!(
            all.contains("Start from scratch"),
            "from-scratch step missing",
        );
    }

    #[test]
    fn mentions_adopt_sessions() {
        assert!(render_all().contains("x a"), "adopt-sessions key missing");
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
    }

    #[test]
    fn key_hints_reference_the_action_catalog() {
        let mut hint_count = 0;
        for step in STEPS {
            for line in step.body {
                for segment in *line {
                    let kind = match segment {
                        TourSegment::Hint(kind) | TourSegment::Leader(kind) => *kind,
                        TourSegment::Text(_) => continue,
                    };
                    assert_eq!(ActionDef::for_kind(kind).kind, kind);
                    hint_count += 1;
                }
            }
        }
        assert!(hint_count > 0);

        // `]]` is `terminal.escape_char` (default `]`) doubled, owned
        // by the terminal latch rather than the catalog.
        assert!(render_all().contains("]]"));
    }

    #[test]
    fn github_leader_demo_matches_the_catalog() {
        use lazybox_tui_core::action::{ActionDef, ActionKind};
        // The ship-it step demonstrates the g leader as a menu, and its
        // chords + labels must be the catalog's — so the demo can't
        // drift from the live keymap (issue #145, #114).
        let all = render_all();
        assert!(
            all.contains("press g"),
            "tour no longer frames g as a which-key menu"
        );
        // The github actions carry their `g <key>` leader as a catalog
        // chord now (ActionGroup is gone); read it straight off the def.
        let github = [
            ActionKind::MergePr,
            ActionKind::RequestReviewers,
            ActionKind::AddAssignees,
            ActionKind::ManageLabels,
            ActionKind::OpenInBrowser,
        ];
        for kind in github {
            let def = ActionDef::for_kind(kind);
            let chord = def
                .default_chords()
                .into_iter()
                .find_map(|c| match c {
                    lazybox_tui_core::action::Chord::Seq(strokes) if strokes.len() == 2 => {
                        Some(format!("{} {}", strokes[0].display(), strokes[1].display()))
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("{kind:?} missing a leader sequence"));
            assert!(all.contains(&chord), "tour missing leader chord {chord}");
            assert!(
                all.contains(def.label),
                "tour missing label {:?} for {chord}",
                def.label
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
