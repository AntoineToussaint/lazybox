//! Ratatui widget that renders a libghostty-vt terminal.
//!
//! # Dirty-state caching
//!
//! Naive render: walk every cell every frame. For a 200×60 terminal
//! that's ~12_000 cells × 4 FFI calls per cell — ~48k FFI hits per
//! frame. Scroll bursts make claude code re-render 10–30 times per
//! gesture, so the cell walk dominates the perceived "scroll feels
//! sluggish."
//!
//! libghostty exposes two layers of dirty tracking we exploit:
//!
//! 1. **`snapshot.dirty()`** — `Clean` / `Partial` / `Full`. When
//!    `Clean`, the entire viewport is byte-identical to the previous
//!    render — we can skip the cell walk entirely and copy the
//!    cached `shadow` into ratatui's buffer.
//! 2. **`row.dirty()`** — per-row flag. In `Partial`, most rows are
//!    unchanged; only the ones libghostty touched need the cell
//!    walk. Clean rows copy from the shadow.
//!
//! The shadow is a `ratatui::buffer::Buffer` we own per terminal
//! slot. Cursor highlight is NOT baked into the shadow — we apply
//! it as a `REVERSED` modifier to the final buffer after copying,
//! so a cursor move between frames doesn't leave a "ghost" cursor
//! at the previous position.
//!
//! # Dirty-flag lifecycle (load-bearing)
//!
//! libghostty's contract is explicit: **`update()` updates dirty
//! flags, the caller must unset them after rendering, and setting
//! one layer doesn't unset the other.** The earlier version of this
//! widget skipped both — flags accumulated `Full` forever and the
//! fast path never fired. After every successful render we:
//!
//! - Call `row.set_dirty(false)` on each row we walked.
//! - Call `snapshot.set_dirty(Clean)` at the end.
//!
//! Skip either and you lose the entire optimization on the next
//! frame; skip both and a future schema change could surface as
//! "renderer is mysteriously slow."

use libghostty_vt::render::{CellIterator, Dirty, RowIterator, Snapshot};
use libghostty_vt::style::Underline;
use ratatui::buffer::Buffer;
use ratatui::prelude::*;

/// A ratatui widget that renders a libghostty-vt terminal snapshot.
///
/// Constructed fresh per frame; the persistent caching lives in
/// the caller-supplied `shadow` buffer.
pub struct GhosttyTerminal<'a, 'alloc, 's> {
    snapshot: &'a Snapshot<'alloc, 's>,
    row_iter: &'a mut RowIterator<'alloc>,
    cell_iter: &'a mut CellIterator<'alloc>,
    shadow: &'a mut Option<Buffer>,
}

impl<'a, 'alloc, 's> GhosttyTerminal<'a, 'alloc, 's> {
    /// Construct the widget with a caller-owned shadow buffer slot.
    /// First call (or after a resize) initialises the shadow; later
    /// calls reuse it to skip clean rows.
    pub fn new(
        snapshot: &'a Snapshot<'alloc, 's>,
        row_iter: &'a mut RowIterator<'alloc>,
        cell_iter: &'a mut CellIterator<'alloc>,
        shadow: &'a mut Option<Buffer>,
    ) -> Self {
        Self {
            snapshot,
            row_iter,
            cell_iter,
            shadow,
        }
    }
}

impl Widget for GhosttyTerminal<'_, '_, '_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let colors = match self.snapshot.colors() {
            Ok(c) => c,
            Err(_) => return,
        };

        // Cursor position (pre-extracted so the cell loops don't
        // pay an Option-deref per cell). Applied at the end of
        // render — NOT written into the shadow — so a cursor move
        // between frames doesn't leave a ghost at the old position.
        let cursor_pos = self
            .snapshot
            .cursor_viewport()
            .ok()
            .flatten()
            .filter(|_| self.snapshot.cursor_visible().unwrap_or(false));

        // Shadow state. Resize / first-render = no usable shadow,
        // so we treat every row as dirty regardless of libghostty's
        // per-row flag.
        let shadow_needs_init = match self.shadow.as_ref() {
            Some(b) => b.area != area,
            None => true,
        };
        if shadow_needs_init {
            *self.shadow = Some(Buffer::empty(area));
        }
        let shadow = self
            .shadow
            .as_mut()
            .expect("shadow set above when needs_init");

        // Snapshot-level dirty: `Clean` means every cell matches
        // the last `RenderState::update` — we can blit the shadow
        // unchanged and skip the whole FFI dance.
        let snapshot_dirty = self.snapshot.dirty().unwrap_or(Dirty::Full);
        if !shadow_needs_init && snapshot_dirty == Dirty::Clean {
            blit_shadow(shadow, buf, area);
            apply_cursor_highlight(buf, area, cursor_pos);
            // Nothing to reset — flags were already Clean.
            return;
        }
        let force_all_rows = shadow_needs_init || snapshot_dirty == Dirty::Full;

        let mut row_iter = match self.row_iter.update(self.snapshot) {
            Ok(r) => r,
            Err(_) => return,
        };

        // Per-cell buffers re-used across the loop — avoid 12_000
        // per-frame allocations of the same shape.
        let mut grapheme_buf: [char; 8] = [' '; 8];
        let mut text_buf = String::with_capacity(8);

        let mut y = 0u16;
        while let Some(row) = row_iter.next() {
            if y >= area.height {
                break;
            }
            let row_dirty = force_all_rows || row.dirty().unwrap_or(true);
            if !row_dirty {
                copy_row_from_shadow(shadow, buf, area, y);
                y += 1;
                continue;
            }

            // Dirty row → cell-walk, write to BOTH shadow and buf.
            let mut cell_iter = match self.cell_iter.update(row) {
                Ok(c) => c,
                Err(_) => {
                    // Iterator failed — leave the row alone rather
                    // than blanking it. Shadow still holds the last
                    // good content. Don't reset dirty — we want to
                    // retry on the next frame.
                    copy_row_from_shadow(shadow, buf, area, y);
                    y += 1;
                    continue;
                }
            };

            let buf_y = area.y + y;
            let mut x = 0u16;
            while let Some(cell) = cell_iter.next() {
                if x >= area.width {
                    break;
                }

                let glen = cell.graphemes_len().unwrap_or(0).min(grapheme_buf.len());
                let text: &str = if glen == 0 {
                    " "
                } else {
                    let _ = cell.graphemes_buf(&mut grapheme_buf[..glen]);
                    text_buf.clear();
                    for ch in &grapheme_buf[..glen] {
                        text_buf.push(*ch);
                    }
                    &text_buf
                };

                let fg_rgb = cell.fg_color().ok().flatten().unwrap_or(colors.foreground);
                let bg_rgb = cell.bg_color().ok().flatten().unwrap_or(colors.background);
                let fg = Color::Rgb(fg_rgb.r, fg_rgb.g, fg_rgb.b);
                let bg = Color::Rgb(bg_rgb.r, bg_rgb.g, bg_rgb.b);
                let mut style = ratatui::style::Style::default().fg(fg).bg(bg);

                if let Ok(cell_style) = cell.style() {
                    if cell_style.bold {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if cell_style.italic {
                        style = style.add_modifier(Modifier::ITALIC);
                    }
                    // Suppress UNDERLINED / CROSSED_OUT on
                    // whitespace-only cells. Claude Code (and other
                    // CLIs) sometimes leave the underline-on ANSI
                    // mode active across newlines, which paints a
                    // styled space for every cell of every following
                    // row — terminals render that as a thin
                    // underline at the bottom of the cell, producing
                    // the stack of stray horizontal bars the user
                    // screenshotted. Real underlines / strikethroughs
                    // are always applied to glyphs; bare whitespace
                    // with these modifiers is essentially never
                    // intentional.
                    //
                    // Widened from the original `text == " "` check:
                    // claude (and other CLIs) sometimes pad with
                    // non-breaking space (U+00A0), figure space
                    // (U+2007), zero-width space (U+200B), tabs, or
                    // wide-glyph trailers. The earlier check missed
                    // all of those.
                    let glyph_is_blank = is_blank_glyph(text);
                    if cell_style.underline != Underline::None && !glyph_is_blank {
                        style = style.add_modifier(Modifier::UNDERLINED);
                    }
                    if cell_style.strikethrough && !glyph_is_blank {
                        style = style.add_modifier(Modifier::CROSSED_OUT);
                    }
                    if cell_style.inverse {
                        style = ratatui::style::Style::default()
                            .fg(bg)
                            .bg(fg)
                            .add_modifier(style.add_modifier & Modifier::all());
                    }
                }

                let buf_x = area.x + x;
                if buf_x < area.x + area.width && buf_y < area.y + area.height {
                    // Write to BOTH the live buf and the shadow.
                    // Shadow stays cursor-free — highlight is
                    // applied at the end, only to the live buf.
                    buf[(buf_x, buf_y)].set_symbol(text).set_style(style);
                    shadow[(buf_x, buf_y)].set_symbol(text).set_style(style);
                }

                x += 1;
            }
            // Pad any cells beyond the cell iterator's end with the
            // background color, so a row that shrank doesn't leave
            // stale shadow content visible.
            let bg = Color::Rgb(
                colors.background.r,
                colors.background.g,
                colors.background.b,
            );
            let fill = ratatui::style::Style::default().bg(bg);
            while x < area.width {
                let buf_x = area.x + x;
                buf[(buf_x, buf_y)].set_symbol(" ").set_style(fill);
                shadow[(buf_x, buf_y)].set_symbol(" ").set_style(fill);
                x += 1;
            }

            // Row rendered — clear its dirty flag so the next
            // `RenderState::update` can re-mark it only if the
            // underlying terminal touched it. Best-effort: if the
            // FFI call fails the only consequence is the row stays
            // marked dirty and we do redundant work next frame.
            let _ = row.set_dirty(false);

            y += 1;
        }
        // Pad rows past the iterator's end (rare — only when the
        // viewport shrank) so the shadow stays in sync.
        let bg = Color::Rgb(
            colors.background.r,
            colors.background.g,
            colors.background.b,
        );
        let fill = ratatui::style::Style::default().bg(bg);
        while y < area.height {
            let buf_y = area.y + y;
            for x in 0..area.width {
                let buf_x = area.x + x;
                buf[(buf_x, buf_y)].set_symbol(" ").set_style(fill);
                shadow[(buf_x, buf_y)].set_symbol(" ").set_style(fill);
            }
            y += 1;
        }

        // Reset the snapshot-level dirty flag. Without this the next
        // frame would see `Partial` or `Full` even when libghostty
        // had no new writes, and the fast `Clean` short-circuit
        // would never fire. See module-level docs.
        let _ = self.snapshot.set_dirty(Dirty::Clean);

        apply_cursor_highlight(buf, area, cursor_pos);
    }
}

/// Copy the entire shadow buffer onto the live buf. Used when the
/// snapshot reports `Dirty::Clean` — fastest possible "render."
/// True when `text` is "visually empty" for the purpose of
/// suppressing underline / strikethrough. Catches every Unicode
/// whitespace codepoint via `char::is_whitespace` (covers ASCII
/// space, NBSP U+00A0, figure space U+2007, ideographic space
/// U+3000, tab, etc.) PLUS zero-width characters that look blank
/// but `is_whitespace` returns false for (ZWSP U+200B, ZWNJ
/// U+200C, ZWJ U+200D, BOM U+FEFF). When every grapheme cluster
/// in the cell falls into one of those buckets, decorations like
/// underline + strikethrough produce stray horizontal bars across
/// otherwise-empty rows — not what the producing CLI meant.
fn is_blank_glyph(text: &str) -> bool {
    if text.is_empty() {
        return true;
    }
    text.chars().all(|c| {
        c.is_whitespace() || matches!(c, '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}')
    })
}

fn blit_shadow(shadow: &Buffer, buf: &mut Buffer, area: Rect) {
    for y in 0..area.height {
        for x in 0..area.width {
            let px = area.x + x;
            let py = area.y + y;
            buf[(px, py)] = shadow[(px, py)].clone();
        }
    }
}

/// Copy one row from the shadow into the live buf. Used when the
/// snapshot is `Dirty::Partial` and this row's `row.dirty()` is
/// false — we have a known-good cached render.
fn copy_row_from_shadow(shadow: &Buffer, buf: &mut Buffer, area: Rect, y: u16) {
    let py = area.y + y;
    for x in 0..area.width {
        let px = area.x + x;
        buf[(px, py)] = shadow[(px, py)].clone();
    }
}

/// Apply the cursor `REVERSED` modifier to the live buf only. The
/// shadow stays cursor-free so a future copy doesn't leave ghosts
/// at old cursor positions.
fn apply_cursor_highlight(
    buf: &mut Buffer,
    area: Rect,
    cursor: Option<libghostty_vt::render::CursorViewport>,
) {
    let Some(cp) = cursor else {
        return;
    };
    if cp.x >= area.width || cp.y >= area.height {
        return;
    }
    let px = area.x + cp.x;
    let py = area.y + cp.y;
    let cell = &mut buf[(px, py)];
    cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
}

#[cfg(test)]
mod tests {
    use super::*;
    use libghostty_vt::render::{CellIterator, RowIterator};
    use libghostty_vt::{RenderState, Terminal, TerminalOptions};

    /// Bundle of state every render test needs. Keeping the helper
    /// loose (no generic lifetimes) instead of a wrapper function
    /// because `Terminal<'alloc, 'cb>` is invariant in 'alloc and
    /// the borrow checker hates a parameterised render helper here.
    struct Harness {
        terminal: Terminal<'static, 'static>,
        render_state: RenderState<'static>,
        row_iter: RowIterator<'static>,
        cell_iter: CellIterator<'static>,
        shadow: Option<Buffer>,
    }

    impl Harness {
        fn new(cols: u16, rows: u16) -> Self {
            Self {
                terminal: Terminal::new(TerminalOptions {
                    cols,
                    rows,
                    max_scrollback: 100,
                })
                .unwrap(),
                render_state: RenderState::new().unwrap(),
                row_iter: RowIterator::new().unwrap(),
                cell_iter: CellIterator::new().unwrap(),
                shadow: None,
            }
        }

        fn render(&mut self, area: Rect) -> Buffer {
            let snapshot = self.render_state.update(&self.terminal).unwrap();
            let widget = GhosttyTerminal::new(
                &snapshot,
                &mut self.row_iter,
                &mut self.cell_iter,
                &mut self.shadow,
            );
            let mut buf = Buffer::empty(area);
            widget.render(area, &mut buf);
            buf
        }

        fn current_dirty(&mut self) -> Result<Dirty, libghostty_vt::Error> {
            self.render_state.update(&self.terminal).unwrap().dirty()
        }
    }

    /// Regression: the original shadow-caching commit forgot to call
    /// `set_dirty(false)` after rendering — flags stayed at `Full`
    /// forever and the fast path never fired (and worse, freezes
    /// reported earlier when stale dirty bits interacted badly with
    /// content updates). This test makes sure every subsequent
    /// frame with no terminal changes:
    ///   1. Reports `Clean` to the next `update()` call (proves the
    ///      contract is honored), AND
    ///   2. Produces a buffer byte-identical to the first frame
    ///      (proves the shadow cache is content-correct).
    #[test]
    fn idle_frame_reports_clean_and_replays_shadow() {
        let mut h = Harness::new(10, 3);
        h.terminal.vt_write(b"hello\r\nworld");
        let area = Rect::new(0, 0, 10, 3);

        let first = h.render(area);
        assert!(h.shadow.is_some(), "first render initialises the shadow");

        // After rendering with no further `vt_write`, `update()` must
        // report `Dirty::Clean`. If the widget skipped `set_dirty`,
        // libghostty returns `Full` (or `Partial`) and the optimization
        // is lost. We accept `Err` only if the widget never managed
        // to reset — surface it as a failed assertion rather than a
        // panic deeper in.
        match h.current_dirty() {
            Ok(Dirty::Clean) => {}
            Ok(other) => panic!(
                "post-render snapshot must be Clean — the widget forgot \
                 to reset dirty flags (got {other:?})",
            ),
            Err(e) => panic!("snapshot.dirty() errored after render: {e:?}"),
        }

        // Second render — terminal unchanged, shadow primed. Must
        // produce a byte-identical buffer via the fast path.
        let second = h.render(area);
        assert_eq!(
            first, second,
            "idle re-render must reproduce the first frame from the shadow",
        );
    }

    /// Dirty-row path: an in-place terminal update should trigger a
    /// real cell walk for the affected row but still leave the
    /// untouched rows readable from the shadow. We can't directly
    /// observe which rows took which path, but we can assert the
    /// output is correct AND that the post-render dirty is Clean.
    #[test]
    fn partial_update_renders_new_content_and_clears_dirty() {
        let mut h = Harness::new(10, 3);
        h.terminal.vt_write(b"hello\r\nworld");
        let area = Rect::new(0, 0, 10, 3);

        let _ = h.render(area);

        // Mutate row 1 (`world` → overwrite with `WORLD`). Row 0
        // (`hello`) is untouched and should serve from shadow.
        h.terminal.vt_write(b"\x1b[2;1HWORLD");
        let after = h.render(area);

        // Row 0 keeps `hello`.
        let row0: String = (0..5).map(|x| after[(x, 0)].symbol().to_string()).collect();
        assert_eq!(row0, "hello", "untouched row served from shadow");

        // Row 1 has the new content.
        let row1: String = (0..5).map(|x| after[(x, 1)].symbol().to_string()).collect();
        assert_eq!(row1, "WORLD", "dirty row re-rendered with new content");

        // Dirty reset works after a partial update too.
        match h.current_dirty() {
            Ok(Dirty::Clean) => {}
            Ok(other) => panic!("expected Clean after partial render, got {other:?}"),
            Err(e) => panic!("snapshot.dirty() errored after partial render: {e:?}"),
        }
    }

    /// Resize invalidates the shadow: the next render must repopulate
    /// it instead of replaying stale content at the new rect.
    #[test]
    fn resized_area_repopulates_shadow_without_using_stale_cache() {
        let mut h = Harness::new(10, 3);
        h.terminal.vt_write(b"abc");

        let area_small = Rect::new(0, 0, 10, 3);
        let _ = h.render(area_small);
        assert_eq!(h.shadow.as_ref().unwrap().area, area_small);

        // Re-render at a different rect — shadow must be rebuilt.
        let area_wide = Rect::new(0, 0, 20, 5);
        let _ = h.render(area_wide);
        assert_eq!(
            h.shadow.as_ref().unwrap().area,
            area_wide,
            "shadow rebuilt for the new rect (no stale 10x3 cache)",
        );
    }

    /// #103 regression: a replayed buffer carrying underline /
    /// strikethrough / box-drawing SGR must render those attributes on
    /// the glyphs that own them and NOWHERE else — not leaked full-width
    /// across the blank tail of the row or onto unrelated rows — and a
    /// redraw (shadow replay) must reproduce the same frame, so the pane
    /// can't blink between a clean and a struck-through version.
    #[test]
    fn replayed_sgr_does_not_leak_across_cells_on_redraw() {
        let mut h = Harness::new(12, 3);
        // Row 0: underlined header, then SGR reset, then plain padding —
        // the reset must stop the underline from running to end-of-row.
        // Row 1: a struck-out box-drawing glyph followed by plain text —
        // the strikethrough must stay on the glyph, not the neighbours.
        h.terminal
            .vt_write(b"\x1b[4mHEAD\x1b[0m ok\r\n\x1b[9m\xe2\x94\x80\x1b[0mtail");
        let area = Rect::new(0, 0, 12, 3);

        let first = h.render(area);

        let underlined =
            |buf: &Buffer, x: u16, y: u16| buf[(x, y)].modifier.contains(Modifier::UNDERLINED);
        let struck =
            |buf: &Buffer, x: u16, y: u16| buf[(x, y)].modifier.contains(Modifier::CROSSED_OUT);

        // Header glyphs keep their underline...
        for x in 0..4 {
            assert!(
                underlined(&first, x, 0),
                "HEAD cell {x} should be underlined"
            );
        }
        // ...but the reset + blank tail must not.
        for x in 4..12 {
            assert!(
                !underlined(&first, x, 0),
                "underline leaked to cell {x} past the SGR reset",
            );
        }
        // The box-drawing glyph keeps its strikethrough; the plain tail
        // after the reset does not.
        assert!(
            struck(&first, 0, 1),
            "box-drawing glyph should be struck out"
        );
        for x in 1..12 {
            assert!(
                !struck(&first, x, 1),
                "strikethrough leaked to cell {x} on row 1",
            );
        }

        // Redraw from the shadow (terminal unchanged) is byte-identical —
        // no flicker between a clean and a corrupted frame.
        let second = h.render(area);
        assert_eq!(
            first, second,
            "redraw must reproduce the frame from the shadow"
        );
    }

    /// Empty + ASCII-whitespace cases. The ORIGINAL `text == " "`
    /// check covered these; the test pins them so a future tighten
    /// doesn't regress.
    #[test]
    fn is_blank_glyph_matches_empty_and_ascii_space() {
        assert!(is_blank_glyph(""));
        assert!(is_blank_glyph(" "));
        assert!(is_blank_glyph("\t"));
        assert!(is_blank_glyph("   "));
    }

    /// Regression for the user-screenshotted bug: phantom strike
    /// lines on cells claude padded with NBSP / figure space /
    /// zero-width space. The original `text == " "` check missed
    /// all of these. Widening to "every grapheme is whitespace or
    /// a known zero-width char" catches them.
    #[test]
    fn is_blank_glyph_matches_unicode_blanks() {
        // NBSP — `is_whitespace` catches it.
        assert!(is_blank_glyph("\u{00A0}"));
        // Figure space.
        assert!(is_blank_glyph("\u{2007}"));
        // Ideographic space.
        assert!(is_blank_glyph("\u{3000}"));
        // Zero-width space — NOT in `is_whitespace`, caught by the
        // explicit list.
        assert!(is_blank_glyph("\u{200B}"));
        // Zero-width joiner + non-joiner + BOM — same family.
        assert!(is_blank_glyph("\u{200C}"));
        assert!(is_blank_glyph("\u{200D}"));
        assert!(is_blank_glyph("\u{FEFF}"));
        // Mixed string of "blank" chars stays blank.
        assert!(is_blank_glyph(" \u{00A0}\t\u{200B}"));
    }

    /// Real glyphs (including box-drawing chars claude uses for
    /// task panels) MUST NOT be classified as blank — they need
    /// to keep their strikethrough / underline so a struck-out
    /// completed item still renders correctly.
    #[test]
    fn is_blank_glyph_rejects_real_content() {
        assert!(!is_blank_glyph("a"));
        assert!(!is_blank_glyph("•"));
        // Box-drawing horizontal — appears verbatim in claude's UI.
        assert!(!is_blank_glyph("─"));
        // Space embedded in real content — overall not blank.
        assert!(!is_blank_glyph("a b"));
    }
}
