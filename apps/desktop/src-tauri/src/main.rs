#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use serde::{Deserialize, Serialize};
use std::{error::Error, fs, path::Path};
mod appearance;
mod attention;
mod capture;
mod feedback;
mod host;
mod layout;
mod links;
mod placement;
mod release;
mod runners;
mod sounds;
mod terminal;
mod title_bar;
mod updates;
mod viewer;
mod viewstate;
mod voice;

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

    // The Runner this window was opened on, as its private locator names it
    // now. Every restart gives it a new bearer: Codefly keeps the port on a
    // hot reload, a launcher restart gets a new one. Both are the same Runner
    // (its persistent identity), so its new bearer and origin are followed
    // whether or not the restart was requested from this app; the host
    // installs the new origin's HTTP scope first. A locator naming another
    // Runner is never followed, and a Runner that reports no identity is
    // followed to a new origin only by an explicit reconnect.
    fn followed(&self, path: &Path) -> Result<Self, Box<dyn Error>> {
        let next = Self::read(path)?;
        if !self.runner_id.is_empty() && self.runner_id != next.runner_id {
            return Err("Runner identity changed; open this connection in a new app launch".into());
        }
        if next.url != self.url && self.runner_id.is_empty() {
            return Err("Runner address changed; use Reconnect this window to follow it".into());
        }
        Ok(Self {
            url: next.url,
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

    // After a requested restart, the successor is the Runner the same locator
    // names with a bearer other than the stopping one's (`previous`). Until it
    // writes its locator there is nothing to adopt; once it has, it may serve
    // on a newly allocated origin, accepted exactly as an explicit reconnect.
    fn restarted(&self, path: &Path, previous: &str) -> Result<Option<Self>, Box<dyn Error>> {
        let next = self.reconnected(path)?;
        Ok((next.token != previous).then_some(next))
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
    let context = tauri::generate_context!();
    // Only a release build carries an updater configuration (updates.rs).
    let updater = updates::configured(context.config());
    let mut builder = tauri::Builder::default();
    if updater {
        builder = builder.plugin(tauri_plugin_updater::Builder::new().build());
    }
    let application = builder
        .plugin(tauri_plugin_single_instance::init(|app, args, _cwd| {
            host::second_instance(app, args)
        }))
        .plugin(tauri_plugin_http::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .manage(attention::Attention::default())
        .manage(runners::Runners::default())
        .manage(links::Links::default())
        .manage(host::HtmlPreviews::default())
        .manage(viewer::Viewers::default())
        .manage(updates::Pending::default())
        .invoke_handler(tauri::generate_handler![
            host::runner_bootstrap,
            runners::runner_list,
            runners::runner_add,
            runners::runner_forget,
            links::remote_hosts,
            links::remote_add,
            links::remote_remove,
            host::save_view_state,
            host::open_sign_in,
            host::open_link,
            host::open_local_media,
            host::open_web_window,
            host::dock_web,
            viewer::web_view_show,
            viewer::web_view_hide,
            viewer::web_view_navigate,
            viewer::web_view_zoom,
            viewer::web_view_action,
            viewer::web_view_reset,
            host::copy_sign_in_code,
            host::choose_repository_folder,
            host::save_artifact_export,
            host::open_html_preview_in_browser,
            host::save_surface,
            host::open_console,
            host::open_chat,
            host::open_workspace,
            host::open_artifact,
            host::dock_chat,
            host::return_chat,
            host::chat_window_open,
            host::reopen_agent,
            host::set_action_menu,
            host::reopen_closed,
            host::reconnect_view,
            host::expect_runner_restart,
            attention::notify_attention,
            attention::take_attention_destination,
            attention::set_attention_badge,
            terminal::terminal_apps,
            terminal::open_terminal,
            sounds::play_system_sound,
            title_bar::title_bar,
            title_bar::title_bar_drag,
            title_bar::title_bar_double_click,
            capture::capture_window,
            capture::focus_window,
            appearance::ui_zoom,
            appearance::default_ui_zoom,
            appearance::set_ui_zoom,
            appearance::share_preference,
            release::app_info,
            release::setup_state,
            release::setup_install,
            release::setup_start,
            release::setup_finish,
            release::run_doctor,
            release::link_cli,
            release::remove_older_lazybox,
            release::show_runner_log,
            updates::check_for_update,
            updates::install_update,
            feedback::feedback_diagnostics,
            feedback::save_feedback,
            voice::voice_status,
            voice::voice_request_permission,
            voice::voice_languages,
            voice::voice_open_settings,
            voice::voice_start,
            voice::voice_stop,
            voice::voice_cancel,
            voice::speech_voices,
            voice::speech_speak,
            voice::speech_stop,
            voice::speech_speaking
        ])
        .setup(|app| {
            // Measured here, on the main thread, before any window is built.
            title_bar::buttons();
            attention::setup(app.handle())?;
            host::setup(app)?;
            #[cfg(debug_assertions)]
            if let Some(receipt) = std::env::var_os("LAZYBOX_DEV_DESKTOP_READY") {
                std::fs::write(receipt, b"ready")?;
            }
            Ok(())
        })
        .on_window_event(host::window_event)
        .on_menu_event(|app, event| host::menu_event(app, event.id().as_ref()))
        .build(context)?;
    application.run(|app, event| match event {
        tauri::RunEvent::ExitRequested {
            code: None, api, ..
        } => api.prevent_exit(),
        tauri::RunEvent::ExitRequested { code: Some(_), .. } => {
            host::finish(app);
        }
        tauri::RunEvent::Exit => {
            host::finish(app);
            links::stop_all(app);
        }
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
        let next = first.followed(file.path()).expect("reload");
        assert_eq!(next.token, "new");
        assert_eq!(next.session_id, first.session_id);
        assert_eq!(next.working_directory, first.working_directory);
        for invalid in [
            r#"{"url":"http://127.0.0.1:43211","token":"other"}"#,
            r#"{"url":"http://127.0.0.1:43210","token":""}"#,
            "partial",
        ] {
            fs::write(file.path(), invalid).expect("write");
            assert!(first.followed(file.path()).is_err());
        }
        assert_eq!(first.token, "old");
    }

    #[test]
    fn an_identified_runner_is_followed_to_a_new_origin_without_a_requested_restart() {
        let file = tempfile::NamedTempFile::new().expect("fixture");
        let old = Bootstrap {
            url: "http://127.0.0.1:12345".into(),
            token: "before".into(),
            runner_id: "runneridentity0001".into(),
            working_directory: "/fixture".into(),
            session_id: "saved".into(),
        };
        // A hot reload: same port, new bearer.
        fs::write(
            file.path(),
            r#"{"url":"http://127.0.0.1:12345","token":"reloaded","runnerId":"runneridentity0001"}"#,
        )
        .expect("write");
        let next = old.followed(file.path()).expect("hot reload");
        assert_eq!(next.url, "http://127.0.0.1:12345");
        assert_eq!(next.token, "reloaded");
        // A launcher restart nobody armed: new port, new bearer, same Runner.
        fs::write(
            file.path(),
            r#"{"url":"http://127.0.0.1:23456","token":"restarted","runnerId":"runneridentity0001"}"#,
        )
        .expect("write");
        let next = old.followed(file.path()).expect("restart on a new port");
        assert_eq!(next.url, "http://127.0.0.1:23456");
        assert_eq!(next.token, "restarted");
        assert_eq!(next.session_id, "saved");
        assert_eq!(next.working_directory, "/fixture");
        // Another Runner, a non-loopback origin or an empty bearer is never followed.
        for invalid in [
            r#"{"url":"http://127.0.0.1:23456","token":"other","runnerId":"someotherrunner0002"}"#,
            r#"{"url":"http://example.com:23456","token":"other","runnerId":"runneridentity0001"}"#,
            r#"{"url":"http://127.0.0.1:23456","token":"","runnerId":"runneridentity0001"}"#,
        ] {
            fs::write(file.path(), invalid).expect("write");
            assert!(old.followed(file.path()).is_err(), "{invalid}");
        }
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

    #[test]
    fn requested_restart_follows_only_the_successor_the_same_locator_names() {
        let file = tempfile::NamedTempFile::new().expect("fixture");
        let old = Bootstrap {
            url: "http://127.0.0.1:12345".into(),
            token: "stopping".into(),
            runner_id: "runneridentity0001".into(),
            working_directory: "/fixture".into(),
            session_id: "saved".into(),
        };
        // The stopping Runner's locator: nothing to adopt yet.
        fs::write(
            file.path(),
            r#"{"url":"http://127.0.0.1:12345","token":"stopping","runnerId":"runneridentity0001"}"#,
        )
        .expect("write");
        assert!(
            old.restarted(file.path(), "stopping")
                .expect("read")
                .is_none()
        );
        // Its successor, on a newly allocated port, is adopted with the saved target.
        fs::write(
            file.path(),
            r#"{"url":"http://127.0.0.1:23456","token":"successor","runnerId":"runneridentity0001"}"#,
        )
        .expect("write");
        let next = old
            .restarted(file.path(), "stopping")
            .expect("read")
            .expect("successor");
        assert_eq!(next.url, "http://127.0.0.1:23456");
        assert_eq!(next.token, "successor");
        assert_eq!(next.session_id, "saved");
        // Another Runner's identity or a non-loopback origin is never followed.
        for invalid in [
            r#"{"url":"http://127.0.0.1:23456","token":"other","runnerId":"someotherrunner0002"}"#,
            r#"{"url":"http://example.com:23456","token":"other","runnerId":"runneridentity0001"}"#,
        ] {
            fs::write(file.path(), invalid).expect("write");
            assert!(old.restarted(file.path(), "stopping").is_err());
        }
    }
}
