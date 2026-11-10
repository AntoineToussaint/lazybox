# Release validation

The deterministic release gate has one owner:
[`scripts/release-gates.json`](scripts/release-gates.json). Staging and
`make check-release` / `make check-dogfood` run the same ordered catalog. CI should
invoke `make check-release`; a remote V2 CI environment is not provisioned by
this change. Codefly still owns service generation and transport validation;
the catalog delegates those checks to the existing Make targets.

```sh
make -C v2 list-release-gates
make -C v2 check-release
# Focused iteration; these receipts cannot promote a release:
cd v2
node scripts/release-gates.mjs run ui
node scripts/release-gates.mjs run core
node scripts/release-gates.mjs run native
```

The catalog covers core Go/TypeScript/contracts/worker checks, native desktop
checks and all isolated UI fixtures, including continuity, scrolling, navigation,
activity and sidebar tests previously omitted by staging. The responsive-shell
gate covers desktop/tablet/phone navigation, visible composition controls and
retained chat DOM/drafts/event readers across panel switches and resize. Every npm `test:`
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

## Evidence and promotion

Each run records its profile, catalog fingerprint, initial/final source digest,
platform, command/arguments, result, exit code and elapsed time per gate. A failed
gate does not suppress later diagnostic results. Cancellation leaves explicit
not-run results. Each gate owns a process group, has a deadline, and uses the
existing launcher cleanup for its descendants. Cleanup failure cannot become a
passing gate.

The policy fingerprint includes npm script commands, the catalog, Makefile and
gate runner implementation. Source mutation during checking fails the run.
Receipts are local validation records, not signed attestations against a malicious
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

## Watcher diagnostic

```sh
cd v2
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
