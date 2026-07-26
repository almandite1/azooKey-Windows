//! Putting the composition back together after the conversion server drops
//! out mid-batch.
//!
//! Two steps, cheapest and least destructive first: rebuild the server's
//! reading from the composition the batch started with (#35), and only if
//! that also fails throw the composition away locally (#33). Both are driven
//! from [`super::actions::handle_action`], which swallows the keystroke's
//! error either way — we handled it, so the host must not also process the
//! key.

use crate::tsf::factory::TextServiceFactory_Impl;

use super::{
    composition::{Composition, CompositionEdit, CompositionState, keystrokes},
    input_mode::InputMode,
    ipc_service::IPCService,
};

impl TextServiceFactory_Impl {
    /// Puts the server's reading back to what it was when this batch started,
    /// so a server that merely stalled costs the user one keystroke instead of
    /// the whole composition (#35). Returns whether the composition survived.
    ///
    /// The rebuild is `ClearText` then the batch-start `raw_input` replayed as
    /// one `AppendText`. Both halves matter: neither the client nor the server
    /// knows whether a timed-out call was applied before the deadline (the
    /// reply was dropped, not refused), so the reading is re-established from
    /// scratch rather than patched, which makes the rebuild idempotent even
    /// though `AppendText` alone is not.
    ///
    /// `raw_input` is the right source because it is exactly the keystrokes
    /// the server has consumed for the reading it currently holds: every arm
    /// that sends keystrokes appends to it, and every arm that commits part of
    /// the reading drops the same prefix from it. Replaying it through
    /// `keystrokes` — the same transform the append arm uses — reproduces the
    /// byte sequence the server was originally given, because that transform
    /// is per-character (see its unit test).
    ///
    /// Nothing is drawn: the failing arms all call the engine BEFORE they
    /// touch the document, so the screen still shows the batch-start
    /// composition and restoring the working copy to match it leaves client,
    /// server and document agreeing again.
    pub(super) fn rebuild_server_composition(
        &self,
        edit: &mut CompositionEdit,
        ipc_service: &IPCService,
        snapshot: &Composition,
        batch_intent: &CompositionState,
        mode: &InputMode,
    ) -> bool {
        // A batch that was ending the composition (Enter, Escape, a mode
        // switch) has already committed or discarded the text on screen.
        // Rebuilding the reading would resurrect a composition the user
        // finished, so those go straight to the teardown, which is what they
        // were doing anyway.
        if *batch_intent == CompositionState::None
            || snapshot.state == CompositionState::None
            || snapshot.raw_input.is_empty()
        {
            return false;
        }

        let replay = keystrokes(mode, &snapshot.raw_input);

        if let Err(error) = ipc_service.clear_text() {
            tracing::warn!("could not clear the server before rebuilding: {error:#}");
            return false;
        }
        if let Err(error) = ipc_service.append_text(replay) {
            tracing::warn!("could not replay the reading onto the server: {error:#}");
            return false;
        }

        // The engine answered, so it is back. Discard its fresh candidate list
        // and keep the one already on screen: the reading is the same, the
        // document was never touched, and redrawing would make a recovered
        // stall look like a candidate list that jumped on its own.
        *edit = CompositionEdit::from_composition(snapshot, snapshot.state.clone());
        tracing::info!(
            "rebuilt the server composition after a stall; kept {} keystrokes",
            snapshot.raw_input.chars().count()
        );
        true
    }

    /// Locally tears down the composition after the server became unreachable
    /// mid-batch. Releases the TSF composition handle and hides the candidate
    /// window, then blanks the working copy so the write-back leaves the client
    /// in the `None` state. Deliberately issues NO conversion-server RPC (that
    /// pipe is the one that just failed — another call would only time out
    /// again); the candidate-window RPCs go to the separate, still-live ui
    /// process. Best-effort: we are already on an error path.
    pub(super) fn reset_composition_after_server_loss(
        &self,
        edit: &mut CompositionEdit,
        ipc_service: &IPCService,
    ) {
        if let Err(error) = self.end_composition() {
            tracing::warn!("end_composition during server-loss reset failed: {error:?}");
        }
        self.close_candidate_ui(ipc_service);

        edit.reset_for_teardown();
        edit.state = CompositionState::None;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::engine::client_action::ClientAction;
    use crate::engine::ipc_service::{Candidates, IpcCall};
    use crate::engine::state::IMEState;
    use crate::engine::test_util::{install_fake_ipc, recorded_calls, scripted};
    use crate::tsf::test_support::{
        EditSessionBehavior, FakeComposition, factory_of, factory_with_fake_context,
        global_state_lock,
    };

    /// A server that stalls for one call and then answers again must cost the
    /// user that keystroke and nothing else (#35). The rebuild re-establishes
    /// the reading the composition started the batch with — `ClearText`
    /// followed by the whole `raw_input` replayed in one `AppendText` — so the
    /// client, the server and the document agree again without the destructive
    /// reset (#33) that used to be the only recovery.
    #[test]
    fn append_rebuilds_the_server_reading_when_the_server_only_stalls() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(scripted(&["水"], "みず", &[4], &[2]));
        // only the keystroke's own AppendText fails; the rebuild's ClearText
        // and AppendText are answered, as by a server that came back
        fake.lock().unwrap().engine_unavailable_for = 1;

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
            composition.surface_count = 2;
            composition.candidates = scripted(&["水"], "みず", &[4], &[2]);
            composition.tip_composition = Some(FakeComposition::new());
        }

        factory
            .handle_action(
                &[ClientAction::AppendText("ka".to_string())],
                CompositionState::Composing,
            )
            .expect("a recovered stall must be swallowed, not surfaced");

        let text_service = factory.borrow().unwrap();
        let composition = text_service.borrow_composition().unwrap();
        assert_eq!(
            composition.state,
            CompositionState::Composing,
            "a recoverable stall must not throw the composition away"
        );
        assert_eq!(
            composition.raw_input, "mizu",
            "the composition must be exactly the one the batch started with — \
             the keystroke that hit the stall is dropped, the rest survives"
        );
        assert_eq!(composition.raw_hiragana, "みず");
        assert_eq!(composition.preview, "水");
        assert!(
            composition.tip_composition.is_some(),
            "the TSF composition must stay open"
        );
        drop(composition);
        drop(text_service);

        let calls = recorded_calls(&fake);
        let clear = calls
            .iter()
            .position(|c| *c == IpcCall::ClearText)
            .expect("the rebuild must clear the server's half-applied reading first");
        let replay = calls
            .iter()
            .position(|c| *c == IpcCall::AppendText("mizu".to_string()))
            .expect("the rebuild must replay the batch-start raw_input in one call");
        assert!(
            clear < replay,
            "the reading must be re-established from scratch, not patched: {calls:?}"
        );
        assert!(
            !calls.contains(&IpcCall::HideWindow),
            "a recovered stall must leave the candidate window alone: {calls:?}"
        );

        IMEState::get().unwrap().ipc_service = None;
    }

    /// A composition-ending batch (Enter, Escape, a mode switch) has already
    /// committed or discarded the text on screen by the time an engine RPC
    /// fails. Rebuilding the reading there would resurrect a composition the
    /// user finished, so the recovery must skip straight to the teardown.
    #[test]
    fn ending_the_composition_does_not_rebuild_a_finished_reading() {
        let _guard = global_state_lock();
        let fake = install_fake_ipc(Candidates::default());
        fake.lock().unwrap().engine_unavailable_for = 1;

        let (tip, _context) = factory_with_fake_context(EditSessionBehavior::RunSync);
        let factory = factory_of(&tip);
        {
            let text_service = factory.borrow().unwrap();
            let mut composition = text_service.borrow_mut_composition().unwrap();
            composition.state = CompositionState::Previewing;
            composition.preview = "水".to_string();
            composition.raw_input = "mizu".to_string();
            composition.raw_hiragana = "みず".to_string();
            composition.tip_composition = Some(FakeComposition::new());
        }

        factory
            .handle_action(&[ClientAction::EndComposition], CompositionState::None)
            .expect("the swallowed server loss must not surface");

        let text_service = factory.borrow().unwrap();
        let composition = text_service.borrow_composition().unwrap();
        assert_eq!(composition.state, CompositionState::None);
        assert!(
            composition.raw_input.is_empty() && composition.preview.is_empty(),
            "the finished composition must stay finished"
        );
        drop(composition);
        drop(text_service);

        assert!(
            !recorded_calls(&fake).contains(&IpcCall::AppendText("mizu".to_string())),
            "the reading of a committed composition must never be replayed"
        );

        IMEState::get().unwrap().ipc_service = None;
    }

    /// A server crash/restart mid-composition must not wedge input. The
    /// engine RPC comes back `ServerUnavailable` (the launcher is restarting
    /// the crashed server, whose per-connection reading is now empty); the
    /// client would otherwise keep its old preview/reading and append the next
    /// keystroke onto a reading the fresh server never had. The rebuild above
    /// is tried first and fails too, so the arm falls back to resetting the
    /// composition to None locally — releasing the TSF composition and hiding
    /// the candidate window — and swallows the error so the host does not also
    /// process the key. The next keystroke then starts clean.
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

        // the candidate window was torn down (that is the still-live ui
        // process); the conversion server was touched only by the one rebuild
        // attempt, and the teardown after it spends no further RPC on a pipe
        // that has already failed twice
        let calls = recorded_calls(&fake);
        assert!(
            calls.contains(&IpcCall::HideWindow),
            "the candidate window must hide on a server-loss reset: {calls:?}"
        );
        let hide = calls
            .iter()
            .position(|c| *c == IpcCall::HideWindow)
            .expect("the teardown must have run");
        assert!(
            !calls[hide..].contains(&IpcCall::ClearText),
            "the teardown must spend no further RPC on a pipe the rebuild \
             already found dead: {calls:?}"
        );
        assert!(
            !calls.contains(&IpcCall::AppendText("mizu".to_string())),
            "the rebuild's ClearText failed, so it must not go on to replay: {calls:?}"
        );

        IMEState::get().unwrap().ipc_service = None;
    }
}
