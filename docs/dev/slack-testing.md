# Slack integration — testing guide

End-to-end smoke test for the Slack bridge. Assumes `docs/slack-setup.md`
is done (app created, both tokens generated, bot invited to `#lazybox`).

## 0. Prereqs

```sh
# Tokens — keep them out of git. Either export them …
export SLACK_BOT_TOKEN=xoxb-...
export SLACK_APP_TOKEN=xapp-...

# … or put them in ~/.lazybox/config.yaml (env wins).
```

Tail the daemon log in another pane while running these steps:

```sh
tail -F /tmp/lazybox.log | grep -i slack
```

## 1. Bot connects on launch

```sh
make run
```

**Expect** (within ~2 s of launch):

- Log line: `slack: connected team=<your-workspace> user=lazybox`.
- Log line: `slack: prefetched channel listing channels=<N>`.
- `#lazybox` (the anchor channel) receives a `*lazybox online* · connected as
  @lazybox. Mirroring N project(s).` message.

**If the anchor message doesn't appear**: confirm `/invite @lazybox` ran in
`#lazybox`. Lazybox logs `slack: anchor channel not visible` if the bot isn't
in the channel.

## 2. Per-workspace channels auto-create

Press `Shift-R` in the TUI to trigger a poll (or wait ~60 s for the next
poll tick).

**Expect**:

- For each workspace lazybox discovers, a new Slack channel
  `#<owner>-<repo>-<n>` (e.g. `#acme-widget-186`).
- Log line per channel: `slack: created channel channel=<name>`.
- The first message posted to that channel is the workspace's primary
  task title + URL:

  ```
  📋 *Fix the date picker keyboard nav*
  <https://github.com/acme/widget/pull/186>
  ```

**Idempotency check**: re-run `make run`. Lazybox should *not* re-post the
title (the `WorkspaceUpserted → already-tracked` guard suppresses
duplicates).

## 3. Asking state — paused vs done

This is the heuristic that flags "claude is mid-question" (output still
streaming) vs "claude is idle, waiting for next task" (output stopped ≥3 s
ago). The label changes based on `last_output_at[terminal_id]`.

### 3a. `paused — input expected` (claude asks mid-task)

1. In the TUI, open any workspace and press `c` to spawn claude.
2. Type something that triggers a yes/no prompt — e.g. give it an
   instruction that requires file deletion ("delete the `tmp/` directory"
   if you have one), so claude prints the confirmation prompt.
3. While claude is still streaming output, watch the Slack channel.

**Expect**:

- A Slack message that opens with `⏸ *paused — input expected*` followed
  by the last ~30 lines of PTY output (the prompt itself).

### 3b. `done — waiting for next task`

1. Let claude finish a task ("ok" → confirm any pending prompt, or wait
   for it to settle on its standard "Ask me anything" prompt).
2. Wait at least 3 seconds *after* claude's last visible output.
3. Make claude transition to `Asking` again — easiest way: send a short
   message that produces a quick prompt, then wait for the prompt to
   re-appear.

**Expect**:

- A Slack message that opens with `✅ *done — waiting for next task*` —
  proving the quiet-time gate fires.

### What's the heuristic doing under the hood?

`DONE_QUIET_THRESHOLD = 3s`. Every `TerminalOutput` event updates
`last_output_at[terminal_id]`. When `AgentState::Asking` fires, lazybox reads
`now - last_output_at`:

- `< 3s` → `paused`. Claude is mid-stream, the prompt is part of an
  ongoing task.
- `≥ 3s` → `done`. Stream has been quiet long enough that the prompt is
  almost certainly the "what next?" idle state.

## 4. Inbound reply routes to claude

From Slack (web or mobile), in *any* of the per-workspace channels:

```
@lazybox yes
```

**Expect** (in the TUI):

- The agent terminal for that workspace receives `yes\r` as input.
- If claude was at a y/n prompt, it advances.
- Log line: `slack: routed inbound message to agent workspace=<key>`.

**Multi-line check** (bracket-paste path):

```
@lazybox here is a longer reply:
- first point
- second point
```

**Expect**:

- Claude receives the whole multi-line block as one paste (no per-line
  submit). On the TUI you'll see the lines appear together rather than
  claude dispatching three separate prompts.
- Confirmed by reading the test `encode_for_pty_multi_line_wraps_in_bracket_paste`
  in `crates/server/src/slack.rs` — lazybox wraps multi-line bodies with
  `ESC[200~ … ESC[201~`.

## 5. Inbound noise is ignored

Drop a message in `#lazybox` (the anchor channel) *without* a mention:

```
just chatting
```

**Expect**: nothing happens. Lazybox only routes messages whose channel is
tracked in the `channel_to_workspace` map. The anchor channel is
intentionally not in that map.

## 6. Bot user can't trigger itself

Worth a manual check on fresh setups — if you accidentally configured the
bot scopes to react to its own posts, you'll see infinite loops. Lazybox
filters by `bot_user_id` in `SocketModeClient`, so messages it posts
should never re-enter the inbound stream. Confirm by watching the log
during step 2 — no `slack: routed inbound message` lines should appear
from lazybox's own bootstrap posts.

## 7. Reconnect resilience

Kill the network briefly (turn wifi off / on, or `pfctl` block on
`slack.com:443` for ~30s on Mac):

```sh
# Mac — block then unblock Slack
echo "block out proto tcp to slack.com" | sudo pfctl -ef -
sleep 30
sudo pfctl -d
```

**Expect**:

- `slack: socket disconnected` log line, then within ~5s
  `slack: socket reconnected` (or equivalent).
- No need to restart lazybox.

## 8. Tear-down

Channels lazybox auto-creates persist in Slack. To clean up after testing:

```
/archive
```

in each test channel, then in `#lazybox`:

```
@lazybox stop
```

(if you want to interrupt active sessions) and `Ctrl-C` the daemon.

## Known limitations

- **One lazybox per Slack workspace.** Two lazybox instances will both create
  channels + both respond to mentions. Use one lazybox per Slack workspace.
- **No threading yet.** Each event posts as a top-level message. The
  `channel_strategy: thread_per_workspace` flag in `docs/slack-setup.md` is
  reserved for a later commit.
- **No rate-limit backoff.** Slack tier-3 endpoints (post + create) cap at
  ~50 req/min. Lazybox fires one per Asking transition or first-seen
  workspace, so you'd need a lot of new workspaces in a minute to hit it.
  If you do, you'll see `SlackError::Api("rate_limited")` lines — back
  off manually for now.
