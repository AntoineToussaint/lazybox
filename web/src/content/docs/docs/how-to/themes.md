---
title: Change the theme
description: Live-preview and pick a color palette; persist your choice to config.
---

lazybox renders every pane through a single active palette, and switching is
instant — the whole UI recolors as you move through the list, with no rebuild.

## Pick a theme

Press `t` (or open the `,` settings palette and choose **Change theme**). Arrow
through the list and the entire UI recolors live as you move. `Enter` keeps the
highlighted theme; `Esc` restores the one that was active when you opened the
picker.

Your choice is written to `~/.lazybox/config.yaml` and re-applied on the next
launch:

```yaml
ui:
  theme: "Lazybox Light"
```

An unknown name (a theme you removed or renamed) silently falls back to the
default palette.

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
| Bloomberg Terminal | dark |

## See also

Adding or registering your own palette is covered in the
[themes reference](https://github.com/AntoineToussaint/lazybox/blob/main/docs/themes.md),
including the semantic color slots (`accent`, `success`, `error`, …) a theme
fills in. The `t` chord is remappable via
[`ui.action_keys`](/docs/reference/configuration/).
