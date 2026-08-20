//! `Footer` — single-row status line at the bottom of the screen.
//!
//! Three zones, all on one row:
//!
//! - **Left**: contextual hints for the focused pane — only actions
//!   that are actually wired up *right now*. The list is built by
//!   each pane from the action catalog (`lazybox_tui_core::action`),
//!   so a binding shown here is — by construction — a binding the
//!   pane will dispatch. A short, pane-independent set of universal
//!   hints (`?` help, `q q` quit) is appended after it so a
//!   first-time user can always find the way out and the way to
//!   orient (issue #100) — the rest of the keymap still lives in the
//!   `?` help modal and the tour. Low-value evergreen hints
//!   (`Shift-T` tour) come in a separate `evergreen` group that is
//!   the *first* to drop under pressure, so context-relevant bindings
//!   survive truncation ahead of them (#805). The run is measured
//!   against the zone's width and elided by whole cells — the
//!   escape-hatch tail (`?`/`q q`) surviving longest, then contextual
//!   hints in order, then evergreen hints first — with a dim
//!   `… +N ? all` overflow cell instead of mid-label clipping. That
//!   cell names `?` and is returned to the caller as a hit-rect so a
//!   click on it opens the `?` catalog listing the hidden hints — the
//!   `… +N` count is no longer a dead end (#805, was #303).
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

impl NoticeSeverity {
    /// Sticky severities own the footer slot: a routine lower-severity
    /// flash cannot displace them, and they carry the full-text inspect
    /// affordance (`Enter`, #453). Auth stays until acknowledged;
    /// Permanent errors are still sticky in this sense but now also
    /// auto-fade on a long timeout (#588) so a resolved condition's
    /// toast doesn't sit in the footer forever.
    pub fn is_sticky(self) -> bool {
        matches!(self, Self::Permanent | Self::Auth)
    }
}

/// One footer notice — message + severity + when it was set
/// (for auto-fade).
#[derive(Debug, Clone)]
pub struct Notice {
    pub message: String,
    pub severity: NoticeSeverity,
    pub set_at: std::time::Instant,
    /// Workspace this notice is about, when it names a specific
    /// PR/issue action (merge/close/update failed). Lets a superseding
    /// success for the same workspace self-clear a stale error (#588).
    pub workspace: Option<String>,
    /// How many times this exact notice has fired while on screen. An
    /// identical re-fire (a poll sweep re-emitting the same error)
    /// bumps this instead of resetting `set_at` — the fade clock runs
    /// from the FIRST appearance, so a repeating condition renders as
    /// an honest `… ×N` and still fades on schedule instead of owning
    /// the footer forever (#1245).
    pub repeats: u32,
}

impl Notice {
    pub fn new(message: impl Into<String>, severity: NoticeSeverity) -> Self {
        Self {
            message: message.into(),
            severity,
            set_at: std::time::Instant::now(),
            workspace: None,
            repeats: 1,
        }
    }

    /// True for an action-error toast — a Permanent failure tagged with
    /// the workspace whose merge/close/update/delete GitHub rejected.
    /// These carry a bounded auto-fade and self-clear on a superseding
    /// success (#588). Untagged Permanent banners (daemon disconnected,
    /// build mismatch, sync failed) are persistent system state and must
    /// stay until dismissed or resolved — so the fade keys off this, not
    /// off `Permanent` severity alone.
    pub fn is_action_toast(&self) -> bool {
        self.severity == NoticeSeverity::Permanent && self.workspace.is_some()
    }
}

/// The persistent "where do my keystrokes go" indicator at the left
/// edge of the footer (#1110). The daily "am I inside the agent?"
/// confusion has an unmissable answer here: terminal focus reads as a
/// bright accent `▶ typing to: <agent>` pill; every navigation pane
/// (sidebar / activity) reads as a quiet `◇ navigating` chip, so the
/// two modes can never be mistaken for each other.
pub struct FocusChip {
    /// Agent/shell label when focus is in a live terminal; `None` when
    /// focus is on a navigation pane.
    pub typing_to: Option<String>,
}

/// Build the focus chip's styled line plus its display width.
fn focus_chip_line(
    chip: &FocusChip,
    theme: &crate::theme::Theme,
    bg: Style,
) -> (Line<'static>, u16) {
    let line = match &chip.typing_to {
        Some(agent) => Line::from(vec![
            Span::styled(" ", bg),
            Span::styled(
                format!(" ▶ typing to: {agent} "),
                Style::default()
                    .bg(theme.accent)
                    .fg(Color::Black)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        None => Line::from(vec![
            Span::styled(" ", bg),
            Span::styled(
                " ◇ navigating ",
                Style::default()
                    .bg(theme.surface)
                    .fg(theme.text_dim)
                    .add_modifier(Modifier::DIM),
            ),
        ]),
    };
    let width = line.width() as u16;
    (line, width)
}

/// Pure render. The orchestrator passes in everything Footer needs:
/// the `focus_chip` mode indicator, the focused pane's keymap, the
/// escape-hatch `globals` tail, the low-value `evergreen` hints
/// (dropped first), the effective help key (`help_key` — what the
/// overflow cell tells the user to press, so a remap of `OpenHelp` is
/// honored), the optional polling status, and the optional notice.
/// Paints directly and returns the screen rect of the `… +N` overflow
/// cell when one was drawn, so the caller can make it clickable
/// (opening help); `None` when everything fit (#805).
#[allow(clippy::too_many_arguments)]
pub fn render(
    f: &mut Frame,
    area: Rect,
    focus_chip: Option<&FocusChip>,
    keymap: &[Binding],
    globals: &[Binding],
    evergreen: &[Binding],
    help_key: &str,
    polling_status: Option<(&str, &str)>, // (spinner, label)
    notice: Option<&Notice>,
) -> Option<Rect> {
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
    //
    // Sticky notices (error / auth banners) get a wider ~50% cap: their
    // text is the reason a merge/close/auth failed and is worth reading,
    // and middle-truncation otherwise spends the head budget on the
    // fixed `✗ <verb> failed:` prefix and clips the reason (#588). The
    // hint zone still elides gracefully around it (#303), so quit stays
    // findable. Rate-limit waits use the same room for their reset and
    // budget; routine notices and active polling keep the 40% cap.
    let sticky_notice = notice.is_some_and(|n| n.severity.is_sticky());
    let rate_limit_wait =
        polling_status.is_some_and(|(_, label)| label.starts_with("GitHub rate-limited"));
    let right_cap = if sticky_notice || rate_limit_wait {
        (area.width as usize) / 2
    } else {
        (area.width as usize) * 2 / 5
    };
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
        // A repeating condition renders its count — `merge failed ×4`
        // reads as "still happening", where a frozen-looking pill read
        // as "the footer is broken" (#1245).
        let text = if n.repeats > 1 {
            format!("{} ×{}", n.message, n.repeats)
        } else {
            n.message.clone()
        };
        let message =
            crate::util::truncate_ellipsis_middle(&text, right_cap.saturating_sub(chrome));
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

    // Left-most: the persistent focus-mode chip (#1110). Rendered
    // before the hint bar and carved off the left of `left_rect`, so
    // everything downstream (elision budget, overflow-cell offset) sees
    // the reduced remainder. Dropped only when the row is too narrow to
    // hold both the chip and any hints — the hints win a starvation
    // fight, since the chip's message is also carried by the terminal
    // header's own focus treatment.
    let left_rect = if let Some(chip) = focus_chip {
        let (line, want) = focus_chip_line(chip, theme, bg);
        // Keep at least a handful of columns for the hint bar.
        let w = want.min(left_rect.width.saturating_sub(8));
        if w >= want {
            let chip_rect = Rect {
                width: w,
                ..left_rect
            };
            f.render_widget(Paragraph::new(line).style(bg), chip_rect);
            Rect {
                x: left_rect.x + w,
                width: left_rect.width - w,
                ..left_rect
            }
        } else {
            left_rect
        }
    } else {
        left_rect
    };

    // Left zone: focused-pane contextual bindings, then the tail
    // cluster — low-value `evergreen` hints (tour) followed by the
    // escape-hatch `globals` (`?` help, `q q` quit). The contextual
    // list is the state-aware "what's actionable right now" guarantee
    // from issue #25; the globals tail (issue #100) re-adds the
    // handful of shortcuts a lost first-time user needs to always see
    // — chiefly how to quit. The rest of the keymap still lives in the
    // `?` help modal + the tour.
    //
    // The row is measured before it renders: whole hint cells are
    // elided when the budget runs out, never clipped mid-label (#303).
    // Survival priority ranks context-relevant bindings above the
    // evergreen extras (#805): evergreen hints drop FIRST (from the
    // end), then contextual hints in catalog order, then the globals
    // escape hatches from the front (so `q q` quit — rightmost —
    // survives above all, keeping #100's guarantee). When anything
    // drops, a dim `… +N ? all` cell ends the bar: it both names the
    // count and points at `?`, and its rect is returned so a click on
    // it opens the catalog listing the hidden hints (#805).
    let budget = left_rect.width as usize;
    let cell_width = |b: &Binding| {
        crate::util::visual_width(&compact_key(&b.keys)) + 1 + crate::util::visual_width(&b.label)
    };
    // "  ·  " between cells; also the contextual → tail gap width.
    const SEP_W: usize = 5;
    // The overflow cell tells the user which key reveals the hidden
    // hints; use the *effective* help key (compact form) so a remap of
    // `OpenHelp` is honored rather than a hardcoded `?` (#805).
    let help_disp = compact_key(help_key);
    let overflow_label = |n: usize| format!("… +{n} {help_disp} all");
    let total = keymap.len() + evergreen.len() + globals.len();
    // Width of the non-overflow content for a given set of kept cells.
    // `kg` keeps the globals *suffix* (drop from front); `ke` keeps the
    // evergreen *prefix* (drop from end). The tail cluster renders as
    // evergreen-prefix ++ globals-suffix with `·` separators between.
    let content_width = |kc: usize, ke: usize, kg: usize| -> usize {
        let cluster = ke + kg;
        let mut w = 1; // leading pad
        if kc > 0 {
            w += keymap[..kc].iter().map(cell_width).sum::<usize>() + SEP_W * (kc - 1);
        }
        if cluster > 0 {
            if kc > 0 {
                w += SEP_W;
            }
            w += evergreen[..ke].iter().map(cell_width).sum::<usize>()
                + globals[globals.len() - kg..]
                    .iter()
                    .map(cell_width)
                    .sum::<usize>()
                + SEP_W * (cluster - 1);
        }
        w
    };
    let mut kept_ctx = keymap.len();
    let mut kept_ever = evergreen.len();
    let mut kept_glob = globals.len();
    loop {
        let kept = kept_ctx + kept_ever + kept_glob;
        let dropped = total - kept;
        let mut w = content_width(kept_ctx, kept_ever, kept_glob);
        if dropped > 0 {
            if kept > 0 {
                w += SEP_W;
            }
            w += crate::util::visual_width(&overflow_label(dropped));
        }
        if w <= budget || kept == 0 {
            break;
        }
        if kept_ever > 0 {
            kept_ever -= 1;
        } else if kept_ctx > 0 {
            kept_ctx -= 1;
        } else {
            kept_glob -= 1;
        }
    }
    let dropped = total - kept_ctx - kept_ever - kept_glob;
    let keymap = &keymap[..kept_ctx];
    let evergreen = &evergreen[..kept_ever];
    let globals = &globals[globals.len() - kept_glob..];

    let mut spans: Vec<Span> =
        Vec::with_capacity((keymap.len() + evergreen.len() + globals.len()) * 4 + 6);
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
    // Tail cluster: evergreen hints then the escape-hatch globals. A
    // wider gap separates it from the contextual group so the two read
    // as distinct clusters rather than one run.
    for (i, b) in evergreen.iter().chain(globals.iter()).enumerate() {
        if i == 0 && !keymap.is_empty() {
            spans.push(Span::styled("     ", bg));
        } else if i > 0 {
            spans.push(Span::styled("  ·  ", sep_style));
        }
        spans.push(Span::styled(compact_key(&b.keys), key_style));
        spans.push(Span::styled(" ", bg));
        spans.push(Span::styled(b.label.clone(), label_style));
    }
    // Overflow cell. `… +N` is dim (a count), the help key is drawn
    // like a key (accent) so it reads as "press <key> for all", and the
    // whole cell is returned as a hit-rect for the click affordance
    // (#805).
    let overflow_rect = if dropped > 0 {
        let leading_sep = if !keymap.is_empty() || !evergreen.is_empty() || !globals.is_empty() {
            spans.push(Span::styled("  ·  ", sep_style));
            SEP_W
        } else {
            0
        };
        spans.push(Span::styled(format!("… +{dropped} "), label_style));
        spans.push(Span::styled(help_disp.to_string(), key_style));
        spans.push(Span::styled(" all", label_style));
        // Offset of the cell within `left_rect` — the width consumed by
        // everything rendered before it. The elision loop guarantees the
        // cell fits, so `offset + width <= left_rect.width`; the `.min`
        // is a defensive clamp for the degenerate ultra-narrow case.
        let offset =
            (content_width(keymap.len(), evergreen.len(), globals.len()) + leading_sep) as u16;
        let width = crate::util::visual_width(&overflow_label(dropped)) as u16;
        Some(Rect {
            x: left_rect.x.saturating_add(offset),
            y: left_rect.y,
            width: width.min(left_rect.width.saturating_sub(offset)),
            height: 1,
        })
    } else {
        None
    };
    f.render_widget(Paragraph::new(Line::from(spans)).style(bg), left_rect);

    if let Some(line) = right_text {
        f.render_widget(Paragraph::new(line).style(bg), right_rect);
    }

    overflow_rect
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
        render_row_at_evergreen(w, keymap, globals, &[], "?", polling_status, notice).0
    }

    /// Render at width `w` with an explicit `evergreen` group and help
    /// key, returning both the flat row and the overflow cell's rect (if
    /// drawn).
    fn render_row_at_evergreen(
        w: u16,
        keymap: &[Binding],
        globals: &[Binding],
        evergreen: &[Binding],
        help_key: &str,
        polling_status: Option<(&str, &str)>,
        notice: Option<&Notice>,
    ) -> (String, Option<Rect>) {
        let backend = TestBackend::new(w, 1);
        let mut term = Terminal::new(backend).unwrap();
        let mut overflow = None;
        term.draw(|f| {
            overflow = render(
                f,
                Rect::new(0, 0, w, 1),
                None,
                keymap,
                globals,
                evergreen,
                help_key,
                polling_status,
                notice,
            );
        })
        .unwrap();
        let buf = term.backend().buffer().clone();
        let row = (0..w).map(|x| buf[(x, 0)].symbol()).collect();
        (row, overflow)
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
            binding("g r", "manage reviewers"),
            binding("z", "snooze until later"),
            binding("Shift-X", "delete forever"),
            binding("/", "filter by role"),
        ]
    }

    fn globals_tail() -> Vec<Binding> {
        vec![
            binding("?", "ask lazybox"),
            binding("Shift-T", "tour"),
            binding("q q", "quit"),
        ]
    }

    /// The escape-hatch tail as the model now supplies it (#805): the
    /// tour hint is split off into its own `evergreen` group.
    fn globals_survivors() -> Vec<Binding> {
        vec![binding("?", "ask lazybox"), binding("q q", "quit")]
    }

    fn evergreen_tail() -> Vec<Binding> {
        vec![binding("Shift-T", "tour")]
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

    /// A sticky error banner gets the wider ~50% cap so more of its
    /// leading reason survives truncation, where a routine notice at
    /// ~40% would clip it — and the hint zone still keeps `q q` (#588).
    /// The reason leads, so the extra head budget is what makes it
    /// readable; the marker `RATELIMIT` sits just past the 40% head
    /// budget but inside the 50% one.
    #[test]
    fn sticky_error_gets_more_room_for_its_reason() {
        let keymap = [binding("w", "work on this")];
        let globals = [binding("?", "help"), binding("q q", "quit")];
        let reason = "RATELIMITED secondary rate limit hit please retry after the window closes";
        let msg = format!("✗ merge failed: {reason} (#588)");

        let sticky = Notice::new(msg.clone(), NoticeSeverity::Permanent);
        let sticky_row = render_row_full(&keymap, &globals, None, Some(&sticky));
        assert!(
            sticky_row.contains("RATELIMIT"),
            "sticky error must reveal the leading reason: {sticky_row:?}",
        );
        assert!(sticky_row.contains("q q"), "quit displaced: {sticky_row:?}");

        // The very same text at Info severity keeps the 40% cap, where
        // the reason head is clipped — proving the widening is what
        // reveals it, not a change in the message.
        let routine = Notice::new(msg, NoticeSeverity::Info);
        let routine_row = render_row_full(&keymap, &globals, None, Some(&routine));
        assert!(
            !routine_row.contains("RATELIMIT"),
            "a routine notice must stay at the 40% cap: {routine_row:?}",
        );
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

    #[test]
    fn rate_limit_wait_keeps_reset_and_budget_visible() {
        let globals = [binding("q q", "quit")];
        let label = "GitHub rate-limited · ~7m · 07:23 UTC · 98/5000 left";
        let row = render_row_full(&[], &globals, Some(("◷", label)), None);

        assert!(row.contains("GitHub rate-limited"), "{row:?}");
        assert!(row.contains("07:23 UTC"), "{row:?}");
        assert!(row.contains("98/5000 left"), "{row:?}");
        assert!(row.contains("quit"), "{row:?}");
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

    /// The overflow cell is not a dead `… +N`: it names `?` (the escape
    /// hatch that reveals the hidden hints) and reports a screen rect so
    /// the caller can make it clickable (#805).
    #[test]
    fn overflow_cell_is_labeled_and_returns_a_hit_rect() {
        let keymap = rich_keymap();
        let globals = globals_survivors();
        let evergreen = evergreen_tail();
        let (row, overflow) =
            render_row_at_evergreen(70, &keymap, &globals, &evergreen, "?", None, None);
        assert!(row.contains("… +"), "overflow indicator missing: {row:?}");
        assert!(
            row.contains("all"),
            "overflow cell must point at `?`/all, not a dead `… +N`: {row:?}",
        );
        let rect = overflow.expect("overflow cell must report a hit-rect");
        assert!(rect.width > 0, "overflow rect has no width: {rect:?}");
        // The reported rect must actually cover the `?` glyph so a click
        // there is a click on the escape hatch.
        let cell: String = row
            .chars()
            .skip(rect.x as usize)
            .take(rect.width as usize)
            .collect();
        assert!(cell.contains('?'), "overflow rect must cover `?`: {cell:?}");
        assert!(
            cell.contains("all"),
            "overflow rect must cover label: {cell:?}"
        );
    }

    /// The overflow cell names the EFFECTIVE help key, not a hardcoded
    /// `?`: a user who remapped `OpenHelp` sees the key they actually
    /// press (#805). The rect still covers the labeled key.
    #[test]
    fn overflow_cell_uses_the_effective_help_key() {
        let keymap = rich_keymap();
        let globals = vec![binding("F1", "ask lazybox"), binding("q q", "quit")];
        let evergreen = evergreen_tail();
        let (row, overflow) =
            render_row_at_evergreen(70, &keymap, &globals, &evergreen, "F1", None, None);
        assert!(
            row.contains("F1 all"),
            "overflow must advertise the remapped help key: {row:?}",
        );
        assert!(!row.contains("? all"), "stale hardcoded `?`: {row:?}");
        let rect = overflow.expect("overflow cell must report a hit-rect");
        let cell: String = row
            .chars()
            .skip(rect.x as usize)
            .take(rect.width as usize)
            .collect();
        assert!(
            cell.contains("F1"),
            "rect must cover the help key: {cell:?}"
        );
    }

    /// A wide row that fits everything reports no overflow rect — there
    /// is nothing to click through to.
    #[test]
    fn no_overflow_rect_when_everything_fits() {
        let keymap = rich_keymap();
        let globals = globals_survivors();
        let evergreen = evergreen_tail();
        let (_, overflow) =
            render_row_at_evergreen(200, &keymap, &globals, &evergreen, "?", None, None);
        assert!(overflow.is_none(), "no overflow rect expected at 200 cols");
    }

    /// Ranking (#805): the low-value evergreen hint (tour) is the FIRST
    /// to drop — before any context-relevant contextual hint — so the
    /// useful set survives truncation. `q q` quit still survives above
    /// all (#100), and the leading contextual hint outlives tour.
    #[test]
    fn evergreen_hint_drops_before_contextual() {
        let keymap = rich_keymap();
        let globals = globals_survivors();
        let evergreen = evergreen_tail();
        // Wide enough for everything: tour shows.
        let (wide, _) =
            render_row_at_evergreen(200, &keymap, &globals, &evergreen, "?", None, None);
        assert!(
            wide.contains("tour"),
            "tour should show when it fits: {wide:?}"
        );
        // As the row narrows, tour is the first cell to fall off while
        // the top contextual hint and quit stay.
        for w in [70u16, 80, 90] {
            let (row, _) =
                render_row_at_evergreen(w, &keymap, &globals, &evergreen, "?", None, None);
            assert!(
                !row.contains("tour"),
                "tour must drop first at {w} cols: {row:?}"
            );
            assert!(
                row.contains("q q"),
                "quit must survive at {w} cols: {row:?}"
            );
            assert!(
                row.contains("work on this"),
                "top contextual hint must outlive tour at {w} cols: {row:?}",
            );
        }
    }

    /// For any width from the globals tail on up, the row never ends
    /// in a partial label and always advertises `q q` quit — the
    /// guarantee issue #100 added and unmeasured clipping silently
    /// broke.
    #[test]
    fn any_width_keeps_quit_and_whole_labels() {
        let keymap = rich_keymap();
        let globals = globals_tail();
        // Full globals tail: " ? ask lazybox  ·  T tour  ·  q q quit".
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
