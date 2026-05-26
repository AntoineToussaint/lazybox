//! Activity-card rendering primitives + the `RightSection` / `CardState`
//! types they share. Pure functions: build `Vec<Line>` from a single
//! `Activity` + state flags. No mutable state, no I/O.

use super::*;

#[allow(dead_code)]
pub(crate) enum RightSection {
    Activity,
}

/// Pure per-card state computed once at the top of the activity
/// render loop. Holds only the booleans the header / body renderers
/// need; passing the workspace + cursor + sets around as positional
/// args was the source of the click-hit-test bug (one branch
/// recorded the hit, the `continue` branch didn't).
#[derive(Debug, Clone, Copy)]
pub(crate) struct CardState {
    pub(crate) is_cursor: bool,
    pub(crate) is_unread: bool,
    pub(crate) is_expanded: bool,
    pub(crate) is_selected: bool,
    pub(crate) focused: bool,
}

impl CardState {
    /// "Should this row dim to text_dim across the byline?" — read
    /// rows do, unless the focused cursor is sitting on them.
    pub(crate) fn dim_byline(&self) -> bool {
        !self.is_unread && !(self.is_cursor && self.focused)
    }

    /// Marker-bar color: focused cursor > unread > chrome.
    fn bar_color(&self, theme: &crate::theme::Theme) -> ratatui::style::Color {
        if self.is_cursor && self.focused {
            theme.accent
        } else if self.is_unread {
            theme.warn
        } else {
            theme.chrome
        }
    }
}

/// Build the header line for a single activity card. Pure: data in,
/// `Line` out, no state mutation. Separated from the render loop so
/// the hit-test push can be a single unconditional statement and
/// each visual element gets its own helper.
pub(crate) fn render_card_header(
    state: &CardState,
    activity: &pilot_core::Activity,
    theme: &crate::theme::Theme,
    now: chrono::DateTime<chrono::Utc>,
    teaser_cells: usize,
    viewer_logins: &std::collections::HashMap<String, String>,
) -> Line<'static> {
    use pilot_core::ActivityKind;
    let (kind_icon, kind_label) = match activity.kind {
        ActivityKind::Comment => (crate::components::icons::COMMENT, "Message"),
        ActivityKind::Review => (crate::components::icons::REVIEW, "Review"),
        ActivityKind::StatusChange => (crate::components::icons::STATUS_CHANGE, "Status"),
        ActivityKind::CiUpdate => (crate::components::icons::CI, "CI"),
    };

    let header_style = if state.is_cursor && state.focused {
        theme.row_focused()
    } else if state.is_unread {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(theme.text_dim)
    };
    let kind_style = Style::default().fg(theme.text_dim);
    let teaser_style = if state.dim_byline() {
        Style::default().fg(theme.chrome)
    } else {
        Style::default().fg(theme.text_dim)
    };

    let mut spans: Vec<Span<'static>> = Vec::with_capacity(10);

    // Unread bullet — reserves the slot so toggling unread doesn't
    // shift columns.
    spans.push(if state.is_unread {
        Span::styled(
            "● ",
            Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("  ")
    });

    // Cursor caret OR plain bar.
    let bar_glyph = if state.is_cursor && state.focused {
        if state.is_expanded { "▾ " } else { "▸ " }
    } else {
        "│ "
    };
    spans.push(Span::styled(
        bar_glyph,
        Style::default()
            .fg(state.bar_color(theme))
            .add_modifier(Modifier::BOLD),
    ));

    // Multi-select ✓ — also reserves its slot to avoid jitter.
    spans.push(if state.is_selected {
        Span::styled(
            "✓ ",
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::raw("  ")
    });

    spans.push(Span::styled(
        format!("{kind_icon}  {kind_label}  "),
        kind_style,
    ));
    // Replace the bare author login with `@me` when it matches any
    // viewer identity (one per provider source — github, linear, …).
    // Single source of truth so the same convention works on PRs
    // I authored AND PRs I just review.
    let author_display = if viewer_logins
        .values()
        .any(|login| login == &activity.author)
    {
        "@me".to_string()
    } else {
        activity.author.clone()
    };
    spans.push(Span::styled(author_display, header_style));

    let ts = crate::components::sidebar::relative_time(activity.created_at, now);
    spans.push(Span::styled(
        format!("  {ts}"),
        Style::default().fg(theme.text_dim),
    ));

    // Inline teaser only when the card is collapsed — expanded
    // cards show the full body on subsequent lines.
    if !state.is_expanded {
        let teaser = teaser_text(&activity.body, teaser_cells);
        if !teaser.is_empty() {
            spans.push(Span::styled("  ›  ", Style::default().fg(theme.chrome)));
            spans.push(Span::styled(teaser, teaser_style));
        }
    }

    Line::from(spans)
}

/// Build the body lines for an expanded card. The markdown→ratatui
/// rendering lives in `comment_render::render_body`; this helper
/// just wraps each body line with the indented marker-bar prefix.
pub(crate) fn render_card_body(
    activity: &pilot_core::Activity,
    theme: &crate::theme::Theme,
    state: &CardState,
    body_width: u16,
    body_indent: u16,
) -> Vec<Line<'static>> {
    let bar_color = state.bar_color(theme);
    let body_lines =
        crate::components::comment_render::render_body(&activity.body, body_width, usize::MAX);
    body_lines
        .into_iter()
        .map(|line| {
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 2);
            spans.push(Span::styled("│ ", Style::default().fg(bar_color)));
            spans.push(Span::raw(" ".repeat((body_indent - 2) as usize)));
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
}
