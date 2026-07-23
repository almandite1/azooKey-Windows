use std::cmp::max;

use crate::{
    engine::user_action::UserAction, extension::VKeyExt as _, tsf::factory::TextServiceFactory_Impl,
};

use super::{
    client_action::{ClientAction, SetSelectionType, SetTextType},
    full_width::{to_fullwidth, to_fullwidth_ascii, to_halfwidth},
    input_mode::InputMode,
    ipc_service::{Candidates, IPCService, ServerUnavailable},
    state::IMEState,
    text_util::{to_half_katakana, to_katakana},
    transition::{KeystrokeContext, is_modifier_key, shortcut_transition, transition},
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

    /// romaji keystrokes the preview covers, spent on `raw_input`
    pub corresponding_count: i32,
    /// kana of the reading the preview covers, spent on `ShrinkText`
    pub surface_count: i32,

    pub selection_index: i32,
    pub candidates: Candidates,

    pub state: CompositionState,
    pub tip_composition: Option<ITfComposition>,
}

/// True when `error` (or any of its causes) is a [`ServerUnavailable`] tag,
/// i.e. an engine RPC failed because the conversion server was unreachable
/// rather than because it rejected the request.
fn is_server_unavailable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| cause.is::<ServerUnavailable>())
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
    surface_count: &mut i32,
) {
    let (text, sub_text, count, surface) = candidates.entry(index as usize);
    *corresponding_count = count;
    *surface_count = surface;
    *preview = text;
    *suffix = sub_text;
    *raw_hiragana = candidates.hiragana.clone();
}

/// Backspace shortened the reading: keep only the leading keystrokes the new
/// top candidate covers. `count` is `corresponding_count`, already clamped to
/// the list by the engine — the `as usize` cast is the original inline
/// arithmetic, kept exact so the golden tests stay unchanged.
fn raw_input_kept_for_count(raw_input: &str, count: i32) -> String {
    raw_input.chars().take(count as usize).collect()
}

/// A candidate was committed (shrink): append the freshly typed keystrokes,
/// then drop from the front the keystrokes that candidate consumed. The same
/// two steps the inline code took (`push_str` then `skip`), as one value.
fn raw_input_after_commit(raw_input: &str, appended: &str, count: i32) -> String {
    let mut combined = raw_input.to_string();
    combined.push_str(appended);
    combined.chars().skip(count as usize).collect()
}

/// The mutable working copy `handle_action` edits while dispatching a batch
/// of actions, then writes back to the live `Composition` in one shot.
///
/// Split out so each action is an ordinary `act_*` method taking `&mut
/// CompositionEdit` instead of threading eight locals through a 300-line
/// `match`. The write-back runs regardless of whether the batch succeeded,
/// so a failed action still leaves the client consistent with the server
/// (a mid-batch `?` used to skip the write-back and wedge input).
struct CompositionEdit {
    preview: String,
    suffix: String,
    raw_input: String,
    raw_hiragana: String,
    corresponding_count: i32,
    surface_count: i32,
    candidates: Candidates,
    selection_index: i32,
    /// the state the composition moves to once the batch finishes
    state: CompositionState,
}

impl CompositionEdit {
    /// Snapshots the live composition into a working copy that transitions
    /// to `state` on write-back.
    fn from_composition(composition: &Composition, state: CompositionState) -> Self {
        Self {
            preview: composition.preview.clone(),
            suffix: composition.suffix.clone(),
            raw_input: composition.raw_input.clone(),
            raw_hiragana: composition.raw_hiragana.clone(),
            corresponding_count: composition.corresponding_count,
            surface_count: composition.surface_count,
            candidates: composition.candidates.clone(),
            selection_index: composition.selection_index,
            state,
        }
    }

    /// Mirrors candidate `index` of `candidates` into the preview fields.
    /// Method form of [`apply_selected_candidate`] operating on this working
    /// copy — the learning hook still lives in that function.
    fn adopt_candidate(&mut self, candidates: &Candidates, index: i32) {
        apply_selected_candidate(
            candidates,
            index,
            &mut self.preview,
            &mut self.suffix,
            &mut self.raw_hiragana,
            &mut self.corresponding_count,
            &mut self.surface_count,
        );
    }

    /// Adopts the top of a freshly returned candidate list: resets the
    /// selection to index 0, mirrors that candidate into the preview, and
    /// stores the list. The shared head of append/remove/shrink — each gets a
    /// new, differently sized list from the engine, so a selection index left
    /// over from the previous list would adopt the wrong candidate or blank
    /// the preview via the `entry()` fallback.
    fn adopt_fresh(&mut self, candidates: Candidates) {
        self.selection_index = 0;
        self.adopt_candidate(&candidates, self.selection_index);
        self.candidates = candidates;
    }

    /// Blanks the composing fields the terminating actions
    /// (end/cancel/mode-switch/server-loss) all reset: the selection, both
    /// spent counts, and the preview/suffix/reading strings. Leaves `state`
    /// and `candidates` to the caller — server-loss also drops the list and
    /// forces `None`, while end/mode-switch take the state from the write-back.
    fn clear(&mut self) {
        self.selection_index = 0;
        self.corresponding_count = 0;
        self.surface_count = 0;
        self.preview.clear();
        self.suffix.clear();
        self.raw_input.clear();
        self.raw_hiragana.clear();
    }

    /// Writes the working copy back onto the live composition. Leaves
    /// `tip_composition` alone — that handle is owned by start/end_composition.
    fn write_back(self, composition: &mut Composition) {
        composition.preview = self.preview;
        composition.state = self.state;
        composition.selection_index = self.selection_index;
        composition.raw_input = self.raw_input;
        composition.raw_hiragana = self.raw_hiragana;
        composition.candidates = self.candidates;
        composition.suffix = self.suffix;
        composition.corresponding_count = self.corresponding_count;
        composition.surface_count = self.surface_count;
    }
}

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

/// Flags for `ui_update` when the whole candidate list was replaced.
const CANDIDATES_CHANGED: u32 = TF_CLUIE_COUNT
    | TF_CLUIE_STRING
    | TF_CLUIE_SELECTION
    | TF_CLUIE_CURRENTPAGE
    | TF_CLUIE_PAGEINDEX;

/// Flags for `ui_update` when only the highlighted candidate moved.
const SELECTION_CHANGED: u32 = TF_CLUIE_SELECTION | TF_CLUIE_CURRENTPAGE;

impl TextServiceFactory_Impl {
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
        // Advisory (CLAUDE.md): UILess bookkeeping must never break typing.
        // Propagating here would mean a host-side element problem also
        // stopped the candidates reaching our own window.
        if let Err(error) = self.ui_update(candidates, selection_index, updated_flags) {
            tracing::warn!("ui_update failed (non-fatal): {error:?}");
        }

        if self.ui_should_show() {
            if updated_flags & TF_CLUIE_STRING != 0 {
                ipc_service.set_candidates(candidates.texts.clone());
            }
            ipc_service.set_selection(selection_index);
        }

        Ok(())
    }

    /// Renders the adopted preview into the document and republishes the whole
    /// candidate list. The tail shared by append and remove; shrink commits
    /// with `shift_start` instead of `set_text`, so it publishes on its own.
    fn render_and_publish_full(
        &self,
        edit: &CompositionEdit,
        ipc_service: &mut crate::engine::ipc_service::IPCService,
    ) -> Result<()> {
        self.set_text(&edit.preview, &edit.suffix)?;
        self.publish_candidates(
            ipc_service,
            &edit.candidates,
            edit.selection_index,
            CANDIDATES_CHANGED,
        )
    }

    /// Opens the candidate UI for a new composition: asks the host first
    /// (UILess), and shows our own window only when the host does not draw
    /// the candidates itself. Advisory throughout — a host-side element
    /// problem must not break typing, so on failure we fall back to our own
    /// window (the pre-UILess behaviour).
    ///
    /// Counterpart of `close_candidate_ui`; the visibility transitions live
    /// in this pair (and host-driven `ITfUIElement::Show`) only, so an arm
    /// cannot forget one half of the teardown again (issue #21).
    fn open_candidate_ui(&self, ipc_service: &mut crate::engine::ipc_service::IPCService) {
        let show = self.ui_begin().unwrap_or_else(|error| {
            tracing::warn!("ui_begin failed (non-fatal): {error:?}");
            true
        });
        if show {
            ipc_service.show_window();
        }
    }

    /// Closes the candidate UI: releases the host's UI element (UILess) and
    /// hides our own window, blanking the now-stale list. Everything here is
    /// unconditional and advisory: hiding is safe even if we never showed,
    /// and ui_end is a no-op with no live element.
    fn close_candidate_ui(&self, ipc_service: &mut crate::engine::ipc_service::IPCService) {
        if let Err(error) = self.ui_end() {
            tracing::warn!("ui_end failed (non-fatal): {error:?}");
        }
        ipc_service.hide_window();
        ipc_service.set_candidates(vec![]);
    }

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
        if self.is_reserved_keystroke(wparam.0)? {
            return Ok(None);
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

        let mut edit = CompositionEdit::from_composition(&composition, transition);
        let mut ipc_service = IMEState::get()?
            .ipc_service
            .clone()
            .context("ipc_service is None")?;

        // preview AND suffix: the caret sits after both, so a window that
        // only steps back over the preview feeds the suffix to the engine as
        // if it were text the user had already committed
        self.update_context(&format!("{}{}", edit.preview, edit.suffix))?;

        // wrapped so the write-back below ALWAYS runs: an early return on a
        // failed action used to skip it, desyncing the client composition
        // from the server (stuck input)
        let result = (|| -> Result<()> {
            for action in actions {
                match action {
                    ClientAction::StartComposition => {
                        self.act_start_composition(&mut ipc_service)?
                    }
                    ClientAction::EndComposition => {
                        self.act_end_composition(&mut edit, &mut ipc_service, false)?
                    }
                    ClientAction::CancelComposition => {
                        self.act_end_composition(&mut edit, &mut ipc_service, true)?
                    }
                    ClientAction::AppendText(text) => {
                        self.act_append_text(&mut edit, &mut ipc_service, &mode, text)?
                    }
                    ClientAction::RemoveText => {
                        self.act_remove_text(&mut edit, &mut ipc_service)?
                    }
                    ClientAction::MoveCursor(_offset) => {
                        // Deliberate no-op for now: the MoveCursor RPC and the
                        // Swift engine's cursor handling are live (kept green by
                        // the move_cursor smoke test in crates/server), but the
                        // client-side wiring is deferred to the predictive-
                        // conversion feature, which needs cursor movement anyway.
                    }
                    ClientAction::SetIMEMode(mode) => {
                        self.act_set_ime_mode(&mut edit, &mut ipc_service, mode)?
                    }
                    ClientAction::SetSelection(selection) => {
                        self.act_set_selection(&mut edit, &mut ipc_service, selection)?
                    }
                    ClientAction::ShrinkText(text) => {
                        self.act_shrink_text(&mut edit, &mut ipc_service, &mode, text)?
                    }
                    ClientAction::SetTextWithType(set_type) => {
                        self.act_set_text_with_type(&mut edit, set_type)?
                    }
                }
            }
            Ok(())
        })();

        // If the batch failed because the conversion server became
        // unreachable (crash/restart/hang), the client composition can no
        // longer be trusted to mirror the server: the server's per-connection
        // reading is gone, but the client still holds the old preview/reading.
        // Continuing would append the next keystroke onto a reading the fresh
        // server never had (raw_input and the server state silently diverge).
        // Reset the composition locally so the next keystroke opens a clean one
        // against the fresh server session, and swallow this keystroke's error
        // (we handled it) so the host does not also process the key.
        let recovered = matches!(&result, Err(err) if is_server_unavailable(err));
        if recovered {
            self.reset_composition_after_server_loss(&mut edit, &mut ipc_service);
        }

        // write back the state of the last successful action even when a
        // later action failed, keeping the client consistent with the server
        let text_service = self.borrow()?;
        let mut composition = text_service.borrow_mut_composition()?;
        edit.write_back(&mut composition);
        drop(composition);
        drop(text_service);

        if recovered { Ok(()) } else { result }
    }

    /// Locally tears down the composition after the server became unreachable
    /// mid-batch. Releases the TSF composition handle and hides the candidate
    /// window, then blanks the working copy so the write-back leaves the client
    /// in the `None` state. Deliberately issues NO conversion-server RPC (that
    /// pipe is the one that just failed — another call would only time out
    /// again); the candidate-window RPCs go to the separate, still-live ui
    /// process. Best-effort: we are already on an error path.
    fn reset_composition_after_server_loss(
        &self,
        edit: &mut CompositionEdit,
        ipc_service: &mut IPCService,
    ) {
        if let Err(error) = self.end_composition() {
            tracing::warn!("end_composition during server-loss reset failed: {error:?}");
        }
        self.close_candidate_ui(ipc_service);

        edit.clear();
        edit.state = CompositionState::None;
        edit.candidates = Candidates::default();
    }

    /// Begins a TSF composition and opens the candidate UI. `open_candidate_ui`
    /// runs after `update_pos` so a UILess host knows where the caret is
    /// before deciding whether it draws the candidates itself.
    fn act_start_composition(&self, ipc_service: &mut IPCService) -> Result<()> {
        self.start_composition()?;
        self.update_pos()?;
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
        ipc_service: &mut IPCService,
        cancel: bool,
    ) -> Result<()> {
        let mut edit_result = Ok(());
        if cancel {
            edit_result = self.set_text("", "");
        }
        // tear down the TSF composition regardless; even on a failed session
        // end_composition releases the client-side handle
        edit_result = edit_result.and(self.end_composition());

        edit.clear();
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
        ipc_service: &mut IPCService,
        mode: &InputMode,
        text: &str,
    ) -> Result<()> {
        edit.raw_input.push_str(text);

        let text = match mode {
            InputMode::Kana => to_fullwidth(text, false),
            InputMode::Latin => text.to_string(),
        };

        let candidates = ipc_service.append_text(text)?;
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
    fn act_remove_text(
        &self,
        edit: &mut CompositionEdit,
        ipc_service: &mut IPCService,
    ) -> Result<()> {
        let candidates = ipc_service.remove_text()?;
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
        ipc_service: &mut IPCService,
        mode: &InputMode,
    ) -> Result<()> {
        self.start_composition()?;
        self.update_pos()?;
        self.end_composition()?;

        self.close_candidate_ui(ipc_service);

        // publishes the mode to the langbar, the indicator, and the OS
        // compartments (so the touch keyboard, IMM32 apps and the shell see
        // it too). Captured rather than `?`: a langbar or compartment failure
        // must not skip the teardown below, or the write-back keeps the stale
        // preview/reading and the next keystroke resurrects the old reading
        // (same pattern as act_end_composition).
        let apply_result = self.apply_input_mode(mode.clone(), true);

        edit.clear();
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
        ipc_service: &mut IPCService,
        selection: &SetSelectionType,
    ) -> Result<()> {
        let candidates = {
            let text_service = self.borrow()?;
            let composition = text_service.borrow_composition()?.clone();
            composition.candidates.clone()
        };

        let texts = candidates.texts.clone();

        // clamp lower bound first: on an empty list len() - 1 is -1 and the
        // later `as usize` cast would go out of bounds
        edit.selection_index = match selection {
            SetSelectionType::Up => edit.selection_index - 1,
            SetSelectionType::Down => edit.selection_index + 1,
        }
        .clamp(0, max(0, texts.len() as i32 - 1));

        self.publish_candidates(
            ipc_service,
            &candidates,
            edit.selection_index,
            SELECTION_CHANGED,
        )?;
        edit.adopt_candidate(&candidates, edit.selection_index);

        self.set_text(&edit.preview, &edit.suffix)
    }

    /// Confirms the selected candidate and keeps composing the remainder:
    /// commits it with `shift_start`, drops the committed input elements, and
    /// forces the state back to Composing for the fresh reading.
    fn act_shrink_text(
        &self,
        edit: &mut CompositionEdit,
        ipc_service: &mut IPCService,
        mode: &InputMode,
        text: &str,
    ) -> Result<()> {
        edit.raw_input = raw_input_after_commit(&edit.raw_input, text, edit.corresponding_count);

        // kana, not keystrokes: only the reading can express a candidate
        // that ends inside a romaji cluster
        ipc_service.shrink_text(edit.surface_count)?;
        let text = match mode {
            InputMode::Kana => to_fullwidth(text, false),
            InputMode::Latin => text.to_string(),
        };
        let candidates = ipc_service.append_text(text)?;

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
        )?;
        self.update_pos()?;

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
    use crate::engine::client_action::SetTextType;
    use crate::engine::ipc_service::{FakeIpc, IPCService, IpcCall};
    use crate::tsf::test_support::{
        EditSessionBehavior, FakeComposition, FakeContext, RangeLog, factory_of,
        factory_with_context, factory_with_fake_context, global_state_lock,
    };
    use std::rc::Rc;
    use std::sync::{Arc, Mutex};

    #[test]
    fn raw_input_kept_for_count_keeps_the_leading_keystrokes() {
        // backspace left a top candidate covering 3 keystrokes of "kyou"
        assert_eq!(raw_input_kept_for_count("kyou", 3), "kyo");
        // covers everything: unchanged
        assert_eq!(raw_input_kept_for_count("kyou", 4), "kyou");
        // covers nothing: emptied
        assert_eq!(raw_input_kept_for_count("kyou", 0), "");
        // a count past the end keeps all of it (take saturates), matching the
        // original `as usize` arithmetic
        assert_eq!(raw_input_kept_for_count("kyou", 9), "kyou");
    }

    #[test]
    fn raw_input_after_commit_appends_then_drops_the_committed_keystrokes() {
        // committed 2 keystrokes, then typed "u": "ki" + "u" -> drop 2 -> "u"
        assert_eq!(raw_input_after_commit("ki", "u", 2), "u");
        // nothing appended, drop the first 3 of "kyou"
        assert_eq!(raw_input_after_commit("kyou", "", 3), "u");
        // committed count covers the whole combined input
        assert_eq!(raw_input_after_commit("ki", "u", 3), "");
        // count of zero drops nothing
        assert_eq!(raw_input_after_commit("ki", "u", 0), "kiu");
    }

    /// Installs a recording IPC service into the global state and scripts
    /// the candidates the engine RPCs answer with. Callers must hold
    /// `global_state_lock` and reset `ipc_service` to `None` when done.
    fn install_fake_ipc(scripted: Candidates) -> Arc<Mutex<FakeIpc>> {
        let (service, fake) = IPCService::new_fake().unwrap();
        fake.lock().unwrap().scripted_candidates = scripted;
        IMEState::get().unwrap().ipc_service = Some(service);
        fake
    }

    /// `counts` are keystrokes (what raw_input is measured in), `surfaces`
    /// are kana of the reading (what ShrinkText spends) — the engine reports
    /// both because they disagree whenever a candidate ends inside a romaji
    /// cluster.
    fn scripted(texts: &[&str], hiragana: &str, counts: &[i32], surfaces: &[i32]) -> Candidates {
        Candidates {
            texts: texts.iter().map(|s| s.to_string()).collect(),
            sub_texts: texts.iter().map(|_| String::new()).collect(),
            hiragana: hiragana.to_string(),
            corresponding_count: counts.to_vec(),
            surface_count: surfaces.to_vec(),
        }
    }

    fn recorded_calls(fake: &Arc<Mutex<FakeIpc>>) -> Vec<IpcCall> {
        fake.lock().unwrap().calls.clone()
    }

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
            composition.state = CompositionState::Composing;
            composition.tip_composition = Some(FakeComposition::new());
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
            composition.state = CompositionState::Previewing;
            composition.selection_index = 3;
            composition.preview = "水".to_string();
            composition.tip_composition = Some(FakeComposition::new());
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
            composition.state = CompositionState::Previewing;
            composition.selection_index = 3;
            composition.raw_input = "mizuu".to_string();
            composition.tip_composition = Some(FakeComposition::new());
        }

        factory
            .handle_action(&[ClientAction::RemoveText], CompositionState::Composing)
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

        assert!(recorded_calls(&fake).contains(&IpcCall::RemoveText));
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
            composition.state = CompositionState::Previewing;
            composition.preview = "水".to_string();
            composition.raw_input = "mizu".to_string();
            composition.corresponding_count = 4;
            composition.surface_count = 2; // みず — two kana, four keystrokes
            composition.tip_composition = Some(FakeComposition::new());
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
            composition.state,
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
            composition.state = CompositionState::Previewing;
            composition.candidates = scripted(&["a", "b", "c"], "あ", &[1, 1, 1], &[1, 1, 1]);
            composition.selection_index = 2;
            composition.tip_composition = Some(FakeComposition::new());
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
            composition.state = CompositionState::Composing;
            composition.preview = "水".to_string();
            composition.tip_composition = Some(FakeComposition::new());
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
            composition.state = CompositionState::Composing;
            composition.preview = "わたし".to_string();
            composition.tip_composition = Some(FakeComposition::new());
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
            composition.state = CompositionState::Composing;
            composition.selection_index = 2;
            composition.corresponding_count = 7;
            composition.preview = "わたし".to_string();
            composition.suffix = "は".to_string();
            composition.raw_input = "watashiha".to_string();
            composition.raw_hiragana = "わたしは".to_string();
            composition.tip_composition = Some(FakeComposition::new());
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
            composition.state = CompositionState::Composing;
            composition.preview = "水".to_string();
            composition.suffix = "うみ".to_string();
            composition.tip_composition = Some(FakeComposition::new());
        }

        // an empty batch still runs update_context, which is all this covers
        factory
            .handle_action(&[], CompositionState::Composing)
            .unwrap();

        assert_eq!(
            log.shift_end_reqs.borrow().last(),
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

    /// A server crash/restart mid-composition must not wedge input. The
    /// engine RPC comes back `ServerUnavailable` (the launcher is restarting
    /// the crashed server, whose per-connection reading is now empty); the
    /// client would otherwise keep its old preview/reading and append the next
    /// keystroke onto a reading the fresh server never had. Instead the arm
    /// resets the composition to None locally — releasing the TSF composition
    /// and hiding the candidate window — and swallows the error so the host
    /// does not also process the key. The next keystroke then starts clean.
    #[test]
    fn append_resets_the_composition_when_the_server_becomes_unavailable() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(scripted(&["水"], "みず", &[4], &[2]));
        // the server crashed: the next engine RPC fails as ServerUnavailable
        fake.lock().unwrap().engine_unavailable = true;

        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let factory = factory_of(&tip);
        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.state = CompositionState::Composing;
            composition.preview = "水".to_string();
            composition.raw_input = "mizu".to_string();
            composition.raw_hiragana = "みず".to_string();
            composition.corresponding_count = 4;
            composition.tip_composition = Some(FakeComposition::new());
        }

        // the crashing keystroke is swallowed, not surfaced as an error
        factory
            .handle_action(
                &[ClientAction::AppendText("ka".to_string())],
                CompositionState::Composing,
            )
            .expect("a server-loss recovery must be swallowed, not surfaced");

        let text_service = factory.borrow().unwrap();
        let composition = text_service.borrow_composition().unwrap();
        assert_eq!(
            composition.state,
            CompositionState::None,
            "the composition must reset to None after the server was lost"
        );
        assert!(
            composition.preview.is_empty()
                && composition.suffix.is_empty()
                && composition.raw_input.is_empty()
                && composition.raw_hiragana.is_empty(),
            "the stale reading must be cleared so the next keystroke starts fresh"
        );
        assert!(
            composition.tip_composition.is_none(),
            "the TSF composition handle must be released"
        );
        drop(composition);
        drop(text_service);

        // the candidate window was torn down (server pipe was NOT touched
        // again — only the still-live ui process)
        let calls = recorded_calls(&fake);
        assert!(
            calls.contains(&IpcCall::HideWindow),
            "the candidate window must hide on a server-loss reset: {calls:?}"
        );
        assert!(
            !calls.contains(&IpcCall::ClearText),
            "the reset must not call the unreachable server again: {calls:?}"
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
        let factory = factory_of(&tip);

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
