import KanaKanjiConverterModule

/// The reading as the user should see it, with a pending romaji `n`
/// resolved to ん.
///
/// `ComposingText.convertTarget` keeps a trailing `n` as the latin letter
/// because it is still ambiguous — `nihon` can continue into `nihona` — and
/// that is the right thing for the engine to hold onto. But this string is
/// also what we hand back as the candidate text, as the remaining reading,
/// and as the reading F6 shows, so the letter reached the document: `nihon`
/// then Enter committed `にほn`, and conversion offered `仁保n` instead of
/// 日本 (issue #38).
///
/// On its own an `n` is ん, so it resolves. Other trailing consonants are
/// not kana by themselves and stay as they are — `nihok` shows and commits
/// `にほk`, which is what MS-IME does too. Uppercase `N` is left alone: the
/// roman2kana table is lowercase, so an `N` in the reading is literal text
/// the user asked for.
///
/// One character replaces one character, so this cannot shift the
/// ruby-length arithmetic in `constructCandidateString`.
func kanaReading(_ convertTarget: String) -> String {
    guard convertTarget.hasSuffix("n") else {
        return convertTarget
    }
    return convertTarget.dropLast() + "ん"
}

/// The text to ask the converter about: the session's own composing text,
/// except that a pending romaji `n` is completed first.
///
/// The engine deliberately leaves that `n` unresolved — `nihon` can still
/// become `nihona` — but the dictionary then never sees ニホン, so `nihon`
/// offered 仁保 and 二歩 and could not convert to 日本 at all (issue #38).
/// Completing it the way a user would, by typing the second `n`, is enough
/// to make the lookup work.
///
/// This is a copy: the session keeps its unresolved `n`, so the very next
/// keystroke can still turn it into な行.
///
/// The counts measured here stay usable against the session's text. The
/// surface count `ShrinkText` spends transfers exactly: the two readings
/// differ only in that last character, `n` for ん, one for one. The
/// keystroke count the client spends can be one too high for a candidate
/// that covers the ん — it counts the `n` we typed on its behalf — which is
/// harmless, the client's `raw_input` has nothing left to drop by then.
///
/// Only when the cursor is at the end: `insertAtCursorPosition` would
/// otherwise splice the `n` into the middle of the reading.
@MainActor func conversionTarget(_ composingText: ComposingText) -> ComposingText {
    guard composingText.isAtEndIndex, composingText.convertTarget.hasSuffix("n") else {
        return composingText
    }
    var completed = composingText
    completed.insertAtCursorPosition("n", inputStyle: .roman2kana)
    return completed
}

/// Commits the front of `text`, spending a count that came from outside —
/// the converter, or the client over the FFI — and is therefore not trusted.
///
/// Every part is clamped to what the text can actually give, because this
/// process is the engine every application on the desktop shares and each
/// unclamped end takes all of their compositions down with it:
///
/// - a negative count reaches `Array.removeFirst` / `String.dropFirst`, both
///   of which trap outright
/// - a surface count past the end of the reading is accepted quietly and
///   drives `convertTargetCursorPosition` negative (`prefixComplete`
///   subtracts the count it was given, not the count it could use), so the
///   next keystroke traps in `prefix(_:)` instead — a delayed trap in an
///   unrelated call. `.inputCount` clamps itself; `.surfaceCount` does not.
///
/// A composite spends its parts in order, each clamped against what the
/// preceding one left.
func spend(_ composingCount: ComposingCount, from text: inout ComposingText) {
    switch composingCount {
    case .inputCount(let count):
        text.prefixComplete(composingCount: .inputCount(min(max(0, count), text.input.count)))
    case .surfaceCount(let count):
        text.prefixComplete(
            composingCount: .surfaceCount(min(max(0, count), text.convertTarget.count))
        )
    case .composite(let lhs, let rhs):
        spend(lhs, from: &text)
        spend(rhs, from: &text)
    }
}

/// The reading left behind by every keystroke count, so index `k` holds the
/// result of spending `.inputCount(k)` on `target`.
///
/// The same table serves every candidate of one conversion request, which is
/// why it is built by the caller rather than inside `remainder`.
func shrinkReadings(of target: ComposingText) -> [String] {
    (0...target.input.count).map { count in
        var text = target
        text.prefixComplete(composingCount: .inputCount(count))
        return text.convertTarget
    }
}

/// What is left of `target` after a candidate covering `composingCount` is
/// committed, and how much of the reading that takes — measured twice,
/// because the engine and the client count in different units.
///
/// `surfaceCount` is kana, and it is the one that decides: `ShrinkText`
/// spends it on the session's own text, so the reading left composing is
/// exactly the `text` returned here. `inputCount` is romaji keystrokes and
/// exists for the client alone, which drops that many characters from its
/// `raw_input` (the string F9/F10 turn into latin).
///
/// A candidate's `composingCount` is neither number directly: since the
/// engine grew `ComposingCount` it is usually a surface count but can be an
/// input count or a composite of several, so let the engine spend it and
/// measure the difference.
///
/// For keystrokes, measuring what the engine spent does not work. The engine
/// looks candidates up by surface index too, so a clause boundary can fall
/// inside one romaji cluster: `henkansuru` is `{he}{nka}{nsuru}` and 「変換」
/// stops after 4 kana, in the middle of the last segment. Spending that
/// surface count re-encodes the whole segment into kana elements
/// (`forceGetInputCursorPosition`), so the input array shrinks for a reason
/// that has nothing to do with the candidate — `henkansuru` measured 7
/// keystrokes for a boundary that sits after 6.
///
/// So look the reading up instead: the answer is the keystroke count whose
/// own remaining reading is the one the candidate leaves. `readings` is
/// `shrinkReadings(of: target)`.
///
/// Not every boundary has one. かんしゃ is `{ka}{nsha}`, so no prefix of the
/// input leaves ゃ behind, and there the measurement stands — `raw_input`
/// keeps a keystroke too many or too few, which only shows if the user then
/// asks F9 to re-render the remainder as latin. The reading itself, on both
/// sides, stays right.
func remainder(
    of target: ComposingText,
    after composingCount: ComposingCount,
    readings: [String]
) -> (text: ComposingText, surfaceCount: Int, inputCount: Int) {
    var remaining = target
    spend(composingCount, from: &remaining)

    let surfaceCount = target.convertTarget.count - remaining.convertTarget.count
    let measured = min(max(0, target.input.count - remaining.input.count), target.input.count)
    let inputCount = readings.firstIndex(of: remaining.convertTarget) ?? measured

    return (remaining, surfaceCount, inputCount)
}

func remainder(
    of target: ComposingText,
    after composingCount: ComposingCount
) -> (text: ComposingText, surfaceCount: Int, inputCount: Int) {
    remainder(of: target, after: composingCount, readings: shrinkReadings(of: target))
}

func constructCandidateString(candidate: Candidate, hiragana: String) -> String {
    var remainingHiragana = hiragana
    var result = ""

    for data in candidate.data {
        if remainingHiragana.count < data.ruby.count {
            result += remainingHiragana
            break
        }
        remainingHiragana.removeFirst(data.ruby.count)
        result += data.word
    }

    return result
}
