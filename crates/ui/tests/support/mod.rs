//! Harness for the ui.exe display tests: starts a real, isolated candidate
//! window process, drives it over its own `WindowService` pipe, and reads
//! back what actually ended up on screen.
//!
//! Three things make this safe to run on a working machine, next to the
//! installed IME:
//!
//! * its own pipe name (`--pipe-base`), because the listener asks for the
//!   first pipe instance and a second server on the real name would fail;
//! * its own `LOCALAPPDATA`, so the spawned process gets a private WebView2
//!   user data folder instead of fighting the live one for it (issue #54);
//! * window lookup restricted to the spawned process tree — the overlay
//!   windows have fixed titles, which the installed ui.exe uses too.
//!
//! Nothing here registers the TIP or synthesises keystrokes; that is Tier 2
//! in `docs/e2e-automation-plan.md` and belongs in a VM.

#![allow(dead_code)] // each test uses a subset of the harness

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use shared::proto::window_service_client::WindowServiceClient;
// The Win32 observation layer is shared with the E2E harness; only what is
// specific to driving an isolated ui.exe lives here.
pub use test_support::win_events::{ImeEvent, ImeEventLog, ime_events};
use test_support::{
    CANDIDATE_TITLE, INDICATOR_TITLE,
    process::process_tree,
    uia::{Apartment, UiaBase},
    windows_enum::find_window,
};
pub use test_support::{Hwnd, poll_until, wait_for};
use tonic::transport::Channel;
use windows::Win32::Foundation::RECT;
use windows::Win32::UI::Accessibility::{
    IUIAutomationElement, IUIAutomationSelectionItemPattern, UIA_CONTROLTYPE_ID,
    UIA_ListControlTypeId, UIA_ListItemControlTypeId, UIA_SelectionItemPatternId,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{GWL_EXSTYLE, GetWindowLongW};

/// How long anything the spawned process does asynchronously gets before the
/// test calls it a failure. Generous: the first WebView2 start on a cold
/// machine is the slow case, and everything here polls rather than sleeps, so
/// a high ceiling costs nothing when things work.
pub const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a single on-screen effect (a show, a resize, a repaint) gets.
pub const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

// ---------------------------------------------------------------------------
// the process under test
// ---------------------------------------------------------------------------

/// A running ui.exe with its own pipe, profile and log files.
pub struct UiProcess {
    child: std::process::Child,
    pipe_base: String,
    work_dir: PathBuf,
    /// Every pid in the spawned tree. UIAccess makes ui.exe re-exec itself,
    /// leaving the process we spawned behind as a supervision shim, so the
    /// windows can belong to a grandchild.
    pids: HashSet<u32>,
    pub candidate: Hwnd,
    pub indicator: Hwnd,
}

/// The measurements the display tests take of a window, on top of
/// `test_support::Hwnd`. An extension trait rather than a wrapper so the
/// shared helpers keep taking the shared type.
pub trait HwndExt {
    fn width(self) -> i32;
    fn height(self) -> i32;
    fn extended_style(self) -> u32;
    fn logical_width(self) -> f64;
    fn logical_height(self) -> f64;
    fn dpi(self) -> u32;
}

impl HwndExt for Hwnd {
    fn width(self) -> i32 {
        let rect = self.rect();
        rect.right - rect.left
    }

    fn height(self) -> i32 {
        let rect = self.rect();
        rect.bottom - rect.top
    }

    fn extended_style(self) -> u32 {
        unsafe { GetWindowLongW(self.raw(), GWL_EXSTYLE) as u32 }
    }

    /// The window's width in CSS px — the unit every sizing decision in
    /// `geometry` is expressed in. Sizes are applied as `LogicalSize`
    /// precisely so the physical result follows the monitor's scale factor,
    /// so a test that asserted physical px would only hold at 100%.
    fn logical_width(self) -> f64 {
        f64::from(self.width()) * 96.0 / f64::from(self.dpi())
    }

    /// The window's height in CSS px, for the same reason as
    /// [`Self::logical_width`] — the height the webview reports is a count of
    /// rows plus chrome, all in CSS px.
    fn logical_height(self) -> f64 {
        f64::from(self.height()) * 96.0 / f64::from(self.dpi())
    }

    fn dpi(self) -> u32 {
        match unsafe { GetDpiForWindow(self.raw()) } {
            0 => 96, // an invalid window; the caller's assertion will say so
            dpi => dpi,
        }
    }
}

impl UiProcess {
    /// Starts ui.exe and waits until it serves its pipe and both overlay
    /// windows exist.
    pub async fn start() -> Self {
        become_dpi_aware();
        let exe = ui_exe_path();
        let id = next_instance_id();
        let pipe_base = format!("azookey_ui_test_{}_{id}", std::process::id());
        let work_dir = std::env::temp_dir()
            .join("azookey-ui-tests")
            .join(&pipe_base);
        std::fs::create_dir_all(&work_dir).expect("failed to create the test work directory");

        let stdout = std::fs::File::create(work_dir.join("stdout.log"))
            .expect("failed to create the stdout log");
        let stderr = std::fs::File::create(work_dir.join("stderr.log"))
            .expect("failed to create the stderr log");

        let child = std::process::Command::new(&exe)
            .arg("--pipe-base")
            .arg(&pipe_base)
            // its own WebView2 user data folder: `webview2_data_dir` resolves
            // under LOCALAPPDATA, and sharing the live IME's profile is asking
            // for an environment-creation failure (issue #54)
            .env("LOCALAPPDATA", &work_dir)
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
            .unwrap_or_else(|e| panic!("failed to start {}: {e}", exe.display()));

        let mut process = Self {
            child,
            pipe_base,
            work_dir,
            pids: HashSet::new(),
            candidate: Hwnd(0),
            indicator: Hwnd(0),
        };

        process.wait_until_serving().await;
        process.pids = process_tree(process.child.id());

        let candidate = process.wait_for_window(CANDIDATE_TITLE);
        let indicator = process.wait_for_window(INDICATOR_TITLE);
        process.candidate = candidate;
        process.indicator = indicator;

        process
    }

    /// A client on this instance's pipe. Each call is a fresh connection, so
    /// dropping one exercises the same disconnect path a dying host app takes.
    pub fn client(&self) -> WindowServiceClient<Channel> {
        WindowServiceClient::new(self.channel())
    }

    fn channel(&self) -> Channel {
        shared::pipe::lazy_pipe_channel(format!(r"\\.\pipe\{}", self.pipe_base))
            .expect("failed to build the pipe channel")
    }

    async fn wait_until_serving(&mut self) {
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut health = tonic_health::pb::health_client::HealthClient::new(self.channel().clone());

        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait failed") {
                panic!(
                    "ui.exe exited with {status} before it served its pipe\n{}",
                    self.captured_output()
                );
            }

            let probe = tokio::time::timeout(
                Duration::from_millis(500),
                health.check(tonic_health::pb::HealthCheckRequest {
                    service: String::new(),
                }),
            )
            .await;

            if let Ok(Ok(response)) = probe
                && response.into_inner().status
                    == tonic_health::pb::health_check_response::ServingStatus::Serving as i32
            {
                return;
            }

            if Instant::now() >= deadline {
                panic!(
                    "ui.exe did not serve its pipe within {READY_TIMEOUT:?}\n{}",
                    self.captured_output()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn wait_for_window(&self, title: &str) -> Hwnd {
        let found = poll_until(READY_TIMEOUT, || {
            find_window(title, |pid| self.pids.contains(&pid))
        });
        match found {
            Some(hwnd) => hwnd,
            None => panic!(
                "no window titled {title:?} appeared in the ui.exe process tree within \
                 {READY_TIMEOUT:?}\n{}",
                self.captured_output()
            ),
        }
    }

    /// Whatever the process wrote to stdout/stderr so far, for failure
    /// messages: a WebView2 or UIAccess problem is reported there and nowhere
    /// else.
    pub fn captured_output(&self) -> String {
        let read = |name: &str| {
            std::fs::read_to_string(self.work_dir.join(name)).unwrap_or_else(|e| format!("<{e}>"))
        };
        format!(
            "--- ui.exe stdout ---\n{}--- ui.exe stderr ---\n{}",
            read("stdout.log"),
            read("stderr.log")
        )
    }
}

impl Drop for UiProcess {
    fn drop(&mut self) {
        // ui.exe ignores WM_CLOSE by design (its lifecycle belongs to the
        // launcher), so killing is the only way out. The UIAccess re-exec ties
        // the real window process to a job object that dies with the shim.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.work_dir);
    }
}

/// The process under test plus everything needed to drive and observe it.
pub struct Ui {
    pub process: UiProcess,
    client: WindowServiceClient<Channel>,
    pub uia: Uia,
    pub events: &'static ImeEventLog,
}

impl Ui {
    pub async fn start() -> Self {
        let process = UiProcess::start().await;
        let client = process.client();
        Self {
            process,
            client,
            uia: Uia::new(),
            events: ime_events(),
        }
    }

    pub fn candidate(&self) -> Hwnd {
        self.process.candidate
    }

    pub fn indicator(&self) -> Hwnd {
        self.process.indicator
    }

    pub async fn show(&mut self) {
        self.client
            .show_window(shared::proto::EmptyResponse {})
            .await
            .expect("ShowWindow failed");
    }

    pub async fn hide(&mut self) {
        self.client
            .hide_window(shared::proto::EmptyResponse {})
            .await
            .expect("HideWindow failed");
    }

    pub async fn set_candidates(&mut self, candidates: &[&str]) {
        self.client
            .set_candidate(shared::proto::SetCandidateRequest {
                candidates: candidates.iter().map(|s| s.to_string()).collect(),
            })
            .await
            .expect("SetCandidate failed");
    }

    pub async fn select(&mut self, index: i32) {
        self.client
            .set_selection(shared::proto::SetSelectionRequest { index })
            .await
            .expect("SetSelection failed");
    }

    pub async fn set_position(&mut self, caret: RECT) {
        self.client
            .set_window_position(shared::proto::SetPositionRequest {
                position: Some(shared::proto::WindowPosition {
                    top: caret.top,
                    left: caret.left,
                    bottom: caret.bottom,
                    right: caret.right,
                }),
            })
            .await
            .expect("SetWindowPosition failed");
    }

    pub async fn set_input_mode(&mut self, mode: &str) {
        self.client
            .set_input_mode(shared::proto::SetInputModeRequest {
                mode: mode.to_string(),
            })
            .await
            .expect("SetInputMode failed");
    }

    /// Puts the candidate window on screen at `caret` and waits until it is.
    /// Both placement gates (a fresh position, a measured height) have to be
    /// satisfied before a `Show` is honoured, so this is the sequence every
    /// content test starts from.
    pub async fn show_at(&mut self, caret: RECT) {
        let mark = self.events.mark();
        self.set_position(caret).await;
        self.show().await;
        wait_for(SETTLE_TIMEOUT, "the candidate window to appear", || {
            self.candidate().is_visible()
        });
        // ...and until the transition has been announced. The window is made
        // visible first and announced immediately after, so a mark taken on
        // visibility alone would land between the two and attribute this
        // show's notification to whatever the test does next.
        wait_for(SETTLE_TIMEOUT, "the show to be announced", || {
            self.events
                .since(mark, self.candidate())
                .contains(&ImeEvent::Show)
        });
    }

    /// Takes the candidate window down and waits until it is gone *and* the
    /// transition has been announced — the mirror of `show_at`, and for the
    /// same reason.
    pub async fn hide_and_wait(&mut self) {
        let mark = self.events.mark();
        self.hide().await;
        wait_for(SETTLE_TIMEOUT, "the candidate window to disappear", || {
            !self.candidate().is_visible()
        });
        wait_for(SETTLE_TIMEOUT, "the hide to be announced", || {
            self.events
                .since(mark, self.candidate())
                .contains(&ImeEvent::Hide)
        });
    }

    /// The candidate window sized for a real list. Until the first
    /// `SetCandidate` the window still has tao's 800x600 default, which is
    /// wide enough to be clamped away from the caret on any normal monitor —
    /// so a geometry assertion has to start from a sized window, exactly as
    /// the TIP does (it sends the candidates before it shows anything).
    pub async fn sized(&mut self) {
        self.set_candidates(&["水"]).await;
        // 225 is MIN_CANDIDATE_WINDOW_WIDTH in src/geometry.rs, spelled out
        // because `ui` is a bin crate and a test cannot import from it
        wait_for(SETTLE_TIMEOUT, "the window to take its list width", || {
            (self.candidate().logical_width() - 225.0).abs() < 2.0
        });
    }

    /// Waits for the mode indicator to render `mode`, re-sending it as it
    /// polls.
    ///
    /// The flash lasts half a second by design, which is not long enough to
    /// rely on for a read-back: WebView2 builds its accessibility tree lazily
    /// and a hidden window has nothing to read. Each `SetInputMode` restarts
    /// the flash, so re-sending keeps the window up for exactly as long as the
    /// read needs — and asserts the same path the TIP uses.
    pub async fn indicator_shows(&mut self, mode: &str) -> Vec<String> {
        let glyph = mode.chars().next().expect("an empty mode shows nothing");
        let deadline = std::time::Instant::now() + SETTLE_TIMEOUT;
        loop {
            self.set_input_mode(mode).await;
            let names = self.uia.all_names(self.indicator());
            if names.iter().any(|name| name.contains(glyph)) {
                return names;
            }
            if std::time::Instant::now() >= deadline {
                panic!("the indicator never rendered {mode:?}; last read was {names:?}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Nothing in the process may have failed along the way: a broken
    /// `evaluate_script` or a panic still leaves the window standing, so the
    /// on-screen assertions alone would not notice.
    pub fn assert_no_errors(&self) {
        let output = self.process.captured_output();
        for symptom in [
            "panicked at",
            "evaluate_script failed",
            "gRPC server terminated",
            "Failed to lock indicator_flash",
        ] {
            assert!(
                !output.contains(symptom),
                "ui.exe reported {symptom:?}\n{output}"
            );
        }
    }
}

/// Opts the test process into per-monitor DPI awareness, once.
///
/// Without it Windows virtualises every coordinate this process reads: on a
/// 150% display `GetWindowRect` reports a 337px window as 225px and the work
/// area comes back shrunk to match, so the geometry assertions would compare
/// scaled-down measurements against the physical rect the TIP actually sends.
/// ui.exe itself is PerMonitorV2 (issue #30); the observer has to be too.
fn become_dpi_aware() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // fails only if awareness was already set, which is just as good
        let _ =
            unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    });
}

fn next_instance_id() -> u32 {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// Where the ui.exe under test lives: `AZOOKEY_UI_EXE` if set, otherwise the
/// one this test binary was built alongside (`target/<profile>/ui.exe`), which
/// is the build the developer just made.
fn ui_exe_path() -> PathBuf {
    if let Some(explicit) = std::env::var_os("AZOOKEY_UI_EXE") {
        let path = PathBuf::from(explicit);
        assert!(path.is_file(), "AZOOKEY_UI_EXE is not a file: {path:?}");
        return path;
    }

    // .../target/<profile>/deps/window_display-<hash>.exe
    let test_exe = std::env::current_exe().expect("current_exe failed");
    let profile_dir = test_exe
        .parent()
        .and_then(Path::parent)
        .expect("unexpected test binary location");
    let path = profile_dir.join("ui.exe");
    assert!(
        path.is_file(),
        "ui.exe not found at {path:?} — build it first (cargo build -p ui) or point \
         AZOOKEY_UI_EXE at one"
    );
    path
}

// ---------------------------------------------------------------------------
// UI Automation read-back
// ---------------------------------------------------------------------------

/// Reads the candidate window's contents the way a screen reader would.
///
/// This is the point of the whole file: the placement and escaping decisions
/// have unit tests already, but nothing proved the candidates ever reach the
/// webview, or that they arrive as an accessible list.
pub struct Uia {
    base: UiaBase,
}

impl Uia {
    pub fn new() -> Self {
        // MTA: this thread does not pump messages, and UIA is happy to be
        // called from a multithreaded apartment. (The E2E harness inherits
        // main's STA instead — hence the explicit choice.)
        let base = UiaBase::new(Apartment::InitMultiThreaded)
            .expect("failed to create the UI Automation client");
        Self { base }
    }

    fn descendants(&self, window: Hwnd) -> Vec<IUIAutomationElement> {
        self.base.descendants(window)
    }

    fn elements_of_type(&self, window: Hwnd, control_type: UIA_CONTROLTYPE_ID) -> Vec<Element> {
        self.descendants(window)
            .into_iter()
            .filter(|element| {
                unsafe { element.CurrentControlType() }.is_ok_and(|kind| kind == control_type)
            })
            .map(Element)
            .collect()
    }

    /// The candidate strings currently rendered, top to bottom.
    pub fn candidates(&self, window: Hwnd) -> Vec<String> {
        self.elements_of_type(window, UIA_ListItemControlTypeId)
            .iter()
            .map(Element::name)
            .collect()
    }

    /// Index of the highlighted candidate, from the accessible selection
    /// state (`aria-selected`) rather than from the pixels.
    pub fn selected_index(&self, window: Hwnd) -> Option<usize> {
        self.elements_of_type(window, UIA_ListItemControlTypeId)
            .iter()
            .position(Element::is_selected)
    }

    /// The name of the list itself — the label a screen reader announces.
    pub fn list_label(&self, window: Hwnd) -> Option<String> {
        self.elements_of_type(window, UIA_ListControlTypeId)
            .first()
            .map(Element::name)
    }

    /// Every name in the window's tree, for windows whose content is not a
    /// list (the mode indicator).
    pub fn all_names(&self, window: Hwnd) -> Vec<String> {
        self.descendants(window)
            .into_iter()
            .map(|element| Element(element).name())
            .filter(|name| !name.is_empty())
            .collect()
    }

    /// Waits for the read-back to satisfy `condition`. WebView2 builds its
    /// accessibility tree lazily and the script that fills the list runs
    /// asynchronously, so the first read after an RPC is routinely empty.
    pub fn wait_for<T: std::fmt::Debug>(
        &self,
        what: &str,
        mut read: impl FnMut(&Self) -> T,
        mut condition: impl FnMut(&T) -> bool,
    ) -> T {
        let mut last = None;
        let found = poll_until(SETTLE_TIMEOUT, || {
            let value = read(self);
            let ok = condition(&value);
            last = Some(value);
            ok.then(|| last.take().expect("just set"))
        });
        match found {
            Some(value) => value,
            None => panic!("timed out waiting for {what}; last read was {last:?}"),
        }
    }
}

struct Element(IUIAutomationElement);

impl Element {
    fn name(&self) -> String {
        unsafe { self.0.CurrentName() }
            .map(|name| name.to_string())
            .unwrap_or_default()
    }

    fn is_selected(&self) -> bool {
        unsafe {
            self.0
                .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                    UIA_SelectionItemPatternId,
                )
                .and_then(|pattern| pattern.CurrentIsSelected())
                .is_ok_and(|selected| selected.as_bool())
        }
    }
}
