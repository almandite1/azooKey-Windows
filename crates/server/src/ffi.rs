//! The raw C FFI surface of the Swift engine.
//!
//! The canonical declaration of this interface is
//! `server-swift/Sources/ffi/include/ffi.h`; the extern block below and the
//! Swift `@_cdecl` exports in `azookey_server.swift` must both match it.
//! Keep the three in sync — nothing checks them mechanically, but the
//! `--ignored` smoke tests (crates/server/tests/ipc_smoke.rs) exercise every
//! function against the live engine.
//!
//! Contract (same as ffi.h):
//! - all functions must be called from a single thread, serially — the Swift
//!   side keeps unsynchronized global state (see the current_thread runtime
//!   in main())
//! - out-parameters are 32-bit (c_int on both sides)
//! - `session` selects the per-client composing state; it is the pipe
//!   connection id assigned in lib.rs (see PipeConnectInfo)
//! - every returned string/list is owned by the callee and must be handed
//!   back to FreeString / FreeComposedText after copying

use std::ffi::{c_char, c_int};

#[derive(Debug, Clone)]
#[repr(C)]
pub(crate) struct FFICandidate {
    pub(crate) text: *mut c_char,
    pub(crate) subtext: *mut c_char,
    pub(crate) corresponding_count: c_int,
}

unsafe extern "C" {
    pub(crate) fn Initialize(path: *const c_char);
    pub(crate) fn SetContext(session: c_int, context: *const c_char);
    pub(crate) fn AppendText(
        session: c_int,
        input: *const c_char,
        cursorPtr: *mut c_int,
    ) -> *mut c_char;
    pub(crate) fn RemoveText(session: c_int, cursorPtr: *mut c_int) -> *mut c_char;
    pub(crate) fn MoveCursor(session: c_int, offset: c_int, cursorPtr: *mut c_int) -> *mut c_char;
    pub(crate) fn ShrinkText(session: c_int, offset: c_int) -> *mut c_char;
    pub(crate) fn ClearText(session: c_int);
    pub(crate) fn GetComposedText(session: c_int, lengthPtr: *mut c_int) -> *mut *mut FFICandidate;
    pub(crate) fn RemoveSession(session: c_int);
    pub(crate) fn LoadConfig();
    pub(crate) fn FreeString(ptr: *mut c_char);
    pub(crate) fn FreeComposedText(listPtr: *mut *mut FFICandidate, length: c_int);
}
