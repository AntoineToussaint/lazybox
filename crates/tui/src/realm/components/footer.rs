//! `Footer` — single-row status line at the bottom of the screen.
//!
//! Three zones, all on one row:
//!
//! - **Left**: contextual hints for the focused pane — only actions
//!   that are actually wired up *right now*. The list is built by
//!   each pane from the action catalog (`lazybox_tui_core::action`),
//!   so a binding shown here is — by construction — a binding the
//!   pane will dispatch. A short, pane-independent set of universal
//!   hints (`?` help, `Shift-T` tour, `q q` quit) is appended after
//!   it so a first-time user can always find the way out and the way
//!   to orient (issue #100) — the rest of the keymap still lives in
//!   the `?` help modal and the tour.
//! - **Right**: background polling status — spinner + "Pulling
//!   tasks from github · PR query: …" — OR the most recent notice /
//!   error if one is set. Retryable hiccups auto-fade; permanent +
//!   auth errors stay until dismissed. Capped at ~40% of the row
//!   with `…` truncation — the hints are the footer's primary
//!   content and a notice must never displace them (#291).
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
    globals: &[Binding],
    polling_status: Option<(&str, &str)>, // (spinner, label)
    notice: Option<&Notice>,
) {
    let theme = crate::theme::current();

    // Background fill so the line stands out.
    let bg = Style::default().bg(theme.surface);
    f.render_widget(Paragraph::new(Line::raw("")).style(bg), area);

    // Reserve the right-most segment for the notice (or polling
    // status if no notice). Keymap fills the rest of the line. The
    // hints are the footer's primary content, so the right segment is
    // capped at ~40% of the row and its message ellipsis-truncated —
    // agent notices used to interpolate full issue titles and wipe
    // out the hint zone entirely (#291).
    let right_cap = (area.width as usize) * 2 / 5;
    let right_text = if let Some(n) = notice {
        let sev_color = match n.severity {
            NoticeSeverity::Retryable | NoticeSeverity::Auth => theme.warn,
            NoticeSeverity::Permanent => theme.error,
            NoticeSeverity::Info | NoticeSeverity::Hint => theme.text_dim,
        };
        // 4 cells of chrome: the two flanking pads + the pill's
        // inner spaces.
        let message = crate::util::truncate_ellipsis(&n.message, right_cap.saturating_sub(4));
        Some(Line::from(vec![
            Span::styled(" ", bg),
            Span::styled(
                format!(" {message} "),
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
        let chrome = crate::util::visual_width(spinner) + 4;
        let label = crate::util::truncate_ellipsis(label, right_cap.saturating_sub(chrome));
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

    // Left zone: focused-pane contextual bindings, then a short
    // pane-independent tail of universal hints (`globals`). The
    // contextual list is the state-aware "what's actionable right
    // now" guarantee from issue #25; the globals tail (issue #100)
    // re-adds the handful of shortcuts a lost first-time user needs
    // to always see — chiefly how to quit. The rest of the keymap
    // still lives in the `?` help modal + the tour.
    let mut spans: Vec<Span> = Vec::with_capacity((keymap.len() + globals.len()) * 4 + 3);
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
    // Globals tail. A wider gap separates it from the contextual
    // group so the two read as distinct clusters rather than one run.
    for (i, b) in globals.iter().enumerate() {
        if i == 0 && !keymap.is_empty() {
            spans.push(Span::styled("     ", bg));
        } else if i > 0 {
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
    use super::*;
    use crate::pane::Binding;
    use std::borrow::Cow;
    use tuirealm::ratatui::Terminal;
    use tuirealm::ratatui::backend::TestBackend;

    /// Render the footer to a flat string of its single row so tests
    /// can assert which hints surfaced.
    fn render_row(keymap: &[Binding], globals: &[Binding]) -> String {
        render_row_full(keymap, globals, None, None)
    }

    fn render_row_full(
        keymap: &[Binding],
        globals: &[Binding],
        polling_status: Option<(&str, &str)>,
        notice: Option<&Notice>,
    ) -> String {
        let w = 120u16;
        let backend = TestBackend::new(w, 1);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            render(
                f,
                Rect::new(0, 0, w, 1),
                keymap,
                globals,
                polling_status,
                notice,
            );
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        (0..w).map(|x| buf[(x, 0)].symbol()).collect()
    }

    fn binding(keys: &'static str, label: &'static str) -> Binding {
        Binding {
            keys: Cow::Borrowed(keys),
            label: Cow::Borrowed(label),
        }
    }

    #[test]
    fn globals_tail_renders_after_contextual() {
        // The universal hints (issue #100) must show even when the
        // pane offers contextual ones — chiefly so quit is findable.
        let keymap = [binding("w", "work on this")];
        let globals = [binding("?", "help"), binding("q q", "quit")];
        let row = render_row(&keymap, &globals);
        assert!(row.contains("work on this"), "contextual hint missing");
        assert!(row.contains("help"), "global help hint missing");
        assert!(row.contains("q q"), "quit chord missing from footer");
        assert!(row.contains("quit"), "quit label missing from footer");
        // Globals come after the contextual group.
        assert!(row.find("work on this") < row.find("quit"));
    }

    #[test]
    fn globals_render_with_no_contextual_hints() {
        // Empty-inbox / first-run case: no workspace selected means a
        // near-empty contextual list, but quit must still be visible.
        let globals = [binding("q q", "quit")];
        let row = render_row(&[], &globals);
        assert!(row.contains("quit"));
    }

    /// A 200-char notice at 120 cols must not displace the hints:
    /// the contextual group and the universal tail stay on the row,
    /// and the notice is visibly truncated (#291).
    #[test]
    fn long_notice_never_displaces_hints() {
        let keymap = [binding("w", "work on this")];
        let globals = [binding("?", "help"), binding("q q", "quit")];
        let notice = Notice::new("t".repeat(200), NoticeSeverity::Info);
        let row = render_row_full(&keymap, &globals, None, Some(&notice));
        assert!(row.contains("work on this"), "contextual hint displaced");
        assert!(row.contains("help"), "global help hint displaced");
        assert!(row.contains("q q"), "quit chord displaced");
        assert!(row.contains("quit"), "quit label displaced");
        assert!(row.contains('…'), "long notice must truncate visibly");
        // The notice segment stays within its ~40% budget (48 cells
        // at 120 cols): no run of notice text longer than the cap.
        assert!(
            !row.contains(&"t".repeat(49)),
            "notice segment exceeded its cap",
        );
    }

    /// Same guarantee for the polling status — the right segment is
    /// capped no matter which variant fills it.
    #[test]
    fn long_polling_label_never_displaces_hints() {
        let keymap = [binding("w", "work on this")];
        let globals = [binding("?", "help"), binding("q q", "quit")];
        let label = "p".repeat(200);
        let row = render_row_full(&keymap, &globals, Some(("⠋", &label)), None);
        assert!(row.contains("work on this"), "contextual hint displaced");
        assert!(row.contains("quit"), "quit label displaced");
        assert!(row.contains('…'), "long label must truncate visibly");
    }

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
