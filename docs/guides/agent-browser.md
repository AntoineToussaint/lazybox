# Let an agent use a browser

A coding agent can open its own browser to read docs, check a staging app or
click through a flow — and you watch every step beside the chat. It is a
separate Chromium, never your Chrome: it has none of your sign-ins, and the
ones made in it stay in its workspace.

**For now the agent browser is a developer-build feature.** The installed
Lazybox app does not include it yet: Settings → Browser says so, and an agent
that tries to browse is told it cannot. To use it, run Lazybox from source
and start it with `make dev` or `make working`. If its Chromium
is missing, Settings → Browser says so and **Install Chromium** installs it
in one click, its output shown there (or run `make browser`).

Every new Claude or Codex conversation gets the browser, unless you turned it
off (Settings → Browser, everywhere or per workspace). Its agent is told so
in its briefing, and told to use it rather than a headless browser of its own
that you cannot see.

## When the agent says it has no browser

A conversation keeps the tools its agent started with. One that started
before the browser was available (or while it was off for its workspace) has
none, and its agent says so. Choose **Give Claude the browser** from the
conversation's **⋯** menu, `/browser`, or ⌘K: the agent restarts with the
browser between turns and the conversation continues where it was. The same
choice turns the browser back on for a workspace you had turned it off for.

## Watch it

When the agent opens its browser, the chat header says **Claude is browsing**
and the **Agent browser** panel opens beside the chat (turn that off in
Settings → Browser). The panel shows the page live, a cursor and highlight on
what the agent clicks or types into, and an action log underneath. Click a log
entry to see where it happened.

## Answer its requests

The first time the agent goes to a site, it asks — with its reason — even in
full access:

| Key | Choice |
| --- | --- |
| `y` | Allow once — this visit to the site |
| `c` | Allow for this conversation |
| `a` | Always allow in this workspace |
| `n` | Deny…, with a reason the agent reads |

A redirect to another site asks before that site is contacted. Downloads and
submitting a sign-in form ask too, however the agent would submit it. The
agent never types or sees passwords: when it needs one it asks you to take
over, and a password you type is hidden from its snapshots and masked in its
screenshots. Sites you go to yourself while you hold the browser become the
agent's to stay on; a page that moves itself to another site meanwhile does
not.

To stop being asked in one conversation, choose **Browse freely in this
conversation** from the panel's menu.

## Take over, give back, stop

- **Take over** (`T`) pauses the agent. Use the page yourself — click, type,
  use the address bar — for example to sign in.
- **Give back** (`G`) lets the agent continue; it reads the page again first.
- **Stop** closes the browser and stops the agent from using it until you
  choose **Let Claude browse again**.

## Check the app you're building

The agent's browser can open the app the workspace runs: an action's preview,
or a dev server the agent (or you) started with `lazybox run`. You see the
same page with **Preview :5173** in the live bar or the command's console.
For a one-click Run and Preview, define the action once
([Run your app](run-your-app.md#a-server-an-agent-starts-preview-port)):

```yaml
# .lazybox/workspace.yaml
actions:
  web:
    run: npm run dev -- --port $LAZYBOX_PORT
    url: http://localhost:$LAZYBOX_PORT
    default: true
```

## What it can never reach

Files on your computer, Lazybox itself, other programs on your computer (only
this workspace's running previews and the servers its commands run), and
addresses on your private network. These hold whatever you allow.

## Settings → Browser

- **Why agents cannot browse**, when they cannot, with the fix: **Install
  Chromium** when it is missing, **Turn on** when it is off.
- **Default:** ask before each new site (recommended), allow any site, or off.
- **Sites any agent may open without asking**, such as `docs.python.org` or
  `*.example.com`.
- **Downloads:** ask or block.
- Per workspace: a **Browser** switch (off: its agents get no browser), its
  always-allowed sites, and **Clear browser data…** to delete its sign-ins,
  cookies and cache.

How it is built and why: [Agent browser](../../BROWSER.md).
