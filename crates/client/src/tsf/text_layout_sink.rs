use windows::{
    core::Interface as _,
    Win32::UI::TextServices::{
        ITfContext, ITfContextView, ITfDocumentMgr, ITfSource, ITfTextLayoutSink,
        ITfTextLayoutSink_Impl, TfLayoutCode,
    },
};

use anyhow::Result;

use super::factory::{TextServiceFactory, TextServiceFactory_Impl};
use super::text_service::TextService;

impl ITfTextLayoutSink_Impl for TextServiceFactory_Impl {
    // This function is called when the text display position changes when the IME is enabled.
    // However, this function **will not be called** in Microsoft Store applications such as Notepad, so be careful.
    #[macros::anyhow]
    fn OnLayoutChange(
        &self,
        _pic: Option<&ITfContext>,
        _lcode: TfLayoutCode,
        _pview: Option<&ITfContextView>,
    ) -> Result<()> {
        let should_skip = match self.borrow_mut() {
            Ok(mut text_service) => text_service
                .update_pos_state
                .should_skip_layout_change(std::time::Instant::now()),
            Err(error) => {
                tracing::warn!("Skip OnLayoutChange due to borrow conflict: {error:?}");
                true
            }
        };

        if should_skip {
            tracing::debug!("Skip layout-triggered update_pos to avoid feedback loop");
            return Ok(());
        }

        if let Err(error) = self.update_pos() {
            tracing::warn!("Failed to update position from OnLayoutChange: {error:?}");
        }

        Ok(())
    }
}

// These live on the factory (they need the COM object for the sink QI) but
// take the caller's `&mut TextService` explicitly: every caller already
// holds the RefMut, so borrowing again here would double-borrow. The cookie
// and context now live in the per-instance TextService (B14) — one TIP per
// UI thread, no cross-thread sharing.
impl TextServiceFactory {
    pub fn advise_text_layout_sink(
        &self,
        text_service: &mut TextService,
        doc_mgr: ITfDocumentMgr,
    ) -> Result<()> {
        if text_service.layout_context.is_some() {
            self.unadvise_text_layout_sink(text_service)?;
        }

        let context = unsafe { doc_mgr.GetTop()? };
        text_service.layout_context = Some(context.clone());
        self.advise_sink::<ITfTextLayoutSink>(&context.cast::<ITfSource>()?, text_service)
    }

    pub fn unadvise_text_layout_sink(&self, text_service: &mut TextService) -> Result<()> {
        if let Some(context) = text_service.layout_context.take() {
            self.unadvise_sink::<ITfTextLayoutSink>(&context.cast::<ITfSource>()?, text_service)?;
        }

        Ok(())
    }
}
