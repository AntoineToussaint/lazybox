//! Setup wizard — layers 2 & 3 (renderer + executor).
//!
//! [`SetupRunner`](crate::setup_flow::SetupRunner) is pure: it speaks in
//! [`Screen`] descriptors and [`Effect`] data. This module is the only
//! place that turns those into tuirealm widgets and tokio tasks, so the
//! state machine itself stays free of rendering and IO.
//!
//! - [`render`] maps a [`Screen`] to a concrete modal widget (plus, for
//!   loading screens, the [`LoadingResult`] handle the executor delivers
//!   into).
//! - [`run_effect`] performs an [`Effect`] against the registered
//!   [`ScopeSource`]s and sends a [`LoadResult`] back through that handle.
//! - [`downcast_load_result`] recovers the typed [`LoadResult`] from the
//!   opaque loading payload once, at the Model boundary.

use crate::realm::components::{
    choice::Choice,
    error::{Accent, ErrorModal},
    loading::{Loading, LoadingPayload, LoadingResult},
    splash::Splash,
};
use crate::realm::{Msg, UserEvent};
use crate::setup;
use crate::setup_flow::{
    Effect, FilterOption, InfoKind, LoadResult, Screen, ToolChoice, provider_display,
};
use lazybox_core::{ProviderError, Scope, ScopeSource};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tuirealm::component::AppComponent;

/// The list of registered scope sources — the executor finds the right
/// one by `provider_id` to enumerate orgs / repos.
pub type ScopeSources = Arc<Vec<Box<dyn ScopeSource>>>;

/// Repo-picker row label: the repo's full name, tagged with a lock
/// marker when it's private so a mixed org's scopes are easy to scan.
fn repo_pick_row_label(s: &Scope) -> String {
    if s.private {
        format!("{} 🔒 private", s.label)
    } else {
        s.label.clone()
    }
}

/// Turn a pure [`Screen`] into a mountable modal widget. For
/// [`Screen::Loading`] the second tuple element is the producer handle
/// the executor delivers the effect's result into; every other screen
/// returns `None` there.
pub fn render(screen: Screen) -> (Box<dyn AppComponent<Msg, UserEvent>>, Option<LoadingResult>) {
    match screen {
        Screen::Splash => (Box::new(Splash::new()), None),

        Screen::Providers { items, selected } => (
            Box::new(
                Choice::multi(
                    "Where do your tasks come from?  Lazybox polls these for new \
                     PRs, issues, and tickets so you don't have to refresh.",
                    items,
                )
                .title("Setup · providers")
                .label(|c: &ToolChoice| c.label())
                .selectable(|c: &ToolChoice| c.found)
                .selected_mask(selected)
                .with_refresh(true)
                .with_back(true),
            ),
            None,
        ),

        Screen::Agents { items, selected } => (
            Box::new(
                Choice::multi(
                    "Which AI coding agents should lazybox let you spawn into a \
                     worktree?  Press `a` then `c`/`x`/`u` on a row to drop into them.",
                    items,
                )
                .title("Setup · agents")
                .label(|c: &ToolChoice| c.label())
                .selectable(|c: &ToolChoice| c.found)
                .selected_mask(selected)
                .with_refresh(true)
                .with_back(true),
            ),
            None,
        ),

        Screen::Filter {
            provider_id,
            options,
            selected,
        } => {
            let display = provider_display(&provider_id);
            (
                Box::new(
                    Choice::multi(
                        format!(
                            "Which {display} items show up in your inbox?  \
                             Untick everything in a section to skip that item type entirely."
                        ),
                        options,
                    )
                    .title(format!("Setup · {display} · filters"))
                    .label(|f: &FilterOption| f.label.clone())
                    .section_for(filter_section_for(&provider_id))
                    .selected_mask(selected)
                    .with_back(true),
                ),
                None,
            )
        }

        Screen::Loading { title, label } => {
            let (modal, result) = Loading::pending(label);
            (Box::new(modal.title(title)), Some(result))
        }

        Screen::ScopePick {
            provider_id,
            scopes,
            selected,
        } => (
            Box::new(
                Choice::multi(format!("{provider_id} · pick orgs (none = all)"), scopes)
                    .title("Setup · scopes")
                    .label(|s: &Scope| match &s.parent {
                        Some(p) => format!("{p} / {}", s.label),
                        None => s.label.clone(),
                    })
                    .selected_mask(selected)
                    .allow_empty(true)
                    .with_back(true),
            ),
            None,
        ),

        Screen::RepoPick {
            parent_label,
            scopes,
            selected,
            ..
        } => (
            Box::new(
                Choice::multi(
                    format!(
                        "Pick the {parent_label} repos to subscribe to.\n\n\
                         Space toggles a repo. Enter confirms.\n\
                         Backspace goes back without changing the existing \
                         subscription.",
                    ),
                    scopes,
                )
                .title(format!("Setup · {parent_label} repos"))
                .label(repo_pick_row_label)
                .selected_mask(selected)
                .with_back(true),
            ),
            None,
        ),

        Screen::Info { title, kind, body } => (
            Box::new(ErrorModal::new(title, accent_for(kind), body)),
            None,
        ),
    }
}

/// How long an effect may run before it is abandoned as a retryable
/// error. A hung scope listing (provider that never responds, e.g. after
/// a sleep/wake stalls its connection) must not leave the wizard stuck on
/// the spinner: on expiry we deliver a `Retryable` error so the runner
/// shows a dismissable error screen and the user continues setup, rather
/// than a bare dismiss that cancels the whole wizard. Kept below the
/// `Loading` modal's own liveness backstop so this graceful path always
/// wins; the modal timeout only covers a result that is produced but
/// *lost* (dropped on an overflowed event channel).
const EFFECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Run an [`Effect`] in the background and deliver its [`LoadResult`]
/// into `result`. The boxed `LoadResult` is later recovered by
/// [`downcast_load_result`] when the Loading modal's tick fires
/// `Msg::LoadingResolved`.
pub fn run_effect(
    effect: Effect,
    sources: ScopeSources,
    detector: Option<crate::realm::SetupDetector>,
    result: LoadingResult,
) {
    tokio::spawn(async move {
        let value: LoadResult = match effect {
            Effect::Detect => {
                let report = match detector {
                    // Tool detection is mostly local but can still stall
                    // (a spawned probe that hangs); bound it and fall back
                    // to an empty report so the wizard advances instead of
                    // spinning.
                    Some(detect) => match tokio::time::timeout(EFFECT_TIMEOUT, detect()).await {
                        Ok(report) => report,
                        Err(_) => {
                            tracing::warn!(
                                timeout_ms = EFFECT_TIMEOUT.as_millis() as u64,
                                "setup detect timed out — yielding an empty report"
                            );
                            setup::SetupReport { tools: Vec::new() }
                        }
                    },
                    // No detector installed (a wizard driven without the
                    // boot crate's detection hook): yield an empty report
                    // rather than pretending everything is present.
                    None => {
                        tracing::warn!("setup re-detect requested but no detector installed");
                        setup::SetupReport { tools: Vec::new() }
                    }
                };
                LoadResult::Detected(report)
            }
            Effect::ListScopes { provider_id } => LoadResult::Scopes(
                bounded_scopes(
                    EFFECT_TIMEOUT,
                    &provider_id,
                    list_scopes(&sources, &provider_id),
                )
                .await,
            ),
            Effect::ListChildren {
                provider_id,
                parent_id,
            } => LoadResult::Scopes(
                bounded_scopes(
                    EFFECT_TIMEOUT,
                    &provider_id,
                    list_children(&sources, &provider_id, &parent_id),
                )
                .await,
            ),
        };
        // Modal already dismissed (user hit Esc) → drop silently.
        let _ = result.send(value);
    });
}

/// Await a scope-listing future under a timeout. On expiry, return a
/// `Retryable` [`ProviderError`] — the variant the runner turns into a
/// dismissable error screen that continues the wizard — instead of
/// letting the caller hang.
async fn bounded_scopes(
    timeout: Duration,
    source: &str,
    fut: impl Future<Output = Result<Vec<Scope>, ProviderError>>,
) -> Result<Vec<Scope>, ProviderError> {
    match tokio::time::timeout(timeout, fut).await {
        Ok(res) => res,
        Err(_) => {
            tracing::warn!(
                source,
                timeout_ms = timeout.as_millis() as u64,
                "setup scope listing timed out — surfacing a retryable error"
            );
            Err(ProviderError::retryable(
                source,
                format!(
                    "timed out after {}s — the provider didn't respond",
                    timeout.as_secs()
                ),
            ))
        }
    }
}

/// Recover the typed [`LoadResult`] from the opaque loading payload.
/// Returns `None` only if the payload wasn't a `LoadResult` — a
/// programming error the Model treats as a dismiss.
pub fn downcast_load_result(payload: LoadingPayload) -> Option<LoadResult> {
    payload.downcast::<LoadResult>().ok().map(|b| *b)
}

async fn list_scopes(
    sources: &ScopeSources,
    provider_id: &str,
) -> Result<Vec<Scope>, lazybox_core::ProviderError> {
    match sources.iter().find(|s| s.provider_id() == provider_id) {
        Some(src) => src.list_scopes().await,
        None => Ok(Vec::new()),
    }
}

async fn list_children(
    sources: &ScopeSources,
    provider_id: &str,
    parent_id: &str,
) -> Result<Vec<Scope>, lazybox_core::ProviderError> {
    match sources.iter().find(|s| s.provider_id() == provider_id) {
        Some(src) => src.list_children(parent_id).await,
        None => Ok(Vec::new()),
    }
}

/// GitHub splits filter roles by item type (PR vs Issue) into sections;
/// other providers show a flat list.
fn filter_section_for(provider_id: &str) -> fn(&FilterOption) -> &'static str {
    match provider_id {
        "github" => |f: &FilterOption| {
            if f.key.starts_with("pr.") {
                "Pull Requests  ·  your relationship"
            } else if f.key.starts_with("issue.") {
                "Issues  ·  your relationship"
            } else {
                ""
            }
        },
        _ => |_| "",
    }
}

/// Map a pure [`InfoKind`] to a themed accent pill.
fn accent_for(kind: InfoKind) -> Accent {
    match kind {
        InfoKind::Auth => Accent::new("auth", crate::theme::current().hover),
        InfoKind::Retryable => Accent::warn("retryable"),
        InfoKind::Permanent => Accent::error("permanent"),
        InfoKind::Notice => Accent::warn("notice"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_core::ScopeKind;

    fn repo(label: &str, private: bool) -> Scope {
        Scope {
            id: format!("github:{label}"),
            label: label.to_string(),
            parent: Some("github:acme".into()),
            kind: ScopeKind::Repo,
            private,
        }
    }

    #[test]
    fn private_repo_row_gets_lock_marker() {
        assert_eq!(
            repo_pick_row_label(&repo("acme/web", true)),
            "acme/web 🔒 private"
        );
    }

    #[test]
    fn public_repo_row_is_plain() {
        assert_eq!(repo_pick_row_label(&repo("acme/web", false)), "acme/web");
    }

    #[tokio::test]
    async fn bounded_scopes_passes_a_result_through_before_the_deadline() {
        let out = bounded_scopes(Duration::from_secs(5), "github", async {
            Ok(vec![repo("acme/web", false)])
        })
        .await;
        assert_eq!(out.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn bounded_scopes_times_out_into_a_retryable_error() {
        // A provider that never responds must not hang the wizard — it
        // must surface as a Retryable error (the variant the runner turns
        // into a dismissable "continue" screen), not spin until the modal
        // backstop cancels setup outright.
        let never = async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(Vec::new())
        };
        let out = bounded_scopes(Duration::from_millis(20), "github", never).await;
        assert!(
            matches!(out, Err(ProviderError::Retryable { .. })),
            "a timed-out scope listing must be a retryable error, got {out:?}"
        );
    }
}
