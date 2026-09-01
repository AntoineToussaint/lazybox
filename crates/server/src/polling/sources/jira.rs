//! Jira poll source: wraps [`lazybox_jira::JiraClient`] as a
//! [`TaskSource`]. Read-only — Jira rows surface in the inbox for
//! triage and open-in-browser; there is no mutation path.

use super::super::{PolledScope, TaskSource};
use lazybox_core::Task;
use lazybox_jira::JiraClient;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};

pub(crate) struct JiraSource {
    client: JiraClient,
    /// Set by the last [`TaskSource::fetch`]: `true` when Jira signalled
    /// more assigned issues than that fetch returned. Read afterwards by
    /// [`TaskSource::polled_scope`] (the trait calls them in that order)
    /// to decide whether the fetch was authoritative.
    last_truncated: AtomicBool,
}

impl JiraSource {
    pub(crate) fn new(client: JiraClient) -> Self {
        Self {
            client,
            last_truncated: AtomicBool::new(false),
        }
    }
}

impl TaskSource for JiraSource {
    fn name(&self) -> &str {
        lazybox_jira::SOURCE
    }

    /// Authoritative ONLY when the last fetch saw the whole assignee set.
    /// When it was truncated (Jira returned a `nextPageToken`), report
    /// "no coverage" — an empty [`PolledScope::Repos`] — so rescope
    /// preserves every stored Jira row this tick. Claiming `Exhaustive`
    /// on a truncated page is the flapping/data-loss bug: with a queue
    /// past the page cap, `ORDER BY updated DESC` shuffles which issues
    /// land on the page, so a row that merely fell below the cap (still
    /// assigned, still unresolved) would be deleted and later re-added —
    /// churning the sidebar and dropping any snooze/read state it held.
    /// A resolved/unassigned issue is retired on the next un-truncated
    /// sweep instead.
    fn polled_scope(&self) -> PolledScope {
        scope_for(self.last_truncated.load(Ordering::Relaxed))
    }

    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>> + Send + 'a>>
    {
        Box::pin(async move {
            let assigned = self.client.fetch_assigned().await?;
            self.last_truncated
                .store(assigned.truncated, Ordering::Relaxed);
            Ok(assigned.tasks)
        })
    }
}

/// A truncated fetch reports "no authoritative coverage" (an empty
/// `Repos` scope) so rescope preserves every Jira row this tick; a
/// complete fetch reports `Exhaustive` so resolved/unassigned rows
/// retire. Split out so the (crucial) truncation branch is testable
/// without a live `JiraClient`.
fn scope_for(truncated: bool) -> PolledScope {
    if truncated {
        PolledScope::Repos(Vec::new())
    } else {
        PolledScope::Exhaustive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncated_fetch_is_not_authoritative() {
        // The bug: always-`Exhaustive` let rescope DELETE assigned rows
        // that merely fell below the 100-row cap. A truncated fetch must
        // preserve everything instead.
        assert!(matches!(scope_for(true), PolledScope::Repos(v) if v.is_empty()));
        assert!(matches!(scope_for(false), PolledScope::Exhaustive));
    }
}
