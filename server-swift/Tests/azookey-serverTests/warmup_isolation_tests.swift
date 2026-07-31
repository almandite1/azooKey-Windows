import Testing
import Foundation
import KanaKanjiConverterModule
@testable import azookey_server

/// The warm-up conversion is meant to be invisible: it exists only so that the
/// gguf load and the first inference do not land on a keystroke. What it must
/// not do is change what the first real conversion returns.
///
/// It converts into the converter's default session and never resets it, so
/// the previous input, the lattice, the Zenzai cache and the prediction caches
/// all still describe the warm-up's text when the first real request arrives.
/// This suite pins that down, because the failure it would produce — worse
/// candidates, but only ever for the first conversion after the engine starts —
/// is nearly impossible to catch by hand.
@Suite("warm-up isolation")
@MainActor
struct WarmUpIsolationTests {
    private static func freshEngine() -> KanaKanjiConverter {
        KanaKanjiConverter(
            dictionaryURL: packageRoot.appendingPathComponent("azooKey_dictionary_storage/Dictionary"),
            preloadDictionary: true
        )
    }

    /// The same reading, converted by two engines that differ only in whether a
    /// warm-up ran first. Zenzai is off (no model in the test environment), so
    /// this covers the dictionary path; a difference here is state leaking, not
    /// model nondeterminism.
    @Test(
        "a warmed engine's first conversion matches a cold one's",
        arguments: [
            "nyuuryokushimashita",
            "kyouhaiitenkidesune",
            "henkankekkagaokashii"
        ]
    )
    func warmUpDoesNotChangeTheFirstConversion(_ reading: String) {
        execURL = packageRoot.appendingPathComponent("azooKey_emoji_dictionary_storage")

        let cold = Self.freshEngine()
        let expected = cold.requestCandidates(romaji(reading), options: getOptions())
            .mainResults.prefix(5).map(\.text)

        let warmed = Self.freshEngine()
        _ = warmed.requestCandidates(
            romaji("kesahaiitenkinanodekouenmadearuiteikimashita"),
            options: getOptions()
        )
        let actual = warmed.requestCandidates(romaji(reading), options: getOptions())
            .mainResults.prefix(5).map(\.text)

        #expect(
            actual == expected,
            "the warm-up changed the first conversion of \(reading): \(actual) instead of \(expected)"
        )
    }
}
