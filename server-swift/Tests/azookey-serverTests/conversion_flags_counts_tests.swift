import Testing
import Foundation
import KanaKanjiConverterModule
@testable import azookey_server

/// The counting contract of `real dictionary`, re-run with each candidate
/// source the `conversion` settings can switch on.
///
/// That suite pins the contract the client depends on — what the candidate
/// window shows as the remaining reading is what ShrinkText leaves composing,
/// and the two together are the reading the user typed — but only for the
/// candidates the engine produced with everything at its default. A setting
/// that adds a new KIND of candidate is exactly the kind of change that can
/// break it, because a candidate whose text is a different length from its
/// reading has to report the reading's length, not its own.
@Suite("conversion flag candidate counts")
@MainActor
struct ConversionFlagCountTests {
    private static let engine = KanaKanjiConverter(
        dictionaryURL: packageRoot.appendingPathComponent("azooKey_dictionary_storage/Dictionary"),
        preloadDictionary: true
    )

    /// Readings whose half-width form is not the same length as the reading:
    /// ｶﾞ and ﾊﾟ are two scalars each, ｼｮ is two characters where しょ is two
    /// as well but ょ is small. If the count travelling to the client follows
    /// the candidate instead of the reading, these are where it shows.
    nonisolated static let inputs = [
        "hankaku",      // はんかく -> ﾊﾝｶｸ, same length
        "gakkou",       // がっこう -> ｶﾞｯｺｳ, one scalar longer
        "pinpon",       // ぴんぽん -> ﾋﾟﾝﾎﾟﾝ, two scalars longer
        "shochou",      // しょちょう -> ｼｮﾁｮｳ
        "abc",
        "world",
    ]

    /// `config` is process-global engine state and these tests write it.
    private func withRestoredConfig(_ body: () -> Void) {
        let saved = config
        defer { config = saved }
        body()
    }

    private static func check(_ input: String, _ label: String) {
        let target = conversionTarget(romaji(input))
        let reading = kanaReading(target.convertTarget)
        let readings = shrinkReadings(of: target)
        let converted = Self.engine.requestCandidates(target, options: getOptions())

        for candidate in converted.mainResults {
            let result = remainder(of: target, after: candidate.composingCount, readings: readings)
            let rest = kanaReading(result.text.convertTarget)

            var session = romaji(input)
            session.prefixComplete(composingCount: .surfaceCount(result.surfaceCount))
            #expect(
                kanaReading(session.convertTarget) == rest,
                "\(label)/\(input): committing \(candidate.text) leaves \(kanaReading(session.convertTarget)) composing but the window shows \(rest)"
            )
            #expect(
                reading.hasSuffix(rest),
                "\(label)/\(input): \(candidate.text) left \(rest), which is not a tail of \(reading)"
            )
        }
    }

    @Test("half-width kana candidates keep the counting contract", arguments: inputs)
    func halfWidthKana(_ input: String) {
        withRestoredConfig {
            execURL = packageRoot.appendingPathComponent("azooKey_emoji_dictionary_storage")
            config = EngineConfig()
            config.halfWidthKanaCandidate = true
            Self.check(input, "half-width kana")
        }
    }

    @Test("full-width roman candidates keep the counting contract", arguments: inputs)
    func fullWidthRoman(_ input: String) {
        withRestoredConfig {
            execURL = packageRoot.appendingPathComponent("azooKey_emoji_dictionary_storage")
            config = EngineConfig()
            config.fullWidthRomanCandidate = true
            Self.check(input, "full-width roman")
        }
    }

    @Test("english candidates keep the counting contract", arguments: inputs)
    func englishInRomanInput(_ input: String) {
        withRestoredConfig {
            execURL = packageRoot.appendingPathComponent("azooKey_emoji_dictionary_storage")
            config = EngineConfig()
            config.englishCandidateInRoman2KanaInput = true
            Self.check(input, "english in roman input")
        }
    }

    @Test("typography candidates keep the counting contract", arguments: inputs)
    func typography(_ input: String) {
        withRestoredConfig {
            execURL = packageRoot.appendingPathComponent("azooKey_emoji_dictionary_storage")
            config = EngineConfig()
            config.typographyCandidates = true
            Self.check(input, "typography")
        }
    }
}
