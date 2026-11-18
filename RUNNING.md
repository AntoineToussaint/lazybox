# Running the V2 agent and console test bench

The test bench is the execution-side **Runner**. Codefly serves one gRPC application implementation through native gRPC and browser-compatible Connect endpoints. The Hub is not implemented by this service.

For daily dogfooding, use the separate [Working and Dev installations](ENVIRONMENTS.md). The commands below retain the original single-environment bench workflow. [GitHub setup](GITHUB.md) documents the one-time Lazybox App registration and user connection flow.

Runner admission and process cleanup are described in
[resource ownership](RESOURCE_OWNERSHIP.md), including configurable live limits
and the remaining crash-recovery boundaries.

## Prerequisites and launch

The verified versions are in [runtime evidence](RUNTIME_EVIDENCE.md). Install Go, Node/npm, Codefly, and the provider harnesses you intend to exercise. Codefly's protocol generation uses its supported companion backend. Claude uses the pinned `@anthropic-ai/claude-agent-sdk` worker; it does not reimplement a private CLI protocol.

From the repository root:

```sh
make deps
make build
make run
```

In a second terminal:

```sh
make desktop
```

This builds and launches **Lazybox V2**, a native Tauri 2 app with bundled UI assets. It reads the resolved endpoint file supplied by the Makefile and uses the official Tauri HTTP plugin for unary and streaming Connect requests. No browser or Vite server is required. The native HTTP scope grants access only to that local Runner service; credentials are neither compiled into assets nor placed in window URLs.

The main native window opens the workspace with workspaces, agent chat and configurable modules. **Settings → Subscriptions** manages private Claude/Codex accounts. Chat startup selects **Claude** by default; use **New conversation** for a retained empty workspace, select an account, and click **Start Claude**. Collection-bound chats and explicitly selected Runner directories remain available. The project directory defaults to the repository root; override it with `make desktop PROJECT=/absolute/project`. `make desktop SESSION=EXECUTION` attaches to an existing session. The Agent selector can switch to Codex; **Model, gateway & permissions** exposes execution options. Existing local CLI sign-ins remain available for explicit session selection.

Send messages or images, answer request cards, interrupt, and queue follow-ups. **Commands & output** opens the command drawer; agents can also use their managed command tool. **Open viewer** creates another native Tauri window for that command. Reloading reattaches to the session ID in the window location. Closing a window disconnects its readers and leaves execution running; **End session** explicitly stops the agent. Native layout persistence, reconnect/reopen commands, and display-placement rules are implemented and have deterministic coverage; full cold-launch and physical display restoration remain unverified.

The composer uses **Enter to send** and **Shift+Enter for a new line**. Confirming an IME composition does not send. Busy sessions accept queued follow-ups through the same controls.

Subscription cards use a compact responsive grid, retaining account names, email addresses, status, usage windows and independent refresh actions. A wide window fits four cards per row; narrow windows use one column. The **Monitor** page is currently a workspace selector and command-output view, not an account or agent-health dashboard. `make dev` uses automatic UI/native/backend reload; see [development reload boundaries](ENVIRONMENTS.md). `make desktop` and Working retain bundled UI assets.

**Attach files** and drag-and-drop into an active chat accept any file type and multiple selections. Dropping adds files to the draft without sending; folders are rejected. The client sends file bytes with the message; it never sends a client-local path for the Runner to guess. The Runner advertises the selection limits through `Info`: four files, 20 MiB per file, and 20 MiB total per message. Selected files can be removed before sending, and an unchanged manual retry preserves the operation identity. Pasting an image into the message box (a screenshot from the clipboard) attaches it the same way, named `Pasted image <date> at <time>.png`; a paste that carries text stays text, so copying cells from a spreadsheet does not attach their picture.

Attached images show as thumbnails, in the composer and in sent messages, in every chat window. Resting the pointer on one, or moving keyboard focus to it, shows it larger in a floating card (within 60% of the window, never over its thumbnail); clicking it, or Enter, opens it full size, and Esc closes it. Thumbnails are a small copy decoded once off the main thread; every object URL is revoked. A sent image is read back from the Runner with the owner-only `ReadAttachment` RPC, only when its message scrolls into view, and only for a file the Runner retained as an attachment of a PNG, JPEG, GIF or WebP image (detected from the bytes, at most 20 MiB). Other files, and images the Runner no longer has (a deleted conversation's), show as named chips.

The Runner retains files under its resolved state directory and gives the agent a local file reference. Agents read or extract formats with their normal tools and permissions; uploading a format does not promise that every model can interpret it directly. Supported PNG/JPEG/WebP/GIF files up to 6 MiB are additionally sent as native image inputs. Larger images and other formats remain generic files. File-only messages are supported, and attachment names/sizes replay with the accepted message.

`make web` remains an optional browser test host and prints an authenticated URL. It uses the same component, with browser tabs instead of native windows.

## Existing repositories without GitHub

Use **Settings → Repositories → Add repositories**, or the Work sidebar.
Choose a folder once, filter the saved list and select checkouts. Create a **New workspace** from them, then
start an agent session. No GitHub App or `gh` login is involved. See
[Workspaces](WORKSPACES.md) for paths, worktrees and current limits.

## GitHub repositories without App installation

Open **Settings → GitHub**, connect a personal token, then select repositories
to import. If an App identity is already connected, explicitly disconnect it
first; imported repositories and sessions remain. Organization token and SSO
policies still apply. See [GitHub connection](GITHUB.md) for supported tokens
and permissions. Lazybox does not read or modify local `gh` authentication.

## Reusable agent window

`@lazybox/surfaces` exports one `AgentWindow` component that owns the authenticated Connect client, profile discovery, session creation/attachment, event replay/reconnection, request cards, composer, and optional command output. A host supplies its resolved connection; it does not wire RPC calls or subscriptions:

```tsx
import { AgentWindow } from '@lazybox/surfaces';

<AgentWindow
  connection={{ baseUrl: runnerUrl, token: runnerToken }}
  agent="claude"
  workingDirectory={projectDirectory}
/>
```

`agent` defaults to `claude`. `workingDirectory`, `initialSessionId`, and `initialCommandId` are initial choices when mounted. Session startup requires a click, so React StrictMode does not create duplicate executions. A connection change resets the component and disconnects the previous readers without closing their execution.

Optional `connection.fetch` supplies a platform transport: Tauri provides the official native HTTP fetch, while browsers use their standard fetch. Optional `onSessionChange(id)` lets the host persist the session location (`''` clears it); `onOpenViewer(sessionId, commandId)` lets it open a platform-specific window and may return a promise. Each mounted window has independent stream cursors and navigation.

`make test-browser` exercises this flow against a running Codefly service without requesting model inference. `LAZYBOX_V2_REAL=1 make test-browser` additionally sends one real Claude prompt and verifies the reply and transcript replay after reload; it requires a logged-in Claude profile and uses that account's capacity.

## Local service lifecycle

Codefly binds temporary ports and injects endpoint carriers. The Runner resolves its Connect address through the Codefly SDK. No daemon address or port is inferred from V1 state.

`BENCH_STATE` selects the isolated state directory; the Makefile default is `.cache/bench`. `make run BENCH_STATE=/absolute/test-state` overrides it. Each Runner locks its state directory and writes an owner-only `endpoint.json` containing its actual URL and bearer. Agents never receive that file: each execution gets its own endpoint file whose credential can only publish that execution's console (see [Agent credentials](RESOURCE_OWNERSHIP.md#agent-credentials)). With no configured state, the service creates a fresh OS temporary directory. Stopping the Runner ends owned processes; it retains evidence files. Remove a test directory only after its Runner has stopped and its evidence is no longer needed.

The Runner's gRPC and Connect listeners answer on loopback only. Codefly decides the bind host: go-grpc `0.1.70` and later bind the host the service's own endpoint carrier names, `localhost` in a native run (`0.1.69` bound every interface). The Runner refuses to start if any listener's host is not loopback, including an empty one, which binds every interface (`listen.go`; `TestRunnerListenersAnswerOnlyOnLoopback` dials each port from this machine's non-loopback addresses). Application RPCs also require the private bearer on **both** protocols; health is public. This is an experimental local authority with no Hub enrollment or production TLS configuration.

## Runner options and metering gateway

Copy `runner.config.example.json` to an ignored `runner.config.json`, then launch:

```sh
make run BENCH_CONFIG=runner.config.json
```

The configuration accepts `profiles`, `gateways`, `defaults`, `defaultGateways`, per-stream spool budgets, `repositories`, and `worktreeRoot`. See [Collections](COLLECTIONS.md) to create a group of worktrees and open a Claude session with `open --collection ID`. `make run` resolves `WORKTREE_ROOT` separately from session state. Each session can select model, `ask`/`read-only` permissions, `execute`/`plan` workflow, and a configured gateway ID. `direct` explicitly bypasses the configured default gateway. Plan workflow enables Codex's native question tool and prevents managed command starts.

Permissions default to **Current settings**. With no explicit permission choice
in the request or Runner configuration, the adapters inherit the provider's
saved settings: Claude retains its filesystem settings sources and configured
permission mode; Codex receives no approval/sandbox/reviewer override on start
or resume. The app does not rewrite provider settings files. Explicit `ask`,
`read-only`, or plan selections remain deliberate per-execution choices.
Native project settings are resolved from the execution's working directory;
the Runner does not silently merge permission files from several repositories.

A gateway entry has this shape; supply the exact endpoint issued by your gateway:

```json
{
  "id": "cost-meter",
  "provider": "codex",
  "url": "https://your-gateway/openai/authorized-route"
}
```

Add it to `gateways`, then set `defaultGateways.codex` to `cost-meter`, or select it per session. For Claude use `provider: "claude"` and its Anthropic base route. No route suffix is guessed: Codex appends `/responses`; Claude appends its Messages API paths.

By default the native provider credential resolver stays active and its authentication reaches the gateway. Codex receives an explicit custom Responses provider with `requires_openai_auth=true` and `supports_websockets=false`; setting `OPENAI_BASE_URL` alone would not establish metering for subscription traffic. Claude receives `ANTHROPIC_BASE_URL` while retaining provider auth.

For a gateway with its own bearer, set `tokenEnv` to an environment variable **name**. Its value is read in the selected child environment. Codex uses `env_key`; Claude maps it to `ANTHROPIC_AUTH_TOKEN`. Secrets are never accepted as URL userinfo, query parameters, profile values, or ordinary session metadata. Event records store gateway identity, not its URL or token.

The built-in `metered` route owns a local forwarding gateway and durable usage ledger. Configured external gateways remain supported. SDK totals are not summed as per-turn bills; provider usage and supported versioned rates determine API-equivalent estimates, with unknown components explicitly unpriced. See the telemetry section below.

## Profiles and continuation

The default `codex-default` and `claude-default` profiles inherit the harness's normal credential chain. They are labelled `inherited`; they do not advertise verified concurrent account isolation.

An explicit profile can select an absolute provider `home` and `envRefs` mapping child variable names to credential environment references. `isolation: "environment"` requires a private provider home and clears competing inherited provider credential variables only in that child context. For Claude it additionally requires explicit credential references or an enrolled private subscription token file; it never relies on a shared Keychain login. Current/default local auth directories and their symlink aliases are rejected. Never switch accounts through a global logout/login.

`continue --mode native` restarts an idle adapter within the same profile using its actual provider conversation reference. Cross-profile native resume, pending approval restoration, and in-flight tool recovery are not advertised. Reconcile active turns, requests, and commands before continuation.

`continue --mode checkpoint --profile <eligible-profile>` requires an explicit checkpoint JSON file. It closes the old writer before starting its successor, preserves the work identity and working directory, and submits an explicit handoff. A checkpoint needs objective, criteria, source revisions, and inspected working-tree state; it can include decisions, unresolved questions, completed results, command references, services, and omitted context. It does not replay old approvals or promise private model-state restoration. Failed startup leaves the old writer closed and visible in the journal.

Exhausted subscription capacity, temporary throttling, authentication failure, billing failure, transient retry, and unknown errors remain distinct where the provider supplies structured evidence. There is no automatic account rotation on arbitrary error prose. Live switching between two real profiles is unverified on this host.

The default inherited profiles keep the current provider credential/configuration
environment. Capacity or authentication errors do not switch subscriptions,
inject another API key, or trigger logout/login. Changing the selected account
requires an explicit profile choice or rotation to another enrolled, isolated subscription of the same provider (on by default, see below); a gateway credential route also requires
explicit configuration. Implementation checks use isolated fixture profiles.

## Test client

Use the endpoint file printed by the Runner:

```sh
export LAZYBOX_V2_ENDPOINT=/absolute/test-state/endpoint.json
bin/lazybox info
bin/lazybox open --profile codex-default --cwd /absolute/project
bin/lazybox send --session EXECUTION --text 'Hello'
bin/lazybox events --session EXECUTION
bin/lazybox approve --session EXECUTION --request REQUEST --decision accept
bin/lazybox answer --session EXECUTION --request REQUEST --answers '{"question-id":["Blue"]}'
bin/lazybox interrupt --session EXECUTION
bin/lazybox exec --session EXECUTION --wait -- /bin/sh -c 'printf hello; exit 7'
bin/lazybox read --id COMMAND --offset 0
bin/lazybox wait --id COMMAND
bin/lazybox cancel --id COMMAND
bin/lazybox output --id COMMAND --after 0
bin/lazybox continue --session EXECUTION --mode native --reason 'adapter restart'
```

Agent operations accept `--id` for idempotent retries. Queue acceptance, submission, provider acknowledgement, request resolution, turn outcome, process exit, and work acceptance are separate facts. These test sessions do not imply accepted shared work.

`exec --wait` returns the actual observed nonzero process status. A background command returns its ID immediately. Managed commands capture stdout/stderr channels and cancel their process group. This implementation targets macOS/Linux, not Windows or interactive PTYs.

To run a command so that the console shows all of it and the agent gets a compact, failure-first view with the real exit status (see [command output](COMMAND_OUTPUT.md)):

```sh
bin/lazybox run -- go test ./...
```

To publish an existing pipe while keeping its output visible to the agent:

```sh
set -o pipefail
some-command 2>&1 | bin/lazybox log --session EXECUTION --title tests
# In a workspace terminal, no flag: the run goes to the workspace's console.
some-command 2>&1 | lazybox log --title "go test ./..." [--to CONVERSATION]
```

To check an orchestration process file ([Orchestration §4](ORCHESTRATION.md#4-the-process-file)) before committing it, from the repository root (no Runner needed):

```sh
bin/lazybox process validate            # .lazybox/process.yaml over the presets; exits 1 on an error
bin/lazybox process explain task.reported
bin/lazybox process show [--json]       # the effective process
bin/lazybox process validate --workspace COLLECTION   # the Runner's view: your process.yaml and ceiling too
```

The user layer is `process.yaml` in the Runner's state directory (`processFile` in the Runner config); pass it to the file-only commands with `--user`.

The pipe publisher echoes stdin and returns a visible failure if publication fails, including startup failure. Capture EOF does not establish the producer's exit status; use shell `pipefail` or managed execution when that status matters. Agents and their managed commands get their execution's own endpoint file and identity so `lazybox run` and `lazybox log` work without guessing a daemon path. That credential publishes only that execution's console; `approve`, `answer`, `send`, `close` and every other verb need the owner's `endpoint.json`.

## Retention and failure behavior

Each command output journal defaults to 8 MiB, configurable from 4 KiB up to 64 MiB. A conversation's event journal is a chain of segments: `sessions/<id>.jsonl` is the first, `<id>.jsonl.1`, `<id>.jsonl.2`… follow, each up to `eventBudget` (default 8 MiB, from 1 KiB up to 64 MiB). When the next event does not fit, the journal starts the next segment; every reader (replay, the latest turns, earlier pages, the turn index, history, deletion) reads the chain as one journal. A journal from before segments is the chain's first segment. `eventHistoryBudget` bounds all the segments of one conversation attempt together: 2 GiB by default (thousands of turns at the ~240 KB a turn real conversations take), at most 64 GiB. Nothing is ever compacted or dropped: segments are evidence, and a resumed conversation starts a new attempt with its own journal. The registry admits at most 32 executions and 128 commands in a Runner lifetime, with 64 queued messages and 4,096 operations per session. It never silently rotates evidence. Exceeding a capture budget makes the failure visible; managed output failure terminates the command. A journal write the disk refuses is retried with the same event while the agent keeps running (its output waits in its session host), and the conversation says so. A journal that can take nothing more (its history budget spent) faults the conversation with event capture failed but never stops its agent; re-attaching it settles the fault once the journal takes writes again. Either way the conversation can still be ended, and is saved ended even when nothing more can be journaled. A single event larger than a segment gets a segment of its own.

A journal whose last record was cut short (power loss, a full disk) opens without it; its bytes are kept beside the segment in `<segment>.torn`. A write that fails part way is undone at once, so readers carry on and the next event takes its place. A journal with a damaged record in the middle is read up to it: the conversation stays in History with the reason as its fault, and can be deleted.

Generic attachments have a separate limit of 64 MiB and 256 retained files per execution, including zero-byte files. The store validates complete batches, rejects path traversal, uses private files, deduplicates identical content/name retries, and rolls back new files on batch write failure. Journal events contain names, media types, sizes, and resolved file references, not raw file contents. Files remain as evidence until the stopped Runner's state is explicitly cleaned; full Runner restart does not restore the in-memory attachment inventory. Historical transcripts and attachment references remain readable.

Events have durable sequence cursors; output also has byte offsets and channels. A future/out-of-range cursor is refused. Independent readers replay from disk rather than receiving producer-side queues, so slow/disconnected viewers do not hold command control. The browser keeps a bounded view; retained evidence remains readable by offset.

Adapter restarts use new execution identities and explicit native resume/checkpoint handoffs. In Dev, Working and the Codefly service (`LAZYBOX_V2_KEEP_AGENTS=1`), a Runner restart does not stop agents: each runs in a [session host](SESSION_HOSTS.md) and the next Runner re-attaches to the same process, with its turn, pending approvals and queued messages. Managed commands run in session hosts too and are re-attached with their output intact. An execution that cannot be re-attached (machine reboot, incompatible host) falls back to read-only history. Resume then explicitly creates one successor using the saved provider reference, account and settings. It never replays active queues or approvals. Unclean process ownership requires explicit reconciliation. Disk exhaustion is handled as a capture failure; a real full-filesystem drill has not been run.


## Start without a repository

Use **New conversation** with no workspace selected, choose Claude or Codex and the saved
account profile, then **Start Claude** or **Start Codex**. Each session gets a
distinct directory under the configured Runner state's `workspaces/` directory.
No repository or Collection is required and no Git repository is initialized.
The chosen account and native permission defaults apply to that session.
Closing the chat or ending the session retains its workspace files. A Runner
restart keeps running agents running (see [session hosts](SESSION_HOSTS.md)).
After a machine reboot, select the saved conversation and resume it when its
provider reference and settings are available.

`make test-empty-workspace` exercises both account choices, separate directories,
chat reattachment and no implicit session closure with a fake authority.

## Settings and subscriptions

App dialogs (New conversation, Add subscription and Add repositories) share a movable
header. Drag the title area to reveal the page behind it; the backdrop keeps
underlying text readable. Double-click the header to recenter, or focus the
header and use arrow keys (Shift for larger steps) and Home. Dialogs stay within
the window, adjust when it resizes and recenter when reopened. Form controls,
focus trapping and Escape retain native dialog behavior. Native desktop windows
have no separate title bar on macOS: each window's own header is it, with the
window buttons at its left, and its empty parts move the window (a
double-click does what System Settings says; `npm run test:title-bar`). `make test-dialogs`
exercises the shared dialog interactions against a synthetic Runner.

Open **Settings** from the main workspace, then **Subscriptions**. **Add
subscription** offers browser sign-in for Claude and Codex.
New named accounts become available without restarting the Runner or changing
an existing session's account. Each account has a private provider directory
under its persistent subscription state (Runner state by default). Working and
Dev can [share an existing subscription store](ENVIRONMENTS.md#share-saved-subscriptions-between-working-and-dev)
without moving credentials or signing in again. Existing local CLI auth files and system sign-ins are not
Settings-managed accounts and cannot be refreshed from this page. Claude
permission/sandbox preferences are copied into a new private settings file;
the original settings are not written.

Codex sign-in opens in the default system browser. Use **Copy code**, paste it into the provider page,
and wait for provider confirmation. Closing the dialog leaves the request on
the Runner; reopening Settings can resume it. **Cancel sign-in** explicitly
cancels enrollment. For Claude, finish the provider sign-in in your browser;
the app confirms and saves the account automatically. The browser and Runner
must currently be on the same machine for Claude’s native callback. No terminal
command or subscription token is needed in Settings. If Codex requests it, enable
device-code sign-in in ChatGPT Settings → Security, then start a fresh sign-in.

Account cards show the provider-reported email in small text beneath the custom
account name, plus usage and reset times. Email is read through Claude account
info or Codex account/read during refresh and retained across restarts; it is
omitted when unavailable, never inferred from the custom label. **Refresh usage**
reads Codex and Claude quota without inference. Claude uses a version-pinned
experimental SDK query and also reports quota during activity; missing data
stays unavailable. Subscription allowance and cost estimates are
different data.

Run `make test-settings` for isolated browser enrollment/usage checks. These
use a fake account authority, not live provider sign-in.

## Enroll subscriptions once

Run these commands **on the Runner host**, using the same explicit state
path passed to `make run`. Build with `make build-go` first. `account-login`
does not require an endpoint and does not change the global CLI login.

```sh
bin/lazybox account-login --state "$RUNNER_STATE" --provider codex --profile codex-work
bin/lazybox account-login --state "$RUNNER_STATE" --provider codex --profile codex-personal
```

Each invokes the native Codex browser login with its own credential store.
Claude is refused here: Lazybox does not import or store a Claude
subscription token ([CLAUDE_COMPLIANCE.md](CLAUDE_COMPLIANCE.md)). Give Claude
agents an Anthropic API key in Settings → Agents & workspaces → Claude, set a
cloud provider's variables (`CLAUDE_CODE_USE_BEDROCK=1` and so on) in the
Runner's environment, or sign in to Claude Code yourself (`claude`, then
`/login`); the built-in `claude-default` account uses that sign-in.

Repeat enrollment for each account with a distinct profile ID. The Runner
loads private `accounts/profiles.json` metadata on startup, alongside configured
profiles. Settings enrollment reloads the registry live. For CLI enrollment,
do initial setup before starting work; executions are not restored by a Runner
restart.
Do not duplicate enrolled IDs in the Runner configuration.

Rotation is off unless an extension's account policy turns it on
([EXTENSIONS.md](EXTENSIONS.md#account-policy)): add and approve **Subscription
rotation** in Settings → Extensions, and a new conversation may then switch to
every other enrolled subscription of its provider. Uncheck **Switch
subscriptions automatically** in session startup (CLI: `--no-rotate`) to keep
one conversation on its account, or turn off **Automatic switching** on an
account in Settings → Subscriptions to leave it out of new conversations. Or
pass an explicit pool through the CLI (ignored without an account policy):

```sh
bin/lazybox --endpoint "$ENDPOINT" open --profile claude-work --collection "$COLLECTION" --rotate-profiles claude-work,claude-personal
bin/lazybox --endpoint "$ENDPOINT" assistant --profile claude-work --purpose planning --context ./planning-context.txt --rotate-profiles claude-work,claude-personal
```

The assistant command returns a normal session ID. Use existing `send`,
`events`, `interrupt`, and `close` commands for that conversation. Help,
planning and summary are supported purposes. The optional **Ask Lazybox**
workspace module provides the same conversation UI without selecting a repo.

Rotation uses a portable checkpoint in the same execution and worktrees; it
never passes a native conversation ID to another account. Accounts that
report invalid authentication remain unavailable to automatic selection until
renewed; expired/revoked credentials are not silently refreshed through a new
interactive login. Native provider refresh tokens continue to be managed by
the provider. Enrollment currently refuses overwriting an existing profile;
credential renewal and account removal UI are subsequent work.

### Context and workspace costs

When starting a conversation, expand **Model, gateway & permissions** and choose
**Metered** to route supported provider requests through the Runner's local
metering gateway. The selected subscription still supplies authentication.
The chat's context disclosure shows provider-reported usage; missing categories
or capacities stay unknown. Add **Usage & cost** through **Panels** in the workspace header to
view the workspace aggregate across conversations and account rotation attempts.
Workspace-bound Ask Lazybox planning conversations use the same attribution.

Costs are API-equivalent estimates from versioned model prices, not subscription
charges. Unsupported models, tiers or incomplete usage stay explicitly unpriced.
The private Runner ledger survives restart and stores usage metadata without
prompts, response text or credentials. Existing Direct conversations remain
direct and can report SDK usage with the limitations described in INTERFACES.md.

Double-click a repository in **New workspace** to select it and focus the name;
checkboxes support selecting several repositories. Workspace **Rename** changes
the display name only. New workspace folders use a generated readable slug that
stays fixed, and existing folders and branches are preserved.


### Local dogfooding loop

1. Open a workspace and start or resume a conversation with the chosen account.
2. Name the conversation with **Rename** above its chat; use the sidebar's
   **Find a conversation…** filter to find it later. Search runs over the whole
   workspace history before pagination, including saved titles and first-message
   titles. Continuation keeps the name and original ordering.
3. Choose **Review changes**. Each repository shows its current local branch and
   changed files; selecting a file opens its unstaged diff. **Staged** shows the
   index diff. Untracked text files can be previewed, binary files are labelled,
   and **Refresh changes** updates an open diff while the agent keeps working.
4. Switch back to Work; your draft remains. Restarting the Runner (promoting a
   release, a Dev reload) leaves agents running and re-attaches them, so there is
   no need to end conversations first. Commands an agent started through
   `lazybox_command` keep running too. Closing a window alone does not
   stop its agent.

Changes inspection is read-only and local: it does not fetch, stage, commit,
push or invoke configured external diff/textconv helpers. Lists are capped at
2,000 files, previews at 1 MiB and Git commands at eight seconds. Truncation,
missing/replaced worktrees and per-repository errors are visible. This is working
tree inspection, not revision-bound acceptance or a merge action.

Run `make check-dogfood` for the deterministic local gate. A live continuity
check is available through `make test-real-dogfood` with
`LAZYBOX_V2_DOGFOOD_SUBSCRIPTIONS` set to the selected existing private subscription
store and `LAZYBOX_V2_DOGFOOD_PROFILE` set to a managed profile ID. It makes real
provider requests in temporary two-repository fixtures, restarts the Runner and
checks native conversation memory and metering persistence. It never enrolls an
account, rewrites permission settings, or changes an existing project.

`make test-outlive` runs the deterministic, race-checked tests that agents
and commands outlive a Runner restart, including a Runner SIGKILLed with its
process group. They use real processes and need no account. How to check the
same by hand in Dev is in [SESSION_HOSTS.md](SESSION_HOSTS.md#testing).

`make test-real-outlive` is the live check that a Runner restart is a
no-op for an agent. It uses your existing `claude` and `codex` logins through the
metered gateway: it detaches the Runner while a reply is streaming, starts a new
Runner, and requires the same agent process to finish that reply and remember the
conversation afterwards. Select one provider with `LAZYBOX_V2_REAL_PROVIDER`.

### Sanitized activity recording and headless replay

From the V2 directory, record an existing Runner conversation without starting
inference or interrupting the agent:

```sh
node --import tsx scripts/record-activity.mts SESSION_ID /tmp/activity.jsonl dev
# Ctrl-C ends the recorder only.
node --import tsx scripts/test-activity-review.mjs /tmp/activity.jsonl
```

The recorder reads the launcher's resolved endpoint, creates a new output file
with mode 0600, and refuses to overwrite one. Its allowlist keeps lifecycle
states and tool kinds, pseudonymizes IDs/custom tool names, and discards prompts,
outputs, arguments, reasoning, account data and paths. `elapsedMs` is capture
arrival time, not original provider execution time when replaying a retained
journal. A recording is a diagnostic projection, not a backup or a provider
transcript. No real recording is checked into source control.

`test-activity-review.mjs` runs a fake Runner and headless WebKit; it never sends
provider requests. Without an input recording it replays the explicitly synthetic
Claude protocol fixture through `NativeStatus` and the production UI. Unit tests
also exercise Codex item lifecycle and its telemetry adapter's native usage
shape. The shared UI can be replayed from captured normalized events; native
adapter regressions must additionally test native events before normalization.

Workspace action menus stay above scrolling panels and support right-click and
keyboard navigation. Delete workspace keeps source repositories, branches, saved
conversations and subscriptions; running sessions or dirty files require explicit
stop/discard acknowledgement. See the [navigation UX audit](audits/2026-11-09/SIDEBAR_UX.md)
for verification and remaining mobile/layout limitations.


### Responsive workspace shell

The header and workspace-row plus buttons open the same **New conversation**
flow. The dialog names the target workspace. **Start with last settings** is an
explicit action inside that dialog; opening navigation, resizing, or opening the
dialog never starts an agent. Page customization, layout import/export and
opening another workspace window live under **Page actions**.

At phone widths (760px and below), use **Open workspace navigator** in the top
left to switch workspaces/conversations. The navigator overlays the active panel.
The panel tabs switch between chat and other configured panels without removing
the chat component or event reader. Wider windows retain the saved split layout.
Dialogs render in a shared overlay layer, so hiding their original panel during
resize does not hide the dialog. The responsive check is part of the required
release catalog: `npm run test:responsive-shell`.

Repository pickers keep the workspace name and action buttons visible while the
repository list scrolls, including on narrow screens. Search filters the catalog
without changing the current selection; **Clear search** restores the full list.
If workspace repository details cannot load, the existing checkouts remain listed
and **Retry** reloads their metadata.
