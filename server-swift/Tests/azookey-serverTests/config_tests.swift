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
    /// Restores `config` around a test: it is process-global engine state and
    /// the suite writes it.
    private func withRestoredConfig(_ body: () -> Void) {
        let saved = config
        defer { config = saved }
        body()
    }

    /// Hands a document to the real FFI export, the way the server does.
    /// `String(cString:)` on the far side, so this goes through exactly the
    /// path a live call takes.
    private func loadConfigThroughFFI(_ json: String) -> Bool {
        json.withCString { load_config(json: $0) }
    }

    @Test("a complete settings document is decoded")
    func completeDocumentIsRead() {
        let settings = decodeSettings(
            #"{"version":"0.1.0","zenzai":{"enable":true,"profile":"私は猫だ","backend":"cpu"}}"#
        )

        #expect(settings?.zenzai?.enable == true)
        #expect(settings?.zenzai?.profile == "私は猫だ")
    }

    /// A document that is not JSON must not stop the engine — every
    /// application on the desktop depends on it coming up — and it must be
    /// reported rather than swallowed, which is what the export's Bool is for.
    @Test("text that is not JSON degrades to nil and is reported")
    func corruptDocumentIsNil() {
        #expect(decodeSettings(#"{"zenzai":{"enable":"#) == nil)

        withRestoredConfig {
            config = EngineConfig(zenzaiEnabled: true, zenzaiProfile: "keep me")

            #expect(loadConfigThroughFFI(#"{"zenzai":{"enable":"#) == false)

            #expect(config.zenzaiEnabled, "nothing may be applied from a document we cannot read")
            #expect(config.zenzaiProfile == "keep me")
        }
    }

    /// The Rust side owns the schema and writes keys this decoder does not
    /// declare (`version`, `zenzai.backend`), plus anything a future version
    /// adds. They must be ignored, not fail the parse.
    @Test("keys the engine does not read are ignored")
    func unknownKeysAreIgnored() {
        let settings = decodeSettings(
            #"{"version":"9.9.9","future":{"x":1},"zenzai":{"enable":true,"backend":"cuda","profile":"p","future_key":42}}"#
        )

        #expect(settings?.zenzai?.enable == true)
        #expect(settings?.zenzai?.profile == "p")
    }

    /// Per-key tolerance for an ABSENT key: a document written before a key
    /// existed decodes, with that key nil rather than the whole parse failing.
    @Test("a partial settings document decodes with the missing keys absent")
    func partialDocumentDecodes() {
        let settings = decodeSettings(#"{"zenzai":{"enable":true}}"#)

        #expect(settings?.zenzai?.enable == true)
        #expect(settings?.zenzai?.profile == nil)
    }

    /// Per-key tolerance for a WRONGLY TYPED key, which `Optional` alone did
    /// not give: the synthesized decoder threw on the first mismatch and took
    /// the whole document with it. Same fixture as the Rust-side regression
    /// (`a_wrongly_typed_value_costs_only_its_own_field`) so the two readers
    /// are pinned to the same behaviour.
    @Test("a wrongly typed value costs only its own key")
    func wrongTypesDegradePerKey() {
        let settings = decodeSettings(
            #"{"zenzai":{"enable":"true","profile":"keep me","inference_limit":"5","topic":42}}"#
        )

        #expect(settings != nil, "the document is still readable")
        #expect(settings?.zenzai?.enable == nil, "a quoted bool is not a bool")
        #expect(settings?.zenzai?.inference_limit == nil, "a quoted number either")
        #expect(settings?.zenzai?.topic == nil, "nor is a number a string")
        #expect(settings?.zenzai?.profile == "keep me", "and the rest survives")
    }

    /// A section of the wrong type costs the section, not the document.
    @Test("a zenzai section that is not an object costs only that section")
    func unusableSectionIsIgnored() {
        let settings = decodeSettings(#"{"version":"0.1.0","zenzai":"on"}"#)

        #expect(settings != nil)
        #expect(settings?.zenzai == nil)
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
        let settings = decodeSettings(
            #"{"zenzai":{"enable":true,"inference_limit":5,"topic":"ソフトウェア開発","style":"ですます調","preference":"漢字は控えめに"}}"#
        )

        #expect(settings?.zenzai?.inference_limit == 5)
        #expect(settings?.zenzai?.topic == "ソフトウェア開発")
        #expect(settings?.zenzai?.style == "ですます調")
        #expect(settings?.zenzai?.preference == "漢字は控えめに")
    }

    /// A settings file written before these keys existed. Same per-key
    /// tolerance as everything else: absent, not a failed parse.
    @Test("a document without the advanced keys decodes with them absent")
    func advancedZenzaiKeysMayBeAbsent() {
        let settings = decodeSettings(#"{"zenzai":{"enable":true,"profile":"p"}}"#)

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

    /// The defaults are the Rust side's to define, and this side has its own
    /// copy of every one of them. Nothing used to compare the two, so "the
    /// default inference limit is 1" could stop being true here only — and the
    /// symptom would be conversion behaving differently from what the settings
    /// app shows, with no test failing anywhere.
    ///
    /// `fixtures/default-settings.json` is what the Rust `AppConfig::default()`
    /// serializes to (its own test pins that end). Applying it to a fresh
    /// `EngineConfig` must therefore change nothing.
    @Test("the engine's defaults match the shared fixture")
    func defaultsMatchTheSharedFixture() throws {
        let fixture = URL(filePath: #filePath)
            .deletingLastPathComponent()  // azookey-serverTests/
            .deletingLastPathComponent()  // Tests/
            .deletingLastPathComponent()  // server-swift/
            .deletingLastPathComponent()  // the repository root
            .appendingPathComponent("fixtures/default-settings.json")
        let json = try String(contentsOf: fixture, encoding: .utf8)

        withRestoredConfig {
            config = EngineConfig()
            let untouched = EngineConfig()

            #expect(loadConfigThroughFFI(json))

            #expect(config.zenzaiEnabled == untouched.zenzaiEnabled)
            #expect(config.zenzaiProfile == untouched.zenzaiProfile)
            #expect(config.zenzaiInferenceLimit == untouched.zenzaiInferenceLimit)
            #expect(config.zenzaiTopic == untouched.zenzaiTopic)
            #expect(config.zenzaiStyle == untouched.zenzaiStyle)
            #expect(config.zenzaiPreference == untouched.zenzaiPreference)
        }
    }

    /// One pass through the real export, from the text the Rust side sends to
    /// the options the converter is handed. The pieces are covered above; this
    /// is the seam between them, and it is the only thing that would catch a
    /// `LoadConfig` that decoded a document and then applied nothing.
    @Test("LoadConfig carries a document all the way into the conversion options")
    func loadConfigReachesTheOptions() {
        withRestoredConfig {
            execURL = URL(filePath: #filePath).deletingLastPathComponent()
            config = EngineConfig()

            let applied = loadConfigThroughFFI(
                #"""
                {"version":"0.1.0","zenzai":{"enable":true,"profile":"私は猫だ",
                 "backend":"cpu","inference_limit":3,"topic":"話題","style":"文体",
                 "preference":"好み"}}
                """#
            )

            #expect(applied)
            #expect(
                getOptions(context: "吾輩は").zenzaiMode == .on(
                    weight: execURL.appendingPathComponent("zenz.gguf"),
                    inferenceLimit: 3,
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

    /// Switching Zenzai on has to take effect without restarting the engine.
    ///
    /// The observable half is here: the options flip to `.on` on the next
    /// conversion. The other half — loading the gguf during `LoadConfig`
    /// instead of on the first keystroke after it — needs a live converter, so
    /// it belongs to the `--ignored` smoke tests; `converter` is nil here and
    /// the warm-up branch is skipped.
    @Test("zenzai switched on at runtime reaches the options without a restart")
    func enablingZenzaiTakesEffectImmediately() {
        withRestoredConfig {
            execURL = URL(filePath: #filePath).deletingLastPathComponent()
            config = EngineConfig(zenzaiEnabled: false)
            #expect(getOptions().zenzaiMode == .off)

            #expect(loadConfigThroughFFI(#"{"zenzai":{"enable":true,"profile":"p"}}"#))

            #expect(getOptions().zenzaiMode != .off, "no restart may be needed for this")
        }
    }
}
