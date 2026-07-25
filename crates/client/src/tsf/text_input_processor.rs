use std::collections::HashMap;

use crate::{
    engine::{composition::Composition, ipc_service, state::IMEState},
    globals::{DllModule, GUID_DISPLAY_ATTRIBUTE},
};

use super::factory::TextServiceFactory_Impl;
use windows::{
    Win32::{
        System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance},
        UI::TextServices::{
            CLSID_TF_CategoryMgr, ITfCategoryMgr, ITfKeyEventSink, ITfKeystrokeMgr, ITfSource,
            ITfTextInputProcessor_Impl, ITfTextInputProcessorEx_Impl, ITfThreadMgr,
            ITfThreadMgrEventSink,
        },
    },
    core::Interface as _,
};

use anyhow::Result;

/// Records the first failure of a best-effort teardown step while letting the
/// remaining steps run. Used across Deactivate and its `teardown_*` helpers so
/// a single failing unadvise cannot strand the others (which would leak a sink
/// into the host or pin its ITfContext).
fn record(slot: &mut Result<()>, result: Result<()>) {
    if let Err(error) = result {
        tracing::warn!("Deactivate step failed: {error:?}");
        if slot.is_ok() {
            *slot = Err(error);
        }
    }
}

impl TextServiceFactory_Impl {
    /// Advises the key-event sink — the lifeline every keystroke arrives
    /// through. Paired with `unadvise_key_sink`; extracted from the inline
    /// closures Activate and Deactivate each open-coded.
    fn advise_key_sink(&self, thread_mgr: &ITfThreadMgr, tid: u32) -> Result<()> {
        unsafe {
            thread_mgr.cast::<ITfKeystrokeMgr>()?.AdviseKeyEventSink(
                tid,
                &self.this::<ITfKeyEventSink>()?,
                // takes a plain bool since 0.62
                true,
            )?;
        }
        Ok(())
    }

    /// Unadvises the key-event sink advised by `advise_key_sink`.
    fn unadvise_key_sink(&self, thread_mgr: &ITfThreadMgr, tid: u32) -> Result<()> {
        unsafe {
            thread_mgr
                .cast::<ITfKeystrokeMgr>()?
                .UnadviseKeyEventSink(tid)?;
        }
        Ok(())
    }

    /// Deactivate phase 1: unadvise the thread-scoped registrations (key-event
    /// sink, langbar item) that need the thread manager. Best-effort — returns
    /// the first failure but attempts every step it can reach.
    fn teardown_thread_scoped(&self) -> Result<()> {
        let (thread_mgr, tid) = {
            let text_service = self.borrow()?;
            (text_service.thread_mgr()?, text_service.tid)
        };
        // Drained only once the thread manager is in hand: with no way to
        // reach ITfKeystrokeMgr there is nothing to unpreserve, and clearing
        // the registry anyway would lose the record of what is still held.
        let preserved = std::mem::take(&mut self.borrow_mut()?.preserved_keys);

        let mut first_error = Ok(());
        tracing::debug!("UnadviseKeyEventSink");
        record(&mut first_error, self.unadvise_key_sink(&thread_mgr, tid));
        tracing::debug!("UnpreserveKey");
        record(
            &mut first_error,
            self.unpreserve_keys(&thread_mgr, &preserved),
        );
        tracing::debug!("Remove langbar");
        record(&mut first_error, self.remove_langbar_item(&thread_mgr));
        first_error
    }

    /// Deactivate phase 2: unadvise the per-instance sinks (thread-mgr event
    /// sink, text-layout sink, compartment sinks) and reset this activation's
    /// fields so TSF can reuse the TextService for the next Activate. Best-
    /// effort — returns the first failure.
    fn teardown_instance_state(&self) -> Result<()> {
        let mut text_service = self.borrow_mut()?;
        let mut first_error = Ok(());

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

        tracing::debug!("UnadviseCompartmentSinks");
        record(
            &mut first_error,
            self.unadvise_compartment_sinks(&mut text_service),
        );

        // clear display attribute
        text_service.display_attribute_atom.clear();

        text_service.tid = 0;
        text_service.thread_mgr = None;
        // The OS compartment is the durable store for the mode now, and the
        // next Activate adopts it back, so the cached copy must not outlive
        // this activation.
        text_service.input_mode = crate::engine::input_mode::InputMode::default();
        text_service.suppress_compartment_echo = false;
        // a stale flag would otherwise be inherited by a plain Activate() that
        // carries no flags of its own
        text_service.activate_flags = 0;
        // Also let go of the last document's context: handle_key re-sets it on
        // every keystroke after the next Activate, and end_composition() above
        // already ran, so nothing dereferences it in between. Keeping it would
        // pin the host's ITfContext while the TIP is deactivated. (The old
        // `this` self-reference is gone entirely — TSF reuses this object
        // across Deactivate/Activate cycles, and every callback reaches its COM
        // interfaces via TextServiceFactory::this(), a QI on the containing
        // allocation, so there is nothing to clear/restore.)
        text_service.context = None;
        text_service.layout_context = None;

        // Reset the engine composition. TSF reuses this TextService across
        // Deactivate/Activate, so a leftover Composing state (and its reading)
        // would otherwise resume after an in-app IME switch (Win+Space and back
        // within the same app, which does NOT fire OnSetFocus): the first
        // keystroke would land in the Composing arm with no live
        // tip_composition, set_text would no-op, and the typing would be
        // invisible.
        match text_service.borrow_mut_composition() {
            Ok(mut composition) => *composition = Composition::default(),
            Err(error) => record(&mut first_error, Err(error)),
        }

        first_error
    }
}

impl ITfTextInputProcessor_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    // skip(self): the #[implement]-generated TextServiceFactory_Impl has no
    // Debug, and the factory's Debug output is a whole composition dump in
    // any case — the tid is what identifies the activation
    #[tracing::instrument(skip(self, ptim))]
    fn Activate(&self, ptim: windows_core::Ref<'_, ITfThreadMgr>, tid: u32) -> Result<()> {
        tracing::debug!("Activated with tid: {tid}");

        // add reference to the dll instance to prevent it from being unloaded
        let mut dll_instance = DllModule::get()?;
        dll_instance.add_ref();

        // initialize ipc_service
        // Activate() should not return an error: if it does, the icon of the
        // previously activated TextService is displayed, confusing the user.
        match ipc_service::IPCService::new() {
            Ok(mut ipc_service) => {
                // Warm up the lazy connection; if the server is not running
                // yet this fails harmlessly and the channel reconnects on
                // the next keystroke. The attempt also settles
                // `engine_health` for the language-bar tooltip, which is the
                // only way a user finds out the engine never started — the
                // TIP otherwise looks perfectly healthy (issue #79).
                //
                // ERROR, not WARN: this is the line to look for first when
                // the report is "it stopped converting".
                if let Err(e) = ipc_service.append_text("".to_string()) {
                    tracing::error!(
                        "azookey server not reachable at Activate; \
                         conversion will not work until launcher.exe is running: {e}"
                    );
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
        let thread_mgr = match ptim.as_ref() {
            Some(thread_mgr) => thread_mgr.clone(),
            None => {
                dll_instance.release();
                return Err(anyhow::anyhow!("Thread manager is null"));
            }
        };

        // Scoped on purpose: the advise below must hold NO borrow. TSF gives
        // the focus to a freshly advised key sink synchronously, from inside
        // AdviseKeyEventSink, and our OnSetFocus goes on to
        // sync_input_mode_from_compartments -> apply_input_mode, which
        // re-enters this very RefCell. Holding the borrow across the advise
        // made that fail on every activation (issue #62).
        {
            let mut text_service = self.borrow_mut()?;
            text_service.tid = tid;
            text_service.thread_mgr = Some(thread_mgr.clone());
        }

        // The key-event sink is the lifeline: without it NO keystroke reaches
        // the TIP and nothing composes or converts. This is the ONLY step
        // whose failure is fatal — undo tid/thread_mgr and the dll ref, and
        // report it.
        //
        // The compartments are not initialised yet at this point, so the
        // re-entrant OnSetFocus reads a VT_EMPTY open/close on a fresh thread
        // — which `decode` already treats as "nobody has decided" rather than
        // as a failure.
        tracing::debug!("AdviseKeyEventSink");
        if let Err(error) = self.advise_key_sink(&thread_mgr, tid) {
            tracing::error!("AdviseKeyEventSink failed; the TIP cannot receive keys: {error:?}");
            // re-take the borrow for the rollback; a borrow that is somehow
            // unavailable here must not mask the advise failure itself
            match self.borrow_mut() {
                Ok(mut text_service) => {
                    text_service.tid = 0;
                    text_service.thread_mgr = None;
                }
                Err(error) => tracing::warn!("could not roll back the activation: {error:?}"),
            }
            dll_instance.release();
            return Err(error);
        }

        // Claim the IME on/off keys. Advisory, and deliberately still outside
        // any borrow: without this the OS keeps Alt+` (the US-layout on/off
        // chord) to itself and the key never reaches the TIP at all (#19).
        tracing::debug!("PreserveKey (IME on/off)");
        let preserved = self.preserve_toggle_keys(&thread_mgr, tid);

        let mut text_service = self.borrow_mut()?;
        text_service.preserved_keys = preserved;

        // Everything below is ADVISORY. A failure here must NOT abort Activate:
        //   - returning Err leaves the previously active IME's icon up (the
        //     user cannot select azooKey), and
        //   - unwinding the key-event sink on such a failure stops conversion
        //     entirely. A langbar AddItem failure taking typing down with it
        //     was exactly the "cannot convert" regression.
        // Warn and keep the key sink. Deactivate later unadvises whatever the
        // per-instance cookie map recorded, so nothing that succeeded leaks.

        tracing::debug!("AdviseThreadMgrEventSink");
        if let Err(error) = (|| -> Result<()> {
            self.advise_sink::<ITfThreadMgrEventSink>(
                &thread_mgr.cast::<ITfSource>()?,
                &mut text_service,
            )
        })() {
            tracing::warn!("AdviseThreadMgrEventSink failed (non-fatal): {error:?}");
        }

        tracing::debug!("AdviseTextLayoutSink");
        if let Ok(doc_mgr) = unsafe { thread_mgr.GetFocus() }
            && let Err(error) = self.advise_text_layout_sink(&mut text_service, doc_mgr)
        {
            tracing::warn!("AdviseTextLayoutSink failed (non-fatal): {error:?}");
        }

        tracing::debug!("Initialize display attribute");
        let atom_map: Result<HashMap<_, _>> = (|| {
            let mut map = HashMap::new();
            let category_mgr: ITfCategoryMgr =
                unsafe { CoCreateInstance(&CLSID_TF_CategoryMgr, None, CLSCTX_INPROC_SERVER)? };
            let atom = unsafe { category_mgr.RegisterGUID(&GUID_DISPLAY_ATTRIBUTE)? };
            map.insert(GUID_DISPLAY_ATTRIBUTE, atom);
            Ok(map)
        })();
        match atom_map {
            Ok(map) => text_service.display_attribute_atom = map,
            Err(error) => {
                tracing::warn!("display attribute registration failed (non-fatal): {error:?}")
            }
        }

        tracing::debug!("Initialize langbar");
        if let Err(error) = self.add_langbar_item(&thread_mgr) {
            tracing::warn!("langbar AddItem failed (non-fatal): {error:?}");
        }

        tracing::debug!("Initialize input-mode compartments");
        // Advisory: a host without ITfCompartmentMgr (or one that refuses the
        // compartments) must still get a working IME.
        let adopted = match self.init_compartments(&mut text_service) {
            Ok(adopted) => adopted,
            Err(error) => {
                tracing::warn!("compartment setup failed (non-fatal): {error:?}");
                None
            }
        };

        // The OS held a mode from before this activation; adopt it so the
        // user's choice survives a profile switch. Must happen after the
        // borrow is released -- apply_input_mode re-enters the RefCell.
        drop(text_service);
        if let Some(mode) = adopted
            && let Err(error) = self.apply_input_mode(mode, false)
        {
            tracing::warn!("adopting the compartment mode failed (non-fatal): {error:?}");
        }

        tracing::debug!("Activate success");

        Ok(())
    }

    #[macros::anyhow]
    #[tracing::instrument(skip(self))]
    fn Deactivate(&self) -> Result<()> {
        tracing::debug!("Deactivated");

        // remove reference to the dll instance
        let mut dll_instance = DllModule::get()?;
        dll_instance.release();

        // Best-effort teardown: every step runs even if an earlier one fails.
        // The old code chained the unadvises with `?`, so a single failing
        // step (e.g. end_composition on a document being torn down during a
        // profile switch) stranded the remaining unadvises — leaking a sink
        // into the host or pinning its ITfContext. `record` remembers the
        // first error; it is surfaced at the end, in the order the steps run.
        let mut first_error: Result<()> = Ok(());

        // end composition (releases the client-side handle even on failure)
        record(&mut first_error, self.end_composition());

        // MANDATORY, not best-effort housekeeping: BeginUIElement made the
        // host AddRef this object and hold it until EndUIElement. Leaving an
        // element open across Deactivate pins the TIP in the host forever —
        // the B15 leak shape all over again.
        record(&mut first_error, self.ui_end());

        // phase 1: thread-scoped registrations (key sink, langbar)
        record(&mut first_error, self.teardown_thread_scoped());

        // phase 2: per-instance sinks + field reset for the next Activate
        record(&mut first_error, self.teardown_instance_state());

        if first_error.is_ok() {
            tracing::debug!("Deactivate success");
        }

        first_error
    }
}

impl ITfTextInputProcessorEx_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    fn ActivateEx(
        &self,
        ptim: windows_core::Ref<'_, ITfThreadMgr>,
        tid: u32,
        dwflags: u32,
    ) -> Result<()> {
        // called when the text service is activated
        // if this function is implemented, the Activate() function won't be called
        // so we need to call the Activate function manually
        tracing::debug!("Activated(Ex) with tid: {tid}, flags: {dwflags:#x}");

        // Diagnostics only: UI suppression is decided by BeginUIElement's
        // pbShow, never by an activation flag. Scope the borrow — Activate
        // takes its own borrow_mut straight away.
        {
            if let Ok(mut text_service) = self.borrow_mut() {
                text_service.activate_flags = dwflags;
            }
        }

        self.Activate(ptim, tid)?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::rc::Rc;
    use std::sync::Mutex;

    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    use windows::Win32::UI::TextServices::{
        GUID_COMPARTMENT_KEYBOARD_OPENCLOSE, ITfTextInputProcessor, ITfThreadMgr,
    };

    use crate::engine::composition::CompositionState;
    use crate::engine::state::IMEState;
    use crate::globals::{DLL_INSTANCE, DllModule};
    use crate::tsf::factory::TextServiceFactory;
    use crate::tsf::test_support::{
        CompartmentLog, EditSessionBehavior, FakeContext, FakeThreadMgr, ThreadMgrLog, factory_of,
        fake_context_of,
    };

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

    /// Regression guard for the "cannot convert" bug: a failing *advisory*
    /// Activate step (here the langbar AddItem, the last step) must NOT take
    /// the key-event sink down with it. The key sink is the lifeline — without
    /// it no keystroke reaches the TIP and nothing converts. An earlier
    /// version rolled back every advised sink on any late failure, which
    /// unadvised the key sink and broke all input whenever AddItem failed.
    #[test]
    fn a_failing_langbar_keeps_the_key_sink_and_activation_succeeds() {
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
            result.is_ok(),
            "a failing langbar is advisory — Activate must still succeed so \
             the key sink stays live and typing works: {result:?}"
        );

        assert_eq!(
            log.key_sink_advises.borrow().as_slice(),
            &[true],
            "the key-event sink must be advised"
        );
        assert_eq!(
            log.key_sink_unadvises.get(),
            0,
            "the key-event sink must NOT be unadvised when only the langbar \
             fails (unadvising it is what stopped conversion)"
        );

        // the TIP is genuinely active: tid is set, so keystrokes are handled
        let factory = factory_of(&tip);
        assert_eq!(
            factory.borrow().unwrap().tid,
            1,
            "a successful (advisory-degraded) Activate keeps the tid"
        );

        unsafe { tip.Deactivate() }.expect("Deactivate must succeed");
        reset_ime_state();
    }

    /// Issue #62: TSF gives the focus to a freshly advised key sink from
    /// *inside* `AdviseKeyEventSink`, synchronously on this thread. Activate
    /// therefore must hold no borrow of the TextService when it advises —
    /// otherwise `OnSetFocus` -> `sync_input_mode_from_compartments` fails
    /// with `RefCell already mutably borrowed` on every single activation.
    #[test]
    fn a_synchronous_focus_during_advise_can_read_the_text_service() {
        let _guard = crate::tsf::test_support::global_state_lock();
        ensure_dll_module();
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        reset_ime_state();

        // an OS that already holds "IME on" — what the callback goes to read
        let compartments = Rc::new(CompartmentLog::default());
        compartments.preset(GUID_COMPARTMENT_KEYBOARD_OPENCLOSE, 1);

        let log = Rc::new(ThreadMgrLog::default());
        let thread_mgr = FakeThreadMgr::with_sync_focus_on_advise(log.clone(), compartments);
        let tip = TextServiceFactory::create::<ITfTextInputProcessor>()
            .expect("failed to create the TIP");

        unsafe { tip.Activate(Some(&thread_mgr), 1) }
            .expect("a synchronous OnSetFocus must not break Activate");

        assert_eq!(
            log.borrow_ok_in_sync_focus.get(),
            Some(true),
            "the TextService must be borrowable during the OnSetFocus that \
             AdviseKeyEventSink dispatches (issue #62)"
        );

        // and the callback really did its work: the mode the OS held is live
        let factory = factory_of(&tip);
        assert_eq!(
            factory.borrow().unwrap().input_mode,
            crate::engine::input_mode::InputMode::Kana,
            "the synchronous focus must have adopted the OS mode"
        );

        unsafe { tip.Deactivate() }.expect("Deactivate must succeed");
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
        let factory = factory_of(&tip);
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
