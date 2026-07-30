import KanaKanjiConverterModule
import Foundation

// The converter is created in `Initialize`, not here: since the engine
// adopted per-instance dictionaries the dictionary directory is an init
// argument, and it is only known once the Rust side hands us the
// installation path. Nothing may convert before that call, but this is the
// FFI boundary — a call that arrives early returns an empty candidate list
// instead of trapping and taking the server down.
@MainActor var converter: KanaKanjiConverter?

// Per-client composing state, keyed by the session id the Rust server
// assigns to each pipe connection. Every application hosting the IME has
// its own session; sharing one global ComposingText made simultaneous
// typing in two apps corrupt each other's composition. The converter
// (dictionary + zenz model) stays shared — it is heavyweight and all
// calls are serialized by the single-threaded caller.
struct SessionState {
    var composingText = ComposingText()
    var context = ""
}

@MainActor var sessions: [Int64: SessionState] = [:]

@MainActor func withSession<T>(_ id: Int64, _ body: (inout SessionState) -> T) -> T {
    var state = sessions[id] ?? SessionState()
    let result = body(&state)
    sessions[id] = state
    return result
}

@MainActor var execURL = URL(filePath: "")

@MainActor var config = EngineConfig()

// Fixed engine parameters, hoisted so they are visible in one place.
// The ./test placeholder predates this refactor: with learningType
// .nothing the memory/shared-container dirs are never written — they
// become real, configurable paths when the learning feature lands.
let emojiDictionaryFileName = "emoji_all_E16.0.txt"
let placeholderDataDirectory = URL(filePath: "./test")
