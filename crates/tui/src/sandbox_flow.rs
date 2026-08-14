//! In-app onboarding for a remote sandbox box — a **pure** staged flow.
//!
//! Before this, `sandbox.{provider,project,zone,user,…}` had to be
//! hand-authored in `~/.lazybox/config.yaml` before `Shift-C`
//! (`ConnectBox`) could do anything: the connect button was wired to
//! config only an insider knew how to write (#1112). This module is the
//! logic that produces a valid [`SandboxConfig`] from a short guided
//! flow, so the box can be set up entirely inside lazybox.
//!
//! Like [`crate::setup_flow`], this layer is pure: it owns the ordered
//! sequence of questions and how each answer maps into the accumulating
//! [`SandboxDraft`]. The Model drives it by mounting one existing modal
//! (Choice / Input / Confirm) per [`SandboxStage`] and feeding answers
//! back through the `set_*` methods; the draft finally serializes to a
//! [`SandboxConfig`] the config writer persists. No widgets, no IO here.

use lazybox_config::SandboxConfig;

/// Default GCE zone, matching the issue's "sensible defaults".
pub const DEFAULT_ZONE: &str = "us-central1-a";
/// Default SSH/gcloud login user on the box.
pub const DEFAULT_USER: &str = "lazybox";

/// One step of the onboarding flow. The order is provider-dependent —
/// GCP walks project/zone/user, E2B walks the template — so the next
/// stage is computed from the draft rather than a fixed list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxStage {
    /// Pick the provider (gcp / e2b). Always first.
    Provider,
    /// GCP sign-in guidance: the app can't shell an interactive browser
    /// login, so it points the user at `! gcloud auth login` and waits
    /// for them to confirm they're signed in (the safe v1 from the
    /// issue's open questions).
    GcpSignIn,
    /// GCP project id.
    Project,
    /// GCE zone (defaulted).
    Zone,
    /// Login user on the box (defaulted).
    User,
    /// E2B credential guidance: E2B authenticates with an API key from the
    /// `E2B_API_KEY` environment variable, so the flow surfaces that
    /// prerequisite instead of silently persisting a box that can't
    /// authenticate.
    E2bSignIn,
    /// E2B template id / alias.
    E2bTemplate,
    /// Auto-connect-at-launch toggle.
    AutoConnect,
}

/// Accumulating answers plus the current [`SandboxStage`]. Every `set_*`
/// records one answer and advances `stage`; [`Self::to_config`] turns a
/// finished draft into a persistable [`SandboxConfig`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxDraft {
    pub provider: String,
    pub project: Option<String>,
    pub zone: Option<String>,
    pub user: Option<String>,
    pub template: Option<String>,
    pub auto_connect: bool,
    pub stage: SandboxStage,
}

impl Default for SandboxDraft {
    fn default() -> Self {
        Self {
            provider: String::new(),
            project: None,
            zone: None,
            user: None,
            template: None,
            auto_connect: false,
            stage: SandboxStage::Provider,
        }
    }
}

/// The provider ids the picker offers, in display order.
pub const PROVIDERS: [&str; 2] = ["gcp", "e2b"];

impl SandboxDraft {
    /// Fresh draft at the provider-pick stage.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pre-seed a draft from an existing `sandbox:` block so re-running
    /// onboarding on a configured box carries the current values forward
    /// (the input steps prefill them via [`Self::input_prefill`]) instead
    /// of re-asking from blank. Always starts at the provider pick.
    pub fn from_config(cfg: &SandboxConfig) -> Self {
        Self {
            provider: cfg.provider.clone().unwrap_or_default(),
            project: cfg.project.clone(),
            zone: cfg.zone.clone(),
            user: cfg.user.clone(),
            template: cfg.template.clone(),
            auto_connect: cfg.auto_connect.unwrap_or(false),
            stage: SandboxStage::Provider,
        }
    }

    fn is_gcp(&self) -> bool {
        self.provider == "gcp"
    }

    /// Record the picked provider and advance to its first question.
    /// Unknown ids fall back to the GCP walk (the picker only offers the
    /// known set, so this is defensive).
    pub fn set_provider(&mut self, provider: impl Into<String>) {
        self.provider = provider.into();
        self.stage = if self.is_gcp() {
            SandboxStage::GcpSignIn
        } else {
            SandboxStage::E2bSignIn
        };
    }

    /// Advance past the GCP sign-in guidance to the project question.
    pub fn confirm_gcp_signin(&mut self) {
        self.stage = SandboxStage::Project;
    }

    /// Advance past the E2B credential guidance to the template question.
    pub fn confirm_e2b_signin(&mut self) {
        self.stage = SandboxStage::E2bTemplate;
    }

    /// The value to prefill the current input step with — the carried-over
    /// answer (on a re-run) or the field's default. Empty for steps that
    /// aren't a text input.
    pub fn input_prefill(&self) -> String {
        match self.stage {
            SandboxStage::Project => self.project.clone().unwrap_or_default(),
            SandboxStage::Zone => self
                .zone
                .clone()
                .unwrap_or_else(|| DEFAULT_ZONE.to_string()),
            SandboxStage::User => self
                .user
                .clone()
                .unwrap_or_else(|| DEFAULT_USER.to_string()),
            SandboxStage::E2bTemplate => self.template.clone().unwrap_or_default(),
            _ => String::new(),
        }
    }

    /// Record the GCP project and advance to the zone question.
    pub fn set_project(&mut self, project: impl Into<String>) {
        self.project = non_empty(project.into());
        self.stage = SandboxStage::Zone;
    }

    /// Record the zone (blank keeps [`DEFAULT_ZONE`]) and advance to the
    /// user question.
    pub fn set_zone(&mut self, zone: impl Into<String>) {
        self.zone = non_empty(zone.into());
        self.stage = SandboxStage::User;
    }

    /// Record the login user (blank keeps [`DEFAULT_USER`]) and advance
    /// to the auto-connect toggle.
    pub fn set_user(&mut self, user: impl Into<String>) {
        self.user = non_empty(user.into());
        self.stage = SandboxStage::AutoConnect;
    }

    /// Record the E2B template and advance to the auto-connect toggle.
    pub fn set_template(&mut self, template: impl Into<String>) {
        self.template = non_empty(template.into());
        self.stage = SandboxStage::AutoConnect;
    }

    /// Record the auto-connect toggle. This is the last answer; the
    /// caller finishes by persisting [`Self::to_config`].
    pub fn set_auto_connect(&mut self, on: bool) {
        self.auto_connect = on;
    }

    /// True when the project step yielded no usable id — the flow can't
    /// finish a GCP box without one, so the Model re-prompts.
    pub fn needs_project(&self) -> bool {
        self.is_gcp() && self.project.is_none()
    }

    /// Serialize the collected answers into a persistable config. The
    /// provider-irrelevant fields stay `None` so the written YAML carries
    /// only what applies (a GCP box gets no `template`, an E2B box gets no
    /// `zone`). Defaults fill the blanks the user skipped.
    pub fn to_config(&self) -> SandboxConfig {
        let gcp = self.is_gcp();
        let zone = gcp.then(|| {
            self.zone
                .clone()
                .unwrap_or_else(|| DEFAULT_ZONE.to_string())
        });
        SandboxConfig {
            provider: Some(self.provider.clone()),
            project: gcp.then(|| self.project.clone()).flatten(),
            // A GCE zone is `<region>-<letter>`; pin the region to match so a
            // non-default zone doesn't leave `region` at its us-central1
            // default and fail provisioning on the mismatch.
            region: zone.as_deref().and_then(region_of_zone),
            zone,
            user: gcp.then(|| {
                self.user
                    .clone()
                    .unwrap_or_else(|| DEFAULT_USER.to_string())
            }),
            template: (!gcp).then(|| self.template.clone()).flatten(),
            auto_connect: Some(self.auto_connect),
            ..SandboxConfig::default()
        }
    }
}

fn non_empty(s: String) -> Option<String> {
    let t = s.trim();
    (!t.is_empty()).then(|| t.to_string())
}

/// The region a GCE zone belongs to — everything before the final
/// `-<letter>` (`europe-west1-b` → `europe-west1`). `None` when the input
/// doesn't look like a zone, so a malformed value doesn't invent a region.
fn region_of_zone(zone: &str) -> Option<String> {
    zone.rsplit_once('-')
        .filter(|(region, letter)| !region.is_empty() && !letter.is_empty())
        .map(|(region, _)| region.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gcp_walk_visits_signin_project_zone_user_autoconnect() {
        let mut d = SandboxDraft::new();
        assert_eq!(d.stage, SandboxStage::Provider);
        d.set_provider("gcp");
        assert_eq!(d.stage, SandboxStage::GcpSignIn);
        d.confirm_gcp_signin();
        assert_eq!(d.stage, SandboxStage::Project);
        d.set_project("my-proj");
        assert_eq!(d.stage, SandboxStage::Zone);
        d.set_zone("europe-west1-b");
        assert_eq!(d.stage, SandboxStage::User);
        d.set_user("dev");
        assert_eq!(d.stage, SandboxStage::AutoConnect);
    }

    #[test]
    fn e2b_walk_skips_gcp_only_steps() {
        let mut d = SandboxDraft::new();
        d.set_provider("e2b");
        assert_eq!(d.stage, SandboxStage::E2bSignIn);
        d.confirm_e2b_signin();
        assert_eq!(d.stage, SandboxStage::E2bTemplate);
        d.set_template("lazybox-e2b");
        assert_eq!(d.stage, SandboxStage::AutoConnect);
    }

    #[test]
    fn from_config_carries_values_forward_at_the_provider_step() {
        let cfg = SandboxConfig {
            provider: Some("gcp".into()),
            project: Some("keep-proj".into()),
            zone: Some("europe-west1-b".into()),
            user: Some("dev".into()),
            auto_connect: Some(true),
            ..SandboxConfig::default()
        };
        let d = SandboxDraft::from_config(&cfg);
        assert_eq!(
            d.stage,
            SandboxStage::Provider,
            "always re-picks the provider"
        );
        assert_eq!(d.project.as_deref(), Some("keep-proj"));
        assert_eq!(d.zone.as_deref(), Some("europe-west1-b"));
        assert_eq!(d.user.as_deref(), Some("dev"));
        assert!(d.auto_connect);
    }

    #[test]
    fn input_prefill_uses_carried_values_then_defaults() {
        // Seeded draft: prefills carry the existing value.
        let cfg = SandboxConfig {
            provider: Some("gcp".into()),
            project: Some("keep-proj".into()),
            zone: Some("us-east1-b".into()),
            ..SandboxConfig::default()
        };
        let mut d = SandboxDraft::from_config(&cfg);
        d.set_provider("gcp");
        d.confirm_gcp_signin();
        assert_eq!(
            d.input_prefill(),
            "keep-proj",
            "project prefilled from config"
        );
        d.stage = SandboxStage::Zone;
        assert_eq!(
            d.input_prefill(),
            "us-east1-b",
            "zone prefilled from config"
        );
        // Fresh draft: prefills fall back to the field defaults.
        let mut fresh = SandboxDraft::new();
        fresh.stage = SandboxStage::Zone;
        assert_eq!(fresh.input_prefill(), DEFAULT_ZONE);
        fresh.stage = SandboxStage::User;
        assert_eq!(fresh.input_prefill(), DEFAULT_USER);
        fresh.stage = SandboxStage::Project;
        assert_eq!(fresh.input_prefill(), "");
    }

    #[test]
    fn blank_zone_and_user_fall_back_to_defaults() {
        let mut d = SandboxDraft::new();
        d.set_provider("gcp");
        d.confirm_gcp_signin();
        d.set_project("my-proj");
        d.set_zone("   ");
        d.set_user("");
        d.set_auto_connect(false);
        let cfg = d.to_config();
        assert_eq!(cfg.zone.as_deref(), Some(DEFAULT_ZONE));
        assert_eq!(cfg.user.as_deref(), Some(DEFAULT_USER));
    }

    #[test]
    fn gcp_config_carries_project_zone_user_no_template() {
        let mut d = SandboxDraft::new();
        d.set_provider("gcp");
        d.confirm_gcp_signin();
        d.set_project("my-proj");
        d.set_zone("us-east1-b");
        d.set_user("alice");
        d.set_auto_connect(true);
        let cfg = d.to_config();
        assert_eq!(cfg.provider.as_deref(), Some("gcp"));
        assert_eq!(cfg.project.as_deref(), Some("my-proj"));
        assert_eq!(cfg.zone.as_deref(), Some("us-east1-b"));
        assert_eq!(
            cfg.region.as_deref(),
            Some("us-east1"),
            "region pinned to the zone"
        );
        assert_eq!(cfg.user.as_deref(), Some("alice"));
        assert_eq!(cfg.auto_connect, Some(true));
        assert!(cfg.template.is_none());
    }

    #[test]
    fn region_is_derived_from_the_zone() {
        assert_eq!(
            region_of_zone("europe-west1-b").as_deref(),
            Some("europe-west1")
        );
        assert_eq!(
            region_of_zone("us-central1-a").as_deref(),
            Some("us-central1")
        );
        assert_eq!(region_of_zone("nonsense").as_deref(), None);
        assert_eq!(region_of_zone("-a").as_deref(), None);
    }

    #[test]
    fn e2b_config_carries_template_no_gcp_fields() {
        let mut d = SandboxDraft::new();
        d.set_provider("e2b");
        d.set_template("lazybox-e2b");
        d.set_auto_connect(false);
        let cfg = d.to_config();
        assert_eq!(cfg.provider.as_deref(), Some("e2b"));
        assert_eq!(cfg.template.as_deref(), Some("lazybox-e2b"));
        assert!(cfg.project.is_none());
        assert!(cfg.zone.is_none());
        assert!(cfg.user.is_none());
        assert_eq!(cfg.auto_connect, Some(false));
    }

    #[test]
    fn empty_project_is_flagged_for_reprompt() {
        let mut d = SandboxDraft::new();
        d.set_provider("gcp");
        d.confirm_gcp_signin();
        d.set_project("   ");
        assert!(d.needs_project());
        d.set_project("real-proj");
        assert!(!d.needs_project());
    }

    #[test]
    fn produced_config_round_trips_through_yaml() {
        let mut d = SandboxDraft::new();
        d.set_provider("gcp");
        d.confirm_gcp_signin();
        d.set_project("my-proj");
        d.set_zone("");
        d.set_user("");
        d.set_auto_connect(false);
        let cfg = lazybox_config::Config {
            sandbox: d.to_config(),
            ..Default::default()
        };

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        cfg.save_to(&path).unwrap();
        let back = lazybox_config::Config::load_from(&path).unwrap();
        assert_eq!(back.sandbox, d.to_config());
    }
}
