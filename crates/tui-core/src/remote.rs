//! Remote-box worker ↔ UI wiring.
//!
//! The `r`-spawn worker (tui-boot's `remote_box`) owns the box lifecycle
//! off the UI thread; the realm `Model` deliberately never drains a remote
//! client's *daemon* events (a second daemon's `Snapshot` would clobber the
//! local inbox). Two narrow channels are the worker's link to the UI:
//! [`RemoteBoxNotice`] carries progress back (bring-up state, dropped
//! commands) so it surfaces as a **persistent connection indicator** rather
//! than vanishing into the log file, and [`RemoteControl`] carries explicit
//! connect/disconnect requests the other way so connection is a first-class,
//! visible action instead of a side-effect of the first spawn (#1066).

use lazybox_core::SessionKey;

/// One worker→UI notice. Sent over a plain `tokio::sync::mpsc` channel the
/// `Model` drains with `try_recv` once per run-loop iteration — never a
/// wire event, so it can't collide with daemon state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteBoxNotice {
    /// A durable connection-state transition — the box moved to
    /// creating / waking / connected / disconnected / error. Promoted from
    /// the old transient info flash so the UI can hold it as a persistent
    /// indicator the user can consult at any time (#1066).
    State(RemoteConnState),
    /// A plain informational flash that is not itself a connection state
    /// (e.g. "found an older box handle under …"). Still a transient notice.
    Info(String),
    /// One or more commands to the box were dropped (a bring-up exhausted
    /// its retries — taking every command queued behind it — or the live
    /// link refused a send). `session_keys` names every workspace whose
    /// optimistic `⇅` tag must be rolled back: the sessions those spawns
    /// advertised will never exist. Aggregated so a bulk fan-out that dies
    /// with the box produces one notice, not one per queued spawn.
    Dropped {
        session_keys: Vec<SessionKey>,
        error: String,
    },
}

/// A UI→worker control request. The `Model` sends these when the user
/// invokes the explicit connect/disconnect action, and once on startup for
/// auto-connect. Connection is thereby a session-level state the user
/// drives, not a hidden effect of the first `r`-spawn (#1066).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteControl {
    /// Bring the box up now (create if missing, wake if asleep, connect)
    /// without spawning anything, so it's live before the first spawn.
    Connect,
    /// Tear the live link down (drop the tunnel); the box itself keeps
    /// running so a reconnect is cheap.
    Disconnect,
}

/// Durable state of the connection to the remote box — the model of "am I
/// connected to my box right now?" that the persistent indicator renders
/// (#1066). Reflects power state (asleep/waking) and reconnect-after-drop,
/// not just the momentary bring-up progress the old transient flashes
/// carried.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RemoteConnState {
    /// No `sandbox:` box is configured — the indicator is hidden and the
    /// local-only path is entirely unchanged.
    #[default]
    NotConfigured,
    /// Provisioning a box that doesn't exist yet (Terraform apply). Slow.
    Creating,
    /// The box exists but is stopped/sleeping.
    Asleep,
    /// Starting a stopped box back up.
    Waking,
    /// Box is up; building the tunnel and dialing its daemon.
    Connecting,
    /// The link is live. `name` is the deployment name → the `⇅ <name>`
    /// glyph.
    Connected { name: String },
    /// Configured, but not currently connected (idle, or torn down).
    Disconnected,
    /// The last connect attempt failed; `reason` is a one-line summary.
    Error { reason: String },
}

impl RemoteConnState {
    /// Whether a bring-up is in flight (an animated indicator is warranted).
    pub fn is_busy(&self) -> bool {
        matches!(self, Self::Creating | Self::Waking | Self::Connecting)
    }

    /// Whether the link is live.
    pub fn is_connected(&self) -> bool {
        matches!(self, Self::Connected { .. })
    }

    /// Whether a box is configured at all (the indicator is shown).
    pub fn is_configured(&self) -> bool {
        !matches!(self, Self::NotConfigured)
    }

    /// The one-line reason when the last bring-up failed, else `None`. A
    /// remote spawn reads this to fail fast — a box stuck in `Error` (bad
    /// creds, provider unreachable) must surface that immediately rather
    /// than present as "spawning" (#1372).
    pub fn error_reason(&self) -> Option<&str> {
        match self {
            Self::Error { reason } => Some(reason),
            _ => None,
        }
    }

    /// A static glyph for the non-busy steady states. Busy states animate
    /// with a spinner instead, so this is unused for them.
    pub fn glyph(&self) -> &'static str {
        match self {
            Self::Connected { .. } => "⇅",
            Self::Asleep => "⏾",
            Self::Error { .. } => "⚠",
            // Creating / Waking / Connecting animate; Disconnected /
            // NotConfigured fall through to a neutral dot.
            _ => "·",
        }
    }

    /// The footer label (no glyph — the renderer prepends the glyph or an
    /// animated spinner frame).
    pub fn label(&self) -> String {
        match self {
            Self::NotConfigured => String::new(),
            Self::Creating => "creating box…".into(),
            Self::Asleep => "box asleep".into(),
            Self::Waking => "waking box…".into(),
            Self::Connecting => "connecting…".into(),
            Self::Connected { name } => format!("connected: {name}"),
            Self::Disconnected => "box disconnected".into(),
            Self::Error { reason } => format!("box error: {reason}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_not_configured_and_hidden() {
        let s = RemoteConnState::default();
        assert_eq!(s, RemoteConnState::NotConfigured);
        assert!(
            !s.is_configured(),
            "an unconfigured box hides the indicator"
        );
        assert!(!s.is_busy());
        assert!(!s.is_connected());
    }

    #[test]
    fn busy_states_are_the_bring_up_transitions() {
        for s in [
            RemoteConnState::Creating,
            RemoteConnState::Waking,
            RemoteConnState::Connecting,
        ] {
            assert!(s.is_busy(), "{s:?} is an in-flight bring-up");
            assert!(s.is_configured());
            assert!(!s.is_connected());
        }
        // Steady states are not busy.
        for s in [
            RemoteConnState::Asleep,
            RemoteConnState::Disconnected,
            RemoteConnState::Connected { name: "b".into() },
            RemoteConnState::Error { reason: "x".into() },
        ] {
            assert!(!s.is_busy(), "{s:?} is steady");
        }
    }

    #[test]
    fn connected_reports_connected_and_names_the_box() {
        let s = RemoteConnState::Connected {
            name: "obin".into(),
        };
        assert!(s.is_connected());
        assert_eq!(s.glyph(), "⇅");
        assert_eq!(s.label(), "connected: obin");
    }

    #[test]
    fn error_label_carries_the_reason() {
        let s = RemoteConnState::Error {
            reason: "auth expired".into(),
        };
        assert_eq!(s.glyph(), "⚠");
        assert_eq!(s.label(), "box error: auth expired");
        assert!(!s.is_connected());
    }

    #[test]
    fn error_reason_reads_only_the_error_state() {
        assert_eq!(
            RemoteConnState::Error {
                reason: "gcp creds".into(),
            }
            .error_reason(),
            Some("gcp creds")
        );
        for ok in [
            RemoteConnState::NotConfigured,
            RemoteConnState::Connecting,
            RemoteConnState::Disconnected,
            RemoteConnState::Connected { name: "b".into() },
        ] {
            assert_eq!(ok.error_reason(), None, "{ok:?} is not an error state");
        }
    }
}
