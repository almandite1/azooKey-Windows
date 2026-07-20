use std::{
    cell::{Ref, RefCell, RefMut},
    collections::HashMap,
    time::{Duration, Instant},
};

use windows::{
    core::{Interface, GUID},
    Win32::UI::TextServices::{ITfCompartment, ITfContext, ITfThreadMgr},
};

use anyhow::{Context, Result};

use crate::engine::{composition::Composition, input_mode::InputMode};

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
    /// Set while we are writing our own mode into the compartments, so the
    /// `OnChange` that TSF dispatches synchronously from inside `SetValue`
    /// does not bounce straight back into another write.
    pub suppress_compartment_echo: bool,
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
