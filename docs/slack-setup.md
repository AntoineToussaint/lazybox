# Slack integration — app setup

Pilot can run on your home machine and surface PR activity through a
Slack workspace so you can monitor + control sessions from your phone.
Inbound messages route to claude sessions; outbound events (PR
updates, CI failures, agent-asking signals) post to per-workspace
channels.

This doc covers the Slack-side setup. The pilot-side wiring (bot
token in config, channel-to-workspace routing) is documented in the
main README under "Slack".

## 1. Create the Slack app (manifest)

1. Go to <https://api.slack.com/apps> → **Create New App** →
   **From manifest**.
2. Pick the Slack workspace where you want pilot to live (typically
   your personal workspace, since the bot can see every channel
   it's invited to).
3. Paste the YAML below. Save.

```yaml
display_information:
  name: pilot
  description: Reactive PR-inbox bot — mirrors PRs into channels, accepts commands to drive claude sessions.
  background_color: "#0b0b0b"

features:
  bot_user:
    display_name: pilot
    always_online: true

oauth_config:
  scopes:
    bot:
      # Read + post in channels pilot creates / is invited to.
      - chat:write
      - channels:read
      - channels:history
      - groups:history    # private channels (in case you DM-style a per-PR channel)
      - im:history        # DMs to the bot
      - im:read
      - mpim:history      # multi-party DMs
      # Receive @mentions + slash commands.
      - app_mentions:read
      # Auto-create one channel per workspace.
      - channels:manage
      # File uploads (paste diff hunks back to channel).
      - files:write

settings:
  event_subscriptions:
    # Socket Mode — pilot opens a WebSocket out from your home
    # machine, so no public HTTPS endpoint required.
    bot_events:
      - app_mention
      - message.channels
      - message.im
      - message.groups
  interactivity:
    is_enabled: true
  socket_mode_enabled: true
  org_deploy_enabled: false
  token_rotation_enabled: false
```

## 2. Generate tokens

You'll need TWO tokens from the Slack app config page:

### Bot User OAuth Token (`xoxb-...`)

For HTTP API calls (`chat.postMessage`, `conversations.create`, etc).

- Sidebar → **OAuth & Permissions** → **Install to Workspace** →
  approve.
- The "Bot User OAuth Token" appears at the top of that page
  (`xoxb-...`). Copy it.

### App-Level Token (`xapp-...`)

For Socket Mode (the persistent WebSocket connection).

- Sidebar → **Basic Information** → scroll to **App-Level Tokens**
  → **Generate Token and Scopes**.
- Name it `socket-mode`.
- Add scope: `connections:write`.
- Click **Generate**, copy the `xapp-...` token.

## 3. Invite the bot to a discovery channel

Pilot needs ONE channel to anchor in — by default a channel named
`#pilot` (configurable). It uses this channel to:
- Post bootstrap messages on first connect ("pilot online, mirroring
  N projects").
- Post errors that don't belong to a specific workspace.

```sh
# In Slack
/invite @pilot
```

(Or invite from the channel settings UI.)

## 4. Configure pilot

Add the tokens to `~/.pilot/config.yaml`:

```yaml
slack:
  # Required.
  bot_token: xoxb-...
  app_token: xapp-...
  # Optional — defaults below.
  anchor_channel: pilot            # bootstrap + error channel
  channel_prefix: ""               # auto-created channel name template;
                                   # empty = "<owner>-<repo>-<n>". A
                                   # value like "pr-" produces
                                   # "pr-<owner>-<repo>-<n>".
  per_workspace_channels: true     # set false to skip auto-create
                                   # and route everything through the
                                   # anchor channel
```

Restart pilot:

```sh
make run
```

You should see `pilot: slack connected as @pilot` in `/tmp/pilot.log`
and a "pilot online" message in `#pilot`.

## 5. Verify

1. Trigger a poll: press `Shift-R` in the TUI, or wait ~60s.
2. For each PR pilot finds, it auto-creates a channel
   `#<owner>-<repo>-<n>` (e.g. `#acme-widget-186`) and posts the
   PR description as the first message.
3. From Slack (web / mobile), type a message in a per-workspace
   channel:
   ```
   @pilot work
   ```
   Pilot spawns claude in that workspace's worktree with the
   role-aware prompt (review for reviewer-role, address-comments
   for author-with-unread, etc.).
4. Other commands:
   ```
   @pilot status              # show session state + agent activity
   @pilot ping                # liveness check
   @pilot stop                # interrupt the current agent
   ```

## Notes / Gotchas

- **Channel limits**: free Slack tiers cap at ~9000 channels. If
  you watch hundreds of repos, set `per_workspace_channels: false`
  and route everything through the anchor channel with thread-per-PR
  (use `channel_strategy: thread_per_workspace`).
- **Channel name length**: Slack truncates at 80 chars. Pilot
  sluggifies and clips automatically.
- **Re-using existing channels**: pilot looks up by name before
  creating. So if `#acme-widget-186` already exists, it just joins
  + posts (won't recreate).
- **Archived channels**: pilot won't auto-unarchive. If a PR you
  archived in Slack comes back to life, pilot posts to the anchor
  channel with a hint to unarchive manually.
- **Two pilot instances on one Slack workspace**: don't. Both will
  try to create the same channels + both will respond to the same
  `@pilot` mentions. Use one workspace per pilot instance.
