//! Lazybox-flavored extras on top of `crate::theme`.
//!
//! The kit ships a generic palette + the `Theme` slots; lazybox adds
//! its own state-pill mapping (PR open/closed/merged → bg/fg color
//! pair), conventional-commit kind colors, etc. Lives here rather
//! than inside the kit so the kit stays domain-free.

use crate::theme::Theme;
use ratatui::style::Color;

/// PR-state buckets the lazybox UI's state pill knows about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatePill {
    /// Open PR.
    Open,
    /// Draft PR.
    Draft,
    /// Merged PR.
    Merged,
    /// Closed (not merged).
    Closed,
    /// Work-in-progress (linked issue).
    InProgress,
    /// In review.
    InReview,
}

/// `(bg, fg)` color pair for the pill keyed by PR state, drawn from
/// the active theme. Lazybox's right-pane header renders these as
/// powerline-style segments.
pub fn state_pill(theme: &Theme, kind: StatePill) -> (Color, Color) {
    match kind {
        StatePill::Open => (theme.success, Color::Black),
        StatePill::Draft => (theme.chrome, theme.text_strong),
        StatePill::Merged => (theme.hover, Color::Black),
        StatePill::Closed => (theme.error, Color::Black),
        StatePill::InProgress => (theme.warn, Color::Black),
        StatePill::InReview => (theme.warn, Color::Black),
    }
}
