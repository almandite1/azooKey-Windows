//! Safe wrappers over the raw FFI: every `unsafe` call into the Swift
//! engine lives here, together with the string-conversion and ownership
//! plumbing. The gRPC service (service.rs) never touches the FFI directly.

use std::ffi::{CStr, CString, c_char, c_int};

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

pub(crate) fn add_text(session: i64, input: &str) -> RawComposingText {
    let input = to_cstring(input);
    composing_call(|cursor| unsafe { AppendText(session, input.as_ptr(), cursor) })
}

pub(crate) fn move_cursor(session: i64, offset: i32) -> RawComposingText {
    composing_call(|cursor| unsafe { MoveCursor(session, offset, cursor) })
}

pub(crate) fn remove_text(session: i64) -> RawComposingText {
    composing_call(|cursor| unsafe { RemoveText(session, cursor) })
}

pub(crate) fn clear_text(session: i64) {
    unsafe { ClearText(session) };
}

// The offset is the full int32 from the wire: it is a count of kana in the
// reading and can exceed 127 in a long composition. Truncating it (a former
// `as i8`) wrapped it negative, and a negative count makes the Swift engine
// trap in dropFirst, killing the whole server.
// ShrinkText has no cursor out-parameter, so the cursor stays 0.
pub(crate) fn shrink_text(session: i64, surface_offset: i32) -> RawComposingText {
    composing_call(|_cursor| unsafe { ShrinkText(session, surface_offset) })
}

pub(crate) fn set_context(session: i64, context: &str) {
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
    fn fetch(session: i64) -> Self {
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

pub(crate) fn get_composed_text(session: i64) -> Vec<Suggestion> {
    let list = ComposedTextList::fetch(session);

    let mut suggestions: Vec<Suggestion> = Vec::with_capacity(list.length as usize);
    for candidate in list.candidates() {
        let suggestion = Suggestion {
            text: unsafe { cstr_or_empty(candidate.text) },
            subtext: unsafe { cstr_or_empty(candidate.subtext) },
            corresponding_count: candidate.corresponding_count,
            surface_count: candidate.surface_count,
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

#[cfg(test)]
mod tests {
    //! IMPORTANT: nothing here may reference an FFI symbol, directly or
    //! through a helper that calls one.
    //!
    //! `build.rs` links `azookey-server.lib` into the whole crate, but the
    //! test binary only ends up DEPENDING on the Swift DLL if it actually
    //! references an imported symbol — otherwise MSVC's `/OPT:REF` drops the
    //! import and the tests run with no Swift runtime present. That is why
    //! these can run in CI at all. Testing `consume_cstr` (it calls
    //! `FreeString`) or any of the `pub(crate)` wrappers would break every
    //! test in the crate, not just the new one.

    use super::{cstr_or_empty, to_cstring};
    use std::ffi::CString;

    #[test]
    fn plain_and_multibyte_text_round_trips() {
        assert_eq!(to_cstring("abc").as_bytes(), b"abc");
        assert_eq!(to_cstring("にほんご").as_bytes(), "にほんご".as_bytes());
    }

    /// Requests arrive over a pipe any local process can open, so an interior
    /// NUL is untrusted input rather than a bug: it is stripped, never a
    /// panic.
    #[test]
    fn interior_nul_bytes_are_stripped() {
        assert_eq!(to_cstring("a\0b").as_bytes(), b"ab");
        assert_eq!(to_cstring("\0").as_bytes(), b"");
        assert_eq!(to_cstring("a\0\0b").as_bytes(), b"ab");
    }

    #[test]
    fn empty_string_is_an_empty_cstring() {
        assert_eq!(to_cstring("").as_bytes(), b"");
    }

    #[test]
    fn cstr_or_empty_tolerates_null() {
        assert_eq!(unsafe { cstr_or_empty(std::ptr::null()) }, "");
    }

    /// The engine's strings are not guaranteed valid UTF-8; pin the
    /// `to_string_lossy` contract so a future rewrite cannot turn a bad byte
    /// into a panic inside a gRPC handler.
    #[test]
    fn cstr_or_empty_replaces_invalid_utf8() {
        let bad = CString::new([b'a', 0xFF, b'b']).expect("no interior NUL");

        assert_eq!(unsafe { cstr_or_empty(bad.as_ptr()) }, "a\u{FFFD}b");
    }
}
