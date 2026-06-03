//! `Footer` — single-row status line at the bottom of the screen.
//!
//! Three zones, all on one row:
//!
//! - **Left**: contextual hints for the focused pane — only actions
//!   that are actually wired up *right now*. The list is built by
//!   each pane from the action catalog (`lazybox_tui_core::action`),
//!   so a binding shown here is — by construction — a binding the
//!   pane will dispatch. Universal shortcuts (`?` help, `q q` quit,
//!   etc.) live in the onboarding tour and the `?` help modal, not
//!   here.
//! - **Right**: background polling status — spinner + "Pulling
//!   tasks from github · PR query: …" — OR the most recent notice /
//!   error if one is set. Retryable hiccups auto-fade; permanent +
//!   auth errors stay until dismissed.
//!
//! Pure render — state lives on `Model` and gets passed in.

use crate::pane::Binding;
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::Paragraph;

/// Severity of a footer notice — drives its color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeSeverity {
    /// Transient hiccup. `theme.warn`. Auto-fades.
    Retryable,
    /// Auth or other actionable error. `theme.warn`. Sticky.
    Auth,
    /// Hard failure. `theme.error`. Sticky.
    Permanent,
    /// Plain informational. `theme.text_dim`. 15s fade.
    Info,
    /// Brief one-shot hint (e.g. "scroll: alt-screen") — dim, 3s
    /// fade. Same color as Info but a much shorter lifetime so
    /// repeated triggers don't follow the user around the UI.
    Hint,
}

/// One footer notice — message + severity + when it was set
/// (for auto-fade).
#[derive(Debug, Clone)]
pub struct Notice {
    pub message: String,
    pub severity: NoticeSeverity,
    pub set_at: std::time::Instant,
}

impl Notice {
    pub fn new(message: impl Into<String>, severity: NoticeSeverity) -> Self {
        Self {
            message: message.into(),
            severity,
            set_at: std::time::Instant::now(),
        }
    }
}

/// Pure render. The orchestrator passes in everything Footer needs:
/// the focused pane's keymap, the optional polling status, and the
/// optional notice. Returns nothing — paints directly.
pub fn render(
    f: &mut Frame,
    area: Rect,
    keymap: &[Binding],
    polling_status: Option<(&str, &str)>, // (spinner, label)
    notice: Option<&Notice>,
) {
    let theme = crate::theme::current();

    // Background fill so the line stands out.
    let bg = Style::default().bg(theme.surface);
    f.render_widget(Paragraph::new(Line::raw("")).style(bg), area);

    // Reserve the right-most segment for the notice (or polling
    // status if no notice). Keymap fills the rest of the line.
    let right_text = if let Some(n) = notice {
        let sev_color = match n.severity {
            NoticeSeverity::Retryable | NoticeSeverity::Auth => theme.warn,
            NoticeSeverity::Permanent => theme.error,
            NoticeSeverity::Info | NoticeSeverity::Hint => theme.text_dim,
        };
        Some(Line::from(vec![
            Span::styled(" ", bg),
            Span::styled(
                format!(" {} ", n.message),
                Style::default()
                    .bg(sev_color)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", bg),
        ]))
    } else if let Some((spinner, label)) = polling_status {
        // Two-tone render: bright accent for the spinner glyph
        // (drives the eye), dim text for the surrounding label so
        // the indicator stays visible without dominating the bar.
        // The source name itself ("github" / "linear") is the only
        // word in the label the user cares about, so we don't try
        // to highlight it separately — at 1-2 characters of width
        // delta it would look like a typo.
        Some(Line::from(vec![
            Span::styled(
                format!(" {spinner} "),
                Style::default()
                    .bg(theme.surface)
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{label} "),
                Style::default().bg(theme.surface).fg(theme.text_strong),
            ),
            Span::styled(" ", bg),
        ]))
    } else {
        None
    };

    let right_width = right_text.as_ref().map(|l| l.width() as u16).unwrap_or(0);
    let right_rect = Rect {
        x: area.x + area.width.saturating_sub(right_width),
        y: area.y,
        width: right_width.min(area.width),
        height: 1,
    };
    let left_rect = Rect {
        x: area.x,
        y: area.y,
        width: area.width.saturating_sub(right_width),
        height: 1,
    };

    // Left zone: focused-pane contextual bindings only. Universal
    // shortcuts (`?` help, `q q` quit, `,` settings, `Tab` cycle,
    // resize, detach) used to be hardcoded into a separate "globals"
    // block on the right side of the line — that's now in the
    // onboarding tour + the `?` help modal, where it gets shown once
    // instead of cluttering every screen. Keeping the footer to
    // contextual actions only is the structural guarantee from
    // issue #25: every hint here is, by construction, an action the
    // pane will actually dispatch.
    let mut spans: Vec<Span> = Vec::with_capacity(keymap.len() * 4 + 2);
    spans.push(Span::styled(" ", bg));
    let key_style = Style::default()
        .bg(theme.surface)
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default().bg(theme.surface).fg(theme.text_dim);
    let sep_style = Style::default().bg(theme.surface).fg(theme.chrome);
    for (i, b) in keymap.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("  ·  ", sep_style));
        }
        spans.push(Span::styled(compact_key(&b.keys), key_style));
        spans.push(Span::styled(" ", bg));
        spans.push(Span::styled(b.label.clone(), label_style));
    }
    f.render_widget(Paragraph::new(Line::from(spans)).style(bg), left_rect);

    if let Some(line) = right_text {
        f.render_widget(Paragraph::new(line).style(bg), right_rect);
    }
}

/// Compact display for footer hints — `Shift-X` → `X`, `Ctrl-Q` →
/// `^Q`, anything else (`q q`, `Tab`, `↑/↓`) as-is. Single source
/// of truth for binding specs stays explicit (so the `?` help modal
/// shows the full chord); the footer renderer just chooses a tighter
/// display form so the line doesn't get jagged with `Shift-` prefixes.
///
/// Returns `Cow` so the pass-through case (most rows) doesn't
/// allocate — the footer redraws on every state change and this is a
/// hot path.
fn compact_key(keys: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if let Some(rest) = keys.strip_prefix("Shift-") {
        // `Shift-X` where X is one ASCII letter → uppercase letter
        // alone (standard Unix convention: uppercase = shifted).
        let mut iter = rest.chars();
        if let (Some(c), None) = (iter.next(), iter.next())
            && c.is_ascii_alphabetic()
        {
            return Cow::Owned(c.to_ascii_uppercase().to_string());
        }
    }
    if let Some(rest) = keys.strip_prefix("Ctrl-") {
        let mut iter = rest.chars();
        if let (Some(c), None) = (iter.next(), iter.next())
            && c.is_ascii_alphabetic()
        {
            return Cow::Owned(format!("^{}", c.to_ascii_uppercase()));
        }
    }
    Cow::Borrowed(keys)
}

#[cfg(test)]
mod tests {
    use super::compact_key;

    #[test]
    fn shift_letter_collapses_to_uppercase() {
        assert_eq!(compact_key("Shift-X"), "X");
        assert_eq!(compact_key("Shift-N"), "N");
        assert_eq!(compact_key("Shift-m"), "M"); // lowercase rest still uppercased
    }

    #[test]
    fn ctrl_letter_collapses_to_caret_form() {
        assert_eq!(compact_key("Ctrl-Q"), "^Q");
        assert_eq!(compact_key("Ctrl-c"), "^C");
    }

    #[test]
    fn single_char_keys_pass_through() {
        assert_eq!(compact_key("w"), "w");
        assert_eq!(compact_key("?"), "?");
    }

    #[test]
    fn multi_token_keys_pass_through() {
        // `q q` is a chord — leave as-is so the user sees they need
        // to double-tap.
        assert_eq!(compact_key("q q"), "q q");
        assert_eq!(compact_key("↑/↓"), "↑/↓");
        assert_eq!(compact_key("Shift-PgUp/Dn"), "Shift-PgUp/Dn");
    }

    /// Non-letter shifted keys (e.g. `Shift-Tab`) keep the prefix —
    /// just letters get the uppercase-alone treatment.
    #[test]
    fn shifted_non_letter_keeps_prefix() {
        assert_eq!(compact_key("Shift-Tab"), "Shift-Tab");
    }
}
