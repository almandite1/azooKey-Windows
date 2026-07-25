//! The composition state the TIP keeps for one document, and the pure
//! helpers that operate on it.
//!
//! Everything that *does* something with a composition lives beside this
//! file: [`super::key_dispatch`] turns keystrokes into action batches,
//! [`super::actions`] runs them, [`super::candidate_ui`] publishes the
//! candidate list, and [`super::recovery`] puts the pieces back together
//! after the conversion server drops out.

use super::{
    client_action::ClientAction, full_width::to_fullwidth, input_mode::InputMode,
    ipc_service::Candidates,
};
use windows::Win32::UI::TextServices::ITfComposition;

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

/// Backspace shortened the reading: keep only the leading keystrokes the new
/// top candidate covers. `count` is `corresponding_count`, already clamped to
/// the list by the engine.
pub(super) fn raw_input_kept_for_count(raw_input: &str, count: i32) -> String {
    raw_input.chars().take(count as usize).collect()
}

/// A candidate was committed (shrink): append the freshly typed keystrokes,
/// then drop from the front the keystrokes that candidate consumed.
pub(super) fn raw_input_after_commit(raw_input: &str, appended: &str, count: i32) -> String {
    let mut combined = raw_input.to_string();
    combined.push_str(appended);
    combined.chars().skip(count as usize).collect()
}

/// The bytes the engine is given for `text` typed in `mode`.
///
/// The one place this transform is decided.
/// [`super::recovery::rebuild_server_composition`] replays a whole
/// `raw_input` through it while the append and shrink arms push one keystroke
/// at a time, and the rebuild is only idempotent if the two produce the same
/// bytes — which holds because the transform is per-character. One function
/// rather than three copies of the same `match` is what makes that testable.
pub(super) fn keystrokes(mode: &InputMode, text: &str) -> String {
    match mode {
        InputMode::Kana => to_fullwidth(text),
        InputMode::Latin => text.to_string(),
    }
}

/// The mutable working copy `handle_action` edits while dispatching a batch
/// of actions, then writes back to the live `Composition` in one shot.
///
/// Split out so each action is an ordinary `act_*` method taking `&mut
/// CompositionEdit` instead of threading eight locals through a 300-line
/// `match`. The write-back runs regardless of whether the batch succeeded,
/// so a failed action still leaves the client consistent with the server
/// (a mid-batch `?` used to skip the write-back and wedge input).
pub(super) struct CompositionEdit {
    pub(super) preview: String,
    pub(super) suffix: String,
    pub(super) raw_input: String,
    pub(super) raw_hiragana: String,
    pub(super) corresponding_count: i32,
    pub(super) surface_count: i32,
    pub(super) candidates: Candidates,
    pub(super) selection_index: i32,
    /// the state the composition moves to once the batch finishes
    pub(super) state: CompositionState,
}

impl CompositionEdit {
    /// Snapshots the live composition into a working copy that transitions
    /// to `state` on write-back.
    pub(super) fn from_composition(composition: &Composition, state: CompositionState) -> Self {
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

    /// Mirrors candidate entry `index` into the preview fields of this
    /// working copy. This is the single point where a selected candidate
    /// becomes the visible preview — the future hook for conversion-history
    /// learning to observe what the user actually picked.
    pub(super) fn adopt_candidate(&mut self, candidates: &Candidates, index: i32) {
        let (text, sub_text, count, surface) = candidates.entry(index as usize);
        self.corresponding_count = count;
        self.surface_count = surface;
        self.preview = text;
        self.suffix = sub_text;
        self.raw_hiragana = candidates.hiragana.clone();
    }

    /// Adopts the top of a freshly returned candidate list: resets the
    /// selection to index 0, mirrors that candidate into the preview, and
    /// stores the list. The shared head of append/remove/shrink — each gets a
    /// new, differently sized list from the engine, so a selection index left
    /// over from the previous list would adopt the wrong candidate or blank
    /// the preview via the `entry()` fallback.
    pub(super) fn adopt_fresh(&mut self, candidates: Candidates) {
        self.selection_index = 0;
        self.adopt_candidate(&candidates, self.selection_index);
        self.candidates = candidates;
    }

    /// Blanks the composing fields the terminating actions
    /// (end/cancel/mode-switch/server-loss) all reset: the selection, both
    /// spent counts, and the preview/suffix/reading strings. Leaves `state`
    /// and `candidates` to the caller — server-loss also drops the list and
    /// forces `None`, while end/mode-switch take the state from the write-back.
    pub(super) fn clear(&mut self) {
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
    pub(super) fn write_back(self, composition: &mut Composition) {
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

/// Whether a batch has to refresh the engine's view of the text surrounding
/// the composition before it runs.
///
/// `update_context` is not cheap: it opens an edit session on the parent
/// context, walks a range back over the composition and reads the document
/// text, then sends it to the engine. It used to run on EVERY batch —
/// candidate navigation, the MoveCursor no-op, mode switches, the teardown of
/// a composition that is already over (issue #36).
///
/// Only the actions that ask the engine to CONVERT can use that context, and
/// each of them refreshes it in its own batch, so nothing observes a stale
/// one. `StartComposition` is not in the list because it never arrives alone:
/// the transition table always pairs it with the keystroke that opened the
/// composition (`transition.rs`), and that keystroke is an `AppendText`.
pub(super) fn needs_context_update(actions: &[ClientAction]) -> bool {
    actions.iter().any(|action| {
        matches!(
            action,
            ClientAction::AppendText(_)
                | ClientAction::RemoveText
                | ClientAction::ShrinkText(_)
                | ClientAction::SetTextWithType(_)
        )
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::engine::client_action::{SetSelectionType, SetTextType};

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

    /// The invariant the rebuild depends on: replaying a whole reading in one
    /// call must produce exactly the bytes the keystroke-at-a-time path sent,
    /// or the rebuild stops being idempotent and the server is left holding a
    /// different reading than the client thinks it does.
    #[test]
    fn replaying_a_reading_whole_equals_replaying_it_per_keystroke() {
        for mode in [InputMode::Kana, InputMode::Latin] {
            // "-" and "," are in the kana punctuation map, so this exercises
            // the branch that actually transforms
            let reading = "ka-nyu,shi";
            let per_keystroke: String = reading
                .chars()
                .map(|c| keystrokes(&mode, &c.to_string()))
                .collect();
            assert_eq!(keystrokes(&mode, reading), per_keystroke, "mode {mode:?}");
        }
    }

    /// The allowlist in prose form: only the actions whose engine call can
    /// USE the surrounding text pay for measuring it (issue #36). The
    /// end-to-end counterpart lives in `actions.rs`.
    #[test]
    fn only_converting_actions_ask_for_the_surrounding_text() {
        assert!(needs_context_update(&[ClientAction::AppendText(
            "a".to_string()
        )]));
        assert!(needs_context_update(&[ClientAction::RemoveText]));
        assert!(needs_context_update(&[ClientAction::ShrinkText(
            "a".to_string()
        )]));
        assert!(needs_context_update(&[ClientAction::SetTextWithType(
            SetTextType::Katakana
        )]));

        assert!(!needs_context_update(&[ClientAction::StartComposition]));
        assert!(!needs_context_update(&[ClientAction::EndComposition]));
        assert!(!needs_context_update(&[ClientAction::MoveCursor(-1)]));
        assert!(!needs_context_update(&[ClientAction::SetSelection(
            SetSelectionType::Down
        )]));

        // any converting action in the batch is enough
        assert!(needs_context_update(&[
            ClientAction::StartComposition,
            ClientAction::AppendText("a".to_string()),
        ]));
    }
}
