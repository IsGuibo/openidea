//! Double-click zoom: fill the current monitor's work area the way AppKit's `zoom:` cannot.
//!
//! `[NSWindow zoom:]` runs one animated resize, and WebKit hides the page for the duration of an
//! animated resize: the frame grows over an empty window and the content appears only at the end.
//! Stepping the frame ourselves makes every step an ordinary (non-animated) resize, so the webview
//! lays its content out again at each step and the content grows with the frame.
use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use ide_core::{IdeError, IdeResult};
use tauri::{PhysicalPosition, PhysicalSize, WebviewWindow};

/// The duration AppKit itself uses for an animated window resize: `NSWindow.animationResizeTime(_:)`
/// returns 0.35 on a 60 Hz display, so matching it keeps the zoom in line with the system.
const DURATION: Duration = Duration::from_millis(350);
/// Fallback pacing where the webview cannot report presentation updates (Windows, Linux).
const STEP: Duration = Duration::from_millis(16);

/// What a zoomed window restores to on the next toggle.
struct Restored {
    position: PhysicalPosition<i32>,
    size: PhysicalSize<u32>,
}

/// Windows currently zoomed by [`toggle`], keyed by label. A destroyed window drops its entry, so a
/// re-opened window never restores a stale frame.
static ZOOMED: LazyLock<Mutex<HashMap<String, Restored>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Windows whose frame is being stepped right now; a second toggle would fight the first.
static MOVING: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

fn zoomed() -> std::sync::MutexGuard<'static, HashMap<String, Restored>> {
    ZOOMED.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn moving() -> std::sync::MutexGuard<'static, HashSet<String>> {
    MOVING.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn err(e: tauri::Error) -> IdeError {
    IdeError::internal(e.to_string())
}

/// Drops the remembered frame of a window that is going away.
pub fn forget(label: &str) {
    zoomed().remove(label);
    moving().remove(label);
}

/// A frame as (x, y, width, height) in physical pixels.
type Frame = (i32, i32, u32, u32);

fn lerp(from: u32, to: u32, t: f64) -> u32 {
    (from as f64 + (to as f64 - from as f64) * t).round() as u32
}

fn lerp_i(from: i32, to: i32, t: f64) -> i32 {
    (from as f64 + (to as f64 - from as f64) * t).round() as i32
}

/// Resolves once the webview has presented the frame it is drawing now.
///
/// `_doAfterNextPresentationUpdate:` is the only synchronisation primitive WKWebView offers on
/// macOS; without it the window moves on while the page still shows its previous size, which is
/// the tearing a fixed timer produces.
#[cfg(target_os = "macos")]
fn next_presentation(window: &WebviewWindow) -> Option<std::sync::mpsc::Receiver<()>> {
    use block2::RcBlock;
    use objc2::runtime::AnyObject;

    let (tx, rx) = std::sync::mpsc::sync_channel::<()>(1);
    let sent = window
        .with_webview(move |webview| unsafe {
            let view = webview.inner().cast::<AnyObject>();
            let block = RcBlock::new(move || {
                let _ = tx.try_send(());
            });
            let _: () = objc2::msg_send![view, _doAfterNextPresentationUpdate: &*block];
        })
        .is_ok();
    sent.then_some(rx)
}

/// Blocks until the webview presented, at most [`PRESENT_TIMEOUT`], so a missed callback cannot
/// stall the animation.
#[cfg(target_os = "macos")]
fn wait_presentation(window: &WebviewWindow) {
    const PRESENT_TIMEOUT: Duration = Duration::from_millis(60);
    if let Some(rx) = next_presentation(window) {
        let _ = rx.recv_timeout(PRESENT_TIMEOUT);
    } else {
        std::thread::sleep(STEP);
    }
}

/// Moves the window through `STEPS` frames, so the webview re-lays out at each one.
fn step_to(window: WebviewWindow, from: Frame, to: Frame) {
    let label = window.label().to_owned();
    std::thread::spawn(move || {
        let start = std::time::Instant::now();
        loop {
            let t = (start.elapsed().as_secs_f64() / DURATION.as_secs_f64()).min(1.0);
            // Smoothstep, like AppKit's own window animations: gentle start, gentle settle.
            let eased = t * t * (3.0 - 2.0 * t);
            let last = t >= 1.0;
            let frame = if last {
                to
            } else {
                (
                    lerp_i(from.0, to.0, eased),
                    lerp_i(from.1, to.1, eased),
                    lerp(from.2, to.2, eased),
                    lerp(from.3, to.3, eased),
                )
            };
            let _ = window.set_position(PhysicalPosition::new(frame.0, frame.1));
            let _ = window.set_size(PhysicalSize::new(frame.2, frame.3));
            if last {
                break;
            }
            // Wait for the page to present this size before moving the frame again.
            #[cfg(target_os = "macos")]
            wait_presentation(&window);
            #[cfg(not(target_os = "macos"))]
            std::thread::sleep(STEP);
        }
        moving().remove(&label);
    });
}

/// Reads the frame a screenshot of the window would show: outer position and inner size.
fn frame_of(window: &WebviewWindow) -> IdeResult<Frame> {
    let position = window.outer_position().map_err(err)?;
    let size = window.inner_size().map_err(err)?;
    Ok((position.x, position.y, size.width, size.height))
}

/// Fills the work area of the window's monitor, or restores the frame it had before. Returns
/// whether the window is filling its work area.
pub fn toggle(window: &WebviewWindow) -> IdeResult<bool> {
    if window.is_fullscreen().map_err(err)? {
        return Ok(false);
    }
    let label = window.label().to_owned();
    if moving().contains(&label) {
        return Ok(zoomed().contains_key(&label));
    }
    let current = frame_of(window)?;
    if let Some(restored) = zoomed().remove(&label) {
        let back = (restored.position.x, restored.position.y, restored.size.width, restored.size.height);
        moving().insert(label.clone());
        step_to(window.clone(), current, back);
        return Ok(false);
    }
    let Some(monitor) = window.current_monitor().map_err(err)?.or(window.primary_monitor().map_err(err)?) else {
        return Ok(false);
    };
    let area = *monitor.work_area();
    // The work area is a frame and the size is a client area: keep the difference constant.
    let outer = window.outer_size().map_err(err)?;
    let inner = window.inner_size().map_err(err)?;
    let target: Frame = (
        area.position.x,
        area.position.y,
        area.size.width.saturating_sub(outer.width.saturating_sub(inner.width)).max(1),
        area.size.height.saturating_sub(outer.height.saturating_sub(inner.height)).max(1),
    );
    zoomed().insert(
        label.clone(),
        Restored {
            position: PhysicalPosition::new(current.0, current.1),
            size: PhysicalSize::new(current.2, current.3),
        },
    );
    moving().insert(label);
    step_to(window.clone(), current, target);
    Ok(true)
}
