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

use std::time::Duration;

/// How often the harness re-checks an asynchronous effect. Slower than the
/// display tests' tick because every step here crosses a keystroke, the TIP,
/// the engine, the host application and the accessibility tree.
const POLL_TICK: Duration = Duration::from_millis(50);

/// Waits for `probe` to produce a value, on this harness's tick. Deliberately
/// no retries around a whole scenario: a flake that a retry hides stops being
/// a substitute for the manual checklist.
pub fn poll_until<T>(timeout: Duration, probe: impl FnMut() -> Option<T>) -> Option<T> {
    test_support::poll_until_tick(timeout, POLL_TICK, probe)
}

/// The IME notifications recorder, re-exported so scenarios keep saying
/// `winevent::…`.
pub mod winevent {
    pub use test_support::win_events::{ImeEvent, ImeEventLog, ime_events as log};
}

/// The stage a run reached, so a failure says *where* it broke rather than
/// only that it did (an acceptance criterion of the plan). Only the two
/// whole-run prerequisites are banners; everything past them is per-scenario
/// and reported by the scenario itself.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Stage {
    Guard,
    Profile,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Guard => "guard    (VM の中か)",
            Stage::Profile => "profile  (既定 IME の切替)",
        }
    }
}

/// Prints a stage banner. The harness is watched from a host-side PowerShell
/// through a captured log, so plain stdout lines are the interface.
pub fn stage(stage: Stage) {
    println!("== {} ==", stage.label());
}
