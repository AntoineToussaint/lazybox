//! macOS notifications through UNUserNotificationCenter.
//!
//! tauri-plugin-notification posts through NSUserNotification, deprecated
//! since macOS 11, and throws its result away: macOS dropping every
//! notification looked exactly like success. This asks macOS for permission,
//! reports what it decided (Settings → Notifications shows it), returns the
//! error a refused notification gets, and receives the click through the
//! center's delegate. It needs a bundled app: the center refuses a process
//! without a bundle identity (`tauri dev`), which keeps the plugin.
use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{AllocAnyThread, DefinedClass, define_class, msg_send};
use objc2_foundation::{NSBundle, NSDictionary, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNMutableNotificationContent, UNNotification,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
    UNNotificationSettings, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};
use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::mpsc::Sender;

/// The keys a notification carries to its click (attention.rs `destination`).
pub const KEYS: [&str; 3] = ["collectionId", "sessionId", "runner"];

/// Whether this process is an app bundle the notification center serves.
pub fn available() -> bool {
    let bundle = NSBundle::mainBundle();
    bundle.bundleIdentifier().is_some() && bundle.bundlePath().to_string().ends_with(".app")
}

/// What macOS lets this app do, in the web's words: granted, denied, or
/// default (never asked).
pub fn permission_word(status: UNAuthorizationStatus) -> &'static str {
    match status {
        UNAuthorizationStatus::Authorized
        | UNAuthorizationStatus::Provisional
        | UNAuthorizationStatus::Ephemeral => "granted",
        UNAuthorizationStatus::Denied => "denied",
        _ => "default",
    }
}

pub fn permission(send: Sender<Result<String, String>>) {
    let done = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
        let status = unsafe { settings.as_ref() }.authorizationStatus();
        let _ = send.send(Ok(permission_word(status).to_string()));
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .getNotificationSettingsWithCompletionHandler(&done);
}

/// Asks macOS once (it remembers the answer and never asks again; the person
/// changes it in System Settings → Notifications).
pub fn request(send: Sender<Result<String, String>>) {
    let done = RcBlock::new(move |granted: Bool, error: *mut NSError| {
        let answer = match unsafe { error.as_ref() } {
            Some(error) => Err(error.localizedDescription().to_string()),
            None => Ok(if granted.as_bool() {
                "granted"
            } else {
                "denied"
            }
            .to_string()),
        };
        let _ = send.send(answer);
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Badge,
            &done,
        );
}

/// Posts a notification; the answer is macOS's: delivered, or why not (not
/// allowed for this app, for one).
pub fn show(
    id: i32,
    title: &str,
    body: &str,
    extra: &[(&str, String)],
    send: Sender<Result<(), String>>,
) {
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str(body));
    let keys: Vec<Retained<NSString>> = extra.iter().map(|(k, _)| NSString::from_str(k)).collect();
    let values: Vec<Retained<NSString>> =
        extra.iter().map(|(_, v)| NSString::from_str(v)).collect();
    let keys: Vec<&NSString> = keys.iter().map(|k| &**k).collect();
    let values: Vec<&NSString> = values.iter().map(|v| &**v).collect();
    let info = NSDictionary::from_slices(&keys, &values);
    // userInfo is untyped; its keys and values are strings.
    unsafe { content.setUserInfo(&*(Retained::as_ptr(&info) as *const NSDictionary)) };
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
        &NSString::from_str(&format!("lazybox-attention-{id}")),
        &content,
        None,
    );
    let done = RcBlock::new(move |error: *mut NSError| {
        let answer = match unsafe { error.as_ref() } {
            Some(error) => Err(error.localizedDescription().to_string()),
            None => Ok(()),
        };
        let _ = send.send(answer);
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .addNotificationRequest_withCompletionHandler(&request, Some(&done));
}

type OnClick = Box<dyn Fn(HashMap<String, String>) + 'static>;

pub struct Ivars {
    on_click: OnClick,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; Delegate has no Drop.
    #[unsafe(super(NSObject))]
    #[name = "LazyboxNotificationDelegate"]
    #[ivars = Ivars]
    pub struct Delegate;

    unsafe impl NSObjectProtocol for Delegate {}

    unsafe impl UNUserNotificationCenterDelegate for Delegate {
        /// Lazybox only notifies about what the person is not looking at
        /// (Inbox.tsx), so a notification shows even while it is in front.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            done: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            done.call((
                UNNotificationPresentationOptions::Banner | UNNotificationPresentationOptions::List,
            ));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            done: &DynBlock<dyn Fn()>,
        ) {
            let info = response.notification().request().content().userInfo();
            let mut extra = HashMap::new();
            for key in KEYS {
                let value: Option<Retained<AnyObject>> =
                    unsafe { msg_send![&*info, objectForKey: &*NSString::from_str(key)] };
                if let Some(text) = value.and_then(|v| v.downcast::<NSString>().ok()) {
                    extra.insert(key.to_string(), text.to_string());
                }
            }
            (self.ivars().on_click)(extra);
            done.call(());
        }
    }
);

/// Sets the center's delegate, which receives clicks (also the one that
/// launched the app) and lets notifications show while Lazybox is in front.
/// The center holds its delegate weakly: this one lives as long as the app.
pub fn listen(on_click: impl Fn(HashMap<String, String>) + 'static) {
    let delegate = Delegate::alloc().set_ivars(Ivars {
        on_click: Box::new(on_click),
    });
    let delegate: Retained<Delegate> = unsafe { msg_send![super(delegate), init] };
    UNUserNotificationCenter::currentNotificationCenter()
        .setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    std::mem::forget(delegate);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_reads_as_the_web_says_it() {
        assert_eq!(
            permission_word(UNAuthorizationStatus::Authorized),
            "granted"
        );
        assert_eq!(
            permission_word(UNAuthorizationStatus::Provisional),
            "granted"
        );
        assert_eq!(permission_word(UNAuthorizationStatus::Denied), "denied");
        assert_eq!(
            permission_word(UNAuthorizationStatus::NotDetermined),
            "default"
        );
    }

    #[test]
    fn a_test_binary_is_no_app_bundle() {
        // `cargo test` runs a bare executable: the center would refuse it.
        assert!(!available());
    }
}
