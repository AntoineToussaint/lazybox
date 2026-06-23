# Themes

lazybox renders every pane through a single active palette
(`crates/tui/src/theme.rs`). Switching is instant — one atomic store,
no rebuild — because components read `theme::current()` on each frame
rather than hard-coding colors.

## Picking a theme

- Press `t` (remappable: `ui.action_keys.open_theme_picker`) or open
  the `,` Settings palette and choose **Change theme**.
- Arrow through the list — the whole UI recolors live as you move.
- `Enter` keeps the highlighted theme; `Esc` restores the one that was
  active when you opened the picker.

Your choice is written to `~/.lazybox/config.yaml`:

```yaml
ui:
  theme: "Lazybox Light"
```

and re-applied at the next launch. An unknown name (a theme you removed
or renamed) silently falls back to the default palette.

## Built-in themes

| Name | Kind |
| --- | --- |
| Lazybox Dark | dark (default) |
| Catppuccin Mocha | dark |
| Tokyo Night | dark |
| Gruvbox Dark | dark |
| Rose Pine | dark |
| Lazybox Light | light |
| High Contrast | accessible |

## Slots

A theme assigns a color to each *semantic* slot — `accent`, not
`cyan` — so a component asks for "the accent color" and every theme
answers in its own hue:

`accent` · `hover` · `success` · `warn` · `error` · `text_strong` ·
`text_dim` · `chrome` · `fill` · `surface`

See the doc comments on `Theme` for what each drives.

## Adding a built-in theme

1. Add a `pub const` `Theme` literal in `crates/tui/src/theme.rs`.
2. Append a reference to it in `BUILT_IN_THEMES`.

It now appears in the picker and the snapshot test
(`built_in_theme_swatches_snapshot`) will flag the new palette for
review (`cargo insta review`).

```rust
pub const MY_THEME: Theme = Theme {
    name: "My Theme",
    accent: Color::Rgb(255, 121, 198),
    // …every slot…
};

pub const BUILT_IN_THEMES: &[&Theme] = &[/* … */, &MY_THEME];
```

## Registering a theme at runtime

A host embedding the TUI can derive from a built-in and register it
without listing every slot. Registered themes join the picker and are
selectable by name:

```rust
let mine = crate::theme::LAZYBOX_DARK
    .derive("My Theme")
    .accent(ratatui::style::Color::Rgb(255, 121, 198))
    .build();
crate::theme::register(mine);
crate::theme::set_by_name("My Theme");
```
