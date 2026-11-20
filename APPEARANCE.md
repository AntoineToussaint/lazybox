# Appearance

How Lazybox V2 looks, and how a person changes it: light and dark following
the system, built-in and user themes, an accent colour, colour overrides,
fonts, text sizes, chat text, density, zoom, minimum contrast and reduced
motion. Appearance settings apply across windows and are kept.

## For people

**Settings → Appearance** holds the look; **Settings → Chat** holds how conversations read (chat text, beside the activity detail). Every change is live on screen.

| Setting | What it does | Kept where |
|---|---|---|
| Appearance | *System* follows the computer's light or dark mode; *Light* and *Dark* fix it. | settings.yaml |
| Dark / light theme | The theme used for each mode: 20 built-in palettes across both modes, and your own (below). | settings.yaml |
| Accent colour | Buttons, selection, focus, links. The whole accent family (tints, borders, solid fills) follows it. | settings.yaml |
| Minimum contrast | Off, 3:1, 4.5:1 (default, WCAG AA) or 7:1. Text too faint against its background is moved just far enough in lightness to reach it, keeping its hue. Surfaces and fills never change. | settings.yaml |
| Colour overrides | Any token, on top of every theme. | This device |
| Interface font and text size | Any installed font; 11–16 px. Every UI text size scales with it. | settings.yaml |
| Code font, size, ligatures | The editor, code blocks, diffs, command output and the terminal. Empty: the system monospace, and for command output the bundled JetBrains Mono. | settings.yaml |
| Zoom | Desktop app, 50–200%. **View → Zoom In / Zoom Out / Actual Size** (`⌘=` `⌘−` `⌘0`) adjusts the focused window until it closes. Settings sets the app-wide default and saves it across launches. In a browser, use the browser's zoom. | settings.yaml (`zoom`), and the desktop app data folder |
| Chat text (Settings → Chat) | Size, line spacing (Compact 1.35, Comfortable 1.5, Relaxed 1.7 or any value), paragraph spacing, and a line width cap (70–120 characters, centred) for wide windows. A sample message shows the result. | settings.yaml |
| Density | Compact, comfortable or relaxed rows in lists and the sidebar; also sets chat line and paragraph spacing. | settings.yaml |
| Motion | *System* follows the computer's reduced-motion setting; *Reduce* turns off animations and smooth scrolling. | settings.yaml |

Everything but colour overrides is kept in **`settings.yaml`**, beside your
other Lazybox files, under `appearance:` ([POWER.md](POWER.md#config-files),
[every field](docs/reference/configuration.md#settingsyaml-settings-you-may-keep-in-a-file)).
Settings → Appearance shows the file's path and its problems by line; a save
in an editor applies within two seconds in every window, a value with a
mistake is left out (the rest applies), and a value you remove goes back to
its default. Settings writes one value at a time and keeps your comments; a
slider dragged is one write, of where it stopped.

Each window also keeps the last preferences applied on the device
(`ViewStateStore`, `appearance:v1`) and applies them before it draws, so it
never flashes the wrong theme; the file then applies over them. A change made
just before a window closed, which had not reached the file yet, wins over the
file at the next start and is written to it (`appearance:unsynced:v1`). What
you set before the file existed moves into it once.

Settings → Appearance saves the default desktop zoom in `settings.yaml`.
Changing that setting applies the new default to all open windows. View menu
shortcuts hold a separate zoom for the focused window while it is open.
In a browser, use the browser's own zoom.

### Information detail

**Focused** keeps the sidebar quieter and summarizes each agent turn.
**Standard** shows repository and Git details with grouped tool activity.
**Power user** shows each tool call and keeps reply times visible. Choose a
preset in Settings → Appearance, then adjust sidebar details there, or tool
activity and timestamps in Settings → Chat. Per-conversation activity
overrides still apply.

### Your own themes

Use **New theme from current** to start an editable copy. **Delete** beside a
theme file removes it from the Runner. If it was selected, Lazybox switches
that mode to its base theme or the default when no base is configured.

A theme is a `.toml` or `.json` file in the Runner's themes folder,
`<Runner state>/themes` (set `themeDirectory` in the Runner configuration to
move it). Settings shows the folder, with **Copy path** and **New theme from
current**, which writes a starting file of the current colours. Save the file
in any editor: the theme reloads within seconds, in every window. A file with
a mistake keeps its last good version on screen, and Settings says what is
wrong, line by line (`tokens.acent: there is no such token (did you mean
"accent"?)`).

```toml
name = "Midnight"
appearance = "dark"        # or "light"
extends = "dark"           # optional: a built-in theme to start from

[tokens]                   # set a few base colours; the rest derive from them
bg = "#05060a"
surface = "#0b0d14"
"surface-raised" = "#121522"
line = "#252a3a"
text = "#e8ecf5"
"text-muted" = "#8b93a7"
accent = "#5eb1ff"

[typography]               # optional suggestions; the person's own settings win
density = "compact"
chatLineHeight = 1.4
```

Colours are `#rgb`, `#rrggbb`, `#rrggbbaa`, `rgb()` or `hsl()`; nothing else
is accepted, so a theme cannot inject CSS. **Token reference** in Settings
lists every token, its meaning and current value; the base tokens are marked.

## For developers

**Colours and text sizes are tokens, never literals.** Every stylesheet in
`packages/surfaces/src` uses `var(--surface)`, `var(--text-muted)`,
`var(--danger-bg)`… and `font-size: calc(12px * var(--text-scale))` (code:
`--code-scale`). `scripts/appearance.test.ts` fails on a literal colour or
pixel font size. To convert a stylesheet, run
`npm run tokenize-css -- path/to/file.css`: it maps each literal to the
nearest Dark token for how it is used (text, line, surface, tinted fill, the
`on-*` text of a fill, diff, focus) and prints nothing else; review the
result. Print styles (`@media print`) keep their literals.

The artifact reader (`artifact-reader.css`) reads in its own bundled faces,
Inter for text and Newsreader for titles (`@fontsource-variable/*`, so the
desktop app needs no network), as `--font-reading` and `--font-display`, with
the interface font as fallback. Its colours are tokens like everything else,
and its sizes follow `--text-scale`.

| Where | What |
|---|---|
| `appearance/tokens.ts` | The token catalogue: groups, meaning, base or derived, how each derives (OKLab mixes), and its contrast class. |
| `appearance/themes.ts` | Built-in themes (Dark and Light write out every token; the others only base ones), resolution, minimum contrast. |
| `appearance/preferences.ts` | The stored preferences and what they amount to. |
| `appearance/user-themes.ts` | Validation of a theme file the Runner read, and the starting template. |
| `appearance/apply.ts` | Applies one `<style>` with every token, `data-scheme`, `data-density`, `data-motion` on `<html>`; a store Monaco, xterm and Mermaid follow. |
| `appearance/integrations.ts` | Monaco, terminal and Mermaid themes from the tokens. Mermaid gets hex only (its parser refuses oklch(), color-mix(), var()); a colour it cannot read falls back to the built-in theme's value for that variable. |
| `AppearanceSettings.tsx` | Settings → Appearance, and the Chat text group shown in Settings → Chat (`ChatTextSettings`). |

The ANSI renderer uses `--ansi-0` … `--ansi-15` for the 16 named colours
(256-colour and truecolour stay the program's own); the console's source column uses
`--log-source-1` … `--log-source-6` (the information colour at six hues away from red and yellow), and
its text `--font-console` (the code font, else the bundled JetBrains Mono). Charts use `--chart-1` …
`--chart-7`. The Runner side is `ListThemes` and `CreateTheme`
([interfaces](INTERFACES.md)), package `internal/themes`. The desktop host
owns zoom (`appearance.rs`) and tells every window when a preference changes
(`share_preference`).

Tests: `npm run test:client` (theme parsing and validation, derivation,
contrast of every built-in theme, the token sweep, the stylesheet guard) and
`npm run test:appearance` (key pages in Dark and Light use only tokens with
body text at 4.5:1 or better; every setting applies live and persists; user
themes load, hot reload and report errors; the appearance section of
settings.yaml written from Settings, applied from an editor save, its mistakes
by line, a change just before a reload kept), against the fake Runner. The
Runner's parser of the section is `internal/power` (`TestSettingsAppearance*`).
