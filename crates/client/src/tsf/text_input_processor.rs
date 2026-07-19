use std::collections::HashMap;

use crate::{
    engine::{ipc_service, state::IMEState},
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

use anyhow::{Context, Result};

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

        let mut text_service = self.borrow_mut()?;

        text_service.tid = tid;
        let thread_mgr = ptim.context("Thread manager is null")?;
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

        {
            let text_service = self.borrow()?;
            let thread_mgr = text_service.thread_mgr()?;

            // end composition
            self.end_composition()?;

            // remove key event sink
            tracing::debug!("UnadviseKeyEventSink");
            unsafe {
                thread_mgr
                    .cast::<ITfKeystrokeMgr>()?
                    .UnadviseKeyEventSink(text_service.tid)?;
            };

            tracing::debug!("Remove langbar");
            unsafe {
                thread_mgr
                    .cast::<ITfLangBarItemMgr>()?
                    .RemoveItem(&self.this::<ITfLangBarItemButton>()?)
            }?;
        }

        let mut text_service = self.borrow_mut()?;
        let thread_mgr = text_service.thread_mgr()?;

        // remove thread manager event sink
        tracing::debug!("UnadviseThreadMgrEventSink");
        self.unadvise_sink::<ITfThreadMgrEventSink>(
            &thread_mgr.cast::<ITfSource>()?,
            &mut text_service,
        )?;

        // remove text layout sink
        tracing::debug!("UnadviseTextLayoutSink");
        self.unadvise_text_layout_sink(&mut text_service)?;

        // clear display attribute
        text_service.display_attribute_atom.clear();

        text_service.tid = 0;
        text_service.thread_mgr = None;
        // Also let go of the last document's context: handle_key re-sets it
        // on every keystroke after the next Activate, and end_composition()
        // above already ran, so nothing dereferences it in between. Keeping
        // it would pin the host's ITfContext while the TIP is deactivated.
        // (The old `this` self-reference is gone entirely — TSF reuses this
        // object across Deactivate/Activate cycles, and every callback now
        // reaches its COM interfaces via TextServiceFactory::this(), a QI on
        // the containing allocation, so there is nothing to clear or restore.)
        text_service.context = None;

        tracing::debug!("Deactivate success");

        Ok(())
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::rc::Rc;
    use std::sync::Mutex;

    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::TextServices::{ITfThreadMgr, ITfTextInputProcessor};

    use crate::engine::state::IMEState;
    use crate::globals::{DllModule, DLL_INSTANCE};
    use crate::tsf::factory::TextServiceFactory;
    use crate::tsf::test_support::{
        fake_context_of, EditSessionBehavior, FakeContext, FakeThreadMgr, ThreadMgrLog,
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
        let tip_a = TextServiceFactory::create::<ITfTextInputProcessor>()
            .expect("failed to create TIP A");
        let tip_b = TextServiceFactory::create::<ITfTextInputProcessor>()
            .expect("failed to create TIP B");

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
}
