//! Shared creation path for the IME's overlay windows (candidate list,
//! mode indicator — and any future popup): borderless, transparent,
//! non-activating, topmost tool windows.

use anyhow::{Context as _, Result};
use tao::{
    event_loop::EventLoop,
    platform::windows::{WindowBuilderExtWindows, WindowExtWindows},
    window::{Window, WindowBuilder},
};
use windows::Win32::{
    Foundation::HWND,
    UI::{
        Accessibility::NotifyWinEvent,
        WindowsAndMessaging::{
            CHILDID_SELF, GWL_EXSTYLE, GWL_STYLE, HWND_TOPMOST, IsWindowVisible, OBJID_CLIENT,
            SW_HIDE, SW_SHOWNOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SetWindowLongW,
            SetWindowPos, ShowWindow, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
        },
    },
};

use crate::UserEvent;

/// Shows or hides a window without activating it (the IME must never steal
/// focus from the host application). Takes the raw HWND so callers off the
/// UI thread — the indicator flash task — can use it too.
/// Returns whether the window was *already* visible, so a caller can tell a
/// real transition from a no-op. `ShowWindow` reports this for free.
pub fn set_visibility(hwnd: isize, visible: bool) -> bool {
    let cmd = if visible { SW_SHOWNOACTIVATE } else { SW_HIDE };
    unsafe { ShowWindow(HWND(hwnd as *mut std::ffi::c_void), cmd) }.as_bool()
}

/// Whether the window is currently visible.
pub fn is_visible(hwnd: isize) -> bool {
    unsafe { IsWindowVisible(HWND(hwnd as *mut std::ffi::c_void)) }.as_bool()
}

/// Announces a candidate-window change to the shell and to assistive
/// technology, which is how an IME "participates in the light dismiss model"
/// (a Windows IME requirement). Without these, nothing outside our process
/// can tell that the candidate UI appeared, moved, or went away — the window
/// is a `WS_EX_NOACTIVATE` tool window in a separate process, so it is
/// otherwise invisible to anything watching focus.
///
/// `event` is one of `EVENT_OBJECT_IME_SHOW` / `_HIDE` / `_CHANGE`.
/// Best-effort: `NotifyWinEvent` reports nothing and there is no useful
/// recovery if the shell is not listening.
pub fn notify_ime_event(hwnd: isize, event: u32) {
    unsafe {
        NotifyWinEvent(
            event,
            HWND(hwnd as *mut std::ffi::c_void),
            OBJID_CLIENT.0,
            CHILDID_SELF as i32,
        );
    }
}

/// Re-asserts the window's place in the topmost band without moving,
/// resizing, or activating it.
pub fn pin_topmost(hwnd: isize) {
    let _ = unsafe {
        SetWindowPos(
            HWND(hwnd as *mut std::ffi::c_void),
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    };
}

pub fn create_overlay_window(
    event_loop: &EventLoop<UserEvent>,
    title: &str,
    visible: bool,
) -> Result<Window> {
    let window = WindowBuilder::new()
        .with_decorations(false)
        .with_title(title)
        .with_focused(false)
        .with_visible(visible)
        .with_undecorated_shadow(false)
        .with_transparent(true)
        .build(event_loop)
        .context("Failed to create window")?;

    let hwnd = window.hwnd() as *mut std::ffi::c_void;

    // set extended window style
    // https://docs.microsoft.com/en-us/windows/win32/winmsg/extended-window-styles
    // https://docs.microsoft.com/en-us/windows/win32/winmsg/window-styles
    unsafe {
        let exnewstyle = WS_EX_TOOLWINDOW.0 | WS_EX_NOACTIVATE.0 | WS_EX_TOPMOST.0;
        SetWindowLongW(HWND(hwnd), GWL_EXSTYLE, exnewstyle as i32);

        let style = WS_POPUP.0;
        SetWindowLongW(HWND(hwnd), GWL_STYLE, style as i32);
    };

    Ok(window)
}
