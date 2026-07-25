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
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use shared::proto::window_service_client::WindowServiceClient;
use tonic::transport::Channel;
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, HWINEVENTHOOK, IUIAutomation, IUIAutomationElement,
    IUIAutomationSelectionItemPattern, SetWinEventHook, TreeScope_Descendants, UIA_CONTROLTYPE_ID,
    UIA_ListControlTypeId, UIA_ListItemControlTypeId, UIA_SelectionItemPatternId, UnhookWinEvent,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EVENT_OBJECT_IME_CHANGE, EVENT_OBJECT_IME_HIDE, EVENT_OBJECT_IME_SHOW,
    EnumWindows, GWL_EXSTYLE, GetMessageW, GetWindowLongW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, MSG, PostThreadMessageW, TranslateMessage,
    WINEVENT_OUTOFCONTEXT, WM_QUIT,
};

/// How long anything the spawned process does asynchronously gets before the
/// test calls it a failure. Generous: the first WebView2 start on a cold
/// machine is the slow case, and everything here polls rather than sleeps, so
/// a high ceiling costs nothing when things work.
pub const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a single on-screen effect (a show, a resize, a repaint) gets.
pub const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

const CANDIDATE_TITLE: &str = "CandidateList";
const INDICATOR_TITLE: &str = "Indicator";

// ---------------------------------------------------------------------------
// polling
// ---------------------------------------------------------------------------

/// Waits for `probe` to produce a value, polling instead of sleeping a fixed
/// amount: every effect here is asynchronous (RPC → event loop → win32 →
/// webview), and a fixed sleep either flakes or wastes the difference.
pub fn poll_until<T>(timeout: Duration, mut probe: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// `poll_until` with a message instead of an `Option`.
pub fn wait_for(timeout: Duration, what: &str, mut condition: impl FnMut() -> bool) {
    if poll_until(timeout, || condition().then_some(())).is_none() {
        panic!("timed out after {timeout:?} waiting for {what}");
    }
}

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

/// A window handle, carried as an integer so it can cross threads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Hwnd(pub isize);

impl Hwnd {
    fn raw(self) -> HWND {
        HWND(self.0 as *mut std::ffi::c_void)
    }

    pub fn is_visible(self) -> bool {
        unsafe { IsWindowVisible(self.raw()) }.as_bool()
    }

    pub fn rect(self) -> RECT {
        let mut rect = RECT::default();
        unsafe { GetWindowRect(self.raw(), &mut rect) }.expect("GetWindowRect failed");
        rect
    }

    pub fn width(self) -> i32 {
        let rect = self.rect();
        rect.right - rect.left
    }

    pub fn height(self) -> i32 {
        let rect = self.rect();
        rect.bottom - rect.top
    }

    pub fn extended_style(self) -> u32 {
        unsafe { GetWindowLongW(self.raw(), GWL_EXSTYLE) as u32 }
    }

    /// The window's width in CSS px — the unit every sizing decision in
    /// `utils` is expressed in. Sizes are applied as `LogicalSize` precisely
    /// so the physical result follows the monitor's scale factor, so a test
    /// that asserted physical px would only hold at 100%.
    pub fn logical_width(self) -> f64 {
        f64::from(self.width()) * 96.0 / f64::from(self.dpi())
    }

    /// The window's height in CSS px, for the same reason as
    /// [`Self::logical_width`] — the height the webview reports is a count of
    /// rows plus chrome, all in CSS px.
    pub fn logical_height(self) -> f64 {
        f64::from(self.height()) * 96.0 / f64::from(self.dpi())
    }

    pub fn dpi(self) -> u32 {
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
        let found = poll_until(READY_TIMEOUT, || find_window(title, &self.pids));
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
// window lookup
// ---------------------------------------------------------------------------

struct Search<'a> {
    title: &'a str,
    pids: &'a HashSet<u32>,
    found: Option<Hwnd>,
}

/// The first top-level window with this title owned by one of `pids`. The
/// titles are fixed strings the installed ui.exe uses too, hence the pid
/// filter.
fn find_window(title: &str, pids: &HashSet<u32>) -> Option<Hwnd> {
    let mut search = Search {
        title,
        pids,
        found: None,
    };
    // EnumWindows returns Err when the callback stops the enumeration, which
    // is exactly what a hit does here — the result is in `search`.
    let _ = unsafe {
        EnumWindows(
            Some(enum_windows_proc),
            LPARAM(&mut search as *mut Search as isize),
        )
    };
    search.found
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
    let search = unsafe { &mut *(lparam.0 as *mut Search) };

    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if !search.pids.contains(&pid) {
        return windows::core::BOOL(1);
    }

    let mut text = [0u16; 64];
    let len = unsafe { GetWindowTextW(hwnd, &mut text) };
    if String::from_utf16_lossy(&text[..len as usize]) != search.title {
        return windows::core::BOOL(1);
    }

    search.found = Some(Hwnd(hwnd.0 as isize));
    windows::core::BOOL(0) // stop enumerating
}

/// `root` and every process descended from it.
fn process_tree(root: u32) -> HashSet<u32> {
    let mut parents: Vec<(u32, u32)> = Vec::new();

    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return HashSet::from([root]);
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                parents.push((entry.th32ProcessID, entry.th32ParentProcessID));
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = windows::Win32::Foundation::CloseHandle(snapshot);
    }

    let mut tree = HashSet::from([root]);
    // one pass per generation; the tree is two deep at most (shim -> ui)
    for _ in 0..4 {
        let before = tree.len();
        for (pid, parent) in &parents {
            if tree.contains(parent) {
                tree.insert(*pid);
            }
        }
        if tree.len() == before {
            break;
        }
    }
    tree
}

// ---------------------------------------------------------------------------
// WinEvent recording
// ---------------------------------------------------------------------------

/// The IME notifications the candidate window announces, in the order they
/// arrived. Global because a `WINEVENTPROC` gets no context pointer; entries
/// carry the hwnd so several harnesses can share it.
static EVENTS: Mutex<Vec<(u32, isize)>> = Mutex::new(Vec::new());

/// One IME notification.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImeEvent {
    Show,
    Hide,
    Change,
}

/// Installs the out-of-context hook (once per test binary) on a thread of its
/// own with a message loop, which is how `WINEVENT_OUTOFCONTEXT` callbacks are
/// delivered.
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
                // every process: the events are announced by the ui.exe under
                // test, whose pid is not known when the hook goes in, and the
                // log is filtered by hwnd anyway
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
    /// this mark", so the tests do not depend on each other or on the live
    /// IME's own notifications.
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
    pub fn expect_exactly(&self, mark: usize, window: Hwnd, expected: &[ImeEvent]) {
        let got = poll_until(SETTLE_TIMEOUT, || {
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

// ---------------------------------------------------------------------------
// UI Automation read-back
// ---------------------------------------------------------------------------

/// Reads the candidate window's contents the way a screen reader would.
///
/// This is the point of the whole file: the placement and escaping decisions
/// have unit tests already, but nothing proved the candidates ever reach the
/// webview, or that they arrive as an accessible list.
pub struct Uia {
    automation: IUIAutomation,
}

impl Uia {
    pub fn new() -> Self {
        unsafe {
            // MTA: this thread does not pump messages, and UIA is happy to be
            // called from a multithreaded apartment. Already-initialised is
            // not an error here.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let automation: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_ALL)
                .expect("failed to create the UI Automation client");
            Self { automation }
        }
    }

    fn descendants(&self, window: Hwnd) -> Vec<IUIAutomationElement> {
        unsafe {
            let Ok(root) = self.automation.ElementFromHandle(window.raw()) else {
                return Vec::new();
            };
            let Ok(condition) = self.automation.CreateTrueCondition() else {
                return Vec::new();
            };
            let Ok(found) = root.FindAll(TreeScope_Descendants, &condition) else {
                return Vec::new();
            };
            let count = found.Length().unwrap_or(0);
            (0..count)
                .filter_map(|i| found.GetElement(i).ok())
                .collect()
        }
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
