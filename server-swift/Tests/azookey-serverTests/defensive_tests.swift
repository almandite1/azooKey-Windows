import Testing
import Foundation
import KanaKanjiConverterModule
import ffi
@testable import azookey_server

/// The engine is the single process every application on the desktop shares,
/// so the FFI boundary has to survive input the callers should never send but
/// might: a request that arrives before `Initialize`, an edit on a session
/// that was never composed into, a shrink offset past the end of the reading.
/// None of these may trap — a trap here takes every app's composition down.
///
/// The clamping arithmetic itself is fixed by `RemainderTests` in
/// `azookey_serverTests.swift`; this suite guards the parts that only show at
/// the C boundary: the uninitialized-converter guard, empty-session safety,
/// and that the `@_cdecl` exports marshal and free their strings and lists
/// symmetrically when called the way the Rust server calls them.
@Suite("defensive FFI boundary")
@MainActor
struct DefensiveTests {
    /// Same layout as `RealDictionaryTests`: the dictionaries are submodules
    /// sitting next to this file, so derive their path from `#filePath`.

    private static let engine = KanaKanjiConverter(
        dictionaryURL: packageRoot.appendingPathComponent("azooKey_dictionary_storage/Dictionary"),
        preloadDictionary: true
    )

    /// Nothing may convert before `Initialize` builds the converter. A call
    /// that arrives early is the FFI boundary doing its job: it returns an
    /// empty candidate list (length 0) instead of trapping on the nil.
    @Test("GetComposedText before Initialize returns an empty list")
    func uninitializedConverterIsEmpty() {
        converter = nil
        let session: Int64 = 0x7000_0001
        defer { remove_session(session: session) }

        // even with a reading pending, the nil converter short-circuits
        var cursor: Int32 = 0
        let appended = "a".withCString { input in
            withUnsafeMutablePointer(to: &cursor) {
                append_text(session: session, input: input, cursorPtr: $0)
            }
        }
        free_string(ptr: appended)

        var length: Int32 = -1
        let list = withUnsafeMutablePointer(to: &length) {
            get_composed_text(session: session, lengthPtr: $0)
        }
        #expect(length == 0)
        // freeing an empty list must still be safe (allocate(capacity: max(0,1)))
        free_composed_text(listPtr: list, length: length)
    }

    /// A RemoveText on a session that was never composed into deletes from an
    /// empty `ComposingText`; `deleteBackwardFromCursorPosition` must no-op,
    /// not trap.
    @Test("RemoveText on an empty session returns an empty reading")
    func removeOnEmptySessionIsSafe() {
        let session: Int64 = 0x7000_0002
        defer { remove_session(session: session) }

        var cursor: Int32 = -1
        let reading = withUnsafeMutablePointer(to: &cursor) {
            remove_text(session: session, cursorPtr: $0)
        }
        #expect(cursor == 0)
        #expect(reading != nil)
        #expect(String(cString: reading!) == "")
        free_string(ptr: reading)
    }

    /// A shrink offset past the end of an empty session is the delayed-trap
    /// case from `RemainderTests`, reached through the `@_cdecl` export:
    /// `spend` clamps the surface count to what the (empty) text can give.
    @Test("ShrinkText past the end of an empty session does not trap")
    func shrinkOnEmptySessionIsSafe() {
        let session: Int64 = 0x7000_0003
        defer { remove_session(session: session) }

        let reading = shrink_text(session: session, surfaceOffset: 5)
        #expect(reading != nil)
        #expect(String(cString: reading!) == "")
        free_string(ptr: reading)
    }

    /// `AppendText` is the one export that marshals a string in and a string
    /// out with a cursor out-parameter. Drive it exactly as the Rust server
    /// does — a C string in, the reading and cursor back — and hand the
    /// result to `FreeString`.
    @Test("AppendText marshals input, reading and cursor across the FFI")
    func appendTextMarshalsAcrossFFI() {
        let session: Int64 = 0x7000_0004
        defer { remove_session(session: session) }

        var cursor: Int32 = -1
        let reading = "kyou".withCString { input in
            withUnsafeMutablePointer(to: &cursor) {
                append_text(session: session, input: input, cursorPtr: $0)
            }
        }
        #expect(reading != nil)
        #expect(String(cString: reading!) == "きょう")
        #expect(cursor == 3)
        free_string(ptr: reading)
    }

    /// `to_list_pointer` allocates the outer array and one box per candidate,
    /// each holding `_strdup`'d strings; `FreeComposedText` frees the strings,
    /// the boxes and the array. Build a list by hand so the symmetry is tested
    /// without the converter — the values are readable back, then freed, with
    /// no leak or double free.
    @Test("to_list_pointer and FreeComposedText are allocation-symmetric")
    func listPointerRoundTrips() {
        let candidates = [
            FFICandidate(text: _strdup("水"), subtext: _strdup(""), correspondingCount: 2, surfaceCount: 2),
            FFICandidate(text: _strdup("見ず"), subtext: _strdup("ず"), correspondingCount: 1, surfaceCount: 1),
        ]
        let list = to_list_pointer(candidates)

        #expect(String(cString: list[0]!.pointee.text!) == "水")
        #expect(String(cString: list[1]!.pointee.text!) == "見ず")
        #expect(String(cString: list[1]!.pointee.subtext!) == "ず")
        #expect(list[1]!.pointee.correspondingCount == 1)

        free_composed_text(listPtr: list, length: Int32(candidates.count))
    }

    /// The whole non-empty path through the boundary: `AppendText` builds a
    /// reading, `GetComposedText` converts it with a real converter and
    /// returns an allocated candidate list, `FreeComposedText` releases it.
    /// Exercises `to_list_pointer` with `length > 0` end to end.
    @Test("AppendText → GetComposedText → FreeComposedText round-trips a real conversion")
    func ffiRoundTripThroughConverter() {
        execURL = packageRoot.appendingPathComponent("azooKey_emoji_dictionary_storage")
        converter = Self.engine
        let session: Int64 = 0x7000_0005
        defer { remove_session(session: session); converter = nil }

        var cursor: Int32 = -1
        let reading = "aisatu".withCString { input in
            withUnsafeMutablePointer(to: &cursor) {
                append_text(session: session, input: input, cursorPtr: $0)
            }
        }
        #expect(reading != nil)
        #expect(String(cString: reading!) == "あいさつ")
        free_string(ptr: reading)

        var length: Int32 = -1
        let list = withUnsafeMutablePointer(to: &length) {
            get_composed_text(session: session, lengthPtr: $0)
        }
        #expect(length > 0, "the dictionary offered no candidates for あいさつ")
        for i in 0..<Int(length) {
            let candidate = list[i]
            #expect(candidate != nil)
            #expect(candidate!.pointee.text != nil)
            #expect(candidate!.pointee.subtext != nil)
        }
        free_composed_text(listPtr: list, length: length)
    }
}
