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

/// Whether a `Show` can be honoured right now or has to wait for a position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShowDecision {
    ShowNow,
    Defer,
}

/// Where the candidate window may appear, and whether it is allowed to yet.
///
/// The TIP sends `SetPosition` before `Show` (`act_start_composition`), but
/// `update_pos` can come back with `TS_E_NOLAYOUT` — the host has no layout
/// for the range yet and answers later, via `OnLayoutChange`. The `Show`
/// still arrives on time, so the window used to become visible at the
/// PREVIOUS composition's caret, or at the origin on the first one, and only
/// then jump to the caret (issue #59).
///
/// So a position is not merely remembered, it is remembered as *fresh* or
/// not: fresh means "reported since the last hide", i.e. belonging to the
/// composition being shown.
///
/// The window's SIZE is the other half of the same problem. Until the webview
/// has measured itself the window still has tao's default (800x600), so a
/// `Show` that early puts an oversized window on screen — clamped against the
/// wrong height, hence in the wrong place — which then snaps down to its real
/// size. Both gates must be satisfied before a `Show` is honoured.
#[derive(Default, Debug)]
pub struct CandidatePlacement {
    /// The last caret rect the TIP reported. Outlives the hide on purpose —
    /// resizes re-clamp against it (issue #3).
    pub caret: Option<CaretRect>,
    /// Whether `caret` belongs to the composition currently being shown.
    fresh: bool,
    /// Whether the webview has reported its measured height yet.
    ///
    /// Deliberately NOT reset by `on_hide`, unlike `fresh`: the candidate
    /// window's height does not vary with the list. `adjustWindowSize` in
    /// candidate.js measures five sample rows once, on `DOMContentLoaded`,
    /// and never runs again — so the height is a property of the *window*,
    /// settled once per ui.exe lifetime. Clearing it per composition would
    /// mean waiting out the full grace period on every single one, for a
    /// measurement that is never coming.
    height_measured: bool,
    /// A `Show` arrived before both gates were satisfied.
    deferred: bool,
}

impl CandidatePlacement {
    /// A `Show` request. Deferring returns the window's visibility decision
    /// to `on_position`/`on_height` (or to `on_deadline`, so a host that
    /// never reports a layout still gets its candidates).
    pub fn on_show(&mut self) -> ShowDecision {
        if self.ready() {
            self.deferred = false;
            ShowDecision::ShowNow
        } else {
            self.deferred = true;
            ShowDecision::Defer
        }
    }

    /// A `SetPosition`. Returns true when it releases a deferred `Show`.
    pub fn on_position(&mut self, caret: CaretRect) -> bool {
        self.caret = Some(caret);
        self.fresh = true;
        self.take_deferred_if_ready()
    }

    /// The webview reported its measured height. Returns true when that
    /// releases a deferred `Show` — the mirror image of `on_position`.
    pub fn on_height(&mut self) -> bool {
        self.height_measured = true;
        self.take_deferred_if_ready()
    }

    /// A `Hide`. The rect stays for re-clamping; its freshness does not — the
    /// next composition's `Show` must wait for a position of its own. The
    /// measured height stays too (see the field).
    pub fn on_hide(&mut self) {
        self.fresh = false;
        self.deferred = false;
    }

    /// Whether a `Show` can be honoured: the caret belongs to this
    /// composition AND the window is the size it is going to be.
    fn ready(&self) -> bool {
        self.fresh && self.height_measured
    }

    /// Consumes a pending `Show` once both gates are satisfied. A gate that
    /// arrives while the other is still open must leave the `Show` deferred,
    /// not swallow it.
    fn take_deferred_if_ready(&mut self) -> bool {
        if self.ready() {
            std::mem::take(&mut self.deferred)
        } else {
            false
        }
    }

    /// The grace period ran out. Returns true when a `Show` is still waiting,
    /// which must now be honoured regardless: no candidates at all is worse
    /// than candidates in a stale spot.
    pub fn on_deadline(&mut self) -> bool {
        std::mem::take(&mut self.deferred)
    }
}

/// Which pipe connection the visible candidate window belongs to.
///
/// `Hide` only ever arrives as an RPC, so a host application killed while
/// converting leaves its candidate window on screen — topmost, over
/// everything, with nobody left to take it down. The connection dying is the
/// signal, but connections also die late: an application closed a moment ago
/// can be reported after the next one has already shown its candidates.
///
/// So the disconnect only hides when it names the connection that put the
/// window up. Anything else is a straggler and must be ignored.
#[derive(Default, Debug)]
pub struct ShowOwner {
    last: Option<i64>,
}

impl ShowOwner {
    /// A `Show` from `session` (`None` when the connection could not be
    /// identified, which then matches no disconnect).
    pub fn on_show(&mut self, session: Option<i64>) {
        self.last = session;
    }

    /// A normal `Hide`. The window is down; no disconnect should take it
    /// down again.
    pub fn on_hide(&mut self) {
        self.last = None;
    }

    /// A connection ended. Returns whether the window must be hidden.
    pub fn on_disconnect(&mut self, session: i64) -> bool {
        if self.last == Some(session) {
            self.last = None;
            true
        } else {
            false
        }
    }
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
    let x = caret.left - 45;
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

    const CARET: CaretRect = CaretRect {
        top: 100,
        left: 200,
        bottom: 140,
        right: 260,
    };

    /// A placement past its one-off startup measurement — the state every
    /// composition after the first runs in.
    fn measured() -> CandidatePlacement {
        let mut placement = CandidatePlacement::default();
        placement.on_height();
        placement
    }

    /// Issue #59: the ordinary path. The TIP positions before it shows, so
    /// the position is already fresh and the Show needs no deferral.
    #[test]
    fn a_show_after_a_position_is_immediate() {
        let mut placement = measured();
        placement.on_position(CARET);

        assert_eq!(placement.on_show(), ShowDecision::ShowNow);
    }

    /// Issue #59: with no position yet — the first composition, or one whose
    /// update_pos came back TS_E_NOLAYOUT — showing would put the window at
    /// the origin. The Show waits, and the position releases it.
    #[test]
    fn a_show_without_a_position_waits_for_one() {
        let mut placement = measured();

        assert_eq!(placement.on_show(), ShowDecision::Defer);
        assert!(
            placement.on_position(CARET),
            "the position must release the deferred show"
        );
        assert!(
            !placement.on_deadline(),
            "and it must not be shown a second time when the grace period ends"
        );
    }

    /// The other half of issue #59: until the webview reports its measured
    /// height the window still has tao's default size, so showing puts an
    /// oversized window up — clamped against the wrong height, hence in the
    /// wrong place — which then snaps down.
    #[test]
    fn a_show_before_the_height_is_measured_waits_for_it() {
        let mut placement = CandidatePlacement::default();
        placement.on_position(CARET);

        assert_eq!(
            placement.on_show(),
            ShowDecision::Defer,
            "a positioned but unmeasured window must not be shown yet"
        );
        assert!(
            placement.on_height(),
            "the measurement must release the deferred show"
        );
        assert!(
            !placement.on_deadline(),
            "and it must not be shown a second time when the grace period ends"
        );
    }

    /// Neither gate on its own is enough, and the one that arrives first must
    /// leave the `Show` deferred rather than swallow it.
    #[test]
    fn one_gate_alone_does_not_release_a_deferred_show() {
        let mut placement = CandidatePlacement::default();
        assert_eq!(placement.on_show(), ShowDecision::Defer);

        assert!(
            !placement.on_position(CARET),
            "the height is still unmeasured"
        );
        assert!(placement.on_height(), "now both gates are satisfied");
        assert!(
            !placement.on_deadline(),
            "the show was already honoured exactly once"
        );
    }

    /// The freshness is per composition: a rect from the PREVIOUS one is
    /// exactly the stale spot this exists to avoid, so a hide must invalidate
    /// it even though the rect itself is kept for re-clamping (issue #3).
    #[test]
    fn a_hide_invalidates_the_position_but_keeps_the_rect() {
        let mut placement = measured();
        placement.on_position(CARET);
        placement.on_hide();

        assert_eq!(
            placement.on_show(),
            ShowDecision::Defer,
            "the next composition must wait for a position of its own"
        );
        assert!(
            placement.caret.is_some(),
            "the rect stays available for re-clamping"
        );
    }

    /// …but the measurement is NOT per composition. candidate.js measures
    /// once on DOMContentLoaded and never again, so clearing this on hide
    /// would make every composition after the first sit out the full grace
    /// period waiting for an UpdateHeight that never comes.
    #[test]
    fn a_hide_keeps_the_measured_height() {
        let mut placement = measured();
        placement.on_position(CARET);
        placement.on_hide();
        placement.on_position(CARET);

        assert_eq!(
            placement.on_show(),
            ShowDecision::ShowNow,
            "the next composition must not wait for a second measurement"
        );
    }

    /// A host that never reports a layout would otherwise never show
    /// candidates at all, which is worse than showing them in a stale spot.
    #[test]
    fn the_grace_period_shows_a_deferred_window_anyway() {
        let mut placement = CandidatePlacement::default();
        placement.on_show();

        assert!(
            placement.on_deadline(),
            "the deferred show must be honoured"
        );
        assert!(
            !placement.on_deadline(),
            "but only once — a later deadline must not re-show a hidden window"
        );
    }

    /// A deadline landing after the composition was cancelled must not pop
    /// the window back up.
    #[test]
    fn a_deadline_after_a_hide_shows_nothing() {
        let mut placement = CandidatePlacement::default();
        placement.on_show();
        placement.on_hide();

        assert!(!placement.on_deadline());
    }

    #[test]
    fn candidate_length_is_measured_in_chars_not_bytes() {
        let candidates = vec!["水".to_string(), "みずうみ".to_string(), "mizu".to_string()];
        // みずうみ = 4 chars (12 UTF-8 bytes) — bytes would return 12
        assert_eq!(max_candidate_chars(&candidates), 4);
        assert_eq!(max_candidate_chars(&[]), 0);
    }

    #[test]
    fn a_disconnect_of_the_showing_connection_hides() {
        let mut owner = ShowOwner::default();
        owner.on_show(Some(7));

        assert!(
            owner.on_disconnect(7),
            "the app that put the window up died"
        );
        assert!(
            !owner.on_disconnect(7),
            "hiding twice for one death would fight the next composition"
        );
    }

    /// A connection can be reported dead after the next application has
    /// already shown its own candidates. Hiding then would blank a live
    /// composition in an unrelated app.
    #[test]
    fn a_stale_connections_disconnect_must_not_hide_the_new_composition() {
        let mut owner = ShowOwner::default();
        owner.on_show(Some(1));
        owner.on_show(Some(2));

        assert!(!owner.on_disconnect(1));
        assert!(owner.on_disconnect(2));
    }

    #[test]
    fn a_disconnect_after_a_normal_hide_is_a_noop() {
        let mut owner = ShowOwner::default();
        owner.on_show(Some(3));
        owner.on_hide();

        assert!(!owner.on_disconnect(3));
    }

    /// Without connect info there is nothing to match a disconnect against;
    /// the window then behaves exactly as it did before this existed.
    #[test]
    fn a_show_without_connect_info_never_matches() {
        let mut owner = ShowOwner::default();
        owner.on_show(None);

        assert!(!owner.on_disconnect(0));
        assert!(!owner.on_disconnect(1));
    }
}
