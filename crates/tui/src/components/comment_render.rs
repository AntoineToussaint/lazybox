//! Comment rendering for the activity feed.
//!
//! GitHub comments (and Linear, eventually) come back as Markdown
//! soup with the occasional HTML comment tucked in for tooling
//! markers (`<!-- BUGBOT_REVIEW -->`). The naive render — `Span::raw`
//! — leaks all of that to the user as visible noise.
//!
//! This module turns one comment body into a `Vec<Line<'static>>`:
//!
//! 1. **Strip HTML comments** (`<!-- … -->`, including multi-line).
//!    These are pure tooling annotations; the user never wants them.
//! 2. **Lightweight Markdown** — the patterns that show up in PR
//!    discussion bodies:
//!    - `# heading` / `### heading` → bold
//!    - `**bold**`
//!    - `*italic*` (single-letter form, distinct from `**`)
//!    - `` `inline code` `` → cyan
//!    - ``` ```fenced``` ``` → cyan block, dim border
//!    - `> quoted` → dim grey, prepended `▎`
//!    - `- item` / `* item` → bullet `•`
//! 3. **Wrap** to a width.
//! 4. **Collapse-when-long**: caller passes a `max_lines` budget; when
//!    the rendered output exceeds it, return only the first
//!    `max_lines - 1` lines plus a `"+N more lines"` hint.
//!
//! Why hand-rolled instead of `tui-markdown`: comments are short, the
//! patterns we care about are a tiny subset, and we want fine control
//! over collapse / styling. A full pulldown-cmark pipeline would also
//! pull syntect, which is overkill for one-or-two-line comments.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Strip every `<!-- ... -->` block from `s`, including multi-line.
/// Greedy match would eat across multiple unrelated comments; we
/// scan-and-skip linearly so we only ever drop one block at a time.
pub fn strip_html_comments(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"<!--") {
            // Find the matching `-->`. If absent (pathological
            // input), drop everything from `<!--` onward — better
            // than emitting a broken half-comment.
            if let Some(end) = s[i + 4..].find("-->") {
                i += 4 + end + 3;
                continue;
            } else {
                break;
            }
        }
        // Push the next char (handle UTF-8 boundaries).
        let ch_end = next_char_boundary(s, i);
        out.push_str(&s[i..ch_end]);
        i = ch_end;
    }
    out
}

/// Strip the handful of HTML tags that show up in GitHub PR/issue
/// bodies and bot comments. Conservative: only the tags below are
/// touched; everything else passes through unchanged so we don't
/// accidentally eat user content that looks like `<foo>`.
///
/// - `<br>` / `<br/>` / `<br />` → newline (matches HTML semantics).
/// - `<details>` / `</details>` / `<summary>` / `</summary>` →
///   removed (tag only — the inner text stays). Codex's review
///   bot wraps "About Codex" notes in `<details><summary>…`, which
///   our markdown renderer otherwise shows as literal tags.
/// - Self-closing variants of those tags (`<details />`, etc.) also
///   get stripped.
///
/// Case-insensitive on the tag name. Attributes inside the opening
/// tag (`<details open>`) are tolerated.
pub fn strip_html_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            // Find the closing `>`. If absent, treat the `<` as
            // literal (don't lose data on malformed input).
            if let Some(end_rel) = s[i + 1..].find('>') {
                let tag_body = &s[i + 1..i + 1 + end_rel];
                if matches_known_tag(tag_body, "br") {
                    out.push('\n');
                    i += 1 + end_rel + 1;
                    continue;
                }
                if matches_known_tag(tag_body, "details") || matches_known_tag(tag_body, "summary")
                {
                    i += 1 + end_rel + 1;
                    continue;
                }
            }
        }
        let ch_end = next_char_boundary(s, i);
        out.push_str(&s[i..ch_end]);
        i = ch_end;
    }
    out
}

/// Check whether the body of a tag (the part between `<` and `>`)
/// is the given tag name. Tolerates: leading `/` (closing tag),
/// attributes (`<details open>`), trailing `/` (self-closing
/// `<br/>`), and case differences.
fn matches_known_tag(tag_body: &str, name: &str) -> bool {
    let stripped = tag_body.strip_prefix('/').unwrap_or(tag_body).trim();
    // First whitespace-delimited token is the tag name; the rest is
    // attributes. Strip a trailing `/` for self-closing forms.
    let head = stripped
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches('/');
    head.eq_ignore_ascii_case(name)
}

fn next_char_boundary(s: &str, i: usize) -> usize {
    let mut j = i + 1;
    while j < s.len() && !s.is_char_boundary(j) {
        j += 1;
    }
    j
}

/// Render a comment body into styled lines that fit within
/// `width` cells. The result is at most `max_lines` lines tall;
/// content beyond that collapses to a `"+N more lines"` row in dim
/// grey. Pass `usize::MAX` to disable the cap.
///
/// `width` of 0 is treated as "no wrap"; useful in tests.
pub fn render_body(body: &str, width: u16, max_lines: usize) -> Vec<Line<'static>> {
    // Two-pass cleanup: drop tooling annotations (`<!-- … -->`)
    // first, then strip the small HTML tag set we know about
    // (`<br>`, `<details>`, `<summary>`). Order matters because a
    // comment-containing HTML tag (rare but possible) should have
    // its outer comment removed first.
    let cleaned = strip_html_tags(&strip_html_comments(body));
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut in_fence = false;
    let rule_width = usize::from(if width == 0 { 8 } else { width });
    // Collapse runs of blank lines to at most one, and drop the
    // leading run entirely. PR comments (especially bot output) often
    // pad with three or four blanks between sections — left alone
    // they crater the signal-to-noise ratio of the activity feed.
    let mut prev_emitted_empty = true;

    for raw_line in cleaned.lines() {
        // Fenced code block delimiter — toggle and emit a thin rule
        // so the user can see where the block starts/ends.
        if raw_line.trim().starts_with("```") {
            in_fence = !in_fence;
            out.push(Line::from(Span::styled(
                "─".repeat(rule_width),
                Style::default().fg(Color::DarkGray),
            )));
            prev_emitted_empty = false;
            continue;
        }

        if in_fence {
            // Inside a fence: render verbatim in cyan, no inline
            // markdown processing — backticks are literal here.
            out.extend(wrap_one(
                Line::from(Span::styled(
                    raw_line.to_string(),
                    Style::default().fg(Color::Cyan),
                )),
                width,
            ));
            prev_emitted_empty = false;
            continue;
        }

        // Thematic break (`---`, `***`, `___`, optionally space-
        // separated). Render a dim rule like the fence delimiter
        // instead of letting the literal dashes leak into the feed
        // as stray "---" segments.
        if is_thematic_break(raw_line) {
            out.push(Line::from(Span::styled(
                "─".repeat(rule_width),
                Style::default().fg(Color::DarkGray),
            )));
            prev_emitted_empty = false;
            continue;
        }

        let is_empty = raw_line.trim().is_empty();
        if is_empty {
            if prev_emitted_empty {
                continue;
            }
            out.push(Line::from(Span::raw("")));
            prev_emitted_empty = true;
            continue;
        }

        let line = render_inline_line(raw_line);
        out.extend(wrap_one(line, width));
        prev_emitted_empty = false;
    }
    // Trailing blank line buys nothing — the caller's own padding
    // adds the breathing room between cards.
    while out.last().is_some_and(line_is_empty) {
        out.pop();
    }

    if out.len() > max_lines && max_lines > 0 {
        let kept = max_lines.saturating_sub(1);
        let dropped = out.len() - kept;
        out.truncate(kept);
        out.push(Line::from(Span::styled(
            format!("+{dropped} more lines"),
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::ITALIC),
        )));
    }
    out
}

fn line_is_empty(line: &Line<'static>) -> bool {
    line.spans
        .iter()
        .all(|s| s.content.chars().all(char::is_whitespace))
}

/// Process one line worth of raw markdown into a styled `Line`.
/// Handles block-level markers (heading / quote / list bullet) and
/// then inline `**bold**`, `*italic*`, and `` `code` ``.
fn render_inline_line(line: &str) -> Line<'static> {
    // Heading: strip leading `#`s, render the rest bold.
    if let Some(rest) = strip_heading(line) {
        let spans = inline_spans(rest, Style::default().add_modifier(Modifier::BOLD));
        return Line::from(spans);
    }

    // Block quote.
    if let Some(rest) = line.strip_prefix("> ").or_else(|| line.strip_prefix(">")) {
        let mut spans = vec![Span::styled("▎ ", Style::default().fg(Color::DarkGray))];
        spans.extend(inline_spans(rest, Style::default().fg(Color::DarkGray)));
        return Line::from(spans);
    }

    // Unordered list bullet — accept `-`, `*`, `+`.
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        let indent = line.len() - trimmed.len();
        let mut spans: Vec<Span<'static>> = vec![Span::raw(" ".repeat(indent))];
        spans.push(Span::styled("• ", Style::default().fg(Color::Yellow)));
        spans.extend(inline_spans(rest, Style::default()));
        return Line::from(spans);
    }

    Line::from(inline_spans(line, Style::default()))
}

/// CommonMark thematic break: a line of three or more matching
/// `-`, `*`, or `_` characters, optionally separated by spaces or
/// tabs, and nothing else. `--` (fewer than three), `- item` (a
/// list bullet), and `**bold**` are not breaks — the marker run has
/// to be uniform and unbroken by other glyphs.
fn is_thematic_break(line: &str) -> bool {
    let mut marker: Option<char> = None;
    let mut count = 0usize;
    for c in line.trim().chars() {
        match c {
            '-' | '*' | '_' => {
                if marker.is_some_and(|m| m != c) {
                    return false;
                }
                marker = Some(c);
                count += 1;
            }
            ' ' | '\t' => {}
            _ => return false,
        }
    }
    count >= 3
}

/// `# Title` / `### Title` → `Some("Title")`. Returns `None` if the
/// line isn't a heading or has no space after the hashes.
fn strip_heading(line: &str) -> Option<&str> {
    let trimmed = line.trim_start_matches('#');
    if std::ptr::eq(trimmed, line) {
        return None;
    }
    let consumed = line.len() - trimmed.len();
    if consumed == 0 || consumed > 6 {
        return None;
    }
    trimmed.strip_prefix(' ')
}

/// Split `s` on inline-markdown markers and emit one `Span` per
/// chunk with the right style. `base_style` is what unmarked text
/// uses (heading sets BOLD, quote sets dim, etc.).
fn inline_spans(s: &str, base_style: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let chars: Vec<char> = s.chars().collect();
    let link_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::UNDERLINED);
    let mut i = 0;
    while i < chars.len() {
        // HTML anchor: `<a href="URL">TEXT</a>`. Render TEXT as a
        // styled link (cyan + underline) and drop the URL — terminals
        // don't have a portable in-buffer hyperlink primitive, but
        // the underline is at least a strong visual cue. Bot review
        // comments embed these heavily; without this they leak as
        // raw HTML noise into the activity feed.
        if chars[i] == '<'
            && i + 2 < chars.len()
            && chars[i + 1] == 'a'
            && (chars[i + 2].is_whitespace() || chars[i + 2] == '>')
            && let Some(open_end) = find_single(&chars, i + 2, '>')
            && let Some(close_start) = find_subseq(&chars, open_end + 1, &['<', '/', 'a', '>'])
        {
            flush(&mut buf, &mut spans, base_style);
            let text: String = chars[open_end + 1..close_start].iter().collect();
            if !text.is_empty() {
                spans.push(Span::styled(text, link_style));
            }
            i = close_start + 4;
            continue;
        }
        // Bare URL — http(s)://... up to the next whitespace or
        // syntactically closing char. Style as a link for
        // discoverability.
        if (chars[i] == 'h')
            && let Some(url_end) = url_end_at(&chars, i)
        {
            flush(&mut buf, &mut spans, base_style);
            let url: String = chars[i..url_end].iter().collect();
            spans.push(Span::styled(url, link_style));
            i = url_end;
            continue;
        }
        // `**bold**`
        if chars[i] == '*'
            && i + 1 < chars.len()
            && chars[i + 1] == '*'
            && let Some(end) = find_pair(&chars, i + 2, '*', '*')
        {
            flush(&mut buf, &mut spans, base_style);
            let inner: String = chars[i + 2..end].iter().collect();
            spans.push(Span::styled(inner, base_style.add_modifier(Modifier::BOLD)));
            i = end + 2;
            continue;
        }
        // `*italic*` (single)
        if chars[i] == '*'
            && i + 1 < chars.len()
            && chars[i + 1] != '*'
            && let Some(end) = find_single(&chars, i + 1, '*')
        {
            flush(&mut buf, &mut spans, base_style);
            let inner: String = chars[i + 1..end].iter().collect();
            spans.push(Span::styled(
                inner,
                base_style.add_modifier(Modifier::ITALIC),
            ));
            i = end + 1;
            continue;
        }
        // `` `code` ``
        if chars[i] == '`'
            && let Some(end) = find_single(&chars, i + 1, '`')
        {
            flush(&mut buf, &mut spans, base_style);
            let inner: String = chars[i + 1..end].iter().collect();
            spans.push(Span::styled(inner, Style::default().fg(Color::Cyan)));
            i = end + 1;
            continue;
        }
        buf.push(chars[i]);
        i += 1;
    }
    flush(&mut buf, &mut spans, base_style);
    spans
}

/// If a bare URL starts at `start`, return the exclusive end index;
/// otherwise None. Recognises `http://` and `https://` only — the
/// 99% case in PR comments. Stops at whitespace, `<`, or `)`/`]`/`,`
/// boundary chars commonly trailing a URL in prose.
fn url_end_at(chars: &[char], start: usize) -> Option<usize> {
    let prefix7: &[char] = &['h', 't', 't', 'p', 's', ':', '/'];
    let prefix6: &[char] = &['h', 't', 't', 'p', ':', '/'];
    let body_start = if chars[start..].starts_with(prefix7) && chars.get(start + 7) == Some(&'/') {
        start + 8
    } else if chars[start..].starts_with(prefix6) && chars.get(start + 6) == Some(&'/') {
        start + 7
    } else {
        return None;
    };
    if body_start >= chars.len() {
        return None;
    }
    let mut end = body_start;
    while end < chars.len() {
        let ch = chars[end];
        if ch.is_whitespace() || matches!(ch, '<' | ')' | ']' | '"' | '\'') {
            break;
        }
        end += 1;
    }
    // Trim trailing punctuation that's almost always sentence noise.
    while end > body_start && matches!(chars[end - 1], '.' | ',' | ':' | ';' | '!' | '?') {
        end -= 1;
    }
    if end == body_start { None } else { Some(end) }
}

/// Find the start index of `needle` in `chars[start..]`, or None.
fn find_subseq(chars: &[char], start: usize, needle: &[char]) -> Option<usize> {
    if needle.is_empty() || start >= chars.len() {
        return None;
    }
    let last = chars.len().saturating_sub(needle.len());
    let mut i = start;
    while i <= last {
        if chars[i..i + needle.len()] == *needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn flush(buf: &mut String, spans: &mut Vec<Span<'static>>, style: Style) {
    if !buf.is_empty() {
        spans.push(Span::styled(std::mem::take(buf), style));
    }
}

/// Find a single closing char at or after `start`. Returns the index
/// of the closing char, or `None` if absent.
fn find_single(chars: &[char], start: usize, target: char) -> Option<usize> {
    chars[start..]
        .iter()
        .position(|&c| c == target)
        .map(|p| start + p)
}

/// Find a `pair[0] pair[1]` sequence at or after `start`. We keep
/// the function generic-ish so the pair detection lives in one
/// place even though today both args are the same char.
fn find_pair(chars: &[char], start: usize, c0: char, c1: char) -> Option<usize> {
    let mut i = start;
    while i + 1 < chars.len() {
        if chars[i] == c0 && chars[i + 1] == c1 {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// Wrap one `Line` to fit `width` cells. ratatui's built-in `Wrap`
/// wraps the whole `Paragraph`, which paints one `Style` across the
/// whole thing — we want per-span styles to survive wrapping. So we
/// re-emit lines manually.
///
/// `width = 0` short-circuits to "don't wrap" so unit tests can
/// inspect the unwrapped span list.
///
/// `pub(crate)` because scrollable modals (e.g. the snippets browser)
/// must pre-wrap their lines: ratatui's `Paragraph::scroll` counts
/// pre-wrap lines, so pairing it with `.wrap()` mis-clamps the scroll.
pub(crate) fn wrap_one(line: Line<'static>, width: u16) -> Vec<Line<'static>> {
    if width == 0 {
        return vec![line];
    }
    let width = width as usize;
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<Span<'static>> = Vec::new();
    let mut current_w: usize = 0;

    let push_word = |out: &mut Vec<Line<'static>>,
                     current: &mut Vec<Span<'static>>,
                     current_w: &mut usize,
                     word: String,
                     style: Style| {
        let w = crate::util::visual_width(&word);
        if *current_w + w > width && !current.is_empty() {
            out.push(Line::from(std::mem::take(current)));
            *current_w = 0;
        }
        // Word longer than line — hard split rather than overflow.
        if w > width {
            let mut chunk = String::new();
            let mut chunk_w = 0;
            for ch in word.chars() {
                let ch_w = crate::util::char_visual_width(ch);
                if chunk_w + ch_w > width.saturating_sub(*current_w) && !chunk.is_empty() {
                    current.push(Span::styled(std::mem::take(&mut chunk), style));
                    out.push(Line::from(std::mem::take(current)));
                    *current_w = 0;
                    chunk_w = 0;
                }
                chunk.push(ch);
                chunk_w += ch_w;
            }
            if !chunk.is_empty() {
                current.push(Span::styled(chunk, style));
                *current_w += chunk_w;
            }
            return;
        }
        current.push(Span::styled(word, style));
        *current_w += w;
    };

    for span in line.spans {
        let style = span.style;
        let text = span.content.into_owned();
        // Split on whitespace but keep the spaces as separate "words"
        // so trailing-space handling is consistent.
        let mut buf = String::new();
        for ch in text.chars() {
            if ch == ' ' {
                if !buf.is_empty() {
                    push_word(
                        &mut out,
                        &mut current,
                        &mut current_w,
                        std::mem::take(&mut buf),
                        style,
                    );
                }
                if current_w < width {
                    current.push(Span::styled(" ".to_string(), style));
                    current_w += 1;
                }
            } else if ch == '\t' {
                if !buf.is_empty() {
                    push_word(
                        &mut out,
                        &mut current,
                        &mut current_w,
                        std::mem::take(&mut buf),
                        style,
                    );
                }
                let stops = 4_usize.saturating_sub(current_w % 4);
                let pad = " ".repeat(stops.min(width.saturating_sub(current_w)));
                if !pad.is_empty() {
                    current.push(Span::styled(pad.clone(), style));
                    current_w += pad.chars().count();
                }
            } else {
                buf.push(ch);
            }
        }
        if !buf.is_empty() {
            push_word(&mut out, &mut current, &mut current_w, buf, style);
        }
    }
    if !current.is_empty() {
        out.push(Line::from(current));
    }
    if out.is_empty() {
        out.push(Line::from(""));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── strip_html_tags ────────────────────────────────────────

    #[test]
    fn strip_html_tags_replaces_br_with_newline() {
        assert_eq!(strip_html_tags("hi<br/>there"), "hi\nthere");
        assert_eq!(strip_html_tags("hi<br>there"), "hi\nthere");
        assert_eq!(strip_html_tags("hi<br />there"), "hi\nthere");
        // Case-insensitive.
        assert_eq!(strip_html_tags("hi<BR>there"), "hi\nthere");
    }

    #[test]
    fn strip_html_tags_removes_details_and_summary_keeping_content() {
        let s = "<details><summary>About</summary>body text</details>";
        let out = strip_html_tags(s);
        assert_eq!(out, "Aboutbody text");
    }

    /// Codex's review-bot template: `<details><summary>[i] About …`.
    /// Our renderer needs to drop the wrappers but keep the inner
    /// text so the user actually sees the note.
    #[test]
    fn strip_html_tags_codex_review_template() {
        let s = "<details> <summary>[i]  About Codex in GitHub</summary>\
                 <br/>\
                 Codex has been enabled.";
        let out = strip_html_tags(s);
        assert_eq!(out, " [i]  About Codex in GitHub\nCodex has been enabled.");
    }

    #[test]
    fn strip_html_tags_tolerates_attributes_in_tag() {
        assert_eq!(strip_html_tags("<details open>x</details>"), "x");
    }

    /// Tags NOT in the safelist pass through unchanged — we don't
    /// want to accidentally eat user content like `<foo>` in a code
    /// snippet.
    #[test]
    fn strip_html_tags_leaves_unknown_tags_alone() {
        assert_eq!(strip_html_tags("a <foo>b</foo> c"), "a <foo>b</foo> c");
    }

    /// Malformed `<` with no closing `>` is treated as literal —
    /// pasted-snippet code like `if x < 3 {` must survive.
    #[test]
    fn strip_html_tags_unclosed_lt_preserves_literal() {
        assert_eq!(strip_html_tags("x < y"), "x < y");
    }

    // ── strip_html_comments ────────────────────────────────────

    #[test]
    fn strip_html_comments_removes_single_block() {
        let out = strip_html_comments("hello <!-- noise --> world");
        assert_eq!(out, "hello  world");
    }

    #[test]
    fn strip_html_comments_handles_multiline() {
        let s = "before\n<!-- line1\nline2 -->\nafter";
        let out = strip_html_comments(s);
        assert_eq!(out, "before\n\nafter");
    }

    #[test]
    fn strip_html_comments_drops_dangling_open() {
        // No closing `-->` → drop everything from `<!--`. Better than
        // leaking `<!--` into the visible output.
        let out = strip_html_comments("kept <!-- never closed");
        assert_eq!(out, "kept ");
    }

    #[test]
    fn strip_html_comments_idempotent_on_clean_input() {
        let out = strip_html_comments("plain text");
        assert_eq!(out, "plain text");
    }

    #[test]
    fn render_body_strips_bugbot_marker() {
        // The whole body was just an HTML marker → nothing to render.
        // Returning zero lines (rather than one empty line) is right:
        // the caller decides whether to emit a placeholder.
        let out = render_body("<!-- BUGBOT_REVIEW -->", 0, 10);
        assert_eq!(out.len(), 0);
    }

    #[test]
    fn render_body_renders_heading_bold() {
        let out = render_body("### Error message", 0, 10);
        assert_eq!(out.len(), 1);
        let span = &out[0].spans[0];
        assert!(span.style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(span.content.as_ref(), "Error message");
    }

    #[test]
    fn render_body_renders_inline_code_cyan() {
        let out = render_body("use the `foo` function", 0, 10);
        let cyan_spans: Vec<&Span> = out[0]
            .spans
            .iter()
            .filter(|s| s.style.fg == Some(Color::Cyan))
            .collect();
        assert_eq!(cyan_spans.len(), 1);
        assert_eq!(cyan_spans[0].content.as_ref(), "foo");
    }

    #[test]
    fn render_body_renders_bold_inline() {
        let out = render_body("hello **world**", 0, 10);
        let bold_spans: Vec<&Span> = out[0]
            .spans
            .iter()
            .filter(|s| s.style.add_modifier.contains(Modifier::BOLD))
            .collect();
        assert_eq!(bold_spans.len(), 1);
        assert_eq!(bold_spans[0].content.as_ref(), "world");
    }

    #[test]
    fn render_body_renders_blockquote_with_glyph() {
        let out = render_body("> quoted text", 0, 10);
        let first = out[0].spans[0].content.as_ref();
        assert!(first.contains("▎"));
    }

    #[test]
    fn render_body_renders_bullets() {
        let out = render_body("- first\n- second", 0, 10);
        assert_eq!(out.len(), 2);
        assert!(
            out[0]
                .spans
                .iter()
                .any(|s| s.content.as_ref().contains('•'))
        );
    }

    #[test]
    fn render_body_collapses_long_to_max_lines() {
        let body = (0..10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let out = render_body(&body, 0, 4);
        assert_eq!(out.len(), 4);
        let last: String = out[3].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            last.contains("more lines"),
            "last line is the count hint, got: {last}"
        );
    }

    #[test]
    fn render_body_does_not_collapse_when_within_budget() {
        let out = render_body("a\nb\nc", 0, 10);
        assert_eq!(out.len(), 3);
    }

    #[test]
    fn render_body_renders_fenced_code_block() {
        let body = "before\n```\nlet x = 1;\n```\nafter";
        let out = render_body(body, 0, 20);
        // 1 (before) + 1 (open fence rule) + 1 (code) + 1 (close fence rule) + 1 (after) = 5
        assert_eq!(out.len(), 5);
        // The code line should be cyan.
        let code_line = &out[2];
        assert!(
            code_line
                .spans
                .iter()
                .any(|s| s.style.fg == Some(Color::Cyan))
        );
    }

    #[test]
    fn is_thematic_break_recognises_rule_variants() {
        assert!(is_thematic_break("---"));
        assert!(is_thematic_break("***"));
        assert!(is_thematic_break("___"));
        assert!(is_thematic_break("- - -"));
        assert!(is_thematic_break("*****"));
        assert!(is_thematic_break("  ---  "));
    }

    #[test]
    fn is_thematic_break_rejects_non_rules() {
        // Fewer than three markers.
        assert!(!is_thematic_break("--"));
        assert!(!is_thematic_break("**"));
        // A list bullet, not a rule.
        assert!(!is_thematic_break("- item"));
        // Mixed markers.
        assert!(!is_thematic_break("-*-"));
        // Bold/italic inline, not a rule.
        assert!(!is_thematic_break("**bold**"));
        assert!(!is_thematic_break(""));
    }

    /// Regression for #177: a Markdown thematic break must render as a
    /// dim horizontal rule, not leak the literal `---` / `***` glyphs
    /// into the feed as stray "shadow" segments.
    #[test]
    fn render_body_renders_thematic_break_as_rule() {
        let out = render_body("intro\n\n---\n\noutro", 20, 50);
        let rule = out
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains('─')))
            .expect("a box-drawing rule line should be present");
        // No literal dashes survive.
        let has_literal_dashes = out.iter().any(|l| {
            let joined: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            joined.contains("---")
        });
        assert!(!has_literal_dashes, "literal --- leaked into the output");
        // The rule is dim grey, matching the fenced-code delimiter.
        assert_eq!(rule.spans[0].style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn rules_respect_tiny_widths() {
        for body in ["---", "```"] {
            let out = render_body(body, 3, 10);
            let text: String = out[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert_eq!(crate::util::visual_width(&text), 3);
        }
    }

    /// A `---` inside a fenced code block is code, not a rule — it
    /// must stay verbatim.
    #[test]
    fn render_body_keeps_dashes_inside_code_fence() {
        let out = render_body("```\n---\n```", 0, 50);
        let has_literal = out.iter().any(|l| {
            let joined: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            joined == "---"
        });
        assert!(has_literal, "dashes inside a fence must render verbatim");
    }

    #[test]
    fn wrap_one_breaks_long_text_to_width() {
        let line = Line::from("one two three four five six");
        let lines = wrap_one(line, 10);
        // None of the wrapped lines exceed `width`.
        for l in &lines {
            let len: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
            assert!(len <= 10, "wrapped line is {len} cells: {l:?}");
        }
        assert!(lines.len() > 1);
    }

    #[test]
    fn wrap_one_budgets_wide_characters_in_terminal_cells() {
        let lines = wrap_one(Line::from("界界界"), 4);
        assert_eq!(lines.len(), 2);
        for line in lines {
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert!(
                crate::util::visual_width(&text) <= 4,
                "wrapped line exceeds its cell budget: {text:?}"
            );
        }
    }

    #[test]
    fn render_body_handles_real_world_bugbot_comment() {
        // The exact pattern the screenshot showed.
        let body =
            "<!-- BUGBOT_REVIEW -->\n### Error message missing backticks around technical terms";
        let out = render_body(body, 0, 10);
        // Two non-empty lines: blank from removed marker + heading.
        // Or one if the empty leading line is filtered. Either way
        // the heading is present and bold.
        let bold_present = out.iter().any(|line| {
            line.spans
                .iter()
                .any(|s| s.style.add_modifier.contains(Modifier::BOLD))
        });
        assert!(bold_present);
    }
}
