# Extensions: panels you add

An extension is a panel you write in HTML, JavaScript and CSS — a board of
your agents, a cost dashboard, a view of your own service — and keep in a
folder. It can also be an [account policy](#account-policies) with no panel at
all, such as *Subscription rotation*. A panel runs in a sandbox, apart from Lazybox, and can read and do **only
what you allow** it, which Lazybox shows you in words before it runs. It cannot
read your files, terminals or messages, run commands, change a setting, see a
credential, or reach any site you did not allow.

Clone Me and Compound Engineering are background workflows run by the Runner.
Open **Extensions** in the main navigation to browse installed and available extensions together. Search by name or purpose, filter by category or installation status, and select a thumbnail to enlarge its preview. After approval, choose
**Teach Clone Me** to assign interview answers to code style, verification, or decisions that need your input. Compound Engineering checks applicable repository instructions and publishes proposed edits with citations to the archived conversation in workspace Artifacts. Use **Archive workspace…** in the workspace menu or **Archive conversation** to trigger a review.

## Try one

1. Open **Extensions** in the main navigation, or **Panels → New Extensions panel** to browse the same catalog beside your conversation.
2. Find *Agent board* or *Cost dashboard* and choose **Install**.
3. Read the sheet: what it can see, what it can do, where it can connect.
   **Allow and turn on** — or **Not now**.

| Extension | What you can do |
| --- | --- |
| Agent board | Search conversations or select **Needs attention only**. Open a card to answer the agent. Use **Older conversations** to browse earlier history. |
| Clone Me | Choose **Teach Clone Me** to review and save coding preferences for your agents. |
| Compound Engineering | Archive a conversation or workspace, then review the proposed documentation edits in Artifacts. |
| Cost dashboard | Filter spend by period, workspace, or model. **Inspect peak spend** ranks conversations in the most expensive interval. |
| Disk Cleanup | Review the saved scan, then choose which build directories and caches to clean. |
| Subscription rotation | Choose **Manage rotation accounts** to select which signed-in accounts may take over at a limit. |
| Mailbox | Use **Needs you** to find review requests, assignments and mentions. Open the item on GitHub. |
| Release Watch | Find deployments awaiting approval, failed deployments, and failing checks. Open the deployment or failing check. |
| Runner Radar | Filter blocked conversations by failure or pending input, see how long they have waited, and open one. |
| Preview Desk | Find a running preview, inspect service logs, or open the repository setup that needs approval. Choose **Show in Actions** to manage a stopped service. |
| Workspace Bootstrap | Find failed or stopped setup, missing files or pending approvals and open the relevant setup or logs. |
| Handoff Brief | Pick the pull requests and artifacts to include, write the next step, then review the draft before publishing. If a source is unavailable or capped at 100 entries, the panel identifies it and you can draft with the sources that loaded. |
| Reminders | Schedule a follow-up at a local time, tomorrow morning or next Monday morning. Cancel it from the scheduled list. |

The panel's bar switches between your extensions (*All extensions…* lists
them, and what waits for your approval), shows what the one in front may do
(the shield) and reloads it.

Opening a specific extension from Settings or a workspace alert shows its docked panel maximized. Choose **Restore layout** to see your other panels again.

A low-disk-space alert has an **Open Disk Cleanup** button. The bundled panel opens the saved report with free space and cleanup candidates, largest measured items first. Earlier installed copies have a **Review latest disk scan** link. Choose **Review and clean…** to start a fresh scan in a new terminal in the Runner’s home folder. Select items in dev-cleaner and confirm to remove them, or quit to keep them. The terminal reports cleaned, skipped or failed items. The panel shows the time of its last scheduled scan. **Dismiss** hides the current warning in this window, including after reload. A change in pressure can show a new warning.

Mailbox, Release Watch, Preview Desk, Runner Radar, Workspace Bootstrap and Handoff Brief support filtering their current list and refreshing it on demand. Row actions open the corresponding conversation, preview, log or GitHub item where available.

## What an extension may do

When you allow an extension, you allow exactly the grants its sheet lists:

| In the sheet | Grant |
| --- | --- |
| See your workspaces | `read:workspaces` |
| See your conversations (titles, status — not messages) | `read:sessions` |
| See what needs you | `read:attention` |
| See a conversation's commands (not their output) | `read:commands` |
| See your missions | `read:missions` |
| See token use and cost | `read:usage` |
| Show a conversation | `show:conversations` |
| Run “Open the inbox” (one action that only shows, focuses or arranges) | `do:inbox.open` |
| Send messages as you | `send:messages` |
| Move Claude (or Codex) conversations to another of your accounts at a limit | `switch:claude`, `switch:codex` (an [account policy](#account-policies)) |

and the network hosts it lists, if any (“what it can see, it could send to
these sites”). Everything else is refused; the panel says so quietly
(“*Agent board asked to send messages as you; you have not allowed it*”, with
**Review access**).

**Updates.** Installing a bundled extension copies its files into your extensions folder. App updates preserve those copies, including your edits. To get a newer bundled panel, save any local edits you want to keep, then remove the installed copy and install it again from the catalog. Review its access when enabling it.

If an extension's files change to ask for more, it keeps running
with what you allowed and shows **Asks for more** in Settings and in its panel
bar; the sheet marks what is new. Asking for less needs nothing from you.

**Settings → Extensions** lists them all with what each may do: the switch
turns one off or on, **Access** shows the sheet again, × removes it (its
folder and what you allowed).

## Account policies

**Manage rotation accounts** opens Subscriptions, where you can include or exclude individual accounts from automatic switching.

An account policy says what Lazybox does when a conversation reaches its
subscription's limit. It is a few lines of the manifest that the Runner reads
itself; no code of the extension runs, and no panel is needed:

```yaml
# extensions/subscription-rotation/extension.yaml (bundled; Settings → Extensions → Available to install)
lazybox: 1
name: Subscription rotation
grants: [switch:codex, switch:claude]    # the providers it may switch
accountPolicy:
  switchOnLimit: true
  accounts: []          # accounts to try, in order (ids); empty is every other signed-in one
```

Nothing switches until you allow it, for exactly the providers its grants name;
an edit that adds one is asked again. Turning it off stops switching at the
next limit, running conversations included. Settings → Subscriptions →
**Automatic switching** still leaves one account out. For Claude, the sheet and
Settings say plainly that Anthropic's terms may not allow moving between Claude
subscriptions to keep working past a usage limit, with a link to them.

## Make your own

```sh
lazybox extension new my-panel          # ~/.config/lazybox/extensions/my-panel
lazybox extension validate my-panel     # every problem by file:line
```

That writes `extension.yaml`, `index.html`, `app.js`, `style.css` and
`lazybox.d.ts` (the bridge's types, for your editor). Open a **New Extensions
panel**, review and allow it, and edit: **every save reloads the panel**.

```yaml
# extensions/my-panel/extension.yaml
lazybox: 1
name: My panel
panels:
  - {id: main, title: My panel, icon: puzzle, entry: index.html}
grants: [read:attention]
network: [https://api.example.com]     # or [] for none
```

```html
<!-- index.html: Lazybox serves _lazybox/ to every extension -->
<link rel="stylesheet" href="_lazybox/lazybox.css">
<script src="_lazybox/lazybox.js"></script>
<script src="app.js" defer></script>
```

```js
// app.js
lazybox.every(5000, async () => {
  const { items } = await lazybox.attention.list();
  render(items.filter(i => i.state === 'needs-input'));
});
```

- **Use the theme.** `lazybox.css` and `lazybox.js` set the person's theme as
  CSS variables — `var(--surface)`, `var(--text)`, `var(--text-muted)`,
  `var(--accent)`, `var(--line)`, `var(--chart-1)`… — and update them when it
  changes. Use them and your panel matches dark, light and every theme.
- **Scripts and styles live in files.** The sandbox refuses inline `<script>`,
  `<style>`, `style="…"` and `eval`. Setting `element.style.width` from your
  script is fine.
- **No storage, no popups, no leaving the panel.** `localStorage` and cookies
  throw; links and forms do nothing outside the panel.
- **The network** is only the hosts in `network:`, over https (or a service on
  this computer, `http://localhost:PORT`).

Every call, its grant and what it returns: [the bridge
reference](../reference/extensions.md). How it is kept safe:
[EXTENSIONS.md](../../EXTENSIONS.md).
