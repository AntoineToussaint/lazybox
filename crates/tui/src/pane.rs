//! Lightweight pane support types — `PaneId`, `PaneOutcome`,
//! `DetachSpec`, `Binding`. Originally lived in the `tui-kit` crate;
//! kept here as plain types pilot's panes use directly.

/// Stable id for a Pane. Allocated by the host app at construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PaneId(u32);

impl PaneId {
    /// Construct a `PaneId` from a raw integer.
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }
    /// The underlying integer.
    pub const fn raw(self) -> u32 {
        self.0
    }
}

/// One entry in a Pane's keymap. Drives the bottom hint bar and the
/// help overlay.
///
/// `keys` / `label` are `Cow<'static, str>` so a Binding can carry
/// either a baked-in static (`"j"` / `"navigate"`) or a runtime-
/// resolved string — the user's key override or a workspace-aware
/// label like `"archive (kills sessions)"`. Static-only call sites
/// stay zero-alloc via `Cow::Borrowed`; the catalog → footer pipeline
/// switches to `Cow::Owned` only when a user override is in play.
#[derive(Debug, Clone)]
pub struct Binding {
    /// What the user presses (`"j"`, `"Tab"`, `"Ctrl-c"`, `"]]"`).
    pub keys: std::borrow::Cow<'static, str>,
    /// Short verb-phrase. Goes straight into the hint bar.
    pub label: std::borrow::Cow<'static, str>,
}

/// Returned by `Pane::detachable()` when this pane (or its content)
/// can pop out into a separate window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachSpec {
    /// Layout name the new window should run, e.g. `"detail"`.
    pub layout: &'static str,
    /// Extra args for the new client, forwarded verbatim.
    pub args: Vec<String>,
}

/// What the host should do after a Pane finishes handling a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneOutcome {
    /// Pane handled the key. Stop.
    Consumed,
    /// Pane didn't handle. Host's global handler takes over.
    Pass,
    /// Detach this Pane into a new window.
    Detach(DetachSpec),
}
