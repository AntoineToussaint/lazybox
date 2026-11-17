//! Updates for an installed release: the Tauri updater reads a signed feed
//! (latest.json on the `v2-updates` GitHub release, RELEASING.md), and an
//! accepted update moves the Runner service to the new version before the
//! app restarts, so the two never run different versions for long. Agents
//! keep running across that Runner restart (SESSION_HOSTS.md).
//!
//! Nothing here runs on its own: the window asks (Settings → Updates, the
//! header's notice), and installs only when the person chooses Install.
//!
//! Only a release build carries an updater configuration
//! (packaging/release.tauri.json plus the public key the build adds); Dev
//! and Working update through stage and promote, and get none of this.

use serde::{Deserialize, Serialize};
use std::{sync::Mutex, time::Duration};
use tauri::{Emitter, Manager, Webview};
use tauri_plugin_updater::{Update, UpdaterExt};

/// How long a check waits for the feed. A download has no overall limit: a
/// release is a few hundred megabytes on whatever network the person has.
const CHECK_TIMEOUT: Duration = Duration::from_secs(20);

/// Whether this build can check for updates: a public key, only HTTPS
/// feeds, and none of the updater's `dangerous*` relaxations. A build
/// without them never registers the updater, so verification cannot be
/// turned off: there is no updater left to install anything.
pub fn configured(config: &tauri::Config) -> bool {
    let Some(updater) = config.plugins.0.get("updater") else {
        return false;
    };
    let relaxed = updater.as_object().is_some_and(|fields| {
        fields.iter().any(|(name, value)| {
            name.starts_with("dangerous") && value != &serde_json::json!(false)
        })
    });
    if relaxed {
        return false;
    }
    let pubkey = updater.get("pubkey").and_then(|v| v.as_str()).unwrap_or("");
    let endpoints = updater
        .get("endpoints")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    !pubkey.trim().is_empty()
        && !endpoints.is_empty()
        && endpoints.iter().all(|e| {
            e.as_str()
                .and_then(|s| url::Url::parse(s).ok())
                .is_some_and(|u| u.scheme() == "https")
        })
}

/// The update found by the last check, kept so installing it does not fetch the feed again.
#[derive(Default)]
pub struct Pending(pub Mutex<Option<Update>>);

/// One changelog section the feed carries (latest.json `lazybox.releases`,
/// written by scripts/release/build.mjs), shown before installing.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ReleaseNotes {
    id: String,
    date: String,
    title: String,
    items: Vec<ReleaseItem>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ReleaseItem {
    /// The heading a big release groups this change under (`### Group`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    group: Option<String>,
    title: String,
    body: String,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Available {
    version: String,
    current: String,
    notes: String,
    date: String,
    releases: Vec<ReleaseNotes>,
}

/// The structured notes of a feed entry. A feed without them, or with ones
/// this app cannot read, still offers the update with its plain notes.
pub fn releases_of(feed: &serde_json::Value) -> Vec<ReleaseNotes> {
    feed.get("lazybox")
        .and_then(|l| l.get("releases"))
        .and_then(|r| serde_json::from_value::<Vec<ReleaseNotes>>(r.clone()).ok())
        .unwrap_or_default()
        .into_iter()
        .take(50)
        .collect()
}

/// Why a check or an install failed, for the window to say as a sentence
/// (`UpdateErrorKind` in packages/platform). `message` is the updater's own,
/// kept for a report; the window does not show it as the explanation.
#[derive(Serialize, Debug, PartialEq)]
pub struct UpdateError {
    kind: &'static str,
    message: String,
}

impl UpdateError {
    fn new(kind: &'static str, message: impl ToString) -> Self {
        Self {
            kind,
            message: message.to_string(),
        }
    }
}

/// Classifies an updater failure. Anything about the signature means the
/// download was refused and nothing was replaced; a network failure is
/// "offline"; a feed that answers with no valid entry is "feed".
pub fn classify(error: &tauri_plugin_updater::Error) -> UpdateError {
    use tauri_plugin_updater::Error as E;
    let kind = match error {
        E::Minisign(_)
        | E::Base64(_)
        | E::SignatureUtf8(_)
        | E::SignedVersionMismatch { .. }
        | E::MissingSignedVersion => "signature",
        E::Reqwest(e) if e.is_connect() || e.is_timeout() || e.is_request() || e.is_body() => {
            "offline"
        }
        E::Network(_) => "offline",
        E::ReleaseNotFound
        | E::Serialization(_)
        | E::Semver(_)
        | E::TargetNotFound(_)
        | E::TargetsNotFound(_)
        | E::UrlParse(_) => "feed",
        E::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied => "permission",
        E::TempDirNotOnSameMountPoint | E::AuthenticationFailed => "permission",
        _ => "install",
    };
    UpdateError::new(kind, error)
}

#[tauri::command]
pub async fn check_for_update(app: tauri::AppHandle) -> Result<Option<Available>, UpdateError> {
    if !configured(app.config()) {
        return Err(UpdateError::new(
            "unavailable",
            "This build of Lazybox does not update itself",
        ));
    }
    let update = app
        .updater_builder()
        .timeout(CHECK_TIMEOUT)
        .build()
        .map_err(|e| classify(&e))?
        .check()
        .await
        .map_err(|e| classify(&e))?;
    let found = update.as_ref().map(|u| Available {
        version: u.version.clone(),
        current: u.current_version.clone(),
        notes: u.body.clone().unwrap_or_default(),
        date: u.date.map(|d| d.to_string()).unwrap_or_default(),
        releases: releases_of(&u.raw_json),
    });
    *app.state::<Pending>()
        .0
        .lock()
        .map_err(|e| UpdateError::new("install", e))? = update.map(|mut u| {
        // The check's limit is not the download's.
        u.timeout = None;
        u
    });
    Ok(found)
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
struct Progress {
    phase: &'static str,
    downloaded: u64,
    total: u64,
}

fn emit(webview: &Webview, phase: &'static str) {
    let _ = webview.emit(
        "update-progress",
        Progress {
            phase,
            downloaded: 0,
            total: 0,
        },
    );
}

/// Puts an update back after a failed download or install, so Try again
/// does not need another check.
fn keep(app: &tauri::AppHandle, update: Update) {
    if let Ok(mut pending) = app.state::<Pending>().0.lock() {
        *pending = Some(update);
    }
}

/// Downloads the pending update, verifies it against the build's public key
/// (the updater refuses anything else, before anything is replaced),
/// installs it, moves the Runner service to it and restarts the app.
///
/// Running agents are safe across this (SESSION_HOSTS.md): the bundle is
/// swapped on disk while every running program keeps its own executable,
/// then `lazybox service install` boots the old Runner out (it detaches;
/// its session hosts live in sessions of their own) and bootstraps the new
/// one, which re-attaches them.
#[tauri::command]
pub async fn install_update(app: tauri::AppHandle, webview: Webview) -> Result<(), UpdateError> {
    let update = app
        .state::<Pending>()
        .0
        .lock()
        .map_err(|e| UpdateError::new("install", e))?
        .take()
        .ok_or_else(|| {
            UpdateError::new("install", "No update to install; check for updates first")
        })?;
    let mut downloaded = 0u64;
    let progress = webview.clone();
    let verifying = webview.clone();
    let result = update
        .download(
            move |chunk, total| {
                downloaded += chunk as u64;
                let _ = progress.emit(
                    "update-progress",
                    Progress {
                        phase: "download",
                        downloaded,
                        total: total.unwrap_or(0),
                    },
                );
            },
            move || emit(&verifying, "verify"),
        )
        .await;
    let bytes = match result {
        Ok(bytes) => bytes,
        Err(error) => {
            let failure = classify(&error);
            keep(&app, update);
            return Err(failure);
        }
    };
    emit(&webview, "install");
    if let Err(error) = update.install(&bytes) {
        let failure = classify(&error);
        keep(&app, update);
        return Err(failure);
    }
    emit(&webview, "runner");
    // The bundle on disk is the new version now, and so is its CLI. If this
    // fails, the next launch updates the service anyway (release::prepare).
    if let Some(release) = crate::release::Release::detect(&app) {
        let result =
            tauri::async_runtime::spawn_blocking(move || release.json(&["service", "install"]))
                .await;
        if let Ok(Err(error)) | Err(error) = result.map_err(|e| e.to_string()) {
            eprintln!("Updating the Runner service after the app update: {error}");
        }
    }
    emit(&webview, "restart");
    app.restart()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(plugins: serde_json::Value) -> tauri::Config {
        let mut config: tauri::Config = serde_json::from_value(serde_json::json!({
            "identifier": "ai.lazybox.test"
        }))
        .expect("config");
        config.plugins = serde_json::from_value(plugins).expect("plugins");
        config
    }

    #[test]
    fn only_a_signed_https_feed_enables_updates() {
        // Dev and Working: no updater section at all.
        assert!(!configured(&config(serde_json::json!({}))));
        let feed =
            "https://github.com/AntoineToussaint/lazybox/releases/download/v2-updates/latest.json";
        assert!(configured(&config(
            serde_json::json!({ "updater": { "pubkey": "dW50cnVzdGVk", "endpoints": [feed] } })
        )));
        // Without the public key the build adds, nothing can be verified.
        assert!(!configured(&config(
            serde_json::json!({ "updater": { "pubkey": "", "endpoints": [feed] } })
        )));
        assert!(!configured(&config(
            serde_json::json!({ "updater": { "pubkey": "k", "endpoints": [] } })
        )));
        assert!(!configured(&config(
            serde_json::json!({ "updater": { "pubkey": "k", "endpoints": ["http://example.com/latest.json"] } })
        )));
        // Nothing may weaken what is verified.
        for relaxation in [
            "dangerousInsecureTransportProtocol",
            "dangerousAcceptInvalidCerts",
            "dangerousAcceptInvalidHostnames",
        ] {
            assert!(!configured(&config(serde_json::json!({ "updater": {
                "pubkey": "dW50cnVzdGVk", "endpoints": [feed], relaxation: true
            } }))));
        }
    }

    #[test]
    fn failures_are_classified_for_the_window() {
        use tauri_plugin_updater::Error as E;
        let kind = |e: E| classify(&e).kind;
        assert_eq!(kind(E::Network("connection reset".into())), "offline");
        assert_eq!(kind(E::ReleaseNotFound), "feed");
        assert_eq!(kind(E::TargetNotFound("darwin-aarch64".into())), "feed");
        assert_eq!(kind(E::SignatureUtf8("x".into())), "signature");
        assert_eq!(kind(E::MissingSignedVersion), "signature");
        assert_eq!(
            kind(E::SignedVersionMismatch {
                signed: "0.1.0".into(),
                announced: "0.2.0".into()
            }),
            "signature"
        );
        assert_eq!(
            kind(E::Io(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied
            ))),
            "permission"
        );
        assert_eq!(kind(E::TempDirNotOnSameMountPoint), "permission");
        assert_eq!(
            kind(E::PreviousAppNotRestored("/tmp/kept".into())),
            "install"
        );
        // The updater's own words are kept for a report.
        assert_eq!(
            classify(&E::ReleaseNotFound).message,
            "Could not fetch a valid release JSON from the remote"
        );
    }

    #[test]
    fn a_feed_carries_its_release_notes_and_still_works_without_them() {
        let feed = serde_json::json!({
            "version": "0.2.0",
            "notes": "Second",
            "lazybox": { "releases": [
                { "id": "0.2.0", "date": "2026-12-01", "title": "Second", "items": [{ "title": "New", "body": "It works." }, { "group": "Chat", "title": "Grouped", "body": "Under a heading." }] },
                { "id": "0.1.0", "date": "2026-11-17", "title": "First", "items": [] }
            ] }
        });
        let releases = releases_of(&feed);
        assert_eq!(releases.len(), 2);
        assert_eq!(releases[0].id, "0.2.0");
        assert_eq!(releases[0].items[0].body, "It works.");
        assert_eq!(releases[0].items[0].group, None);
        assert_eq!(releases[0].items[1].group.as_deref(), Some("Chat"));
        assert!(releases_of(&serde_json::json!({ "version": "0.2.0" })).is_empty());
        // Unreadable notes never stop the update from being offered.
        assert!(releases_of(&serde_json::json!({ "lazybox": { "releases": "nope" } })).is_empty());
    }

    /// The structured error is what the window reads (`{ kind, message }`).
    #[test]
    fn an_update_error_serializes_as_kind_and_message() {
        assert_eq!(
            serde_json::to_value(UpdateError::new("offline", "no route")).expect("json"),
            serde_json::json!({ "kind": "offline", "message": "no route" })
        );
    }

    /// The committed configuration ships no updater: only a release build
    /// merges one in, with its public key (scripts/release/build.mjs).
    #[test]
    fn the_committed_configuration_does_not_update_itself() {
        let config: tauri::Config =
            serde_json::from_str(include_str!("../tauri.conf.json")).expect("tauri.conf.json");
        assert!(!configured(&config));
    }

    /// The release overlay names an HTTPS feed and never a key: the public
    /// key is added at build time, and no private key is ever in the tree.
    #[test]
    fn the_release_overlay_has_a_feed_and_no_key() {
        let overlay: serde_json::Value =
            serde_json::from_str(include_str!("../../../../packaging/release.tauri.json"))
                .expect("release.tauri.json");
        let updater = &overlay["plugins"]["updater"];
        assert!(updater.get("pubkey").is_none());
        let endpoints = updater["endpoints"].as_array().expect("endpoints");
        assert!(!endpoints.is_empty());
        for endpoint in endpoints {
            assert!(
                endpoint
                    .as_str()
                    .expect("endpoint")
                    .starts_with("https://github.com/")
            );
        }
        assert_eq!(overlay["identifier"], "ai.lazybox.app");
        // The signature must name the version the feed announces: a feed
        // cannot pair a new version number with an older signed release.
        assert_eq!(updater["requireSignedVersion"], true);
        let mut merged = overlay.clone();
        merged["plugins"]["updater"]["pubkey"] = "dW50cnVzdGVk".into();
        let mut config: tauri::Config =
            serde_json::from_str(include_str!("../tauri.conf.json")).expect("tauri.conf.json");
        config.plugins = serde_json::from_value(merged["plugins"].clone()).expect("plugins");
        assert!(configured(&config));
    }
}
