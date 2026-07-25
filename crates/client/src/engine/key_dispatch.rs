//! Turning key events into action batches.
//!
//! The impure shell around the pure decision tables in
//! [`super::transition`]: it reads the OS/COM state those tables need,
//! decodes the key, and hands the resulting batch to
//! [`super::actions::handle_action`]. New key bindings belong in the
//! transition table, not here.

use anyhow::Result;
use windows::Win32::{
    Foundation::WPARAM,
    UI::{
        Input::KeyboardAndMouse::{VK_CONTROL, VK_MENU},
        TextServices::{ITfComposition, ITfCompositionSink_Impl, ITfContext},
    },
};

use crate::{
    engine::user_action::{UserAction, is_ime_toggle_key},
    extension::VKeyExt as _,
    tsf::factory::TextServiceFactory_Impl,
};

use super::{
    client_action::ClientAction,
    composition::CompositionState,
    transition::{KeystrokeContext, is_modifier_key, shortcut_transition, transition},
};

impl ITfCompositionSink_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    fn OnCompositionTerminated(
        &self,
        _ecwrite: u32,
        _pcomposition: windows_core::Ref<'_, ITfComposition>,
    ) -> Result<()> {
        // if user clicked outside the composition, the composition will be terminated
        tracing::debug!("OnCompositionTerminated");

        let actions = vec![ClientAction::EndComposition];
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
                text_service.borrow_composition()?.state.clone()
            };
            return Ok(shortcut_transition(&state, is_modifier_key(wparam.0))
                .map(|(next_state, actions)| (actions, next_state)));
        }

        let ctx = self.keystroke_context()?;
        let action = UserAction::try_from(wparam.0)?;

        Ok(transition(&ctx, action).map(|(next_state, actions)| (actions, next_state)))
    }

    /// The state the pure transition table decides against.
    fn keystroke_context(&self) -> Result<KeystrokeContext> {
        let text_service = self.borrow()?;
        let composition = text_service.borrow_composition()?;
        Ok(KeystrokeContext {
            state: composition.state.clone(),
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
    pub fn test_key(&self, context: Option<&ITfContext>, wparam: WPARAM) -> Result<bool> {
        Ok(self.process_key(context, wparam)?.is_some())
    }

    #[tracing::instrument(skip(self, context))]
    pub fn handle_key(&self, context: Option<&ITfContext>, wparam: WPARAM) -> Result<bool> {
        if let Some(context) = context {
            self.borrow_mut()?.context = Some(context.clone());
        } else {
            return Ok(false);
        };

        if let Some((actions, transition)) = self.process_key(context, wparam)? {
            self.handle_action(&actions, transition)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}
