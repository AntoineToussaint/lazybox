//! Free helper functions used by the model layer:
//!
//! - **Layout / mouse hit-testing**: `rect_contains`, `split_for_footer`.
//! - **Rendering**: `paint_selection` (drag-selection reverse-video),
//!   `placeholder` (dev scaffold).
//! - **Key / catalog**: `key_event_to_chord` (crossterm → catalog
//!   chord), `find_action_for_chord` (catalog lookup honoring user
//!   overrides).
//! - **Detach + clipboard**: `spawn_detached_pilot` (Ctrl-Shift-D
//!   spawn), `emit_clipboard_copy` (OSC 52).
//! - **Run loop entry points**: `run_with_client`, `run_loop_with_model`.
//! - **Misc encoders**: `base64_encode` (OSC 52 payload).
//!
//! Most consumers are siblings (`keys.rs`, `events.rs`, the `view`
//! and `update` methods on `Model`). Co-locating the helpers here
//! keeps mod.rs focused on the `Model` struct + its constructors.

use super::{Model, PaneFocus};
use pilot_ipc::Client;
use std::time::Duration;
use tuirealm::application::PollStrategy;
use tuirealm::event::{Event as RealmEvent, Key, KeyEvent as RealmKey, KeyModifiers};
use tuirealm::ratatui::Frame;
use tuirealm::ratatui::layout::Rect;
use tuirealm::ratatui::widgets::{Block, Borders};
use tuirealm::terminal::TerminalAdapter;

/// True if `(col, row)` lies within `rect`'s half-open bounds.
pub(crate) fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}

/// Paint a drag-selection range as reverse-video over `rect`. `start`
/// and `end` are screen coordinates from the mouse events; we
/// normalize so the lower-row end is the start and the higher-row end
/// is the end, then highlight cells in the visual range:
///
/// - Single-row selection: cells from `min_col` to `max_col`.
/// - Multi-row selection: from `start_col` to end-of-row on the start
///   row, full rows between, and start-of-row to `end_col` on the
///   final row.
///
/// All writes are clipped to `rect` so a drag that strayed outside
/// the terminal pane can't recolor pilot's sidebar or activity feed.
pub(crate) fn paint_selection(
    buf: &mut ratatui::buffer::Buffer,
    rect: Rect,
    start: (u16, u16),
    end: (u16, u16),
) {
    use ratatui::style::Modifier;
    if rect.width == 0 || rect.height == 0 {
        return;
    }
    let max_x = rect.x.saturating_add(rect.width.saturating_sub(1));
    let max_y = rect.y.saturating_add(rect.height.saturating_sub(1));
    // Normalize so `a` is row-earlier or equal to `b`.
    let (a, b) = if (start.1, start.0) <= (end.1, end.0) {
        (start, end)
    } else {
        (end, start)
    };
    // Clamp endpoints to the terminal rect.
    let clamp = |p: (u16, u16)| (p.0.clamp(rect.x, max_x), p.1.clamp(rect.y, max_y));
    let a = clamp(a);
    let b = clamp(b);
    // No-op for a degenerate "click without drag" — Up handler
    // already skips the copy in that case; the highlight pass would
    // just reverse-video one cell, which is more confusing than
    // helpful.
    if a == b {
        return;
    }
    let mut y = a.1;
    while y <= b.1 {
        let row_start = if y == a.1 { a.0 } else { rect.x };
        let row_end = if y == b.1 { b.0 } else { max_x };
        let (lo, hi) = if row_start <= row_end {
            (row_start, row_end)
        } else {
            (row_end, row_start)
        };
        let mut x = lo;
        while x <= hi {
            // `buf[(x, y)]` is bounds-checked but our clamp already
            // guarantees in-range; this just sets the modifier
            // without touching the underlying char.
            let cell = &mut buf[(x, y)];
            cell.set_style(cell.style().add_modifier(Modifier::REVERSED));
            x = x.saturating_add(1);
        }
        if y == max_y {
            break;
        }
        y = y.saturating_add(1);
    }
}

/// Convert a crossterm `KeyEvent` to a typed `KeyChord` for catalog
/// lookup. Uppercase letters auto-shift so `KeyEvent { Char('M'),
/// no_mods }` produces the same chord as `KeyEvent { Char('m'),
/// SHIFT }` — matches the catalog's parser convention. Returns
/// `None` for codes the catalog doesn't model (function keys,
/// release events).
pub(crate) fn key_event_to_chord(
    key: crossterm::event::KeyEvent,
) -> Option<pilot_tui_core::action::KeyChord> {
    use crossterm::event::{KeyCode, KeyModifiers};
    use pilot_tui_core::action::{ChordCode, KeyChord, NamedKey};

    let mut ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let _ = &mut ctrl;
    let mut shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let code = match key.code {
        KeyCode::Char(c) => {
            if c.is_ascii_uppercase() {
                shift = true;
            }
            ChordCode::Char(c.to_ascii_lowercase())
        }
        KeyCode::Tab => ChordCode::Named(NamedKey::Tab),
        KeyCode::Enter => ChordCode::Named(NamedKey::Enter),
        KeyCode::Esc => ChordCode::Named(NamedKey::Esc),
        KeyCode::Backspace => ChordCode::Named(NamedKey::Backspace),
        KeyCode::Up => ChordCode::Named(NamedKey::Up),
        KeyCode::Down => ChordCode::Named(NamedKey::Down),
        KeyCode::Left => ChordCode::Named(NamedKey::Left),
        KeyCode::Right => ChordCode::Named(NamedKey::Right),
        KeyCode::Home => ChordCode::Named(NamedKey::Home),
        KeyCode::End => ChordCode::Named(NamedKey::End),
        KeyCode::PageUp => ChordCode::Named(NamedKey::PageUp),
        KeyCode::PageDown => ChordCode::Named(NamedKey::PageDown),
        KeyCode::Delete => ChordCode::Named(NamedKey::Delete),
        KeyCode::Insert => ChordCode::Named(NamedKey::Insert),
        // Space is reported as Char(' ') by crossterm — covered by
        // the Char arm above. Function keys / unknown variants fall
        // through to None.
        _ => return None,
    };
    let _ = ctrl;
    Some(KeyChord::Single {
        ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
        shift,
        alt,
        code,
    })
}

/// Look up the catalog `Action` matching `chord` in the sections
/// the focused pane should resolve. Globals always match; pane-
/// scoped sections only match when their pane is focused.
///
/// Honors user keybinding overrides from `~/.pilot/config.yaml::ui
/// .action_keys`: each catalog entry's effective chord falls back
/// to its default only when the user hasn't set an override for
/// that `ActionKind::name()`.
///
/// Returns `None` when no catalog entry has a matching chord —
/// the caller falls back to the legacy match arms (used today for
/// navigation keys, latches, and any action whose `default_keys`
/// is a presentation form like `g/G`).
pub(crate) fn find_action_for_chord(
    chord: &pilot_tui_core::action::KeyChord,
    focus: PaneFocus,
    overrides: &std::collections::BTreeMap<String, String>,
) -> Option<&'static pilot_tui_core::action::ActionDef> {
    use pilot_tui_core::action::{ActionDef, Section};
    let allowed = |s: Section| -> bool {
        match (s, focus) {
            (Section::Global, _) => true,
            // Workspace = "operates on the focused workspace". The
            // workspace cursor lives in the sidebar, but it's still
            // the active reference frame when the user is reading
            // the right pane — so accept both. Reply / Shift-V /
            // Shift-G all dual-fire today, and this widening lets
            // their inline match arms retire.
            (Section::Workspace, PaneFocus::Sidebar | PaneFocus::Right) => true,
            // Activity = "operates on the focused activity row" —
            // the row cursor only exists on the right pane.
            (Section::Activity, PaneFocus::Right) => true,
            // Terminal section binds to actual PTY keys; we don't
            // route them through the catalog yet — the terminal
            // pane forwards `all keys` to the PTY and the escape
            // sequence (`]]`) has its own latch logic.
            _ => false,
        }
    };
    ActionDef::all()
        .find(|d| allowed(d.section) && d.effective_chord(overrides).as_ref() == Some(chord))
}

/// Spawn a new `pilot` process pinned to the focused pane's
/// detachable scope. Detached: the new process gets its own session
/// so closing the parent doesn't kill it. Errors are logged, not
/// surfaced — detach is best-effort UX.
pub(crate) fn spawn_detached_pilot(spec: &crate::pane::DetachSpec) {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!("detach: current_exe unavailable: {e}");
            return;
        }
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(&spec.args);
    // Decouple from the parent so closing this pilot doesn't take
    // the detached one with it. Implementation lives in
    // `crate::platform` — setsid() on unix, DETACHED_PROCESS on
    // Windows (TODO).
    crate::platform::detach_child_process(&mut cmd);
    cmd.stdin(std::process::Stdio::null());
    cmd.stdout(std::process::Stdio::null());
    cmd.stderr(std::process::Stdio::null());
    if let Err(e) = cmd.spawn() {
        tracing::warn!("detach: spawn failed: {e}");
    }
}

/// Carve the bottom row off for the footer. Returns
/// (pane_area, footer_area) — `pane_area` is what the three panes
/// fill; `footer_area` is the 1-row hint/status line at the bottom.
pub(crate) fn split_for_footer(area: Rect) -> (Rect, Rect) {
    if area.height < 2 {
        return (area, Rect::default());
    }
    let pane = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: area.height - 1,
    };
    let footer = Rect {
        x: area.x,
        y: area.y + area.height - 1,
        width: area.width,
        height: 1,
    };
    (pane, footer)
}

#[allow(dead_code)]
fn placeholder(f: &mut Frame, area: Rect) {
    let block = Block::default()
        .title(" pilot · realm migration scaffold ")
        .borders(Borders::ALL);
    f.render_widget(block, area);
}

/// Run the realm-based pilot loop with a pre-built IPC client.
/// `main.rs::run_embedded_realm` constructs the client + daemon pair
/// before calling this so the daemon is already serving when the UI
/// boots.
pub fn run_with_client(client: Client) -> anyhow::Result<()> {
    let mut model = Model::new(client)?;
    let result = run_loop(&mut model);
    model.shutdown();
    result
}

/// Test-only: run with an unconnected client. Useful for manual
/// smoke tests without spinning up the full daemon stack.
#[allow(dead_code)]
pub fn run() -> anyhow::Result<()> {
    let (client, _server) = pilot_ipc::channel::pair();
    run_with_client(client)
}

/// Run the loop on a pre-configured model. Used by
/// `main::run_embedded_realm` so it can install the on-setup-complete
/// hook + start the wizard before entering the loop.
pub fn run_loop_with_model<T: TerminalAdapter>(mut model: Model<T>) -> anyhow::Result<()> {
    let result = run_loop(&mut model);
    model.shutdown();
    result
}

fn run_loop<T: TerminalAdapter>(model: &mut Model<T>) -> anyhow::Result<()> {
    while !model.quit {
        // 1. Drain inbound daemon events (cheap try_recv).
        while let Ok(evt) = model.client.rx.try_recv() {
            model.handle_daemon_event(evt);
        }

        // 2. Polling-modal spinner heartbeat + retryable notice fade.
        if let Some(msg) = model.polling_tick() {
            model.dismiss_polling();
            model.update(msg);
        }
        model.tick_notice();
        model.tick_right();
        model.tick_working();

        // 3. Process tuirealm-side messages (timer ticks for Loading,
        // injected modal keys). Non-blocking — listener thread already
        // queued any work it had.
        if let Ok(messages) = model.app.tick(PollStrategy::Once(Duration::ZERO)) {
            if !messages.is_empty() {
                model.redraw = true;
                for msg in messages {
                    model.update(msg);
                }
            }
        }

        // 4. Render if dirty — before the blocking input read so the
        // user sees their last action immediately.
        if model.redraw {
            // Per-frame timing log behind the `pilot=debug` filter.
            // Lets us see in `/tmp/pilot.log` whether a slow scroll
            // is the render itself (would show large `frame_ms`)
            // versus daemon round-trips between renders. Cheap —
            // `Instant::now` is ~10ns and `tracing::debug!` is a
            // no-op when the level isn't enabled.
            let t = std::time::Instant::now();
            model.view();
            let elapsed_ms = t.elapsed().as_micros() as f32 / 1000.0;
            tracing::debug!(frame_ms = elapsed_ms, "render");
            model.redraw = false;
        }

        // 5. Block briefly for input. One event per iteration,
        // render between events — the "drain all then render once"
        // pattern looked good on paper (fewer renders per second)
        // but broke scroll fluidity: a 30-event trackpad gesture
        // collapsed into a single jump-cut render, so the user saw
        // the screen teleport from start to end with no
        // intermediate frames ("not progressive, I don't even see
        // which direction I'm going"). The render cost is 1-2ms
        // (verified via the `render frame_ms` debug log) so per-
        // event rendering at 50-100Hz easily keeps up.
        //
        // The 16ms poll is the IDLE-WAIT bound: when no events are
        // queued, we block here up to one display refresh worth.
        // With events queued, `poll` returns immediately — we don't
        // pay the 16ms; the loop body runs again. So during an
        // active scroll burst, this loop runs as fast as the
        // render + daemon-roundtrip allows, which is what gives
        // the progressive-scroll feel.
        const POLL_IDLE: Duration = Duration::from_millis(16);
        if let Ok(true) = crossterm::event::poll(POLL_IDLE)
            && let Ok(event) = crossterm::event::read()
        {
            dispatch_event(model, event);
        }
    }
    Ok(())
}

/// Route one crossterm event to the right handler. Extracted from
/// the run-loop body so the loop can `dispatch_event` once per
/// poll, then poll(0) to drain the rest before rendering — the
/// batching is what turns 20 scroll-wheel events into 1 frame.
fn dispatch_event<T: TerminalAdapter>(model: &mut Model<T>, event: crossterm::event::Event) {
    match event {
        crossterm::event::Event::Key(key) => {
            // With KeyboardEnhancementFlags::REPORT_EVENT_TYPES pushed
            // at startup, the host terminal distinguishes Press /
            // Repeat / Release. We skip Release only — Repeat must
            // be honored so held keys autorepeat (arrow keys in
            // Claude code, holding j to scroll, etc.). The previous
            // filter skipped Repeat too, which made every "held key"
            // feel broken even though Backspace worked (Backspace
            // events arrive as Press from the terminal's auto-repeat
            // emulation when extended keyboards aren't on).
            if matches!(key.kind, crossterm::event::KeyEventKind::Release) {
                return;
            }
            let realm_key = crossterm_to_realm(key);
            if model.modal_stack.is_empty() {
                model.handle_pane_key(realm_key);
            } else {
                let _ = model.modal_event_tx.send(RealmEvent::Keyboard(realm_key));
                // ChannelPort is polled by the listener thread every
                // 10ms, so a tight 15ms window often expires before
                // the listener delivers the event we just pushed —
                // the keypress sits in the channel and isn't acted on
                // until the user presses another key. The Confirm
                // modal showed this loudly: "Y not responsive; Esc
                // worked after a few tries".
                //
                // Poll in a short loop with a 150ms deadline so we
                // keep checking until messages arrive or the user
                // perceives latency. 150ms is well under the human-
                // noticeable threshold for key feedback but long
                // enough to absorb the 10ms listener cadence + jitter.
                let deadline = std::time::Instant::now() + Duration::from_millis(150);
                let mut handled = false;
                loop {
                    match model.app.tick(PollStrategy::Once(Duration::ZERO)) {
                        Ok(messages) if !messages.is_empty() => {
                            for msg in messages {
                                model.update(msg);
                            }
                            handled = true;
                            break;
                        }
                        Ok(_) => {}
                        Err(_) => break,
                    }
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                // After the first tick lands, drain anything else the
                // modal pushed in the same window — a single tuirealm
                // `Cmd` can fan out into multiple `Msg`s and we don't
                // want them to straggle into the next keypress.
                if handled && let Ok(messages) = model.app.tick(PollStrategy::Once(Duration::ZERO))
                {
                    for msg in messages {
                        model.update(msg);
                    }
                }
                // Modals can mutate internal state without producing a
                // `Msg`, so force a redraw too.
                model.redraw = true;
            }
        }
        crossterm::event::Event::Mouse(m) => {
            if model.modal_stack.is_empty() {
                model.handle_mouse(m);
            }
        }
        crossterm::event::Event::Paste(text) => {
            // Bracketed paste arrived. Two destinations depending on
            // where focus is — both go through `handle_paste` which
            // inspects pane state.
            if model.modal_stack.is_empty() {
                model.handle_paste(&text);
            } else {
                // Modal owns input — forward as raw text via the
                // modal event channel. The textarea modal will see
                // this as a multi-char paste and insert at cursor.
                let _ = model.modal_event_tx.send(RealmEvent::Paste(text));
            }
        }
        _ => {}
    }
}

fn crossterm_to_realm(key: crossterm::event::KeyEvent) -> RealmKey {
    use crossterm::event::{KeyCode as CKC, KeyModifiers as CKM};
    let code = match key.code {
        CKC::Char(c) => Key::Char(c),
        CKC::Enter => Key::Enter,
        CKC::Esc => Key::Esc,
        CKC::Backspace => Key::Backspace,
        CKC::Left => Key::Left,
        CKC::Right => Key::Right,
        CKC::Up => Key::Up,
        CKC::Down => Key::Down,
        CKC::Home => Key::Home,
        CKC::End => Key::End,
        CKC::PageUp => Key::PageUp,
        CKC::PageDown => Key::PageDown,
        CKC::Tab => Key::Tab,
        CKC::BackTab => Key::BackTab,
        CKC::Delete => Key::Delete,
        CKC::Insert => Key::Insert,
        CKC::F(n) => Key::Function(n),
        _ => Key::Null,
    };
    let mut mods = KeyModifiers::empty();
    if key.modifiers.contains(CKM::SHIFT) {
        mods |= KeyModifiers::SHIFT;
    }
    if key.modifiers.contains(CKM::CONTROL) {
        mods |= KeyModifiers::CONTROL;
    }
    if key.modifiers.contains(CKM::ALT) {
        mods |= KeyModifiers::ALT;
    }
    RealmKey::new(code, mods)
}

/// Write OSC 52 clipboard-set to the host terminal's stdout. The host
/// (Ghostty / iTerm2 / Kitty / WezTerm) lands the text on the system
/// clipboard. Format: `ESC ] 52 ; c ; <base64> ESC \`. Wraps the
/// pilot-side "copy from terminal selection" gesture — without OSC 52
/// the extracted text would just live in memory.
pub(crate) fn emit_clipboard_copy(text: &str) {
    let encoded = base64_encode(text.as_bytes());
    let sequence = format!("\x1b]52;c;{encoded}\x1b\\");
    use std::io::Write;
    let _ = std::io::stdout().write_all(sequence.as_bytes());
    let _ = std::io::stdout().flush();
}

/// Tiny RFC 4648 base64 encoder. Pilot doesn't have a `base64` dep
/// and pulling one in for one OSC 52 call is overkill. ~25 lines,
/// allocation-free aside from the output `String`.
pub(super) fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut i = 0;
    while i + 3 <= bytes.len() {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8) | (bytes[i + 2] as u32);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push(ALPHABET[(n & 0x3f) as usize] as char);
        i += 3;
    }
    let rem = bytes.len() - i;
    if rem == 1 {
        let n = (bytes[i] as u32) << 16;
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rem == 2 {
        let n = ((bytes[i] as u32) << 16) | ((bytes[i + 1] as u32) << 8);
        out.push(ALPHABET[((n >> 18) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 12) & 0x3f) as usize] as char);
        out.push(ALPHABET[((n >> 6) & 0x3f) as usize] as char);
        out.push('=');
    }
    out
}
