//! Input-mode compartments: the OS-visible half of the IME's on/off and
//! conversion state.
//!
//! `register.rs` declares `GUID_TFCAT_TIPCAP_INPUTMODECOMPARTMENT`, which
//! tells every TSF host that this TIP manages these compartments. Until this
//! module existed that was a false claim: the mode lived only in
//! `TextService.input_mode` and nothing outside the TIP could read or change
//! it. The visible consequence was that an application setting
//! `GUID_COMPARTMENT_KEYBOARD_DISABLED` — a password field, most importantly
//! — could not turn the IME off, and we kept eating keys.
//!
//! Two rules govern everything here:
//!
//! 1. **The OS compartment is the source of truth**; `input_mode` is a cache
//!    of it. `Activate` adopts whatever the compartment already holds, which
//!    is what makes the user's mode survive a profile switch.
//! 2. **Never hold a `RefCell` borrow across `SetValue`.** TSF dispatches
//!    `OnChange` synchronously, on this same STA thread, from inside
//!    `SetValue` — so a live borrow turns into a borrow error inside the
//!    callback. This is the same re-entrancy that `update_lang_bar` hits via
//!    `AddItem` -> `GetIcon` (see engine/composition.rs).
//!
//!    Today that failure happens to be harmless here, because the only
//!    `OnChange` we trigger ourselves is an echo that the guard below
//!    suppresses regardless — so no test can currently catch a violation.
//!    Keep the discipline anyway: it stops being harmless the moment
//!    `OnChange` has real work to do.

use anyhow::Result;
use windows::{
    Win32::System::Variant::VARIANT,
    Win32::UI::TextServices::{
        GUID_COMPARTMENT_EMPTYCONTEXT, GUID_COMPARTMENT_KEYBOARD_DISABLED,
        GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION, GUID_COMPARTMENT_KEYBOARD_OPENCLOSE,
        ITfCompartment, ITfCompartmentEventSink, ITfCompartmentEventSink_Impl, ITfCompartmentMgr,
        ITfContext, ITfSource, TF_CONVERSIONMODE_ALPHANUMERIC, TF_CONVERSIONMODE_FULLSHAPE,
        TF_CONVERSIONMODE_NATIVE, TF_CONVERSIONMODE_ROMAN,
    },
    core::{GUID, Interface},
};

use crate::engine::input_mode::InputMode;

use super::{factory::TextServiceFactory_Impl, text_service::TextService};

/// Japanese kana input: native conversion, full-width, roman entry.
const CONVERSION_KANA: u32 =
    TF_CONVERSIONMODE_NATIVE | TF_CONVERSIONMODE_FULLSHAPE | TF_CONVERSIONMODE_ROMAN;

/// Direct alphanumeric input.
const CONVERSION_LATIN: u32 = TF_CONVERSIONMODE_ALPHANUMERIC;

/// `InputMode` -> `(open/close, conversion)`, both VT_I4.
pub fn encode(mode: &InputMode) -> (i32, i32) {
    match mode {
        InputMode::Kana => (1, CONVERSION_KANA as i32),
        InputMode::Latin => (0, CONVERSION_LATIN as i32),
    }
}

/// `(open/close, conversion)` -> `InputMode`. `conversion` is `None` when
/// nobody has written that compartment yet.
///
/// Open/close is the authority; conversion only refines an OPEN IME. That
/// asymmetry matters because `TF_CONVERSIONMODE_ALPHANUMERIC` is *zero*: an
/// unwritten conversion compartment is indistinguishable from an explicit
/// "alphanumeric" if the two are collapsed. Requiring the `NATIVE` bit
/// unconditionally therefore read every host that toggles only OPENCLOSE —
/// the standard IME on/off path, and what another TIP sharing the thread
/// leaves behind — as Latin no matter how often the user turned the IME on
/// (issue #58).
///
/// Deliberately lossy: `InputMode` has only `Latin` and `Kana`, so an
/// external `TF_CONVERSIONMODE_KATAKANA` decodes to `Kana` and our write-back
/// clears the katakana bit. Full katakana / half-width round-tripping needs a
/// wider `InputMode` and is tracked separately.
pub fn decode(open_close: i32, conversion: Option<i32>) -> InputMode {
    // A closed IME is direct input whatever the conversion mode says.
    if open_close == 0 {
        return InputMode::Latin;
    }

    match conversion {
        // Someone chose a conversion mode: NATIVE is what separates kana
        // input from an IME that is open in alphanumeric ("A") mode.
        Some(conversion) => {
            if (conversion as u32 & TF_CONVERSIONMODE_NATIVE) != 0 {
                InputMode::Kana
            } else {
                InputMode::Latin
            }
        }
        // Open, and nobody has said anything about conversion. Take open at
        // its word rather than inventing an alphanumeric preference.
        None => InputMode::Kana,
    }
}

/// Reads a compartment that may never have been written. `VT_EMPTY` means
/// "nobody has decided yet" and is returned as `None`, which for the
/// conversion compartment is a genuinely different answer from an explicit
/// zero — `TF_CONVERSIONMODE_ALPHANUMERIC` *is* zero (issue #58).
fn read_optional_i32(compartment: &ITfCompartment) -> Option<i32> {
    let variant = unsafe { compartment.GetValue() }.ok()?;
    if variant.is_empty() {
        return None;
    }
    i32::try_from(&variant).ok()
}

/// Reads a compartment as an i32, where "unset" and zero mean the same thing
/// (open/close and the boolean flags, unlike the conversion mode).
fn read_i32(compartment: &ITfCompartment) -> i32 {
    read_optional_i32(compartment).unwrap_or(0)
}

/// Whether a compartment holds a non-zero (i.e. "set") value.
fn is_set(compartment: &ITfCompartment) -> bool {
    read_i32(compartment) != 0
}

fn compartment_of(mgr: &ITfCompartmentMgr, guid: &GUID) -> Result<ITfCompartment> {
    Ok(unsafe { mgr.GetCompartment(guid)? })
}

impl TextServiceFactory_Impl {
    /// The thread-scoped compartment manager. Open/close and conversion mode
    /// both live on the thread manager, not on a context.
    fn thread_compartments(text_service: &TextService) -> Result<ITfCompartmentMgr> {
        Ok(text_service.thread_mgr()?.cast::<ITfCompartmentMgr>()?)
    }

    /// Adopts the mode the OS already holds, then starts listening.
    ///
    /// Order matters: we read (and only write a default if the compartment is
    /// genuinely unset) *before* advising, so our own initialisation cannot
    /// echo back into `OnChange` while `Activate` still holds the borrow.
    ///
    /// Returns the adopted mode when it differs from what we had, so the
    /// caller can refresh the langbar *after* dropping its borrow.
    pub fn init_compartments(&self, text_service: &mut TextService) -> Result<Option<InputMode>> {
        let mgr = Self::thread_compartments(text_service)?;
        let tid = text_service.tid;

        let open_close = compartment_of(&mgr, &GUID_COMPARTMENT_KEYBOARD_OPENCLOSE)?;
        let conversion = compartment_of(&mgr, &GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION)?;

        // VT_EMPTY on open/close means no IME has claimed this thread yet, so
        // there is nothing to adopt and we publish our own default instead.
        // Note `i32::try_from` succeeds on an empty VARIANT (yielding 0), so
        // emptiness is what has to be inspected here, not the value.
        let open_close_state = read_optional_i32(&open_close);

        let adopted = if let Some(open) = open_close_state {
            let mode = decode(open, read_optional_i32(&conversion));
            let changed = mode != text_service.input_mode;
            text_service.input_mode = mode.clone();
            changed.then_some(mode)
        } else {
            let (open, conv) = encode(&text_service.input_mode);
            // safe to write with the borrow live: no sink is advised yet
            unsafe {
                open_close.SetValue(tid, &VARIANT::from(open))?;
                conversion.SetValue(tid, &VARIANT::from(conv))?;
            }
            None
        };

        for compartment in [
            open_close,
            conversion,
            compartment_of(&mgr, &GUID_COMPARTMENT_KEYBOARD_DISABLED)?,
        ] {
            // `advise_sink` keys its cookie map by sink IID and so can hold
            // only one cookie per sink type; one sink advised on three
            // different compartments needs the source kept alongside the
            // cookie, because UnadviseSink must target the same object.
            let source = compartment.cast::<ITfSource>()?;
            let sink = self.this::<ITfCompartmentEventSink>()?;
            let cookie = unsafe { source.AdviseSink(&ITfCompartmentEventSink::IID, &sink)? };
            text_service.compartment_sinks.push((compartment, cookie));
        }

        Ok(adopted)
    }

    /// Releases every compartment sink advised by `init_compartments`.
    ///
    /// Does **not** clear the compartments themselves: open/close is
    /// thread-scoped and both the host and the next TIP read it.
    pub fn unadvise_compartment_sinks(&self, text_service: &mut TextService) -> Result<()> {
        let mut first_error: Result<()> = Ok(());

        for (compartment, cookie) in std::mem::take(&mut text_service.compartment_sinks) {
            let result = (|| -> Result<()> {
                unsafe { compartment.cast::<ITfSource>()?.UnadviseSink(cookie)? };
                Ok(())
            })();

            if let Err(error) = result {
                tracing::warn!("UnadviseSink(compartment) failed: {error:?}");
                if first_error.is_ok() {
                    first_error = Err(error);
                }
            }
        }

        first_error
    }

    /// Publishes `mode` to the OS compartments.
    ///
    /// The echo guard is set and the borrow dropped *before* `SetValue`,
    /// because the resulting `OnChange` runs synchronously on this thread.
    pub fn write_compartments(&self, mode: &InputMode) -> Result<()> {
        let (tid, open_close, conversion) = {
            let mut text_service = self.borrow_mut()?;
            let mgr = Self::thread_compartments(&text_service)?;
            let pair = (
                text_service.tid,
                compartment_of(&mgr, &GUID_COMPARTMENT_KEYBOARD_OPENCLOSE)?,
                compartment_of(&mgr, &GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION)?,
            );
            text_service.suppress_compartment_echo = true;
            pair
        };

        let (open, conv) = encode(mode);
        let result = (|| -> Result<()> {
            unsafe {
                open_close.SetValue(tid, &VARIANT::from(open))?;
                conversion.SetValue(tid, &VARIANT::from(conv))?;
            }
            Ok(())
        })();

        if let Ok(mut text_service) = self.borrow_mut() {
            text_service.suppress_compartment_echo = false;
        }

        result
    }

    /// Pulls the OS mode back into `input_mode` when they have diverged.
    /// Used from `OnChange` and on focus gain.
    pub fn sync_input_mode_from_compartments(&self) -> Result<()> {
        let (mode, current, suppressed) = {
            let text_service = self.borrow()?;
            let mgr = Self::thread_compartments(&text_service)?;
            let mode = decode(
                read_i32(&compartment_of(&mgr, &GUID_COMPARTMENT_KEYBOARD_OPENCLOSE)?),
                read_optional_i32(&compartment_of(
                    &mgr,
                    &GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION,
                )?),
            );
            (
                mode,
                text_service.input_mode.clone(),
                text_service.suppress_compartment_echo,
            )
        };

        // Two independent guards. The flag covers the notification our own
        // SetValue triggers; the equality check covers deferred notifications
        // and hosts that normalise the value we wrote. Without both, a write
        // can bounce back and forth forever.
        if suppressed || mode == current {
            return Ok(());
        }

        self.apply_input_mode(mode, false)
    }

    /// Whether the host has switched input off for this context.
    ///
    /// A host-capability probe, not a failure path: a host that cannot answer
    /// is treated as enabled, matching the `let Ok(..) = .. else` style used
    /// throughout `surrounded_text.rs`. Note that open/close being 0 is *not*
    /// disablement — Latin is still one of our modes.
    pub fn is_input_disabled(&self, context: Option<&ITfContext>) -> bool {
        let Some(context) = context else {
            return false;
        };

        if let Ok(mgr) = context.cast::<ITfCompartmentMgr>() {
            for guid in [
                &GUID_COMPARTMENT_KEYBOARD_DISABLED,
                &GUID_COMPARTMENT_EMPTYCONTEXT,
            ] {
                if let Ok(compartment) = compartment_of(&mgr, guid)
                    && is_set(&compartment)
                {
                    return true;
                }
            }
        }

        // the thread-scoped flag disables the TIP wholesale
        let Ok(text_service) = self.borrow() else {
            return false;
        };
        let Ok(mgr) = Self::thread_compartments(&text_service) else {
            return false;
        };
        let Ok(compartment) = compartment_of(&mgr, &GUID_COMPARTMENT_KEYBOARD_DISABLED) else {
            return false;
        };

        is_set(&compartment)
    }
}

impl ITfCompartmentEventSink_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    fn OnChange(&self, rguid: *const GUID) -> Result<()> {
        let Some(guid) = (unsafe { rguid.as_ref() }) else {
            return Ok(());
        };

        // GUID is not a structural-match type, so this cannot be a `match`.
        if *guid == GUID_COMPARTMENT_KEYBOARD_OPENCLOSE
            || *guid == GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION
        {
            // Advisory: a host that reports an unreadable compartment must
            // not break typing.
            if let Err(error) = self.sync_input_mode_from_compartments() {
                tracing::warn!("compartment sync failed: {error:?}");
            }
        }

        // KEYBOARD_DISABLED needs no work here: `is_input_disabled` reads it
        // on the key path, so it is always current.

        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    // the tests build a bare factory; the module itself only needs the
    // generated outer type now
    use crate::globals::{DLL_INSTANCE, DllModule};
    use crate::tsf::factory::TextServiceFactory;
    use crate::tsf::test_support::{
        CompartmentLog, EditSessionBehavior, FakeContext, FakeThreadMgr, ThreadMgrLog, factory_of,
        global_state_lock,
    };
    use std::rc::Rc;
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    use windows::Win32::UI::TextServices::{ITfTextInputProcessor, TF_CONVERSIONMODE_KATAKANA};

    fn ensure_dll_module() {
        let _ = DLL_INSTANCE.set(std::sync::Mutex::new(DllModule::new()));
    }

    fn activate_with(compartments: Rc<CompartmentLog>) -> ITfTextInputProcessor {
        ensure_dll_module();
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let tip = TextServiceFactory::create::<ITfTextInputProcessor>()
            .expect("failed to create the TIP");
        let thread_mgr =
            FakeThreadMgr::with_compartments(Rc::new(ThreadMgrLog::default()), compartments);
        unsafe { tip.Activate(Some(&thread_mgr), 1) }.expect("Activate must succeed");
        tip
    }

    #[test]
    fn kana_and_latin_survive_a_compartment_round_trip() {
        for mode in [InputMode::Kana, InputMode::Latin] {
            let (open, conversion) = encode(&mode);
            assert_eq!(decode(open, Some(conversion)), mode);
        }
    }

    /// Open/close being 0 means the IME is off, which is Latin for us — and
    /// crucially is NOT the same as "input disabled".
    #[test]
    fn a_closed_ime_decodes_to_latin() {
        assert_eq!(decode(0, Some(CONVERSION_KANA as i32)), InputMode::Latin);
        assert_eq!(decode(0, None), InputMode::Latin);
    }

    /// Issue #58: a host that turns the IME on by writing OPENCLOSE alone —
    /// the standard on/off path — leaves the conversion compartment
    /// untouched. Requiring the NATIVE bit read that as Latin, so the IME
    /// looked stuck in alphanumeric no matter how often it was switched on.
    #[test]
    fn an_open_ime_with_no_conversion_mode_decodes_to_kana() {
        assert_eq!(decode(1, None), InputMode::Kana);
    }

    /// The other half of that asymmetry: an EXPLICIT alphanumeric conversion
    /// mode is a real state (an IME that is open in "A" mode), and must not
    /// be overridden by open/close. This is why `None` and `Some(0)` cannot
    /// be collapsed — `TF_CONVERSIONMODE_ALPHANUMERIC` is zero.
    #[test]
    fn an_explicit_alphanumeric_mode_beats_an_open_ime() {
        assert_eq!(
            decode(1, Some(TF_CONVERSIONMODE_ALPHANUMERIC as i32)),
            InputMode::Latin
        );
    }

    /// Documented lossiness: `InputMode` has no katakana variant, so an
    /// external katakana request lands on Kana. If `InputMode` ever grows
    /// that variant, this test should change with it.
    #[test]
    fn an_external_katakana_mode_degrades_to_kana() {
        let katakana = (TF_CONVERSIONMODE_NATIVE | TF_CONVERSIONMODE_KATAKANA) as i32;
        assert_eq!(decode(1, Some(katakana)), InputMode::Kana);
    }

    /// The whole point of the feature: when the host disables input for a
    /// context — a password field — the key path must hand the key back.
    #[test]
    fn a_disabled_context_disables_input() {
        let _guard = global_state_lock();
        let compartments = Rc::new(CompartmentLog::default());
        compartments.preset(GUID_COMPARTMENT_KEYBOARD_DISABLED, 1);
        let context = FakeContext::with_compartments(EditSessionBehavior::RunSync, compartments);

        let tip = TextServiceFactory::create::<ITfTextInputProcessor>().unwrap();
        let factory = factory_of(&tip);

        assert!(
            factory.is_input_disabled(Some(&context)),
            "a context with KEYBOARD_DISABLED must disable input"
        );
    }

    #[test]
    fn an_empty_context_disables_input() {
        let _guard = global_state_lock();
        let compartments = Rc::new(CompartmentLog::default());
        compartments.preset(GUID_COMPARTMENT_EMPTYCONTEXT, 1);
        let context = FakeContext::with_compartments(EditSessionBehavior::RunSync, compartments);

        let tip = TextServiceFactory::create::<ITfTextInputProcessor>().unwrap();
        let factory = factory_of(&tip);

        assert!(factory.is_input_disabled(Some(&context)));
    }

    /// An ordinary editable context must stay enabled — otherwise this
    /// feature would silently break all typing.
    #[test]
    fn an_ordinary_context_leaves_input_enabled() {
        let _guard = global_state_lock();
        let context = FakeContext::new(EditSessionBehavior::RunSync);
        let tip = TextServiceFactory::create::<ITfTextInputProcessor>().unwrap();
        let factory = factory_of(&tip);

        assert!(
            !factory.is_input_disabled(Some(&context)),
            "an editable context must not be treated as disabled"
        );
    }

    /// Activate must adopt a mode another IME already published rather than
    /// stamping its own default over it — this is what makes the user's mode
    /// survive a profile switch.
    #[test]
    fn activate_adopts_an_existing_compartment_mode() {
        let _guard = global_state_lock();
        let compartments = Rc::new(CompartmentLog::default());
        compartments.preset(GUID_COMPARTMENT_KEYBOARD_OPENCLOSE, 1);
        compartments.preset(
            GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION,
            CONVERSION_KANA as i32,
        );

        let tip = activate_with(compartments);
        let factory = factory_of(&tip);

        assert_eq!(
            factory.borrow().unwrap().input_mode,
            InputMode::Kana,
            "Activate must adopt the mode the OS already held"
        );
        let _ = unsafe { tip.Deactivate() };
    }

    /// Issue #58, end to end: a host (or another TIP) that opened the IME by
    /// writing OPENCLOSE alone leaves the conversion compartment empty.
    /// Activate must still adopt "open" as Kana instead of reading the empty
    /// compartment as an alphanumeric preference.
    #[test]
    fn activate_adopts_an_open_ime_with_no_conversion_mode() {
        let _guard = global_state_lock();
        let compartments = Rc::new(CompartmentLog::default());
        compartments.preset(GUID_COMPARTMENT_KEYBOARD_OPENCLOSE, 1);
        // deliberately no CONVERSION preset: it stays VT_EMPTY

        let tip = activate_with(compartments);
        let factory = factory_of(&tip);

        assert_eq!(
            factory.borrow().unwrap().input_mode,
            InputMode::Kana,
            "an open IME must not be read as Latin just because nobody \
             wrote a conversion mode"
        );
        let _ = unsafe { tip.Deactivate() };
    }

    /// With nothing published yet, we publish our own default so the shell
    /// and the touch keyboard have something to read.
    #[test]
    fn activate_publishes_a_default_when_the_compartment_is_unset() {
        let _guard = global_state_lock();
        let compartments = Rc::new(CompartmentLog::default());

        let tip = activate_with(compartments.clone());

        assert_eq!(
            compartments.value(GUID_COMPARTMENT_KEYBOARD_OPENCLOSE),
            Some(0),
            "the default (Latin) must be published to the OS"
        );
        let _ = unsafe { tip.Deactivate() };
    }

    /// Deactivate must release every compartment sink; a leaked sink keeps
    /// the host calling into a TIP that has torn its state down.
    #[test]
    fn deactivate_releases_every_compartment_sink() {
        let _guard = global_state_lock();
        let compartments = Rc::new(CompartmentLog::default());

        let tip = activate_with(compartments.clone());
        assert!(
            compartments.live_sinks() > 0,
            "Activate must advise compartment sinks"
        );

        unsafe { tip.Deactivate() }.expect("Deactivate must succeed");

        assert_eq!(
            compartments.live_sinks(),
            0,
            "Deactivate must unadvise every compartment sink it advised"
        );
    }

    /// The behaviour the whole compartment feature exists for: when
    /// something *else* changes the mode — the touch keyboard, the shell, an
    /// IMM32 app — the change must reach `input_mode`.
    ///
    /// Note on scope: this exercises the `OnChange` -> sync path, but it does
    /// **not** prove the borrow discipline. Holding a borrow across our own
    /// `SetValue` turns out to be unobservable, because the resulting
    /// `OnChange` is our own echo and is supposed to do nothing anyway — the
    /// borrow failure and the echo guard produce the same outcome. Verified
    /// by deliberately reintroducing the borrow: every test still passed.
    #[test]
    fn an_external_mode_change_reaches_the_input_mode() {
        let _guard = global_state_lock();
        let compartments = Rc::new(CompartmentLog::default());

        let tip = activate_with(compartments.clone());
        let factory = factory_of(&tip);
        assert_eq!(factory.borrow().unwrap().input_mode, InputMode::Latin);

        compartments.external_set(
            GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION,
            CONVERSION_KANA as i32,
        );
        compartments.external_set(GUID_COMPARTMENT_KEYBOARD_OPENCLOSE, 1);

        assert_eq!(
            factory.borrow().unwrap().input_mode,
            InputMode::Kana,
            "an OS-side mode change must be adopted (this fails if a borrow \
             is held across the synchronous OnChange)"
        );
        let _ = unsafe { tip.Deactivate() };
    }

    #[test]
    fn writing_the_mode_survives_the_synchronous_on_change() {
        let _guard = global_state_lock();
        let compartments = Rc::new(CompartmentLog::default());

        let tip = activate_with(compartments.clone());
        let factory = factory_of(&tip);

        factory
            .write_compartments(&InputMode::Kana)
            .expect("writing the mode must not deadlock or fail on the re-entrant OnChange");

        assert_eq!(
            compartments.value(GUID_COMPARTMENT_KEYBOARD_OPENCLOSE),
            Some(1)
        );
        assert_eq!(
            compartments.value(GUID_COMPARTMENT_KEYBOARD_INPUTMODE_CONVERSION),
            Some(CONVERSION_KANA as i32)
        );
        let _ = unsafe { tip.Deactivate() };
    }

    /// The echo guard must stop our own write from bouncing back. Without
    /// it, OnChange would apply the mode and write it out again, and the two
    /// writes would ping-pong.
    #[test]
    fn our_own_write_does_not_echo_back() {
        let _guard = global_state_lock();
        let compartments = Rc::new(CompartmentLog::default());

        let tip = activate_with(compartments.clone());
        let factory = factory_of(&tip);

        let before = compartments.set_value_calls.get();
        factory.write_compartments(&InputMode::Kana).unwrap();
        let writes = compartments.set_value_calls.get() - before;

        assert_eq!(
            writes, 2,
            "exactly one open/close and one conversion write; more means the \
             OnChange echo wrote again"
        );
        let _ = unsafe { tip.Deactivate() };
    }
}
