# Provider state machines

How lazybox writes to a source — GitHub, Linear — and keeps what it shows
in step with what the source actually holds. Issue
[#1736](https://github.com/AntoineToussaint/lazybox/issues/1736).

Read this before adding a provider mutation, before adding a field a
mutation can claim, and before touching the poll's ingest path.

## The three dimensions

A write tangles three different things that must stay separate:

| Dimension | Lives in | Meaning |
| --- | --- | --- |
| Observed remote facts | `Task` on the workspace | what the provider's last response said |
| Desired user intent | `ProviderOps` on the workspace | what the user asked for, not yet confirmed upstream |
| Operation lifecycle | `PendingMutation::phase` | accepted, sent, acknowledged, uncertain, retrying |

Collapsing them produces the two failures the machinery exists to
prevent:

- a poll reply that left the provider *before* our write carries the
  pre-write value, so applying it verbatim silently undoes the write;
- an error from a write the user has already replaced or withdrawn
  flashes a failure and restores fields nobody is waiting on.

`crates/core/src/provider_ops.rs` owns the first two and the transition
rules; it is pure and does no IO. `crates/server/src/polling/ops.rs` is
the daemon coordinator that persists transitions and runs their effects.

## The projection

Every transition re-projects the entity it concerns
(`ProviderOps::apply(event, &mut task)`), so the ledger and the row it
paints move together and cannot drift. The daemon persists and broadcasts
that projection, which is why there is no client-side copy of the same
intent to disagree with it.

Each pending operation also carries `previous` — what the provider last
said about exactly the fields it claims. A claim that ends *without ever
landing* (rejected, budget spent, cancelled before the wire) puts that
back and re-overlays whatever intent still stands, so the row shows the
truth at once rather than a failed value until the next poll.

`previous` is inherited, not re-read, when a second write stacks on a
field a first write is already painting — otherwise the fallback would be
a value that never landed either. Every observation refreshes it, so it
always names the freshest thing the provider actually told us.

This is what the client-side optimism it replaces could not do: that
correlated a failure to an edit by a source *string*, so it could not
tell a failure belonging to the edit on screen from one belonging to an
edit the user had already replaced.

## Freshness, not arrival order

Neither provider exposes a universal monotonic revision, and a response
arriving later is not evidence it was produced later. The one sound proof
available is the entity's own revision stamp (`Task::updated_at`): a
successful write bumps it, so an observation at or after the moment we
were acknowledged has necessarily seen our write.

An operation settles when any of:

1. the observation's revision is at or after the operation's bar
   (`Acked` → the ack instant; anything else → the request instant);
2. the operation is `Uncertain` — a re-read is by construction the answer
   it was waiting for, whatever it says;
3. the claim has outlived `SETTLE_DEADLINE` (10 min). This closes the one
   hole in the revision proof: a write the provider accepts as a *no-op*
   need not bump `updated_at`, so no future observation could ever be
   shown to postdate it, and the claim would paint the row forever.

Once settled, whatever the provider reports wins — **including a value
that contradicts the write**. That is how a legitimate external edit is
accepted rather than fought.

## Shared transition table

`ProviderOps::apply(event, &mut entity) -> Vec<OpEffect>`:

| Event | Transition | Effects |
| --- | --- | --- |
| `Requested` | bump each claimed field's generation; drop any operation on those fields that never reached the provider; push the new one as `Accepted` | `Settled(dropped…)`, `Send(new)` |
| `Sent` | → `Sent` | — |
| `Acked` | → `Acked { at }` | — |
| `Failed{Transient}`, still current, budget left | → `RetryScheduled` | `Retry` |
| `Failed{Transient}`, budget spent | restore, drop | `Report`, `Settled` |
| `Failed{Rejected}`, still current | restore, drop | `Report`, `Settled` |
| `Failed{*}`, superseded or cancelled | restore, drop, silently | `Settled` |
| `Failed{Uncertain}` (current **or** not) | → `Uncertain` | `Reconcile` |
| `Cancelled`, operation unsent | restore, drop | `Settled` |
| `Cancelled`, operation on the wire | keep | `Reconcile` |
| observation (`reconcile_observation`) | settle per the rules above; refresh surviving baselines | — |

Restart recovery is its own entry point (`ProviderOps::recover`) because
it speaks for every entity a workspace holds at once and drops nothing,
so no row changes:

| Phase on disk | Transition | Effect |
| --- | --- | --- |
| `Accepted` | — | `Send` (it provably never left the daemon) |
| `Sent` / `Uncertain` | → `Uncertain` | `Reconcile` |
| `Acked` | — | — (it settles when an observation catches up) |
| `RetryScheduled` | — | `Retry`, not before *now* |

Invariants the table encodes:

- **A verdict only affects the generation it belongs to.** Generations are
  **per field**, so an assignment and a label edit never obsolete each
  other and a long-running write never blocks an unrelated one.
- **An uncertain outcome is never replayed blind**, whoever owns the
  fields now: the write may have landed. A timeout is not proof of
  failure.
- **Transitions are persisted before effects run.** The coordinator drops
  the effects of a transition it could not commit, so a restart never
  resumes from state no one can reconstruct.
- **Completion is idempotent.** A verdict for an operation that already
  settled is inert.
- **No exactly-once claim.** A crash between the send and its
  acknowledgement leaves an unknown outcome; the answer is a targeted
  re-read, not a replay.

## Provider-specific rules stay in the adapters

Core owns identity, generations, conflict, freshness and lifecycle. It
does not know what a merge queue or a workflow state *means*.

| Rule | Owner |
| --- | --- |
| Which workflow state "close" lands on | `LinearClient::close_issue_by_id` resolves a `canceled`-type state from the **issue's own team**. State ids are per-team configuration, never a global Open/In Progress/Done enum. |
| A Linear issue holds one assignee | `lazybox_linear::narrow_assignees`. Applied to the recorded intent too, so the row mid-flight shows what the write will do. |
| GitHub closes an issue / requests reviewers | the `TaskProvider` impl on `GhClient`. |
| Completion reversibility | neither provider is special-cased. Freshness decides, which is what lets a completed Linear issue legitimately reopen. A verified-merged GitHub PR is terminal by GitHub's own rules and by the merge path (`polling::auto_merge`), not by this ledger. |

`StateWrite` carries the canonical `TaskState` **and** the provider's own
name for the destination. Core never enumerates those names.

## What routes through the coordinator today

| Writer | Command | Status |
| --- | --- | --- |
| Assignees (set) | `SetAssignees` | migrated |
| Assignees (add) | `AddAssignees` | migrated — recorded as the resulting set (`union_with_current`), because replay-safety and the painted value both rest on writes being absolute rather than deltas |
| Labels | `SetLabels` | migrated |
| Reviewers | `RequestReviewers` | migrated — `requestReviews` unions rather than replaces, so it too records the resulting set |
| Close issue (workflow state) | `CloseIssue` | migrated |

Not yet routed, each with its own reason:

| Writer | Why not |
| --- | --- |
| Merge / merge-on-green / native auto-merge (`polling::auto_merge`) | Its own orchestration track, [#1734](https://github.com/AntoineToussaint/lazybox/issues/1734), with a head-OID latch and its own regression set. Folding it in is a second change of comparable size. |
| `close_pr`, `convert_to_draft`, `mark_ready`, `delete_issue`, `update_branch` | GitHub-only PR lifecycle writes with no Linear counterpart; they claim `State` and belong behind the same claim, but each carries its own event surface and cleanup path. |
| `post_reply` | Appends a comment. It claims no field, so there is nothing for an observation to undo and nothing to overlay. |
| Jira, Slack | No mutation paths today. |

When migrating one, the checklist is: express it as an absolute
`DesiredFields` value, route it through `ops::request`, surface the
returned `RequestOutcome` in whatever notice shape that command already
has, and delete whatever optimistic flag the clients kept for it.

## Testing

`crates/core/src/provider_ops.rs` proves the transitions in isolation
with a deterministic clock — reordered and duplicate events, stale
snapshots, overlapping and independent mutations, external changes,
cancel and re-arm, rate limits, ambiguous timeouts, restart recovery.

`crates/server/tests/provider_ops.rs` proves the wiring through the
daemon's real `upsert`: an accepted write is durable before any effect
runs, a stale poll cannot undo it, a fresh external change can.

The send half (`ops::send` → `ProviderHandle`) has no test double:
`ProviderHandle` is a concrete enum over the real GitHub and Linear
clients with no injection seam, so covering it would mean network IO.
Giving it one is the natural next step for this area.
