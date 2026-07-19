//! Safe wrappers over the raw FFI: every `unsafe` call into the Swift
//! engine lives here, together with the string-conversion and ownership
//! plumbing. The gRPC service (service.rs) never touches the FFI directly.

use std::ffi::{c_char, c_int, CStr, CString};

use shared::proto::Suggestion;

use crate::ffi::{
    AppendText, ClearText, FreeComposedText, FreeString, GetComposedText, Initialize, LoadConfig,
    MoveCursor, RemoveText, SetContext, ShrinkText,
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

pub(crate) fn initialize(path: &str) {
    unsafe {
        let path = to_cstring(path);
        Initialize(path.as_ptr());
    }
}

pub(crate) fn add_text(session: i32, input: &str) -> RawComposingText {
    unsafe {
        let input = to_cstring(input);
        let mut cursor: c_int = 0;

        let result = AppendText(session, input.as_ptr(), &mut cursor);
        let text = consume_cstr(result);

        RawComposingText { text, cursor }
    }
}

pub(crate) fn move_cursor(session: i32, offset: i32) -> RawComposingText {
    unsafe {
        let mut cursor: c_int = 0;

        let result = MoveCursor(session, offset, &mut cursor);
        let text = consume_cstr(result);

        RawComposingText { text, cursor }
    }
}

pub(crate) fn remove_text(session: i32) -> RawComposingText {
    unsafe {
        let mut cursor: c_int = 0;

        let result = RemoveText(session, &mut cursor);
        let text = consume_cstr(result);

        RawComposingText { text, cursor }
    }
}

pub(crate) fn clear_text(session: i32) {
    unsafe {
        ClearText(session);
    }
}

// offset is the full int32 from the wire: it is a count of roman-input
// keystrokes and routinely exceeds 127 in long compositions. Truncating
// it (a former `as i8`) wrapped it negative, and a negative count makes
// the Swift engine's Array.removeFirst trap, killing the whole server.
pub(crate) fn shrink_text(session: i32, offset: i32) -> RawComposingText {
    unsafe {
        let result = ShrinkText(session, offset);
        let text = consume_cstr(result);

        RawComposingText { text, cursor: 0 }
    }
}

pub(crate) fn set_context(session: i32, context: &str) {
    let context = to_cstring(context);
    unsafe { SetContext(session, context.as_ptr()) };
}

pub(crate) fn load_config() {
    unsafe { LoadConfig() };
}

pub(crate) fn get_composed_text(session: i32) -> Vec<Suggestion> {
    unsafe {
        let mut length: c_int = 0;
        let result = GetComposedText(session, &mut length);

        if result.is_null() {
            return Vec::new();
        }

        let mut suggestions = Vec::with_capacity(length.max(0) as usize);

        for index in 0..length.max(0) as usize {
            let candidate_ptr = *result.add(index);
            if candidate_ptr.is_null() {
                continue;
            }
            let candidate = (*candidate_ptr).clone();
            let text = cstr_or_empty(candidate.text);
            let subtext = cstr_or_empty(candidate.subtext);
            let corresponding_count = candidate.corresponding_count;

            let suggestion = Suggestion {
                text,
                subtext,
                corresponding_count,
            };

            // check if suggestions have the same text
            if suggestions
                .iter()
                .any(|s: &Suggestion| s.text == suggestion.text)
            {
                continue;
            }
            suggestions.push(suggestion);
        }

        FreeComposedText(result, length.max(0));

        suggestions
    }
}
