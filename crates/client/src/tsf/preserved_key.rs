//! Preserved keys: the IME on/off keys TSF routes to us instead of to the
//! focused application.
//!
//! Until this module existed the TIP only saw Zenkaku/Hankaku (VK 0xF3/0xF4)
//! because those keys happen to reach `OnKeyDown` as ordinary virtual keys on
//! a JIS keyboard. Everything else the OS treats as an IME hotkey — VK_KANJI,
//! and above all **Alt+`**, the standard IME on/off chord on a US layout — is
//! swallowed by TSF and never delivered as a keystroke at all. A US-layout
//! user therefore had no way to turn the IME on or off from the keyboard
//! (issue #19).
//!
//! `ITfKeystrokeMgr::PreserveKey` is the supported way to claim those: TSF
//! then calls `ITfKeyEventSink::OnPreservedKey` with the GUID we registered.
//!
//! Two rules govern everything here:
//!
//! 1. **Reserving is advisory.** A host that refuses (or has no
//!    `ITfKeystrokeMgr`) must still get a working IME, so a failure is warned
//!    about and the raw-VK path in `process_key` stays as the safety net.
//! 2. **Only what was reserved is unpreserved, and only what was reserved
//!    suppresses the raw VK.** Both directions read the same per-activation
//!    registry (`TextService::preserved_keys`) — the same discipline the sink
//!    cookies follow (B14). Suppressing the raw VK unconditionally would lose
//!    the toggle on hosts where the reservation failed; not suppressing it
//!    when the reservation took makes hosts that deliver *both* toggle twice.

use std::time::{Duration, Instant};

use anyhow::Result;
use windows::{
    Win32::UI::TextServices::{ITfKeystrokeMgr, ITfThreadMgr, TF_MOD_ALT, TF_PRESERVEDKEY},
    core::{GUID, Interface},
};

use crate::globals::{
    GUID_PRESERVEDKEY_TOGGLE_ALT_GRAVE, GUID_PRESERVEDKEY_TOGGLE_KANJI,
    GUID_PRESERVEDKEY_TOGGLE_ZENHAN,
};

use super::factory::TextServiceFactory_Impl;

/// `VK_DBE_SBCSCHAR` / `VK_DBE_DBCSCHAR` — the two virtual keys the
/// Zenkaku/Hankaku key produces depending on the current mode.
const VK_ZENKAKU_HANKAKU: [u32; 2] = [0xF3, 0xF4];
/// `VK_KANJI`. The 漢字 key of a JIS keyboard — and, with Alt held, what
/// Windows translates **Alt+`** into on a 101-key Japanese layout. Measured
/// on hardware: the key arrives as `OnKeyDown(wparam=0x19)` with the Alt
/// flag set in lparam and the scan code of the `` ` `` key (0x29). It does
/// NOT arrive as `VK_OEM_3`, so that reservation alone never fires.
const VK_KANJI: u32 = 0x19;
/// `VK_OEM_3` — `` ` `` on a US layout. Reserved with Alt as well, for
/// layouts and hosts where the translation above does not happen.
const VK_OEM_3: u32 = 0xC0;

/// Every key this TIP asks TSF to route through `OnPreservedKey`, with the
/// description TSF shows in the keyboard-shortcut UI.
const TOGGLE_KEYS: [(GUID, TF_PRESERVEDKEY, &str); 5] = [
    (
        GUID_PRESERVEDKEY_TOGGLE_ZENHAN,
        TF_PRESERVEDKEY {
            uVKey: VK_ZENKAKU_HANKAKU[0],
            uModifiers: 0,
        },
        "azooKey: 入力モード切替",
    ),
    (
        GUID_PRESERVEDKEY_TOGGLE_ZENHAN,
        TF_PRESERVEDKEY {
            uVKey: VK_ZENKAKU_HANKAKU[1],
            uModifiers: 0,
        },
        "azooKey: 入力モード切替",
    ),
    (
        GUID_PRESERVEDKEY_TOGGLE_KANJI,
        TF_PRESERVEDKEY {
            uVKey: VK_KANJI,
            uModifiers: 0,
        },
        "azooKey: 入力モード切替 (漢字)",
    ),
    // Alt+` on a 101-key Japanese layout. Windows has already turned the
    // `` ` `` key into VK_KANJI by the time TSF sees it, but Alt is still
    // held — so the unmodified reservation above does not match and this
    // one is what actually fires. Without it the chord reaches OnKeyDown,
    // where the Ctrl/Alt branch discards it as a host shortcut, and a
    // US-layout user has no keyboard route to the IME at all (issue #19).
    (
        GUID_PRESERVEDKEY_TOGGLE_KANJI,
        TF_PRESERVEDKEY {
            uVKey: VK_KANJI,
            uModifiers: TF_MOD_ALT,
        },
        "azooKey: 入力モード切替 (Alt+`)",
    ),
    (
        GUID_PRESERVEDKEY_TOGGLE_ALT_GRAVE,
        TF_PRESERVEDKEY {
            uVKey: VK_OEM_3,
            uModifiers: TF_MOD_ALT,
        },
        "azooKey: 入力モード切替 (Alt+`)",
    ),
];

impl TextServiceFactory_Impl {
    /// Reserves the IME on/off keys and reports the ones that took, for the
    /// caller to record. Never fails: an unreservable key is warned about and
    /// left to the raw-VK path.
    ///
    /// Takes no borrow of the `TextService` on purpose — this runs from
    /// Activate, right after the key sink is advised, and everything on that
    /// stretch has to stay re-entrancy-safe (issue #62).
    pub fn preserve_toggle_keys(
        &self,
        thread_mgr: &ITfThreadMgr,
        tid: u32,
    ) -> Vec<(GUID, TF_PRESERVEDKEY)> {
        let keystroke_mgr = match thread_mgr.cast::<ITfKeystrokeMgr>() {
            Ok(keystroke_mgr) => keystroke_mgr,
            Err(error) => {
                tracing::warn!(
                    "no ITfKeystrokeMgr; the IME on/off keys stay unreserved: {error:?}"
                );
                return Vec::new();
            }
        };

        let mut reserved = Vec::new();
        for (guid, key, description) in TOGGLE_KEYS {
            // PreserveKey takes a counted (not NUL-terminated) string
            let description: Vec<u16> = description.encode_utf16().collect();
            let result = unsafe { keystroke_mgr.PreserveKey(tid, &guid, &key, &description) };

            match result {
                Ok(()) => reserved.push((guid, key)),
                Err(error) => tracing::warn!(
                    "PreserveKey(vk={:#04x}, modifiers={:#x}) failed (non-fatal): {error:?}",
                    key.uVKey,
                    key.uModifiers
                ),
            }
        }

        self.log_reservation_state(&keystroke_mgr, &reserved);

        reserved
    }

    /// Releases the reservations `preserve_toggle_keys` obtained. Best-effort
    /// like the unadvise steps around it: every key is attempted and the first
    /// failure is reported.
    pub fn unpreserve_keys(
        &self,
        thread_mgr: &ITfThreadMgr,
        keys: &[(GUID, TF_PRESERVEDKEY)],
    ) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }

        let keystroke_mgr = thread_mgr.cast::<ITfKeystrokeMgr>()?;
        let mut first_error: Result<()> = Ok(());

        for (guid, key) in keys {
            if let Err(error) = unsafe { keystroke_mgr.UnpreserveKey(guid, key) } {
                tracing::warn!("UnpreserveKey(vk={:#04x}) failed: {error:?}", key.uVKey);
                if first_error.is_ok() {
                    first_error = Err(error.into());
                }
            }
        }

        first_error
    }

    /// Whether `guid` names a key *this activation* reserved. `OnPreservedKey`
    /// is a process-wide callback, so a GUID we never registered belongs to
    /// somebody else and must be handed back.
    pub fn is_preserved_toggle(&self, guid: &GUID) -> Result<bool> {
        Ok(self.borrow()?.preserved_keys.iter().any(|(g, _)| g == guid))
    }

    /// Records that `OnPreservedKey` just toggled the mode, so a raw VK for
    /// the same press can be recognised as a duplicate.
    pub fn note_preserved_toggle(&self) -> Result<()> {
        self.borrow_mut()?.last_preserved_toggle = Some(Instant::now());
        Ok(())
    }

    /// Whether a raw on/off keystroke is the echo of a press `OnPreservedKey`
    /// has already handled.
    ///
    /// A host that both dispatches the preserved key and delivers the raw VK
    /// would toggle twice for one press. The window is deliberately tiny: the
    /// two deliveries of one press are microseconds apart, while a human
    /// double-tap is tens of milliseconds at best.
    pub fn raw_toggle_is_duplicate(&self) -> Result<bool> {
        Ok(self
            .borrow()?
            .last_preserved_toggle
            .is_some_and(|at| at.elapsed() < DOUBLE_DELIVERY_WINDOW))
    }

    /// Asks TSF whether it actually holds the reservations we made. Purely
    /// diagnostic: `PreserveKey` returning S_OK turned out NOT to mean the
    /// chord will ever be dispatched (issue #19), so the log has to record
    /// TSF's own view rather than our request.
    fn log_reservation_state(
        &self,
        keystroke_mgr: &ITfKeystrokeMgr,
        reserved: &[(GUID, TF_PRESERVEDKEY)],
    ) {
        for (guid, key) in reserved {
            let held = unsafe { keystroke_mgr.IsPreservedKey(guid, key) };
            tracing::debug!(
                "IsPreservedKey(vk={:#04x}, modifiers={:#x}) = {:?}",
                key.uVKey,
                key.uModifiers,
                held.map(|b| b.as_bool())
            );
        }
    }
}

/// How long after an `OnPreservedKey` toggle a raw on/off VK counts as the
/// same press rather than a new one.
const DOUBLE_DELIVERY_WINDOW: Duration = Duration::from_millis(50);

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    use std::rc::Rc;
    use windows::Win32::Foundation::WPARAM;
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    use windows::Win32::UI::TextServices::{ITfContext, ITfKeyEventSink, ITfTextInputProcessor};

    use crate::engine::input_mode::InputMode;
    use crate::engine::ipc_service::IPCService;
    use crate::engine::state::IMEState;
    use crate::globals::{DLL_INSTANCE, DllModule};
    use crate::tsf::factory::TextServiceFactory;
    use crate::tsf::test_support::{
        EditSessionBehavior, FakeContext, FakeThreadMgr, ThreadMgrLog, factory_of,
        global_state_lock,
    };

    /// A TIP activated against `thread_mgr`, with a recording IPC service in
    /// place so the mode-switch actions can run to completion (the real one
    /// Activate installs talks to a pipe no test has).
    fn activate(
        thread_mgr: &windows::Win32::UI::TextServices::ITfThreadMgr,
    ) -> ITfTextInputProcessor {
        let _ = DLL_INSTANCE.set(std::sync::Mutex::new(DllModule::new()));
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };

        let tip = TextServiceFactory::create::<ITfTextInputProcessor>()
            .expect("failed to create the TIP");
        unsafe { tip.Activate(Some(thread_mgr), 1) }.expect("Activate must succeed");

        let (service, _fake) = IPCService::new_fake().unwrap();
        IMEState::get().unwrap().ipc_service = Some(service);

        tip
    }

    fn teardown(tip: &ITfTextInputProcessor) {
        let _ = unsafe { tip.Deactivate() };
        IMEState::get().unwrap().ipc_service = None;
    }

    /// The advise/unadvise balance, for reservations: Deactivate must release
    /// exactly the (guid, key) pairs Activate got — no more (another TIP's
    /// reservation) and no fewer (a key the OS keeps routing to a dead sink).
    #[test]
    fn activate_reserves_the_toggle_keys_and_deactivate_releases_them() {
        let _guard = global_state_lock();
        let log = Rc::new(ThreadMgrLog::default());
        let thread_mgr = FakeThreadMgr::new(log.clone());

        let tip = activate(&thread_mgr);

        let reserved = log.preserved_keys.borrow().clone();
        assert_eq!(
            reserved.len(),
            TOGGLE_KEYS.len(),
            "Activate must reserve every IME on/off key: {reserved:?}"
        );
        assert!(
            reserved
                .iter()
                .any(|(_, key)| key.uVKey == VK_OEM_3 && key.uModifiers == TF_MOD_ALT),
            "Alt+` is the US-layout on/off chord and the reason for issue #19"
        );

        unsafe { tip.Deactivate() }.expect("Deactivate must succeed");

        assert_eq!(
            log.unpreserved_keys.borrow().as_slice(),
            reserved.as_slice(),
            "Deactivate must unpreserve exactly what Activate reserved"
        );
        IMEState::get().unwrap().ipc_service = None;
    }

    /// The feature itself: TSF delivers the reserved key through
    /// `OnPreservedKey`, and that must flip あ/A. The old stub did nothing
    /// but answer TRUE.
    #[test]
    fn a_preserved_key_toggles_the_input_mode() {
        let _guard = global_state_lock();
        let thread_mgr = FakeThreadMgr::new(Rc::new(ThreadMgrLog::default()));
        let tip = activate(&thread_mgr);
        let context = FakeContext::new(EditSessionBehavior::RunSync);

        let factory = factory_of(&tip);
        assert_eq!(factory.borrow().unwrap().input_mode, InputMode::Latin);

        let sink: ITfKeyEventSink = windows::core::Interface::cast(&tip).unwrap();
        let handled =
            unsafe { sink.OnPreservedKey(Some(&context), &GUID_PRESERVEDKEY_TOGGLE_ALT_GRAVE) }
                .expect("OnPreservedKey must not fail");

        assert!(handled.as_bool(), "our own preserved key must be eaten");
        assert_eq!(
            factory.borrow().unwrap().input_mode,
            InputMode::Kana,
            "the reserved key must toggle the input mode"
        );
        teardown(&tip);
    }

    /// `OnPreservedKey` is called for keys we never registered too. The old
    /// unconditional TRUE ate those — including other TIPs' and the system's.
    #[test]
    fn an_unknown_preserved_key_is_handed_back() {
        let _guard = global_state_lock();
        let thread_mgr = FakeThreadMgr::new(Rc::new(ThreadMgrLog::default()));
        let tip = activate(&thread_mgr);
        let context: ITfContext = FakeContext::new(EditSessionBehavior::RunSync);
        let factory = factory_of(&tip);

        let stranger = GUID::from_u128(0x0badf00d_0000_0000_0000_000000000001);
        let sink: ITfKeyEventSink = windows::core::Interface::cast(&tip).unwrap();
        let handled = unsafe { sink.OnPreservedKey(Some(&context), &stranger) }.unwrap();

        assert!(
            !handled.as_bool(),
            "a key we never reserved belongs to somebody else"
        );
        assert_eq!(
            factory.borrow().unwrap().input_mode,
            InputMode::Latin,
            "and it must not have changed anything"
        );
        teardown(&tip);
    }

    /// A raw on/off VK is handled even though the reservation was accepted.
    ///
    /// This asserts the opposite of what the first attempt did, on purpose.
    /// Suppressing the raw VK whenever `PreserveKey` had returned S_OK is
    /// unsound: TSF accepts reservations it then never dispatches (measured
    /// with Alt+VK_KANJI on a 101-key layout), and the keystroke fell into
    /// the gap — Alt+` did nothing at all. A raw VK arriving IS the evidence
    /// that TSF did not route the key, because a key it dispatches through
    /// `OnPreservedKey` is not also delivered raw.
    #[test]
    fn a_raw_toggle_vk_is_handled_even_when_the_reservation_was_accepted() {
        let _guard = global_state_lock();
        let log = Rc::new(ThreadMgrLog::default());
        let thread_mgr = FakeThreadMgr::new(log.clone());
        let tip = activate(&thread_mgr);
        let context = FakeContext::new(EditSessionBehavior::RunSync);
        let factory = factory_of(&tip);

        assert!(
            !log.preserved_keys.borrow().is_empty(),
            "this host accepts the reservations"
        );
        assert!(
            factory.test_key(Some(&context), WPARAM(0xF3)).unwrap(),
            "the raw VK must still be handled — its arrival proves TSF did \
             not route it as a preserved key"
        );
        teardown(&tip);
    }

    /// The double-delivery guard, in its evidence-based form: only a toggle
    /// `OnPreservedKey` actually performed suppresses the raw VK, and only
    /// for as long as one press could plausibly still be arriving.
    #[test]
    fn a_raw_toggle_right_after_a_preserved_one_is_ignored() {
        let _guard = global_state_lock();
        let thread_mgr = FakeThreadMgr::new(Rc::new(ThreadMgrLog::default()));
        let tip = activate(&thread_mgr);
        let context = FakeContext::new(EditSessionBehavior::RunSync);
        let factory = factory_of(&tip);

        factory.note_preserved_toggle().unwrap();

        assert!(
            factory.raw_toggle_is_duplicate().unwrap(),
            "the raw VK arriving with the preserved dispatch is one press"
        );
        assert!(
            !factory.test_key(Some(&context), WPARAM(0xF3)).unwrap(),
            "so it must not toggle a second time"
        );
        teardown(&tip);
    }

    /// …but the suppression must expire, or the next deliberate press would
    /// be swallowed too.
    #[test]
    fn the_double_delivery_window_expires() {
        let _guard = global_state_lock();
        let thread_mgr = FakeThreadMgr::new(Rc::new(ThreadMgrLog::default()));
        let tip = activate(&thread_mgr);
        let factory = factory_of(&tip);

        factory.borrow_mut().unwrap().last_preserved_toggle =
            Some(Instant::now() - DOUBLE_DELIVERY_WINDOW * 2);

        assert!(
            !factory.raw_toggle_is_duplicate().unwrap(),
            "a press long after the last preserved toggle is a new press"
        );
        teardown(&tip);
    }

    /// On a host that refuses the reservation the raw VK is all there is.
    #[test]
    fn the_raw_vk_still_toggles_when_the_reservation_fails() {
        let _guard = global_state_lock();
        let log = Rc::new(ThreadMgrLog::default());
        let thread_mgr = FakeThreadMgr::with_failing_preserve_key(log.clone());

        let tip = activate(&thread_mgr);
        let context = FakeContext::new(EditSessionBehavior::RunSync);
        let factory = factory_of(&tip);

        assert!(
            log.preserved_keys.borrow().is_empty(),
            "the fake host refuses every reservation"
        );
        assert!(
            factory.borrow().unwrap().preserved_keys.is_empty(),
            "only reservations that took may be recorded"
        );
        assert!(
            factory.test_key(Some(&context), WPARAM(0xF3)).unwrap(),
            "without a reservation the raw Zenkaku/Hankaku VK must still toggle"
        );
        teardown(&tip);
    }

    /// The Alt+` reservation is the whole point of the issue-#19 scope
    /// extension: without it a US-layout user cannot toggle the IME at all.
    #[test]
    fn the_reservation_table_covers_both_layouts() {
        let vks: Vec<(u32, u32)> = TOGGLE_KEYS
            .iter()
            .map(|(_, key, _)| (key.uVKey, key.uModifiers))
            .collect();

        assert!(vks.contains(&(0xF3, 0)), "JIS 半角/全角");
        assert!(vks.contains(&(0xF4, 0)), "JIS 半角/全角 (the other VK)");
        assert!(vks.contains(&(VK_KANJI, 0)), "JIS 漢字");
        assert!(vks.contains(&(VK_OEM_3, TF_MOD_ALT)), "US Alt+`");
    }

    /// The reservation that Alt+` ACTUALLY needs. Measured on a 101-key
    /// Japanese layout: the chord reaches the TIP as VK_KANJI with Alt still
    /// held, not as VK_OEM_3. Reserving VK_KANJI unmodified does not match
    /// it, so the first attempt at issue #19 left US-layout users with no
    /// keyboard route to the IME at all — TSF delivered the chord to
    /// OnKeyDown, where the Ctrl/Alt branch discarded it.
    #[test]
    fn alt_grave_is_reserved_as_alt_plus_vk_kanji() {
        let vks: Vec<(u32, u32)> = TOGGLE_KEYS
            .iter()
            .map(|(_, key, _)| (key.uVKey, key.uModifiers))
            .collect();

        assert!(
            vks.contains(&(VK_KANJI, TF_MOD_ALT)),
            "Alt+` arrives as Alt+VK_KANJI on a 101-key layout: {vks:x?}"
        );
    }

    /// Both VK_KANJI reservations must be matchable: the plain 漢字 key with
    /// no modifier, and the Alt chord. One would exclude the other.
    #[test]
    fn both_kanji_reservations_match_their_own_modifier_state() {
        let kanji: Vec<u32> = TOGGLE_KEYS
            .iter()
            .filter(|(_, key, _)| key.uVKey == VK_KANJI)
            .map(|(_, key, _)| key.uModifiers)
            .collect();

        assert_eq!(kanji.len(), 2, "plain 漢字 and Alt+`");
        assert_eq!(kanji[0], 0, "the bare 漢字 key");
        assert_eq!(kanji[1], TF_MOD_ALT, "Alt+` after Windows' translation");
    }

    /// Zenkaku/Hankaku's two virtual keys are the same physical key, so they
    /// share a GUID; the other two must stay distinguishable in the logs.
    #[test]
    fn the_reservation_table_uses_one_guid_per_purpose() {
        let guids: Vec<GUID> = TOGGLE_KEYS.iter().map(|(guid, _, _)| *guid).collect();

        assert_eq!(guids[0], guids[1], "both zenkaku VKs share one GUID");
        assert_eq!(
            std::collections::BTreeSet::from_iter(guids.iter().map(|g| g.to_u128())).len(),
            3
        );
    }
}
