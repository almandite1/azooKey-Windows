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
/// `ComposingCount`, which can be a surface (kana) count, an input
/// (keystroke) count, or a composite of several. Both units have to come out
/// of it: ShrinkText spends the kana on the session's reading, and the client
/// spends the keystrokes on its own raw_input. Reinterpreting the enum's
/// payload as an input count would cut `にゅうりょく` (6 kana, 9 keystrokes)
/// three keystrokes short.
@Suite("remainder")
struct RemainderTests {
    @Test("a surface count is translated into keystrokes")
    func surfaceCountBecomesInputCount() {
        // にゅうりょく: 6 kana from 9 romaji letters
        let result = remainder(of: romaji("nyuuryoku"), after: .surfaceCount(6))

        #expect(result.text.convertTarget == "")
        #expect(result.surfaceCount == 6)
        #expect(result.inputCount == 9)
    }

    @Test("a partial candidate leaves the rest composing")
    func partialCandidate() {
        // にゅう = "nyuu", leaving りょく
        let result = remainder(of: romaji("nyuuryoku"), after: .surfaceCount(3))

        #expect(result.text.convertTarget == "りょく")
        #expect(result.surfaceCount == 3)
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

    /// ShrinkText spends the surface count on the session's own text, so
    /// that is the number that has to reproduce the reading we advertise.
    @Test("the surface count is what ShrinkText would spend")
    func agreesWithShrink() {
        let target = romaji("nyuuryoku")
        let result = remainder(of: target, after: .surfaceCount(3))

        var shrunk = target
        shrunk.prefixComplete(composingCount: .surfaceCount(result.surfaceCount))

        #expect(shrunk.convertTarget == result.text.convertTarget)
        #expect(shrunk.convertTarget == "りょく")
    }

    @Test("an input count passes through unchanged")
    func inputCountIsIdentity() {
        #expect(remainder(of: romaji("nihon"), after: .inputCount(4)).inputCount == 4)
    }

    /// The engine looks candidates up by surface index too, so a clause
    /// boundary can fall inside a romaji cluster: へんかん|する splits
    /// `{nsuru}` — the ん belongs to the same independent segment as する.
    /// Measuring what `prefixComplete` took overcounts there, because
    /// spending a surface count re-encodes that segment into kana and the
    /// input array shrinks for a reason unrelated to the candidate.
    @Test("a clause boundary inside a cluster still yields spendable keystrokes")
    func boundaryInsideCluster() {
        // へんかんする: {he}{nka}{nsuru}; 「変換」covers the first 4 kana,
        // which is inside the last segment
        let target = romaji("henkansuru")
        let result = remainder(of: target, after: .surfaceCount(4))

        #expect(result.text.convertTarget == "する")
        #expect(result.surfaceCount == 4)
        // h,e,n,k,a,n — not 7, which is what the re-encoded input measures
        #expect(result.inputCount == 6)

        // both units have to land on the same reading here
        var shrunk = target
        shrunk.prefixComplete(composingCount: .inputCount(result.inputCount))
        #expect(shrunk.convertTarget == "する")
    }

    /// Some boundaries cannot be expressed as a number of keystrokes at all:
    /// かんしゃ is `{ka}{nsha}`, so no prefix of the input leaves ゃ behind —
    /// 「監視」on that reading is exactly the case the keystroke count could
    /// not carry, and the reason the surface count exists.
    @Test("an inexpressible boundary survives as kana")
    func inexpressibleBoundary() {
        let target = romaji("kansha")
        let result = remainder(of: target, after: .surfaceCount(3))

        #expect(result.text.convertTarget == "ゃ")
        #expect(result.surfaceCount == 3)

        var shrunk = target
        shrunk.prefixComplete(composingCount: .surfaceCount(result.surfaceCount))
        #expect(shrunk.convertTarget == "ゃ")
    }

    /// The counts come from the converter, not from us. `removeFirst` and
    /// `dropFirst` both trap on a negative one, and this process is the whole
    /// engine — every application's composition dies with it.
    @Test("a negative count from the converter does not trap")
    func negativeCountsAreClamped() {
        #expect(remainder(of: romaji("nihon"), after: .inputCount(-1)).inputCount == 0)
        #expect(remainder(of: romaji("nihon"), after: .surfaceCount(-3)).surfaceCount == 0)
        #expect(
            remainder(
                of: romaji("nihon"),
                after: .composite(lhs: .surfaceCount(-1), rhs: .inputCount(-1))
            ).surfaceCount == 0
        )
    }

    /// A surface count past the end of the reading is the dangerous one: it
    /// is accepted quietly and leaves `convertTargetCursorPosition` negative,
    /// so the trap comes later, on the next keystroke, in an unrelated call.
    /// This is what took the live server down (exit code 0xc000001d) when a
    /// shrink offset outran the composition.
    @Test("an overlong surface count leaves a usable composition")
    func overlongSurfaceCountKeepsTheCursorValid() {
        var text = romaji("kakikukeko")

        spend(.surfaceCount(999), from: &text)
        #expect(text.convertTarget == "")

        // the delayed trap: prefix(negative cursor)
        text.insertAtCursorPosition("a", inputStyle: .roman2kana)
        #expect(text.convertTarget == "あ")
    }

    /// An overlong count is the converter's business too; prefixComplete
    /// clamps it, and the measured keystroke count must not exceed the input.
    @Test("an overlong count consumes everything and no more")
    func overlongCountsAreClamped() {
        let result = remainder(of: romaji("nihon"), after: .surfaceCount(99))

        #expect(result.text.convertTarget == "")
        #expect(result.surfaceCount == 3) // にほn — the whole reading, no more
        #expect(result.inputCount == 5)
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
