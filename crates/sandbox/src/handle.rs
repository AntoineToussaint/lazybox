use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Power state of a box, normalized across providers.
///
/// GCP reports a richer instance-status vocabulary (`PROVISIONING`,
/// `STAGING`, `REPAIRING`, `SUSPENDED`, …); we fold the transient states
/// into the four a caller actually branches on and keep [`Unknown`] for a
/// status string a provider adds later, so an unrecognized value never
/// silently reads as "stopped" and gets woken (or as "running" and left
/// billing).
///
/// [`Unknown`]: PowerState::Unknown
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PowerState {
    Running,
    Stopped,
    Starting,
    Stopping,
    Unknown,
}

impl PowerState {
    /// A box is billable/reachable only when fully `Running`; every other
    /// state means a `connect` must wake it first.
    pub fn is_running(self) -> bool {
        self == PowerState::Running
    }
}

impl std::fmt::Display for PowerState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            PowerState::Running => "running",
            PowerState::Stopped => "stopped",
            PowerState::Starting => "starting",
            PowerState::Stopping => "stopping",
            PowerState::Unknown => "unknown",
        };
        f.write_str(s)
    }
}

/// A persisted, per-worktree pointer to a provisioned box.
///
/// `ensure` returns one; [`crate::persist`] round-trips it through the
/// store so a later `start`/`stop`/`connect` addresses the same instance
/// without re-running Terraform. `zone` and `project` are carried beyond
/// the issue's `{provider, id, region, power_state, last_active}` because
/// the native GCP lifecycle ops (`gcloud instances start/stop/describe`)
/// need the exact zone and project, not just the region.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxHandle {
    /// Provider id, e.g. `"gcp"`.
    pub provider: String,
    /// Provider-native instance identifier (the GCE instance name).
    pub id: String,
    pub region: String,
    /// GCP zone (`us-central1-a`) — required by the instance lifecycle ops.
    pub zone: String,
    /// GCP project the instance lives in.
    pub project: String,
    pub power_state: PowerState,
    /// Last time the box was observed active; `None` until first seen.
    pub last_active: Option<DateTime<Utc>>,
}

impl BoxHandle {
    /// Fold a fresh power observation into the handle, stamping
    /// `last_active` whenever the box is seen running.
    pub fn observe(&mut self, power: PowerState, now: DateTime<Utc>) {
        self.power_state = power;
        if power.is_running() {
            self.last_active = Some(now);
        }
    }
}

/// The result of a `status` probe: the box's power state plus whether its
/// daemon is actually reachable over the tunnel. A box can be `Running`
/// yet not-yet-`reachable` in the window between wake and SSH coming up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxStatus {
    pub power: PowerState,
    pub reachable: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn handle() -> BoxHandle {
        BoxHandle {
            provider: "gcp".into(),
            id: "lazybox-sbx-abc".into(),
            region: "us-central1".into(),
            zone: "us-central1-a".into(),
            project: "proj".into(),
            power_state: PowerState::Stopped,
            last_active: None,
        }
    }

    #[test]
    fn handle_serde_round_trips() {
        let h = handle();
        let json = serde_json::to_string(&h).unwrap();
        let back: BoxHandle = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn observe_stamps_last_active_only_when_running() {
        let now = DateTime::<Utc>::UNIX_EPOCH;
        let mut h = handle();

        h.observe(PowerState::Starting, now);
        assert_eq!(h.power_state, PowerState::Starting);
        assert_eq!(
            h.last_active, None,
            "a non-running observation is not activity"
        );

        h.observe(PowerState::Running, now);
        assert!(h.power_state.is_running());
        assert_eq!(h.last_active, Some(now), "running stamps last_active");
    }
}
