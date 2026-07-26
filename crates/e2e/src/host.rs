//! Launching a host application and getting the caret into it.
//!
//! Always a *fresh* process. A profile set as the ja-JP default is only
//! picked up by applications started after the change — the manual Phase 0
//! spike confirmed this — so reusing a running window would test whichever
//! IME it happened to load with.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use test_support::{
    Hwnd,
    process::image_name_of,
    windows_enum::{Walk, enumerate_windows, window_pid, window_title},
};
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, SW_RESTORE, SetForegroundWindow, ShowWindow,
};

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
            window: window.raw(),
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
fn windows_of_image(image: &str) -> HashSet<Hwnd> {
    let image = image.to_ascii_lowercase();
    let mut found = HashSet::new();
    enumerate_windows(|window| {
        // an untitled window is a helper (tooltips, IME overlays), not the
        // application's document window
        if window.is_visible()
            && !window_title(window).is_empty()
            && image_name_of(window_pid(window)).is_some_and(|name| name == image)
        {
            found.insert(window);
        }
        Walk::Continue
    });
    found
}
