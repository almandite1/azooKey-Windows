import Testing
import Foundation
import KanaKanjiConverterModule
@testable import azookey_server

/// What the `conversion` settings have to be held to, in both directions.
///
/// **That they do something.** A setting is finished when the candidate list
/// changes, not when the value reaches the converter. `english_in_roman_input`
/// shipped and was closed on the strength of the second: it reached the
/// converter, which on Windows handed it to a spell checker that returns nil
/// on every non-Apple platform, so the list never changed and the switch was
/// furniture. A test that only checked the values would have passed then too.
///
/// **That what they add still counts correctly.** `real dictionary` pins the
/// contract the client depends on -- what the candidate window shows as the
/// remaining reading is what ShrinkText leaves composing, and the two together
/// are the reading the user typed -- but only for the default candidate set. A
/// candidate whose text is a different length from its reading has to report
/// the reading's, and half-width kana is exactly that case.
@Suite("conversion flag candidates")
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

    @Test("typography candidates keep the counting contract", arguments: inputs)
    func typography(_ input: String) {
        withRestoredConfig {
            execURL = packageRoot.appendingPathComponent("azooKey_emoji_dictionary_storage")
            config = EngineConfig()
            config.typographyCandidates = true
            Self.check(input, "typography")
        }
    }

    private static func texts(_ input: String) -> [String] {
        Self.engine.requestCandidates(romaji(input), options: getOptions()).mainResults.map(\.text)
    }

    private static func enabling(_ flag: String) -> EngineConfig {
        var config = EngineConfig()
        switch flag {
        case "half-width kana": config.halfWidthKanaCandidate = true
        case "full-width roman": config.fullWidthRomanCandidate = true
        default: config.typographyCandidates = true
        }
        return config
    }

    /// Each flag, an input that should make it fire, and something it must put
    /// in the list. The engine only ever composes with `.roman2kana`, so these
    /// are readings a user could actually type -- note that typography needs a
    /// composing text that is entirely roman letters or digits, which after
    /// roman-to-kana means one with no vowels in it.
    @Test(
        "each flag adds the candidates it promises",
        arguments: [
            ("half-width kana", "hankaku", "\u{FF8A}\u{FF9D}\u{FF76}\u{FF78}"),
            ("full-width roman", "bcd", "\u{FF42}\u{FF43}\u{FF44}"),
            ("typography", "bcd", "\u{1D41B}\u{1D41C}\u{1D41D}"),
        ]
    )
    func flagsAddCandidates(_ flag: String, _ input: String, _ expected: String) {
        withRestoredConfig {
            execURL = packageRoot.appendingPathComponent("azooKey_emoji_dictionary_storage")
            config = EngineConfig()
            #expect(!Self.texts(input).contains(expected), "\(flag): it must need the flag")

            config = Self.enabling(flag)
            #expect(
                Self.texts(input).contains(expected),
                "\(flag): turning it on added nothing usable to \(input) -- the setting is furniture"
            )
        }
    }

    /// The other direction: a flag must not smuggle candidates into a
    /// composition it has no business touching. Typography is the one with a
    /// real precondition -- anything with a kana in it is not its business.
    @Test("typography stays out of ordinary Japanese conversion")
    func typographyDoesNotFireOnKana() {
        withRestoredConfig {
            execURL = packageRoot.appendingPathComponent("azooKey_emoji_dictionary_storage")
            config = EngineConfig()
            let off = Self.texts("hankaku")
            config = EngineConfig()
            config.typographyCandidates = true
            #expect(Self.texts("hankaku") == off, "a kana reading is not roman letters")
        }
    }
}
