//! The fake document side of the host: a context, its view, and the document
//! manager that owns it.
//!
//! This is where a hostile host is simulated. [`EditSessionBehavior`] decides
//! whether `RequestEditSession` runs the session synchronously, defers it, or
//! refuses outright, and [`TextExtBehavior`] decides whether the view reports
//! a rect, a pending layout or a clipped one — the three answers `update_pos`
//! has to tell apart.

// new() returning the COM interface rather than Self is deliberate: the
// wrapped struct is consumed by .into() and only the interface is usable
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::new_ret_no_self)]

use std::cell::{Cell, RefCell};
use std::mem::ManuallyDrop;
use std::rc::Rc;

use windows::{
    Win32::{
        Foundation::{E_FAIL, E_NOTIMPL, HWND, POINT, RECT, S_OK},
        System::Com::IDataObject,
        UI::TextServices::{
            IEnumITfCompositionView, IEnumTfContextViews, IEnumTfContexts, IEnumTfProperties,
            INSERT_TEXT_AT_SELECTION_FLAGS, ITfCompartment, ITfCompartmentMgr,
            ITfCompartmentMgr_Impl, ITfComposition, ITfCompositionSink, ITfCompositionView,
            ITfContext, ITfContext_Impl, ITfContextComposition, ITfContextComposition_Impl,
            ITfContextView, ITfContextView_Impl, ITfDocumentMgr, ITfDocumentMgr_Impl,
            ITfEditSession, ITfInsertAtSelection, ITfInsertAtSelection_Impl, ITfProperty, ITfRange,
            ITfRangeBackup, ITfReadOnlyProperty, ITfSource, ITfSource_Impl,
            TF_CONTEXT_EDIT_CONTEXT_FLAGS, TF_E_SYNCHRONOUS, TF_ES_SYNC, TF_S_ASYNC, TF_SELECTION,
            TS_E_NOLAYOUT, TS_STATUS,
        },
    },
    core::{BOOL, GUID, OutRef, PCWSTR, Result as WinResult, implement},
};

use super::super::test_support::FAKE_COOKIE;
use super::compartment::{CompartmentLog, FakeCompartment};
use super::range::{FakeComposition, FakeProperty, FakeRange, RangeLog};

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

#[implement(
    ITfContext,
    ITfContextComposition,
    ITfInsertAtSelection,
    ITfSource,
    ITfCompartmentMgr
)]
pub struct FakeContext {
    behavior: Cell<EditSessionBehavior>,
    requests: RefCell<Vec<EditSessionRequest>>,
    /// Context-scoped compartments (`KEYBOARD_DISABLED`, `EMPTYCONTEXT`) —
    /// how a host turns the IME off for e.g. a password field.
    compartments: Rc<CompartmentLog>,
    /// When set, `GetSelection` serves a [`FakeRange`] over this log and
    /// `GetActiveView` serves a [`FakeContextView`]; when `None` both keep
    /// the historical `E_NOTIMPL` so older tests see the same host.
    range_log: Option<Rc<RangeLog>>,
    view_log: Rc<ViewLog>,
    /// `ITfSource` traffic (the text-layout sink advises here).
    sink_advises: RefCell<Vec<u32>>,
    sink_unadvises: RefCell<Vec<u32>>,
    next_sink_cookie: Cell<u32>,
}

impl FakeContext {
    pub fn new(behavior: EditSessionBehavior) -> ITfContext {
        Self::build(behavior, None, Rc::new(CompartmentLog::default()))
    }

    /// A context that also models a document: `GetSelection` hands out
    /// ranges over `log` (whose `text` is the document content) and
    /// `GetActiveView` answers with a fake view. Enables driving
    /// `update_pos` / `update_context`.
    pub fn with_ranges(behavior: EditSessionBehavior, log: Rc<RangeLog>) -> ITfContext {
        Self::build(behavior, Some(log), Rc::new(CompartmentLog::default()))
    }

    /// A context whose compartments the test controls — set
    /// `GUID_COMPARTMENT_KEYBOARD_DISABLED` on `compartments` to model a
    /// password field.
    pub fn with_compartments(
        behavior: EditSessionBehavior,
        compartments: Rc<CompartmentLog>,
    ) -> ITfContext {
        Self::build(behavior, None, compartments)
    }

    fn build(
        behavior: EditSessionBehavior,
        range_log: Option<Rc<RangeLog>>,
        compartments: Rc<CompartmentLog>,
    ) -> ITfContext {
        FakeContext {
            behavior: Cell::new(behavior),
            requests: RefCell::new(Vec::new()),
            compartments,
            range_log,
            view_log: Rc::new(ViewLog::default()),
            sink_advises: RefCell::new(Vec::new()),
            sink_unadvises: RefCell::new(Vec::new()),
            next_sink_cookie: Cell::new(0x9000),
        }
        .into()
    }

    pub fn requests(&self) -> Vec<EditSessionRequest> {
        self.requests.borrow().clone()
    }

    pub fn view_log(&self) -> Rc<ViewLog> {
        self.view_log.clone()
    }

    pub fn sink_advises(&self) -> Vec<u32> {
        self.sink_advises.borrow().clone()
    }

    pub fn sink_unadvises(&self) -> Vec<u32> {
        self.sink_unadvises.borrow().clone()
    }
}

impl ITfCompartmentMgr_Impl for FakeContext_Impl {
    fn GetCompartment(&self, rguid: *const GUID) -> WinResult<ITfCompartment> {
        let guid = unsafe { rguid.as_ref() }.ok_or_else(|| {
            windows::core::Error::from_hresult(windows::Win32::Foundation::E_INVALIDARG)
        })?;

        Ok(FakeCompartment {
            guid: *guid,
            log: self.compartments.clone(),
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

impl ITfSource_Impl for FakeContext_Impl {
    fn AdviseSink(
        &self,
        _riid: *const GUID,
        _punk: windows_core::Ref<'_, windows_core::IUnknown>,
    ) -> WinResult<u32> {
        let cookie = self.next_sink_cookie.get();
        self.next_sink_cookie.set(cookie + 1);
        self.sink_advises.borrow_mut().push(cookie);
        Ok(cookie)
    }

    fn UnadviseSink(&self, dwcookie: u32) -> WinResult<()> {
        self.sink_unadvises.borrow_mut().push(dwcookie);
        Ok(())
    }
}

impl ITfContext_Impl for FakeContext_Impl {
    fn RequestEditSession(
        &self,
        tid: u32,
        pes: windows_core::Ref<'_, ITfEditSession>,
        dwflags: TF_CONTEXT_EDIT_CONTEXT_FLAGS,
    ) -> WinResult<windows::core::HRESULT> {
        self.requests.borrow_mut().push(EditSessionRequest {
            tid,
            flags: dwflags,
        });

        match self.behavior.get() {
            EditSessionBehavior::RunSync => {
                let session = pes
                    .as_ref()
                    .ok_or_else(|| windows::core::Error::from_hresult(E_FAIL))?;
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
                    let session = pes
                        .as_ref()
                        .ok_or_else(|| windows::core::Error::from_hresult(E_FAIL))?;
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
        ulcount: u32,
        pselection: *mut TF_SELECTION,
        pcfetched: *mut u32,
    ) -> WinResult<()> {
        let Some(log) = self.range_log.as_ref() else {
            return Err(E_NOTIMPL.into());
        };
        if ulcount == 0 || pselection.is_null() {
            return Err(E_FAIL.into());
        }
        // [out]: the range leaves here AddRef'd inside a ManuallyDrop, like
        // a real host — the caller leaks it unless it takes ownership (B10)
        unsafe {
            (*pselection).range = ManuallyDrop::new(Some(FakeRange::new(log.clone())));
            if !pcfetched.is_null() {
                *pcfetched = 1;
            }
        }
        Ok(())
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
        if self.range_log.is_some() {
            Ok(FakeContextView {
                log: self.view_log.clone(),
            }
            .into())
        } else {
            Err(E_NOTIMPL.into())
        }
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

    fn CreateRangeBackup(
        &self,
        _ec: u32,
        _prange: windows_core::Ref<'_, ITfRange>,
    ) -> WinResult<ITfRangeBackup> {
        Err(E_NOTIMPL.into())
    }
}

impl ITfContextComposition_Impl for FakeContext_Impl {
    fn StartComposition(
        &self,
        _ecwrite: u32,
        _pcompositionrange: windows_core::Ref<'_, ITfRange>,
        _psink: windows_core::Ref<'_, ITfCompositionSink>,
    ) -> WinResult<ITfComposition> {
        Ok(FakeComposition::new())
    }

    fn EnumCompositions(&self) -> WinResult<IEnumITfCompositionView> {
        Err(E_NOTIMPL.into())
    }

    fn FindComposition(
        &self,
        _ecread: u32,
        _ptestrange: windows_core::Ref<'_, ITfRange>,
    ) -> WinResult<IEnumITfCompositionView> {
        Err(E_NOTIMPL.into())
    }

    fn TakeOwnership(
        &self,
        _ecwrite: u32,
        _pcomposition: windows_core::Ref<'_, ITfCompositionView>,
        _psink: windows_core::Ref<'_, ITfCompositionSink>,
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
        // A real host inserts at the caret and returns the new (empty) range;
        // start_composition then hands that range to StartComposition. Return
        // a FakeRange so the composition-creation path runs end to end (the
        // recovery-then-fresh-start case depends on this). Uses the context's
        // range log when it has one, else a throwaway.
        Ok(FakeRange::new(self.range_log.clone().unwrap_or_default()))
    }

    fn InsertEmbeddedAtSelection(
        &self,
        _ec: u32,
        _dwflags: u32,
        _pdataobject: windows_core::Ref<'_, IDataObject>,
    ) -> WinResult<ITfRange> {
        Err(E_NOTIMPL.into())
    }
}

/// How a [`FakeContextView`] answers `GetTextExt`. Hosts routinely return
/// `TS_E_NOLAYOUT` while layout is pending, and report a clipped rect when
/// the composition is scrolled out of view; both are normal, so `update_pos`
/// has to tell them apart from real failures.
#[derive(Default, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextExtBehavior {
    /// Serve [`FAKE_TEXT_EXT`], unclipped.
    #[default]
    Ok,
    /// Fail with `TS_E_NOLAYOUT`.
    NoLayout,
    /// Serve [`FAKE_TEXT_EXT`] but flag it clipped.
    Clipped,
}

/// What a [`FakeContextView`] was asked. The rect it serves is fixed and
/// known, so a test can assert it reached the IPC layer unchanged.
#[derive(Default)]
pub struct ViewLog {
    pub get_text_ext_calls: Cell<usize>,
    pub text_ext_behavior: Cell<TextExtBehavior>,
}

/// The rect every [`FakeContextView`] reports for any range.
pub const FAKE_TEXT_EXT: RECT = RECT {
    left: 10,
    top: 20,
    right: 110,
    bottom: 44,
};

/// A stand-in for the context's active view: `GetTextExt` answers with
/// [`FAKE_TEXT_EXT`] (unclipped) and records the call.
#[implement(ITfContextView)]
pub struct FakeContextView {
    log: Rc<ViewLog>,
}

impl ITfContextView_Impl for FakeContextView_Impl {
    fn GetRangeFromPoint(
        &self,
        _ec: u32,
        _ppt: *const POINT,
        _dwflags: u32,
    ) -> WinResult<ITfRange> {
        Err(E_NOTIMPL.into())
    }

    fn GetTextExt(
        &self,
        _ec: u32,
        _prange: windows_core::Ref<'_, ITfRange>,
        prc: *mut RECT,
        pfclipped: *mut BOOL,
    ) -> WinResult<()> {
        self.log
            .get_text_ext_calls
            .set(self.log.get_text_ext_calls.get() + 1);

        let behavior = self.log.text_ext_behavior.get();
        if behavior == TextExtBehavior::NoLayout {
            return Err(TS_E_NOLAYOUT.into());
        }

        unsafe {
            if !prc.is_null() {
                *prc = FAKE_TEXT_EXT;
            }
            if !pfclipped.is_null() {
                *pfclipped = (behavior == TextExtBehavior::Clipped).into();
            }
        }
        Ok(())
    }

    fn GetScreenExt(&self) -> WinResult<RECT> {
        Ok(RECT::default())
    }

    fn GetWnd(&self) -> WinResult<HWND> {
        Err(E_NOTIMPL.into())
    }
}

/// A document manager that owns exactly one context — enough for
/// `GetTop`/`GetBase` (the text-layout-sink advise path).
#[implement(ITfDocumentMgr)]
pub struct FakeDocumentMgr {
    top: ITfContext,
}

impl FakeDocumentMgr {
    pub fn new(top: ITfContext) -> ITfDocumentMgr {
        FakeDocumentMgr { top }.into()
    }
}

impl ITfDocumentMgr_Impl for FakeDocumentMgr_Impl {
    fn CreateContext(
        &self,
        _tidowner: u32,
        _dwflags: u32,
        _punk: windows_core::Ref<'_, windows_core::IUnknown>,
        _ppic: OutRef<'_, ITfContext>,
        _pectextstore: *mut u32,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn Push(&self, _pic: windows_core::Ref<'_, ITfContext>) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn Pop(&self, _dwflags: u32) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn GetTop(&self) -> WinResult<ITfContext> {
        Ok(self.top.clone())
    }

    fn GetBase(&self) -> WinResult<ITfContext> {
        Ok(self.top.clone())
    }

    fn EnumContexts(&self) -> WinResult<IEnumTfContexts> {
        Err(E_NOTIMPL.into())
    }
}
