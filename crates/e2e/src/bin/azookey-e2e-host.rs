//! A minimal Win32 text host for the E2E scenarios.
//!
//! Notepad is the obvious host, but it is a packaged application whose editor
//! changes shape between Windows releases; pinning "conversion works in a
//! second application" to a control we own keeps that scenario stable. It is
//! deliberately tiny: one overlapped window with a single multiline EDIT
//! child that always holds the focus, so a TSF input method composes straight
//! into it and UI Automation reads it back through the Value pattern.
//!
//! This also lays the groundwork for the password-field scenario (the plan's
//! #4): a second EDIT with `ES_PASSWORD` slots in the same way.
//!
//! Not a test by itself — it is launched BY the harness (`second_host_converts`).

#![windows_subsystem = "windows"]

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_WINDOW, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DispatchMessageW, GWLP_USERDATA,
    GetClientRect, GetMessageW, GetWindowLongPtrW, HMENU, IDC_ARROW, IsDialogMessageW, LoadCursorW,
    MSG, MoveWindow, PostQuitMessage, RegisterClassW, SW_SHOW, SetWindowLongPtrW, ShowWindow,
    TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CREATE, WM_DESTROY, WM_SETFOCUS, WM_SIZE,
    WNDCLASSW, WS_CHILD, WS_EX_LEFT, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};
use windows::core::{Result, w};

/// The edit controls, kept so `WM_SIZE`/`WM_SETFOCUS` can reach them. Stashed
/// in the window's `GWLP_USERDATA` rather than a global, so nothing is shared
/// across the (single) window this process owns.
struct State {
    /// The ordinary text field: where conversion is expected to work.
    edit: HWND,
    /// An `ES_PASSWORD` field. A TSF host sets the disable-IME compartment on
    /// a password control, and the TIP is supposed to fall back to direct
    /// input there — the plan's scenario 4. Tab moves between the two.
    password: HWND,
}

// Standard EDIT control window styles (windows 0.62 does not surface these as
// named constants, so the values are inlined with the constant they mirror).
const ES_MULTILINE: i32 = 0x0004;
const ES_WANTRETURN: i32 = 0x1000;
const ES_PASSWORD: i32 = 0x0020;

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

        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            // Tab between the text field and the password field. A plain
            // window gets no tab navigation for free — this is what a dialog
            // does for its controls, and scenario 4 moves focus with Tab
            // because that is how a person reaches a password box.
            if IsDialogMessageW(window, &message).as_bool() {
                continue;
            }
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
                    Some(HMENU::default()),
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
                    Some(HMENU::default()),
                    Some(instance),
                    None,
                )
                .expect("failed to create the password control");

                let state = Box::into_raw(Box::new(State { edit, password }));
                SetWindowLongPtrW(window, GWLP_USERDATA, state as isize);

                let _ = SetFocus(Some(edit));
                LRESULT(0)
            }
            WM_SIZE => {
                if let Some(state) = state_of(window) {
                    let mut rect = RECT::default();
                    let _ = GetClientRect(window, &mut rect);
                    // the text field takes everything above a fixed-height
                    // password box at the bottom
                    const PASSWORD_HEIGHT: i32 = 28;
                    let text_height = (rect.bottom - PASSWORD_HEIGHT).max(0);
                    let _ = MoveWindow(state.edit, 0, 0, rect.right, text_height, true);
                    let _ = MoveWindow(
                        state.password,
                        0,
                        text_height,
                        rect.right,
                        PASSWORD_HEIGHT,
                        true,
                    );
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
