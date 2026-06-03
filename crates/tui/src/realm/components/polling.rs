//! `Polling` — first-poll progress modal. tuirealm port of
//! `crate::components::polling_modal::PollingModal`.
//!
//! Shows after setup completes while lazybox kicks off the first poll
//! cycle. Dismisses on first `WorkspaceUpserted`, or after a timeout
//! so the user is never stuck staring at it.
//!
//! Subscribes to:
//! - `Event::Tick` for the spinner animation + timeout check.
//! - `Event::User(UserEvent::Daemon(...))` for `WorkspaceUpserted` /
//!   `PollProgress` / `PollCompleted` / `ProviderError`.

use crate::realm::{Msg, UserEvent};
use lazybox_ipc::Event as IpcEvent;
use std::time::{Duration, Instant};
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, KeyEvent};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use tuirealm::state::State;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How long to wait before giving up and dismissing.
const TIMEOUT: Duration = Duration::from_secs(15);

/// Polling-progress modal.
pub struct Polling {
    sources: Vec<String>,
    spinner_idx: usize,
    started_at: Instant,
    saw_workspace: bool,
    polls_completed: std::collections::BTreeSet<String>,
    last_progress: Option<(String, String)>,
    queries_seen: Vec<String>,
    /// Set when an error arrives — we surface it as `Msg::PollingError`
    /// so the caller can push an `ErrorModal`.
    error: Option<(String, String, String, String)>,
}

impl Polling {
    /// Construct with the source ids that will poll.
    pub fn new(sources: Vec<String>) -> Self {
        Self {
            sources,
            spinner_idx: 0,
            started_at: Instant::now(),
            saw_workspace: false,
            polls_completed: Default::default(),
            last_progress: None,
            queries_seen: Vec::new(),
            error: None,
        }
    }

    fn message(&self) -> String {
        if self.sources.is_empty() {
            "Polling for tasks…".to_string()
        } else {
            format!("Pulling tasks from {}…", self.sources.join(", "))
        }
    }

    /// Whether every enabled source has reported a complete poll.
    fn all_polls_done(&self) -> bool {
        !self.sources.is_empty()
            && self
                .sources
                .iter()
                .all(|s| self.polls_completed.contains(s))
    }

    /// Direct entry point for the orchestrator: feed one daemon event
    /// into the polling state. Mirrors the `Event::User` arm in the
    /// `AppComponent` impl so the Model can drive Polling without
    /// mounting it via tuirealm.
    pub fn feed_daemon_event(&mut self, evt: &IpcEvent) {
        match evt {
            IpcEvent::WorkspaceUpserted(_) => {
                self.saw_workspace = true;
            }
            IpcEvent::PollCompleted { source, .. } => {
                self.polls_completed.insert(source.clone());
            }
            IpcEvent::PollProgress { source, message } => {
                self.last_progress = Some((source.clone(), message.clone()));
                if message.starts_with("PR query:") || message.starts_with("Issue query:") {
                    self.queries_seen.push(format!("[{source}] {message}"));
                }
            }
            IpcEvent::ProviderError {
                source,
                message,
                detail,
                kind,
            } => {
                // Retryable errors self-heal next cycle. Surface them
                // inline as a progress hiccup rather than escalating
                // to an ErrorModal that interrupts the user every
                // poll cycle.
                if kind == "retryable" {
                    self.last_progress = Some((source.clone(), message.clone()));
                    return;
                }
                if self.error.is_none() {
                    self.error = Some((
                        source.clone(),
                        kind.clone(),
                        detail.clone(),
                        message.clone(),
                    ));
                }
            }
            _ => {}
        }
    }

    /// Direct tick — advances the spinner + checks termination
    /// conditions. Returns `Some(msg)` if Polling wants to be
    /// dismissed (saw workspace / timed out / hit empty / error).
    pub fn tick_direct(&mut self) -> Option<Msg> {
        self.spinner_idx = self.spinner_idx.wrapping_add(1);
        if self.saw_workspace {
            return Some(Msg::ModalDismissed);
        }
        if let Some(err) = self.error.take() {
            return Some(Msg::PollingError(err));
        }
        if self.all_polls_done() {
            return Some(Msg::PollingEmptyInbox(self.queries_seen.clone()));
        }
        if self.started_at.elapsed() > TIMEOUT {
            return Some(Msg::PollingTimeout);
        }
        None
    }

    /// Direct render — orchestrator-friendly entry point that
    /// bypasses tuirealm's mount dance. Kept for legacy modal-style
    /// rendering; the realm `Model` uses `spinner_glyph` +
    /// `status_label` to render Polling in the footer instead.
    pub fn view_direct(&mut self, frame: &mut Frame, area: Rect) {
        self.view(frame, area);
    }

    /// Current spinner frame as a static glyph. Driven by `tick_direct`.
    pub fn spinner_glyph(&self) -> &'static str {
        SPINNER_FRAMES[self.spinner_idx % SPINNER_FRAMES.len()]
    }

    /// Footer-friendly status label: source list + most recent
    /// progress message, joined into a single line.
    pub fn status_label(&self) -> String {
        match (&self.last_progress, self.sources.is_empty()) {
            (Some((source, msg)), _) => {
                format!("Pulling from {source} · {msg}")
            }
            (None, false) => format!("Pulling tasks from {}…", self.sources.join(", ")),
            (None, true) => "Polling…".to_string(),
        }
    }
}

impl Component for Polling {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 100u16.min(area.width.saturating_sub(4));
        let modal_h = 14u16.min(area.height.saturating_sub(2));
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme.modal_border());
        let inner = block.inner(modal);
        frame.render_widget(block, modal);

        let spinner = SPINNER_FRAMES[self.spinner_idx % SPINNER_FRAMES.len()];
        let mut lines = vec![
            Line::raw(""),
            Line::from(vec![
                Span::styled(
                    format!("  {spinner}  "),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(self.message(), Style::default().fg(theme.text_strong)),
            ]),
            Line::raw(""),
        ];
        if let Some((source, msg)) = &self.last_progress {
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(
                    format!("{source}  ·  "),
                    Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
                ),
                Span::styled(msg.clone(), Style::default().fg(theme.text_dim)),
            ]));
            lines.push(Line::raw(""));
        }
        let completed_chunks: Vec<Span> = self
            .sources
            .iter()
            .map(|s| {
                let done = self.polls_completed.contains(s);
                let glyph = if done { "● " } else { "○ " };
                let style = if done {
                    Style::default().fg(theme.success)
                } else {
                    Style::default().fg(theme.chrome)
                };
                Span::styled(format!("{glyph}{s}  "), style)
            })
            .collect();
        if !completed_chunks.is_empty() {
            let mut row = vec![Span::raw("  ")];
            row.extend(completed_chunks);
            lines.push(Line::from(row));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "  Press any key to dismiss",
            theme.hint(),
        )));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
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

impl AppComponent<Msg, UserEvent> for Polling {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        match ev {
            // Any key dismisses.
            Event::Keyboard(KeyEvent { .. }) => Some(Msg::ModalDismissed),
            // Spinner + timeout drive on Tick.
            Event::Tick => {
                self.spinner_idx = self.spinner_idx.wrapping_add(1);
                if self.saw_workspace {
                    return Some(Msg::ModalDismissed);
                }
                if let Some(err) = self.error.take() {
                    return Some(Msg::PollingError(err));
                }
                if self.all_polls_done() {
                    return Some(Msg::PollingEmptyInbox(self.queries_seen.clone()));
                }
                if self.started_at.elapsed() > TIMEOUT {
                    return Some(Msg::PollingTimeout);
                }
                None
            }
            // Daemon events drive the inner state.
            Event::User(UserEvent::Daemon(evt)) => {
                match evt.as_ref() {
                    IpcEvent::WorkspaceUpserted(_) => {
                        self.saw_workspace = true;
                    }
                    IpcEvent::PollCompleted { source, .. } => {
                        self.polls_completed.insert(source.clone());
                    }
                    IpcEvent::PollProgress { source, message } => {
                        self.last_progress = Some((source.clone(), message.clone()));
                        if message.starts_with("PR query:") || message.starts_with("Issue query:") {
                            self.queries_seen.push(format!("[{source}] {message}"));
                        }
                    }
                    IpcEvent::ProviderError {
                        source,
                        message,
                        detail,
                        kind,
                    } => {
                        if kind == "retryable" {
                            self.last_progress = Some((source.clone(), message.clone()));
                        } else if self.error.is_none() {
                            self.error = Some((
                                source.clone(),
                                kind.clone(),
                                detail.clone(),
                                message.clone(),
                            ));
                        }
                    }
                    _ => {}
                }
                None
            }
            _ => None,
        }
    }
}
