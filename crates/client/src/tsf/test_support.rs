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

use windows::{
    core::{implement, Result as WinResult, GUID, PCWSTR},
    Win32::{
        Foundation::{BOOL, E_FAIL, E_NOTIMPL, S_OK},
        System::Com::IDataObject,
        UI::TextServices::{
            IEnumITfCompositionView, IEnumTfContextViews, IEnumTfProperties, ITfComposition,
            ITfCompositionSink, ITfCompositionView, ITfComposition_Impl, ITfContext,
            ITfContextComposition, ITfContextComposition_Impl, ITfContextView, ITfContext_Impl,
            ITfDocumentMgr, ITfEditSession, ITfInsertAtSelection, ITfInsertAtSelection_Impl,
            ITfProperty, ITfRange, ITfRangeBackup, ITfReadOnlyProperty,
            INSERT_TEXT_AT_SELECTION_FLAGS, TF_CONTEXT_EDIT_CONTEXT_FLAGS, TF_S_ASYNC, TS_STATUS,
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
        Err(E_NOTIMPL.into())
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
        Err(E_NOTIMPL.into())
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

/// A stand-in for a live composition. Enough to make
/// `Composition::tip_composition` non-`None`.
#[implement(ITfComposition)]
pub struct FakeComposition;

impl FakeComposition {
    pub fn new() -> ITfComposition {
        FakeComposition.into()
    }
}

impl ITfComposition_Impl for FakeComposition_Impl {
    fn GetRange(&self) -> WinResult<ITfRange> {
        Err(E_NOTIMPL.into())
    }

    fn ShiftStart(&self, _ecwrite: u32, _pnewstart: Option<&ITfRange>) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn ShiftEnd(&self, _ecwrite: u32, _pnewend: Option<&ITfRange>) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn EndComposition(&self, _ecwrite: u32) -> WinResult<()> {
        Ok(())
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
