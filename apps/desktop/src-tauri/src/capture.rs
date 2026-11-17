//! `lazybox shot` and `lazybox open` (POWER.md): the main window takes a PNG
//! of itself, or of one of its panels, and comes to the front.
//!
//! The snapshot is the webview's own (WKWebView takeSnapshot): it needs no
//! screen-recording permission, shows only Lazybox, and is pixel-exact,
//! Monaco and the terminal included.
use serde::Deserialize;
use tauri::{Manager, WebviewWindow};

/// A region of the page in CSS pixels, with the page's own width so the host
/// can scale it to the view (the page may be zoomed).
#[derive(Debug, Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Region {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub viewport_width: f64,
}

/// The region in view points, clamped to the view, or None for the whole view.
pub fn to_view(
    region: Option<Region>,
    view_width: f64,
    view_height: f64,
) -> Result<Option<(f64, f64, f64, f64)>, String> {
    let Some(r) = region else { return Ok(None) };
    let finite = [r.x, r.y, r.width, r.height, r.viewport_width]
        .iter()
        .all(|v| v.is_finite());
    if !finite
        || r.width <= 0.0
        || r.height <= 0.0
        || r.viewport_width <= 0.0
        || view_width <= 0.0
        || view_height <= 0.0
    {
        return Err("invalid capture region".into());
    }
    let scale = view_width / r.viewport_width;
    let x = (r.x * scale).clamp(0.0, view_width);
    let y = (r.y * scale).clamp(0.0, view_height);
    let width = (r.width * scale).min(view_width - x);
    let height = (r.height * scale).min(view_height - y);
    if width < 1.0 || height < 1.0 {
        return Err("the panel is not visible".into());
    }
    Ok(Some((x, y, width, height)))
}

/// Brings the window to the front: `lazybox open` shows what it opened.
#[tauri::command]
pub fn focus_window(app: tauri::AppHandle, window: WebviewWindow) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    app.show().map_err(|e| e.to_string())?;
    #[cfg(not(target_os = "macos"))]
    let _ = &app;
    if window.is_minimized().unwrap_or(false) {
        window.unminimize().map_err(|e| e.to_string())?;
    }
    window.show().map_err(|e| e.to_string())?;
    window.set_focus().map_err(|e| e.to_string())
}

/// A PNG of the calling window, or of a region of it.
#[tauri::command]
pub async fn capture_window(
    window: WebviewWindow,
    region: Option<Region>,
) -> Result<tauri::ipc::Response, String> {
    let (send, receive) = std::sync::mpsc::channel::<Result<Vec<u8>, String>>();
    let label = window.label().to_string();
    let target = window
        .app_handle()
        .get_webview_window(&label)
        .ok_or("the window closed")?;
    target
        .with_webview(move |webview| snapshot(webview, region, send))
        .map_err(|e| e.to_string())?;
    let png = tauri::async_runtime::spawn_blocking(move || {
        receive
            .recv_timeout(std::time::Duration::from_secs(20))
            .map_err(|_| "the window did not produce a screenshot in time".to_string())?
    })
    .await
    .map_err(|e| e.to_string())??;
    Ok(tauri::ipc::Response::new(png))
}

#[cfg(target_os = "macos")]
fn snapshot(
    webview: tauri::webview::PlatformWebview,
    region: Option<Region>,
    send: std::sync::mpsc::Sender<Result<Vec<u8>, String>>,
) {
    use block2::RcBlock;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSImage};
    use objc2_core_foundation::{CGPoint, CGRect, CGSize};
    use objc2_foundation::{NSDictionary, NSError};
    use objc2_web_kit::{WKSnapshotConfiguration, WKWebView};
    use std::sync::Mutex;

    let fail = |send: &std::sync::mpsc::Sender<Result<Vec<u8>, String>>, message: &str| {
        let _ = send.send(Err(message.to_string()));
    };
    let Some(mtm) = MainThreadMarker::new() else {
        return fail(&send, "screenshots are taken on the main thread");
    };
    // SAFETY: on macOS the platform webview is the window's WKWebView, alive
    // for this call (with_webview runs on the main thread).
    let view: &WKWebView = unsafe { &*(webview.inner() as *const WKWebView) };
    let bounds = view.bounds();
    let rect = match to_view(region, bounds.size.width, bounds.size.height) {
        Ok(rect) => rect,
        Err(message) => return fail(&send, &message),
    };
    // SAFETY: a fresh configuration, used on the main thread.
    let configuration = unsafe { WKSnapshotConfiguration::new(mtm) };
    if let Some((x, y, width, height)) = rect {
        unsafe {
            configuration.setRect(CGRect::new(CGPoint::new(x, y), CGSize::new(width, height)))
        };
    }
    unsafe { configuration.setAfterScreenUpdates(true) };
    let send = Mutex::new(Some(send));
    let done = RcBlock::new(move |image: *mut NSImage, _error: *mut NSError| {
        let Some(send) = send.lock().ok().and_then(|mut s| s.take()) else {
            return;
        };
        // SAFETY: WebKit hands a valid image or null.
        let result = unsafe { image.as_ref() }
            .and_then(|image| image.TIFFRepresentation())
            .and_then(|tiff| NSBitmapImageRep::imageRepWithData(&tiff))
            .and_then(|rep| unsafe {
                rep.representationUsingType_properties(
                    NSBitmapImageFileType::PNG,
                    &NSDictionary::new(),
                )
            })
            .map(|png| png.to_vec())
            .ok_or_else(|| "WebKit could not take the screenshot".to_string());
        let _ = send.send(result);
    });
    unsafe { view.takeSnapshotWithConfiguration_completionHandler(Some(&configuration), &done) };
}

#[cfg(not(target_os = "macos"))]
fn snapshot(
    _webview: tauri::webview::PlatformWebview,
    _region: Option<Region>,
    send: std::sync::mpsc::Sender<Result<Vec<u8>, String>>,
) {
    let _ = send.send(Err("screenshots are available on macOS".into()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_scale_to_the_zoomed_view_and_stay_inside_it() {
        assert_eq!(to_view(None, 800.0, 600.0), Ok(None));
        // The page is 1000 CSS px wide in an 800 pt view (zoomed out).
        let region = Region {
            x: 100.0,
            y: 50.0,
            width: 500.0,
            height: 200.0,
            viewport_width: 1000.0,
        };
        assert_eq!(
            to_view(Some(region), 800.0, 600.0),
            Ok(Some((80.0, 40.0, 400.0, 160.0)))
        );
        // A panel partly outside the view is clipped to it.
        let wide = Region {
            x: 700.0,
            y: 0.0,
            width: 500.0,
            height: 100.0,
            viewport_width: 800.0,
        };
        assert_eq!(
            to_view(Some(wide), 800.0, 600.0),
            Ok(Some((700.0, 0.0, 100.0, 100.0)))
        );
        for bad in [
            Region {
                x: 0.0,
                y: 0.0,
                width: 0.0,
                height: 10.0,
                viewport_width: 800.0,
            },
            Region {
                x: f64::NAN,
                y: 0.0,
                width: 10.0,
                height: 10.0,
                viewport_width: 800.0,
            },
            Region {
                x: 900.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
                viewport_width: 800.0,
            },
            Region {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
                viewport_width: -1.0,
            },
        ] {
            assert!(to_view(Some(bad), 800.0, 600.0).is_err(), "{bad:?}");
        }
    }
}
