//! Safe wrappers over the raw FFI: every `unsafe` call into the Swift
//! engine lives here, together with the string-conversion and ownership
//! plumbing. The gRPC service (service.rs) never touches the FFI directly.

use std::ffi::{c_char, c_int, CStr, CString};

use shared::proto::Suggestion;

use crate::ffi::{
    AppendText, ClearText, FFICandidate, FreeComposedText, FreeString, GetComposedText, Initialize,
    LoadConfig, MoveCursor, RemoveText, SetContext, ShrinkText,
};

pub(crate) struct RawComposingText {
    pub(crate) text: String,
    // provided by the engine but not yet exposed over gRPC (the client's
    // MoveCursor handling is still a TODO)
    #[allow(dead_code)]
    pub(crate) cursor: i32,
}

/// Builds a CString from possibly untrusted input. Interior NUL bytes cannot
/// be represented in a C string, so they are stripped instead of panicking —
/// requests arrive over a pipe any local process can open.
fn to_cstring(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}

/// Copies a C string returned by the Swift engine and frees the original.
/// Tolerates null pointers and invalid UTF-8 instead of crashing the server.
unsafe fn consume_cstr(ptr: *mut c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        let text = unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() };
        unsafe { FreeString(ptr) };
        text
    }
}

/// Copies a C string without taking ownership (the containing candidate
/// list is freed as a whole by FreeComposedText).
unsafe fn cstr_or_empty(ptr: *const c_char) -> String {
    if ptr.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() }
    }
}

/// Shared shape of the composing-text calls: the callee may write the
/// cursor position through the out-parameter and returns a string it owns
/// (consumed and freed here). Callers wrap exactly one FFI call in the
/// closure.
fn composing_call(call: impl FnOnce(*mut c_int) -> *mut c_char) -> RawComposingText {
    let mut cursor: c_int = 0;
    let text = unsafe { consume_cstr(call(&mut cursor)) };
    RawComposingText { text, cursor }
}

pub(crate) fn initialize(path: &str) {
    let path = to_cstring(path);
    unsafe { Initialize(path.as_ptr()) };
}

pub(crate) fn add_text(session: i32, input: &str) -> RawComposingText {
    let input = to_cstring(input);
    composing_call(|cursor| unsafe { AppendText(session, input.as_ptr(), cursor) })
}

pub(crate) fn move_cursor(session: i32, offset: i32) -> RawComposingText {
    composing_call(|cursor| unsafe { MoveCursor(session, offset, cursor) })
}

pub(crate) fn remove_text(session: i32) -> RawComposingText {
    composing_call(|cursor| unsafe { RemoveText(session, cursor) })
}

pub(crate) fn clear_text(session: i32) {
    unsafe { ClearText(session) };
}

// offset is the full int32 from the wire: it is a count of roman-input
// keystrokes and routinely exceeds 127 in long compositions. Truncating
// it (a former `as i8`) wrapped it negative, and a negative count makes
// the Swift engine's Array.removeFirst trap, killing the whole server.
// ShrinkText has no cursor out-parameter, so the cursor stays 0.
pub(crate) fn shrink_text(session: i32, offset: i32) -> RawComposingText {
    composing_call(|_cursor| unsafe { ShrinkText(session, offset) })
}

pub(crate) fn set_context(session: i32, context: &str) {
    let context = to_cstring(context);
    unsafe { SetContext(session, context.as_ptr()) };
}

pub(crate) fn load_config() {
    unsafe { LoadConfig() };
}

/// Owns the candidate list returned by GetComposedText and hands it back
/// to FreeComposedText on drop, so every return path — including future
/// early returns and panics unwinding through here — frees it exactly once.
struct ComposedTextList {
    ptr: *mut *mut FFICandidate,
    length: c_int,
}

impl ComposedTextList {
    fn fetch(session: i32) -> Self {
        let mut length: c_int = 0;
        let ptr = unsafe { GetComposedText(session, &mut length) };
        ComposedTextList {
            ptr,
            length: length.max(0),
        }
    }

    fn candidates(&self) -> impl Iterator<Item = &FFICandidate> {
        let list = self;
        (0..if list.ptr.is_null() {
            0
        } else {
            list.length as usize
        })
            .filter_map(move |index| unsafe { (*list.ptr.add(index)).as_ref() })
    }
}

impl Drop for ComposedTextList {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe { FreeComposedText(self.ptr, self.length) };
        }
    }
}

pub(crate) fn get_composed_text(session: i32) -> Vec<Suggestion> {
    let list = ComposedTextList::fetch(session);

    let mut suggestions: Vec<Suggestion> = Vec::with_capacity(list.length as usize);
    for candidate in list.candidates() {
        let suggestion = Suggestion {
            text: unsafe { cstr_or_empty(candidate.text) },
            subtext: unsafe { cstr_or_empty(candidate.subtext) },
            corresponding_count: candidate.corresponding_count,
        };

        // the engine can propose the same surface text more than once;
        // keep the first (highest-ranked) occurrence
        if suggestions.iter().any(|s| s.text == suggestion.text) {
            continue;
        }
        suggestions.push(suggestion);
    }

    suggestions
}
