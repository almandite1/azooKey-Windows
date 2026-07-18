//! A fake TSF host for unit tests.
//!
//! The TIP normally only runs inside a real host (Word, Chrome, Explorer),
//! which is why the COM-facing half of this crate has had no test coverage.
//! `TextServiceFactory::start_composition` and friends are `pub fn` and
//! `TextService::context` is a plain `Option<ITfContext>` field, so a fake
//! context implementing the same interfaces can be injected and the real
//! code driven against it.
//!
//! What this buys us over a real host: the fake can be told to *misbehave*
//! on demand (deny an edit session, defer it asynchronously) and it records
//! what it was asked to do, so the tests can assert on both the arguments
//! the TIP passes and the way it handles a hostile answer.

// new() returning the COM interface rather than Self is deliberate: the
// wrapped struct is consumed by .into() and only the interface is usable
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::new_ret_no_self)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use windows::{
    core::{implement, BSTR, IUnknown, Result as WinResult, GUID, PCWSTR, PWSTR, VARIANT},
    Win32::{
        Foundation::{BOOL, E_FAIL, E_NOTIMPL, HWND, LPARAM, RECT, S_OK, WPARAM},
        System::Com::IDataObject,
        UI::TextServices::{
            IEnumITfCompositionView, IEnumTfContextViews, IEnumTfDocumentMgrs,
            IEnumTfFunctionProviders, IEnumTfLangBarItems, IEnumTfProperties, IEnumTfRanges,
            ITfComposition, ITfCompositionSink, ITfCompositionView, ITfComposition_Impl,
            ITfCompartmentMgr, ITfContext, ITfContextComposition, ITfContextComposition_Impl,
            ITfContextView, ITfContext_Impl, ITfDocumentMgr, ITfEditSession, ITfFunctionProvider,
            ITfInsertAtSelection, ITfInsertAtSelection_Impl, ITfKeyEventSink, ITfKeystrokeMgr,
            ITfKeystrokeMgr_Impl, ITfLangBarItem, ITfLangBarItemMgr, ITfLangBarItemMgr_Impl,
            ITfLangBarItemSink, ITfProperty, ITfPropertyStore, ITfProperty_Impl, ITfRange,
            ITfRangeBackup, ITfRange_Impl, ITfReadOnlyProperty, ITfReadOnlyProperty_Impl, ITfSource,
            ITfSource_Impl, ITfThreadMgr, ITfThreadMgr_Impl, INSERT_TEXT_AT_SELECTION_FLAGS,
            TF_CONTEXT_EDIT_CONTEXT_FLAGS, TF_E_SYNCHRONOUS, TF_ES_SYNC, TF_HALTCOND,
            TF_LANGBARITEMINFO, TF_PRESERVEDKEY, TF_S_ASYNC, TS_STATUS,
        },
    },
};

/// The edit cookie the fake hands to `DoEditSession`. Any non-zero value
/// works; a real host's cookie is opaque to the TIP.
pub const FAKE_COOKIE: u32 = 0x1234;

/// How the fake host answers `RequestEditSession`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EditSessionBehavior {
    /// A cooperative host: run the session inline and report its result.
    RunSync,
    /// The request is accepted, but the session is *not* run and the failure
    /// is reported the way TSF actually reports it — through `phrSession`,
    /// while `RequestEditSession` itself still returns `S_OK`. This is what
    /// a disconnected or read-only context does.
    DenyViaSessionResult,
    /// The host defers the session: `TF_S_ASYNC` now, `DoEditSession` later
    /// (or never). Only possible because the TIP does not pass `TF_ES_SYNC`.
    Async,
    /// The request itself fails.
    Reject,
    /// A Notepad-like host that does not support synchronous edit sessions:
    /// a request carrying `TF_ES_SYNC` is rejected with `TF_E_SYNCHRONOUS`,
    /// while an ordinary (async-capable) request runs inline. This is the
    /// host behavior that B7 tripped over — forcing `TF_ES_SYNC` made every
    /// keystroke fail here. The TIP must keep requesting read/write only.
    RejectSyncRequests,
}

/// One `RequestEditSession` call, as the fake saw it.
#[derive(Clone, Copy, Debug)]
pub struct EditSessionRequest {
    pub tid: u32,
    pub flags: TF_CONTEXT_EDIT_CONTEXT_FLAGS,
}

#[implement(ITfContext, ITfContextComposition, ITfInsertAtSelection)]
pub struct FakeContext {
    behavior: Cell<EditSessionBehavior>,
    requests: RefCell<Vec<EditSessionRequest>>,
}

impl FakeContext {
    pub fn new(behavior: EditSessionBehavior) -> ITfContext {
        FakeContext {
            behavior: Cell::new(behavior),
            requests: RefCell::new(Vec::new()),
        }
        .into()
    }

    pub fn requests(&self) -> Vec<EditSessionRequest> {
        self.requests.borrow().clone()
    }
}

impl ITfContext_Impl for FakeContext_Impl {
    fn RequestEditSession(
        &self,
        tid: u32,
        pes: Option<&ITfEditSession>,
        dwflags: TF_CONTEXT_EDIT_CONTEXT_FLAGS,
    ) -> WinResult<windows::core::HRESULT> {
        self.requests.borrow_mut().push(EditSessionRequest {
            tid,
            flags: dwflags,
        });

        match self.behavior.get() {
            EditSessionBehavior::RunSync => {
                let session = pes.ok_or_else(|| windows::core::Error::from_hresult(E_FAIL))?;
                // phrSession carries the session's own result, which is
                // exactly what the TIP currently throws away
                Ok(match unsafe { session.DoEditSession(FAKE_COOKIE) } {
                    Ok(()) => S_OK,
                    Err(e) => e.code(),
                })
            }
            EditSessionBehavior::DenyViaSessionResult => Ok(E_FAIL),
            EditSessionBehavior::Async => Ok(TF_S_ASYNC),
            EditSessionBehavior::Reject => Err(windows::core::Error::from_hresult(E_FAIL)),
            EditSessionBehavior::RejectSyncRequests => {
                if (dwflags.0 & TF_ES_SYNC.0) != 0 {
                    Err(windows::core::Error::from_hresult(TF_E_SYNCHRONOUS))
                } else {
                    let session = pes.ok_or_else(|| windows::core::Error::from_hresult(E_FAIL))?;
                    Ok(match unsafe { session.DoEditSession(FAKE_COOKIE) } {
                        Ok(()) => S_OK,
                        Err(e) => e.code(),
                    })
                }
            }
        }
    }

    fn InWriteSession(&self, _tid: u32) -> WinResult<BOOL> {
        Ok(false.into())
    }

    fn GetSelection(
        &self,
        _ec: u32,
        _ulindex: u32,
        _ulcount: u32,
        _pselection: *mut windows::Win32::UI::TextServices::TF_SELECTION,
        _pcfetched: *mut u32,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn SetSelection(
        &self,
        _ec: u32,
        _ulcount: u32,
        _pselection: *const windows::Win32::UI::TextServices::TF_SELECTION,
    ) -> WinResult<()> {
        // [in]-only: the caller keeps ownership of the ranges it passed
        Ok(())
    }

    fn GetStart(&self, _ec: u32) -> WinResult<ITfRange> {
        Err(E_NOTIMPL.into())
    }

    fn GetEnd(&self, _ec: u32) -> WinResult<ITfRange> {
        Err(E_NOTIMPL.into())
    }

    fn GetActiveView(&self) -> WinResult<ITfContextView> {
        Err(E_NOTIMPL.into())
    }

    fn EnumViews(&self) -> WinResult<IEnumTfContextViews> {
        Err(E_NOTIMPL.into())
    }

    fn GetStatus(&self) -> WinResult<TS_STATUS> {
        Err(E_NOTIMPL.into())
    }

    fn GetProperty(&self, _guidprop: *const GUID) -> WinResult<ITfProperty> {
        Ok(FakeProperty.into())
    }

    fn GetAppProperty(&self, _guidprop: *const GUID) -> WinResult<ITfReadOnlyProperty> {
        Err(E_NOTIMPL.into())
    }

    fn TrackProperties(
        &self,
        _prgprop: *const *const GUID,
        _cprop: u32,
        _prgappprop: *const *const GUID,
        _cappprop: u32,
    ) -> WinResult<ITfReadOnlyProperty> {
        Err(E_NOTIMPL.into())
    }

    fn EnumProperties(&self) -> WinResult<IEnumTfProperties> {
        Err(E_NOTIMPL.into())
    }

    fn GetDocumentMgr(&self) -> WinResult<ITfDocumentMgr> {
        Err(E_NOTIMPL.into())
    }

    fn CreateRangeBackup(&self, _ec: u32, _prange: Option<&ITfRange>) -> WinResult<ITfRangeBackup> {
        Err(E_NOTIMPL.into())
    }
}

impl ITfContextComposition_Impl for FakeContext_Impl {
    fn StartComposition(
        &self,
        _ecwrite: u32,
        _pcompositionrange: Option<&ITfRange>,
        _psink: Option<&ITfCompositionSink>,
    ) -> WinResult<ITfComposition> {
        Ok(FakeComposition::new())
    }

    fn EnumCompositions(&self) -> WinResult<IEnumITfCompositionView> {
        Err(E_NOTIMPL.into())
    }

    fn FindComposition(
        &self,
        _ecread: u32,
        _ptestrange: Option<&ITfRange>,
    ) -> WinResult<IEnumITfCompositionView> {
        Err(E_NOTIMPL.into())
    }

    fn TakeOwnership(
        &self,
        _ecwrite: u32,
        _pcomposition: Option<&ITfCompositionView>,
        _psink: Option<&ITfCompositionSink>,
    ) -> WinResult<ITfComposition> {
        Err(E_NOTIMPL.into())
    }
}

impl ITfInsertAtSelection_Impl for FakeContext_Impl {
    fn InsertTextAtSelection(
        &self,
        _ec: u32,
        _dwflags: INSERT_TEXT_AT_SELECTION_FLAGS,
        _pchtext: &PCWSTR,
        _cch: i32,
    ) -> WinResult<ITfRange> {
        Err(E_NOTIMPL.into())
    }

    fn InsertEmbeddedAtSelection(
        &self,
        _ec: u32,
        _dwflags: u32,
        _pdataobject: Option<&IDataObject>,
    ) -> WinResult<ITfRange> {
        Err(E_NOTIMPL.into())
    }
}

/// Shared recorder for every [`FakeRange`] a test's composition hands out.
///
/// `live_ranges` counts range *objects* currently alive. windows-rs drops
/// the Rust struct exactly when the COM refcount reaches zero, so a leaked
/// AddRef (e.g. a `ManuallyDrop` clone that is never released) shows up
/// here as a count that never returns to zero.
#[derive(Default)]
pub struct RangeLog {
    pub shift_start_reqs: RefCell<Vec<i32>>,
    pub shift_end_reqs: RefCell<Vec<i32>>,
    live: Cell<isize>,
}

impl RangeLog {
    pub fn live_ranges(&self) -> isize {
        self.live.get()
    }
}

/// A recording stand-in for a text range. `ShiftStart`/`ShiftEnd` log the
/// requested cch (the unit-of-measure bug B9 asserts on) and report it as
/// fully shifted; text operations succeed and return nothing.
#[implement(ITfRange)]
pub struct FakeRange {
    log: Rc<RangeLog>,
}

impl FakeRange {
    pub fn new(log: Rc<RangeLog>) -> ITfRange {
        log.live.set(log.live.get() + 1);
        FakeRange { log }.into()
    }
}

impl Drop for FakeRange {
    fn drop(&mut self) {
        self.log.live.set(self.log.live.get() - 1);
    }
}

impl ITfRange_Impl for FakeRange_Impl {
    fn GetText(
        &self,
        _ec: u32,
        _dwflags: u32,
        _pchtext: PWSTR,
        _cchmax: u32,
        pcch: *mut u32,
    ) -> WinResult<()> {
        if !pcch.is_null() {
            unsafe { *pcch = 0 };
        }
        Ok(())
    }

    fn SetText(&self, _ec: u32, _dwflags: u32, _pchtext: &PCWSTR, _cch: i32) -> WinResult<()> {
        Ok(())
    }

    fn GetFormattedText(&self, _ec: u32) -> WinResult<IDataObject> {
        Err(E_NOTIMPL.into())
    }

    fn GetEmbedded(
        &self,
        _ec: u32,
        _rguidservice: *const GUID,
        _riid: *const GUID,
    ) -> WinResult<IUnknown> {
        Err(E_NOTIMPL.into())
    }

    fn InsertEmbedded(
        &self,
        _ec: u32,
        _dwflags: u32,
        _pdataobject: Option<&IDataObject>,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn ShiftStart(
        &self,
        _ec: u32,
        cchreq: i32,
        pcch: *mut i32,
        _phalt: *const TF_HALTCOND,
    ) -> WinResult<()> {
        self.log.shift_start_reqs.borrow_mut().push(cchreq);
        if !pcch.is_null() {
            unsafe { *pcch = cchreq };
        }
        Ok(())
    }

    fn ShiftEnd(
        &self,
        _ec: u32,
        cchreq: i32,
        pcch: *mut i32,
        _phalt: *const TF_HALTCOND,
    ) -> WinResult<()> {
        self.log.shift_end_reqs.borrow_mut().push(cchreq);
        if !pcch.is_null() {
            unsafe { *pcch = cchreq };
        }
        Ok(())
    }

    fn ShiftStartToRange(
        &self,
        _ec: u32,
        _prange: Option<&ITfRange>,
        _apos: windows::Win32::UI::TextServices::TfAnchor,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn ShiftEndToRange(
        &self,
        _ec: u32,
        _prange: Option<&ITfRange>,
        _apos: windows::Win32::UI::TextServices::TfAnchor,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn ShiftStartRegion(
        &self,
        _ec: u32,
        _dir: windows::Win32::UI::TextServices::TfShiftDir,
    ) -> WinResult<BOOL> {
        Err(E_NOTIMPL.into())
    }

    fn ShiftEndRegion(
        &self,
        _ec: u32,
        _dir: windows::Win32::UI::TextServices::TfShiftDir,
    ) -> WinResult<BOOL> {
        Err(E_NOTIMPL.into())
    }

    fn IsEmpty(&self, _ec: u32) -> WinResult<BOOL> {
        Ok(false.into())
    }

    fn Collapse(
        &self,
        _ec: u32,
        _apos: windows::Win32::UI::TextServices::TfAnchor,
    ) -> WinResult<()> {
        Ok(())
    }

    fn IsEqualStart(
        &self,
        _ec: u32,
        _pwith: Option<&ITfRange>,
        _apos: windows::Win32::UI::TextServices::TfAnchor,
    ) -> WinResult<BOOL> {
        Err(E_NOTIMPL.into())
    }

    fn IsEqualEnd(
        &self,
        _ec: u32,
        _pwith: Option<&ITfRange>,
        _apos: windows::Win32::UI::TextServices::TfAnchor,
    ) -> WinResult<BOOL> {
        Err(E_NOTIMPL.into())
    }

    fn CompareStart(
        &self,
        _ec: u32,
        _pwith: Option<&ITfRange>,
        _apos: windows::Win32::UI::TextServices::TfAnchor,
    ) -> WinResult<i32> {
        Err(E_NOTIMPL.into())
    }

    fn CompareEnd(
        &self,
        _ec: u32,
        _pwith: Option<&ITfRange>,
        _apos: windows::Win32::UI::TextServices::TfAnchor,
    ) -> WinResult<i32> {
        Err(E_NOTIMPL.into())
    }

    fn AdjustForInsert(&self, _ec: u32, _cchinsert: u32) -> WinResult<BOOL> {
        Err(E_NOTIMPL.into())
    }

    fn GetGravity(
        &self,
        _pgstart: *mut windows::Win32::UI::TextServices::TfGravity,
        _pgend: *mut windows::Win32::UI::TextServices::TfGravity,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn SetGravity(
        &self,
        _ec: u32,
        _gstart: windows::Win32::UI::TextServices::TfGravity,
        _gend: windows::Win32::UI::TextServices::TfGravity,
    ) -> WinResult<()> {
        Ok(())
    }

    fn Clone(&self) -> WinResult<ITfRange> {
        Ok(FakeRange::new(self.log.clone()))
    }

    fn GetContext(&self) -> WinResult<ITfContext> {
        Err(E_NOTIMPL.into())
    }
}

/// A property that accepts SetValue/Clear and refuses everything else —
/// enough for the display-attribute bookkeeping in the edit sessions.
#[implement(ITfProperty)]
pub struct FakeProperty;

impl ITfReadOnlyProperty_Impl for FakeProperty_Impl {
    fn GetType(&self) -> WinResult<GUID> {
        Ok(GUID::zeroed())
    }

    fn EnumRanges(
        &self,
        _ec: u32,
        _ppenum: *mut Option<IEnumTfRanges>,
        _ptargetrange: Option<&ITfRange>,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn GetValue(&self, _ec: u32, _prange: Option<&ITfRange>) -> WinResult<VARIANT> {
        Err(E_NOTIMPL.into())
    }

    fn GetContext(&self) -> WinResult<ITfContext> {
        Err(E_NOTIMPL.into())
    }
}

impl ITfProperty_Impl for FakeProperty_Impl {
    fn FindRange(
        &self,
        _ec: u32,
        _prange: Option<&ITfRange>,
        _pprange: *mut Option<ITfRange>,
        _apos: windows::Win32::UI::TextServices::TfAnchor,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn SetValueStore(
        &self,
        _ec: u32,
        _prange: Option<&ITfRange>,
        _ppropstore: Option<&ITfPropertyStore>,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn SetValue(
        &self,
        _ec: u32,
        _prange: Option<&ITfRange>,
        _pvarvalue: *const VARIANT,
    ) -> WinResult<()> {
        Ok(())
    }

    fn Clear(&self, _ec: u32, _prange: Option<&ITfRange>) -> WinResult<()> {
        Ok(())
    }
}

/// A stand-in for a live composition. Hands out [`FakeRange`]s that all
/// report into the same [`RangeLog`].
#[implement(ITfComposition)]
pub struct FakeComposition {
    log: Rc<RangeLog>,
}

impl FakeComposition {
    pub fn new() -> ITfComposition {
        Self::with_log(Rc::new(RangeLog::default()))
    }

    pub fn with_log(log: Rc<RangeLog>) -> ITfComposition {
        FakeComposition { log }.into()
    }
}

impl ITfComposition_Impl for FakeComposition_Impl {
    fn GetRange(&self) -> WinResult<ITfRange> {
        Ok(FakeRange::new(self.log.clone()))
    }

    fn ShiftStart(&self, _ecwrite: u32, _pnewstart: Option<&ITfRange>) -> WinResult<()> {
        Ok(())
    }

    fn ShiftEnd(&self, _ecwrite: u32, _pnewend: Option<&ITfRange>) -> WinResult<()> {
        Ok(())
    }

    fn EndComposition(&self, _ecwrite: u32) -> WinResult<()> {
        Ok(())
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
    next_cookie: Cell<u32>,
}

/// A fake `ITfThreadMgr` that answers just enough of TSF for the TIP's
/// `Activate`/`Deactivate` to run end to end in a unit test, and records the
/// advise/unadvise traffic. `GetFocus` deliberately reports no focus so the
/// text-layout-sink path (which needs a real document manager) is skipped.
#[implement(ITfThreadMgr, ITfKeystrokeMgr, ITfSource, ITfLangBarItemMgr)]
pub struct FakeThreadMgr {
    log: Rc<ThreadMgrLog>,
}

impl FakeThreadMgr {
    pub fn new(log: Rc<ThreadMgrLog>) -> ITfThreadMgr {
        FakeThreadMgr { log }.into()
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
        // No focus: the TIP's `if let Ok(doc_mgr)` skips advising the text
        // layout sink, which would otherwise need a real document manager.
        Err(E_FAIL.into())
    }
    fn SetFocus(&self, _pdimfocus: Option<&ITfDocumentMgr>) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
    fn AssociateFocus(
        &self,
        _hwnd: HWND,
        _pdimnew: Option<&ITfDocumentMgr>,
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
        psink: Option<&ITfKeyEventSink>,
        _fforeground: BOOL,
    ) -> WinResult<()> {
        self.log.key_sink_advises.borrow_mut().push(psink.is_some());
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
        _pic: Option<&ITfContext>,
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
        _rguid: *const GUID,
        _prekey: *const TF_PRESERVEDKEY,
        _pchdesc: &PCWSTR,
        _cchdesc: u32,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }
    fn UnpreserveKey(
        &self,
        _rguid: *const GUID,
        _pprekey: *const TF_PRESERVEDKEY,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
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
        _pic: Option<&ITfContext>,
        _rguid: *const GUID,
    ) -> WinResult<BOOL> {
        Err(E_NOTIMPL.into())
    }
}

impl ITfSource_Impl for FakeThreadMgr_Impl {
    fn AdviseSink(&self, _riid: *const GUID, punk: Option<&IUnknown>) -> WinResult<u32> {
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
    fn AddItem(&self, _punk: Option<&ITfLangBarItem>) -> WinResult<()> {
        self.log.langbar_adds.set(self.log.langbar_adds.get() + 1);
        Ok(())
    }
    fn RemoveItem(&self, _punk: Option<&ITfLangBarItem>) -> WinResult<()> {
        self.log
            .langbar_removes
            .set(self.log.langbar_removes.get() + 1);
        Ok(())
    }
    fn AdviseItemSink(
        &self,
        _punk: Option<&ITfLangBarItemSink>,
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
        _ppitem: *mut Option<ITfLangBarItem>,
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

/// Casts an `ITfContext` produced by [`FakeContext::new`] back to its Rust
/// implementation so a test can read what it recorded.
///
/// # Safety
/// The context must have come from [`FakeContext::new`].
pub unsafe fn fake_context_of(context: &ITfContext) -> &FakeContext {
    use windows::core::AsImpl as _;
    context.as_impl()
}

/// Builds a `TextServiceFactory` wired to a fake context, the way
/// `TextServiceFactory::create` wires a real one.
pub fn factory_with_fake_context(
    behavior: EditSessionBehavior,
) -> (
    windows::Win32::UI::TextServices::ITfTextInputProcessor,
    ITfContext,
) {
    use windows::core::AsImpl as _;
    use windows::Win32::UI::TextServices::ITfTextInputProcessor;

    use super::factory::TextServiceFactory;

    let tip = TextServiceFactory::create::<ITfTextInputProcessor>()
        .expect("failed to create the factory");
    let context = FakeContext::new(behavior);

    {
        let factory = unsafe { tip.as_impl() };
        let mut text_service = factory.borrow_mut().expect("factory is already borrowed");
        text_service.tid = 1;
        text_service.context = Some(context.clone());
    }

    (tip, context)
}
