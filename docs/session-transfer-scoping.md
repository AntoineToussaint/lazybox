# Transferring a session — scoping (#729)

*How hard is it to move a workspace's live session — its worktree, uncommitted
changes, agent context, and scrollback — somewhere else?*

## Where this sits

Real session serialization/transfer is a first-class, built-in capability of
**mind** — that's the main effort and the place this is done properly. lazybox is
a deliberately constrained experiment: reuse the *existing* agents-in-terminal
model and see how far the boundary pushes **without rocking the boat** (no
architectural rewrite, no giving up the "it's just a terminal running Claude"
model).

So the question here is narrow and commercial, not architectural:

> "Start work on my laptop, finish it on my iPhone" is something a **lot** of
> people want. If lazybox can ship a credible version of that for **minimum
> effort** by reusing what's already here, that's a cheap revenue path worth
> taking. If it needs real infrastructure, it's **not** worth building here —
> mind covers it natively.

This doc scopes the options through exactly that lens: cheapest thing that
reuses the terminal-agent model. The full "portable session bundle" is
interesting engineering, but it's mind's territory; for lazybox the win is
finding the 80/20 that doesn't exist yet as a rewrite.

## TL;DR

| Path | Effort in lazybox | Gets you "finish on iPhone"? | Owner |
|---|---|---|---|
| **Live migrate** a running session | Impossible | — | nobody (physics) |
| **Checkpoint + resume** (move between daemons) | Moderate | Yes, but heavy | really **mind** |
| **Attach/detach to a persistent daemon** | **Mostly already shipped** | **Yes — the cheap path** | **lazybox** |

The punchline: **the money version of "finish on iPhone" is not a transfer at
all.** You don't move the session — you leave it running on a daemon and attach a
thin client from wherever you are. lazybox already has the bones of this
(standalone daemon, `--connect`, the JSON API gateway, structured agent runs
built for non-terminal clients). That's the minimum-effort, don't-rock-the-boat
path. Literal serialization/transfer is the heavier thing, and it's what mind is
for.

## What "a session" actually is

A session is a durable record plus live process state that references it. The
durable state already lives in three distinct keyspaces (this fragmentation is
what makes literal *transfer* fiddly — and why attach/detach, which touches none
of it, is so much cheaper):

1. **Workspace blob** — self-contained JSON keyed by `WorkspaceKey`, one kv row
   `workspace:<key>` (`crates/store/src/sqlite.rs:66`,
   `crates/store/src/traits.rs:46`). Holds `sessions: Vec<Session>`, read/unread,
   `snoozed_until`, automation arms, `notes` (`crates/core/src/workspace.rs:200`+).
   Each `Session` carries `worktree_path`, `worktree_branch`, and
   `provider_session_ids: BTreeMap<String,String>` (agent id → upstream
   conversation uuid) (`crates/core/src/workspace.rs:1572`,`:1591`).
2. **Terminal kv rows** — keyed by `backend_key` (the tmux session name): prompt
   history (`terminal-msgs:<key>`, #523), draft (`terminal-draft:<key>`), agent
   resume context (`terminal-agent-resume:<key>`), agent state, access flags
   (`crates/server/src/spawn_handler.rs:113`).
3. **Scrollback files** — one raw-byte file per session at
   `<home>/v2/scrollback/<session-uuid>`, append-only, capped at 2 MiB
   (`crates/server/src/pty.rs:395`, `crates/core/src/paths.rs:76`). Raw-PTY
   backend only; the tmux backend keeps history in the tmux server.

On top sits **live process state** — the PTY child, its master fd and pid, the
in-memory replay ring, the tmux server (`DaemonPty`,
`crates/server/src/pty.rs:118`). Not durable, not portable; a restart already
cold-respawns it and re-seeds from the scrollback file.

Identity keys are all **provider-derived and machine-independent**: `WorkspaceKey`
(e.g. `github-owner-repo-123`, `crates/core/src/workspace.rs:1123`), `SessionKey`
(`crates/core/src/session_key.rs:25`), the per-session `SessionId` UUID. Two
daemons on two machines derive the *same* keys for the same PR — which is exactly
why a client can attach to a session it didn't start.

## The cheap path: attach/detach to a persistent daemon

"Finish on iPhone" does not require moving anything. Leave the session running on
a daemon (your always-on laptop, or a cheap server); attach a thin client from
the phone. The session never leaves the machine it was born on, so **none** of
the three keyspaces or the live process has to be serialized, bundled, or
re-materialized. This is the tmux-server model, one layer up, and lazybox already
has most of it:

- **Standalone daemon** — `lazybox server start` runs the daemon as a long-lived
  process surviving client disconnects (`crates/tui-boot/src/main.rs`,
  socket at `~/.lazybox/run/daemon.sock`).
- **Remote attach** — `lazybox --connect <socket>` (`run_remote`,
  `crates/tui-boot/src/main.rs:360`) attaches a TUI over `ssh -L`-forwarded
  Unix socket; ring-buffer replay reconstructs the screen on connect.
- **Non-terminal clients** — the JSON HTTP API gateway
  (`crates/server/src/api_gateway.rs`) plus **structured agent runs** (Claude
  launched `-p --input-format stream-json --output-format stream-json`) exist
  *specifically* so a phone/Tauri client can send input, render assistant
  deltas, render tool calls, and interrupt — without a PTY (ROADMAP §4). A
  raw-terminal TUI is the wrong shape for a phone; this normalized event stream
  is the right one, and it's already built.

So the iPhone story is: daemon stays up, phone speaks the structured-agent-run
API to the *same live session* the laptop was driving. The gap is not transfer
machinery — it's:

1. a phone-shaped client over the existing JSON/stream-json API (product work,
   not architecture), and
2. making a remotely-reachable daemon safe: multi-principal auth (ROADMAP §6 —
   today it's single-user, polls with its own credentials) and TLS + token
   rotation (ROADMAP §5 — SSH is the only trust boundary today).

That's the minimum-effort, boundary-pushing, don't-rock-the-boat path, and it's
the one that can make money without competing with mind.

## The heavy path: checkpoint + resume (mind's territory)

If you genuinely want to *move* a session between two daemons (laptop daemon →
server daemon), that's checkpoint + resume: serialize the durable state, ship it,
re-materialize the worktree, cold-respawn the PTY, resume the agent. It's
tractable but it's real work, and it's the thing mind does natively — so for
lazybox this is "only if attach/detach genuinely can't cover the use case."

Why live migration is *not* an option at all (so checkpoint+resume is the
floor): the PTY child, master fd, pid, in-memory ring, and tmux server are
host-local kernel state (`crates/server/src/pty.rs:118`). No terminal
multiplexer relocates a running process — tmux/screen/zellij/dtach/abduco are all
*local* attach/detach; lazybox's tmux `capture-pane` seed is reattach-on-the-same-box
(`crates/server/src/pty.rs:153`), not migration. The only real process-migration
tech is **CRIU**: Linux-only, same-kernel, same-arch, and it breaks on
ptys/sockets/GPU handles — so **mac → laptop → linux → server is a hard no.**

What checkpoint+resume would need if lazybox ever did build it:

- **Already portable (cheap):** identity keys, the workspace JSON blob,
  `provider_session_ids`, the scrollback byte file, and
  **`AgentResumeContext`** (`crates/server/src/agent_auth.rs:12`) — which already
  collects `session_key`, `agent_id`, `cwd`, `provider_session_id`,
  `prompt_history`, `composing_buffer`, access flags. That struct is ~80% of a
  bundle manifest already.
- **Fiddly bit 1 — lossless worktree capture:** today's diff
  (`inspect_worktree_diff`, `crates/git-ops/src/inspect.rs:955`) is
  display-oriented, byte-capped, and truncatable — a review artifact, not a
  round-trippable one. A checkpoint needs new capture (`git bundle` + `git diff
  HEAD` patch + untracked file contents, or `git stash create` + bundle).
- **Fiddly bit 2 — the agent transcript:** lazybox resumes with
  `--resume <uuid>` (`crates/agents/src/agent.rs:599`, Codex `:804`), but that id
  names a transcript file the *agent CLI* keeps locally and **cwd-indexed** —
  Claude at `~/.claude/projects/<cwd-hash>/<uuid>.jsonl`. lazybox persists only
  the uuid, does no transcript-file handling. On the target box `--resume` finds
  nothing unless the `.jsonl` ships with the bundle (and the cwd path-hash
  differs mac vs linux). This is the one genuinely-new piece, identical on any
  OS pair.
- **Transport already exists:** length-prefixed bincode `Command`/`Event` over
  the `ssh -L` Unix socket (`crates/ipc/src/socket.rs`,
  `crates/ipc/src/transport.rs`); a transfer adds export/import variants, not a
  new protocol.

For context, the intra-daemon version of transfer is **already shipped** —
workspace→workspace adopt and issue→PR collapse both route through
`Workspace::absorb_user_state_from` / `absorb_activity_from`
(`crates/core/src/workspace.rs:755`,`:689`) and a live-handle rebadge
(`commit_workspace_move`, `crates/server/src/polling/upsert.rs:802`). That's the
proof the *durable* state is already keyed and movable; only the cross-machine
serialization boundary is missing, and it's mind that should own it.

## Prior art: superlogical

[superlogical](https://www.superlogical.com/) is building exactly the
"start on laptop, finish on phone" pitch — and notably they build it as
**attach/detach to a durable, server-hosted session** (long-lived sessions,
web + macOS/iOS clients, multiplayer), *not* as machine-to-machine transfer.
That's independent confirmation of the split above: nobody serious ships literal
session *serialization/transfer*, because you can't move a running process — the
market has converged on durable-session-plus-thin-client. It also means the cheap
path is a real, validated product shape, not a hack.

Two cautions their approach surfaces:

- **The durable unit is a *terminal*.** A portable byte-stream is a poor phone UX;
  the thing worth preserving is the *work* — task, diff, conversation, and
  structured approve/interrupt actions (which the structured-agent-run stream,
  ROADMAP §4, already normalizes). Betting on "portable terminal" risks
  preserving the emulator instead of the work model.
- **But their stated vision reaches for the work model too** ("a durable session
  around the work itself… structured data and actions"). So it's an abstraction/
  timing difference, not a clean miss — and a reminder that "hosted terminal +
  nice clients" is a thin moat the agent vendors themselves can absorb. Value
  accrues to whoever owns the agent + work model, not the transport.

For lazybox this reinforces the recommendation below: **don't compete on the
infra layer.** Ride the existing agent-in-terminal model with a persistent daemon
and a thin client; leave the durable-work-model bet to mind.

## Recommendation

For lazybox — the constrained, don't-rock-the-boat experiment — **don't build
session serialization/transfer.** That's mind's built-in, and duplicating it here
is exactly the boat-rocking rewrite this project is trying to avoid.

The cheap, credible, sellable version of "start on my laptop, finish on my
iPhone" is **attach/detach to a persistent daemon**, which lazybox already has
the bones for (standalone daemon + `--connect` + JSON API gateway + structured
agent runs for non-terminal clients). The remaining spend is a phone-shaped
client and remote-daemon safety (auth §6, TLS §5) — **product and hardening
work, not session-serialization architecture.** That's the minimum-effort path to
the feature people will pay for, and it stays out of mind's lane.

Checkpoint + resume (daemon-to-daemon *move*) is real but heavier, and it's the
capability mind ships natively — so scope it into lazybox only if attach/detach
provably can't serve the use case, and even then treat it as a bounded one-shot
bridge, not a platform.

> **Update (#1089):** the ephemeral GCP box is that carved-out case — you want to
> *stop paying for the box*, so attach/detach (which requires it to keep running)
> can't serve it. [`session-transfer-adr.md`][transfer-adr] designs the bounded
> one-shot bridge this exception reserved: ring-window output sync, push/bundle
> code transfer, and a per-agent native-resume-or-log-replay handoff.

[transfer-adr]: ./session-transfer-adr.md

**Anchors:** cheap path — standalone daemon + `run_remote`
(`crates/tui-boot/src/main.rs:360`), JSON API (`crates/server/src/api_gateway.rs`),
structured agent runs (ROADMAP §4); intra-daemon transfer (shipped) —
`absorb_user_state_from`/`absorb_activity_from`
(`crates/core/src/workspace.rs:755`,`:689`), `commit_workspace_move`
(`crates/server/src/polling/upsert.rs:802`); heavy path — `AgentResumeContext`
(`crates/server/src/agent_auth.rs:12`), agent resume argv
(`crates/agents/src/agent.rs:599`), scrollback (`crates/server/src/pty.rs:395`,
#468), worktree diff (`crates/git-ops/src/inspect.rs:955`), transport
(`crates/ipc/src/socket.rs`). Related: #672 (remote-daemon offering), ROADMAP §5
(security) / §6 (multi-user auth). Full native session serialization: **mind.**
