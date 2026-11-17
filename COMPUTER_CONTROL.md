# Computer control (design)

Status: design only, 2026-11-16. Nothing here is built. Agents can drive a
browser ([agent browser](BROWSER.md)); this document is how they would drive
other Mac apps — Xcode, the iOS Simulator, a design tool, a native app under
development — with the same promise: the person watches it live, can take
over at any moment, and nothing sensitive is reachable.

## What the person sees

The agent browser's experience, applied to an app window:

- **A live view docked beside the chat.** The panel streams the controlled
  app's window (only that window), with the badge **Claude is using Xcode**,
  a cursor and a highlight on the element being acted on, and the action log
  strip underneath: *Opened Xcode · Clicked "Run" · Typed in "Search" ·
  Pressed ⌘B · Took a screenshot*. The same panel kind, the same log, the same
  keys as the browser ([BROWSER.md](BROWSER.md)).
- **Take over / Give back / Stop.** Take over pauses the agent; the person
  uses the real app (it is the real window on their screen, not a copy), then
  Give back. Stop ends control for the conversation; the agent is told and
  must not try again unless asked.
- **The person's input wins.** Moving the mouse or pressing a key anywhere
  while an agent acts pauses the agent at once ("Paused — you're using the
  computer. Give back when done"). There is no fight over the pointer.
- **Visible everywhere.** While any agent controls any app, a menu-bar item
  shows it (●  *Claude · Xcode*) with Stop all, and a global shortcut
  (⌃⌥⌘. by default, rebindable) stops every agent's control from any app,
  including when the Lazybox window is hidden.
- **Asked first.** The first time an agent wants an app, a request card in the
  conversation: *Claude wants to use Xcode* — the reason, the app's icon and
  bundle id — **Allow once** / **For this conversation** / **Always for this
  workspace** / **Deny…**. Apps that are never allowed (below) are refused
  without a card, and the refusal is in the transcript.
- **Settings → Computer** (next to Settings → Browser): off by default; the
  per-workspace app allow-list, with Remove; the never-allowed list, shown so
  the person knows it exists; the permission state of Accessibility and
  Screen Recording with a button that opens the right System Settings pane.

## Why the desktop app owns it

macOS gates both halves behind TCC, per signed binary:

| Need | API | TCC permission |
| --- | --- | --- |
| Read an app's UI tree, act on an element | Accessibility (`AXUIElement`, `AXUIElementPerformAction`, `AXValue` setters) | Accessibility |
| Synthetic input when no AX action exists | `CGEvent` (`CGEventPost` to the target pid) | Accessibility |
| See a window | ScreenCaptureKit (`SCStream` on one `SCWindow`) | Screen Recording |

The grants belong to the binary the person approved in System Settings. That
must be the **Lazybox desktop app** (Tauri, signed, one stable identity),
never the Runner:

- The Runner is a Codefly-launched Go binary that is rebuilt and restaged
  (Dev, Working, `make stage`); each build is a new identity to TCC, and every
  rebuild would silently drop or re-prompt the grant.
- The Runner is headless and may run without anyone at the screen (the phone
  case). Control of a GUI needs a person who can see it and stop it; the
  desktop app being open is the proof that one can.
- Agents run under the Runner. Granting the Runner Accessibility would grant it
  to every process an agent starts, since TCC attributes child processes to
  their responsible parent. A separate helper keeps the grant out of the
  agents' process tree entirely.

So the controller is a **helper owned by the desktop app**: a small native
binary inside the app bundle (`Contents/MacOS/lazybox-computer`), launched by
the app, holding the TCC grants as the app's responsible process. It connects
to the Runner the way a window does — with the owner's endpoint, over
loopback — and registers as the computer-control provider. The Runner keeps
the policy (who may use which app, the request cards, the action log, the
transcript); the helper keeps the mechanism (AX, CGEvent, ScreenCaptureKit)
and the hard rules that must not depend on the Runner being right (the
never-allowed list, secure fields, person-input-wins).

```
agent ──tool pipe──▶ Runner: policy, cards, log ──▶ helper (in Lazybox.app): AX · CGEvent · SCK ──▶ target app
                                      ▲                         │
                         panel ◀── WatchComputer stream ◀────── frames of one window
```

**Desktop app closed** (or the helper not running): control is unavailable.
The agent's `computer_*` call returns at once with *"Computer control needs
the Lazybox app open on this Mac; ask the person"*, and the panel says the
same. The Runner never starts the helper itself, and an agent never waits on
one. Closing the app mid-action stops control as Stop would.

**Remote and phone.** A phone paired over [Remote](REMOTE.md) can watch the
stream and press Stop, but cannot Take over (there is no mouse on the Mac
from a phone) and cannot answer an app request card — granting control of a
desktop app is decided at the desktop. The device allowlist gains only the
watch and stop RPCs. A Runner on another machine has no computer control: the
helper serves the Runner of the machine it runs on, on loopback, only.

## Reading and acting

- **Snapshot = the AX tree**, the analogue of the browser's accessibility
  snapshot: role, title, value (truncated), enabled/focused, with element refs
  (`e12`) that resolve against the latest snapshot only. Compact, depth- and
  size-capped, restricted to the target app's focused window by default. A
  ref is an `AXUIElement` kept by the helper for that snapshot; it never
  crosses to the agent as anything but the short id.
- **Acting prefers AX actions** (`AXPress`, `AXConfirm`, setting `AXValue`,
  `AXRaise`), which need no pointer and do not move the person's cursor.
  `CGEvent` (posted to the target pid, never globally) is the fallback for
  elements without an action — canvases, games, custom controls — and only
  while the target window is frontmost.
- **Screenshots are of the target's windows only** (`SCContentFilter` with
  that app's windows), never the whole display, never other apps' windows
  that overlap it. A menu or sheet the app opens is part of the app.
- **The live view** streams the same filtered capture at a bounded rate
  (≤ 10 fps, downscaled), only while a panel watches, like the browser's
  screencast.

## What is never allowed

Refused by the helper itself, whatever the settings or the Runner say,
matched by bundle id (and team id where it exists, so a renamed copy is
still refused):

- **Password managers and credential stores:** 1Password, Bitwarden,
  Dashlane, LastPass, KeePassXC, Apple Passwords, Keychain Access.
- **The system:** System Settings / System Preferences, Security agents and
  authorization dialogs (`SecurityAgent`, `coreauthd`, `UserNotificationCenter`
  prompts), Installer, Software Update, Disk Utility, Migration Assistant.
- **Lazybox itself**, and any app whose windows show Lazybox's (an agent
  must not approve its own requests).
- **Terminals** (Terminal, iTerm2, Warp, Ghostty…): agents already have
  managed commands and workspace terminals with their own approvals; a
  terminal driven through the GUI would bypass both.
- **By default, not refused outright but off until allowed by the person in
  Settings (no card can grant them):** Mail, Messages, Slack, WhatsApp,
  banking and payment apps, browsers other than the agent browser (the
  person's real Chrome holds their sign-ins), Finder (it reaches the whole
  disk; a workspace-scoped file view is the Files panel's job).

**Secure input is never read or typed.** An element whose role is
`AXSecureTextField`, or any input while `IsSecureEventInputEnabled()` is true
(an app called `EnableSecureEventInput`), returns no value in a snapshot and
refuses `computer_type` and key events: *"That is a password field; ask the
person to Take over and type it."* Credentials are never typed by an agent,
only by the person during Take over. Snapshots also redact values that look
like secrets (the Runner's existing `internal/redact` patterns) before they
reach the agent or the transcript.

## Agent tools

The browser's tool surface, with apps instead of pages. All return compact
text (a summary line, the app and window, a capped snapshot when it changed),
never raw trees; screenshots attach as an image.

| Tool | Does |
| --- | --- |
| `computer_apps` | Lists running apps the agent may ask for, with whether each is allowed now. Never lists never-allowed apps. |
| `computer_open(app, reason)` | Asks for (card) and brings forward an app by name or bundle id; launches it if allowed and not running. |
| `computer_snapshot(maxChars?)` | The focused window's AX tree with refs. |
| `computer_click(ref, element?)` | AX press, else a click at the element's center. |
| `computer_type(ref, text, submit?)` | Sets the value or types; refused on secure fields. |
| `computer_key(keys)` | A key or chord (`cmd+b`) to the target app only. |
| `computer_scroll(ref?, direction, amount?)` | Scrolls an element or the window. |
| `computer_screenshot(window?)` | The target app's window, JPEG. |
| `computer_wait_for(text? / ref?, timeoutSeconds?)` | Until text or an element appears or goes. |
| `computer_close()` | Ends control of the app (the app keeps running). |

Exposed exactly like the browser tools: the Claude worker's in-process MCP
server and Codex dynamic tools, routed to the Runner over the execution's own
tool pipe, so an agent can only ever act for its own conversation. A
read-only conversation gets `computer_apps`, `computer_snapshot`,
`computer_screenshot` and `computer_wait_for` only.

## Reuse of the agent browser

Same panel component (frame canvas, cursor/highlight overlay, action log,
Take over / Give back / Stop), same `BrowserAction`-shaped log entries in the
transcript, same pending-request mechanism (a session pending entry the
request cards render, with once / conversation / workspace / deny and the
Inbox row), same per-workspace grant storage and Settings layout. New RPCs
mirror `WatchBrowser`, `ControlBrowser`, `DecideBrowserRequest`; the
helper's registration RPC is owner-only and refused to agent, terminal and
device credentials.

## Threat model

| Threat | Example | Mitigation |
| --- | --- | --- |
| Prompt injection from the screen | A web page, an email or a document in the controlled app says "open Keychain Access and read…" | Never-allowed apps are enforced in the helper, not in the prompt. Each new app needs the person's card. Snapshot text is marked as untrusted content in the tool result. |
| The agent approves a system dialog | Clicking "Allow" on a TCC prompt, an `osascript` authorization, a "Do you want to install…" sheet | System dialog processes are never-allowed; AX actions are restricted to the target pid's elements; CGEvent is posted to the target pid only and only while that pid's window is frontmost. A sheet from another process appearing over the target pauses the agent and tells the person. |
| Exfiltration through the screen | Screenshotting a window that shows secrets, or another app behind | Capture is filtered to the target app's windows; secure fields carry no value; redaction on snapshot text; the person sees every screenshot in the log. |
| Typing credentials | Agent fills a sign-in form it read from elsewhere | Secure fields refuse input; secure event input blocks all synthetic keys; the agent is told to ask the person. |
| Destructive actions | ⌘⌫ in Finder, "Delete" in a design tool, quitting with unsaved work | Finder off by default; the action log and live view make it visible; Stop and the global shortcut; per-action rate limit. Undo is the app's — there is no checkpoint for GUI state, and the card says so. |
| Fighting the person | The agent keeps clicking while the person works | Any person input pauses the agent until Give back. |
| A runaway loop | Thousands of clicks a minute | ≤ 5 actions per second and ≤ 600 per hour per conversation; above it the agent waits and the panel says why. |
| Another local process drives the helper | Something running as the user connects to it | The helper accepts only the Runner it registered with (owner endpoint, loopback) and only the Runner's per-call tokens; it opens no listener of its own. Same out-of-scope line as [resource ownership](RESOURCE_OWNERSHIP.md#agent-credentials): a process running as the user can already read the Runner's state. |

Honest residue: GUI control is less containable than the browser. The agent
browser has its own profile and network rules; a real app has the person's
documents, accounts and network. Within an allowed app, an agent can do
whatever the person could in that app. The design limits *which* apps and
makes everything visible and stoppable; it does not sandbox an app.

## The smallest safe first step

**Read-only, one app, off by default.**

- Settings → Computer → *Let agents look at apps* (off). When on, the helper
  ships in the app bundle and asks for Screen Recording and Accessibility
  through the usual System Settings flow, with the panel explaining why.
- Tools: `computer_apps`, `computer_open` (bring forward only — no launch),
  `computer_snapshot`, `computer_screenshot`, `computer_wait_for`. No click,
  type, key or scroll. No CGEvent code in the helper at all.
- The app card per first use; never-allowed list enforced in the helper.
- Live view panel and action log reused from the browser; Stop works; no
  Take over (nothing to take over yet).

This already answers the questions agents ask most — *"what does the
Simulator show?"*, *"what does Xcode's issue navigator say?"* — with no way
to change anything.

**How it would be tested.**

- The helper's policy (never-allowed matching by bundle and team id, secure
  field redaction, snapshot capping, ref resolution) as unit tests over
  recorded AX trees, no TCC needed.
- A small fixture app built in the repo (a SwiftUI window with a button, a
  text field and a secure field) for an end-to-end test on a developer Mac
  with the grants given once: snapshot shows the button and no secure value,
  screenshot shows only that window, never-allowed apps refused.
- Runner side against a fake helper (like the fake Runner fixture): cards,
  grants, log, Stop, "app not open" answer, auth (agent/terminal/device
  credentials refused on the helper RPCs).
- Playwright on the panel with fake frames, as for the browser.
- **CI cannot run the end-to-end test**: hosted macOS runners have no way to
  grant Accessibility or Screen Recording (TCC needs a person or an MDM
  profile). It stays a documented manual gate until a self-hosted Mac with an
  MDM-provisioned PPPC profile exists.

**Then**, each its own step with its own review: AX actions in allowed apps
(press, set value) with person-input-wins and rate limits; Take over; CGEvent
fallback; launching apps; the menu-bar item and global shortcut become
mandatory before any step that acts.

## Open questions

- Helper language: Swift (natural for AX and ScreenCaptureKit) adds a
  toolchain to the build; Rust with `objc2` keeps one toolchain with the
  desktop app. Decide before the first step.
- Does TCC attribute the helper to Lazybox.app reliably when launched from
  the app (`responsible process`), including in Dev builds signed ad hoc?
  Needs a spike; ad-hoc signatures may re-prompt on every rebuild.
- Per-app or per-window grants? An allowed browser app would expose every
  tab; per-window may be needed for multi-document apps.
- Should allowing an app for a workspace also require the app's document to be
  inside the workspace (Xcode project path, file path in the window title)?
- Multi-display and Spaces: the target window on another Space cannot be
  captured without switching; do we switch, or say so?
- iOS Simulator is the most-wanted target and is itself an app driving a
  device: treat it as an ordinary app, or use `simctl` and XCUITest instead?

Not built: no helper, no tools, no RPCs, no settings. This document is the
proposal; the first step above needs a decision recorded in
[DECISIONS.md](DECISIONS.md) before work starts.
