# Demo

The lazybox demo is **code, not a hand-recorded screencast**. The source
of truth is two [VHS](https://github.com/charmbracelet/vhs) scripts that
launch a real `lazybox --test` session, type a scripted sequence of keys,
and render the result. That makes the demo reproducible, reviewable as a
diff, and cheap to regenerate locally without putting multimedia tooling
in CI.

- [`lazybox.tape`](lazybox.tape) — the reproducible local demo. Opens the inbox,
  enters a workspace, spawns a shell, and runs a couple of git commands.
- [`golden.tape`](golden.tape) — a manual visual reference. Opens the `?`
  help overlay only, which is static lazybox chrome (no live shell).
  `golden.ascii` keeps the byte-stable, machine-independent keybinding table
  extracted from that render.

For the parts the sandbox tape can't show — live GitHub events, a real
agent working via `w w`, the Slack-from-your-phone loop — see
[`screencast.md`](screencast.md), a version-controlled shot list for a
human-recorded screencast.

## Outputs

| File           | From          | Purpose                                                              |
| -------------- | ------------- | -------------------------------------------------------------------- |
| `lazybox.gif`    | `lazybox.tape`  | Locally generated animated demo for review or reuse.                 |
| `lazybox.mp4`    | `lazybox.tape`  | Locally generated seekable video for review or reuse.                |
| `lazybox.png`    | `lazybox.tape`  | Optional static frame from a local render.                           |
| `golden.ascii` | `golden.tape` | Manually extracted keybinding-table reference (see below).           |

`golden.ascii` is committed as a manually updated visual reference. It holds
**only** the keybinding table, not VHS's full text capture: that capture is
the terminal scrollback at end-of-tape, whose repaint count and blank
padding vary with render timing and across machines, so diffing it whole is
flaky. The table content is byte-stable, so it changes only when the UI or
a keybinding does. The `lazybox.*` assets are committed too, but are not
automatically regenerated: `lazybox.tape` spawns a shell, whose prompt carries
a random tempdir suffix, the wall clock, and the OS username, so its text
capture changes on every render and can't be a golden.

## Regenerating locally

1. Install VHS (it pulls in `ttyd` and `ffmpeg` as render dependencies):

   ```bash
   brew install vhs
   ```

   On Linux see the [VHS install docs](https://github.com/charmbracelet/vhs#installation).

2. Build lazybox so the tapes' `lazybox --test` resolves, then render both
   tapes from the repo root with the binary on `PATH`:

   ```bash
   cargo build -p lazybox-tui-boot                # or: make build
   PATH="$PWD/target/debug:$PATH" vhs demo/golden.tape
   PATH="$PWD/target/debug:$PATH" vhs demo/lazybox.tape
   # golden.tape writes the full scrollback; reduce it to just the table
   # so the committed golden stays machine-independent:
   awk '/  Tab .* cycle panes/{c=1;b=""} c{b=b $0 ORS} c&&/exit to sidebar/{last=b;c=0} END{printf "%s",last}' \
     demo/golden.ascii > demo/golden.ascii.tmp && mv demo/golden.ascii.tmp demo/golden.ascii
   ```

   A release build works too — just point `PATH` at `target/release`.

   `lazybox --test` spins up a throwaway tempdir repo with one seeded
   workspace and talks to no real GitHub account, so the render is
   hermetic and safe to run anywhere.

3. Review the diff and commit the regenerated artifacts together:

   ```bash
   git add demo/golden.ascii demo/lazybox.gif demo/lazybox.mp4 demo/lazybox.png
   git commit
   ```

## Determinism

Both tapes pin everything that would otherwise make renders differ
machine-to-machine, via VHS `Set` directives at the top of each tape:

- `Set FontSize` — fixed cell size.
- `Set Width` / `Set Height` — fixed terminal geometry so layout and
  line wrapping are stable.
- `Set Theme` — fixed color palette.

Because geometry, font, and theme are pinned, `golden.tape` plus the same
`lazybox` binary renders the same keybinding table every time. (The
scrollback *around* the table — how many full-screen repaints VHS captured
and the blank padding between them — still varies with timing and across
machines, which is why the manual reference keeps only the extracted table,
not the whole capture.)

## Manual golden reference

Demo rendering is intentionally not part of CI. The multimedia/browser stack
is slow and fragile, while the TUI's deterministic ratatui/insta tests already
guard visual structure and the generated keymap tests guard shortcut drift.
Maintainers regenerate these artifacts locally when a release or documentation
change needs fresh media, then review and commit the binary diff explicitly.

## Accessibility

The published hero media and how it's referenced are kept friendly to motion-sensitive
and screen-reader users:

- **Short loop.** The tape is a brief, looping sequence — long enough to
  show the inbox and a workspace, short enough not to demand attention.
- **No strobing.** No rapid flashing or high-contrast flicker; transitions
  are ordinary terminal redraws.
- **Descriptive alt text.** The `README.md` embed uses descriptive alt
  text so the demo is meaningful without playback.
- **Reduced-motion fallback.** For `prefers-reduced-motion`, the README
  pairs the video with `hero.png`, a static poster, so readers who opt out
  of motion still see what lazybox looks like.
