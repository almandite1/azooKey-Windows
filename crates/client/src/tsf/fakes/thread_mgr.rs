//! The thread-scoped half of the fake host: the thread manager the TIP
//! activates against, plus the langbar, keystroke, UI-element and compartment
//! managers it reaches through it.
//!
//! What it is for is the advise/unadvise ledger: a test can prove the sinks
//! the TIP advised on `Activate` are exactly the ones it unadvises on
//! `Deactivate` — no leak, no double-advise.

// new() returning the COM interface rather than Self is deliberate: the
// wrapped struct is consumed by .into() and only the interface is usable
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::new_ret_no_self)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use windows::{
    Win32::{
        Foundation::{E_FAIL, E_NOTIMPL, HWND, LPARAM, RECT, WPARAM},
        UI::TextServices::{
            IEnumTfDocumentMgrs, IEnumTfFunctionProviders, IEnumTfLangBarItems, IEnumTfUIElements,
            ITfCompartment, ITfCompartmentMgr, ITfCompartmentMgr_Impl, ITfContext, ITfDocumentMgr,
            ITfFunctionProvider, ITfKeyEventSink, ITfKeystrokeMgr, ITfKeystrokeMgr_Impl,
            ITfLangBarItem, ITfLangBarItemMgr, ITfLangBarItemMgr_Impl, ITfLangBarItemSink,
            ITfSource, ITfSource_Impl, ITfThreadMgr, ITfThreadMgr_Impl, ITfUIElement,
            ITfUIElementMgr, ITfUIElementMgr_Impl, TF_LANGBARITEMINFO, TF_PRESERVEDKEY,
        },
    },
    core::{BOOL, BSTR, GUID, OutRef, PCWSTR, Result as WinResult, implement},
};

use super::compartment::{CompartmentLog, FakeCompartment};
use super::context::FakeDocumentMgr;

/// What a fake host's `ITfUIElementMgr` recorded, and how it answers.
///
/// `show` is the `pbShow` the host returns from `BeginUIElement` — the only
/// gate that decides whether the TIP may draw its own candidate window.
pub struct UiElementLog {
    /// The host's answer to "may the TIP show its own UI?".
    pub show: Cell<bool>,
    pub begin_calls: Cell<usize>,
    pub update_calls: Cell<usize>,
    pub end_calls: Cell<usize>,
    /// Ids handed out by `BeginUIElement`, and those passed to `EndUIElement`.
    pub begun_ids: RefCell<Vec<u32>>,
    pub ended_ids: RefCell<Vec<u32>>,
    next_id: Cell<u32>,
}

impl Default for UiElementLog {
    fn default() -> Self {
        Self {
            // a host that has no opinion lets the TIP draw
            show: Cell::new(true),
            begin_calls: Cell::new(0),
            update_calls: Cell::new(0),
            end_calls: Cell::new(0),
            begun_ids: RefCell::new(Vec::new()),
            ended_ids: RefCell::new(Vec::new()),
            next_id: Cell::new(0x4100),
        }
    }
}

impl UiElementLog {
    /// A host that draws the candidates itself (a UILess thread), so the TIP
    /// must keep its own window hidden.
    pub fn suppressing() -> Self {
        let log = Self::default();
        log.show.set(false);
        log
    }

    pub fn live_elements(&self) -> usize {
        self.begun_ids.borrow().len() - self.ended_ids.borrow().len()
    }
}

/// What a [`FakeThreadMgr`] recorded while a TIP activated and deactivated
/// against it. Lets a test prove the sinks the TIP advised on Activate are
/// exactly the ones it unadvises on Deactivate — no leak, no double-advise.
#[derive(Default)]
pub struct ThreadMgrLog {
    /// One entry per `AdviseKeyEventSink`; `true` means a live sink was
    /// passed. A `false` (or a missing entry) is the B15 regression: with
    /// `this` cleared, the TIP cannot produce its key sink and Activate dies
    /// before it ever reaches this call.
    pub key_sink_advises: RefCell<Vec<bool>>,
    pub key_sink_unadvises: Cell<u32>,
    /// Cookies handed out from `ITfSource::AdviseSink` (the thread-manager
    /// event sink), and the cookies later passed back to `UnadviseSink`.
    pub advise_cookies: RefCell<Vec<u32>>,
    pub unadvise_cookies: RefCell<Vec<u32>>,
    pub langbar_adds: Cell<u32>,
    pub langbar_removes: Cell<u32>,
    /// `PreserveKey`/`UnpreserveKey` traffic, in call order — the balance a
    /// test asserts on (issue #19).
    pub preserved_keys: RefCell<Vec<(GUID, TF_PRESERVEDKEY)>>,
    pub unpreserved_keys: RefCell<Vec<(GUID, TF_PRESERVEDKEY)>>,
    /// Whether the TIP could take a borrow of its own `TextService` during
    /// the synchronous `OnSetFocus` that `AdviseKeyEventSink` dispatched.
    /// `None` when the fake never dispatched one (the default host).
    ///
    /// This is the invariant issue #62 is about: real TSF calls back into the
    /// sink from *inside* `AdviseKeyEventSink`, so Activate must hold no
    /// borrow at that point.
    pub borrow_ok_in_sync_focus: Cell<Option<bool>>,
    next_cookie: Cell<u32>,
}

impl ThreadMgrLog {
    /// A log whose cookies are handed out starting above `base`, so two fake
    /// thread managers' cookies are distinguishable in cross-instance tests
    /// (e.g. proving one TIP's Deactivate doesn't unadvise with another's
    /// cookie).
    pub fn with_cookie_base(base: u32) -> Self {
        Self {
            next_cookie: Cell::new(base),
            ..Self::default()
        }
    }
}

/// A fake `ITfThreadMgr` that answers just enough of TSF for the TIP's
/// `Activate`/`Deactivate` to run end to end in a unit test, and records the
/// advise/unadvise traffic. Without a focus context, `GetFocus` reports no
/// focus and the text-layout-sink path is skipped; `with_focus` makes it
/// serve a [`FakeDocumentMgr`] over the given context so that path runs too.
#[implement(
    ITfThreadMgr,
    ITfKeystrokeMgr,
    ITfSource,
    ITfLangBarItemMgr,
    ITfCompartmentMgr,
    ITfUIElementMgr
)]
pub struct FakeThreadMgr {
    log: Rc<ThreadMgrLog>,
    config: FakeThreadMgrConfig,
}

/// How a [`FakeThreadMgr`] misbehaves. A struct rather than a growing list of
/// `build` parameters: every constructor below overrides one or two fields and
/// takes the defaults (a cooperative host) for the rest.
#[derive(Default)]
pub struct FakeThreadMgrConfig {
    /// When set, `GetFocus` serves a [`FakeDocumentMgr`] over this context so
    /// the text-layout-sink path runs; otherwise it reports no focus.
    focus_context: Option<ITfContext>,
    /// When true, `AddItem` fails — models a host where the last Activate
    /// step (adding the language-bar item) fails, so a test can prove the
    /// TIP keeps the key sink it advised earlier.
    fail_add_item: bool,
    /// When true, `PreserveKey` fails — a host that will not hand over the
    /// IME on/off keys, where the raw-VK path must still work (issue #19).
    fail_preserve_key: bool,
    /// When true, `AdviseKeyEventSink` calls the sink's `OnSetFocus(TRUE)`
    /// back synchronously, before returning — which is what TSF does when the
    /// thread already owns the focus at Activate time (issue #62).
    focus_on_advise: bool,
    compartments: Rc<CompartmentLog>,
    ui_elements: Rc<UiElementLog>,
}

impl FakeThreadMgr {
    pub fn new(log: Rc<ThreadMgrLog>) -> ITfThreadMgr {
        Self::build(log, Default::default())
    }

    pub fn with_focus(log: Rc<ThreadMgrLog>, focus_context: ITfContext) -> ITfThreadMgr {
        Self::build(
            log,
            FakeThreadMgrConfig {
                focus_context: Some(focus_context),
                ..Default::default()
            },
        )
    }

    /// A thread manager that re-enters the TIP the way TSF does: the sink's
    /// `OnSetFocus(TRUE)` fires synchronously from inside
    /// `AdviseKeyEventSink`. Shares `compartments` so the callback has a mode
    /// to read.
    pub fn with_sync_focus_on_advise(
        log: Rc<ThreadMgrLog>,
        compartments: Rc<CompartmentLog>,
    ) -> ITfThreadMgr {
        Self::build(
            log,
            FakeThreadMgrConfig {
                focus_on_advise: true,
                compartments,
                ..Default::default()
            },
        )
    }

    /// A thread manager whose `AddItem` (the final Activate step) fails, so
    /// the TIP's Activate must roll back the key-event and thread-manager
    /// sinks it advised earlier.
    pub fn with_failing_langbar(log: Rc<ThreadMgrLog>) -> ITfThreadMgr {
        Self::build(
            log,
            FakeThreadMgrConfig {
                fail_add_item: true,
                ..Default::default()
            },
        )
    }

    /// A thread manager that refuses every `PreserveKey`, so a test can prove
    /// the raw-VK toggle survives as the fallback (issue #19).
    pub fn with_failing_preserve_key(log: Rc<ThreadMgrLog>) -> ITfThreadMgr {
        Self::build(
            log,
            FakeThreadMgrConfig {
                fail_preserve_key: true,
                ..Default::default()
            },
        )
    }

    /// A thread manager sharing `compartments`, so a test can seed the
    /// open/close state before Activate and inspect it afterwards.
    pub fn with_compartments(
        log: Rc<ThreadMgrLog>,
        compartments: Rc<CompartmentLog>,
    ) -> ITfThreadMgr {
        Self::build(
            log,
            FakeThreadMgrConfig {
                compartments,
                ..Default::default()
            },
        )
    }

    /// A thread manager whose `ITfUIElementMgr` the test controls — set
    /// `show` on `ui_elements` to model a UILess host that draws the
    /// candidates itself.
    pub fn with_ui_elements(log: Rc<ThreadMgrLog>, ui_elements: Rc<UiElementLog>) -> ITfThreadMgr {
        Self::build(
            log,
            FakeThreadMgrConfig {
                ui_elements,
                ..Default::default()
            },
        )
    }

    fn build(log: Rc<ThreadMgrLog>, config: FakeThreadMgrConfig) -> ITfThreadMgr {
        FakeThreadMgr { log, config }.into()
    }
}

impl ITfCompartmentMgr_Impl for FakeThreadMgr_Impl {
    fn GetCompartment(&self, rguid: *const GUID) -> WinResult<ITfCompartment> {
        let guid = unsafe { rguid.as_ref() }.ok_or_else(|| {
            windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG)
        })?;

        Ok(FakeCompartment {
            guid: *guid,
            log: self.config.compartments.clone(),
        }
        .into())
    }

    fn ClearCompartment(&self, _tid: u32, _rguid: *const GUID) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn EnumCompartments(&self) -> WinResult<windows::Win32::System::Com::IEnumGUID> {
        Err(E_NOTIMPL.into())
    }
}

impl ITfUIElementMgr_Impl for FakeThreadMgr_Impl {
    fn BeginUIElement(
        &self,
        _pelement: windows_core::Ref<'_, ITfUIElement>,
        pbshow: *mut BOOL,
        pdwuielementid: *mut u32,
    ) -> WinResult<()> {
        let log = &self.config.ui_elements;
        log.begin_calls.set(log.begin_calls.get() + 1);

        let id = log.next_id.get() + 1;
        log.next_id.set(id);
        log.begun_ids.borrow_mut().push(id);

        unsafe {
            if !pbshow.is_null() {
                *pbshow = log.show.get().into();
            }
            if !pdwuielementid.is_null() {
                *pdwuielementid = id;
            }
        }
        Ok(())
    }

    fn UpdateUIElement(&self, _dwuielementid: u32) -> WinResult<()> {
        let log = &self.config.ui_elements;
        log.update_calls.set(log.update_calls.get() + 1);
        Ok(())
    }

    fn EndUIElement(&self, dwuielementid: u32) -> WinResult<()> {
        let log = &self.config.ui_elements;
        log.end_calls.set(log.end_calls.get() + 1);
        log.ended_ids.borrow_mut().push(dwuielementid);
        Ok(())
    }

    fn GetUIElement(&self, _dwuielementid: u32) -> WinResult<ITfUIElement> {
        Err(E_NOTIMPL.into())
    }

    fn EnumUIElements(&self) -> WinResult<IEnumTfUIElements> {
        Err(E_NOTIMPL.into())
    }
}

impl ITfThreadMgr_Impl for FakeThreadMgr_Impl {
    fn Activate(&self) -> WinResult<u32> {
        Err(E_NOTIMPL.into())
    }
    fn Deactivate(&self) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
    fn CreateDocumentMgr(&self) -> WinResult<ITfDocumentMgr> {
        Err(E_NOTIMPL.into())
    }
    fn EnumDocumentMgrs(&self) -> WinResult<IEnumTfDocumentMgrs> {
        Err(E_NOTIMPL.into())
    }
    fn GetFocus(&self) -> WinResult<ITfDocumentMgr> {
        match &self.config.focus_context {
            Some(context) => Ok(FakeDocumentMgr::new(context.clone())),
            // No focus: the TIP's `if let Ok(doc_mgr)` skips advising the
            // text layout sink, exactly like a host with no focused document.
            None => Err(E_FAIL.into()),
        }
    }
    fn SetFocus(&self, _pdimfocus: windows_core::Ref<'_, ITfDocumentMgr>) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
    fn AssociateFocus(
        &self,
        _hwnd: HWND,
        _pdimnew: windows_core::Ref<'_, ITfDocumentMgr>,
    ) -> WinResult<ITfDocumentMgr> {
        Err(E_NOTIMPL.into())
    }
    fn IsThreadFocus(&self) -> WinResult<BOOL> {
        Err(E_NOTIMPL.into())
    }
    fn GetFunctionProvider(&self, _clsid: *const GUID) -> WinResult<ITfFunctionProvider> {
        Err(E_NOTIMPL.into())
    }
    fn EnumFunctionProviders(&self) -> WinResult<IEnumTfFunctionProviders> {
        Err(E_NOTIMPL.into())
    }
    fn GetGlobalCompartment(&self) -> WinResult<ITfCompartmentMgr> {
        Err(E_NOTIMPL.into())
    }
}

impl ITfKeystrokeMgr_Impl for FakeThreadMgr_Impl {
    fn AdviseKeyEventSink(
        &self,
        _tid: u32,
        psink: windows_core::Ref<'_, ITfKeyEventSink>,
        _fforeground: BOOL,
    ) -> WinResult<()> {
        self.log.key_sink_advises.borrow_mut().push(psink.is_some());

        if let (true, Some(sink)) = (self.config.focus_on_advise, psink.as_ref()) {
            // TSF hands the focus to a freshly advised sink from *inside*
            // AdviseKeyEventSink. Record whether the TIP's own RefCell is
            // free at that instant before letting the callback run: that is
            // the invariant, and OnSetFocus itself swallows the failure into
            // a tracing::warn! (advisory path), so nothing else would show it
            // (issue #62).
            if let Ok(factory) =
                windows::core::ComObject::<crate::tsf::factory::TextServiceFactory>::cast_from(sink)
            {
                self.log
                    .borrow_ok_in_sync_focus
                    .set(Some(factory.borrow().is_ok()));
            }
            unsafe { sink.OnSetFocus(true)? };
        }

        Ok(())
    }
    fn UnadviseKeyEventSink(&self, _tid: u32) -> WinResult<()> {
        self.log
            .key_sink_unadvises
            .set(self.log.key_sink_unadvises.get() + 1);
        Ok(())
    }
    fn GetForeground(&self) -> WinResult<GUID> {
        Err(E_NOTIMPL.into())
    }
    fn TestKeyDown(&self, _wparam: WPARAM, _lparam: LPARAM) -> WinResult<BOOL> {
        Err(E_NOTIMPL.into())
    }
    fn TestKeyUp(&self, _wparam: WPARAM, _lparam: LPARAM) -> WinResult<BOOL> {
        Err(E_NOTIMPL.into())
    }
    fn KeyDown(&self, _wparam: WPARAM, _lparam: LPARAM) -> WinResult<BOOL> {
        Err(E_NOTIMPL.into())
    }
    fn KeyUp(&self, _wparam: WPARAM, _lparam: LPARAM) -> WinResult<BOOL> {
        Err(E_NOTIMPL.into())
    }
    fn GetPreservedKey(
        &self,
        _pic: windows_core::Ref<'_, ITfContext>,
        _pprekey: *const TF_PRESERVEDKEY,
    ) -> WinResult<GUID> {
        Err(E_NOTIMPL.into())
    }
    fn IsPreservedKey(
        &self,
        _rguid: *const GUID,
        _pprekey: *const TF_PRESERVEDKEY,
    ) -> WinResult<BOOL> {
        Err(E_NOTIMPL.into())
    }
    fn PreserveKey(
        &self,
        _tid: u32,
        rguid: *const GUID,
        prekey: *const TF_PRESERVEDKEY,
        _pchdesc: &PCWSTR,
        _cchdesc: u32,
    ) -> WinResult<()> {
        if self.config.fail_preserve_key {
            return Err(E_FAIL.into());
        }
        let (Some(guid), Some(key)) = (unsafe { rguid.as_ref() }, unsafe { prekey.as_ref() })
        else {
            return Err(windows::Win32::Foundation::E_INVALIDARG.into());
        };
        self.log.preserved_keys.borrow_mut().push((*guid, *key));
        Ok(())
    }
    fn UnpreserveKey(&self, rguid: *const GUID, pprekey: *const TF_PRESERVEDKEY) -> WinResult<()> {
        let (Some(guid), Some(key)) = (unsafe { rguid.as_ref() }, unsafe { pprekey.as_ref() })
        else {
            return Err(windows::Win32::Foundation::E_INVALIDARG.into());
        };
        self.log.unpreserved_keys.borrow_mut().push((*guid, *key));
        Ok(())
    }
    fn SetPreservedKeyDescription(
        &self,
        _rguid: *const GUID,
        _pchdesc: &PCWSTR,
        _cchdesc: u32,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetPreservedKeyDescription(&self, _rguid: *const GUID) -> WinResult<BSTR> {
        Err(E_NOTIMPL.into())
    }
    fn SimulatePreservedKey(
        &self,
        _pic: windows_core::Ref<'_, ITfContext>,
        _rguid: *const GUID,
    ) -> WinResult<BOOL> {
        Err(E_NOTIMPL.into())
    }
}

impl ITfSource_Impl for FakeThreadMgr_Impl {
    fn AdviseSink(
        &self,
        _riid: *const GUID,
        punk: windows_core::Ref<'_, windows_core::IUnknown>,
    ) -> WinResult<u32> {
        if punk.is_none() {
            return Err(E_FAIL.into());
        }
        let cookie = self.log.next_cookie.get() + 1;
        self.log.next_cookie.set(cookie);
        self.log.advise_cookies.borrow_mut().push(cookie);
        Ok(cookie)
    }
    fn UnadviseSink(&self, dwcookie: u32) -> WinResult<()> {
        self.log.unadvise_cookies.borrow_mut().push(dwcookie);
        Ok(())
    }
}

impl ITfLangBarItemMgr_Impl for FakeThreadMgr_Impl {
    fn EnumItems(&self) -> WinResult<IEnumTfLangBarItems> {
        Err(E_NOTIMPL.into())
    }
    fn GetItem(&self, _rguid: *const GUID) -> WinResult<ITfLangBarItem> {
        Err(E_NOTIMPL.into())
    }
    fn AddItem(&self, _punk: windows_core::Ref<'_, ITfLangBarItem>) -> WinResult<()> {
        self.log.langbar_adds.set(self.log.langbar_adds.get() + 1);
        if self.config.fail_add_item {
            return Err(E_FAIL.into());
        }
        Ok(())
    }
    fn RemoveItem(&self, _punk: windows_core::Ref<'_, ITfLangBarItem>) -> WinResult<()> {
        self.log
            .langbar_removes
            .set(self.log.langbar_removes.get() + 1);
        Ok(())
    }
    fn AdviseItemSink(
        &self,
        _punk: windows_core::Ref<'_, ITfLangBarItemSink>,
        _pdwcookie: *mut u32,
        _rguiditem: *const GUID,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
    fn UnadviseItemSink(&self, _dwcookie: u32) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetItemFloatingRect(&self, _dwthreadid: u32, _rguid: *const GUID) -> WinResult<RECT> {
        Err(E_NOTIMPL.into())
    }
    fn GetItemsStatus(
        &self,
        _ulcount: u32,
        _prgguid: *const GUID,
        _pdwstatus: *mut u32,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
    fn GetItemNum(&self) -> WinResult<u32> {
        Ok(0)
    }
    fn GetItems(
        &self,
        _ulcount: u32,
        _ppitem: OutRef<'_, ITfLangBarItem>,
        _pinfo: *mut TF_LANGBARITEMINFO,
        _pdwstatus: *mut u32,
        _pcfetched: *mut u32,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
    fn AdviseItemsSink(
        &self,
        _ulcount: u32,
        _ppunk: *const Option<ITfLangBarItemSink>,
        _pguiditem: *const GUID,
        _pdwcookie: *mut u32,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
    fn UnadviseItemsSink(&self, _ulcount: u32, _pdwcookie: *const u32) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
}
