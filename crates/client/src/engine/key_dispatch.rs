//! Turning key events into action batches.
//!
//! The impure shell around the pure decision tables in
//! [`super::transition`]: it reads the OS/COM state those tables need,
//! decodes the key, and hands the resulting batch to
//! [`super::actions::handle_action`]. New key bindings belong in the
//! transition table, not here.

use anyhow::Result;
use windows::Win32::{
    Foundation::{LPARAM, WPARAM},
    UI::{
        Input::KeyboardAndMouse::{VK_CONTROL, VK_MENU},
        TextServices::{ITfComposition, ITfCompositionSink_Impl, ITfContext},
    },
};

use crate::{
    engine::user_action::{KeyRepeat, UserAction, is_ime_toggle_key, key_repeat},
    extension::VKeyExt as _,
    tsf::factory::TextServiceFactory_Impl,
};

use super::{
    client_action::ClientAction,
    composition::CompositionState,
    transition::{KeystrokeContext, is_modifier_key, shortcut_transition, transition},
};

/// VK_BACK. Named here because this module reasons about the key itself —
/// what its autorepeat means — rather than about the action it decodes to.
const VK_BACK: usize = 0x08;

impl ITfCompositionSink_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    fn OnCompositionTerminated(
        &self,
        _ecwrite: u32,
        _pcomposition: windows_core::Ref<'_, ITfComposition>,
    ) -> Result<()> {
        // The host ended the composition (a click outside, or the host's own
        // decision — Chromium does this right after half-width katakana is
        // written into a composition). Its text is already committed on that
        // side and the object dies when this callback returns, so the
        // dedicated action tears our state down WITHOUT touching the
        // document; the ordinary EndComposition re-writes the range, which a
        // host that replays edits turns into a duplicate insertion (#109).
        tracing::debug!("OnCompositionTerminated");

        let actions = vec![ClientAction::CompositionTerminated];
        self.handle_action(&actions, CompositionState::None)?;

        Ok(())
    }
}

impl TextServiceFactory_Impl {
    /// Impure shell around the pure decision functions
    /// (engine::transition): reads the OS/COM state they need, decodes the
    /// key, and adapts the result. New key bindings belong in the
    /// transition table, not here.
    ///
    /// `Some` means the TIP eats the key and `handle_key` runs the attached
    /// actions; `None` hands the key to the host untouched.
    // skip(self, context): self's Debug is the entire composition including
    // the candidate list — hundreds of entries per span, which buried the
    // logs it was meant to illuminate
    #[tracing::instrument(skip(self, context))]
    pub fn process_key(
        &self,
        context: Option<&ITfContext>,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Result<Option<(Vec<ClientAction>, CompositionState)>> {
        if context.is_none() {
            return Ok(None);
        };

        // The host can switch input off for this context — a password field
        // is the case that matters. Answering None hands the raw key back,
        // which is the whole point: we must not compose here.
        if self.is_input_disabled(context) {
            return Ok(None);
        }

        // A key TSF reserved for us has already arrived through
        // OnPreservedKey. Some hosts deliver the raw VK as well, and acting
        // on both toggles the mode twice. Keyed off the per-activation
        // registry rather than a fixed VK list, so a host where the
        // reservation FAILED keeps the raw-VK toggle as its safety net
        // (issue #19).
        // The IME on/off keys outrank the chord branch below. Windows
        // translates Alt+` on a 101-key Japanese layout into VK_KANJI with
        // Alt STILL HELD (measured on hardware), so the chord branch would
        // throw the user's only on/off key away as a host shortcut.
        //
        // Reaching this at all means TSF did NOT route the key through
        // OnPreservedKey — a reserved key is not also delivered raw — so the
        // press is ours to handle. The recency check below is a precaution
        // against one press being delivered twice, which no host tested so
        // far does; two flips would cancel and the key would look dead.
        if is_ime_toggle_key(wparam.0) {
            if self.toggle_is_duplicate()? {
                tracing::debug!("ignoring a second delivery of one on/off press");
                return Ok(None);
            }
            let ctx = self.keystroke_context()?;
            return Ok(transition(&ctx, UserAction::ToggleInputMode)
                .map(|(next_state, actions)| (actions, next_state)));
        }

        // A Ctrl or Alt chord is the host's shortcut. During a composition
        // the TIP owns the keyboard (MS-IME convention): the chord is eaten
        // and cancels the composition, and the next press — or the chord's
        // own autorepeat, since the state is None by then — passes through
        // and fires the shortcut (issue #5). Without the Alt check,
        // Alt+letter fell through to the ToUnicode decoder, which translates
        // it like a WM_SYSCHAR — the TIP ate the host's menu accelerator and
        // turned it into composition input. (AltGr arrives as Ctrl+Alt, so
        // it took this branch already.)
        if VK_CONTROL.is_pressed() || VK_MENU.is_pressed() {
            let state = {
                let text_service = self.borrow()?;
                text_service.borrow_composition()?.state().clone()
            };
            return Ok(shortcut_transition(&state, is_modifier_key(wparam.0))
                .map(|(next_state, actions)| (actions, next_state)));
        }

        let repeat = key_repeat(lparam.0);

        // The tail of a held Backspace, after the composition it was deleting
        // has ended. Eaten with no actions: passed through, the host would
        // delete the text the user just committed. Checked BEFORE the key is
        // decoded, because decoding runs ToUnicode and this key is not going
        // to become text either way.
        //
        // A read, never a write — `test_key` comes through here too and has
        // to stay a pure query (issue #26). Arming and disarming belong to
        // `handle_key`, which is the path a real keystroke takes.
        if self.backspace_repeat_is_discarded(wparam.0, repeat)? {
            tracing::debug!("discarding a Backspace repeat past the end of the composition");
            return Ok(Some((vec![], CompositionState::None)));
        }

        let ctx = self.keystroke_context()?;
        let action = UserAction::decode(wparam.0, repeat)?;

        Ok(transition(&ctx, action).map(|(next_state, actions)| (actions, next_state)))
    }

    /// Whether this keystroke is autorepeat left over from a Backspace that
    /// already ended the composition. See
    /// [`TextService::discard_backspace_repeat`].
    ///
    /// The state check is what keeps this narrow: a repeat that still has a
    /// composition to delete from is ordinary input, and only the gap between
    /// "the composition ended" and "the user let go" is discarded.
    fn backspace_repeat_is_discarded(&self, key_code: usize, repeat: KeyRepeat) -> Result<bool> {
        if key_code != VK_BACK || !repeat.is_repeat {
            return Ok(false);
        }

        let text_service = self.borrow()?;
        if !text_service.discard_backspace_repeat {
            return Ok(false);
        }

        let state = text_service.borrow_composition()?.state().clone();
        Ok(state == CompositionState::None)
    }

    /// Arms or disarms the discard guard for the key that just ran.
    ///
    /// Called after the actions, so `process_key` above saw the state as it
    /// was when the key arrived:
    ///
    /// * a Backspace that ended the composition arms it,
    /// * any other key, and a Backspace pressed fresh rather than held,
    ///   disarms it — a new press is the user asking to delete their own text,
    /// * a discarded repeat leaves it exactly as it was.
    fn note_key_handled(&self, key_code: usize, repeat: KeyRepeat, actions: &[ClientAction]) {
        let is_backspace = key_code == VK_BACK;
        let ended_composition = is_backspace
            && actions
                .iter()
                .any(|action| matches!(action, ClientAction::EndComposition));

        let armed = match (ended_composition, is_backspace && repeat.is_repeat) {
            (true, _) => true,
            // a held Backspace that is still deleting, or one whose repeat was
            // discarded: neither changes anything
            (false, true) => return,
            (false, false) => false,
        };

        // advisory: a guard that could not be written must never fail the
        // keystroke that was already applied (see CLAUDE.md)
        match self.borrow_mut() {
            Ok(mut text_service) => text_service.discard_backspace_repeat = armed,
            Err(error) => tracing::warn!("could not update the Backspace repeat guard: {error:?}"),
        }
    }

    /// Disarms the guard when Backspace is released — the third way out,
    /// alongside another key and a fresh press. Advisory: a key-up that
    /// cannot be recorded must not fail.
    pub fn note_key_up(&self, wparam: WPARAM) {
        if wparam.0 != VK_BACK {
            return;
        }
        match self.borrow_mut() {
            Ok(mut text_service) => text_service.discard_backspace_repeat = false,
            Err(error) => tracing::warn!("could not clear the Backspace repeat guard: {error:?}"),
        }
    }

    /// The state the pure transition table decides against.
    fn keystroke_context(&self) -> Result<KeystrokeContext> {
        let text_service = self.borrow()?;
        let composition = text_service.borrow_composition()?;
        Ok(KeystrokeContext {
            state: composition.state().clone(),
            mode: text_service.input_mode.clone(),
            reading_chars: composition.raw_hiragana.chars().count(),
            suffix_is_empty: composition.suffix.is_empty(),
        })
    }

    /// Flips あ/A from somewhere other than a raw key event — today that is
    /// `OnPreservedKey`, where TSF delivers the reserved on/off keys.
    ///
    /// Runs the *same* transition the raw VK would, so a composition in
    /// flight is ended exactly the way Zenkaku/Hankaku has always ended it
    /// rather than being abandoned open.
    #[tracing::instrument(skip(self, context))]
    pub fn toggle_input_mode(&self, context: Option<&ITfContext>) -> Result<()> {
        // The preserved-key callback carries the context, and the edit
        // sessions below need it; without one there is nothing to compose in.
        if let Some(context) = context {
            self.borrow_mut()?.context = Some(context.clone());
        }

        let ctx = self.keystroke_context()?;
        let Some((next_state, actions)) = transition(&ctx, UserAction::ToggleInputMode) else {
            return Ok(());
        };

        self.handle_action(&actions, next_state)
    }

    /// Answers OnTestKeyDown. A pure query, as the ITfKeyEventSink contract
    /// requires (issue #26): it decides but never acts, so a host that
    /// probes speculatively — without a following OnKeyDown — cannot
    /// disturb the composition. The actions run in `handle_key` when the
    /// host delivers the key for real.
    #[tracing::instrument(skip(self, context))]
    pub fn test_key(
        &self,
        context: Option<&ITfContext>,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Result<bool> {
        Ok(self.process_key(context, wparam, lparam)?.is_some())
    }

    #[tracing::instrument(skip(self, context))]
    pub fn handle_key(
        &self,
        context: Option<&ITfContext>,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Result<bool> {
        if let Some(context) = context {
            self.borrow_mut()?.context = Some(context.clone());
        } else {
            return Ok(false);
        };

        if let Some((actions, transition)) = self.process_key(context, wparam, lparam)? {
            // Before `?`: the guard tracks what the KEY meant, and a failed
            // edit session does not make a Backspace stop having ended the
            // composition — act_end_composition tears the composition down
            // either way.
            self.note_key_handled(wparam.0, key_repeat(lparam.0), &actions);
            self.handle_action(&actions, transition)?;
            Ok(true)
        } else {
            // Not handled, so it goes to the host — including a fresh
            // Backspace press outside a composition, which is exactly the
            // keystroke the guard must stop discarding.
            self.note_key_handled(wparam.0, key_repeat(lparam.0), &[]);
            Ok(false)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::engine::ipc_service::IpcCall;
    use crate::engine::state::IMEState;
    use crate::engine::test_util::{install_fake_ipc, recorded_calls, scripted};
    use crate::tsf::test_support::{
        EditSessionBehavior, factory_of, factory_with_fake_context, global_state_lock,
    };
    use windows::Win32::UI::TextServices::ITfTextInputProcessor;

    /// A keydown lparam: `count` presses coalesced into one message, and the
    /// previous-key-state bit set when the key is being held.
    fn keydown(count: u32, held: bool) -> LPARAM {
        LPARAM((count | if held { 1 << 30 } else { 0 }) as isize)
    }

    /// Puts the factory into a live composition whose reading is `reading`.
    fn compose(tip: &ITfTextInputProcessor, reading: &str) {
        let factory = factory_of(tip);
        let text_service = factory.borrow().unwrap();
        let mut composition = text_service.borrow_mut_composition().unwrap();
        composition.set_up_for_test(CompositionState::Composing);
        composition.raw_hiragana = reading.to_string();
        composition.raw_input = reading.to_string();
    }

    fn composition_state(tip: &ITfTextInputProcessor) -> CompositionState {
        let factory = factory_of(tip);
        // bound so the borrows outlive the clone rather than being dropped
        // mid-expression
        let text_service = factory.borrow().unwrap();
        let composition = text_service.borrow_composition().unwrap();
        composition.state().clone()
    }

    const VK_BACK_W: WPARAM = WPARAM(0x08);

    /// The point of the whole change: a held Backspace that the host coalesced
    /// into one message costs ONE round trip, not one per press. Each of those
    /// round trips reconverts the entire reading (Zenzai inference included),
    /// which is what made a long press lag behind the key.
    #[test]
    fn a_held_backspace_removes_the_whole_batch_in_one_call() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(scripted(&["みず"], "みず", &[2], &[2]));

        let (tip, context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        compose(&tip, "みずうみあ");

        let factory = factory_of(&tip);
        assert!(
            factory
                .handle_key(Some(&context), VK_BACK_W, keydown(3, true))
                .unwrap(),
            "the composition owns Backspace"
        );

        let removals: Vec<_> = recorded_calls(&fake)
            .into_iter()
            .filter(|call| matches!(call, IpcCall::RemoveText(_)))
            .collect();
        assert_eq!(
            removals,
            vec![IpcCall::RemoveText(3)],
            "three presses must be one call for three kana, not three calls"
        );
        assert_eq!(composition_state(&tip), CompositionState::Composing);

        IMEState::get().unwrap().ipc_service = None;
    }

    /// The damage this guards against: the reading runs out mid-hold, the
    /// composition ends, and the presses that keep arriving would reach the
    /// host and delete the text the user just committed. They are eaten
    /// instead — and eaten silently, with no RPC of any kind.
    #[test]
    fn the_tail_of_a_held_backspace_is_eaten_once_the_composition_ends() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(scripted(&[""], "", &[0], &[0]));

        let (tip, context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        compose(&tip, "あ");

        let factory = factory_of(&tip);
        factory
            .handle_key(Some(&context), VK_BACK_W, keydown(1, true))
            .unwrap();
        assert_eq!(
            composition_state(&tip),
            CompositionState::None,
            "precondition: the last kana ends the composition"
        );

        let before = recorded_calls(&fake).len();
        for _ in 0..3 {
            assert!(
                factory
                    .handle_key(Some(&context), VK_BACK_W, keydown(1, true))
                    .unwrap(),
                "the repeat must be eaten, not handed to the host"
            );
        }
        assert_eq!(
            recorded_calls(&fake).len(),
            before,
            "an eaten repeat must issue no RPC at all"
        );

        IMEState::get().unwrap().ipc_service = None;
    }

    /// The other half of the guard: it must not turn into "Backspace stops
    /// working". Letting go and pressing again is the user asking to delete
    /// their own text, and that press goes to the host untouched — as does
    /// everything after it, because the fresh press disarmed the guard.
    #[test]
    fn a_fresh_backspace_press_reaches_the_host_and_disarms_the_guard() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(scripted(&[""], "", &[0], &[0]));

        let (tip, context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        compose(&tip, "あ");

        let factory = factory_of(&tip);
        factory
            .handle_key(Some(&context), VK_BACK_W, keydown(1, true))
            .unwrap();

        assert!(
            !factory
                .handle_key(Some(&context), VK_BACK_W, keydown(1, false))
                .unwrap(),
            "a fresh press outside a composition belongs to the host"
        );
        assert!(
            !factory
                .handle_key(Some(&context), VK_BACK_W, keydown(1, true))
                .unwrap(),
            "and its own autorepeat must follow it, the guard being disarmed"
        );

        drop(fake);
        IMEState::get().unwrap().ipc_service = None;
    }

    /// Any other key disarms it too. Without this, a guard armed by one
    /// composition would still be discarding Backspace repeats after the user
    /// had typed a whole sentence past it.
    #[test]
    fn another_key_disarms_the_guard() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(scripted(&["あ"], "あ", &[1], &[1]));

        let (tip, context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        compose(&tip, "あ");

        let factory = factory_of(&tip);
        factory
            .handle_key(Some(&context), VK_BACK_W, keydown(1, true))
            .unwrap();
        // the A key: starts a new composition, and disarms on the way
        factory
            .handle_key(Some(&context), WPARAM(0x41), keydown(1, false))
            .unwrap();

        assert!(
            !factory.borrow().unwrap().discard_backspace_repeat,
            "the guard belongs to the Backspace that armed it, not to the session"
        );

        drop(fake);
        IMEState::get().unwrap().ipc_service = None;
    }

    /// Releasing the key is the most direct end of an autorepeat, and the one
    /// that does not depend on the user pressing anything else.
    #[test]
    fn releasing_backspace_disarms_the_guard() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(scripted(&[""], "", &[0], &[0]));

        let (tip, context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        compose(&tip, "あ");

        let factory = factory_of(&tip);
        factory
            .handle_key(Some(&context), VK_BACK_W, keydown(1, true))
            .unwrap();
        assert!(
            factory.borrow().unwrap().discard_backspace_repeat,
            "precondition: ending the composition arms the guard"
        );

        factory.note_key_up(VK_BACK_W);
        assert!(!factory.borrow().unwrap().discard_backspace_repeat);

        drop(fake);
        IMEState::get().unwrap().ipc_service = None;
    }

    /// `OnTestKeyDown` must stay a pure query (issue #26): it answers whether
    /// the repeat would be eaten without arming, disarming, or converting
    /// anything.
    #[test]
    fn testing_a_discarded_repeat_changes_nothing() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(scripted(&[""], "", &[0], &[0]));

        let (tip, context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        compose(&tip, "あ");

        let factory = factory_of(&tip);
        factory
            .handle_key(Some(&context), VK_BACK_W, keydown(1, true))
            .unwrap();

        let before = recorded_calls(&fake).len();
        assert!(
            factory
                .test_key(Some(&context), VK_BACK_W, keydown(1, true))
                .unwrap(),
            "the probe must agree with what OnKeyDown will do"
        );
        assert!(
            factory.borrow().unwrap().discard_backspace_repeat,
            "a probe must not disarm the guard"
        );
        assert_eq!(recorded_calls(&fake).len(), before, "and must not act");

        IMEState::get().unwrap().ipc_service = None;
    }
}
