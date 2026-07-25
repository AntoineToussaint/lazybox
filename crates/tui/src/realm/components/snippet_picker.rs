//! `SnippetPicker` — categorized fuzzy picker for the snippet system.
//!
//! Combines a single-line filter with a scrolling, category-grouped
//! list and a live preview pane. Typing extends the filter; ↑/↓
//! navigate (the list scrolls to keep the cursor in view); Enter
//! expands the selected snippet AND auto-submits (the caller's
//! `handle_choice_picked` arm writes `body + "\r"` to the active
//! terminal). The "expand and submit" pair is the whole point of the
//! feature — see issue #40.
//!
//! Layout (#244): a left list grouped under colored category headers,
//! a right preview pane showing the highlighted snippet's full wrapped
//! body plus its category and origin, and a header line with the
//! total/visible count.
//!
//! Modal returns:
//! - `Msg::ChoicePicked(vec![ChoicePayload::Text(key)])` — the chosen
//!   snippet's key, read straight off the highlighted row. The key
//!   travels with the row, so a filtered / re-grouped display can't
//!   resolve to the wrong snippet and there's no model-side stash
//!   (issue #512). An empty pick (`Ctrl-F`, free-text) is
//!   `ChoicePicked(vec![])`.
//! - `Msg::ModalDismissed` — Esc or Ctrl-C.
//!
//! Entry + fast path (#252): the terminal `]]` leader opens this
//! picker on `]]s`, mounted with an empty filter. Typing extends the
//! filter; if it equals a snippet KEY exactly AND that key is the only
//! snippet whose key starts with the filter, the picker auto-submits —
//! so `]]srev` sends `rev` with no `Enter`. Filtering the *display*
//! also matches description/category text, but that broader set never
//! gates the exact-key auto-submit.
//!
//! Recent group (#252): [`SnippetPicker::with_recent`] seeds a
//! most-recently-used key list; on an empty filter those snippets show
//! in a "Recent" group at the top (cursor on the newest), so repeating
//! one is `]]s` + `Enter`. Any filter text steps the group aside.

use crate::components::comment_render::wrap_one;
use crate::realm::ChoicePayload;
use crate::realm::Msg;
use crate::realm::UserEvent;
use crate::realm::components::filterable::FilterableList;
use crate::theme::Theme;
use lazybox_config::Snippet;
use tuirealm::command::{Cmd, CmdResult};
use tuirealm::component::{AppComponent, Component};
use tuirealm::event::{Event, Key, KeyEvent, KeyModifiers};
use tuirealm::props::{AttrValue, Attribute, QueryResult};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::prelude::*;
use tuirealm::ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use tuirealm::state::State;

/// A single picker row — the bits the picker needs to render the list,
/// the preview pane, and the filter. Carries the full `body` (unlike
/// the pre-#244 one-line teaser) so the preview pane can show exactly
/// what will be sent.
#[derive(Clone, Debug)]
pub struct PickerRow {
    pub key: String,
    pub description: String,
    pub category: String,
    pub body: String,
    pub origin: lazybox_config::SnippetOrigin,
}

impl PickerRow {
    pub fn new(key: &str, snippet: &Snippet) -> Self {
        Self {
            key: key.to_string(),
            description: snippet.description.clone(),
            category: snippet.category.clone(),
            body: snippet.body.clone(),
            origin: snippet.origin,
        }
    }
}

/// Category display order: the opinionated built-in groups first (in
/// the order the issue lists them), then any custom user category
/// alphabetically, then the empty "Other" bucket last.
fn category_rank(cat: &str) -> u8 {
    match cat {
        "Review" => 0,
        "Git & PR" => 1,
        "Testing" => 2,
        "Debugging" => 3,
        "Refactor" => 4,
        "Performance" => 5,
        "Security" => 6,
        "Docs" => 7,
        "Chores" => 8,
        "" => 254, // "Other" bucket, always last
        _ => 200,  // custom categories, before Other, sorted by name
    }
}

/// Label shown for a group header — empty category renders as "Other",
/// and the whitespace-sentinel recent group as "Recent".
fn category_label(cat: &str) -> &str {
    match cat {
        "" => "Other",
        RECENT_CAT => "Recent",
        other => other,
    }
}

/// Deterministic per-category accent, so the same category always
/// draws in the same color across renders (and custom categories get
/// a stable color too). Every palette entry is a theme color designed
/// to be legible as *foreground* (accent / success / warn / error are
/// all used as text elsewhere) — `theme.hover`, a subtle highlight
/// tint, is deliberately excluded so no category label lands
/// low-contrast on the picker surface in either theme.
fn category_color(theme: &Theme, cat: &str) -> Color {
    if cat.is_empty() {
        return theme.text_dim;
    }
    if cat == RECENT_CAT {
        return theme.accent;
    }
    let palette = [theme.accent, theme.success, theme.warn, theme.error];
    let h = cat.bytes().fold(0usize, |a, b| a.wrapping_add(b as usize));
    palette[h % palette.len()]
}

/// Case-insensitive ASCII prefix check that doesn't allocate.
fn starts_with_icase(haystack: &str, needle: &str) -> bool {
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    h.len() >= n.len() && h[..n.len()].eq_ignore_ascii_case(n)
}

/// Case-insensitive ASCII substring check that doesn't allocate — the
/// filter re-runs on every keystroke, so we scan bytes rather than
/// lowercasing a fresh `String` per row.
fn contains_icase(haystack: &str, needle: &str) -> bool {
    let n = needle.as_bytes();
    if n.is_empty() {
        return true;
    }
    let h = haystack.as_bytes();
    h.len() >= n.len() && h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// A rendered list line: either a category header or a snippet row.
enum LayoutItem {
    Header { cat: String, count: usize },
    Row { pos: usize, row_idx: usize },
}

/// Header label for the most-recently-used group shown at the top of
/// the picker on an empty filter. A leading space keeps it from
/// colliding with a real user category of the same name in
/// `category_rank` (categories never start with whitespace).
const RECENT_CAT: &str = " Recent";

pub struct SnippetPicker {
    /// Caller-supplied rows. Must arrive sorted by key (we
    /// `debug_assert!` it in `new` so a mis-sorted test input
    /// fails loudly instead of silently rendering wrong).
    rows: Vec<PickerRow>,
    /// Current filter string. Driven by the input field.
    filter: String,
    /// Cursor index into `visible_indices`. `None` when the
    /// visible set is empty — avoids the "cursor=0 but no row
    /// at that slot" pseudo-valid state.
    cursor: Option<usize>,
    /// Indices into `rows` that match the current filter, grouped by
    /// category in display order. Recomputed on every keystroke.
    visible_indices: Vec<usize>,
    /// Row indices of recently-used snippets, most-recent first
    /// (resolved from the caller's MRU key list in `with_recent`).
    /// Shown as a "Recent" group at the top when the filter is empty,
    /// so a repeated snippet is one `]]s` + `Enter` away (#252).
    recent_rows: Vec<usize>,
    /// How many leading `visible_indices` belong to the "Recent" group
    /// this refilter — non-zero only on an empty filter with recents.
    /// Lets `layout` emit the single "Recent" header without re-deriving
    /// it from per-row categories (recent rows keep their real one).
    recent_count: usize,
    /// Topmost visible list line (headers + rows), kept in step with
    /// the cursor in `view`.
    list_scroll: usize,
    /// Modal-border title override. `None` renders the default
    /// " Snippets "; the broadcast flow names its targets here.
    title: Option<String>,
    /// Offer a `Ctrl-F` "no snippet — free text only" escape that
    /// resolves as an empty pick (`Msg::ChoicePicked(vec![])`). Set by
    /// the broadcast flow, whose compose step follows either way.
    offer_free_text: bool,
}

impl SnippetPicker {
    pub fn new(rows: Vec<PickerRow>, initial_filter: String) -> Self {
        debug_assert!(
            rows.windows(2).all(|w| w[0].key <= w[1].key),
            "SnippetPicker::new expects rows pre-sorted by key (Snippets::all walks a BTreeMap)",
        );
        let mut picker = Self {
            rows,
            filter: initial_filter,
            cursor: None,
            visible_indices: Vec::new(),
            recent_rows: Vec::new(),
            recent_count: 0,
            list_scroll: 0,
            title: None,
            offer_free_text: false,
        };
        picker.refilter();
        picker
    }

    /// Override the modal-border title (default " Snippets ").
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    /// Offer the `Ctrl-F` "free text only" escape — Enter-equivalent
    /// that resolves as an empty pick instead of a snippet row.
    pub fn with_free_text_option(mut self) -> Self {
        self.offer_free_text = true;
        self
    }

    /// Seed the most-recently-used group from a caller's MRU key list
    /// (most-recent first). Keys not present in `rows` are dropped;
    /// duplicates keep their first (most-recent) position. Builder-style
    /// so the many 2-arg test constructions stay untouched (#252).
    pub fn with_recent(mut self, recent: Vec<String>) -> Self {
        let mut seen = std::collections::HashSet::new();
        self.recent_rows = recent
            .iter()
            .filter(|k| seen.insert((*k).clone()))
            .filter_map(|k| self.rows.iter().position(|r| &r.key == k))
            .collect();
        self.refilter();
        self
    }

    /// Recompute `visible_indices` from `filter`, grouped by category,
    /// and reset the cursor to the first visible row. A row matches
    /// when the filter is a case-insensitive substring of its key,
    /// description, or category — richer than the pre-#244 key-only
    /// prefix so snippets are discoverable by what they *do*. The
    /// exact-key auto-submit fast path is decided separately (see
    /// [`auto_submit_index`]) and is unaffected by description hits.
    /// `Some(idx)` when the picker should auto-submit: the typed filter
    /// exactly equals a snippet key AND that key is the *only* snippet
    /// whose key starts with the filter. Decided over key-prefix
    /// matches alone, so the broader description-inclusive display
    /// filter can't suppress the `]rev` fast path (nor make an
    /// ambiguous prefix auto-fire).
    ///
    /// Case-tie break: matching is ASCII case-insensitive, so a library
    /// defining both `rev` and `Rev` used to make the fast path
    /// permanently ambiguous — typing either full key never
    /// auto-submitted. When several case-insensitive prefix matches
    /// exist but exactly one is a case-EXACT full-key match of the
    /// typed text, that one wins.
    fn auto_submit_index(&self) -> Option<usize> {
        let q = self.filter.trim();
        if q.is_empty() {
            return None;
        }
        let prefix_matches: Vec<(usize, &PickerRow)> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| starts_with_icase(&r.key, q))
            .collect();
        match prefix_matches.as_slice() {
            [] => None,
            [(idx, row)] => row.key.eq_ignore_ascii_case(q).then_some(*idx),
            many => {
                let mut exact = many.iter().filter(|(_, r)| r.key == q);
                let &(idx, _) = exact.next()?;
                // Keys are unique (BTreeMap-backed), so a second exact
                // match can't happen — the guard is belt-and-braces.
                exact.next().is_none().then_some(idx)
            }
        }
    }

    /// Build the list layout (headers + rows) from `visible_indices`.
    /// `visible_indices` is grouped by category, so each category's run
    /// is contiguous and gets exactly one header.
    fn layout(&self) -> Vec<LayoutItem> {
        let mut out: Vec<LayoutItem> = Vec::new();
        let mut i = 0;
        // The leading `recent_count` rows are the "Recent" group — one
        // explicit header, since those rows keep their real category and
        // the contiguous-category scan below would otherwise mis-split
        // them.
        if self.recent_count > 0 {
            out.push(LayoutItem::Header {
                cat: RECENT_CAT.to_string(),
                count: self.recent_count,
            });
            while i < self.recent_count {
                out.push(LayoutItem::Row {
                    pos: i,
                    row_idx: self.visible_indices[i],
                });
                i += 1;
            }
        }
        while i < self.visible_indices.len() {
            let cat = self.rows[self.visible_indices[i]].category.clone();
            let mut j = i;
            while j < self.visible_indices.len()
                && self.rows[self.visible_indices[j]].category == cat
            {
                j += 1;
            }
            out.push(LayoutItem::Header {
                cat: cat.clone(),
                count: j - i,
            });
            for pos in i..j {
                out.push(LayoutItem::Row {
                    pos,
                    row_idx: self.visible_indices[pos],
                });
            }
            i = j;
        }
        out
    }

    /// Pure key handler. Tests drive it directly without spinning up a
    /// tuirealm `Application`; it forwards to the shared picker protocol
    /// in [`FilterableList::dispatch_key`], which routes Enter through
    /// [`SnippetPicker::pick`], typed characters through the
    /// [`FilterableList::auto_submit`] hook, and `Ctrl-F` through the
    /// [`FilterableList::custom_key`] hook.
    pub fn on_key(&mut self, key: &KeyEvent) -> Option<Msg> {
        self.dispatch_key(key)
    }

    /// Render the left, category-grouped list into `area`, scrolling so
    /// the cursor stays visible.
    fn render_list(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        if self.visible_indices.is_empty() {
            let msg = if self.rows.is_empty() {
                "  (no snippets configured — see ~/.lazybox/snippets.yaml)"
            } else {
                "  (no matches)"
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    msg,
                    Style::default().fg(theme.text_dim).italic(),
                ))),
                area,
            );
            return;
        }

        let items = self.layout();
        let h = area.height.max(1) as usize;
        // Keep the cursor row visible; when it's the first row of a
        // group, pull its header into view too.
        if let Some(c) = self.cursor
            && let Some(cl) = items
                .iter()
                .position(|it| matches!(it, LayoutItem::Row { pos, .. } if *pos == c))
        {
            let anchor = if cl > 0 && matches!(items[cl - 1], LayoutItem::Header { .. }) {
                cl - 1
            } else {
                cl
            };
            if anchor < self.list_scroll {
                self.list_scroll = anchor;
            }
            if cl >= self.list_scroll + h {
                self.list_scroll = cl + 1 - h;
            }
        }
        let max_scroll = items.len().saturating_sub(h);
        if self.list_scroll > max_scroll {
            self.list_scroll = max_scroll;
        }

        let mut lines: Vec<Line> = Vec::with_capacity(h);
        for item in items.iter().skip(self.list_scroll).take(h) {
            match item {
                LayoutItem::Header { cat, count } => {
                    lines.push(Line::from(vec![
                        Span::styled(
                            category_label(cat).to_string(),
                            Style::default()
                                .fg(category_color(theme, cat))
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(format!("  {count}"), Style::default().fg(theme.text_dim)),
                    ]));
                }
                LayoutItem::Row { pos, row_idx } => {
                    let r = &self.rows[*row_idx];
                    let is_cursor = self.cursor == Some(*pos);
                    let bg = |s: Style| if is_cursor { s.bg(theme.fill) } else { s };
                    let caret = if is_cursor { "▸ " } else { "  " };
                    let mut base = Style::default().fg(theme.text_strong);
                    if is_cursor {
                        base = base.add_modifier(Modifier::BOLD);
                    }
                    lines.push(Line::from(vec![
                        Span::styled(caret.to_string(), bg(base)),
                        Span::styled(
                            "● ".to_string(),
                            bg(Style::default().fg(category_color(theme, &r.category))),
                        ),
                        Span::styled(
                            format!("]{:<9} ", r.key),
                            bg(Style::default().fg(theme.accent)),
                        ),
                        Span::styled(r.description.clone(), bg(base)),
                    ]));
                }
            }
        }
        frame.render_widget(Paragraph::new(lines), area);
    }

    /// Render the right preview pane: the highlighted snippet's title,
    /// category + origin, and full wrapped body — so the user sees
    /// exactly what auto-submit will send.
    fn render_preview(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let Some(c) = self.cursor else {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    "(no snippet selected)",
                    Style::default().fg(theme.text_dim).italic(),
                ))),
                area,
            );
            return;
        };
        let r = &self.rows[self.visible_indices[c]];
        let mut lines: Vec<Line> = Vec::new();

        let mut title = vec![Span::styled(
            format!("]{}", r.key),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )];
        if !r.description.is_empty() {
            title.push(Span::raw("  "));
            title.push(Span::styled(
                r.description.clone(),
                Style::default().fg(theme.text_strong),
            ));
        }
        lines.extend(wrap_one(Line::from(title), area.width));

        let mut meta = vec![Span::styled(
            category_label(&r.category).to_string(),
            Style::default().fg(category_color(theme, &r.category)),
        )];
        let origin = r.origin.label();
        if !origin.is_empty() {
            meta.push(Span::styled("  ·  ", Style::default().fg(theme.text_dim)));
            meta.push(Span::styled(
                origin.to_string(),
                Style::default().fg(theme.text_dim).italic(),
            ));
        }
        lines.push(Line::from(meta));
        lines.push(Line::raw(""));

        let body_style = Style::default().fg(theme.text_dim);
        for raw in r.body.lines() {
            lines.extend(wrap_one(
                Line::from(Span::styled(raw.to_string(), body_style)),
                area.width,
            ));
        }
        frame.render_widget(Paragraph::new(lines), area);
    }
}

impl FilterableList for SnippetPicker {
    /// Recompute the visible index list, grouped by category, and record
    /// whether a "Recent" group leads it (`recent_count`). The shared
    /// [`FilterableList::refilter`] stores the result and resets the
    /// cursor; [`SnippetPicker::set_visible`] zeroes `list_scroll`. A row
    /// matches when the filter is a case-insensitive substring of its
    /// key, description, or category — richer than the pre-#244 key-only
    /// prefix so snippets are discoverable by what they *do*. The
    /// exact-key auto-submit fast path is decided separately (see
    /// [`SnippetPicker::auto_submit_index`]) and is unaffected by
    /// description hits.
    fn compute_visible(&mut self) -> Vec<usize> {
        let q = self.filter.trim();
        let mut idxs: Vec<usize> = if q.is_empty() {
            (0..self.rows.len()).collect()
        } else {
            self.rows
                .iter()
                .enumerate()
                .filter_map(|(i, r)| {
                    (contains_icase(&r.key, q)
                        || contains_icase(&r.description, q)
                        || contains_icase(&r.category, q))
                    .then_some(i)
                })
                .collect()
        };
        // Group by category (headers), keeping rows key-sorted within a
        // group — `rows` already arrives key-sorted, so a stable sort by
        // category rank preserves that.
        idxs.sort_by(|&a, &b| {
            let (ca, cb) = (&self.rows[a].category, &self.rows[b].category);
            category_rank(ca)
                .cmp(&category_rank(cb))
                .then_with(|| ca.cmp(cb))
        });
        // On an empty filter, float the recently-used snippets into a
        // "Recent" group at the very top (they also remain in their real
        // category below — the group is a shortcut, not a move). A
        // non-empty filter is a deliberate search, so recents step aside.
        if q.is_empty() && !self.recent_rows.is_empty() {
            self.recent_count = self.recent_rows.len();
            self.recent_rows.iter().copied().chain(idxs).collect()
        } else {
            self.recent_count = 0;
            idxs
        }
    }

    fn pick(&self, item_idx: usize) -> Option<Msg> {
        let key = self.rows.get(item_idx)?.key.clone();
        Some(Msg::ChoicePicked(vec![ChoicePayload::Text(key)]))
    }

    fn filter(&self) -> &str {
        &self.filter
    }
    fn filter_mut(&mut self) -> &mut String {
        &mut self.filter
    }
    fn cursor(&self) -> Option<usize> {
        self.cursor
    }
    fn set_cursor(&mut self, cursor: Option<usize>) {
        self.cursor = cursor;
    }
    fn visible(&self) -> &[usize] {
        &self.visible_indices
    }
    fn set_visible(&mut self, visible: Vec<usize>) {
        self.visible_indices = visible;
        // A refilter re-anchors the viewport to the top; the cursor is
        // reset by the shared `refilter`.
        self.list_scroll = 0;
    }

    /// Typed-character auto-submit: the `]]srev` fast path. Fires only
    /// when the filter uniquely identifies a snippet key.
    fn auto_submit(&self) -> Option<Msg> {
        self.auto_submit_index()
            .and_then(|idx| self.rows.get(idx))
            .map(|row| Msg::ChoicePicked(vec![ChoicePayload::Text(row.key.clone())]))
    }

    /// `Ctrl-F` "no snippet — free text only": an empty pick the
    /// broadcast handler reads as "skip straight to compose". Inert
    /// unless the broadcast flow opted in with `with_free_text_option`.
    fn custom_key(&mut self, key: &KeyEvent) -> Option<Msg> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        (ctrl && matches!(key.code, Key::Char('f')) && self.offer_free_text)
            .then(|| Msg::ChoicePicked(Vec::new()))
    }
}

impl Component for SnippetPicker {
    fn view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = crate::theme::current();
        let modal_w = 92u16.min(area.width.saturating_sub(4));
        let modal_h = 26u16.min(area.height.saturating_sub(4));
        let x = area.x + area.width.saturating_sub(modal_w) / 2;
        let y = area.y + area.height.saturating_sub(modal_h) / 2;
        let modal = Rect::new(x, y, modal_w, modal_h);

        frame.render_widget(Clear, modal);
        let title = match &self.title {
            Some(t) => format!(" {t} "),
            None => " Snippets ".to_string(),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .title(Span::styled(title, theme.modal_title()))
            .border_style(theme.modal_border());
        let inner = block.inner(modal);
        frame.render_widget(block, modal);

        if inner.height < 4 || inner.width < 8 {
            return;
        }

        // Header: filter (left) + visible/total count (right).
        let header_rect = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: 1,
        };
        let count = format!("{}/{}", self.visible_indices.len(), self.rows.len());
        let count_w = count.len() as u16;
        let filter_w = inner.width.saturating_sub(count_w + 1);
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("] ", Style::default().fg(theme.accent).bold()),
                Span::styled(self.filter.clone(), Style::default().fg(theme.text_strong)),
                Span::styled("▌", Style::default().fg(theme.accent)),
            ])),
            Rect {
                width: filter_w,
                ..header_rect
            },
        );
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                count,
                Style::default().fg(theme.text_dim),
            )))
            .alignment(Alignment::Right),
            Rect {
                x: inner.x + filter_w,
                width: count_w,
                ..header_rect
            },
        );

        // Divider under the header.
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "─".repeat(inner.width as usize),
                theme.divider(),
            ))),
            Rect {
                x: inner.x,
                y: inner.y + 1,
                width: inner.width,
                height: 1,
            },
        );

        // Main = list | preview, leaving the last inner line for help.
        let main = Rect {
            x: inner.x,
            y: inner.y + 2,
            width: inner.width,
            height: inner.height - 3,
        };
        // Split only when there's room for both; otherwise list-only.
        let list_w = if main.width >= 56 {
            (main.width / 2).clamp(24, 46)
        } else {
            main.width
        };
        let list_rect = Rect {
            width: list_w,
            ..main
        };
        self.render_list(frame, list_rect, theme);

        if main.width > list_w + 1 {
            // Vertical divider column between list and preview.
            let div_x = main.x + list_w;
            let divider: Vec<Line> = (0..main.height)
                .map(|_| Line::from(Span::styled("│", theme.divider())))
                .collect();
            frame.render_widget(
                Paragraph::new(divider),
                Rect {
                    x: div_x,
                    width: 1,
                    ..main
                },
            );
            let preview_rect = Rect {
                x: div_x + 2,
                width: main.width - list_w - 2,
                ..main
            };
            self.render_preview(frame, preview_rect, theme);
        }

        // Help line.
        let mut help = vec![
            Span::styled("↑↓", Style::default().fg(theme.accent).bold()),
            Span::raw(" navigate  "),
            Span::styled("Enter", Style::default().fg(theme.success).bold()),
            Span::raw(" send  "),
            Span::styled("Type", Style::default().fg(theme.accent).bold()),
            Span::raw(" filter  "),
        ];
        if self.offer_free_text {
            help.push(Span::styled(
                "Ctrl-F",
                Style::default().fg(theme.accent).bold(),
            ));
            help.push(Span::raw(" free text  "));
        }
        help.push(Span::styled("Esc", Style::default().fg(theme.error).bold()));
        help.push(Span::raw(" cancel"));
        frame.render_widget(
            Paragraph::new(Line::from(help)),
            Rect {
                x: inner.x,
                y: inner.y + inner.height - 1,
                width: inner.width,
                height: 1,
            },
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

impl AppComponent<Msg, UserEvent> for SnippetPicker {
    fn on(&mut self, ev: &Event<UserEvent>) -> Option<Msg> {
        match ev {
            Event::Keyboard(key) => self.on_key(key),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_config::{Snippet, SnippetOrigin};

    fn snip(category: &str, description: &str, body: &str, origin: SnippetOrigin) -> Snippet {
        Snippet {
            description: description.into(),
            category: category.into(),
            body: body.into(),
            origin,
        }
    }

    /// Build a sorted row vec. The production caller (`mount_snippet_picker`)
    /// hands the picker rows derived from `Snippets::all()`, which walks
    /// a BTreeMap and is therefore already sorted; tests have to match
    /// that contract or trip the `debug_assert!` in `SnippetPicker::new`.
    fn make_rows() -> Vec<PickerRow> {
        // Keys: deploy, pr, rev — already alphabetical.
        vec![
            PickerRow::new(
                "deploy",
                &snip(
                    "Chores",
                    "Pre-deploy check",
                    "deploy check",
                    SnippetOrigin::Repo,
                ),
            ),
            PickerRow::new(
                "pr",
                &snip(
                    "Git & PR",
                    "Open PR",
                    "open pr please",
                    SnippetOrigin::Global,
                ),
            ),
            PickerRow::new(
                "rev",
                &snip(
                    "Review",
                    "Review diff",
                    "review please",
                    SnippetOrigin::Global,
                ),
            ),
        ]
    }

    fn ke(c: char) -> KeyEvent {
        KeyEvent::new(Key::Char(c), KeyModifiers::NONE)
    }

    fn key(code: Key) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn render(comp: &mut SnippetPicker, w: u16, h: u16) -> String {
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

    #[test]
    fn empty_filter_shows_all_rows_grouped_by_category() {
        let picker = SnippetPicker::new(make_rows(), String::new());
        // Grouped display order: Review, Git & PR, Chores → rev, pr, deploy.
        let keys: Vec<_> = picker
            .visible_indices
            .iter()
            .map(|&i| picker.rows[i].key.clone())
            .collect();
        assert_eq!(keys, vec!["rev", "pr", "deploy"]);
        assert_eq!(picker.cursor, Some(0));
    }

    /// Acceptance criterion: against the *real* built-in library,
    /// `]]rev` still auto-submits (regression guard for the case where
    /// a longer built-in key shares the `rev` prefix).
    #[test]
    fn builtin_rev_auto_submits() {
        let s = lazybox_config::Snippets::builtin();
        let rows: Vec<PickerRow> = s.all().map(|(k, v)| PickerRow::new(k, v)).collect();
        let mut picker = SnippetPicker::new(rows, String::new());
        assert!(picker.on_key(&ke('r')).is_none());
        assert!(picker.on_key(&ke('e')).is_none());
        match picker.on_key(&ke('v')) {
            Some(Msg::ChoicePicked(v)) => {
                assert_eq!(v, vec![ChoicePayload::Text("rev".into())])
            }
            other => panic!("expected `]]rev` to auto-submit, got {other:?}"),
        }
    }

    /// `Ctrl-F` with the free-text option on resolves as an EMPTY pick
    /// — the broadcast flow's "no snippet" escape. Without the option
    /// (the plain `]]s` picker) the same chord does nothing.
    #[test]
    fn ctrl_f_free_text_resolves_as_empty_pick_only_when_offered() {
        let ctrl_f = KeyEvent::new(Key::Char('f'), KeyModifiers::CONTROL);
        let mut plain = SnippetPicker::new(make_rows(), String::new());
        assert_eq!(plain.on_key(&ctrl_f), None, "not offered — inert");

        let mut offered = SnippetPicker::new(make_rows(), String::new()).with_free_text_option();
        assert_eq!(
            offered.on_key(&ctrl_f),
            Some(Msg::ChoicePicked(Vec::new())),
            "offered — resolves as an empty pick",
        );
        // The help line advertises the escape.
        let screen = render(&mut offered, 92, 26);
        assert!(screen.contains("Ctrl-F"), "help line names it:\n{screen}");
        assert!(screen.contains("free text"), "help line names it");
    }

    /// The broadcast picker's title override lands on the modal border
    /// so the target list is visible while picking.
    #[test]
    fn with_title_overrides_modal_border_title() {
        let mut picker =
            SnippetPicker::new(make_rows(), String::new()).with_title("Broadcast to 2: ws-a, ws-b");
        let screen = render(&mut picker, 92, 26);
        assert!(
            screen.contains("Broadcast to 2: ws-a, ws-b"),
            "custom title rendered:\n{screen}"
        );
        assert!(!screen.contains(" Snippets "), "default title replaced");
    }

    #[test]
    fn typing_filters_and_recovers_on_backspace() {
        let mut picker = SnippetPicker::new(make_rows(), String::new());
        let out = picker.on_key(&ke('r'));
        assert!(out.is_none(), "no auto-submit on incomplete key");
        // `r` matches `rev` by key (plus others by description); the
        // exact-key auto-submit hasn't fired, so `rev` is just visible.
        assert!(
            picker
                .visible_indices
                .iter()
                .any(|&i| picker.rows[i].key == "rev")
        );

        let _ = picker.on_key(&key(Key::Backspace));
        assert_eq!(picker.filter, "");
        assert_eq!(picker.visible_indices.len(), 3);
    }

    /// Headline test: typing a full snippet key auto-submits.
    /// `]rev` (where `r`, `e`, `v` are streamed in) should land
    /// on `ChoicePicked` after the `v` — no Enter.
    #[test]
    fn typing_full_key_auto_submits() {
        let mut picker = SnippetPicker::new(make_rows(), String::new());
        assert!(picker.on_key(&ke('r')).is_none());
        assert!(picker.on_key(&ke('e')).is_none());
        let out = picker.on_key(&ke('v'));
        match out {
            Some(Msg::ChoicePicked(v)) => {
                assert_eq!(v, vec![ChoicePayload::Text("rev".into())]);
            }
            other => panic!("expected ChoicePicked, got {other:?}"),
        }
    }

    /// Description-matching must NOT break the exact-key fast path.
    /// Here another snippet's *description* contains "rev" ("Review
    /// the branch"), so the display filter shows two rows — but the
    /// sole key-prefix match still auto-submits on the exact key.
    #[test]
    fn description_match_does_not_block_exact_key_auto_submit() {
        let rows = vec![
            PickerRow::new(
                "audit",
                &snip(
                    "Review",
                    "Review the branch",
                    "audit body",
                    SnippetOrigin::Global,
                ),
            ),
            PickerRow::new(
                "rev",
                &snip(
                    "Review",
                    "Review diff",
                    "review please",
                    SnippetOrigin::Global,
                ),
            ),
        ];
        let mut picker = SnippetPicker::new(rows, String::new());
        assert!(picker.on_key(&ke('r')).is_none());
        assert!(picker.on_key(&ke('e')).is_none());
        // Both rows are visible (audit's description matches "rev"),
        // but only `rev`'s key prefixes "rev", so it auto-submits.
        let out = picker.on_key(&ke('v'));
        match out {
            Some(Msg::ChoicePicked(v)) => {
                assert_eq!(v, vec![ChoicePayload::Text("rev".into())])
            }
            other => panic!("expected ChoicePicked, got {other:?}"),
        }
    }

    /// Filtering matches description text, not just the key.
    #[test]
    fn filter_matches_description() {
        let mut picker = SnippetPicker::new(make_rows(), String::new());
        // "diff" appears only in rev's description ("Review diff").
        for c in "diff".chars() {
            let _ = picker.on_key(&ke(c));
        }
        let keys: Vec<_> = picker
            .visible_indices
            .iter()
            .map(|&i| picker.rows[i].key.clone())
            .collect();
        assert_eq!(keys, vec!["rev"]);
    }

    /// Mounted with an initial filter (the `]r` flow).
    #[test]
    fn initial_filter_carries_through() {
        let mut picker = SnippetPicker::new(make_rows(), "re".into());
        assert!(
            picker
                .visible_indices
                .iter()
                .any(|&i| picker.rows[i].key == "rev")
        );
        match picker.on_key(&ke('v')) {
            Some(Msg::ChoicePicked(v)) => {
                assert_eq!(v, vec![ChoicePayload::Text("rev".into())])
            }
            other => panic!("expected ChoicePicked, got {other:?}"),
        }
    }

    /// Enter on the cursor submits even when the filter prefixes
    /// multiple keys (the discovery / manual-pick path).
    #[test]
    fn enter_submits_cursor_selection() {
        // ping, pr, rev — both ping and pr start with `p`, same category
        // so they stay adjacent in display order.
        let rows = vec![
            PickerRow::new(
                "ping",
                &snip("Chores", "Ping", "ping body", SnippetOrigin::Global),
            ),
            PickerRow::new(
                "pr",
                &snip("Chores", "Open PR", "pr body", SnippetOrigin::Global),
            ),
            PickerRow::new(
                "rev",
                &snip(
                    "Chores",
                    "Review diff",
                    "review body",
                    SnippetOrigin::Global,
                ),
            ),
        ];
        let mut picker = SnippetPicker::new(rows, String::new());
        assert!(picker.on_key(&ke('p')).is_none());
        assert_eq!(picker.visible_indices.len(), 2);
        let _ = picker.on_key(&key(Key::Down));
        match picker.on_key(&key(Key::Enter)) {
            Some(Msg::ChoicePicked(v)) => {
                assert_eq!(v, vec![ChoicePayload::Text("pr".into())])
            }
            other => panic!("expected ChoicePicked, got {other:?}"),
        }
    }

    #[test]
    fn esc_dismisses() {
        let mut picker = SnippetPicker::new(make_rows(), String::new());
        let out = picker.on_key(&key(Key::Esc));
        assert!(matches!(out, Some(Msg::ModalDismissed)));
    }

    #[test]
    fn ctrl_c_dismisses() {
        let mut picker = SnippetPicker::new(make_rows(), String::new());
        let out = picker.on_key(&KeyEvent::new(Key::Char('c'), KeyModifiers::CONTROL));
        assert!(matches!(out, Some(Msg::ModalDismissed)));
    }

    #[test]
    fn picker_with_no_snippets_handles_typing_safely() {
        let mut picker = SnippetPicker::new(vec![], String::new());
        assert!(picker.cursor.is_none());
        assert!(picker.on_key(&ke('x')).is_none());
        assert!(picker.visible_indices.is_empty());
        assert!(picker.on_key(&key(Key::Enter)).is_none());
    }

    #[test]
    fn no_matches_clears_cursor_and_enter_is_noop() {
        let mut picker = SnippetPicker::new(make_rows(), "zzqq".into());
        assert!(picker.visible_indices.is_empty());
        assert!(picker.cursor.is_none());
        assert!(picker.on_key(&key(Key::Enter)).is_none());
    }

    /// Case-tie break: with both `rev` and `Rev` defined, matching is
    /// case-insensitive so BOTH prefix-match either typed form — which
    /// used to permanently disable auto-submit. Now the case-exact
    /// full-key match wins the tie; a genuinely ambiguous prefix
    /// (`re`) still never auto-fires.
    #[test]
    fn case_tied_keys_auto_submit_on_the_exact_match() {
        let rows = vec![
            PickerRow::new(
                "Rev",
                &snip("Review", "Big review", "big body", SnippetOrigin::Global),
            ),
            PickerRow::new(
                "rev",
                &snip(
                    "Review",
                    "Small review",
                    "small body",
                    SnippetOrigin::Global,
                ),
            ),
        ];
        // Lowercase `rev` typed → the lowercase key submits.
        let mut picker = SnippetPicker::new(rows.clone(), String::new());
        assert!(picker.on_key(&ke('r')).is_none());
        assert!(picker.on_key(&ke('e')).is_none());
        match picker.on_key(&ke('v')) {
            Some(Msg::ChoicePicked(v)) => {
                assert_eq!(v, vec![ChoicePayload::Text("rev".into())])
            }
            other => panic!("expected exact-case auto-submit of `rev`, got {other:?}"),
        }
        // Uppercase `Rev` typed → the uppercase key submits.
        let mut picker = SnippetPicker::new(rows, String::new());
        for c in ['R', 'e'] {
            assert!(picker.on_key(&ke(c)).is_none());
        }
        match picker.on_key(&ke('v')) {
            Some(Msg::ChoicePicked(v)) => {
                assert_eq!(v, vec![ChoicePayload::Text("Rev".into())])
            }
            other => panic!("expected exact-case auto-submit of `Rev`, got {other:?}"),
        }
    }

    #[test]
    fn filter_is_case_insensitive() {
        let mut picker = SnippetPicker::new(make_rows(), String::new());
        for c in ['R', 'E', 'V'] {
            let out = picker.on_key(&ke(c));
            if let Some(Msg::ChoicePicked(v)) = out {
                assert_eq!(v, vec![ChoicePayload::Text("rev".into())]);
                return;
            }
        }
        panic!("expected auto-submit on case-insensitive full key");
    }

    /// The rendered modal shows category headers, a preview of the
    /// highlighted body, and the total count.
    #[test]
    fn render_shows_categories_preview_and_count() {
        let mut picker = SnippetPicker::new(make_rows(), String::new());
        let out = render(&mut picker, 92, 20);
        assert!(out.contains("Snippets"), "title: {out}");
        assert!(out.contains("Review"), "category header: {out}");
        assert!(out.contains("Git & PR"), "category header: {out}");
        assert!(out.contains("]rev"), "row key: {out}");
        // Cursor starts on the first row (pr, key-sorted within nothing —
        // actually first display row is pr under Git & PR? order is
        // Review→rev first). Preview shows the highlighted body.
        assert!(out.contains("3/3"), "count in header: {out}");
        assert!(
            out.contains("review please") || out.contains("open pr"),
            "preview body: {out}"
        );
    }

    /// A library taller than the list viewport scrolls to keep the
    /// cursor visible instead of running off the bottom.
    #[test]
    fn long_list_scrolls_to_keep_cursor_visible() {
        let mut rows: Vec<PickerRow> = (0..40)
            .map(|i| {
                PickerRow::new(
                    // zero-padded so key order is stable and sorted
                    Box::leak(format!("k{i:02}").into_boxed_str()),
                    &snip("Chores", "desc", "body", SnippetOrigin::Global),
                )
            })
            .collect();
        rows.sort_by(|a, b| a.key.cmp(&b.key));
        let mut picker = SnippetPicker::new(rows, String::new());
        // Move the cursor to the last row.
        for _ in 0..39 {
            let _ = picker.on_key(&key(Key::Down));
        }
        assert_eq!(picker.cursor, Some(39));
        let _ = render(&mut picker, 92, 20);
        // The scroll offset advanced so the bottom row is on screen.
        assert!(
            picker.list_scroll > 0,
            "list scrolled: {}",
            picker.list_scroll
        );
    }

    // ── Recent group (#252) ──────────────────────────────────────────

    /// A recently-used snippet floats into a "Recent" group at the top,
    /// with the cursor on it — so reopening and pressing Enter re-runs
    /// it. It stays in its real category too (a shortcut, not a move).
    #[test]
    fn recent_group_floats_to_top_with_cursor() {
        let picker = SnippetPicker::new(make_rows(), String::new()).with_recent(vec!["pr".into()]);
        assert_eq!(picker.recent_count, 1);
        assert_eq!(picker.cursor, Some(0));
        assert_eq!(picker.rows[picker.visible_indices[0]].key, "pr");
        let pr_seen = picker
            .visible_indices
            .iter()
            .filter(|&&i| picker.rows[i].key == "pr")
            .count();
        assert_eq!(
            pr_seen, 2,
            "recent is a shortcut — pr also under its category"
        );
    }

    /// Enter on a freshly-opened picker submits the most-recent snippet.
    #[test]
    fn enter_on_fresh_open_submits_most_recent() {
        let mut picker =
            SnippetPicker::new(make_rows(), String::new()).with_recent(vec!["rev".into()]);
        match picker.on_key(&key(Key::Enter)) {
            Some(Msg::ChoicePicked(v)) => {
                assert_eq!(v, vec![ChoicePayload::Text("rev".into())])
            }
            other => panic!("expected most-recent submit, got {other:?}"),
        }
    }

    /// A search steps the Recent group aside — typing means "find", not
    /// "repeat" — so `pr` stops being duplicated at the top.
    #[test]
    fn filtering_hides_the_recent_group() {
        let mut picker =
            SnippetPicker::new(make_rows(), String::new()).with_recent(vec!["pr".into()]);
        assert_eq!(picker.recent_count, 1);
        // `pr` shows twice on an empty filter: once in Recent, once in
        // its category.
        assert_eq!(
            picker
                .visible_indices
                .iter()
                .filter(|&&i| picker.rows[i].key == "pr")
                .count(),
            2,
        );
        // Filter to `pr` by key: the recent group is gone, so `pr`
        // appears exactly once.
        let _ = picker.on_key(&ke('p'));
        let _ = picker.on_key(&ke('r'));
        assert_eq!(picker.recent_count, 0, "a filter drops the recent group");
        assert_eq!(
            picker
                .visible_indices
                .iter()
                .filter(|&&i| picker.rows[i].key == "pr")
                .count(),
            1,
            "no recent duplicate while filtering",
        );
    }

    /// Recent keys not present in the library are dropped; MRU order is
    /// preserved for the ones that survive.
    #[test]
    fn stale_recent_key_ignored_order_preserved() {
        let picker = SnippetPicker::new(make_rows(), String::new()).with_recent(vec![
            "ghost".into(),
            "rev".into(),
            "pr".into(),
        ]);
        assert_eq!(picker.recent_count, 2);
        assert_eq!(picker.rows[picker.visible_indices[0]].key, "rev");
        assert_eq!(picker.rows[picker.visible_indices[1]].key, "pr");
    }

    /// The rendered modal labels the group "Recent" when recents exist.
    #[test]
    fn render_shows_recent_header() {
        let mut picker =
            SnippetPicker::new(make_rows(), String::new()).with_recent(vec!["rev".into()]);
        let out = render(&mut picker, 92, 20);
        assert!(out.contains("Recent"), "recent header: {out}");
    }
}
