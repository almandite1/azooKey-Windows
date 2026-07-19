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
    UI::WindowsAndMessaging::{
        SetWindowLongW, SetWindowPos, ShowWindow, GWL_EXSTYLE, GWL_STYLE, HWND_TOPMOST,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_HIDE, SW_SHOWNOACTIVATE, WS_EX_NOACTIVATE,
        WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    },
};

use crate::UserEvent;

/// Shows or hides a window without activating it (the IME must never steal
/// focus from the host application). Takes the raw HWND so callers off the
/// UI thread — the indicator flash task — can use it too.
pub fn set_visibility(hwnd: isize, visible: bool) {
    let cmd = if visible { SW_SHOWNOACTIVATE } else { SW_HIDE };
    let _ = unsafe { ShowWindow(HWND(hwnd as *mut std::ffi::c_void), cmd) };
}

/// Re-asserts the window's place in the topmost band without moving,
/// resizing, or activating it.
pub fn pin_topmost(hwnd: isize) {
    let _ = unsafe {
        SetWindowPos(
            HWND(hwnd as *mut std::ffi::c_void),
            HWND_TOPMOST,
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
