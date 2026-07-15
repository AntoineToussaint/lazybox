//! `lazybox slack init` + `lazybox slack doctor` — interactive setup +
//! read-only verification for the Slack integration.
//!
//! Both commands share the same validations (bot token shape +
//! `auth.test`, app token shape + `apps.connections.open`, anchor
//! channel reachable). `init` writes results to
//! `~/.lazybox/config.yaml::slack.{bot_token, app_token}` via
//! `Config::save_with` and best-effort joins `#lazybox` so step 3 of
//! `docs/slack-setup.md` ("/invite @lazybox") goes away. `doctor` runs
//! the same validations read-only on the existing config so a user
//! who edited YAML by hand can confirm it still works.
//!
//! ## Why the wizard isn't a tui-realm modal
//!
//! `lazybox slack init` runs from a plain shell, before lazybox is set
//! up at all — the user hasn't seen the inbox yet and the daemon
//! isn't necessarily running. Driving it through `realm::Model`
//! would force us to construct the whole component tree just to ask
//! two questions. Stdin / println is the right surface here, the
//! same way `lazybox server status` is plain text and not a panel.
//!
//! ## I/O abstraction
//!
//! Stdin + the Slack HTTP client are both injected through traits
//! (`PromptIo`, `SlackProbe`) so the wizard logic is unit-testable
//! without a real terminal or network. The default impls are thin
//! adapters; the tests at the bottom drive `run_init_with` with
//! scripted answers.

use std::io::Write;

use lazybox_config::{Config, ConfigError};
use lazybox_slack::SlackError;
use lazybox_slack::api::{AuthTestResponse, Client as SlackApi};
use lazybox_slack::socket::SocketModeClient;

/// Result of a full wizard run. Used by `lazybox slack init` to set
/// the binary's exit code (`0` for `Ready`, `1` otherwise).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitOutcome {
    /// Both tokens validated, config written, anchor channel reached.
    Ready,
    /// Tokens validated and config written, but the bot couldn't
    /// join / find the anchor channel (e.g. private channel). User
    /// is told the exact manual step.
    ReadyNeedsInvite { anchor_channel: String },
    /// Validation failed somewhere. The wizard already printed the
    /// reason; the variant just signals a non-zero exit.
    Failed,
}

/// Outcome of `lazybox slack doctor`. Distinct from `InitOutcome`
/// because doctor never writes config; the "needs invite" case is
/// still actionable but isn't a hard failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoctorOutcome {
    Healthy,
    HealthyNeedsInvite { anchor_channel: String },
    Failed,
}

/// Secret-input / stdout abstraction. The wizard prompts twice (bot +
/// app token) with terminal echo disabled and prints status lines as it
/// goes. Replacing both in tests means no real terminal interaction is
/// required.
pub trait PromptIo {
    /// Ask the user for a secret. Production implementations must
    /// disable terminal echo for the duration of the read.
    fn prompt_secret(&mut self, label: &str) -> std::io::Result<String>;
    /// Print a line of status output. Used for both success ticks
    /// and failure explanations so tests can assert on the message
    /// stream.
    fn print(&mut self, line: &str) -> std::io::Result<()>;
}

/// Real terminal/stdout adapter. `rpassword` reads and writes via the
/// controlling TTY, so prompts remain visible even though the binary
/// redirects stderr to its log file, while pasted tokens never echo.
pub struct StdioPrompt;

impl PromptIo for StdioPrompt {
    fn prompt_secret(&mut self, label: &str) -> std::io::Result<String> {
        rpassword::prompt_password(label)
    }
    fn print(&mut self, line: &str) -> std::io::Result<()> {
        let mut out = std::io::stdout().lock();
        writeln!(out, "{line}")?;
        Ok(())
    }
}

/// Slack-side calls the wizard needs. Trait-based so we can stub
/// success / failure scenarios in tests without spinning up a fake
/// HTTPS server.
///
/// Return types are spelled `Pin<Box<dyn Future>>` longhand instead
/// of `async fn` so the trait is object-safe (`&dyn SlackProbe`)
/// without pulling in `async-trait`. Real impls + test stubs both
/// follow the same shape — see `LiveProbe` and `FakeProbe`.
pub trait SlackProbe {
    fn auth_test<'a>(
        &'a self,
        bot_token: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<AuthTestResponse, SlackError>> + Send + 'a>,
    >;
    fn validate_app_token<'a>(
        &'a self,
        app_token: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SlackError>> + Send + 'a>>;
    fn find_channel_id<'a>(
        &'a self,
        bot_token: &'a str,
        name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<String>, SlackError>> + Send + 'a>,
    >;
    fn join_channel<'a>(
        &'a self,
        bot_token: &'a str,
        channel_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SlackError>> + Send + 'a>>;
}

/// Real-network probe — what `lazybox slack init` runs against
/// production Slack.
#[derive(Default)]
pub struct LiveProbe;

impl LiveProbe {
    pub fn new() -> Self {
        Self
    }
}

impl SlackProbe for LiveProbe {
    fn auth_test<'a>(
        &'a self,
        bot_token: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<AuthTestResponse, SlackError>> + Send + 'a>,
    > {
        Box::pin(async move { SlackApi::new(bot_token.to_string()).auth_test().await })
    }

    fn validate_app_token<'a>(
        &'a self,
        app_token: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SlackError>> + Send + 'a>>
    {
        Box::pin(async move {
            SocketModeClient::new(app_token.to_string())
                .validate()
                .await
        })
    }

    fn find_channel_id<'a>(
        &'a self,
        bot_token: &'a str,
        name: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<String>, SlackError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let api = SlackApi::new(bot_token.to_string());
            // 1000 covers the typical workspace; a future paginated
            // sweep would only matter for >1000-channel workspaces,
            // which are vanishingly rare.
            let listing = api.conversations_list(1000).await?;
            for c in listing.channels {
                if c.name == name {
                    return Ok(Some(c.id));
                }
            }
            Ok(None)
        })
    }

    fn join_channel<'a>(
        &'a self,
        bot_token: &'a str,
        channel_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SlackError>> + Send + 'a>>
    {
        Box::pin(async move {
            SlackApi::new(bot_token.to_string())
                .conversations_join(channel_id)
                .await
                .map(|_| ())
        })
    }
}

/// Real-mode `lazybox slack init` entry point. Reads from stdin,
/// writes config, prints status. Returns the exit-code mapping via
/// `InitOutcome`.
pub async fn run_init() -> std::io::Result<InitOutcome> {
    let probe = LiveProbe::new();
    let mut io = StdioPrompt;
    run_init_with(&probe, &mut io, ConfigWriter::Real).await
}

/// Real-mode `lazybox slack doctor` entry point. Doesn't prompt, just
/// reads the existing config + runs validations.
pub async fn run_doctor() -> std::io::Result<DoctorOutcome> {
    let probe = LiveProbe::new();
    let mut io = StdioPrompt;
    run_doctor_with(&probe, &mut io).await
}

/// Config-persistence surface. The `Real` variant writes through
/// `Config::save_with`; tests pass `Captured` to inspect the value
/// without touching `~/.lazybox/config.yaml`.
pub enum ConfigWriter {
    Real,
    #[cfg(test)]
    Captured(std::sync::Arc<std::sync::Mutex<Option<(String, String)>>>),
}

impl ConfigWriter {
    fn save(&self, bot: &str, app: &str) -> Result<(), ConfigError> {
        match self {
            ConfigWriter::Real => Config::save_with(|c| {
                c.slack.bot_token = Some(bot.to_string());
                c.slack.app_token = Some(app.to_string());
            }),
            #[cfg(test)]
            ConfigWriter::Captured(slot) => {
                *slot.lock().unwrap() = Some((bot.to_string(), app.to_string()));
                Ok(())
            }
        }
    }
}

/// Core wizard, parameterized over I/O + Slack probe + config sink.
/// `lazybox slack init` calls this with the live wiring; tests call
/// it with stubs.
pub async fn run_init_with<P: SlackProbe + ?Sized, I: PromptIo + ?Sized>(
    probe: &P,
    io: &mut I,
    writer: ConfigWriter,
) -> std::io::Result<InitOutcome> {
    io.print("lazybox slack init — interactive setup")?;
    io.print("")?;
    io.print(
        "You'll need an existing Slack app (see docs/slack-setup.md for the\n\
         one-time create-from-manifest step). This wizard handles the rest.",
    )?;
    io.print("")?;

    // ── Bot token ────────────────────────────────────────────────
    let bot_token = io.prompt_secret("Bot User OAuth Token (xoxb-..., input hidden): ")?;
    let bot_token = bot_token.trim().to_string();
    if let Err(msg) = validate_bot_shape(&bot_token) {
        io.print(&format!("✗ {msg}"))?;
        return Ok(InitOutcome::Failed);
    }
    let auth = match probe.auth_test(&bot_token).await {
        Ok(a) => a,
        Err(e) => {
            io.print(&format!("✗ auth.test: {}", describe_slack_err(&e)))?;
            return Ok(InitOutcome::Failed);
        }
    };
    io.print(&format!(
        "✓ bot token OK — connected as @{} in team {}",
        auth.user, auth.team
    ))?;

    // ── App token ────────────────────────────────────────────────
    let app_token = io.prompt_secret("App-Level Token (xapp-..., input hidden): ")?;
    let app_token = app_token.trim().to_string();
    if let Err(msg) = validate_app_shape(&app_token) {
        io.print(&format!("✗ {msg}"))?;
        return Ok(InitOutcome::Failed);
    }
    if let Err(e) = probe.validate_app_token(&app_token).await {
        io.print(&format!(
            "✗ apps.connections.open: {}",
            describe_slack_err(&e)
        ))?;
        return Ok(InitOutcome::Failed);
    }
    io.print("✓ app token OK — Socket Mode reachable")?;

    // ── Persist tokens ───────────────────────────────────────────
    if let Err(e) = writer.save(&bot_token, &app_token) {
        io.print(&format!("✗ write ~/.lazybox/config.yaml: {e}"))?;
        return Ok(InitOutcome::Failed);
    }
    io.print("✓ wrote tokens to ~/.lazybox/config.yaml")?;

    // ── Anchor channel ───────────────────────────────────────────
    // Default name matches `SlackConfig::default_anchor_channel`.
    let anchor = "lazybox";
    let outcome =
        ensure_anchor_channel(probe, &bot_token, anchor, io, /*write_on_join=*/ false).await?;

    Ok(match outcome {
        AnchorResult::Joined | AnchorResult::AlreadyMember => InitOutcome::Ready,
        AnchorResult::NotVisible => InitOutcome::ReadyNeedsInvite {
            anchor_channel: anchor.to_string(),
        },
        AnchorResult::Errored => InitOutcome::Failed,
    })
}

/// `lazybox slack doctor` — read-only validations against the current
/// config. Does NOT prompt or write.
pub async fn run_doctor_with<P: SlackProbe + ?Sized, I: PromptIo + ?Sized>(
    probe: &P,
    io: &mut I,
) -> std::io::Result<DoctorOutcome> {
    io.print("lazybox slack doctor — read-only validation")?;
    io.print("")?;

    let cfg = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            io.print(&format!("✗ load ~/.lazybox/config.yaml: {e}"))?;
            return Ok(DoctorOutcome::Failed);
        }
    };

    // Env var > YAML, mirroring `lazybox_server::slack::resolve_token`.
    let bot_token = resolve_token(cfg.slack.bot_token.as_deref(), "SLACK_BOT_TOKEN");
    let app_token = resolve_token(cfg.slack.app_token.as_deref(), "SLACK_APP_TOKEN");

    let Some(bot_token) = bot_token else {
        io.print("✗ bot_token not set (config.slack.bot_token or $SLACK_BOT_TOKEN)")?;
        io.print("  Run `lazybox slack init` to configure interactively.")?;
        return Ok(DoctorOutcome::Failed);
    };
    let Some(app_token) = app_token else {
        io.print("✗ app_token not set (config.slack.app_token or $SLACK_APP_TOKEN)")?;
        io.print("  Run `lazybox slack init` to configure interactively.")?;
        return Ok(DoctorOutcome::Failed);
    };

    if let Err(msg) = validate_bot_shape(&bot_token) {
        io.print(&format!("✗ {msg}"))?;
        return Ok(DoctorOutcome::Failed);
    }
    let auth = match probe.auth_test(&bot_token).await {
        Ok(a) => a,
        Err(e) => {
            io.print(&format!("✗ auth.test: {}", describe_slack_err(&e)))?;
            return Ok(DoctorOutcome::Failed);
        }
    };
    io.print(&format!(
        "✓ bot token OK — connected as @{} in team {}",
        auth.user, auth.team
    ))?;

    if let Err(msg) = validate_app_shape(&app_token) {
        io.print(&format!("✗ {msg}"))?;
        return Ok(DoctorOutcome::Failed);
    }
    if let Err(e) = probe.validate_app_token(&app_token).await {
        io.print(&format!(
            "✗ apps.connections.open: {}",
            describe_slack_err(&e)
        ))?;
        return Ok(DoctorOutcome::Failed);
    }
    io.print("✓ app token OK — Socket Mode reachable")?;

    let anchor = cfg.slack.normalized_anchor_channel();
    let outcome = ensure_anchor_channel(
        probe, &bot_token, &anchor, io, /*write_on_join=*/ false,
    )
    .await?;

    Ok(match outcome {
        AnchorResult::Joined | AnchorResult::AlreadyMember => DoctorOutcome::Healthy,
        AnchorResult::NotVisible => DoctorOutcome::HealthyNeedsInvite {
            anchor_channel: anchor,
        },
        AnchorResult::Errored => DoctorOutcome::Failed,
    })
}

enum AnchorResult {
    /// Channel found by name; the bot self-joined this run.
    Joined,
    /// Channel found by name; bot was already a member (idempotent
    /// `conversations.join` succeeded with `already_in_channel`).
    AlreadyMember,
    /// Channel name not found in `conversations.list`. Slack returns
    /// only channels the bot can see, so this means either it
    /// doesn't exist or it's private — both surface to the user as
    /// "create / invite manually".
    NotVisible,
    /// Slack API error during the join attempt.
    Errored,
}

/// Idempotent anchor-channel resolution: look up by name, self-join
/// if found. Used by both `init` and `doctor`. `write_on_join` is
/// reserved for a future "create the channel on init" path; today
/// lazybox relies on the user (or the workspace having a `#general`-
/// style default) to have `#lazybox` available. This keeps the wizard
/// from accidentally creating a public channel that nobody else
/// will see.
async fn ensure_anchor_channel<P: SlackProbe + ?Sized, I: PromptIo + ?Sized>(
    probe: &P,
    bot_token: &str,
    anchor: &str,
    io: &mut I,
    _write_on_join: bool,
) -> std::io::Result<AnchorResult> {
    match probe.find_channel_id(bot_token, anchor).await {
        Ok(Some(id)) => match probe.join_channel(bot_token, &id).await {
            Ok(()) => {
                io.print(&format!(
                    "✓ Slack ready — bot is in #{anchor} (joined / already a member)"
                ))?;
                Ok(AnchorResult::Joined)
            }
            Err(SlackError::Api(ref e)) if e == "already_in_channel" => {
                io.print(&format!("✓ Slack ready — bot is already in #{anchor}"))?;
                Ok(AnchorResult::AlreadyMember)
            }
            Err(e) => {
                io.print(&format!(
                    "✗ conversations.join #{anchor}: {}",
                    describe_slack_err(&e)
                ))?;
                Ok(AnchorResult::Errored)
            }
        },
        Ok(None) => {
            io.print(&format!(
                "! anchor channel #{anchor} not visible to the bot.\n\
                 Create it in Slack (or rename an existing channel), then run:\n\
                   lazybox slack doctor"
            ))?;
            Ok(AnchorResult::NotVisible)
        }
        Err(e) => {
            io.print(&format!("✗ conversations.list: {}", describe_slack_err(&e)))?;
            Ok(AnchorResult::Errored)
        }
    }
}

/// Up-front token-shape validation. Slack's `auth.test` returns
/// `invalid_auth` for nearly every bad input; matching the prefix
/// here lets us hand the user a clearer message ("did you paste
/// `xapp-` into the bot-token slot?") before the network round-trip.
pub fn validate_bot_shape(token: &str) -> Result<(), String> {
    if token.is_empty() {
        return Err("bot token is empty — paste your xoxb-... token and press Enter".into());
    }
    if !token.starts_with("xoxb-") {
        return Err(format!(
            "bot token must start with `xoxb-` (got `{}…`). Looks like you pasted the wrong one — \
             the bot token is on the 'OAuth & Permissions' page, not 'Basic Information'.",
            short_prefix(token)
        ));
    }
    Ok(())
}

pub fn validate_app_shape(token: &str) -> Result<(), String> {
    if token.is_empty() {
        return Err("app token is empty — paste your xapp-... token and press Enter".into());
    }
    if !token.starts_with("xapp-") {
        return Err(format!(
            "app token must start with `xapp-` (got `{}…`). The app-level token lives under \
             'Basic Information' → 'App-Level Tokens', not 'OAuth & Permissions'.",
            short_prefix(token)
        ));
    }
    Ok(())
}

fn short_prefix(s: &str) -> String {
    s.chars().take(6).collect()
}

fn describe_slack_err(e: &SlackError) -> String {
    match e {
        SlackError::Api(code) => format!("Slack returned `{code}`"),
        other => other.to_string(),
    }
}

fn resolve_token(yaml: Option<&str>, env_key: &str) -> Option<String> {
    if let Ok(v) = std::env::var(env_key)
        && !v.trim().is_empty()
    {
        return Some(v);
    }
    yaml.filter(|s| !s.trim().is_empty()).map(str::to_string)
}

// Tests may block (tokio::test bodies run via block_on); the
// crate-wide blocking-call ban in clippy.toml targets the run loop.
#[allow(clippy::disallowed_methods)]
#[cfg(test)]
mod tests {
    use super::*;
    use lazybox_slack::api::AuthTestResponse;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// Drives the wizard with scripted prompt answers. The output
    /// buffer is captured for assertions on the printed message
    /// stream.
    struct ScriptedIo {
        answers: VecDeque<String>,
        out: Vec<String>,
        secret_prompts: Vec<String>,
    }

    impl ScriptedIo {
        fn new(answers: Vec<&str>) -> Self {
            Self {
                answers: answers.iter().map(|s| s.to_string()).collect(),
                out: Vec::new(),
                secret_prompts: Vec::new(),
            }
        }
        fn joined(&self) -> String {
            self.out.join("\n")
        }
    }

    impl PromptIo for ScriptedIo {
        fn prompt_secret(&mut self, label: &str) -> std::io::Result<String> {
            self.secret_prompts.push(label.to_string());
            self.out.push(label.trim_end().to_string());
            Ok(self
                .answers
                .pop_front()
                .expect("test ran out of scripted answers"))
        }
        fn print(&mut self, line: &str) -> std::io::Result<()> {
            self.out.push(line.to_string());
            Ok(())
        }
    }

    /// In-memory Slack probe with fixed responses per call.
    struct FakeProbe {
        auth: Result<AuthTestResponse, SlackError>,
        app: Result<(), SlackError>,
        // None = channel not visible; Some(id) = found.
        anchor_id: Option<String>,
        join: Result<(), SlackError>,
    }

    impl FakeProbe {
        fn ok() -> Self {
            Self {
                auth: Ok(AuthTestResponse {
                    url: "https://acme.slack.com/".into(),
                    team: "Acme".into(),
                    user: "lazybox".into(),
                    user_id: "U1".into(),
                    bot_id: Some("B1".into()),
                }),
                app: Ok(()),
                anchor_id: Some("C123".into()),
                join: Ok(()),
            }
        }
    }

    impl SlackProbe for FakeProbe {
        fn auth_test<'a>(
            &'a self,
            _bot_token: &'a str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<AuthTestResponse, SlackError>> + Send + 'a>,
        > {
            Box::pin(async move {
                match &self.auth {
                    Ok(a) => Ok(a.clone()),
                    Err(e) => Err(clone_err(e)),
                }
            })
        }
        fn validate_app_token<'a>(
            &'a self,
            _app_token: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SlackError>> + Send + 'a>>
        {
            Box::pin(async move { self.app.as_ref().map(|_| ()).map_err(clone_err) })
        }
        fn find_channel_id<'a>(
            &'a self,
            _bot_token: &'a str,
            _name: &'a str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<Option<String>, SlackError>> + Send + 'a>,
        > {
            Box::pin(async move { Ok(self.anchor_id.clone()) })
        }
        fn join_channel<'a>(
            &'a self,
            _bot_token: &'a str,
            _channel_id: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), SlackError>> + Send + 'a>>
        {
            Box::pin(async move { self.join.as_ref().map(|_| ()).map_err(clone_err) })
        }
    }

    fn clone_err(e: &SlackError) -> SlackError {
        // SlackError doesn't implement Clone (reqwest::Error isn't
        // Clone). The variants we care about in tests are.
        match e {
            SlackError::Api(s) => SlackError::Api(s.clone()),
            SlackError::Config(s) => SlackError::Config(s.clone()),
            SlackError::WebSocket(s) => SlackError::WebSocket(s.clone()),
            SlackError::Json(_) | SlackError::Http(_) => {
                SlackError::Api("unknown (cloned for test)".into())
            }
        }
    }

    #[test]
    fn validate_bot_shape_accepts_xoxb() {
        assert!(validate_bot_shape("xoxb-abc").is_ok());
    }

    #[test]
    fn validate_bot_shape_rejects_xapp() {
        // Most common typo — pasting the app token into the bot
        // slot. Error message must NAME the problem, not just say
        // "invalid".
        let err = validate_bot_shape("xapp-deadbeef").unwrap_err();
        assert!(err.contains("xoxb-"));
        assert!(err.contains("OAuth & Permissions"));
    }

    #[test]
    fn validate_bot_shape_rejects_empty() {
        assert!(validate_bot_shape("").is_err());
    }

    #[test]
    fn validate_app_shape_accepts_xapp() {
        assert!(validate_app_shape("xapp-1-abc").is_ok());
    }

    #[test]
    fn validate_app_shape_rejects_xoxb() {
        let err = validate_app_shape("xoxb-deadbeef").unwrap_err();
        assert!(err.contains("xapp-"));
    }

    #[tokio::test]
    async fn init_happy_path_writes_tokens_and_returns_ready() {
        let probe = FakeProbe::ok();
        let mut io = ScriptedIo::new(vec!["xoxb-good", "xapp-good"]);
        let slot = Arc::new(Mutex::new(None));
        let writer = ConfigWriter::Captured(slot.clone());

        let outcome = run_init_with(&probe, &mut io, writer).await.unwrap();
        assert_eq!(outcome, InitOutcome::Ready);
        let saved = slot.lock().unwrap().clone().unwrap();
        assert_eq!(saved.0, "xoxb-good");
        assert_eq!(saved.1, "xapp-good");
        assert_eq!(io.secret_prompts.len(), 2);
        assert!(
            io.secret_prompts
                .iter()
                .all(|prompt| prompt.contains("input hidden"))
        );
        let log = io.joined();
        // Success ticks land in order: bot, app, write, channel.
        assert!(log.contains("✓ bot token OK"));
        assert!(log.contains("✓ app token OK"));
        assert!(log.contains("✓ wrote tokens"));
        assert!(log.contains("Slack ready"));
    }

    #[tokio::test]
    async fn init_bails_on_wrong_token_type_before_calling_api() {
        let probe = FakeProbe::ok();
        // User pasted the app token (xapp-...) into the bot slot.
        let mut io = ScriptedIo::new(vec!["xapp-oops"]);
        let slot = Arc::new(Mutex::new(None));
        let writer = ConfigWriter::Captured(slot.clone());

        let outcome = run_init_with(&probe, &mut io, writer).await.unwrap();
        assert_eq!(outcome, InitOutcome::Failed);
        // Critically: no token was written.
        assert!(slot.lock().unwrap().is_none());
        // Critically: the message names the issue without making
        // the user wait for a Slack round-trip.
        let log = io.joined();
        assert!(log.contains("must start with `xoxb-`"));
    }

    #[tokio::test]
    async fn init_bails_when_auth_test_rejects_token() {
        let mut probe = FakeProbe::ok();
        probe.auth = Err(SlackError::Api("invalid_auth".into()));
        let mut io = ScriptedIo::new(vec!["xoxb-stale", "unused"]);
        let slot = Arc::new(Mutex::new(None));

        let outcome = run_init_with(&probe, &mut io, ConfigWriter::Captured(slot.clone()))
            .await
            .unwrap();
        assert_eq!(outcome, InitOutcome::Failed);
        assert!(slot.lock().unwrap().is_none());
        let log = io.joined();
        assert!(log.contains("Slack returned `invalid_auth`"));
    }

    #[tokio::test]
    async fn init_bails_when_app_token_validation_fails() {
        let mut probe = FakeProbe::ok();
        probe.app = Err(SlackError::Api("not_allowed_token_type".into()));
        let mut io = ScriptedIo::new(vec!["xoxb-good", "xapp-wrong-scope"]);
        let slot = Arc::new(Mutex::new(None));

        let outcome = run_init_with(&probe, &mut io, ConfigWriter::Captured(slot.clone()))
            .await
            .unwrap();
        assert_eq!(outcome, InitOutcome::Failed);
        // App-token failure must not write the bot token alone —
        // partial writes turn a re-run of the wizard into a guessing
        // game ("is my bot token persisted or not?"). Either both
        // land or neither does.
        assert!(slot.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn init_reports_needs_invite_when_anchor_not_visible() {
        let mut probe = FakeProbe::ok();
        probe.anchor_id = None;
        let mut io = ScriptedIo::new(vec!["xoxb-good", "xapp-good"]);
        let slot = Arc::new(Mutex::new(None));

        let outcome = run_init_with(&probe, &mut io, ConfigWriter::Captured(slot.clone()))
            .await
            .unwrap();
        assert!(matches!(outcome, InitOutcome::ReadyNeedsInvite { .. }));
        // Tokens still get written — they're valid; the missing
        // channel is the only gap.
        assert!(slot.lock().unwrap().is_some());
        let log = io.joined();
        assert!(log.contains("not visible"));
    }

    #[tokio::test]
    async fn init_handles_already_in_channel_idempotently() {
        let mut probe = FakeProbe::ok();
        probe.join = Err(SlackError::Api("already_in_channel".into()));
        let mut io = ScriptedIo::new(vec!["xoxb-good", "xapp-good"]);
        let slot = Arc::new(Mutex::new(None));

        let outcome = run_init_with(&probe, &mut io, ConfigWriter::Captured(slot.clone()))
            .await
            .unwrap();
        assert_eq!(outcome, InitOutcome::Ready);
    }

    #[tokio::test]
    async fn init_strips_whitespace_around_pasted_tokens() {
        // Common paste mishap: trailing newline or surrounding
        // spaces from a "copy from quoted YAML" attempt.
        let probe = FakeProbe::ok();
        let mut io = ScriptedIo::new(vec!["  xoxb-good  ", "\txapp-good\t"]);
        let slot = Arc::new(Mutex::new(None));

        let outcome = run_init_with(&probe, &mut io, ConfigWriter::Captured(slot.clone()))
            .await
            .unwrap();
        assert_eq!(outcome, InitOutcome::Ready);
        let saved = slot.lock().unwrap().clone().unwrap();
        assert_eq!(saved.0, "xoxb-good");
        assert_eq!(saved.1, "xapp-good");
    }
}
