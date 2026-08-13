# Transferring a session between a GCP box and local — ADR (#1089)

_Design decision + trait sketch for **pulling a live session off a remote GCP
box down to the local machine (and back)**: the code, the agent's state, and the
scrollback, so you can `r c` a Claude session on a box, work on it, then reclaim
it locally and **stop paying for the box**. Parent: epic #885 (remote dev).
Follow-on to r-spawn (#965/#981) and box provisioning (#977/#987)._

**Status:** accepted design; implementation follows in the phases below.
**Investigation-first** per the issue — this lands the decision and the trait
shapes; no transfer code ships in this PR.

## Why this is *not* the case #729 already closed

[`session-transfer-scoping.md`][scoping] (#729) asked "should lazybox build
session serialization/transfer?" and answered **no** — for the *persistent
daemon* framing. There, "finish on my iPhone" is best served by **attach/detach**:
leave the session running on a daemon and attach a thin client from anywhere.
Nothing moves, so none of the three keyspaces has to be serialized. That
recommendation stands for that framing.

**#1089 is the one case #729 explicitly carved out.** Its recommendation ended:
"scope [checkpoint+resume] into lazybox only if attach/detach provably can't
serve the use case, and even then treat it as a **bounded one-shot bridge**, not
a platform." The GCP box is exactly that case:

- The box is **ephemeral and metered** — an `e2-standard-8` at ~$210/mo
  ([`remote-gcp-roadmap.md`][roadmap], Cost posture). The whole point is to
  **stop paying for it**. Attach/detach *requires the box to keep running*; it
  is the anti-goal here.
- So the session genuinely has to **move**, then the box gets stopped. That is
  checkpoint + resume across two daemons — the heavy path #729 sized but declined
  for the general case. #1089 is the bounded, one-directional bridge that path
  was reserved for, not a general "portable session" platform.

This ADR therefore builds the smallest bridge that reclaims the work and lets the
box die — reusing existing machinery wherever the scoping doc already proved it
portable, and adding the three genuinely-new pieces it flagged.

## TL;DR — the three decisions

| Concern | Decision | Why |
|---|---|---|
| **Output / logs** | **Ring-window, not a durable append log.** Generalize the existing per-chunk `seq` into an `OutputSource { latest_offset, read_since }` read-model over the ring lazybox already keeps. | Fidelity comes from **agent resume** replaying the transcript, not from byte-perfect scrollback. A new durable-log subsystem is the rewrite the project avoids. |
| **Code** | **Push the branch when pushable; else `git bundle` + a round-trippable dirty-tree patch** (staged + unstaged + untracked). Never silently drop untracked. | (a) is durable/reviewable; (b) is offline-capable and captures the index. (c) straight `git diff` is rejected — loses untracked + history. |
| **Agent handoff** | **Per-agent `Handoff` capability**: `NativeResume` (sync the transcript, re-spawn with the existing resume argv) where the CLI supports it; `LogReplay` (scrollback splice) otherwise. Every session is transferable, degraded only for non-resumable ones. | Maps directly onto the `Agent` trait's `resume_session` + `provider_session_ids` that already exist. The only net-new piece is **shipping the cwd-indexed transcript file**. |

The unifying insight (issue #3): **agent handoff subsumes output sync.** If we can
resume the agent, replaying its transcript reconstructs the visible output for
free. So the output abstraction (#1) only has to be good enough for the
*degraded* path (shells, non-resumable agents) — which makes ring-window the
right call, not a durable log.

## What exists vs. net-new

Every anchor below is current on `main` (the sandbox + r-spawn tracks landed —
[`remote-gcp-roadmap.md`][roadmap]).

| Piece | State today | Anchor |
|---|---|---|
| Per-terminal replay ring (2 MiB) | **Exists.** Circular byte ring, whole-snapshot replay on reconnect. | `ReplayRing` `crates/server/src/pty.rs:247`; `REPLAY_RING_BYTES` `:45` |
| Monotonic per-chunk `seq` | **Exists** — but as a *gap detector*, not a random-access cursor. | `OutputChunk.seq` `crates/server/src/pty.rs:112`; `last_seq` `:152` |
| On-disk scrollback (raw-PTY only, 2 MiB, session-UUID keyed) | **Exists.** tmux instead reseeds from `capture-pane`. | `ScrollbackLog` `crates/server/src/pty.rs:395`; `scrollback_dir()` `crates/core/src/paths.rs:80` |
| "Read since N" byte API | **Missing.** Only `TerminalResync`/`RequestTerminalResync{required_seq}` return the *whole* ring. | `crates/ipc/src/lib.rs:790,1857` |
| Agent resume argv + exact-conversation resume | **Exists.** Claude `--resume <id>`, Codex `resume <id>`; Cursor none; GenericCli optional. | `Agent::resume_session` `crates/agents/src/agent.rs:239`; Claude `:655`, Codex `:862`, Cursor `:1008` (no override) |
| Resume identity persisted per daemon | **Exists** but **local-only**, keyed by `SessionKey`. | `AgentResumeContext` `crates/server/src/agent_auth.rs:12`; `provider_session_ids` `crates/core/src/workspace.rs:1629` |
| Round-trippable worktree capture | **Missing.** `inspect_worktree_diff` is display-only, byte-capped, truncatable. No bundle/stash/patch transport anywhere. | `inspect_worktree_diff` `crates/git-ops/src/inspect.rs:955`; `DIFF_PATCH_BYTES=4 MiB` `:960` |
| r-spawn box link | **Exists, one-way.** Forwards `Spawn` to the box daemon; box events are drained **and discarded** — no state merge back. | `remote_box.rs::run` `crates/tui-boot/src/remote_box.rs:284` (drain/discard doc `:22`) |
| Intra-daemon handoff UX (`x s`) + `source → target` breadcrumb | **Exists.** Capture visible text → pick target → inject. | `resolve_send_to_session` `crates/tui-core/src/intent.rs:792`; breadcrumb `crates/tui/src/realm/model/modals.rs:2030` |

**The net-new surface is exactly three things:** (1) an `OutputSource` read-model
+ a `read_since(seq)` command; (2) a round-trippable code-capture/apply pair; (3)
transcript-file sync + a cross-daemon resume-context import. Everything else is
reuse.

## Decision 1 — Output: `OutputSource`, ring-window, seq-cursored

lazybox already has a monotonic per-terminal cursor: the chunk `seq`
(`OutputChunk.seq`, `pty.rs:112`). It is used only for gap detection today. The
abstraction the issue asks for is a thin *read-model* that makes local and remote
identical to the puller, cursored on that existing seq:

```rust
/// A terminal's output, readable as a delta from a watermark. Implemented over
/// the local ring directly, and over the box link by round-tripping a
/// `ReadSince` command to the box daemon — the puller can't tell which.
pub trait OutputSource {
    /// High-water seq: the newest chunk this source can serve.
    fn latest_offset(&self) -> u64;

    /// Everything since `offset`. `covers_offset == false` means the ring had
    /// already evicted bytes at/after `offset` — the delta is a *suffix*, not a
    /// gap-free continuation, and the caller must treat older history as lost.
    fn read_since(&self, offset: u64) -> OutputDelta;
}

pub struct OutputDelta {
    pub from_offset: u64,
    pub to_offset: u64,
    pub bytes: Vec<u8>,
    pub covers_offset: bool, // false ⇒ bounded ring dropped older bytes
}
```

Transfer = `read_since(shared_watermark)` on the remote source → splice into the
local terminal's ring + scrollback file. On the wire this is one new command,
`RequestTerminalDelta { terminal_id, since_seq }` → `TerminalDelta { .. }`,
a straight generalization of the existing `RequestTerminalResync{required_seq}`
(`ipc/src/lib.rs:790`) from "whole ring" to "since N".

**Ring-window, not a durable append log** (the issue's headline open question):

- **Fidelity is the agent transcript's job, not the ring's.** For a resumable
  agent, resume replays the full conversation — spliced scrollback is redundant.
  A durable log would buy byte-exact deep history *only* for the degraded path
  (shells, Cursor), which by definition doesn't have a conversation worth
  reconstructing perfectly.
- A per-terminal durable append log is a **new persistence subsystem** (retention,
  compaction, GC, cross-backend parity with tmux which owns its own history).
  That is precisely the boat-rocking rewrite [`session-transfer-scoping.md`][scoping]
  warns against.
- The 2 MiB ring/scrollback window already exceeds a terminal viewport by far;
  `covers_offset` makes truncation **explicit** rather than silent.

**Decision:** ring-window. Reconsider a durable log **only** if a concrete
requirement appears that resume-replay cannot satisfy (e.g. an auditable
full-history export). Recorded as the ADR answer to the deliverable "Decide
ring-window vs. durable append log."

## Decision 2 — Code: push-preferred, bundle+dirty-patch fallback

The box worktree has committed + staged + unstaged + untracked work, and the box
daemon owns it exactly as a local daemon would (`remote_box.rs` forwards an
ordinary `Spawn`; no worktree state comes back today). Two mechanisms, tried in
order:

- **(a) Push the branch to origin, pull locally.** Durable, reviewable. Covers
  only *committed* work, needs a writable remote, and needs an auto-commit of the
  dirty tree first (`wip: transfer <ts>`). Preferred when the branch is pushable.
- **(b) `git bundle` the branch commits not on origin + a round-trippable patch
  of the dirty tree** (staged + unstaged + **untracked**), shipped over the
  control channel. Offline-capable, no remote required, captures the index.
  Fallback when the remote isn't writable / branch isn't pushable.
- **(c) straight `git diff` patch — rejected.** Loses untracked files and commit
  history; a non-starter given "never silently drop untracked."

The capture reuses the **throwaway-index technique** already in
`inspect_worktree_diff` (`inspect.rs:963` — a scratch `GIT_INDEX_FILE` +
`read-tree HEAD` + `add -N -A`), but the display function itself is unfit
(byte-capped at 4 MiB, truncatable — `:960`). Net-new is a **sibling capture that
is lossless and round-trippable**, plus untracked-file contents:

```rust
pub enum CodeTransfer {
    /// Branch pushed to `origin`; local side pulls. `wip_commit` is the
    /// auto-committed dirty tree (None if the tree was clean).
    Pushed { branch: String, wip_commit: Option<String> },
    /// Offline bundle + dirty-tree capture, carried over the control channel.
    Bundle {
        bundle: Vec<u8>,                    // git bundle of commits not on origin
        tracked_patch: Vec<u8>,             // round-trippable `git diff HEAD` (uncapped)
        untracked: Vec<(PathBuf, Vec<u8>)>, // contents, never dropped
    },
}
```

**Reconcile with an existing / diverged local worktree** (the issue's open Q):
apply onto a **fresh transfer branch** (`transfer/<branch>-<ts>`), never clobber
the user's existing checkout. If a worktree for the same branch already exists and
is **clean**, fast-forward it; if it has **diverged**, refuse the in-place apply,
land the transfer on the fresh branch, and surface a notice naming both — the
user reconciles with their normal git tools. A transfer must never be a silent
overwrite of local work.

## Decision 3 — Agent handoff: a per-agent `Handoff` capability

Handoff subsumes output sync where the agent is resumable. Model it as a
capability the `Agent` trait already all-but-declares (`resume_session` at
`agent.rs:239` is the hook):

```rust
pub enum Handoff {
    /// Locate the CLI's own transcript/session store, sync it to the target
    /// daemon, re-spawn with the existing resume argv.
    NativeResume {
        /// The transcript file(s) to ship, relative to the agent's home.
        /// Claude: ~/.claude/projects/<cwd-hash>/<session-id>.jsonl
        transcript_paths: fn(&SpawnCtx, session_id: &str) -> Vec<PathBuf>,
    },
    /// No resumable store: splice scrollback via `OutputSource` and re-spawn
    /// fresh. Degraded fidelity, but every session stays transferable.
    LogReplay,
}

impl dyn Agent {
    fn handoff(&self) -> Handoff; // default: LogReplay
}
```

Per built-in (verified against `agent.rs`):

| Agent | Capability | Basis |
|---|---|---|
| **Claude** | `NativeResume` | `--resume <id>` (`:655`); transcript at `~/.claude/projects/<cwd-hash>/<id>.jsonl` |
| **Codex** | `NativeResume` | `codex resume <id>` (`:862`) |
| **Cursor** | `LogReplay` | no `resume` override (`:1008`) — no session store |
| **GenericCli** | `NativeResume` iff `resume_cmd` set, else `LogReplay` | `resume_cmd` optional (`:1070`) |
| **Shell** | `LogReplay` | no session concept |

The resume **identity** already exists and is ~80% portable
(`AgentResumeContext` — `session_key`, `cwd`, `provider_session_id`,
`prompt_history`, `composing_buffer`; `agent_auth.rs:12`) and
`provider_session_ids` (`workspace.rs:1629`). The transfer imports that context
into the target daemon under the same `SessionKey` (both daemons derive
identical keys — the machine-independence [`session-transfer-scoping.md`][scoping]
established). **The one genuinely-new piece** is shipping the transcript file
itself: it is **cwd-indexed by a path hash that differs mac↔linux**, so
`transcript_paths` recomputes the target-side hash and the sync rewrites the
directory name. This is identical on any OS pair.

**Does resume need the agent's home to match, or just the transcript** (open Q)?
Just the transcript **moves**; the home (`~/.claude`, MCP config, creds) must
*exist* on the target but is not transferred. For **box → local** (the primary
direction) the target home is the user's real machine — already complete. For
**local → box** the box home is provisioned build-matched by #977, so it is
present too. So: transcript is the moving part; home/creds are a target-side
precondition, satisfied by both directions already.

## Decision 4 — UX: `r t` transfer-here, reverse via the `r` leader

Mirror `x s` (send-to-session) across the local↔remote boundary, reusing its
picker + breadcrumb machinery (`modals.rs:1968`, breadcrumb `:2030`):

- **From a `⇅ <box>` workspace: `r t` — transfer here.** Progress modal steps:
  `syncing code → syncing session → resuming locally`. On success a local
  workspace opens focused, resuming the agent; the box is then offered for
  idle-stop (the point of the whole feature).
- **Reverse — send-to-remote** from a local workspace folds into the existing `r`
  remote leader (same flow, opposite direction).
- **Breadcrumb:** the transferred workspace keeps a `box → local` (or
  `local → box`) trail, exactly like the `source → target` notice on `x s`.
- **Degraded path** (`LogReplay` agent or a shell): still transfers code +
  spliced scrollback, with a notice that the agent restarts fresh.

This closes the one-way gap `remote_box.rs` documents (`:22` — box events drained
and discarded, box→Model merge deferred): `r t` is the first path that pulls real
state *back* from the box.

## Phased implementation (follow-up PRs)

1. **`OutputSource` + `read_since`** — the read-model over the ring, and the
   `RequestTerminalDelta`/`TerminalDelta` IPC pair. Local-only first (splice a
   local terminal's delta into another), fully testable without a box.
2. **Round-trippable code capture/apply** — the lossless sibling to
   `inspect_worktree_diff` + `CodeTransfer` apply onto a fresh transfer branch.
   Boundary-tested against real git (bundle + untracked round-trip).
3. **`Handoff` capability + transcript sync + resume-context import** — the
   cross-daemon piece; the cwd-hash remap is the sharp edge to test on a mac↔linux
   pair.
4. **`r t` UX + progress modal + breadcrumb + idle-stop offer** — wiring over
   1–3, reusing the `x s` picker.

Each phase is independently landable and useful; 1–2 have no box dependency.

## Risks / decisions

- **Ring-window loses deep scrollback for the degraded path.** Accepted:
  `covers_offset=false` makes it explicit; resumable agents are unaffected
  because resume replays the transcript.
- **`wip: transfer` auto-commit mutates history on the pushable path.** Named to
  the user; reversible with a normal `git reset` after transfer. The bundle path
  avoids it entirely.
- **Transcript sync assumes the CLI's on-disk format is stable.** It is the
  agent CLI's own file; if the vendor changes it, `NativeResume` degrades to
  `LogReplay` rather than failing — the capability is a fallback ladder, not a
  hard dependency.
- **Never a silent overwrite.** Diverged local worktree ⇒ fresh transfer branch +
  notice, never clobber (Decision 2).

## Anchors

Output — `ReplayRing`/`OutputChunk.seq` (`crates/server/src/pty.rs:247,112`),
`ScrollbackLog` (`:395`), resync IPC (`crates/ipc/src/lib.rs:790,1857`). Agent —
`Agent::resume_session` (`crates/agents/src/agent.rs:239`), Claude/Codex/Cursor
(`:655,:862,:1008`), `AgentResumeContext` (`crates/server/src/agent_auth.rs:12`),
`provider_session_ids` (`crates/core/src/workspace.rs:1629`). Code —
`inspect_worktree_diff` (`crates/git-ops/src/inspect.rs:955`). Remote —
`remote_box.rs::run` (`crates/tui-boot/src/remote_box.rs:284`), `⇅` badge
(`crates/tui/src/components/workspace_row.rs:892`). Handoff UX — `x s`
(`crates/tui-core/src/intent.rs:792`, `crates/tui/src/realm/model/modals.rs:2030`).
Related: [`session-transfer-scoping.md`][scoping] (#729, the general case),
[`obin-remote-dev-scoping.md`][obin] (epic #885), [`remote-gcp-roadmap.md`][roadmap].

[scoping]: ./session-transfer-scoping.md
[roadmap]: ./remote-gcp-roadmap.md
[obin]: ./obin-remote-dev-scoping.md
