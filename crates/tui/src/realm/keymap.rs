//! Tiny utility: translate `tuirealm::event::KeyEvent` into
//! `crossterm::event::KeyEvent` so existing lazybox panes (which take
//! crossterm events) can be wrapped without rewriting their key
//! handlers. Goes away once the panes are fully ported and stop
//! delegating through UFCS.

use crossterm::event::{KeyCode as CKC, KeyModifiers as CKM};
use tuirealm::event::{Key, KeyEvent, KeyModifiers as RKM};

/// Translate a tuirealm key event to its crossterm equivalent.
pub fn realm_key_to_crossterm(key: &KeyEvent) -> crossterm::event::KeyEvent {
    let code = match key.code {
        Key::Char(c) => CKC::Char(c),
        Key::Enter => CKC::Enter,
        Key::Esc => CKC::Esc,
        Key::Backspace => CKC::Backspace,
        Key::Left => CKC::Left,
        Key::Right => CKC::Right,
        Key::Up => CKC::Up,
        Key::Down => CKC::Down,
        Key::Home => CKC::Home,
        Key::End => CKC::End,
        Key::PageUp => CKC::PageUp,
        Key::PageDown => CKC::PageDown,
        Key::Tab => CKC::Tab,
        Key::BackTab => CKC::BackTab,
        Key::Delete => CKC::Delete,
        Key::Insert => CKC::Insert,
        Key::Function(n) => CKC::F(n),
        Key::Null => CKC::Null,
        // Realm has a few extras (CapsLock, ScrollLock, Menu, …);
        // lazybox's keymaps ignore them, so treat as Null.
        _ => CKC::Null,
    };
    let realm_mods = key.modifiers;
    let mut modifiers = CKM::empty();
    if realm_mods.contains(RKM::SHIFT) {
        modifiers |= CKM::SHIFT;
    }
    if realm_mods.contains(RKM::CONTROL) {
        modifiers |= CKM::CONTROL;
    }
    if realm_mods.contains(RKM::ALT) {
        modifiers |= CKM::ALT;
    }
    crossterm::event::KeyEvent::new(code, modifiers)
}
