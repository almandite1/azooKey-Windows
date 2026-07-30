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
                    inferenceLimit: 1,
                    requestRichCandidates: true,
                    personalizationMode: nil,
                    versionDependentMode: .v3(
                        .init(
                            profile: "私は猫だ",
                            topic: "",
                            style: "",
                            preference: "",
                            leftSideContext: "吾輩は"
                        )
                    )
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
                    inferenceLimit: 1,
                    requestRichCandidates: true,
                    personalizationMode: nil,
                    versionDependentMode: .v3(
                        .init(
                            profile: "p",
                            topic: "",
                            style: "",
                            preference: "",
                            leftSideContext: ""
                        )
                    )
                )
            )
        }
    }

    /// The keys the settings app grew after the first release. They are read
    /// by field name out of a file the Rust side writes, so a rename on either
    /// side shows up here rather than as conversion that quietly ignores what
    /// the user typed into the settings app.
    @Test("the inference limit and the v3 context keys are decoded")
    func advancedZenzaiKeysAreRead() {
        let appData = AppData(#"{"zenzai":{"enable":true,"inference_limit":5,"topic":"ソフトウェア開発","style":"ですます調","preference":"漢字は控えめに"}}"#)
        defer { appData.remove() }

        let settings = loadSettingsFile(appDataPath: appData.path)

        #expect(settings?.zenzai?.inference_limit == 5)
        #expect(settings?.zenzai?.topic == "ソフトウェア開発")
        #expect(settings?.zenzai?.style == "ですます調")
        #expect(settings?.zenzai?.preference == "漢字は控えめに")
    }

    /// A settings file written before these keys existed. Same per-key
    /// tolerance as everything else: absent, not a failed parse.
    @Test("a file without the advanced keys decodes with them absent")
    func advancedZenzaiKeysMayBeAbsent() {
        let appData = AppData(#"{"zenzai":{"enable":true,"profile":"p"}}"#)
        defer { appData.remove() }

        let settings = loadSettingsFile(appDataPath: appData.path)

        #expect(settings?.zenzai?.enable == true)
        #expect(settings?.zenzai?.inference_limit == nil)
        #expect(settings?.zenzai?.topic == nil)
        #expect(settings?.zenzai?.style == nil)
        #expect(settings?.zenzai?.preference == nil)
    }

    @Test("the advanced keys reach the engine config")
    func advancedKeysAreApplied() {
        withRestoredConfig {
            config = EngineConfig()

            applySettings(
                SettingsFile(
                    zenzai: .init(
                        inference_limit: 3,
                        topic: "話題",
                        style: "文体",
                        preference: "好み"
                    )
                )
            )

            #expect(config.zenzaiInferenceLimit == 3)
            #expect(config.zenzaiTopic == "話題")
            #expect(config.zenzaiStyle == "文体")
            #expect(config.zenzaiPreference == "好み")
        }
    }

    /// settings.json is a text file the user can edit, and the settings app is
    /// not the only thing that writes it. Nothing the file says may put the
    /// engine outside the range it was tested at — a zero would mean no
    /// inference at all, and a large one stalls the single-threaded server for
    /// every application at once.
    @Test("a hand-edited inference limit is clamped to the supported range")
    func inferenceLimitIsClamped() {
        withRestoredConfig {
            config = EngineConfig()

            applySettings(SettingsFile(zenzai: .init(inference_limit: 0)))
            #expect(config.zenzaiInferenceLimit == 1)

            applySettings(SettingsFile(zenzai: .init(inference_limit: -7)))
            #expect(config.zenzaiInferenceLimit == 1)

            applySettings(SettingsFile(zenzai: .init(inference_limit: 100)))
            #expect(config.zenzaiInferenceLimit == 10)

            applySettings(SettingsFile(zenzai: .init(inference_limit: 4)))
            #expect(config.zenzaiInferenceLimit == 4, "a value in range is untouched")
        }
    }

    /// The whole point of exposing these: what the user set has to end up in
    /// the request the model sees.
    @Test("the configured limit and context strings reach the conversion options")
    func advancedKeysReachTheOptions() {
        withRestoredConfig {
            execURL = URL(filePath: #filePath).deletingLastPathComponent()
            config = EngineConfig(
                zenzaiEnabled: true,
                zenzaiProfile: "私は猫だ",
                zenzaiInferenceLimit: 10,
                zenzaiTopic: "話題",
                zenzaiStyle: "文体",
                zenzaiPreference: "好み"
            )

            #expect(
                getOptions(context: "吾輩は").zenzaiMode == .on(
                    weight: execURL.appendingPathComponent("zenz.gguf"),
                    inferenceLimit: 10,
                    requestRichCandidates: true,
                    personalizationMode: nil,
                    versionDependentMode: .v3(
                        .init(
                            profile: "私は猫だ",
                            topic: "話題",
                            style: "文体",
                            preference: "好み",
                            leftSideContext: "吾輩は"
                        )
                    )
                )
            )
        }
    }
}
