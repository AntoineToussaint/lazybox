//! `WorktreeProgress` — spinner + step checklist shown while a first
//! `w`/`c`/`s` on a fresh workspace provisions its worktree (refresh the
//! base ref / `git worktree add` / mounts / scripts, plus a one-time
//! blobless bare clone the very first time a repo is used).
//!
//! lazybox clones a repo's bare mirror exactly once and then does a
//! `git worktree add` per workspace — it never re-clones. So the first
//! checklist row reads "Preparing worktree" for the common (warm) case
//! and only upgrades to "Cloning repository (one-time)" when a genuine
//! cold clone is actually running, never implying a per-workspace clone.
//!
//! Unlike [`super::loading::Loading`], this modal is NOT channel-driven:
//! progress arrives as `Event::WorktreeProgress` over IPC, which the
//! `Model` folds into a [`WorktreeProgressState`] and re-mounts a fresh
//! component from on each step. The spinner advances itself on `Tick`
//! and emits [`Msg::WorktreeProgressTick`] so the run loop repaints
//! during the long, otherwise-silent checkout — without it the spinner
//! would freeze exactly when the user needs to see liveness.
//!
//! Provisioning can be near-instant (warm clone, no mounts/scripts), so
//! the *displayed* checklist is decoupled from the *daemon* truth: the
//! daemon's progress sets a `target` stage, and the display walks toward
//! it at no more than one step per [`MIN_STEP_DWELL`] (driven by the
//! per-tick [`WorktreeProgressState::tick`]). The dwell is a floor, not
//! a fixed delay — a step the daemon genuinely spends seconds on still
//! shows in real time, because the display tracks `target` whenever it's
//! ahead.
//!
//! The matching `TerminalSpawned` doesn't tear the modal down on the
//! spot; it *queues* the dismiss (`target` jumps to ready), and the
//! modal closes only once every step has been shown for its dwell — so
//! a fast spawn walks the full checklist instead of flashing the first
//! step. A failed step freezes the display (red, with the error) so the
//! user reads it before pressing Esc rather than facing a silent hang.

use crate::realm::Msg;
use crate::realm::UserEvent;
use lazybox_core::SessionKey;
use lazybox_ipc::{WorktreeRecovery, WorktreeStep, WorktreeStepStatus};
use std::time::{Duration, Instant};
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

/// Minimum time each checklist step stays visible before the display
/// advances to the next one. A floor: a step the daemon spends longer on
/// shows in real time. Keeps a fast provision from flashing past the
/// checklist faster than the eye can read it.
pub const MIN_STEP_DWELL: Duration = Duration::from_millis(500);

/// The checklist rows, in display order. The discriminant IS the
/// `target`/`shown` frontier index for that row — naming the rows here
/// is what keeps [`WorktreeProgressState::apply`]'s advance arithmetic
/// and [`WorktreeProgressState::steps`]'s label order from drifting
/// apart (they used to be hand-synced magic numbers). [`ROWS`] lists
/// them in order; `row_indices_match_discriminants` pins the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Row {
    /// An optional one-time `git clone --bare` plus the always-present
    /// base-ref fetch — both fold into this single "Preparing worktree"
    /// row, so a warm provision never lights up a clone row.
    Prepare = 0,
    /// `git worktree add` materializing the checkout on disk.
    WorktreeAdd = 1,
    /// Applying configured mounts + setup scripts.
    Setup = 2,
    /// Handing off to the agent / shell.
    Agent = 3,
}

/// The rows in display order. Indexed positionally by `steps` and the
/// renderer; each entry's discriminant equals its index (asserted in
/// tests) so positional and `Row`-keyed access agree.
const ROWS: [Row; STEP_COUNT as usize] = [Row::Prepare, Row::WorktreeAdd, Row::Setup, Row::Agent];

impl Row {
    /// Which checklist row a failed [`WorktreeStep`] belongs to. The
    /// daemon now emits the step that actually aborted (issue #557
    /// acceptance #2), so the ✗ can land on the right row instead of
    /// always "Cloning". `Clone`/`Fetch` are both the "Preparing
    /// worktree" row; `Setup` and the agent launch share the setup row.
    fn for_step(step: WorktreeStep) -> Self {
        match step {
            WorktreeStep::Clone | WorktreeStep::Fetch => Row::Prepare,
            WorktreeStep::WorktreeAdd => Row::WorktreeAdd,
            WorktreeStep::Setup => Row::Setup,
        }
    }
}

/// Number of checklist rows: prepare (clone-if-needed + fetch base),
/// worktree-add, setup, agent launch.
const STEP_COUNT: u8 = Row::Agent as u8 + 1;

/// `target`/`shown` value meaning "all steps complete, ready to dismiss"
/// — one past the last real step.
const READY: u8 = STEP_COUNT;

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
///
/// The display lags the daemon on purpose. `target` is how far the
/// daemon has actually gotten (0 = preparing the worktree, i.e. an
/// optional one-time clone plus the base-ref fetch, 1 = worktree-add,
/// 2 = setup, 3 = agent, `READY` = the session is live); `shown` is how
/// far the checklist has been *revealed* to the user. [`Self::tick`]
/// walks `shown` toward `target` at no more than one step per
/// [`MIN_STEP_DWELL`], so a fast provision still shows every step
/// legibly.
#[derive(Debug, Clone)]
pub struct WorktreeProgressState {
    pub session_key: SessionKey,
    /// A genuine cold clone is running. Set when a `Clone` step arrives,
    /// which the daemon only emits on a real first-time bare clone (a
    /// cached bare clone is reused silently). Upgrades the first row's
    /// label from "Preparing worktree" to the one-time clone message —
    /// the warm path never claims to be cloning.
    cold_clone: bool,
    /// Latest transfer-progress line from the in-flight clone
    /// (`Receiving objects: 42% …`), rendered as a dim detail line
    /// under the clone row while it's active — so a multi-hundred-MB
    /// transfer shows bytes/percent instead of an opaque spinner.
    clone_progress: Option<String>,
    /// Daemon truth: the highest stage reached (`0..=READY`).
    target: u8,
    /// Displayed frontier: the stage currently shown as `Active`
    /// (`0..=READY`). Lower steps render `Done`, higher ones `Pending`.
    shown: u8,
    /// When `shown` last advanced — the min-dwell clock.
    shown_since: Instant,
    /// `TerminalSpawned` arrived: dismiss once `shown` reaches `READY`.
    dismiss_queued: bool,
    /// Index of the failed step (and its error message). Freezes the
    /// display and keeps the modal up until the user acknowledges it.
    failed_step: Option<u8>,
    error: Option<String>,
    /// How the failure can be recovered, classified from the error text
    /// (issue #557). Drives the modal's per-class hint + retry
    /// affordance so a failed provision is never just "Esc dismiss".
    recovery: Option<WorktreeRecovery>,
    /// A step completed but degraded — the base-ref fetch failed and the
    /// worktree branched off a possibly-stale local ref (issue #320).
    /// Unlike a failure this doesn't freeze the checklist (provisioning
    /// succeeded), but it holds the modal open until the user
    /// acknowledges the note so the staleness isn't invisible.
    warning: Option<String>,
}

impl WorktreeProgressState {
    pub fn new(session_key: SessionKey) -> Self {
        Self {
            session_key,
            cold_clone: false,
            clone_progress: None,
            target: 0,
            shown: 0,
            shown_since: Instant::now(),
            dismiss_queued: false,
            failed_step: None,
            error: None,
            recovery: None,
            warning: None,
        }
    }

    /// A step failed — the modal should stay up showing the error
    /// rather than auto-dismissing on the (fallback) `TerminalSpawned`.
    pub fn failed(&self) -> bool {
        self.error.is_some()
    }

    /// A step completed but degraded (stale base ref). The checklist
    /// still finishes, but the modal is held open until the user
    /// acknowledges the note — see [`Self::ready_to_dismiss`].
    pub fn warned(&self) -> bool {
        self.warning.is_some()
    }

    /// A completion signal (`TerminalSpawned` or a lag-recovery
    /// `Snapshot`) has queued a dismiss; the modal stays up until the
    /// display has walked every remaining step for its dwell.
    pub fn dismiss_queued(&self) -> bool {
        self.dismiss_queued
    }

    /// Fold one daemon progress transition into the checklist's
    /// `target`. The display catches up later via [`Self::tick`]. Each
    /// step's `Started` advances `target` to that step's row, so the
    /// previous step checks off and this one becomes the in-flight
    /// spinner. `Clone` and `Fetch` both belong to row 0 ("Preparing
    /// worktree") — the clone is an optional one-time prelude to the
    /// always-present base fetch — so neither advances `target`; `Clone`
    /// only flips the row's label to the one-time clone message.
    pub fn apply(&mut self, step: WorktreeStep, status: WorktreeStepStatus) {
        match (step, status) {
            // A failure freezes the checklist. The daemon now names the
            // sub-phase that actually aborted (issue #557), so place the
            // ✗ on that row instead of wherever the display happened to
            // be; the error text still carries the full detail.
            (step, WorktreeStepStatus::Failed(e)) => self.fail_current(step, e),
            // A degraded (not failed) step: record the note so the modal
            // surfaces it and holds for acknowledgement, but let the
            // checklist keep advancing — provisioning did succeed.
            (_, WorktreeStepStatus::Warned(msg)) => self.warning = Some(msg),
            (WorktreeStep::Clone, WorktreeStepStatus::Progress(line)) => {
                self.cold_clone = true;
                self.clone_progress = Some(line);
            }
            // No other step streams progress today; tolerate rather
            // than misfile a future one.
            (_, WorktreeStepStatus::Progress(_)) => {}
            (WorktreeStep::Clone, _) => self.cold_clone = true,
            (WorktreeStep::Fetch, _) => {}
            (WorktreeStep::WorktreeAdd, _) => self.advance_to(Row::WorktreeAdd),
            (WorktreeStep::Setup, WorktreeStepStatus::Started) => self.advance_to(Row::Setup),
            // Setup done means the daemon is now launching the agent, so
            // the in-flight frontier is the agent row.
            (WorktreeStep::Setup, WorktreeStepStatus::Done) => self.advance_to(Row::Agent),
        }
    }

    /// Advance the daemon-truth frontier to `row`. Monotonic — a later
    /// out-of-order event can never rewind the checklist.
    fn advance_to(&mut self, row: Row) {
        self.target = self.target.max(row as u8);
    }

    /// The label for `row`, cold/warm-aware on the first two rows.
    fn label(&self, row: Row) -> &'static str {
        match row {
            Row::Prepare if self.cold_clone => "Cloning repository (one-time)",
            Row::Prepare => "Preparing worktree",
            // The blobless clone defers file contents, so the first
            // checkout after a cold clone is where the bulk download
            // actually happens — name the wait instead of implying a
            // quick local materialization.
            Row::WorktreeAdd if self.cold_clone => "Creating worktree (downloading files)",
            Row::WorktreeAdd => "Creating worktree",
            Row::Setup => "Setting up",
            Row::Agent => "Starting agent",
        }
    }

    fn fail_current(&mut self, step: WorktreeStep, error: String) {
        // Land the ✗ on the row the daemon says aborted, but never rewind
        // it behind a row already shown in-flight (a late/unknown failure
        // during a later phase shouldn't jump the mark backwards).
        let row = Row::for_step(step) as u8;
        self.failed_step = Some(row.max(self.shown));
        self.recovery = Some(WorktreeRecovery::classify(&error));
        self.error = Some(error);
    }

    /// The failure's recovery class, once a step has failed. `None` while
    /// provisioning is in flight or succeeded.
    pub fn recovery(&self) -> Option<WorktreeRecovery> {
        self.recovery
    }

    /// The raw failure text, once a step has failed. Carries the holder
    /// path the recovery actions parse out (issue #787).
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Queue dismissal: the session is live (`TerminalSpawned`), so the
    /// modal should close once the display has walked through every step
    /// for its dwell. The work itself already finished in the background.
    pub fn queue_dismiss(&mut self) {
        self.dismiss_queued = true;
        self.target = READY;
    }

    /// Advance the displayed checklist one step toward `target` if the
    /// current step has been shown for at least [`MIN_STEP_DWELL`].
    /// Returns whether the display changed (so the caller re-renders).
    /// A no-op once the display has caught up, or on a failed step.
    pub fn tick(&mut self, now: Instant) -> bool {
        if self.failed() || self.shown >= self.target {
            return false;
        }
        if now.duration_since(self.shown_since) < MIN_STEP_DWELL {
            return false;
        }
        self.shown += 1;
        self.shown_since = now;
        true
    }

    /// Whether the modal should now be torn down: a `TerminalSpawned`
    /// queued the dismiss and the display has finished walking the
    /// checklist. A degraded provision (`warned`) is held open even once
    /// the checklist is complete, so the stale-base note is acknowledged
    /// with Esc rather than flashing past — the session is already live
    /// behind the modal.
    pub fn ready_to_dismiss(&self) -> bool {
        self.dismiss_queued && self.shown >= READY && self.warning.is_none()
    }

    fn steps(&self) -> [(&'static str, StepState); STEP_COUNT as usize] {
        let mut out = [("", StepState::Pending); STEP_COUNT as usize];
        for (i, &row) in ROWS.iter().enumerate() {
            let idx = i as u8;
            let state = match self.failed_step {
                Some(f) if idx == f => StepState::Failed,
                Some(f) if idx < f => StepState::Done,
                Some(_) => StepState::Pending,
                None if idx < self.shown => StepState::Done,
                None if idx == self.shown => StepState::Active,
                None => StepState::Pending,
            };
            out[i] = (self.label(row), state);
        }
        out
    }
}

/// Modal renderer. A pure snapshot of [`WorktreeProgressState`] plus a
/// self-advancing spinner index.
pub struct WorktreeProgress {
    steps: [(&'static str, StepState); STEP_COUNT as usize],
    clone_progress: Option<String>,
    error: Option<String>,
    recovery: Option<WorktreeRecovery>,
    /// Whether the failed operation can be retried without first leaving
    /// the modal to resolve a conflict or invalid input.
    retryable: bool,
    /// Whether lazybox can recover in-modal by preserving the conflicting
    /// checkout aside and re-provisioning (issue #787) — a wrong-branch,
    /// dirty-leftover, or non-live managed holder.
    recreatable: bool,
    /// Whether the conflict is a live session holding the branch, so the
    /// modal offers a jump to that session instead of a recreate.
    jump: bool,
    /// Whether the failure is an unmapped Linear team, so the modal offers
    /// a repo pick that persists the mapping and re-provisions (#1041).
    picks_repo: bool,
    warning: Option<String>,
    spinner_idx: usize,
}

impl WorktreeProgress {
    pub fn from_state(state: &WorktreeProgressState) -> Self {
        Self {
            steps: state.steps(),
            clone_progress: state.clone_progress.clone(),
            error: state.error.clone(),
            recovery: state.recovery,
            retryable: state.recovery.is_some_and(|recovery| recovery.retryable()),
            recreatable: state
                .recovery
                .is_some_and(|recovery| recovery.recreatable()),
            jump: state
                .recovery
                .is_some_and(|recovery| recovery.jump_to_holder()),
            picks_repo: state.recovery.is_some_and(|recovery| recovery.picks_repo()),
            warning: state.warning.clone(),
            spinner_idx: 0,
        }
    }
}

/// Rows a line occupies under ratatui's word wrap at `width` columns.
/// A plain `ceil(width / cols)` under-counts: word wrapping breaks at
/// spaces and leaves the tail of each row unused, so a long remediation
/// sentence took one row more than the estimate and its last line was
/// clipped. Greedy on whitespace, hard-splitting a word wider than the
/// row, and never less than the character-count bound.
fn wrapped_rows(text: &str, width: u16) -> u16 {
    let display_width = |s: &str| Span::raw(s).width();
    let width = usize::from(width.max(1));
    let mut rows: usize = 1;
    let mut col: usize = 0;
    for (i, word) in text.split(' ').enumerate() {
        let mut w = display_width(word);
        // The separating space rides with the following word.
        let lead = usize::from(i > 0);
        if col + lead + w > width && col > 0 {
            rows += 1;
            col = 0;
        } else {
            col += lead;
        }
        while w > width {
            rows += 1;
            w -= width;
        }
        col += w;
    }
    let ceil = display_width(text).div_ceil(width).max(1);
    u16::try_from(rows.max(ceil)).unwrap_or(u16::MAX)
}

impl Component for WorktreeProgress {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let spinner = SPINNER_FRAMES[self.spinner_idx % SPINNER_FRAMES.len()];

        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::raw(""));
        for (i, (label, state)) in self.steps.iter().enumerate() {
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
            // Live transfer detail under the clone row while it spins —
            // bytes/percent for the one genuinely long step.
            if i == Row::Prepare as usize
                && *state == StepState::Active
                && let Some(progress) = &self.clone_progress
            {
                lines.push(Line::from(Span::styled(
                    format!("     {progress}"),
                    Style::default().fg(theme.text_dim),
                )));
            }
        }
        lines.push(Line::raw(""));
        // The key hints live in a footer row rendered *separately* from
        // the wrapped body: an error naming a long managed path (twice)
        // plus its remediation can exceed the modal's height, and when
        // they flowed through one paragraph the last line — the only one
        // telling the user what keys do anything — was the one clipped.
        let footer: &str = if let Some(err) = &self.error {
            lines.push(Line::from(Span::styled(
                format!("  {err}"),
                Style::default().fg(theme.error),
            )));
            // Per-class recovery guidance (issue #557): every failure
            // names a concrete next step.
            let recovery = self.recovery.unwrap_or(WorktreeRecovery::Unknown);
            lines.push(Line::from(Span::styled(
                format!("  {}", recovery.remediation(err)),
                Style::default().fg(theme.warn),
            )));
            // Every recoverable class offers a one-keypress path back to a
            // working state (issue #787): `r` retries the transient
            // classes and recreates the ones lazybox can safely rebuild
            // (preserve the conflicting checkout aside, re-provision); `g`
            // jumps to the live session holding a branch we can't take.
            if self.retryable {
                "  r retry · Esc dismiss"
            } else if self.recreatable {
                "  r recreate · Esc dismiss"
            } else if self.jump {
                "  g go to holder · Esc dismiss"
            } else if self.picks_repo {
                "  r pick repo · Esc dismiss"
            } else {
                "  Esc dismiss"
            }
        } else if let Some(warn) = &self.warning {
            // Provisioning succeeded but degraded: show the stale-base
            // note in amber and hold for acknowledgement (Esc), so the
            // "branched off latest main" degradation isn't invisible.
            lines.push(Line::from(Span::styled(
                format!("  ⚠ {warn}"),
                Style::default().fg(theme.warn),
            )));
            "  Esc dismiss"
        } else {
            "  Esc cancel"
        };

        let modal_w = 60u16.min(area.width.saturating_sub(4));
        // Size the modal to the *wrapped* height, not the logical line
        // count — a long stale-base note or error message wraps inside
        // the fixed-width modal.
        let inner_w = modal_w.saturating_sub(2).max(1);
        let body_rows: u16 = lines
            .iter()
            .map(|l| {
                let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                wrapped_rows(&text, inner_w)
            })
            .sum();
        // body + footer row + borders, capped at the screen.
        let modal_h = (body_rows + 1 + 2).min(area.height);
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
        // Reserve the last inner row for the footer no matter how much
        // the body wraps; the body is what gets clipped, never the keys.
        let footer_h = 1u16.min(inner.height);
        let body_area = Rect::new(
            inner.x,
            inner.y,
            inner.width,
            inner.height.saturating_sub(footer_h),
        );
        let footer_area = Rect::new(inner.x, inner.y + body_area.height, inner.width, footer_h);
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body_area);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(footer, theme.hint()))),
            footer_area,
        );
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
            // On a retryable failure, `r` re-issues the spawn that
            // failed. Failures requiring an out-of-band action leave it
            // unbound so the UI never advertises a retry that must fail.
            Event::Keyboard(KeyEvent {
                code: Key::Char('r'),
                ..
            }) if self.retryable => Some(Msg::WorktreeRetry),
            // `r` on a recreatable failure preserves the conflicting
            // checkout aside and re-provisions (issue #787) — the same
            // "get me unstuck" key, since retry is meaningless there.
            Event::Keyboard(KeyEvent {
                code: Key::Char('r'),
                ..
            }) if self.recreatable => Some(Msg::WorktreeRecreate),
            // `g` jumps to the live session already holding the branch.
            Event::Keyboard(KeyEvent {
                code: Key::Char('g'),
                ..
            }) if self.jump => Some(Msg::WorktreeJumpToHolder),
            // `r` on an unmapped Linear team opens the repo picker (#1041).
            Event::Keyboard(KeyEvent {
                code: Key::Char('r'),
                ..
            }) if self.picks_repo => Some(Msg::WorktreePickRepo),
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

    /// The rendered body as one line: borders stripped, wrapped rows
    /// re-joined on a space. Lets a test assert on a whole remediation
    /// command regardless of where the 60-column modal word-wraps it.
    fn flatten(out: &str) -> String {
        out.lines()
            .map(|l| l.trim_matches(|c| c == '│' || c == ' '))
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn state() -> WorktreeProgressState {
        WorktreeProgressState::new(SessionKey::new("github:acme/widget#42"))
    }

    /// `apply()` advances `target` by `Row as u8` while `steps()` indexes
    /// `ROWS` positionally — the two only agree if each row's discriminant
    /// equals its slot. Pin that so a future reorder can't silently put
    /// the spinner on the wrong row.
    #[test]
    fn row_indices_match_discriminants() {
        assert_eq!(ROWS.len(), STEP_COUNT as usize);
        for (i, &row) in ROWS.iter().enumerate() {
            assert_eq!(
                row as u8, i as u8,
                "ROWS[{i}] discriminant must equal its index"
            );
        }
    }

    #[test]
    fn cold_clone_labels_the_first_row_as_a_one_time_step() {
        let mut st = state();
        // A real cold clone is the only thing that emits `Clone`.
        st.apply(WorktreeStep::Clone, WorktreeStepStatus::Started);
        let mut comp = WorktreeProgress::from_state(&st);
        let out = render(&mut comp, 70, 12);
        assert!(out.contains("Setting up workspace"), "{out}");
        assert!(out.contains("Cloning repository (one-time)"), "{out}");
        // The first checkout after a cold clone is the bulk download.
        assert!(
            out.contains("Creating worktree (downloading files)"),
            "{out}"
        );
        assert!(out.contains("Starting agent"), "{out}");
        // Later steps still pending → hollow bullet.
        assert!(out.contains('○'), "{out}");
        assert!(out.contains("Esc cancel"), "{out}");
    }

    /// A live clone-transfer line renders as a detail under the active
    /// clone row (issue #405: a multi-hundred-MB clone must show
    /// bytes/percent, not an opaque spinner) — and disappears once the
    /// checklist moves past the clone.
    #[test]
    fn clone_progress_detail_shows_while_cloning_then_clears() {
        let mut st = state();
        st.apply(WorktreeStep::Clone, WorktreeStepStatus::Started);
        st.apply(
            WorktreeStep::Clone,
            WorktreeStepStatus::Progress(
                "Receiving objects: 42% (1200/2900), 12.00 MiB | 1.20 MiB/s".into(),
            ),
        );
        let out = render(&mut WorktreeProgress::from_state(&st), 70, 12);
        assert!(out.contains("Cloning repository (one-time)"), "{out}");
        assert!(out.contains("Receiving objects: 42%"), "{out}");
        // A later line replaces, not appends.
        st.apply(
            WorktreeStep::Clone,
            WorktreeStepStatus::Progress("Receiving objects: 90% (2610/2900)".into()),
        );
        let out = render(&mut WorktreeProgress::from_state(&st), 70, 12);
        assert!(out.contains("Receiving objects: 90%"), "{out}");
        assert!(!out.contains("42%"), "{out}");
        // Once the worktree add starts and the display advances, the
        // clone row is Done and the transfer detail goes with it.
        st.apply(WorktreeStep::WorktreeAdd, WorktreeStepStatus::Started);
        let t0 = Instant::now();
        st.shown_since = t0;
        assert!(st.tick(t0 + MIN_STEP_DWELL));
        let out = render(&mut WorktreeProgress::from_state(&st), 70, 12);
        assert!(!out.contains("Receiving objects"), "{out}");
    }

    /// The warm path (a bare clone already exists, so only `git worktree
    /// add` runs) must never claim to be cloning — it walks straight from
    /// the daemon's mount `Fetch` to the worktree add.
    #[test]
    fn warm_provision_never_says_cloning() {
        let mut st = state();
        // Mirrors the daemon's mount event for a repo whose bare clone is
        // already cached: `Fetch`, never `Clone`.
        st.apply(WorktreeStep::Fetch, WorktreeStepStatus::Started);
        let out = render(&mut WorktreeProgress::from_state(&st), 70, 12);
        assert!(out.contains("Preparing worktree"), "{out}");
        assert!(
            !out.to_lowercase().contains("clon"),
            "warm provision must not imply a clone: {out}",
        );
        // A following worktree-add still advances the checklist normally
        // — and, blobs being local on the warm path, never claims to be
        // downloading either.
        st.apply(WorktreeStep::WorktreeAdd, WorktreeStepStatus::Started);
        let t0 = Instant::now();
        st.shown_since = t0;
        assert!(st.tick(t0 + MIN_STEP_DWELL));
        let out = render(&mut WorktreeProgress::from_state(&st), 70, 12);
        assert!(out.contains("Creating worktree"), "{out}");
        assert!(!out.contains("downloading"), "{out}");
    }

    #[test]
    fn sub_steps_check_off_earlier_rows_in_order() {
        let mut st = state();
        // Cold-clone provision walks clone → fetch (both row 0) →
        // worktree-add → setup.
        st.apply(WorktreeStep::Clone, WorktreeStepStatus::Started);
        st.apply(WorktreeStep::Fetch, WorktreeStepStatus::Started);
        st.apply(WorktreeStep::WorktreeAdd, WorktreeStepStatus::Started);
        st.apply(WorktreeStep::Setup, WorktreeStepStatus::Started);
        st.apply(WorktreeStep::Setup, WorktreeStepStatus::Done);
        assert!(!st.failed());
        // Daemon truth says the agent is launching (target == 3), but the
        // display only checks earlier steps off once they've dwelt.
        let t0 = Instant::now();
        st.shown_since = t0;
        for n in 1..=3 {
            assert!(st.tick(t0 + MIN_STEP_DWELL * n));
        }
        let mut comp = WorktreeProgress::from_state(&st);
        let out = render(&mut comp, 70, 12);
        // Three completed steps render check marks; the agent row spins.
        assert_eq!(out.matches('✓').count(), 3, "{out}");
    }

    /// The regression: with provisioning faster than the dwell, every
    /// step must still be shown before the modal is allowed to dismiss.
    #[test]
    fn fast_provision_walks_every_step_before_dismissing() {
        let mut st = state();
        let t0 = Instant::now();
        st.shown_since = t0;
        // All daemon transitions + the TerminalSpawned dismiss land in a
        // single burst, far faster than the dwell.
        st.apply(WorktreeStep::Clone, WorktreeStepStatus::Started);
        st.apply(WorktreeStep::Fetch, WorktreeStepStatus::Started);
        st.apply(WorktreeStep::WorktreeAdd, WorktreeStepStatus::Started);
        st.apply(WorktreeStep::Setup, WorktreeStepStatus::Started);
        st.apply(WorktreeStep::Setup, WorktreeStepStatus::Done);
        st.queue_dismiss();

        // First step is shown as Active and the dismiss is held.
        assert_eq!(st.shown, 0);
        assert!(
            !st.ready_to_dismiss(),
            "dismissed before walking the checklist"
        );

        // A tick faster than the dwell does NOT advance — the step stays
        // legible.
        assert!(!st.tick(t0 + MIN_STEP_DWELL / 5));
        assert_eq!(st.shown, 0);

        // Each step in turn becomes Active, and only after all rows have
        // dwelt does the modal become ready to dismiss.
        let mut active_seen = Vec::new();
        for step in 0..STEP_COUNT {
            let out = render(&mut WorktreeProgress::from_state(&st), 70, 12);
            // The currently-shown step is the spinning one; record it so
            // we can prove each rendered before dismissal.
            active_seen.push(st.shown);
            assert!(!st.ready_to_dismiss(), "dismissed at step {step}: {out}");
            assert!(st.tick(t0 + MIN_STEP_DWELL * u32::from(step + 1)));
        }
        assert_eq!(active_seen, vec![0, 1, 2, 3], "every step shown in order");
        assert!(
            st.ready_to_dismiss(),
            "never dismissed after walking checklist"
        );
    }

    /// The dwell is a floor, not a ceiling: a slow provision tracks the
    /// daemon in real time instead of being held back.
    #[test]
    fn slow_provision_tracks_daemon_in_real_time() {
        let mut st = state();
        let t0 = Instant::now();
        st.shown_since = t0;
        st.apply(WorktreeStep::Clone, WorktreeStepStatus::Started);
        st.apply(WorktreeStep::Fetch, WorktreeStepStatus::Started);
        // Clone + fetch share the "Preparing worktree" row: the display
        // stays on it (no later step is revealed) however many ticks
        // fire while those phases run.
        assert!(!st.tick(t0 + MIN_STEP_DWELL * 10));
        assert_eq!(st.shown, 0);
        // The worktree add finally starts — now the display is free to
        // advance, and does so immediately (the dwell long since elapsed).
        st.apply(WorktreeStep::WorktreeAdd, WorktreeStepStatus::Started);
        assert!(st.tick(t0 + MIN_STEP_DWELL * 11));
        assert_eq!(st.shown, 1);
    }

    /// A failed step freezes the display on the error and never reaches
    /// the dismiss state, even if a TerminalSpawned tries to queue one.
    #[test]
    fn failed_step_never_dismisses() {
        let mut st = state();
        let t0 = Instant::now();
        st.shown_since = t0;
        st.apply(WorktreeStep::WorktreeAdd, WorktreeStepStatus::Started);
        st.apply(
            WorktreeStep::WorktreeAdd,
            WorktreeStepStatus::Failed("boom".into()),
        );
        st.queue_dismiss();
        assert!(
            !st.tick(t0 + MIN_STEP_DWELL * 5),
            "advanced past a failed step"
        );
        assert!(!st.ready_to_dismiss(), "dismissed despite a failed step");
        let out = render(&mut WorktreeProgress::from_state(&st), 70, 12);
        assert!(out.contains('✗'), "{out}");
        assert!(out.contains("boom"), "{out}");
    }

    /// A degraded (not failed) provision surfaces the stale-base note in
    /// the checklist and holds the modal open until the user acknowledges
    /// it, even though the session is already live.
    #[test]
    fn warned_step_surfaces_note_and_holds_open() {
        let mut st = state();
        let t0 = Instant::now();
        st.shown_since = t0;
        st.apply(WorktreeStep::Fetch, WorktreeStepStatus::Started);
        st.apply(
            WorktreeStep::Fetch,
            WorktreeStepStatus::Warned(
                "could not refresh main — branched from local ref (a1b2c3d, 3 days ago)".into(),
            ),
        );
        // A warning is not a failure — the checklist still advances.
        assert!(!st.failed());
        assert!(st.warned());

        // The session comes up: the display walks the whole checklist to
        // completion …
        st.apply(WorktreeStep::WorktreeAdd, WorktreeStepStatus::Started);
        st.apply(WorktreeStep::Setup, WorktreeStepStatus::Started);
        st.apply(WorktreeStep::Setup, WorktreeStepStatus::Done);
        st.queue_dismiss();
        for n in 1..=STEP_COUNT {
            st.tick(t0 + MIN_STEP_DWELL * u32::from(n));
        }
        // … but never auto-dismisses while the warning is unacknowledged.
        assert!(
            !st.ready_to_dismiss(),
            "a warned provision must hold open for acknowledgement"
        );

        let out = render(&mut WorktreeProgress::from_state(&st), 70, 14);
        assert!(out.contains('⚠'), "{out}");
        assert!(out.contains("could not refresh main"), "{out}");
        assert!(out.contains("a1b2c3d"), "{out}");
        assert!(out.contains("Esc dismiss"), "{out}");
    }

    #[test]
    fn failed_step_surfaces_error_and_switches_footer() {
        let mut st = state();
        st.apply(WorktreeStep::Clone, WorktreeStepStatus::Started);
        st.apply(
            WorktreeStep::Clone,
            WorktreeStepStatus::Failed("fatal: could not read from remote".into()),
        );
        assert!(st.failed());
        let mut comp = WorktreeProgress::from_state(&st);
        let out = render(&mut comp, 70, 12);
        assert!(out.contains('✗'), "{out}");
        assert!(out.contains("could not read from remote"), "{out}");
        assert!(out.contains("Esc dismiss"), "{out}");
    }

    /// Issue #557 acceptance #2: a failure the daemon reports on the
    /// `WorktreeAdd` phase lands the ✗ on the "Creating worktree" row —
    /// with the earlier "Preparing worktree" row already checked ✓ — even
    /// though the display never dwelt past row 0. No always-"Cloning"
    /// mislabel.
    #[test]
    fn failure_lands_on_the_phase_that_actually_aborted() {
        let mut st = state();
        // Blob download during `git worktree add` fails while the display
        // is still on the Preparing row (shown == 0).
        st.apply(WorktreeStep::Fetch, WorktreeStepStatus::Started);
        st.apply(
            WorktreeStep::WorktreeAdd,
            WorktreeStepStatus::Failed(
                "could not download file contents from origin — worktrees from a \
                 blobless clone need the remote reachable"
                    .into(),
            ),
        );
        assert_eq!(st.recovery(), Some(WorktreeRecovery::Offline));
        let steps = st.steps();
        assert_eq!(steps[Row::Prepare as usize].1, StepState::Done, "prepare ✓");
        assert_eq!(
            steps[Row::WorktreeAdd as usize].1,
            StepState::Failed,
            "worktree-add ✗ — not the Prepare/Clone row"
        );
    }

    /// A live branch holder needs a user decision before provisioning
    /// can succeed, so the modal points at the existing session without
    /// advertising or binding an impossible retry.
    #[test]
    fn branch_holder_modal_offers_join_instead_of_retry() {
        let mut st = state();
        st.apply(WorktreeStep::WorktreeAdd, WorktreeStepStatus::Started);
        st.apply(
            WorktreeStep::WorktreeAdd,
            WorktreeStepStatus::Failed(
                "worktree: checkout_at: branch 'feat' is already checked out at \
                 /tmp/other — refusing to take it from another live worktree"
                    .into(),
            ),
        );
        assert_eq!(st.recovery(), Some(WorktreeRecovery::BranchHeldLive));
        let out = render(&mut WorktreeProgress::from_state(&st), 70, 20);
        assert!(out.contains('✗'), "{out}");
        assert!(out.contains("press g"), "jump guidance: {out}");
        assert!(
            !out.contains("Shift-A"),
            "Shift-A toggles auto-fix now: {out}"
        );
        assert!(
            flatten(&out).contains("(cd '/tmp/other' && git status && git switch --detach)"),
            "{out}"
        );
        assert!(!out.contains("x a"), "adopt cannot release a branch: {out}");
        assert!(!out.contains("r retry"), "invalid retry affordance: {out}");
        assert!(out.contains("Esc dismiss"), "{out}");
        let mut failed = WorktreeProgress::from_state(&st);
        assert!(
            failed
                .on(&Event::Keyboard(KeyEvent::from(Key::Char('r'))))
                .is_none(),
            "r must not retry while another live checkout holds the branch"
        );
    }

    #[test]
    fn managed_branch_holder_names_the_preservation_recovery() {
        let mut st = state();
        st.apply(WorktreeStep::WorktreeAdd, WorktreeStepStatus::Started);
        st.apply(
            WorktreeStep::WorktreeAdd,
            WorktreeStepStatus::Failed(
                "branch 'feat' is held by the non-live managed worktree at /tmp/other \
                 — automatic reclaim blocked because the checkout contains ignored local files"
                    .into(),
            ),
        );
        assert_eq!(st.recovery(), Some(WorktreeRecovery::BranchHeldManaged));
        let out = render(&mut WorktreeProgress::from_state(&st), 70, 20);
        assert!(out.contains("ignored local files"), "{out}");
        assert!(out.contains("Managed checkout"), "{out}");
        assert!(
            flatten(&out).contains("(cd '/tmp/other' && git status && git switch --detach)"),
            "{out}"
        );
        assert!(!out.contains("join the live session"), "{out}");
        assert!(!out.contains("r retry"), "{out}");
    }

    /// Issue #787: a `BranchMismatch` (the workspace's own leftover on
    /// another branch) is no Esc-only dead end — the modal offers a
    /// one-keypress recreate and `r` dispatches it.
    #[test]
    fn branch_mismatch_modal_offers_recreate() {
        let mut st = state();
        st.apply(WorktreeStep::WorktreeAdd, WorktreeStepStatus::Started);
        st.apply(
            WorktreeStep::WorktreeAdd,
            WorktreeStepStatus::Failed(
                "checkout_at: worktree /tmp/wt is checked out on branch 'issue-1-old', \
                 not the requested branch 'issue-1-new' — refusing to reuse it; preserve \
                 or switch that checkout, then retry"
                    .into(),
            ),
        );
        assert_eq!(st.recovery(), Some(WorktreeRecovery::BranchMismatch));
        let out = render(&mut WorktreeProgress::from_state(&st), 72, 20);
        assert!(out.contains('✗'), "{out}");
        assert!(out.contains("r recreate"), "recreate affordance: {out}");
        assert!(
            !out.contains("r retry"),
            "must not advertise a bare retry: {out}"
        );
        let mut comp = WorktreeProgress::from_state(&st);
        assert!(matches!(
            comp.on(&Event::Keyboard(KeyEvent::from(Key::Char('r')))),
            Some(Msg::WorktreeRecreate)
        ));
    }

    /// Issue #787: a `BranchHeldLive` failure offers a jump to the live
    /// session holding the branch; `g` dispatches it and `r` stays inert
    /// (recreating a live holder would be destructive).
    #[test]
    fn branch_held_live_modal_offers_jump() {
        let mut st = state();
        st.apply(WorktreeStep::WorktreeAdd, WorktreeStepStatus::Started);
        st.apply(
            WorktreeStep::WorktreeAdd,
            WorktreeStepStatus::Failed(
                "worktree: checkout_at: branch 'feat' is already checked out at \
                 /tmp/other — refusing to take it from another live worktree"
                    .into(),
            ),
        );
        assert_eq!(st.recovery(), Some(WorktreeRecovery::BranchHeldLive));
        let out = render(&mut WorktreeProgress::from_state(&st), 72, 20);
        assert!(out.contains("go to holder"), "jump affordance: {out}");
        let mut comp = WorktreeProgress::from_state(&st);
        assert!(matches!(
            comp.on(&Event::Keyboard(KeyEvent::from(Key::Char('g')))),
            Some(Msg::WorktreeJumpToHolder)
        ));
        assert!(
            comp.on(&Event::Keyboard(KeyEvent::from(Key::Char('r'))))
                .is_none(),
            "r must not recreate a live holder"
        );
    }

    /// The row estimate matches word wrapping, not a bare character
    /// count: a sentence whose words straddle the row boundary needs an
    /// extra row, and a token wider than the row is hard-split.
    #[test]
    fn wrapped_rows_counts_word_wrap_boundaries() {
        assert_eq!(wrapped_rows("", 10), 1);
        assert_eq!(wrapped_rows("short", 10), 1);
        // 12 chars would be ceil(12/10) = 2 by characters, and the word
        // boundary makes it 2 as well.
        assert_eq!(wrapped_rows("hello world!", 10), 2);
        // "aaaa bbbb cccc" — 14 chars: ceil = 2, but words of 4 with a
        // space ahead wrap as "aaaa bbbb" / "cccc" → 2; at width 9 the
        // first row holds "aaaa bbbb" exactly.
        assert_eq!(wrapped_rows("aaaa bbbb cccc", 9), 2);
        // Width 8 can't hold "aaaa bbbb": one word per row → 3, while
        // the character bound says 2.
        assert_eq!(wrapped_rows("aaaa bbbb cccc", 8), 3);
        // A single token wider than the row is hard-split.
        assert_eq!(wrapped_rows("/a/very/long/path/without/spaces", 10), 4);
        assert!(wrapped_rows("x", 0) >= 1);
    }

    /// The key hints survive a body taller than the screen. A real
    /// `BranchHeldLive` error names a ~100-char managed path in both the
    /// error and the remediation; in a 60-col modal on a short terminal
    /// that wrapped past the bottom and the `g go to holder · Esc
    /// dismiss` line — the only actionable one — was the line clipped,
    /// leaving a modal the user couldn't act on.
    #[test]
    fn key_hints_stay_visible_when_the_error_overflows_the_modal() {
        let holder = "/Users/someone/.lazybox/v2/github-acme-lodestar/\
                      issue-202-audit-query-parity-and-permission-model-across-both";
        let mut st = state();
        st.apply(WorktreeStep::WorktreeAdd, WorktreeStepStatus::Started);
        st.apply(
            WorktreeStep::WorktreeAdd,
            WorktreeStepStatus::Failed(format!(
                "worktree: checkout_new_branch_at: branch \
                 'issue-202-audit-query-parity-and-permission-model-across-both' is \
                 already checked out at {holder} — refusing to take it from another \
                 live worktree; join the live session that owns that checkout, or \
                 free an external checkout before retrying"
            )),
        );
        assert_eq!(st.recovery(), Some(WorktreeRecovery::BranchHeldLive));
        // 16 rows: shorter than the wrapped body.
        let out = render(&mut WorktreeProgress::from_state(&st), 80, 16);
        assert!(
            out.contains("go to holder"),
            "jump affordance clipped: {out}"
        );
        assert!(out.contains("Esc dismiss"), "dismiss hint clipped: {out}");
        assert!(
            out.contains("Creating worktree"),
            "checklist clipped: {out}"
        );
    }

    /// Issue #1041: an unmapped-Linear-team failure offers a repo pick
    /// (not a bare retry, which can't fix it); `r` dispatches the pick.
    #[test]
    fn unmapped_linear_team_modal_offers_a_repo_pick() {
        let mut st = state();
        st.apply(
            WorktreeStep::Fetch,
            WorktreeStepStatus::Failed(
                "workspace: Linear team `OBI` has no repo mapping and the ticket has no \
                 linked GitHub PR — set providers.linear.teams.OBI in ~/.lazybox/config.yaml"
                    .into(),
            ),
        );
        assert_eq!(st.recovery(), Some(WorktreeRecovery::LinearUnmapped));
        let out = render(&mut WorktreeProgress::from_state(&st), 72, 20);
        assert!(out.contains("r pick repo"), "pick-repo affordance: {out}");
        assert!(
            !out.contains("r retry"),
            "must not advertise a bare retry: {out}"
        );
        let mut comp = WorktreeProgress::from_state(&st);
        assert!(matches!(
            comp.on(&Event::Keyboard(KeyEvent::from(Key::Char('r')))),
            Some(Msg::WorktreePickRepo)
        ));
    }

    /// The `r` key only fires for a retryable failure — a stray `r`
    /// mid-provision must not intercept.
    #[test]
    fn retry_key_only_binds_on_the_failure_screen() {
        use tuirealm::event::{Key, KeyEvent};
        let mut st = state();
        st.apply(WorktreeStep::WorktreeAdd, WorktreeStepStatus::Started);
        // Mid-provision: `r` is inert.
        let mut live = WorktreeProgress::from_state(&st);
        assert!(
            live.on(&Event::Keyboard(KeyEvent::from(Key::Char('r'))))
                .is_none(),
            "r must not retry while provisioning is in flight"
        );
        // After a failure: `r` asks the model to retry.
        st.apply(
            WorktreeStep::WorktreeAdd,
            WorktreeStepStatus::Failed("boom".into()),
        );
        let mut failed = WorktreeProgress::from_state(&st);
        assert!(matches!(
            failed.on(&Event::Keyboard(KeyEvent::from(Key::Char('r')))),
            Some(Msg::WorktreeRetry)
        ));
    }
}
