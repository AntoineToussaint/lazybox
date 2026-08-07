//! `PrChat` — "Ask about this PR" (#945).
//!
//! Reached with `a` from the description reader modal (#448). It is the
//! same streamed-answer chat as Ask Lazybox (`help_ask`), scoped to the
//! focused PR/issue instead of to lazybox itself: it reuses
//! [`HelpConvo`]/[`HelpTurn`] for the transcript and rides the same
//! headless `Command::StartAgentRun` (StreamJson, read-only). The
//! difference is the context document the `Model` feeds the run — PR
//! metadata + activity + the local diff (`lazybox_tui_core::pr_chat`) —
//! and the sentinel session key.
//!
//! The conversation state is a `SharedHelpConvo` owned by the `Model`
//! and streamed into by daemon-event handlers while this component is
//! mounted, so a delta doesn't remount the modal (which would drop the
//! user's in-flight typing).

use crate::components::comment_render;
use crate::realm::components::help_ask::SharedHelpConvo;
use crate::realm::{HelpQuestionKind, Msg, UserEvent};
use std::sync::MutexGuard;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyEvent, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use tuirealm::state::State;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub(crate) struct PrChat {
    convo: SharedHelpConvo,
    /// One-line subject shown in the modal title bar (the PR/issue).
    subject: String,
    /// Short note on what context grounds the answers (metadata / diff /
    /// no worktree), shown above the transcript so the user knows what
    /// the assistant can and can't see.
    grounding: String,
    query: String,
    /// Transcript scroll, in lines up from the bottom. 0 = pinned to the
    /// bottom so a streaming answer auto-follows.
    scroll_up: usize,
    spinner_idx: usize,
}

impl PrChat {
    pub(crate) fn new(
        convo: SharedHelpConvo,
        subject: impl Into<String>,
        grounding: impl Into<String>,
    ) -> Self {
        Self {
            convo,
            subject: subject.into(),
            grounding: grounding.into(),
            query: String::new(),
            scroll_up: 0,
            spinner_idx: 0,
        }
    }

    fn convo(&self) -> MutexGuard<'_, super::help_ask::HelpConvo> {
        match self.convo.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn on_key(&mut self, key: &KeyEvent) -> Option<Msg> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if matches!(key.code, Key::Esc) || (ctrl && matches!(key.code, Key::Char('c'))) {
            return Some(Msg::ModalDismissed);
        }
        match key.code {
            Key::Tab => {
                let mut convo = self.convo();
                let next = convo.next_question().toggled();
                convo.select_next_question(next);
                None
            }
            Key::Enter => {
                let question = self.query.trim().to_string();
                if question.is_empty() {
                    return None;
                }
                self.query.clear();
                self.scroll_up = 0;
                let kind = self.convo().next_question();
                Some(Msg::PrChatAsked(question, kind))
            }
            Key::Backspace => {
                self.query.pop();
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
                None
            }
            _ => None,
        }
    }

    fn transcript_lines(&self, width: u16) -> Vec<Line<'static>> {
        let theme = crate::theme::current();
        let convo = self.convo();
        let mut out: Vec<Line<'static>> = Vec::new();
        out.push(Line::from(Span::styled(
            format!("  {}", self.grounding),
            Style::default().fg(theme.text_dim).italic(),
        )));
        if convo.turns.is_empty() && convo.notice.is_none() {
            out.push(Line::default());
            out.push(Line::from(Span::styled(
                "  Ask what the change does, why this approach, whether an edge case is handled…",
                Style::default().fg(theme.text_strong).bold(),
            )));
            out.push(Line::from(Span::styled(
                "  Answers cite file:line and comments when grounded in the diff or activity.",
                Style::default().fg(theme.text_dim),
            )));
            return out;
        }
        for turn in &convo.turns {
            out.push(Line::default());
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
                        format!(
                            "  {} thinking…",
                            SPINNER_FRAMES[self.spinner_idx % SPINNER_FRAMES.len()]
                        ),
                        Style::default().fg(theme.accent).italic(),
                    )));
                }
            } else {
                out.extend(comment_render::render_body(&turn.answer, width, usize::MAX));
                if !turn.done {
                    out.push(Line::from(Span::styled(
                        format!(
                            "{} answering…",
                            SPINNER_FRAMES[self.spinner_idx % SPINNER_FRAMES.len()]
                        ),
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

impl Component for PrChat {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 100u16.min(area.width.saturating_sub(4));
        let modal_h = 30u16.min(area.height.saturating_sub(2));
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let title = crate::util::truncate_ellipsis(
            &format!(" Ask about {} ", self.subject),
            modal_w.saturating_sub(4) as usize,
        )
        .into_owned();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Span::styled(title, theme.modal_title()))
            .border_style(theme.modal_border());
        let inner = block.inner(modal);
        frame.render_widget(block, modal);
        if inner.height < 5 {
            return;
        }

        let (next_question, follow_up_available) = {
            let convo = self.convo();
            (convo.next_question(), convo.follow_up_available())
        };
        let mode_style = |kind| {
            if next_question == kind {
                Style::default().fg(Color::Black).bg(theme.accent).bold()
            } else if kind == HelpQuestionKind::FollowUp && !follow_up_available {
                Style::default().fg(theme.text_dim).dim()
            } else {
                Style::default().fg(theme.text_dim)
            }
        };
        let mode_line = Line::from(vec![
            Span::raw(" "),
            Span::styled(
                " Follow-up · keeps context ",
                mode_style(HelpQuestionKind::FollowUp),
            ),
            Span::raw("  "),
            Span::styled(
                " New question · fresh thread ",
                mode_style(HelpQuestionKind::NewQuestion),
            ),
            Span::styled("  Tab switch", Style::default().fg(theme.text_dim)),
        ]);
        let mode_rect = Rect { height: 1, ..inner };
        frame.render_widget(Paragraph::new(mode_line), mode_rect);

        let input_line = Line::from(vec![
            Span::styled(
                format!("{} › ", next_question.input_label()),
                Style::default().fg(theme.accent).bold(),
            ),
            Span::styled(self.query.clone(), Style::default().fg(theme.text_strong)),
            Span::styled("▌", Style::default().fg(theme.accent)),
        ]);
        let input_lines = comment_render::wrap_one(input_line, inner.width);
        const MAX_INPUT_ROWS: u16 = 4;
        let wrapped = input_lines.len();
        let input_h = wrapped.clamp(
            1,
            MAX_INPUT_ROWS.min(inner.height.saturating_sub(4)) as usize,
        ) as u16;
        let input_rect = Rect {
            y: inner.y + 1,
            height: input_h,
            ..inner
        };
        let div_rect = Rect {
            y: inner.y + input_h + 1,
            height: 1,
            ..inner
        };
        let body_rect = Rect {
            y: inner.y + input_h + 2,
            height: inner.height - input_h - 3,
            ..inner
        };
        let help_rect = Rect {
            y: inner.y + inner.height - 1,
            height: 1,
            ..inner
        };

        let input_scroll = wrapped.saturating_sub(input_h as usize) as u16;
        frame.render_widget(
            Paragraph::new(input_lines).scroll((input_scroll, 0)),
            input_rect,
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(inner.width as usize),
                theme.divider(),
            ))),
            div_rect,
        );

        let lines = self.transcript_lines(body_rect.width);
        let total = lines.len();
        let visible = body_rect.height as usize;
        self.scroll_up = self.scroll_up.min(total.saturating_sub(visible));
        let offset = total.saturating_sub(visible + self.scroll_up);
        frame.render_widget(Paragraph::new(lines).scroll((offset as u16, 0)), body_rect);

        let hint = vec![
            Span::styled("Type", Style::default().fg(theme.accent).bold()),
            Span::raw(" ask  "),
            Span::styled("Enter", Style::default().fg(theme.success).bold()),
            Span::raw(" send  "),
            Span::styled("Tab", Style::default().fg(theme.accent).bold()),
            Span::raw(" mode  "),
            Span::styled("↑↓", Style::default().fg(theme.accent).bold()),
            Span::raw(" scroll  "),
            Span::styled("Esc", Style::default().fg(theme.error).bold()),
            Span::raw(" close"),
        ];
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

impl AppComponent<Msg, UserEvent> for PrChat {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        match ev {
            Event::Keyboard(key) => self.on_key(key),
            Event::Paste(text) => {
                self.query.push_str(text);
                None
            }
            Event::Tick if self.convo().turns.iter().any(|turn| !turn.done) => {
                self.spinner_idx = self.spinner_idx.wrapping_add(1);
                Some(Msg::PrChatSpinnerTick)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realm::components::help_ask::HelpTurn;

    fn component() -> PrChat {
        PrChat::new(
            SharedHelpConvo::default(),
            "owner/repo#12",
            "Grounded in PR metadata + activity + local diff.",
        )
    }

    fn ke(c: char) -> KeyEvent {
        KeyEvent::new(Key::Char(c), KeyModifiers::NONE)
    }
    fn key(code: Key) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn text(lines: &[Line<'_>]) -> String {
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
    }

    #[test]
    fn enter_submits_question_and_clears_query() {
        let mut c = component();
        for ch in "what changed?".chars() {
            assert!(c.on_key(&ke(ch)).is_none());
        }
        match c.on_key(&key(Key::Enter)) {
            Some(Msg::PrChatAsked(q, kind)) => {
                assert_eq!(q, "what changed?");
                assert_eq!(kind, HelpQuestionKind::NewQuestion);
            }
            other => panic!("expected PrChatAsked, got {other:?}"),
        }
        assert!(c.query.is_empty());
    }

    #[test]
    fn enter_on_empty_query_is_noop() {
        let mut c = component();
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

    /// The transcript renders the shared conversation: the grounding
    /// note, the question line, the streamed markdown answer, and the
    /// working indicator while a turn is open.
    #[test]
    fn transcript_renders_shared_convo() {
        let c = component();
        c.convo().turns.push(HelpTurn {
            question: "why this approach?".into(),
            answer: "It reuses `poll()` — see `src/poll.rs:2`.".into(),
            done: false,
        });
        let rendered = text(&c.transcript_lines(80));
        assert!(rendered.contains("Grounded in PR metadata"));
        assert!(rendered.contains("❯ why this approach?"));
        assert!(rendered.contains("src/poll.rs:2"));
        assert!(rendered.contains("⠋ answering…"));

        c.convo().turns[0].done = true;
        let rendered = text(&c.transcript_lines(80));
        assert!(!rendered.contains("answering…"));
    }

    #[test]
    fn tick_animates_spinner_while_answering() {
        let mut c = component();
        c.convo().turns.push(HelpTurn {
            question: "q".into(),
            ..Default::default()
        });
        assert!(text(&c.transcript_lines(80)).contains("⠋ thinking…"));
        assert!(matches!(c.on(&Event::Tick), Some(Msg::PrChatSpinnerTick)));
        assert!(text(&c.transcript_lines(80)).contains("⠙ thinking…"));
    }

    #[test]
    fn render_smoke_via_test_backend() {
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;
        let mut c = component();
        c.convo().turns.push(HelpTurn {
            question: "what changed?".into(),
            answer: "The poller now retries.".into(),
            done: true,
        });
        let mut term = Terminal::new(TestBackend::new(110, 34)).unwrap();
        term.draw(|f| {
            let area = f.area();
            c.view(f, area);
        })
        .unwrap();
        let rendered = format!("{:?}", term.backend().buffer());
        assert!(rendered.contains("Ask about"));
        assert!(rendered.contains("owner/repo#12"));
        assert!(rendered.contains("Follow-up"));
    }
}
