//! Attention notifications: a conversation that needs the person, shown as a
//! native notification and a dock badge. The webview never holds a
//! notification permission; it asks the host through these two commands, and
//! a click on a notification brings the main window to that conversation.
use crate::layout::valid_id;
use serde::{Deserialize, Serialize};
use std::sync::{
    Mutex,
    atomic::{AtomicI32, Ordering},
};
use tauri::{Emitter, Manager, Window};
use tauri_plugin_notification::NotificationExt;

/// Where a clicked notification leads: one conversation in one workspace, or
/// nowhere in particular (a test notification only brings the window forward).
/// `runner` names which of the window's Runners it is on when the window shows
/// several (runners.rs); empty is the window's own.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
pub struct Destination {
    pub collection_id: String,
    pub session_id: String,
    pub runner: String,
}

impl Destination {
    fn valid(&self) -> bool {
        let collection = self.collection_id.is_empty() || valid_id(&self.collection_id);
        let runner = self.runner.is_empty() || valid_id(&self.runner);
        collection
            && runner
            && (valid_id(&self.session_id)
                || (self.session_id.is_empty() && self.collection_id.is_empty()))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Notice {
    title: String,
    body: String,
    destination: Destination,
}

/// The conversation of the last clicked notification, until the main window
/// takes it. A click can reopen a closed main window, whose page is not
/// listening yet when the click arrives.
#[derive(Default)]
pub struct Attention {
    pending: Mutex<Option<Destination>>,
    sequence: AtomicI32,
}

const TITLE_LIMIT: usize = 120;
const BODY_LIMIT: usize = 280;

fn bounded(text: &str, limit: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(limit - 1).collect();
    cut.push('…');
    cut
}

impl Notice {
    fn validate(self) -> Result<Self, String> {
        let destination = self.destination;
        if !destination.valid() {
            return Err("A notification must lead to a valid conversation".into());
        }
        if self.title.trim().is_empty() {
            return Err("A notification needs a title".into());
        }
        Ok(Self {
            title: bounded(&self.title, TITLE_LIMIT),
            body: bounded(&self.body, BODY_LIMIT),
            destination,
        })
    }
}

/// The conversation a clicked notification leads to; none for a notification
/// that leads nowhere in particular, or one whose payload is not ours.
fn destination(
    extra: &std::collections::HashMap<String, serde_json::Value>,
) -> Option<Destination> {
    let text = |key: &str| {
        extra
            .get(key)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let destination = Destination {
        collection_id: text("collectionId"),
        session_id: text("sessionId"),
        runner: text("runner"),
    };
    (!destination.session_id.is_empty() && destination.valid()).then_some(destination)
}

/// A click on a notification opens (or focuses) the main window and tells it
/// which conversation to show.
pub fn setup(app: &tauri::AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    let handle = app.clone();
    app.notification().on_action(move |performed| {
        if performed.action_id() != "tap" {
            return;
        }
        let target = performed
            .notification()
            .and_then(|n| destination(n.extra()));
        if let Some(target) = target
            && let Ok(mut pending) = handle.state::<Attention>().pending.lock()
        {
            *pending = Some(target);
        }
        let app = handle.clone();
        let _ = handle.run_on_main_thread(move || {
            if let Err(error) = crate::host::open(&app, "main") {
                crate::host::report(&app, error);
            }
            let _ = app.emit_to("main", "attention-open", ());
        });
    })?;
    Ok(())
}

#[tauri::command]
pub fn notify_attention(
    app: tauri::AppHandle,
    state: tauri::State<'_, Attention>,
    notice: Notice,
) -> Result<(), String> {
    let notice = notice.validate()?;
    let id = state
        .sequence
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    app.notification()
        .builder()
        .id(id)
        .title(notice.title)
        .body(notice.body)
        .extra("collectionId", notice.destination.collection_id)
        .extra("sessionId", notice.destination.session_id)
        .extra("runner", notice.destination.runner)
        .show()
        .map_err(|e| e.to_string())
}

/// The conversation a clicked notification asked for, once.
#[tauri::command]
pub fn take_attention_destination(
    state: tauri::State<'_, Attention>,
) -> Result<Option<Destination>, String> {
    Ok(state.pending.lock().map_err(|e| e.to_string())?.take())
}

/// The dock (macOS) or taskbar (Linux) badge; zero removes it.
#[tauri::command]
pub fn set_attention_badge(window: Window, count: u32) -> Result<(), String> {
    let count = (count > 0).then_some(i64::from(count.min(999)));
    window.set_badge_count(count).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(session: &str, collection: &str, title: &str, body: &str) -> Notice {
        Notice {
            title: title.into(),
            body: body.into(),
            destination: Destination {
                collection_id: collection.into(),
                session_id: session.into(),
                runner: String::new(),
            },
        }
    }

    #[test]
    fn a_notice_leads_to_a_valid_conversation_and_is_bounded() {
        let checked = notice("alpha-session", "alpha", " Needs input ", &"x".repeat(400))
            .validate()
            .expect("valid notice");
        assert_eq!(checked.title, "Needs input");
        assert_eq!(checked.body.chars().count(), BODY_LIMIT);
        assert!(checked.body.ends_with('…'));
        assert!(notice("standalone", "", "Failed", "").validate().is_ok());
        assert!(
            notice("", "", "Notifications are on", "")
                .validate()
                .is_ok()
        );
        for (session, collection, title) in [
            ("", "alpha", "Needs input"),
            ("../escape", "alpha", "Needs input"),
            ("alpha-session", "a/b", "Needs input"),
            ("alpha-session", "alpha", "  "),
        ] {
            assert!(notice(session, collection, title, "").validate().is_err());
        }
        let mut elsewhere = notice("alpha-session", "alpha", "Needs input", "");
        elsewhere.destination.runner = "working".into();
        assert!(elsewhere.validate().is_ok());
        let mut forged = notice("alpha-session", "alpha", "Needs input", "");
        forged.destination.runner = "a/b".into();
        assert!(forged.validate().is_err());
    }

    #[test]
    fn a_click_reads_its_destination_from_the_notification() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("sessionId".to_string(), serde_json::json!("alpha-session"));
        extra.insert("collectionId".to_string(), serde_json::json!("alpha"));
        assert_eq!(
            destination(&extra),
            Some(Destination {
                collection_id: "alpha".into(),
                session_id: "alpha-session".into(),
                runner: String::new(),
            })
        );
        // Another Runner of the window: the click goes there.
        extra.insert("runner".to_string(), serde_json::json!("3f2a9c0d1e4b5a6c"));
        assert_eq!(
            destination(&extra).map(|d| d.runner),
            Some("3f2a9c0d1e4b5a6c".into())
        );
        extra.insert("runner".to_string(), serde_json::json!("../other"));
        assert_eq!(destination(&extra), None);
        extra.remove("runner");
        extra.insert("sessionId".to_string(), serde_json::json!("not valid"));
        assert_eq!(destination(&extra), None);
        assert_eq!(destination(&Default::default()), None);
    }
}
