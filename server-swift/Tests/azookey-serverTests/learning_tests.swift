import Testing
import Foundation
import KanaKanjiConverterModule
@testable import azookey_server

/// Learning is the one feature that writes what the user has typed to disk,
/// and the whole path runs through the FFI boundary: an index the Rust side
/// computed, a `Candidate` this side kept from the last conversion, and a
/// commit that has to actually reach the memory directory.
///
/// Two halves, deliberately. The defensive half needs no dictionary and pins
/// what an out-of-range or unbacked index does — the pipe carrying these
/// calls can be opened by any local process, so those are untrusted input.
/// The end-to-end half converts with the real dictionary and asserts that
/// files appear, which is the only thing that can catch a learn-then-commit
/// that quietly writes nowhere.
///
/// Session ids: this suite owns 0x7102_xxxx (see the convention in
/// TestHelpers.swift). Zenzai stays off, so no model file is needed.
@Suite("learning")
@MainActor
struct LearningTests {
    private static let engine = KanaKanjiConverter(
        dictionaryURL: packageRoot.appendingPathComponent("azooKey_dictionary_storage/Dictionary"),
        preloadDictionary: true
    )

    /// Restores the process-global engine state this suite writes.
    private func withRestoredConfig(_ body: () -> Void) {
        let saved = config
        let savedConverter = converter
        defer {
            config = saved
            converter = savedConverter
        }
        body()
    }

    /// A directory that exists for the duration of `body`, standing in for
    /// `%APPDATA%\Azookey\memory`.
    private func withTemporaryDirectory(_ body: (URL) -> Void) {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("azookey-learning-\(UUID().uuidString)")
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: url) }
        body(url)
    }

    /// What the converter has written into `directory`. The store is a LOUDS
    /// trie plus its two payload files; naming them rather than counting
    /// files is what makes "something was written" mean "the memory store
    /// was written".
    private func memoryFiles(in directory: URL) -> [String] {
        let names =
            (try? FileManager.default.contentsOfDirectory(atPath: directory.path)) ?? []
        return names.filter { $0.hasPrefix("memory") }.sorted()
    }

    /// The engine's own list, kept so an index can name one of its entries.
    @discardableResult
    private func convert(_ input: String, in session: Int64) -> Int32 {
        append(input, to: session)
        var length: Int32 = 0
        let list = withUnsafeMutablePointer(to: &length) {
            get_composed_text(session: session, lengthPtr: $0)
        }
        free_composed_text(listPtr: list, length: length)
        return length
    }

    // MARK: - defensive (no dictionary needed)

    /// A session that has never converted has no candidate list, so any index
    /// at all names nothing. It must be ignored rather than trapped: this
    /// arrives over a pipe any local process can open.
    @Test("confirming a candidate a session never had is harmless")
    func confirmingWithoutACandidateListIsSafe() {
        let session: Int64 = 0x7102_0001
        defer { remove_session(session: session) }

        clear_text(session: session, confirmedCandidate: 5)

        #expect(sessions[session]?.lastCandidates.isEmpty ?? true)
        #expect(sessions[session]?.composingText.convertTarget ?? "" == "")
    }

    /// The same for the clause commit, which also has to leave the reading
    /// in the state `spend` would have left it in regardless.
    @Test("an out-of-range index on a shrink does not trap")
    func outOfRangeIndexOnShrinkIsSafe() {
        let session: Int64 = 0x7102_0002
        defer { remove_session(session: session) }
        append("mizu", to: session)

        let reading = shrink_text(
            session: session,
            surfaceOffset: 1,
            confirmedCandidate: Int32.max
        )
        defer { free_string(ptr: reading) }

        #expect(reading != nil)
        #expect(String(cString: reading!) == "ず", "the shrink itself still happened")
    }

    /// -1 is how "learn nothing" travels, and how a TIP too old to send an
    /// index comes out. It must reach the same place as an unusable one.
    @Test("a negative index learns nothing and still clears")
    func negativeIndexIsTheDiscardPath() {
        let session: Int64 = 0x7102_0003
        defer { remove_session(session: session) }
        append("mizu", to: session)

        clear_text(session: session, confirmedCandidate: -1)

        #expect(sessions[session]?.composingText.convertTarget ?? "x" == "")
    }

    /// The retry guard. The client retries `ClearText` as idempotent, and
    /// without emptying the list a resent confirmation would be learned
    /// twice. Emptying it is what makes the retry cost nothing.
    @Test("a confirmed ClearText leaves nothing for a retry to learn from")
    func clearTextEmptiesTheCandidateList() {
        withRestoredConfig {
            withTemporaryDirectory { directory in
                converter = Self.engine
                config = EngineConfig()
                config.memoryDirectory = directory
                execURL = packageRoot.appendingPathComponent("azooKey_emoji_dictionary_storage")

                let session: Int64 = 0x7102_0004
                defer { remove_session(session: session) }

                #expect(convert("mizu", in: session) > 0)
                #expect(!(sessions[session]?.lastCandidates.isEmpty ?? true))

                clear_text(session: session, confirmedCandidate: 0)

                #expect(
                    sessions[session]?.lastCandidates.isEmpty ?? true,
                    "a resent confirmation must find nothing to learn from"
                )
            }
        }
    }

    // MARK: - end to end (real dictionary)

    /// The whole path, and the only test that can show the commit actually
    /// reaching the disk: convert, confirm index 0, and expect the memory
    /// store to exist afterwards.
    @Test("a confirmed candidate is learned and written to the memory directory")
    func confirmingWritesTheMemoryStore() {
        withRestoredConfig {
            withTemporaryDirectory { directory in
                converter = Self.engine
                config = EngineConfig()
                config.memoryDirectory = directory
                execURL = packageRoot.appendingPathComponent("azooKey_emoji_dictionary_storage")

                let session: Int64 = 0x7102_0005
                defer { remove_session(session: session) }

                #expect(memoryFiles(in: directory).isEmpty, "nothing is written yet")
                #expect(convert("kisha", in: session) > 0, "the dictionary answered nothing")

                clear_text(session: session, confirmedCandidate: 0)

                #expect(
                    !memoryFiles(in: directory).isEmpty,
                    "learn-then-commit must leave a memory store behind, got \(memoryFiles(in: directory))"
                )
            }
        }
    }

    /// The setting is the user's answer, and it has to reach the disk — not
    /// just the options. The same flow with learning off must write nothing.
    @Test("learning switched off writes nothing")
    func disabledLearningWritesNothing() {
        withRestoredConfig {
            withTemporaryDirectory { directory in
                converter = Self.engine
                config = EngineConfig()
                config.memoryDirectory = directory
                config.learningEnabled = false
                execURL = packageRoot.appendingPathComponent("azooKey_emoji_dictionary_storage")

                let session: Int64 = 0x7102_0006
                defer { remove_session(session: session) }

                #expect(convert("kisha", in: session) > 0)
                clear_text(session: session, confirmedCandidate: 0)

                #expect(
                    memoryFiles(in: directory).isEmpty,
                    "nothing may be written with learning off, got \(memoryFiles(in: directory))"
                )
            }
        }
    }

    /// The reset reports whether it happened, and refuses when there is
    /// nowhere to reset — it must never create the directory itself, because
    /// the Rust server is what locks that directory's permissions down.
    @Test("ResetLearning reports whether there was anything to reset")
    func resetReportsWhatItDid() {
        withRestoredConfig {
            withTemporaryDirectory { directory in
                converter = Self.engine
                config = EngineConfig()

                config.memoryDirectory = nil
                #expect(!reset_learning(), "no directory means nothing was reset")

                config.memoryDirectory = directory
                #expect(reset_learning())
            }
        }
    }
}
