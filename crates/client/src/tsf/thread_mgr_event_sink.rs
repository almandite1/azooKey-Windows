use windows::Win32::UI::TextServices::{ITfContext, ITfDocumentMgr, ITfThreadMgrEventSink_Impl};

use anyhow::Result;

use crate::engine::{client_action::ClientAction, composition::CompositionState};

use super::factory::TextServiceFactory_Impl;

impl ITfThreadMgrEventSink_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    fn OnInitDocumentMgr(&self, _pdim: Option<&ITfDocumentMgr>) -> Result<()> {
        Ok(())
    }

    #[macros::anyhow]
    fn OnUninitDocumentMgr(&self, _pdim: Option<&ITfDocumentMgr>) -> Result<()> {
        Ok(())
    }

    #[macros::anyhow]
    fn OnSetFocus(
        &self,
        focus: Option<&ITfDocumentMgr>,
        _prevfocus: Option<&ITfDocumentMgr>,
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
        if let Some(focus) = focus {
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
    fn OnPushContext(&self, _pic: Option<&ITfContext>) -> Result<()> {
        Ok(())
    }

    #[macros::anyhow]
    fn OnPopContext(&self, _pic: Option<&ITfContext>) -> Result<()> {
        Ok(())
    }
}
