//! `Loading<T>` — spinner + label while a background task resolves.
//! tuirealm port of `tui_kit::widgets::LoadingModal`.
//!
//! Channel-driven so the kit stays runtime-agnostic. Use
//! `Loading::pending(label)` to get back `(component, LoadingResult)`;
//! the caller spawns the work and `LoadingResult::send(value)` the
//! moment it completes. The component polls its receiver each tick
//! and emits `Msg::LoadingResolved(...)` when the value lands.
//!
//! ## On the type erasure
//!
//! `Msg` doesn't have a generic — it's an enum stored inside
//! `Application`. So this component fixes `T` to a single dynamic
//! payload type. We use `Box<dyn Any + Send>` and let the
//! `Model::update` arm downcast to whatever the calling flow expects.
//! That's the same pattern lazybox uses for `LoadingModal<T>` in tui-kit.

use crate::realm::Msg;
use crate::realm::UserEvent;
use std::any::Any;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyEvent, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use tuirealm::state::State;

/// Type-erased payload — flows downcast to their expected concrete
/// type after receiving `Msg::LoadingResolved`.
pub type LoadingPayload = Box<dyn Any + Send>;

/// Producer side. `send(value)` → modal resolves → `Msg::LoadingResolved`.
pub struct LoadingResult {
    tx: Option<SyncSender<LoadingPayload>>,
}

impl LoadingResult {
    /// Deliver. Returns `Err(value)` when the modal was cancelled.
    pub fn send<T: Send + 'static>(mut self, value: T) -> Result<(), T> {
        let Some(tx) = self.tx.take() else {
            return Err(value);
        };
        match tx.try_send(Box::new(value)) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_v)) | Err(TrySendError::Disconnected(_v)) => {
                // We boxed `value` already; we can't recover the
                // original `T` from the box without an unsafe downcast
                // because TrySendError gives us the box back. For
                // simplicity, the cancelled branch reports a generic
                // error rather than the typed value — flows that need
                // recovery can wrap their value in a local Option
                // before calling `send`.
                Err(unsafe {
                    // Reconstruct: we know the box held `T`. The
                    // alternative is exposing `Result<(), Box<dyn Any>>`
                    // which is uglier at every call site.
                    let raw = Box::into_raw(_v) as *mut T;
                    *Box::from_raw(raw)
                })
            }
        }
    }
}

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Spinner modal. `pending(label)` builds it; the producer hand
/// resolves via [`LoadingResult::send`].
pub struct Loading {
    title: String,
    label: String,
    spinner_idx: usize,
    rx: Option<Receiver<LoadingPayload>>,
}

impl Loading {
    /// Build a loading modal + the producer handle.
    pub fn pending(label: impl Into<String>) -> (Self, LoadingResult) {
        let (tx, rx) = sync_channel::<LoadingPayload>(1);
        let modal = Self {
            title: "Loading".to_string(),
            label: label.into(),
            spinner_idx: 0,
            rx: Some(rx),
        };
        (modal, LoadingResult { tx: Some(tx) })
    }

    /// Override the title.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    fn take_result(&mut self) -> TakeOutcome {
        let Some(rx) = self.rx.as_ref() else {
            return TakeOutcome::Done;
        };
        match rx.try_recv() {
            Ok(v) => {
                self.rx = None;
                TakeOutcome::Got(v)
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => TakeOutcome::Pending,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                // Producer task dropped its sender without sending —
                // most commonly because it panicked. Without this
                // branch the modal stayed up forever and the user
                // had to press Esc. Treat as cancel so the caller
                // (setup runner / partial flow) gets a chance to
                // back out gracefully.
                self.rx = None;
                TakeOutcome::Cancelled
            }
        }
    }
}

enum TakeOutcome {
    Got(LoadingPayload),
    /// No data yet; channel still open. Caller keeps ticking.
    Pending,
    /// Producer dropped its sender without delivering. Modal should
    /// dismiss so the user isn't stuck staring at a spinner.
    Cancelled,
    /// Already-resolved on a prior tick; nothing to do.
    Done,
}

impl Component for Loading {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 60u16.min(area.width.saturating_sub(4));
        let modal_h = 5u16;
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Span::styled(
                format!(" {} ", self.title),
                theme.modal_title(),
            ))
            .border_style(theme.modal_border());
        let inner = block.inner(modal);
        frame.render_widget(block, modal);

        let spinner = SPINNER_FRAMES[self.spinner_idx % SPINNER_FRAMES.len()];
        let lines = vec![
            Line::raw(""),
            Line::from(vec![
                Span::styled(
                    format!("{spinner}  "),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(self.label.clone(), Style::default().fg(theme.text_strong)),
            ]),
            Line::raw(""),
            Line::from(Span::styled("Esc cancel", theme.hint())),
        ];
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

impl AppComponent<Msg, UserEvent> for Loading {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        match ev {
            Event::Keyboard(KeyEvent { code: Key::Esc, .. }) => {
                self.rx = None;
                Some(Msg::ModalDismissed)
            }
            Event::Keyboard(KeyEvent {
                code: Key::Char('c'),
                modifiers,
                ..
            }) if modifiers.contains(KeyModifiers::CONTROL) => {
                self.rx = None;
                Some(Msg::ModalDismissed)
            }
            // Tick events drive the spinner + result polling. tuirealm
            // emits `Event::Tick` based on `EventListenerCfg::tick_interval`.
            Event::Tick => {
                self.spinner_idx = self.spinner_idx.wrapping_add(1);
                match self.take_result() {
                    TakeOutcome::Got(payload) => {
                        Some(Msg::LoadingResolved(crate::realm::model::PayloadCarrier(
                            std::sync::Arc::new(std::sync::Mutex::new(Some(payload))),
                        )))
                    }
                    TakeOutcome::Cancelled => {
                        // Producer task died (panic, dropped sender,
                        // etc.) — dismiss the modal so the user
                        // isn't stuck on a forever spinner.
                        tracing::warn!(
                            label = %self.label,
                            "Loading modal producer dropped sender without delivering — dismissing"
                        );
                        Some(Msg::ModalDismissed)
                    }
                    TakeOutcome::Pending | TakeOutcome::Done => None,
                }
            }
            _ => None,
        }
    }
}
