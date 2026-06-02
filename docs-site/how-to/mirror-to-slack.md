# Mirror to Slack

Goal: reflect your pilot inbox into Slack so workspace activity shows up in
channels — useful for visibility or for triaging away from the terminal.

## Prerequisites

- pilot is running.
- A Slack workspace where you can install an app, and permission to create one.

## 1. Create the Slack app

pilot needs a Slack app with a bot token (`xoxb-…`) and an app-level token
(`xapp-…`). The repository ships a ready-to-use app manifest and a step-by-step
walkthrough:

- App manifest and full setup guide:
  [docs/slack-setup.md in the repo](https://github.com/AntoineToussaint/pilot/blob/main/docs/slack-setup.md)

Follow that guide to create the app and obtain both tokens.

## 2. Configure pilot

Add a `slack` block to `~/.pilot/config.yaml`:

```yaml
slack:
  bot_token: xoxb-your-bot-token
  app_token: xapp-your-app-token
  anchor_channel: pilot-inbox       # the channel the mirror anchors to
  per_workspace_channels: true      # give each workspace its own channel
```

See the [configuration reference](../reference/configuration.md#slack) for the
full schema.

## 3. Initialize and check

```sh
pilot slack init       # set up channels / mirror from your config
pilot slack doctor     # diagnose token, scope, and connectivity issues
```

Run `pilot slack doctor` first whenever something looks wrong — it reports
missing scopes or bad tokens directly.

## Housekeeping

```sh
pilot slack prune      # remove stale per-workspace channels
```

## Related

- The [CLI reference](../reference/cli.md) for the `slack` subcommands.
