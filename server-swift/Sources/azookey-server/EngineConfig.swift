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

    struct Zenzai: Codable {
        var enable: Bool?
        var profile: String?
        // snake_case because the JSON keys are whatever serde names the Rust
        // fields, and matching them by eye beats a CodingKeys table that has
        // to be kept in sync separately
        var inference_limit: Int?
        var topic: String?
        var style: String?
        var preference: String?
    }
}

/// Bounds for `inference_limit`. The floor is what the engine used while the
/// value was hardcoded; the ceiling is the converter's own default, which is
/// as high as anyone has reason to go. A hand-edited settings.json is the
/// reason this is clamped rather than trusted: the server converts on a
/// single thread, so an absurd limit stalls every application typing through
/// the IME, not just the one that triggered it.
let zenzaiInferenceLimitRange = 1...10

struct EngineConfig {
    var zenzaiEnabled = false
    var zenzaiProfile = ""
    var zenzaiInferenceLimit = zenzaiInferenceLimitRange.lowerBound
    var zenzaiTopic = ""
    var zenzaiStyle = ""
    var zenzaiPreference = ""
}

/// Reads and decodes `%APPDATA%\Azookey\settings.json`, or returns nil when
/// it cannot. A missing file is normal (first run) and a corrupt one must
/// not stop the engine, so both degrade to nil and the caller keeps the
/// current config. Only the file I/O and parsing live here; applying the
/// result to `config` stays with the LoadConfig export.
/// - Parameter appDataPath: overrides `%APPDATA%`. Only the tests pass it;
///   the engine always reads the environment.
func loadSettingsFile(appDataPath: String? = nil) -> SettingsFile? {
    guard let appDataPath = appDataPath ?? ProcessInfo.processInfo.environment["APPDATA"] else {
        return nil
    }
    let settingsPath = URL(filePath: appDataPath).appendingPathComponent("Azookey/settings.json")
    do {
        let data = try Data(contentsOf: settingsPath)
        return try JSONDecoder().decode(SettingsFile.self, from: data)
    } catch {
        enginePrint(level: .error, "failed to read settings: \(error)")
        return nil
    }
}

/// Applies a decoded settings file to the engine's `config`, overriding only
/// the keys that are present — a partial file keeps the current values, which
/// is what the settings app relies on when it writes one key at a time.
/// Split out of the `LoadConfig` export so it can be tested without a file.
@MainActor func applySettings(_ settings: SettingsFile?) {
    guard let zenzai = settings?.zenzai else { return }
    if let enable = zenzai.enable {
        config.zenzaiEnabled = enable
    }
    if let profile = zenzai.profile {
        config.zenzaiProfile = profile
    }
    if let limit = zenzai.inference_limit {
        config.zenzaiInferenceLimit = min(
            max(limit, zenzaiInferenceLimitRange.lowerBound),
            zenzaiInferenceLimitRange.upperBound
        )
    }
    if let topic = zenzai.topic {
        config.zenzaiTopic = topic
    }
    if let style = zenzai.style {
        config.zenzaiStyle = style
    }
    if let preference = zenzai.preference {
        config.zenzaiPreference = preference
    }
}

@MainActor func getOptions(context: String = "") -> ConvertRequestOptions {
    let zenzaiEnabled = config.zenzaiEnabled
    let zenzaiProfile = config.zenzaiProfile
    let zenzaiInferenceLimit = config.zenzaiInferenceLimit
    let zenzaiTopic = config.zenzaiTopic
    let zenzaiStyle = config.zenzaiStyle
    let zenzaiPreference = config.zenzaiPreference
    return ConvertRequestOptions(
        requireJapanesePrediction: .autoMix,
        requireEnglishPrediction: .disabled,
        keyboardLanguage: .ja_JP,
        learningType: .nothing,
        memoryDirectoryURL: placeholderDataDirectory,
        sharedContainerURL: placeholderDataDirectory,
        textReplacer: .init {
            return execURL.appendingPathComponent("EmojiDictionary").appendingPathComponent(emojiDictionaryFileName)
        },
        // nil, not [] — the converter substitutes its own default set, which
        // is what the engine used before the providers became an argument
        specialCandidateProviders: nil,
        // zenzai
        zenzaiMode: zenzaiEnabled ? .on(
            weight: execURL.appendingPathComponent("zenz.gguf"),
            inferenceLimit: zenzaiInferenceLimit,
            requestRichCandidates: true,
            personalizationMode: nil,
            versionDependentMode: .v3(
                .init(
                    profile: zenzaiProfile,
                    topic: zenzaiTopic,
                    style: zenzaiStyle,
                    preference: zenzaiPreference,
                    leftSideContext: context
                )
            )
        ) : .off,
        preloadDictionary: true,
        metadata: .init(versionString: "Azookey for Windows")
    )
}
