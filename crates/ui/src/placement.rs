//! When the candidate window may be shown, and whose it is once it is.
//!
//! Two independent pieces of bookkeeping, both of them answers to "the TIP
//! and the host application do not tell us everything we need, in the order
//! we need it": [`CandidatePlacement`] holds a `Show` back until the window
//! has a position and a size of its own (issue #59), and [`ShowOwner`]
//! remembers which connection put the window up so a late disconnect cannot
//! take down somebody else's composition (issue #67).

use crate::geometry::CaretRect;

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
    /// Whether the webview has EVER reported a measured height.
    ///
    /// Deliberately NOT reset by `on_hide`, unlike `fresh`. The height does
    /// vary with the list (candidate.js re-measures on every
    /// `updateCandidates` — issue #81), but this gate is not asking "is the
    /// height current", it is asking "has the window stopped being tao's
    /// 800x600 default". That happens once per ui.exe lifetime and never
    /// unhappens. Clearing it per composition would defer every `Show` until
    /// the grace period expired, waiting on a measurement that only arrives
    /// once the candidates do.
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// …but the measurement gate is NOT per composition. candidate.js
    /// re-measures on every `updateCandidates` (#81), yet the gate only asks
    /// "has the window stopped being tao's 800x600 default" — which happens
    /// once per ui.exe lifetime. Clearing it on hide would make every
    /// composition after the first sit out the full grace period, waiting on
    /// a measurement that only arrives once the candidates do.
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
