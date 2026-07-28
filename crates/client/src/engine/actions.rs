//! Running a batch of [`ClientAction`]s against the composition.
//!
//! [`TextServiceFactory_Impl::handle_action`] is the single entry point: it
//! snapshots the live composition into a [`CompositionEdit`], dispatches the
//! batch onto that working copy, and writes it back — *always*, even when an
//! action failed, so a failure cannot leave the client disagreeing with the
//! server about what has been typed.

use std::cmp::max;

use anyhow::{Context, Result};

use crate::tsf::factory::TextServiceFactory_Impl;

use super::{
    candidate_ui::{CANDIDATES_CHANGED, SELECTION_CHANGED},
    client_action::{ClientAction, SetSelectionType, SetTextType},
    composition::{
        CompositionEdit, CompositionState, keystrokes, needs_context_update,
        raw_input_after_commit, raw_input_kept_for_count,
    },
    full_width::{to_fullwidth_ascii, to_halfwidth},
    input_mode::InputMode,
    ipc_service::{IPCService, is_server_unavailable},
    state::IMEState,
    text_util::{to_half_katakana, to_katakana},
};

impl TextServiceFactory_Impl {
    #[tracing::instrument(skip(self))]
    pub fn handle_action(
        &self,
        actions: &[ClientAction],
        transition: CompositionState,
    ) -> Result<()> {
        let (composition, mode) = {
            let text_service = self.borrow()?;
            let composition = text_service.borrow_composition()?.clone();
            (composition, text_service.input_mode.clone())
        };

        // The batch's INTENT, kept immutable and out of `edit`. `edit.state`
        // starts as this same value but is the OUTCOME: an arm may override
        // it (act_shrink_text forces Composing). The recovery below has to
        // ask about the intent — "was this batch tearing the composition
        // down?" — after the arms have already rewritten the outcome.
        let batch_intent = transition.clone();
        let mut edit = CompositionEdit::from_composition(&composition, transition);
        // The one call site that cannot degrade: without a service there is
        // no engine to convert with and no window to show, so a batch would
        // silently do nothing. Every other caller of `ipc()` skips instead.
        let ipc_service = IMEState::ipc()?.context("ipc_service is None")?;

        // preview AND suffix: the caret sits after both, so a window that
        // only steps back over the preview feeds the suffix to the engine as
        // if it were text the user had already committed
        if needs_context_update(actions) {
            self.update_context(&format!("{}{}", edit.preview, edit.suffix));
        }

        // Deliberately captured, not `?`: the write-back below must ALWAYS
        // run. An early return on a failed action used to skip it, desyncing
        // the client composition from the server (stuck input).
        let result = self.dispatch_batch(actions, &mut edit, &ipc_service, &mode);

        // If the batch failed because the conversion server became
        // unreachable (crash/restart/hang), the client composition can no
        // longer be trusted to mirror the server: the server's per-connection
        // reading is gone or half-applied, but the client still holds the old
        // preview/reading. Continuing would append the next keystroke onto a
        // reading the fresh server never had (raw_input and the server state
        // silently diverge).
        //
        // Recover in two steps, cheapest and least destructive first (#35):
        // rebuild the server's reading from the composition this batch started
        // with, and only if that also fails throw the composition away (#33).
        // Either way swallow this keystroke's error — we handled it, so the
        // host must not also process the key.
        let recovered = matches!(&result, Err(err) if is_server_unavailable(err));
        if recovered
            && !self.rebuild_server_composition(
                &mut edit,
                &ipc_service,
                &composition,
                &batch_intent,
                &mode,
            )
        {
            self.reset_composition_after_server_loss(&mut edit, &ipc_service);
        }

        // write back the state of the last successful action even when a
        // later action failed, keeping the client consistent with the server
        let text_service = self.borrow()?;
        let mut composition = text_service.borrow_mut_composition()?;
        edit.commit(&mut composition);
        drop(composition);
        drop(text_service);

        if recovered { Ok(()) } else { result }
    }

    /// Runs the batch onto the working copy, stopping at the first failure.
    ///
    /// A separate function rather than a `?`-using block inside
    /// `handle_action` so the "write back whatever the batch reached" step
    /// cannot be skipped by an early return: there is no `?` in
    /// `handle_action` between this call and the write-back.
    fn dispatch_batch(
        &self,
        actions: &[ClientAction],
        edit: &mut CompositionEdit,
        ipc_service: &IPCService,
        mode: &InputMode,
    ) -> Result<()> {
        for action in actions {
            match action {
                ClientAction::StartComposition => self.act_start_composition(ipc_service)?,
                ClientAction::EndComposition => {
                    self.act_end_composition(edit, ipc_service, false)?
                }
                ClientAction::CancelComposition => {
                    self.act_end_composition(edit, ipc_service, true)?
                }
                ClientAction::AppendText(text) => {
                    self.act_append_text(edit, ipc_service, mode, text)?
                }
                ClientAction::RemoveText(count) => {
                    self.act_remove_text(edit, ipc_service, *count)?
                }
                // deliberate no-op; see ClientAction::MoveCursor
                ClientAction::MoveCursor(_offset) => {}
                ClientAction::SetIMEMode(mode) => self.act_set_ime_mode(edit, ipc_service, mode)?,
                ClientAction::SetSelection(selection) => {
                    self.act_set_selection(edit, ipc_service, selection)?
                }
                ClientAction::ShrinkText(text) => {
                    self.act_shrink_text(edit, ipc_service, mode, text)?
                }
                ClientAction::SetTextWithType(set_type) => {
                    self.act_set_text_with_type(edit, set_type)?
                }
            }
        }
        Ok(())
    }

    /// Begins a TSF composition and opens the candidate UI. `open_candidate_ui`
    /// runs after `update_pos` so a UILess host knows where the caret is
    /// before deciding whether it draws the candidates itself.
    fn act_start_composition(&self, ipc_service: &IPCService) -> Result<()> {
        self.start_composition()?;
        self.update_pos();
        self.open_candidate_ui(ipc_service);
        Ok(())
    }

    /// Ends the composition (committing the range) or, when `cancel`, empties
    /// it first — Escape used to leave the leftover reading committed (issue
    /// #35 family).
    ///
    /// Every teardown step runs even when an edit session fails: the old code
    /// bailed at the first `?`, so a host that rejected the edit session
    /// skipped `clear_text` and left the server's reading alive; the next
    /// keystroke appended to it and the previous composition's text
    /// reappeared. Clear the client state and the server unconditionally,
    /// then surface the first failure.
    fn act_end_composition(
        &self,
        edit: &mut CompositionEdit,
        ipc_service: &IPCService,
        cancel: bool,
    ) -> Result<()> {
        let mut edit_result = Ok(());
        if cancel {
            edit_result = self.set_text("", "");
        }
        // tear down the TSF composition regardless; even on a failed session
        // end_composition releases the client-side handle
        edit_result = edit_result.and(self.end_composition());

        edit.reset_for_teardown();
        self.close_candidate_ui(ipc_service);
        let clear_result = ipc_service.clear_text();

        // surface the first failure only after both the client state and the
        // server reading were cleared
        edit_result?;
        clear_result?;
        Ok(())
    }

    /// Feeds a keystroke to the engine and shows the fresh candidate list.
    fn act_append_text(
        &self,
        edit: &mut CompositionEdit,
        ipc_service: &IPCService,
        mode: &InputMode,
        text: &str,
    ) -> Result<()> {
        let candidates = ipc_service.append_text(keystrokes(mode, text))?;

        // Only now, with the engine's answer in hand. raw_input is the
        // keystrokes the SERVER has consumed, and the write-back runs even
        // when an action fails, so recording the keystroke before the call
        // meant a failed call still spent it: F9/F10 would then render input
        // the engine never saw (#85).
        edit.raw_input.push_str(text);
        // The engine returned a fresh list; an index carried over from
        // Previewing (the arrow keys transition to Composing but map to the
        // MoveCursor no-op, which keeps the index) would adopt the wrong
        // candidate or blank the preview via the entry() fallback.
        edit.adopt_fresh(candidates);

        self.render_and_publish_full(edit, ipc_service)
    }

    /// Backspace: the engine returns a fresh, shorter list. A selection index
    /// carried over from Previewing would point past the new list (blanking
    /// the preview via the `entry()` fallback) or at the wrong candidate, so
    /// reset to the top and shrink `raw_input` to what the new top covers.
    ///
    /// `count` kana go in one call. Nothing below it needs to know: the
    /// truncation is driven by the new top candidate's `corresponding_count`,
    /// which the engine reports for the reading that is left — however many
    /// kana it took to get there.
    fn act_remove_text(
        &self,
        edit: &mut CompositionEdit,
        ipc_service: &IPCService,
        count: u32,
    ) -> Result<()> {
        let candidates = ipc_service.remove_text(count)?;
        edit.adopt_fresh(candidates);

        edit.raw_input = raw_input_kept_for_count(&edit.raw_input, edit.corresponding_count);

        self.render_and_publish_full(edit, ipc_service)
    }

    /// Switches the IME mode. Clears the composition and hides the candidate
    /// window with it (issue #21). The clearing is unconditional: a failure
    /// publishing the mode used to abort the arm before it, leaving the
    /// reading alive on both sides.
    fn act_set_ime_mode(
        &self,
        edit: &mut CompositionEdit,
        ipc_service: &IPCService,
        mode: &InputMode,
    ) -> Result<()> {
        // Stamped here, where the mode actually changes, so every path that
        // toggles feeds the double-delivery guard — the raw VK, the
        // preserved key, and the langbar alike (issue #19).
        if let Err(error) = self.note_mode_toggle() {
            tracing::warn!("could not record the mode toggle time: {error:?}");
        }

        self.start_composition()?;
        self.update_pos();
        self.end_composition()?;

        self.close_candidate_ui(ipc_service);

        // publishes the mode to the langbar, the indicator, and the OS
        // compartments (so the touch keyboard, IMM32 apps and the shell see
        // it too). Captured rather than `?`: a langbar or compartment failure
        // must not skip the teardown below, or the write-back keeps the stale
        // preview/reading and the next keystroke resurrects the old reading
        // (same pattern as act_end_composition).
        let apply_result = self.apply_input_mode(mode.clone(), true);

        edit.reset_for_teardown();
        let clear_result = ipc_service.clear_text();

        // surface the first failure only after both the client state and the
        // server reading were cleared
        apply_result?;
        clear_result?;
        Ok(())
    }

    /// Moves the highlighted candidate up or down, clamped to the list.
    fn act_set_selection(
        &self,
        edit: &mut CompositionEdit,
        ipc_service: &IPCService,
        selection: &SetSelectionType,
    ) -> Result<()> {
        // The WORKING COPY's list, not the live composition's. They are the
        // same today, because the transition table only ever emits
        // SetSelection on its own — but the moment a batch pairs it with an
        // action that fetches candidates (predictive conversion is the
        // obvious one), the live composition still holds the previous list:
        // the write-back does not happen until the batch ends. Reading it
        // here would silently highlight an entry of a list that is already
        // gone.
        let candidates = edit.candidates.clone();

        // clamp lower bound first: on an empty list len() - 1 is -1 and the
        // later `as usize` cast would go out of bounds
        edit.selection_index = match selection {
            SetSelectionType::Up => edit.selection_index - 1,
            SetSelectionType::Down => edit.selection_index + 1,
        }
        .clamp(0, max(0, candidates.texts.len() as i32 - 1));

        self.publish_candidates(
            ipc_service,
            &candidates,
            edit.selection_index,
            SELECTION_CHANGED,
        );
        edit.adopt_candidate(&candidates, edit.selection_index);

        self.set_text(&edit.preview, &edit.suffix)
    }

    /// Confirms the selected candidate and keeps composing the remainder:
    /// commits it with `shift_start`, drops the committed input elements, and
    /// forces the state back to Composing for the fresh reading.
    fn act_shrink_text(
        &self,
        edit: &mut CompositionEdit,
        ipc_service: &IPCService,
        mode: &InputMode,
        text: &str,
    ) -> Result<()> {
        // Both values are computed up front because they read
        // `corresponding_count`, which `adopt_fresh` below overwrites — but
        // each is APPLIED only once the call it describes has landed. The
        // write-back runs even when an action fails, so assigning either one
        // early spent keystrokes on a call that might not happen (#85).
        //
        // Two of them because this arm makes two calls and can stop between
        // them: after the commit the server has dropped the prefix but has
        // not seen the new keystroke yet.
        let spent = edit.corresponding_count;
        let after_commit = raw_input_after_commit(&edit.raw_input, "", spent);
        let after_append = raw_input_after_commit(&edit.raw_input, text, spent);

        // kana, not keystrokes: only the reading can express a candidate
        // that ends inside a romaji cluster
        ipc_service.shrink_text(edit.surface_count)?;
        edit.raw_input = after_commit;

        let candidates = ipc_service.append_text(keystrokes(mode, text))?;
        edit.raw_input = after_append;

        // shift_start needs the preview being replaced, captured before
        // adopt_fresh overwrites it (it does not read edit.candidates, so
        // storing the new list first is harmless)
        let previous_preview = edit.preview.clone();
        edit.adopt_fresh(candidates);
        self.shift_start(&previous_preview, &edit.preview)?;

        self.publish_candidates(
            ipc_service,
            &edit.candidates,
            edit.selection_index,
            CANDIDATES_CHANGED,
        );
        self.update_pos();

        edit.state = CompositionState::Composing;
        Ok(())
    }

    /// F6–F10: rewrites the whole reading as hiragana/katakana/half-katakana/
    /// full- or half-width latin, then syncs the written-back state with what
    /// is now on screen — the whole reading converted, no suffix left. A stale
    /// preview made the next ShrinkText's shift_start commit only the first
    /// `old_preview.len()` units, and a stale suffix sent Enter down the
    /// pending-suffix path instead of ending.
    fn act_set_text_with_type(
        &self,
        edit: &mut CompositionEdit,
        set_type: &SetTextType,
    ) -> Result<()> {
        let text = match set_type {
            SetTextType::Hiragana => edit.raw_hiragana.clone(),
            SetTextType::Katakana => to_katakana(&edit.raw_hiragana),
            SetTextType::HalfKatakana => to_half_katakana(&edit.raw_hiragana),
            SetTextType::FullLatin => to_fullwidth_ascii(&edit.raw_input),
            SetTextType::HalfLatin => to_halfwidth(&edit.raw_input),
        };

        self.set_text(&text, "")?;

        edit.preview = text;
        edit.suffix.clear();
        // the conversion covers everything typed so far, so a following
        // ShrinkText must drop it all — each count in its own unit
        edit.corresponding_count = edit.raw_input.chars().count() as i32;
        edit.surface_count = edit.raw_hiragana.chars().count() as i32;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::engine::ipc_service::{Candidates, FakeIpc, IpcCall};
    use crate::engine::test_util::{install_fake_ipc, recorded_calls, scripted};
    use crate::tsf::test_support::{
        EditSessionBehavior, FakeContext, RangeLog, factory_of, factory_with_context,
        factory_with_fake_context, global_state_lock,
    };
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    /// AppendText is the main typing path: the engine's answer must become
    /// the written-back preview state, and the candidate list must be
    /// published to the window (full CANDIDATES_CHANGED: list AND selection).
    #[test]
    fn append_text_applies_the_engine_answer_and_publishes_the_list() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(scripted(&["水", "未"], "みず", &[4, 4], &[2, 2]));

        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let factory = factory_of(&tip);
        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.set_up_for_test(CompositionState::Composing);
        }

        factory
            .handle_action(
                &[ClientAction::AppendText("mi".to_string())],
                CompositionState::Composing,
            )
            .unwrap();

        let text_service = factory.borrow().unwrap();
        let composition = text_service.borrow_composition().unwrap();
        assert_eq!(composition.preview, "水");
        assert_eq!(composition.raw_input, "mi");
        assert_eq!(composition.raw_hiragana, "みず");
        assert_eq!(composition.corresponding_count, 4);
        assert_eq!(composition.candidates.texts, vec!["水", "未"]);
        drop(composition);
        drop(text_service);

        let calls = recorded_calls(&fake);
        assert!(calls.contains(&IpcCall::AppendText("mi".to_string())));
        assert!(
            calls.contains(&IpcCall::SetCandidates(vec![
                "水".to_string(),
                "未".to_string()
            ])),
            "a CANDIDATES_CHANGED update must push the new list to the window: {calls:?}"
        );
        assert!(calls.contains(&IpcCall::SetSelection(0)));

        IMEState::get().unwrap().ipc_service = None;
    }

    /// The arrow keys leave Previewing for Composing but map to the
    /// MoveCursor no-op, so a non-zero selection index survives into the
    /// next keystroke. AppendText must not apply it to the fresh list: it
    /// would adopt the wrong candidate, or blank the preview when the index
    /// points past the shorter list.
    #[test]
    fn append_text_resets_a_selection_carried_over_from_previewing() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(scripted(&["水", "未"], "みず", &[4, 4], &[2, 2]));

        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let factory = factory_of(&tip);
        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.set_up_for_test(CompositionState::Previewing);
            composition.selection_index = 3;
            composition.preview = "水".to_string();
        }

        // the left arrow: Previewing + MoveCursor transitions to Composing,
        // and the no-op arm leaves the selection index behind
        factory
            .handle_action(&[ClientAction::MoveCursor(-1)], CompositionState::Composing)
            .unwrap();
        {
            let text_service = factory.borrow().unwrap();
            let composition = text_service.borrow_composition().unwrap();
            assert_eq!(
                composition.selection_index, 3,
                "precondition: the no-op arm must leave the stale index in Composing"
            );
        }

        factory
            .handle_action(
                &[ClientAction::AppendText("mi".to_string())],
                CompositionState::Composing,
            )
            .unwrap();

        let text_service = factory.borrow().unwrap();
        let composition = text_service.borrow_composition().unwrap();
        assert_eq!(
            composition.selection_index, 0,
            "a stale Previewing selection must not survive the next keystroke"
        );
        assert_eq!(
            composition.preview, "水",
            "the top of the fresh list must be adopted, not the stale index"
        );
        drop(composition);
        drop(text_service);

        assert!(
            recorded_calls(&fake).contains(&IpcCall::SetSelection(0)),
            "the window must be told the selection went back to the top"
        );

        IMEState::get().unwrap().ipc_service = None;
    }

    /// Backspace returns to Composing with a fresh, shorter list: a
    /// selection index carried over from Previewing pointed past the new
    /// list (or at the wrong candidate), so it must reset to the top, and
    /// raw_input must shrink to what the new top candidate covers.
    #[test]
    fn remove_text_resets_the_selection_and_truncates_raw_input() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(scripted(&["水"], "みず", &[4], &[2]));

        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let factory = factory_of(&tip);
        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.set_up_for_test(CompositionState::Previewing);
            composition.selection_index = 3;
            composition.raw_input = "mizuu".to_string();
        }

        factory
            .handle_action(&[ClientAction::RemoveText(1)], CompositionState::Composing)
            .unwrap();

        let text_service = factory.borrow().unwrap();
        let composition = text_service.borrow_composition().unwrap();
        assert_eq!(
            composition.selection_index, 0,
            "a stale Previewing selection must not survive Backspace"
        );
        assert_eq!(
            composition.raw_input, "mizu",
            "raw_input must shrink to the new top candidate's corresponding count"
        );
        drop(composition);
        drop(text_service);

        assert!(recorded_calls(&fake).contains(&IpcCall::RemoveText(1)));
        IMEState::get().unwrap().ipc_service = None;
    }

    /// Confirm-and-continue: ShrinkText commits the selected candidate
    /// (shift_start with the OLD preview), drops the committed input
    /// elements from raw_input, and forces the state back to Composing
    /// regardless of what the caller passed.
    #[test]
    fn shrink_text_commits_and_returns_to_composing() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(scripted(&["ん"], "ん", &[1], &[1]));

        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let factory = factory_of(&tip);
        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.set_up_for_test(CompositionState::Previewing);
            composition.preview = "水".to_string();
            composition.raw_input = "mizu".to_string();
            composition.corresponding_count = 4;
            composition.surface_count = 2; // みず — two kana, four keystrokes
        }

        factory
            .handle_action(
                &[ClientAction::ShrinkText("n".to_string())],
                CompositionState::Previewing,
            )
            .unwrap();

        let text_service = factory.borrow().unwrap();
        let composition = text_service.borrow_composition().unwrap();
        assert_eq!(
            *composition.state(),
            CompositionState::Composing,
            "the arm must force Composing for the fresh reading"
        );
        assert_eq!(
            composition.raw_input, "n",
            "the committed input elements must be dropped"
        );
        assert_eq!(composition.selection_index, 0);
        drop(composition);
        drop(text_service);

        let calls = recorded_calls(&fake);
        let shrink = calls
            .iter()
            .position(|c| *c == IpcCall::ShrinkText(2))
            .expect("shrink_text must be sent the committed KANA count, not the keystrokes");
        let append = calls
            .iter()
            .position(|c| *c == IpcCall::AppendText("n".to_string()))
            .expect("the new keystroke must be appended");
        assert!(
            shrink < append,
            "the server must commit BEFORE the new reading starts: {calls:?}"
        );

        IMEState::get().unwrap().ipc_service = None;
    }

    /// Candidate navigation clamps at both ends (an empty list included —
    /// the lower clamp guards the later `as usize` cast) and publishes a
    /// SELECTION_CHANGED update: selection only, never the list itself.
    #[test]
    fn set_selection_clamps_and_publishes_only_the_selection() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(Candidates::default());

        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let factory = factory_of(&tip);
        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.set_up_for_test(CompositionState::Previewing);
            composition.candidates = scripted(&["a", "b", "c"], "あ", &[1, 1, 1], &[1, 1, 1]);
            composition.selection_index = 2;
        }

        // Down at the last candidate must stay clamped there
        factory
            .handle_action(
                &[ClientAction::SetSelection(SetSelectionType::Down)],
                CompositionState::Previewing,
            )
            .unwrap();
        {
            let text_service = factory.borrow().unwrap();
            let composition = text_service.borrow_composition().unwrap();
            assert_eq!(composition.selection_index, 2, "Down must clamp at len-1");
        }

        // Up three times from index 2 must clamp at 0, not go negative
        for _ in 0..3 {
            factory
                .handle_action(
                    &[ClientAction::SetSelection(SetSelectionType::Up)],
                    CompositionState::Previewing,
                )
                .unwrap();
        }
        {
            let text_service = factory.borrow().unwrap();
            let composition = text_service.borrow_composition().unwrap();
            assert_eq!(composition.selection_index, 0, "Up must clamp at 0");
        }

        let calls = recorded_calls(&fake);
        assert!(calls.contains(&IpcCall::SetSelection(2)));
        assert!(
            !calls.iter().any(|c| matches!(c, IpcCall::SetCandidates(_))),
            "SELECTION_CHANGED must not re-send the candidate list: {calls:?}"
        );

        // an empty list must not panic and must stay at 0
        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.candidates = Candidates::default();
            composition.selection_index = 0;
        }
        factory
            .handle_action(
                &[ClientAction::SetSelection(SetSelectionType::Down)],
                CompositionState::Previewing,
            )
            .unwrap();
        {
            let text_service = factory.borrow().unwrap();
            let composition = text_service.borrow_composition().unwrap();
            assert_eq!(composition.selection_index, 0);
        }

        IMEState::get().unwrap().ipc_service = None;
    }

    /// Ending a composition must tear the candidate UI down over IPC:
    /// hide the window, blank the stale list, and clear the server reading.
    #[test]
    fn end_composition_tears_down_the_candidate_ui() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(Candidates::default());

        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let factory = factory_of(&tip);
        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.set_up_for_test(CompositionState::Composing);
            composition.preview = "水".to_string();
            composition.candidates = scripted(&["水", "未"], "みず", &[4, 4], &[2, 2]);
        }

        factory
            .handle_action(&[ClientAction::EndComposition], CompositionState::None)
            .unwrap();

        let calls = recorded_calls(&fake);
        assert!(calls.contains(&IpcCall::HideWindow), "{calls:?}");
        assert!(
            calls.contains(&IpcCall::SetCandidates(vec![])),
            "the stale list must be blanked: {calls:?}"
        );
        assert!(
            calls.contains(&IpcCall::ClearText),
            "the server reading must be cleared: {calls:?}"
        );

        // ...and the written-back composition must not keep the list either.
        // Two of the three teardown paths used to, leaving a `None`-state
        // composition holding candidates that describe a composition that is
        // over — an inconsistency nothing read yet, which is what let it
        // survive.
        let text_service = factory.borrow().unwrap();
        let composition = text_service.borrow_composition().unwrap();
        assert_eq!(*composition.state(), CompositionState::None);
        assert!(
            composition.candidates.texts.is_empty(),
            "a finished composition must not keep its candidate list: {:?}",
            composition.candidates.texts
        );
        drop(composition);
        drop(text_service);

        IMEState::get().unwrap().ipc_service = None;
    }

    /// The same for the mode switch, which is the other path that used to
    /// leave the list behind (the server-loss reset already dropped it).
    #[test]
    fn a_mode_switch_does_not_keep_the_candidate_list() {
        let _guard = global_state_lock();
        let _fake = install_fake_ipc(Candidates::default());

        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let factory = factory_of(&tip);
        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.set_up_for_test(CompositionState::Composing);
            composition.preview = "わたし".to_string();
            composition.candidates = scripted(&["私", "渡し"], "わたし", &[7, 7], &[3, 3]);
        }

        // apply_input_mode fails in this fake environment (no thread
        // manager); the teardown before it is what is under test
        let _ = factory.handle_action(
            &[ClientAction::SetIMEMode(InputMode::Kana)],
            CompositionState::None,
        );

        let text_service = factory.borrow().unwrap();
        let composition = text_service.borrow_composition().unwrap();
        assert!(
            composition.candidates.texts.is_empty(),
            "a mode switch ends the composition, so its list is dead too: {:?}",
            composition.candidates.texts
        );
        drop(composition);
        drop(text_service);

        IMEState::get().unwrap().ipc_service = None;
    }

    /// Switching the IME mode clears the composition, so the candidate
    /// window must hide with it. This arm used to call ui_end alone and
    /// skip hide_window (issue #21), leaving our window floating over a
    /// composition that no longer existed.
    ///
    /// The teardown runs before apply_input_mode, whose langbar update
    /// needs a thread manager this fake environment does not provide — the
    /// arm therefore errors afterwards, which is exactly why the teardown
    /// must already have happened by then.
    #[test]
    fn set_ime_mode_hides_the_candidate_window() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(Candidates::default());

        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let factory = factory_of(&tip);
        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.set_up_for_test(CompositionState::Composing);
            composition.preview = "わたし".to_string();
        }

        let _ = factory.handle_action(
            &[ClientAction::SetIMEMode(InputMode::Kana)],
            CompositionState::None,
        );

        let calls = recorded_calls(&fake);
        assert!(
            calls.contains(&IpcCall::HideWindow),
            "a mode switch must hide the candidate window (issue #21): {calls:?}"
        );
        assert!(
            calls.contains(&IpcCall::SetCandidates(vec![])),
            "the stale list must be blanked on a mode switch: {calls:?}"
        );

        IMEState::get().unwrap().ipc_service = None;
    }

    /// A mode switch can arrive mid-composition straight from the language
    /// bar, which never sends EndComposition first. Publishing the mode is
    /// fallible (langbar item swap, compartments, indicator placement), and
    /// bailing there used to skip the clearing below: the write-back then
    /// restored the old preview and reading, and the next keystroke appended
    /// to a reading the user thought was gone.
    #[test]
    fn set_ime_mode_clears_the_reading_even_when_apply_input_mode_fails() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(Candidates::default());

        // the fake host has no thread manager, so apply_input_mode fails
        // on its own — no extra hook needed
        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let factory = factory_of(&tip);
        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.set_up_for_test(CompositionState::Composing);
            composition.selection_index = 2;
            composition.corresponding_count = 7;
            composition.preview = "わたし".to_string();
            composition.suffix = "は".to_string();
            composition.raw_input = "watashiha".to_string();
            composition.raw_hiragana = "わたしは".to_string();
        }

        let result = factory.handle_action(
            &[ClientAction::SetIMEMode(InputMode::Kana)],
            CompositionState::None,
        );
        assert!(
            result.is_err(),
            "the failure must still be surfaced, just not before the teardown"
        );

        let text_service = factory.borrow().unwrap();
        let composition = text_service.borrow_composition().unwrap();
        assert_eq!(composition.preview, "");
        assert_eq!(composition.suffix, "");
        assert_eq!(composition.raw_input, "");
        assert_eq!(composition.raw_hiragana, "");
        assert_eq!(composition.selection_index, 0);
        assert_eq!(composition.corresponding_count, 0);
        drop(composition);
        drop(text_service);

        assert!(
            recorded_calls(&fake).contains(&IpcCall::ClearText),
            "the server's reading must be dropped too, or it comes back on the next key"
        );

        IMEState::get().unwrap().ipc_service = None;
    }

    /// The context sent to the engine is the text before the composition,
    /// and the caret sits after the preview AND the suffix. Passing only the
    /// preview left the suffix inside the window, so the engine saw the
    /// user's own half-typed reading as committed context.
    #[test]
    fn the_context_window_steps_back_over_the_suffix_too() {
        let _guard = global_state_lock();
        let _fake = install_fake_ipc(Candidates::default());

        let log = Rc::new(RangeLog::default());
        *log.text.borrow_mut() = "こんにちは".encode_utf16().collect();
        let context = FakeContext::with_ranges(EditSessionBehavior::RunSync, log.clone());
        let tip = factory_with_context(context.clone());
        let factory = factory_of(&tip);
        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.set_up_for_test(CompositionState::Composing);
            composition.preview = "水".to_string();
            composition.suffix = "うみ".to_string();
        }

        // A converting batch, not an empty one: only those measure the
        // context now (issue #36). update_context still runs before the
        // actions, so the FIRST range request is the one under test — the
        // rendering that follows makes requests of its own.
        factory
            .handle_action(&[ClientAction::RemoveText(1)], CompositionState::Composing)
            .unwrap();

        assert_eq!(
            log.shift_end_reqs.borrow().first(),
            Some(&-3),
            "preview + suffix is 3 UTF-16 units; the window must end before all of it"
        );

        IMEState::get().unwrap().ipc_service = None;
    }

    /// Starting a composition asks the host first (UILess); a host without
    /// a UI element manager wants our own window shown.
    #[test]
    fn start_composition_shows_our_window_without_a_uiless_host() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(Candidates::default());

        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let factory = factory_of(&tip);

        factory
            .handle_action(
                &[ClientAction::StartComposition],
                CompositionState::Composing,
            )
            .unwrap();

        assert!(
            recorded_calls(&fake).contains(&IpcCall::ShowWindow),
            "no UILess host answered, so our own window must be shown"
        );

        IMEState::get().unwrap().ipc_service = None;
    }

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
        let factory = factory_of(&tip);

        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.set_up_for_test(CompositionState::Previewing);
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

    /// Only the batches whose engine call can USE the surrounding text pay
    /// for measuring it (issue #36).
    ///
    /// `update_context` opens an edit session on the parent context and reads
    /// the document back — per batch, on the UI thread, in front of the
    /// keystroke. Candidate navigation and the MoveCursor no-op issue no
    /// conversion at all, so the context they measured was thrown away; worse,
    /// with the engine down each one also paid the SetContext deadline.
    #[test]
    fn only_converting_batches_measure_the_surrounding_text() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(scripted(&["水", "見ず"], "みず", &[4, 4], &[2, 2]));

        let log = Rc::new(RangeLog::default());
        *log.text.borrow_mut() = "こんにちは".encode_utf16().collect();
        let context = FakeContext::with_ranges(EditSessionBehavior::RunSync, log.clone());
        let tip = factory_with_context(context.clone());
        let factory = factory_of(&tip);
        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.set_up_for_test(CompositionState::Previewing);
            composition.preview = "水".to_string();
            composition.candidates = scripted(&["水", "見ず"], "みず", &[4, 4], &[2, 2]);
        }

        let context_calls = |fake: &Arc<Mutex<FakeIpc>>| {
            recorded_calls(fake)
                .iter()
                .filter(|c| matches!(c, IpcCall::SetContext(_)))
                .count()
        };

        // moving the highlight converts nothing
        factory
            .handle_action(
                &[ClientAction::SetSelection(SetSelectionType::Down)],
                CompositionState::Previewing,
            )
            .unwrap();
        assert_eq!(
            context_calls(&fake),
            0,
            "candidate navigation must not read the document back"
        );

        // ...nor does the arrow-key no-op
        factory
            .handle_action(&[ClientAction::MoveCursor(-1)], CompositionState::Composing)
            .unwrap();
        assert_eq!(
            context_calls(&fake),
            0,
            "the MoveCursor no-op must not either"
        );

        // typing does: this is the batch whose engine call reads the context
        factory
            .handle_action(
                &[ClientAction::AppendText("mi".to_string())],
                CompositionState::Composing,
            )
            .unwrap();
        assert_eq!(
            context_calls(&fake),
            1,
            "a conversion must be given the text it is converting after"
        );

        IMEState::get().unwrap().ipc_service = None;
    }

    /// A keystroke the engine REJECTED must not be recorded as typed (#85).
    ///
    /// `raw_input` is the keystrokes the server has consumed, and the
    /// write-back runs even when an action fails — deliberately, so a failure
    /// cannot wedge input. Recording the keystroke before the call therefore
    /// made a failed call spend it anyway, and F9/F10 (which render
    /// `raw_input` directly) would then show input the engine never saw.
    ///
    /// The failure here is a plain engine error, NOT `ServerUnavailable`: that
    /// one is already covered by the rebuild, which restores the whole
    /// composition from the batch-start snapshot.
    #[test]
    fn a_rejected_keystroke_is_not_recorded_as_typed() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(scripted(&["水"], "みず", &[4], &[2]));
        fake.lock().unwrap().engine_fails = true;

        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let factory = factory_of(&tip);
        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.set_up_for_test(CompositionState::Composing);
            composition.raw_input = "mizu".to_string();
            composition.raw_hiragana = "みず".to_string();
        }

        let result = factory.handle_action(
            &[ClientAction::AppendText("ka".to_string())],
            CompositionState::Composing,
        );
        assert!(result.is_err(), "an engine error must still be surfaced");

        let text_service = factory.borrow().unwrap();
        let composition = text_service.borrow_composition().unwrap();
        assert_eq!(
            composition.raw_input, "mizu",
            "the engine rejected the keystroke, so it was never typed as far \
             as the composition is concerned"
        );
        drop(composition);
        drop(text_service);

        IMEState::get().unwrap().ipc_service = None;
    }

    /// The same for the commit path, which makes TWO calls and can stop
    /// between them. A failed `ShrinkText` must leave the reading whole: the
    /// old arm had already dropped the committed prefix (and appended the new
    /// keystroke) before asking the server to commit anything (#85).
    #[test]
    fn a_failed_commit_leaves_the_keystrokes_alone() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(scripted(&["ん"], "ん", &[1], &[1]));
        fake.lock().unwrap().engine_fails = true;

        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let factory = factory_of(&tip);
        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.set_up_for_test(CompositionState::Previewing);
            composition.preview = "水".to_string();
            composition.raw_input = "mizu".to_string();
            composition.corresponding_count = 4;
            composition.surface_count = 2;
        }

        let result = factory.handle_action(
            &[ClientAction::ShrinkText("n".to_string())],
            CompositionState::Previewing,
        );
        assert!(result.is_err(), "an engine error must still be surfaced");

        let text_service = factory.borrow().unwrap();
        let composition = text_service.borrow_composition().unwrap();
        assert_eq!(
            composition.raw_input, "mizu",
            "ShrinkText failed, so nothing was committed and nothing was typed"
        );
        drop(composition);
        drop(text_service);

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
        let factory = factory_of(&tip);

        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.set_up_for_test(CompositionState::Composing);
            composition.preview = "わたし".to_string();
            composition.raw_input = "watashi".to_string();
            composition.raw_hiragana = "わたし".to_string();
            composition.corresponding_count = 7;
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
        assert_eq!(*composition.state(), CompositionState::None);
        assert!(
            composition.raw_hiragana.is_empty()
                && composition.raw_input.is_empty()
                && composition.preview.is_empty(),
            "the reading must be cleared even though the edit session failed; \
             a stale reading made the next keystroke resurrect the old text"
        );
        assert!(
            !composition.has_tip(),
            "the dead composition handle must be released"
        );

        drop(composition);
        drop(text_service);
        IMEState::get().unwrap().ipc_service = None;
    }
}
