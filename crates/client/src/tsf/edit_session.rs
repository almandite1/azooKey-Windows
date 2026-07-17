use macros::anyhow;
use windows::{
    core::{implement, AsImpl, VARIANT},
    Win32::{
        Foundation::RECT,
        UI::TextServices::{
            ITfComposition, ITfCompositionSink, ITfContext, ITfContextComposition, ITfEditSession,
            ITfEditSession_Impl, ITfInsertAtSelection, ITfRange, GUID_PROP_ATTRIBUTE, TF_AE_NONE,
            TF_ANCHOR_END, TF_ANCHOR_START, TF_ES_READWRITE, TF_ES_SYNC, TF_IAS_QUERYONLY,
            TF_SELECTION, TF_SELECTIONSTYLE, TF_ST_CORRECTION, TF_TF_MOVESTART,
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

    // key handling is synchronous: the caller needs the session's outcome
    // before it returns to the host, so an asynchronous grant is useless
    let result =
        unsafe { context.RequestEditSession(client_id, &session, TF_ES_SYNC | TF_ES_READWRITE) };

    let hr = result.map_err(anyhow::Error::new)?;
    // RequestEditSession succeeding only means the request was delivered;
    // the session's own outcome comes back through phrSession
    hr.ok().map_err(anyhow::Error::new)?;

    let session = unsafe { session.as_impl() };
    match session.result.take() {
        Some(value) => Ok(Some(value)),
        // a success HRESULT with no result means the host deferred the
        // session (TF_S_ASYNC) and DoEditSession never ran
        None => anyhow::bail!("edit session was accepted but did not run synchronously"),
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
        let sink = text_service.this::<ITfCompositionSink>()?;
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

                            // set existing text to the composition
                            let mut text = vec![0; 1024];
                            let mut text_len = 1024;

                            let range_new = range.Clone()?;
                            range_new.GetText(cookie, TF_TF_MOVESTART, &mut text, &mut text_len)?;

                            text = text[..text_len as usize].to_vec();
                            range.SetText(cookie, TF_ST_CORRECTION, &text)?;

                            let prop = context.GetProperty(&GUID_PROP_ATTRIBUTE)?;
                            prop.Clear(cookie, &range)?;

                            // shift the start of the composition
                            range.Collapse(cookie, TF_ANCHOR_END)?;
                            let selection = TF_SELECTION {
                                range: ManuallyDrop::new(Some(range.clone())),
                                style: TF_SELECTIONSTYLE {
                                    ase: TF_AE_NONE,
                                    fInterimChar: false.into(),
                                },
                            };

                            context.SetSelection(cookie, &[selection])?;

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
                    let text_len = text.chars().count() as i32;

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
                        let selection = TF_SELECTION {
                            range: ManuallyDrop::new(Some(range.clone())),
                            style: TF_SELECTIONSTYLE {
                                ase: TF_AE_NONE,
                                fInterimChar: false.into(),
                            },
                        };

                        context.SetSelection(cookie, &[selection])?;

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
                    let text_len = text.chars().count() as i32;
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
                        let selection = TF_SELECTION {
                            range: ManuallyDrop::new(Some(range)),
                            style: TF_SELECTIONSTYLE {
                                ase: TF_AE_NONE,
                                fInterimChar: false.into(),
                            },
                        };

                        context.SetSelection(cookie, &[selection])?;

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
        FakeContext, FAKE_COOKIE,
    };
    use windows::Win32::UI::TextServices::TF_ES_SYNC;

    /// B7: a denied session must surface as an error.
    ///
    /// TSF reports a refused edit session through `phrSession` — the *inner*
    /// HRESULT — while `RequestEditSession` itself still returns `S_OK`.
    /// `edit_session` only matches on the outer `Result`, so it reports
    /// success for a session that never ran, and the caller silently drops
    /// the user's keystroke.
    #[test]
    fn denied_edit_session_is_an_error() {
        let context = FakeContext::new(EditSessionBehavior::DenyViaSessionResult);

        let result = edit_session::<()>(1, context, Rc::new(|_cookie| Ok(())));

        assert!(
            result.is_err(),
            "a session the host refused must not be reported as success, \
             got {result:?}"
        );
    }

    /// B7: a deferred session must surface as an error rather than a silent
    /// `None`. The host may only defer because the TIP does not ask for
    /// `TF_ES_SYNC` (see `sync_edit_session_is_requested`).
    #[test]
    fn deferred_edit_session_is_an_error() {
        let context = FakeContext::new(EditSessionBehavior::Async);

        let result = edit_session::<()>(1, context, Rc::new(|_cookie| Ok(())));

        assert!(
            result.is_err(),
            "a session the host deferred produced no result, so it must not \
             be reported as success, got {result:?}"
        );
    }

    /// B7: keystroke handling is synchronous, so the edit session must be
    /// requested synchronously. Without `TF_ES_SYNC` the host is free to
    /// defer, which is what makes `deferred_edit_session_is_an_error`
    /// reachable in the first place.
    #[test]
    fn sync_edit_session_is_requested() {
        let context = FakeContext::new(EditSessionBehavior::RunSync);

        let _ = edit_session::<()>(1, context.clone(), Rc::new(|_cookie| Ok(())));

        let requests = unsafe { fake_context_of(&context) }.requests();
        assert_eq!(requests.len(), 1, "exactly one session should be requested");
        assert!(
            requests[0].flags.0 & TF_ES_SYNC.0 != 0,
            "the edit session must be requested with TF_ES_SYNC, got {:?}",
            requests[0].flags
        );
    }

    /// A cooperative host still works: the session runs and its value comes
    /// back. Guards the fix to B7 against over-correction.
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

    /// B5: `start_composition` takes a `RefMut` on the text service and then,
    /// if a composition is already live, calls `end_composition` — which takes
    /// a second borrow of the same `RefCell`. The `RefMut` is still alive, so
    /// the recovery path can only ever fail with a borrow error.
    ///
    /// This is the path taken after a composition survives a focus change, so
    /// in practice the IME stops accepting input until it is restarted.
    #[test]
    fn start_composition_recovers_from_a_stale_composition() {
        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let factory = unsafe { tip.as_impl() };

        // a composition left over from a previous, half-torn-down session
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

    /// B6: if the edit session fails, `end_composition` returns early and
    /// leaves `tip_composition` set. The TSF-side composition is gone (or
    /// unreachable) but the client still believes one is live, which is what
    /// feeds the stale composition into B5 on the next keystroke.
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
            "the client must let go of a composition it cannot end, or it \
             will keep trying to reuse a dead one"
        );
    }
}
