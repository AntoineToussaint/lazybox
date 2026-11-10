# Reload, native lifecycle and first resource-ownership increment

2026-11-09. Local macOS qualification; no user's Working pointer was changed.
This supersedes the earlier reload blocker, not the broader phone/Hub release gates.

## Change classification

| Change | Classification and owning boundary |
| --- | --- |
| Configurable macOS polling backend shared by desktop and web Vite entry points | **Hack / explicit compatibility stopgap**, not a fix for the host's FSEvents failure. Bounded intervals, native-build exclusion, selectable original backend and removal criterion are documented in `RELEASE_GATES.md`. Existing upstream owner issue: [fsevents #406](https://github.com/fsevents/fsevents/issues/406). |
| Wait for the browser's HMR connection before editing its fixture | **Fix** in the acceptance fixture: readiness is observed, not replaced with a sleep. The original reload assertion remains and now also covers nested atomic saves. |
| Private lifetime capability for provider/command process groups | **Fix** at Runner process ownership. Reuses the reviewed guard idea from the staged candidate, with idempotent Stop/Wait and explicit failure when cleanup evidence is missing. Other staged workflow changes remain unapplied. |
| Configurable bounded admission and retention of uncertain command ownership | **Fix** at Runner/command admission and cleanup. No new Hub state or OS service manager. |
| Repeatable isolated native service qualification | **Fix** in qualification tooling, exercising the existing Codefly lifecycle API. |

## Reload diagnosis and acceptance

On macOS 26.6.2, Vite 8.3.5 / FSEvents 2.3.3 missed writes under Node 24.20.0,
24.22.1 and 26.10.0. A standalone C program calling Apple's API reported
`FSEventStreamStart=1` and zero callbacks after its write. This rules out a problem
confined to the npm binding's prebuilt binary; the host-level root cause remains
unresolved. No system service was restarted or OS event logs reset.

Selecting `fs.watch` delivered browser CSS changes, but a real-project shutdown
hung. Sampling that owned diagnostic process located the main thread in
`uv__fs_event_close` → `uv__fsevents_close` → `uv_sem_wait`. A temporary-directory
delivery-only probe had missed this boundary. All stalled diagnostic processes
created by this work were terminated by their recorded identities; their test
files were removed before final staging.

Polling passed the browser test, including shutdown. Selecting the original
backend again reproduced the 15-second reload failure. The configured macOS
default is polling at 250 ms for text / 1,000 ms for binary files; other platforms
keep Vite's default. `LAZYBOX_V2_UI_WATCHER` permits explicit selection. No silent
retry/fallback loop or disabled acceptance test was introduced. The raw upstream
diagnostic remains failing; a product gate pass is qualified by this stopgap.

## Passing release and real service qualification

Candidate `2026-11-09T17-06-56-727Z-12f409ec` passed **all 25 release gates** and
built its macOS app. This snapshot precedes the process-guard increment and was
used only for isolated service qualification. Logs: `.cache/p03-stage.log`.

`scripts/qualify-runner-service.mjs` copied that validated candidate into
`.cache/service-qualification/run-jigBVd`, selected it only in that test environment,
and exercised the installed Codefly executable and native macOS supervisor:

- Installation did not start the service.
- Two start/stop cycles reached authenticated Runner `Info`; an invalid token
  was rejected, and each start published a new endpoint credential.
- Independent HTTP readers completed/disconnected while the Runner stayed ready.
- Each stop released its listener and preserved a fixture in product state.
- Finally, uninstall removed the uniquely labelled qualification service and
  retained its private report/state. No provider session was opened.

Receipt: `.cache/service-qualification/run-jigBVd/qualification.json`.
Test label: `ai.lazybox.v2.working.28db01a446ee16e2`; final state: not installed.
This verifies actual native service lifecycle and HTTP reader independence,
not native desktop-window closing, inference, reboot or remote connectivity.

## Process ownership and admission evidence

The guard, JSONL, commands and Runner suites passed with Go's race detector.
They exercise real fixture processes: kill the fixture owner, require its helper,
child and grandchild to stop, and require an unrelated process to remain alive.
Normal exit also reclaims background descendants. Stop/Wait concurrency, argv,
environment, cwd, output, exit status, missing targets and unowned helper invocation
are checked. JSONL reports missing guard cleanup evidence as a closure failure.

A second regression deliberately creates a fixture child in a new session that
keeps stdout open. Previously, the helper outcome hid the resulting `WaitDelay`
error. The new test failed against that behavior; the fix now returns unconfirmed
cleanup instead of successful completion. The test explicitly terminates its
escaped child. This detects that case; it does not claim to contain detached
processes. Log: `.cache/p03-unclosed-output-before.log`.

Go's `exec.Cmd.Wait` prioritizes the helper's signal exit over its output-copy
error. The guard therefore owns and bounds its output drains explicitly. JSONL
owns its read pipe independently of helper reaping: queued messages drain, while
an escaped descriptor cannot prevent reaping indefinitely. Added race tests check
an 80-message final burst and an escaped stdout-only descriptor, as well as the
guard's captured-output case. Both escaped fixtures are explicitly cleaned up.

Countercheck: temporarily removing lifetime EOF termination made
`TestRunnerSIGKILLReclaimsOwnedGroupWithoutTouchingOtherProcesses` fail with a
surviving owned process. The test cleaned its group; the implementation was
restored and the race suites passed again. Log:
`.cache/p03-guard-without-lifetime.log`.

Candidate `2026-11-09T17-21-19-029Z-ab0955e5` also passed all 25 gates with the
initial guard/admission changes. It precedes the final output-drain hardening.

**Final candidate:** `2026-11-09T17-30-06-622Z-9c329304` passed all 25 gates,
including 35 environment/service/gate tests and 18 native Rust tests, and built
the macOS app. Its source digest stayed unchanged during validation:
`39dae23d2fa02be297a85a2baea92c0903da7a1d6de45f110034e188e9ffcfbe`.
The final guard, JSONL, commands and Runner sources/tests were compared with that
snapshot and match. Logs: `.cache/p03-qualified-stage.log` and
`.cache/p03-final-race.log`; full gate receipt is the candidate's `gates.json`.

The same native lifecycle qualification was repeated against that **final**
candidate and passed both start/stop cycles. Receipt:
`.cache/service-qualification/run-Io96Kn/qualification.json`; test label:
`ai.lazybox.v2.working.1e4eba266e2933a0`. A fresh Codefly status query confirmed
both qualification labels are not installed and not running after cleanup.
No candidate was promoted into the user's Working environment.

Admission tests cover pending starts, concurrent command registration, failed
startup releasing its reservation, confirmed closure freeing a slot, negative
configuration rejection, and zero retaining bounded defaults. Missing command
cleanup evidence keeps its slot and prevents workspace/history deletion.

See [the resource contract](../../RESOURCE_OWNERSHIP.md) for exact guarantees and
configuration. Detached groups, containers, PTYs, other helper subprocesses,
durable ownership reconciliation, reboot, Linux runtime and live-provider trees
remain unqualified. P03 is not complete; automatic native restart/login remains
disabled. The Obin Hub and phone workflow are not implemented by this increment.
