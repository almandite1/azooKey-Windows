use windows::Win32::UI::TextServices::{ITfContext, ITfDocumentMgr, ITfThreadMgrEventSink_Impl};

use anyhow::Result;

use crate::engine::{client_action::ClientAction, composition::CompositionState};

use super::factory::TextServiceFactory_Impl;

impl ITfThreadMgrEventSink_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    fn OnInitDocumentMgr(&self, _pdim: windows_core::Ref<'_, ITfDocumentMgr>) -> Result<()> {
        Ok(())
    }

    #[macros::anyhow]
    fn OnUninitDocumentMgr(&self, _pdim: windows_core::Ref<'_, ITfDocumentMgr>) -> Result<()> {
        Ok(())
    }

    #[macros::anyhow]
    fn OnSetFocus(
        &self,
        focus: windows_core::Ref<'_, ITfDocumentMgr>,
        _prevfocus: windows_core::Ref<'_, ITfDocumentMgr>,
    ) -> Result<()> {
        // A focus change must ALWAYS end the composition on the document
        // being left; the advisory re-wiring below (language bar, text
        // layout sink) must never be able to skip it. The old order ran
        // both advisory steps first with `?`, so a failure there — common
        // when the previous document is mid-teardown, which is exactly when
        // focus moves — left the composition and its uncommitted reading
        // alive in the previous app.
        let end_result =
            self.handle_action(&[ClientAction::EndComposition], CompositionState::None);

        // advisory: the language bar icon is cosmetic, a failure must not
        // break input (CLAUDE.md error-handling rule)
        if let Err(error) = self.update_lang_bar() {
            tracing::warn!("OnSetFocus: failed to update the language bar: {error:?}");
        }

        // advisory: re-advise the text layout sink on the newly focused
        // document so the candidate window tracks its caret. A failure here
        // (e.g. the new document has no top context yet) must not break input
        if let Some(focus) = focus.as_ref() {
            let advise = (|| {
                let mut text_service = self.borrow_mut()?;
                self.advise_text_layout_sink(&mut text_service, focus.clone())
            })();
            if let Err(error) = advise {
                tracing::warn!("OnSetFocus: failed to advise the text layout sink: {error:?}");
            }
        }

        // surface the composition-teardown error only after the advisory
        // re-wiring ran
        end_result
    }

    #[macros::anyhow]
    fn OnPushContext(&self, _pic: windows_core::Ref<'_, ITfContext>) -> Result<()> {
        Ok(())
    }

    #[macros::anyhow]
    fn OnPopContext(&self, _pic: windows_core::Ref<'_, ITfContext>) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    use windows::Win32::UI::TextServices::{
        ITfContext, ITfTextInputProcessor, ITfThreadMgrEventSink,
    };

    use crate::engine::ipc_service::{FakeIpc, IPCService, IpcCall};
    use crate::engine::state::IMEState;
    use crate::tsf::test_support::{
        EditSessionBehavior, FakeContext, FakeDocumentMgr, FakeThreadMgr, ThreadMgrLog, factory_of,
        factory_with_context, fake_context_of, global_state_lock,
    };

    /// A TIP wired to a fake context, a fake thread manager, and a recording
    /// IPC service — everything `OnSetFocus` reaches. `langbar_fails` picks
    /// the host that makes the advisory half of the callback fail.
    fn tip_for_focus(
        behavior: EditSessionBehavior,
        langbar_fails: bool,
    ) -> (
        ITfTextInputProcessor,
        ITfContext,
        Rc<ThreadMgrLog>,
        Arc<Mutex<FakeIpc>>,
    ) {
        let (service, fake) = IPCService::new_fake().unwrap();
        IMEState::get().unwrap().ipc_service = Some(service);

        let context = FakeContext::new(behavior);
        let tip = factory_with_context(context.clone());
        let log = Rc::new(ThreadMgrLog::default());
        let thread_mgr = if langbar_fails {
            FakeThreadMgr::with_failing_langbar(log.clone())
        } else {
            FakeThreadMgr::new(log.clone())
        };
        {
            let factory = factory_of(&tip);
            let mut text_service = factory.borrow_mut().unwrap();
            text_service.thread_mgr = Some(thread_mgr);
        }
        (tip, context, log, fake)
    }

    fn ipc_calls(fake: &Arc<Mutex<FakeIpc>>) -> Vec<IpcCall> {
        fake.lock().unwrap().calls.clone()
    }

    /// Put a composition in flight so `EndComposition` has something to tear
    /// down, and its IPC traffic proves the arm ran.
    fn start_composing(tip: &ITfTextInputProcessor) {
        use crate::engine::composition::CompositionState;

        let factory = factory_of(tip);
        let text_service = factory.borrow().unwrap();
        let mut composition = text_service.borrow_mut_composition().unwrap();
        composition.set_up_for_test(CompositionState::Composing);
        composition.preview = "みず".to_string();
    }

    /// The regression the ordering comment describes: focus is leaving a
    /// document, and re-wiring the language bar for the NEW one fails —
    /// routine, because focus moves precisely when the old document is being
    /// torn down. The composition on the document being left must still end,
    /// or its uncommitted reading stays alive in the previous application.
    #[test]
    fn a_failing_langbar_does_not_skip_the_composition_teardown() {
        let _guard = global_state_lock();
        let (tip, _context, log, fake) = tip_for_focus(EditSessionBehavior::RunSync, true);
        start_composing(&tip);

        let sink: ITfThreadMgrEventSink = windows::core::Interface::cast(&tip).unwrap();
        let result = unsafe { sink.OnSetFocus(None, None) };

        assert!(log.langbar_adds.get() > 0, "the langbar update was tried");
        assert!(result.is_ok(), "a cosmetic failure must not break input");
        let calls = ipc_calls(&fake);
        assert!(
            calls.contains(&IpcCall::HideWindow) && calls.contains(&IpcCall::ClearText),
            "the composition must have been torn down anyway: {calls:?}"
        );

        IMEState::get().unwrap().ipc_service = None;
    }

    /// The other advisory step, and the one that runs last: re-advising the
    /// text layout sink on the newly focused document. It must not be able to
    /// swallow the teardown either — and it does run, so the candidate window
    /// tracks the new document's caret.
    #[test]
    fn the_layout_sink_is_readvised_on_the_newly_focused_document() {
        let _guard = global_state_lock();
        let (tip, context, _log, fake) = tip_for_focus(EditSessionBehavior::RunSync, false);
        start_composing(&tip);
        let focus = FakeDocumentMgr::new(context.clone());

        let sink: ITfThreadMgrEventSink = windows::core::Interface::cast(&tip).unwrap();
        unsafe { sink.OnSetFocus(Some(&focus), None) }.unwrap();

        assert_eq!(
            unsafe { fake_context_of(&context) }.sink_advises().len(),
            1,
            "the new document's context must be advised"
        );
        let calls = ipc_calls(&fake);
        assert!(calls.contains(&IpcCall::HideWindow), "{calls:?}");

        IMEState::get().unwrap().ipc_service = None;
    }

    /// A teardown that genuinely failed is still reported — but only after
    /// the advisory re-wiring has run. A host that refuses the edit session
    /// (a disconnected or read-only document, which is what focus is leaving)
    /// must not cost the newly focused document its language bar or its
    /// layout sink.
    #[test]
    fn a_failed_teardown_is_reported_but_only_after_the_rewiring_ran() {
        let _guard = global_state_lock();
        let (tip, context, log, _fake) = tip_for_focus(EditSessionBehavior::Reject, false);
        start_composing(&tip);
        let focus = FakeDocumentMgr::new(context.clone());

        let sink: ITfThreadMgrEventSink = windows::core::Interface::cast(&tip).unwrap();
        let result = unsafe { sink.OnSetFocus(Some(&focus), None) };

        assert!(result.is_err(), "the teardown failure must surface");
        assert!(log.langbar_adds.get() > 0, "the langbar still refreshed");
        assert_eq!(
            unsafe { fake_context_of(&context) }.sink_advises().len(),
            1,
            "and the layout sink was still advised"
        );

        IMEState::get().unwrap().ipc_service = None;
    }
}
