//! Finding the IME's own windows — the candidate list and the mode indicator
//! that `ui.exe` puts on screen.
//!
//! They are the only outward sign of some behaviour a document cannot show:
//! whether the IME engaged at all. Scenario 4 needs exactly that — a password
//! field masks its content, so "was anything converted there" cannot be read
//! from the field, but "did the candidate window come up" can.

use test_support::{CANDIDATE_TITLE, Hwnd, process::image_name_of, windows_enum::find_window};

/// The process that owns the overlays.
const UI_IMAGE: &str = "ui.exe";

/// The candidate window of the running `ui.exe`, if it exists.
///
/// Matched on title *and* owning image: `ui.exe` runs as two processes (the
/// UIAccess re-exec leaves a supervision shim behind), and only one of them
/// owns the window. The title itself is set in `crates/ui/src/candidate.rs`.
pub fn candidate_window() -> Option<Hwnd> {
    find_window(CANDIDATE_TITLE, |pid| {
        image_name_of(pid).is_some_and(|name| name == UI_IMAGE)
    })
}

/// Whether the candidate window is on screen right now. `false` when there is
/// no candidate window at all, which for these scenarios means the same
/// thing: the IME is not showing candidates.
pub fn candidates_visible() -> bool {
    candidate_window().is_some_and(Hwnd::is_visible)
}
