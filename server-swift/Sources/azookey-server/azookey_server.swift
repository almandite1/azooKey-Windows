import KanaKanjiConverterModule
import Foundation
import ffi

// FFI THREADING CONTRACT:
// All exported functions below mutate this @MainActor global state, but the
// annotation is NOT enforced across the C boundary. The Rust server MUST
// call every exported function from a single thread, serially (it runs a
// current_thread tokio runtime on the process main thread).
//
// The canonical declaration of every exported signature is
// Sources/ffi/include/ffi.h; keep the @_cdecl exports below (and the Rust
// mirror in crates/server/src/ffi.rs) in sync with it.
@MainActor let converter = KanaKanjiConverter()

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

@MainActor var sessions: [Int32: SessionState] = [:]

@MainActor func withSession<T>(_ id: Int32, _ body: (inout SessionState) -> T) -> T {
    var state = sessions[id] ?? SessionState()
    let result = body(&state)
    sessions[id] = state
    return result
}

@MainActor var execURL = URL(filePath: "")
@MainActor var config: [String : Any] = [
    "enable": false,
    "profile": "",
]

@MainActor func getOptions(context: String = "") -> ConvertRequestOptions {
    let zenzaiEnabled = (config["enable"] as? Bool) ?? false
    let zenzaiProfile = (config["profile"] as? String) ?? ""
    return ConvertRequestOptions(
        requireJapanesePrediction: true,
        requireEnglishPrediction: false,
        keyboardLanguage: .ja_JP,
        learningType: .nothing,
        dictionaryResourceURL: execURL.appendingPathComponent("Dictionary"),
        memoryDirectoryURL: URL(filePath: "./test"),
        sharedContainerURL: URL(filePath: "./test"),
        textReplacer: .init {
            return execURL.appendingPathComponent("EmojiDictionary").appendingPathComponent("emoji_all_E15.1.txt")
        },
        // zenzai
        zenzaiMode: zenzaiEnabled ? .on(
            weight: execURL.appendingPathComponent("zenz.gguf"),
            inferenceLimit: 1,
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

func constructCandidateString(candidate: Candidate, hiragana: String) -> String {
    var remainingHiragana = hiragana
    var result = ""

    for data in candidate.data {
        if remainingHiragana.count < data.ruby.count {
            result += remainingHiragana
            break
        }
        remainingHiragana.removeFirst(data.ruby.count)
        result += data.word
    }

    return result
}

@_cdecl("LoadConfig")
@MainActor public func load_config() {
    if let appDataPath = ProcessInfo.processInfo.environment["APPDATA"] {
        let settingsPath = URL(filePath: appDataPath).appendingPathComponent("Azookey/settings.json")

        do {
            let data = try Data(contentsOf: settingsPath)
            if let json = try JSONSerialization.jsonObject(with: data) as? [String: Any],
               let zenzaiDict = json["zenzai"] as? [String: Any] {

                if let enableValue = zenzaiDict["enable"] as? Bool {
                    config["enable"] = enableValue
                }

                if let profileValue = zenzaiDict["profile"] as? String {
                    config["profile"] = profileValue
                }
            }
        } catch {
            print("Failed to read settings: \(error)")
        }
    }
}

@_cdecl("Initialize")
@MainActor public func initialize(
    path: UnsafePointer<CChar>
) {
    let path = String(cString: path)
    execURL = URL(filePath: path)

    load_config()

    // warm up the converter (and the zenzai model when enabled)
    var warmup = ComposingText()
    warmup.insertAtCursorPosition("a", inputStyle: .roman2kana)
    converter.requestCandidates(warmup, options: getOptions())

    // a missing/corrupt zenz.gguf degrades silently to non-neural
    // conversion inside the converter; surface its status in the log
    if !converter.zenzStatus.isEmpty {
        print("zenzai status: \(converter.zenzStatus)")
    }
}

@_cdecl("AppendText")
@MainActor public func append_text(
    session: Int32,
    input: UnsafePointer<CChar>,
    cursorPtr: UnsafeMutablePointer<Int32>
) -> UnsafeMutablePointer<CChar>? {
    let inputString = String(cString: input)
    return withSession(session) { state in
        state.composingText.insertAtCursorPosition(inputString, inputStyle: .roman2kana)

        cursorPtr.pointee = Int32(state.composingText.convertTargetCursorPosition)
        return _strdup(state.composingText.convertTarget)
    }
}

@_cdecl("RemoveText")
@MainActor public func remove_text(
    session: Int32,
    cursorPtr: UnsafeMutablePointer<Int32>
) -> UnsafeMutablePointer<CChar>? {
    withSession(session) { state in
        state.composingText.deleteBackwardFromCursorPosition(count: 1)

        cursorPtr.pointee = Int32(state.composingText.convertTargetCursorPosition)
        return _strdup(state.composingText.convertTarget)
    }
}

@_cdecl("MoveCursor")
@MainActor public func move_cursor(
    session: Int32,
    offset: Int32,
    cursorPtr: UnsafeMutablePointer<Int32>
) -> UnsafeMutablePointer<CChar>? {
    withSession(session) { state in
        let cursor = state.composingText.moveCursorFromCursorPosition(count: Int(offset))

        cursorPtr.pointee = Int32(cursor)
        return _strdup(state.composingText.convertTarget)
    }
}

@_cdecl("ClearText")
@MainActor public func clear_text(session: Int32) {
    withSession(session) { state in
        state.composingText = ComposingText()
    }
}

@_cdecl("RemoveSession")
@MainActor public func remove_session(session: Int32) {
    sessions.removeValue(forKey: session)
}

func to_list_pointer(_ list: [FFICandidate]) -> UnsafeMutablePointer<UnsafeMutablePointer<FFICandidate>?> {
    let pointer = UnsafeMutablePointer<UnsafeMutablePointer<FFICandidate>?>.allocate(capacity: max(list.count, 1))
    for (i, item) in list.enumerated() {
        pointer[i] = UnsafeMutablePointer<FFICandidate>.allocate(capacity: 1)
        pointer[i]?.pointee = item
    }
    return pointer
}

@_cdecl("GetComposedText")
@MainActor public func get_composed_text(
    session: Int32,
    lengthPtr: UnsafeMutablePointer<Int32>
) -> UnsafeMutablePointer<UnsafeMutablePointer<FFICandidate>?> {
    let (composingText, contextString) = withSession(session) { state in
        (state.composingText, state.context)
    }
    let hiragana = composingText.convertTarget
    let options = getOptions(context: contextString)
    let converted = converter.requestCandidates(composingText, options: options)
    var result: [FFICandidate] = []

    for i in 0..<converted.mainResults.count {
        let candidate = converted.mainResults[i]

        let text = _strdup(constructCandidateString(candidate: candidate, hiragana: hiragana))
        let hiragana = _strdup(hiragana)
        let correspondingCount = candidate.correspondingCount

        var afterComposingText = composingText
        afterComposingText.prefixComplete(correspondingCount: correspondingCount)
        let subtext = _strdup(afterComposingText.convertTarget)

        result.append(FFICandidate(text: text, subtext: subtext, hiragana: hiragana, correspondingCount: Int32(correspondingCount)))
    }

    lengthPtr.pointee = Int32(result.count)

    return to_list_pointer(result)
}

@_cdecl("ShrinkText")
@MainActor public func shrink_text(
    session: Int32,
    offset: Int32
) -> UnsafeMutablePointer<CChar>? {
    withSession(session) { state in
        var afterComposingText = state.composingText
        // prefixComplete clamps the count from above (min with input.count)
        // but a negative count traps in Array.removeFirst and takes the
        // whole server down — clamp from below here, at the FFI boundary
        afterComposingText.prefixComplete(correspondingCount: max(0, Int(offset)))
        state.composingText = afterComposingText

        return _strdup(state.composingText.convertTarget)
    }
}

@_cdecl("SetContext")
@MainActor public func set_context(
    session: Int32,
    context: UnsafePointer<CChar>
) {
    let contextString = String(cString: context)
    withSession(session) { state in
        state.context = contextString
    }
}

// MEMORY OWNERSHIP CONTRACT:
// Every string returned by the functions above is allocated with
// strdup/_strdup and every candidate list with allocate(); the caller must
// return them to FreeString / FreeComposedText after copying. Freeing on
// the Swift side keeps allocation and deallocation in the same CRT.

@_cdecl("FreeString")
public func free_string(ptr: UnsafeMutablePointer<CChar>?) {
    free(ptr)
}

@_cdecl("FreeComposedText")
public func free_composed_text(
    listPtr: UnsafeMutablePointer<UnsafeMutablePointer<FFICandidate>?>?,
    length: Int32
) {
    guard let listPtr else { return }
    for i in 0..<Int(length) {
        if let item = listPtr[i] {
            free(item.pointee.text)
            free(item.pointee.subtext)
            free(item.pointee.hiragana)
            item.deallocate()
        }
    }
    listPtr.deallocate()
}
