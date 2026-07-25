//! Lazybox's UI on `tuirealm`. Lives parallel to the `tui-kit`-based
//! tree (`crate::app`, `crate::components`) during the migration;
//! once every pane + modal is ported here and the new `app::run`
//! works end-to-end, the old code is deleted and `crate::tui_kit`
//! becomes unused.
//!
//! ## How it's organized
//!
//! ```text
//! realm/
//! ├── mod.rs         this file — re-exports + the Msg/Id types
//! ├── model.rs       the Application + main loop (lazybox's `App`
//! │                  equivalent under tuirealm)
//! └── components/    one file per pane / modal port
//!     ├── splash.rs
//!     ├── error.rs
//!     ├── ...
//! ```
//!
//! ## Naming conventions during the migration
//!
//! - Old `Pane`/`Modal` impls live in `crate::components::*` and use
//!   `tui_kit::*`. **Don't touch them** — they're still load-bearing
//!   for `crate::app::run`.
//! - New ports live in `crate::realm::components::*` and use
//!   `tuirealm::*`. Reuse render functions / state structs from the
//!   old impls where it's clean — duplicate them and edit when it's
//!   not.
//!
//! ## What's lazybox-domain (stays) vs framework-shaped (rewires)
//!
//! - **State + render bodies + helpers** — copy verbatim from
//!   `crate::components::*`. The ratatui calls work identically
//!   inside `Component::view`.
//! - **Trait impl + key routing** — rewrite. `Pane::handle_key
//!   → PaneOutcome` becomes `AppComponent::on(&Event) → Option<Msg>`.

pub mod components;
pub mod keymap;
pub(crate) mod layout;
pub mod model;
pub(crate) mod setup_ctx;
pub(crate) mod setup_screen;
pub(crate) mod status_ctx;
pub mod user_event;

pub use model::{ChoicePayload, Id, Model, Msg};
pub use setup_ctx::{SetupCompleteHook, SetupDetector, SetupSaveResult};
pub use user_event::UserEvent;

/// Two clicks on the same target within this window count as a
/// double-click. Shared by the pane mouse router (`model::keys`) and
/// the modal button handlers so they can't drift apart.
pub(crate) const DOUBLE_CLICK_WINDOW: std::time::Duration = std::time::Duration::from_millis(400);
