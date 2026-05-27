# Slack integration — app setup

Pilot can run on your home machine and surface workspace activity through a
Slack workspace so you can monitor + control sessions from your phone.
Inbound messages route to claude sessions; outbound events (workspace
updates, CI failures, agent-asking signals) post to per-workspace
channels.

Setup is two steps:

1. **Create the Slack app from the shipped manifest.** This step
   requires a browser session — Slack requires it.
2. **Run `pilot slack init`.** Pastes the two tokens through a
   wizard that validates each and writes them to
   `~/.pilot/config.yaml`.

## 1. Create the Slack app (manifest)

1. Go to <https://api.slack.com/apps> → **Create New App** →
   **From manifest**.
2. Pick the Slack workspace where you want pilot to live (typically
   your personal workspace, since the bot can see every channel
   it's invited to).
3. Paste the contents of [`slack-manifest.yml`](./slack-manifest.yml)
   into the manifest editor. Save.
4. Slack walks you through OAuth consent + workspace install — accept.

You also need an app-level token for Socket Mode. Slack hides this
behind a separate dialog:

- Sidebar → **Basic Information** → scroll to **App-Level Tokens**
  → **Generate Token and Scopes**.
- Name it `socket-mode`.
- Add scope: `connections:write`.
- Click **Generate**. Leave this tab open — `pilot slack init`
  will ask you to paste this token in a moment.

The Bot User OAuth Token lives under **OAuth & Permissions** (it
appears once you've installed the app to a workspace in step 4
above). Also leave that tab open.

## 2. Run the wizard

```sh
pilot slack init
```

The wizard:

1. Prompts for the Bot User OAuth Token (`xoxb-...`), validates it
   via `auth.test`, and bails with a clear message if the token is
   the wrong type or lacks scopes.
2. Prompts for the App-Level Token (`xapp-...`) and validates it by
   opening (and immediately closing) one Socket Mode connection.
3. Writes both tokens into `~/.pilot/config.yaml` under `slack:`,
   preserving every other key in the file.
4. Looks up the anchor channel (`#pilot` by default) and self-joins
   it, so there's no manual `/invite @pilot` step.
5. Prints `✓ Slack ready` or, if `#pilot` doesn't exist yet, the
   exact next manual step (create the channel, then run
   `pilot slack doctor`).

If you ever want to confirm the setup still works (e.g. after a
hand-edit to `~/.pilot/config.yaml`):

```sh
pilot slack doctor
```

This runs the same validations read-only — no prompts, no writes.

## Configuration knobs

`pilot slack init` only writes the two tokens. Everything else lives
under `slack:` in `~/.pilot/config.yaml` and has sensible defaults:

```yaml
slack:
  bot_token: xoxb-...               # written by `pilot slack init`
  app_token: xapp-...               # written by `pilot slack init`
  anchor_channel: pilot             # bootstrap + error channel
  channel_prefix: ""                # auto-created channel name template;
                                    # empty = "<owner>-<repo>-<n>". A
                                    # value like "pr-" produces
                                    # "pr-<owner>-<repo>-<n>".
  per_workspace_channels: true      # set false to skip auto-create
                                    # and route everything through the
                                    # anchor channel
```

Restart pilot after editing:

```sh
make run
```

You should see `pilot: slack connected as @pilot` in `/tmp/pilot.log`
and a "pilot online" message in `#pilot`.

## Verify

1. Trigger a poll: press `Shift-R` in the TUI, or wait ~60s.
2. For each workspace pilot finds, it auto-creates a channel
   `#<owner>-<repo>-<n>` (e.g. `#acme-widget-186`) and posts the
   primary task's description (or workspace name) as the first message.
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
  and route everything through the anchor channel with thread-per-workspace
  (use `channel_strategy: thread_per_workspace`).
- **Channel name length**: Slack truncates at 80 chars. Pilot
  sluggifies and clips automatically.
- **Re-using existing channels**: pilot looks up by name before
  creating. So if `#acme-widget-186` already exists, it just joins
  + posts (won't recreate).
- **Archived channels**: pilot won't auto-unarchive. If a workspace you
  archived in Slack comes back to life, pilot posts to the anchor
  channel with a hint to unarchive manually.
- **Two pilot instances on one Slack workspace**: don't. Both will
  try to create the same channels + both will respond to the same
  `@pilot` mentions. Use one workspace per pilot instance.
