//! The raw C FFI surface of the Swift engine.
//!
//! The canonical declaration of this interface is
//! `server-swift/Sources/ffi/include/ffi.h`; the extern block below and the
//! Swift `@_cdecl` exports in `FFIExports.swift` must both match it.
//! Keep the three in sync — `tests/ffi_surface.rs` checks that the three
//! sets of function names agree (add/rename/typo in one place fails it),
//! and the `--ignored` smoke tests (crates/server/tests/ipc_smoke.rs)
//! exercise every function against the live engine.
//!
//! Contract (same as ffi.h):
//! - all functions must be called from a single thread, serially — the Swift
//!   side keeps unsynchronized global state (see the current_thread runtime
//!   in main())
//! - out-parameters are 32-bit (c_int on both sides)
//! - `session` selects the per-client composing state; it is the pipe
//!   connection id assigned in lib.rs (see PipeConnectInfo). 64-bit, so the
//!   monotonic counter behind it cannot realistically wrap onto a live
//!   session
//! - every returned string/list is owned by the callee and must be handed
//!   back to FreeString / FreeComposedText after copying

use std::ffi::{c_char, c_int};

#[derive(Debug, Clone)]
#[repr(C)]
pub(crate) struct FFICandidate {
    pub(crate) text: *mut c_char,
    pub(crate) subtext: *mut c_char,
    pub(crate) corresponding_count: c_int,
    pub(crate) surface_count: c_int,
}

unsafe extern "C" {
    pub(crate) fn Initialize(path: *const c_char);
    pub(crate) fn SetContext(session: i64, context: *const c_char);
    // `cursorPtr` receives the absolute cursor position in the reading after
    // the call, counted in kana from its start. MoveCursor takes a relative
    // offset but reports a position too, not the distance it moved (#82).
    pub(crate) fn AppendText(
        session: i64,
        input: *const c_char,
        cursorPtr: *mut c_int,
    ) -> *mut c_char;
    pub(crate) fn RemoveText(session: i64, cursorPtr: *mut c_int) -> *mut c_char;
    pub(crate) fn MoveCursor(session: i64, offset: c_int, cursorPtr: *mut c_int) -> *mut c_char;
    pub(crate) fn ShrinkText(session: i64, surfaceOffset: c_int) -> *mut c_char;
    pub(crate) fn ClearText(session: i64);
    pub(crate) fn GetComposedText(session: i64, lengthPtr: *mut c_int) -> *mut *mut FFICandidate;
    pub(crate) fn RemoveSession(session: i64);
    pub(crate) fn LoadConfig();
    pub(crate) fn FreeString(ptr: *mut c_char);
    pub(crate) fn FreeComposedText(listPtr: *mut *mut FFICandidate, length: c_int);
}
