# Working and development installations

Use separate native apps and separate Runners when dogfooding. A working build
must not execute from the source tree currently being edited.

From the repository root on macOS:

```sh
make -C v2 dev
```

This builds **Lazybox Dev**, starts its own Codefly Runner on allocated ports,
and opens the native app. Its sessions, GitHub connection, workspaces and native
window preferences are independent of Working. Managed subscriptions can use
one explicitly selected shared store, as described below.
Keep the launcher running: closing the window leaves its Runner alive; Ctrl+C
in the launcher stops that Runner and its development watchers.

Dev reloads automatically: Vite/React refreshes UI edits inside Tauri; `tauri dev`
rebuilds and relaunches the native shell when Rust changes; Codefly's go-grpc
`hot-reload` watcher rebuilds and restarts the Runner when Go sources change.
The desktop rereads its selected private endpoint file and adopts the new bearer
on the same resolved origin. No credential is put in the Vite URL or public assets.
Vite binds a loopback port assigned by the OS and serves only UI source/dependencies.
UI watchers share the explicit macOS compatibility policy in
[release validation](RELEASE_GATES.md#watcher-diagnostic). The desktop launcher
reports its backend; `LAZYBOX_V2_UI_WATCHER=default|native|polling` overrides it.
The Claude SDK worker recompiles in watch mode; existing worker processes keep
their loaded code until the next session startup.

A backend reload does not stop Dev agents. Each runs in a [session host](SESSION_HOSTS.md)
that outlives the Runner, and the reloaded Runner re-attaches to it. Managed
commands an agent started keep running the same way. History and managed subscriptions remain in
persistent storage. Changing the launcher or dependencies still needs a relaunch.

Known Codefly limitation (CLI v0.1.212 / go-grpc v0.1.69): a Go edit that
does not compile exits the launcher instead of waiting for the corrected edit.
Fix the compile error and rerun `make -C v2 dev`. Successful compilable edits
reload automatically. The agent reports “compile error, waiting for hot-reload”,
but CLI `pkg/orchestration/runner.go` treats the resulting `StartStatus_ERROR`
as fatal through the flow failure sink. Recovery belongs in Codefly's
runtime/flow lifecycle; Lazybox does not wrap it in a second backend watcher.
Reproduce with an idle Dev Runner: introduce a Go syntax error, observe the
launcher exit, then correct it and observe that no watcher remains. An upstream
fix must keep the flow alive for recoverable rebuild errors and successfully
restart after correction, while still reporting fatal startup failures.

## Freeze a candidate and promote it

```sh
make -C v2 stage
make -C v2 promote RELEASE=<candidate-id-printed-by-stage>
make -C v2 working
```

Staging copies source into a new release directory, excluding `.cache`, private
`runner.config.json`, `.env` files, dependencies and build output. It installs
locked npm dependencies, runs the [shared release catalog](RELEASE_GATES.md),
builds the native app, and records each gate result plus source, app and compiled
worker checksums.
It disables Codefly source watching in the candidate before validation/hashing;
Working uses bundled assets and does not follow Dev source edits.
It never reads the credential stores to populate another environment.

New candidates use metadata version 2. Promotion rejects partial/failed receipts,
changed gate policy and older metadata without a complete receipt. Previously
pinned version-1 builds keep their existing launch path; stage again to promote
them under the new validation rules. A failing stage leaves its diagnostic
`gates.json` without promoting or starting anything.

**Working** runs the frozen source and its bundled native **Lazybox** app.
Codefly builds the Runner from that snapshot at launch; it still requires the
installed toolchain and provider CLIs. This is a local dogfooding workflow,
not yet a self-contained release installer. Source, npm and Go dependency locks
are pinned; installed external tools are not bundled. Fixture checks do not
certify live provider authorization or every resumability path.

Promotion changes only the release selected for the next Working launch. It
neither replaces a running app nor restarts its Runner, copies credentials,
or migrates its database. Restart Working explicitly to adopt a candidate. The
restart keeps running agents. They stay on the binary that started their
session host until they end.
The pointer records the previous release; selecting an older binary is **not**
a database rollback. A backwards-compatible storage version or an explicit
backup/migration procedure is required before downgrading a used environment.

## Codefly-managed Working Runner

The independent service path has passed an isolated macOS native lifecycle
qualification with two start/stop cycles and independent HTTP readers. Actual
native-window lifecycle and live provider execution remain unqualified. It uses
Codefly's `service` API for OS supervision. It does not implement a second
launchd/systemd manager. The current frozen release format supports macOS.

Repeat the live lifecycle check against an existing fully validated release:

```sh
node v2/scripts/qualify-runner-service.mjs <release-directory>
```

This deliberately installs a uniquely labelled test service in a separate
environment, verifies authenticated readiness and listener cleanup, and uninstalls
it in cleanup. Its private report and preserved test state live below
`v2/.cache/service-qualification/`. It does not change the user's Working pointer
or start provider sessions. Inspect `qualification.json` if cleanup fails.

After staging and promoting a validated candidate, stop the terminal Working
launcher, then use:

```sh
make -C v2 runner-service-plan       # inspect public config; no installation
make -C v2 runner-service-install    # install definition; does not start
make -C v2 runner-service-start      # explicitly start the selected release
make -C v2 runner-service-status
make -C v2 working-client            # attach; closing this client leaves Runner alive
```

This requires a Codefly CLI with `service install/start/stop/status/uninstall`
and `--json`. The CLI executable is resolved from PATH, or from an absolute
`LAZYBOX_V2_CODEFLY` override. The service records only resolved public filesystem
locators and an allowlisted toolchain PATH/Go resource limit. Credentials remain
in their existing private stores. An installed definition is bound to the
selected release, compiled worker, executable and public configuration hashes.
Changing that contract requires explicit installation while stopped.

`runner-service-status` reports supervisor process state; `readiness: not-probed`
is intentional. `working-client` checks the private endpoint's authenticated
Runner Info before opening the app. A process marked healthy by a supervisor
without a product probe is not evidence that the Runner is ready. During startup,
retry client attachment after Codefly finishes building/starting the service.

Automatic restart and login start are disabled in this slice. They must not hide
the outstanding agent resource-ownership/crash-cleanup gate. This provides
client-independent lifetime, not crash-safe agent recovery. The service still
needs the installed Codefly/Go/Node/provider toolchain; it is not a standalone
remote installer or a mobile connection mechanism.

Promotion selects a future release; it does not change an installed service.
To adopt a promoted release, explicitly stop, install and start. To remove only
the supervisor definition:

```sh
make -C v2 runner-service-stop
make -C v2 runner-service-uninstall
```

Stop/uninstall do not require the release files to remain present. Uninstall
requires a stopped service and leaves product state, credentials and logs alone.
The terminal `working` launcher refuses a Codefly-managed environment, preventing
two launch owners. Uninstall first to return to the terminal launcher. Dev
continues to use its separate terminal/watch lifecycle.

The service label derives from the canonical environment root; use the same
`LAZYBOX_V2_ENVIRONMENTS` for every command. Keep the same public toolchain PATH
when installing, starting and attaching, or reinstall the changed configuration
while stopped. Use a separate test environment for native lifecycle qualification.

## Isolation

The default environment root is `v2/.cache/environments`. Override it with
`LAZYBOX_V2_ENVIRONMENTS=/absolute/path` consistently for all commands. A custom root must be outside source or inside
an excluded state/build directory, so a snapshot cannot include itself.
`make -C v2 environments` shows the selected releases and resolved state paths.

| Resource | Working | Dev |
| --- | --- | --- |
| Native identity | `ai.lazybox.v2.working` | `ai.lazybox.v2.dev` |
| Native name | Lazybox | Lazybox Dev |
| Runner state | `<root>/working/state` | `<root>/dev/state` |
| Runner config | `<root>/working/runner.config.json` | `<root>/dev/runner.config.json` |
| Worktrees | `<root>/working/collections` | `<root>/dev/collections` |
| Execution source | Checked release snapshot | Current V2 source |

Each launcher has an ownership lock; the Runner separately locks its state.
Codefly allocates independent ports. Tauri namespaces window preferences by
app identity and Runner authority. Credentials are never copied between channels.
Explicitly selected **inherited local CLI** profiles still use the same system
login; isolated environments do not imply separate subscriptions or quotas.

The earlier `make run` / `make desktop` prototype remains separate under its
existing identity and chosen state. These commands do not adopt, move or
modify its sessions or workspaces. Never copy a live state directory.

## Share saved subscriptions between Working and Dev

An ordinary restart already reloads accounts from the same persistent store.
Switching to another environment previously showed an empty account library.
To reuse existing managed logins without signing in again, select their existing
Runner state directory once:

```sh
make -C v2 share-subscriptions SUBSCRIPTION_STATE=/absolute/existing/runner-state
```

This saves only a store reference in `<environment-root>/subscriptions.json`.
The next Working and Dev launch supplies that store through
`LAZYBOX_V2_SUBSCRIPTION_STATE`; standalone Runners can configure
`subscriptionState` in their Runner config. Without an explicit selection the
existing per-Runner default remains compatible. Both channel builds must support
shared subscriptions; an older pinned release is rejected instead of silently
opening an empty account list. Stage and promote an updated Working release.

Account registration and private provider homes stay at their original paths.
This matters for native authentication that can bind credential storage to the
config directory. No tokens, provider settings, permissions or system CLI
credentials are copied or overwritten. New enrollments use this same store;
Settings reloads its registry so another Runner's completed enrollment appears
without restarting. Atomic writes and the existing cross-process enrollment lock
protect registration. Pending browser login flows remain on the Runner that
started them; quota observations remain per Runner and can be refreshed there.

Selection refuses to hide existing channel-local subscriptions or replace a
different shared-store selection. Merging multiple populated account libraries
is not implemented. Shared subscriptions share the provider's allowance too;
sessions, workspaces, GitHub connections and release selection remain separate.

Native windows, pages and drafts are now keyed by a persistent Runner ID stored
in the session database, not its temporary port or bearer. Native view data is
saved privately beside native window layouts; it survives a changed Dev Vite
origin. Separate Working/Dev identities and state retain separate personal views.
Old layout files remain intact; first adoption of the stable identity starts a
new scope instead of guessing which old authority's private drafts to import.
Individual view entries are limited to 256 KiB, with a 4 MiB total budget; failed
writes are visible. An unreadable view file is retained and reported. Attachment
bytes are not persisted as drafts: reattach missing files after reopening.

The selected private locator is reread in both Dev and Working, and a different
Runner identity is rejected. A changed loopback port requires explicit Retry;
a renewed bearer at the same origin is adopted automatically. Native storage
and restoration logic are tested; physical display restoration is still a
manual validation gate.

Dev polls Info with a bounded timeout every three seconds and retries automatically.
The workspace displays one connection banner while the Runner is unavailable.
**Retry now** in native Dev rereads the same private endpoint locator, validates
its loopback URL, grants only that resolved RPC origin to the HTTP client, and
reloads the window with its saved target. This handles a full launcher restart
that allocated a different port. Normal hot reload adopts a new bearer at the
same origin automatically. Requests that mutate agents are never replayed by
connection retry.

Vite runs in middleware mode under a launcher-owned HTTP/HMR server. Standalone
Vite's SIGTERM handler calls `process.exit()`, which previously cut off watcher
cleanup; middleware mode leaves shutdown with the embedding launcher. A real
browser CSS-update test verifies that HMR still works on the bound server.
The Codefly supervisor has its own process group, so its shutdown signals cannot
terminate the desktop launcher before cleanup. On shutdown, Dev terminates only
the desktop and worker process groups it
started, waits a bounded grace period, and force-stops watchers that ignore
termination. This prevents orphaned watchers from rebuilding a window against
an old Vite origin. Dependency changes still require a Dev relaunch; avoid
reinstalling dependencies underneath a running development environment.
