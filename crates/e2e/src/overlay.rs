//! Finding the IME's own windows — the candidate list and the mode indicator
//! that `ui.exe` puts on screen.
//!
//! They are the only outward sign of some behaviour a document cannot show:
//! whether the IME engaged at all. Scenario 4 needs exactly that — a password
//! field masks its content, so "was anything converted there" cannot be read
//! from the field, but "did the candidate window come up" can.

use windows::Win32::Foundation::{HWND, LPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
};
use windows::core::BOOL;

use crate::host::image_name_of;

/// The process that owns the overlays.
const UI_IMAGE: &str = "ui.exe";
/// Window titles set in `crates/ui/src/{candidate,indicator}.rs`.
const CANDIDATE_TITLE: &str = "CandidateList";

/// The candidate window of the running `ui.exe`, if it exists.
///
/// Matched on title *and* owning image: `ui.exe` runs as two processes (the
/// UIAccess re-exec leaves a supervision shim behind), and only one of them
/// owns the window.
pub fn candidate_window() -> Option<HWND> {
    find_window(UI_IMAGE, CANDIDATE_TITLE)
}

/// Whether the candidate window is on screen right now. `false` when there is
/// no candidate window at all, which for these scenarios means the same
/// thing: the IME is not showing candidates.
pub fn candidates_visible() -> bool {
    candidate_window().is_some_and(|hwnd| unsafe { IsWindowVisible(hwnd) }.as_bool())
}

struct Search<'a> {
    image: &'a str,
    title: &'a str,
    found: Option<isize>,
}

fn find_window(image: &str, title: &str) -> Option<HWND> {
    let mut search = Search {
        image,
        title,
        found: None,
    };
    // EnumWindows reports failure when the callback stops the walk, which a
    // hit does — the answer is in `search` either way.
    let _ = unsafe { EnumWindows(Some(enum_proc), LPARAM(&mut search as *mut Search as isize)) };
    search.found.map(|hwnd| HWND(hwnd as *mut std::ffi::c_void))
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = unsafe { &mut *(lparam.0 as *mut Search) };

    let mut text = [0u16; 64];
    let len = unsafe { GetWindowTextW(hwnd, &mut text) };
    if String::from_utf16_lossy(&text[..len as usize]) != search.title {
        return BOOL(1);
    }

    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if image_name_of(pid).is_some_and(|name| name == search.image) {
        search.found = Some(hwnd.0 as isize);
        return BOOL(0);
    }
    BOOL(1)
}
