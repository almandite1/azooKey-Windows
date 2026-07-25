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
    }
}

struct EngineConfig {
    var zenzaiEnabled = false
    var zenzaiProfile = ""
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
}

@MainActor func getOptions(context: String = "") -> ConvertRequestOptions {
    let zenzaiEnabled = config.zenzaiEnabled
    let zenzaiProfile = config.zenzaiProfile
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
                    leftSideContext: context
                )
            )
        ) : .off,
        preloadDictionary: true,
        metadata: .init(versionString: "Azookey for Windows")
    )
}
