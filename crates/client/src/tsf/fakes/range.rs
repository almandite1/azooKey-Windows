//! Ranges, the property that marks them, and the composition that hands them
//! out.
//!
//! [`RangeLog`] is the only state shared with the other fakes: every range a
//! test's composition produces reports into it, which is what lets a test see
//! both the arguments the TIP passed (the unit-of-measure bug B9) and whether
//! it released every range it took (a leaked AddRef never returns
//! `live_ranges` to zero).

// new() returning the COM interface rather than Self is deliberate: the
// wrapped struct is consumed by .into() and only the interface is usable
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::new_ret_no_self)]

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use windows::{
    Win32::{
        Foundation::E_NOTIMPL,
        System::Com::IDataObject,
        System::Variant::VARIANT,
        UI::TextServices::{
            IEnumTfRanges, ITfComposition, ITfComposition_Impl, ITfContext, ITfProperty,
            ITfProperty_Impl, ITfPropertyStore, ITfRange, ITfRange_Impl, ITfReadOnlyProperty_Impl,
            TF_HALTCOND,
        },
    },
    core::{BOOL, GUID, IUnknown, OutRef, PCWSTR, PWSTR, Result as WinResult, implement},
};

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
    /// The backing text `GetText` serves (UTF-16). Empty by default, so the
    /// old cch=0 behavior is preserved for tests that don't care about text.
    pub text: RefCell<Vec<u16>>,
    /// Every payload `SetText` received, in call order.
    pub set_texts: RefCell<Vec<Vec<u16>>>,
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
        pchtext: PWSTR,
        cchmax: u32,
        pcch: *mut u32,
    ) -> WinResult<()> {
        // Serve up to cchmax units of the configured backing text, like a
        // real host: a full buffer tells the caller there may be more.
        // (Serving from the start on every call matches the TIP's
        // fresh-clone-per-attempt read; MOVESTART is not modeled.)
        let text = self.log.text.borrow();
        let served = text.len().min(cchmax as usize);
        if !pchtext.is_null() && served > 0 {
            unsafe { std::ptr::copy_nonoverlapping(text.as_ptr(), pchtext.0, served) };
        }
        if !pcch.is_null() {
            unsafe { *pcch = served as u32 };
        }
        Ok(())
    }

    fn SetText(&self, _ec: u32, _dwflags: u32, pchtext: &PCWSTR, cch: i32) -> WinResult<()> {
        let received = if pchtext.is_null() || cch <= 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(pchtext.0, cch as usize) }.to_vec()
        };
        self.log.set_texts.borrow_mut().push(received);
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
        _pdataobject: windows_core::Ref<'_, IDataObject>,
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
        _prange: windows_core::Ref<'_, ITfRange>,
        _apos: windows::Win32::UI::TextServices::TfAnchor,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn ShiftEndToRange(
        &self,
        _ec: u32,
        _prange: windows_core::Ref<'_, ITfRange>,
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
        _pwith: windows_core::Ref<'_, ITfRange>,
        _apos: windows::Win32::UI::TextServices::TfAnchor,
    ) -> WinResult<BOOL> {
        Err(E_NOTIMPL.into())
    }

    fn IsEqualEnd(
        &self,
        _ec: u32,
        _pwith: windows_core::Ref<'_, ITfRange>,
        _apos: windows::Win32::UI::TextServices::TfAnchor,
    ) -> WinResult<BOOL> {
        Err(E_NOTIMPL.into())
    }

    fn CompareStart(
        &self,
        _ec: u32,
        _pwith: windows_core::Ref<'_, ITfRange>,
        _apos: windows::Win32::UI::TextServices::TfAnchor,
    ) -> WinResult<i32> {
        Err(E_NOTIMPL.into())
    }

    fn CompareEnd(
        &self,
        _ec: u32,
        _pwith: windows_core::Ref<'_, ITfRange>,
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
        _ppenum: OutRef<'_, IEnumTfRanges>,
        _ptargetrange: windows_core::Ref<'_, ITfRange>,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn GetValue(&self, _ec: u32, _prange: windows_core::Ref<'_, ITfRange>) -> WinResult<VARIANT> {
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
        _prange: windows_core::Ref<'_, ITfRange>,
        _pprange: OutRef<'_, ITfRange>,
        _apos: windows::Win32::UI::TextServices::TfAnchor,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn SetValueStore(
        &self,
        _ec: u32,
        _prange: windows_core::Ref<'_, ITfRange>,
        _ppropstore: windows_core::Ref<'_, ITfPropertyStore>,
    ) -> WinResult<()> {
        Err(E_NOTIMPL.into())
    }

    fn SetValue(
        &self,
        _ec: u32,
        _prange: windows_core::Ref<'_, ITfRange>,
        _pvarvalue: *const VARIANT,
    ) -> WinResult<()> {
        Ok(())
    }

    fn Clear(&self, _ec: u32, _prange: windows_core::Ref<'_, ITfRange>) -> WinResult<()> {
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

    fn ShiftStart(
        &self,
        _ecwrite: u32,
        _pnewstart: windows_core::Ref<'_, ITfRange>,
    ) -> WinResult<()> {
        Ok(())
    }

    fn ShiftEnd(&self, _ecwrite: u32, _pnewend: windows_core::Ref<'_, ITfRange>) -> WinResult<()> {
        Ok(())
    }

    fn EndComposition(&self, _ecwrite: u32) -> WinResult<()> {
        Ok(())
    }
}
