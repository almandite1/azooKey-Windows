//! Test helpers shared by the engine modules.
//!
//! The TSF-side fakes (contexts, thread managers, compositions) live in
//! `tsf/test_support.rs`; this is the engine-side counterpart — scripting
//! what the conversion server answers and reading back what was sent.

use std::sync::{Arc, Mutex};

use super::ipc_service::{Candidates, FakeIpc, IPCService, IpcCall};
use super::state::IMEState;

/// Installs a recording IPC service into the global state and scripts
/// the candidates the engine RPCs answer with. Callers must hold
/// `global_state_lock` and reset `ipc_service` to `None` when done.
#[allow(clippy::unwrap_used)]
pub(super) fn install_fake_ipc(scripted: Candidates) -> Arc<Mutex<FakeIpc>> {
    let (service, fake) = IPCService::new_fake().unwrap();
    fake.lock().unwrap().scripted_candidates = scripted;
    IMEState::get().unwrap().ipc_service = Some(service);
    fake
}

/// `counts` are keystrokes (what raw_input is measured in), `surfaces`
/// are kana of the reading (what ShrinkText spends) — the engine reports
/// both because they disagree whenever a candidate ends inside a romaji
/// cluster.
pub(super) fn scripted(
    texts: &[&str],
    hiragana: &str,
    counts: &[i32],
    surfaces: &[i32],
) -> Candidates {
    Candidates {
        texts: texts.iter().map(|s| s.to_string()).collect(),
        sub_texts: texts.iter().map(|_| String::new()).collect(),
        hiragana: hiragana.to_string(),
        corresponding_count: counts.to_vec(),
        surface_count: surfaces.to_vec(),
    }
}

#[allow(clippy::unwrap_used)]
pub(super) fn recorded_calls(fake: &Arc<Mutex<FakeIpc>>) -> Vec<IpcCall> {
    fake.lock().unwrap().calls.clone()
}
