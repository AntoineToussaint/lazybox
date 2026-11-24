# Release validation

The deterministic release gate has one owner:
[`scripts/release-gates.json`](scripts/release-gates.json). Staging and
`make check-release` / `make check-dogfood` run the same ordered catalog, and
per-change [CI](#ci) runs it in parallel jobs. Codefly still owns service generation and transport validation;
the catalog delegates those checks to the existing Make targets.

```sh
make list-release-gates
make check-release
# Focused iteration; these receipts cannot promote a release:
node scripts/release-gates.mjs run ui
node scripts/release-gates.mjs run core
node scripts/release-gates.mjs run native
node scripts/release-gates.mjs run ui --only inbox,chat-scroll   # named gates
node scripts/release-gates.mjs shards ui 6                       # CI's matrix
node scripts/release-gates.mjs run ui --only inbox --retry-flaky # CI: a failed UI gate runs once more
```

The `reboot` gate kills the Runner, every session host and their programs as
a reboot does, starts a new Runner on the same state, and checks that
conversations, checkpoints, missions, queued messages and command history come
back and the agent resumes its provider session with a notice
([SESSION_HOSTS.md](SESSION_HOSTS.md#after-a-reboot)).

The `claude-settings` gate (`npm run test:claude-settings`) starts the
bundled Claude Code with bad settings and checks it still prints the
settings-not-in-effect warning committed in
`workers/claude-sdk/src/testdata/settings-not-in-effect.json`, which the
worker's unit tests read. Starting the real binary takes as long as the
machine allows, so it is a gate with its own deadline (and its own CI step),
never a unit test: a unit test reads only committed files. Drift, a start
that times out and a platform with no bundled binary fail it;
`npm run test:claude-settings -- --write` rewrites the fixture, and refuses to when a capture is not the warning naming its case's source (a crash, a sign-in prompt) or `--version` fails.

The catalog covers core Go/TypeScript/contracts/worker checks, native desktop
checks and all isolated UI fixtures, including continuity, scrolling, navigation,
activity and sidebar tests previously omitted by staging. The responsive-shell
gate covers desktop/tablet/phone navigation, visible composition controls and
retained chat DOM/drafts/event readers across panel switches and resize. The inbox
gate covers attention counts across workspaces, grouping, keyboard, mark-seen,
opening in the owning workspace, sidebar unread markers and notifications on
watched transitions only. The scenarios gate plays every user story in
[`scenarios/`](scenarios/README.md) as an end-to-end test without recording
it, the same steps that make the demo videos. Every npm `test:`
script must be admitted, declared an exact alias, or explicitly classified as
requiring a live environment. Adding a test without classifying it fails the
catalog check. This does not imply that every conceivable test already exists.

Native checks build the frontend assets required by Tauri before compiling its
embedded context, so they work from a clean snapshot rather than relying on a
previous desktop build. Deterministic gates remove inherited live-Runner/provider
opt-ins and source/watcher test overrides; `make test` supplies its own freshly
built worker path. Explicit live tests remain separate commands.

The live browser/provider test stays explicit because it needs a selected Runner
and credentials. Real-device, remote Hub, native service recovery and provider
qualification remain separate roadmap gates. A deterministic pass is not a
claim that the phone development journey is released.

## A deadline on every unit test

Every unit test finishes within 30 seconds, and every unit suite enforces it
per test, so a hung or slow test fails fast and names itself instead of
spending its package's or its gate's whole deadline:

| Suite | Runs | Per-test deadline |
| --- | --- | --- |
| `node --test`: `npm run test:client`, `test:worker`, `test:browser-service`, `make test-environments` | `--test-timeout=30000` | A test (its subtests included) still running at 30 s fails. `publish-cask_test.sh`, one sequential shell test whose cases share the taps the first creates, runs as one such test (`scripts/release/publish-cask.test.mjs`). |
| Go: `make test`, `make test-outlive`, `make test-reboot`, `make stress-go` | [`scripts/go-test-guard.mjs`](scripts/go-test-guard.mjs) around `go test -json` | A test running for 30 s, not counting time `t.Parallel` holds it paused, gets its binary a SIGQUIT: the binary prints every goroutine's stack, the run fails, and the summary names the test. A parent's time includes its subtests', so when a subtest hangs the parent's clock runs out first; the guard names the deepest test still running under it. |
| Rust: `make check-desktop` (the Tauri crate and `tools/lazybox-snippets`) | `cargo nextest run` with [`.config/nextest.toml`](.config/nextest.toml) | `slow-timeout = { period = "30s", terminate-after = 1 }`: nextest ends the test's process and names it. |

`go test -timeout` bounds a whole package binary and stays as the backstop for
a binary that hangs outside any test, set above the slowest package as
measured on a loaded shared box: 6 minutes for `make test` (`internal/transport`
took 352 s with `GOMAXPROCS=1`), 5 for `make test-outlive` (`internal/runner`,
161 s under `-race`), 1 for `make test-reboot` (two tests, under 5 s). Only
that backstop covers what runs outside every test: package initialisation,
a `TestMain`'s work before and after `m.Run`, and the binary's start and
exit. The guard times tests from the `go test -json` events, and no test is
running then. The guard prints `go test`'s own text, not JSON: every line
with `-v`, otherwise a failing test's lines and the package results, as
`go test` prints them.

The rule cannot erode quietly. Loading the gate catalog (CI's first job,
staging, `make check-release`) runs
[`scripts/unit-test-timeouts.mjs`](scripts/unit-test-timeouts.mjs), which fails
on a `node --test` without `--test-timeout=30000`; on a `go test` outside
the guard (however `go` is named: `go -C <dir> test`, `$GO test`,
`$(GO) test`), except in the real-provider and release-smoke targets and
scripts it lists with their reasons; on `cargo test`; on a test file
(`*.test.mjs`, `*_test.sh`) run other than by `node --test`; and on a nextest
config without the 30 s deadline or a CI install of another nextest version
than the config requires. It reads the npm scripts, the Makefile, the
workflows and the scripts under `scripts/` (what they spawn, and shell
lines).

Real-provider tests (`make test-real*`) and the Playwright UI gates are not
unit tests and keep their own deadlines (the gate's `timeoutMs`). A test
that cannot fit in 30 s is integration work: it moves to such a target with
its own limit, and the pull request says so with numbers.

## CI

[The CI workflow](.github/workflows/ci.yml) has two profiles. Pull requests
run **fast** on Linux. Pushes to `main` and release builds run **full**.
To qualify a branch before merging, open **Actions → CI → Run workflow**
and choose the branch and **full**. Manual runs default to full.

| Check | Fast | Full |
| --- | --- | --- |
| Catalog, spelling, export, private-file checks | Yes | Yes |
| TypeScript, client and worker tests | Yes | Yes |
| Codefly and Buf regeneration, wire compatibility | Yes | Yes |
| Linux Go | Format and vet | Format, vet, full tests and restart race checks |
| macOS Go and launcher | — | Full tests, restart race checks and launcher tests |
| macOS Tauri | — | Build, format, tests and clippy |
| Playwright | Workspace, inbox, send recovery and voice, in parallel | Every UI gate across six shards |

Both profiles publish the `ci` result. Fast CI checks the common paths
before merge. The full profile covers native and cross-platform behavior and
the complete UI catalog. The release workflow always calls the full profile.
The TypeScript and UI jobs use the same Playwright image, matched to the
lockfile, for Chromium and its system libraries.

The UI matrix reads the same catalog as local checks.
`release-gates.mjs shards ui <count>` splits the UI gates into disjoint shards.
Fast CI selects its smoke gates with `--only` before sharding,
and each shard runs `release-gates.mjs run ui --only <ids>`, so it gets the same
environment scrubbing, process groups and deadlines as staging. An `--only`
report records its selection and can never promote a release. CI adds
`--retry-flaky` ([Flaky in CI](#flaky-in-ci)). Adding a gate to
the catalog adds it to full CI with no workflow edit; an unclassified `test:` script
fails the first job, as it fails staging. A failed shard uploads the
screenshots its suites saved (`.cache/*.png`, `/tmp/lazybox-*.png`) and its
gate report. The suites drive Playwright as a library, so there are no traces.

The launcher and browser worker tests use a cached Chromium headless shell.
The UI shards use the Playwright Ubuntu image matching the npm lockfile. The
image bundles Chromium, WebKit, and their Linux libraries, so each shard can
start testing without installing system packages.

The Go suite runs on macOS, where the desktop's Runner runs, and on Linux,
where a Runner for remote use runs ([REMOTE_RUNNERS.md](REMOTE_RUNNERS.md)).
macOS runs only what cannot run elsewhere: the launcher refuses other hosts,
and Tauri embeds WKWebView. CI runs the UI gates on Linux: they drive Playwright against the
fixture Runner, and the suites take their browsers from
[`scripts/fixtures/playwright.mjs`](scripts/fixtures/playwright.mjs), whose
pages report macOS off a Mac, so ⌘ stays ⌘ (the suites press `Meta`). Run
`make stage` on a Mac before a release: it runs every gate there, the UI gates
in macOS WebKit included.

`generated` installs the Codefly CLI release that produced the committed output
(pinned in the workflow, checksum-verified; v0.1.212 reproduces it byte for byte). `codefly sync` downloads the
pinned `go-grpc` agent and generates in its public `ghcr.io/codefly-dev/proto`
companion image through the runner's Docker, so it needs no Codefly login.

Not in CI, and why:

- `test:browser`, `make test-real*` and every other live/provider test: they
  need a selected live Runner and provider credentials (the catalog's
  `external` entries). CI holds neither.
- `make stage`/`promote` and the real Tauri app bundle: per-change CI checks
  the desktop crate (fmt, tests, clippy) but does not build, sign or launch
  the `.app`. The release workflow does, on a `v2-v*` tag (below).
- Real-device, remote Hub, native service install and provider qualification:
  separate roadmap gates, as above.

### Release

[`.github/workflows/release.yml`](.github/workflows/release.yml)
runs on a `v2-v<x.y.z>` tag. Its first job *is* this workflow
(`workflow_call`), so a release passes exactly the gates a pull request does;
only then does it build, sign and smoke-test the installable app
(`make release`, [RELEASING.md](RELEASING.md)) and publish it. The smoke test
starts the built standalone Runner in a temporary state directory, checks the
bundled CLI reaches it with the same version, and stops it.

`ci` is one check that passes only when every job did; it is the one to
require in branch protection.

## Evidence and promotion

Each run records its profile, catalog fingerprint, initial/final source digest,
platform, command/arguments, result, exit code and elapsed time per gate. A failed
gate does not suppress later diagnostic results in `make check-release`.
Staging uses `--fail-fast`: the first failure stops execution, with all remaining
gates recorded as `not-run`. Such a receipt cannot promote. Cancellation leaves explicit
not-run results. Each gate owns a process group, has a deadline, and uses the
existing launcher cleanup for its descendants. Cleanup failure cannot become a
passing gate.

The policy fingerprint includes npm script commands, the catalog, Makefile and
gate runner implementation. Source mutation during checking fails the run,
even when every gate passed; the run names the files that changed (the
receipt's `sourceChanged`, the log, and the CI job summary). A gate writes only
under `.cache` or a temporary directory. Receipts are local validation records, not signed attestations against a malicious
operator with write access to the release store. External toolchain distributions
still need P00/P02 version qualification and packaging.

`stage` runs inside its frozen candidate, saves `gates.json` there, and writes
release metadata version 2 only after the full catalog and desktop build succeed.
`promote` requires every expected gate to have passed for the exact source and
current policy, plus the original app/source/compiled-worker hashes. Missing, failed, partial,
outdated-policy or legacy metadata cannot replace the Working pointer. A changed
policy requires a freshly staged candidate.

Already pinned version-1 installations can still launch; existing user state is
not migrated or interrupted. They cannot be newly promoted under the stronger
rules. Version-2 launch verifies the receipt against its frozen policy. Updating
Dev's policy does not silently invalidate a running Working release.

Default reports go under `.cache/gates-<profile>.json`; `--report <path>` selects
another output. Run mutable-tree validation while editing is idle, or use
`make stage` for a separate snapshot. Dependencies and generated outputs may be
validated/rebuilt; source changes are reported, not silently blessed.

## Flake detection

A gate that fails once and passes on a re-run is a race or wrong
synchronisation until its cause is found. Re-running until green or raising a
timeout hides it. Reproduce it first, under the conditions that expose it:

```sh
make stress-go RUN='^TestName$' PKGS=./internal/runner [COUNT=50] [LOAD=15]
make stress-ui SUITES='chat-polish file-navigation' [RUNS=10] [PARALLEL=3] [LOAD=0]
```

`stress-go` runs `go test -race` with `GOMAXPROCS=1`, alongside `LOAD` busy
loops (default: one per core). `stress-ui` runs each suite `RUNS` times in
`PARALLEL` lanes at once, so suites contend for the machine and anything they
share. Both report failures per test, keep each failing run's log, and never
retry. Record the failure rate before and after the fix, with the run count.
A diagnosis is confirmed by widening the suspected window, for example a delay
just before the operation, until the test fails every time, and showing that
the fix passes with that delay still in place.

Shared state between UI suites stays out of the way by design. Each suite has
its own Vite dependency cache (`scripts/fixtures/vite-cache.mjs`), because
suites with different configs otherwise rewrite one shared cache under each
other. Its Vite server comes from the same fixture's `createServer`, which
neither watches the checkout nor hot reloads: an edit anywhere in the tree
while a gate runs (another agent's, a branch switch) would otherwise reload
the gate's page mid-step (`scripts/vite-server.test.ts` holds every suite to
it). Such a run still fails, on the runner's `sourceChanged` check, since what
it tested mixes modules served before the edit with modules compiled after.
`test:transcript-performance` bounds a long conversation's opening
relative to a short calibration conversation opened alongside it. The
absolute budgets apply only with `PERF_STRICT=1`, on a quiet machine.

### Flaky in CI

GitHub's runners are slower than a development machine (a few cores, a
cold Vite dependency cache for every suite), and a UI suite that waits on the
wrong signal fails there first. So that one such flake does not fail a pull
request while staying in sight, CI's UI shards run the gate runner with
`--retry-flaky`:

- a UI gate that **fails** runs once more. A deadline or a cancellation is not
  retried, and neither is any `core` or `native` gate;
- passing on that second run, its result is `flaky`, not `passed`, with both
  attempts in the receipt (`attempts`), and the run passes;
- failing again, it is `failed`, with both attempts;
- the log says `[<gate>] flaky (passed on retry)`; on GitHub Actions each flaky
  gate is a warning annotation on the run, and the job summary lists the run's
  flaky and failed gates with their attempts.

Staging and `make check-release` never retry: `--retry-flaky` is refused for
any profile but `ui`, a receipt from a retrying run is marked `retryFlaky`, and
`promote` admits only a receipt whose every gate `passed` on its first run. A
flaky gate is still a bug to fix, as above. What has held on a slow runner:

- wait for the state the next step needs (a switch enabled once its save is
  answered, a panel switcher rendered), never a count taken at once;
- time what the test does from when it does it, not from when the fixture was
  made: a cold dev server can take more than 10 s to show the page;
- a card that lasts a moment is recorded by the page as it comes
  (a `MutationObserver`), not read twice from the test;
- a reload or a closed context cuts in-flight polls, which WebKit reports as
  page errors ("access control checks"): ignore those while leaving, only;
- a fixture answers as the Runner does: proto3 JSON leaves out a field at its
  zero value, which the Runner saves as that value.

## Watcher diagnostic

```sh
node scripts/diagnose-ui-watcher.mjs
```

This compares Vite's default, native and polling backends in a disposable
directory, alongside Node's file watcher, then cleans up. It returns failure if
the default backend misses the change. It does **not** configure a fallback or
change Dev. The existing real-browser CSS reload test remains required.

On the current macOS machine, FSEvents delivery fails. The native backend delivers
changes but can hang during directory-watcher shutdown in libuv's FSEvents path.
The direct macOS API also starts without delivering events. See the existing
[upstream report](https://github.com/fsevents/fsevents/issues/406).

**Explicit compatibility stopgap (not an upstream fix):** Dev defaults to Vite's
polling backend on macOS, with a 250 ms text / 1,000 ms binary interval. Tauri's
native tree and source-local caches are excluded. Other platforms retain Vite's
default. `LAZYBOX_V2_UI_WATCHER=default|native|polling` selects a backend explicitly;
the launcher reports it. The deterministic gate checks the shipped default, not
an inherited override. Remove this stopgap after notification delivery **and
shutdown** pass on the supported macOS/toolchain combination.

The real browser gate still requires live CSS replacement, nested atomic saves,
private-path rejection and complete server shutdown. A passing configured-backend
gate does not mean the upstream FSEvents problem was fixed. The raw diagnostic
deliberately continues to fail while its default-backend probe fails.

### Conversation regression fixtures

The conversation-load gate holds older-page responses until the newest turn
is visible and checks that live following starts at the retained cursor. Its
absolute display budget remains enforced; relative replay timings are reported
for diagnosis, not used to infer which protocol path ran. The conversation-tabs
selection fixture scrolls the source into view and blurs the previous composer
before selecting text, so a right-click tests the intended selection.

In Go link fixtures, cleanup closes and joins every in-process server before
temporary state is removed, including the server's final audit write.

The reorder gate schedules a focus restoration and moves focus in the same
JavaScript task, proving the old row cannot steal focus on the next frame.
The latest-turn reload fixture waits for an explicit final event before
expanding commands; a count of two can appear before their final updates.

Hover-delay assertions use a paused browser clock: absence before the delay
and presence afterward cannot depend on how long CI stalls between commands.
Docking resize assertions wait for the final movement to reach the layout
before releasing the pointer, then verify the width remains persisted.

Every UI gate, and the scenario runner, counts browser runtime errors and
unhandled rejections through DOM events (`collectPageErrors` and
`collectContextPageErrors` in `scripts/fixtures/page-errors.mjs`), never
through Playwright's raw `pageerror` or `weberror`. WebKit's protocol labels a
fetch that a navigation cut, and the app caught, as `pageerror` ("… due to
access control checks"); that diagnostic is not an uncaught application error,
and counting it failed gates at random whenever a reload or goto cut a poll.

The collector fails closed. It reports an error the moment it is dispatched,
so one thrown in a `pagehide` handler or by a gate's last action still counts
(an error a document could not deliver while unloading waits in
`sessionStorage` and its tab's next same-origin document sends it). Only after
every listener has run does it retract an error the app marked handled with
`preventDefault`: on a `MessageChannel` task, which Playwright's clock never
pauses, or, while the document unloads and that task never comes, from a
`pagehide` listener moved behind the app's own when `beforeunload` fires (a
retraction the unload drops is sent again by the next document).
`drainPageErrors(errors)` waits until everything every collected page and
frame dispatched so far has arrived, and throws for an array no collector
fills; gates call it before asserting on their errors. The collector is an
init script: it must be installed before the page's first navigation (it
refuses otherwise) and before any other init script of the page or context;
the context form also covers popups and pages opened later.

The page-errors gate tests both browsers: genuine throws and rejected promises
are kept, caught fetches across navigation and handled errors are not; an
error from a `pagehide` handler, a last click, a paused clock and a later init
script arrive, and one the app handles in `pagehide` does not; drained errors
carry the step they were dispatched in; draining waits for a frame in another
process (Chromium with site isolation) and refuses an array no collector
fills; and the context, label and refusal behaviour. `scripts/page-errors.test.ts`
(`npm run test:client`) fails when a script listens to a raw error event (also
by a name built from parts, or not spelled out) or calls `page.pageErrors()`,
without a `raw-pageerror: <why>` exception; when a UI gate's script or a
`test-*.mjs` opens a page without the fixture and no
`page-errors-opt-out: <why>`; when it asserts on collected errors without
`drainPageErrors` on that array in the three lines before, without
`undrained-page-errors: <why>`; and when an init script is installed before a
collector on that same page or context (by variable, since its last
assignment), without `init-before-page-errors: <why>`.

The issue-start gate checks the action row at 1024×680 and 390×680. The new-task form scrolls its fields independently of the footer, including when agent controls wrap; repository selection and branch pickers remain covered by their existing gates.

The context-menu gate holds file loading while keyboard focus enters a menu and
injects a zero-motion pointer event: layout under a stationary pointer must not
replace keyboard focus. Mermaid's reader-position gate holds the actual near-view
observer notification until wheel movement is measured, then checks anchoring
during rendering. The chat-scroll gate gives native scrollbars explicit track and
thumb geometry before dragging; platform-dependent arrow buttons and minimum
thumb sizes must not turn a drag into a click on a scroll arrow.


End in a streaming chat immediately resumes following the latest output; its
regression checks this before native scrolling runs. Context-menu pointer
ownership compares client coordinates because some WebKit builds report zero
movementX/Y for real pointer motion. Subscription-switch uses the same runtime
error collector as workspace reorder, including its refresh page.

The chat-scroll gate also applies a small downward compositor correction during an upward key gesture: this must not resume following near the bottom. Near-bottom tolerance belongs to downward input; an unclassified scrollbar movement must reach the actual bottom. Link revocation tests join the serving side before checking its final audit record.

The chat-scroll gate keeps native Chromium scrollbars visible and verifies that prompt markers stay inside the content edge, then drags the thumb while output streams. The prompt-navigation gate checks that those markers still navigate the transcript.
