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
//!   the `?` help modal and the tour. The run is measured against
//!   the zone's width and elided by whole cells — universal tail
//!   surviving first, then contextual hints in order — with a dim
//!   `… +N` overflow cell instead of mid-label clipping (#303).
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
        let pill = |msg: &str| {
            Line::from(vec![
                Span::styled(" ", bg),
                Span::styled(
                    format!(" {msg} "),
                    Style::default()
                        .bg(sev_color)
                        .fg(Color::Black)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(" ", bg),
            ])
        };
        // The chrome (pads + pill spaces) is measured off the layout
        // itself so a padding tweak can't silently desync the budget.
        // Middle truncation, not end: notice tails carry the
        // actionable part ("… — press ! to jump", an error reason
        // after its fixed prefix), so cutting from the end would
        // delete exactly what the message exists to deliver.
        let chrome = pill("").width();
        let message =
            crate::util::truncate_ellipsis_middle(&n.message, right_cap.saturating_sub(chrome));
        Some(pill(&message))
    } else if let Some((spinner, label)) = polling_status {
        // Two-tone render: bright accent for the spinner glyph
        // (drives the eye), dim text for the surrounding label so
        // the indicator stays visible without dominating the bar.
        // The source name itself ("github" / "linear") is the only
        // word in the label the user cares about, so we don't try
        // to highlight it separately — at 1-2 characters of width
        // delta it would look like a typo.
        let status = |label: &str| {
            Line::from(vec![
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
            ])
        };
        let chrome = status("").width();
        let label = crate::util::truncate_ellipsis(label, right_cap.saturating_sub(chrome));
        Some(status(&label))
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
    //
    // The row is measured before it renders: whole hint cells are
    // elided when the budget runs out, never clipped mid-label
    // (#303). Survival priority is the reverse of render order —
    // the globals tail first (rightmost = the escape hatches that
    // exist precisely for the worst case), most-important-last
    // within it, then contextual hints in catalog order. When
    // anything drops, a dim `… +N` cell ends the bar so the user
    // can tell "that's all" from "the rest fell off the edge".
    let budget = left_rect.width as usize;
    let cell_width = |b: &Binding| {
        crate::util::visual_width(&compact_key(&b.keys)) + 1 + crate::util::visual_width(&b.label)
    };
    // "  ·  " between cells; also the contextual → globals gap width.
    const SEP_W: usize = 5;
    let total = keymap.len() + globals.len();
    let mut kept_ctx = keymap.len();
    let mut kept_glob = globals.len();
    loop {
        let dropped = total - kept_ctx - kept_glob;
        let mut w = 1; // leading pad
        if kept_ctx > 0 {
            w += keymap[..kept_ctx].iter().map(cell_width).sum::<usize>() + SEP_W * (kept_ctx - 1);
        }
        if kept_glob > 0 {
            if kept_ctx > 0 {
                w += SEP_W;
            }
            w += globals[globals.len() - kept_glob..]
                .iter()
                .map(cell_width)
                .sum::<usize>()
                + SEP_W * (kept_glob - 1);
        }
        if dropped > 0 {
            if kept_ctx + kept_glob > 0 {
                w += SEP_W;
            }
            w += crate::util::visual_width(&format!("… +{dropped}"));
        }
        if w <= budget || kept_ctx + kept_glob == 0 {
            break;
        }
        if kept_ctx > 0 {
            kept_ctx -= 1;
        } else {
            kept_glob -= 1;
        }
    }
    let dropped = total - kept_ctx - kept_glob;
    let keymap = &keymap[..kept_ctx];
    let globals = &globals[globals.len() - kept_glob..];

    let mut spans: Vec<Span> = Vec::with_capacity((keymap.len() + globals.len()) * 4 + 5);
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
    if dropped > 0 {
        if !keymap.is_empty() || !globals.is_empty() {
            spans.push(Span::styled("  ·  ", sep_style));
        }
        spans.push(Span::styled(format!("… +{dropped}"), label_style));
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
        render_row_at(120, keymap, globals, polling_status, notice)
    }

    fn render_row_at(
        w: u16,
        keymap: &[Binding],
        globals: &[Binding],
        polling_status: Option<(&str, &str)>,
        notice: Option<&Notice>,
    ) -> String {
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

    /// A hint-rich contextual keymap like the sidebar's, with
    /// labels whose 4-char heads appear in no other label so partial
    /// rendering is detectable.
    fn rich_keymap() -> Vec<Binding> {
        vec![
            binding("w", "work on this"),
            binding("Enter", "open activity"),
            binding("g m", "merge pull request"),
            binding("Shift-V", "manage reviewers"),
            binding("z", "snooze until later"),
            binding("Shift-X", "delete forever"),
            binding("/", "filter by role"),
        ]
    }

    fn globals_tail() -> Vec<Binding> {
        vec![
            binding("?", "help"),
            binding("Shift-T", "tour"),
            binding("q q", "quit"),
        ]
    }

    /// Every hint label must appear whole or not at all — a cell
    /// that shows its first characters but not the rest was clipped
    /// mid-label, exactly what measurement is supposed to prevent.
    fn assert_no_partial_labels(row: &str, bindings: &[Binding]) {
        for b in bindings {
            let head: String = b.label.chars().take(4).collect();
            assert!(
                row.contains(b.label.as_ref()) || !row.contains(&head),
                "label {:?} clipped mid-word in {row:?}",
                b.label,
            );
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
        let notice = Notice::new("x".repeat(200), NoticeSeverity::Info);
        let row = render_row_full(&keymap, &globals, None, Some(&notice));
        assert!(row.contains("work on this"), "contextual hint displaced");
        assert!(row.contains("help"), "global help hint displaced");
        assert!(row.contains("q q"), "quit chord displaced");
        assert!(row.contains("quit"), "quit label displaced");
        assert!(row.contains('…'), "long notice must truncate visibly");
        // The notice segment stays within its ~40% budget: message
        // cells ≤ right_cap - 4 chrome = 44 at 120 cols. Middle
        // truncation splits the payload around the '…', so bound the
        // total ('x' appears in no hint label), not a single run.
        let shown = row.matches('x').count();
        assert!(shown <= 44, "notice text exceeded its cap: {shown} cells");
    }

    /// The tail of an agent notice is its payload — the composed
    /// "<slug> needs input — press ! to jump" exceeds the cap at 120
    /// cols, and end-truncation used to delete the jump instruction.
    /// Middle truncation must keep it.
    #[test]
    fn notice_actionable_tail_survives_truncation() {
        let keymap = [binding("w", "work on this")];
        let globals = [binding("?", "help"), binding("q q", "quit")];
        let msg = format!("{}… needs input — press ! to jump", "T".repeat(23));
        let notice = Notice::new(msg, NoticeSeverity::Hint);
        let row = render_row_full(&keymap, &globals, None, Some(&notice));
        assert!(
            row.contains("! to jump"),
            "actionable tail truncated away: {row:?}",
        );
        assert!(row.contains("work on this"), "contextual hint displaced");
    }

    /// The cap is enforced in terminal cells, not chars: a CJK/emoji
    /// issue title renders two cells per char, and a char-budgeted
    /// truncation would let the pill spill to ~80% of the row and
    /// displace the hints all over again.
    #[test]
    fn wide_char_notice_stays_within_cap() {
        let keymap = [binding("w", "work on this")];
        let globals = [binding("?", "help"), binding("q q", "quit")];
        let notice = Notice::new("好".repeat(100), NoticeSeverity::Info);
        let row = render_row_full(&keymap, &globals, None, Some(&notice));
        assert!(row.contains("work on this"), "contextual hint displaced");
        assert!(row.contains("help"), "global help hint displaced");
        assert!(row.contains("quit"), "quit label displaced");
        assert!(row.contains('…'), "wide-char notice must truncate visibly");
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

    /// Narrow terminals elide whole hint cells by priority: the
    /// globals tail survives (quit above all), contextual hints drop
    /// from the end, and a `… +N` cell owns up to the hidden rest —
    /// no hint ever clips mid-label (#303).
    #[test]
    fn narrow_width_elides_whole_cells_and_keeps_quit() {
        let keymap = rich_keymap();
        let globals = globals_tail();
        for w in [60u16, 80, 100] {
            let row = render_row_at(w, &keymap, &globals, None, None);
            assert!(row.contains("q q"), "quit chord missing at {w} cols");
            assert!(row.contains("quit"), "quit label missing at {w} cols");
            assert_no_partial_labels(&row, &keymap);
            assert_no_partial_labels(&row, &globals);
            assert!(
                row.contains("… +"),
                "dropped cells need an overflow indicator at {w} cols: {row:?}",
            );
        }
        // Sanity: at 60 cols the low-priority tail really is gone.
        let row = render_row_at(60, &keymap, &globals, None, None);
        assert!(!row.contains("filter by role"), "nothing elided at 60 cols");
    }

    /// At a comfortable width everything still fits — measurement
    /// must not elide or add an indicator when there's room.
    #[test]
    fn wide_row_shows_everything_without_indicator() {
        let keymap = rich_keymap();
        let globals = globals_tail();
        let row = render_row_at(200, &keymap, &globals, None, None);
        for b in keymap.iter().chain(globals.iter()) {
            assert!(row.contains(b.label.as_ref()), "{:?} missing", b.label);
        }
        assert!(!row.contains("… +"), "spurious overflow indicator: {row:?}");
    }

    /// For any width from the globals tail on up, the row never ends
    /// in a partial label and always advertises `q q` quit — the
    /// guarantee issue #100 added and unmeasured clipping silently
    /// broke.
    #[test]
    fn any_width_keeps_quit_and_whole_labels() {
        let keymap = rich_keymap();
        let globals = globals_tail();
        // Full globals tail: " ? help  ·  T tour  ·  q q quit" = 31.
        for w in 31u16..=140 {
            let row = render_row_at(w, &keymap, &globals, None, None);
            assert!(row.contains("q q"), "quit chord missing at {w} cols");
            assert!(row.contains("quit"), "quit label missing at {w} cols");
            assert_no_partial_labels(&row, &keymap);
            assert_no_partial_labels(&row, &globals);
        }
    }

    /// Regression for the #303 worst case: a long notice shrinks the
    /// hint zone on top of a hint-rich keymap. Hints must elide
    /// cleanly (whole cells + indicator), not clip under the notice.
    #[test]
    fn long_notice_plus_rich_keymap_elides_cleanly() {
        let keymap = rich_keymap();
        let globals = globals_tail();
        let notice = Notice::new("x".repeat(200), NoticeSeverity::Permanent);
        let row = render_row_at(100, &keymap, &globals, None, Some(&notice));
        assert!(row.contains("q q"), "quit chord displaced by notice");
        assert!(row.contains("quit"), "quit label displaced by notice");
        assert_no_partial_labels(&row, &keymap);
        assert_no_partial_labels(&row, &globals);
        assert!(
            row.contains("… +"),
            "dropped cells need an overflow indicator: {row:?}",
        );
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
