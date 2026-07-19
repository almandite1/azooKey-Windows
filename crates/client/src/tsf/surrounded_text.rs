// reference to the original code:
// https://github.com/google/mozc/blob/master/src/win32/tip/tip_surrounding_text.cc

use std::{mem::ManuallyDrop, rc::Rc};

use anyhow::Result;
use windows::{
    core::{IUnknown, Interface},
    Win32::UI::TextServices::{
        ITfCompartmentMgr, ITfContext, ITfDocumentMgr, GUID_COMPARTMENT_TRANSITORYEXTENSION_PARENT,
        TF_ANCHOR_START, TF_DEFAULT_SELECTION, TF_HALTCOND, TF_HF_OBJECT, TF_SELECTION,
        TF_TF_MOVESTART, TS_SS_TRANSITORY,
    },
};

use crate::engine::state::IMEState;

use super::{edit_session::edit_session, factory::TextServiceFactory};

impl TextServiceFactory {
    fn to_parent_document_if_exists(
        &self,
        document_manager: Option<ITfDocumentMgr>,
    ) -> Result<ITfDocumentMgr> {
        let document_manager = match document_manager {
            Some(doc_mgr) => doc_mgr,
            None => return Err(anyhow::anyhow!("Document manager is null")),
        };

        unsafe {
            // Get top context
            let context = match document_manager.GetTop() {
                Ok(ctx) => ctx,
                Err(_) => return Ok(document_manager),
            };

            // Get status
            let status = match context.GetStatus() {
                Ok(s) => s,
                Err(_) => return Ok(document_manager),
            };

            // Check if context is transitory
            if (status.dwStaticFlags & TS_SS_TRANSITORY) != TS_SS_TRANSITORY {
                return Ok(document_manager);
            }

            // Get compartment manager
            let compartment_mgr = match document_manager.cast::<ITfCompartmentMgr>() {
                Ok(mgr) => mgr,
                Err(_) => return Ok(document_manager),
            };

            // Get compartment
            let compartment = match compartment_mgr
                .GetCompartment(&GUID_COMPARTMENT_TRANSITORYEXTENSION_PARENT)
            {
                Ok(comp) => comp,
                Err(_) => return Ok(document_manager),
            };

            // Get value
            let variant = match compartment.GetValue() {
                Ok(var) => var,
                Err(_) => return Ok(document_manager),
            };

            // Use a cloned IUnknown from VARIANT to avoid invalid reference-count handling.
            // If this is not VT_UNKNOWN (or null), treat it as "parent not available".
            let variant_unk = match IUnknown::try_from(&variant) {
                Ok(unk) => unk,
                Err(_) => return Ok(document_manager),
            };

            match variant_unk.cast::<ITfDocumentMgr>() {
                Ok(parent_doc_mgr) => Ok(parent_doc_mgr),
                Err(_) => Ok(document_manager),
            }
        }
    }

    fn to_parent_context_if_exists(&self, context: Option<ITfContext>) -> Result<ITfContext> {
        let context = match context {
            Some(ctx) => ctx,
            None => return Err(anyhow::anyhow!("Context is null")),
        };

        unsafe {
            // Get document manager
            let document_mgr = match context.GetDocumentMgr() {
                Ok(doc_mgr) => doc_mgr,
                Err(_) => return Ok(context),
            };

            // Get parent document
            let parent_doc_mgr = self.to_parent_document_if_exists(Some(document_mgr))?;

            // Get top context from parent document
            let parent_context = match parent_doc_mgr.GetTop() {
                Ok(ctx) => ctx,
                Err(_) => return Ok(context),
            };

            Ok(parent_context)
        }
    }

    pub fn update_context(&self, preview: &str) -> Result<()> {
        let result: Result<()> = (|| unsafe {
            let text_service = self.borrow()?;

            let context = text_service.context::<ITfContext>()?;
            let parent_context = self.to_parent_context_if_exists(Some(context))?;

            let preceding_text = edit_session::<String>(
                text_service.tid,
                parent_context.clone(),
                Rc::new({
                    // ShiftEnd counts UTF-16 code units, not chars
                    let preview_count = preview.encode_utf16().count() as i32;

                    move |cookie| {
                        // 2. Get the selection from the parent context.
                        let mut pselection: [TF_SELECTION; 1] = [TF_SELECTION::default()];
                        let mut pfetched = 0;
                        parent_context.GetSelection(
                            cookie,
                            TF_DEFAULT_SELECTION,
                            &mut pselection,
                            &mut pfetched,
                        )?;

                        if pfetched == 0 {
                            return Ok(String::new());
                        }

                        // GetSelection is [out]: the range arrives AddRef'd
                        // inside a ManuallyDrop, so take ownership or it
                        // leaks in the host app on every keystroke
                        let selection_range = ManuallyDrop::take(&mut pselection[0].range);
                        let range = match selection_range.as_ref() {
                            Some(range) => range.Clone()?,
                            None => return Ok(String::new()),
                        };

                        let mut preceding_range_shifted = 0;

                        let halt_cond = TF_HALTCOND {
                            pHaltRange: ManuallyDrop::new(None),
                            aHaltPos: TF_ANCHOR_START,
                            dwFlags: TF_HF_OBJECT,
                        };

                        let preceding_range = range.Clone()?;
                        preceding_range.Collapse(cookie, TF_ANCHOR_START)?;
                        preceding_range.ShiftStart(
                            cookie,
                            -30,
                            &mut preceding_range_shifted,
                            &halt_cond,
                        )?;

                        preceding_range.ShiftEnd(
                            cookie,
                            -preview_count,
                            &mut preceding_range_shifted,
                            &halt_cond,
                        )?;

                        let mut pchtext = [0u16; 64];
                        let mut pcch = 0;
                        preceding_range.GetText(
                            cookie,
                            TF_TF_MOVESTART,
                            &mut pchtext,
                            &mut pcch,
                        )?;

                        Ok(String::from_utf16_lossy(&pchtext[..pcch as usize]))
                    }
                }),
            )?;

            let Some(preceding_text) = preceding_text else {
                return Ok(());
            };

            let Some(mut ipc_service) = IMEState::get()?.ipc_service.clone() else {
                return Ok(());
            };

            ipc_service.set_context(preceding_text)?;

            Ok(())
        })();

        if let Err(error) = result {
            tracing::warn!("Failed to update surrounded text context: {error:?}");
        }

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::rc::Rc;

    use windows::core::AsImpl as _;

    use crate::tsf::factory::TextServiceFactory;
    use crate::tsf::test_support::{
        factory_with_context, global_state_lock, EditSessionBehavior, FakeContext, RangeLog,
    };

    /// Drives update_context end to end against the fake host: the
    /// preceding text is located by shifting the selection back — the fixed
    /// 30-unit window, then forward past the preview — and the shift must be
    /// in UTF-16 code units, not chars (B9's unit). Every range the host
    /// handed out must be released (B10's leak).
    #[test]
    fn update_context_shifts_by_utf16_units_and_releases_the_ranges() {
        let _guard = global_state_lock();
        let log = Rc::new(RangeLog::default());
        *log.text.borrow_mut() = "こんにちは".encode_utf16().collect();
        let context = FakeContext::with_ranges(EditSessionBehavior::RunSync, log.clone());
        let tip = factory_with_context(context.clone());
        let factory: &TextServiceFactory = unsafe { tip.as_impl() };

        // 𠮷 is one char but two UTF-16 units — the difference that matters
        factory.update_context("みず𠮷").unwrap();

        assert_eq!(
            log.shift_start_reqs.borrow().last(),
            Some(&-30),
            "the fixed 30-unit context window must be requested"
        );
        assert_eq!(
            log.shift_end_reqs.borrow().last(),
            Some(&-4),
            "ShiftEnd must retreat by the preview's UTF-16 length (みず𠮷 = 4)"
        );
        assert_eq!(
            log.live_ranges(),
            0,
            "every range the host handed out must be released"
        );
    }
}
