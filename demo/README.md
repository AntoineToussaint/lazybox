# Demo

The pilot demo is **code, not a hand-recorded screencast**. The source
of truth is two [VHS](https://github.com/charmbracelet/vhs) scripts that
launch a real `pilot --test` session, type a scripted sequence of keys,
and render the result. That makes the demo reproducible, reviewable as a
diff, and guarded against drift by CI.

- [`pilot.tape`](pilot.tape) — the pretty README demo. Opens the inbox,
  enters a workspace, spawns a shell, and runs a couple of git commands.
- [`golden.tape`](golden.tape) — the CI snapshot. Opens the `?` help
  overlay only, which is static pilot chrome (no live shell), so its text
  capture is byte-stable and usable as a golden.

## Outputs

| File           | From          | Purpose                                                              |
| -------------- | ------------- | -------------------------------------------------------------------- |
| `pilot.gif`    | `pilot.tape`  | The animated demo embedded inline in the top-level `README.md`.      |
| `pilot.mp4`    | `pilot.tape`  | Seekable video (nicer for docs/social where a player exists).        |
| `pilot.png`    | `pilot.tape`  | Static frame used as the README's `prefers-reduced-motion` fallback. |
| `golden.ascii` | `golden.tape` | Deterministic text snapshot — the CI golden (see below).             |

`golden.ascii` is committed and acts as the golden snapshot. The
`pilot.*` assets are committed too, but they are **not** diffed in CI:
`pilot.tape` spawns a shell, whose prompt carries a random tempdir
suffix, the wall clock, and the OS username, so its text capture changes
on every render and can't be a golden.

## Regenerating locally

1. Install VHS (it pulls in `ttyd` and `ffmpeg` as render dependencies):

   ```bash
   brew install vhs
   ```

   On Linux see the [VHS install docs](https://github.com/charmbracelet/vhs#installation).

2. Build pilot so the tapes' `pilot --test` resolves, then render both
   tapes from the repo root with the binary on `PATH`:

   ```bash
   cargo build -p pilot-tui                     # or: make build
   PATH="$PWD/target/debug:$PATH" vhs demo/golden.tape
   PATH="$PWD/target/debug:$PATH" vhs demo/pilot.tape
   ```

   A release build works too — just point `PATH` at `target/release`.

   `pilot --test` spins up a throwaway tempdir repo with one seeded
   workspace and talks to no real GitHub account, so the render is
   hermetic and safe to run anywhere.

3. Review the diff and commit the regenerated artifacts together:

   ```bash
   git add demo/golden.ascii demo/pilot.gif demo/pilot.mp4 demo/pilot.png
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
`pilot` binary renders the same `golden.ascii` every time — which is what
makes the golden snapshot meaningful.

## Golden snapshot test

CI re-renders `golden.tape` on every PR that touches `demo/**` or
`crates/**`, then runs `git diff --exit-code demo/golden.ascii`. If the
TUI or a keybinding changed but the recorded demo wasn't regenerated, the
freshly rendered `golden.ascii` diverges from the committed one and CI
fails. The fix is exactly the [regenerate-and-commit](#regenerating-locally)
flow above. The pretty GIF is re-rendered in the same job as a smoke test
(it must still boot `pilot --test`), but it is not diffed.

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
  pairs the GIF with `pilot.png`, a static frame, so readers who opt out
  of motion still see what pilot looks like.
