//! Launching a host application and getting the caret into it.
//!
//! Always a *fresh* process. A profile set as the ja-JP default is only
//! picked up by applications started after the change — the manual Phase 0
//! spike confirmed this — so reusing a running window would test whichever
//! IME it happened to load with.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, MAX_PATH};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    SW_RESTORE, SetForegroundWindow, ShowWindow,
};
use windows::core::{BOOL, PWSTR};

use crate::poll_until;

const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30);
const FOCUS_TIMEOUT: Duration = Duration::from_secs(10);

/// A host application under test, killed when the harness is done with it.
pub struct HostApp {
    child: std::process::Child,
    image: String,
    pub window: HWND,
}

impl HostApp {
    /// Starts `exe` and waits for a new top-level window belonging to a
    /// process called `image` (e.g. `notepad.exe`).
    ///
    /// The window is matched on the image name rather than on the pid we
    /// spawned, because Windows 11 ships Notepad as a packaged application:
    /// `System32\notepad.exe` can hand off to another process, leaving the
    /// pid we hold owning no window at all.
    pub fn launch(exe: &str, image: &str) -> Result<Self> {
        let before = windows_of_image(image);

        let child = std::process::Command::new(exe)
            .spawn()
            .with_context(|| format!("failed to start {exe}"))?;

        let window = poll_until(LAUNCH_TIMEOUT, || {
            windows_of_image(image)
                .into_iter()
                .find(|hwnd| !before.contains(hwnd))
        });

        let Some(window) = window else {
            bail!("no new {image} window appeared within {LAUNCH_TIMEOUT:?}");
        };
        println!("host window: {:?} ({image})", window.0);

        Ok(Self {
            child,
            image: image.to_string(),
            window: HWND(window.0 as *mut std::ffi::c_void),
        })
    }

    /// Brings the window to the foreground and waits until the system agrees
    /// it is there. Keystrokes go to the foreground window, so this failing
    /// silently would look like "the IME dropped my input".
    pub fn focus(&self) -> Result<()> {
        unsafe {
            let _ = ShowWindow(self.window, SW_RESTORE);
            let _ = SetForegroundWindow(self.window);
        }

        let target = self.window.0 as isize;
        let ok = poll_until(FOCUS_TIMEOUT, || {
            (unsafe { GetForegroundWindow() }.0 as isize == target).then_some(())
        });

        if ok.is_none() {
            bail!(
                "{} never reached the foreground — something else is holding it \
                 (a notification, or a more privileged window)",
                self.image
            );
        }
        Ok(())
    }
}

impl Drop for HostApp {
    fn drop(&mut self) {
        // killed rather than closed: a close would raise the unsaved-changes
        // prompt, and the harness has no business clicking dialogs
        let _ = self.child.kill();
        let _ = self.child.wait();
        // the packaged-application case: the window's owner is not the child
        // we spawned, so take it down by image name too
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/IM", &self.image])
            .output();
    }
}

/// Handles of visible, titled, top-level windows owned by a process with this
/// image name.
fn windows_of_image(image: &str) -> HashSet<HwndKey> {
    let mut found = Search {
        image: image.to_ascii_lowercase(),
        found: HashSet::new(),
    };
    let _ = unsafe { EnumWindows(Some(enum_proc), LPARAM(&mut found as *mut Search as isize)) };
    found.found
}

/// A window handle usable as a set key (`HWND` is a raw pointer).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct HwndKey(pub isize);

impl HwndKey {
    fn from(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }
}

struct Search {
    image: String,
    found: HashSet<HwndKey>,
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let search = unsafe { &mut *(lparam.0 as *mut Search) };

    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return BOOL(1);
    }
    // an untitled window is a helper (tooltips, IME overlays), not the
    // application's document window
    let mut title = [0u16; 8];
    if unsafe { GetWindowTextW(hwnd, &mut title) } == 0 {
        return BOOL(1);
    }

    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if image_name_of(pid).is_some_and(|name| name == search.image) {
        search.found.insert(HwndKey::from(hwnd));
    }
    BOOL(1)
}

fn image_name_of(pid: u32) -> Option<String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;

        let mut buffer = [0u16; MAX_PATH as usize];
        let mut len = buffer.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            Default::default(),
            PWSTR(buffer.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        ok.ok()?;

        let path = String::from_utf16_lossy(&buffer[..len as usize]);
        Some(path.rsplit(['\\', '/']).next()?.to_ascii_lowercase())
    }
}
