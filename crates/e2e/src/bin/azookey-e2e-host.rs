//! A minimal Win32 text host for the E2E scenarios.
//!
//! Notepad is the obvious host, but it is a packaged application whose editor
//! changes shape between Windows releases; pinning "conversion works in a
//! second application" to a control we own keeps that scenario stable.
//!
//! Three controls: an ordinary multiline EDIT, an `ES_PASSWORD` field, and
//! a read-only mirror of the latter. Owning the host is what
//! makes the password scenario decidable at all — see [`State::mirror`].
//!
//! Not a test by itself — it is launched BY the harness
//! (`second_host_converts`, `password_field_disables_ime`).

#![windows_subsystem = "windows"]

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_WINDOW, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW, GWLP_USERDATA,
    GetClientRect, GetMessageW, GetWindowLongPtrW, GetWindowTextW, HMENU, IDC_ARROW, LoadCursorW,
    MSG, MoveWindow, PostQuitMessage, RegisterClassW, SW_SHOW, SetWindowLongPtrW, SetWindowTextW,
    ShowWindow, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_COMMAND, WM_CREATE, WM_DESTROY,
    WM_SETFOCUS, WM_SIZE, WNDCLASSW, WS_CHILD, WS_EX_LEFT, WS_OVERLAPPEDWINDOW, WS_TABSTOP,
    WS_VISIBLE, WS_VSCROLL,
};
use windows::core::{PCWSTR, Result, w};

/// The edit controls, kept so `WM_SIZE`/`WM_SETFOCUS` can reach them. Stashed
/// in the window's `GWLP_USERDATA` rather than a global, so nothing is shared
/// across the (single) window this process owns.
struct State {
    /// The ordinary text field: where conversion is expected to work.
    edit: HWND,
    /// An `ES_PASSWORD` field. A TSF host sets the disable-IME compartment on
    /// a password control, and the TIP is supposed to fall back to direct
    /// input there — the plan's scenario 4.
    password: HWND,
    /// A read-only echo of what the password field currently holds.
    ///
    /// The test cannot read the password field itself: UI Automation refuses
    /// to hand out a password's value, by design, and a plain EDIT does not
    /// surface an in-flight composition either (CUAS keeps it out of the
    /// control's buffer until it is committed) — which is exactly why the
    /// earlier attempts at this scenario could observe nothing at all. The
    /// host owns the control, so it can read it with `GetWindowTextW` and
    /// publish it somewhere the test CAN read.
    ///
    /// This exists only in the E2E fixture, never in anything shipped, and the
    /// only thing ever typed into that field is the test's own `mizu`.
    mirror: HWND,
}

/// Control ids. UI Automation exposes a Win32 control's id as its
/// AutomationId, which is how the test addresses one field rather than
/// whichever happens to come first in the tree.
const ID_EDIT: usize = 1;
const ID_PASSWORD: usize = 2;
pub const ID_MIRROR: usize = 3;

// Standard EDIT control window styles (windows 0.62 does not surface these as
// named constants, so the values are inlined with the constant they mirror).
const ES_MULTILINE: i32 = 0x0004;
const ES_WANTRETURN: i32 = 0x1000;
const ES_PASSWORD: i32 = 0x0020;
const ES_READONLY: i32 = 0x0800;
/// `EN_CHANGE`, the EDIT notification carried in `WM_COMMAND`'s high word.
const EN_CHANGE: u16 = 0x0300;

fn main() -> Result<()> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class_name = w!("azookeyE2EHostClass");

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: class_name,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: HBRUSH((COLOR_WINDOW.0 + 1) as usize as *mut _),
            ..Default::default()
        };
        let atom = RegisterClassW(&wc);
        debug_assert!(atom != 0, "RegisterClassW failed");

        // the title is how the harness finds this window (host.rs matches on
        // a non-empty title owned by the right image)
        let window = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("azookey-e2e-host"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            640,
            420,
            None,
            None,
            Some(instance.into()),
            None,
        )?;

        let _ = ShowWindow(window, SW_SHOW);

        // A plain loop on purpose. `IsDialogMessageW` was here to give Tab
        // navigation between the fields, and it does not move focus out of a
        // multiline EDIT — which cost the password scenario three rounds of
        // blaming the IME for a caret that never moved. The harness places the
        // caret through UI Automation instead, so nothing needs to sit between
        // a keystroke and the control under test.
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}

extern "system" fn wndproc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match message {
            WM_CREATE => {
                let instance = (*(lparam.0 as *const CREATESTRUCTW)).hInstance;
                // one multiline EDIT filling the window; ES_WANTRETURN so
                // Enter (the commit key) stays inside the control
                let edit = CreateWindowExW(
                    WS_EX_LEFT,
                    w!("EDIT"),
                    w!(""),
                    WS_CHILD
                        | WS_VISIBLE
                        | WS_TABSTOP
                        | WS_VSCROLL
                        | WINDOW_STYLE((ES_MULTILINE | ES_WANTRETURN) as u32),
                    0,
                    0,
                    0,
                    0,
                    Some(window),
                    Some(HMENU(ID_EDIT as *mut std::ffi::c_void)),
                    Some(instance),
                    None,
                )
                .expect("failed to create the EDIT control");

                // single-line and masked: what a real password box is, and
                // what makes the host disable the IME on it
                let password = CreateWindowExW(
                    WS_EX_LEFT,
                    w!("EDIT"),
                    w!(""),
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(ES_PASSWORD as u32),
                    0,
                    0,
                    0,
                    0,
                    Some(window),
                    Some(HMENU(ID_PASSWORD as *mut std::ffi::c_void)),
                    Some(instance),
                    None,
                )
                .expect("failed to create the password control");

                // read-only and not a tab stop: it is an observation window,
                // never something typed into
                let mirror = CreateWindowExW(
                    WS_EX_LEFT,
                    w!("EDIT"),
                    w!(""),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(ES_READONLY as u32),
                    0,
                    0,
                    0,
                    0,
                    Some(window),
                    Some(HMENU(ID_MIRROR as *mut std::ffi::c_void)),
                    Some(instance),
                    None,
                )
                .expect("failed to create the mirror control");

                let state = Box::into_raw(Box::new(State {
                    edit,
                    password,
                    mirror,
                }));
                SetWindowLongPtrW(window, GWLP_USERDATA, state as isize);

                let _ = SetFocus(Some(edit));
                LRESULT(0)
            }
            WM_SIZE => {
                if let Some(state) = state_of(window) {
                    let mut rect = RECT::default();
                    let _ = GetClientRect(window, &mut rect);
                    // the text field takes everything above the password box
                    // and its mirror, both fixed height, at the bottom
                    const ROW_HEIGHT: i32 = 28;
                    let text_height = (rect.bottom - ROW_HEIGHT * 2).max(0);
                    let _ = MoveWindow(state.edit, 0, 0, rect.right, text_height, true);
                    let _ =
                        MoveWindow(state.password, 0, text_height, rect.right, ROW_HEIGHT, true);
                    let _ = MoveWindow(
                        state.mirror,
                        0,
                        text_height + ROW_HEIGHT,
                        rect.right,
                        ROW_HEIGHT,
                        true,
                    );
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                // republish the password field's content where the test can
                // read it (see `State::mirror`)
                let id = wparam.0 & 0xFFFF;
                let notification = ((wparam.0 >> 16) & 0xFFFF) as u16;
                if id == ID_PASSWORD
                    && notification == EN_CHANGE
                    && let Some(state) = state_of(window)
                {
                    let mut buffer = [0u16; 256];
                    let len = GetWindowTextW(state.password, &mut buffer);
                    let text: Vec<u16> = buffer[..len as usize]
                        .iter()
                        .copied()
                        .chain(std::iter::once(0))
                        .collect();
                    let _ = SetWindowTextW(state.mirror, PCWSTR(text.as_ptr()));
                }
                LRESULT(0)
            }
            WM_SETFOCUS => {
                // the caret belongs in the edit, never on the frame — so a
                // synthesised keystroke always has somewhere to compose
                if let Some(state) = state_of(window) {
                    let _ = SetFocus(Some(state.edit));
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                if let Some(ptr) = take_state(window) {
                    drop(Box::from_raw(ptr));
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(window, message, wparam, lparam),
        }
    }
}

unsafe fn state_of(window: HWND) -> Option<&'static State> {
    let ptr = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *const State;
    (!ptr.is_null()).then(|| unsafe { &*ptr })
}

unsafe fn take_state(window: HWND) -> Option<*mut State> {
    let ptr = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *mut State;
    if ptr.is_null() {
        return None;
    }
    unsafe { SetWindowLongPtrW(window, GWLP_USERDATA, 0) };
    Some(ptr)
}
