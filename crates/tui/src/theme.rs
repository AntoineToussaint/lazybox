//! Centralized color palette + style helpers.
//!
//! ## Why a runtime-switchable theme
//!
//! Lazybox ships several built-in palettes (Lazybox Dark, Catppuccin
//! Mocha, Tokyo Night, Gruvbox Dark, Rose Pine). The active palette
//! lives behind `theme::current()`; every component reads from it
//! instead of hard-coding `Color::*` literals. Switching themes at
//! runtime is then a single atomic store — no rebuild, no reload,
//! every render after the switch picks up the new palette.
//!
//! ## How a theme is built
//!
//! Slots are *semantic*, not chromatic — `accent`, not `cyan`. Each
//! theme picks a hue for each slot. `text_strong` / `text_dim` /
//! `chrome` / `fill` are calibrated against the same dark backdrop so
//! contrast stays acceptable across every palette.
//!
//! ## Adding a theme
//!
//! 1. Add a `pub const` Theme literal below.
//! 2. Append it to the `THEMES` slice.
//!
//! That's it — `T` cycles include the new entry, and any component
//! reading from `theme::current()` updates automatically.
//!
//! ## Color tokens
//!
//! Themes use `Color::Rgb(r, g, b)` literals so the result is
//! consistent across terminal palettes. Modern terminals (iTerm2,
//! Ghostty, WezTerm, Kitty, Alacritty, modern macOS Terminal) all
//! render truecolor; for the rare 8-color holdouts we still get
//! reasonable approximations.

use ratatui::style::{Color, Modifier, Style};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{OnceLock, RwLock};

/// Lazybox's color palette. One global instance via `THEME`.
#[derive(Clone)]
pub struct Theme {
    /// Human-readable name, shown in the theme picker / status bar.
    pub name: &'static str,
    /// Primary accent. Focus rings, active titles, the breadcrumb.
    pub accent: Color,
    /// Active / hovered selection. Used for "the row your cursor is
    /// on" inside an unfocused list, or "currently active tab".
    pub hover: Color,
    /// Success states (open PRs, CI passing, etc.).
    pub success: Color,
    /// Attention without alarm — unread badges, draft state, perf bumps.
    pub warn: Color,
    /// Hard errors — closed PRs, CI failure, panics.
    pub error: Color,
    /// Body text emphasis (bold rows, focused selection foreground).
    pub text_strong: Color,
    /// Dimmed/secondary text (timestamps, counts, "rest" key chord).
    pub text_dim: Color,
    /// Chrome — borders, dividers, separators, the splitter line.
    pub chrome: Color,
    /// Solid background block used for highlighted rows / mode pill bg.
    pub fill: Color,
    /// Surface background — modals, panels, popovers. Distinct from
    /// `fill` so a modal-on-row doesn't disappear into the row's
    /// highlight; calibrated against `text_strong` for body contrast.
    pub surface: Color,
}

/// Lazybox Dark — the default. Restrained palette in the spirit of
/// yazi's defaults: one cyan accent, one magenta hover, otherwise
/// grays. Calibrated against a near-black terminal background.
pub const LAZYBOX_DARK: Theme = Theme {
    name: "Lazybox Dark",
    accent: Color::Rgb(125, 207, 255),      // soft sky blue
    hover: Color::Rgb(247, 118, 142),       // muted coral
    success: Color::Rgb(158, 206, 106),     // sage green
    warn: Color::Rgb(224, 175, 104),        // warm amber
    error: Color::Rgb(247, 118, 142),       // same as hover for cohesion
    text_strong: Color::Rgb(192, 202, 245), // off-white
    text_dim: Color::Rgb(122, 130, 167),    // muted blue-gray
    chrome: Color::Rgb(58, 64, 96),         // slate divider
    fill: Color::Rgb(41, 46, 66),           // panel bg
    surface: Color::Rgb(26, 29, 46),        // modal/panel bg, deeper
};

/// Catppuccin Mocha — popular dark, soft pastels. Balances pinks +
/// blues, has a deserved reputation for being easy on the eyes during
/// long sessions.
pub const CATPPUCCIN_MOCHA: Theme = Theme {
    name: "Catppuccin Mocha",
    accent: Color::Rgb(137, 220, 235),      // sky
    hover: Color::Rgb(245, 194, 231),       // pink
    success: Color::Rgb(166, 227, 161),     // green
    warn: Color::Rgb(249, 226, 175),        // yellow
    error: Color::Rgb(243, 139, 168),       // pink-red
    text_strong: Color::Rgb(205, 214, 244), // text
    text_dim: Color::Rgb(147, 153, 178),    // overlay2
    chrome: Color::Rgb(69, 71, 90),         // surface1
    fill: Color::Rgb(49, 50, 68),           // surface0
    surface: Color::Rgb(30, 30, 46),        // base
};

/// Tokyo Night — slightly cooler, navy-leaning dark theme. Higher
/// contrast than Catppuccin; reads well on OLED.
pub const TOKYO_NIGHT: Theme = Theme {
    name: "Tokyo Night",
    accent: Color::Rgb(125, 207, 255),      // blue
    hover: Color::Rgb(187, 154, 247),       // magenta
    success: Color::Rgb(158, 206, 106),     // green
    warn: Color::Rgb(224, 175, 104),        // orange
    error: Color::Rgb(247, 118, 142),       // red
    text_strong: Color::Rgb(192, 202, 245), // fg
    text_dim: Color::Rgb(86, 95, 137),      // comment
    chrome: Color::Rgb(65, 72, 104),        // bg_visual
    fill: Color::Rgb(41, 46, 66),           // bg_highlight
    surface: Color::Rgb(26, 27, 38),        // bg
};

/// Gruvbox Dark — earthy retro palette. Warmer ambers + olive greens,
/// for users who like the classic vim feel.
pub const GRUVBOX_DARK: Theme = Theme {
    name: "Gruvbox Dark",
    accent: Color::Rgb(131, 165, 152),      // aqua
    hover: Color::Rgb(211, 134, 155),       // pink
    success: Color::Rgb(184, 187, 38),      // green
    warn: Color::Rgb(250, 189, 47),         // yellow
    error: Color::Rgb(251, 73, 52),         // red
    text_strong: Color::Rgb(235, 219, 178), // fg
    text_dim: Color::Rgb(168, 153, 132),    // gray
    chrome: Color::Rgb(80, 73, 69),         // bg2
    fill: Color::Rgb(60, 56, 54),           // bg1
    surface: Color::Rgb(40, 40, 40),        // bg
};

/// Rose Pine — soothing low-saturation pastels on a deep purple
/// backdrop. Distinct from the others; lots of users swear by it.
pub const ROSE_PINE: Theme = Theme {
    name: "Rose Pine",
    accent: Color::Rgb(156, 207, 216),      // foam
    hover: Color::Rgb(196, 167, 231),       // iris
    success: Color::Rgb(49, 116, 143),      // pine
    warn: Color::Rgb(246, 193, 119),        // gold
    error: Color::Rgb(235, 111, 146),       // love
    text_strong: Color::Rgb(224, 222, 244), // text
    text_dim: Color::Rgb(144, 140, 170),    // subtle
    chrome: Color::Rgb(57, 53, 82),         // overlay
    fill: Color::Rgb(38, 35, 58),           // surface
    surface: Color::Rgb(25, 23, 36),        // base
};

/// Built-in themes shipped with the kit, in cycle order. Index 0 is
/// the default. The runtime registry (see [`register`]) starts with
/// these and grows as apps register their own.
pub const BUILT_IN_THEMES: &[&Theme] = &[
    &LAZYBOX_DARK,
    &CATPPUCCIN_MOCHA,
    &TOKYO_NIGHT,
    &GRUVBOX_DARK,
    &ROSE_PINE,
];

/// Mutable theme registry — built-ins plus anything the host has
/// registered via [`register`]. Behind an `OnceLock<RwLock<...>>` so
/// `current()` stays a cheap read on the steady-state path.
fn registry() -> &'static RwLock<Vec<&'static Theme>> {
    static REGISTRY: OnceLock<RwLock<Vec<&'static Theme>>> = OnceLock::new();
    REGISTRY.get_or_init(|| RwLock::new(BUILT_IN_THEMES.to_vec()))
}

/// Index into the registry that `current()` resolves to. Atomic so
/// theme switching from a key handler is wait-free.
static ACTIVE_THEME_IDX: AtomicUsize = AtomicUsize::new(0);

/// Currently active theme. Cheap: one relaxed load + a read-locked
/// vector index. Apps that hot-loop over `current()` don't need to
/// cache.
pub fn current() -> &'static Theme {
    let reg = registry().read().expect("theme registry poisoned");
    let i = ACTIVE_THEME_IDX.load(Ordering::Relaxed) % reg.len();
    reg[i]
}

/// Register an additional theme. Returns the static reference so the
/// host can keep a handle. The theme leaks — registered themes live
/// for the rest of the process. Treat this as one-time setup.
///
/// ```ignore
/// let mine = crate::theme::LAZYBOX_DARK
///     .derive("My Theme")
///     .accent(ratatui::style::Color::Rgb(255, 100, 100))
///     .build();
/// crate::theme::register(mine);
/// crate::theme::set_by_name("My Theme");
/// ```
pub fn register(theme: Theme) -> &'static Theme {
    let leaked: &'static Theme = Box::leak(Box::new(theme));
    registry()
        .write()
        .expect("theme registry poisoned")
        .push(leaked);
    leaked
}

/// Snapshot of every registered theme, in cycle order. Useful for
/// theme pickers.
pub fn list() -> Vec<&'static Theme> {
    registry().read().expect("theme registry poisoned").clone()
}

/// Cycle to the next theme. Returns the new theme name so callers
/// can flash it in a status bar.
pub fn cycle_next() -> &'static str {
    let reg = registry().read().expect("theme registry poisoned");
    let prev = ACTIVE_THEME_IDX.fetch_add(1, Ordering::Relaxed);
    let next = (prev + 1) % reg.len();
    ACTIVE_THEME_IDX.store(next, Ordering::Relaxed);
    reg[next].name
}

/// Switch to a theme by exact name match. Returns true on hit, false
/// when no theme has that name (caller should report the error).
/// Used by the persisted "remember last theme" path on startup.
pub fn set_by_name(name: &str) -> bool {
    let reg = registry().read().expect("theme registry poisoned");
    if let Some(i) = reg.iter().position(|t| t.name == name) {
        ACTIVE_THEME_IDX.store(i, Ordering::Relaxed);
        true
    } else {
        false
    }
}

/// Builder returned by [`Theme::derive`]. Lets apps copy a built-in
/// palette and override individual slots without specifying every
/// color from scratch.
pub struct ThemeBuilder {
    theme: Theme,
}

impl ThemeBuilder {
    /// Override the accent color.
    pub fn accent(mut self, c: Color) -> Self {
        self.theme.accent = c;
        self
    }
    /// Override the hover/active accent.
    pub fn hover(mut self, c: Color) -> Self {
        self.theme.hover = c;
        self
    }
    /// Override the success color.
    pub fn success(mut self, c: Color) -> Self {
        self.theme.success = c;
        self
    }
    /// Override the warn color.
    pub fn warn(mut self, c: Color) -> Self {
        self.theme.warn = c;
        self
    }
    /// Override the error color.
    pub fn error(mut self, c: Color) -> Self {
        self.theme.error = c;
        self
    }
    /// Override `text_strong`.
    pub fn text_strong(mut self, c: Color) -> Self {
        self.theme.text_strong = c;
        self
    }
    /// Override `text_dim`.
    pub fn text_dim(mut self, c: Color) -> Self {
        self.theme.text_dim = c;
        self
    }
    /// Override the chrome (border / divider) color.
    pub fn chrome(mut self, c: Color) -> Self {
        self.theme.chrome = c;
        self
    }
    /// Override the fill (highlighted-row bg) color.
    pub fn fill(mut self, c: Color) -> Self {
        self.theme.fill = c;
        self
    }
    /// Override the surface (modal/panel bg) color.
    pub fn surface(mut self, c: Color) -> Self {
        self.theme.surface = c;
        self
    }
    /// Finalize the derived theme.
    pub fn build(self) -> Theme {
        self.theme
    }
}

impl Theme {
    /// Start a builder seeded from this theme. Useful for shipping
    /// "almost the default but with a different accent" without
    /// listing every slot. Combine with [`register`] to make the
    /// derived theme cycleable.
    pub fn derive(&self, name: &'static str) -> ThemeBuilder {
        ThemeBuilder {
            theme: Theme {
                name,
                ..self.clone()
            },
        }
    }

    // ── Reusable style recipes ────────────────────────────────────────

    /// Pane title — bold accent when focused, bold dim otherwise.
    pub fn title(&self, focused: bool) -> Style {
        Style::default()
            .fg(if focused { self.accent } else { self.text_dim })
            .add_modifier(Modifier::BOLD)
    }

    /// Thin gray rule under titles, between cards, splitter line.
    pub fn divider(&self) -> Style {
        Style::default().fg(self.chrome)
    }

    /// Body text — default. Use `Style::default()` directly when you
    /// need to compose; this is for "explicitly the body color".
    pub fn body(&self) -> Style {
        Style::default()
    }

    /// Hint / footnote text — dimmed.
    pub fn hint(&self) -> Style {
        Style::default()
            .fg(self.text_dim)
            .add_modifier(Modifier::ITALIC)
    }

    /// Selected row when the pane has focus. Bg-filled, bold strong fg.
    pub fn row_focused(&self) -> Style {
        Style::default()
            .bg(self.fill)
            .fg(self.text_strong)
            .add_modifier(Modifier::BOLD)
    }

    /// Selected row when the pane lacks focus — fg-only, no bg fill.
    pub fn row_unfocused(&self) -> Style {
        Style::default().fg(self.text_strong)
    }

    /// Unread / "new" badge inline.
    pub fn badge_unread(&self) -> Style {
        Style::default().fg(self.warn).add_modifier(Modifier::BOLD)
    }

    /// Modal border — accent-tinted, matches the focus ring on panes
    /// so lazybox has a single identity color across surfaces.
    pub fn modal_border(&self) -> Style {
        Style::default().fg(self.accent)
    }

    /// Modal title style — bold accent-on-default. Sits inside the
    /// modal's bordered Block.
    pub fn modal_title(&self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }

    /// Section heading inside a modal (e.g. "Inbox" / "Activity" in
    /// the help panel, "Bindings" inside the picker).
    pub fn section_heading(&self) -> Style {
        Style::default().fg(self.warn).add_modifier(Modifier::BOLD)
    }

    /// Error pill foreground — used inside error_modal and any other
    /// place that wants the "this is bad" color but on a clear bg.
    pub fn error_text(&self) -> Style {
        Style::default().fg(self.error).add_modifier(Modifier::BOLD)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_overrides_named_slot_only() {
        let red = Color::Rgb(255, 0, 0);
        let derived = LAZYBOX_DARK.derive("test").accent(red).build();
        assert_eq!(derived.name, "test");
        assert_eq!(derived.accent, red);
        assert_eq!(derived.success, LAZYBOX_DARK.success);
        assert_eq!(derived.text_strong, LAZYBOX_DARK.text_strong);
    }

    #[test]
    fn registered_theme_appears_in_list_and_is_settable() {
        let derived = LAZYBOX_DARK.derive("test_registered_unique").build();
        register(derived);
        let names: Vec<_> = list().iter().map(|t| t.name).collect();
        assert!(names.contains(&"test_registered_unique"));
        assert!(set_by_name("test_registered_unique"));
        assert_eq!(current().name, "test_registered_unique");
        // Restore default for other tests.
        set_by_name("Lazybox Dark");
    }
}
