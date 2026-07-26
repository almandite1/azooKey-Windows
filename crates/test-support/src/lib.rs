//! Watching the IME from outside the process: windows, processes, IME
//! notifications and the accessibility tree.
//!
//! Shared by the two harnesses that do that — the ui.exe display tests
//! (`crates/ui/tests`) and the E2E suite (`crates/e2e`). See Cargo.toml for
//! why this is its own crate rather than part of `shared`.

pub mod hwnd;
pub mod process;
pub mod uia;
pub mod win_events;
pub mod windows_enum;

use std::time::{Duration, Instant};

pub use hwnd::Hwnd;

/// Titles the overlay windows are created with, from
/// `crates/ui/src/{candidate,indicator}.rs`. Declared here so both harnesses
/// match on the same strings; the ui source remains the origin.
pub const CANDIDATE_TITLE: &str = "CandidateList";
pub const INDICATOR_TITLE: &str = "Indicator";

/// Waits for `probe` to produce a value, polling on `tick` instead of
/// sleeping a fixed amount.
///
/// Everything these harnesses observe is asynchronous — RPC to event loop to
/// win32 to webview, or keystroke to TIP to engine to host application to the
/// accessibility tree — so a fixed sleep either flakes or wastes the
/// difference. Deliberately no retry around a whole scenario: a flake a retry
/// hides stops being a substitute for the manual checklist.
pub fn poll_until_tick<T>(
    timeout: Duration,
    tick: Duration,
    mut probe: impl FnMut() -> Option<T>,
) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(tick);
    }
}

/// [`poll_until_tick`] with the default 25 ms tick.
pub fn poll_until<T>(timeout: Duration, probe: impl FnMut() -> Option<T>) -> Option<T> {
    poll_until_tick(timeout, Duration::from_millis(25), probe)
}

/// [`poll_until`] with a message instead of an `Option`.
pub fn wait_for(timeout: Duration, what: &str, mut condition: impl FnMut() -> bool) {
    if poll_until(timeout, || condition().then_some(())).is_none() {
        panic!("timed out after {timeout:?} waiting for {what}");
    }
}
