use macros::anyhow;
use windows::{
    core::{implement, AsImpl, VARIANT},
    Win32::{
        Foundation::RECT,
        UI::TextServices::{
            ITfComposition, ITfCompositionSink, ITfContext, ITfContextComposition, ITfEditSession,
            ITfEditSession_Impl, ITfInsertAtSelection, ITfRange, GUID_PROP_ATTRIBUTE, TF_AE_NONE,
            TF_ANCHOR_END, TF_ANCHOR_START, TF_ES_READWRITE, TF_IAS_QUERYONLY, TF_SELECTION,
            TF_SELECTIONSTYLE, TF_ST_CORRECTION, TF_TF_MOVESTART,
        },
    },
};

use std::{cell::Cell, mem::ManuallyDrop, rc::Rc, time::Instant};

use anyhow::Result;

use crate::{engine::state::IMEState, extension::StringExt as _, globals::GUID_DISPLAY_ATTRIBUTE};

use super::factory::TextServiceFactory;

#[implement(ITfEditSession)]
struct EditSession<'a, T> {
    callback: Rc<dyn Fn(u32) -> anyhow::Result<T>>,
    pub result: Cell<Option<T>>,
    phantom: std::marker::PhantomData<&'a T>,
}

// edit action will be performed within this function
pub fn edit_session<T>(
    client_id: u32,
    context: ITfContext,
    callback: Rc<dyn Fn(u32) -> anyhow::Result<T>>,
) -> Result<Option<T>> {
    let session: ITfEditSession = EditSession {
        callback,
        result: Cell::new(None),
        phantom: std::marker::PhantomData,
    }
    .into();

    // Request read/write access only. Do NOT force TF_ES_SYNC (hosts like
    // Notepad reject it with TF_E_SYNCHRONOUS, failing every keystroke) and
    // do NOT inspect phrSession (the inner HRESULT): key-context sessions
    // return non-S_OK-but-harmless values there, and treating those as errors
    // fails every edit session so nothing gets composed. This is the original
    // 6ec1764 behavior; B7 broke real input on both counts.
    let result = unsafe { context.RequestEditSession(client_id, &session, TF_ES_READWRITE) };

    match result {
        Ok(_) => {
            let session = unsafe { session.as_impl() };
            Ok(session.result.take())
        }
        Err(e) => Err(anyhow::Error::new(e)),
    }
}

impl<'a, T> ITfEditSession_Impl for EditSession_Impl<'a, T> {
    #[anyhow]
    fn DoEditSession(&self, cookie: u32) -> Result<()> {
        let result = (self.callback)(cookie)?;
        self.result.set(Some(result));
        Ok(())
    }
}

/// Collapses the selection to `range`. `TF_SELECTION.range` is
/// `ManuallyDrop` and `SetSelection` is [in]-only, so the AddRef taken here
/// must be released by us afterwards — win or lose — or the range leaks in
/// the host application once per call.
fn set_selection(context: &ITfContext, cookie: u32, range: &ITfRange) -> windows::core::Result<()> {
    let selections = [TF_SELECTION {
        range: ManuallyDrop::new(Some(range.clone())),
        style: TF_SELECTIONSTYLE {
            ase: TF_AE_NONE,
            fInterimChar: false.into(),
        },
    }];

    let result = unsafe { context.SetSelection(cookie, &selections) };
    let [selection] = selections;
    drop(ManuallyDrop::into_inner(selection.range));
    result
}

impl TextServiceFactory {
    #[tracing::instrument]
    pub fn start_composition(&self) -> Result<()> {
        tracing::debug!("start_composition");

        // the stale-composition check must not hold a borrow on the text
        // service: end_composition borrows it again
        let tip_exists = {
            let text_service = self.borrow()?;
            let exists = text_service.borrow_composition()?.tip_composition.is_some();
            exists
        };

        if tip_exists {
            // the leftover composition is likely dead (e.g. the host
            // disconnected mid-composition); failing to end it must not
            // wedge the input path — letting go of it is what matters
            if let Err(error) = self.end_composition() {
                tracing::warn!("Failed to end a stale composition: {error:?}");
            }
            return Ok(());
        }

        let text_service = self.borrow_mut()?;
        let context = text_service.context()?;
        let context_composition = text_service.context::<ITfContextComposition>()?;
        let sink = self.this::<ITfCompositionSink>()?;
        let insert = text_service.context::<ITfInsertAtSelection>()?;

        let composition = edit_session::<ITfComposition>(
            text_service.tid,
            context,
            Rc::new({
                move |cookie| unsafe {
                    let range = insert.InsertTextAtSelection(cookie, TF_IAS_QUERYONLY, &[])?;
                    let composition =
                        context_composition.StartComposition(cookie, &range, &sink)?;

                    Ok(composition)
                }
            }),
        )?;

        tracing::debug!("Composition started {composition:?}");
        text_service.borrow_mut_composition()?.tip_composition = composition;

        Ok(())
    }

    #[tracing::instrument]
    pub fn end_composition(&self) -> Result<()> {
        tracing::debug!("end_composition");
        let text_service = self.borrow()?;

        let result =
            if let Some(composition) = text_service.borrow_composition()?.tip_composition.clone() {
                edit_session(
                    text_service.tid,
                    text_service.context()?,
                    Rc::new({
                        let context = text_service.context::<ITfContext>()?;

                        move |cookie| unsafe {
                            // clear display attribute first
                            let range: ITfRange = composition.GetRange()?;

                            // Read the FULL composition text. GetText fills at
                            // most the buffer, so a full buffer means "maybe
                            // more" — retry from a fresh clone with a larger
                            // one (B23: a fixed 1024 buffer silently truncated
                            // longer compositions on commit). cch < capacity
                            // proves completeness.
                            const MAX_COMPOSITION_UNITS: usize = 1 << 20;
                            let mut capacity: usize = 1024;
                            let text = loop {
                                let mut buf = vec![0u16; capacity];
                                let mut cch: u32 = 0;

                                // fresh clone each attempt: TF_TF_MOVESTART
                                // moves the previous clone's start anchor
                                let probe = range.Clone()?;
                                probe.GetText(cookie, TF_TF_MOVESTART, &mut buf, &mut cch)?;

                                if (cch as usize) < capacity {
                                    buf.truncate(cch as usize);
                                    break buf;
                                }
                                if capacity >= MAX_COMPOSITION_UNITS {
                                    tracing::warn!(
                                        "composition text exceeds {MAX_COMPOSITION_UNITS} \
                                         UTF-16 units; committing truncated"
                                    );
                                    buf.truncate(cch as usize);
                                    break buf;
                                }
                                capacity *= 4;
                            };
                            range.SetText(cookie, TF_ST_CORRECTION, &text)?;

                            let prop = context.GetProperty(&GUID_PROP_ATTRIBUTE)?;
                            prop.Clear(cookie, &range)?;

                            // shift the start of the composition
                            range.Collapse(cookie, TF_ANCHOR_END)?;
                            set_selection(&context, cookie, &range)?;

                            composition.EndComposition(cookie)?;
                            Ok(())
                        }
                    }),
                )
                .map(|_| ())
            } else {
                tracing::warn!("Composition is not started");
                Ok(())
            };

        // whether or not the TSF side could be ended, the client must let
        // go: keeping a handle to a dead composition wedges every later
        // start_composition
        text_service.borrow_mut_composition()?.tip_composition = None;

        result
    }

    #[tracing::instrument]
    pub fn set_text(&self, text: &str, subtext: &str) -> Result<()> {
        let text_service = self.borrow()?;

        if let Some(composition) = text_service.borrow_composition()?.tip_composition.clone() {
            edit_session(
                text_service.tid,
                text_service.context()?,
                Rc::new({
                    // TSF measures ranges in UTF-16 code units (like ACP
                    // offsets); chars() would undercount non-BMP characters
                    let text_len = text.encode_utf16().count() as i32;

                    // unpadded is all you need!
                    let text = format!("{text}{subtext}").as_str().to_wide_16_unpadded();
                    let context = text_service.context::<ITfContext>()?;
                    let display_attribute_atom = text_service.display_attribute_atom.clone();

                    move |cookie| unsafe {
                        let range = composition.GetRange()?;
                        range.SetText(cookie, TF_ST_CORRECTION, &text)?;

                        // first, set the display attribute to the "text" part
                        let text_range = range.Clone()?;
                        text_range.Collapse(cookie, TF_ANCHOR_START)?;
                        let mut shifted: i32 = 0;
                        text_range.ShiftEnd(cookie, text_len, &mut shifted, std::ptr::null())?;
                        let display_attribute = display_attribute_atom.get(&GUID_DISPLAY_ATTRIBUTE);
                        if let Some(display_attribute) = display_attribute {
                            let pvar = VARIANT::from(*display_attribute as i32);
                            let prop = context.GetProperty(&GUID_PROP_ATTRIBUTE)?;
                            prop.SetValue(cookie, &text_range, &pvar)?;
                        }

                        range.Collapse(cookie, TF_ANCHOR_END)?;
                        set_selection(&context, cookie, &range)?;

                        Ok(())
                    }
                }),
            )?;
        } else {
            tracing::warn!("Composition is not started");
        }

        Ok(())
    }

    #[tracing::instrument]
    pub fn shift_start(&self, text: &str, subtext: &str) -> Result<()> {
        let text_service = self.borrow()?;

        if let Some(composition) = text_service.borrow_composition()?.tip_composition.clone() {
            edit_session(
                text_service.tid,
                text_service.context()?,
                Rc::new({
                    // UTF-16 code units, not chars: a boundary computed with
                    // chars() lands inside a surrogate pair on confirm and
                    // the following SetText corrupts committed text
                    let text_len = text.encode_utf16().count() as i32;
                    let subtext = subtext.to_wide_16_unpadded();
                    let context = text_service.context::<ITfContext>()?;
                    let display_attribute_atom = text_service.display_attribute_atom.clone();

                    move |cookie| unsafe {
                        // first, shift the start of the composition
                        let range = composition.GetRange()?;
                        let mut shifted: i32 = 0;

                        // and clear the display attribute
                        let prop = context.GetProperty(&GUID_PROP_ATTRIBUTE)?;
                        prop.Clear(cookie, &range)?;

                        range.Collapse(cookie, TF_ANCHOR_START)?;
                        range.ShiftStart(cookie, text_len, &mut shifted, std::ptr::null())?;

                        composition.ShiftStart(cookie, &range)?;

                        // then, set the display attribute
                        let range = composition.GetRange()?;

                        range.SetText(cookie, TF_ST_CORRECTION, &subtext)?;

                        let display_attribute = display_attribute_atom.get(&GUID_DISPLAY_ATTRIBUTE);
                        if let Some(display_attribute) = display_attribute {
                            let pvar = VARIANT::from(*display_attribute as i32);
                            let prop = context.GetProperty(&GUID_PROP_ATTRIBUTE)?;
                            prop.SetValue(cookie, &range, &pvar)?;
                        }

                        range.Collapse(cookie, TF_ANCHOR_END)?;
                        set_selection(&context, cookie, &range)?;

                        Ok(())
                    }
                }),
            )?;
        } else {
            tracing::warn!("Composition is not started");
        }

        Ok(())
    }

    #[tracing::instrument]
    pub fn update_pos(&self) -> Result<()> {
        {
            let mut text_service = match self.borrow_mut() {
                Ok(text_service) => text_service,
                Err(error) => {
                    tracing::warn!("Skip update_pos due to borrow conflict: {error:?}");
                    return Ok(());
                }
            };

            if !text_service
                .update_pos_state
                .try_begin_update(Instant::now())
            {
                tracing::debug!("Skip re-entrant update_pos call");
                return Ok(());
            }
        }

        let result: Result<()> = (|| {
            let (tid, context, tip_composition) = {
                let text_service = self.borrow()?;
                let composition = text_service.borrow_composition()?;
                (
                    text_service.tid,
                    text_service.context::<ITfContext>()?,
                    composition.tip_composition.clone(),
                )
            };

            if let Some(tip_composition) = tip_composition {
                edit_session(
                    tid,
                    context.clone(),
                    Rc::new({
                        let context = context.clone();

                        move |cookie| unsafe {
                            let view = context.GetActiveView()?;
                            let range = tip_composition.GetRange()?;

                            let Some(mut ipc_service) = IMEState::get()?.ipc_service.clone() else {
                                return Ok(());
                            };

                            let mut rect = RECT::default();
                            let mut clipped = false.into();
                            view.GetTextExt(cookie, &range, &mut rect, &mut clipped)?;

                            ipc_service.set_window_position(
                                rect.top,
                                rect.left,
                                rect.bottom,
                                rect.right,
                            );

                            Ok(())
                        }
                    }),
                )?;
            }

            Ok(())
        })();

        match self.borrow_mut() {
            Ok(mut text_service) => {
                text_service.update_pos_state.finish_update(Instant::now());
            }
            Err(error) => {
                tracing::warn!("Failed to reset update_pos guard: {error:?}");
            }
        }

        if let Err(error) = result {
            tracing::warn!("Failed to update composition window position: {error:?}");
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::tsf::test_support::{
        factory_with_fake_context, fake_context_of, EditSessionBehavior, FakeComposition,
        FakeContext, RangeLog, FAKE_COOKIE,
    };
    use windows::Win32::UI::TextServices::ITfTextInputProcessor;

    /// A factory whose composition is live and whose ranges report into the
    /// returned RangeLog.
    fn factory_with_live_composition() -> (ITfTextInputProcessor, Rc<RangeLog>) {
        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let log = Rc::new(RangeLog::default());
        let factory = unsafe { tip.as_impl() };
        factory
            .borrow()
            .unwrap()
            .borrow_mut_composition()
            .unwrap()
            .tip_composition = Some(FakeComposition::with_log(log.clone()));
        (tip, log)
    }

    /// A deferred session (TF_S_ASYNC, DoEditSession not run) yields no
    /// result. We do not force TF_ES_SYNC or inspect phrSession, so a defer
    /// is Ok(None), not an error.
    #[test]
    fn deferred_edit_session_yields_no_result() {
        let context = FakeContext::new(EditSessionBehavior::Async);
        let result = edit_session::<()>(1, context, Rc::new(|_cookie| Ok(())));
        assert!(
            matches!(result, Ok(None)),
            "a deferred session should yield Ok(None), got {result:?}"
        );
    }

    /// The edit session must NOT be requested with TF_ES_SYNC: forcing it
    /// makes hosts like Notepad reject every request with TF_E_SYNCHRONOUS,
    /// failing all input.
    #[test]
    fn edit_session_is_not_forced_synchronous() {
        let context = FakeContext::new(EditSessionBehavior::RunSync);
        let _ = edit_session::<()>(1, context.clone(), Rc::new(|_cookie| Ok(())));
        let requests = unsafe { fake_context_of(&context) }.requests();
        assert_eq!(requests.len(), 1, "exactly one session should be requested");
        assert_eq!(
            requests[0].flags,
            TF_ES_READWRITE,
            "the session must be requested read/write only, not synchronous"
        );
    }

    /// A cooperative host: the session runs and its value comes back.
    #[test]
    fn successful_edit_session_returns_the_value() {
        let context = FakeContext::new(EditSessionBehavior::RunSync);
        let result = edit_session::<u32>(1, context, Rc::new(Ok));
        assert_eq!(
            result.unwrap(),
            Some(FAKE_COOKIE),
            "the callback's value should reach the caller"
        );
    }

    /// B7 regression (behavioral): on a Notepad-like host that rejects
    /// synchronous requests with TF_E_SYNCHRONOUS, the edit session must still
    /// succeed — because the TIP requests read/write only, never TF_ES_SYNC.
    /// `edit_session_is_not_forced_synchronous` proves the flag is absent;
    /// this proves the *consequence*, so reintroducing TF_ES_SYNC fails here
    /// (the fake would answer TF_E_SYNCHRONOUS and the callback never runs).
    #[test]
    fn edit_session_succeeds_on_a_host_that_rejects_sync() {
        let context = FakeContext::new(EditSessionBehavior::RejectSyncRequests);
        let result = edit_session::<u32>(1, context, Rc::new(Ok));
        assert_eq!(
            result.unwrap(),
            Some(FAKE_COOKIE),
            "a host that only rejects TF_ES_SYNC should still run the session"
        );
    }

    /// B5: start_composition's stale-composition recovery must not fail with
    /// a RefCell double-borrow.
    #[test]
    fn start_composition_recovers_from_a_stale_composition() {
        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let factory = unsafe { tip.as_impl() };
        factory
            .borrow()
            .unwrap()
            .borrow_mut_composition()
            .unwrap()
            .tip_composition = Some(FakeComposition::new());
        let result = factory.start_composition();
        assert!(
            result.is_ok(),
            "the stale-composition recovery path must not fail: {result:?}"
        );
        assert!(
            factory
                .borrow()
                .unwrap()
                .borrow_composition()
                .unwrap()
                .tip_composition
                .is_none(),
            "the stale composition should have been dropped"
        );
    }

    /// B6: end_composition must drop tip_composition even when the edit
    /// session fails.
    #[test]
    fn end_composition_drops_the_composition_even_when_the_session_fails() {
        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::Reject);
        let factory = unsafe { tip.as_impl() };
        factory
            .borrow()
            .unwrap()
            .borrow_mut_composition()
            .unwrap()
            .tip_composition = Some(FakeComposition::new());
        let _ = factory.end_composition();
        assert!(
            factory
                .borrow()
                .unwrap()
                .borrow_composition()
                .unwrap()
                .tip_composition
                .is_none(),
            "the client must let go of a composition it cannot end"
        );
    }

    /// B9: shift_start must measure the composition boundary in UTF-16 code
    /// units, not chars (U+20BB7 is 2 units).
    #[test]
    fn shift_start_measures_utf16_code_units() {
        let (tip, log) = factory_with_live_composition();
        let factory = unsafe { tip.as_impl() };
        factory.shift_start("\u{20BB7}", "a").expect("shift_start failed");
        assert_eq!(
            log.shift_start_reqs.borrow().as_slice(),
            &[2],
            "U+20BB7 is two UTF-16 code units"
        );
    }

    /// B9: set_text's underline extent is measured the same way.
    #[test]
    fn set_text_measures_utf16_code_units() {
        let (tip, log) = factory_with_live_composition();
        let factory = unsafe { tip.as_impl() };
        factory.set_text("\u{20BB7}", "").expect("set_text failed");
        assert_eq!(log.shift_end_reqs.borrow().as_slice(), &[2]);
    }

    /// B10: set_text must release every range it obtains (no per-keystroke
    /// ITfRange leak).
    #[test]
    fn set_text_releases_its_ranges() {
        let (tip, log) = factory_with_live_composition();
        let factory = unsafe { tip.as_impl() };
        factory.set_text("a", "b").expect("set_text failed");
        assert_eq!(log.live_ranges(), 0, "every range must be released");
    }

    /// B10: shift_start likewise.
    #[test]
    fn shift_start_releases_its_ranges() {
        let (tip, log) = factory_with_live_composition();
        let factory = unsafe { tip.as_impl() };
        factory.shift_start("a", "b").expect("shift_start failed");
        assert_eq!(log.live_ranges(), 0);
    }

    /// B10: end_composition likewise.
    #[test]
    fn end_composition_releases_its_ranges() {
        let (tip, log) = factory_with_live_composition();
        let factory = unsafe { tip.as_impl() };
        factory.end_composition().expect("end_composition failed");
        assert_eq!(log.live_ranges(), 0);
    }

    /// B23: a composition longer than the old fixed 1024-unit buffer must be
    /// written back in full on commit, not silently truncated.
    #[test]
    fn end_composition_preserves_text_longer_than_1024_units() {
        let (tip, log) = factory_with_live_composition();
        let long: Vec<u16> = "あ".encode_utf16().collect::<Vec<u16>>()
            .into_iter().cycle().take(3000).collect();
        *log.text.borrow_mut() = long.clone();

        let factory = unsafe { tip.as_impl() };
        factory.end_composition().expect("end_composition failed");

        assert_eq!(
            log.set_texts.borrow().last().expect("SetText was never called"),
            &long,
            "the full composition text must reach SetText"
        );
        assert_eq!(log.live_ranges(), 0, "every retry clone must be released");
    }

    /// B23 boundary: exactly 1024 units looks like "maybe more" (cch ==
    /// cchMax), so one retry must confirm completeness and commit all 1024.
    #[test]
    fn end_composition_handles_exactly_1024_units() {
        let (tip, log) = factory_with_live_composition();
        let text: Vec<u16> = vec!['a' as u16; 1024];
        *log.text.borrow_mut() = text.clone();

        let factory = unsafe { tip.as_impl() };
        factory.end_composition().expect("end_composition failed");

        assert_eq!(log.set_texts.borrow().last().expect("SetText was never called"), &text);
        assert_eq!(log.live_ranges(), 0);
    }

    /// B15: when the host releases its last interface reference, the whole
    /// TextService (and everything it owns) must actually be freed. The old
    /// stored `this` self-reference kept the COM refcount above zero forever,
    /// leaking one TextService + its ITfContext per profile switch. The
    /// Rc<RangeLog> sentinel inside the composition dies iff the COM box is
    /// dropped, which is exactly what the weak handle observes.
    #[test]
    fn tip_is_freed_when_the_host_releases_its_last_reference() {
        let (tip, context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let log = Rc::new(RangeLog::default());
        let sentinel = Rc::downgrade(&log);
        {
            let factory = unsafe { tip.as_impl() };
            factory
                .borrow()
                .unwrap()
                .borrow_mut_composition()
                .unwrap()
                .tip_composition = Some(FakeComposition::with_log(log));
        }
        drop(context);
        drop(tip); // the host's last Release
        assert!(
            sentinel.upgrade().is_none(),
            "the TextService must reach refcount 0 when the host lets go \
             (B15: a stored self-reference keeps it alive forever)"
        );
    }
}
