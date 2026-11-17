# P02 release-validation evidence

2026-11-09; local source and isolated candidates. This is not a deployed release.

Follow-up: [reload and ownership qualification](RESOURCE_OWNERSHIP.md) records
the explicit watcher stopgap, subsequent 25-gate pass and real native lifecycle
check. The failed snapshots below remain historical evidence.

## Changes and ownership

- **Fix at the release-tooling owner:** one catalog now drives staging and
  `make check-release` / `make check-dogfood`. Every npm test must be classified.
  Receipts include each command, outcome, duration, policy fingerprint and source
  digests. Failed, partial, stale-policy or legacy receipts cannot promote.
- **Fix at artifact validation:** promotion and both Working launch paths share
  source/app validation; version-2 releases also verify their receipt and compiled
  worker. Already pinned legacy releases can still launch, but cannot be newly
  promoted. Promotion only changes the selected release pointer.
- **Fix at keyboard ownership:** the detached agent window owns navigation even
  when its chat uses embedded presentation. Enter previously failed in that
  window. The workspace host retains ownership for its own embedded chats.
- **Fix at Vite filesystem policy:** deny private paths relative to the canonical
  source root. The previous global `.cache` pattern denied all frontend assets
  when the release itself lived below `.cache`. Private files remain denied.
- **Fix at the native build dependency:** build frontend assets before Tauri
  checks that embed them. A clean snapshot previously depended on an earlier
  unrecorded frontend build.
- Updated stale UI fixture selectors and account fixtures to match the current
  unified repository/workspace UI. Assertions for identity, permissions,
  continuity, uploads and non-destructive operations remain in place.

No alternate watcher backend was configured. No Hub implementation or separate
service supervisor was introduced. Codefly generation and service ownership
remain in their existing layers. See [release validation](../../RELEASE_GATES.md).

## Reproduced regressions

Both product fixes were checked in both directions, with the fixed implementation
restored afterward:

1. Restoring the old detached-window keyboard condition made the attachment
   fixture fail because Enter never sent its message. Restoring window ownership
   passed standalone, detached and workspace cases.
2. Restoring the old global cache deny rule made the staged-source fixture fail
   with HTTP 403 for public CSS. Restoring source-relative rules passed public
   serving and denials for `.cache`, `.git`, `.env`, Runner configuration, native
   source and PEM files, including a source path containing glob characters.

The new gate tests cover omitted tests, duplicate classifications, altered npm
commands/Make policy, failed and incomplete receipts, cancellation, source
mutation, argument boundaries, deadline cleanup of an owned child process, and
removal of inherited live-provider/source/watcher overrides. Promotion tests also
reject a changed compiled worker while preserving the existing pointer.

## Isolated staging

First candidate: `2026-11-09T16-36-48-022Z-a50404af`.

- All 23 isolated UI gates passed.
- Core failed because public frontend files inside the frozen `.cache` source
  were denied; native checks failed because frontend assets had not been built.
- Staging refused the candidate. Both defects above were fixed before the second
  snapshot. The failed receipt and log remain diagnostic artifacts.

Second candidate: `2026-11-09T16-46-38-740Z-97321f54`.

- Core Go, TypeScript, generated-contract, client and worker checks passed.
- Environment/service/gate tests: 33 passed; the existing real-browser CSS reload
  test timed out at 15 seconds. It was not removed or relaxed.
- Native format, frontend build, all 18 Rust tests and clippy passed.
- All 23 isolated UI gates passed again, including scrolling, resume continuity,
  editor navigation, sidebar, repository selection and attachments.
- Final catalog result: **24 of 25 gates passed**; core failed solely at the CSS
  reload test. The source digest remained unchanged throughout validation:
  `48e0536a5302a5d7d9ee7efecf80e7d279cafedbc11fd9832e282617cefe3bcd`.
- Staging exited with failure and did not write `release.json`. The complete
  receipt is in the candidate's `gates.json`; local output is retained in
  `.cache/p02-stage-final.log`.

Each candidate has its own source snapshot. Build concurrency was limited to one
job. These checks used isolated UI/transport fixtures, not live model inference.

## Open blocker: macOS filesystem notifications

Reproducer: `node scripts/diagnose-ui-watcher.mjs`. It creates and removes its own
temporary files and watchers; it does not change Dev configuration.

| Runtime | Vite default backend | Explicit native backend | Explicit polling backend |
| --- | --- | --- | --- |
| Node 24.22.1 / Vite 8.3.5 | Missed change | Received change | Received change |
| Node 24.20.0 / Vite 8.3.5 | Missed change | Received change | Received change |
| Node 26.10.0 / Vite 8.3.5 | Missed change | Received change | Received change |

Direct Node file watches received the same changes. A direct FSEvents 2.3.3 probe
also missed them. The failure is below browser rendering and is not confined to
one tested Node version. This does not identify whether the cause is in FSEvents,
its integration, or the host. No OS reset, global service restart or polling
fallback was attempted.

Owning-boundary follow-up: reproduce the standalone notification probe on a clean
macOS host with the same dependencies; repair notification delivery at the
responsible library/host boundary. Acceptance is the existing browser test:
already-open CSS updates, forbidden filesystem paths remain forbidden, and
shutdown releases owned watchers/processes. A configurable alternate backend, if
chosen later, must be labelled as a stopgap and separately qualified. No external
issue was filed in this work.

## Limits and environment side effect

P02 is incomplete: no passing full release receipt, remote CI provisioning,
Linux package, pinned toolchain distribution, live service/reboot qualification,
real-phone flow, Hub integration or live-provider parity run. The unapplied
coordination candidate and existing V1 edits were left untouched. No Working
release was promoted and no active Runner was intentionally restarted.

An inventory command, `mise ls node --json`, unexpectedly reported uninstalling
the inactive Node 24.21.0 installation. Active Node remained 24.22.1. No further
mise commands or automatic restoration were attempted; subsequent comparisons
used already installed executable paths directly. This is an observed environment
side effect, not a required release change.
