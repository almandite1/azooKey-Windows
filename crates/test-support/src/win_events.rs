//! Recording the IME notifications the candidate window announces.
//!
//! `ui.exe` calls `NotifyWinEvent` with `EVENT_OBJECT_IME_SHOW` / `_HIDE` /
//! `_CHANGE` so the shell and assistive technology can tell that the IME UI
//! appeared, moved or went away — a Windows IME requirement, since the window
//! is a `WS_EX_NOACTIVATE` tool window in another process and nothing else
//! would notice it. The contract is one announcement per real transition; a
//! regression that announced every request buried listeners in duplicates.
//!
//! The hook is `WINEVENT_OUTOFCONTEXT`, so its callbacks arrive on a thread
//! that pumps messages — hence the dedicated thread below.
//!
//! Both assertion verbs the two harnesses grew are kept: [`expect_exactly`]
//! for the display tests, which own the window under test and can demand an
//! exact sequence, and [`wait_for_event`] for the E2E suite, which watches a
//! live IME and can only ask whether something happened.
//!
//! [`expect_exactly`]: ImeEventLog::expect_exactly
//! [`wait_for_event`]: ImeEventLog::wait_for_event

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook, UnhookWinEvent};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EVENT_OBJECT_IME_CHANGE, EVENT_OBJECT_IME_HIDE, EVENT_OBJECT_IME_SHOW,
    GetMessageW, MSG, PostThreadMessageW, TranslateMessage, WINEVENT_OUTOFCONTEXT, WM_QUIT,
};

use crate::{Hwnd, poll_until};

/// Every notification seen, in order, with the window that announced it.
/// Global because a `WINEVENTPROC` gets no context pointer; entries carry the
/// hwnd so several harnesses can share one hook.
static EVENTS: Mutex<Vec<(u32, isize)>> = Mutex::new(Vec::new());

/// One IME notification.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImeEvent {
    Show,
    Hide,
    Change,
}

/// Installs the hook (once per process) and returns the log.
pub fn ime_events() -> &'static ImeEventLog {
    static LOG: OnceLock<ImeEventLog> = OnceLock::new();
    LOG.get_or_init(|| {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || unsafe {
            let hook = SetWinEventHook(
                EVENT_OBJECT_IME_SHOW,
                EVENT_OBJECT_IME_CHANGE,
                None,
                Some(win_event_proc),
                // every process: the announcements come from a ui.exe whose
                // pid is not known when the hook goes in, and entries are
                // filtered by window afterwards
                0,
                0,
                WINEVENT_OUTOFCONTEXT,
            );
            let _ = ready_tx.send(GetCurrentThreadId());

            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&message);
                DispatchMessageW(&message);
            }
            let _ = UnhookWinEvent(hook);
        });

        ImeEventLog {
            thread: ready_rx
                .recv()
                .expect("the WinEvent hook thread died on startup"),
        }
    })
}

pub struct ImeEventLog {
    thread: u32,
}

impl ImeEventLog {
    /// A cursor into the log. Everything asserted on is "what happened since
    /// this mark", so tests do not inherit each other's events — or the live
    /// IME's.
    pub fn mark(&self) -> usize {
        EVENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// The notifications `window` announced after `mark`.
    pub fn since(&self, mark: usize, window: Hwnd) -> Vec<ImeEvent> {
        EVENTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .skip(mark)
            .filter(|(_, hwnd)| *hwnd == window.0)
            .filter_map(|(event, _)| match *event {
                EVENT_OBJECT_IME_SHOW => Some(ImeEvent::Show),
                EVENT_OBJECT_IME_HIDE => Some(ImeEvent::Hide),
                EVENT_OBJECT_IME_CHANGE => Some(ImeEvent::Change),
                _ => None,
            })
            .collect()
    }

    /// Waits until `window` has announced `expected` since `mark` — and then
    /// keeps watching briefly, so a *second* notification for the same
    /// transition still fails the assertion.
    pub fn expect_exactly(
        &self,
        mark: usize,
        window: Hwnd,
        expected: &[ImeEvent],
        timeout: Duration,
    ) {
        let got = poll_until(timeout, || {
            let got = self.since(mark, window);
            (got.len() >= expected.len()).then_some(got)
        });
        let Some(got) = got else {
            panic!(
                "timed out waiting for {expected:?}; saw {:?}",
                self.since(mark, window)
            );
        };
        assert_eq!(got, expected, "unexpected IME notifications");

        // a duplicate arrives right behind the real one if it arrives at all
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(
            self.since(mark, window),
            expected,
            "a transition was announced more than once"
        );
    }

    /// Waits (up to `timeout`) for `window` to have announced `event` at
    /// least once since `mark`, and returns everything seen.
    pub fn wait_for_event(
        &self,
        mark: usize,
        window: Hwnd,
        event: ImeEvent,
        timeout: Duration,
    ) -> Vec<ImeEvent> {
        poll_until(timeout, || {
            let seen = self.since(mark, window);
            seen.contains(&event).then_some(seen)
        })
        .unwrap_or_else(|| self.since(mark, window))
    }
}

impl Drop for ImeEventLog {
    fn drop(&mut self) {
        let _ = unsafe { PostThreadMessageW(self.thread, WM_QUIT, WPARAM(0), LPARAM(0)) };
    }
}

unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    _object_id: i32,
    _child_id: i32,
    _thread: u32,
    _time: u32,
) {
    EVENTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push((event, hwnd.0 as isize));
}
