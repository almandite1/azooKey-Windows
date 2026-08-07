import Testing
import Foundation
import KanaKanjiConverterModule
@testable import azookey_server

/// One engine process serves every application on the desktop, and the only
/// thing keeping their compositions apart is the `sessions` map keyed by the
/// session id the Rust server assigns per pipe connection. That mechanism —
/// the reason it exists at all — had no test: typing in two applications at
/// once used to corrupt both compositions, and nothing here would have
/// caught its return.
///
/// Session ids follow the convention in `defensive_tests.swift`: an
/// 0x7100_xxxx range of this suite's own, cleaned up with `defer`, so the
/// tests cannot collide with each other or with a real client.
@Suite("session isolation")
@MainActor
struct SessionTests {
    /// What the session is currently composing, straight out of the map.
    private func reading(_ session: Int64) -> String {
        sessions[session]?.composingText.convertTarget ?? ""
    }

    /// Two applications typing at the same time. Interleaved on purpose: a
    /// shared `ComposingText` would splice the two readings together.
    @Test("two sessions compose independently")
    func sessionsDoNotShareComposingText() {
        let a: Int64 = 0x7100_0001
        let b: Int64 = 0x7100_0002
        defer { remove_session(session: a); remove_session(session: b) }

        #expect(append("mi", to: a) == "み")
        #expect(append("ka", to: b) == "か")
        #expect(append("zu", to: a) == "みず")
        #expect(append("ki", to: b) == "かき")

        #expect(reading(a) == "みず")
        #expect(reading(b) == "かき")
    }

    /// A session is created on first use, so a brand-new connection must
    /// start empty even while another one is mid-composition.
    @Test("a new session starts empty next to a busy one")
    func newSessionStartsEmpty() {
        let busy: Int64 = 0x7100_0003
        let fresh: Int64 = 0x7100_0004
        defer { remove_session(session: busy); remove_session(session: fresh) }
        append("nihon", to: busy)

        #expect(append("a", to: fresh) == "あ")
        #expect(reading(busy) == "にほn", "the other composition is untouched")
    }

    /// One application committing or cancelling its composition must not
    /// wipe another's.
    @Test("ClearText only clears its own session")
    func clearTextIsScopedToOneSession() {
        let a: Int64 = 0x7100_0005
        let b: Int64 = 0x7100_0006
        defer { remove_session(session: a); remove_session(session: b) }
        append("mizu", to: a)
        append("kaki", to: b)

        clear_text(session: a, confirmedCandidate: -1)

        #expect(reading(a) == "")
        #expect(sessions[a] != nil, "the session survives its composition")
        #expect(reading(b) == "かき")
    }

    /// The eviction path (an idle or disconnected client) takes the whole
    /// entry, and must take nobody else's.
    @Test("RemoveSession drops one session and leaves the rest")
    func removeSessionIsScopedToOneSession() {
        let a: Int64 = 0x7100_0007
        let b: Int64 = 0x7100_0008
        defer { remove_session(session: a); remove_session(session: b) }
        append("mizu", to: a)
        append("kaki", to: b)

        remove_session(session: a)

        #expect(sessions[a] == nil)
        #expect(reading(b) == "かき")
    }

    /// Removing a session that never existed happens whenever the server
    /// evicts an idle connection that never composed; it must be a no-op.
    @Test("removing an unknown session is harmless")
    func removingAnUnknownSessionIsSafe() {
        let live: Int64 = 0x7100_0009
        defer { remove_session(session: live) }
        append("a", to: live)

        remove_session(session: 0x7100_00FF)

        #expect(reading(live) == "あ")
    }

    /// `SetContext` carries the text to the left of the composition, which
    /// the model conditions on — it belongs to the application that sent it.
    @Test("SetContext is per session")
    func contextIsPerSession() {
        let a: Int64 = 0x7100_000A
        let b: Int64 = 0x7100_000B
        defer { remove_session(session: a); remove_session(session: b) }

        "吾輩は".withCString { set_context(session: a, context: $0) }
        "こんにちは".withCString { set_context(session: b, context: $0) }

        #expect(sessions[a]?.context == "吾輩は")
        #expect(sessions[b]?.context == "こんにちは")
        // and it survives editing the reading
        append("neko", to: a)
        #expect(sessions[a]?.context == "吾輩は")
    }
}

/// The exports the client drives between `AppendText` and `GetComposedText`.
/// Their empty-session behavior is covered by `DefensiveTests`; what was
/// missing is that they do the right thing when there IS something composing.
@Suite("composition editing exports")
@MainActor
struct CompositionEditingTests {
    private func removeText(_ session: Int64) -> (reading: String, cursor: Int32) {
        var cursor: Int32 = -1
        let reading = withUnsafeMutablePointer(to: &cursor) {
            remove_text(session: session, cursorPtr: $0)
        }
        defer { free_string(ptr: reading) }
        return (reading.map { String(cString: $0) } ?? "", cursor)
    }

    private func moveCursor(_ session: Int64, _ offset: Int32) -> (reading: String, cursor: Int32) {
        var cursor: Int32 = -1
        let reading = withUnsafeMutablePointer(to: &cursor) {
            move_cursor(session: session, offset: offset, cursorPtr: $0)
        }
        defer { free_string(ptr: reading) }
        return (reading.map { String(cString: $0) } ?? "", cursor)
    }

    private func shrink(_ session: Int64, by surfaces: Int32) -> String {
        let reading = shrink_text(session: session, surfaceOffset: surfaces, confirmedCandidate: -1)
        defer { free_string(ptr: reading) }
        return reading.map { String(cString: $0) } ?? ""
    }

    /// Backspace: one kana per call, with the cursor following the end of the
    /// reading.
    @Test("RemoveText deletes one kana at a time")
    func removeTextDeletesOneKanaPerCall() {
        let session: Int64 = 0x7101_0001
        defer { remove_session(session: session) }
        #expect(append("mizu", to: session) == "みず")

        #expect(removeText(session) == ("み", 1))
        #expect(removeText(session) == ("", 0))
        // past the start it stays empty rather than trapping
        #expect(removeText(session) == ("", 0))
    }

    /// Backspace works on the READING, not on keystrokes: `kyou` is four
    /// letters but three characters of reading, and it takes three
    /// Backspaces — the small ょ is its own character, deleted separately
    /// from the き it was typed with.
    @Test("RemoveText deletes reading characters, not keystrokes")
    func removeTextDeletesReadingCharacters() {
        let session: Int64 = 0x7101_0002
        defer { remove_session(session: session) }
        #expect(append("kyou", to: session) == "きょう")

        #expect(removeText(session).reading == "きょ", "the う goes first")
        #expect(removeText(session).reading == "き", "then the small ょ alone")
        #expect(removeText(session).reading == "")
    }

    /// Moving the cursor is a pure navigation: the reading it returns must be
    /// the same one, or the client would redraw a composition that never
    /// changed.
    @Test("MoveCursor leaves the reading alone")
    func moveCursorKeepsTheReading() {
        let session: Int64 = 0x7101_0003
        defer { remove_session(session: session) }
        append("mizu", to: session)

        #expect(moveCursor(session, -1).reading == "みず")
        #expect(sessions[session]?.composingText.convertTargetCursorPosition == 1)
        #expect(moveCursor(session, 1).reading == "みず")
        #expect(sessions[session]?.composingText.convertTargetCursorPosition == 2)
    }

    /// The offset is whatever the client sent, so this is the FFI boundary:
    /// it clamps at both ends instead of running off the reading.
    ///
    /// The out-parameter is the absolute cursor position after the move, the
    /// same thing `AppendText`/`RemoveText` report in that field — the
    /// underlying `moveCursorFromCursorPosition` hands back the distance it
    /// travelled instead, and letting that through made one argument mean two
    /// things (#82).
    @Test("MoveCursor clamps at both ends of the reading")
    func moveCursorClamps() {
        let session: Int64 = 0x7101_0004
        defer { remove_session(session: session) }
        append("mizu", to: session)  // 2 kana, cursor at 2

        #expect(moveCursor(session, -100).cursor == 0, "clamped to the start")
        #expect(sessions[session]?.composingText.convertTargetCursorPosition == 0)

        #expect(moveCursor(session, 100).cursor == 2, "clamped to the end")
        #expect(sessions[session]?.composingText.convertTargetCursorPosition == 2)

        // a move that goes nowhere still reports where the cursor is, not the
        // zero distance it covered
        #expect(moveCursor(session, 100).cursor == 2, "already at the end")
    }

    /// Committing the front of the composition: `ShrinkText` spends a KANA
    /// count (not a keystroke count) and leaves the rest composing. This is
    /// the client's half of accepting a candidate that covers only part of
    /// the reading.
    @Test("ShrinkText commits the front and leaves the remainder")
    func shrinkTextLeavesTheRemainder() {
        let session: Int64 = 0x7101_0005
        defer { remove_session(session: session) }
        #expect(append("aisatu", to: session) == "あいさつ")

        #expect(shrink(session, by: 2) == "さつ")
        #expect(sessions[session]?.composingText.convertTarget == "さつ", "the session kept it")

        #expect(shrink(session, by: 1) == "つ")
        #expect(shrink(session, by: 1) == "")
    }

    /// The count is measured in kana precisely so a candidate can end inside
    /// a romaji cluster: かんしゃ is 6 keystrokes but 4 kana, and 「監視」
    /// covers the first 3 of them.
    @Test("ShrinkText spends kana, not keystrokes")
    func shrinkTextSpendsKana() {
        let session: Int64 = 0x7101_0006
        defer { remove_session(session: session) }
        #expect(append("kansha", to: session) == "かんしゃ")

        #expect(shrink(session, by: 3) == "ゃ")
    }

    /// A count past the end is clamped rather than trapped — the count comes
    /// over the pipe, and a trap here takes every application's composition
    /// down with the engine.
    @Test("ShrinkText past the end empties the reading instead of trapping")
    func shrinkTextPastTheEndIsClamped() {
        let session: Int64 = 0x7101_0007
        defer { remove_session(session: session) }
        append("mizu", to: session)

        #expect(shrink(session, by: 99) == "")
        #expect(shrink(session, by: -5) == "", "a negative count is clamped too")
    }

    /// `ClearText` and `RemoveSession` both end a composition, and both call
    /// `endComposition` to drop the shared converter's per-composition caches.
    /// That reset is internal to the converter and cannot be observed from
    /// here; what this pins is that the calls are safe before `Initialize`
    /// has ever built a converter — the same FFI-boundary rule the rest of
    /// `DefensiveTests` covers — and that the session state they own is reset.
    @Test("ending a composition without a converter is safe")
    func endingACompositionWithoutAConverterIsSafe() {
        converter = nil
        let session: Int64 = 0x7101_0008
        defer { remove_session(session: session) }
        append("mizu", to: session)

        clear_text(session: session, confirmedCandidate: -1)
        #expect(sessions[session]?.composingText.convertTarget == "")

        append("kaki", to: session)
        remove_session(session: session)
        #expect(sessions[session] == nil)
    }
}
