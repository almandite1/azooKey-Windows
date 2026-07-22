import Testing
import KanaKanjiConverterModule
@testable import azookey_server

/// Issue #38, second half: normalising the reading we *display* was not
/// enough. The converter is asked about the session's own ComposingText,
/// whose pending `n` is still the latin letter, so the dictionary never saw
/// ニホン and `nihon` + Space offered 仁保 / 二歩 but never 日本.
@Suite("conversionTarget")
@MainActor
struct ConversionTargetTests {
    private func romaji(_ input: String) -> ComposingText {
        var text = ComposingText()
        text.insertAtCursorPosition(input, inputStyle: .roman2kana)
        return text
    }

    @Test("a pending n is completed for the lookup")
    func pendingNIsCompleted() {
        #expect(romaji("nihon").convertTarget == "にほn", "precondition")
        #expect(conversionTarget(romaji("nihon")).convertTarget == "にほん")
    }

    /// The session must keep its unresolved `n`, or the next keystroke can
    /// no longer turn it into な行 — `nihon` then `a` has to give にほな.
    @Test("the original text is left alone")
    func originalIsUntouched() {
        let original = romaji("nihon")

        _ = conversionTarget(original)

        #expect(original.convertTarget == "にほn")
    }

    @Test("a reading that needs nothing is returned as is")
    func finishedReadingsAreUnchanged() {
        #expect(conversionTarget(romaji("nihonn")).convertTarget == "にほん")
        #expect(conversionTarget(romaji("nihona")).convertTarget == "にほな")
        #expect(conversionTarget(romaji("nihok")).convertTarget == "にほk")
        #expect(conversionTarget(ComposingText()).convertTarget == "")
    }

    /// correspondingCount is measured against the completed copy but spent
    /// on the session's text, which works because candidate boundaries fall
    /// on kana boundaries: a candidate stopping before the ん counts the
    /// same inputs in both.
    @Test("input counts line up below the pending n")
    func inputCountsAgreeBeforeTheN() {
        var session = romaji("nihon")
        var completed = conversionTarget(session)

        // "にほ" — two kana, the same four romaji inputs in either text
        session.prefixComplete(composingCount: .inputCount(4))
        completed.prefixComplete(composingCount: .inputCount(4))

        #expect(session.convertTarget == "n")
        #expect(completed.convertTarget == "ん")
    }
}

/// The engine reports how much of the reading a candidate covers as a
/// `ComposingCount`, which is a *surface* (kana) count for nearly every real
/// candidate, while the number crossing the FFI has to stay an input
/// (keystroke) count — the client spends it on its own raw_input and hands it
/// back to ShrinkText. Reinterpreting the enum's payload as an input count
/// would cut `にゅうりょく` (6 kana, 9 keystrokes) three keystrokes short.
@Suite("remainder")
struct RemainderTests {
    private func romaji(_ input: String) -> ComposingText {
        var text = ComposingText()
        text.insertAtCursorPosition(input, inputStyle: .roman2kana)
        return text
    }

    @Test("a surface count is translated into keystrokes")
    func surfaceCountBecomesInputCount() {
        // にゅうりょく: 6 kana from 9 romaji letters
        let result = remainder(of: romaji("nyuuryoku"), after: .surfaceCount(6))

        #expect(result.text.convertTarget == "")
        #expect(result.inputCount == 9)
    }

    @Test("a partial candidate leaves the rest composing")
    func partialCandidate() {
        // にゅう = "nyuu", leaving りょく
        let result = remainder(of: romaji("nyuuryoku"), after: .surfaceCount(3))

        #expect(result.text.convertTarget == "りょく")
        #expect(result.inputCount == 4)
    }

    @Test("a composite count spends every part")
    func compositeCount() {
        let result = remainder(
            of: romaji("nyuuryoku"),
            after: .composite(lhs: .inputCount(0), rhs: .surfaceCount(6))
        )

        #expect(result.inputCount == 9)
    }

    /// ShrinkText spends the number this returns as `.inputCount`, so the two
    /// have to agree on what it means.
    @Test("the count is what ShrinkText would spend")
    func agreesWithShrink() {
        let target = romaji("nyuuryoku")
        let count = remainder(of: target, after: .surfaceCount(3)).inputCount

        var shrunk = target
        shrunk.prefixComplete(composingCount: .inputCount(count))

        #expect(shrunk.convertTarget == "りょく")
    }

    @Test("an input count passes through unchanged")
    func inputCountIsIdentity() {
        #expect(remainder(of: romaji("nihon"), after: .inputCount(4)).inputCount == 4)
    }
}

/// Issue #38: `ComposingText.convertTarget` keeps a pending romaji `n` as
/// the latin letter, and that string is what we hand back as the candidate
/// text, the remaining reading and the reading F6 shows — so `nihon` then
/// Enter committed `にほn` into the document.
@Suite("kanaReading")
struct KanaReadingTests {
    @Test("a trailing n becomes ん")
    func trailingNResolves() {
        #expect(kanaReading("にほn") == "にほん")
        #expect(kanaReading("かんばn") == "かんばん")
        #expect(kanaReading("n") == "ん")
    }

    /// The replacement must stay one-for-one: `constructCandidateString`
    /// walks the reading by each candidate ruby's length, so a reading that
    /// changed length would misalign every word after it.
    @Test("the reading keeps its length")
    func lengthIsUnchanged() {
        #expect(kanaReading("にほn").count == "にほn".count)
    }

    /// Other trailing consonants are not kana on their own. MS-IME shows
    /// and commits those raw too, so they must pass through untouched.
    @Test("an incomplete consonant is left alone")
    func otherConsonantsPassThrough() {
        #expect(kanaReading("にほk") == "にほk")
        #expect(kanaReading("にほsh") == "にほsh")
    }

    /// The roman2kana table is lowercase, so an uppercase N in the reading
    /// is literal text the user asked for, not a pending kana.
    @Test("an uppercase N is literal text")
    func uppercaseIsLiteral() {
        #expect(kanaReading("にほN") == "にほN")
    }

    /// Readings that are already kana — including one that legitimately
    /// ends in ん — must not be touched.
    @Test("a finished reading is unchanged")
    func finishedReadingsAreUnchanged() {
        #expect(kanaReading("にほん") == "にほん")
        #expect(kanaReading("にほな") == "にほな")
        #expect(kanaReading("") == "")
    }
}
