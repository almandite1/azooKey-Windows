import Testing
import Foundation
import KanaKanjiConverterModule
@testable import azookey_server

/// `settings.json` is written by the Rust side (crates/shared: AppConfig /
/// ZenzaiConfig) and read here, by a decoder that declares only the keys the
/// engine uses. The whole path — read, decode, apply, turn into conversion
/// options — had no test, and it is the only thing standing between a
/// hand-edited or half-written settings file and an engine that either
/// converts with the wrong profile or does not start.
///
/// No inference happens here: the options are built and inspected, so this
/// needs no model file and is deterministic.
@Suite("engine configuration")
@MainActor
struct EngineConfigTests {
    /// A throwaway `%APPDATA%` containing `Azookey\settings.json` (or, when
    /// `contents` is nil, no file at all).
    private struct AppData {
        let path: String

        init(_ contents: String?) {
            let root = URL(filePath: NSTemporaryDirectory())
                .appendingPathComponent("azk-settings-\(UUID().uuidString)")
            let folder = root.appendingPathComponent("Azookey")
            try? FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
            if let contents {
                try? contents.write(
                    to: folder.appendingPathComponent("settings.json"),
                    atomically: true,
                    encoding: .utf8
                )
            }
            self.path = root.path
        }

        func remove() {
            try? FileManager.default.removeItem(atPath: path)
        }
    }

    /// Restores `config` around a test: it is process-global engine state and
    /// the suite writes it.
    private func withRestoredConfig(_ body: () -> Void) {
        let saved = config
        defer { config = saved }
        body()
    }

    @Test("a complete settings file is read and decoded")
    func completeFileIsRead() {
        let appData = AppData(#"{"version":"0.1.0","zenzai":{"enable":true,"profile":"私は猫だ","backend":"cpu"}}"#)
        defer { appData.remove() }

        let settings = loadSettingsFile(appDataPath: appData.path)

        #expect(settings?.zenzai?.enable == true)
        #expect(settings?.zenzai?.profile == "私は猫だ")
    }

    /// First run: there is no file yet. That is normal, not an error, and the
    /// caller keeps the current config.
    @Test("a missing settings file degrades to nil")
    func missingFileIsNil() {
        let appData = AppData(nil)
        defer { appData.remove() }

        #expect(loadSettingsFile(appDataPath: appData.path) == nil)
    }

    /// A hand-edited (or half-written) file must not stop the engine from
    /// starting — every application on the desktop depends on it coming up.
    @Test("a corrupt settings file degrades to nil")
    func corruptFileIsNil() {
        let appData = AppData(#"{"zenzai":{"enable":"#)
        defer { appData.remove() }

        #expect(loadSettingsFile(appDataPath: appData.path) == nil)
    }

    /// The Rust side owns the schema and writes keys this decoder does not
    /// declare (`version`, `zenzai.backend`), plus anything a future version
    /// adds. They must be ignored, not fail the parse.
    @Test("keys the engine does not read are ignored")
    func unknownKeysAreIgnored() {
        let appData = AppData(#"{"version":"9.9.9","future":{"x":1},"zenzai":{"enable":true,"backend":"cuda","profile":"p","future_key":42}}"#)
        defer { appData.remove() }

        let settings = loadSettingsFile(appDataPath: appData.path)

        #expect(settings?.zenzai?.enable == true)
        #expect(settings?.zenzai?.profile == "p")
    }

    /// Per-key tolerance, mirroring `#[serde(default)]` on the Rust struct: a
    /// file written before a key existed decodes, with that key absent rather
    /// than the whole parse failing.
    @Test("a partial settings file decodes with the missing keys absent")
    func partialFileDecodes() {
        let appData = AppData(#"{"zenzai":{"enable":true}}"#)
        defer { appData.remove() }

        let settings = loadSettingsFile(appDataPath: appData.path)

        #expect(settings?.zenzai?.enable == true)
        #expect(settings?.zenzai?.profile == nil)
    }

    @Test("LoadConfig applies every key it was given")
    func applyOverridesBothKeys() {
        withRestoredConfig {
            config = EngineConfig(zenzaiEnabled: false, zenzaiProfile: "old")

            applySettings(SettingsFile(zenzai: .init(enable: true, profile: "new")))

            #expect(config.zenzaiEnabled)
            #expect(config.zenzaiProfile == "new")
        }
    }

    /// The reason `applySettings` unwraps key by key: a file carrying only
    /// `enable` must not blank the profile the user set earlier.
    @Test("a key the file omits keeps its current value")
    func absentKeysAreKept() {
        withRestoredConfig {
            config = EngineConfig(zenzaiEnabled: false, zenzaiProfile: "私は猫だ")

            applySettings(SettingsFile(zenzai: .init(enable: true, profile: nil)))
            #expect(config.zenzaiEnabled)
            #expect(config.zenzaiProfile == "私は猫だ", "the profile must survive")

            applySettings(SettingsFile(zenzai: .init(enable: nil, profile: "他の")))
            #expect(config.zenzaiEnabled, "and so must enable")
            #expect(config.zenzaiProfile == "他の")
        }
    }

    /// Nothing to apply — a missing or corrupt file (nil) and an empty object
    /// both mean "keep what the engine has".
    @Test("an unreadable or empty settings file changes nothing")
    func nothingToApplyChangesNothing() {
        withRestoredConfig {
            config = EngineConfig(zenzaiEnabled: true, zenzaiProfile: "私は猫だ")

            applySettings(nil)
            applySettings(SettingsFile(zenzai: nil))
            applySettings(SettingsFile(zenzai: .init(enable: nil, profile: nil)))

            #expect(config.zenzaiEnabled)
            #expect(config.zenzaiProfile == "私は猫だ")
        }
    }

    /// Zenzai off is the default and must produce exactly `.off`: any other
    /// value would send the converter looking for a model file.
    @Test("zenzai disabled builds options with neural conversion off")
    func disabledYieldsOffMode() {
        withRestoredConfig {
            config = EngineConfig(zenzaiEnabled: false, zenzaiProfile: "私は猫だ")

            #expect(getOptions().zenzaiMode == .off)
            #expect(getOptions(context: "こんにちは").zenzaiMode == .off, "context does not enable it")
        }
    }

    /// Enabled, the profile and the surrounding text have to reach the model:
    /// they are the whole point of the v3 mode, and a silent drop would just
    /// look like slightly worse conversion.
    @Test("zenzai enabled carries the profile, the context and the model path")
    func enabledCarriesProfileAndContext() {
        withRestoredConfig {
            execURL = URL(filePath: #filePath).deletingLastPathComponent()
            config = EngineConfig(zenzaiEnabled: true, zenzaiProfile: "私は猫だ")

            let options = getOptions(context: "吾輩は")

            #expect(
                options.zenzaiMode == .on(
                    weight: execURL.appendingPathComponent("zenz.gguf"),
                    inferenceLimit: zenzaiInferenceLimit,
                    requestRichCandidates: true,
                    personalizationMode: nil,
                    versionDependentMode: .v3(.init(profile: "私は猫だ", leftSideContext: "吾輩は"))
                )
            )
            #expect(options.zenzaiMode != .off)
        }
    }

    /// `getOptions()` is called with no context from `Initialize`'s warm-up
    /// and from `GetComposedText` for a session that never got a `SetContext`.
    @Test("no context is an empty left-side context, not a missing one")
    func noContextIsEmpty() {
        withRestoredConfig {
            execURL = URL(filePath: #filePath).deletingLastPathComponent()
            config = EngineConfig(zenzaiEnabled: true, zenzaiProfile: "p")

            #expect(
                getOptions().zenzaiMode == .on(
                    weight: execURL.appendingPathComponent("zenz.gguf"),
                    inferenceLimit: zenzaiInferenceLimit,
                    requestRichCandidates: true,
                    personalizationMode: nil,
                    versionDependentMode: .v3(.init(profile: "p", leftSideContext: ""))
                )
            )
        }
    }
}
