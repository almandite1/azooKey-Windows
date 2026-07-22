use macros::anyhow;
use windows::{
    core::{implement, AsImpl, VARIANT},
    Win32::{
        Foundation::RECT,
        UI::TextServices::{
            ITfComposition, ITfCompositionSink, ITfContext, ITfContextComposition, ITfEditSession,
            ITfEditSession_Impl, ITfInsertAtSelection, ITfRange, GUID_PROP_ATTRIBUTE, TF_AE_NONE,
            TF_ANCHOR_END, TF_ANCHOR_START, TF_DEFAULT_SELECTION, TF_ES_READWRITE,
            TF_IAS_QUERYONLY, TF_SELECTION, TF_SELECTIONSTYLE, TF_ST_CORRECTION, TF_TF_MOVESTART,
            TS_E_NOLAYOUT,
        },
    },
};

use std::{cell::Cell, mem::ManuallyDrop, rc::Rc, time::Instant};

use anyhow::Result;

use crate::{engine::state::IMEState, extension::StringExt as _, globals::GUID_DISPLAY_ATTRIBUTE};

use super::factory::TextServiceFactory;

/// Outcome of one `update_pos` attempt. `PendingLayout` and `Clipped` are
/// normal transient states, not errors: the candidate window keeps its current
/// position and a later `OnLayoutChange` retries. We deliberately do *not*
/// hide the window on either — `show_window` is only ever sent from
/// `ClientAction::StartComposition` (engine/composition.rs), so hiding
/// mid-composition would leave the candidates invisible for the rest of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PositionUpdate {
    Sent,
    PendingLayout,
    Clipped,
    NoIpc,
}

/// Classifies what `GetTextExt` told us. Split out from the edit-session
/// closure so the three-way decision can be tested without a live host.
///
/// `TS_E_NOLAYOUT` is *not* a failure: the host has no layout for this range
/// yet and will call `OnLayoutChange` when it does. Every other HRESULT is a
/// genuine error and keeps propagating.
fn classify_text_ext(
    measured: windows::core::Result<()>,
    clipped: windows::Win32::Foundation::BOOL,
) -> Result<PositionUpdate> {
    if let Err(error) = measured {
        if error.code() == TS_E_NOLAYOUT {
            return Ok(PositionUpdate::PendingLayout);
        }
        return Err(error.into());
    }

    if clipped.as_bool() {
        return Ok(PositionUpdate::Clipped);
    }

    Ok(PositionUpdate::Sent)
}

/// Measures `range` on the context's active view and publishes the rect as
/// the caret position. Shared by the two callers that have a range worth
/// reporting: the composition (`update_pos`) and, with none, the selection
/// (`update_pos_from_selection`).
///
/// Must run inside an edit session — `cookie` is what proves that.
fn send_range_position(
    context: &ITfContext,
    cookie: u32,
    range: &ITfRange,
) -> Result<PositionUpdate> {
    let Some(mut ipc_service) = IMEState::get()?.ipc_service.clone() else {
        return Ok(PositionUpdate::NoIpc);
    };

    unsafe {
        let view = context.GetActiveView()?;

        let mut rect = RECT::default();
        let mut clipped = false.into();
        let measured = view.GetTextExt(cookie, range, &mut rect, &mut clipped);

        let outcome = classify_text_ext(measured, clipped)?;
        if outcome == PositionUpdate::Sent {
            ipc_service.set_window_position(rect.top, rect.left, rect.bottom, rect.right);
        }

        Ok(outcome)
    }
}

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
///
/// `selected_range` is the receiving-direction pair; every `TF_SELECTION`
/// crossing the TSF boundary goes through one of the two.
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

/// Reads the FULL text of `range`. GetText fills at most the buffer, so a
/// full buffer means "maybe more" — retry from a fresh clone with a larger
/// one (B23: a fixed 1024 buffer silently truncated longer compositions on
/// commit). cch < capacity proves completeness.
fn read_range_text(range: &ITfRange, cookie: u32) -> windows::core::Result<Vec<u16>> {
    const MAX_COMPOSITION_UNITS: usize = 1 << 20;
    let mut capacity: usize = 1024;
    loop {
        let mut buf = vec![0u16; capacity];
        let mut cch: u32 = 0;

        // fresh clone each attempt: TF_TF_MOVESTART moves the previous
        // clone's start anchor
        unsafe {
            let probe = range.Clone()?;
            probe.GetText(cookie, TF_TF_MOVESTART, &mut buf, &mut cch)?;
        }

        if (cch as usize) < capacity {
            buf.truncate(cch as usize);
            return Ok(buf);
        }
        if capacity >= MAX_COMPOSITION_UNITS {
            tracing::warn!(
                "composition text exceeds {MAX_COMPOSITION_UNITS} UTF-16 units; \
                 committing truncated"
            );
            buf.truncate(cch as usize);
            return Ok(buf);
        }
        capacity *= 4;
    }
}

/// Marks `range` with the composition's display attribute (the conversion
/// underline) when the atom was registered on Activate.
fn apply_display_attribute(
    context: &ITfContext,
    cookie: u32,
    range: &ITfRange,
    display_attribute_atom: &std::collections::HashMap<windows::core::GUID, u32>,
) -> windows::core::Result<()> {
    if let Some(atom) = display_attribute_atom.get(&GUID_DISPLAY_ATTRIBUTE) {
        let pvar = VARIANT::from(*atom as i32);
        unsafe {
            let prop = context.GetProperty(&GUID_PROP_ATTRIBUTE)?;
            prop.SetValue(cookie, range, &pvar)?;
        }
    }
    Ok(())
}

/// Places the caret at the end of `range` (collapse + select) — the common
/// tail of every text-writing edit session.
fn caret_to_end(context: &ITfContext, cookie: u32, range: &ITfRange) -> windows::core::Result<()> {
    unsafe { range.Collapse(cookie, TF_ANCHOR_END)? };
    set_selection(context, cookie, range)
}

/// Fetches the host's current selection and takes ownership of its range.
/// `GetSelection` is [out]: the range arrives AddRef'd inside a
/// `ManuallyDrop`, so failing to take it leaked one host-side range per
/// keystroke (B10). Returns `None` when the host reports no selection.
pub(super) fn selected_range(
    context: &ITfContext,
    cookie: u32,
) -> windows::core::Result<Option<ITfRange>> {
    let mut pselection: [TF_SELECTION; 1] = [TF_SELECTION::default()];
    let mut pfetched = 0;
    unsafe {
        context.GetSelection(cookie, TF_DEFAULT_SELECTION, &mut pselection, &mut pfetched)?;
    }
    if pfetched == 0 {
        return Ok(None);
    }
    let [selection] = pselection;
    Ok(ManuallyDrop::into_inner(selection.range))
}

impl TextServiceFactory {
    /// Shared skeleton for edit sessions that operate on the LIVE
    /// composition: borrows the service and, when no composition is
    /// started, warns and no-ops — a missing composition is a normal race
    /// (e.g. the host tore it down first), not an error. `build` receives
    /// the borrowed service and the live composition handle, captures what
    /// the session body needs, and returns the body to run.
    ///
    /// Every composition-mutating path (set_text / shift_start /
    /// end_composition — and future ones like MoveCursor) goes through
    /// here, so the guard, the tid/context plumbing and the warn stay in
    /// one place.
    fn with_live_composition(
        &self,
        build: impl FnOnce(
            &crate::tsf::text_service::TextService,
            ITfComposition,
        ) -> Result<Rc<dyn Fn(u32) -> Result<()>>>,
    ) -> Result<()> {
        let text_service = self.borrow()?;
        let Some(composition) = text_service.borrow_composition()?.tip_composition.clone() else {
            tracing::warn!("Composition is not started");
            return Ok(());
        };

        let callback = build(&text_service, composition)?;
        edit_session(text_service.tid, text_service.context()?, callback)?;
        Ok(())
    }

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
            // wedge the input path — letting go of it is what matters.
            // Then fall through to start a FRESH composition: returning
            // Ok here left the caller (the StartComposition arm) believing
            // a composition already existed, so the following set_text
            // no-op'd ("Composition is not started") and everything typed
            // into that composition was invisible until the next recovery.
            if let Err(error) = self.end_composition() {
                tracing::warn!("Failed to end a stale composition: {error:?}");
            }
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

        let result = self.with_live_composition(|text_service, composition| {
            let context = text_service.context::<ITfContext>()?;

            Ok(Rc::new(move |cookie: u32| unsafe {
                let range: ITfRange = composition.GetRange()?;

                // re-write the full text without the composition's
                // display attribute (B23: read it whole, not the
                // first 1024 units)
                let text = read_range_text(&range, cookie)?;
                range.SetText(cookie, TF_ST_CORRECTION, &text)?;

                let prop = context.GetProperty(&GUID_PROP_ATTRIBUTE)?;
                prop.Clear(cookie, &range)?;

                caret_to_end(&context, cookie, &range)?;

                composition.EndComposition(cookie)?;
                Ok(())
            }) as Rc<dyn Fn(u32) -> Result<()>>)
        });

        // whether or not the TSF side could be ended, the client must let
        // go: keeping a handle to a dead composition wedges every later
        // start_composition
        self.borrow()?.borrow_mut_composition()?.tip_composition = None;

        result
    }

    #[tracing::instrument]
    pub fn set_text(&self, text: &str, subtext: &str) -> Result<()> {
        self.with_live_composition(|text_service, composition| {
            // TSF measures ranges in UTF-16 code units (like ACP
            // offsets); chars() would undercount non-BMP characters
            let text_len = text.encode_utf16().count() as i32;

            // unpadded is all you need!
            let text = format!("{text}{subtext}").as_str().to_wide_16_unpadded();
            let context = text_service.context::<ITfContext>()?;
            let display_attribute_atom = text_service.display_attribute_atom.clone();

            Ok(Rc::new(move |cookie: u32| unsafe {
                let range = composition.GetRange()?;
                range.SetText(cookie, TF_ST_CORRECTION, &text)?;

                // mark only the "text" part (not the subtext) with
                // the display attribute
                let text_range = range.Clone()?;
                text_range.Collapse(cookie, TF_ANCHOR_START)?;
                let mut shifted: i32 = 0;
                text_range.ShiftEnd(cookie, text_len, &mut shifted, std::ptr::null())?;
                apply_display_attribute(&context, cookie, &text_range, &display_attribute_atom)?;

                caret_to_end(&context, cookie, &range)?;

                Ok(())
            }) as Rc<dyn Fn(u32) -> Result<()>>)
        })
    }

    #[tracing::instrument]
    pub fn shift_start(&self, text: &str, subtext: &str) -> Result<()> {
        self.with_live_composition(|text_service, composition| {
            // UTF-16 code units, not chars: a boundary computed with
            // chars() lands inside a surrogate pair on confirm and
            // the following SetText corrupts committed text
            let text_len = text.encode_utf16().count() as i32;
            let subtext = subtext.to_wide_16_unpadded();
            let context = text_service.context::<ITfContext>()?;
            let display_attribute_atom = text_service.display_attribute_atom.clone();

            Ok(Rc::new(move |cookie: u32| unsafe {
                // first, shift the start of the composition
                let range = composition.GetRange()?;
                let mut shifted: i32 = 0;

                // and clear the display attribute
                let prop = context.GetProperty(&GUID_PROP_ATTRIBUTE)?;
                prop.Clear(cookie, &range)?;

                range.Collapse(cookie, TF_ANCHOR_START)?;
                range.ShiftStart(cookie, text_len, &mut shifted, std::ptr::null())?;

                composition.ShiftStart(cookie, &range)?;

                // then, write the remaining subtext and re-mark it
                let range = composition.GetRange()?;
                range.SetText(cookie, TF_ST_CORRECTION, &subtext)?;
                apply_display_attribute(&context, cookie, &range, &display_attribute_atom)?;

                caret_to_end(&context, cookie, &range)?;

                Ok(())
            }) as Rc<dyn Fn(u32) -> Result<()>>)
        })
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
                let outcome = edit_session(
                    tid,
                    context.clone(),
                    Rc::new({
                        let context = context.clone();

                        move |cookie| {
                            let range = unsafe { tip_composition.GetRange()? };
                            send_range_position(&context, cookie, &range)
                        }
                    }),
                )?;

                match outcome {
                    Some(PositionUpdate::PendingLayout) => {
                        tracing::debug!("Layout not ready yet; waiting for OnLayoutChange")
                    }
                    Some(PositionUpdate::Clipped) => {
                        tracing::debug!("Composition rect is clipped; keeping the current position")
                    }
                    _ => {}
                }
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

    /// Publishes the CARET position measured from the current selection.
    ///
    /// `update_pos` only ever measures the composition range, so with no
    /// composition there is no fresh position at all and the mode indicator
    /// flashes wherever the last composition happened to leave it — a stale
    /// spot, or the screen origin if nothing has been composed yet (issue
    /// #55). Switching input mode outside a composition is exactly when that
    /// happens.
    ///
    /// Advisory, like `update_pos`: every failure is logged and swallowed.
    /// Nothing here may break typing.
    #[tracing::instrument]
    pub fn update_pos_from_selection(&self) -> Result<()> {
        let result: Result<()> = (|| {
            let (tid, context, composing) = {
                let text_service = self.borrow()?;
                let composition = text_service.borrow_composition()?;
                // No focused document yet — Activate adopts the OS mode
                // before any context exists. Normal, not a failure, so it
                // must not reach the warn below.
                let Ok(context) = text_service.context::<ITfContext>() else {
                    tracing::debug!("No context yet; skipping the caret position update");
                    return Ok(());
                };
                (
                    text_service.tid,
                    context,
                    composition.tip_composition.is_some(),
                )
            };

            // While composing, the composition range is the better anchor and
            // update_pos already keeps it current — measuring the selection
            // instead would fight it.
            if composing {
                return Ok(());
            }

            let outcome = edit_session(
                tid,
                context.clone(),
                Rc::new({
                    let context = context.clone();

                    move |cookie| {
                        let Some(range) = selected_range(&context, cookie)? else {
                            // no selection to anchor to (an empty document
                            // view, or a host that reports none)
                            return Ok(PositionUpdate::NoIpc);
                        };
                        send_range_position(&context, cookie, &range)
                    }
                }),
            )?;

            if let Some(PositionUpdate::PendingLayout) = outcome {
                tracing::debug!("Caret layout not ready yet; keeping the current position");
            }

            Ok(())
        })();

        if let Err(error) = result {
            tracing::warn!("Failed to update the caret position: {error:?}");
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::engine::ipc_service::IPCService;
    use crate::tsf::test_support::{
        factory_with_context, factory_with_fake_context, fake_context_of, global_state_lock,
        EditSessionBehavior, FakeComposition, FakeContext, RangeLog, TextExtBehavior, FAKE_COOKIE,
    };
    use windows::Win32::Foundation::E_FAIL;
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
            requests[0].flags, TF_ES_READWRITE,
            "the session must be requested read/write only, not synchronous"
        );
    }

    /// Drives update_pos end to end against the fake view: the composition
    /// range must be measured on the context's active view (GetTextExt) and
    /// every range involved must be released. This path was untestable
    /// before the fake modeled GetActiveView.
    #[test]
    fn update_pos_measures_the_composition_via_the_active_view() {
        let _guard = global_state_lock();
        let log = Rc::new(RangeLog::default());
        let context = FakeContext::with_ranges(EditSessionBehavior::RunSync, log.clone());
        let tip = factory_with_context(context.clone());
        let factory: &TextServiceFactory = unsafe { tip.as_impl() };
        factory
            .borrow()
            .unwrap()
            .borrow_mut_composition()
            .unwrap()
            .tip_composition = Some(FakeComposition::with_log(log.clone()));
        // the rect is only measured when there is an IPC service to report
        // it to; the send itself may fail (no UI process) and is logged only
        IMEState::get().unwrap().ipc_service = Some(IPCService::new().unwrap());

        factory.update_pos().unwrap();

        let view = unsafe { fake_context_of(&context) }.view_log();
        assert_eq!(
            view.get_text_ext_calls.get(),
            1,
            "the composition rect must be measured on the active view"
        );
        assert_eq!(log.live_ranges(), 0, "no range may leak from update_pos");
        IMEState::get().unwrap().ipc_service = None;
    }

    /// Issue #55: with no composition there is nothing for `update_pos` to
    /// measure, so a mode switch outside one had no position to flash the
    /// indicator at. The selection is the anchor in that case, and it must
    /// be measured on the active view like any other range.
    #[test]
    fn the_caret_position_comes_from_the_selection_without_a_composition() {
        let _guard = global_state_lock();
        let log = Rc::new(RangeLog::default());
        let context = FakeContext::with_ranges(EditSessionBehavior::RunSync, log.clone());
        let tip = factory_with_context(context.clone());
        let factory: &TextServiceFactory = unsafe { tip.as_impl() };
        // deliberately NO tip_composition: this is the non-composing case
        IMEState::get().unwrap().ipc_service = Some(IPCService::new().unwrap());

        factory.update_pos_from_selection().unwrap();

        let view = unsafe { fake_context_of(&context) }.view_log();
        assert_eq!(
            view.get_text_ext_calls.get(),
            1,
            "the selection must be measured on the active view"
        );
        assert_eq!(
            log.live_ranges(),
            0,
            "the range GetSelection handed out must be released"
        );
        IMEState::get().unwrap().ipc_service = None;
    }

    /// While composing, `update_pos` owns the position and measures the
    /// composition range. Measuring the selection too would fight it, so the
    /// selection path must stand down.
    #[test]
    fn the_selection_is_not_measured_while_composing() {
        let _guard = global_state_lock();
        let log = Rc::new(RangeLog::default());
        let context = FakeContext::with_ranges(EditSessionBehavior::RunSync, log.clone());
        let tip = factory_with_context(context.clone());
        let factory: &TextServiceFactory = unsafe { tip.as_impl() };
        factory
            .borrow()
            .unwrap()
            .borrow_mut_composition()
            .unwrap()
            .tip_composition = Some(FakeComposition::with_log(log.clone()));
        IMEState::get().unwrap().ipc_service = Some(IPCService::new().unwrap());

        factory.update_pos_from_selection().unwrap();

        let view = unsafe { fake_context_of(&context) }.view_log();
        assert_eq!(
            view.get_text_ext_calls.get(),
            0,
            "update_pos already anchors to the composition while it is live"
        );
        IMEState::get().unwrap().ipc_service = None;
    }

    /// Activate adopts the OS compartment mode before any document has
    /// focus, so this path runs with no context on every activation. That is
    /// normal: it must no-op quietly rather than warn.
    #[test]
    fn no_context_is_not_a_failure_for_the_caret_position() {
        let _guard = global_state_lock();
        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let factory: &TextServiceFactory = unsafe { tip.as_impl() };
        factory.borrow_mut().unwrap().context = None;

        factory
            .update_pos_from_selection()
            .expect("a missing context is a skip, not an error");
    }

    /// C-1: `TS_E_NOLAYOUT` means "layout is not ready yet, wait for
    /// `OnLayoutChange`" — a normal transient state every host produces, not
    /// a failure. Before this, it was collapsed into the generic error warn.
    #[test]
    fn a_pending_layout_is_not_an_error() {
        let outcome = classify_text_ext(Err(TS_E_NOLAYOUT.into()), false.into())
            .expect("NOLAYOUT is a transient state, not a failure");
        assert_eq!(outcome, PositionUpdate::PendingLayout);
    }

    /// Only NOLAYOUT gets that treatment; a real failure must still propagate
    /// so it is logged rather than silently swallowed.
    #[test]
    fn other_hresults_still_propagate_as_errors() {
        let outcome = classify_text_ext(Err(E_FAIL.into()), false.into());
        assert!(
            outcome.is_err(),
            "a genuine GetTextExt failure must not be mistaken for a pending layout"
        );
    }

    /// C-1: a clipped rect is not where the text actually is, so it must not
    /// be forwarded as a window position — that would move the candidate
    /// window somewhere the caret is not.
    #[test]
    fn a_clipped_rect_is_not_forwarded() {
        let outcome = classify_text_ext(Ok(()), true.into()).unwrap();
        assert_eq!(
            outcome,
            PositionUpdate::Clipped,
            "a clipped rect must not reach set_window_position"
        );
    }

    /// The ordinary path still sends the position.
    #[test]
    fn an_unclipped_measurement_is_sent() {
        let outcome = classify_text_ext(Ok(()), false.into()).unwrap();
        assert_eq!(outcome, PositionUpdate::Sent);
    }

    /// The transient answers must still release the range they measured.
    #[test]
    fn update_pos_leaks_no_range_when_layout_is_pending() {
        let _guard = global_state_lock();
        let log = Rc::new(RangeLog::default());
        let context = FakeContext::with_ranges(EditSessionBehavior::RunSync, log.clone());
        let tip = factory_with_context(context.clone());
        let factory: &TextServiceFactory = unsafe { tip.as_impl() };
        factory
            .borrow()
            .unwrap()
            .borrow_mut_composition()
            .unwrap()
            .tip_composition = Some(FakeComposition::with_log(log.clone()));
        IMEState::get().unwrap().ipc_service = Some(IPCService::new().unwrap());

        let view = unsafe { fake_context_of(&context) }.view_log();
        view.text_ext_behavior.set(TextExtBehavior::NoLayout);

        factory.update_pos().unwrap();

        assert_eq!(
            view.get_text_ext_calls.get(),
            1,
            "the view must still be measured; NOLAYOUT is the answer, not a skip"
        );
        assert_eq!(
            log.live_ranges(),
            0,
            "a NOLAYOUT answer must not leak the measured range"
        );
        IMEState::get().unwrap().ipc_service = None;
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
    /// a RefCell double-borrow. And after dropping the dead composition it
    /// must start a FRESH one rather than returning early: returning left the
    /// StartComposition caller thinking a composition existed, so set_text
    /// no-op'd and the typing that followed was invisible.
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
                .is_some(),
            "recovery must leave a fresh live composition, not return early \
             with none (which made the next set_text a silent no-op)"
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
        factory
            .shift_start("\u{20BB7}", "a")
            .expect("shift_start failed");
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
        let long: Vec<u16> = "あ"
            .encode_utf16()
            .collect::<Vec<u16>>()
            .into_iter()
            .cycle()
            .take(3000)
            .collect();
        *log.text.borrow_mut() = long.clone();

        let factory = unsafe { tip.as_impl() };
        factory.end_composition().expect("end_composition failed");

        assert_eq!(
            log.set_texts
                .borrow()
                .last()
                .expect("SetText was never called"),
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

        assert_eq!(
            log.set_texts
                .borrow()
                .last()
                .expect("SetText was never called"),
            &text
        );
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
