//! `WorktreeProgress` — spinner + step checklist shown while a first
//! `w`/`c`/`s` on a fresh workspace provisions its worktree (cold
//! clone / fetch / `git worktree add` / mounts / scripts).
//!
//! Unlike [`super::loading::Loading`], this modal is NOT channel-driven:
//! progress arrives as `Event::WorktreeProgress` over IPC, which the
//! `Model` folds into a [`WorktreeProgressState`] and re-mounts a fresh
//! component from on each step. The spinner advances itself on `Tick`
//! and emits [`Msg::WorktreeProgressTick`] so the run loop repaints
//! during the long, otherwise-silent checkout — without it the spinner
//! would freeze exactly when the user needs to see liveness.
//!
//! The matching `TerminalSpawned` dismisses the modal; a failed step
//! keeps it up (red, with the error) so the user reads it before
//! pressing Esc rather than facing a silent hang.

use crate::realm::Msg;
use crate::realm::UserEvent;
use lazybox_core::SessionKey;
use lazybox_ipc::{WorktreeStep, WorktreeStepStatus};
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyEvent, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use tuirealm::state::State;

const SPINNER_FRAMES: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Render state of one checklist row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    /// Not started yet — hollow bullet.
    Pending,
    /// In flight — spinner.
    Active,
    /// Finished — check mark.
    Done,
    /// Errored — cross; the modal stops auto-dismissing.
    Failed,
}

/// Accumulated provisioning progress for one spawn. Lives on the
/// `Model`; each `Event::WorktreeProgress` folds in via [`Self::apply`]
/// and the modal is re-mounted from the result so the checklist
/// advances in place.
#[derive(Debug, Clone)]
pub struct WorktreeProgressState {
    pub session_key: SessionKey,
    checkout: StepState,
    setup: StepState,
    /// The agent/shell launch. The daemon doesn't report it (its
    /// completion IS the `TerminalSpawned` that dismisses this modal),
    /// so it only ever goes Pending → Active, set when `Setup` finishes.
    agent: StepState,
    error: Option<String>,
}

impl WorktreeProgressState {
    pub fn new(session_key: SessionKey) -> Self {
        Self {
            session_key,
            checkout: StepState::Pending,
            setup: StepState::Pending,
            agent: StepState::Pending,
            error: None,
        }
    }

    /// A step failed — the modal should stay up showing the error
    /// rather than auto-dismissing on the (fallback) `TerminalSpawned`.
    pub fn failed(&self) -> bool {
        self.error.is_some()
    }

    /// Fold one daemon progress transition into the checklist.
    pub fn apply(&mut self, step: WorktreeStep, status: WorktreeStepStatus) {
        match (step, status) {
            (WorktreeStep::Checkout, WorktreeStepStatus::Started) => {
                self.checkout = StepState::Active;
            }
            (WorktreeStep::Checkout, WorktreeStepStatus::Done) => {
                self.checkout = StepState::Done;
            }
            (WorktreeStep::Checkout, WorktreeStepStatus::Failed(e)) => {
                self.checkout = StepState::Failed;
                self.error = Some(e);
            }
            (WorktreeStep::Setup, WorktreeStepStatus::Started) => {
                self.checkout = StepState::Done;
                self.setup = StepState::Active;
            }
            (WorktreeStep::Setup, WorktreeStepStatus::Done) => {
                self.setup = StepState::Done;
                self.agent = StepState::Active;
            }
            (WorktreeStep::Setup, WorktreeStepStatus::Failed(e)) => {
                self.setup = StepState::Failed;
                self.error = Some(e);
            }
        }
    }

    fn steps(&self) -> [(&'static str, StepState); 3] {
        [
            ("Creating worktree", self.checkout),
            ("Setting up", self.setup),
            ("Starting agent", self.agent),
        ]
    }
}

/// Modal renderer. A pure snapshot of [`WorktreeProgressState`] plus a
/// self-advancing spinner index.
pub struct WorktreeProgress {
    steps: [(&'static str, StepState); 3],
    error: Option<String>,
    spinner_idx: usize,
}

impl WorktreeProgress {
    pub fn from_state(state: &WorktreeProgressState) -> Self {
        Self {
            steps: state.steps(),
            error: state.error.clone(),
            spinner_idx: 0,
        }
    }
}

impl Component for WorktreeProgress {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let spinner = SPINNER_FRAMES[self.spinner_idx % SPINNER_FRAMES.len()];

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::raw(""));
        for (label, state) in &self.steps {
            let (glyph, glyph_style) = match state {
                StepState::Pending => ("○".to_string(), Style::default().fg(theme.text_dim)),
                StepState::Active => (
                    spinner.to_string(),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                StepState::Done => ("✓".to_string(), Style::default().fg(theme.success)),
                StepState::Failed => ("✗".to_string(), Style::default().fg(theme.error)),
            };
            let label_style = match state {
                StepState::Pending => Style::default().fg(theme.text_dim),
                StepState::Failed => Style::default().fg(theme.error),
                _ => Style::default().fg(theme.text_strong),
            };
            lines.push(Line::from(vec![
                Span::styled(format!("  {glyph}  "), glyph_style),
                Span::styled((*label).to_string(), label_style),
            ]));
        }
        lines.push(Line::raw(""));
        if let Some(err) = &self.error {
            lines.push(Line::from(Span::styled(
                format!("  {err}"),
                Style::default().fg(theme.error),
            )));
            lines.push(Line::from(Span::styled("  Esc dismiss", theme.hint())));
        } else {
            lines.push(Line::from(Span::styled("  Esc cancel", theme.hint())));
        }

        let modal_w = 60u16.min(area.width.saturating_sub(4));
        let modal_h = (lines.len() as u16 + 2).min(area.height);
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Span::styled(" Setting up workspace ", theme.modal_title()))
            .border_style(theme.modal_border());
        let inner = block.inner(modal);
        frame.render_widget(block, modal);
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

impl AppComponent<Msg, UserEvent> for WorktreeProgress {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        match ev {
            Event::Keyboard(KeyEvent { code: Key::Esc, .. }) => Some(Msg::ModalDismissed),
            Event::Keyboard(KeyEvent {
                code: Key::Char('c'),
                modifiers,
                ..
            }) if modifiers.contains(KeyModifiers::CONTROL) => Some(Msg::ModalDismissed),
            // Advance the spinner and ask the run loop to repaint — the
            // checkout phase emits no events for seconds, so without a
            // per-tick redraw the spinner would look frozen.
            Event::Tick => {
                self.spinner_idx = self.spinner_idx.wrapping_add(1);
                Some(Msg::WorktreeProgressTick)
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(comp: &mut WorktreeProgress, w: u16, h: u16) -> String {
        use tuirealm::ratatui::Terminal;
        use tuirealm::ratatui::backend::TestBackend;
        let backend = TestBackend::new(w, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|frame| comp.view(frame, Rect::new(0, 0, w, h)))
            .unwrap();
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

    fn state() -> WorktreeProgressState {
        WorktreeProgressState::new(SessionKey::new("github:acme/widget#42"))
    }

    #[test]
    fn checkout_in_flight_shows_spinner_and_pending_rows() {
        let mut st = state();
        st.apply(WorktreeStep::Checkout, WorktreeStepStatus::Started);
        let mut comp = WorktreeProgress::from_state(&st);
        let out = render(&mut comp, 70, 12);
        assert!(out.contains("Setting up workspace"), "{out}");
        assert!(out.contains("Creating worktree"), "{out}");
        assert!(out.contains("Starting agent"), "{out}");
        // Later steps still pending → hollow bullet.
        assert!(out.contains('○'), "{out}");
        assert!(out.contains("Esc cancel"), "{out}");
    }

    #[test]
    fn setup_done_checks_off_earlier_steps_and_starts_agent() {
        let mut st = state();
        st.apply(WorktreeStep::Checkout, WorktreeStepStatus::Started);
        st.apply(WorktreeStep::Checkout, WorktreeStepStatus::Done);
        st.apply(WorktreeStep::Setup, WorktreeStepStatus::Started);
        st.apply(WorktreeStep::Setup, WorktreeStepStatus::Done);
        assert!(!st.failed());
        let mut comp = WorktreeProgress::from_state(&st);
        let out = render(&mut comp, 70, 12);
        // Two completed steps render check marks.
        assert_eq!(out.matches('✓').count(), 2, "{out}");
    }

    #[test]
    fn failed_step_surfaces_error_and_switches_footer() {
        let mut st = state();
        st.apply(WorktreeStep::Checkout, WorktreeStepStatus::Started);
        st.apply(
            WorktreeStep::Checkout,
            WorktreeStepStatus::Failed("fatal: could not read from remote".into()),
        );
        assert!(st.failed());
        let mut comp = WorktreeProgress::from_state(&st);
        let out = render(&mut comp, 70, 12);
        assert!(out.contains('✗'), "{out}");
        assert!(out.contains("could not read from remote"), "{out}");
        assert!(out.contains("Esc dismiss"), "{out}");
    }
}
