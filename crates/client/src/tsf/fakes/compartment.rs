//! The compartments the OS uses to publish the IME's on/off and conversion
//! state, and the sinks advised on them.
//!
//! The interesting part is that `SetValue` dispatches `OnChange`
//! **synchronously**, on the calling thread, exactly as TSF does. That is
//! what makes a `RefCell` borrow held across a compartment write show up as a
//! `BorrowMutError` in the callback rather than in production.

// new() returning the COM interface rather than Self is deliberate: the
// wrapped struct is consumed by .into() and only the interface is usable
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::new_ret_no_self)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use windows::{
    Win32::{
        Foundation::E_FAIL,
        System::Variant::VARIANT,
        UI::TextServices::{
            ITfCompartment, ITfCompartment_Impl, ITfCompartmentEventSink, ITfSource, ITfSource_Impl,
        },
    },
    core::{GUID, Result as WinResult, implement},
};

/// Shared state behind every [`FakeCompartment`] a [`FakeThreadMgr`] hands
/// out: the compartment values, and the sinks advised on them.
///
/// The interesting part is that `SetValue` dispatches `OnChange`
/// **synchronously**, on the calling thread, exactly as TSF does. That is
/// what makes a `RefCell` borrow held across a compartment write show up as
/// a `BorrowMutError` in the callback rather than in production.
#[derive(Default)]
pub struct CompartmentLog {
    values: RefCell<std::collections::HashMap<GUID, i32>>,
    sinks: RefCell<Vec<(u32, GUID, ITfCompartmentEventSink)>>,
    next_cookie: Cell<u32>,
    pub set_value_calls: Cell<usize>,
    pub advise_count: Cell<usize>,
    pub unadvise_cookies: RefCell<Vec<u32>>,
}

impl CompartmentLog {
    /// Seeds a compartment as if another IME had already set it, so a test
    /// can prove Activate adopts the existing mode instead of stamping over
    /// it.
    pub fn preset(&self, guid: GUID, value: i32) {
        self.values.borrow_mut().insert(guid, value);
    }

    pub fn value(&self, guid: GUID) -> Option<i32> {
        self.values.borrow().get(&guid).copied()
    }

    pub fn live_sinks(&self) -> usize {
        self.sinks.borrow().len()
    }

    /// Simulates somebody *else* writing the compartment — the touch
    /// keyboard, the shell, or an IMM32 app — and dispatches `OnChange` the
    /// way TSF does: synchronously, on this thread.
    pub fn external_set(&self, guid: GUID, value: i32) {
        self.values.borrow_mut().insert(guid, value);

        let sinks: Vec<ITfCompartmentEventSink> = self
            .sinks
            .borrow()
            .iter()
            .filter(|(_, g, _)| *g == guid)
            .map(|(_, _, sink)| sink.clone())
            .collect();

        for sink in sinks {
            let _ = unsafe { sink.OnChange(&guid) };
        }
    }
}

/// One compartment. `GetValue` reports `VT_EMPTY` until something writes,
/// matching a real unset compartment.
#[implement(ITfCompartment, ITfSource)]
pub struct FakeCompartment {
    // pub(super) so the two managers that hand these out — the context and
    // the thread manager — can construct one; nothing outside `fakes` does.
    pub(super) guid: GUID,
    pub(super) log: Rc<CompartmentLog>,
}

impl ITfCompartment_Impl for FakeCompartment_Impl {
    fn SetValue(&self, _tid: u32, pvarvalue: *const VARIANT) -> WinResult<()> {
        self.log
            .set_value_calls
            .set(self.log.set_value_calls.get() + 1);

        let value = unsafe { pvarvalue.as_ref() }
            .and_then(|v| i32::try_from(v).ok())
            .unwrap_or(0);
        self.log.values.borrow_mut().insert(self.guid, value);

        // Synchronous dispatch, like TSF. Clone the sink list first so the
        // callback may advise/unadvise without a borrow conflict here.
        let sinks: Vec<ITfCompartmentEventSink> = self
            .log
            .sinks
            .borrow()
            .iter()
            .filter(|(_, guid, _)| *guid == self.guid)
            .map(|(_, _, sink)| sink.clone())
            .collect();

        for sink in sinks {
            unsafe { sink.OnChange(&self.guid)? };
        }

        Ok(())
    }

    fn GetValue(&self) -> WinResult<VARIANT> {
        match self.log.values.borrow().get(&self.guid) {
            Some(value) => Ok(VARIANT::from(*value)),
            // unset compartment: VT_EMPTY
            None => Ok(VARIANT::default()),
        }
    }
}

impl ITfSource_Impl for FakeCompartment_Impl {
    fn AdviseSink(
        &self,
        _riid: *const GUID,
        punk: windows_core::Ref<'_, windows_core::IUnknown>,
    ) -> WinResult<u32> {
        let punk = punk
            .as_ref()
            .ok_or_else(|| windows::core::Error::from_hresult(E_FAIL))?;
        let sink: ITfCompartmentEventSink = windows::core::Interface::cast(punk)?;

        let cookie = self.log.next_cookie.get() + 1;
        self.log.next_cookie.set(cookie);
        self.log.advise_count.set(self.log.advise_count.get() + 1);
        self.log.sinks.borrow_mut().push((cookie, self.guid, sink));
        Ok(cookie)
    }

    fn UnadviseSink(&self, dwcookie: u32) -> WinResult<()> {
        self.log.unadvise_cookies.borrow_mut().push(dwcookie);
        self.log
            .sinks
            .borrow_mut()
            .retain(|(c, _, _)| *c != dwcookie);
        Ok(())
    }
}
