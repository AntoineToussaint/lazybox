//! Several Runners in one window (MULTI_RUNNER.md step 2). The main window
//! shows its own Runner and every other Runner on this machine the person can
//! reach: the ones the launchers registered (`~/.config/lazybox/runners/*.json`,
//! what `lazybox runners` lists) and the ones the person added in Settings →
//! Runners by their locator or endpoint file (another checkout, a standalone
//! Runner). Each is read as the window's own is: a loopback origin and a
//! bearer from a private endpoint file that only this user can read. A Runner
//! that is listed gets an HTTP scope for exactly its RPC path, so the webview
//! can reach it and nothing else. Nothing here listens or leaves the machine.
//! A Runner on another computer (REMOTE_RUNNERS.md) is listed the same way:
//! its locator, written by `lazybox remote add`, names the endpoint file of
//! the link agent that serves it on loopback (links.rs keeps one running), and
//! carries a `remote` field; the agent's `status.json` says how the link is.
use crate::{
    Bootstrap, RPC_PATH,
    host::{APP_WEBVIEWS, hex},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    env, fs,
    io::Write,
    net::{SocketAddr, TcpStream},
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};
use tauri::{Manager, State, ipc::CapabilityBuilder};

/// One Runner as the window lists it (RunnerListing in @lazybox/platform).
#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    id: String,
    name: String,
    product: String,
    channel: String,
    source: &'static str,
    locator: String,
    current: bool,
    state: &'static str,
    problem: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    token: Option<String>,
    /// A Runner on another computer, reached through a link agent (links.rs).
    #[serde(skip_serializing_if = "Option::is_none")]
    remote: Option<Remote>,
}

/// What `lazybox remote add` writes in a remote Runner's locator, and what
/// its link agent says in `status.json` beside the endpoint file.
#[derive(Debug, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Remote {
    host: String,
    route: String,
    machine: String,
    os: String,
    fingerprint: String,
    /// The link: "connecting", "connected", "reconnecting", "refused", "stopped".
    state: String,
    code: String,
    problem: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    rtt_ms: Option<u64>,
}

/// A remote Runner's locator `remote` field, when the file has one.
fn remote_of(locator: &Path) -> Option<Remote> {
    let value: serde_json::Value = serde_json::from_slice(&fs::read(locator).ok()?).ok()?;
    let remote = value.get("remote")?;
    let text = |key: &str| {
        remote
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .chars()
            .take(256)
            .collect::<String>()
    };
    Some(Remote {
        host: text("host"),
        route: text("route"),
        machine: text("machine"),
        os: text("os"),
        fingerprint: text("fingerprint"),
        state: "stopped".into(),
        ..Remote::default()
    })
}

/// The link agent's last word about its link (`status.json` beside the endpoint).
fn read_link_status(remote: &mut Remote, endpoint: &Path) {
    let Some(value) = endpoint
        .parent()
        .and_then(|dir| fs::read(dir.join("status.json")).ok())
        .and_then(|data| serde_json::from_slice::<serde_json::Value>(&data).ok())
    else {
        return;
    };
    let text = |key: &str| {
        value
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .chars()
            .take(512)
            .collect::<String>()
    };
    remote.state = text("state");
    remote.code = text("code");
    remote.problem = text("problem");
    remote.rtt_ms = value.get("rttMs").and_then(|v| v.as_u64());
}

/// What to do about a remote Runner whose link agent is not serving yet.
fn remote_hint(remote: &Remote) -> String {
    format!("Connecting to {} over SSH…", remote.host)
}

/// What a launcher writes (scripts/user-config.mjs): where the Runner's private endpoint file is.
#[derive(Deserialize)]
struct Locator {
    #[serde(default)]
    name: String,
    #[serde(default)]
    product: String,
    endpoint: PathBuf,
}

/// The files the person added, kept in the app's own configuration (never a credential).
#[derive(Default, Deserialize, Serialize)]
struct Added {
    #[serde(default)]
    files: Vec<PathBuf>,
}

/// The origins this app already gave an HTTP scope, so a poll does not add another.
#[derive(Default)]
pub struct Runners {
    allowed: Mutex<HashSet<String>>,
}

const ADDED_LIMIT: usize = 32;
const PROBE: Duration = Duration::from_millis(250);

/// `$XDG_CONFIG_HOME/lazybox`, else `~/.config/lazybox`: the directory the launchers and the CLI use.
fn config_dir() -> Option<PathBuf> {
    if let Some(base) = env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        && base.is_absolute()
    {
        return Some(base.join("lazybox"));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".config").join("lazybox"))
}

fn added_file(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    Ok(app
        .path()
        .app_config_dir()
        .map_err(|e| e.to_string())?
        .join("runners.json"))
}

fn read_added(path: &Path) -> Added {
    fs::read(path)
        .ok()
        .and_then(|data| serde_json::from_slice(&data).ok())
        .unwrap_or_default()
}

fn write_added(path: &Path, added: &Added) -> Result<(), String> {
    let parent = path.parent().ok_or("No configuration directory")?;
    fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    serde_json::to_writer_pretty(&mut file, added).map_err(|e| e.to_string())?;
    file.flush().map_err(|e| e.to_string())?;
    file.persist(path).map_err(|e| e.to_string())?;
    Ok(())
}

/// A Runner's id: stable while its endpoint file stays where it is, and the
/// same whether it was registered or added, so it is listed once.
fn runner_id(endpoint: &Path) -> String {
    let digest = Sha256::digest(endpoint.to_string_lossy().as_bytes());
    hex(&digest[..8])
}

/// The channel a launcher's name stands for.
fn channel(name: &str) -> &'static str {
    match name {
        "dev" => "dev",
        "working" => "working",
        "lazybox" | "release" => "release",
        _ => "",
    }
}

/// What to do about a Runner that is not running, in the person's words.
fn start_hint(name: &str) -> String {
    match name {
        "dev" => "Start it with make dev.".into(),
        "working" => "Start it with make working.".into(),
        "lazybox" | "release" => "Open Lazybox to start it.".into(),
        _ => "Start it, and its workspaces show here.".into(),
    }
}

/// The endpoint file must be this user's alone: it holds an owner bearer.
#[cfg(unix)]
fn private_file(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.file_type().is_file() {
        return Err("Its endpoint is not a regular file".into());
    }
    if meta.permissions().mode() & 0o077 != 0 {
        return Err("Its endpoint file can be read by other users".into());
    }
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("No home directory")?;
    if fs::metadata(home).map_err(|e| e.to_string())?.uid() != meta.uid() {
        return Err("Its endpoint file belongs to another user".into());
    }
    Ok(())
}
#[cfg(not(unix))]
fn private_file(_: &Path) -> Result<(), String> {
    Ok(())
}

/// Whether something accepts connections at a loopback origin (a Runner whose
/// file outlived it does not).
fn listening(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    let Ok(addrs) = parsed.socket_addrs(|| None) else {
        return false;
    };
    addrs
        .iter()
        .filter(|addr: &&SocketAddr| addr.ip().is_loopback())
        .any(|addr| TcpStream::connect_timeout(addr, PROBE).is_ok())
}

/// A file the person points at: a launcher's locator (`endpoint` inside), or
/// the endpoint file itself (`url` and `token` inside).
fn resolve(path: &Path) -> Result<(String, String, PathBuf), String> {
    let data = fs::read(path).map_err(|e| format!("Cannot read {}: {e}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_slice(&data).map_err(|_| "Not a Runner locator or endpoint file")?;
    if value.get("endpoint").is_some() {
        let locator: Locator = serde_json::from_value(value).map_err(|_| "Not a Runner locator")?;
        let name = if locator.name.is_empty() {
            stem(path)
        } else {
            locator.name
        };
        return Ok((name, locator.product, locator.endpoint));
    }
    if value.get("url").is_some() && value.get("token").is_some() {
        return Ok((stem(path), String::new(), path.to_path_buf()));
    }
    Err("Not a Runner locator or endpoint file".into())
}

fn stem(path: &Path) -> String {
    let name = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if name == "endpoint" {
        // `<state>/endpoint.json`: the directory says which Runner.
        if let Some(parent) = path.parent().and_then(|p| p.file_name()) {
            return parent.to_string_lossy().to_string();
        }
    }
    name
}

/// One Runner's listing: running (with its origin and bearer), stopped, or unusable, and why.
fn listing(
    name: String,
    product: String,
    endpoint: &Path,
    locator: &Path,
    source: &'static str,
    own: &Bootstrap,
    own_endpoint: Option<&Path>,
) -> Listing {
    let mut out = Listing {
        id: runner_id(endpoint),
        channel: channel(&name).into(),
        name: name.clone(),
        product,
        source,
        locator: locator.display().to_string(),
        current: own_endpoint == Some(endpoint),
        state: "stopped",
        problem: String::new(),
        url: None,
        token: None,
        remote: remote_of(locator),
    };
    let hint = |out: &Listing| match &out.remote {
        Some(remote) => remote_hint(remote),
        None => start_hint(&name),
    };
    if !endpoint.exists() {
        out.problem = hint(&out);
        return out;
    }
    if let Err(problem) = private_file(endpoint) {
        out.state = "invalid";
        out.problem = problem;
        return out;
    }
    let found = match Bootstrap::read(endpoint) {
        Ok(found) => found,
        Err(error) => {
            out.state = "invalid";
            out.problem = format!("Its endpoint file cannot be used: {error}");
            return out;
        }
    };
    // The window's own Runner, found through another file: one Runner, listed once as the window's.
    if !own.runner_id.is_empty() && found.runner_id == own.runner_id {
        out.current = true;
    }
    if out.current {
        out.state = "running";
        return out;
    }
    if !listening(&found.url) {
        out.problem = hint(&out);
        return out;
    }
    // The link agent serves even while it reconnects: its status says how the link is.
    if let Some(remote) = out.remote.as_mut() {
        read_link_status(remote, endpoint);
    }
    out.state = "running";
    out.url = Some(found.url);
    out.token = Some(found.token);
    out
}

/// Every Runner this app can find now: registered by a launcher, then added by the person.
fn discover(
    config: Option<&Path>,
    added: &Added,
    own: &Bootstrap,
    own_endpoint: Option<&Path>,
) -> Vec<Listing> {
    let mut out: Vec<Listing> = Vec::new();
    let mut seen = HashSet::new();
    if let Some(dir) = config.map(|dir| dir.join("runners"))
        && let Ok(entries) = fs::read_dir(&dir)
    {
        let mut files: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        files.sort();
        for path in files {
            let Ok((name, product, endpoint)) = resolve(&path) else {
                continue;
            };
            if seen.insert(endpoint.clone()) {
                out.push(listing(
                    name,
                    product,
                    &endpoint,
                    &path,
                    "registered",
                    own,
                    own_endpoint,
                ));
            }
        }
    }
    for path in added.files.iter().take(ADDED_LIMIT) {
        match resolve(path) {
            Ok((name, product, endpoint)) => {
                if seen.insert(endpoint.clone()) {
                    out.push(listing(
                        name,
                        product,
                        &endpoint,
                        path,
                        "added",
                        own,
                        own_endpoint,
                    ));
                }
            }
            Err(problem) => out.push(Listing {
                id: runner_id(path),
                name: stem(path),
                product: String::new(),
                channel: String::new(),
                source: "added",
                locator: path.display().to_string(),
                current: false,
                state: "invalid",
                problem,
                url: None,
                token: None,
                remote: None,
            }),
        }
    }
    out
}

/// Lets the app's windows call one more Runner origin's RPC path, once.
fn allow(app: &tauri::AppHandle, runners: &Runners, url: &str) -> Result<(), String> {
    let mut allowed = runners.allowed.lock().map_err(|e| e.to_string())?;
    if allowed.contains(url) {
        return Ok(());
    }
    app.add_capability(
        CapabilityBuilder::new(format!("runner-{}", hex(&Sha256::digest(url.as_bytes()))))
            .webviews(APP_WEBVIEWS)
            .permission_scoped(
                "http:default",
                vec![serde_json::json!({ "url": format!("{url}{RPC_PATH}*") })],
                Vec::<serde_json::Value>::new(),
            ),
    )
    .map_err(|e| e.to_string())?;
    allowed.insert(url.to_string());
    Ok(())
}

fn list(
    app: &tauri::AppHandle,
    host: &crate::host::Host,
    runners: &Runners,
) -> Result<Vec<Listing>, String> {
    let own = host.connection.lock().map_err(|e| e.to_string())?.clone();
    let added = read_added(&added_file(app)?);
    let mut listings = discover(
        config_dir().as_deref(),
        &added,
        &own,
        host.reload_endpoint.as_deref(),
    );
    // One link agent per computer the person added (links.rs).
    let remotes: Vec<(String, bool)> = listings
        .iter()
        .filter(|l| l.remote.is_some() && l.state != "invalid")
        .map(|l| (l.name.clone(), l.state == "running"))
        .collect();
    if let Some(problem) = crate::links::ensure(app, &remotes) {
        for listing in listings
            .iter_mut()
            .filter(|l| l.remote.is_some() && l.state == "stopped")
        {
            listing.problem = problem.clone();
        }
    }
    for url in listings.iter().filter_map(|l| l.url.as_deref()) {
        allow(app, runners, url)?;
    }
    Ok(listings)
}

/// Every Runner the main window can show. Only the main window asks.
#[tauri::command]
pub fn runner_list(
    window: tauri::Webview,
    host: State<'_, crate::host::Host>,
    runners: State<'_, Runners>,
) -> Result<Vec<Listing>, String> {
    if window.label() != "main" {
        return Err("Only the main window shows several Runners".into());
    }
    list(window.app_handle(), &host, &runners)
}

/// Adds a Runner by its locator or endpoint file; it must be a loopback Runner whose file only this user reads.
#[tauri::command]
pub fn runner_add(
    window: tauri::Webview,
    host: State<'_, crate::host::Host>,
    runners: State<'_, Runners>,
    path: String,
) -> Result<Listing, String> {
    if window.label() != "main" {
        return Err("Only the main window adds Runners".into());
    }
    let path = PathBuf::from(expand_home(path.trim()));
    if !path.is_absolute() {
        return Err("Give the full path of the file".into());
    }
    let path =
        fs::canonicalize(&path).map_err(|e| format!("Cannot find {}: {e}", path.display()))?;
    let (_, _, endpoint) = resolve(&path)?;
    let own = host.connection.lock().map_err(|e| e.to_string())?.clone();
    let checked = listing(
        String::new(),
        String::new(),
        &endpoint,
        &path,
        "added",
        &own,
        host.reload_endpoint.as_deref(),
    );
    if checked.state == "invalid" {
        return Err(checked.problem);
    }
    if checked.current {
        return Err("That is this window's own Runner".into());
    }
    let file = added_file(window.app_handle())?;
    let mut added = read_added(&file);
    if !added.files.contains(&path) {
        if added.files.len() >= ADDED_LIMIT {
            return Err("Forget a Runner you added before adding another".into());
        }
        added.files.push(path.clone());
        write_added(&file, &added)?;
    }
    list(window.app_handle(), &host, &runners)?
        .into_iter()
        .find(|l| l.id == checked.id || l.locator == path.display().to_string())
        .ok_or_else(|| "The Runner was added but is not listed".into())
}

/// Forgets a Runner the person added (a launcher's are listed while their locator exists).
#[tauri::command]
pub fn runner_forget(window: tauri::Webview, id: String) -> Result<(), String> {
    if window.label() != "main" {
        return Err("Only the main window forgets Runners".into());
    }
    let file = added_file(window.app_handle())?;
    let mut added = read_added(&file);
    let before = added.files.len();
    added.files.retain(|path| {
        let endpoint = resolve(path)
            .map(|(_, _, e)| e)
            .unwrap_or_else(|_| path.clone());
        runner_id(&endpoint) != id && runner_id(path) != id
    });
    if added.files.len() == before {
        return Err("No Runner you added has that id".into());
    }
    write_added(&file, &added)
}

fn expand_home(path: &str) -> String {
    match (path.strip_prefix("~/"), env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => path.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn own() -> Bootstrap {
        Bootstrap {
            url: "http://127.0.0.1:1".into(),
            token: "own".into(),
            runner_id: "ownrunneridentity0".into(),
            working_directory: String::new(),
            session_id: String::new(),
        }
    }

    fn endpoint(dir: &Path, name: &str, url: &str, runner: &str) -> PathBuf {
        let path = dir.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, serde_json::json!({ "url": url, "token": "bearer-of-".to_string() + name, "runnerId": runner }).to_string()).unwrap();
        #[cfg(unix)]
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    fn locator(config: &Path, name: &str, endpoint: &Path) {
        let dir = config.join("runners");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join(format!("{name}.json")),
            serde_json::json!({ "name": name, "product": "Lazybox Fixture", "endpoint": endpoint })
                .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn lists_registered_runners_with_their_state_once_each() {
        let temp = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let live = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let gone = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = format!("http://127.0.0.1:{}", gone.local_addr().unwrap().port());
        drop(gone);
        let config = temp.path().join("config");
        let working = endpoint(
            temp.path(),
            "working/endpoint.json",
            &live,
            "workingrunneridentity",
        );
        let stale = endpoint(
            temp.path(),
            "dev/endpoint.json",
            &dead,
            "devrunneridentity000",
        );
        locator(&config, "working", &working);
        locator(&config, "dev", &stale);
        locator(
            &config,
            "lazybox",
            &temp.path().join("release/endpoint.json"),
        );
        // The window's own Runner, registered too: listed as the window's, without its bearer.
        let mine = endpoint(
            temp.path(),
            "mine/endpoint.json",
            &live,
            "ownrunneridentity0",
        );
        locator(&config, "mine", &mine);
        // Added by the person and also registered: listed once.
        let added = Added {
            files: vec![config.join("runners/working.json")],
        };

        let listings = discover(Some(&config), &added, &own(), None);
        let by = |name: &str| listings.iter().find(|l| l.name == name).unwrap();
        assert_eq!(listings.len(), 4);
        let w = by("working");
        assert_eq!(
            (w.state, w.channel.as_str(), w.source, w.current),
            ("running", "working", "registered", false)
        );
        assert_eq!(w.url.as_deref(), Some(live.as_str()));
        assert_eq!(w.token.as_deref(), Some("bearer-of-working/endpoint.json"));
        let d = by("dev");
        assert_eq!(
            (d.state, d.url.is_none(), d.token.is_none()),
            ("stopped", true, true)
        );
        assert_eq!(d.problem, "Start it with make dev.");
        assert_eq!(by("lazybox").problem, "Open Lazybox to start it.");
        assert_eq!(by("lazybox").channel, "release");
        let m = by("mine");
        assert!(m.current && m.token.is_none() && m.url.is_none());
        assert!(
            listings
                .iter()
                .all(|l| l.id.len() == 16 && l.id.bytes().all(|b| b.is_ascii_hexdigit()))
        );
        drop(listener);
    }

    #[cfg(unix)]
    #[test]
    fn refuses_endpoint_files_others_can_read_and_non_loopback_origins() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let open = endpoint(
            temp.path(),
            "open/endpoint.json",
            "http://127.0.0.1:9",
            "openrunneridentity00",
        );
        fs::set_permissions(&open, fs::Permissions::from_mode(0o644)).unwrap();
        locator(&config, "open", &open);
        let lan = endpoint(
            temp.path(),
            "lan/endpoint.json",
            "http://192.168.1.20:7000",
            "lanrunneridentity000",
        );
        locator(&config, "lan", &lan);
        let listings = discover(Some(&config), &Added::default(), &own(), None);
        for listing in &listings {
            assert_eq!(listing.state, "invalid", "{listing:?}");
            assert!(listing.token.is_none() && listing.url.is_none());
        }
        assert!(listings.iter().any(|l| l.problem.contains("other users")));
        assert!(
            listings
                .iter()
                .any(|l| l.problem.contains("cannot be used"))
        );
    }

    #[test]
    fn a_file_is_a_locator_or_an_endpoint_and_nothing_else() {
        let temp = tempfile::tempdir().unwrap();
        let file = endpoint(
            temp.path(),
            "standalone/endpoint.json",
            "http://127.0.0.1:9",
            "standalonerunnerid00",
        );
        let (name, _, resolved) = resolve(&file).unwrap();
        assert_eq!(
            (name.as_str(), resolved.as_path()),
            ("standalone", file.as_path())
        );
        let other = temp.path().join("other.json");
        fs::write(&other, r#"{"hello":"world"}"#).unwrap();
        assert!(resolve(&other).is_err());
        assert!(resolve(&temp.path().join("missing.json")).is_err());
        // An added file that is gone stays listed, so the person can forget it.
        let listings = discover(
            None,
            &Added {
                files: vec![other.clone()],
            },
            &own(),
            None,
        );
        assert_eq!(listings[0].state, "invalid");
        assert_eq!(listings[0].source, "added");
    }

    #[test]
    fn a_remote_runner_is_listed_through_its_link_agent_with_the_links_state() {
        let temp = tempfile::tempdir().unwrap();
        let config = temp.path().join("config");
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let live = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        // What `lazybox remote add` writes: a locator with `remote`, naming the link agent's endpoint.
        let file = endpoint(
            temp.path(),
            "remotes/build-box/endpoint.json",
            &live,
            "remoterunneridentity00",
        );
        fs::create_dir_all(config.join("runners")).unwrap();
        fs::write(
            config.join("runners/build-box.json"),
            serde_json::json!({ "name": "build-box", "product": "Lazybox", "endpoint": file,
                "remote": { "host": "build-box", "route": "ssh", "machine": "build-box", "os": "Linux", "fingerprint": "SHA256:abc" } })
            .to_string(),
        )
        .unwrap();
        fs::write(
            file.parent().unwrap().join("status.json"),
            serde_json::json!({ "state": "reconnecting", "rttMs": 41, "problem": "", "pid": 1 })
                .to_string(),
        )
        .unwrap();
        let listings = discover(Some(&config), &Added::default(), &own(), None);
        let remote = listings[0].remote.as_ref().unwrap();
        assert_eq!(listings[0].state, "running");
        assert_eq!(listings[0].url.as_deref(), Some(live.as_str()));
        assert_eq!(
            (
                remote.host.as_str(),
                remote.route.as_str(),
                remote.state.as_str(),
                remote.rtt_ms
            ),
            ("build-box", "ssh", "reconnecting", Some(41))
        );
        // The link agent is not serving (yet): stopped, and the app is connecting.
        let gone = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = format!("http://127.0.0.1:{}", gone.local_addr().unwrap().port());
        drop(gone);
        endpoint(
            temp.path(),
            "remotes/build-box/endpoint.json",
            &dead,
            "remoterunneridentity00",
        );
        let listings = discover(Some(&config), &Added::default(), &own(), None);
        assert_eq!(listings[0].state, "stopped");
        assert_eq!(listings[0].problem, "Connecting to build-box over SSH…");
        assert!(listings[0].token.is_none());
        // A local Runner has no `remote`.
        let json = serde_json::to_value(&listings[0]).unwrap();
        assert_eq!(json["remote"]["host"], "build-box");
        assert!(json["remote"].get("rttMs").is_none());
    }

    #[test]
    fn ids_are_stable_and_safe_in_keys() {
        let a = runner_id(Path::new("/Users/me/.lazybox/working/endpoint.json"));
        assert_eq!(
            a,
            runner_id(Path::new("/Users/me/.lazybox/working/endpoint.json"))
        );
        assert_ne!(
            a,
            runner_id(Path::new("/Users/me/.lazybox/dev/endpoint.json"))
        );
        assert!(crate::layout::valid_id(&a));
    }
}
