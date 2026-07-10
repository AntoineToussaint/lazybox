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
use lazybox_core::{Scope, ScopeSource};
use std::sync::Arc;
use tuirealm::component::AppComponent;

/// The list of registered scope sources — the executor finds the right
/// one by `provider_id` to enumerate orgs / repos.
pub type ScopeSources = Arc<Vec<Box<dyn ScopeSource>>>;

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
                .label(|s: &Scope| s.label.clone())
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

/// Run an [`Effect`] in the background and deliver its [`LoadResult`]
/// into `result`. The boxed `LoadResult` is later recovered by
/// [`downcast_load_result`] when the Loading modal's tick fires
/// `Msg::LoadingResolved`.
pub fn run_effect(effect: Effect, sources: ScopeSources, result: LoadingResult) {
    tokio::spawn(async move {
        let value: LoadResult = match effect {
            Effect::Detect => LoadResult::Detected(setup::detect_all().await),
            Effect::ListScopes { provider_id } => {
                LoadResult::Scopes(list_scopes(&sources, &provider_id).await)
            }
            Effect::ListChildren {
                provider_id,
                parent_id,
            } => LoadResult::Scopes(list_children(&sources, &provider_id, &parent_id).await),
        };
        // Modal already dismissed (user hit Esc) → drop silently.
        let _ = result.send(value);
    });
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
