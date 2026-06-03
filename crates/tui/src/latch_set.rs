//! Group of `ConfirmLatch`es indexed by their triggering key.
//!
//! Sidebar has two two-press-confirm actions (`Shift-X` kill,
//! `Shift-Z` long-snooze) — both wanting:
//! - "first press arms with the focused session as payload";
//! - "second press of the SAME key fires";
//! - "any other key disarms".
//!
//! Three identical fields + three identical disarm-on-other-key
//! lines is the kind of duplication that drifts: a new confirm
//! action needs three new edits, and forgetting the disarm one
//! leaves the latch in a half-armed state across redraws. This
//! type folds the registry into one struct.
//!
//! Generic over the payload `K` (sidebar uses `SessionKey`). The
//! trigger is a `(KeyCode, KeyModifiers)` pair — the same shape
//! the existing handle_key match statement uses, so wiring is
//! mechanical.

use crossterm::event::{KeyCode, KeyModifiers};
use lazybox_tui_core::confirm_latch::ConfirmLatch;

/// A keystroke that arms / fires a latch. `Shift-X` is
/// `KeyTrigger { code: Char('X'), modifiers: SHIFT }`. Match
/// semantics: the input's modifiers must `.contains()` the
/// trigger's — extra modifiers (e.g. Shift+X+Ctrl) still match
/// `Shift-X`, mirroring the prior inline check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyTrigger {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyTrigger {
    pub const fn new(code: KeyCode, modifiers: KeyModifiers) -> Self {
        Self { code, modifiers }
    }

    /// Does an input keystroke match this trigger? Uses `.contains()`
    /// so a SHIFT trigger matches SHIFT-only AND SHIFT+anything-else.
    pub fn matches(&self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        self.code == code && modifiers.contains(self.modifiers)
    }
}

/// A named registry of `ConfirmLatch`es indexed by trigger
/// keystroke. Generic over the payload `K` (typically `SessionKey`).
pub struct LatchSet<K> {
    latches: Vec<(KeyTrigger, ConfirmLatch<K>)>,
}

impl<K> Default for LatchSet<K> {
    fn default() -> Self {
        Self {
            latches: Vec::new(),
        }
    }
}

impl<K: Clone + PartialEq> LatchSet<K> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a fresh latch for `trigger`. Idempotent: calling
    /// register twice for the same trigger replaces the first.
    pub fn register(&mut self, trigger: KeyTrigger) {
        if let Some(slot) = self.latches.iter_mut().find(|(t, _)| *t == trigger) {
            slot.1 = ConfirmLatch::new();
        } else {
            self.latches.push((trigger, ConfirmLatch::new()));
        }
    }

    /// Is `trigger`'s latch currently armed? Returns the payload
    /// from the arm call. `None` if not armed or trigger isn't
    /// registered.
    pub fn armed(&self, trigger: KeyTrigger) -> Option<&K> {
        self.latches
            .iter()
            .find(|(t, _)| *t == trigger)
            .and_then(|(_, l)| l.armed())
    }

    /// Arm-or-fire on `trigger` with `target`. Returns `true` if
    /// this press *fired* (second press with the same target),
    /// `false` if it merely armed. No-op when trigger isn't
    /// registered.
    pub fn arm_or_fire(&mut self, trigger: KeyTrigger, target: K) -> bool {
        self.latches
            .iter_mut()
            .find(|(t, _)| *t == trigger)
            .map(|(_, l)| l.arm_or_fire(target))
            .unwrap_or(false)
    }

    /// Disarm every latch whose trigger doesn't match the given
    /// keystroke. Call once at the top of `handle_key` — replaces
    /// N inline `if !is_shift_X { latch.disarm() }` lines.
    pub fn disarm_others(&mut self, code: KeyCode, modifiers: KeyModifiers) {
        for (trigger, latch) in &mut self.latches {
            if !trigger.matches(code, modifiers) {
                latch.disarm();
            }
        }
    }

    /// Disarm a specific latch by trigger. No-op when not
    /// registered.
    pub fn disarm(&mut self, trigger: KeyTrigger) {
        if let Some((_, latch)) = self.latches.iter_mut().find(|(t, _)| *t == trigger) {
            latch.disarm();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shift_x() -> KeyTrigger {
        KeyTrigger::new(KeyCode::Char('X'), KeyModifiers::SHIFT)
    }

    fn shift_z() -> KeyTrigger {
        KeyTrigger::new(KeyCode::Char('Z'), KeyModifiers::SHIFT)
    }

    #[test]
    fn unregistered_trigger_armed_returns_none() {
        let s: LatchSet<u32> = LatchSet::new();
        assert!(s.armed(shift_x()).is_none());
    }

    #[test]
    fn arm_or_fire_arms_on_first_press_and_fires_on_second() {
        let mut s = LatchSet::new();
        s.register(shift_x());
        assert!(!s.arm_or_fire(shift_x(), 7u32));
        assert_eq!(s.armed(shift_x()).copied(), Some(7));
        assert!(s.arm_or_fire(shift_x(), 7));
        // After firing, the latch is disarmed.
        assert!(s.armed(shift_x()).is_none());
    }

    /// Two latches with different triggers each maintain their
    /// own armed state.
    #[test]
    fn separate_triggers_keep_independent_armed_state() {
        let mut s = LatchSet::new();
        s.register(shift_x());
        s.register(shift_z());
        s.arm_or_fire(shift_x(), 1u32);
        assert!(s.armed(shift_x()).is_some());
        assert!(s.armed(shift_z()).is_none());
    }

    /// `disarm_others(code, mods)` disarms every latch whose
    /// trigger doesn't match — the central replacement for inline
    /// `if !is_shift_X { latch.disarm() }` blocks.
    #[test]
    fn disarm_others_keeps_matching_latch_armed() {
        let mut s = LatchSet::new();
        s.register(shift_x());
        s.register(shift_z());
        s.arm_or_fire(shift_x(), 1u32);
        s.arm_or_fire(shift_z(), 2u32);
        // Press Shift-X — should keep X armed, disarm Z.
        s.disarm_others(KeyCode::Char('X'), KeyModifiers::SHIFT);
        assert!(s.armed(shift_x()).is_some());
        assert!(s.armed(shift_z()).is_none());
    }

    /// Any unrelated keypress disarms BOTH (matches the pre-
    /// refactor inline behavior).
    #[test]
    fn disarm_others_on_unrelated_key_disarms_all() {
        let mut s = LatchSet::new();
        s.register(shift_x());
        s.register(shift_z());
        s.arm_or_fire(shift_x(), 1u32);
        s.arm_or_fire(shift_z(), 2u32);
        s.disarm_others(KeyCode::Char('j'), KeyModifiers::NONE);
        assert!(s.armed(shift_x()).is_none());
        assert!(s.armed(shift_z()).is_none());
    }

    /// SHIFT trigger matches SHIFT+other-mod (mirrors `.contains()`
    /// semantics from the prior inline check).
    #[test]
    fn trigger_modifier_match_is_subset_via_contains() {
        let t = shift_x();
        assert!(t.matches(KeyCode::Char('X'), KeyModifiers::SHIFT));
        assert!(t.matches(KeyCode::Char('X'), KeyModifiers::SHIFT | KeyModifiers::ALT,));
        // Missing the required SHIFT modifier → no match.
        assert!(!t.matches(KeyCode::Char('X'), KeyModifiers::NONE));
    }

    /// Re-registering a trigger replaces the latch state (so the
    /// caller gets a fresh latch, not a half-armed one).
    #[test]
    fn register_replaces_existing_latch_state() {
        let mut s = LatchSet::new();
        s.register(shift_x());
        s.arm_or_fire(shift_x(), 1u32);
        assert!(s.armed(shift_x()).is_some());
        s.register(shift_x());
        assert!(s.armed(shift_x()).is_none());
    }
}
