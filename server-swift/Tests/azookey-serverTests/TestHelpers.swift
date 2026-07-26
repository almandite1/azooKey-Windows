import Foundation
import KanaKanjiConverterModule
@testable import azookey_server

// What more than one suite needs. Each of these existed two or three times,
// verbatim, in the files beside this one.

/// A `ComposingText` holding `input` typed as romaji — the way every keystroke
/// reaches the engine from the TIP.
func romaji(_ input: String) -> ComposingText {
    var text = ComposingText()
    text.insertAtCursorPosition(input, inputStyle: .roman2kana)
    return text
}

/// The package root, derived from this file's own path. The dictionaries are
/// submodules of the repository, so they are always next to it.
let packageRoot = URL(filePath: #filePath)
    .deletingLastPathComponent()  // azookey-serverTests
    .deletingLastPathComponent()  // Tests
    .deletingLastPathComponent()  // server-swift

// SESSION ID CONVENTION, written down once here rather than restated in each
// suite's header: ids are partitioned per suite so tests cannot collide with
// each other or with a real client — `DefensiveTests` owns 0x7000_xxxx,
// `SessionTests` 0x7100_xxxx, `CompositionEditingTests` 0x7101_xxxx — and
// every test removes its own session with `defer`. A new suite takes the next
// free block.

/// `AppendText` across the FFI, with the string and cursor plumbing every
/// caller needs. Returns the reading the engine answered with.
@MainActor
@discardableResult
func append(_ input: String, to session: Int64) -> String {
    var cursor: Int32 = -1
    let reading = input.withCString { text in
        withUnsafeMutablePointer(to: &cursor) {
            append_text(session: session, input: text, cursorPtr: $0)
        }
    }
    defer { free_string(ptr: reading) }
    return reading.map { String(cString: $0) } ?? ""
}
