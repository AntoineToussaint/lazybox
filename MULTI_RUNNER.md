# Several Runners in one window

Status: step 1 shipped (2026-11-16); step 2 shipped (2026-11-17, audit brief
R8); step 3's owner level built (2026-11-18, over SSH: [REMOTE_RUNNERS P0b](REMOTE_RUNNERS.md#p0b-the-owner-link-over-ssh-built)),
its device level designed below, not built. herdr shows the spaces and agents of
several machines in one sidebar, grouped by machine
([audit P2-2](audits/2026-11-16/herdr-gui.md#p2--later-or-strategic)).
Lazybox's equivalent is one window that lists several Runners — Dev and
Working on this machine, and Runners on other machines — and groups
workspaces and agents by Runner. The person's guide is
[docs/guides/several-runners.md](docs/guides/several-runners.md).

## What exists

- Every Runner is independent: its own state, credentials and endpoint file.
  The desktop app is bound to one Runner per launch (`--endpoint`), follows it
  across restarts, and keeps per-Runner view state (`viewRoot` is derived from
  the Runner's authority).
- A Runner can already be reached from another device, safely: the device
  gateway serves paired devices over Tailscale with scoped, revocable device
  credentials ([remote control](REMOTE.md)). It never accepts the owner's
  bearer and never listens on the LAN.

## Step 1 (shipped): the CLI knows every Runner

- The Dev and Working launchers record a locator per environment in
  `~/.config/lazybox/runners/<name>.json` (`scripts/user-config.mjs`): the
  path of its private endpoint file, never a credential.
- `lazybox runners` lists them with their state (running or not);
  `--runner NAME` (or `LAZYBOX_RUNNER`) picks one; `lazybox ls --all` lists the
  workspaces, conversations and agents of every running Runner, grouped by
  Runner — the sidebar this design asks for, in text.

## Step 2 (shipped): local Runners in one window

**What the person gets.** Workspaces has one collapsible section per Runner
(name, channel, version, health, what waits there); the Runner the page shows
lists its workspaces in full, the others one line each. Agents, the Inbox,
notifications, the dock badge, ⌘K and ⌘J merge every Runner, each row badged
with its Runner; Agents can also group by Runner. Opening anything of another
Runner shows that Runner's page. ⌘N starts on the page's Runner, which its
title names (*New task on Dev ▾*); the *on Dev* menu picks another. Each
Runner's section header has its own + (*New workspace on Working*), and an
empty Runner offers *Create one*: New task on that Runner, wherever the page
is; the page moves there once it starts. The panel's + names the Runner the
page shows in its tooltip. A Runner that stops answering says *Not reachable* on its
header and leaves the Inbox; nothing is an error. Settings → Runners lists
them all: show, make primary (the window opens on it), hide, add by file,
forget.

**Opt-in by default (2026-11-18).** Showing Working's workspaces in Dev's
window by default read as shared state ("Dev and non-Dev share state!"), so a
window now shows **only its own Runner** until the person opts in. Another
launcher's Runner on this machine (`source: registered`, no `remote`) is
hidden unless the person chose it (`RunnerPreferences.shown`, Settings →
Runners → *Show in this window*, or *Show it here* on the hint) or made it
primary; a Runner added by file and a computer added with *Add a computer*
are shown, and anything can still be hidden (`hidden`). A hidden Runner is not
connected at all. The first time another local Runner is found running, one
dismissible toast says so ("Lazybox Working is also running on this Mac. Show
it here? (Settings → Runners)"); each Runner is pointed out once (`noticed`).
Saved preferences from before keep a primary shown. The brand carries the
window's channel ("Dev", "Working"; nothing for the release app), from its own
Runner's listing, else `AppInfo.channel`.

**How.**

- **Discovery** (`apps/desktop/src-tauri/src/runners.rs`, `runner_list`,
  `runner_add`, `runner_forget`, main window only): the launchers' locators in
  `$XDG_CONFIG_HOME/lazybox/runners/*.json` (else `~/.config/…`, as the CLI)
  and files the person added (kept in the app's config dir as paths, never a
  credential). Each endpoint file is read like the window's own
  (`Bootstrap::read`: loopback origin, bearer, identity) and must be a regular
  file of this user with no group or other bits; a Runner is *running* when its
  port accepts a connection. A running Runner gets an HTTP capability for
  exactly `<origin>/lazybox.bench.v1.RunnerService/*`, once. The window's own
  Runner (same endpoint file or same Runner identity) is listed as `current`,
  without its bearer. `runner_bootstrap` is unchanged: a window still has one
  own Runner, which it follows across restarts as before.
- **Connections** (`RunnerDirectory.tsx`): the window's own Runner stays its
  `RunnerProvider`; each other running, shown Runner gets its own
  `useRunnerState` (own `RunnerLink`, bearer, renewal from the listing, info
  poll). `RunnerScope` re-provides one Runner's state, identity and seen-mark
  root to everything under it, so every `useRunner()` call below talks to that
  Runner — a sidebar section, an inbox row's actions and dialogs
  (`useRunnerActionHosts`), a peek, the ⌘N sheet, or the whole page.
- **The page** belongs to one Runner (`ActiveRunner` in `Workspace.tsx`):
  switching mounts that Runner's `WorkspaceView` with its own saved pages
  (view-state keys under `<authority>:runner:<id>`, so the host accepts them;
  app-wide preferences stay the window's). A target naming another Runner
  (`RunnerTarget`, `runner-directory.ts`) switches the page and is then shown
  there; layouts never store a Runner (`validTarget` refuses it). Windows the
  host opens (chat, console, workspace, artifact windows) and `lazybox
  open`/`shot` stay the window's own Runner's: on another Runner's page they
  are not offered.
- **Attention**: one merged inbox (`useMergedInbox`, `mergeInboxes`), each
  Runner's rows from its own sessions and its own seen marks; rows of a Runner
  other than the page's are keyed under it, so two Runners' conversations never
  share a key. Notices carry `runner`; the desktop's notification destination
  has a validated `runner` field, so a click opens the conversation on its
  Runner.
- **Browser**: the web client lists Runners from `/__lazybox/runners` when its
  server provides one (same-origin paths only); Vite alone does not, so a tab
  shows its one Runner.

**Not in step 2.** Missions and plans awaiting approval are the page's
Runner's only (the Inbox counts them for that Runner). A workspace window or a
detached chat of another Runner (the host opens windows on its own Runner).
Two app instances (Dev's and Working's) that both opted in to show each other
both notify for both; hide a Runner in one of them to stop that.

**Verification.** `scripts/runner-directory.test.ts` (preferences, health,
order, launch Runner, merged inbox keys and seen marks, targets, Agents
sections, peek keys); `runners.rs` tests (registered and added listed once,
running vs stopped by probe, the window's own without its bearer, endpoint
files others can read and non-loopback origins refused, locator vs endpoint
files); `attention.rs` (a destination's Runner is validated and read back);
`test:multi-runner` (two fake Runners at once plus a stopped one, WebKit, dark
and light: only the window's own Runner by default, the brand's "Dev", the
hint once and never again once dismissed, a hidden Runner never called, opting
in from the hint and from Settings; then grouping, versions and counts, merged inbox with badges, a pin, a
peek, an approval and a new task each reaching only their Runner, switching
pages from the sidebar, Agents and ⌘K, the Inbox popover badged, ⌘N's title and Runner menu (by mouse and
keyboard, Escape closing the menu only) with the message kept, the panel's +
naming its Runner, a section's + and an empty Runner's *Create one* starting on
that Runner only, a notification naming its Runner, one Runner going offline and coming
back, folding, Settings → Runners hide and primary, the primary opening the
window and a stopped Runner handing the page back).

`test:remote-computers` also lists a running local Working: left out while
the computer added over SSH shows.

**Not verified in the real desktop app:** the brand's channel read from
`AppInfo.channel` (Dev's and Working's bundle ids) when the listing has no
`current` row, `runner_list` against real Dev and
Working launchers and their locators, the HTTP capability added at run time
for a second origin (the code path is the one `adopt` already uses), a
notification click carrying a Runner through macOS, and the owner-only
check against a real Runner state directory. The Playwright suite runs the web
client against fake Runners.

## Step 3: Runners on other machines (owner level built over SSH; device level designed)

> **Superseded in scope by [Runners on other computers](REMOTE_RUNNERS.md)**:
> owner access to your own remote machines over SSH, the tailnet, a relay or
> the Hub, with per-client keys. The see-and-answer mode below becomes its
> *device* access level (P1).

**Built: the owner level over SSH** ([REMOTE_RUNNERS P0b](REMOTE_RUNNERS.md#p0b-the-owner-link-over-ssh-built),
guide [docs/guides/remote-computers.md](docs/guides/remote-computers.md)).
A computer added with Settings → Runners → *Add a computer* (or `lazybox
remote add <host>`) gets a locator in `~/.config/lazybox/runners/<name>.json`
with a `remote` field; its endpoint file is the link agent's loopback origin
and per-start token (`lazybox link connect <name>`, which the desktop keeps
running, `links.rs`). So everything in step 2 applies unchanged: its own
`RunnerLink` and renewal, `RunnerScope`, the merged inbox, notifications,
⌘K and ⌘N, and **it can be the page**, because owner access is the same as
local. What differs: the sidebar groups Runners by computer (route, RTT,
*Reconnecting…*, *Access revoked*); actions on this computer's own files
(Reveal in Finder, a home checkout's Finder and editor) are not offered on its
page, and the host refuses `OpenRepositoryHome` over a link; a folder picked
on this computer is not offered as a repository for it; Settings → Runners
shows its route, RTT and pinned key, and **Remove** revokes this computer's
key there. Its owner bearer never leaves it: the link authenticates this
computer's own key, and the host adds the bearer locally.

**Still designed, below:** a desktop at *device* level (see and answer) on a
Runner it does not own, through the device gateway.

A remote Runner is reached through **its device gateway**, as a phone reaches
it — never with its owner bearer, which must not leave its machine.

- **Pairing:** Settings → Runners → *Add a Runner on another machine* takes
  the pairing link the other machine's Settings → Devices shows under its QR
  code (*Copy link*, to add there). The desktop pairs itself exactly as a
  phone does (`client-core/device.ts`): a non-extractable WebCrypto P-256 key
  in the app's IndexedDB, the same `/remote/v1/pair`, challenge and session
  calls, the same 15-minute device credential. Grants are chosen on the other
  machine (read, respond, start); the remote section says them on its header
  (*Remote · See and answer*).
- **Transport:** Tailscale HTTPS, as for phones. A new host command
  (`runner_add_remote`) accepts only `https://<name>.<tailnet>.ts.net[:port]`
  origins and adds an HTTP capability for exactly that origin's
  `/remote/v1/*` and RPC path; the pairing link's secret never reaches the
  host or any log (it stays in the page, as on the phone).
- **What a remote section is.** The device allowlist (REMOTE.md) gives
  `ListCollections`, `Info` (redacted), history, sessions, commands' output and
  `Apply` for answers: enough for its Workspaces section, its rows in the
  merged Inbox and Agents, pins, peeks, and answering approvals and questions.
  It is **not** enough for a page: files, terminals, settings, processes,
  diffs-as-review and starting conversations are refused by design. So a
  remote Runner never becomes the page. Opening one of its conversations opens
  a *remote conversation* surface instead: the transcript, the pending request
  with its answer buttons, the composer if `respond` is granted — the phone's
  conversation view (`remote/*`) inside the desktop, scoped with `RunnerScope`.
  Everything else is greyed with "Not available on a remote Runner".
- **CLI:** `lazybox pair <link>` stores a device credential for the CLI with
  the same grants; `--runner work-laptop` uses it.

**What is missing before it can ship**, in order:

1. *Copy link* beside the pairing QR code (Settings → Devices), so a desktop
   can be paired without a camera.
2. `runner_add_remote` (origin validation, capability) and a remote listing
   kind (`RunnerListing.remote: { origin, grants, deviceId }`), stored by the
   app (no secret: the key is in IndexedDB, non-extractable).
3. A device-credential `renew` for `RunnerLink` (sign a fresh challenge; the
   phone's code, moved to `client-core`).
4. The remote conversation surface, and `ActiveRunner` refusing to make a
   remote Runner the page.
5. Grant limits in the UI: actions the device may not call are disabled with
   the reason, before a call is refused.
6. Tests: the fake gateway (`runner-ui.mjs`'s `remote`) behind a second
   origin; the allowlist proving the desktop calls nothing else.

Why it stopped at design: each of 2–4 is a security boundary (a new HTTP
capability, a credential lifecycle, a surface that must never reach for an
owner-only RPC), and shipping them without a real tailnet and a second machine
to verify against would put an unverified path in front of the person.

Open questions: a desktop's device key lifetime and rotation (the phone's has
none beyond revoke); whether a remote Runner may push notifications through the
local one (no — each Runner notifies its own windows; the desktop polls the
gateway's `Info` as the phone does).

## Not doing

- Proxying one Runner through another: each Runner stays the only authority
  over its machine.
- Sharing the owner bearer between machines, or a LAN listener.
