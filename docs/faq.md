# FAQ

### Is Lazybox a terminal?

**No.** Lazybox does not try to be a terminal emulator, and terminal splits or
agents running in terminal panes are out of scope. Lazybox owns the
*conversation* — so it can show approvals as cards, fold tool calls, review
diffs, checkpoint turns and record evidence — which a terminal pane cannot.

Keep using your terminal (Ghostty, iTerm, …) for your own commands, and send
their output to Lazybox when it matters:

```sh
go test ./... 2>&1 | lazybox log --title "go test ./..."
```

That works with no flag in a Lazybox workspace terminal — a deliberately
minimal shell on the Runner, there as an escape hatch. From your own terminal
it currently needs the Runner and workspace spelled out; see
[Run your app](guides/run-your-app.md#send-your-terminals-output-to-lazybox).

### What is the difference between V1 and V2?

**V1** (the `lazybox` you install with Homebrew) is a terminal inbox for pull
requests and issues, with an embedded terminal per task — documented at
[lazybox.ai/docs](https://lazybox.ai/docs/). **V2**, documented here, is a
desktop app for running many agents and checking their work: it owns the
conversations, workspaces span several repositories, and agents keep running
when the app or its Runner restarts. V2 is a preview built from source; it
does not read V1's data.

### Which agents does it support?

Claude (through the Claude Agent SDK) and Codex (through `codex app-server`).
You can mix them, even in one workspace.

### What happens to my agents when I close the window or restart?

Nothing. Agents, the commands they started, your actions and your terminals run
on the Runner, each in its own host process. Close the window, reload, restart
the Runner or install a new build: they keep running and Lazybox picks them up
again. Only **End session**, **Stop**, or deleting the conversation stops one.

### Does it touch my checkouts?

No. Every workspace works in its own git worktrees. Checkpoints use private refs
and never move your branch, index or stash. Lazybox commits and pushes only
when you click (or ask) it to, never force-pushes, and never merges a pull
request that GitHub does not report green.

### Can an agent approve its own requests or run my repository's scripts?

No. An agent runs with a credential scoped to its own console and its
workspace's artifacts; approving, answering, changing settings, approving
workspace scripts and the PR loop are yours alone. (An agent does run as your
OS user, so this is a guard against mistakes, not a sandbox.)

### Where is my data?

On your machine, in the Runner's state folder: for a Working install from a
checkout, `.cache/environments/working/` (`make environments` prints
the paths). Dev keeps its own, separately.

### Does it work on Linux or Windows?

Not yet as a desktop app: it is built and tested on macOS only. The interface
also runs in a browser (`make web`, a test host), and your phone can
[pair](guides/phone.md) over Tailscale.

### Is there a light theme?

Yes: Settings → Appearance offers System, Light or Dark, four built-in themes,
and your own theme files. See [Appearance](guides/appearance.md).

### How do I find a key?

`⌘K` and type what you want; `?` lists every shortcut with your keys. All
defaults: [Keyboard shortcuts](reference/keyboard-shortcuts.md).

### Where is the documentation in the app?

**Help → Documentation** opens these pages inside Lazybox, offline, with
search; they are bundled with the app, so they match the version you have.
lazybox.ai documents V1.
