//! Helpers for rendering a task as a single sidebar row:
//!
//! - `pr_number(task)` — extracts the trailing `#NNN` from `task.id.key`.
//!   The pure extractor moved to `lazybox_tui_core::inbox` (#731, it's a
//!   dependency of the client-free search); re-exported here so row
//!   renderers keep their `task_label::pr_number` path.
//! - `task_identifier(task)` / `identifier_width(task)` — the identifier
//!   column's text: the PR number, or a tracker key (`ENG-123`) when the
//!   key has no `#N` suffix.
//! - `identifier_color(task)` — deterministic color for that identifier.
//! - `pr_number_color(n)` — deterministic color from the PR number.
//!   Same number → same color across renders. Stays here — it produces
//!   a ratatui `Color`.

use ratatui::style::Color;

pub use lazybox_tui_core::inbox::{identifier_width, pr_number, task_identifier};

/// Stable color for a task's identifier: the PR number's palette slot
/// for GitHub keys, a hash of the key for tracker identifiers, so
/// `ENG-123` keeps one color across renders like `#312` does.
pub fn identifier_color(task: &lazybox_core::Task) -> Color {
    let n = pr_number(task).unwrap_or_else(|| {
        task.id
            .key
            .bytes()
            .fold(0u64, |h, b| h.wrapping_mul(31).wrapping_add(b as u64))
    });
    pr_number_color(n)
}

/// Stable color for a PR number. Same number → same color across
/// renders (and across launches — no RNG state). Picked from a
/// 6-color palette that stays readable on dark terminal backgrounds.
pub fn pr_number_color(n: u64) -> Color {
    // Deliberately small palette: the goal is "different from your
    // neighbour", not "256 unique colors". Adjacent PR numbers tend
    // to fall in different slots which is what the eye notices.
    const PALETTE: [Color; 6] = [
        Color::Cyan,
        Color::Magenta,
        Color::Blue,
        Color::Yellow,
        Color::Green,
        Color::LightRed,
    ];
    PALETTE[(n as usize) % PALETTE.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_number_color_is_deterministic() {
        // Same number gives same color across calls.
        assert_eq!(pr_number_color(42), pr_number_color(42));
        assert_eq!(pr_number_color(0), pr_number_color(0));
    }

    #[test]
    fn pr_number_color_varies_across_palette() {
        // Six distinct PR numbers should hit every palette slot.
        let colors: std::collections::HashSet<_> = (0..6).map(pr_number_color).collect();
        assert_eq!(colors.len(), 6);
    }
}
