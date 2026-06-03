//! # lazybox-tui-term
//!
//! Embedded terminal widget for ratatui, powered by libghostty-vt.

mod ghostty_widget;
mod session;

pub use ghostty_widget::GhosttyTerminal;
pub use portable_pty::PtySize;
pub use session::{TermError, TermSession};

/// Minimal terminal lifecycle interface — enough for `TerminalManager` to
/// drive the session's liveness and pending-output pump without coupling
/// to the concrete `TermSession` type. This lets tests substitute a fake.
///
/// Intentionally NOT `Send` — libghostty-vt holds raw pointers that the
/// Rust type system treats as !Send. All terminal work happens on the main
/// thread, so the bound would be wrong.
pub trait Terminal {
    /// The reader thread has reached EOF (child process exited).
    fn is_finished(&self) -> bool;
    /// Drain any buffered PTY bytes into the VT parser.
    fn process_pending(&mut self);
}

impl Terminal for TermSession {
    fn is_finished(&self) -> bool {
        TermSession::is_finished(self)
    }
    fn process_pending(&mut self) {
        let _ = TermSession::process_pending(self);
    }
}

/// Render a terminal session into a ratatui frame.
pub fn render_to_frame(
    term: &mut TermSession,
    frame: &mut ratatui::Frame,
    area: ratatui::prelude::Rect,
) {
    let (terminal, render_state, row_iter, cell_iter, shadow) = term.render_data();
    if let Ok(snapshot) = render_state.update(terminal) {
        let widget = GhosttyTerminal::new(&snapshot, row_iter, cell_iter, shadow);
        frame.render_widget(widget, area);
    }
}
