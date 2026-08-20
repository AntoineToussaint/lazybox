//! `focus_header` — the slim event strip shown at the top of focus
//! mode (issue #156).
//!
//! Focus mode hides the sidebar and activity pane to give the coding
//! agent's terminal the whole window. This one-row strip is the only
//! chrome that survives: it names the workspace the terminal belongs
//! to and carries a live tally of incoming work — unread activity,
//! agents waiting on input, failing CI, pending reviews — so a
//! heads-down user keeps situational awareness. Actionable signals
//! (asking / CI) render in their alert colors; quiet ones stay dim.
//!
//! Pure render — every value is passed in from `Model::view`.

use crate::components::sidebar::AttentionSummary;
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, Borders, Paragraph};

/// Compose the focus-mode header title from a workspace's `name`, its
/// canonical `owner/repo` slug (if any), and its 1-based jump `number`
/// (its slot in the focused roster, `None` when unfocused).
///
/// Shape: `<n> · <name> (<repo>)` — the number prefix tells the user
/// which `]]<digit>` returns here, and the repo in parentheses restores
/// the context the hidden sidebar normally carries (issue #1118). Every
/// component is optional and collapses cleanly: a repo-less workspace
/// drops the `(<repo>)`, an unfocused one drops the `<n> · ` prefix.
pub fn compose_title(name: &str, repo: Option<&str>, number: Option<usize>) -> String {
    let labeled = match repo {
        Some(repo) => format!("{name} ({repo})"),
        None => name.to_string(),
    };
    match number {
        Some(i) => format!("{} · {labeled}", i + 1),
        None => labeled,
    }
}

/// Render the focus-mode header into `area` (a single row).
/// `title` names the workspace whose terminal is showing; `hint` is
/// the short keybinding reminder pinned to the right edge.
pub fn render(frame: &mut Frame, area: Rect, title: &str, summary: AttentionSummary, hint: &str) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let theme = crate::theme::current();
    let bg = Style::default().bg(theme.surface);

    // Background fill so the strip reads as chrome distinct from the
    // terminal body below it.
    frame.render_widget(Paragraph::new(Line::raw("")).style(bg), area);

    // ── Left: focus glyph + workspace title + attention counts ──────
    let mut left: Vec<Span> = vec![
        Span::styled(
            " ◆ ",
            Style::default()
                .bg(theme.surface)
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{title}  "),
            Style::default()
                .bg(theme.surface)
                .fg(theme.text_strong)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    for (count, glyph, label, color, bold) in [
        (summary.asking, "!", "asking", theme.warn, true),
        (summary.ci_failing, "✗", "CI", theme.error, true),
        (summary.unread, "●", "new", theme.accent, false),
        (summary.review_pending, "⟳", "review", theme.warn, false),
    ] {
        if count == 0 {
            continue;
        }
        let mut style = Style::default().bg(theme.surface).fg(color);
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        left.push(Span::styled(format!("{glyph} {count} {label}  "), style));
    }
    // Reserve the hint's columns on the right edge and render the left
    // run (title + attention counts) only into the remaining span, so
    // the two occupy *non-overlapping* regions. The previous code drew
    // the left run across the full width and then painted the hint on
    // top of it: correct only as long as the hint text exactly fills
    // its rect and the left run clips cleanly at the edge — a fragile,
    // draw-order-dependent coupling. Partitioning the row removes that
    // coupling so a long title (adding the repo per #1118 makes titles
    // longer) can never bleed into — or be garbled against — the hint.
    let hint_w = (hint.chars().count() as u16).saturating_add(1);
    let hint_fits = hint_w < area.width;
    let left_width = if hint_fits {
        area.width - hint_w
    } else {
        area.width
    };
    let left_area = Rect {
        width: left_width,
        ..area
    };
    frame.render_widget(Paragraph::new(Line::from(left)).style(bg), left_area);

    // ── Right: keybinding hint, right-aligned ───────────────────────
    if hint_fits {
        let hint_rect = Rect {
            x: area.x + area.width - hint_w,
            y: area.y,
            width: hint_w,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("{hint} "),
                Style::default().bg(theme.surface).fg(theme.text_dim),
            )))
            .style(bg),
            hint_rect,
        );
    }
}

/// Chrome for one focus-mode workspace pane (#1258), precomputed by
/// `Model::view` outside the draw closure (same borrow discipline as
/// the event header above). Pure data — the render fns below turn it
/// into a bordered pane frame.
pub struct FocusPaneChrome {
    /// Workspace display name; the placeholder text for an empty pane.
    pub title: String,
    /// 0-based slot in the starred roster (rendered 1-based) — the
    /// digit `]]<digit>` retargets this pane by. `None` when the
    /// workspace isn't starred (it entered via fallback or retarget).
    pub digit: Option<usize>,
    /// Compact agent-state badge (`● asking`, `· working`, …) with its
    /// alert style, straight from the terminal stack's vocabulary.
    pub badge: Option<(&'static str, Style)>,
    /// Whether this pane holds pane focus (accent border, input target).
    pub focused: bool,
    /// The terminal to render in the body; `None` renders the dim
    /// placeholder instead.
    pub terminal: Option<lazybox_ipc::TerminalId>,
    /// Body text when `terminal` is `None` — the star nudge for an
    /// unassigned pane, a no-terminal note for an assigned workspace
    /// whose session has nothing running.
    pub placeholder: String,
}

/// Draw one focus-pane frame: a bordered block whose top line carries
/// the pane header — star digit + workspace name + agent-state badge —
/// with the accent border marking the focused (input-receiving) pane.
/// Returns the inner rect the pane body (terminal or placeholder)
/// renders into.
pub fn render_pane_frame(frame: &mut Frame, area: Rect, chrome: &FocusPaneChrome) -> Rect {
    if area.width < 2 || area.height < 2 {
        return Rect::default();
    }
    let theme = crate::theme::current();
    let border_style = if chrome.focused {
        Style::default().fg(theme.accent)
    } else {
        Style::default().fg(theme.chrome)
    };
    let mut title: Vec<Span> = Vec::new();
    if let Some(d) = chrome.digit {
        title.push(Span::styled(
            format!(" {} ", d + 1),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    } else {
        title.push(Span::raw(" "));
    }
    title.push(Span::styled(
        format!("{} ", chrome.title),
        if chrome.focused {
            Style::default()
                .fg(theme.text_strong)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme.text_dim)
        },
    ));
    if let Some((label, style)) = chrome.badge {
        title.push(Span::styled(format!("{label} "), style));
    }
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Line::from(title));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    inner
}

/// Body of a pane with nothing to show: a dim centered nudge naming
/// the way to fill it (star a workspace, then `]]<digit>`).
pub fn render_pane_placeholder(frame: &mut Frame, inner: Rect, text: &str) {
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let theme = crate::theme::current();
    let row = Rect {
        y: inner.y + inner.height / 2,
        height: 1,
        ..inner
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            text.to_string(),
            Style::default().fg(theme.text_dim),
        )))
        .alignment(Alignment::Center),
        row,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuirealm::ratatui::Terminal;
    use tuirealm::ratatui::backend::TestBackend;

    fn render_to_string(title: &str, summary: AttentionSummary, hint: &str) -> String {
        let mut term = Terminal::new(TestBackend::new(70, 1)).unwrap();
        term.draw(|f| render(f, Rect::new(0, 0, 70, 1), title, summary, hint))
            .unwrap();
        let buf = term.backend().buffer().clone();
        (0..buf.area.width)
            .map(|x| buf[(x, 0)].symbol())
            .collect::<String>()
    }

    #[test]
    fn shows_title_and_nonzero_counts() {
        let summary = AttentionSummary {
            unread: 3,
            asking: 1,
            ci_failing: 0,
            review_pending: 2,
        };
        let line = render_to_string("fix-the-thing", summary, "]]q exit");
        assert!(line.contains("fix-the-thing"), "title missing: {line:?}");
        assert!(line.contains("1 asking"), "asking count missing: {line:?}");
        assert!(line.contains("3 new"), "unread count missing: {line:?}");
        assert!(line.contains("2 review"), "review count missing: {line:?}");
        // ci_failing is zero, so its segment is omitted entirely.
        assert!(!line.contains("CI"), "zero CI should not render: {line:?}");
        assert!(line.contains("]]q exit"), "hint missing: {line:?}");
    }

    #[test]
    fn compose_title_appends_repo_in_parentheses() {
        // issue #1118: the repo rides in parentheses after the name.
        assert_eq!(
            compose_title("fix-the-thing", Some("obin-ai/lazybox"), None),
            "fix-the-thing (obin-ai/lazybox)"
        );
    }

    #[test]
    fn compose_title_keeps_jump_number_prefix_before_repo() {
        assert_eq!(
            compose_title("fix-the-thing", Some("obin-ai/lazybox"), Some(2)),
            "3 · fix-the-thing (obin-ai/lazybox)"
        );
    }

    #[test]
    fn compose_title_drops_parens_when_repo_less() {
        assert_eq!(compose_title("scratch", None, None), "scratch");
        assert_eq!(compose_title("scratch", None, Some(0)), "1 · scratch");
    }

    #[test]
    fn repo_renders_on_the_focus_strip() {
        let line = render_to_string(
            "fix-the-thing (obin-ai/lazybox)",
            AttentionSummary::default(),
            "]]q exit",
        );
        assert!(
            line.contains("fix-the-thing (obin-ai/lazybox)"),
            "repo missing from strip: {line:?}"
        );
    }

    #[test]
    fn hint_and_left_run_never_overlap() {
        // A long title (name + repo, per #1118) plus attention counts
        // competes with the right-edge hint for columns. The row is
        // partitioned into non-overlapping regions, so the hint must
        // render fully and intact at the right edge no matter how long
        // the left run is — never garbled by title/count characters
        // bleeding into it.
        let summary = AttentionSummary {
            asking: 1,
            ci_failing: 2,
            ..AttentionSummary::default()
        };
        let hint = "]]<n> jump · ]]q exit";
        let line = render_to_string(
            "a-fairly-long-workspace-name (some-owner/some-long-repo)",
            summary,
            hint,
        );
        // The hint occupies the rightmost columns, intact and contiguous.
        assert!(
            line.trim_end().ends_with(hint),
            "hint must render intact at the right edge: {line:?}"
        );
        // It appears exactly once — the left run did not spill a stray
        // copy of any hint fragment into the title region.
        assert_eq!(
            line.matches(hint).count(),
            1,
            "hint must appear exactly once: {line:?}"
        );
        // The left region still shows the start of the title.
        assert!(
            line.contains("a-fairly-long-workspace-name"),
            "title prefix missing: {line:?}"
        );
    }

    #[test]
    fn quiet_workspace_shows_only_title_and_hint() {
        let line = render_to_string("scratch", AttentionSummary::default(), "]]q exit");
        assert!(line.contains("scratch"));
        assert!(line.contains("]]q exit"));
        assert!(!line.contains("new"));
        assert!(!line.contains("asking"));
    }
}
