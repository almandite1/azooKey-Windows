//! Tier 2 harness parts: switch the default input method, drive a real host
//! application with synthesised keystrokes, read back what it received.
//!
//! Every one of those actions is global to the desktop it runs on — it
//! changes the user's default IME and types into whatever holds focus — so
//! [`guard::require_vm`] refuses to let any of it start outside a virtual
//! machine. Running it on a development machine would retype the plan into
//! whatever window happened to be in front.

pub mod engine;
pub mod guard;
pub mod host;
pub mod keyboard;
pub mod logs;
pub mod overlay;
pub mod profile;
pub mod scenarios;
pub mod uia;
pub mod winevent;

use std::time::{Duration, Instant};

/// Waits for `probe` to produce a value. Everything here is asynchronous —
/// keystroke to TIP to engine to host application to accessibility tree — so
/// the harness polls rather than sleeping a fixed amount. Deliberately no
/// retries around a whole scenario: a flake that a retry hides stops being a
/// substitute for the manual checklist.
pub fn poll_until<T>(timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The stage a run reached, so a failure says *where* it broke rather than
/// only that it did (an acceptance criterion of the plan).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    Guard,
    Profile,
    Launch,
    Focus,
    Inject,
    Read,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Guard => "guard    (VM の中か)",
            Stage::Profile => "profile  (既定 IME の切替)",
            Stage::Launch => "launch   (ホストアプリの新規起動)",
            Stage::Focus => "focus    (前面化と入力先の確定)",
            Stage::Inject => "inject   (SendInput による打鍵)",
            Stage::Read => "read     (UI Automation での本文読取)",
        }
    }
}

/// Prints a stage banner. The harness is watched from a host-side PowerShell
/// through a captured log, so plain stdout lines are the interface.
pub fn stage(stage: Stage) {
    println!("== {} ==", stage.label());
}
