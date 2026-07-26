---
title: Mirror to Slack
description: Reflect your lazybox inbox into Slack channels.
---

Goal: reflect your lazybox inbox into Slack so workspace activity shows up in
channels — useful for visibility or for triaging away from the terminal.

## Prerequisites

- lazybox is running.
- A Slack workspace where you can install an app, and permission to create one.

## 1. Create the Slack app

lazybox needs a Slack app with a bot token (`xoxb-…`) and an app-level token
(`xapp-…`). The repository ships a ready-to-use app manifest and a step-by-step
walkthrough:

- App manifest and full setup guide:
  [docs/slack-setup.md in the repo](https://github.com/AntoineToussaint/lazybox/blob/main/docs/slack-setup.md)

Follow that guide to create the app and obtain both tokens.

## 2. Configure lazybox

Add a `slack` block to `~/.lazybox/config.yaml`:

```yaml
slack:
  bot_token: xoxb-your-bot-token
  app_token: xapp-your-app-token
  anchor_channel: lazybox-inbox       # the channel the mirror anchors to
  per_workspace_channels: true        # give each workspace its own channel
```

See the [configuration reference](/docs/reference/configuration/#slack) for the
full schema.

## 3. Initialize and check

```sh
lazybox slack init       # validate and store tokens, then join the anchor channel
lazybox slack doctor     # diagnose token, scope, and connectivity issues
```

Run `lazybox slack doctor` first whenever something looks wrong — it reports
missing scopes or bad tokens directly.

## Housekeeping

```sh
lazybox slack prune      # archive stale per-workspace channels
```

## Related

- The [CLI reference](/docs/reference/cli/) for the `slack` subcommands.
