use windows::{
    Win32::UI::TextServices::{
        ITfContext, ITfContextView, ITfDocumentMgr, ITfSource, ITfTextLayoutSink,
        ITfTextLayoutSink_Impl, TfLayoutCode,
    },
    core::Interface as _,
};

use anyhow::Result;

use super::factory::TextServiceFactory_Impl;
use super::text_service::TextService;

impl ITfTextLayoutSink_Impl for TextServiceFactory_Impl {
    // Called when the text display position changes while the IME is active.
    //
    // A long-standing comment here claimed this is **never** called in
    // Microsoft Store applications such as Notepad, and issue #37 was opened
    // on that basis. Measured on Windows 11 with the packaged Notepad
    // (Microsoft.WindowsNotepad 11.2605.34.0, under WindowsApps), it is
    // called: 83 firings for 85 keystrokes. Two third-party Win32 hosts
    // (75/57 and 195/71) fire it too, and none of the three ever answered
    // TS_E_NOLAYOUT. The claim may have held for an older Notepad; it does
    // not hold now, so do not design around it without re-measuring.
    #[macros::anyhow]
    fn OnLayoutChange(
        &self,
        _pic: windows_core::Ref<'_, ITfContext>,
        _lcode: TfLayoutCode,
        _pview: windows_core::Ref<'_, ITfContextView>,
    ) -> Result<()> {
        // Entry-level, deliberately: whether a host fires this at all is the
        // question issue #37 turns on, and a successful call otherwise logs
        // nothing of its own (only the skip and failure paths do), so the
        // firing rate could not be compared against set_window_position.
        tracing::debug!("OnLayoutChange fired");

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
impl TextServiceFactory_Impl {
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
        // bound to a local rather than left a temporary in the tail
        // expression: the release point of an interface temporary moves
        // between editions 2021 and 2024
        let source = context.cast::<ITfSource>()?;
        self.advise_sink::<ITfTextLayoutSink>(&source, text_service)
    }

    pub fn unadvise_text_layout_sink(&self, text_service: &mut TextService) -> Result<()> {
        if let Some(context) = text_service.layout_context.take() {
            self.unadvise_sink::<ITfTextLayoutSink>(&context.cast::<ITfSource>()?, text_service)?;
        }

        Ok(())
    }
}
