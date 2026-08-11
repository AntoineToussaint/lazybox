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
make desktop          # or, by hand: cd apps/desktop && npm ci && npm run tauri dev
```

The Rust shell spawns `lazybox-server` through the shared `ClientRuntime`,
binds an authenticated loopback gateway on an ephemeral port, and points the
webview at it. Complete setup in the first-run dialog on first launch (GitHub
auth reuses the existing `gh` credential; no token is pasted into the webview).

Verification of the same build (what CI gates):

```sh
make desktop-test     # frontend tests/build + Rust shell tests
cd apps/desktop && npm run check && npm run e2e
```

The native integration test drives the real path end to end without a GitHub
credential: persisted setup, the authenticated gateway, an inbox mutation, and
a live shell PTY.

**Current status:** the frontend suite, browser accessibility/integration lane,
build, and native shell suite are CI-gated. The exact test totals are reported
by CI rather than copied here, so this log cannot become stale when coverage is
added. Empirical day-to-day dogfooding is still ongoing, and its findings land
in the ledger below as they happen.

## Fallback-to-TUI ledger

Every time the desktop can't finish something and I switch to the TUI, it gets
a row here. New rows are empirical — log them as they happen during real use,
newest first. The seed rows below are the known parity gaps from the readiness
assessment; the "Tracked in" column is the source of truth for each, and a row
is deleted (not marked done) once its issue closes — so the ledger only ever
lists live gaps.

| Gap | What I was trying to do | Tracked in |
| --- | --- | --- |
| Open in editor | Open a worktree in `$EDITOR` (`e`) | [#843][issue-843] (Tier-A leftover — terminal-spawn-with-command semantics; [readiness §4][readiness]) |
| Reviewers / assignees / labels | Edit PR metadata (`g r` / `g a` / `g l`) | [#843][issue-843] |
| Multi-select + broadcast | Act on several workspaces at once (`v`, `Shift-B`, `Shift-U`) | [#843][issue-843] |
| Repo pin | Pin a repo group to the top of the sidebar (`p`) | [#843][issue-843] |
| Session adopt / send / convert | Agent-to-agent handoff and adoption (`x a` / `x s` / `x j`) | [#843][issue-843] |
| Quick-jump navigation | Jump to asking / failing-CI / by-digit workspace (`!`, `Shift-F`, `]]<n>`) | [#843][issue-843] |
| Activity-pane row interactions | Expand/collapse rows, description reader, per-row mark-read | [#843][issue-843] |

**Landed, no longer a fallback:** connect-to-remote gateway + reconnect/resync
([#814][issue-814]); protocol version-skew tolerance ([#815][issue-815]); Tier-A
act-on-work — agent + model-tier + on-main spawn, merge, update branch, archive,
close/delete, browser, mailbox, rename ([#816][issue-816]); the Tier-B/C
automation & workspace-management slice — merge-on-green / auto-fix / track-main
policies, snooze/unsnooze, targeted sync, notes ([#817][issue-817], via #826);
concurrent terminals (tiles/tabs) + focus mode ([#818][issue-818]); read-only
diff inspection; and the live theme picker.

## Phase tracking

- [ ] **Phase 1** — run the desktop locally against its own in-process daemon
  and dogfood the triage + drive-an-agent loop, logging fallbacks-to-TUI in the
  ledger above. _In progress:_ the build is verified runnable (headless checks
  green); empirical day-to-day dogfooding is still accruing ledger rows.
- [ ] **Phase 2** — close the daily-driver gaps: [#843][issue-843] (the deferred
  Tier-B/C slice + the diff/editor Tier-A leftovers). Track the burn-down by
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
[issue-843]: https://github.com/AntoineToussaint/lazybox/issues/843
[readiness]: desktop-remote-readiness.md
[runbook]: byo-remote-runbook.md
