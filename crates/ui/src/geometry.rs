//! Where the overlays go and how big they get: the pure size and position
//! arithmetic, and the one Win32 call it needs (the monitor work area).
//!
//! Everything here is a function of its arguments — no window handles, no
//! event loop — so the edge cases that actually bite (work-area flips,
//! saturating widths, a caret straddling two monitors) are unit-testable.

use windows::Win32::{
    Foundation::RECT,
    Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromRect},
};

/// The caret rectangle the TIP last reported (physical px, screen
/// coordinates). Kept around so the candidate window can be re-clamped when
/// it RESIZES: the window grows for longer candidates and taller lists after
/// it was positioned, and clamping only at position time let the grown
/// window overflow the work area (issue #3).
#[derive(Clone, Copy, Debug)]
pub struct CaretRect {
    pub top: i32,
    pub left: i32,
    pub bottom: i32,
    pub right: i32,
}

impl CaretRect {
    /// The caret as a Win32 `RECT`, for the monitor lookup. Both the
    /// candidate window and the indicator resolve their monitor through
    /// this, so the two cannot pick different monitors for the same caret
    /// (issue #29: the indicator used to build a zero-area rect of its own).
    pub fn as_rect(&self) -> RECT {
        RECT {
            left: self.left,
            top: self.top,
            right: self.right,
            bottom: self.bottom,
        }
    }
}

/// Narrowest the candidate window may get, in CSS px: below this the window
/// reads as a stray tooltip rather than a list.
pub const MIN_CANDIDATE_WINDOW_WIDTH: u32 = 225;

/// Widest it may get. Candidate length is not bounded — a prediction can
/// cover a whole phrase — and an uncapped window stretched to match, at which
/// point `clamp_candidate_position` had to slide it away from the caret to
/// keep it inside the work area, leaving the candidates nowhere near the text
/// they belong to. Past this the rows are ellipsized instead
/// (`candidate.css`), which costs the tail of one long candidate rather than
/// the position of all of them.
pub const MAX_CANDIDATE_WINDOW_WIDTH: u32 = 640;

/// Everything in a candidate row that is not the candidate text: the index
/// number, the gaps around it, the border and the scrollbar gutter. Added
/// once, whatever the list holds.
const CANDIDATE_ROW_CHROME_WIDTH: u32 = 120;

/// Assumed advance width of one candidate character, in CSS px. A rough
/// average rather than a measurement — the webview is the only thing that
/// could measure the actual font, and asking it would mean a round trip
/// before the window could be sized at all.
const CANDIDATE_PX_PER_CHAR: u32 = 18;

/// How far LEFT of the caret each overlay is placed, in physical px, before
/// clamping. The candidate window nudges just past the row chrome so the
/// first candidate lines up under the text; the indicator sits further out
/// so it does not cover the caret itself.
const CANDIDATE_CARET_X_OFFSET: i32 = -15;
const INDICATOR_CARET_X_OFFSET: i32 = -45;

/// Logical (CSS px) width of the candidate window for the longest candidate,
/// in characters. The webview lays out in CSS px, so this must be applied as
/// a `LogicalSize` — applying it as physical px left the window too small on
/// high-DPI displays (B20).
pub fn candidate_window_logical_width(max_len: u32) -> u32 {
    // saturating: `max_len` is a character count that arrives from the engine,
    // and an overflowing multiply would wrap to a comically narrow window
    // rather than a wide one
    let wanted =
        CANDIDATE_ROW_CHROME_WIDTH.saturating_add(max_len.saturating_mul(CANDIDATE_PX_PER_CHAR));
    wanted.clamp(MIN_CANDIDATE_WINDOW_WIDTH, MAX_CANDIDATE_WINDOW_WIDTH)
}

/// Length of the longest candidate in CHARS, not bytes — a byte count would
/// size the window 3x too wide for CJK candidates.
pub fn max_candidate_chars(candidates: &[String]) -> u32 {
    candidates
        .iter()
        .map(|s| s.chars().count())
        .max()
        .unwrap_or(0) as u32
}

/// Work-area clamping for the candidate window, in physical px.
///
/// `(x, y)` is the tentative top-left, `top` the caret top (used to flip the
/// window above the caret when it would overflow the bottom edge).
fn clamp_candidate_position(
    x: i32,
    y: i32,
    top: i32,
    win_width: i32,
    win_height: i32,
    work: &RECT,
) -> (i32, i32) {
    // If the bottom of the candidate window is hidden, show it above
    let y = if y + win_height > work.bottom {
        top - win_height
    } else {
        y
    };
    // A list taller than the space above the caret would flip right off the
    // top edge; a clipped-at-the-top list at least keeps the first (most
    // likely) candidates visible
    let y = y.max(work.top);

    // If the right of the candidate window is hidden, show it to the left
    let mut x = if x + win_width > work.right {
        work.right - win_width
    } else {
        x
    };

    // If the left of the candidate window is hidden, show it to the right
    if x < work.left {
        x = work.left;
    }

    (x, y)
}

/// Work-area clamping for the mode indicator, in physical px: keep the whole
/// window inside the work area (no above-the-caret flip; the indicator is a
/// transient badge).
fn clamp_indicator_position(
    x: i32,
    y: i32,
    win_width: i32,
    win_height: i32,
    work: &RECT,
) -> (i32, i32) {
    let x = x.min(work.right - win_width).max(work.left);
    let y = y.min(work.bottom - win_height).max(work.top);
    (x, y)
}

/// Looks up the work area of the monitor nearest to `rect`. Returns `None`
/// when the lookup fails — the caller must then skip clamping entirely:
/// clamping against the default zero rect threw the window to the screen
/// origin (B20).
fn work_area_near(rect: RECT) -> Option<RECT> {
    let monitor = unsafe { MonitorFromRect(&rect as *const _, MONITOR_DEFAULTTONEAREST) };

    let mut monitor_info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };

    let ok = unsafe { GetMonitorInfoW(monitor, &mut monitor_info) }.as_bool();
    ok.then_some(monitor_info.rcWork)
}

/// Computes the candidate window's top-left for a caret rect and a WINDOW
/// SIZE PASSED IN by the caller (physical px). The size is a parameter, not
/// read from the window, because the interesting call sites are resizes:
/// right after `set_inner_size` the window may not report its new size yet,
/// and clamping against the stale one is what let the window overflow the
/// screen edge (issue #3).
pub fn get_candidate_window_position(
    caret: &CaretRect,
    win_width: i32,
    win_height: i32,
) -> (f64, f64) {
    let x = caret.left + CANDIDATE_CARET_X_OFFSET;
    let y = caret.bottom;

    let Some(work) = work_area_near(caret.as_rect()) else {
        return (x as f64, y as f64);
    };

    let (x, y) = clamp_candidate_position(x, y, caret.top, win_width, win_height, &work);

    (x as f64, y as f64)
}

/// Same contract as `get_candidate_window_position`: the caret rect the TIP
/// reported, and the window size from the caller. It takes the whole rect
/// rather than a corner so its monitor lookup is the candidate window's
/// lookup — a caret straddling a monitor boundary used to be able to put the
/// two on different monitors (issue #29).
pub fn get_indicator_position(caret: &CaretRect, win_width: i32, win_height: i32) -> (f64, f64) {
    let x = caret.left + INDICATOR_CARET_X_OFFSET;
    let y = caret.bottom;

    let Some(work) = work_area_near(caret.as_rect()) else {
        return (x as f64, y as f64);
    };

    let (x, y) = clamp_indicator_position(x, y, win_width, win_height, &work);

    (x as f64, y as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORK: RECT = RECT {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040, // taskbar excluded
    };

    #[test]
    fn width_has_a_minimum() {
        // 120 + 5*18 = 210 < 225, so short candidates get the floor
        assert_eq!(candidate_window_logical_width(0), 225);
        assert_eq!(candidate_window_logical_width(5), 225);
    }

    #[test]
    fn width_scales_with_the_longest_candidate() {
        assert_eq!(candidate_window_logical_width(6), 228);
        assert_eq!(candidate_window_logical_width(20), 480);
    }

    /// Candidate length is unbounded, the window is not. Without the cap one
    /// long prediction stretched the window until `clamp_candidate_position`
    /// had to slide it away from the caret to fit the work area — so a single
    /// candidate moved the whole list away from the text it belonged to.
    #[test]
    fn width_has_a_maximum() {
        // 120 + 28*18 = 624, still under the cap
        assert_eq!(candidate_window_logical_width(28), 624);
        // 120 + 29*18 = 642 would exceed it
        assert_eq!(candidate_window_logical_width(29), 640);
        assert_eq!(candidate_window_logical_width(200), 640);
    }

    /// The character count comes from the engine, and `120 + len * 18` is a
    /// u32 multiply. Wrapping would size the window from the low bits of a
    /// huge number, which lands anywhere — including below the floor.
    #[test]
    fn an_absurd_candidate_length_saturates_instead_of_wrapping() {
        assert_eq!(candidate_window_logical_width(u32::MAX), 640);
        assert_eq!(candidate_window_logical_width(u32::MAX / 18), 640);
    }

    #[test]
    fn candidate_position_is_unchanged_when_it_fits() {
        assert_eq!(
            clamp_candidate_position(100, 500, 480, 300, 200, &WORK),
            (100, 500)
        );
    }

    #[test]
    fn candidate_flips_above_the_caret_at_the_bottom_edge() {
        // 1000 + 200 > 1040 -> show above the caret top (900 - 200)
        assert_eq!(
            clamp_candidate_position(100, 1000, 900, 300, 200, &WORK),
            (100, 700)
        );
    }

    #[test]
    fn candidate_flip_is_clamped_at_the_top_edge() {
        // caret near the bottom (top 300, bottom 320) with an 800px list:
        // 320 + 800 > 1040 flips it above to 300 - 800 = -500, which would
        // put the first candidates off-screen — clamp to the top edge
        assert_eq!(
            clamp_candidate_position(100, 320, 300, 300, 800, &WORK),
            (100, 0)
        );
    }

    #[test]
    fn candidate_is_pulled_in_at_the_right_edge() {
        // 1800 + 300 > 1920 -> right-aligned to the work area
        assert_eq!(
            clamp_candidate_position(1800, 500, 480, 300, 200, &WORK),
            (1620, 500)
        );
    }

    #[test]
    fn candidate_is_pushed_in_at_the_left_edge() {
        assert_eq!(
            clamp_candidate_position(-30, 500, 480, 300, 200, &WORK),
            (0, 500)
        );
    }

    #[test]
    fn indicator_is_clamped_into_the_work_area() {
        assert_eq!(clamp_indicator_position(-10, 500, 90, 90, &WORK), (0, 500));
        assert_eq!(
            clamp_indicator_position(1900, 1020, 90, 90, &WORK),
            (1830, 950)
        );
        assert_eq!(
            clamp_indicator_position(100, 500, 90, 90, &WORK),
            (100, 500)
        );
    }

    /// Issue #29: the indicator built its own lookup rect as
    /// `{left, top: bottom, right: left, bottom}` — zero area, and with
    /// left/bottom reused in the wrong fields. MONITOR_DEFAULTTONEAREST
    /// treats that like a point, so it mostly behaved, but a caret spanning
    /// a monitor boundary could resolve to a different monitor than the
    /// candidate window's. Both now ask about the same rectangle.
    #[test]
    fn the_monitor_lookup_rect_is_the_whole_caret() {
        let caret = CaretRect {
            top: 100,
            left: 200,
            bottom: 140,
            right: 260,
        };

        let rect = caret.as_rect();

        assert_eq!(
            (rect.left, rect.top, rect.right, rect.bottom),
            (200, 100, 260, 140),
            "the caret's own edges, each in its own field"
        );
        assert!(
            rect.right > rect.left && rect.bottom > rect.top,
            "a degenerate rect leaves the monitor choice to how \
             MonitorFromRect happens to treat empty input"
        );
    }

    #[test]
    fn candidate_length_is_measured_in_chars_not_bytes() {
        let candidates = vec!["水".to_string(), "みずうみ".to_string(), "mizu".to_string()];
        // みずうみ = 4 chars (12 UTF-8 bytes) — bytes would return 12
        assert_eq!(max_candidate_chars(&candidates), 4);
        assert_eq!(max_candidate_chars(&[]), 0);
    }

    /// The indicator sits further from the caret than the candidate list so
    /// it does not cover the text being composed. Both offsets are applied
    /// through the two `get_*_position` functions, which is where a swap
    /// would actually show up.
    #[test]
    fn the_indicator_is_offset_further_left_than_the_candidate_window() {
        let caret = CaretRect {
            top: 100,
            left: 800,
            bottom: 140,
            right: 860,
        };
        // small windows in the middle of any plausible work area, so neither
        // result is clamped and the offsets are what is being compared
        let (candidate_x, _) = get_candidate_window_position(&caret, 200, 100);
        let (indicator_x, _) = get_indicator_position(&caret, 90, 90);

        assert!(
            indicator_x < candidate_x,
            "the indicator must sit further from the caret ({indicator_x} vs {candidate_x})"
        );
    }
}
