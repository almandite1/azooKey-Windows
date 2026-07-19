use std::collections::HashMap;

use crate::{
    engine::{composition::Composition, ipc_service, state::IMEState},
    globals::{DllModule, GUID_DISPLAY_ATTRIBUTE},
};

use super::factory::TextServiceFactory_Impl;
use windows::{
    core::Interface as _,
    Win32::{
        Foundation::BOOL,
        System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER},
        UI::TextServices::{
            CLSID_TF_CategoryMgr, ITfCategoryMgr, ITfKeyEventSink, ITfKeystrokeMgr,
            ITfLangBarItemButton, ITfLangBarItemMgr, ITfSource, ITfTextInputProcessorEx_Impl,
            ITfTextInputProcessor_Impl, ITfThreadMgr, ITfThreadMgrEventSink,
        },
    },
};

use anyhow::Result;

impl ITfTextInputProcessor_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    #[tracing::instrument]
    fn Activate(&self, ptim: Option<&ITfThreadMgr>, tid: u32) -> Result<()> {
        tracing::debug!("Activated with tid: {tid}");

        // add reference to the dll instance to prevent it from being unloaded
        let mut dll_instance = DllModule::get()?;
        dll_instance.add_ref();

        // initialize ipc_service
        // Activate() should not return an error: if it does, the icon of the
        // previously activated TextService is displayed, confusing the user.
        match ipc_service::IPCService::new() {
            Ok(mut ipc_service) => {
                // warm up the lazy connection; if the server is not running
                // yet this fails harmlessly and the channel reconnects on
                // the next keystroke
                if let Err(e) = ipc_service.append_text("".to_string()) {
                    tracing::warn!("azookey server not reachable yet: {e}");
                }
                IMEState::get()?.ipc_service = Some(ipc_service);
            }
            Err(e) => {
                tracing::error!("Failed to initialize IPC service: {e}");
                return Ok(());
            }
        }

        // resolve the thread manager up front; on null, release the dll ref
        // taken above before bailing (the old code leaked it here)
        let thread_mgr = match ptim {
            Some(thread_mgr) => thread_mgr.clone(),
            None => {
                dll_instance.release();
                return Err(anyhow::anyhow!("Thread manager is null"));
            }
        };

        // Wire the sinks. If any step fails partway, the sinks already
        // advised must be unwound: otherwise they leak into the host and,
        // worse, TSF can keep calling back into a half-activated TIP. The
        // old code returned Err here leaving every prior advise in place.
        let setup = (|| -> Result<()> {
            let mut text_service = self.borrow_mut()?;

            text_service.tid = tid;
            text_service.thread_mgr = Some(thread_mgr.clone());

            // initialize key event sink
            tracing::debug!("AdviseKeyEventSink");
            unsafe {
                thread_mgr.cast::<ITfKeystrokeMgr>()?.AdviseKeyEventSink(
                    tid,
                    &self.this::<ITfKeyEventSink>()?,
                    BOOL::from(true),
                )?;
            };

            // initialize thread manager event sink
            tracing::debug!("AdviseThreadMgrEventSink");
            self.advise_sink::<ITfThreadMgrEventSink>(
                &thread_mgr.cast::<ITfSource>()?,
                &mut text_service,
            )?;

            // initialize text layout sink
            tracing::debug!("AdviseTextLayoutSink");
            let doc_mgr = unsafe { thread_mgr.GetFocus() };
            if let Ok(doc_mgr) = doc_mgr {
                self.advise_text_layout_sink(&mut text_service, doc_mgr)?;
            }

            // initialize display attribute
            tracing::debug!("Initialize display attribute");
            let atom_map = unsafe {
                let mut map = HashMap::new();
                let category_mgr: ITfCategoryMgr =
                    CoCreateInstance(&CLSID_TF_CategoryMgr, None, CLSCTX_INPROC_SERVER)?;

                let atom = category_mgr.RegisterGUID(&GUID_DISPLAY_ATTRIBUTE)?;
                map.insert(GUID_DISPLAY_ATTRIBUTE, atom);
                map
            };

            text_service.display_attribute_atom = atom_map;

            // initialize langbar
            tracing::debug!("Initialize langbar");
            unsafe {
                thread_mgr
                    .cast::<ITfLangBarItemMgr>()?
                    .AddItem(&self.this::<ITfLangBarItemButton>()?)?;
            };

            Ok(())
        })();

        if let Err(error) = setup {
            tracing::error!("Activate failed, rolling back partial state: {error:?}");
            self.rollback_activation(&thread_mgr, tid);
            // balance the add_ref taken at the top of Activate
            dll_instance.release();
            return Err(error);
        }

        tracing::debug!("Activate success");

        Ok(())
    }

    #[macros::anyhow]
    #[tracing::instrument]
    fn Deactivate(&self) -> Result<()> {
        tracing::debug!("Deactivated");

        // remove reference to the dll instance
        let mut dll_instance = DllModule::get()?;
        dll_instance.release();

        // Best-effort teardown: every step below runs even if an earlier one
        // fails. The old code chained the unadvises with `?`, so a single
        // failing step (e.g. end_composition on a document being torn down
        // during a profile switch) stranded the remaining unadvises — leaking
        // a sink into the host or pinning its ITfContext. The first error is
        // remembered and surfaced at the end.
        fn record(slot: &mut Result<()>, result: Result<()>) {
            if let Err(error) = result {
                tracing::warn!("Deactivate step failed: {error:?}");
                if slot.is_ok() {
                    *slot = Err(error);
                }
            }
        }
        let mut first_error: Result<()> = Ok(());

        // end composition (releases the client-side handle even on failure)
        record(&mut first_error, self.end_composition());

        // key event sink + langbar removal need the thread manager
        match self.borrow() {
            Ok(text_service) => match text_service.thread_mgr() {
                Ok(thread_mgr) => {
                    tracing::debug!("UnadviseKeyEventSink");
                    record(
                        &mut first_error,
                        (|| -> Result<()> {
                            unsafe {
                                thread_mgr
                                    .cast::<ITfKeystrokeMgr>()?
                                    .UnadviseKeyEventSink(text_service.tid)?;
                            }
                            Ok(())
                        })(),
                    );

                    tracing::debug!("Remove langbar");
                    record(
                        &mut first_error,
                        (|| -> Result<()> {
                            unsafe {
                                thread_mgr
                                    .cast::<ITfLangBarItemMgr>()?
                                    .RemoveItem(&self.this::<ITfLangBarItemButton>()?)?;
                            }
                            Ok(())
                        })(),
                    );
                }
                Err(error) => record(&mut first_error, Err(error)),
            },
            Err(error) => record(&mut first_error, Err(error)),
        }

        // thread-mgr event sink + text layout sink + field cleanup
        match self.borrow_mut() {
            Ok(mut text_service) => {
                tracing::debug!("UnadviseThreadMgrEventSink");
                match text_service.thread_mgr() {
                    Ok(thread_mgr) => match thread_mgr.cast::<ITfSource>() {
                        Ok(source) => record(
                            &mut first_error,
                            self.unadvise_sink::<ITfThreadMgrEventSink>(&source, &mut text_service),
                        ),
                        Err(error) => record(&mut first_error, Err(error.into())),
                    },
                    Err(error) => record(&mut first_error, Err(error)),
                }

                tracing::debug!("UnadviseTextLayoutSink");
                record(
                    &mut first_error,
                    self.unadvise_text_layout_sink(&mut text_service),
                );

                // clear display attribute
                text_service.display_attribute_atom.clear();

                text_service.tid = 0;
                text_service.thread_mgr = None;
                // Also let go of the last document's context: handle_key
                // re-sets it on every keystroke after the next Activate, and
                // end_composition() above already ran, so nothing dereferences
                // it in between. Keeping it would pin the host's ITfContext
                // while the TIP is deactivated. (The old `this` self-reference
                // is gone entirely — TSF reuses this object across
                // Deactivate/Activate cycles, and every callback reaches its
                // COM interfaces via TextServiceFactory::this(), a QI on the
                // containing allocation, so there is nothing to clear/restore.)
                text_service.context = None;
                text_service.layout_context = None;

                // Reset the engine composition. TSF reuses this TextService
                // across Deactivate/Activate, so a leftover Composing state
                // (and its reading) would otherwise resume after an in-app IME
                // switch (Win+Space and back within the same app, which does
                // NOT fire OnSetFocus): the first keystroke would land in the
                // Composing arm with no live tip_composition, set_text would
                // no-op, and the typing would be invisible.
                match text_service.borrow_mut_composition() {
                    Ok(mut composition) => *composition = Composition::default(),
                    Err(error) => record(&mut first_error, Err(error)),
                }
            }
            Err(error) => record(&mut first_error, Err(error)),
        }

        if first_error.is_ok() {
            tracing::debug!("Deactivate success");
        }

        first_error
    }
}

impl ITfTextInputProcessorEx_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    fn ActivateEx(&self, ptim: Option<&ITfThreadMgr>, tid: u32, _dwflags: u32) -> Result<()> {
        // called when the text service is activated
        // if this function is implemented, the Activate() function won't be called
        // so we need to call the Activate function manually
        tracing::debug!("Activated(Ex) with tid: {tid}");
        self.Activate(ptim, tid)?;
        Ok(())
    }
}

impl TextServiceFactory_Impl {
    /// Best-effort unwind of the sinks [`Activate`](TextServiceFactory_Impl::Activate)
    /// may have advised before a later step failed. Each step no-ops if its
    /// resource was never set up, so calling this after a partial Activate
    /// leaves the TIP as if it had never activated — no sink stranded in the
    /// host, no context pinned.
    fn rollback_activation(&self, thread_mgr: &ITfThreadMgr, tid: u32) {
        if let Err(error) = (|| -> Result<()> {
            unsafe {
                thread_mgr
                    .cast::<ITfKeystrokeMgr>()?
                    .UnadviseKeyEventSink(tid)?;
            }
            Ok(())
        })() {
            tracing::warn!("rollback: UnadviseKeyEventSink failed: {error:?}");
        }

        if let Ok(mut text_service) = self.borrow_mut() {
            if let Ok(source) = thread_mgr.cast::<ITfSource>() {
                if let Err(error) =
                    self.unadvise_sink::<ITfThreadMgrEventSink>(&source, &mut text_service)
                {
                    tracing::warn!("rollback: unadvise thread-mgr sink failed: {error:?}");
                }
            }
            if let Err(error) = self.unadvise_text_layout_sink(&mut text_service) {
                tracing::warn!("rollback: unadvise text layout sink failed: {error:?}");
            }

            // AddItem is the last setup step; if it succeeded before another
            // failure (there is none after it today) this removes it, and if
            // it never ran RemoveItem simply reports no such item — harmless
            if let Err(error) = (|| -> Result<()> {
                unsafe {
                    thread_mgr
                        .cast::<ITfLangBarItemMgr>()?
                        .RemoveItem(&self.this::<ITfLangBarItemButton>()?)?;
                }
                Ok(())
            })() {
                tracing::debug!("rollback: RemoveItem (langbar not added): {error:?}");
            }

            text_service.display_attribute_atom.clear();
            text_service.tid = 0;
            text_service.thread_mgr = None;
            text_service.context = None;
            text_service.layout_context = None;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::rc::Rc;
    use std::sync::Mutex;

    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::TextServices::{ITfTextInputProcessor, ITfThreadMgr};

    use crate::engine::composition::CompositionState;
    use crate::engine::state::IMEState;
    use crate::globals::{DllModule, DLL_INSTANCE};
    use crate::tsf::factory::TextServiceFactory;
    use crate::tsf::test_support::{
        fake_context_of, EditSessionBehavior, FakeContext, FakeThreadMgr, ThreadMgrLog,
    };
    use windows::core::AsImpl as _;

    // Activate/Deactivate mutate the process-global IMEState and DllModule,
    // so every test here holds the crate-wide global_state_lock (shared with
    // the update_pos/update_context tests, which read the same global).

    /// A `#[test]` never runs DllMain, so the global DllModule the real
    /// Activate reference-counts is never initialized. Set it up once; the
    /// hinst stays None, which is fine because Activate only ever add_ref/
    /// releases the counter, never dereferences the module handle.
    fn ensure_dll_module() {
        let _ = DLL_INSTANCE.set(Mutex::new(DllModule::new()));
    }

    /// Fresh IMEState so one test's leftover ipc_service can't leak into the
    /// next. (Sink cookies and the layout context are per-TextService now, so
    /// they die with each test's TIP instance.)
    fn reset_ime_state() {
        if let Ok(mut state) = IMEState::get() {
            state.ipc_service = None;
        }
    }

    /// Builds a TIP and activates it against a recording fake thread manager.
    fn activate_fresh_tip() -> (ITfTextInputProcessor, ITfThreadMgr, Rc<ThreadMgrLog>) {
        ensure_dll_module();
        // Activate calls CoCreateInstance(CLSID_TF_CategoryMgr); the test
        // thread needs an initialized COM apartment. Idempotent per thread.
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        reset_ime_state();
        let tip = TextServiceFactory::create::<ITfTextInputProcessor>()
            .expect("failed to create the TIP");
        let log = Rc::new(ThreadMgrLog::default());
        let thread_mgr = FakeThreadMgr::new(log.clone());
        unsafe { tip.Activate(Some(&thread_mgr), 1) }.expect("first Activate must succeed");
        (tip, thread_mgr, log)
    }

    /// B14: two TIP instances (one per UI thread in a real host) must not
    /// share sink bookkeeping. With the old process-global cookie map, B's
    /// Activate overwrote A's thread-mgr-sink cookie, so A's Deactivate
    /// unadvised B's cookie on A's thread manager — leaking A's sink and
    /// making B's un-removable.
    #[test]
    fn deactivating_one_tip_does_not_disturb_anothers_cookie() {
        let _guard = crate::tsf::test_support::global_state_lock();
        ensure_dll_module();
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        reset_ime_state();

        // distinguishable cookie ranges: A hands out 101.., B hands out 201..
        let log_a = Rc::new(ThreadMgrLog::with_cookie_base(100));
        let log_b = Rc::new(ThreadMgrLog::with_cookie_base(200));
        let tm_a = FakeThreadMgr::new(log_a.clone());
        let tm_b = FakeThreadMgr::new(log_b.clone());
        let tip_a =
            TextServiceFactory::create::<ITfTextInputProcessor>().expect("failed to create TIP A");
        let tip_b =
            TextServiceFactory::create::<ITfTextInputProcessor>().expect("failed to create TIP B");

        unsafe { tip_a.Activate(Some(&tm_a), 1) }.expect("Activate A must succeed");
        unsafe { tip_b.Activate(Some(&tm_b), 2) }.expect("Activate B must succeed");

        unsafe { tip_a.Deactivate() }.expect("Deactivate A must succeed");
        assert_eq!(
            log_a.unadvise_cookies.borrow().as_slice(),
            log_a.advise_cookies.borrow().as_slice(),
            "A must unadvise exactly the cookie its own thread manager issued, \
             not one belonging to another instance"
        );

        unsafe { tip_b.Deactivate() }.expect("Deactivate B must succeed");
        assert_eq!(
            log_b.unadvise_cookies.borrow().as_slice(),
            log_b.advise_cookies.borrow().as_slice(),
            "B's cookie must survive A's lifecycle and be unadvised on B"
        );
        reset_ime_state();
    }

    /// R4: Activate must return Ok even though the engine is unreachable in a
    /// test (no server). If it returned Err, TSF would leave the previously
    /// active IME's icon up and the user could never select azooKey.
    #[test]
    fn activate_succeeds_when_the_server_is_unreachable() {
        let _guard = crate::tsf::test_support::global_state_lock();
        let (_tip, _tm, log) = activate_fresh_tip();
        assert_eq!(
            log.key_sink_advises.borrow().as_slice(),
            &[true],
            "Activate should advise exactly one live key-event sink"
        );
        assert!(
            IMEState::get().unwrap().ipc_service.is_some(),
            "the IPC service should be installed even though its warm-up RPC fails"
        );
        reset_ime_state();
    }

    /// R3: the B15 regression guard. TSF reuses one TextService across every
    /// IME switch, so Activate → Deactivate → Activate must all succeed. B15
    /// cleared `this` in Deactivate; the second Activate then failed at
    /// `this::<ITfKeyEventSink>()`, leaving azooKey unselectable. Here the
    /// second Activate must succeed and advise a live key sink again.
    #[test]
    fn activate_deactivate_activate_cycle_succeeds() {
        let _guard = crate::tsf::test_support::global_state_lock();
        let (tip, thread_mgr, log) = activate_fresh_tip();
        unsafe { tip.Deactivate() }.expect("Deactivate must succeed");
        unsafe { tip.Activate(Some(&thread_mgr), 1) }
            .expect("re-Activate must succeed (B15 regression)");
        assert_eq!(
            log.key_sink_advises.borrow().as_slice(),
            &[true, true],
            "both activations must advise a live key-event sink"
        );
        reset_ime_state();
    }

    /// Deactivate must unwind exactly what Activate advised: the key-event
    /// sink, the thread-manager event sink (by its cookie), and the language
    /// bar item — no leak, no double-advise. This locks in the advise/unadvise
    /// balance that the Activate-failure investigation kept suspecting.
    #[test]
    fn deactivate_unadvises_what_activate_advised() {
        let _guard = crate::tsf::test_support::global_state_lock();
        let (tip, _tm, log) = activate_fresh_tip();
        assert_eq!(log.langbar_adds.get(), 1, "Activate adds one langbar item");
        let advised = log.advise_cookies.borrow().clone();
        assert_eq!(advised.len(), 1, "Activate advises one thread-mgr sink");

        unsafe { tip.Deactivate() }.expect("Deactivate must succeed");

        assert_eq!(log.key_sink_unadvises.get(), 1, "the key sink is unadvised");
        assert_eq!(log.langbar_removes.get(), 1, "the langbar item is removed");
        assert_eq!(
            log.unadvise_cookies.borrow().as_slice(),
            advised.as_slice(),
            "Deactivate must unadvise exactly the cookie Activate advised"
        );
        reset_ime_state();
    }

    /// With a focused document, Activate must advise the text-layout sink on
    /// the document's top context and Deactivate must unadvise that exact
    /// cookie. This drives the path every prior lifecycle test skipped
    /// (GetFocus used to be un-fakeable).
    #[test]
    fn activate_advises_the_layout_sink_when_a_document_has_focus() {
        let _guard = crate::tsf::test_support::global_state_lock();
        ensure_dll_module();
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        reset_ime_state();

        let log = Rc::new(ThreadMgrLog::default());
        let doc_context = FakeContext::new(EditSessionBehavior::RunSync);
        let thread_mgr = FakeThreadMgr::with_focus(log.clone(), doc_context.clone());

        let tip = TextServiceFactory::create::<ITfTextInputProcessor>()
            .expect("failed to create the TIP");
        unsafe { tip.Activate(Some(&thread_mgr), 1) }.expect("Activate must succeed");

        let doc = unsafe { fake_context_of(&doc_context) };
        let advised = doc.sink_advises();
        assert_eq!(
            advised.len(),
            1,
            "Activate advises the layout sink on the focused document's context"
        );

        unsafe { tip.Deactivate() }.expect("Deactivate must succeed");
        assert_eq!(
            doc.sink_unadvises(),
            advised,
            "Deactivate must unadvise exactly the layout-sink cookie it advised"
        );
        reset_ime_state();
    }

    /// Fix 4: when a late Activate step fails (here AddItem, the final step),
    /// the TIP must unwind the sinks it already advised instead of stranding
    /// them in the host. The old code returned Err leaving the key-event sink
    /// and the thread-manager event sink advised.
    #[test]
    fn activate_rolls_back_advised_sinks_when_a_later_step_fails() {
        let _guard = crate::tsf::test_support::global_state_lock();
        ensure_dll_module();
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        reset_ime_state();

        let log = Rc::new(ThreadMgrLog::default());
        let thread_mgr = FakeThreadMgr::with_failing_langbar(log.clone());
        let tip = TextServiceFactory::create::<ITfTextInputProcessor>()
            .expect("failed to create the TIP");

        let result = unsafe { tip.Activate(Some(&thread_mgr), 1) };
        assert!(
            result.is_err(),
            "Activate must fail when its AddItem step fails"
        );

        assert_eq!(
            log.key_sink_advises.borrow().as_slice(),
            &[true],
            "the key-event sink was advised before AddItem"
        );
        assert_eq!(
            log.key_sink_unadvises.get(),
            1,
            "a failed Activate must unadvise the key-event sink it advised"
        );
        assert_eq!(
            log.unadvise_cookies.borrow().as_slice(),
            log.advise_cookies.borrow().as_slice(),
            "a failed Activate must unadvise the thread-manager sink it advised"
        );

        // the partial state must be cleared so a later Activate starts clean
        let factory: &TextServiceFactory = unsafe { tip.as_impl() };
        assert_eq!(
            factory.borrow().unwrap().tid,
            0,
            "the rolled-back Activate must clear tid"
        );
        reset_ime_state();
    }

    /// Fix 2: TSF reuses one TextService across an in-app IME switch (Win+Space
    /// and back), which does NOT fire OnSetFocus. Deactivate must reset the
    /// engine composition, or the next Activate resumes a stale Composing state
    /// and the first keystrokes land in the Composing arm with no live
    /// composition — set_text no-ops and the typing is invisible.
    #[test]
    fn deactivate_resets_the_engine_composition() {
        let _guard = crate::tsf::test_support::global_state_lock();
        let (tip, _tm, _log) = activate_fresh_tip();
        let factory: &TextServiceFactory = unsafe { tip.as_impl() };
        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.state = CompositionState::Composing;
            composition.raw_hiragana = "わたし".to_string();
            composition.preview = "わたし".to_string();
        }

        unsafe { tip.Deactivate() }.expect("Deactivate must succeed");

        let text_service = factory.borrow().unwrap();
        let composition = text_service.borrow_composition().unwrap();
        assert_eq!(
            composition.state,
            CompositionState::None,
            "the composition state must be reset on Deactivate"
        );
        assert!(
            composition.raw_hiragana.is_empty() && composition.preview.is_empty(),
            "the leftover reading must be cleared so a re-Activate starts clean"
        );
        drop(composition);
        drop(text_service);
        reset_ime_state();
    }
}
