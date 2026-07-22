import Testing
import Foundation
import KanaKanjiConverterModule
@testable import azookey_server

/// The counting bugs this suite guards against are not visible in a unit
/// test with a hand-written `ComposingCount`: they only appear once the real
/// dictionary starts proposing clause boundaries we did not think of. The
/// engine catch-up (7d5dd99 → bbef9d2d) introduced exactly that — the new
/// lattice looks candidates up by surface index, so 「変換」is offered for
/// へんかんする and 「監視」for かんしゃ — and the keystroke count we used to
/// send left うる and あ behind.
///
/// So convert real readings with the dictionary in the repository and hold
/// every candidate to the contract the client depends on:
/// **what the candidate window shows as the remaining reading is what
/// ShrinkText leaves composing, and the two together are the reading the
/// user typed.**
///
/// Zenzai stays off (the default `config`), so this needs no model file and
/// is deterministic.
@Suite("real dictionary")
@MainActor
struct RealDictionaryTests {
    /// The package root, from this file's own path — the dictionaries are
    /// submodules of the repository, so they are always next to it.
    private static let root = URL(filePath: #filePath)
        .deletingLastPathComponent()  // azookey-serverTests
        .deletingLastPathComponent()  // Tests
        .deletingLastPathComponent()  // server-swift

    private static let engine = KanaKanjiConverter(
        dictionaryURL: root.appendingPathComponent("azooKey_dictionary_storage/Dictionary"),
        preloadDictionary: true
    )

    private func composing(_ input: String) -> ComposingText {
        var text = ComposingText()
        text.insertAtCursorPosition(input, inputStyle: .roman2kana)
        return text
    }

    @Test(
        "every candidate's counts reconstruct the reading",
        arguments: [
            "henkansuru",   // {he}{nka}{nsuru} — 「変換」ends inside the last cluster
            "kansha",       // {ka}{nsha} — 「監視」ends inside ん-し-ゃ, no keystroke count can say so
            "nyuuryokudekiru",
            "kyouhaame",
            "nihonngo",
            "watashiha"
        ]
    )
    func countsReconstructTheReading(_ input: String) {
        // exactly what GetComposedText does
        execURL = Self.root.appendingPathComponent("azooKey_emoji_dictionary_storage")
        let target = conversionTarget(composing(input))
        let reading = kanaReading(target.convertTarget)
        let readings = shrinkReadings(of: target)
        let converted = Self.engine.requestCandidates(target, options: getOptions())

        #expect(!converted.mainResults.isEmpty, "the dictionary answered nothing for \(input)")

        for candidate in converted.mainResults {
            let result = remainder(of: target, after: candidate.composingCount, readings: readings)
            let rest = kanaReading(result.text.convertTarget)
            let covered = candidate.data.map(\.ruby).joined().count

            // what ShrinkText will actually do with the count we send
            var session = composing(input)
            session.prefixComplete(composingCount: .surfaceCount(result.surfaceCount))
            #expect(
                kanaReading(session.convertTarget) == rest,
                "\(input): the reading left after committing \(candidate.text) is not the one shown"
            )

            if covered > reading.count {
                // a prediction: it proposes text past what was typed, so it
                // covers the whole reading and nothing is left composing
                #expect(rest.isEmpty, "\(input): the prediction \(candidate.text) left \(rest)")
            } else {
                #expect(
                    reading.hasSuffix(rest) && covered + rest.count == reading.count,
                    "\(input): \(candidate.text) covers \(covered) kana but left \(rest)"
                )
            }
        }
    }
}
