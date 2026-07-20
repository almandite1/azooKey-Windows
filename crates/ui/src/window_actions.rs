//! The interpreter for [`WindowAction`] requests coming over IPC from the
//! TIP. Position/size *decisions* are the pure functions in `utils`
//! (tested); the win32 calls are the named helpers in `window` — this
//! module only sequences them.

use std::sync::Arc;

use tao::dpi::{LogicalSize, PhysicalPosition};
use tao::event_loop::EventLoopProxy;
use tao::platform::windows::WindowExtWindows;
use tao::window::Window;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::ipc::WindowAction;
use crate::utils::{self, CaretRect};
use crate::window::{is_visible, notify_ime_event, pin_topmost, set_visibility};
use crate::UserEvent;
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
    let (x, y) = utils::get_candidate_window_position(caret, win_width, win_height);
    candidate_window.set_outer_position(PhysicalPosition::new(x, y));
}

pub fn handle_window_action(
    action: WindowAction,
    candidate_window: &Window,
    indicator_window: &Window,
    indicator_flash: &Arc<Mutex<Option<JoinHandle<()>>>>,
    proxy: &EventLoopProxy<UserEvent>,
    last_caret: &mut Option<CaretRect>,
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

            // only on a real transition: the TIP sends Show/Hide freely, and
            // announcing every request buried listeners in redundant events
            if !set_visibility(candidate_window.hwnd(), true) {
                notify_ime_event(candidate_window.hwnd(), EVENT_OBJECT_IME_SHOW);
            }
        }
        WindowAction::Hide => {
            if set_visibility(candidate_window.hwnd(), false) {
                notify_ime_event(candidate_window.hwnd(), EVENT_OBJECT_IME_HIDE);
            }
        }
        WindowAction::SetPosition {
            top,
            left,
            bottom,
            right,
        } => {
            let caret = CaretRect {
                top,
                left,
                bottom,
                right,
            };
            // remembered so later RESIZES (SetCandidate width, UpdateHeight,
            // DPI changes) can re-clamp against the same caret (issue #3)
            *last_caret = Some(caret);

            pin_topmost(candidate_window.hwnd());
            pin_topmost(indicator_hwnd);

            let size = candidate_window.inner_size();
            reposition_candidate(
                candidate_window,
                last_caret,
                size.width as i32,
                size.height as i32,
            );
            // clamp the indicator into the work area too — it used to hang
            // off-screen near screen edges (B20)
            let indicator_size = indicator_window.inner_size();
            let (ix, iy) = utils::get_indicator_position(
                left,
                bottom,
                indicator_size.width as i32,
                indicator_size.height as i32,
            );
            indicator_window.set_outer_position(PhysicalPosition::new(ix, iy));

            if is_visible(candidate_window.hwnd()) {
                notify_ime_event(candidate_window.hwnd(), EVENT_OBJECT_IME_CHANGE);
            }
        }
        WindowAction::SetCandidate { candidates } => {
            let max_len = utils::max_candidate_chars(&candidates);

            // logical (CSS px) size: the webview lays out in CSS px, so a
            // physical-px window stayed too small at high DPI and clipped
            // the candidate text (B20)
            let scale = candidate_window.scale_factor();
            let height = candidate_window
                .inner_size()
                .to_logical::<f64>(scale)
                .height;
            let new_size = LogicalSize::new(
                utils::candidate_window_logical_width(max_len) as f64,
                height,
            );
            candidate_window.set_inner_size(new_size);

            // the window may have just grown for a longer candidate;
            // re-clamp for the size it is GOING to have — inner_size() can
            // still report the pre-resize value here (issue #3)
            let physical = new_size.to_physical::<i32>(scale);
            reposition_candidate(
                candidate_window,
                last_caret,
                physical.width,
                physical.height,
            );

            // Vec<String> serialization cannot fail; fall back to an empty
            // list rather than crash the UI
            let candidates =
                serde_json::to_string(&candidates).unwrap_or_else(|_| "[]".to_string());

            let _ = proxy.send_event(UserEvent::UpdateCandidates(candidates));

            // The window has already been resized here; the list contents
            // land asynchronously once the webview runs the script.
            //
            // Only while visible: ending a composition sends hide_window()
            // and then set_candidates(vec![]), which announced a CHANGE on a
            // window that had just been hidden.
            if is_visible(candidate_window.hwnd()) {
                notify_ime_event(candidate_window.hwnd(), EVENT_OBJECT_IME_CHANGE);
            }
        }
        WindowAction::SetSelection { index } => {
            let _ = proxy.send_event(UserEvent::UpdateSelection(index));
        }
        WindowAction::SetInputMode(input_method) => {
            let _ = proxy.send_event(UserEvent::UpdateInputMethod(input_method));

            if let Ok(mut flash) = indicator_flash.try_lock() {
                if let Some(task) = flash.take() {
                    task.abort();
                }

                // flash the mode indicator for half a second
                *flash = Some(tokio::spawn(async move {
                    set_visibility(indicator_hwnd, true);
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    set_visibility(indicator_hwnd, false);
                }));
            }
        }
    }
}
