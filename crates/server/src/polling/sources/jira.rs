//! Jira poll source: wraps [`lazybox_jira::JiraClient`] as a
//! [`TaskSource`]. Read-only — Jira rows surface in the inbox for
//! triage and open-in-browser; there is no mutation path.

use super::super::{PolledScope, TaskSource};
use lazybox_core::Task;
use lazybox_jira::JiraClient;
use std::future::Future;
use std::pin::Pin;

pub(crate) struct JiraSource {
    client: JiraClient,
}

impl JiraSource {
    pub(crate) fn new(client: JiraClient) -> Self {
        Self { client }
    }
}

impl TaskSource for JiraSource {
    fn name(&self) -> &str {
        lazybox_jira::SOURCE
    }

    /// The single-page assignee JQL is the complete in-scope set (an
    /// assignment queue deeper than the page cap is out of scope by
    /// design), so a fetched result is authoritative: an issue absent
    /// from it was resolved or unassigned and its row should retire.
    fn polled_scope(&self) -> PolledScope {
        PolledScope::Exhaustive
    }

    fn fetch<'a>(
        &'a self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<Task>, lazybox_core::ProviderError>> + Send + 'a>>
    {
        Box::pin(async move { self.client.fetch_assigned().await.map_err(Into::into) })
    }
}
