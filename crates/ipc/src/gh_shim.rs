//! The contract between a session's `gh` shim and the daemon (#1801).
//!
//! #1799 handed a spawned session the tracker record the daemon had already
//! fetched, which helps only an agent that *chooses* to read it. A session
//! that runs `gh issue view`, `gh api` or `gh search` anyway still spends the
//! shared 5,000/hour budget with no quota and no dedupe, and a mutation made
//! inside a workspace stays invisible to the daemon until the next sweep —
//! which is how twenty issues closed with `gh` were still showing open forty
//! minutes later, with the poller out of budget to re-read them.
//!
//! So the shim asks before it spends ([`crate::Command::GhAdmit`]) and tells after
//! it has spent ([`crate::Command::GhCompleted`]). Three things fall out of those
//! two messages:
//!
//! - **Dedupe.** An identical read already answered within the TTL comes back
//!   as [`GhVerdict::Cached`] — the second session's `gh issue view 12` costs
//!   nothing upstream.
//! - **Per-session quota.** A session fanning out hundreds of reads is paced
//!   by its own bucket, and stopped outright while the governor's reserve is
//!   at risk, so it can never be the reason the poller starves.
//! - **Change signal.** A mutation reports what it did, and the daemon writes
//!   that state onto its cached row immediately. Flipping a closed issue's row
//!   costs zero GitHub calls, which is the whole point: it works with the
//!   budget at zero.
//!
//! The shim never *needs* any of this. Admission failing, or the daemon not
//! running at all, degrades to running real `gh` unchanged — the shim is a
//! coordination point, not a gate on the agent's work.

use serde::{Deserialize, Serialize};

/// Environment variable carrying the directory the shims are installed in.
/// Set on every spawn, and read back by the shim so it can drop that directory
/// from `PATH` when it goes looking for the real `gh` — the one guard that
/// makes re-entry structurally impossible rather than merely unlikely.
pub const SHIM_DIR_ENV: &str = "LAZYBOX_GH_SHIM_DIR";

/// Set to `0` / `false` to make the shim exec real `gh` immediately, before it
/// parses anything or opens a socket. The escape hatch for a session that must
/// bypass lazybox entirely; `gh.real` in the shim directory is the other.
pub const SHIM_OPT_OUT_ENV: &str = "LAZYBOX_GH_SHIM";

/// What the shim made of the invocation's subcommand.
///
/// The distinction that matters is deferrability. A read can wait — the agent
/// gets the same answer a minute later — so a read is the thing that yields
/// when the shared budget is down to the reserve the poller needs. A mutation
/// cannot: refusing `gh pr merge` strands the task rather than delaying it, so
/// a mutation is paced by the session's own bucket and never held at the
/// reserve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum GhCallKind {
    /// A subcommand the shim recognises as a read, and whose stdout is
    /// therefore safe to serve to an identical read from another session.
    Read,
    /// A subcommand the shim recognises as changing a record.
    Mutation,
    /// Anything else. Passed through untouched and never cached — the shim
    /// cannot know whether an unrecognised invocation is safe to replay — but
    /// still paced, because this is where an unclassified fan-out (`gh api`
    /// in a loop) would otherwise hide.
    Other,
}

/// What the daemon says when a session asks to run one `gh` invocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum GhVerdict {
    /// Spend the call: the session's bucket had a token and the governor's
    /// reserve is intact.
    Allow,
    /// Byte-for-byte stdout of an identical read answered within the TTL.
    /// Reproducing `gh`'s own output rather than re-rendering lazybox's cached
    /// [`lazybox_core::Task`] is deliberate: an agent parses what `gh` prints,
    /// and a lookalike rendering would diverge from it silently.
    Cached { stdout: String },
    /// The session has outrun its quota, or the shared budget is into the
    /// reserve the poller needs. `wait_secs` is how long until it is worth
    /// asking again; `reason` is what to tell the agent if it asks why.
    Throttle { wait_secs: u64, reason: String },
}

/// The daemon's answer to either shim message, on one correlated channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum GhReply {
    /// Answer to [`crate::Command::GhAdmit`].
    Admission(GhVerdict),
    /// Answer to [`crate::Command::GhCompleted`]: the outcome is filed.
    ///
    /// The shim exits the instant `gh` has run, and the socket's writer is a
    /// background task, so without an ack to wait on the report would race
    /// process teardown and be lost exactly when it matters — every mutation
    /// the daemon never hears about is a row that stays stale.
    Recorded,
}

/// What a `gh` mutation did, in terms the daemon can apply to its cached row
/// without spending a GitHub call.
///
/// [`Self::Touched`] is the honest fallback: a comment, an edit or a brand-new
/// record changes content the daemon cannot synthesize, so it only invalidates
/// the read cache and wakes the poller, which re-reads when budget allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub enum GhChangeKind {
    /// The record is now closed (an issue closed, a PR closed unmerged).
    Closed,
    /// The record is open again.
    Reopened,
    Merged,
    /// Content changed in a way only GitHub holds — a comment, an edit, a
    /// record that did not exist before.
    Touched,
}

/// A mutation a session just made, addressed at the record it changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "desktop-contract", derive(ts_rs::TS))]
pub struct GhRecordChange {
    /// The record as the shim resolved it: `owner/repo#N`, a GitHub URL, or a
    /// bare `#N` the daemon resolves against `repo`.
    pub reference: String,
    /// `owner/repo` the shim read off the worktree's `origin` remote, for the
    /// repo-less forms. Resolved locally, so naming the record costs nothing.
    pub repo: Option<String>,
    pub kind: GhChangeKind,
}
