//! Centralized color palette + style helpers.
//!
//! ## Why a runtime-switchable theme
//!
//! Lazybox ships several built-in palettes — five dark (Lazybox Dark,
//! Catppuccin Mocha, Tokyo Night, Gruvbox Dark, Rose Pine), one light
//! (Lazybox Light), and one accessible High Contrast variant. The
//! active palette lives behind `theme::current()`; every component
//! reads from it instead of hard-coding `Color::*` literals. Switching
//! themes at runtime is then a single atomic store — no rebuild, no
//! reload, every render after the switch picks up the new palette.
//!
//! ## Choosing a theme
//!
//! Users open the theme picker with `t` (or via the `,` Settings
//! palette), arrow through the list with a live preview, and Enter to
//! keep one. The choice is written to `ui.theme` in
//! `~/.lazybox/config.yaml` and re-applied with [`set_by_name`] at the
//! next launch. See `docs/themes.md` for shipping a custom palette.
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
//! The raw palette colors are the single source of truth in
//! `lazybox_tui_core::theme` (client-free so the desktop can read them
//! too). To add a built-in:
//!
//! 1. Add a `pub const ThemePalette` there and append it to
//!    `BUILT_IN_PALETTES`.
//! 2. Nothing else — the ratatui `Theme` values and `BUILT_IN_THEMES`
//!    below are derived from that slice at compile time.
//!
//! `T` cycles include the new entry, and any component reading from
//! `theme::current()` updates automatically.
//!
//! ## Color tokens
//!
//! Themes use `Color::Rgb(r, g, b)` literals so the result is
//! consistent across terminal palettes. Modern terminals (iTerm2,
//! Ghostty, WezTerm, Kitty, Alacritty, modern macOS Terminal) all
//! render truecolor; for the rare 8-color holdouts we still get
//! reasonable approximations.

use lazybox_tui_core::theme::{self as palette, ThemePalette};
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

const fn rgb(c: palette::Rgb) -> Color {
    Color::Rgb(c.0, c.1, c.2)
}

/// Build a render-time [`Theme`] from a client-free
/// [`ThemePalette`](palette::ThemePalette). The palette is the single
/// source of truth for the colors; this just maps each semantic slot to
/// a `ratatui::style::Color`.
const fn from_palette(p: &ThemePalette) -> Theme {
    Theme {
        name: p.name,
        accent: rgb(p.accent),
        hover: rgb(p.hover),
        success: rgb(p.success),
        warn: rgb(p.warn),
        error: rgb(p.error),
        text_strong: rgb(p.text_strong),
        text_dim: rgb(p.text_dim),
        chrome: rgb(p.chrome),
        fill: rgb(p.fill),
        surface: rgb(p.surface),
    }
}

/// Lazybox Dark — the default. Restrained palette in the spirit of
/// yazi's defaults: one cyan accent, one magenta hover, otherwise grays.
pub const LAZYBOX_DARK: Theme = from_palette(&palette::LAZYBOX_DARK);

/// Catppuccin Mocha — popular dark, soft pastels.
pub const CATPPUCCIN_MOCHA: Theme = from_palette(&palette::CATPPUCCIN_MOCHA);

/// Tokyo Night — slightly cooler, navy-leaning dark theme.
pub const TOKYO_NIGHT: Theme = from_palette(&palette::TOKYO_NIGHT);

/// Gruvbox Dark — earthy retro palette.
pub const GRUVBOX_DARK: Theme = from_palette(&palette::GRUVBOX_DARK);

/// Rose Pine — soothing low-saturation pastels on a deep purple backdrop.
pub const ROSE_PINE: Theme = from_palette(&palette::ROSE_PINE);

/// Lazybox Light — the one bright palette. Dark ink on a near-white
/// surface, with the accent / status hues darkened until they hold
/// contrast against the light background.
pub const LAZYBOX_LIGHT: Theme = from_palette(&palette::LAZYBOX_LIGHT);

/// High Contrast — accessibility-first. Pure-white text and saturated,
/// bright status hues on a pure-black surface.
pub const HIGH_CONTRAST: Theme = from_palette(&palette::HIGH_CONTRAST);

/// Bloomberg Terminal — the hallmark amber-on-black financial display.
pub const BLOOMBERG_TERMINAL: Theme = from_palette(&palette::BLOOMBERG_TERMINAL);

/// Built-in themes shipped with the kit, in cycle order. Index 0 is
/// the default. The runtime registry (see [`register`]) starts with
/// these and grows as apps register their own. Derived from
/// [`palette::BUILT_IN_PALETTES`], the shared source of truth.
pub const BUILT_IN_THEMES: &[&Theme] = &[
    &LAZYBOX_DARK,
    &CATPPUCCIN_MOCHA,
    &TOKYO_NIGHT,
    &GRUVBOX_DARK,
    &ROSE_PINE,
    &LAZYBOX_LIGHT,
    &HIGH_CONTRAST,
    &BLOOMBERG_TERMINAL,
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

    /// Rule under a pane's title row — the pane's "border" in this
    /// box-less layout. Accent when the pane has focus so the active
    /// pane is identifiable at a glance (#286), chrome otherwise.
    /// Pairs with [`Self::title`], which colors the title text the
    /// same way.
    pub fn pane_divider(&self, focused: bool) -> Style {
        Style::default().fg(if focused { self.accent } else { self.chrome })
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
    fn pane_divider_is_accent_only_when_focused() {
        let t = &LAZYBOX_DARK;
        assert_eq!(t.pane_divider(true).fg, Some(t.accent));
        assert_eq!(t.pane_divider(false).fg, Some(t.chrome));
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

    /// Approximate relative luminance (0 = black, 1 = white) of an RGB
    /// color, for the light-vs-dark assertions below. Non-truecolor
    /// `Color` variants never appear in a built-in theme.
    fn luminance(c: Color) -> f32 {
        let Color::Rgb(r, g, b) = c else {
            panic!("built-in themes use Color::Rgb literals only");
        };
        (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.0
    }

    #[test]
    fn ships_at_least_one_light_and_one_dark_theme() {
        // A "light" theme has a bright surface; a "dark" theme a deep
        // one. The picker is pointless without both poles available.
        let lightest = BUILT_IN_THEMES
            .iter()
            .map(|t| luminance(t.surface))
            .fold(0.0_f32, f32::max);
        let darkest = BUILT_IN_THEMES
            .iter()
            .map(|t| luminance(t.surface))
            .fold(1.0_f32, f32::min);
        assert!(
            lightest > 0.8,
            "no light theme (brightest surface {lightest})"
        );
        assert!(darkest < 0.1, "no dark theme (darkest surface {darkest})");
    }

    #[test]
    fn built_in_themes_track_the_shared_palette_catalog() {
        // The desktop reads `palette::BUILT_IN_PALETTES`; the ratatui UI
        // reads these. They must stay the same list, in the same order,
        // with the same colors, or the two clients disagree on themes.
        assert_eq!(BUILT_IN_THEMES.len(), palette::BUILT_IN_PALETTES.len());
        for (theme, pal) in BUILT_IN_THEMES.iter().zip(palette::BUILT_IN_PALETTES) {
            assert_eq!(theme.name, pal.name);
            assert_eq!(theme.accent, rgb(pal.accent));
            assert_eq!(theme.surface, rgb(pal.surface));
            assert_eq!(theme.text_strong, rgb(pal.text_strong));
        }
    }

    #[test]
    fn built_in_theme_names_are_unique() {
        let mut names: Vec<_> = BUILT_IN_THEMES.iter().map(|t| t.name).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "duplicate built-in theme name");
    }

    /// Render a compact swatch — one row per semantic slot, drawn in
    /// that slot's color — and capture the glyphs plus each row's
    /// foreground color. Snapshotting this per theme freezes every
    /// built-in palette: an accidental edit to any slot fails review.
    fn render_swatch(theme: &Theme) -> String {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::text::{Line, Span};
        use ratatui::widgets::Paragraph;

        let slots: [(&str, Color); 10] = [
            ("accent", theme.accent),
            ("hover", theme.hover),
            ("success", theme.success),
            ("warn", theme.warn),
            ("error", theme.error),
            ("text_strong", theme.text_strong),
            ("text_dim", theme.text_dim),
            ("chrome", theme.chrome),
            ("fill", theme.fill),
            ("surface", theme.surface),
        ];
        let (w, h) = (24u16, slots.len() as u16);
        let lines: Vec<Line> = slots
            .iter()
            .map(|(name, color)| {
                Line::from(Span::styled(
                    format!("{name:<12} ████"),
                    Style::default().fg(*color),
                ))
            })
            .collect();
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| f.render_widget(Paragraph::new(lines), f.area()))
            .unwrap();
        let buf = term.backend().buffer().clone();
        (0..h)
            .map(|y| {
                let text: String = (0..w).map(|x| buf[(x, y)].symbol()).collect();
                format!("{} fg={:?}", text.trim_end(), buf[(0, y)].fg)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn built_in_theme_swatches_snapshot() {
        for theme in BUILT_IN_THEMES {
            insta::assert_snapshot!(
                format!("swatch_{}", theme.name.replace(' ', "_")),
                render_swatch(theme)
            );
        }
    }
}
