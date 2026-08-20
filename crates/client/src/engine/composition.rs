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

    /// PRIVATE, with `tip`, because the two are one fact stated twice: a
    /// composition is `Composing`/`Previewing` exactly when TSF is holding a
    /// composition open for us. See the module note on [`Composition`]'s
    /// invariant.
    state: CompositionState,
    tip: Option<ITfComposition>,
}

// THE INVARIANT: between batches, `state == None` if and only if there is no
// TSF composition handle.
//
// "Between batches" is the honest scope. The two are written by different
// mechanisms at different moments: `tip` by start/end_composition, the
// instant TSF hands one over or takes it back, and `state` by
// `CompositionEdit::commit` when the whole batch is done. Inside a batch they
// legitimately disagree — `act_start_composition` attaches the handle while
// the live `state` is still the previous batch's — which is why every reader
// *during* a batch asks about the handle (the physical truth) and never about
// `state`.
//
// Breaking it has cost this project two bugs, both with the same symptom:
// typing that goes nowhere. A `Composing` state with no handle sends the next
// keystroke into the composing arm, where `set_text` finds nothing to write
// to and no-ops — invisible input with nothing logged. It happened when
// `Deactivate` left the state behind for the next activation, and again when
// `start_composition` found a stale handle and returned early.
//
// `commit` is the one place both are in scope, so it is where the invariant
// is enforced rather than assumed.
impl Composition {
    /// What the last completed batch left this composition in.
    pub fn state(&self) -> &CompositionState {
        &self.state
    }

    /// The live TSF composition, if TSF is holding one open.
    ///
    /// This — not `state` — is what the edit-session paths ask, because they
    /// run *during* a batch, when only the handle is current.
    pub fn tip(&self) -> Option<&ITfComposition> {
        self.tip.as_ref()
    }

    /// Whether TSF is holding a composition open for us.
    pub fn has_tip(&self) -> bool {
        self.tip.is_some()
    }

    /// Records the handle TSF just gave us. `start_composition` only.
    pub fn attach_tip(&mut self, tip: Option<ITfComposition>) {
        self.tip = tip;
    }

    /// Lets go of the handle. `end_composition` only, and unconditionally —
    /// keeping a handle to a composition TSF has finished wedges every later
    /// `start_composition`.
    pub fn detach_tip(&mut self) {
        self.tip = None;
    }

    /// Puts a test's composition into `state` WITH a matching fake handle.
    ///
    /// One call rather than two assignments on purpose: a test that set only
    /// `state` used to build the very inconsistency the invariant forbids,
    /// and then pass — proving something about a shape production can no
    /// longer reach.
    #[cfg(test)]
    pub fn set_up_for_test(&mut self, state: CompositionState) {
        self.tip = match state {
            CompositionState::None => None,
            _ => Some(crate::tsf::test_support::FakeComposition::new()),
        };
        self.state = state;
    }

    /// The forbidden shape, on purpose: `state` says composing, TSF holds
    /// nothing. Only the tests that assert the reconciliation repairs it may
    /// use this.
    #[cfg(test)]
    pub fn force_desync_for_test(&mut self, state: CompositionState, tip: bool) {
        self.tip = tip.then(crate::tsf::test_support::FakeComposition::new);
        self.state = state;
    }
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
    /// Where the composition actually ENDS UP — the value the write-back
    /// commits. It starts as the batch's intent and an arm may override it:
    /// `act_shrink_text` forces `Composing`, because after a commit there is
    /// a fresh reading to compose whatever the transition table asked for.
    ///
    /// Distinct from `handle_action`'s `batch_intent`, which is that same
    /// starting value kept immutable. Both exist because the recovery path
    /// has to ask "was this batch tearing the composition down?" — a
    /// question about the intent — after the arms have already rewritten
    /// this field.
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

    /// Which candidate this composition would be confirming, for the engine
    /// to learn from — or `None` when what is about to be committed is not a
    /// candidate the user picked off the list.
    ///
    /// The check is against the screen rather than against a flag: a
    /// selection index only names a confirmed candidate while the preview is
    /// still that candidate's text. That is what makes it correct without any
    /// new state to keep in step, and it excludes cases like backspace
    /// emptying the list — an index into nothing matches nothing — without
    /// enumerating them here.
    ///
    /// What it does NOT catch on its own is a preview that was rewritten into
    /// the same text by something other than a choice off the list. F6–F10
    /// transform the reading, and a transformation can spell what the
    /// highlighted entry spells (ぱそこん → F7 → パソコン, which is also the
    /// top candidate), so the texts match while the user picked nothing. That
    /// is why those arms clear the selection themselves, through
    /// [`CompositionEdit::discard_candidate_selection`] (#110).
    ///
    /// The counterpart to [`CompositionEdit::adopt_candidate`], which is
    /// where the preview and the index are put INTO agreement.
    pub(super) fn confirmed_candidate_index(&self) -> Option<i32> {
        let index = usize::try_from(self.selection_index).ok()?;
        (self.candidates.texts.get(index) == Some(&self.preview)).then_some(self.selection_index)
    }

    /// Says the preview is no longer any candidate's, so committing it
    /// confirms nothing and the engine learns nothing.
    ///
    /// The undo of [`CompositionEdit::adopt_candidate`], for the arms that
    /// replace the preview with something derived from the READING instead
    /// of from the list.
    ///
    /// A negative index rather than an emptied list: the list is still what
    /// the candidate window is showing and what an arrow key moves through.
    /// Nothing downstream sees the negative — `act_set_selection` clamps into
    /// range before it publishes or adopts, and the UILess accessors clamp
    /// with `max(0)`.
    pub(super) fn discard_candidate_selection(&mut self) {
        self.selection_index = -1;
    }

    /// Mirrors candidate entry `index` into the preview fields of this
    /// working copy. This is the single point where a selected candidate
    /// becomes the visible preview, and so the thing
    /// [`CompositionEdit::confirmed_candidate_index`] checks against when a
    /// commit asks what the user actually picked.
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

    /// Blanks everything a terminating action (end/cancel/mode-switch/
    /// server-loss) leaves behind: the selection, both spent counts, the
    /// preview/suffix/reading strings AND the candidate list.
    ///
    /// The list is in here rather than at the call sites because it is dead
    /// on every one of those paths — the composition it described is over —
    /// and only one of the three used to drop it. The other two wrote a
    /// stale list back onto a composition whose `state` was `None`. Nothing
    /// reads it in that state today (the next batch's `adopt_fresh`
    /// overwrites it before anything can), which is exactly what made the
    /// inconsistency survive: it is a trap for the next arm that looks at
    /// `candidates` without checking `state` first.
    ///
    /// `state` is still the caller's: server-loss forces `None` on the spot,
    /// while end/mode-switch take it from the batch's write-back.
    pub(super) fn reset_for_teardown(&mut self) {
        self.selection_index = 0;
        self.corresponding_count = 0;
        self.surface_count = 0;
        self.preview.clear();
        self.suffix.clear();
        self.raw_input.clear();
        self.raw_hiragana.clear();
        self.candidates = Candidates::default();
    }

    /// Writes the working copy back onto the live composition, reconciling
    /// the state with the TSF handle.
    ///
    /// The handle itself is left alone — it belongs to start/end_composition,
    /// which are the only things that can legitimately obtain or release one.
    /// What happens here is the other direction: the batch's `state` is
    /// checked against the handle, because this is the one moment both are in
    /// scope (see the invariant note on [`Composition`]).
    ///
    /// A batch that meant to compose but has no handle is the failure that
    /// wedges input: the next keystroke lands in the composing arm and
    /// `set_text` no-ops against nothing. It happens when `start_composition`
    /// fails and the batch still writes back its intent. Forcing `None` there
    /// costs that batch's keystroke and lets the NEXT one open a fresh
    /// composition, instead of every later keystroke disappearing.
    pub(super) fn commit(self, composition: &mut Composition) {
        composition.preview = self.preview;
        composition.state = reconcile(self.state, composition.tip.is_some());
        composition.selection_index = self.selection_index;
        composition.raw_input = self.raw_input;
        composition.raw_hiragana = self.raw_hiragana;
        composition.candidates = self.candidates;
        composition.suffix = self.suffix;
        composition.corresponding_count = self.corresponding_count;
        composition.surface_count = self.surface_count;
    }
}

/// The state a batch may leave behind, given whether TSF is still holding a
/// composition open.
///
/// Pure, so both directions of the invariant can be pinned by a unit test
/// rather than only by driving a whole batch.
fn reconcile(intended: CompositionState, has_tip: bool) -> CompositionState {
    match (&intended, has_tip) {
        // The wedge: composing with nothing to compose in. Start over.
        (CompositionState::Composing | CompositionState::Previewing, false) => {
            tracing::warn!(
                "batch ended in {intended:?} with no TSF composition; \
                 resetting to None so the next keystroke can start one"
            );
            CompositionState::None
        }
        // The other direction cannot be repaired from here — releasing a TSF
        // composition needs an edit session, which `commit` has no business
        // opening. Say so; `start_composition`'s stale check and `Deactivate`
        // both clean it up, and neither loses input the way the case above
        // does.
        (CompositionState::None, true) => {
            tracing::warn!("batch ended in None while TSF still holds a composition");
            CompositionState::None
        }
        _ => intended,
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
/// Only the actions that rewrite the reading can use that context, and each
/// of them refreshes it in its own batch, so nothing observes a stale one.
/// `StartComposition` is not in the list because it never arrives alone: the
/// transition table always pairs it with the keystroke that opened the
/// composition (`transition.rs`), and that keystroke is an `AppendText`.
pub(super) fn needs_context_update(actions: &[ClientAction]) -> bool {
    actions.iter().any(uses_surrounding_text)
}

/// Whether this one action's outcome can depend on the text around the
/// composition.
///
/// An exhaustive `match` with NO wildcard, deliberately: the answer for a new
/// `ClientAction` is a judgement call, and a wildcard would make it silently
/// "no". Adding a variant is a compile error here until someone decides.
fn uses_surrounding_text(action: &ClientAction) -> bool {
    match action {
        // these rewrite the reading, and the conversion the engine runs for
        // them is what the left-side context feeds
        ClientAction::AppendText(_)
        | ClientAction::RemoveText(_)
        | ClientAction::ShrinkText(_)
        // NOTE: SetTextWithType (F6-F10) issues no engine RPC at all — it
        // transforms `raw_input`/`raw_hiragana` locally. It has always been
        // in this set, so it stays; dropping it is a behaviour change and
        // belongs with whoever measures whether the extra edit session per
        // F-key is worth anything.
        | ClientAction::SetTextWithType(_) => true,

        // navigation and lifecycle: no conversion happens, so the context
        // measured for them would be thrown away (issue #36). Doubly so for
        // CompositionTerminated, which must not open ANY edit session — the
        // host ended the composition and owns the document again (#109).
        ClientAction::StartComposition
        | ClientAction::EndComposition
        | ClientAction::CancelComposition
        | ClientAction::CompositionTerminated
        | ClientAction::EndCompositionAtFocusLoss
        | ClientAction::MoveCursor(_)
        | ClientAction::SetSelection(_)
        | ClientAction::SetIMEMode(_) => false,
    }
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

    /// A working copy in the shape `confirmed_candidate_index` is asked
    /// about: a list, a selection, and whatever the preview currently says.
    fn previewing(texts: &[&str], selection_index: i32, preview: &str) -> CompositionEdit {
        CompositionEdit {
            preview: preview.to_string(),
            suffix: String::new(),
            raw_input: String::new(),
            raw_hiragana: String::new(),
            corresponding_count: 0,
            surface_count: 0,
            candidates: Candidates {
                texts: texts.iter().map(|t| (*t).to_string()).collect(),
                ..Candidates::default()
            },
            selection_index,
            state: CompositionState::Previewing,
        }
    }

    /// What the engine learns from is decided by comparing the highlighted
    /// candidate with what is actually on screen, so this is the whole
    /// contract in four cases.
    #[test]
    fn a_confirmed_candidate_is_the_one_the_preview_still_shows() {
        assert_eq!(
            previewing(&["水", "見ず", "みず"], 1, "見ず").confirmed_candidate_index(),
            Some(1),
            "the highlighted candidate is what the user picked"
        );
    }

    /// F6–F10 rewrite the preview without touching the selection. The user
    /// asked for a transformation of the reading, not for the kanji the
    /// index still points at — and learning that pairing would be wrong.
    #[test]
    fn a_rewritten_preview_confirms_no_candidate() {
        assert_eq!(
            previewing(&["水", "見ず"], 0, "ミズ").confirmed_candidate_index(),
            None
        );
    }

    /// Backspace can leave the list empty while an index survives.
    #[test]
    fn an_index_into_an_empty_list_confirms_nothing() {
        assert_eq!(previewing(&[], 0, "").confirmed_candidate_index(), None);
        assert_eq!(
            previewing(&["水"], 5, "水").confirmed_candidate_index(),
            None
        );
    }

    /// Not reachable today, and cheap to hold: `usize::try_from` is what
    /// stands between a negative index and an `as usize` that would wrap
    /// into a very large one.
    #[test]
    fn a_negative_index_confirms_nothing() {
        assert_eq!(
            previewing(&["水"], -1, "水").confirmed_candidate_index(),
            None
        );
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
        assert!(needs_context_update(&[ClientAction::RemoveText(1)]));
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

    /// A batch that meant to keep composing but has no TSF composition is the
    /// shape that eats keystrokes: the next one lands in the composing arm
    /// and `set_text` writes to nothing. Both of the historical bugs (the
    /// `Deactivate` leftover, the stale handle in `start_composition`) had
    /// this shape. Resetting to `None` costs that batch and lets the next
    /// keystroke open a fresh composition.
    #[test]
    fn composing_without_a_handle_resets_to_none() {
        assert_eq!(
            reconcile(CompositionState::Composing, false),
            CompositionState::None
        );
        assert_eq!(
            reconcile(CompositionState::Previewing, false),
            CompositionState::None
        );
    }

    /// The ordinary outcomes pass through untouched — the reconciliation is
    /// a repair, not a policy.
    #[test]
    fn a_state_that_matches_the_handle_is_left_alone() {
        assert_eq!(
            reconcile(CompositionState::Composing, true),
            CompositionState::Composing
        );
        assert_eq!(
            reconcile(CompositionState::Previewing, true),
            CompositionState::Previewing
        );
        assert_eq!(
            reconcile(CompositionState::None, false),
            CompositionState::None
        );
    }

    /// The other direction cannot be repaired here — releasing a TSF
    /// composition needs an edit session — so `None` stands and the warning
    /// is the whole remedy. `start_composition`'s stale check and
    /// `Deactivate` are what actually clean it up.
    #[test]
    fn a_leftover_handle_does_not_resurrect_the_state() {
        assert_eq!(
            reconcile(CompositionState::None, true),
            CompositionState::None
        );
    }
}
