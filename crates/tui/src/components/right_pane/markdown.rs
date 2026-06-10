//! Markdown-stripping helpers for the activity-feed teaser. Drops
//! inline noise (bare URLs, lazy alt-text from `[text](url)` links,
//! HTML angle-brackets), collapses whitespace, clips to a width
//! budget with `…`. Used by `card::render_card_header` to make
//! one-line teasers readable.

pub(crate) fn strip_inline_markdown_noise(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    // Walk by char boundaries, NOT byte boundaries — slicing &s[i..]
    // panics if `i` lands mid-UTF-8-sequence. Earlier this function
    // bumped `i += 1` per loop and `&s[i..]` blew up on `✓ APPROVED`
    // (✓ is 3 bytes; index 1 is inside it).
    let mut chars = s.char_indices().peekable();
    while let Some(&(byte_idx, ch)) = chars.peek() {
        let rest = &s[byte_idx..];
        // `<sub>` / `</sub>` tags — common in PR descriptions for
        // smaller-text disclaimers. We don't render the size change
        // in plain text so the tags are noise.
        if let Some(stripped) = rest
            .strip_prefix("<sub>")
            .or_else(|| rest.strip_prefix("</sub>"))
            .or_else(|| rest.strip_prefix("<sup>"))
            .or_else(|| rest.strip_prefix("</sup>"))
        {
            // Advance the iterator past the consumed prefix length.
            let target = s.len() - stripped.len();
            while let Some(&(b, _)) = chars.peek() {
                if b >= target {
                    break;
                }
                chars.next();
            }
            continue;
        }
        // `![alt](url)` image refs → keep just alt.
        if rest.starts_with("![") {
            if let Some((alt, after)) = parse_alt_then_paren_url(&rest[2..]) {
                if !alt.trim().is_empty() {
                    out.push('[');
                    out.push_str(&alt);
                    out.push(']');
                }
                let target = s.len() - after.len();
                while let Some(&(b, _)) = chars.peek() {
                    if b >= target {
                        break;
                    }
                    chars.next();
                }
                continue;
            }
        }
        // `[text](url)` link → keep just text.
        if rest.starts_with('[') {
            if let Some((text, after)) = parse_alt_then_paren_url(&rest[1..]) {
                out.push_str(&text);
                let target = s.len() - after.len();
                while let Some(&(b, _)) = chars.peek() {
                    if b >= target {
                        break;
                    }
                    chars.next();
                }
                continue;
            }
        }
        // Default: passthrough one CHAR (not one byte — that was the
        // panic source). Pushing the char re-encodes correctly.
        out.push(ch);
        chars.next();
    }
    out
}

/// Helper for `strip_inline_markdown_noise`: parse `alt](url)` and
/// return `(alt, remaining_after_)`. Returns None if the shape isn't
/// a proper `]( … )` pair — caller passes through the original `[`.
pub(crate) fn parse_alt_then_paren_url(after_open: &str) -> Option<(String, &str)> {
    let close_bracket = after_open.find(']')?;
    let alt = &after_open[..close_bracket];
    let after_bracket = &after_open[close_bracket + 1..];
    let after_paren_open = after_bracket.strip_prefix('(')?;
    let close_paren = after_paren_open.find(')')?;
    let after = &after_paren_open[close_paren + 1..];
    Some((alt.to_string(), after))
}

/// Strip leading Markdown BLOCK markers from one (pre-trimmed) line
/// so `### Title` reads as `Title` and `- item` as `item`. Thematic
/// breaks (`---`, `***`, `___`) carry no content and strip to empty
/// so the caller skips them. List markers are only stripped with
/// their trailing space — a prose line like `-2 degrees` keeps its
/// leading dash.
fn strip_block_markers(line: &str) -> &str {
    let s = line.trim_start_matches('#').trim_start_matches('>').trim();
    // Thematic break / setext underline: nothing but marker chars.
    if !s.is_empty() && s.chars().all(|c| matches!(c, '-' | '*' | '_' | '=')) {
        return "";
    }
    s.strip_prefix("- ")
        .or_else(|| s.strip_prefix("* "))
        .or_else(|| s.strip_prefix("+ "))
        .map(str::trim)
        .unwrap_or(s)
}

/// collapsed activity card so the user gets the gist without reading
/// six wrapped lines.
pub(crate) fn teaser_text(body: &str, max_cells: usize) -> String {
    let cleaned = crate::components::comment_render::strip_html_comments(body);
    // Inline markdown / HTML soup that wrecks teasers (looks like
    // `<sub><sub>![P1 Badge](https://img.shields.io/...)</sub></sub>`
    // in GitHub PR descriptions). Collapse them to just the alt /
    // link text before line-splitting so we don't pick an empty
    // first line just because it was all badges.
    let cleaned = strip_inline_markdown_noise(&cleaned);
    // Take the first line that still has CONTENT once block markers
    // are stripped. A body that opens with a thematic break (`---`)
    // or a bare heading marker would otherwise produce an empty
    // teaser — skip such lines and keep looking.
    let stripped = cleaned
        .lines()
        .map(|l| strip_block_markers(l.trim()))
        .find(|l| !l.is_empty())
        .unwrap_or("");
    // Collapse runs of whitespace and drop simple inline emphasis
    // markers so the teaser stays plain text.
    let mut out = String::with_capacity(stripped.len());
    let mut last_space = false;
    for ch in stripped.chars() {
        if ch == '*' || ch == '`' {
            continue;
        }
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(ch);
            last_space = false;
        }
    }
    let out = out.trim().to_string();
    if out.chars().count() <= max_cells {
        return out;
    }
    let mut clipped: String = out.chars().take(max_cells.saturating_sub(1)).collect();
    clipped.push('…');
    clipped
}
