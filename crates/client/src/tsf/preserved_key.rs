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

use anyhow::Result;
use windows::{
    Win32::UI::{
        Input::KeyboardAndMouse::{VK_CONTROL, VK_MENU, VK_SHIFT},
        TextServices::{
            ITfKeystrokeMgr, ITfThreadMgr, TF_MOD_ALT, TF_MOD_CONTROL, TF_MOD_SHIFT,
            TF_PRESERVEDKEY,
        },
    },
    core::{GUID, Interface},
};

use crate::extension::VKeyExt;
use crate::globals::{
    GUID_PRESERVEDKEY_TOGGLE_ALT_GRAVE, GUID_PRESERVEDKEY_TOGGLE_KANJI,
    GUID_PRESERVEDKEY_TOGGLE_ZENHAN,
};

use super::factory::TextServiceFactory_Impl;

/// `VK_DBE_SBCSCHAR` / `VK_DBE_DBCSCHAR` — the two virtual keys the
/// Zenkaku/Hankaku key produces depending on the current mode.
const VK_ZENKAKU_HANKAKU: [u32; 2] = [0xF3, 0xF4];
/// `VK_KANJI`. The 漢字 key of a JIS keyboard, and what the OS synthesises
/// for some IME on/off shortcuts.
const VK_KANJI: u32 = 0x19;
/// `VK_OEM_3` — `` ` `` on a US layout. With Alt this is the standard IME
/// on/off chord there, and the reason this module exists.
const VK_OEM_3: u32 = 0xC0;

/// Every key this TIP asks TSF to route through `OnPreservedKey`, with the
/// description TSF shows in the keyboard-shortcut UI.
const TOGGLE_KEYS: [(GUID, TF_PRESERVEDKEY, &str); 4] = [
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
    (
        GUID_PRESERVEDKEY_TOGGLE_ALT_GRAVE,
        TF_PRESERVEDKEY {
            uVKey: VK_OEM_3,
            uModifiers: TF_MOD_ALT,
        },
        "azooKey: 入力モード切替 (Alt+`)",
    ),
];

/// Whether a `TF_PRESERVEDKEY`'s `uModifiers` is satisfied by the given
/// modifier state. Pure so it can be tested without the keyboard: an
/// unmodified reservation requires all three modifiers *up*, or Alt+`
/// would fire on a plain `` ` `` and eat the character.
///
/// Only Alt/Ctrl/Shift are modelled because those are the only bits
/// [`TOGGLE_KEYS`] uses; a side-specific reservation (`TF_MOD_LALT` and
/// friends) would need this widened.
fn modifiers_satisfied(modifiers: u32, alt: bool, control: bool, shift: bool) -> bool {
    ((modifiers & TF_MOD_ALT) != 0) == alt
        && ((modifiers & TF_MOD_CONTROL) != 0) == control
        && ((modifiers & TF_MOD_SHIFT) != 0) == shift
}

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

    /// Whether a raw keystroke duplicates a reservation that took. Such a key
    /// has already been handled through `OnPreservedKey`, so acting on it
    /// again would toggle twice on hosts that deliver both.
    pub fn is_reserved_keystroke(&self, key_code: usize) -> Result<bool> {
        let Ok(key_code) = u32::try_from(key_code) else {
            return Ok(false);
        };
        let alt = VK_MENU.is_pressed();
        let control = VK_CONTROL.is_pressed();
        let shift = VK_SHIFT.is_pressed();

        Ok(self.borrow()?.preserved_keys.iter().any(|(_, key)| {
            key.uVKey == key_code && modifiers_satisfied(key.uModifiers, alt, control, shift)
        }))
    }
}

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

    /// Double-delivery guard: once the reservation took, the key arrives
    /// through `OnPreservedKey`. A host that ALSO sends the raw VK would
    /// otherwise toggle twice.
    #[test]
    fn a_reserved_zenkaku_key_no_longer_toggles_through_the_raw_vk() {
        let _guard = global_state_lock();
        let thread_mgr = FakeThreadMgr::new(Rc::new(ThreadMgrLog::default()));
        let tip = activate(&thread_mgr);
        let context = FakeContext::new(EditSessionBehavior::RunSync);
        let factory = factory_of(&tip);

        assert!(
            !factory.test_key(Some(&context), WPARAM(0xF3)).unwrap(),
            "the raw VK must be ignored once TSF routes the key to us"
        );
        teardown(&tip);
    }

    /// …and the other half: on a host that refuses the reservation, the raw
    /// VK stays the safety net. Dropping it unconditionally would lose the
    /// toggle entirely there.
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

    /// An unmodified reservation must NOT match while a modifier is down —
    /// otherwise reserving `` ` `` for Alt+` would swallow the plain
    /// backtick, and Ctrl+Space would look like the Zenkaku/Hankaku key.
    #[test]
    fn an_unmodified_reservation_requires_every_modifier_up() {
        assert!(modifiers_satisfied(0, false, false, false));
        assert!(!modifiers_satisfied(0, true, false, false));
        assert!(!modifiers_satisfied(0, false, true, false));
        assert!(!modifiers_satisfied(0, false, false, true));
    }

    #[test]
    fn an_alt_reservation_requires_alt_and_nothing_else() {
        assert!(modifiers_satisfied(TF_MOD_ALT, true, false, false));
        assert!(!modifiers_satisfied(TF_MOD_ALT, false, false, false));
        assert!(
            !modifiers_satisfied(TF_MOD_ALT, true, true, false),
            "Ctrl+Alt+` is AltGr+` on many layouts and is not our chord"
        );
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
