# P01 implementation evidence

2026-11-09; local working tree, not a deployed or committed release.

Follow-up: [reload and resource qualification](RESOURCE_OWNERSHIP.md) records
the passing full release gate and isolated real macOS service lifecycle check.
The fixture-only status below describes the initial P01 implementation.

## Change and ownership

Fix at the lifecycle configuration boundary: use Codefly's existing native
service manager and separate client attachment. No bespoke OS supervisor,
restart loop, Hub, credential store or port allocation was added. Shared Runner
arguments keep the terminal and managed launch paths consistent.

Implementation: `scripts/runner-command.mjs`, `scripts/runner-service.mjs`,
Working launcher conflict check in `scripts/environments.mjs`, Make targets,
and `scripts/runner-service.test.mjs`. See [the package plan](../../IMPLEMENTATION_ROADMAP.md)
and [operating instructions](../../ENVIRONMENTS.md#codefly-managed-working-runner).

## Verified

- `node --test scripts/runner-service.test.mjs`: 12 passing tests, including
  an actual subprocess using a fake Codefly executable. No OS service mutations.
- `node scripts/runner-service.mjs status`: actual installed Codefly CLI returned
  the selected Working environment's `not-installed` state. Native manager is
  launchd; adapter reports product readiness as `not-probed`.
- Source syntax checks passed for the new modules. Tracked-file whitespace check
  passed; V2 is still untracked in the enclosing repository.

Tests verify changed source/app rejection, frozen reload, worker version changes,
explicit public arguments/environment, private config preservation, conflict with
a running terminal/service, no implicit start during installation, cleanup
commands without release files, readiness rejection, independent client lifetime,
Codefly executable resolution and environment-specific labels.

## Environment gate still failing

`make test-environments` (before the final managed-launcher regression was added):
24 passing tests; the existing test
`real Vite server watches CSS and refuses runtime-state files` timed out at 15s.
It also times out when run alone. A temporary isolated reproduction imported
only the unchanged `desktop-dev.mjs`, excluding all new service modules, and
reproduced the timeout. Temporary files and servers were cleaned up.

Observed sequence: Vite serves the initial stylesheet, rejects out-of-scope
source with HTTP 403, the browser connects to HMR, and the initial CSS renders.
The test writes changed CSS and waits indefinitely for the watcher's `change`.
A focused diagnostic shows the file present in `watcher.getWatched()`, the Vite
watcher using FSEvents, and direct Node `fs.watch` receiving the same write while
the Vite watcher emits no change. This narrows the failure to that notification
path; it does not establish the upstream root cause. Increasing the test timeout
or claiming a clean gate would conceal the defect.

P02 follow-up: reproduce with the pinned Vite/FSEvents versions on this machine
and an isolated machine; fix the notification behavior at its owner, or record
an explicitly labelled, configurable fallback if an upstream fix is unavailable.
Acceptance remains updating already-open browser CSS without reloading the app,
while keeping the existing filesystem serving restrictions and shutdown ownership.
No polling fallback or disabled test was shipped in this change.

## Not verified / not claimed

No live native service install/start/stop/uninstall, provider inference, client
window lifecycle against an installed service, Linux packaging, reboot recovery,
SIGKILL resource cleanup, Obin Hub integration, remote enrollment or real-phone
flow. No current user session was intentionally restarted or killed. Automatic
restart/login start remain disabled pending resource-ownership qualification.
The broader audit's authority and dependency defects remain open.
