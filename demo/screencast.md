# Screencast

The [VHS tape](lazybox.tape) renders the README GIF deterministically, but
by design it runs `lazybox --test` — a credential-free sandbox. It can show
the *shape* of the app (inbox → workspace → shell → git) but not the parts
that need a live account: GitHub events landing in real time, a real agent
working via `w w`, and the Slack "control it from your phone" loop.

This is the shot list for a **human-recorded screencast** that captures
those moments. It lives next to the tape so the recording is reproducible:
same setup, same beats, same length, every take.

Target: **90–120s**, terminal-only, stitched from short takes.

## Setup

- **Curated state, not the sandbox.** Record against a real-but-disposable
  profile so events and agents are genuine:

  ```bash
  make dev          # runs against $LAZYBOX_HOME (defaults to ~/.lazybox-dev)
  ```

  Seed it ahead of time with a couple of workspaces, including one PR whose
  CI is failing (that PR is the star of shots 2–3). Keep the list short so
  the inbox reads at a glance on camera.
- **Legible type.** ~18pt terminal font, high-contrast theme.
- **No log spam.** lazybox logs to `/tmp/lazybox.log`; keep that out of frame
  (don't `tail` it in a visible pane).
- **A second window** you can switch to for pushing a live comment in
  shot 1 (`gh pr comment …` or the GitHub UI).
- **Phone on screen mirror** for shot 4, framed beside the terminal.
- Do a dry run of the Slack reply path on a throwaway channel *before*
  recording — see [#186](https://github.com/AntoineToussaint/lazybox/issues/186).

## Shot list

1. **Hook — the reactive inbox (0–15s).**
   Open `lazybox` on the curated inbox. From the second window, push a
   comment to one of the PRs; on camera, its row lights up and reorders
   live. No refresh, no polling by hand — the event arrives on its own.

2. **Triage → workspace (15–35s).**
   `j`/`k` down to the failing-CI PR. `Enter` to open it. The worktree is
   created and the embedded terminal appears in the workspace — a real
   shell in a real checkout, not a preview.

3. **`w w` — Work the failing PR (35–60s).**
   With the failing-CI row selected, press `w w`. The first `w` opens the work
   menu and the second starts the default agent (Claude) in the worktree with a
   context-aware prompt already filled in —
   on a PR with failing CI it's the "fix the failing checks" prompt, not a
   blank box. Let a few lines of the agent's first turn play.

4. **Slack loop — control from your phone (60–90s).**
   Split frame: lazybox on the left, Slack on the phone (mirrored) on the
   right. The agent hits a decision point and posts a "needs input"
   message to Slack. On the phone, reply `@lazybox yes`. Back in lazybox, the
   agent picks the reply up and continues. This is the bidirectional path
   — drive a running agent without touching the keyboard.

5. **Close — merge and clean up (90–110s).**
   Back on the PR row, `g m` to merge. The "remove worktree?" prompt
   appears; confirm. The worktree is torn down and the inbox returns to a
   clean state — the task is done and gone from the queue.

## Editing notes

- Stitch the takes; don't try to nail all five in one continuous run.
- Trim dead air between key presses, but keep each live moment (the row
  lighting up, the agent's first lines, the phone reply landing) on screen
  long enough to read.
- Caption the keys as they're pressed (`w w`, `g m`) so the screencast
  doubles as a quick keybinding reference.
