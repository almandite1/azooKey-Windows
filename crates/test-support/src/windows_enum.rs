//! Walking the top-level windows.
//!
//! Three copies of the same `EnumWindows` + `Search` struct + `extern
//! "system"` callback existed, differing only in what they asked about each
//! window. Here the caller passes the question.

use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId,
};
use windows::core::BOOL;

use crate::Hwnd;

/// What a walk should do after looking at one window.
pub enum Walk {
    /// Keep enumerating.
    Continue,
    /// This is the one; stop.
    Stop,
}

/// Calls `visit` for every top-level window until it answers [`Walk::Stop`].
///
/// `EnumWindows` reports failure when the callback stops the enumeration,
/// which is exactly what a hit does — the result belongs to the caller's
/// closure, so the return value is discarded.
pub fn enumerate_windows(mut visit: impl FnMut(Hwnd) -> Walk) {
    let mut visit: &mut dyn FnMut(Hwnd) -> Walk = &mut visit;
    let _ = unsafe {
        EnumWindows(
            Some(enum_proc),
            LPARAM(&mut visit as *mut &mut dyn FnMut(Hwnd) -> Walk as isize),
        )
    };
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let visit = unsafe { &mut *(lparam.0 as *mut &mut dyn FnMut(Hwnd) -> Walk) };
    match visit(Hwnd::from(hwnd)) {
        Walk::Continue => BOOL(1),
        Walk::Stop => BOOL(0),
    }
}

/// The window's title. Empty for an untitled window, which for these
/// harnesses means a helper (tooltip, IME overlay) rather than a document
/// window.
pub fn window_title(window: Hwnd) -> String {
    let mut text = [0u16; 64];
    let len = unsafe { GetWindowTextW(window.raw(), &mut text) };
    String::from_utf16_lossy(&text[..len.max(0) as usize])
}

/// The pid of the process that owns `window`.
pub fn window_pid(window: Hwnd) -> u32 {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(window.raw(), Some(&mut pid)) };
    pid
}

/// The first top-level window whose title is `title` and whose owning pid
/// satisfies `owned_by`.
///
/// Both halves matter: the overlay titles are fixed strings the INSTALLED
/// ui.exe uses too, so a title match alone can find the wrong window.
pub fn find_window(title: &str, owned_by: impl Fn(u32) -> bool) -> Option<Hwnd> {
    let mut found = None;
    enumerate_windows(|window| {
        if window_title(window) == title && owned_by(window_pid(window)) {
            found = Some(window);
            Walk::Stop
        } else {
            Walk::Continue
        }
    });
    found
}
