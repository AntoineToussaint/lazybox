# Working and development installations

Use separate native apps and separate Runners when dogfooding. A working build
must not execute from the source tree currently being edited.

This page is about developing Lazybox. The **installed release** people use
(a disk image or Homebrew cask, its Runner a launchd service, updates from a
signed feed) is a third, separate installation: see [INSTALL.md](INSTALL.md)
and [RELEASING.md](RELEASING.md). It has its own app identity
(`ai.lazybox.app`), state (`~/Library/Application Support/Lazybox`) and
launchd job (`ai.lazybox.runner`), runs its Runner without Codefly
(`lazybox-runner serve`), and never touches Dev or Working.

From the repository root on macOS:

```sh
make dev
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
The launcher rebuilds the `lazybox` CLI agents run (`<dev>/bin/lazybox`) from
the same change: it polls `services/runner/code` (Go files other than tests,
`go.mod`, `go.sum`), waits for edits to settle, builds beside the binary and
renames it over, so an agent never runs a half-written CLI nor one older than
its Runner. A failed build keeps the previous CLI and says so; it never stops
Dev. The desktop rereads its selected private endpoint file and adopts the new bearer
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

### Restart the Runner from the app

Settings → **Runner** (also reached by clicking the Runner status in the
workspace header) offers **Restart Runner** for a Dev or Working Runner started
by `make dev` or `make working`. After a confirmation, the Runner
records a restart request in `<environment>/restart-request.json`
(`LAZYBOX_V2_RESTART_REQUEST`, set by `scripts/runner-command.mjs` only for this
launcher) and stops through its ordinary shutdown, detaching agents, managed
commands and terminals exactly as any restart does. The launcher sees the exit,
finds the request, and runs Codefly again; the desktop and the Dev watchers keep
running, and the ownership lock stays held. Any other Runner exit still stops
the environment. A restarted Runner that does not become ready stops it too,
as a first launch would.

The new Runner usually gets a different port. The app follows it without
reloading: Restart Runner arms the desktop to adopt the next Runner its private
locator names (same Runner identity, loopback origin only), so drafts and
selection stay as they were. The workspace shows *Restarting…* rather than a
disconnection, then *Runner restarted*, or the reason a restart was refused.
`RestartRunner` is the owner's alone: an agent's scoped credential is denied,
and terminals hold no Runner credential.

Known Codefly limitation (CLI v0.1.212 / go-grpc v0.1.69): a Go edit that
does not compile exits the launcher instead of waiting for the corrected edit.
Fix the compile error and rerun `make dev`. Successful compilable edits
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
make stage
make promote RELEASE=<candidate-id-printed-by-stage>
make working
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
node scripts/qualify-runner-service.mjs <release-directory>
```

This deliberately installs a uniquely labelled test service in a separate
environment, verifies authenticated readiness and listener cleanup, and uninstalls
it in cleanup. Its private report and preserved test state live below
`.cache/service-qualification/`. It does not change the user's Working pointer
or start provider sessions. Inspect `qualification.json` if cleanup fails.

After staging and promoting a validated candidate, stop the terminal Working
launcher, then use:

```sh
make runner-service-plan             # inspect public config; no installation
make runner-service-install          # install definition; does not start
make runner-service-start            # explicitly start the selected release
make runner-service-status
make working-client                  # attach; closing this client leaves Runner alive
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

Restart Runner in the app is unavailable for the service, which nothing would
start again (`--restart never`); Settings → Runner says so. Restart it with
`make runner-service-stop` and then `make runner-service-start`;
agents keep running across that restart too.

Automatic restart and login start are disabled in this slice. They must not hide
the outstanding agent resource-ownership/crash-cleanup gate. (The installed
release's own launchd job, [INSTALL.md](INSTALL.md), does start at login and
keeps its Runner alive: agents outlive the Runner in session hosts, and a
restarted Runner adopts them.) This provides
client-independent lifetime, not crash-safe agent recovery. The service still
needs the installed Codefly/Go/Node/provider toolchain; it is not a standalone
remote installer or a mobile connection mechanism.

Promotion selects a future release; it does not change an installed service.
To adopt a promoted release, explicitly stop, install and start. To remove only
the supervisor definition:

```sh
make runner-service-stop
make runner-service-uninstall
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

If a terminal launcher starts from inside a Lazybox agent, it strips the
parent agent's provider authentication overrides before spawning its Runner.
The Runner then selects accounts from its own subscription configuration.

The default environment root is `.cache/environments`. Override it with
`LAZYBOX_V2_ENVIRONMENTS=/absolute/path` consistently for all commands. A custom root must be outside source or inside
an excluded state/build directory, so a snapshot cannot include itself.
`make environments` shows the selected releases and resolved state paths.

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
make share-subscriptions SUBSCRIPTION_STATE=/absolute/existing/runner-state
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
Runner identity is rejected. Every Runner start publishes a new bearer: a Codefly
hot reload keeps the port, a launcher restart (Restart Runner, or any other) gets
a new one. Both are followed automatically for a Runner that reports its
identity, whether or not the restart was requested from this app: the host
validates the loopback URL and grants only that RPC origin to the HTTP client
first. A Runner without an identity on a new port still needs **Reconnect this
window**. Native storage and restoration logic are tested; physical display
restoration is still a manual validation gate.

Each window rereads the locator every three seconds (every second while a
restart it requested is under way), and at once when a request is refused as
unauthenticated or cannot reach the Runner. A refused request is retried once
against the Runner the locator names (it was rejected before anything ran); one
that could not complete is not repeated, since it may have run. All of a window's clients, including open
streams and cached conversation readers, send the current bearer, never an empty
one; streams reconnect from their own cursor. A bearer the Runner keeps refusing
shows as **Not authorized** in the header. The workspace list keeps the last list
the Runner gave while it is unreachable and says it is reconnecting; a window
that has not heard from the Runner yet says so instead of showing no workspaces.
Dev polls Info with a bounded timeout every three seconds and retries automatically.
The workspace displays one connection banner while the Runner is unavailable.
**Retry now** in native Dev rereads the same private endpoint locator and reloads
the window with its saved target. Requests that mutate agents are never replayed
by connection retry.

Lazybox Dev (`ai.lazybox.v2.dev`) and Lazybox (Working, `ai.lazybox.v2.working`)
are separate applications with separate Runners and separate workspace stores
(`<environments>/dev/collections` and `<environments>/working/collections`): a
workspace created in one is never listed by the other.

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
