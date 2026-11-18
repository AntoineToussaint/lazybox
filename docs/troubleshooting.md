# Troubleshooting

Start with the conversation's own words — Lazybox says why it refuses
something — then ask `lazybox doctor` (the installation) or `lazybox debug`
(a conversation).

- [The installation: `lazybox doctor`](#the-installation-lazybox-doctor)
- [First: `lazybox debug`](#first-lazybox-debug)
- [An account hit its limit](#an-account-hit-its-limit)
- [The provider returned an error](#the-provider-returned-an-error)
- [I interrupted a turn](#i-interrupted-a-turn)
- [Enter does nothing / my message is not sent](#enter-does-nothing--my-message-is-not-sent)
- [The window says Reconnecting](#the-window-says-reconnecting)
- [An agent seems stuck](#an-agent-seems-stuck)
- [Setup or an action does not start](#setup-or-an-action-does-not-start)
- [Pull requests are unavailable](#pull-requests-are-unavailable)
- [The CLI cannot find the Runner](#the-cli-cannot-find-the-runner)
- [An agent says `unknown command` for a lazybox command](#an-agent-says-unknown-command-for-a-lazybox-command)
- [The launcher exited after a Go edit (Dev)](#the-launcher-exited-after-a-go-edit-dev)

## The installation: `lazybox doctor`

```sh
lazybox doctor
```

For the installed app ([Install](install.md)). It checks the release,
the Runner service (installed, running, answering on this Mac only), that the
app, the CLI and the Runner are the same version, the bundled Node and Claude
worker, Claude Code, Codex, `git` and `gh` on the PATH agents get, which
`lazybox` agents' shells reach (*Agents' lazybox*), your GitHub connection and
free disk space, and prints the fix beside anything wrong. The
setup window runs the same checks (**Run diagnostics**) when the app cannot
reach its Runner. `lazybox service logs -f` follows the Runner's log;
`lazybox service restart` gives you a fresh Runner and keeps your agents.

## First: `lazybox debug`

```sh
lazybox debug "fix the tests"
```

It reads the conversation's history and the Runner's state and answers in plain
words: whether it is busy, queued or faulted, what is pending, which sub-agents
run, the last lifecycle events, and **findings** such as *"the last turn ended by
an interrupt you sent at 16:59"*, *"a message has been queued behind the running
turn for 5m"* or *"no event for 12m while a turn runs"*. `lazybox watch
<conversation>` then follows it live.

(V1 — the terminal app — has its own troubleshooting; it does not apply here.)

## Ask Lazybox says no agent is ready

**You see** *No agent is ready to answer* in the Ask Lazybox panel, with what
each agent needs. Lazybox runs agents with your own subscription, on the
Runner's computer, so each needs its program and a sign-in:

- **Claude Code — Not installed.** A Lazybox built without Claude Code (or
  one whose copy is missing) runs the one on your PATH. Settings → Agents &
  workspaces → Claude → **Install Claude Code** types Anthropic's installer
  (`curl -fsSL https://claude.ai/install.sh | bash`) into a shell panel; then
  restart the Runner (Settings → Runner) so it finds it. In a checkout,
  `make deps` restores the bundled copy.
- **Codex — Not installed.** Run `npm install -g @openai/codex` (or `brew
  install codex`). An installed Lazybox's Runner reads the PATH once: run
  `lazybox service install` to refresh it, then **Check again**.
- **Not signed in, or no subscription.** For Claude, save an Anthropic API key
  in Settings → Agents & workspaces → Claude, or sign in once in a terminal
  (`claude`, then `/login`) and Lazybox uses that sign-in as *CLI sign-in*. For
  Codex, Settings → Subscriptions → add a ChatGPT subscription, or `codex
  login`. Why Claude differs: [Subscriptions and limits](guides/subscriptions.md#claude-an-api-key-or-your-own-claude-code-sign-in).
- **Limit reached · back at …** Every subscription of that agent is at its
  limit. Wait for the reset, or add another subscription.
- **Cannot run.** The panel says why (a program that does not start, the Claude
  worker or Node missing). `lazybox doctor` checks the same things with a fix
  for each.

**Check again** looks again for programs and sign-ins. Help → Documentation
works without any agent. More in [Ask Lazybox](guides/ask-lazybox.md).

## An account hit its limit

**You see** a card: *Personal Claude reached its limit (resets 6:00 PM)*, or the
sidebar reads *Waiting for reset 6:00 PM*.

- **Switch to** another subscription of the same provider — the same
  conversation carries on. Several to choose from? The picker shows each one's
  state and reset.
- **Wait for reset** — it resumes by itself at the reset while the window is open.

Lazybox switches only when you ask. To switch by itself at a limit, add the
**Subscription rotation** extension (Settings → Extensions); the card then
also offers **Always switch automatically**.

Messages you send meanwhile are kept and delivered once it continues. No other
subscription? Add one in Settings → Subscriptions. Details:
[Subscriptions and limits](guides/subscriptions.md).

## The provider returned an error

**You see** the turn end with an error from Claude or Codex (an API error, an
image it could not read). **Send your next message:** the conversation
continues and the error clears. If instead the conversation says it **needs
recovery**, Lazybox lost track of the agent's process; use **Resume** in the
conversation before sending.

## Gemini CLI (or another ACP agent) does not start

**You see** *Gemini CLI needs you to sign in* when you start the
conversation. **Run the agent once in your own terminal** (`gemini`, `goose
configure`, …) and sign in the way it asks; the message names its ways. Then
start the conversation again.

**It is not in New conversation:** Settings → Agents & workspaces → Agent
programs says whether the Runner found it. The Runner looks on its own PATH:
an agent installed after the Runner started is found by **Check again**; one
installed where the Runner's PATH does not reach (for example `~/.local/bin`)
needs `lazybox service install` to refresh that PATH, or **Edit** with its
absolute path. *speaks ACP version N* means the agent is newer or older than
the protocol Lazybox speaks (version 1): update one of them. See
[Gemini CLI, Goose and other agents](guides/other-agents.md).

## I interrupted a turn

That is not an error. The turn ends as *interrupted*, the conversation is
intact, and your next message continues it. `lazybox debug` confirms *the last
turn ended by an interrupt you sent*.

## Enter does nothing / my message is not sent

Lazybox always says why under the message box: still sending, connecting, the
conversation needs recovery, it ended, or the Runner's reason. A message the
Runner never confirms is released after 30 seconds with a note; sending again
can never deliver it twice. If a draft's attached files were lost in a reload,
the notice offers **Attach again…** or **Send without it**.

## The window says Reconnecting

The Runner restarted or the window lost it. Your agents are not affected — they
keep running and the Runner re-attaches to them. The window follows a
restarted Runner by itself and keeps your drafts. If it stays disconnected,
check the launcher's terminal (`make working` or `dev`) is still running;
start it again if not.

## An agent seems stuck

1. `lazybox debug <conversation>` — is it waiting for you (a question or an
   approval), queued, or silent while a turn runs?
2. Open **Processes** (the **Panels** menu in the header) to see its CPU, memory and
   uptime.
3. **Interrupt** the turn from the live bar (or `lazybox stop <conversation>`).
   A sub-agent can be stopped alone from its card.

## Setup or an action does not start

- **Review scripts** is showing: nothing from `.lazybox/workspace.yaml` runs
  until you approve that exact version.
- *Port in use*: another program holds the action's port. The message names the
  port; `lsof -i :<port>` finds the program.
- The file has a mistake: the error names its line and the key it did not
  understand, and nothing from that file runs until it is fixed.
- An action cannot start while its repository's setup is still running.

## Pull requests are unavailable

Commit and push work with any remote; pull requests need GitHub. Connect it in
Settings → Repositories → *Connections & folders* → **Connect with a personal
token**. A token that expired or was revoked is reported when GitHub rejects
a request: paste a new one. For organizations with SSO, authorize the token for
each organization on GitHub.

## The CLI cannot find the Runner

`lazybox: no Lazybox Runner found` means no Runner is registered: open the
Lazybox app once (or `lazybox service install`), start `make working`
(or `dev`) from a checkout, or pass `--endpoint <state>/endpoint.json`. `lazybox runners` lists the Runners it knows
and which is live. Inside an agent or a workspace terminal, the CLI only uses
that place's own limited credential; your own commands there fail with exit 4.

## An agent says `unknown command` for a lazybox command

`unknown command artifact` (or `run`, `workspace`, `review`…) means the agent's
shell ran another `lazybox`, usually an older Lazybox in `~/.cargo/bin` or
Homebrew's, which your shell startup files (`~/.zshenv`, `~/.zprofile`, mise, Homebrew's
`shellenv`, macOS's `path_helper`) put first on `PATH`. `lazybox --version`
tells them apart: this one prints `lazybox <version> (Lazybox V2)`.

Lazybox handles it for you: every agent, every command it starts and every
workspace terminal gets a shell integration that runs your startup files and
then puts this Lazybox's CLI back first, for zsh, bash and Claude Code's Bash
tool ([command output](../COMMAND_OUTPUT.md#which-lazybox-agents-run)). When
your files shadow it, the first new conversation says so in a Runner line, and
**Settings → About & updates → Command line** and `lazybox doctor` (*Agents' lazybox*) show
what was found. Nothing to do then.

If they say the integration could not fix a shell, every new conversation says
so, and agents are told to call the CLI by its full path (`$LAZYBOX_V2_CLI`).
To fix it for good, remove or rename the other `lazybox`, or stop your startup
files from putting its directory before the others. A conversation started
before Lazybox had the integration keeps its old environment: its agent gets
the full path with its next message ("Instructions updated"), and a new
conversation gets the integration.

## The launcher exited after a Go edit (Dev)

Only for `make dev`: a Go change that does not compile stops the
launcher instead of waiting for the fix (a limitation of Codefly's reloader).
Fix the compile error and run `make dev` again. Your agents kept running
meanwhile.
