import KanaKanjiConverterModule
import Foundation

// Typed mirror of the settings schema owned by the Rust side
// (crates/shared/src/lib.rs: AppConfig / ZenzaiConfig) — keep the field
// names in sync. Keys the engine does not read (version, zenzai.backend)
// are simply not declared; JSONDecoder ignores extra JSON keys. Every
// field is optional so a hand-edited or partial settings.json degrades
// per-key instead of failing the whole parse.
struct SettingsFile: Codable {
    var zenzai: Zenzai?
    var conversion: Conversion?

    struct Conversion: Codable {
        var half_width_kana: Bool?
        var full_width_roman: Bool?
        var english_in_roman_input: Bool?
        var typo_correction: String?
        var typography: Bool?
    }

    struct Zenzai: Codable {
        var enable: Bool?
        var profile: String?
        // snake_case because the JSON keys are whatever serde names the Rust
        // fields, and matching them by eye beats a CodingKeys table that has
        // to be kept in sync separately
        var inference_limit: Int?
        var context_size: UInt32?
        var topic: String?
        var style: String?
        var preference: String?
    }
}

// Decoded key by key, tolerating a value of the wrong TYPE and not just an
// absent one.
//
// `Optional` properties alone only give the second: with the synthesized
// decoder, `"enable": "true"` threw and took the whole document with it —
// the same trap the Rust reader had, where one quoted value cost every
// setting. The Rust side normalizes what it sends here, so a mismatch should
// not arrive any more; this is what makes that a belt rather than the only
// thing holding the trousers up, and it means both ends of the same file
// genuinely behave alike.
extension SettingsFile.Zenzai {
    enum CodingKeys: String, CodingKey {
        case enable, profile, inference_limit, context_size, topic, style, preference
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        // `try?` per key: a mismatch becomes nil for that key alone, which
        // applySettings then treats exactly like an absent one — the current
        // value is kept
        enable = try? container.decodeIfPresent(Bool.self, forKey: .enable)
        profile = try? container.decodeIfPresent(String.self, forKey: .profile)
        inference_limit = try? container.decodeIfPresent(Int.self, forKey: .inference_limit)
        context_size = try? container.decodeIfPresent(UInt32.self, forKey: .context_size)
        topic = try? container.decodeIfPresent(String.self, forKey: .topic)
        style = try? container.decodeIfPresent(String.self, forKey: .style)
        preference = try? container.decodeIfPresent(String.self, forKey: .preference)
    }
}

extension SettingsFile.Conversion {
    enum CodingKeys: String, CodingKey {
        case half_width_kana, full_width_roman, english_in_roman_input, typo_correction, typography
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        half_width_kana = try? container.decodeIfPresent(Bool.self, forKey: .half_width_kana)
        full_width_roman = try? container.decodeIfPresent(Bool.self, forKey: .full_width_roman)
        english_in_roman_input = try? container.decodeIfPresent(Bool.self, forKey: .english_in_roman_input)
        typo_correction = try? container.decodeIfPresent(String.self, forKey: .typo_correction)
        typography = try? container.decodeIfPresent(Bool.self, forKey: .typography)
    }
}

extension SettingsFile {
    enum CodingKeys: String, CodingKey {
        case zenzai, conversion
    }

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        // a `zenzai` that is not an object at all costs the section, not the
        // document — same as the Rust reader
        zenzai = try? container.decodeIfPresent(Zenzai.self, forKey: .zenzai)
        conversion = try? container.decodeIfPresent(Conversion.self, forKey: .conversion)
    }
}

/// Bounds for `inference_limit`. The floor is what the engine used while the
/// value was hardcoded; the ceiling is the converter's own default, which is
/// as high as anyone has reason to go. A hand-edited settings.json is the
/// reason this is clamped rather than trusted: the server converts on a
/// single thread, so an absurd limit stalls every application typing through
/// the IME, not just the one that triggered it.
let zenzaiInferenceLimitRange = 1...10

/// Bounds for `context_size`, mirroring `ZENZAI_CONTEXT_SIZE` on the Rust side.
/// The floor is what the converter hardcoded before the size was a parameter;
/// the ceiling keeps a hand-edited settings.json from asking llama.cpp for an
/// allocation that may land in VRAM. Input longer than the cache is not
/// evaluated by the model at all — the converter falls back to statistical
/// conversion — so a small value costs quality, not correctness.
///
/// The converter clamps once more, to the training context of the model it
/// loads (1024 for the zenz weights shipped here), because that is what sizes
/// gpt2's position embedding table. So the effective ceiling is the model's,
/// and this range only has to be sane.
let zenzaiContextSizeRange: ClosedRange<UInt32> = 512...4096

/// `conversion.typo_correction` as the converter wants it. Anything the file
/// does not spell correctly becomes `.automatic`, which is what the engine used
/// before the setting existed — an unreadable value must not change behaviour.
func typoCorrectionMode(_ name: String?) -> ConvertRequestOptions.TypoCorrectionMode {
    switch name {
    case "enabled": .enabled
    case "disabled": .disabled
    default: .automatic
    }
}

struct EngineConfig {
    var zenzaiEnabled = false
    // Every one of these defaults to what the engine did before it was
    // settable, so the section can be added to a settings file without
    // changing a single conversion.
    var halfWidthKanaCandidate = false
    var fullWidthRomanCandidate = false
    var englishCandidateInRoman2KanaInput = false
    var typoCorrection: ConvertRequestOptions.TypoCorrectionMode = .automatic
    var typographyCandidates = false
    var zenzaiProfile = ""
    var zenzaiInferenceLimit = zenzaiInferenceLimitRange.lowerBound
    /// Same default as `ZenzaiConfig::default` on the Rust side.
    var zenzaiContextSize: UInt32 = 1024
    var zenzaiTopic = ""
    var zenzaiStyle = ""
    var zenzaiPreference = ""
}

/// Decodes the settings document the Rust side hands over, or nil when it
/// cannot be read as one.
///
/// The engine no longer opens `%APPDATA%\Azookey\settings.json` for itself.
/// It used to, while the Rust caller read the same file in the same
/// `UpdateConfig`, so a save landing between the two reads applied half of one
/// version and half of the other — and the path was spelled out on both sides
/// of the FFI boundary. Now there is one reader, and this decodes what it
/// passes.
///
/// Individual keys are tolerated one at a time (see the decoder below), which
/// is the same thing the Rust decoder does; nil here means the text was not
/// JSON at all.
func decodeSettings(_ json: String) -> SettingsFile? {
    guard let data = json.data(using: .utf8) else {
        enginePrint(level: .error, "the settings document is not valid UTF-8")
        return nil
    }
    do {
        return try JSONDecoder().decode(SettingsFile.self, from: data)
    } catch {
        enginePrint(level: .error, "failed to decode settings: \(error)")
        return nil
    }
}

/// Applies a decoded settings file to the engine's `config`, overriding only
/// the keys that are present — a partial file keeps the current values, which
/// is what the settings app relies on when it writes one key at a time.
/// Split out of the `LoadConfig` export so it can be tested without a file.
@MainActor func applySettings(_ settings: SettingsFile?) {
    guard let settings else { return }

    // Applied before the zenzai section and independently of it. The guard
    // below returns when there is no `zenzai` object, and a document that
    // carries only `conversion` -- which is exactly what the settings app
    // sends when one of these is toggled -- would otherwise be dropped whole.
    if let conversion = settings.conversion {
        if let value = conversion.half_width_kana { config.halfWidthKanaCandidate = value }
        if let value = conversion.full_width_roman { config.fullWidthRomanCandidate = value }
        if let value = conversion.english_in_roman_input {
            config.englishCandidateInRoman2KanaInput = value
        }
        if let value = conversion.typo_correction { config.typoCorrection = typoCorrectionMode(value) }
        if let value = conversion.typography { config.typographyCandidates = value }
    }

    guard let zenzai = settings.zenzai else { return }
    if let enable = zenzai.enable {
        config.zenzaiEnabled = enable
    }
    // Clamped even though the writer now clamps too: settings.json is a text
    // file people edit, and this is the side that hands the number to the
    // converter. The server converts on a single thread, so an absurd limit
    // stalls every application typing through the IME at once.
    if let limit = zenzai.inference_limit {
        config.zenzaiInferenceLimit = min(
            max(limit, zenzaiInferenceLimitRange.lowerBound),
            zenzaiInferenceLimitRange.upperBound
        )
    }
    // Clamped for the same reason as the limit above, and read by the
    // converter only while it loads the model — changing it mid-session
    // leaves the already-loaded model alone until the engine restarts.
    if let contextSize = zenzai.context_size {
        config.zenzaiContextSize = min(
            max(contextSize, zenzaiContextSizeRange.lowerBound),
            zenzaiContextSizeRange.upperBound
        )
    }
    // The four v3 context strings, paired with where each one lands. A table
    // rather than four more `if let` blocks: they differ only in name, and the
    // repetition is where a copy-paste puts `topic` into `style`. Local, not a
    // top-level `let`: a global of this type is not Sendable enough for strict
    // concurrency, and this is the only caller.
    let contextFields: [(
        source: KeyPath<SettingsFile.Zenzai, String?>,
        destination: WritableKeyPath<EngineConfig, String>
    )] = [
        (\.profile, \.zenzaiProfile),
        (\.topic, \.zenzaiTopic),
        (\.style, \.zenzaiStyle),
        (\.preference, \.zenzaiPreference),
    ]
    for field in contextFields {
        if let value = zenzai[keyPath: field.source] {
            config[keyPath: field.destination] = value
        }
    }
}

@MainActor func getOptions(context: String = "") -> ConvertRequestOptions {
    ConvertRequestOptions(
        requireJapanesePrediction: .autoMix,
        requireEnglishPrediction: .disabled,
        keyboardLanguage: .ja_JP,
        englishCandidateInRoman2KanaInput: config.englishCandidateInRoman2KanaInput,
        fullWidthRomanCandidate: config.fullWidthRomanCandidate,
        halfWidthKanaCandidate: config.halfWidthKanaCandidate,
        learningType: .nothing,
        memoryDirectoryURL: placeholderDataDirectory,
        sharedContainerURL: placeholderDataDirectory,
        textReplacer: .init {
            return execURL.appendingPathComponent("EmojiDictionary").appendingPathComponent(emojiDictionaryFileName)
        },
        // nil, not [] — the converter substitutes its own default set, which is
        // what the engine used before the providers became an argument, and it
        // means a set the converter grows upstream is picked up for free.
        //
        // Typography is the one provider outside that set, so asking for it
        // means naming the whole list and giving that up. Only when it is
        // switched on: with it off this stays nil and keeps following upstream.
        specialCandidateProviders: config.typographyCandidates
            ? KanaKanjiConverter.defaultSpecialCandidateProviders + [TypographySpecialCandidateProvider()]
            : nil,
        // zenzai — read straight off `config`; this function is @MainActor, so
        // the six local copies that used to sit above it bought nothing but a
        // second place for a name to be wrong
        zenzaiMode: config.zenzaiEnabled ? .on(
            weight: execURL.appendingPathComponent("zenz.gguf"),
            inferenceLimit: config.zenzaiInferenceLimit,
            requestRichCandidates: true,
            personalizationMode: nil,
            versionDependentMode: .v3(
                .init(
                    profile: config.zenzaiProfile,
                    topic: config.zenzaiTopic,
                    style: config.zenzaiStyle,
                    preference: config.zenzaiPreference,
                    leftSideContext: context
                )
            ),
            // Passed on every conversion, but the converter only reads it
            // while loading the model, so in practice this is whatever the
            // setting said when the engine started.
            contextSize: config.zenzaiContextSize
        ) : .off,
        preloadDictionary: true,
        typoCorrectionMode: config.typoCorrection,
        metadata: .init(versionString: "Azookey for Windows")
    )
}
