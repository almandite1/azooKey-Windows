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

        // advisory, and infallible by construction: update_pos logs and
        // swallows everything itself (CLAUDE.md)
        self.update_pos();

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use windows::Win32::UI::TextServices::ITfTextInputProcessor;

    use crate::tsf::test_support::{
        EditSessionBehavior, FakeContext, FakeDocumentMgr, factory_of, factory_with_context,
        fake_context_of, global_state_lock,
    };

    /// Advises the layout sink on `context`, the way `OnSetFocus` and
    /// `Activate` both do — through the document manager the host handed us.
    fn advise_on(
        tip: &ITfTextInputProcessor,
        context: &windows::Win32::UI::TextServices::ITfContext,
    ) {
        let factory = factory_of(tip);
        let mut text_service = factory.borrow_mut().unwrap();
        factory
            .advise_text_layout_sink(&mut text_service, FakeDocumentMgr::new(context.clone()))
            .expect("advise must succeed against a cooperative host");
    }

    /// Focus can move to a document we are already advised on (B14): TSF
    /// fires `OnSetFocus` for switches within an application too. Advising
    /// again without unadvising first would leak the previous cookie — the
    /// host keeps calling a sink nobody will ever take down, and our own
    /// bookkeeping only remembers the newest one.
    #[test]
    fn advising_twice_unadvises_the_first_cookie_first() {
        let _guard = global_state_lock();
        let context = FakeContext::new(EditSessionBehavior::RunSync);
        let tip = factory_with_context(context.clone());

        advise_on(&tip, &context);
        let first = unsafe { fake_context_of(&context) }.sink_advises();
        assert_eq!(first.len(), 1);

        advise_on(&tip, &context);

        let fake = unsafe { fake_context_of(&context) };
        assert_eq!(fake.sink_advises().len(), 2, "the new sink is advised");
        assert_eq!(
            fake.sink_unadvises(),
            first,
            "and the old cookie was handed back, exactly once"
        );
    }

    /// The unadvise is driven by `layout_context`, so it must be cleared when
    /// it is consumed: a second unadvise would otherwise hand the host a
    /// cookie it has already released.
    #[test]
    fn unadvising_twice_only_talks_to_the_host_once() {
        let _guard = global_state_lock();
        let context = FakeContext::new(EditSessionBehavior::RunSync);
        let tip = factory_with_context(context.clone());
        advise_on(&tip, &context);

        let factory = factory_of(&tip);
        {
            let mut text_service = factory.borrow_mut().unwrap();
            factory
                .unadvise_text_layout_sink(&mut text_service)
                .expect("the first unadvise must succeed");
            assert!(text_service.layout_context.is_none());
            factory
                .unadvise_text_layout_sink(&mut text_service)
                .expect("a second unadvise is a no-op, not an error");
        }

        assert_eq!(
            unsafe { fake_context_of(&context) }.sink_unadvises().len(),
            1
        );
    }

    /// After a clean unadvise the next advise starts over — no stale context
    /// to release, and the host sees one advise with nothing before it.
    #[test]
    fn readvising_after_an_unadvise_does_not_release_anything() {
        let _guard = global_state_lock();
        let context = FakeContext::new(EditSessionBehavior::RunSync);
        let tip = factory_with_context(context.clone());
        advise_on(&tip, &context);
        {
            let factory = factory_of(&tip);
            let mut text_service = factory.borrow_mut().unwrap();
            factory
                .unadvise_text_layout_sink(&mut text_service)
                .unwrap();
        }

        advise_on(&tip, &context);

        let fake = unsafe { fake_context_of(&context) };
        assert_eq!(fake.sink_advises().len(), 2);
        assert_eq!(
            fake.sink_unadvises().len(),
            1,
            "only the explicit unadvise, not one from the re-advise"
        );
    }
}
