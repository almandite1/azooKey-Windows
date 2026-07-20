use std::cmp::max;

use crate::{
    engine::user_action::UserAction,
    extension::VKeyExt as _,
    tsf::factory::{TextServiceFactory, TextServiceFactory_Impl},
};

use super::{
    client_action::{ClientAction, SetSelectionType, SetTextType},
    full_width::{to_fullwidth, to_fullwidth_ascii, to_halfwidth},
    input_mode::InputMode,
    ipc_service::Candidates,
    state::IMEState,
    text_util::{to_half_katakana, to_katakana},
    transition::{
        is_modifier_key, shortcut_transition, transition, KeyDisposition, KeystrokeContext,
    },
};
use windows::Win32::{
    Foundation::WPARAM,
    UI::{
        Input::KeyboardAndMouse::{VK_CONTROL, VK_MENU},
        TextServices::{
            ITfComposition, ITfCompositionSink_Impl, ITfContext, TF_CLUIE_COUNT,
            TF_CLUIE_CURRENTPAGE, TF_CLUIE_PAGEINDEX, TF_CLUIE_SELECTION, TF_CLUIE_STRING,
        },
    },
};

use anyhow::{Context, Result};

#[derive(Default, Clone, PartialEq, Debug)]
pub enum CompositionState {
    #[default]
    None,
    Composing,
    Previewing,
}

#[derive(Default, Clone, Debug)]
pub struct Composition {
    pub preview: String, // text to be previewed
    pub suffix: String,  // text to be appended after preview
    pub raw_input: String,
    pub raw_hiragana: String,

    pub corresponding_count: i32, // corresponding count of the preview

    pub selection_index: i32,
    pub candidates: Candidates,

    pub state: CompositionState,
    pub tip_composition: Option<ITfComposition>,
}

/// Mirrors candidate entry `index` into the client-side composition
/// fields. This is the single point where a selected candidate becomes
/// the visible preview — the future hook for conversion-history learning
/// to observe what the user actually picked.
fn apply_selected_candidate(
    candidates: &Candidates,
    index: i32,
    preview: &mut String,
    suffix: &mut String,
    raw_hiragana: &mut String,
    corresponding_count: &mut i32,
) {
    let (text, sub_text, count) = candidates.entry(index as usize);
    *corresponding_count = count;
    *preview = text;
    *suffix = sub_text;
    *raw_hiragana = candidates.hiragana.clone();
}

impl ITfCompositionSink_Impl for TextServiceFactory_Impl {
    #[macros::anyhow]
    fn OnCompositionTerminated(
        &self,
        _ecwrite: u32,
        _pcomposition: Option<&ITfComposition>,
    ) -> Result<()> {
        // if user clicked outside the composition, the composition will be terminated
        tracing::debug!("OnCompositionTerminated");

        let actions = vec![ClientAction::EndComposition];
        self.handle_action(&actions, CompositionState::None)?;

        Ok(())
    }
}

/// Flags for `ui_update` when the whole candidate list was replaced.
const CANDIDATES_CHANGED: u32 = TF_CLUIE_COUNT
    | TF_CLUIE_STRING
    | TF_CLUIE_SELECTION
    | TF_CLUIE_CURRENTPAGE
    | TF_CLUIE_PAGEINDEX;

/// Flags for `ui_update` when only the highlighted candidate moved.
const SELECTION_CHANGED: u32 = TF_CLUIE_SELECTION | TF_CLUIE_CURRENTPAGE;

impl TextServiceFactory {
    /// Hands the candidate list to the host (UILess mode) and, unless the
    /// host said it draws them itself, to our own window.
    ///
    /// Every candidate update goes through here so the two can never
    /// disagree about what is displayed.
    fn publish_candidates(
        &self,
        ipc_service: &mut crate::engine::ipc_service::IPCService,
        candidates: &Candidates,
        selection_index: i32,
        updated_flags: u32,
    ) -> Result<()> {
        self.ui_update(candidates, selection_index, updated_flags)?;

        if self.ui_should_show() {
            if updated_flags & TF_CLUIE_STRING != 0 {
                ipc_service.set_candidates(candidates.texts.clone());
            }
            ipc_service.set_selection(selection_index);
        }

        Ok(())
    }

    /// Impure shell around the pure decision functions
    /// (engine::transition): reads the OS/COM state they need, decodes the
    /// key, and adapts the result. New key bindings belong in the
    /// transition table, not here.
    #[tracing::instrument]
    pub fn process_key(
        &self,
        context: Option<&ITfContext>,
        wparam: WPARAM,
    ) -> Result<Option<(Vec<ClientAction>, CompositionState, KeyDisposition)>> {
        if context.is_none() {
            return Ok(None);
        };

        // The host can switch input off for this context — a password field
        // is the case that matters. Answering None hands the raw key back,
        // which is the whole point: we must not compose here.
        if self.is_input_disabled(context) {
            return Ok(None);
        }

        // a Ctrl or Alt chord is the host's shortcut: cancel any composition
        // so the shortcut actually works (issue #5), and never eat the key.
        // Without the Alt check, Alt+letter fell through to the ToUnicode
        // decoder, which translates it like a WM_SYSCHAR — the TIP ate the
        // host's menu accelerator and turned it into composition input.
        // (AltGr arrives as Ctrl+Alt, so it took this branch already.)
        if VK_CONTROL.is_pressed() || VK_MENU.is_pressed() {
            let state = {
                let text_service = self.borrow()?;
                let state = text_service.borrow_composition()?.state.clone();
                state
            };
            return Ok(shortcut_transition(&state, is_modifier_key(wparam.0))
                .map(|(next_state, actions)| (actions, next_state, KeyDisposition::PassThrough)));
        }

        let ctx = {
            let text_service = self.borrow()?;
            let composition = text_service.borrow_composition()?;
            KeystrokeContext {
                state: composition.state.clone(),
                mode: text_service.input_mode.clone(),
                reading_chars: composition.raw_hiragana.chars().count(),
                suffix_is_empty: composition.suffix.is_empty(),
            }
        };

        let action = UserAction::try_from(wparam.0)?;

        Ok(transition(&ctx, action)
            .map(|(next_state, actions)| (actions, next_state, KeyDisposition::Eat)))
    }

    /// Answers OnTestKeyDown. Pass-through actions (canceling the
    /// composition on a shortcut) must run HERE: once we answer "not
    /// eaten", the host processes the key itself and never calls
    /// OnKeyDown. Canceling twice is harmless (a no-op without a live
    /// composition), so a host that probes speculatively stays safe.
    #[tracing::instrument]
    pub fn test_key(&self, context: Option<&ITfContext>, wparam: WPARAM) -> Result<bool> {
        match self.process_key(context, wparam)? {
            None => Ok(false),
            Some((actions, next_state, KeyDisposition::PassThrough)) => {
                if let Some(context) = context {
                    self.borrow_mut()?.context = Some(context.clone());
                }
                self.handle_action(&actions, next_state)?;
                Ok(false)
            }
            Some((_, _, KeyDisposition::Eat)) => Ok(true),
        }
    }

    #[tracing::instrument]
    pub fn handle_key(&self, context: Option<&ITfContext>, wparam: WPARAM) -> Result<bool> {
        if let Some(context) = context {
            self.borrow_mut()?.context = Some(context.clone());
        } else {
            return Ok(false);
        };

        if let Some((actions, transition, disposition)) = self.process_key(context, wparam)? {
            self.handle_action(&actions, transition)?;
            Ok(disposition == KeyDisposition::Eat)
        } else {
            Ok(false)
        }
    }

    #[tracing::instrument]
    pub fn handle_action(
        &self,
        actions: &[ClientAction],
        transition: CompositionState,
    ) -> Result<()> {
        #[allow(clippy::let_and_return)]
        let (composition, mode) = {
            let text_service = self.borrow()?;
            let composition = text_service.borrow_composition()?.clone();
            let mode = text_service.input_mode.clone();
            (composition, mode)
        };

        let mut preview = composition.preview.clone();
        let mut suffix = composition.suffix.clone();
        let mut raw_input = composition.raw_input.clone();
        let mut raw_hiragana = composition.raw_hiragana.clone();
        let mut corresponding_count = composition.corresponding_count;
        let mut candidates = composition.candidates.clone();
        let mut selection_index = composition.selection_index;
        let mut ipc_service = IMEState::get()?
            .ipc_service
            .clone()
            .context("ipc_service is None")?;
        let mut transition = transition;

        self.update_context(&preview)?;

        // the loop below is wrapped so that the state write-back at the end
        // ALWAYS runs: an early return on a failed action used to skip it,
        // desyncing the client composition from the server (stuck input)
        let mut process = || -> Result<()> {
            for action in actions {
                match action {
                    ClientAction::StartComposition => {
                        self.start_composition()?;
                        self.update_pos()?;
                        // ask the host first -- after update_pos, so it knows
                        // where the caret is before deciding. A host that
                        // draws the candidates itself answers false and our
                        // own window stays hidden.
                        if self.ui_begin()? {
                            ipc_service.show_window();
                        }
                    }
                    ClientAction::EndComposition | ClientAction::CancelComposition => {
                        // ending COMMITS whatever the range holds, so a
                        // cancel must empty it first — Escape used to leave
                        // the leftover reading committed (issue #35 family).
                        //
                        // Every teardown step below must run even when an
                        // edit session fails: the old code bailed at the
                        // first `?`, so a host that rejected the edit session
                        // skipped clear_text and left the server's reading
                        // alive. The next keystroke then appended to that old
                        // reading and the previous composition's text
                        // reappeared. Clear the client state and the server
                        // unconditionally, then surface the failure.
                        let mut edit_result = Ok(());
                        if matches!(action, ClientAction::CancelComposition) {
                            edit_result = self.set_text("", "");
                        }
                        // tear down the TSF composition regardless; even on a
                        // failed session end_composition releases the
                        // client-side handle
                        edit_result = edit_result.and(self.end_composition());

                        selection_index = 0;
                        corresponding_count = 0;
                        preview.clear();
                        suffix.clear();
                        raw_input.clear();
                        raw_hiragana.clear();
                        // unconditional: hiding is safe even if we never
                        // showed, and ui_end is a no-op with no live element
                        let ui_result = self.ui_end();
                        ipc_service.hide_window();
                        ipc_service.set_candidates(vec![]);
                        edit_result = edit_result.and(ui_result);
                        let clear_result = ipc_service.clear_text();

                        // surface the first failure only after both the
                        // client state and the server reading were cleared
                        edit_result?;
                        clear_result?;
                    }
                    ClientAction::AppendText(text) => {
                        raw_input.push_str(text);

                        let text = match mode {
                            InputMode::Kana => to_fullwidth(text, false),
                            InputMode::Latin => text.to_string(),
                        };

                        candidates = ipc_service.append_text(text.clone())?;
                        apply_selected_candidate(
                            &candidates,
                            selection_index,
                            &mut preview,
                            &mut suffix,
                            &mut raw_hiragana,
                            &mut corresponding_count,
                        );

                        self.set_text(&preview, &suffix)?;
                        self.publish_candidates(
                            &mut ipc_service,
                            &candidates,
                            selection_index,
                            CANDIDATES_CHANGED,
                        )?;
                    }
                    ClientAction::RemoveText => {
                        candidates = ipc_service.remove_text()?;
                        // Backspace returns to Composing with a fresh, shorter
                        // candidate list; a selection index carried over from
                        // Previewing would point past the new list (blanking
                        // the preview via the entry() fallback) or at the wrong
                        // candidate. Reset to the top.
                        selection_index = 0;
                        apply_selected_candidate(
                            &candidates,
                            selection_index,
                            &mut preview,
                            &mut suffix,
                            &mut raw_hiragana,
                            &mut corresponding_count,
                        );

                        raw_input = raw_input
                            .chars()
                            .take(corresponding_count as usize)
                            .collect();

                        self.set_text(&preview, &suffix)?;
                        self.publish_candidates(
                            &mut ipc_service,
                            &candidates,
                            selection_index,
                            CANDIDATES_CHANGED,
                        )?;
                    }
                    ClientAction::MoveCursor(_offset) => {
                        // Deliberate no-op for now: the MoveCursor RPC and the
                        // Swift engine's cursor handling are live (kept green by
                        // the move_cursor smoke test in crates/server), but the
                        // client-side wiring is deferred to the predictive-
                        // conversion feature, which needs cursor movement anyway.
                    }
                    ClientAction::SetIMEMode(mode) => {
                        self.start_composition()?;
                        self.update_pos()?;
                        self.end_composition()?;

                        // this arm clears the composition, so any candidate
                        // element the host is holding is now stale.
                        // (Pre-existing gap, left alone here: unlike the
                        // End/CancelComposition arm this one never sends
                        // hide_window, so our own window relies on the next
                        // composition to reposition it.)
                        self.ui_end()?;

                        // publishes the mode to the langbar, the indicator,
                        // and the OS compartments (so the touch keyboard,
                        // IMM32 apps and the shell see it too)
                        self.apply_input_mode(mode.clone(), true)?;

                        selection_index = 0;
                        corresponding_count = 0;
                        preview.clear();
                        suffix.clear();
                        raw_input.clear();
                        raw_hiragana.clear();
                        ipc_service.clear_text()?;
                    }
                    ClientAction::SetSelection(selection) => {
                        let candidates = {
                            let text_service = self.borrow()?;
                            let composition = text_service.borrow_composition()?.clone();

                            composition.candidates.clone()
                        };

                        let texts = candidates.texts.clone();

                        // clamp lower bound first: on an empty list len() - 1 is
                        // -1 and the later `as usize` cast would go out of bounds
                        selection_index = match selection {
                            SetSelectionType::Up => selection_index - 1,
                            SetSelectionType::Down => selection_index + 1,
                        }
                        .clamp(0, max(0, texts.len() as i32 - 1));

                        self.publish_candidates(
                            &mut ipc_service,
                            &candidates,
                            selection_index,
                            SELECTION_CHANGED,
                        )?;
                        apply_selected_candidate(
                            &candidates,
                            selection_index,
                            &mut preview,
                            &mut suffix,
                            &mut raw_hiragana,
                            &mut corresponding_count,
                        );

                        self.set_text(&preview, &suffix)?;
                    }
                    ClientAction::ShrinkText(text) => {
                        // shrink text
                        raw_input.push_str(text);
                        raw_input = raw_input
                            .chars()
                            .skip(corresponding_count as usize)
                            .collect();

                        ipc_service.shrink_text(corresponding_count)?;
                        let text = match mode {
                            InputMode::Kana => to_fullwidth(text, false),
                            InputMode::Latin => text.to_string(),
                        };
                        candidates = ipc_service.append_text(text)?;
                        selection_index = 0;

                        // shift_start needs the preview being replaced
                        let previous_preview = preview.clone();
                        apply_selected_candidate(
                            &candidates,
                            selection_index,
                            &mut preview,
                            &mut suffix,
                            &mut raw_hiragana,
                            &mut corresponding_count,
                        );
                        self.shift_start(&previous_preview, &preview)?;

                        self.publish_candidates(
                            &mut ipc_service,
                            &candidates,
                            selection_index,
                            CANDIDATES_CHANGED,
                        )?;
                        self.update_pos()?;

                        transition = CompositionState::Composing;
                    }
                    ClientAction::SetTextWithType(set_type) => {
                        let text = match set_type {
                            SetTextType::Hiragana => raw_hiragana.clone(),
                            SetTextType::Katakana => to_katakana(&raw_hiragana),
                            SetTextType::HalfKatakana => to_half_katakana(&raw_hiragana),
                            SetTextType::FullLatin => to_fullwidth_ascii(&raw_input),
                            SetTextType::HalfLatin => to_halfwidth(&raw_input),
                        };

                        self.set_text(&text, "")?;

                        // sync the written-back state with what is now on
                        // screen: the whole reading converted, no suffix
                        // left. A stale preview made the next ShrinkText's
                        // shift_start commit only the first
                        // `old_preview.len()` units of the converted text,
                        // and a stale suffix sent Enter down the
                        // pending-suffix path instead of ending
                        preview = text;
                        suffix.clear();
                        // the conversion covers every input element typed so
                        // far, so a following ShrinkText must drop them all
                        corresponding_count = raw_input.chars().count() as i32;
                    }
                }
            }
            Ok(())
        };
        let result = process();

        // write back the state of the last successful action even when a
        // later action failed, keeping the client consistent with the server
        let text_service = self.borrow()?;
        let mut composition = text_service.borrow_mut_composition()?;

        composition.preview = preview.clone();
        composition.state = transition;
        composition.selection_index = selection_index;
        composition.raw_input = raw_input.clone();
        composition.raw_hiragana = raw_hiragana.clone();
        composition.candidates = candidates;
        composition.suffix = suffix.clone();
        composition.corresponding_count = corresponding_count;

        result
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::engine::client_action::SetTextType;
    use crate::engine::ipc_service::IPCService;
    use crate::tsf::test_support::{
        factory_with_fake_context, global_state_lock, EditSessionBehavior, FakeComposition,
    };
    use windows::core::AsImpl as _;

    /// After F6–F10 (SetTextWithType) the written-back composition state must
    /// describe what is actually on screen: the converted reading as the
    /// preview, no pending suffix, and a corresponding_count covering every
    /// input element. The old arm updated only the on-screen range; the stale
    /// preview then made the next ShrinkText's shift_start commit just the
    /// first `old_preview.len()` UTF-16 units of the converted text (e.g.
    /// わたし → F7 →「ワタシ」, next keystroke committed only「ワ」), and a
    /// stale non-empty suffix sent Enter down the "commit candidate, keep
    /// composing" path instead of ending the composition.
    #[test]
    fn set_text_with_type_syncs_the_written_back_state_with_the_screen() {
        let _guard = global_state_lock();
        // handle_action requires a live-looking IPC service; the lazy
        // channels never connect because this arm issues no RPC
        IMEState::get().unwrap().ipc_service = Some(IPCService::new().unwrap());

        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let factory = unsafe { tip.as_impl() };

        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.state = CompositionState::Previewing;
            composition.preview = "私".to_string(); // the selected candidate
            composition.suffix = "の".to_string(); // unconverted remainder
            composition.raw_input = "watashino".to_string();
            composition.raw_hiragana = "わたしの".to_string();
            composition.corresponding_count = 7; // 私 ← "watashi"
        }

        factory
            .handle_action(
                &[ClientAction::SetTextWithType(SetTextType::Katakana)],
                CompositionState::Previewing,
            )
            .unwrap();

        let text_service = factory.borrow().unwrap();
        let composition = text_service.borrow_composition().unwrap();
        assert_eq!(
            composition.preview, "ワタシノ",
            "the preview must be the converted text now shown on screen"
        );
        assert_eq!(
            composition.suffix, "",
            "the conversion consumed the whole reading — Enter must commit \
             and end, not take the pending-suffix path"
        );
        assert_eq!(
            composition.corresponding_count, 9,
            "all typed input elements (watashino) correspond to the shown \
             text, so a following ShrinkText must drop them all"
        );

        IMEState::get().unwrap().ipc_service = None;
    }

    /// End/Cancel teardown must clear the client state AND release the
    /// composition even when the edit session fails partway. A host that
    /// rejects the edit session used to abort the arm at the first `?`,
    /// leaving raw_hiragana (and the server's reading) alive; the next
    /// keystroke then appended to the old reading and the previous
    /// composition's text reappeared.
    #[test]
    fn cancel_clears_client_state_even_when_the_edit_session_fails() {
        let _guard = global_state_lock();
        IMEState::get().unwrap().ipc_service = Some(IPCService::new().unwrap());

        // a host that rejects every edit session: set_text and
        // end_composition both fail
        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::Reject);
        let factory = unsafe { tip.as_impl() };

        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.state = CompositionState::Composing;
            composition.preview = "わたし".to_string();
            composition.raw_input = "watashi".to_string();
            composition.raw_hiragana = "わたし".to_string();
            composition.corresponding_count = 7;
            composition.tip_composition = Some(FakeComposition::new());
        }

        // the arm still surfaces the edit-session error...
        let result =
            factory.handle_action(&[ClientAction::CancelComposition], CompositionState::None);
        assert!(
            result.is_err(),
            "a rejected edit session must still surface as an error"
        );

        // ...but only after the teardown ran
        let text_service = factory.borrow().unwrap();
        let composition = text_service.borrow_composition().unwrap();
        assert_eq!(composition.state, CompositionState::None);
        assert!(
            composition.raw_hiragana.is_empty()
                && composition.raw_input.is_empty()
                && composition.preview.is_empty(),
            "the reading must be cleared even though the edit session failed; \
             a stale reading made the next keystroke resurrect the old text"
        );
        assert!(
            composition.tip_composition.is_none(),
            "the dead composition handle must be released"
        );

        drop(composition);
        drop(text_service);
        IMEState::get().unwrap().ipc_service = None;
    }
}
