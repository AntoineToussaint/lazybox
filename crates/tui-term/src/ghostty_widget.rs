//! Ratatui widget that renders a libghostty-vt terminal.
//!
//! # Why we don't trust libghostty's dirty flags
//!
//! libghostty exposes dirty tracking at two layers — a snapshot-level
//! `Clean`/`Partial`/`Full` flag and a per-row `row.dirty()` flag —
//! and this widget used to skip work based on both: blit a cached
//! `shadow` buffer on `Clean`, copy individual `clean` rows from it on
//! `Partial`. Both layers proved **unsound** as a redraw-skip signal
//! against a viewport-indexed cache.
//!
//! The flags report whether a *logical* row's content changed, not
//! whether the *viewport position* it occupies still shows the same
//! thing. A scroll-region (`DECSTBM`) or reverse-index scroll shifts
//! unchanged rows to new viewport positions without dirtying them —
//! and under that churn libghostty even reports the whole snapshot
//! `Clean` while cells the VT moved actually differ from the previous
//! frame. The `shadow` is indexed by viewport position, so any skip
//! based on these flags replays whatever previously sat at that
//! position: stale glyphs and styles the VT never put there. That is
//! issue #239 — Codex scrolls a region every spinner frame and the
//! embedded terminal filled with ghost cells and misplaced text.
//!
//! So we walk every cell of every row, every frame. Rendering is
//! event-driven (a redraw fires on real output / input), so the walk
//! runs when there is plausibly something to draw, and an optimized
//! VT build (#68) makes a full viewport walk cheap. The `shadow` is
//! retained only to back the per-row FFI-error fallback — it is no
//! longer a render fast path.
//!
//! Cursor highlight is NOT baked into the shadow — we apply it as a
//! `REVERSED` modifier to the final buffer at the end of render, so a
//! cursor move between frames doesn't leave a "ghost" cursor at the
//! previous position.
//!
//! # Dirty-flag lifecycle
//!
//! libghostty's contract is that the caller unsets dirty flags after
//! rendering (`update()` only sets them). We honor it even though we
//! no longer read the flags to skip work: after rendering we call
//! `row.set_dirty(false)` on every row and `snapshot.set_dirty(Clean)`
//! at the end, so libghostty's internal dirty state doesn't accumulate
//! unbounded across frames.

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
        //
        // A blinking cursor drives its blink by toggling DECTCEM
        // (mode 25 / `cursor_visible`) on a timer. Gating the
        // reversed-block highlight on `cursor_visible()` made it
        // flash in time with that blink — over the prompt's
        // horizontal rule it reads as a blinking horizontal line.
        // Same no-blink stance the cell path already takes for SGR
        // 5/6: when the cursor is *blinking*, render it steadily on
        // presence and ignore the phase. A non-blinking cursor still
        // honours `cursor_visible()`, so a full-screen TUI that hides
        // its cursor (DECTCEM off) doesn't get a stray block.
        let cursor_pos = self.snapshot.cursor_viewport().ok().flatten().filter(|_| {
            self.snapshot.cursor_visible().unwrap_or(false)
                || self.snapshot.cursor_blinking().unwrap_or(false)
        });

        // Shadow buffer. No longer a render fast path — see module
        // docs: libghostty's dirty flags can't be trusted to skip work
        // (region scrolls under-report all the way to `Clean`). The
        // shadow now only backs the per-row FFI-error fallback below,
        // holding the last good content for a row whose cell iterator
        // transiently fails. Reset it on resize so the fallback never
        // replays content sized for a different rect.
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

            // Every row is walked — `row.dirty()` is deliberately not
            // consulted to skip rows (see module docs: it's unsound
            // against the viewport-indexed shadow under scroll). Walk
            // the cells, writing to BOTH shadow and buf.
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
                    // Deliberately drop `cell_style.blink` (SGR 5/6).
                    // Forwarding it to Modifier::SLOW_BLINK makes the host
                    // terminal blink every styled glyph — divider rules and
                    // status panels visibly flicker even though they never
                    // change. The grid already redraws on real updates, so a
                    // blink attribute buys nothing but flicker.
                    //
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

/// Copy one row from the shadow into the live buf. Used only as the
/// per-row fallback when a cell iterator transiently fails — the
/// shadow still holds the last good content for that row.
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

        /// Ground-truth render: a full cell walk against a throwaway
        /// shadow, so no state carries between frames. Comparing the
        /// persistent-shadow `render` against this pins that the widget
        /// reproduces the VT's current viewport exactly — never replays
        /// stale cached content.
        fn render_fresh(&mut self, area: Rect) -> Buffer {
            let snapshot = self.render_state.update(&self.terminal).unwrap();
            let mut shadow = None;
            let widget = GhosttyTerminal::new(
                &snapshot,
                &mut self.row_iter,
                &mut self.cell_iter,
                &mut shadow,
            );
            let mut buf = Buffer::empty(area);
            widget.render(area, &mut buf);
            buf
        }

        /// Stronger ground truth than `render_fresh`: renders through a
        /// BRAND-NEW render state as well as a throwaway shadow. A fresh
        /// render state's first update copies the whole viewport from the
        /// terminal regardless of the (untrustworthy) dirty flags, so it
        /// can carry no stale incremental content at *either* layer — the
        /// widget shadow or libghostty's own per-render-state buffer.
        /// `render` / `render_fresh` both reuse the persistent render
        /// state, so they can't catch staleness that lives one layer below
        /// the widget.
        fn render_via_fresh_state(&mut self, area: Rect) -> Buffer {
            let mut fresh_state = RenderState::new().unwrap();
            let snapshot = fresh_state.update(&self.terminal).unwrap();
            let mut shadow = None;
            let widget = GhosttyTerminal::new(
                &snapshot,
                &mut self.row_iter,
                &mut self.cell_iter,
                &mut shadow,
            );
            let mut buf = Buffer::empty(area);
            widget.render(area, &mut buf);
            buf
        }

        /// Read the dirty state libghostty reports for the pending
        /// changes, then honour the reset contract exactly as `render`
        /// does (clear per-row + snapshot flags). The subsequent `render`
        /// therefore sees already-synced content and still full-walks it —
        /// which is the behaviour under test.
        fn peek_dirty(&mut self) -> Dirty {
            let snapshot = self.render_state.update(&self.terminal).unwrap();
            let dirty = snapshot.dirty().unwrap();
            let mut ri = RowIterator::new().unwrap();
            let mut rows = ri.update(&snapshot).unwrap();
            while let Some(row) = rows.next() {
                let _ = row.set_dirty(false);
            }
            let _ = snapshot.set_dirty(Dirty::Clean);
            dirty
        }
    }

    #[test]
    fn cached_render_matches_full_walk_under_scroll_churn() {
        let mut cached = Harness::new(20, 5);
        let mut truth = Harness::new(20, 5);
        let area = Rect::new(0, 0, 20, 5);

        for i in 0..40 {
            let chunk = format!("line {i} content\r\n");
            cached.terminal.vt_write(chunk.as_bytes());
            truth.terminal.vt_write(chunk.as_bytes());

            let c = cached.render(area);
            let t = truth.render_fresh(area);
            assert_eq!(c, t, "cached render diverged from full walk at write {i}");
        }
    }

    /// Simulate what the host terminal actually shows after ratatui
    /// flushes `buf`: walk cells left-to-right, emitting each symbol and
    /// skipping the columns it covers per `unicode-width` (the same logic
    /// ratatui's `BufferDiff` uses to position writes).
    fn host_rendered_row(buf: &Buffer, y: u16, width: u16) -> String {
        use ratatui::buffer::CellWidth;
        let mut out = String::new();
        let mut skip = 0u16;
        for x in 0..width {
            let cell = &buf[(x, y)];
            if skip > 0 {
                skip -= 1;
                continue;
            }
            out.push_str(cell.symbol());
            skip = cell.cell_width().saturating_sub(1);
        }
        out
    }

    /// Ghostty's intended visible row: honour the VT's own wide flags
    /// (a `Wide` base glyph owns two columns; its `SpacerTail` is not
    /// drawn) independent of `unicode-width`.
    fn ghostty_intended_row(h: &mut Harness, y: u16, width: u16) -> String {
        use libghostty_vt::screen::CellWide;
        let snapshot = h.render_state.update(&h.terminal).unwrap();
        let mut ri = RowIterator::new().unwrap();
        let mut rows = ri.update(&snapshot).unwrap();
        let mut row_y = 0u16;
        let mut out = String::new();
        while let Some(row) = rows.next() {
            if row_y == y {
                let mut ci = CellIterator::new().unwrap();
                let mut cells = ci.update(row).unwrap();
                let mut cols = 0u16;
                while let Some(cell) = cells.next() {
                    if cols >= width {
                        break;
                    }
                    let wide = cell.wide().unwrap_or(CellWide::Narrow);
                    match wide {
                        CellWide::SpacerTail => {}
                        _ => {
                            let glen = cell.graphemes_len().unwrap_or(0);
                            if glen == 0 {
                                out.push(' ');
                            } else {
                                let mut g = vec!['\0'; glen];
                                let _ = cell.graphemes_buf(&mut g);
                                for c in g {
                                    out.push(c);
                                }
                            }
                        }
                    }
                    cols += 1;
                }
                break;
            }
            row_y += 1;
        }
        out
    }

    #[test]
    fn replay_real_agent_fixtures_no_corruption() {
        let fixtures_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../agents/tests/fixtures");
        let names = [
            "claude_real_working_statusbar.bin",
            "claude_real_bypass_idle.bin",
            "claude_startup_repaint.bin",
            "codex_real_working.bin",
            "codex_real_edit_approval.bin",
            "codex_real_command_approval.bin",
            "codex_real_idle.bin",
        ];
        for name in names {
            let bytes = match std::fs::read(format!("{fixtures_dir}/{name}")) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let mut cached = Harness::new(80, 24);
            let mut truth = Harness::new(80, 24);
            let area = Rect::new(0, 0, 80, 24);
            for chunk in bytes.chunks(64) {
                cached.terminal.vt_write(chunk);
                truth.terminal.vt_write(chunk);
                let c = cached.render(area);
                let t = truth.render_fresh(area);
                assert_eq!(c, t, "{name}: cached render diverged from full walk");
            }
            // Final frame must match ghostty's intended columns exactly —
            // no width/column drift, no ghost cells.
            let frame = cached.render(area);
            for y in 0..24u16 {
                let host = host_rendered_row(&frame, y, 80);
                let intent = ghostty_intended_row(&mut cached, y, 80);
                assert_eq!(
                    host.trim_end(),
                    intent.trim_end(),
                    "{name} row {y}: host render diverges from ghostty intent",
                );
            }
        }
    }

    #[test]
    fn host_render_matches_ghostty_intent_for_emoji_and_wide() {
        let mut h = Harness::new(24, 6);
        let area = Rect::new(0, 0, 24, 6);
        // Mix of glyphs an agent emits: emoji (some VS16-presented),
        // CJK, box drawing, spinner braille, plus trailing real text
        // whose column position is corrupted if a width disagreement
        // hides the cell after a "wide" glyph.
        let lines = [
            "AB 🎉 CD\r\n",
            "EF 你好 GH\r\n",
            "ok ✓ warn ⚠ x\r\n",
            "spin ⠋⠙⠹ end\r\n",
            "tag 🚀 PR #28 z\r\n",
        ];
        for line in lines {
            h.terminal.vt_write(line.as_bytes());
        }
        let _ = h.render(area);
        for y in 0..5u16 {
            let host = host_rendered_row(&h.render(area), y, 24);
            let intent = ghostty_intended_row(&mut h, y, 24);
            assert_eq!(
                host.trim_end(),
                intent.trim_end(),
                "row {y}: host render diverges from ghostty's intended columns",
            );
        }
    }

    #[test]
    fn cached_render_matches_full_walk_with_erase_and_scroll_region() {
        let mut cached = Harness::new(20, 6);
        let mut truth = Harness::new(20, 6);
        let area = Rect::new(0, 0, 20, 6);
        // Ink-style sticky-footer churn: scroll region, cursor moves,
        // erase-line / erase-display, insert/delete-line.
        let ops = [
            "\x1b[2J\x1b[Hheader\r\n",
            "\x1b[3;6r\x1b[3;1Hbody line\r\n",
            "\x1b[5;1H\x1b[2Kfooter ⠋\r\n",
            "\x1b[3;1H\x1b[Lnew top\r\n",
            "\x1b[4;1H\x1b[Mdeleted\r\n",
            "\x1b[6;1Hmore output here\r\n",
            "\x1b[5;1H\x1b[2Kfooter ⠙ tick\r\n",
            "\x1b[2J\x1b[Hreset frame\r\n",
        ];
        for (i, op) in ops.iter().cycle().take(40).enumerate() {
            cached.terminal.vt_write(op.as_bytes());
            truth.terminal.vt_write(op.as_bytes());
            let c = cached.render(area);
            let t = truth.render_fresh(area);
            assert_eq!(c, t, "cached diverged at erase/scroll-region op {i}");
        }
    }

    /// #257 regression: an agent orchestration UI (a live multi-line
    /// subagent status list with spinners, on the alternate screen, driven
    /// by scroll-region and reverse-index churn) makes libghostty
    /// under-report dirtiness — reporting a frame `Clean`/`Partial` even
    /// though moved cells differ from the prior frame (#239). The reported
    /// symptom was a mostly-black viewport with a few orphaned status
    /// fragments: the pre-#242 fast path copied the stale (blank) shadow
    /// for those under-reported rows.
    ///
    /// This pins the always-full-walk fix against that exact scenario, and
    /// does so with a stronger reference than the other cached-vs-fresh
    /// tests: `render_via_fresh_state` walks a BRAND-NEW render state, so a
    /// match proves the widget reproduces the live viewport even if
    /// staleness were to live one layer below the widget, in libghostty's
    /// own incremental render-state buffer. `saw_underreport` asserts the
    /// stream genuinely exercises the hazard, so the test can't pass
    /// vacuously.
    #[test]
    fn orchestration_churn_renders_full_walk_not_stale_shadow() {
        let mut h = Harness::new(48, 12);
        let area = Rect::new(0, 0, 48, 12);
        // Alt-screen; a header; then a scroll-region status list whose rows
        // are rewritten with spinners and shifted via reverse index (ESC M)
        // each frame; plus a footer rewrite — the shape of the #257 UI.
        let ops = [
            "\x1b[?1049h\x1b[2J\x1b[H",
            "* Orchestrating…\r\n",
            "\x1b[2;10r",
            "\x1b[2;1H\x1b[2K  agent 1  ⠋  6  52s\r\n",
            "\x1b[3;1H\x1b[2K  agent 2  ⠙  7  12s\r\n",
            "\x1b[4;1H\x1b[2K  agent 3  ⠹  3  4s\r\n",
            "\x1b[2;1H\x1bM",
            "\x1b[2;1H\x1b[2K  agent 0  ⠸  9  1s\r\n",
            "\x1b[11;1H\x1b[K] exit for ? · q q",
        ];
        let mut saw_underreport = false;
        for (i, op) in ops.iter().cycle().take(60).enumerate() {
            h.terminal.vt_write(op.as_bytes());
            if h.peek_dirty() != Dirty::Full {
                saw_underreport = true;
            }
            let cached = h.render(area);
            let truth = h.render_via_fresh_state(area);
            assert_eq!(
                cached, truth,
                "cached widget render diverged from a fresh-state full walk at op {i}",
            );
        }
        assert!(
            saw_underreport,
            "orchestration stream never made libghostty under-report dirty — \
             the regression test would pass vacuously",
        );
    }

    #[test]
    fn cached_render_matches_full_walk_with_wide_chars() {
        let mut cached = Harness::new(20, 5);
        let mut truth = Harness::new(20, 5);
        let area = Rect::new(0, 0, 20, 5);

        let lines = [
            "héllo 你好 wörld\r\n",
            "spin ⠋ 你好世界\r\n",
            "emoji 🎉 done 🚀\r\n",
            "\x1b[2;1H\x1b[2Koverwrite 你\r\n",
        ];
        for (i, line) in lines.iter().cycle().take(40).enumerate() {
            cached.terminal.vt_write(line.as_bytes());
            truth.terminal.vt_write(line.as_bytes());
            let c = cached.render(area);
            let t = truth.render_fresh(area);
            assert_eq!(c, t, "cached render diverged at wide-char write {i}");
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
    /// #174 regression: libghostty sets `blink` on cells carrying the
    /// SGR 5 / 6 attribute, but the widget must NOT translate it into a
    /// ratatui blink modifier — doing so makes the host terminal blink
    /// every styled glyph (divider rules, status panels), which is the
    /// reported flicker. Assert an SGR-5 box-drawing run reaches ratatui
    /// with no blink modifier even though the VT cell flags it.
    #[test]
    fn blink_attribute_is_not_forwarded_to_ratatui() {
        let mut h = Harness::new(6, 1);
        let area = Rect::new(0, 0, 6, 1);
        // SGR 5 (slow blink) on three box-drawing dashes, then reset.
        h.terminal
            .vt_write("\x1b[5m\u{2500}\u{2500}\u{2500}\x1b[0m".as_bytes());

        // Sanity: the VT really does carry the blink flag on those cells,
        // so this test would catch a future change that maps it through.
        // Scoped so the snapshot's borrow of `h` ends before `h.render`.
        {
            let snap = h.render_state.update(&h.terminal).unwrap();
            let mut ri = RowIterator::new().unwrap();
            let mut rows = ri.update(&snap).unwrap();
            let row = rows.next().unwrap();
            let mut ci = CellIterator::new().unwrap();
            let mut cells = ci.update(row).unwrap();
            assert!(
                cells.next().unwrap().style().unwrap().blink,
                "precondition: libghostty must flag the SGR-5 cell as blink",
            );
        }

        let buf = h.render(area);
        let blink = Modifier::SLOW_BLINK | Modifier::RAPID_BLINK;
        for x in 0..3 {
            assert!(
                !buf[(x, 0u16)].modifier.intersects(blink),
                "cell {x} must not carry a blink modifier",
            );
        }
    }

    /// #174 regression: a static horizontal rule must stay put while a
    /// neighbouring line updates live (spinner / token counter ticking).
    /// Feed an Ink-style status block, then redraw only the spinner row
    /// repeatedly; the divider rows must show ZERO spurious damage in the
    /// frame-to-frame diff and keep their content, so the host terminal
    /// never repaints — and therefore never flickers — them.
    #[test]
    fn horizontal_rules_stay_stable_across_partial_redraws() {
        let mut h = Harness::new(30, 6);
        let area = Rect::new(0, 0, 30, 6);
        let dash = "\u{2500}".repeat(30);
        // header / divider / prompt / divider / spinner / blank
        h.terminal.vt_write(
            format!("header\r\n{dash}\r\n\u{276f} \r\n{dash}\r\n  . 0 tokens\r\n").as_bytes(),
        );
        let mut prev = h.render(area);
        let rowstr = |buf: &Buffer, y: u16| {
            (0..30)
                .map(|x| buf[(x, y)].symbol().to_string())
                .collect::<String>()
        };
        assert_eq!(rowstr(&prev, 1), dash);
        assert_eq!(rowstr(&prev, 3), dash);

        // Spinner ticks: reposition to the spinner row, erase it, rewrite
        // only its content. The divider rows are never touched.
        let frames = [
            "  o 12 tokens",
            "  O 48 tokens",
            "  o 96 tokens",
            "  . 150 tokens",
        ];
        for (n, body) in frames.iter().enumerate() {
            h.terminal
                .vt_write(format!("\x1b[5;1H\x1b[2K{body}").as_bytes());
            let cur = h.render(area);
            let damage = prev.diff(&cur);
            let dmg_on = |y: u16| damage.iter().filter(|(_, yy, _)| *yy == y).count();
            assert_eq!(dmg_on(1), 0, "tick {n}: top divider spuriously damaged");
            assert_eq!(dmg_on(3), 0, "tick {n}: bottom divider spuriously damaged");
            assert_eq!(
                rowstr(&cur, 1),
                dash,
                "tick {n}: top divider content changed"
            );
            assert_eq!(
                rowstr(&cur, 3),
                dash,
                "tick {n}: bottom divider content changed"
            );
            prev = cur;
        }
    }

    /// #192 regression: a blinking cursor must render as a STEADY
    /// reversed block. Agents drive the blink by toggling DECTCEM
    /// (mode 25 / `cursor_visible`) on a timer; gating the highlight
    /// on `cursor_visible()` made the block flash in time with that
    /// blink — over the prompt's horizontal rule it read as a
    /// blinking horizontal line. Render two frames one blink phase
    /// apart (mode 25 on, then off) with the cursor *blinking*: the
    /// cursor cell must be REVERSED in BOTH.
    #[test]
    fn blinking_cursor_stays_steady_across_blink_phases() {
        let mut h = Harness::new(6, 1);
        let area = Rect::new(0, 0, 6, 1);
        // Park the cursor on a horizontal rule (`\x1b[1G` returns to
        // column 1, onto the first dash) and mark it blinking.
        h.terminal
            .vt_write("\u{2500}\u{2500}\u{2500}\x1b[1G\x1b[?12h".as_bytes());

        // Blink "on" phase.
        h.terminal.vt_write(b"\x1b[?25h");
        let on = h.render(area);
        assert!(
            on[(0u16, 0u16)].modifier.contains(Modifier::REVERSED),
            "cursor cell must be highlighted in the blink-on phase",
        );

        // Blink "off" phase: the agent clears DECTCEM. The highlight
        // must NOT vanish — that toggle is the blink we refuse to
        // follow, so the two frames are identical at the cursor cell.
        h.terminal.vt_write(b"\x1b[?25l");
        let off = h.render(area);
        assert!(
            off[(0u16, 0u16)].modifier.contains(Modifier::REVERSED),
            "blinking cursor must stay steady when DECTCEM toggles off",
        );
    }

    /// Counterpart to the steady-cursor fix: a NON-blinking cursor the
    /// app genuinely hides (DECTCEM off, blink mode unset) must NOT be
    /// drawn — otherwise a full-screen TUI that parks and hides its
    /// cursor would show a stray reversed block.
    #[test]
    fn hidden_non_blinking_cursor_is_not_drawn() {
        let mut h = Harness::new(6, 1);
        let area = Rect::new(0, 0, 6, 1);
        h.terminal.vt_write("abc\x1b[1G\x1b[?25l".as_bytes());
        let buf = h.render(area);
        assert!(
            !buf[(0u16, 0u16)].modifier.contains(Modifier::REVERSED),
            "a hidden non-blinking cursor must not be highlighted",
        );
    }
}
