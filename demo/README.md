# Demo

The lazybox demo is **code, not a hand-recorded screencast**. The source
of truth is two [VHS](https://github.com/charmbracelet/vhs) scripts that
launch a real `lazybox --test` session, type a scripted sequence of keys,
and render the result. That makes the demo reproducible, reviewable as a
diff, and guarded against drift by CI.

- [`lazybox.tape`](lazybox.tape) — the pretty README demo. Opens the inbox,
  enters a workspace, spawns a shell, and runs a couple of git commands.
- [`golden.tape`](golden.tape) — the CI snapshot. Opens the `?` help
  overlay only, which is static lazybox chrome (no live shell). CI diffs
  just the keybinding table extracted from that render — the byte-stable,
  machine-independent part — against the committed `golden.ascii`.

For the parts the sandbox tape can't show — live GitHub events, a real
agent working via `w`, the Slack-from-your-phone loop — see
[`screencast.md`](screencast.md), a version-controlled shot list for a
human-recorded screencast.

## Outputs

| File           | From          | Purpose                                                              |
| -------------- | ------------- | -------------------------------------------------------------------- |
| `lazybox.gif`    | `lazybox.tape`  | The animated demo embedded inline in the top-level `README.md`.      |
| `lazybox.mp4`    | `lazybox.tape`  | Seekable video (nicer for docs/social where a player exists).        |
| `lazybox.png`    | `lazybox.tape`  | Static frame used as the README's `prefers-reduced-motion` fallback. |
| `golden.ascii` | `golden.tape` | Keybinding table extracted from the render — the CI golden (see below). |

`golden.ascii` is committed and acts as the golden snapshot. It holds
**only** the keybinding table, not VHS's full text capture: that capture is
the terminal scrollback at end-of-tape, whose repaint count and blank
padding vary with render timing and across machines, so diffing it whole is
flaky. The table content is byte-stable, so it changes only when the UI or
a keybinding does. The `lazybox.*` assets are committed too, but they are
**not** diffed in CI: `lazybox.tape` spawns a shell, whose prompt carries a
random tempdir suffix, the wall clock, and the OS username, so its text
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
   cargo build -p lazybox-tui                     # or: make build
   PATH="$PWD/target/debug:$PATH" vhs demo/golden.tape
   PATH="$PWD/target/debug:$PATH" vhs demo/lazybox.tape
   # golden.tape writes the full scrollback; reduce it to just the table
   # (the same awk CI uses) so the committed golden stays machine-independent:
   awk '/  Tab .* cycle panes/{c=1;b=""} c{b=b $0 ORS} c&&/snippets/{last=b;c=0} END{printf "%s",last}' \
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
machines, which is exactly why CI compares only the extracted table, not
the whole capture.)

## Golden snapshot test

CI re-renders `golden.tape` on every PR that touches `demo/**` or
`crates/**`, extracts the keybinding table from the render, and diffs it
against the committed `demo/golden.ascii`. If the TUI or a keybinding
changed but the recorded demo wasn't regenerated, the table diverges from
the committed one and CI fails. The fix is exactly the
[regenerate-and-commit](#regenerating-locally) flow above. The pretty GIF
is re-rendered in the same job as a smoke test (it must still boot
`lazybox --test`), but it is not diffed.

CI does not auto-commit regenerated assets — maintainers regenerate and
commit locally so the binary GIF stays reviewable in the PR.

## Accessibility

The GIF and how it's referenced are kept friendly to motion-sensitive
and screen-reader users:

- **Short loop.** The tape is a brief, looping sequence — long enough to
  show the inbox and a workspace, short enough not to demand attention.
- **No strobing.** No rapid flashing or high-contrast flicker; transitions
  are ordinary terminal redraws.
- **Descriptive alt text.** The `README.md` embed uses descriptive alt
  text so the demo is meaningful without playback.
- **Reduced-motion fallback.** For `prefers-reduced-motion`, the README
  pairs the GIF with `lazybox.png`, a static frame, so readers who opt out
  of motion still see what lazybox looks like.
