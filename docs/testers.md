# Testing the preview

Thank you for trying Lazybox. This page is your hour one: what to install,
what to try, how to tell us what went wrong, and what not to count on yet.

## Before you start

- **Install it.** In a clone of the repository, `make install`
  ([Install from source](install.md#install-from-source)). It checks what the
  build needs and tells you how to get anything missing; the first build takes
  5 to 10 minutes.
- **Have an agent account.** Claude Code and/or Codex. If `claude` or `codex`
  already works in your terminal, Lazybox uses that sign-in.
- **Pick a repository you do not mind agents working in.** Lazybox gives every
  workspace its own git worktree and never touches your checkout, but agents
  can still push, open pull requests and run commands. A side project, or a
  fork, is a good start.
- **Run `lazybox doctor`** in a new terminal. Everything should be ✓ except,
  perhaps, an agent you do not use.

## What to try in your first hour

Each one takes a few minutes. Tick what worked; tell us about what did not.

1. **New task.** `⌘N`, describe a small change ("add a --version flag"),
   pick your repository, keep *Ask for approval*, **Enter**. A workspace
   appears on a branch named from your message and the agent starts.
   *Expected:* one live line at the bottom says what it is doing.
   [Getting started →](getting-started.md#the-fastest-start-new-task-n)
2. **Chat with approvals.** When the agent asks to run a command or edit a
   file, the card shows exactly what: try **Allow once**, then **Deny** with a
   reason, and see the agent change course. Answer a question card with
   `1`–`9`. [Work in the chat →](guides/chat.md#answer-questions-and-approvals)
3. **Send now.** While the agent works, type a correction: **Enter** queues
   it for after the current step, `⌘↵` sends it now and stops the step.
   [Send, steer and stop →](guides/chat.md#send-steer-and-stop)
4. **Close the window mid-task.** Quit Lazybox (`⌘Q`) while the agent works,
   wait a minute, open it again. *Expected:* the agent kept working, and the
   conversation is where it was.
5. **Review with another agent.** In the conversation's header, **Review**
   (or `/review`): Codex reviews Claude's work, or the other way round, without
   changing anything. Its findings land in the Review tray of **Changes**
   (`]] d`), where you can send them to the agent that did the work.
   [Review an agent's work →](guides/review-agent-work.md#let-another-agent-review-it)
6. **Commit, push and follow the pull request.** Connect GitHub first
   (Settings → Repositories → *Connections & folders* → **Connect with a
   personal token**; `gh auth token` works). In Changes: **Commit…**,
   **Push**, **Create PR…**, then watch the card's checks, failing first.
   Ask an agent to open one with `gh pr create` and Lazybox tracks it too.
   [Pull requests →](guides/review-agent-work.md#commit-push-open-and-merge-a-pull-request)
7. **Artifacts.** Ask the agent for a plan in a Markdown file, then **Track**
   its message (or the file). Ask for a revision and compare the versions.
   [Artifacts →](guides/artifacts.md)
8. **Voice.** Click into the message box, hold **Right ⌥**, speak, let go.
   macOS asks for the microphone and speech recognition the first time.
   [Voice →](guides/voice.md)
9. **Extensions.** **Panels → New Extensions panel**, **Add** the *Agent
   board*, read what it may do, **Allow and turn on**.
   [Extensions →](guides/extensions.md#try-one)
10. **Your phone** *(optional, needs Tailscale)*. Settings → Devices →
    **Turn on**, **Pair a phone**, scan the code, then answer an approval from
    the phone. [Pair your phone →](guides/phone.md)

Lost? `⌘K` finds every action by name, `?` lists every shortcut, and
**Help → Ask Lazybox** answers questions about the app.

## How to report a problem

**Help → Send feedback…** is the quickest way. It writes a report with what
you say, a screenshot of the window and the Runner's recent log, with tokens,
e-mail addresses and your home folder removed. You read all of it first;
nothing is sent by itself. **Open a GitHub issue…** opens a new issue filled
in with it, for you to post; **Save to file** keeps it in Downloads.
[Send feedback →](install.md#send-feedback)

Or open an [issue](https://github.com/AntoineToussaint/lazybox/issues)
yourself, with:

- **What you did, what you expected, what happened**, and a screenshot for
  anything you can see.
- **The output of `lazybox doctor`**, and the commit you built
  (`git rev-parse --short HEAD` in your clone; Settings → About & updates shows it too).
- **For a conversation that misbehaves:** `lazybox debug "<its title>"`
  explains its state in plain words; paste that.
- **For an install that failed:** the end of the terminal output of
  `make install`.

Where the logs are:

| What | Where |
| --- | --- |
| The Runner | `~/Library/Logs/Lazybox/runner.log` (or `lazybox service logs`) |
| Each agent's session host | `~/Library/Application Support/Lazybox/state/hosts/*/host.log` |
| The build | the terminal you ran `make install` in |

Logs can contain file paths and command lines from your work: read them
before you attach them. Security problems go privately, as
[SECURITY.md](../SECURITY.md) describes.

## What not to rely on yet

- **Your data's format.** Conversations and settings survive `make update`,
  but an early preview can still change how they are stored. Keep the work
  that matters committed and pushed: a workspace's worktrees live in
  `~/Library/Application Support/Lazybox/workspaces`.
- **Previews.** Missions, agents that run agents, the phone, Gemini CLI,
  Goose and other ACP agents are marked Preview in the app: they work, but are
  incomplete.
- **YOLO and full access.** An agent with full access runs anything without
  asking. Lazybox is not a sandbox; use it only in workspaces you can throw
  away.
- **Updates.** A build from source never updates itself: `make update`.
  Because it is signed for your Mac only, macOS may ask again for the
  microphone or notifications after an update.
- **Platforms.** Apple Silicon Macs only; nothing runs in a cloud.

Done testing? `make uninstall` removes the app, its Runner service and the
`lazybox` link and keeps your data; `make uninstall PURGE=1` deletes that too.
