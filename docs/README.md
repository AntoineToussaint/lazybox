# Lazybox documentation

Lazybox is one window for many coding agents across your workspaces — and the
place you check their work before it lands. These pages are for people **using**
Lazybox. They describe what is on `main` today;
anything marked **Preview** works but is incomplete or not yet packaged.

> **Lazybox is an early preview.** You install it from a clone with one
> command, `make install` ([Install from source](install.md#install-from-source)).
> Testing it? Start with [the testers' guide](testers.md). The `lazybox`
> formula on Homebrew is the older terminal inbox — its docs are at
> [lazybox.ai/docs](https://lazybox.ai/docs/).

## Start

- **[Getting started](getting-started.md)** — install, first workspace, first
  agent, and the five keys worth learning on day one.
- **[Install](install.md)** — the Mac app: install from source, first run,
  updates, the `lazybox` command, troubleshooting and uninstalling.
- **[Testing the preview](testers.md)** — what to try in your first hour, how
  to report a problem, and what not to rely on yet.
- **[Concepts](concepts.md)** — the Runner, workspaces, conversations,
  sub-agents, missions, artifacts and subscriptions, in one page.

## Guides

Task by task:

| I want to… | Guide |
| --- | --- |
| Ask how to do something in Lazybox, or have it change a setting for me | [Ask Lazybox](guides/ask-lazybox.md) |
| Start a task from a GitHub issue: paste its link, or pick one assigned to me | [Start from an issue](guides/start-from-an-issue.md) |
| Work with an agent: answer its questions, approve, follow sub-agents | [Work in the chat](guides/chat.md) |
| See less (or more) of what the agent ran | [Activity detail](guides/activity-detail.md) |
| Check what an agent changed, comment, undo a turn, open and merge a PR | [Review an agent's work](guides/review-agent-work.md) |
| Run my app per workspace and look at it beside the chat | [Run your app](guides/run-your-app.md) |
| Keep my own clone of a repo for logins, dev servers and trying things by hand | [Home checkouts](guides/home-checkouts.md) |
| Let an agent use a browser while I watch, and take it over | [Agent browser](guides/agent-browser.md) |
| Track a goal across several conversations, with evidence | [Missions and evidence](guides/missions.md) |
| Let one agent start and drive others for a mission, under limits I set | [Let an agent run agents](guides/orchestrator.md) |
| Keep plans and reports as versioned pages | [Artifacts](guides/artifacts.md) |
| Use several Claude or Codex accounts, and survive limits | [Subscriptions and limits](guides/subscriptions.md) |
| Run Gemini CLI, Goose, OpenCode or any other ACP agent | [Gemini CLI, Goose and other agents](guides/other-agents.md) |
| Answer my agents from my phone | [Pair your phone](guides/phone.md) |
| See Dev, Working and my other Runners in one window | [Several Runners in one window](guides/several-runners.md) |
| Arrange panels, pages and windows | [Layouts and docking](guides/layouts.md) |
| Switch to light, make my own theme, change fonts, zoom, notifications | [Appearance and notifications](guides/appearance.md) |
| Talk instead of typing: dictate, say commands, hear replies | [Voice](guides/voice.md) |
| Script Lazybox, add my own actions and hooks | [Automate with the CLI, actions and hooks](guides/automate.md) |
| Add a panel of my own (a board, a dashboard), sandboxed | [Extensions](guides/extensions.md) |

## Reference

- [Keyboard shortcuts](reference/keyboard-shortcuts.md) — generated from the
  app's action registry, so it always matches the defaults.
- [The `lazybox` CLI](reference/cli.md) — every command, exit codes, completions.
- [Voice settings](reference/voice-settings.md) — every option of Settings →
  Voice, with its default.
- [Extensions](reference/extensions.md) — `extension.yaml`, every bridge call
  and the grant it needs, what an extension's page may load.
- [Configuration files](reference/configuration.md) — `.lazybox/workspace.yaml`,
  `.lazybox/process.yaml`, `~/.config/lazybox/actions.yaml`, with examples and
  their JSON Schemas.

## When something is wrong

- [Troubleshooting](troubleshooting.md) — `lazybox debug`, no agent ready,
  limits, provider errors, interrupted turns, a Runner that restarted.
- [FAQ](faq.md) — including "is Lazybox a terminal?" (no).

## Contributing

The design notes, audits, plans and test evidence behind V2 are indexed in
[internal docs](internal/README.md).
