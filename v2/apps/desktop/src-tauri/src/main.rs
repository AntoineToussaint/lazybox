#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::{error::Error, fs, path::Path};
mod host;
mod layout;
mod placement;
mod viewstate;

const RPC_PATH: &str = "/lazybox.bench.v1.RunnerService/";

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Bootstrap {
    url: String,
    token: String,
    #[serde(default)]
    runner_id: String,
    #[serde(default)]
    working_directory: String,
    #[serde(default)]
    session_id: String,
}

impl Bootstrap {
    fn read(path: &Path) -> Result<Self, Box<dyn Error>> {
        let mut config: Self = serde_json::from_slice(&fs::read(path)?)?;
        config.url = validate_endpoint(&config.url)?;
        if config.token.is_empty() || config.token.chars().any(char::is_whitespace) {
            return Err("Runner endpoint must contain a nonempty bearer token".into());
        }
        if !config.runner_id.is_empty()
            && (config.runner_id.len() < 16
                || config.runner_id.len() > 128
                || !config.runner_id.bytes().all(|c| c.is_ascii_alphanumeric()))
        {
            return Err("Invalid Runner identity".into());
        }
        Ok(config)
    }

    // Codefly keeps this invocation's port while replacing its Go process.
    // Accept its new bearer only from the already-selected private locator;
    // moving to another origin requires a new launch and a new HTTP scope.
    fn refreshed(&self, path: &Path) -> Result<Self, Box<dyn Error>> {
        let next = Self::read(path)?;
        if !self.runner_id.is_empty() && self.runner_id != next.runner_id {
            return Err("Runner identity changed; open this connection in a new app launch".into());
        }
        if next.url != self.url {
            return Err("Runner address changed; relaunch Dev to select its new origin".into());
        }
        Ok(Self {
            token: next.token,
            ..self.clone()
        })
    }

    // Explicit reconnect may adopt a newly allocated loopback origin from the
    // same private locator. The host installs that exact HTTP scope first.
    fn reconnected(&self, path: &Path) -> Result<Self, Box<dyn Error>> {
        let next = Self::read(path)?;
        if !self.runner_id.is_empty() && self.runner_id != next.runner_id {
            return Err("Runner identity changed; open this connection in a new app launch".into());
        }
        Ok(Self {
            url: next.url,
            token: next.token,
            ..self.clone()
        })
    }
}

fn validate_endpoint(value: &str) -> Result<String, Box<dyn Error>> {
    let url = url::Url::parse(value)?;
    let local = match url.host() {
        Some(url::Host::Domain("localhost")) => true,
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    if url.scheme() != "http"
        || !local
        || url.port().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err("Expected the local Runner HTTP origin with its resolved port".into());
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

fn main() -> Result<(), Box<dyn Error>> {
    let application = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            host::second_instance(app, args)
        }))
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            host::runner_bootstrap,
            host::save_view_state,
            host::open_sign_in,
            host::copy_sign_in_code,
            host::choose_repository_folder,
            host::save_surface,
            host::open_console,
            host::open_chat,
            host::open_workspace,
            host::dock_chat,
            host::reopen_agent,
            host::reopen_closed,
            host::reconnect_view
        ])
        .setup(host::setup)
        .on_window_event(host::window_event)
        .on_menu_event(|app, event| host::menu_event(app, event.id().as_ref()))
        .build(tauri::generate_context!())?;
    application.run(|app, event| match event {
        tauri::RunEvent::ExitRequested {
            code: None, api, ..
        } => api.prevent_exit(),
        tauri::RunEvent::ExitRequested { code: Some(_), .. } => {
            host::finish(app);
        }
        tauri::RunEvent::Exit => host::finish(app),
        #[cfg(target_os = "macos")]
        tauri::RunEvent::Reopen {
            has_visible_windows: false,
            ..
        } => {
            if let Err(error) = host::open(app, "main") {
                host::report(app, error);
            }
        }
        _ => (),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bootstrap_reads_the_supplied_file_and_rejects_missing_or_invalid_authority() {
        let path = std::env::temp_dir().join(format!(
            "lazybox-v2-bootstrap-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        struct Remove(std::path::PathBuf);
        impl Drop for Remove {
            fn drop(&mut self) {
                let _ = fs::remove_file(&self.0);
            }
        }
        let _remove = Remove(path.clone());
        assert!(Bootstrap::read(&path).is_err());
        for invalid in [
            "not json",
            r#"{"url":"http://localhost:43210"}"#,
            r#"{"url":"http://localhost:43210","token":""}"#,
            r#"{"url":"http://example.com:43210","token":"fixture"}"#,
        ] {
            fs::write(&path, invalid).expect("write fixture");
            assert!(Bootstrap::read(&path).is_err());
        }
        fs::write(
            &path,
            r#"{"url":"http://localhost:43210/","token":"fixture"}"#,
        )
        .expect("write endpoint");
        let bootstrap = Bootstrap::read(&path).expect("read endpoint");
        assert_eq!(bootstrap.url, "http://localhost:43210");
        assert_eq!(bootstrap.token, "fixture");
        assert!(bootstrap.working_directory.is_empty());
    }

    #[test]
    fn local_endpoint_keeps_the_resolved_authority_and_rejects_other_destinations() {
        for origin in [
            "http://localhost:43210",
            "http://127.0.0.1:43210",
            "http://[::1]:43210/",
        ] {
            assert_eq!(
                validate_endpoint(origin).expect("local endpoint"),
                origin.trim_end_matches('/')
            );
        }
        for origin in [
            "http://example.com:43210",
            "https://localhost:43210",
            "http://localhost",
            "http://secret@localhost:43210",
            "http://localhost:43210/path",
            "http://localhost:43210/?token=secret",
            "http://localhost:43210/#secret",
            "file:///tmp/endpoint",
        ] {
            assert!(validate_endpoint(origin).is_err(), "{origin}");
        }
    }

    #[test]
    fn reload_adopts_a_new_bearer_without_changing_the_selected_origin_or_target() {
        let file = tempfile::NamedTempFile::new().expect("endpoint fixture");
        let first = Bootstrap {
            url: "http://127.0.0.1:43210".into(),
            token: "old".into(),
            runner_id: String::new(),
            working_directory: "/fixture/project".into(),
            session_id: "session-one".into(),
        };
        fs::write(
            file.path(),
            r#"{"url":"http://127.0.0.1:43210","token":"new"}"#,
        )
        .expect("write");
        let next = first.refreshed(file.path()).expect("reload");
        assert_eq!(next.token, "new");
        assert_eq!(next.session_id, first.session_id);
        assert_eq!(next.working_directory, first.working_directory);
        for invalid in [
            r#"{"url":"http://127.0.0.1:43211","token":"other"}"#,
            r#"{"url":"http://127.0.0.1:43210","token":""}"#,
            "partial",
        ] {
            fs::write(file.path(), invalid).expect("write");
            assert!(first.refreshed(file.path()).is_err());
        }
        assert_eq!(first.token, "old");
    }

    #[test]
    fn explicit_reconnect_accepts_only_a_valid_loopback_locator_and_keeps_target() {
        let file = tempfile::NamedTempFile::new().expect("fixture");
        let old = Bootstrap {
            url: "http://127.0.0.1:12345".into(),
            token: "old".into(),
            runner_id: String::new(),
            working_directory: "/fixture".into(),
            session_id: "saved".into(),
        };
        fs::write(
            file.path(),
            r#"{"url":"http://localhost:23456","token":"new"}"#,
        )
        .expect("write");
        let next = old.reconnected(file.path()).expect("reconnect");
        assert_eq!(next.url, "http://localhost:23456");
        assert_eq!(next.session_id, "saved");
        assert_eq!(next.working_directory, "/fixture");
        fs::write(
            file.path(),
            r#"{"url":"http://example.com:23456","token":"new"}"#,
        )
        .expect("write");
        assert!(old.reconnected(file.path()).is_err());
    }
}
