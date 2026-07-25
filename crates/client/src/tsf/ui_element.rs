//! UILess mode: letting the host draw (or suppress) our candidate list.
//!
//! `register.rs` declares `GUID_TFCAT_TIPCAP_UIELEMENTENABLED`, which per the
//! UILess Mode Overview obliges a TIP to route **all** UI display through
//! `ITfUIElementMgr` so the application can control visibility. Until this
//! module existed nothing implemented it: on a UILess thread — a full-screen
//! DirectX game, the Windows search box — we were activated as a supporting
//! TIP and then drew our own topmost UIAccess window anyway, overriding the
//! host.
//!
//! Three things are worth knowing before changing this file.
//!
//! **The gate is `pbShow`, not a flag.** `TF_TMAE_UIELEMENTENABLED` does not
//! exist in windows-rs (only `TF_TMAE_UIELEMENTENABLEDONLY`), and activation
//! flags are the wrong signal anyway: every host answers `BeginUIElement`
//! with a `pbShow` regardless of how it activated us. `ActivateEx` therefore
//! only logs its `dwflags` and drops them.
//!
//! **A live element pins the TIP.** `BeginUIElement` makes the host AddRef
//! this object and hold it until `EndUIElement`. Deactivating with an
//! element still open would keep the host holding us forever — the same
//! shape as the B15 leak (see factory.rs). `Deactivate` therefore calls
//! `ui_end` unconditionally.
//!
//! **Failing open.** Every helper degrades to "show our own window". A host
//! with no `ITfUIElementMgr`, a failed `BeginUIElement`, even a borrow
//! conflict — all of them mean the user still sees candidates. The opposite
//! default would make a bug here invisible *and* silent.

use anyhow::Result;
use windows::{
    Win32::{
        Foundation::{E_FAIL, E_INVALIDARG},
        UI::TextServices::{
            ITfCandidateListUIElement_Impl, ITfContext, ITfDocumentMgr, ITfUIElement,
            ITfUIElement_Impl, ITfUIElementMgr,
        },
    },
    core::{BOOL, BSTR, GUID, Interface},
};

use crate::{engine::ipc_service::Candidates, globals::GUID_CANDIDATE_LIST_UI_ELEMENT};

use super::factory::TextServiceFactory_Impl;

/// Candidates per page, as reported to the host.
///
/// Must match the grouping the webview scrolls by — `const groupSize = 5` in
/// crates/ui/assets/candidate.js. A mismatch makes the host's page
/// navigation disagree with what the user sees.
const CANDIDATE_PAGE_SIZE: u32 = 5;

/// Break-glass switch: set `AZOOKEY_FORCE_CANDIDATE_WINDOW=1` to keep drawing
/// our own window whatever the host says.
///
/// A host answering `pbShow = FALSE` plus any defect in the element methods
/// below means the user sees *no* candidates at all, with nothing on screen
/// to explain why. This makes that recoverable without a rebuild.
///
/// An environment variable rather than a `settings.json` key on purpose: it
/// is read only by this DLL, needs no IPC or settings-app plumbing, and
/// cannot leave a broken value persisted in the user's config.
fn force_candidate_window() -> bool {
    static FORCED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FORCED.get_or_init(|| {
        matches!(
            std::env::var("AZOOKEY_FORCE_CANDIDATE_WINDOW").as_deref(),
            Ok("1") | Ok("true")
        )
    })
}

impl TextServiceFactory_Impl {
    /// The host's UILess element manager, or `None` when the host has no
    /// `ITfUIElementMgr` — such a host wants our own window, so every caller
    /// treats `None` as "fall back, advisory". A borrow conflict still
    /// propagates as an error, matching the `self.borrow()?` these call sites
    /// used before this was folded out of them.
    fn ui_element_mgr(&self) -> Result<Option<ITfUIElementMgr>> {
        let Ok(thread_mgr) = self.borrow()?.thread_mgr() else {
            return Ok(None);
        };
        Ok(thread_mgr.cast::<ITfUIElementMgr>().ok())
    }

    /// Announces the candidate list to the host and asks whether we may draw
    /// it ourselves. Idempotent: a second call returns the stored answer.
    pub fn ui_begin(&self) -> Result<bool> {
        {
            let text_service = self.borrow()?;
            if text_service.ui_element.id.is_some() {
                return Ok(text_service.ui_element.show);
            }
        }

        let Some(mgr) = self.ui_element_mgr()? else {
            return Ok(true);
        };

        // BeginUIElement synchronously calls GetDescription/GetGUID and may
        // call GetCount, so the (empty) snapshot must already be readable
        // and no borrow may be held here.
        let element = self.this::<ITfUIElement>()?;
        let mut show = BOOL::from(true);
        let mut id: u32 = 0;

        if let Err(error) = unsafe { mgr.BeginUIElement(&element, &mut show, &mut id) } {
            tracing::warn!("BeginUIElement failed; showing our own window: {error:?}");
            return Ok(true);
        }

        // still Begin/End properly even when forced, so the host's element
        // bookkeeping stays consistent; only the answer is overridden
        let show = show.as_bool() || force_candidate_window();
        {
            let mut text_service = self.borrow_mut()?;
            text_service.ui_element.id = Some(id);
            text_service.ui_element.show = show;
        }

        if !show {
            tracing::debug!("host suppressed the candidate window (UILess)");
        }

        Ok(show)
    }

    /// Publishes a new candidate snapshot and notifies the host.
    ///
    /// The snapshot is written under a borrow that is dropped before
    /// `UpdateUIElement`, because the host reads it back synchronously from
    /// inside that call.
    pub fn ui_update(
        &self,
        candidates: &Candidates,
        selection_index: i32,
        updated_flags: u32,
    ) -> Result<()> {
        let id = {
            let mut text_service = self.borrow_mut()?;
            text_service.ui_element.candidates = candidates.clone();
            text_service.ui_element.selection_index = selection_index;
            text_service.ui_element.updated_flags = updated_flags;
            text_service.ui_element.id
        };

        let Some(id) = id else {
            return Ok(());
        };

        let Some(mgr) = self.ui_element_mgr()? else {
            return Ok(());
        };

        // advisory: a host that rejects the update must not break typing
        if let Err(error) = unsafe { mgr.UpdateUIElement(id) } {
            tracing::warn!("UpdateUIElement failed: {error:?}");
        }

        Ok(())
    }

    /// Ends the element and clears the snapshot. Safe to call when no
    /// element is open, which is why `Deactivate` can call it blindly.
    pub fn ui_end(&self) -> Result<()> {
        let id = {
            let mut text_service = self.borrow_mut()?;
            let id = text_service.ui_element.id.take();
            text_service.ui_element.show = false;
            text_service.ui_element.candidates = Candidates::default();
            text_service.ui_element.selection_index = 0;
            text_service.ui_element.updated_flags = 0;
            id
        };

        let Some(id) = id else {
            return Ok(());
        };

        let Some(mgr) = self.ui_element_mgr()? else {
            return Ok(());
        };

        if let Err(error) = unsafe { mgr.EndUIElement(id) } {
            // the host may already have torn the element down
            tracing::warn!("EndUIElement failed: {error:?}");
        }

        Ok(())
    }

    /// Whether our own out-of-process window may be shown.
    ///
    /// A pure read that fails open: with no element open (or a borrow
    /// conflict) the answer is yes, which is the pre-UILess behaviour.
    pub fn ui_should_show(&self) -> bool {
        if force_candidate_window() {
            return true;
        }

        let Ok(text_service) = self.borrow() else {
            return true;
        };

        match text_service.ui_element.id {
            Some(_) => text_service.ui_element.show,
            None => true,
        }
    }
}

impl ITfUIElement_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    fn GetDescription(&self) -> Result<BSTR> {
        Ok(BSTR::from("azooKey candidate list"))
    }

    #[macros::anyhow]
    fn GetGUID(&self) -> Result<GUID> {
        Ok(GUID_CANDIDATE_LIST_UI_ELEMENT)
    }

    #[macros::anyhow]
    fn Show(&self, bshow: BOOL) -> Result<()> {
        let show = bshow.as_bool();
        {
            let mut text_service = self.borrow_mut()?;
            text_service.ui_element.show = show;
        }

        // honour a host that flips visibility mid-composition. Advisory: with
        // no service there is no window of ours to flip.
        if let Some(ipc_service) = crate::engine::state::IMEState::ipc()? {
            if show {
                ipc_service.show_window();
            } else {
                ipc_service.hide_window();
            }
        }

        Ok(())
    }

    #[macros::anyhow]
    fn IsShown(&self) -> Result<BOOL> {
        Ok(self.ui_should_show().into())
    }
}

impl ITfCandidateListUIElement_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    fn GetUpdatedFlags(&self) -> Result<u32> {
        Ok(self.borrow()?.ui_element.updated_flags)
    }

    #[macros::anyhow]
    fn GetDocumentMgr(&self) -> Result<ITfDocumentMgr> {
        let text_service = self.borrow()?;

        if let Ok(context) = text_service.context::<ITfContext>()
            && let Ok(doc_mgr) = unsafe { context.GetDocumentMgr() }
        {
            return Ok(doc_mgr);
        }

        // fall back to whatever currently has focus
        if let Ok(thread_mgr) = text_service.thread_mgr()
            && let Ok(doc_mgr) = unsafe { thread_mgr.GetFocus() }
        {
            return Ok(doc_mgr);
        }

        Err(windows::core::Error::from_hresult(E_FAIL).into())
    }

    #[macros::anyhow]
    fn GetCount(&self) -> Result<u32> {
        let text_service = self.borrow()?;
        Ok(text_service.ui_element.candidates.texts.len() as u32)
    }

    #[macros::anyhow]
    fn GetSelection(&self) -> Result<u32> {
        let text_service = self.borrow()?;
        Ok(text_service.ui_element.selection_index.max(0) as u32)
    }

    #[macros::anyhow]
    fn GetString(&self, uindex: u32) -> Result<BSTR> {
        let text_service = self.borrow()?;
        let candidates = &text_service.ui_element.candidates;

        if uindex as usize >= candidates.texts.len() {
            return Err(windows::core::Error::from_hresult(E_INVALIDARG).into());
        }

        // entry() is bounds-safe by design (ipc_service.rs): a panic here
        // would unwind out of a COM callback and abort the host
        Ok(BSTR::from(candidates.entry(uindex as usize).0))
    }

    /// Two-call protocol: with a null `pindex` the host is only asking how
    /// many pages there are.
    ///
    /// The windows-rs parameter is literally named `usize`, shadowing the
    /// primitive, so it is renamed here. Both out-pointers are checked —
    /// a raw deref in a COM callback is a segfault, which `catch_unwind`
    /// cannot turn into an HRESULT.
    #[macros::anyhow]
    fn GetPageIndex(&self, pindex: *mut u32, ucount: u32, pupagecnt: *mut u32) -> Result<()> {
        if pupagecnt.is_null() {
            return Err(windows::core::Error::from_hresult(E_INVALIDARG).into());
        }

        let count = {
            let text_service = self.borrow()?;
            text_service.ui_element.candidates.texts.len() as u32
        };
        let page_count = count.div_ceil(CANDIDATE_PAGE_SIZE);

        if pindex.is_null() {
            unsafe { *pupagecnt = page_count };
            return Ok(());
        }

        let writable = page_count.min(ucount);
        for i in 0..writable {
            unsafe { *pindex.add(i as usize) = i * CANDIDATE_PAGE_SIZE };
        }
        unsafe { *pupagecnt = writable };

        Ok(())
    }

    /// We are not an `ITfCandidateListUIElementBehavior`, so page changes
    /// are the host's business. `E_NOTIMPL` makes some hosts drop the
    /// element entirely, so this succeeds without doing anything.
    #[macros::anyhow]
    fn SetPageIndex(&self, _pindex: *const u32, _upagecnt: u32) -> Result<()> {
        Ok(())
    }

    #[macros::anyhow]
    fn GetCurrentPage(&self) -> Result<u32> {
        let text_service = self.borrow()?;
        let selection = text_service.ui_element.selection_index.max(0) as u32;
        Ok(selection / CANDIDATE_PAGE_SIZE)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    // the tests build a bare factory; the module itself only needs the
    // generated outer type now
    use crate::globals::{DLL_INSTANCE, DllModule};
    use crate::tsf::factory::TextServiceFactory;
    use crate::tsf::test_support::{
        FakeThreadMgr, ThreadMgrLog, UiElementLog, factory_of, global_state_lock,
    };
    use std::rc::Rc;
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    use windows::Win32::UI::TextServices::{
        ITfCandidateListUIElement, ITfTextInputProcessor, TF_CLUIE_CURRENTPAGE, TF_CLUIE_PAGEINDEX,
        TF_CLUIE_STRING,
    };

    /// Calls `GetPageIndex` through the raw vtable, the way a host does.
    ///
    /// The safe wrapper takes `&mut [u32]`, whose pointer is never null even
    /// for an empty slice, so the documented "null pindex = count-only
    /// query" protocol cannot be reached through it.
    unsafe fn raw_get_page_index(
        element: &ITfCandidateListUIElement,
        pindex: *mut u32,
        count: u32,
        page_count: *mut u32,
    ) -> windows::core::HRESULT {
        unsafe {
            (Interface::vtable(element).GetPageIndex)(
                Interface::as_raw(element),
                pindex,
                count,
                page_count,
            )
        }
    }

    fn ensure_dll_module() {
        let _ = DLL_INSTANCE.set(std::sync::Mutex::new(DllModule::new()));
    }

    fn activate_with(ui: Rc<UiElementLog>) -> ITfTextInputProcessor {
        ensure_dll_module();
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let tip = TextServiceFactory::create::<ITfTextInputProcessor>()
            .expect("failed to create the TIP");
        let thread_mgr = FakeThreadMgr::with_ui_elements(Rc::new(ThreadMgrLog::default()), ui);
        unsafe { tip.Activate(Some(&thread_mgr), 1) }.expect("Activate must succeed");
        tip
    }

    fn candidates(texts: &[&str]) -> Candidates {
        Candidates {
            texts: texts.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    /// An ordinary host lets us draw, so nothing changes for the existing
    /// out-of-process candidate window.
    #[test]
    fn a_permissive_host_lets_us_show_our_own_window() {
        let _guard = global_state_lock();
        let ui = Rc::new(UiElementLog::default());
        let tip = activate_with(ui.clone());
        let factory = factory_of(&tip);

        assert!(factory.ui_begin().unwrap(), "pbShow=TRUE means we may draw");
        assert_eq!(ui.begin_calls.get(), 1);
        assert!(factory.ui_should_show());

        let _ = unsafe { tip.Deactivate() };
    }

    /// The point of UILess mode: a host that draws the candidates itself
    /// answers pbShow=FALSE, and our own window must stay hidden.
    #[test]
    fn a_uiless_host_suppresses_our_window() {
        let _guard = global_state_lock();
        let ui = Rc::new(UiElementLog::suppressing());
        let tip = activate_with(ui.clone());
        let factory = factory_of(&tip);

        assert!(
            !factory.ui_begin().unwrap(),
            "pbShow=FALSE must suppress our window"
        );
        assert!(!factory.ui_should_show());

        let _ = unsafe { tip.Deactivate() };
    }

    /// Failing open: a host with no ITfUIElementMgr must still get
    /// candidates, otherwise UILess support would silently break every
    /// ordinary application.
    #[test]
    fn a_host_without_the_element_manager_still_gets_candidates() {
        let _guard = global_state_lock();
        ensure_dll_module();
        let tip = TextServiceFactory::create::<ITfTextInputProcessor>().unwrap();
        let factory = factory_of(&tip);

        // never activated: no thread manager at all
        assert!(factory.ui_begin().unwrap());
        assert!(factory.ui_should_show());
    }

    /// The host reads the candidate list back synchronously, so what it sees
    /// must be what we just published — not the previous keystroke's list.
    #[test]
    fn the_host_reads_back_the_published_snapshot() {
        let _guard = global_state_lock();
        let ui = Rc::new(UiElementLog::default());
        let tip = activate_with(ui.clone());
        let factory = factory_of(&tip);
        factory.ui_begin().unwrap();

        factory
            .ui_update(&candidates(&["水", "みず", "ミズ"]), 1, TF_CLUIE_STRING)
            .unwrap();

        let element: ITfCandidateListUIElement = tip.cast().unwrap();
        assert_eq!(unsafe { element.GetCount() }.unwrap(), 3);
        assert_eq!(unsafe { element.GetSelection() }.unwrap(), 1);
        assert_eq!(unsafe { element.GetString(0) }.unwrap(), BSTR::from("水"));
        assert_eq!(unsafe { element.GetString(2) }.unwrap(), BSTR::from("ミズ"));
        assert_eq!(ui.update_calls.get(), 1);

        let _ = unsafe { tip.Deactivate() };
    }

    /// Out-of-range must be an error, not a panic: a panic here unwinds out
    /// of a COM callback and aborts the host application.
    #[test]
    fn an_out_of_range_candidate_is_an_error_not_a_panic() {
        let _guard = global_state_lock();
        let ui = Rc::new(UiElementLog::default());
        let tip = activate_with(ui);
        let factory = factory_of(&tip);
        factory.ui_begin().unwrap();
        factory
            .ui_update(&candidates(&["水"]), 0, TF_CLUIE_STRING)
            .unwrap();

        let element: ITfCandidateListUIElement = tip.cast().unwrap();
        assert!(unsafe { element.GetString(1) }.is_err());
        assert!(unsafe { element.GetString(u32::MAX) }.is_err());

        let _ = unsafe { tip.Deactivate() };
    }

    /// GetPageIndex has a two-call protocol: a null pindex asks only for the
    /// page count. Both out-pointers are raw, and a bad deref here is a
    /// segfault that catch_unwind cannot convert into an HRESULT.
    #[test]
    fn get_page_index_answers_the_count_only_query() {
        let _guard = global_state_lock();
        let ui = Rc::new(UiElementLog::default());
        let tip = activate_with(ui);
        let factory = factory_of(&tip);
        factory.ui_begin().unwrap();
        // 7 candidates over pages of 5 -> 2 pages
        factory
            .ui_update(
                &candidates(&["1", "2", "3", "4", "5", "6", "7"]),
                0,
                TF_CLUIE_PAGEINDEX,
            )
            .unwrap();

        let element: ITfCandidateListUIElement = tip.cast().unwrap();

        let mut page_count = 0u32;
        let hr = unsafe { raw_get_page_index(&element, std::ptr::null_mut(), 0, &mut page_count) };
        assert!(hr.is_ok(), "the count-only query must succeed");
        assert_eq!(page_count, 2, "7 candidates over pages of 5 is 2 pages");

        let mut pages = [0u32; 4];
        let mut written = 0u32;
        let hr = unsafe { raw_get_page_index(&element, pages.as_mut_ptr(), 4, &mut written) };
        assert!(hr.is_ok());
        assert_eq!(written, 2);
        assert_eq!(&pages[..2], &[0, 5], "pages start at candidate 0 and 5");

        let _ = unsafe { tip.Deactivate() };
    }

    /// A null page-count pointer must be rejected rather than dereferenced.
    #[test]
    fn get_page_index_rejects_a_null_count_pointer() {
        let _guard = global_state_lock();
        let ui = Rc::new(UiElementLog::default());
        let tip = activate_with(ui);
        let element: ITfCandidateListUIElement = tip.cast().unwrap();

        let hr =
            unsafe { raw_get_page_index(&element, std::ptr::null_mut(), 0, std::ptr::null_mut()) };
        assert!(
            hr.is_err(),
            "a null out-pointer must be rejected, not dereferenced"
        );

        let _ = unsafe { tip.Deactivate() };
    }

    /// A live element makes the host hold a reference to this object until
    /// EndUIElement. Deactivating without ending it pins the TIP in the host
    /// forever — the B15 leak shape.
    #[test]
    fn deactivate_ends_a_live_ui_element() {
        let _guard = global_state_lock();
        let ui = Rc::new(UiElementLog::default());
        let tip = activate_with(ui.clone());
        let factory = factory_of(&tip);

        factory.ui_begin().unwrap();
        assert_eq!(ui.live_elements(), 1, "an element is open");

        unsafe { tip.Deactivate() }.expect("Deactivate must succeed");

        assert_eq!(
            ui.live_elements(),
            0,
            "Deactivate must end the element or the host pins the TIP forever"
        );
    }

    /// Ending twice, or without ever beginning, must be harmless — that is
    /// what lets Deactivate call it unconditionally.
    #[test]
    fn ending_without_a_live_element_is_a_no_op() {
        let _guard = global_state_lock();
        let ui = Rc::new(UiElementLog::default());
        let tip = activate_with(ui.clone());
        let factory = factory_of(&tip);

        factory
            .ui_end()
            .expect("ui_end with no element must succeed");
        factory.ui_begin().unwrap();
        factory.ui_end().unwrap();
        factory.ui_end().expect("a second ui_end must be harmless");

        assert_eq!(ui.end_calls.get(), 1, "only the live element is ended");

        let _ = unsafe { tip.Deactivate() };
    }

    /// Pages are reported in the same grouping the webview scrolls by.
    #[test]
    fn the_current_page_follows_the_selection() {
        let _guard = global_state_lock();
        let ui = Rc::new(UiElementLog::default());
        let tip = activate_with(ui);
        let factory = factory_of(&tip);
        factory.ui_begin().unwrap();

        let list = candidates(&["1", "2", "3", "4", "5", "6", "7"]);
        let element: ITfCandidateListUIElement = tip.cast().unwrap();

        factory.ui_update(&list, 4, TF_CLUIE_CURRENTPAGE).unwrap();
        assert_eq!(unsafe { element.GetCurrentPage() }.unwrap(), 0);

        factory.ui_update(&list, 5, TF_CLUIE_CURRENTPAGE).unwrap();
        assert_eq!(unsafe { element.GetCurrentPage() }.unwrap(), 1);

        let _ = unsafe { tip.Deactivate() };
    }
}
