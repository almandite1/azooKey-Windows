use std::{
    cell::{Ref, RefCell, RefMut},
    collections::HashMap,
    time::{Duration, Instant},
};

use windows::{
    Win32::UI::TextServices::{ITfCompartment, ITfContext, ITfThreadMgr, TF_PRESERVEDKEY},
    core::{GUID, Interface},
};

use anyhow::{Context, Result};

use crate::engine::{composition::Composition, input_mode::InputMode, ipc_service::Candidates};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UpdatePosState {
    #[default]
    Idle,
    Updating {
        suppress_layout_until: Instant,
    },
    SuppressingLayoutChange {
        until: Instant,
    },
}

impl UpdatePosState {
    const LAYOUT_CHANGE_SUPPRESSION: Duration = Duration::from_millis(200);

    pub fn try_begin_update(&mut self, now: Instant) -> bool {
        if matches!(self, Self::Updating { .. }) {
            return false;
        }

        *self = Self::Updating {
            suppress_layout_until: now + Self::LAYOUT_CHANGE_SUPPRESSION,
        };

        true
    }

    pub fn finish_update(&mut self, now: Instant) {
        *self = match *self {
            Self::Updating {
                suppress_layout_until,
            } if now <= suppress_layout_until => Self::SuppressingLayoutChange {
                until: suppress_layout_until,
            },
            Self::Updating { .. } => Self::Idle,
            state => state,
        };
    }

    pub fn should_skip_layout_change(&mut self, now: Instant) -> bool {
        match *self {
            Self::Idle => false,
            Self::Updating { .. } => true,
            Self::SuppressingLayoutChange { until } if now <= until => true,
            Self::SuppressingLayoutChange { .. } => {
                *self = Self::Idle;
                false
            }
        }
    }
}

/// What the host is told about our candidate list, and what it may read back.
///
/// The candidate snapshot lives here rather than being read from
/// `Composition` on demand, because the host calls `GetCount`/`GetString`
/// **synchronously from inside** `BeginUIElement`/`UpdateUIElement` — and we
/// issue those from inside `handle_action`'s dispatch loop, where
/// `Composition` still holds the *previous* keystroke's candidates (the loop
/// works on local copies and writes back only at the end, see
/// engine/composition.rs). Reading `Composition` there would serve stale
/// candidates to the host.
#[derive(Default, Debug)]
pub struct UiElementState {
    /// `Some` once `BeginUIElement` succeeded; the id the host gave us.
    pub id: Option<u32>,
    /// The host's latest answer to "may our own window be shown?".
    pub show: bool,
    /// Reported by `GetUpdatedFlags`; the host reads it *after*
    /// `UpdateUIElement` returns, so it must outlive the call.
    pub updated_flags: u32,
    pub candidates: Candidates,
    pub selection_index: i32,
}

#[derive(Default, Debug)]
pub struct TextService {
    pub tid: u32,
    pub thread_mgr: Option<ITfThreadMgr>,
    pub context: Option<ITfContext>,
    /// Advise cookies for this activation's sinks (thread-mgr event sink,
    /// text layout sink). Per-instance on purpose: TSF activates one TIP per
    /// UI thread, and keeping these in the process-global IMEState let one
    /// thread overwrite (and later unadvise with) another thread's cookie
    /// (B14).
    pub cookies: HashMap<GUID, u32>,
    /// The document context the text layout sink is advised on. Distinct
    /// from `context` (the key-input context set per keystroke). Also
    /// per-instance: a global slot made one thread Unadvise another
    /// thread's context across COM apartments (B14).
    pub layout_context: Option<ITfContext>,
    pub composition: RefCell<Composition>,
    pub update_pos_state: UpdatePosState,
    pub display_attribute_atom: HashMap<GUID, u32>,
    /// The current input mode (あ/A). Per-instance on purpose: the langbar
    /// item, mode indicator, and conversion mode are all activation-scoped
    /// (one TIP per UI thread) — this was the last activation-scoped field
    /// left in the process-global IMEState.
    pub input_mode: InputMode,
    /// Compartments this activation advised `ITfCompartmentEventSink` on,
    /// paired with their cookies. Kept separate from `cookies` because that
    /// map holds one cookie per sink IID, while this one sink is advised on
    /// several compartments — and `UnadviseSink` must be called on the very
    /// object that `AdviseSink` was called on.
    pub compartment_sinks: Vec<(ITfCompartment, u32)>,
    /// The keys this activation reserved with `ITfKeystrokeMgr::PreserveKey`,
    /// and only those: Deactivate must unpreserve exactly what Activate got,
    /// and a host that refused one leaves it out (same per-instance rule as
    /// the sink cookies, B14). Also read on the key path — a reserved key
    /// arrives through `OnPreservedKey`, so the raw VK must not toggle again.
    pub preserved_keys: Vec<(GUID, TF_PRESERVEDKEY)>,
    /// When the input mode was last toggled, by whichever path.
    ///
    /// The double-delivery guard (issue #19): if one physical press of the
    /// on/off key ever reached the TIP twice, each delivery would flip the
    /// mode and the two would cancel — the key would look dead. Keyed off
    /// the toggle rather than off which path delivered the key, because the
    /// paths cannot be told apart reliably: TSF answers `IsPreservedKey`
    /// true for a chord it then never dispatches through `OnPreservedKey`
    /// (measured with Alt+VK_KANJI on a 101-key layout).
    ///
    /// PRECAUTIONARY, not a fix for anything observed: no host tested so far
    /// double-delivers. An earlier reading of the logs suggested it did, and
    /// that was a measurement error — the log is UTF-8 and was being read as
    /// ANSI, which mangled the `mode="あ"` lines into what looked like a
    /// second keystroke.
    pub last_mode_toggle: Option<Instant>,
    /// Set while we are writing our own mode into the compartments, so the
    /// `OnChange` that TSF dispatches synchronously from inside `SetValue`
    /// does not bounce straight back into another write.
    pub suppress_compartment_echo: bool,
    /// Armed when a Backspace ended the composition, so the rest of that
    /// key's autorepeat can be thrown away.
    ///
    /// Holding Backspace deletes the reading kana by kana and then the
    /// composition is gone — but the key is still down, and the presses that
    /// keep arriving pass through to the host, which deletes the text the
    /// user just committed. The window is small and the damage is not: it is
    /// the document, not the composition.
    ///
    /// Only autorepeat is discarded. A fresh press with the key released in
    /// between is the user asking to delete their own text, and must reach the
    /// host untouched — which is also what disarms this, along with any other
    /// key and the Backspace key-up.
    pub discard_backspace_repeat: bool,
    /// UILess-mode state for the candidate list.
    pub ui_element: UiElementState,
    // NOTE: no `this` self-reference here. The COM object is reachable from
    // any TSF callback via TextServiceFactory::this() (a QueryInterface on
    // the containing allocation); storing a strong interface pointer in the
    // object itself created a refcount cycle that leaked every instance (B15).
}

impl TextService {
    pub fn thread_mgr(&self) -> Result<ITfThreadMgr> {
        self.thread_mgr.clone().context("Thread manager is null")
    }

    pub fn context<I: Interface>(&self) -> Result<I> {
        let context = self.context.as_ref().context("Context is null")?;
        Ok(context.cast()?)
    }

    pub fn borrow_composition(&self) -> Result<Ref<'_, Composition>> {
        Ok(self.composition.try_borrow()?)
    }

    pub fn borrow_mut_composition(&self) -> Result<RefMut<'_, Composition>> {
        Ok(self.composition.try_borrow_mut()?)
    }
}

/// The suppression window is what keeps `update_pos` and `OnLayoutChange`
/// from feeding each other: moving the candidate window makes the host
/// report a layout change, which would move the window again. The state
/// machine is pure and takes its `Instant`, so every boundary below is
/// exact rather than timing-dependent (issue #37 is about how often hosts
/// fire `OnLayoutChange`, so the arithmetic that answers it must be pinned).
#[cfg(test)]
mod tests {
    use super::UpdatePosState;
    use std::time::{Duration, Instant};

    const WINDOW: Duration = UpdatePosState::LAYOUT_CHANGE_SUPPRESSION;

    #[test]
    fn beginning_an_update_arms_the_suppression_window() {
        let now = Instant::now();
        let mut state = UpdatePosState::Idle;

        assert!(state.try_begin_update(now));
        assert_eq!(
            state,
            UpdatePosState::Updating {
                suppress_layout_until: now + WINDOW
            }
        );
    }

    /// A second `update_pos` entered while the first is still running is
    /// refused — and must not push the deadline out, or a busy host could
    /// keep the window armed indefinitely.
    #[test]
    fn a_reentrant_begin_is_refused_and_keeps_the_deadline() {
        let now = Instant::now();
        let mut state = UpdatePosState::Idle;
        assert!(state.try_begin_update(now));

        assert!(!state.try_begin_update(now + Duration::from_millis(50)));
        assert_eq!(
            state,
            UpdatePosState::Updating {
                suppress_layout_until: now + WINDOW
            }
        );
    }

    #[test]
    fn finishing_inside_the_window_keeps_suppressing_until_the_same_deadline() {
        let now = Instant::now();
        let mut state = UpdatePosState::Idle;
        state.try_begin_update(now);

        state.finish_update(now + Duration::from_millis(50));

        assert_eq!(
            state,
            UpdatePosState::SuppressingLayoutChange {
                until: now + WINDOW
            },
            "the deadline is the one armed at begin, not a fresh one"
        );
    }

    /// `now <= suppress_layout_until`: an update that finishes exactly on the
    /// deadline still suppresses.
    #[test]
    fn finishing_exactly_on_the_deadline_still_suppresses() {
        let now = Instant::now();
        let mut state = UpdatePosState::Idle;
        state.try_begin_update(now);

        state.finish_update(now + WINDOW);

        assert_eq!(
            state,
            UpdatePosState::SuppressingLayoutChange {
                until: now + WINDOW
            }
        );
    }

    /// An update slower than the window has already outlived any layout
    /// change it could have caused, so there is nothing left to suppress.
    #[test]
    fn finishing_past_the_deadline_goes_straight_to_idle() {
        let now = Instant::now();
        let mut state = UpdatePosState::Idle;
        state.try_begin_update(now);

        state.finish_update(now + WINDOW + Duration::from_millis(1));

        assert_eq!(state, UpdatePosState::Idle);
    }

    /// `finish_update` is called from the tail of `update_pos`, including the
    /// paths that never began one (a refused re-entrant call). Those must
    /// leave the state alone rather than clearing somebody else's window.
    #[test]
    fn finishing_without_an_update_in_flight_changes_nothing() {
        let now = Instant::now();

        let mut idle = UpdatePosState::Idle;
        idle.finish_update(now);
        assert_eq!(idle, UpdatePosState::Idle);

        let mut suppressing = UpdatePosState::SuppressingLayoutChange {
            until: now + WINDOW,
        };
        suppressing.finish_update(now);
        assert_eq!(
            suppressing,
            UpdatePosState::SuppressingLayoutChange {
                until: now + WINDOW
            }
        );
    }

    #[test]
    fn layout_changes_are_not_skipped_when_idle() {
        let mut state = UpdatePosState::Idle;

        assert!(!state.should_skip_layout_change(Instant::now()));
        assert_eq!(state, UpdatePosState::Idle);
    }

    /// The layout change a host fires from inside our own `SetWindowPos` —
    /// arriving before `finish_update` — is the innermost turn of the loop.
    #[test]
    fn layout_changes_during_an_update_are_skipped() {
        let now = Instant::now();
        let mut state = UpdatePosState::Idle;
        state.try_begin_update(now);

        assert!(state.should_skip_layout_change(now + Duration::from_millis(1)));
        assert_eq!(
            state,
            UpdatePosState::Updating {
                suppress_layout_until: now + WINDOW
            },
            "an in-flight update is not cleared by a layout change"
        );
    }

    #[test]
    fn layout_changes_exactly_on_the_deadline_are_still_skipped() {
        let now = Instant::now();
        let mut state = UpdatePosState::SuppressingLayoutChange { until: now };

        assert!(state.should_skip_layout_change(now));
        assert_eq!(
            state,
            UpdatePosState::SuppressingLayoutChange { until: now },
            "the window has not elapsed yet, so it stays armed"
        );
    }

    /// Past the deadline the first layout change is honoured *and* disarms
    /// the window, so a host that only ever reports position late (the
    /// `TS_E_NOLAYOUT` then `OnLayoutChange` sequence) is not starved.
    #[test]
    fn the_first_layout_change_past_the_deadline_is_honoured_and_rearms_idle() {
        let now = Instant::now();
        let mut state = UpdatePosState::SuppressingLayoutChange { until: now };

        assert!(!state.should_skip_layout_change(now + Duration::from_millis(1)));
        assert_eq!(state, UpdatePosState::Idle);
        assert!(
            !state.should_skip_layout_change(now + Duration::from_millis(2)),
            "and stays honoured afterwards"
        );
    }

    /// One full keystroke's worth of traffic, in the order the TIP produces
    /// it: begin → host fires a layout change → finish → the echo arrives
    /// and is swallowed → a genuine scroll later gets through.
    #[test]
    fn a_full_update_cycle_swallows_only_the_echo() {
        let now = Instant::now();
        let mut state = UpdatePosState::default();

        assert!(state.try_begin_update(now));
        assert!(state.should_skip_layout_change(now + Duration::from_millis(1)));
        state.finish_update(now + Duration::from_millis(2));
        assert!(state.should_skip_layout_change(now + Duration::from_millis(3)));

        assert!(!state.should_skip_layout_change(now + WINDOW + Duration::from_millis(1)));
    }
}
