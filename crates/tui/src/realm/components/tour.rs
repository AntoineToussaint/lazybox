//! `Tour` — the in-app feature walkthrough (issue #146).
//!
//! A skippable, stepped overlay card that introduces lazybox's
//! highlights: the inbox + attention signals, spawning work, the
//! snippet system (`]<key>`), navigation, and where config lives.
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
use tuirealm::event::{Event, Key, KeyEvent, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use tuirealm::state::State;

/// One tour card: a heading plus the body lines under it. Body keeps
/// inline key hints (`]<key>`, `w`, `Tab`) as plain text — the
/// default bindings are stable and a card reads cleaner than a table.
struct TourStep {
    title: &'static str,
    body: &'static [&'static str],
}

const STEPS: &[TourStep] = &[
    TourStep {
        title: "Welcome to lazybox",
        body: &[
            "A reactive PR inbox in your terminal. Instead of polling",
            "GitHub, events flow to you — new comments, CI failures and",
            "review requests surface automatically, with read/unread",
            "tracking.",
            "",
            "This quick tour shows the highlights. Step through with",
            "→ / Enter, go back with ←, or press Esc to skip — you can",
            "re-open it any time with Shift-T.",
        ],
    },
    TourStep {
        title: "1 · The inbox",
        body: &[
            "The sidebar is your inbox: PRs and Issues across every",
            "scope you subscribed to, grouped by repo.",
            "",
            "Rows carry attention signals so you can triage at a glance:",
            "  • CI failing            • review pending",
            "  • input-needed (agent asking)   • unread activity",
            "",
            "j / k move the cursor; Enter opens a row; / searches.",
        ],
    },
    TourStep {
        title: "2 · Spawn work on an item",
        body: &[
            "Press w on a row to start working on it. lazybox spins up a",
            "git worktree and launches the default agent (Claude Code)",
            "with a contextual prompt — fix the failing CI, address the",
            "review, implement the issue.",
            "",
            "The agent runs in the embedded terminal pane on the right.",
            "c / x / u spawn a specific agent; s opens a plain shell.",
        ],
    },
    TourStep {
        title: "3 · Snippets — ]<key>",
        body: &[
            "Inside a terminal, press ] then a snippet key to expand a",
            "pre-defined prompt and auto-send it to the agent. Type the",
            "key to filter; a unique match fires instantly (so ]rev",
            "sends your review prompt in three keystrokes).",
            "",
            "Starter snippets ship out of the box — rev (review the",
            "diff) and pr (open a PR with summary + test plan).",
            "",
            "Add your own in ~/.lazybox/snippets.yaml (global) or a repo's",
            ".lazybox/snippets.yaml (checked in, shared with the team).",
        ],
    },
    TourStep {
        title: "4 · Navigation & layout",
        body: &[
            "Tab cycles focus between the sidebar, activity and terminal",
            "panes. Shift+arrows resize the splitters; mouse-drag works",
            "too.",
            "",
            "! jumps to the next agent waiting on input; Shift-F jumps",
            "to the next failing PR. ? opens the full keymap any time.",
        ],
    },
    TourStep {
        title: "5 · Where config lives",
        body: &[
            "Everything is plain YAML you can hand-edit:",
            "",
            "  ~/.lazybox/config.yaml    scopes, agents, UI, keybindings",
            "  ~/.lazybox/snippets.yaml  your global snippet library",
            "  <repo>/.lazybox/snippets.yaml   repo-local snippets",
            "",
            "Press , for the in-app Settings palette.",
            "",
            "That's the tour — press Enter to finish. Re-open with",
            "Shift-T whenever you want a refresher.",
        ],
    },
];

pub struct Tour {
    /// Index into [`STEPS`]. Always in range — navigation clamps.
    cursor: usize,
}

impl Tour {
    pub fn new() -> Self {
        Self { cursor: 0 }
    }

    fn is_last(&self) -> bool {
        self.cursor + 1 >= STEPS.len()
    }

    /// Pure key handler — kept a method so tests can drive it without
    /// a tuirealm `Application`.
    pub fn on_key(&mut self, key: &KeyEvent) -> Option<Msg> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if matches!(key.code, Key::Esc) || (ctrl && matches!(key.code, Key::Char('c'))) {
            return Some(Msg::TourFinished);
        }
        match key.code {
            // Advance — and finish once we step past the last card.
            Key::Right | Key::Enter | Key::Char(' ' | 'n' | 'l') => {
                if self.is_last() {
                    Some(Msg::TourFinished)
                } else {
                    self.cursor += 1;
                    None
                }
            }
            Key::Left | Key::Backspace | Key::Char('p' | 'h') => {
                self.cursor = self.cursor.saturating_sub(1);
                None
            }
            Key::Char('q') => Some(Msg::TourFinished),
            _ => None,
        }
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

        let progress = format!("step {}/{}", self.cursor + 1, STEPS.len());
        let next_label = if self.is_last() { "finish" } else { "next" };
        let help = Line::from(vec![
            Span::styled(
                format!("  {progress}   "),
                Style::default().fg(theme.text_dim),
            ),
            Span::styled("→/Enter", Style::default().fg(theme.success).bold()),
            Span::styled(
                format!(" {next_label}  "),
                Style::default().fg(theme.text_dim),
            ),
            Span::styled("←", Style::default().fg(theme.accent).bold()),
            Span::styled(" back  ", Style::default().fg(theme.text_dim)),
            Span::styled("Esc", Style::default().fg(theme.error).bold()),
            Span::styled(" skip", Style::default().fg(theme.text_dim)),
        ]);
        frame.render_widget(Paragraph::new(help), help_rect);
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

    #[test]
    fn steps_are_non_empty() {
        assert!(!STEPS.is_empty());
        for s in STEPS {
            assert!(!s.title.is_empty());
            assert!(!s.body.is_empty());
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
}
