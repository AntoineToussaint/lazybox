//! Client-free palette catalog — the single source of truth for the
//! built-in theme colors.
//!
//! The ratatui TUI (`lazybox-tui`) builds its render-time `Theme`
//! values from these palettes, and the non-ratatui clients (the Tauri
//! desktop) render their own swatches from the same data — exposed
//! through the desktop settings read command — so every surface agrees
//! on the theme list without re-declaring colors. Keeping the raw
//! colors here (rather than in the ratatui crate) lets the desktop read
//! them without depending on ratatui.
//!
//! Slots are *semantic*, not chromatic — `accent`, not `cyan`. See
//! `crates/tui/src/theme.rs` for how each slot is used at render time
//! and `docs/themes.md` for shipping a custom palette.

/// One RGB triple. Truecolor only — every built-in slot is an explicit
/// `(r, g, b)` so the result is consistent across terminal palettes.
pub type Rgb = (u8, u8, u8);

/// Raw color data for one built-in theme. Mirrors the semantic slots of
/// the ratatui `Theme` one-to-one; the render crate maps each slot to a
/// `ratatui::style::Color`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThemePalette {
    /// Human-readable name, shown in the theme picker / status bar.
    pub name: &'static str,
    /// Primary accent — focus rings, active titles, the breadcrumb.
    pub accent: Rgb,
    /// Active / hovered selection.
    pub hover: Rgb,
    /// Success states (open PRs, CI passing).
    pub success: Rgb,
    /// Attention without alarm — unread badges, draft state.
    pub warn: Rgb,
    /// Hard errors — closed PRs, CI failure, panics.
    pub error: Rgb,
    /// Body text emphasis (bold rows, focused selection foreground).
    pub text_strong: Rgb,
    /// Dimmed / secondary text (timestamps, counts).
    pub text_dim: Rgb,
    /// Chrome — borders, dividers, separators, the splitter line.
    pub chrome: Rgb,
    /// Solid background block for highlighted rows / mode pill bg.
    pub fill: Rgb,
    /// Surface background — modals, panels, popovers.
    pub surface: Rgb,
}

impl ThemePalette {
    /// Render an RGB triple as a `#rrggbb` hex string — the form web
    /// clients (xterm.js, CSS custom properties) consume directly.
    pub fn hex(rgb: Rgb) -> String {
        format!("#{:02x}{:02x}{:02x}", rgb.0, rgb.1, rgb.2)
    }
}

/// Lazybox Dark — the default.
pub const LAZYBOX_DARK: ThemePalette = ThemePalette {
    name: "Lazybox Dark",
    accent: (125, 207, 255),
    hover: (247, 118, 142),
    success: (158, 206, 106),
    warn: (224, 175, 104),
    error: (247, 118, 142),
    text_strong: (192, 202, 245),
    text_dim: (128, 136, 173),
    chrome: (58, 64, 96),
    fill: (41, 46, 66),
    surface: (26, 29, 46),
};

/// Catppuccin Mocha — popular dark, soft pastels.
pub const CATPPUCCIN_MOCHA: ThemePalette = ThemePalette {
    name: "Catppuccin Mocha",
    accent: (137, 220, 235),
    hover: (245, 194, 231),
    success: (166, 227, 161),
    warn: (249, 226, 175),
    error: (243, 139, 168),
    text_strong: (205, 214, 244),
    text_dim: (147, 153, 178),
    chrome: (69, 71, 90),
    fill: (49, 50, 68),
    surface: (30, 30, 46),
};

/// Tokyo Night — cooler, navy-leaning dark theme.
pub const TOKYO_NIGHT: ThemePalette = ThemePalette {
    name: "Tokyo Night",
    accent: (125, 207, 255),
    hover: (187, 154, 247),
    success: (158, 206, 106),
    warn: (224, 175, 104),
    error: (247, 118, 142),
    text_strong: (192, 202, 245),
    text_dim: (86, 95, 137),
    chrome: (65, 72, 104),
    fill: (41, 46, 66),
    surface: (26, 27, 38),
};

/// Gruvbox Dark — earthy retro palette.
pub const GRUVBOX_DARK: ThemePalette = ThemePalette {
    name: "Gruvbox Dark",
    accent: (131, 165, 152),
    hover: (211, 134, 155),
    success: (184, 187, 38),
    warn: (250, 189, 47),
    error: (251, 73, 52),
    text_strong: (235, 219, 178),
    text_dim: (168, 153, 132),
    chrome: (80, 73, 69),
    fill: (60, 56, 54),
    surface: (40, 40, 40),
};

/// Rose Pine — low-saturation pastels on a deep purple backdrop.
pub const ROSE_PINE: ThemePalette = ThemePalette {
    name: "Rose Pine",
    accent: (156, 207, 216),
    hover: (196, 167, 231),
    success: (49, 116, 143),
    warn: (246, 193, 119),
    error: (235, 111, 146),
    text_strong: (224, 222, 244),
    text_dim: (144, 140, 170),
    chrome: (57, 53, 82),
    fill: (38, 35, 58),
    surface: (25, 23, 36),
};

/// Lazybox Light — the one bright palette.
pub const LAZYBOX_LIGHT: ThemePalette = ThemePalette {
    name: "Lazybox Light",
    accent: (26, 110, 196),
    hover: (193, 53, 116),
    success: (35, 134, 78),
    warn: (159, 106, 0),
    error: (193, 53, 116),
    text_strong: (28, 32, 48),
    text_dim: (96, 104, 128),
    chrome: (196, 201, 214),
    fill: (218, 223, 233),
    surface: (247, 248, 250),
};

/// High Contrast — accessibility-first.
pub const HIGH_CONTRAST: ThemePalette = ThemePalette {
    name: "High Contrast",
    accent: (0, 224, 255),
    hover: (255, 110, 200),
    success: (80, 240, 120),
    warn: (255, 214, 64),
    error: (255, 92, 92),
    text_strong: (255, 255, 255),
    text_dim: (200, 200, 200),
    chrome: (160, 160, 160),
    fill: (58, 58, 58),
    surface: (0, 0, 0),
};

/// Built-in palettes in cycle order. Index 0 is the default. The
/// ratatui crate's `BUILT_IN_THEMES` and the desktop theme picker both
/// read this ordering.
pub const BUILT_IN_PALETTES: &[ThemePalette] = &[
    LAZYBOX_DARK,
    CATPPUCCIN_MOCHA,
    TOKYO_NIGHT,
    GRUVBOX_DARK,
    ROSE_PINE,
    LAZYBOX_LIGHT,
    HIGH_CONTRAST,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_pads_each_channel_to_two_digits() {
        assert_eq!(ThemePalette::hex((0, 0, 0)), "#000000");
        assert_eq!(ThemePalette::hex((255, 255, 255)), "#ffffff");
        assert_eq!(ThemePalette::hex((26, 110, 196)), "#1a6ec4");
    }

    #[test]
    fn built_in_palette_names_are_unique() {
        let mut names: Vec<_> = BUILT_IN_PALETTES.iter().map(|p| p.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate built-in palette name");
    }

    #[test]
    fn ships_a_light_and_a_dark_default_pole() {
        // The picker needs both poles; guard against an edit that drops
        // one. Luminance of the surface separates them.
        let luminance =
            |c: Rgb| (0.299 * c.0 as f32 + 0.587 * c.1 as f32 + 0.114 * c.2 as f32) / 255.0;
        let lightest = BUILT_IN_PALETTES
            .iter()
            .map(|p| luminance(p.surface))
            .fold(0.0_f32, f32::max);
        let darkest = BUILT_IN_PALETTES
            .iter()
            .map(|p| luminance(p.surface))
            .fold(1.0_f32, f32::min);
        assert!(lightest > 0.8, "no light palette (brightest {lightest})");
        assert!(darkest < 0.1, "no dark palette (darkest {darkest})");
    }
}
