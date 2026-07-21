//! Full markdown renderer for the description-reader modal (#448).
//!
//! The inline teaser path (`comment_render` + `right_pane::markdown`)
//! is deliberately hand-rolled and lossy — it strips noise and degrades
//! links to inert styled text because a one-line teaser doesn't need
//! more. The reader modal is the opposite: it's the "read the whole
//! thing" surface, so it renders *real* markdown — headings, emphasis,
//! bullet/ordered lists (nested), blockquotes, fenced code, inline code,
//! links, images, thematic breaks, and GFM tables.
//!
//! Rather than extend the teaser renderer, this parses through
//! `pulldown-cmark` (CommonMark + GFM) and lays the event stream out
//! into width-wrapped `Line`s. Links keep their destination: each
//! rendered link records the `(line, column-range) → url` span in
//! [`RenderedDoc::links`] so the modal can hit-test a click and open the
//! URL — the ratatui-drawn overlay can't emit OSC-8, so click-mapping is
//! how "clickable links" is delivered.
//!
//! Column math is char-based (like the teaser renderer), so link
//! hit-testing is exact for ASCII and drifts by the excess width of any
//! wide/CJK glyphs earlier on the line — an accepted tradeoff shared
//! with `comment_render`.

use crate::theme::Theme;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// A clickable link region in the rendered document: a half-open
/// `[start, end)` column range on `line`, resolving to `url`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkHit {
    pub line: usize,
    pub start: u16,
    pub end: u16,
    pub url: String,
}

/// A markdown body laid out for the reader modal: styled lines plus the
/// link click-map.
#[derive(Debug, Clone, Default)]
pub struct RenderedDoc {
    pub lines: Vec<Line<'static>>,
    pub links: Vec<LinkHit>,
}

impl RenderedDoc {
    /// The URL of the link covering `col` on `line`, if any.
    pub fn link_at(&self, line: usize, col: u16) -> Option<&str> {
        self.links
            .iter()
            .find(|l| l.line == line && col >= l.start && col < l.end)
            .map(|l| l.url.as_str())
    }
}

/// Render `src` to a [`RenderedDoc`] fitted to `width` content columns.
pub fn render_markdown(src: &str, width: u16, theme: &Theme) -> RenderedDoc {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(src, opts);

    let mut r = Renderer::new(width.max(1), theme);
    for ev in parser {
        r.event(ev);
    }
    r.finish()
}

/// A styled text fragment with an optional link destination.
#[derive(Clone)]
struct Tok {
    text: String,
    style: Style,
    url: Option<String>,
}

impl Tok {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Self {
            text: text.into(),
            style,
            url: None,
        }
    }
    fn width(&self) -> usize {
        self.text.chars().count()
    }
}

/// A piece of the current inline run.
enum Piece {
    Text(Tok),
    HardBreak,
}

/// One nesting frame contributing a line prefix (blockquote marker or
/// list-item indent).
struct Frame {
    /// Prefix for the first line under this frame (the list bullet /
    /// number). Same as `cont` for a blockquote.
    first: Vec<Tok>,
    /// Prefix for continuation (wrapped) lines: indentation only.
    cont: Vec<Tok>,
}

/// State for one ordered/unordered list level.
struct ListCtx {
    /// `Some(n)` for an ordered list (next item number), `None` for a
    /// bullet list.
    next_number: Option<u64>,
}

/// A table being accumulated between `Table` start/end.
struct TableCtx {
    rows: Vec<Vec<String>>,
    /// Whether the row currently being filled is the header row.
    in_head: bool,
    cell: String,
}

struct Renderer<'t> {
    width: usize,
    theme: &'t Theme,

    lines: Vec<Line<'static>>,
    links: Vec<LinkHit>,

    inline: Vec<Piece>,
    frames: Vec<Frame>,
    /// Index of the frame whose first-line marker hasn't rendered yet.
    pending_marker: Option<usize>,

    lists: Vec<ListCtx>,

    // inline style nesting
    bold: u32,
    italic: u32,
    strike: u32,
    link_url: Option<String>,

    /// `Some(buffer)` while inside a fenced/indented code block; text
    /// events append to it and it renders verbatim on block end.
    code_block: Option<String>,
    table: Option<TableCtx>,
}

impl<'t> Renderer<'t> {
    fn new(width: u16, theme: &'t Theme) -> Self {
        Self {
            width: width as usize,
            theme,
            lines: Vec::new(),
            links: Vec::new(),
            inline: Vec::new(),
            frames: Vec::new(),
            pending_marker: None,
            lists: Vec::new(),
            bold: 0,
            italic: 0,
            strike: 0,
            link_url: None,
            code_block: None,
            table: None,
        }
    }

    fn finish(mut self) -> RenderedDoc {
        self.flush_inline();
        // Drop a trailing blank line so the modal doesn't open with a
        // gap of empty rows below short bodies.
        while matches!(self.lines.last(), Some(l) if line_is_blank(l)) {
            self.lines.pop();
        }
        RenderedDoc {
            lines: self.lines,
            links: self.links,
        }
    }

    fn inline_style(&self) -> Style {
        let mut s = Style::default().fg(self.theme.text_strong);
        if self.bold > 0 {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.link_url.is_some() {
            s = s.fg(self.theme.accent).add_modifier(Modifier::UNDERLINED);
        }
        s
    }

    fn push_text(&mut self, text: &str) {
        if let Some(t) = &mut self.table {
            t.cell.push_str(text);
            return;
        }
        let style = self.inline_style();
        let url = self.link_url.clone();
        self.inline.push(Piece::Text(Tok {
            text: text.to_string(),
            style,
            url,
        }));
    }

    fn event(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(t) => {
                if let Some(buf) = &mut self.code_block {
                    buf.push_str(&t);
                } else {
                    self.push_text(&t);
                }
            }
            Event::Code(t) => {
                if self.table.is_some() {
                    self.push_text(&t);
                } else {
                    let style = Style::default().fg(self.theme.accent).bg(self.theme.fill);
                    let url = self.link_url.clone();
                    self.inline.push(Piece::Text(Tok {
                        text: format!(" {t} "),
                        style,
                        url,
                    }));
                }
            }
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => {
                if self.table.is_some() {
                    self.push_text(" ");
                } else {
                    self.inline.push(Piece::HardBreak);
                }
            }
            Event::Rule => {
                self.flush_inline();
                self.emit_rule();
            }
            Event::TaskListMarker(done) => {
                let mark = if done { "☑ " } else { "☐ " };
                self.inline.push(Piece::Text(Tok::new(
                    mark,
                    Style::default().fg(self.theme.accent),
                )));
            }
            // Raw HTML / footnotes: render nothing structural. Inline
            // HTML text is dropped (the teaser strips it too); block
            // HTML is ignored.
            Event::Html(_) | Event::InlineHtml(_) | Event::FootnoteReference(_) => {}
            // Math extensions are not enabled; ignore defensively.
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {}
            Tag::Heading { .. } => {
                self.flush_inline();
                // Heading text is styled at End via a whole-run restyle
                // (`restyle_heading`, which reads the level off `TagEnd`);
                // bumping bold here makes the inline text read bold as it
                // accumulates.
                self.bold += 1;
            }
            Tag::BlockQuote(_) => {
                let marker = vec![Tok::new("▎ ", Style::default().fg(self.theme.text_dim))];
                self.frames.push(Frame {
                    first: marker.clone(),
                    cont: marker,
                });
            }
            Tag::CodeBlock(_) => {
                self.flush_inline();
                self.code_block = Some(String::new());
            }
            Tag::List(start) => {
                self.flush_inline();
                self.lists.push(ListCtx { next_number: start });
            }
            Tag::Item => {
                // Nesting indent is inherited from the enclosing item's
                // continuation prefix (already in `base`); only this
                // item's own marker is added here.
                let base = self.frames_cont_prefix();
                let marker_text = match self.lists.last_mut() {
                    Some(ListCtx {
                        next_number: Some(n),
                    }) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                let mut first = base.clone();
                first.push(Tok::new(
                    marker_text.clone(),
                    Style::default().fg(self.theme.accent),
                ));
                let mut cont = base;
                cont.push(Tok::new(
                    " ".repeat(marker_text.chars().count()),
                    Style::default(),
                ));
                let idx = self.frames.len();
                self.frames.push(Frame { first, cont });
                self.pending_marker = Some(idx);
            }
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => {
                self.link_url = Some(dest_url.to_string());
            }
            Tag::Image { dest_url, .. } => {
                // Render images as a labelled, clickable stand-in — a
                // terminal can't show the bitmap, but the alt text +
                // link keeps the reference usable.
                self.link_url = Some(dest_url.to_string());
                self.push_text("🖼 ");
            }
            Tag::Table(_) => {
                self.flush_inline();
                self.table = Some(TableCtx {
                    rows: Vec::new(),
                    in_head: false,
                    cell: String::new(),
                });
            }
            Tag::TableHead => {
                if let Some(t) = &mut self.table {
                    t.in_head = true;
                    t.rows.push(Vec::new());
                }
            }
            Tag::TableRow => {
                if let Some(t) = &mut self.table {
                    t.rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(t) = &mut self.table {
                    t.cell.clear();
                }
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_inline();
                self.emit_blank();
            }
            TagEnd::Heading(level) => {
                self.restyle_heading(level);
                self.flush_inline();
                self.bold = self.bold.saturating_sub(1);
                self.emit_blank();
            }
            TagEnd::BlockQuote(_) => {
                self.flush_inline();
                self.frames.pop();
                self.emit_blank();
            }
            TagEnd::CodeBlock => {
                if let Some(buf) = self.code_block.take() {
                    self.emit_code_block(&buf);
                    self.emit_blank();
                }
            }
            TagEnd::List(_) => {
                self.lists.pop();
                if self.lists.is_empty() {
                    self.emit_blank();
                }
            }
            TagEnd::Item => {
                self.flush_inline();
                self.frames.pop();
                // If the item produced no line at all, the marker never
                // rendered — drop the pending flag so it can't leak onto
                // the next item.
                self.pending_marker = None;
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => self.link_url = None,
            TagEnd::Image => self.link_url = None,
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.emit_table(t);
                    self.emit_blank();
                }
            }
            TagEnd::TableCell => {
                if let Some(t) = &mut self.table {
                    let cell = t.cell.trim().to_string();
                    if let Some(row) = t.rows.last_mut() {
                        row.push(cell);
                    }
                    t.cell.clear();
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.table {
                    t.in_head = false;
                }
            }
            _ => {}
        }
    }

    /// Render a fenced/indented code block: each line verbatim (no
    /// wrap; clipped to width) on a distinct filled background so the
    /// block reads as code.
    fn emit_code_block(&mut self, text: &str) {
        let bg = self.theme.fill;
        let fg = self.theme.text_strong;
        // Trim the single trailing newline the parser appends, but keep
        // interior blank lines.
        let body = text.strip_suffix('\n').unwrap_or(text);
        let base = self.frames_cont_prefix();
        let base_w = prefix_toks_width(&base);
        let avail = self.width.saturating_sub(base_w);
        for raw in body.split('\n') {
            let mut spans = self.prefix_spans(&base);
            let content = clip(raw, avail);
            let padded = format!("{content:<avail$}");
            spans.push(Span::styled(padded, Style::default().fg(fg).bg(bg)));
            self.lines.push(Line::from(spans));
        }
    }

    /// The continuation prefix built from every frame (blockquote
    /// markers + enclosing list-item indents), used as the base a list
    /// marker sits on.
    fn frames_cont_prefix(&self) -> Vec<Tok> {
        let mut out = Vec::new();
        for f in &self.frames {
            // Only blockquote frames (first == cont) belong to the base
            // for a fresh item marker; list-item frames are the ones
            // being extended, so include their continuation indent too.
            out.extend(f.cont.iter().cloned());
        }
        out
    }

    fn prefix_spans(&self, prefix: &[Tok]) -> Vec<Span<'static>> {
        prefix
            .iter()
            .map(|t| Span::styled(t.text.clone(), t.style))
            .collect()
    }

    fn emit_blank(&mut self) {
        if matches!(self.lines.last(), Some(l) if line_is_blank(l)) {
            return;
        }
        self.lines.push(Line::from(String::new()));
    }

    fn emit_rule(&mut self) {
        let base = self.frames_cont_prefix();
        let mut spans = self.prefix_spans(&base);
        let w = self.width.saturating_sub(prefix_width(&spans)).max(1);
        spans.push(Span::styled(
            "─".repeat(w),
            Style::default().fg(self.theme.text_dim),
        ));
        self.lines.push(Line::from(spans));
    }

    /// Restyle the just-accumulated heading run so the whole thing reads
    /// as a heading (accent for h1/h2, strong otherwise). The bold bump
    /// from `start` already made the text bold.
    fn restyle_heading(&mut self, level: HeadingLevel) {
        let color = if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) {
            self.theme.accent
        } else {
            self.theme.text_strong
        };
        for p in &mut self.inline {
            if let Piece::Text(t) = p {
                if t.url.is_none() {
                    t.style = t.style.fg(color).add_modifier(Modifier::BOLD);
                }
            }
        }
    }

    fn flush_inline(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let pieces = std::mem::take(&mut self.inline);
        let first_prefix = self.take_first_prefix();
        let cont_prefix = self.frames_cont_prefix();
        let first_w = prefix_toks_width(&first_prefix);
        let cont_w = prefix_toks_width(&cont_prefix);

        // Split on hard breaks; wrap each segment independently.
        let mut segments: Vec<Vec<Tok>> = vec![Vec::new()];
        for p in pieces {
            match p {
                Piece::Text(t) => segments.last_mut().expect("seed segment").push(t),
                Piece::HardBreak => segments.push(Vec::new()),
            }
        }

        let mut first_line = true;
        for seg in segments {
            let words = split_words(&seg);
            let mut packed = pack_words(&words, self.width, first_w, cont_w, first_line);
            if packed.is_empty() {
                packed.push(Vec::new());
            }
            for (i, toks) in packed.into_iter().enumerate() {
                let use_first = first_line && i == 0;
                let prefix = if use_first {
                    &first_prefix
                } else {
                    &cont_prefix
                };
                self.emit_line(prefix, &toks);
            }
            first_line = false;
        }
    }

    /// Build the first-line prefix, consuming the pending list marker.
    fn take_first_prefix(&mut self) -> Vec<Tok> {
        let mut out = Vec::new();
        for (i, f) in self.frames.iter().enumerate() {
            if Some(i) == self.pending_marker {
                out.extend(f.first.iter().cloned());
            } else {
                out.extend(f.cont.iter().cloned());
            }
        }
        self.pending_marker = None;
        out
    }

    fn emit_line(&mut self, prefix: &[Tok], toks: &[Tok]) {
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut col: u16 = 0;
        for t in prefix {
            col = col.saturating_add(t.width() as u16);
            spans.push(Span::styled(t.text.clone(), t.style));
        }
        for t in toks {
            let start = col;
            let w = t.width() as u16;
            col = col.saturating_add(w);
            if let Some(url) = &t.url {
                self.links.push(LinkHit {
                    line: self.lines.len(),
                    start,
                    end: col,
                    url: url.clone(),
                });
            }
            spans.push(Span::styled(t.text.clone(), t.style));
        }
        self.lines.push(Line::from(spans));
    }

    fn emit_table(&mut self, t: TableCtx) {
        let rows = t.rows;
        if rows.is_empty() {
            return;
        }
        let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
        if cols == 0 {
            return;
        }
        // Natural column widths, then shrink proportionally to fit.
        let base = self.frames_cont_prefix();
        let base_w = prefix_toks_width(&base);
        // Budget: total width minus prefix minus separators (" │ ").
        let sep_cols = cols.saturating_sub(1) * 3;
        let avail = self.width.saturating_sub(base_w + sep_cols).max(cols);
        let mut widths = vec![0usize; cols];
        for row in &rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(cell.chars().count());
            }
        }
        shrink_to_fit(&mut widths, avail);

        for (ri, row) in rows.iter().enumerate() {
            let mut spans = self.prefix_spans(&base);
            for (c, cw) in widths.iter().enumerate() {
                if c > 0 {
                    spans.push(Span::styled(
                        " │ ",
                        Style::default().fg(self.theme.text_dim),
                    ));
                }
                let cell = row.get(c).map(String::as_str).unwrap_or("");
                let text = pad_clip(cell, *cw);
                let style = if ri == 0 {
                    Style::default()
                        .fg(self.theme.text_strong)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(self.theme.text_strong)
                };
                spans.push(Span::styled(text, style));
            }
            self.lines.push(Line::from(spans));
            if ri == 0 {
                // Header underline.
                let mut sep = self.prefix_spans(&base);
                for (c, w) in widths.iter().enumerate() {
                    if c > 0 {
                        sep.push(Span::styled(
                            "─┼─",
                            Style::default().fg(self.theme.text_dim),
                        ));
                    }
                    sep.push(Span::styled(
                        "─".repeat(*w),
                        Style::default().fg(self.theme.text_dim),
                    ));
                }
                self.lines.push(Line::from(sep));
            }
        }
    }
}

/// A word: a run of styled fragments with no interior spaces.
type Word = Vec<Tok>;

/// Split a styled segment into words (space-delimited), preserving each
/// fragment's style and link.
fn split_words(seg: &[Tok]) -> Vec<Word> {
    let mut words: Vec<Word> = Vec::new();
    let mut cur: Word = Vec::new();
    for t in seg {
        let mut parts = t.text.split(' ').peekable();
        while let Some(part) = parts.next() {
            if !part.is_empty() {
                cur.push(Tok {
                    text: part.to_string(),
                    style: t.style,
                    url: t.url.clone(),
                });
            }
            if parts.peek().is_some() {
                // a space boundary ends the current word
                if !cur.is_empty() {
                    words.push(std::mem::take(&mut cur));
                }
            }
        }
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

fn word_width(w: &Word) -> usize {
    w.iter().map(Tok::width).sum()
}

/// Greedily pack words into lines. `first_w` / `cont_w` are the prefix
/// widths already reserved on the first vs. continuation lines.
fn pack_words(
    words: &[Word],
    total: usize,
    first_w: usize,
    cont_w: usize,
    first_is_first_line: bool,
) -> Vec<Vec<Tok>> {
    let mut lines: Vec<Vec<Tok>> = Vec::new();
    let mut cur: Vec<Tok> = Vec::new();
    let mut cur_cols = 0usize;
    let mut on_first = first_is_first_line;

    let avail = |on_first: bool| -> usize {
        let reserved = if on_first { first_w } else { cont_w };
        total.saturating_sub(reserved).max(1)
    };

    for w in words {
        let ww = word_width(w);
        let space = if cur.is_empty() { 0 } else { 1 };
        if !cur.is_empty() && cur_cols + space + ww > avail(on_first) {
            lines.push(std::mem::take(&mut cur));
            cur_cols = 0;
            on_first = false;
        }
        if cur.is_empty() && ww > avail(on_first) {
            // A single word longer than the line: hard-split by chars.
            for chunk in hard_split(w, avail(on_first)) {
                if chunk_width(&chunk) >= avail(on_first) {
                    lines.push(chunk);
                    on_first = false;
                } else {
                    cur = chunk;
                    cur_cols = chunk_width(&cur);
                }
            }
            continue;
        }
        if !cur.is_empty() {
            cur.push(Tok::new(" ", Style::default()));
            cur_cols += 1;
        }
        cur.extend(w.iter().cloned());
        cur_cols += ww;
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

fn chunk_width(toks: &[Tok]) -> usize {
    toks.iter().map(Tok::width).sum()
}

/// Break an over-long word into `max`-column chunks, preserving style.
fn hard_split(word: &Word, max: usize) -> Vec<Vec<Tok>> {
    let max = max.max(1);
    let mut out: Vec<Vec<Tok>> = Vec::new();
    let mut cur: Vec<Tok> = Vec::new();
    let mut cur_cols = 0usize;
    for frag in word {
        for ch in frag.text.chars() {
            if cur_cols >= max {
                out.push(std::mem::take(&mut cur));
                cur_cols = 0;
            }
            cur.push(Tok {
                text: ch.to_string(),
                style: frag.style,
                url: frag.url.clone(),
            });
            cur_cols += 1;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    // Merge adjacent single-char toks of equal style back into runs so
    // the span count stays sane.
    out.into_iter().map(coalesce).collect()
}

/// Merge neighbouring toks that share style + url into one run.
fn coalesce(toks: Vec<Tok>) -> Vec<Tok> {
    let mut out: Vec<Tok> = Vec::new();
    for t in toks {
        match out.last_mut() {
            Some(last) if last.style == t.style && last.url == t.url => {
                last.text.push_str(&t.text);
            }
            _ => out.push(t),
        }
    }
    out
}

fn prefix_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

fn prefix_toks_width(prefix: &[Tok]) -> usize {
    prefix.iter().map(Tok::width).sum()
}

fn line_is_blank(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// Pad or ellipsis-clip a cell to exactly `w` columns.
fn pad_clip(s: &str, w: usize) -> String {
    let len = s.chars().count();
    if len == w {
        s.to_string()
    } else if len < w {
        format!("{s}{}", " ".repeat(w - len))
    } else if w == 0 {
        String::new()
    } else if w == 1 {
        "…".to_string()
    } else {
        let kept: String = s.chars().take(w - 1).collect();
        format!("{kept}…")
    }
}

/// Shrink column widths so their sum fits `avail`, trimming the widest
/// columns first so narrow columns stay readable.
fn shrink_to_fit(widths: &mut [usize], avail: usize) {
    let total: usize = widths.iter().sum();
    if total <= avail {
        return;
    }
    let mut over = total - avail;
    while over > 0 {
        // Trim the current widest column by one.
        let Some((idx, _)) = widths
            .iter()
            .enumerate()
            .max_by_key(|(_, w)| **w)
            .filter(|(_, w)| **w > 1)
        else {
            break;
        };
        widths[idx] -= 1;
        over -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::LAZYBOX_DARK;

    fn flat(doc: &RenderedDoc) -> Vec<String> {
        doc.lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn render(src: &str, width: u16) -> RenderedDoc {
        render_markdown(src, width, &LAZYBOX_DARK)
    }

    #[test]
    fn heading_and_paragraph() {
        let doc = render("# Hello\n\nWorld body.", 40);
        let text = flat(&doc).join("\n");
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("World body."), "{text}");
    }

    #[test]
    fn bullet_list_renders_markers() {
        let doc = render("- one\n- two\n- three", 40);
        let lines = flat(&doc);
        assert!(lines.iter().any(|l| l.contains("• one")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("• two")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("• three")), "{lines:?}");
    }

    #[test]
    fn ordered_list_numbers() {
        let doc = render("1. first\n2. second\n3. third", 40);
        let lines = flat(&doc);
        assert!(lines.iter().any(|l| l.contains("1. first")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("2. second")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("3. third")), "{lines:?}");
    }

    #[test]
    fn nested_list_indents_child_under_parent() {
        let doc = render("- parent\n  - child", 40);
        let lines = flat(&doc);
        let parent = lines.iter().find(|l| l.contains("parent")).unwrap();
        let child = lines.iter().find(|l| l.contains("child")).unwrap();
        let parent_indent = parent.find('•').unwrap();
        let child_indent = child.find('•').unwrap();
        assert!(
            child_indent > parent_indent,
            "child bullet must sit further right: {lines:?}",
        );
    }

    #[test]
    fn blockquote_gets_bar_prefix() {
        let doc = render("> quoted line", 40);
        let lines = flat(&doc);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("▎") && l.contains("quoted line")),
            "{lines:?}",
        );
    }

    #[test]
    fn code_block_is_verbatim() {
        let doc = render("```\nlet x = 1;\nfoo();\n```", 40);
        let lines = flat(&doc);
        assert!(lines.iter().any(|l| l.contains("let x = 1;")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("foo();")), "{lines:?}");
    }

    #[test]
    fn link_records_url_at_its_columns() {
        let doc = render("see [docs](https://example.com/x) here", 60);
        let hit = doc.links.first().expect("one link recorded");
        assert_eq!(hit.url, "https://example.com/x");
        // The link text is present on that line and `link_at` resolves
        // the whole recorded span.
        assert_eq!(
            doc.link_at(hit.line, hit.start),
            Some("https://example.com/x")
        );
        assert_eq!(
            doc.link_at(hit.line, hit.end - 1),
            Some("https://example.com/x")
        );
        assert_eq!(doc.link_at(hit.line, hit.end), None);
        let text: String = doc.lines[hit.line]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert!(text.contains("docs"), "{text}");
    }

    #[test]
    fn table_renders_header_separator_and_rows() {
        let src = "| A | B |\n| --- | --- |\n| 1 | 2 |\n| 3 | 4 |";
        let doc = render(src, 40);
        let lines = flat(&doc);
        assert!(
            lines.iter().any(|l| l.contains('A') && l.contains('B')),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains('┼')),
            "header separator: {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains('1') && l.contains('2')),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains('3') && l.contains('4')),
            "{lines:?}"
        );
    }

    #[test]
    fn long_paragraph_wraps_to_width() {
        let src = "word ".repeat(60);
        let doc = render(&src, 20);
        assert!(doc.lines.len() > 1, "a long paragraph must wrap");
        for l in &doc.lines {
            let w: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(w <= 20, "line exceeds width: {w}");
        }
    }

    #[test]
    fn emphasis_text_survives() {
        let doc = render("a **bold** and *italic* and `code` word", 60);
        let text = flat(&doc).join("\n");
        assert!(text.contains("bold"), "{text}");
        assert!(text.contains("italic"), "{text}");
        assert!(text.contains("code"), "{text}");
    }

    #[test]
    fn empty_source_renders_nothing() {
        let doc = render("", 40);
        assert!(doc.lines.is_empty());
        assert!(doc.links.is_empty());
    }
}
