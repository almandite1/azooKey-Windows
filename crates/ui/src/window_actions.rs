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
use crate::utils;
use crate::window::{pin_topmost, set_visibility};
use crate::UserEvent;

pub fn handle_window_action(
    action: WindowAction,
    candidate_window: &Window,
    indicator_window: &Window,
    indicator_flash: &Arc<Mutex<Option<JoinHandle<()>>>>,
    proxy: &EventLoopProxy<UserEvent>,
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

            set_visibility(candidate_window.hwnd(), true);
        }
        WindowAction::Hide => {
            set_visibility(candidate_window.hwnd(), false);
        }
        WindowAction::SetPosition {
            top,
            left,
            bottom,
            right,
        } => {
            let (x, y) =
                utils::get_candidate_window_position(top, left, bottom, right, candidate_window);

            pin_topmost(candidate_window.hwnd());
            pin_topmost(indicator_hwnd);

            candidate_window.set_outer_position(PhysicalPosition::new(x, y));
            // clamp the indicator into the work area too — it used to hang
            // off-screen near screen edges (B20)
            let (ix, iy) = utils::get_indicator_position(left, bottom, indicator_window);
            indicator_window.set_outer_position(PhysicalPosition::new(ix, iy));
        }
        WindowAction::SetCandidate { candidates } => {
            let max_len = utils::max_candidate_chars(&candidates);

            // logical (CSS px) size: the webview lays out in CSS px, so a
            // physical-px window stayed too small at high DPI and clipped
            // the candidate text (B20)
            let scale = candidate_window.scale_factor();
            let height = candidate_window.inner_size().to_logical::<f64>(scale).height;
            candidate_window.set_inner_size(LogicalSize::new(
                utils::candidate_window_logical_width(max_len) as f64,
                height,
            ));

            // Vec<String> serialization cannot fail; fall back to an empty
            // list rather than crash the UI
            let candidates =
                serde_json::to_string(&candidates).unwrap_or_else(|_| "[]".to_string());

            let _ = proxy.send_event(UserEvent::UpdateCandidates(candidates));
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
