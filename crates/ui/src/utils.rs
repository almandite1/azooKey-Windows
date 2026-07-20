use windows::Win32::{
    Foundation::RECT,
    Graphics::Gdi::{GetMonitorInfoW, MonitorFromRect, MONITORINFO, MONITOR_DEFAULTTONEAREST},
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

/// Logical (CSS px) width of the candidate window for the longest candidate,
/// in characters. The webview lays out in CSS px, so this must be applied as
/// a `LogicalSize` — applying it as physical px left the window too small on
/// high-DPI displays (B20).
pub fn candidate_window_logical_width(max_len: u32) -> u32 {
    std::cmp::max(225, 120 + max_len * 18)
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
    let x = caret.left - 15;
    let y = caret.bottom;

    let Some(work) = work_area_near(RECT {
        left: caret.left,
        top: caret.top,
        right: caret.right,
        bottom: caret.bottom,
    }) else {
        return (x as f64, y as f64);
    };

    let (x, y) = clamp_candidate_position(x, y, caret.top, win_width, win_height, &work);

    (x as f64, y as f64)
}

pub fn get_indicator_position(
    left: i32,
    bottom: i32,
    win_width: i32,
    win_height: i32,
) -> (f64, f64) {
    let x = left - 45;
    let y = bottom;

    let Some(work) = work_area_near(RECT {
        left,
        top: bottom,
        right: left,
        bottom,
    }) else {
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

    #[test]
    fn candidate_length_is_measured_in_chars_not_bytes() {
        let candidates = vec!["水".to_string(), "みずうみ".to_string(), "mizu".to_string()];
        // みずうみ = 4 chars (12 UTF-8 bytes) — bytes would return 12
        assert_eq!(max_candidate_chars(&candidates), 4);
        assert_eq!(max_candidate_chars(&[]), 0);
    }
}
