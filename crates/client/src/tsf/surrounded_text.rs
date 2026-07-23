// reference to the original code:
// https://github.com/google/mozc/blob/master/src/win32/tip/tip_surrounding_text.cc

use std::{mem::ManuallyDrop, rc::Rc};

use anyhow::Result;
use windows::{
    Win32::UI::TextServices::{
        GUID_COMPARTMENT_TRANSITORYEXTENSION_PARENT, ITfCompartmentMgr, ITfContext, ITfDocumentMgr,
        TF_ANCHOR_START, TF_HALTCOND, TF_HF_OBJECT, TF_TF_MOVESTART, TS_SS_TRANSITORY,
    },
    core::{IUnknown, Interface},
};

use crate::engine::state::IMEState;

use super::{
    edit_session::{edit_session, selected_range},
    factory::TextServiceFactory,
};

impl TextServiceFactory {
    /// Every probe below is a host-capability question, not a failure: a
    /// host that answers Err simply has no transitory parent, so we fall
    /// back to the document manager we were given (`let Ok(..) = .. else`
    /// throughout — see the error-handling rule in CLAUDE.md).
    fn to_parent_document_if_exists(
        &self,
        document_manager: Option<ITfDocumentMgr>,
    ) -> Result<ITfDocumentMgr> {
        let Some(document_manager) = document_manager else {
            return Err(anyhow::anyhow!("Document manager is null"));
        };

        unsafe {
            let Ok(context) = document_manager.GetTop() else {
                return Ok(document_manager);
            };

            let Ok(status) = context.GetStatus() else {
                return Ok(document_manager);
            };

            // only a transitory context (e.g. the search box's proxy
            // document) has a parent worth chasing
            if (status.dwStaticFlags & TS_SS_TRANSITORY) != TS_SS_TRANSITORY {
                return Ok(document_manager);
            }

            let Ok(compartment_mgr) = document_manager.cast::<ITfCompartmentMgr>() else {
                return Ok(document_manager);
            };

            let Ok(compartment) =
                compartment_mgr.GetCompartment(&GUID_COMPARTMENT_TRANSITORYEXTENSION_PARENT)
            else {
                return Ok(document_manager);
            };

            let Ok(variant) = compartment.GetValue() else {
                return Ok(document_manager);
            };

            // Use a cloned IUnknown from VARIANT to avoid invalid
            // reference-count handling. If this is not VT_UNKNOWN (or
            // null), treat it as "parent not available".
            let Ok(variant_unk) = IUnknown::try_from(&variant) else {
                return Ok(document_manager);
            };

            let Ok(parent_doc_mgr) = variant_unk.cast::<ITfDocumentMgr>() else {
                return Ok(document_manager);
            };

            Ok(parent_doc_mgr)
        }
    }

    fn to_parent_context_if_exists(&self, context: Option<ITfContext>) -> Result<ITfContext> {
        let Some(context) = context else {
            return Err(anyhow::anyhow!("Context is null"));
        };

        unsafe {
            let Ok(document_mgr) = context.GetDocumentMgr() else {
                return Ok(context);
            };

            let parent_doc_mgr = self.to_parent_document_if_exists(Some(document_mgr))?;

            let Ok(parent_context) = parent_doc_mgr.GetTop() else {
                return Ok(context);
            };

            Ok(parent_context)
        }
    }

    /// Sends the engine the text that precedes the composition. `shown_text`
    /// is everything the composition currently puts on screen (preview and
    /// suffix): the window has to end before it, or the composition's own
    /// text would come back as its context.
    pub fn update_context(&self, shown_text: &str) -> Result<()> {
        let result: Result<()> = (|| unsafe {
            let text_service = self.borrow()?;

            let context = text_service.context::<ITfContext>()?;
            let parent_context = self.to_parent_context_if_exists(Some(context))?;

            let preceding_text = edit_session::<String>(
                text_service.tid,
                parent_context.clone(),
                Rc::new({
                    // ShiftEnd counts UTF-16 code units, not chars
                    let shown_count = shown_text.encode_utf16().count() as i32;

                    move |cookie| {
                        // 2. Get the selection from the parent context
                        // (selected_range owns the ManuallyDrop bookkeeping).
                        let Some(range) = selected_range(&parent_context, cookie)? else {
                            return Ok(String::new());
                        };

                        let mut preceding_range_shifted = 0;

                        let halt_cond = TF_HALTCOND {
                            pHaltRange: ManuallyDrop::new(None),
                            aHaltPos: TF_ANCHOR_START,
                            dwFlags: TF_HF_OBJECT,
                        };

                        let preceding_range = range.Clone()?;
                        preceding_range.Collapse(cookie, TF_ANCHOR_START)?;
                        // both anchors clear the composed text: shifting
                        // only the end by it left a window of
                        // [sel - 30, sel - shown], which inverts (end before
                        // start, so an empty or garbled context) as soon as
                        // the composition passes 30 units
                        preceding_range.ShiftStart(
                            cookie,
                            -(30 + shown_count),
                            &mut preceding_range_shifted,
                            &halt_cond,
                        )?;

                        preceding_range.ShiftEnd(
                            cookie,
                            -shown_count,
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
        EditSessionBehavior, FakeContext, RangeLog, factory_with_context, global_state_lock,
    };

    /// Drives update_context end to end against the fake host: the
    /// preceding text is the 30-unit window that ends where the composition
    /// begins, so both anchors retreat past the composed text, and the
    /// shifts must be in UTF-16 code units, not chars (B9's unit). Every
    /// range the host handed out must be released (B10's leak).
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
            Some(&-34),
            "the start must clear the composed text (4 units) and then open the 30-unit window"
        );
        assert_eq!(
            log.shift_end_reqs.borrow().last(),
            Some(&-4),
            "ShiftEnd must retreat by the composed text's UTF-16 length (みず𠮷 = 4)"
        );
        assert_eq!(
            log.live_ranges(),
            0,
            "every range the host handed out must be released"
        );
    }

    /// The old window was [sel - 30, sel - shown], which inverted once the
    /// composition grew past 30 units: the end landed before the start and
    /// the engine got an empty or garbled context exactly when a long
    /// reading needed it most.
    #[test]
    fn a_long_composition_must_not_invert_the_context_window() {
        let _guard = global_state_lock();
        let log = Rc::new(RangeLog::default());
        *log.text.borrow_mut() = "こんにちは".encode_utf16().collect();
        let context = FakeContext::with_ranges(EditSessionBehavior::RunSync, log.clone());
        let tip = factory_with_context(context.clone());
        let factory: &TextServiceFactory = unsafe { tip.as_impl() };

        let long = "あ".repeat(35);
        factory.update_context(&long).unwrap();

        let start = *log.shift_start_reqs.borrow().last().unwrap();
        let end = *log.shift_end_reqs.borrow().last().unwrap();
        assert_eq!(start, -65);
        assert_eq!(end, -35);
        assert_eq!(
            end - start,
            30,
            "the window must stay 30 units wide however long the composition gets"
        );
    }
}
