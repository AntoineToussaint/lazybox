# Resiliency review (issue #74)

A pass over the 18 crates aimed at state-management fragility — the
class of bug behind the #64 sync regression. Findings are ranked by
severity × confidence. "Verified" means I read the code path and
confirmed the behaviour; "Reported" means it surfaced in the audit but
still needs a repro before acting.

## Applied in this round

| Fix | Where | Status |
| --- | --- | --- |
| **VT parser built in Debug mode froze the UI** — `build.zig` defaults `-Doptimize` to `.Debug`; `build.rs` never overrode it, so the parser that runs on the UI thread shipped ~570× too slow. Under chatty agent output the run loop stalls and buffered keystrokes/events drop. | `crates/libghostty-vt-sys/build.rs` | **Fixed in PR #76** (`-Doptimize=ReleaseFast` + a guard test asserting the linked lib is `ReleaseFast`). |
| **Linear partial fetch wiped issues on rescope** — a page failing mid-pagination returns a prefix, but `LinearSource::polled_scope()` always claimed `Exhaustive`, so rescope deleted every Linear workspace on the un-fetched pages (same wipe-on-partial-sync class as #64). | `crates/server/src/polling/mod.rs` (`LinearSource`), `crates/linear-provider/src/lib.rs` | **Fixed in this PR** (downgrade to `Repos([])` on partial; regression tests). |

## Verified findings worth scheduling

### State reconciliation
- **Issue absorption discards activity + read state** —
  `absorb_issue_workspace` (`crates/server/src/polling/mod.rs`) moves
  sessions and attaches tasks but does not merge the absorbed
  workspace's `activity` / `read_indices`. An issue folded into its PR
  loses its comment history and read marks. *Severity: high. Fix: call
  `merge_activity` before attaching.* Deferred — needs a dedicated
  regression test and care around read-index remapping; larger than
  this PR's scope.

### Persistence — accepted tradeoffs (no action)
- **`commit_upsert` broadcasts even when `save_workspace` fails**
  (`polling/mod.rs:3104`). This is **deliberate and documented**: the
  live UI should reflect current state on a transient DB hiccup, and a
  restart reconciles from the next poll. Left as-is; called out so it
  isn't "fixed" into a UI/daemon divergence later.

## Reported — needs a repro before acting

These came out of an automated sweep and are recorded for triage. Each
needs verification; do not treat as confirmed.

- **`load_workspace` swallows corrupt JSON** — returns `None` (treated
  as "not found") with no log. A truncated record disappears silently.
  Low-risk improvement: log at `error!` before returning `None`.
- **`MemoryStore::list_workspaces` fabricates `created_at`** vs
  `SqliteStore` deriving it from the JSON (`crates/store/src/mock.rs`).
  Test/prod parity gap; harmless in prod but can mask age-sort bugs in
  tests.
- **Unbounded raw-event channels** in the daemon→client path
  (`crates/server/src/pty.rs`, `crates/ipc/src/{socket,channel}.rs`)
  rely on the forwarder draining promptly; a stalled forwarder buffers
  without bound. Note: PR #76 removes the dominant cause of forwarder
  stalls (the Debug-mode VT parser), so this is far less likely to bite
  in practice — re-measure before bounding.
- **Broadcast bus (1024) vs per-client channel (2048) capacity
  asymmetry** — worth documenting the rationale or aligning.

## Audit hygiene note

The `unwrap()`/`expect()` sweep over library crates produced many false
positives — most hits were in `#[cfg(test)]` modules (e.g.
`claude_trust.rs:114`) or on effectively-infallible literals, and the
`.expect("… poisoned")` lock pattern is an accepted idiom (a poisoned
lock already implies a prior panic). The genuinely worth-checking
remainder is parsing of user-controlled input (agent hook output,
`~/.claude.json`) on the daemon thread; verify those degrade rather
than panic before filing fixes.
