use std::cmp::max;

use crate::{
    engine::user_action::UserAction,
    extension::VKeyExt as _,
    tsf::factory::{TextServiceFactory, TextServiceFactory_Impl},
};

use super::{
    client_action::{ClientAction, SetSelectionType, SetTextType},
    full_width::{to_fullwidth, to_halfwidth},
    input_mode::InputMode,
    ipc_service::Candidates,
    state::IMEState,
    text_util::{to_half_katakana, to_katakana},
    transition::{transition, KeystrokeContext},
};
use windows::Win32::{
    Foundation::WPARAM,
    UI::{
        Input::KeyboardAndMouse::VK_CONTROL,
        TextServices::{ITfComposition, ITfCompositionSink_Impl, ITfContext},
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

impl TextServiceFactory {
    /// Impure shell around the pure transition table
    /// (engine::transition::transition): reads the OS/COM state the table
    /// needs, decodes the key, and adapts the result. New key bindings
    /// belong in the table, not here.
    #[tracing::instrument]
    pub fn process_key(
        &self,
        context: Option<&ITfContext>,
        wparam: WPARAM,
    ) -> Result<Option<(Vec<ClientAction>, CompositionState)>> {
        if context.is_none() {
            return Ok(None);
        };

        // check shortcut keys
        if VK_CONTROL.is_pressed() {
            return Ok(None);
        }

        let ctx = {
            let text_service = self.borrow()?;
            let composition = text_service.borrow_composition()?;
            KeystrokeContext {
                state: composition.state.clone(),
                mode: IMEState::get()?.input_mode.clone(),
                preview_chars: composition.preview.chars().count(),
                suffix_is_empty: composition.suffix.is_empty(),
            }
        };

        let action = UserAction::try_from(wparam.0)?;

        Ok(transition(&ctx, action).map(|(next_state, actions)| (actions, next_state)))
    }

    #[tracing::instrument]
    pub fn handle_key(&self, context: Option<&ITfContext>, wparam: WPARAM) -> Result<bool> {
        if let Some(context) = context {
            self.borrow_mut()?.context = Some(context.clone());
        } else {
            return Ok(false);
        };

        if let Some((actions, transition)) = self.process_key(context, wparam)? {
            self.handle_action(&actions, transition)?;
        } else {
            return Ok(false);
        }

        Ok(true)
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
            let mode = IMEState::get()?.input_mode.clone();
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
                        ipc_service.show_window();
                    }
                    ClientAction::EndComposition => {
                        self.end_composition()?;
                        selection_index = 0;
                        corresponding_count = 0;
                        preview.clear();
                        suffix.clear();
                        raw_input.clear();
                        raw_hiragana.clear();
                        ipc_service.hide_window();
                        ipc_service.set_candidates(vec![]);
                        ipc_service.clear_text()?;
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
                        ipc_service.set_candidates(candidates.texts.clone());
                        ipc_service.set_selection(selection_index);
                    }
                    ClientAction::RemoveText => {
                        candidates = ipc_service.remove_text()?;
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
                        ipc_service.set_candidates(candidates.texts.clone());
                        ipc_service.set_selection(selection_index);
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

                        // scope the guard tightly: update_lang_bar re-enters
                        // IMEState through AddItem -> GetIcon (a try_lock),
                        // which fails outright if we still hold it here
                        {
                            IMEState::get()?.input_mode = mode.clone();
                        }

                        // update the language bar
                        self.update_lang_bar()?;

                        let mode = match mode {
                            InputMode::Latin => "A",
                            InputMode::Kana => "あ",
                        };

                        ipc_service.set_input_mode(mode);

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

                        ipc_service.set_selection(selection_index);
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

                        ipc_service.set_candidates(candidates.texts.clone());
                        ipc_service.set_selection(selection_index);
                        self.update_pos()?;

                        transition = CompositionState::Composing;
                    }
                    ClientAction::SetTextWithType(set_type) => {
                        let text = match set_type {
                            SetTextType::Hiragana => raw_hiragana.clone(),
                            SetTextType::Katakana => to_katakana(&raw_hiragana),
                            SetTextType::HalfKatakana => to_half_katakana(&raw_hiragana),
                            SetTextType::FullLatin => to_fullwidth(&raw_input, true),
                            SetTextType::HalfLatin => to_halfwidth(&raw_input),
                        };

                        self.set_text(&text, "")?;
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
