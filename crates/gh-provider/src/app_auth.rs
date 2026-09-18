//! GitHub App installation credentials — the daemon poller's own budget.
//!
//! One personal token serves the poller *and* every agent session, and they
//! share its 5,000 requests/hour. The poller is the only consumer that keeps
//! a reserve, so it is the one that starves: a busy fleet can spend the whole
//! budget and leave the inbox frozen while looking perfectly healthy.
//!
//! A GitHub App *installation* token carries its own 5,000/hour, independent
//! of the user's. This module resolves one through the [`CredentialProvider`]
//! trait so the poller can resolve it from a chain like any other credential,
//! while everything that authors on the user's behalf — comments, merges,
//! commits — keeps resolving [`crate::credential_chain`] and stays attributed
//! to the user.
//!
//! Installation tokens live one hour. Refresh is octocrab's: the installation
//! transport is process-global and holds its own cached token, re-minting it
//! when less than [`TOKEN_REFRESH_BUFFER_MINUTES`] remains. A fresh provider per
//! poll tick therefore does not mint a fresh token per tick — it reads the same
//! cached one until it is genuinely near expiry.
//!
//! An installation covers a *set* of repositories. [`InstallationCoverage`]
//! reports that set so a caller can tell whether the credential actually
//! reaches everything it is about to poll, rather than discovering the gap as
//! silently missing rows.

use lazybox_auth::{Credential, CredentialError, CredentialProvider};
use octocrab::Octocrab;
use octocrab::models::{AppId, InstallationId};
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::sync::OnceLock;

/// Re-mint the installation token when less than this many minutes remain on
/// it. Ten minutes is far longer than any poll tick, so a token handed out is
/// always usable for the whole tick that received it.
pub const TOKEN_REFRESH_BUFFER_MINUTES: i64 = 10;

/// Environment overrides for the App registration. They mirror the
/// `providers.github.app` config block and win over it, matching how
/// `LAZYBOX_GITHUB_TOKEN` overrides the configured user credential.
pub const APP_ID_ENV: &str = "LAZYBOX_GITHUB_APP_ID";
pub const PRIVATE_KEY_PATH_ENV: &str = "LAZYBOX_GITHUB_APP_PRIVATE_KEY_PATH";
pub const INSTALLATION_ID_ENV: &str = "LAZYBOX_GITHUB_APP_INSTALLATION_ID";

/// A registered GitHub App lazybox can authenticate as. The private key is
/// the App's own signing key (PEM, `BEGIN RSA PRIVATE KEY`), used only to
/// mint the short-lived App JWT that exchanges for an installation token.
#[derive(Clone, PartialEq, Eq)]
pub struct AppCredentials {
    pub app_id: u64,
    pub private_key_pem: String,
    /// The installation to authenticate as. Discovered when absent — which
    /// only works when the App has exactly one installation, since lazybox
    /// cannot guess which of several the user meant.
    pub installation_id: Option<u64>,
}

// The PEM is a signing key; never print it.
impl fmt::Debug for AppCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppCredentials")
            .field("app_id", &self.app_id)
            .field("private_key_pem", &"[REDACTED]")
            .field("installation_id", &self.installation_id)
            .finish()
    }
}

impl AppCredentials {
    /// Read an App registration from the environment, or `None` when
    /// [`APP_ID_ENV`] and [`PRIVATE_KEY_PATH_ENV`] are not both set.
    ///
    /// A set-but-unusable registration (unparseable id, unreadable key file)
    /// is `None` with a warning rather than an error: the App is an optional
    /// upgrade, and a broken one must fall back to the user token rather than
    /// take GitHub polling down with it.
    pub fn from_env() -> Option<Self> {
        let app_id = std::env::var(APP_ID_ENV).ok().filter(|v| !v.is_empty())?;
        let key_path = std::env::var(PRIVATE_KEY_PATH_ENV)
            .ok()
            .filter(|v| !v.is_empty())?;
        Self::from_parts(
            &app_id,
            std::path::Path::new(&key_path),
            std::env::var(INSTALLATION_ID_ENV).ok().as_deref(),
        )
    }

    /// Build a registration from the three raw values a config block or the
    /// environment supplies. `None` (with a warning) when any is unusable.
    pub fn from_parts(
        app_id: &str,
        private_key_path: &std::path::Path,
        installation_id: Option<&str>,
    ) -> Option<Self> {
        let app_id = match app_id.trim().parse::<u64>() {
            Ok(id) => id,
            Err(e) => {
                tracing::warn!(app_id, error = %e, "GitHub App id is not a number; ignoring the App registration");
                return None;
            }
        };
        let private_key_pem = match std::fs::read_to_string(private_key_path) {
            Ok(pem) => pem,
            Err(e) => {
                tracing::warn!(
                    path = %private_key_path.display(),
                    error = %e,
                    "GitHub App private key is unreadable; ignoring the App registration"
                );
                return None;
            }
        };
        let installation_id = match installation_id.map(str::trim).filter(|v| !v.is_empty()) {
            None => None,
            Some(raw) => match raw.parse::<u64>() {
                Ok(id) => Some(id),
                Err(e) => {
                    tracing::warn!(installation_id = raw, error = %e, "GitHub App installation id is not a number; discovering it instead");
                    None
                }
            },
        };
        Some(Self {
            app_id,
            private_key_pem,
            installation_id,
        })
    }

    /// `Credential::source` label for a token minted from this App.
    fn credential_source(&self, installation_id: u64) -> String {
        format!("github-app:{}/installation:{installation_id}", self.app_id)
    }
}

/// True when `source` labels a credential minted from a GitHub App
/// installation rather than a user token. Callers that must not author as
/// the App (comments, merges, commits) check this before using a credential.
pub fn is_installation_source(source: &str) -> bool {
    source.starts_with("github-app:")
}

/// Which repositories an installation reaches.
///
/// `all_repositories` is GitHub's "All repositories" selection on `account`:
/// it covers repos created after the install, which an enumerated list never
/// does, so an org-wide poll scope is only ever covered by this flag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstallationCoverage {
    pub installation_id: u64,
    /// The org or user the App is installed on, lowercased.
    pub account: String,
    pub all_repositories: bool,
    /// Lowercased `owner/repo` for each repository the installation reaches.
    /// Empty and meaningless when `all_repositories` is set.
    pub repositories: BTreeSet<String>,
    /// The installation record's `updated_at`, which GitHub bumps whenever
    /// the installation changes — including when a repository is added to
    /// or removed from it. [`fetch_coverage`] re-walks the repository list
    /// only when this moves, so freshness costs one request rather than a
    /// pagination walk per poll tick.
    pub updated_at: Option<String>,
}

impl InstallationCoverage {
    /// Whether this installation reaches `scope` — either a bare owner
    /// (`acme`, the whole org) or one repository (`acme/widget`).
    pub fn covers(&self, scope: &str) -> bool {
        let scope = scope.trim().to_ascii_lowercase();
        match scope.split_once('/') {
            // A whole-org scope is only covered by an "All repositories"
            // installation: an enumerated list says nothing about the repo
            // the user creates tomorrow, which that scope would include.
            None => self.all_repositories && scope == self.account,
            Some((owner, _)) => {
                if self.all_repositories && owner == self.account {
                    return true;
                }
                self.repositories.contains(&scope)
            }
        }
    }

    /// The members of `scopes` this installation does NOT reach, in input
    /// order and deduplicated. Empty means the installation covers the lot.
    pub fn uncovered<'a>(&self, scopes: impl IntoIterator<Item = &'a str>) -> Vec<String> {
        let mut seen = BTreeSet::new();
        scopes
            .into_iter()
            .filter(|scope| !self.covers(scope))
            .filter(|scope| seen.insert(scope.trim().to_ascii_lowercase()))
            .map(|scope| scope.trim().to_string())
            .collect()
    }
}

/// Resolves a GitHub App installation token through the credential chain.
///
/// Constructed only with a registration the caller has already resolved —
/// "no App configured" is answered before a chain is built, not by a
/// provider that declines. A registered-but-broken App (bad key, revoked
/// installation, several installations and no id) is a
/// [`CredentialError::Provider`] failure, which the chain surfaces rather
/// than masking: an App that has stopped working must not read as one that
/// was never configured.
pub struct InstallationTokenProvider {
    app: AppCredentials,
    host: Option<String>,
}

impl InstallationTokenProvider {
    pub fn new(app: AppCredentials, host: Option<&str>) -> Self {
        Self {
            app,
            host: host.map(str::to_string),
        }
    }
}

impl CredentialProvider for InstallationTokenProvider {
    fn name(&self) -> &str {
        "github-app"
    }

    async fn resolve(&self, _scope: &str) -> Result<Credential, CredentialError> {
        let app = &self.app;
        let handle = installation_handle(app, self.host.as_deref()).await?;
        let token = handle
            .installation
            .installation_token_with_buffer(chrono::TimeDelta::minutes(
                TOKEN_REFRESH_BUFFER_MINUTES,
            ))
            .await
            .map_err(|e| {
                CredentialError::Provider(format!("mint GitHub App installation token: {e}"))
            })?;
        use secrecy::ExposeSecret;
        Ok(Credential::new(
            token.expose_secret(),
            app.credential_source(handle.installation_id),
        ))
    }
}

/// Fetch which repositories `app`'s installation reaches.
///
/// Always re-reads the installation record, because the caller uses the
/// answer to decide whether a sweep may *retire* rows — acting on a stale
/// "covered" verdict deletes rows for a repository that has since left the
/// installation. `previous` is the last answer: when GitHub reports the same
/// `updated_at`, the enumerated repository list is reused instead of walked
/// again, so the freshness guarantee costs one request per call rather than
/// a pagination pass.
pub async fn fetch_coverage(
    app: &AppCredentials,
    host: Option<&str>,
    previous: Option<&InstallationCoverage>,
) -> Result<InstallationCoverage, CredentialError> {
    let handle = installation_handle(app, host).await?;

    // `/app/installations/{id}` is the authoritative answer for the account,
    // the selection mode and the change stamp; the repository list alone
    // cannot name the account of an installation that happens to hold no
    // repositories.
    let installation = handle
        .app
        .apps()
        .installation(InstallationId(handle.installation_id))
        .await
        .map_err(|e| CredentialError::Provider(format!("read GitHub App installation: {e}")))?;
    let account = installation.account.login.to_ascii_lowercase();
    let all_repositories = installation
        .repository_selection
        .as_deref()
        .is_some_and(|s| s.eq_ignore_ascii_case("all"));
    let updated_at = installation.updated_at.map(|at| at.to_rfc3339());

    if all_repositories {
        return Ok(InstallationCoverage {
            installation_id: handle.installation_id,
            account,
            all_repositories: true,
            repositories: BTreeSet::new(),
            updated_at,
        });
    }

    // An unchanged stamp means the installation's repository selection has
    // not moved, so the list we already hold is still exact.
    if let Some(prev) = previous
        && !prev.all_repositories
        && prev.installation_id == handle.installation_id
        && prev.updated_at.is_some()
        && prev.updated_at == updated_at
    {
        return Ok(InstallationCoverage {
            installation_id: handle.installation_id,
            account,
            all_repositories: false,
            repositories: prev.repositories.clone(),
            updated_at,
        });
    }

    #[derive(serde::Serialize)]
    struct PageParams {
        per_page: u8,
        page: u32,
    }
    #[derive(serde::Deserialize)]
    struct Owner {
        login: String,
    }
    #[derive(serde::Deserialize)]
    struct Repo {
        name: String,
        owner: Owner,
    }
    #[derive(serde::Deserialize)]
    struct RepoPage {
        #[serde(default)]
        repositories: Vec<Repo>,
    }

    // Paginated by hand: octocrab has no typed builder for this route, and a
    // selected-repository list is small enough that the walk is one request
    // in practice.
    let mut repositories = BTreeSet::new();
    for page in 1..=MAX_COVERAGE_PAGES {
        let body: RepoPage = handle
            .installation
            .get(
                "/installation/repositories",
                Some(&PageParams {
                    per_page: PER_PAGE,
                    page,
                }),
            )
            .await
            .map_err(|e| CredentialError::Provider(format!("list GitHub App repositories: {e}")))?;
        let count = body.repositories.len();
        for repo in body.repositories {
            repositories.insert(format!("{}/{}", repo.owner.login, repo.name).to_ascii_lowercase());
        }
        if count < PER_PAGE as usize {
            break;
        }
    }

    Ok(InstallationCoverage {
        installation_id: handle.installation_id,
        account,
        all_repositories,
        repositories,
        updated_at,
    })
}

const PER_PAGE: u8 = 100;

/// Cap on the `/installation/repositories` walk. 50 pages is 5,000 repos —
/// far past any real installation, and a bound so a server that keeps
/// answering "full page" can never spin the poll tick forever.
const MAX_COVERAGE_PAGES: u32 = 50;

/// One App's authenticated transports: the JWT-authenticated App client and
/// the installation client, which holds octocrab's cached installation token.
#[derive(Clone)]
struct InstallationHandle {
    app: Octocrab,
    installation: Octocrab,
    installation_id: u64,
}

/// Process-global installation transports, keyed by App + installation +
/// host. The cache is what makes refresh invisible: the token lives inside
/// the installation client, so a provider constructed fresh on every poll
/// tick still reads the token minted an hour ago instead of minting a new
/// one — which would rotate the credential fingerprint every tick and make
/// the daemon rebuild its GitHub client just as often.
fn installation_handles() -> &'static tokio::sync::Mutex<HashMap<String, InstallationHandle>> {
    static HANDLES: OnceLock<tokio::sync::Mutex<HashMap<String, InstallationHandle>>> =
        OnceLock::new();
    HANDLES.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()))
}

async fn installation_handle(
    app: &AppCredentials,
    host: Option<&str>,
) -> Result<InstallationHandle, CredentialError> {
    let key = format!(
        "{}|{}|{}|{}",
        app.app_id,
        app.installation_id.unwrap_or_default(),
        host.unwrap_or_default(),
        crate::credential_fingerprint(&app.private_key_pem),
    );
    let mut handles = installation_handles().lock().await;
    if let Some(handle) = handles.get(&key) {
        return Ok(handle.clone());
    }

    let encoding_key = jsonwebtoken::EncodingKey::from_rsa_pem(app.private_key_pem.as_bytes())
        .map_err(|e| {
            CredentialError::Provider(format!("GitHub App private key is not an RSA PEM: {e}"))
        })?;
    // The builder is not `Send`; keep it inside a block so it is dropped
    // before the awaits below, which run in a `Send` provider future.
    let app_crab = {
        let mut builder = Octocrab::builder().app(AppId(app.app_id), encoding_key);
        if let Some(host) = host {
            builder = builder
                .base_uri(format!("https://{host}/api/v3"))
                .map_err(|e| CredentialError::Provider(format!("GitHub App base uri: {e}")))?;
        }
        builder
            .build()
            .map_err(|e| CredentialError::Provider(format!("build GitHub App client: {e}")))?
    };

    let installation_id = match app.installation_id {
        Some(id) => id,
        None => discover_installation(&app_crab).await?,
    };
    let installation = app_crab
        .installation(InstallationId(installation_id))
        .map_err(|e| {
            CredentialError::Provider(format!("GitHub App installation {installation_id}: {e}"))
        })?;

    let handle = InstallationHandle {
        app: app_crab,
        installation,
        installation_id,
    };
    handles.insert(key, handle.clone());
    Ok(handle)
}

/// Find the App's single installation. Several installations with no
/// configured id is a real failure, not an absence: picking one arbitrarily
/// would poll the wrong org and look like an empty inbox.
async fn discover_installation(app_crab: &Octocrab) -> Result<u64, CredentialError> {
    let page = app_crab
        .apps()
        .installations()
        .per_page(100)
        .send()
        .await
        .map_err(|e| CredentialError::Provider(format!("list GitHub App installations: {e}")))?;
    let mut ids = page.items.iter().map(|i| i.id.0);
    let Some(first) = ids.next() else {
        return Err(CredentialError::Provider(
            "the GitHub App has no installations; install it on your org or account".into(),
        ));
    };
    if ids.next().is_some() {
        return Err(CredentialError::Provider(format!(
            "the GitHub App has {} installations; set `providers.github.app.installation_id`",
            page.items.len()
        )));
    }
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn coverage(all: bool, repos: &[&str]) -> InstallationCoverage {
        InstallationCoverage {
            installation_id: 7,
            account: "acme".into(),
            all_repositories: all,
            repositories: repos.iter().map(|r| r.to_string()).collect(),
            updated_at: None,
        }
    }

    #[test]
    fn enumerated_installation_covers_only_its_repositories() {
        let c = coverage(false, &["acme/widget", "acme/gadget"]);
        assert!(c.covers("acme/widget"));
        assert!(c.covers("ACME/Widget"), "repo matching is case-insensitive");
        assert!(!c.covers("acme/sprocket"));
        assert!(!c.covers("other/widget"));
    }

    #[test]
    fn enumerated_installation_never_covers_a_whole_org() {
        // A repo created tomorrow falls inside the `acme` poll scope but
        // outside an enumerated installation, so claiming coverage here
        // would silently drop it from the inbox.
        let c = coverage(false, &["acme/widget"]);
        assert!(!c.covers("acme"));
    }

    #[test]
    fn all_repositories_covers_the_org_and_every_repo_under_it() {
        let c = coverage(true, &[]);
        assert!(c.covers("acme"));
        assert!(c.covers("acme/anything"));
        assert!(!c.covers("other"));
        assert!(!c.covers("other/anything"));
    }

    #[test]
    fn uncovered_reports_the_gap_in_input_order_without_duplicates() {
        let c = coverage(false, &["acme/widget"]);
        assert_eq!(
            c.uncovered([
                "acme/widget",
                "acme/sprocket",
                "other/thing",
                "ACME/SPROCKET"
            ]),
            vec!["acme/sprocket".to_string(), "other/thing".to_string()],
        );
        assert!(c.uncovered(["acme/widget"]).is_empty());
    }

    #[test]
    fn installation_sources_are_distinguishable_from_user_tokens() {
        let app = AppCredentials {
            app_id: 42,
            private_key_pem: "pem".into(),
            installation_id: Some(7),
        };
        let source = app.credential_source(7);
        assert_eq!(source, "github-app:42/installation:7");
        assert!(is_installation_source(&source));
        assert!(!is_installation_source("cmd:gh auth token"));
        assert!(!is_installation_source("env:GH_TOKEN"));
        assert!(!is_installation_source(crate::oauth::CREDENTIAL_SOURCE));
    }

    #[test]
    fn app_credentials_never_print_their_signing_key() {
        let app = AppCredentials {
            app_id: 42,
            private_key_pem: "-----BEGIN RSA PRIVATE KEY-----secret".into(),
            installation_id: None,
        };
        let rendered = format!("{app:?}");
        assert!(!rendered.contains("secret"), "{rendered}");
        assert!(rendered.contains("[REDACTED]"), "{rendered}");
    }

    #[test]
    fn a_registration_with_an_unreadable_key_is_declined_not_fatal() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(
            AppCredentials::from_parts("42", &dir.path().join("missing.pem"), None).is_none(),
            "an unreadable key must fall back to the user token, not error"
        );
    }

    #[test]
    fn a_registration_with_a_non_numeric_app_id_is_declined() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pem = dir.path().join("key.pem");
        std::fs::write(&pem, "pem").expect("write pem");
        assert!(AppCredentials::from_parts("not-a-number", &pem, None).is_none());
    }

    #[test]
    fn a_non_numeric_installation_id_falls_back_to_discovery() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pem = dir.path().join("key.pem");
        std::fs::write(&pem, "pem").expect("write pem");
        let app = AppCredentials::from_parts("42", &pem, Some("oops")).expect("registration");
        assert_eq!(app.installation_id, None);
        assert_eq!(app.app_id, 42);
    }

    #[tokio::test]
    async fn a_malformed_private_key_is_a_failure_not_an_absence() {
        // A registered App that cannot sign must NOT read as "no App
        // configured" — the chain would silently fall through and the
        // budget separation would be gone with nothing said about it.
        let provider = InstallationTokenProvider::new(
            AppCredentials {
                app_id: 42,
                private_key_pem: "not a pem".into(),
                installation_id: Some(7),
            },
            None,
        );
        match provider.resolve("github-app").await {
            Err(CredentialError::Provider(msg)) => assert!(msg.contains("RSA PEM"), "{msg}"),
            other => panic!("expected a provider failure, got {other:?}"),
        }
    }
}
