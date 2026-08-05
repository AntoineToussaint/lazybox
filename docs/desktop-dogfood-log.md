# Desktop dogfood log

_The running log for [#837][issue-837] — turning the desktop app on as a daily
driver, toward the remote-server + local-client topology. This is the live
companion to the one-shot readiness assessment in
[`desktop-remote-readiness.md`][readiness] (#806): that doc answered "is it
wired?", this one records "what happens when I actually use it." Update it as
you go — every fallback to the TUI is a line item here._

## How to run it locally (Phase 1)

Against its own in-process daemon (the default — no remote box involved):

```sh
cd apps/desktop
npm ci
npm run tauri dev
```

The Rust shell spawns `lazybox-server` through the shared `ClientRuntime`,
binds an authenticated loopback gateway on an ephemeral port, and points the
webview at it. Complete setup in the first-run dialog on first launch (GitHub
auth reuses the existing `gh` credential; no token is pasted into the webview).

Headless verification of the same build (what CI gates, runnable without a
GUI):

```sh
cd apps/desktop
npm test                                                    # frontend unit tests
npm run build                                               # tsc + vite production build
cargo test --manifest-path src-tauri/Cargo.toml --locked    # Rust shell + native integration test
```

The native integration test drives the real path end to end without a GitHub
credential: persisted setup, the authenticated gateway, an inbox mutation, and
a live shell PTY.

**Status as of this commit:** all three headless checks pass on a clean
checkout (`npm test` 67 passing, `npm run build` clean, the Rust shell test
green). Phase 1 is "it launches and drives a local agent"; the fallbacks below
are what still send me back to the TUI.

## Fallback-to-TUI ledger

Every time the desktop can't finish something and I switch to the TUI, it gets
a row here. New rows are empirical — log them as they happen during real use,
newest first. The seed rows below are the known parity gaps from the readiness
assessment, each mapped to where the fix is tracked; they graduate to
_resolved_ (or get deleted) as those land.

| Gap | What I was trying to do | Tracked in |
| --- | --- | --- |
| View diff | Review a PR's changes before merging | Tier A leftover — `InspectWorkspaceDiff` + a diff reader ([readiness §4][readiness]) |
| Open in editor | Open a worktree in `$EDITOR` (`e`) | Tier A leftover — needs terminal-spawn-with-command semantics ([readiness §4][readiness]) |
| Automation policies | Arm merge-on-green / auto-fix / auto-merge (`g p`, `g g`) | [#817][issue-817] |
| Reviewers / assignees / labels | Edit PR metadata (`g r` / `g a` / `g l`) | [#817][issue-817] |
| Snooze | Snooze a row out of the inbox (`z`, `x z`) | [#817][issue-817] |
| Multi-select + broadcast | Act on several workspaces at once (`v`, `Shift-B`, `Shift-U`) | [#817][issue-817] |
| Repo pin | Pin a repo group to the top of the sidebar (`p`) | [#817][issue-817] |
| Session adopt / send / convert | Agent-to-agent handoff and adoption (`x a` / `x s` / `x j`) | [#817][issue-817] |
| Quick-jump navigation | Jump to asking / failing-CI / by-digit workspace (`!`, `Shift-F`, `]]<n>`) | [#817][issue-817] |
| Theme picker | Switch theme live (`t`) | [#817][issue-817] |
| Activity-pane row interactions | Expand/collapse rows, description reader, per-row mark-read | [#817][issue-817] |

**Landed, no longer a fallback:** connect-to-remote gateway + reconnect/resync
([#814][issue-814]); protocol version-skew tolerance ([#815][issue-815]); Tier-A
act-on-work — agent + model-tier + on-main spawn, merge, update branch, archive,
close/delete, browser, mailbox, rename ([#816][issue-816]); concurrent terminals
(tiles/tabs) + focus mode ([#818][issue-818]).

## Phase tracking

- [x] **Phase 1** — run the desktop locally against its own in-process daemon;
  start dogfooding the triage + drive-an-agent loop; log fallbacks-to-TUI in the
  ledger above.
- [ ] **Phase 2** — close the daily-driver gaps: [#817][issue-817] (Tier B/C)
  plus the Tier-A leftovers (view diff, open in editor). Track the burn-down by
  deleting resolved ledger rows.
- [ ] **Phase 3** — dogfood against a remote box per
  [`byo-remote-runbook.md`][runbook]: daemon (`lazybox server api`) on the box,
  `ssh -L` forward, desktop pointed at the forwarded loopback port. Shake out
  reconnect robustness over real network events (sleep, wifi change, tunnel
  reset) and the degrade-under-remote behaviors (editor declines, browser /
  notifications fire locally).

## Verdict

_Filled in once the phases close._ The bar is two-staged, mirroring the
readiness assessment: **(1)** desktop credible as the primary *local* UI (Phase
2 gaps closed), then **(2)** credible as the primary *remote-box* UI (Phase 3
holds up over a real link).

[issue-837]: https://github.com/AntoineToussaint/lazybox/issues/837
[issue-814]: https://github.com/AntoineToussaint/lazybox/issues/814
[issue-815]: https://github.com/AntoineToussaint/lazybox/issues/815
[issue-816]: https://github.com/AntoineToussaint/lazybox/issues/816
[issue-817]: https://github.com/AntoineToussaint/lazybox/issues/817
[issue-818]: https://github.com/AntoineToussaint/lazybox/issues/818
[readiness]: desktop-remote-readiness.md
[runbook]: byo-remote-runbook.md
