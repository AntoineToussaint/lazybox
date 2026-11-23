# Ask Lazybox

Ask Lazybox is the help assistant: an agent that answers questions about
Lazybox itself — where a setting is, which key does what, how to run your app in
a workspace — from these pages. It can also change a few of your settings for
you, and only after you allow it.

## Open it

- **Help (?) → Ask Lazybox…** in the window's header, or **Help → Ask
  Lazybox…** in the Mac menu bar.
- **`]] h`**, or **⌘K** and type *ask*.
- The keyboard shortcuts overlay (`?`) lists it under General.

It opens as a panel on the page you are on (like any panel: close it, dock it on
another edge, resize it) and puts the cursor in its message box. Type your
question and press **Enter**, or pick one of the suggested questions. There is
nothing to set up first.

## Which agent answers

Ask Lazybox picks one by itself, and its header says which: *Help · using Claude
(Work subscription)*.

- It prefers the agent you last chose for a conversation (Claude Code by
  default) and the best of its subscriptions: one known to be available, a
  private subscription before the CLI's own sign-in.
- It skips a subscription that is at its limit, signed out or has a billing
  problem, and falls back to the other agent when none of the first is usable
  (the header then says, for example, *Claude is not ready*).
- **Switch** in the header lists every subscription with its state (*Signed
  in*, *Limit reached · back at 6:00 PM*, *Not signed in*). Choosing one starts
  the next question on it; the previous conversation is kept. **Check again**
  looks for newly installed agents and new sign-ins.

The same check powers the welcome: *Claude Code 2.1.311 installed · signed in*,
*Codex not installed*.

## When no agent is ready

Instead of an error, the panel lists what each agent needs and the step that
fixes it:

| It says | Do this |
| --- | --- |
| **Not installed** | Copy the install command it shows, run it in a terminal, then **Check again**. See [Ask Lazybox says no agent is ready](../troubleshooting.md#ask-lazybox-says-no-agent-is-ready). |
| **Not signed in** / **no subscription** | **Add a subscription…** opens Settings → Subscriptions. See [Subscriptions and limits](subscriptions.md). |
| **Limit reached · back at …** | Wait for the reset, or add another subscription. |

**Read the docs** opens this documentation, which works without any agent.

## What it can do

- **Create or adjust a workspace layout.** Describe the arrangement you want, such as “put changes beside my agent and leave the terminal below.” A new help conversation receives the current docked layout. Choose **Review layout** on its proposal to edit the YAML, then **Import** to validate, save, and apply it. For scripting, `lazybox do --list layout` lists the available actions. `lazybox do layout.import "<layout YAML>"` opens the same importer. For detached windows, ask for equal columns or a grid and choose the arrangement button in the reply. From the CLI, run `lazybox do layout.arrange-windows columns`. It also accepts `focus`, `grid`, `next-display`, `save`, and `restore`, mirroring the desktop’s **Surfaces → Arrange Windows** menu.
- **Answer from the documentation.** It searches and reads these pages (the
  same ones Help → Documentation shows) and tells you which page it used.
- **Start adding a subscription.** Choose **Add subscription** in the answer to open the sign-in dialog, then choose your provider and sign in.
- **Propose a settings change** — suggested workspace names, default
  permissions for new sessions, and the Lazybox, Claude and Codex context. You
  see the current and proposed values with its reason, and nothing changes
  until you choose **Allow**.
- It never runs commands, edits files, or touches your accounts, repositories,
  keyboard shortcuts or snippets; it tells you where to change those yourself.

The panel's **Switch** menu also starts a planning or summary conversation
instead; both are advice only, like the help assistant.

## Training Wheels

Open **Help → Training Wheels** and select **Enable coaching**. After you use the panel controls to maximize or restore the layout three times, the panel can show your configured shortcut. Using the shortcut silences that tip for the window session. **Dismiss tip** remembers your choice. Turning coaching off clears the counts.

The tip and shortcut guide work offline. **Ask Lazybox** opens an editable follow-up question. Send it to start a new help conversation. Your existing draft stays available.
