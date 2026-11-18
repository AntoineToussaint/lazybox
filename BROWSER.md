# Agent browser

Status: built, 2026-11-16. Agents in a coding conversation can open a
browser, read pages, click, type and take screenshots, while the person
watches every step live beside the chat and can take the browser over or stop
it at any moment. Driving other Mac apps is designed, not built
([computer control](COMPUTER_CONTROL.md)).

Until now agents had only the provider's own WebFetch/WebSearch: they could
read a URL's text, never use a site (sign in to a staging app, click through a
flow, check what a page actually renders). The agent browser closes that gap
without handing an agent the person's browser, cookies or network.

## What the person sees

- **A badge in the chat header** — *Claude is browsing* (with a live dot),
  *You're browsing* after Take over, *Waiting for you* while a request
  waits, *Browser stopped* after Stop.
  Clicking it shows the browser.
- **The Agent browser panel**, docked beside the chat (Settings → Browser →
  *Show the agent browser beside the chat* opens it automatically the first
  time an agent opens its browser; it is an ordinary panel: closable, movable,
  resizable). It shows the page live, a cursor that moves to the element the
  agent acts on and a highlight around it, the page's origin and title (a lock
  for https), and an **action log** strip underneath: *Opened docs.example ·
  Read the page · Clicked button "Sign in" · Typed into "Search" · Took a
  screenshot*, each with its time and outcome (running, done, failed, blocked).
  Clicking an entry flashes where it happened.
- **Take over (T) / Give back (G).** Take over pauses the agent: its next
  browser call waits. The person browses with their own mouse and keyboard in
  the panel, with an address bar and back/forward/reload. Give back resumes
  the agent, which is told the person used the browser and to read the page
  again before using element refs.
- **Stop.** Closes the page and refuses the agent's browser calls ("the
  person stopped your browser; do not use it again unless they ask") until the
  person chooses *Let Claude browse again*. Stop also refuses every request
  still waiting.
- **Request cards** in the conversation, like approvals, and in the Inbox:
  - *Claude wants to open docs.example* — the URL, the agent's reason, and
    **Allow once** (y) / **Allow for this conversation** (c) / **Always allow
    in this workspace** (a) / **Deny…** (n, with a reason the agent is told).
    In full access (YOLO) the card says that full access still asks before a
    new site.
  - *Claude wants to download report.pdf* — **Allow this download** / **Deny…**.
  - *Claude wants to sign in on example.com* — before the agent submits a
    form holding a filled password field — **Allow this sign-in** / **Deny…**.
- **Browse freely in this conversation** (panel menu): no site asks for the
  rest of the conversation. Hard blocks still hold.
- **Settings → Browser**: when agents cannot browse, why and the fix in one
  click — *Chromium isn't installed* with **Install Chromium** (a developer
  build: the install runs as a Runner command whose output shows there; once
  it is in, agents' browsers open without a restart), *The agent browser is
  off* with **Turn on**, or why this Runner has no browser at all. Then the
  default permission (*Ask before each new site*, recommended; *Allow any
  site*; *Off* — agents get no browser), sites any agent may open without
  asking (`example.com`, `*.example.com`), downloads (*Ask* / *Block*), the
  panel preference, and per workspace (every workspace is listed) a
  **Browser** switch (off: its agents get no browser), its always-allowed
  sites (removable) and **Clear browser data…** (sign-ins, cookies, storage,
  cache; closes that workspace's agent browsers).
- **Give Claude the browser** (the conversation's ⋯ menu and header menu,
  `/browser`, ⌘K) when its agent has no browser: it started before the
  service was there, or while the browser was off for its workspace. It turns
  the browser back on for that workspace and, for an agent started without
  the tools, restarts it between turns, the provider conversation resumed
  (as a model change that cannot apply live does). `Session.browser_access`
  says which: `has`, `can_give`, `off`, or empty where it does not apply.
- **Preview :port** in the live bar and a command's console, for a server a
  `lazybox run` listens on: its page in a Web panel, no workspace action
  needed.

Everything an agent does in the browser is in the transcript as tool calls
(*Opened docs.example*, *Clicked “Sign in”*, *Took a screenshot* with the
image the agent received), in the action log, and — for asks — as cards with
the person's answer.

## What agents get

Thirteen tools, the same for Claude (`mcp__lazybox_browser__browser_*` on the
worker's in-process MCP server, `workers/claude-sdk/src/browser.ts`) and Codex
(dynamic tools, `internal/runner/adapters/codex/tools.go`); the canonical list
is `agent.BrowserTools` (`internal/agent/browser_tools.go`):

| Tool | Does |
| --- | --- |
| `browser_open(url?, reason?)` | Opens the browser, optionally at a URL; reopens the last page after the browser closed underneath the agent. |
| `browser_navigate(url, reason?)` | Goes to a URL. The first visit to a site asks the person, with the reason. |
| `browser_snapshot(maxChars?)` | The page as a compact accessibility tree with element refs (`[ref=e12]`), capped at 8000 characters by default. |
| `browser_click(ref, element?)` | Clicks an element; `element` names it for the person ("Sign in button"). |
| `browser_type(ref, text, submit?, element?)` | Fills a field; `submit` presses Enter. Never into a password field. |
| `browser_select(ref, values, element?)` | Chooses options. |
| `browser_press(key)` | Presses a key. |
| `browser_scroll(direction?, amount?, ref?)` | Scrolls, or scrolls an element into view. |
| `browser_screenshot(fullPage?)` | A JPEG, attached to the agent's context as an image. |
| `browser_back()`, `browser_forward()` | History. |
| `browser_wait_for(text?, textGone?, ref?, timeoutSeconds?)` | Waits up to 60s. |
| `browser_close()` | Closes the browser. |

Results are compact like `lazybox run`: a line saying what happened, the
page's title and URL, and after anything that changes the page a capped
snapshot — never raw HTML. Things that happened besides the step (a download
saved, a request refused, a popup folded into the page) are added as *Also:*.

The tools need no per-call approval in any permission mode (Claude: an allow
rule for `mcp__lazybox_browser`; Codex: no approval title): the browser asks
the person per site, download and sign-in itself, and a prompt per click on
top would teach them to click through. In a **read-only** or Plan
conversation the agent may open, read, scroll and screenshot pages, never
click, type, select or press keys.

**Who gets it.** Every new Claude or Codex coding conversation, whenever the
Runner has the service, unless Settings → Browser is *Off* or the browser is
off for the conversation's workspace (ACP agents bring their own tools;
assistants get none). A missing Chromium does not withhold the tools:
installing it makes them work without restarting anyone, and until then the
agent is told to ask the person to install it from Settings rather than
install it itself. Switching the browser off later refuses the tools of agents
already running. The tools are fixed when the agent starts (the Claude
worker's MCP servers and permission rule are options of its query; Codex's
dynamic tools are given at `thread/start`), and an agent outlives Runner
restarts (session hosts), so one started before the service was available
keeps having none: the Runner records what each agent was started with
(`Record.BrowserTools`; for an older record, the last tool list the agent
reported), and offers **Give Claude the browser** for it.

**The briefing** has a browser section of its own, per conversation
(`browser_briefing.go`): *You have a browser* — the `browser_*` tools, use
them rather than a headless browser of your own, the person watches in the
Agent browser panel — or why it has none (it started before the browser was
available, or the browser is off for the workspace or everywhere) and how the
person gives it one. When that changes, the agent hears the section as it now
reads with its next message, like any change of its briefing
([COMMAND_OUTPUT.md](COMMAND_OUTPUT.md#the-briefing-and-what-running-conversations-hear-when-it-changes)).

## The engine, and why

The browser is **Playwright's Chromium (headless shell), driven by a small
Node service under the Runner** (`workers/browser`), streamed to the panel as
screencast frames drawn on a canvas.

- **Fidelity.** It is a real Chromium: the sites agents need (staging apps,
  dashboards, docs behind sign-in) behave as they do for the person, with
  JavaScript, cookies and modern layout. Playwright's accessibility snapshot in
  "ai" mode gives element refs an agent can act on (`page.getByRef`), which is
  far cheaper in tokens than screenshots and more robust than CSS selectors.
- **Isolation.** It is never the person's Chrome and never their profile. Each
  workspace has its own persistent profile under
  `<state>/browser/profiles/<workspace>`, so a sign-in made in a workspace stays
  there and nowhere else (*Clear browser data* deletes it). Downloads go to
  `<state>/browser/downloads/<conversation>` and are deleted with the
  conversation. The service's environment carries no credentials (PATH, HOME
  and Playwright's own variables only).
- **Tauri's limits.** The Web panel's native child webviews (`viewer.rs`) are
  for the person: WKWebView exposes no accessibility tree with refs, no
  network interception, no download control and no headless mode, and it lives
  in the desktop app, so an agent could not browse while the window is closed,
  from a phone, or on a remote Runner. The Runner-side browser works the same
  in all of them; the panel only draws frames, so the phone or a browser tab can
  show it too.
- **CI testability.** Headless Chromium runs on any CI host. The service has
  its own tests against local fixture sites
  (`node --import tsx --test workers/browser/src/*.test.ts`), the Runner's
  policy is tested against a fake service, and one Go test drives the real
  service end to end.

`make browser` installs the Chromium build matched to the pinned
`playwright-core` into Playwright's own cache. Without it, opening the browser
fails with that instruction, in the panel and to the agent.

## Lifetime and ownership

The service is a child of the Runner (`internal/runner/browser`), started on
the first browser an agent opens, under the same process guard as any
Runner-owned process; Chromium is its descendant. One profile is one
persistent browser context; one conversation's browser is one page in its
workspace's context.

**The agent browser does not outlive the Runner.** Agents, their commands and
terminals do ([session hosts](SESSION_HOSTS.md)); a browser is a tool the
agent uses, and its state that matters survives anyway: the profile (sign-ins)
is on disk, the Runner keeps each conversation's last URL, grants and
*browse freely* in `<state>/browser/conversations.json`, and the agent's next
browser call after a restart reopens the page where it was. A call in flight
when the Runner stops gets an explicit outcome-unknown answer (reading tools
are simply run again). Hosting the service in a session host is possible
(it speaks JSON lines) but would push every frame through the host's durable
buffer; it is not worth it until browsing sessions need to survive restarts
mid-form.

## Safety

**Who decides.** The Runner owns policy; the service owns Chromium and the
hard network rules. An agent reaches only its own conversation's browser: the
Runner derives the conversation from the calling execution, never from the
tool's arguments. Every browser RPC (`WatchBrowser`, `ControlBrowser`,
`SendBrowserInput`, `DecideBrowserRequest`, the settings and Clear browser
data) is the owner's; agent and terminal credentials are refused them
(`auth.go`, walked exhaustively by
`TestAgentCredentialIsDeniedEveryRPCOffTheAllowlist`, and explicitly by
`TestBrowserRPCsAreTheOwnersAndAnAgentDrivesOnlyItsOwnConversation`). A paired
phone sees the badge and the waiting request through `GetSession`, but cannot
yet answer it, take over or stop (not on the device allowlist; see below).

**New sites ask.** The first main-frame navigation to an origin
(scheme://host:port) the agent has not been allowed asks the person — whether
the agent typed the URL or clicked a link, and **in full access (YOLO) too**,
unless the site is on the allow-list, always allowed in the workspace, allowed
for the conversation, or the person chose *browse freely* (or *Allow any
site*). *Allow once* covers that visit: same-origin pages are not asked again,
and coming back after visiting another site asks again. Sites the person
navigates to during Take over count as their choice. A request no one answers
is refused after 10 minutes; at most 8 wait per conversation.

**What a page may never reach**, whoever drives it and whatever the person
answered: anything but http(s)/ws(s) (and data:/blob: inside a page) — so no
`file:`, no `chrome:`; this computer (loopback) except the ports of the
workspace's running previews (the workspace runtime's actions) and of the
servers its running commands listen on (a `lazybox run` of its agents, or a
run from its terminals); the Runner's
own address and the other host:port pairs it lists; private, link-local,
CGNAT (Tailscale) and unique-local addresses, including names that resolve to
them. The service enforces this in a loopback proxy every connection goes
through, which vets the resolved address and connects to that address, so a
redirect, a WebSocket, a service worker or DNS rebinding cannot get around it;
request interception only attributes and explains refusals in the log. The
Runner checks the same rules before asking, so a blocked URL is refused
without a card.

**Credentials.** The agent may not type into a password field: it is told to
ask the person to Take over and type it. Submitting a form that holds a filled
password field (filled by the person, or by the profile) asks first (*sign
in*). What the person types during Take over goes to the page only; the agent
sees the page afterwards like any page, never the keystrokes.

**Downloads** ask (or are blocked, per Settings); an allowed file is saved
under the conversation's downloads folder and its path given to the agent.
**Popups** are folded into the conversation's page, through the same gate;
**dialogs** (alert, confirm, prompt) are dismissed and logged.

**Limits.** One page per conversation; navigation times out after 30s, an
action after 2 minutes, `wait_for` after 60s; screenshots are JPEG and a full
page is capped in height; snapshots are capped (40 000 characters at most);
the action log keeps the last 300 entries; frames flow only while a panel
watches and a slow watcher gets the latest frame, never a backlog.

## Protocol and code

- `workers/browser/src/protocol.ts` — the Runner ⇄ service protocol (JSON
  lines over stdio, the Claude worker's framing): `open`, `act`, `input`,
  `screencast`, `close`, `close_profile`; the service asks `site`, `download`,
  `sign_in`, and emits `frame`, `state`, `log`, `closed`.
- `internal/runner/browser` — policy, requests, Take over, Stop, the log and
  watchers; `internal/runner/browser.go` connects it to executions and
  workspace previews; `internal/transport/browser.go` the RPCs.
- Proto: `WatchBrowser`, `ControlBrowser`, `SendBrowserInput`,
  `DecideBrowserRequest`, `Get/SaveBrowserSettings`, `ClearBrowserData`;
  `Session.browser`; a waiting request is a `Session.pending` entry of type
  `browser_request` whose provider data is a `BrowserRequest`.
- UI: `packages/surfaces/src/BrowserPanel.tsx`, `BrowserRequestCard.tsx`,
  `BrowserSettingsPanel.tsx`, `browser-model.ts`, `useBrowser.ts`.
- Launch: `LAZYBOX_V2_BROWSER_WORKER` names the built service
  (`scripts/runner-command.mjs`, `make run`, `make test`); `make build-worker`
  builds it with the Claude worker. Unset, agents get no browser and Settings →
  Browser says why.
- Availability (`internal/runner/browser/availability.go`): the service's
  `playwright-core` (found beside it, as Node resolves it) pins a headless
  shell revision; it is installed when Playwright's cache
  (`PLAYWRIGHT_BROWSERS_PATH`, else the platform cache) holds
  `chromium_headless_shell-<revision>/INSTALLATION_COMPLETE`. Without a
  `playwright-core` beside the service the Runner does not guess.
  `InstallBrowser` runs `node <playwright-core>/cli.js install --only-shell
  --no-remove chromium` (what `make browser` runs, without its `npm ci`) as a
  `runner` command, with the service's environment.
- Proto: `GiveBrowser`, `InstallBrowser`; `Session.browser_access`;
  `BrowserSettings.availability`, `installable`, `install_command_id`;
  `BrowserWorkspaceSites.off`.

## Tests

- Service (`npm run test:browser-service`): navigate, snapshot refs, click,
  type and submit, select, press, back/forward, wait_for, screenshots; the site
  gate for typed URLs and clicked links; blocked schemes, loopback ports,
  private addresses, the Runner's address, a redirect and a page's fetch to a
  refused port; downloads denied and allowed; password typing refused; sign-in
  asks; popups, dialogs, screencast frames, Take over input; the stdio protocol.
- Runner (`go test ./internal/runner/browser`): asks and answers (once,
  conversation, workspace, deny with a reason), grants across a restart, YOLO
  still asking, allow-list and browse freely, blocked URLs without a card,
  navigation the service asks about, read-only, Take over pausing and Give
  back resuming, Stop and Resume, watchers' history, frames and boxes,
  screenshots, downloads, Off, settings validation, Clear browser data; and
  `TestRealServiceNavigatesReadsClicksAndShowsFrames` with the real service.
- Transport: the RPCs are the owner's; an agent's request lands on its own
  conversation; Stop is per conversation; settings round trip.
- Adapters: Codex offers the tools without approval and returns screenshots
  as images; the Claude worker exposes and routes all thirteen tools, returns
  screenshots as images and allows them without a prompt.
- Availability and access (`availability_test.go`,
  `internal/runner/browser_access_test.go`, `transport/browser_test.go`):
  ready, Chromium missing (with its install command), off, no service; off
  for one workspace; a new conversation gets the tools and the briefing says
  so; Give the browser restarts an agent started without it (not mid-turn) and
  the restarted agent hears it has it; a re-attached agent keeps what it
  reported; Install Chromium runs as a Runner command; the RPCs are the
  owner's.
- UI (`npm run test:agent-browser`) also: *Give Claude the browser* in the ⋯
  menu; **Preview :5173** in the live bar opening the Web panel, and in the
  console; Settings → Browser with Chromium missing (Install Chromium, its
  output), off (Turn on) and one workspace off.
- UI (`npm run test:agent-browser`, fake Runner): badge, auto-show, live
  frames, action log, cursor and highlight, request cards and their keys,
  Take over input and Give back, Stop and Let Claude browse again, Settings →
  Browser, a narrow window.

## Not built, not verified

- **Not verified:** a real Claude or Codex agent browsing end to end (the
  adapters are tested at their boundaries); Codex's handling of an
  `inputImage` content item in a dynamic tool reply; public HTTPS sites and
  real DNS (the tests are loopback only); IME during Take over.
- A phone cannot yet answer a browser request, take over or stop the browser
  (it shows that the agent is browsing and that a request waits). Adding
  `DecideBrowserRequest` (minus *Always in this workspace*) and Stop to the
  device grants is the next step.
- The browser does not outlive the Runner (above).
- Giving a running agent the browser restarts it. The Claude SDK could add
  an MCP server live (`setMcpServers`), but the browser's permission rule is
  an option of the query too, Codex has no live equivalent, and the agents
  that lack the tools are exactly those whose worker predates them; a
  restart between turns, conversation kept, covers all of them.
- The installed release (`lazybox-runner serve`) does not ship the service or
  its Chromium yet, so its agents get no browser unless `--browser-worker`
  (or `LAZYBOX_V2_BROWSER_WORKER`) names a built service; Dev and Working run
  it from the checkout.
- Computer control: [COMPUTER_CONTROL.md](COMPUTER_CONTROL.md).
