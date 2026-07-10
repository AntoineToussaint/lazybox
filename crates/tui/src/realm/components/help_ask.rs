//! `HelpAsk` — ask lazybox how to use lazybox (#302).
//!
//! Opened by pressing `?` on the `?` help panel. One modal, two
//! layers:
//!
//! - **Typing** fuzzy-searches the runtime action catalog live
//!   (`lazybox_tui_core::help::search`) — the offline layer that
//!   answers most "where is X" queries instantly, with the user's
//!   effective (post-override) keys.
//! - **Enter** escalates the typed text as a question to a headless
//!   Claude run (`Command::StartAgentRun`, stream-json — no PTY, no
//!   worktree) whose first message is the generated catalog + docs
//!   context. The streamed markdown answer renders here via
//!   `comment_render`.
//!
//! The conversation state lives in a `SharedHelpConvo` owned by the
//! `Model` and mutated by daemon-event handlers while this component
//! is mounted — shared by `Arc<Mutex<…>>` so a text delta doesn't
//! have to remount the modal (which would drop the user's in-flight
//! typing). It persists across open/close: the agent run stays alive
//! for the app's lifetime, so follow-up questions are cheap.

use crate::components::comment_render;
use crate::realm::Msg;
use crate::realm::UserEvent;
use lazybox_tui_core::action::CatalogEntry;
use std::sync::{Arc, Mutex, MutexGuard};
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyEvent, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use tuirealm::state::State;

/// One question → streamed-answer exchange with the help agent.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct HelpTurn {
    pub question: String,
    /// Accumulated `AgentAssistantTextDelta` markdown; replaced by the
    /// authoritative result text when the turn finishes.
    pub answer: String,
    pub done: bool,
}

/// Help-conversation state shared between the `Model` (which feeds it
/// from daemon events) and a mounted `HelpAsk` (which renders it).
#[derive(Debug, Default)]
pub struct HelpConvo {
    pub turns: Vec<HelpTurn>,
    /// Out-of-band status shown under the transcript — agent
    /// unavailable, run exited, spawn failure. Cleared on the next
    /// question.
    pub notice: Option<String>,
}

impl HelpConvo {
    /// The turn currently streaming, if any.
    pub fn streaming_turn_mut(&mut self) -> Option<&mut HelpTurn> {
        self.turns.last_mut().filter(|t| !t.done)
    }
}

pub type SharedHelpConvo = Arc<Mutex<HelpConvo>>;

pub struct HelpAsk {
    /// Catalog snapshot taken at mount — the search corpus.
    catalog: Vec<CatalogEntry>,
    convo: SharedHelpConvo,
    query: String,
    /// Indices into `catalog` matching `query`, best first.
    matches: Vec<usize>,
    /// Transcript scroll, in lines up from the bottom. 0 = pinned to
    /// the bottom so a streaming answer auto-follows.
    scroll_up: usize,
}

impl HelpAsk {
    pub fn new(catalog: Vec<CatalogEntry>, convo: SharedHelpConvo) -> Self {
        Self {
            catalog,
            convo,
            query: String::new(),
            matches: Vec::new(),
            scroll_up: 0,
        }
    }

    fn convo(&self) -> MutexGuard<'_, HelpConvo> {
        match self.convo.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn refilter(&mut self) {
        self.matches = lazybox_tui_core::help::search(&self.catalog, &self.query);
    }

    fn on_key(&mut self, key: &KeyEvent) -> Option<Msg> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if matches!(key.code, Key::Esc) || (ctrl && matches!(key.code, Key::Char('c'))) {
            return Some(Msg::ModalDismissed);
        }
        match key.code {
            Key::Enter => {
                let question = self.query.trim().to_string();
                if question.is_empty() {
                    return None;
                }
                self.query.clear();
                self.refilter();
                self.scroll_up = 0;
                Some(Msg::HelpAsked(question))
            }
            Key::Backspace => {
                self.query.pop();
                self.refilter();
                None
            }
            Key::Up => {
                self.scroll_up = self.scroll_up.saturating_add(1);
                None
            }
            Key::Down => {
                self.scroll_up = self.scroll_up.saturating_sub(1);
                None
            }
            Key::PageUp => {
                self.scroll_up = self.scroll_up.saturating_add(10);
                None
            }
            Key::PageDown => {
                self.scroll_up = self.scroll_up.saturating_sub(10);
                None
            }
            Key::Char(c) if !ctrl => {
                self.query.push(c);
                self.refilter();
                None
            }
            _ => None,
        }
    }

    /// Body content for the current mode: catalog matches while a
    /// query is being typed, the conversation transcript otherwise.
    /// Lines are pre-wrapped to `width` so `Paragraph::scroll` offsets
    /// count rendered rows.
    fn body_lines(&self, width: u16) -> Vec<Line<'static>> {
        if !self.query.trim().is_empty() {
            return self.match_lines();
        }
        self.transcript_lines(width)
    }

    fn match_lines(&self) -> Vec<Line<'static>> {
        let theme = crate::theme::current();
        if self.matches.is_empty() {
            return vec![Line::from(Span::styled(
                "  no keybinding matches — press Enter to ask the assistant",
                Style::default().fg(theme.text_dim).italic(),
            ))];
        }
        const KEY_PAD: usize = 14;
        self.matches
            .iter()
            .map(|&idx| {
                let entry = &self.catalog[idx];
                let mut keys = if entry.keys_display.is_empty() {
                    "(unbound)".to_string()
                } else {
                    entry.keys_display.to_string()
                };
                if keys.chars().count() < KEY_PAD {
                    keys.push_str(&" ".repeat(KEY_PAD - keys.chars().count()));
                }
                Line::from(vec![
                    Span::styled(format!(" {keys}"), Style::default().fg(theme.accent).bold()),
                    Span::styled(
                        format!(" {}", entry.label),
                        Style::default().fg(theme.text_strong),
                    ),
                    Span::styled(
                        format!(" · {}", entry.section.title()),
                        Style::default().fg(theme.text_dim),
                    ),
                    Span::styled(
                        format!(" — {}", entry.describe),
                        Style::default().fg(theme.text_dim),
                    ),
                ])
            })
            .collect()
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let theme = crate::theme::current();
        let convo = self.convo();
        let mut out: Vec<Line<'static>> = Vec::new();
        if convo.turns.is_empty() && convo.notice.is_none() {
            out.push(Line::from(Span::styled(
                "  Type to search every keybinding (your remaps included).",
                Style::default().fg(theme.text_dim),
            )));
            out.push(Line::from(Span::styled(
                "  Press Enter to ask the help assistant in plain language,",
                Style::default().fg(theme.text_dim),
            )));
            out.push(Line::from(Span::styled(
                "  e.g. \"how do I multi-select activity rows?\"",
                Style::default().fg(theme.text_dim).italic(),
            )));
            return out;
        }
        for turn in &convo.turns {
            if !out.is_empty() {
                out.push(Line::default());
            }
            let q = Line::from(vec![
                Span::styled("❯ ", Style::default().fg(theme.accent).bold()),
                Span::styled(
                    turn.question.clone(),
                    Style::default().fg(theme.text_strong).bold(),
                ),
            ]);
            out.extend(comment_render::wrap_one(q, width));
            if turn.answer.is_empty() {
                if !turn.done {
                    out.push(Line::from(Span::styled(
                        "  thinking…",
                        Style::default().fg(theme.text_dim).italic(),
                    )));
                }
            } else {
                out.extend(comment_render::render_body(&turn.answer, width, usize::MAX));
                if !turn.done {
                    out.push(Line::from(Span::styled(
                        "▌",
                        Style::default().fg(theme.accent),
                    )));
                }
            }
        }
        if let Some(notice) = &convo.notice {
            out.push(Line::default());
            out.push(Line::from(Span::styled(
                format!("⚠ {notice}"),
                Style::default().fg(theme.error),
            )));
        }
        out
    }
}

impl Component for HelpAsk {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 100u16.min(area.width.saturating_sub(4));
        let modal_h = 30u16.min(area.height.saturating_sub(2));
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Span::styled(" Ask lazybox ", theme.modal_title()))
            .border_style(theme.modal_border());
        let inner = block.inner(modal);
        frame.render_widget(block, modal);
        if inner.height < 4 {
            return;
        }

        let input_rect = Rect { height: 1, ..inner };
        let div_rect = Rect {
            y: inner.y + 1,
            height: 1,
            ..inner
        };
        let body_rect = Rect {
            y: inner.y + 2,
            height: inner.height - 3,
            ..inner
        };
        let help_rect = Rect {
            y: inner.y + inner.height - 1,
            height: 1,
            ..inner
        };

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("› ", Style::default().fg(theme.accent).bold()),
                Span::styled(self.query.clone(), Style::default().fg(theme.text_strong)),
                Span::styled("▌", Style::default().fg(theme.accent)),
            ])),
            input_rect,
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(inner.width as usize),
                theme.divider(),
            ))),
            div_rect,
        );

        let lines = self.body_lines(body_rect.width);
        let total = lines.len();
        let visible = body_rect.height as usize;
        let searching = !self.query.trim().is_empty();
        // Matches render top-anchored; the transcript pins to the
        // bottom so a streaming answer stays in view.
        let offset = if searching {
            0
        } else {
            self.scroll_up = self.scroll_up.min(total.saturating_sub(visible));
            total.saturating_sub(visible + self.scroll_up)
        };
        frame.render_widget(Paragraph::new(lines).scroll((offset as u16, 0)), body_rect);

        let hint = if searching {
            vec![
                Span::styled("Enter", Style::default().fg(theme.success).bold()),
                Span::raw(" ask the assistant  "),
                Span::styled("Type", Style::default().fg(theme.accent).bold()),
                Span::raw(" filter keys  "),
                Span::styled("Esc", Style::default().fg(theme.error).bold()),
                Span::raw(" close"),
            ]
        } else {
            vec![
                Span::styled("Type", Style::default().fg(theme.accent).bold()),
                Span::raw(" search / ask  "),
                Span::styled("Enter", Style::default().fg(theme.success).bold()),
                Span::raw(" ask  "),
                Span::styled("↑↓", Style::default().fg(theme.accent).bold()),
                Span::raw(" scroll  "),
                Span::styled("Esc", Style::default().fg(theme.error).bold()),
                Span::raw(" close"),
            ]
        };
        frame.render_widget(Paragraph::new(Line::from(hint)), help_rect);
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

impl AppComponent<Msg, UserEvent> for HelpAsk {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        match ev {
            Event::Keyboard(key) => self.on_key(key),
            Event::Paste(text) => {
                self.query.push_str(text);
                self.refilter();
                None
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_tui_core::action::ActionDef;

    fn component() -> HelpAsk {
        let catalog =
            ActionDef::catalog(&["claude".to_string()], &std::collections::BTreeMap::new());
        HelpAsk::new(catalog, SharedHelpConvo::default())
    }

    fn ke(c: char) -> KeyEvent {
        KeyEvent::new(Key::Char(c), KeyModifiers::NONE)
    }
    fn key(code: Key) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Typing fuzzy-filters the catalog live — the offline layer that
    /// answers "where is X" without an agent (#302 phase 1).
    #[test]
    fn typing_filters_the_catalog() {
        let mut c = component();
        assert!(c.matches.is_empty());
        for ch in "merge".chars() {
            assert!(c.on_key(&ke(ch)).is_none(), "typing must not submit");
        }
        assert!(!c.matches.is_empty());
        assert_eq!(c.catalog[c.matches[0]].label, "merge PR");
        // Backspacing to empty returns to transcript mode.
        for _ in 0.."merge".len() {
            let _ = c.on_key(&key(Key::Backspace));
        }
        assert!(c.matches.is_empty());
    }

    /// Enter submits the typed question and clears the input so the
    /// streamed answer (transcript mode) is immediately visible.
    #[test]
    fn enter_submits_question_and_clears_query() {
        let mut c = component();
        for ch in "how do I snooze?".chars() {
            let _ = c.on_key(&ke(ch));
        }
        match c.on_key(&key(Key::Enter)) {
            Some(Msg::HelpAsked(q)) => assert_eq!(q, "how do I snooze?"),
            other => panic!("expected HelpAsked, got {other:?}"),
        }
        assert!(c.query.is_empty());
    }

    /// Enter with an empty (or whitespace) query is a no-op — nothing
    /// to ask.
    #[test]
    fn enter_on_empty_query_is_noop() {
        let mut c = component();
        assert!(c.on_key(&key(Key::Enter)).is_none());
        let _ = c.on_key(&ke(' '));
        assert!(c.on_key(&key(Key::Enter)).is_none());
    }

    #[test]
    fn esc_and_ctrl_c_dismiss() {
        let mut c = component();
        assert!(matches!(
            c.on_key(&key(Key::Esc)),
            Some(Msg::ModalDismissed)
        ));
        let ev = KeyEvent::new(Key::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(c.on_key(&ev), Some(Msg::ModalDismissed)));
    }

    /// The transcript renders the shared conversation: question line,
    /// streamed markdown answer, and the streaming cursor while a turn
    /// is open — then drops the cursor once the turn completes.
    #[test]
    fn transcript_renders_shared_convo() {
        let c = component();
        {
            let mut convo = c.convo();
            convo.turns.push(HelpTurn {
                question: "how do I snooze?".into(),
                answer: "Press `z` on a **workspace**.".into(),
                done: false,
            });
        }
        let text = |lines: &[Line<'_>]| {
            lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        let rendered = text(&c.body_lines(80));
        assert!(rendered.contains("❯ how do I snooze?"));
        // Markdown syntax (backticks, `**`) is consumed into styling
        // by `comment_render`; the plain text remains.
        assert!(rendered.contains("Press z on a workspace."));
        assert!(rendered.contains("▌"), "streaming cursor missing");

        c.convo().turns[0].done = true;
        c.convo().notice = Some("assistant exited".into());
        let rendered = text(&c.body_lines(80));
        assert!(!rendered.contains("▌"));
        assert!(rendered.contains("⚠ assistant exited"));
    }

    /// Full-frame render smoke over a `TestBackend`: title, input
    /// echo, and match rows all land in the buffer.
    #[test]
    fn render_smoke_via_test_backend() {
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;
        let mut c = component();
        c.convo().turns.push(HelpTurn {
            question: "q".into(),
            answer: "a".into(),
            done: true,
        });
        for ch in "merge".chars() {
            let _ = c.on_key(&ke(ch));
        }
        let mut term = Terminal::new(TestBackend::new(110, 34)).unwrap();
        term.draw(|f| {
            let area = f.area();
            c.view(f, area);
        })
        .unwrap();
        let rendered = format!("{:?}", term.backend().buffer());
        assert!(rendered.contains("Ask lazybox"));
        assert!(rendered.contains("merge"));
    }
}
