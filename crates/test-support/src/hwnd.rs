//! A window handle that can cross threads and live in a set.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::{GetWindowRect, IsWindowVisible};

/// A window handle carried as an integer.
///
/// `HWND` is a raw pointer, so it is neither `Send` nor hashable; a harness
/// that records a window in one thread and asserts on it in another (which
/// both of them do) needs this. Both harnesses had grown their own copy.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Hwnd(pub isize);

impl Hwnd {
    pub fn raw(self) -> HWND {
        HWND(self.0 as *mut std::ffi::c_void)
    }

    pub fn is_visible(self) -> bool {
        unsafe { IsWindowVisible(self.raw()) }.as_bool()
    }

    /// The window rect in screen coordinates, or a zero rect when the window
    /// is gone — a harness asserting on geometry has already established the
    /// window exists.
    pub fn rect(self) -> RECT {
        let mut rect = RECT::default();
        let _ = unsafe { GetWindowRect(self.raw(), &mut rect) };
        rect
    }
}

impl From<HWND> for Hwnd {
    fn from(hwnd: HWND) -> Self {
        Self(hwnd.0 as isize)
    }
}
