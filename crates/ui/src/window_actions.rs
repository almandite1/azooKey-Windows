//! The interpreter for [`WindowAction`] requests coming over IPC from the
//! TIP. Position/size *decisions* are the pure functions in `geometry` and
//! `placement` (tested); the win32 calls are the named helpers in `window` —
//! this module only sequences them.

use std::sync::Arc;

use tao::dpi::{LogicalSize, PhysicalPosition};
use tao::event_loop::EventLoopProxy;
use tao::platform::windows::WindowExtWindows;
use tao::window::Window;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::UserEvent;
use crate::geometry::{self, CaretRect};
use crate::ipc::WindowAction;
use crate::placement::{CandidatePlacement, ShowDecision};
use crate::webview;
use crate::window::{is_visible, notify_ime_event, pin_topmost, set_visibility};
use windows::Win32::UI::WindowsAndMessaging::{
    EVENT_OBJECT_IME_CHANGE, EVENT_OBJECT_IME_HIDE, EVENT_OBJECT_IME_SHOW,
};

/// Re-clamps the candidate window against the last caret rect for the size
/// it is about to have (physical px). Call after every resize — clamping
/// only at position time let a subsequently grown window overflow the work
/// area (issue #3).
pub fn reposition_candidate(
    candidate_window: &Window,
    last_caret: &Option<CaretRect>,
    win_width: i32,
    win_height: i32,
) {
    // no caret yet (nothing was composed since startup): nothing to clamp to
    let Some(caret) = last_caret else {
        return;
    };
    let (x, y) = geometry::get_candidate_window_position(caret, win_width, win_height);
    candidate_window.set_outer_position(PhysicalPosition::new(x, y));
}

/// Resizes the candidate window and re-clamps it for the size it is GOING to
/// have. `width` and `height` are LOGICAL (CSS) px; `None` keeps the current
/// value.
///
/// One function for what used to be two mirror-image copies — width set with
/// the height kept (a longer candidate), height set with the width kept (a
/// longer list) — each carrying its own copy of the B20 and issue-#3
/// invariants:
///
/// - the size must be applied as a `LogicalSize`, because the webview lays
///   out in CSS px: feeding the PHYSICAL inner size back in grew the window
///   by the scale factor on every resize at high DPI (B20);
/// - the re-clamp must use the size the window is about to have, because
///   `inner_size()` can still report the pre-resize value right here
///   (issue #3).
pub fn resize_candidate(
    candidate_window: &Window,
    width: Option<f64>,
    height: Option<f64>,
    last_caret: &Option<CaretRect>,
) {
    let scale = candidate_window.scale_factor();
    let current = candidate_window.inner_size().to_logical::<f64>(scale);
    let new_size = LogicalSize::new(
        width.unwrap_or(current.width),
        height.unwrap_or(current.height),
    );
    candidate_window.set_inner_size(new_size);

    let physical = new_size.to_physical::<i32>(scale);
    reposition_candidate(
        candidate_window,
        last_caret,
        physical.width,
        physical.height,
    );
}

/// How long a `Show` waits for the position that belongs to it before giving
/// up and showing anyway. Long enough for the `OnLayoutChange` that follows a
/// `TS_E_NOLAYOUT`, short enough not to read as lag.
const POSITION_GRACE: std::time::Duration = std::time::Duration::from_millis(120);

/// How long the mode indicator stays up after an あ/A switch. Long enough to
/// read, short enough not to sit over the text the user went on to type.
const INDICATOR_FLASH: std::time::Duration = std::time::Duration::from_millis(500);

/// Makes the candidate window visible, announcing the transition only when it
/// really is one: the TIP sends Show/Hide freely, and announcing every
/// request buried listeners in redundant events.
pub fn show_candidate(candidate_window: &Window) {
    if !set_visibility(candidate_window.hwnd(), true) {
        notify_ime_event(candidate_window.hwnd(), EVENT_OBJECT_IME_SHOW);
    }
}

/// The counterpart of [`show_candidate`], with the same "only a real
/// transition is announced" rule.
pub fn hide_candidate(candidate_window: &Window) {
    if set_visibility(candidate_window.hwnd(), false) {
        notify_ime_event(candidate_window.hwnd(), EVENT_OBJECT_IME_HIDE);
    }
}

/// Announces a content/position change, but only while the window is visible.
///
/// The guard is the point: ending a composition sends `hide_window()` and
/// then an update with an empty list, so without it a CHANGE was announced on
/// a window that had just been hidden.
fn notify_change_if_visible(candidate_window: &Window) {
    if is_visible(candidate_window.hwnd()) {
        notify_ime_event(candidate_window.hwnd(), EVENT_OBJECT_IME_CHANGE);
    }
}

pub fn handle_window_action(
    action: WindowAction,
    candidate_window: &Window,
    indicator_window: &Window,
    indicator_flash: &Arc<Mutex<Option<JoinHandle<()>>>>,
    proxy: &EventLoopProxy<UserEvent>,
    placement: &mut CandidatePlacement,
) {
    let indicator_hwnd = indicator_window.hwnd();

    match action {
        WindowAction::Show => {
            // if the mode indicator is mid-flash, cancel it and hide it
            let mut flash = match indicator_flash.try_lock() {
                Ok(guard) => guard,
                Err(_) => {
                    eprintln!("Warning: Failed to lock indicator_flash, skipping cleanup");
                    return;
                }
            };
            if let Some(task) = flash.take() {
                task.abort();
                set_visibility(indicator_hwnd, false);
            }

            match placement.on_show() {
                ShowDecision::ShowNow => show_candidate(candidate_window),
                // No position for THIS composition yet: showing now would
                // put the window at the last one's caret (issue #59). Arm
                // the grace period so a host that never reports a layout
                // still gets its candidates.
                ShowDecision::Defer => {
                    let proxy = proxy.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(POSITION_GRACE).await;
                        let _ = proxy.send_event(UserEvent::ShowDeadline);
                    });
                }
            }
        }
        WindowAction::Hide => {
            placement.on_hide();
            hide_candidate(candidate_window);
        }
        // One update, applied in the order the placement invariants need:
        // position, then the list, then the highlight.
        //
        // Position first because it is what releases a deferred `Show`
        // (issue #59) and what later resizes re-clamp against (issue #3):
        // sizing the window for a new list before the caret it belongs to had
        // arrived would clamp it against the previous composition's rect. The
        // highlight comes last because it scrolls the list it is moving
        // through, which has to exist first.
        WindowAction::Update {
            position,
            candidates,
            selection,
        } => {
            if let Some(caret) = position {
                // remembered so later RESIZES (a wider list, UpdateHeight, DPI
                // changes) can re-clamp against the same caret (issue #3),
                // and marked fresh so a Show may now be honoured (issue #59)
                let release_deferred_show = placement.on_position(caret);

                pin_topmost(candidate_window.hwnd());
                pin_topmost(indicator_hwnd);

                let size = candidate_window.inner_size();
                reposition_candidate(
                    candidate_window,
                    &placement.caret,
                    size.width as i32,
                    size.height as i32,
                );
                // clamp the indicator into the work area too — it used to hang
                // off-screen near screen edges (B20)
                let indicator_size = indicator_window.inner_size();
                let (ix, iy) = geometry::get_indicator_position(
                    &caret,
                    indicator_size.width as i32,
                    indicator_size.height as i32,
                );
                indicator_window.set_outer_position(PhysicalPosition::new(ix, iy));

                // the window is placed now, so a Show that was waiting on this
                // position can finally be honoured (issue #59)
                if release_deferred_show {
                    show_candidate(candidate_window);
                }
            }

            if let Some(candidates) = &candidates {
                let max_len = geometry::max_candidate_chars(candidates);
                resize_candidate(
                    candidate_window,
                    Some(geometry::candidate_window_logical_width(max_len) as f64),
                    None,
                    &placement.caret,
                );
            }

            // Both halves in ONE script evaluation, so the webview lays out
            // once for a keystroke instead of once per RPC. The window has
            // already been resized here; the contents land asynchronously once
            // the webview runs the script.
            if candidates.is_some() || selection.is_some() {
                let payload = webview::candidate_update_json(candidates.as_deref(), selection);
                let _ = proxy.send_event(UserEvent::ApplyCandidateUpdate(payload));
            }

            // A moved highlight is deliberately NOT announced, exactly as it
            // was not when it had an RPC of its own: the arrow keys walk a
            // list the shell has already been told about, and announcing every
            // step buried the transitions that matter. A new list or a new
            // position still is one.
            if position.is_some() || candidates.is_some() {
                notify_change_if_visible(candidate_window);
            }
        }
        WindowAction::SetInputMode(input_method) => {
            let _ = proxy.send_event(UserEvent::UpdateInputMethod(input_method));

            if let Ok(mut flash) = indicator_flash.try_lock() {
                if let Some(task) = flash.take() {
                    task.abort();
                }

                *flash = Some(tokio::spawn(async move {
                    set_visibility(indicator_hwnd, true);
                    tokio::time::sleep(INDICATOR_FLASH).await;
                    set_visibility(indicator_hwnd, false);
                }));
            }
        }
    }
}
