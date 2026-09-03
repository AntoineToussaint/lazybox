# Demo

The lazybox demo is **code, not a hand-recorded screencast**. The source of
truth is a set of [VHS](https://github.com/charmbracelet/vhs) scripts that
launch a real lazybox session, type a scripted sequence of keys, and render the
result. That makes the demo reproducible, reviewable as a diff, and cheap to
regenerate locally without putting multimedia tooling in CI.

The homepage clips are driven by `lazybox --demo` — a **scenario-injection
harness** (`crates/tui-boot/src/scenario.rs`, #1333) that boots a deterministic,
throwaway multi-repo/multi-owner inbox (no GitHub, no PTY, no worktrees) and
replays the built-in **"fleet" scenario**: several agents working across repos,
one asking, one done, one rate-limited, live terminals streaming canned output,
and a reactive CI red→green flip. It renders identically every run. Each scene
tape preselects the workspace it needs (`--workspace <key>`) and drives real
UI — leader menus, the snippet picker, `!`-jump-to-asking — against the harness.

- [`lazybox.tape`](lazybox.tape) — the hero. `--demo` preselected on the Claude
  agent working `obin-ai/lazybox#1332`, run to the moment a PR's CI flips green.
  Outputs `demo/lazybox.{gif,mp4,png}` (README GIF + the homepage `01-inbox`).
- [`02-snippets.tape`](02-snippets.tape) — the `]]s` snippet picker inside an
  agent, with its live body preview.
- [`03-policies.tape`](03-policies.tape) — the `g` GitHub leader menu and the
  `g p` automation-policies surface.
- [`04-spawn.tape`](04-spawn.tape) — a live Codex agent working a task in its own
  worktree branch.
- [`05-autowork.tape`](05-autowork.tape) — the reactive inbox and `!`-jump to the
  agent that needs a decision.
- [`golden.tape`](golden.tape) — a manual visual reference. Boots `lazybox
  --test` and opens the `?` help overlay only, which is static lazybox chrome
  (no live shell). `golden.ascii` keeps the byte-stable, machine-independent
  keybinding table extracted from that render. (Kept on `--test` on purpose: it
  snapshots static chrome, not the fleet.)

For the parts a recorded cast can't show — real GitHub events, a real agent
working via `w w`, the Slack-from-your-phone loop — see
[`screencast.md`](screencast.md), a version-controlled shot list for a
human-recorded screencast.

## Outputs

| File                          | From              | Purpose                                                       |
| ----------------------------- | ----------------- | ------------------------------------------------------------- |
| `lazybox.gif`                 | `lazybox.tape`    | Animated hero for the README embed.                           |
| `lazybox.mp4`                 | `lazybox.tape`    | Seekable hero video; copied to the site as `01-inbox.mp4`.    |
| `lazybox.png`                 | `lazybox.tape`    | Static hero frame; the site poster + docs screenshot.         |
| `../web/public/demo/01-inbox.*` | `lazybox.tape`  | Homepage hero: `.mp4` (copy) + `.webp` poster + `.vtt`.       |
| `../web/public/demo/02-snippets.*` | `02-snippets.tape` | Homepage "Snippets" clip: `.mp4` + `.jpg` poster + `.vtt`. |
| `../web/public/demo/03-policies.*` | `03-policies.tape` | Homepage "GitHub controls" clip.                        |
| `../web/public/demo/04-spawn.*`    | `04-spawn.tape`    | Homepage "worktree + agent" clip.                       |
| `../web/public/demo/05-autowork.*` | `05-autowork.tape` | Homepage "github auto-work" clip.                       |
| `golden.ascii`                | `golden.tape`     | Manually extracted keybinding-table reference (see below).    |

The `.vtt` caption tracks and posters are committed alongside each clip. Caption
cues must end **inside** the clip's runtime — `web/scripts/homepage-install.test.mjs`
reads each MP4's true video-track length and fails a cue that outlives it. The
`aria-label`s and figcaptions in `web/src/pages/index.astro` are copy that must
describe what each clip actually shows; update them when the footage changes.

## Regenerating locally

1. Install VHS (it pulls in `ttyd` and `ffmpeg` as render dependencies), plus
   ImageMagick for poster conversion:

   ```bash
   brew install vhs imagemagick
   ```

   On Linux see the [VHS install docs](https://github.com/charmbracelet/vhs#installation).

2. Build lazybox so the tapes' `lazybox` resolves, then render from the repo
   root with the binary on `PATH`:

   ```bash
   cargo build -p lazybox-tui-boot                # or: make build
   export PATH="$PWD/target/debug:$PATH"          # a release build works too
   vhs demo/lazybox.tape                          # hero → demo/lazybox.{gif,mp4,png}
   vhs demo/02-snippets.tape                      # → web/public/demo/02-snippets.mp4 (+ .png poster)
   vhs demo/03-policies.tape
   vhs demo/04-spawn.tape
   vhs demo/05-autowork.tape
   ```

   `--demo` spins up a throwaway tempdir repo and talks to no real GitHub
   account, so every render is hermetic and safe to run anywhere. The tapes pin
   **FontSize 32 @ 2560×1440** with Padding 30 and the Catppuccin Mocha theme so
   the output is crisp on retina and downscales cleanly — the old 1200×720
   renders looked low-res.

3. Build the web hero from the freshly rendered hero, and turn each scene's
   screenshot into a compressed poster. Posters count toward the site's
   Lighthouse total-byte-weight budget (≤ 500 KiB, enforced by
   `web/scripts/lighthouse.mjs`), so the hero poster is a full-width WebP and the
   four grid posters are downscaled JPEGs:

   ```bash
   cd "$(git rev-parse --show-toplevel)"
   # Hero: the site reuses the hero clip + a WebP poster from its frame.
   cp demo/lazybox.mp4 web/public/demo/01-inbox.mp4
   cp demo/lazybox.mp4 web/public/demo/lazybox.mp4
   cp demo/lazybox.gif web/public/demo/lazybox.gif
   cp demo/lazybox.png web/public/demo/lazybox.png
   magick demo/lazybox.png -resize 1920x -strip -quality 62 web/public/demo/01-inbox.webp
   # Scene posters: downscale each tape's screenshot and drop the transient PNG.
   for s in 02-snippets 03-policies 04-spawn 05-autowork; do
     magick "web/public/demo/$s.png" -resize 900x -strip -quality 66 "web/public/demo/$s.jpg"
     rm -f "web/public/demo/$s.png"
   done
   ```

4. Refresh the `.vtt` captions if the footage changed, keeping every cue's end
   inside the clip runtime, then verify:

   ```bash
   cd web && npm ci && npm run build
   node --test scripts/demo-captions.test.mjs scripts/homepage-install.test.mjs
   npm run lighthouse          # optional: confirms the poster byte budget
   ```

5. Regenerate the keybinding golden whenever the UI changes, and review the diff:

   ```bash
   vhs demo/golden.tape
   awk '/  Tab .* cycle panes/{c=1;b=""} c{b=b $0 ORS} c&&/exit to sidebar/{last=b;c=0} END{printf "%s",last}' \
     demo/golden.ascii > demo/golden.ascii.tmp && mv demo/golden.ascii.tmp demo/golden.ascii
   git add demo/*.gif demo/*.mp4 demo/*.png demo/golden.ascii web/public/demo
   git commit
   ```

## Determinism

Every tape pins what would otherwise make renders differ machine-to-machine, via
VHS `Set` directives at the top of each tape: `Set FontSize` (fixed cell size),
`Set Width` / `Set Height` (fixed geometry, so layout and wrapping are stable),
and `Set Theme` (fixed palette). The `--demo` fleet scenario is scripted on a
fixed timeline, so with geometry, font, and theme pinned it renders the same
content every run.

The scene tapes synchronize on painted content with `Wait+Screen@<timeout>
/regex/` rather than fixed `Sleep`s wherever a beat's timing depends on the
scenario (e.g. `Wait+Screen@30s /Finished in/` blocks until the hero's agent has
finished, so the CI-flip that follows is always captured). A fixed `Sleep` only
moves *when* a frame is grabbed and can't guarantee it lands after the event —
the historical flake. Keep interactions to what the harness can service: its
bus-injected terminals accept input and survive recovery (Tier-2 MockBackend),
but there is no real agent behind them, so don't script a response the harness
can't produce.

`golden.tape` never spawns a shell, so its keybinding table is byte-stable and
diffable. The scrollback *around* the table still varies with render timing and
across machines, which is why the committed reference keeps only the extracted
table, not VHS's full text capture.

## Manual golden reference

Demo rendering is intentionally not part of CI. The multimedia/browser stack is
slow and fragile, while the TUI's deterministic ratatui/insta tests already guard
visual structure and the generated keymap tests guard shortcut drift. Maintainers
regenerate these artifacts locally when a release or documentation change needs
fresh media, then review and commit the binary diff explicitly.

## Accessibility

The published media and how it's referenced are kept friendly to motion-sensitive
and screen-reader users:

- **Short loops.** Each clip is a brief, looping sequence — long enough to show
  the workflow, short enough not to demand attention.
- **No strobing.** No rapid flashing or high-contrast flicker; transitions are
  ordinary terminal redraws.
- **Descriptive alt text.** The README embed and each homepage `<video>` carry
  descriptive `aria-label`/alt text so the demo is meaningful without playback.
- **Reduced-motion fallback.** The homepage clips do not autoplay (poster +
  click-to-play, `preload="none"`), and the README pairs its video with
  `hero.png`, a static poster, so readers who opt out of motion still see what
  lazybox looks like.
