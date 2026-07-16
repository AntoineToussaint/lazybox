//! `HelpAsk` — ask lazybox how to use lazybox (#302).
//!
//! Opened directly by pressing `?`. One modal, two layers:
//!
//! - **Typing** fuzzy-searches the runtime action catalog live
//!   (`lazybox_tui_core::help::search`) — the offline layer that
//!   answers most "where is X" queries instantly, with the user's
//!   effective (post-override) keys.
//! - **Enter** escalates the typed text as a question to a headless
//!   structured agent run (`Command::StartAgentRun` — Claude first,
//!   Codex fallback; no PTY or worktree) whose first message is the
//!   generated catalog + docs context. The streamed markdown answer
//!   renders here via `comment_render`.
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
use lazybox_tui_core::action::{ActionKind, CatalogEntry, Section};
use std::borrow::Cow;
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
    /// The turn awaiting its answer: the *earliest* un-done turn. The
    /// run answers questions in submission order, so deltas and results
    /// always belong to the oldest open turn — keyed on the last turn
    /// instead, a follow-up asked mid-stream would hijack the previous
    /// answer's tail and orphan its own.
    pub fn streaming_turn_mut(&mut self) -> Option<&mut HelpTurn> {
        self.turns.iter_mut().find(|t| !t.done)
    }

    /// Close every open turn — the run is gone (exit or spawn failure),
    /// so no answer can arrive for any of them. Returns `true` when a
    /// closed turn had no answer yet.
    pub fn close_open_turns(&mut self) -> bool {
        let mut unanswered = false;
        for turn in self.turns.iter_mut().filter(|t| !t.done) {
            turn.done = true;
            unanswered |= turn.answer.is_empty();
        }
        unanswered
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
    pub fn new(
        mut catalog: Vec<CatalogEntry>,
        convo: SharedHelpConvo,
        terminal_escape_char: char,
    ) -> Self {
        let leader = format!("{0}{0}", terminal_escape_char);
        // LeaveTerminal is dispatched by the configurable terminal latch, not
        // by its catalog chord. Make fuzzy-search results reflect the live key.
        if let Some(exit) = catalog
            .iter_mut()
            .find(|entry| entry.kind == ActionKind::LeaveTerminal)
        {
            exit.keys_display = Cow::Owned(format!("{leader}q"));
        }
        catalog.extend(terminal_search_entries(&leader));
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
        // The assistant is the primary `?` surface. Pressing `?` again
        // from its empty prompt swaps to the compact all-shortcuts index;
        // typing inside a non-empty question still accepts punctuation.
        if self.query.is_empty() && matches!(key.code, Key::Char('?')) {
            return Some(Msg::HelpIndexOpen);
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
                "  Ask about workflows, shortcuts, agents, providers—anything in lazybox.",
                Style::default().fg(theme.text_strong).bold(),
            )));
            out.push(Line::from(Span::styled(
                "  Typing searches your live keymap instantly; Enter asks in plain language.",
                Style::default().fg(theme.text_dim),
            )));
            out.push(Line::default());
            out.push(Line::from(Span::styled(
                "  Try “how do I send one prompt to several workspaces?”",
                Style::default().fg(theme.text_dim).italic(),
            )));
            out.push(Line::from(Span::styled(
                "      “what can I do while a terminal is focused?”",
                Style::default().fg(theme.text_dim).italic(),
            )));
            out.push(Line::from(Span::styled(
                "      “how do I act on a failing PR?”",
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

/// Search-only rows for commands owned by the terminal escape latch rather
/// than the Action catalog. They make “split terminal”, “jump agent”, etc.
/// resolve instantly in Ask Lazybox while the live `]]` popup remains the
/// source of truth for dispatch.
fn terminal_search_entries(leader: &str) -> Vec<CatalogEntry> {
    let row = |kind, suffix: &str, label, describe, config_key: &str| CatalogEntry {
        kind,
        param: None,
        section: Section::Terminal,
        label: Cow::Borrowed(label),
        describe,
        chords: Vec::new(),
        keys_display: Cow::Owned(format!("{leader}{suffix}")),
        config_key: config_key.to_string(),
    };
    vec![
        row(
            ActionKind::OpenSnippets,
            "s<key>",
            "send snippet",
            "Open the terminal snippet picker and send the chosen saved prompt.",
            "terminal.snippets",
        ),
        row(
            ActionKind::ToggleFocusMode,
            "f",
            "focus mode",
            "Toggle the focused terminal between the tiled layout and near-fullscreen focus mode.",
            "terminal.focus_mode",
        ),
        row(
            ActionKind::JumpToWorkspace,
            "`",
            "jump to workspace",
            "Open the fuzzy workspace picker without leaving the focused terminal.",
            "terminal.jump_workspace",
        ),
        row(
            ActionKind::JumpToWorkspace,
            "1…9",
            "jump to agent",
            "Jump to an agent workspace by its position in the sidebar.",
            "terminal.jump_agent",
        ),
        row(
            ActionKind::ResizeSplitter,
            "|",
            "split right",
            "Split the focused terminal tile side-by-side.",
            "terminal.split_right",
        ),
        row(
            ActionKind::ResizeSplitter,
            "-",
            "split down",
            "Split the focused terminal tile vertically.",
            "terminal.split_down",
        ),
        row(
            ActionKind::CyclePane,
            "<arrow>",
            "move terminal tile",
            "Move tile focus in a split layout or switch tabs in a tabbed layout.",
            "terminal.move_tile",
        ),
        row(
            ActionKind::LeaveTerminal,
            "x",
            "close terminal",
            "Close the focused terminal tile or active tab.",
            "terminal.close",
        ),
    ]
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
            .title(Span::styled(" Ask Lazybox ", theme.modal_title()))
            .border_style(theme.modal_border());
        let inner = block.inner(modal);
        frame.render_widget(block, modal);
        if inner.height < 4 {
            return;
        }

        let input_line = Line::from(vec![
            Span::styled("› ", Style::default().fg(theme.accent).bold()),
            Span::styled(self.query.clone(), Style::default().fg(theme.text_strong)),
            Span::styled("▌", Style::default().fg(theme.accent)),
        ]);
        let input_lines = comment_render::wrap_one(input_line, inner.width);
        // Grow the input to fit the wrapped query, capped so the divider,
        // transcript, and footer keep room; past the cap the input scrolls
        // to keep the cursor on the last row.
        const MAX_INPUT_ROWS: u16 = 4;
        let input_h =
            (input_lines.len() as u16).clamp(1, MAX_INPUT_ROWS.min(inner.height.saturating_sub(3)));
        let input_rect = Rect {
            height: input_h,
            ..inner
        };
        let div_rect = Rect {
            y: inner.y + input_h,
            height: 1,
            ..inner
        };
        let body_rect = Rect {
            y: inner.y + input_h + 1,
            height: inner.height - input_h - 2,
            ..inner
        };
        let help_rect = Rect {
            y: inner.y + inner.height - 1,
            height: 1,
            ..inner
        };

        let input_scroll = (input_lines.len() as u16).saturating_sub(input_h);
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
                Span::raw(" close  "),
                Span::styled("?", Style::default().fg(theme.accent).bold()),
                Span::raw(" shortcuts (clear query first)"),
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
                Span::raw(" close  "),
                Span::styled("?", Style::default().fg(theme.accent).bold()),
                Span::raw(" all shortcuts"),
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
        HelpAsk::new(catalog, SharedHelpConvo::default(), ']')
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

    #[test]
    fn question_mark_at_empty_prompt_opens_shortcut_index() {
        let mut c = component();
        assert!(matches!(c.on_key(&ke('?')), Some(Msg::HelpIndexOpen)));

        // Once a question is in progress, punctuation remains ordinary input.
        let _ = c.on_key(&ke('w'));
        assert!(c.on_key(&ke('?')).is_none());
        assert_eq!(c.query, "w?");
    }

    #[test]
    fn terminal_commands_are_searchable_with_the_live_escape_char() {
        let catalog =
            ActionDef::catalog(&["claude".to_string()], &std::collections::BTreeMap::new());
        let mut c = HelpAsk::new(catalog, SharedHelpConvo::default(), '}');

        for ch in "split right".chars() {
            let _ = c.on_key(&ke(ch));
        }
        let split = c
            .match_lines()
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(split.contains("}}|"), "live split chord missing: {split}");
        assert!(
            split.contains("split right"),
            "terminal action missing: {split}"
        );

        let exit = c
            .catalog
            .iter()
            .find(|entry| entry.label == "exit to sidebar")
            .expect("exit row");
        assert_eq!(exit.keys_display, "}}q");
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

    /// Read each row of a rendered `TestBackend` buffer as a trimmed
    /// string.
    fn buffer_rows(backend: &tuirealm::ratatui::backend::TestBackend) -> Vec<String> {
        let buf = backend.buffer();
        let area = buf.area;
        (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    /// A query wider than the modal wraps onto further input rows, and
    /// the divider drops below the grown input instead of overwriting it.
    #[test]
    fn long_query_wraps_input_and_pushes_divider_down() {
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;
        let mut c = component();
        // Distinct words so each wrapped row is identifiable, long
        // enough to exceed the ~54-col inner width and span >1 row.
        for ch in
            "alpha bravo charlie delta echo foxtrot golf hotel india juliett kilo lima".chars()
        {
            let _ = c.on_key(&ke(ch));
        }
        let mut term = Terminal::new(TestBackend::new(60, 34)).unwrap();
        term.draw(|f| c.view(f, f.area())).unwrap();
        let rows = buffer_rows(term.backend());

        let prompt_row = rows.iter().position(|r| r.contains('›')).expect("prompt");
        // The query wrapped: text lands on the prompt row and the row
        // below it, before any divider.
        assert!(rows[prompt_row].contains("alpha"), "first word on row 1");
        assert!(
            rows[prompt_row + 1].contains("lima") || rows[prompt_row + 1].contains("kilo"),
            "wrapped tail on row 2: {:?}",
            rows[prompt_row + 1]
        );
        // The divider sits below the grown input, not on the second
        // input row.
        let div_row = rows
            .iter()
            .skip(prompt_row + 1)
            .position(|r| r.contains("──────"))
            .map(|i| i + prompt_row + 1)
            .expect("divider");
        assert!(
            div_row > prompt_row + 1,
            "divider must clear the wrapped input (prompt={prompt_row}, div={div_row})"
        );
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
        assert!(rendered.contains("Ask Lazybox"));
        assert!(rendered.contains("merge"));
    }
}
