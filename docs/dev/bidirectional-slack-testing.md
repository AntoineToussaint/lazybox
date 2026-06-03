# Bidirectional Slack — manual test plan

Walkthrough for the work in PR #3 (`feat/bidirectional-slack-support`).
Pairs with the existing `docs/dev/slack-testing.md` — that file covers
boot, anchor-channel hello, and the asking-state heuristic. This file
focuses on the new shape:

- **Channel granularity:** one channel per `(session, agent)` pair,
  not per workspace. A workspace with Claude in worktree A and Codex
  in worktree B gets two distinct channels.
- **`ChatProvider` abstraction:** Slack is one adapter; Discord /
  Matrix / IRC plug in without touching dispatch logic.
- **`status` query:** inbound `status` / `state` / `ls` / `list` in
  a channel lazybox can see produces a reply with the agent's state.

## 0. Prereqs

App setup done per `docs/slack-setup.md`: bot installed, both tokens
issued, `/invite @lazybox` run in `#lazybox`.

```sh
export SLACK_BOT_TOKEN=xoxb-...
export SLACK_APP_TOKEN=xapp-...
```

Tail the log in a side pane:

```sh
tail -F /tmp/lazybox.log | egrep -i 'slack|chat'
```

Launch:

```sh
make run
```

Expect within ~2 s of launch:

- Log: `slack: connected team=<ws> user=lazybox`
- Log: `slack: prefetched channel listing channels=<N>`
- `#lazybox` receives `*lazybox online* · connected as @lazybox. Mirroring N project(s).`

If the anchor message doesn't appear, the bot isn't in `#lazybox` — fix
that before continuing.

---

## Part A. Channel-per-(session, agent) — the core new behavior

### A1. First agent in a workspace creates one channel

1. In the TUI, open a workspace.
2. Press `c` to spawn Claude.
3. Watch Slack.

Expect:

- A new channel auto-created with a name like
  `#<workspace-slug>-<8-hex>-claude` — e.g.
  `#github-acme-widget-186-a3f1c277-claude`.
- That channel's first message is a header:
  ```
  🤖 *Fix the date picker keyboard nav* · `claude` session
  reply in this channel to send to the agent
  ```
- Log: `slack: created channel channel=<name>`.

> **Note:** `WorkspaceUpserted` no longer creates a channel by
> itself. Channels are created lazily on first agent spawn. This
> avoids creating channels for workspaces you never open an agent
> in.

### A2. Second agent in the same session = second channel

1. With Claude already running, press `x` in the TUI to spawn Codex
   in the same workspace.

Expect:

- A second channel auto-created: `#<workspace>-<same-8-hex>-codex`.
  Same session prefix (worktree didn't change); different agent
  suffix.
- That channel gets its own header.

### A3. Second session in the same workspace = different prefix

1. In the TUI, create a second session in the same workspace
   (sidebar → expand workspace → `s` to add).
2. Spawn Claude in that new session.

Expect:

- A third channel: `#<workspace>-<NEW-8-hex>-claude`. Different
  8-char session prefix.
- Three channels live for one workspace now.

### A4. TerminalExited does not delete channels

1. From the TUI's terminal stack, kill the Codex session
   (`Ctrl-D` in the terminal, or close the session).

Expect:

- The Codex channel **persists in Slack** (channels accumulate;
  lazybox doesn't trigger admin actions per session-end).
- Lazybox's internal `terminal_to_channel` map drops the entry.
- A subsequent message in that channel hits "untracked channel" and
  is ignored unless it's a `status` query.

---

## Part B. Asking notifications route to the right channel

### B1. Claude asks in worktree A — only A's channel pings

1. Two channels live: `#…-aaa11111-claude` (worktree A) and
   `#…-bbb22222-claude` (worktree B).
2. Trigger a y/n prompt in worktree A's Claude.

Expect:

- The notification posts to A's channel **only**. B's channel
  stays silent.
- The body still includes the recent PTY tail in a code block.

### B2. Paused vs done labels still work

The asking-state heuristic is unchanged from `docs/dev/slack-testing.md`
§3. Run those steps inside any per-(session, agent) channel —
expect `⏸ *paused — input expected*` mid-stream and
`✅ *done — waiting for next task*` after ≥3s quiet.

---

## Part C. Inbound routes by channel → terminal directly

### C1. Single-line reply lands in the right agent

1. In `#…-aaa11111-claude`, post `@lazybox yes`.

Expect:

- The Claude PTY for **session aaa11111** receives `yes\r`.
- Codex (any session) does NOT receive anything.
- Log: `chat: routed inbound message to agent provider=slack terminal_id=…`.

### C2. Same workspace, different agent — no cross-talk

1. In `#…-aaa11111-codex`, post `@lazybox do the codex thing`.

Expect:

- The Codex PTY receives the message.
- Claude (in any session) does NOT.

### C3. Multi-line bracket-paste still works

In any per-(session, agent) channel:

```
@lazybox here is a longer reply:
- first point
- second point
```

Expect:

- The agent receives the whole block as one paste (one dispatch).
- Wire bytes are wrapped in `ESC[200~ … ESC[201~`.

### C4. Untracked channel ignores non-command messages

In `#random` (any channel lazybox doesn't map to a terminal):

```
@lazybox anything
```

Expect:

- Log (at debug): `chat: inbound in untracked channel — ignoring`.
- No PTY write.

> Status keywords still trigger a global reply even in untracked
> channels — see D3.

---

## Part D. `status` query

The new path: inbound messages whose **leading token** is `status`,
`state`, `ls`, or `list` short-circuit before the PTY forward.

### D1. `status` in an agent's own channel

In `#…-aaa11111-claude`, post:

```
@lazybox status
```

Expect a reply like:

```
⏸ *Fix the date picker keyboard nav*
agent: `claude`
session: `aaa11111`
state: ⏸ asking
```

Icon and state reflect `agent_states`:
- `▶` = Active
- `⏸` = Asking
- `·` = no recorded state

### D2. `status` for an exited agent

1. Kill the Claude session in worktree A.
2. In its now-orphaned channel, post `status`.

Expect:

```
❓ this channel's agent is no longer tracked
```

(The chat dispatcher cleared the mapping on `TerminalExited`.)

### D3. `status` in the anchor channel — global summary

In `#lazybox`, post:

```
status
```

(No `@lazybox` prefix needed; the bot has `channels:history` on the
anchor channel and the keyword leads.)

Expect a list of every tracked `(session, agent)` row, grouped by
workspace:

```
📋 *3 agent session(s)*
▶ `Fix the date picker keyboard nav` · claude
⏸ `Fix the date picker keyboard nav` · codex
· `Add OTel parsing for Azure` · claude
```

Capped at 30 rows with a `… and N more` trailer if exceeded.

### D4. Synonym keywords

Each of these should produce the same reply shape:

- `state`
- `ls`
- `list`
- `Status?` (case + punctuation tolerant)
- `ls -la` (everything after the keyword is ignored)

### D5. Non-leading keyword does NOT trigger

In any channel, post:

```
@lazybox what is the status of this PR
```

Expect:

- No status reply.
- If in a tracked agent channel → text routes to PTY as input.
- If in an untracked channel → message ignored.

### D6. Empty inbox

Launch with `--fresh` (no workspaces, no agents). Post `status` in
`#lazybox`:

```
📭 no agent sessions tracked yet
```

### D7. Status reply does not re-enter the agent

After D1, switch to the TUI. Confirm:

- The Claude PTY did **not** receive `status\r` as input.
- Scrollback unchanged.

The short-circuit returns before reaching the PTY-forward branch.

---

## Part E. Failure modes (one-pass sanity)

### E1. Bot removed from a (session, agent) channel

1. `/remove @lazybox` from one per-(session, agent) channel.
2. Trigger an Asking event for that agent.

Expect:

- Log: `chat: post failed: not_in_channel`.
- Lazybox keeps running, other agents still post.

### E2. `per_workspace_channels: false`

Edit `~/.lazybox/config.yaml`:

```yaml
slack:
  per_workspace_channels: false
```

Restart lazybox. Expect:

- No per-(session, agent) channels created.
- Outbound asking notifications are silently dropped (today; future
  work could route to `#lazybox` threads).
- `status` in `#lazybox` still returns the global summary.

### E3. Channel name race

1. Before spawning the agent, manually create
   `#<workspace>-<session>-claude` in Slack with the exact name
   lazybox would have used.
2. Spawn Claude in that session in lazybox.

Expect:

- Log: `slack: channel exists, looking up id` (the `name_taken`
  recovery branch fires).
- Lazybox uses the existing channel — no duplicate, no error.
- Header still posted.

---

## Checklist

- [ ] **Prereqs**: tokens exported, `/invite @lazybox` done, daemon logs tailing
- [ ] **A1** First agent spawn creates `#<ws>-<session>-<agent>`
- [ ] **A2** Second agent in same session = second channel, same session prefix
- [ ] **A3** Second session = different 8-hex prefix
- [ ] **A4** TerminalExited leaves channel in Slack but drops the mapping
- [ ] **B1** Asking routes to the right session-agent channel only
- [ ] **B2** Paused vs done labels still flip on quiet threshold
- [ ] **C1** `@lazybox yes` lands in the right agent
- [ ] **C2** Sibling agent doesn't receive cross-channel messages
- [ ] **C3** Multi-line bracket-paste delivers as one dispatch
- [ ] **C4** Untracked-channel non-keyword chat is ignored
- [ ] **D1** `status` in agent channel reports that agent's state
- [ ] **D2** `status` in an exited-agent channel reports "no longer tracked"
- [ ] **D3** `status` in `#lazybox` returns global summary
- [ ] **D4** Synonyms (`state`, `ls`, `list`, `Status?`, `ls -la`)
- [ ] **D5** Non-leading keyword does NOT trigger
- [ ] **D6** Empty-inbox global status reads `📭 no agent sessions tracked yet`
- [ ] **D7** Status reply does not enter the agent PTY
- [ ] **E1** Bot removed from a channel — logged, doesn't crash
- [ ] **E2** `per_workspace_channels: false` suppresses per-channel posts
- [ ] **E3** Manual pre-existing channel — lazybox finds, doesn't dupe

## Cleanup

Auto-created channels accumulate. To remove them:

```
/archive
```

per channel, or via the Slack admin UI. Lazybox won't auto-unarchive.
