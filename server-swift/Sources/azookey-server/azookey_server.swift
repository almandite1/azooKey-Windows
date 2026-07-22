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
//
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

@MainActor var config = EngineConfig()

// Fixed engine parameters, hoisted so they are visible in one place.
// The ./test placeholder predates this refactor: with learningType
// .nothing the memory/shared-container dirs are never written — they
// become real, configurable paths when the learning feature lands.
let emojiDictionaryFileName = "emoji_all_E16.0.txt"
let placeholderDataDirectory = URL(filePath: "./test")
let zenzaiInferenceLimit = 1

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

/// The reading as the user should see it, with a pending romaji `n`
/// resolved to ん.
///
/// `ComposingText.convertTarget` keeps a trailing `n` as the latin letter
/// because it is still ambiguous — `nihon` can continue into `nihona` — and
/// that is the right thing for the engine to hold onto. But this string is
/// also what we hand back as the candidate text, as the remaining reading,
/// and as the reading F6 shows, so the letter reached the document: `nihon`
/// then Enter committed `にほn`, and conversion offered `仁保n` instead of
/// 日本 (issue #38).
///
/// On its own an `n` is ん, so it resolves. Other trailing consonants are
/// not kana by themselves and stay as they are — `nihok` shows and commits
/// `にほk`, which is what MS-IME does too. Uppercase `N` is left alone: the
/// roman2kana table is lowercase, so an `N` in the reading is literal text
/// the user asked for.
///
/// One character replaces one character, so this cannot shift the
/// ruby-length arithmetic in `constructCandidateString`.
func kanaReading(_ convertTarget: String) -> String {
    guard convertTarget.hasSuffix("n") else {
        return convertTarget
    }
    return convertTarget.dropLast() + "ん"
}

/// The text to ask the converter about: the session's own composing text,
/// except that a pending romaji `n` is completed first.
///
/// The engine deliberately leaves that `n` unresolved — `nihon` can still
/// become `nihona` — but the dictionary then never sees ニホン, so `nihon`
/// offered 仁保 and 二歩 and could not convert to 日本 at all (issue #38).
/// Completing it the way a user would, by typing the second `n`, is enough
/// to make the lookup work.
///
/// This is a copy: the session keeps its unresolved `n`, so the very next
/// keystroke can still turn it into な行.
///
/// The counts measured here stay usable against the session's text. The
/// surface count `ShrinkText` spends transfers exactly: the two readings
/// differ only in that last character, `n` for ん, one for one. The
/// keystroke count the client spends can be one too high for a candidate
/// that covers the ん — it counts the `n` we typed on its behalf — which is
/// harmless, the client's `raw_input` has nothing left to drop by then.
///
/// Only when the cursor is at the end: `insertAtCursorPosition` would
/// otherwise splice the `n` into the middle of the reading.
@MainActor func conversionTarget(_ composingText: ComposingText) -> ComposingText {
    guard composingText.isAtEndIndex, composingText.convertTarget.hasSuffix("n") else {
        return composingText
    }
    var completed = composingText
    completed.insertAtCursorPosition("n", inputStyle: .roman2kana)
    return completed
}

/// Commits the front of `text`, spending a count that came from outside —
/// the converter, or the client over the FFI — and is therefore not trusted.
///
/// Every part is clamped to what the text can actually give, because this
/// process is the engine every application on the desktop shares and each
/// unclamped end takes all of their compositions down with it:
///
/// - a negative count reaches `Array.removeFirst` / `String.dropFirst`, both
///   of which trap outright
/// - a surface count past the end of the reading is accepted quietly and
///   drives `convertTargetCursorPosition` negative (`prefixComplete`
///   subtracts the count it was given, not the count it could use), so the
///   next keystroke traps in `prefix(_:)` instead — a delayed trap in an
///   unrelated call. `.inputCount` clamps itself; `.surfaceCount` does not.
///
/// A composite spends its parts in order, each clamped against what the
/// preceding one left.
func spend(_ composingCount: ComposingCount, from text: inout ComposingText) {
    switch composingCount {
    case .inputCount(let count):
        text.prefixComplete(composingCount: .inputCount(min(max(0, count), text.input.count)))
    case .surfaceCount(let count):
        text.prefixComplete(
            composingCount: .surfaceCount(min(max(0, count), text.convertTarget.count))
        )
    case .composite(let lhs, let rhs):
        spend(lhs, from: &text)
        spend(rhs, from: &text)
    }
}

/// The reading left behind by every keystroke count, so index `k` holds the
/// result of spending `.inputCount(k)` on `target`.
///
/// The same table serves every candidate of one conversion request, which is
/// why it is built by the caller rather than inside `remainder`.
func shrinkReadings(of target: ComposingText) -> [String] {
    (0...target.input.count).map { count in
        var text = target
        text.prefixComplete(composingCount: .inputCount(count))
        return text.convertTarget
    }
}

/// What is left of `target` after a candidate covering `composingCount` is
/// committed, and how much of the reading that takes — measured twice,
/// because the engine and the client count in different units.
///
/// `surfaceCount` is kana, and it is the one that decides: `ShrinkText`
/// spends it on the session's own text, so the reading left composing is
/// exactly the `text` returned here. `inputCount` is romaji keystrokes and
/// exists for the client alone, which drops that many characters from its
/// `raw_input` (the string F9/F10 turn into latin).
///
/// A candidate's `composingCount` is neither number directly: since the
/// engine grew `ComposingCount` it is usually a surface count but can be an
/// input count or a composite of several, so let the engine spend it and
/// measure the difference.
///
/// For keystrokes, measuring what the engine spent does not work. The engine
/// looks candidates up by surface index too, so a clause boundary can fall
/// inside one romaji cluster: `henkansuru` is `{he}{nka}{nsuru}` and 「変換」
/// stops after 4 kana, in the middle of the last segment. Spending that
/// surface count re-encodes the whole segment into kana elements
/// (`forceGetInputCursorPosition`), so the input array shrinks for a reason
/// that has nothing to do with the candidate — `henkansuru` measured 7
/// keystrokes for a boundary that sits after 6.
///
/// So look the reading up instead: the answer is the keystroke count whose
/// own remaining reading is the one the candidate leaves. `readings` is
/// `shrinkReadings(of: target)`.
///
/// Not every boundary has one. かんしゃ is `{ka}{nsha}`, so no prefix of the
/// input leaves ゃ behind, and there the measurement stands — `raw_input`
/// keeps a keystroke too many or too few, which only shows if the user then
/// asks F9 to re-render the remainder as latin. The reading itself, on both
/// sides, stays right.
func remainder(
    of target: ComposingText,
    after composingCount: ComposingCount,
    readings: [String]
) -> (text: ComposingText, surfaceCount: Int, inputCount: Int) {
    var remaining = target
    spend(composingCount, from: &remaining)

    let surfaceCount = target.convertTarget.count - remaining.convertTarget.count
    let measured = min(max(0, target.input.count - remaining.input.count), target.input.count)
    let inputCount = readings.firstIndex(of: remaining.convertTarget) ?? measured

    return (remaining, surfaceCount, inputCount)
}

func remainder(
    of target: ComposingText,
    after composingCount: ComposingCount
) -> (text: ComposingText, surfaceCount: Int, inputCount: Int) {
    remainder(of: target, after: composingCount, readings: shrinkReadings(of: target))
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
            let settings = try JSONDecoder().decode(SettingsFile.self, from: data)
            // only override keys that are present, matching the previous
            // behavior: a partial file keeps the current values
            if let zenzai = settings.zenzai {
                if let enable = zenzai.enable {
                    config.zenzaiEnabled = enable
                }
                if let profile = zenzai.profile {
                    config.zenzaiProfile = profile
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

    // the dictionary belongs to the converter instance now, so this is the
    // earliest point it can be built: `path` is where the installer put
    // Dictionary/ and EmojiDictionary/
    let engine = KanaKanjiConverter(
        dictionaryURL: execURL.appendingPathComponent("Dictionary"),
        preloadDictionary: true
    )
    converter = engine

    // warm up the converter (and the zenzai model when enabled)
    var warmup = ComposingText()
    warmup.insertAtCursorPosition("a", inputStyle: .roman2kana)
    _ = engine.requestCandidates(warmup, options: getOptions())

    // a missing/corrupt zenz.gguf degrades silently to non-neural
    // conversion inside the converter; surface its status in the log
    if !engine.zenzStatus.isEmpty {
        print("zenzai status: \(engine.zenzStatus)")
    }
}

@_cdecl("AppendText")
@MainActor public func append_text(
    session: Int64,
    input: UnsafePointer<CChar>,
    cursorPtr: UnsafeMutablePointer<Int32>
) -> UnsafeMutablePointer<CChar>? {
    let inputString = String(cString: input)
    return withSession(session) { state in
        state.composingText.insertAtCursorPosition(inputString, inputStyle: .roman2kana)

        cursorPtr.pointee = Int32(state.composingText.convertTargetCursorPosition)
        return _strdup(kanaReading(state.composingText.convertTarget))
    }
}

@_cdecl("RemoveText")
@MainActor public func remove_text(
    session: Int64,
    cursorPtr: UnsafeMutablePointer<Int32>
) -> UnsafeMutablePointer<CChar>? {
    withSession(session) { state in
        state.composingText.deleteBackwardFromCursorPosition(count: 1)

        cursorPtr.pointee = Int32(state.composingText.convertTargetCursorPosition)
        return _strdup(kanaReading(state.composingText.convertTarget))
    }
}

@_cdecl("MoveCursor")
@MainActor public func move_cursor(
    session: Int64,
    offset: Int32,
    cursorPtr: UnsafeMutablePointer<Int32>
) -> UnsafeMutablePointer<CChar>? {
    withSession(session) { state in
        let cursor = state.composingText.moveCursorFromCursorPosition(count: Int(offset))

        cursorPtr.pointee = Int32(cursor)
        return _strdup(kanaReading(state.composingText.convertTarget))
    }
}

@_cdecl("ClearText")
@MainActor public func clear_text(session: Int64) {
    withSession(session) { state in
        state.composingText = ComposingText()
    }
    endComposition()
}

@_cdecl("RemoveSession")
@MainActor public func remove_session(session: Int64) {
    sessions.removeValue(forKey: session)
    endComposition()
}

/// Drops the converter's own per-composition caches (lattice, zenzai
/// sequence, last committed data).
///
/// The converter keeps that state internally and keys it on nothing we
/// control — one instance is shared by every application, so a reading left
/// behind by one app is what the next lookup is incrementally built on.
/// Upstream resets it when a composition ends; ours end at ClearText and at
/// RemoveSession, so those are the two places to say so.
@MainActor func endComposition() {
    converter?.stopComposition()
}

func to_list_pointer(_ list: [FFICandidate]) -> UnsafeMutablePointer<UnsafeMutablePointer<FFICandidate>?> {
    let pointer = UnsafeMutablePointer<UnsafeMutablePointer<FFICandidate>?>.allocate(capacity: max(list.count, 1))
    for (i, item) in list.enumerated() {
        let element = UnsafeMutablePointer<FFICandidate>.allocate(capacity: 1)
        element.initialize(to: item)
        pointer[i] = element
    }
    return pointer
}

@_cdecl("GetComposedText")
@MainActor public func get_composed_text(
    session: Int64,
    lengthPtr: UnsafeMutablePointer<Int32>
) -> UnsafeMutablePointer<UnsafeMutablePointer<FFICandidate>?> {
    let (composingText, contextString) = withSession(session) { state in
        (state.composingText, state.context)
    }
    let target = conversionTarget(composingText)
    let hiragana = kanaReading(target.convertTarget)
    let options = getOptions(context: contextString)
    guard let converter else {
        lengthPtr.pointee = 0
        return to_list_pointer([])
    }
    let converted = converter.requestCandidates(target, options: options)
    // one table for the whole request: it depends only on the reading
    let readings = shrinkReadings(of: target)
    var result: [FFICandidate] = []

    for i in 0..<converted.mainResults.count {
        let candidate = converted.mainResults[i]

        let text = _strdup(constructCandidateString(candidate: candidate, hiragana: hiragana))

        let (afterComposingText, surfaceCount, correspondingCount) = remainder(
            of: target,
            after: candidate.composingCount,
            readings: readings
        )
        let subtext = _strdup(kanaReading(afterComposingText.convertTarget))

        result.append(
            FFICandidate(
                text: text,
                subtext: subtext,
                correspondingCount: Int32(correspondingCount),
                surfaceCount: Int32(surfaceCount)
            )
        )
    }

    lengthPtr.pointee = Int32(result.count)

    return to_list_pointer(result)
}

@_cdecl("ShrinkText")
@MainActor public func shrink_text(
    session: Int64,
    surfaceOffset: Int32
) -> UnsafeMutablePointer<CChar>? {
    withSession(session) { state in
        var afterComposingText = state.composingText
        // A surface (kana) count, not the keystroke count this used to take:
        // a candidate can end inside a romaji cluster (かんし|ゃ), and only
        // the kana boundary can express that. It transfers to the session's
        // own text even though it was measured on the pending-`n` copy —
        // kanaReading replaces one character with one character.
        //
        // `spend` clamps it at both ends; the count is whatever the client
        // sent, and this is the FFI boundary
        spend(.surfaceCount(Int(surfaceOffset)), from: &afterComposingText)
        state.composingText = afterComposingText

        return _strdup(kanaReading(state.composingText.convertTarget))
    }
}

@_cdecl("SetContext")
@MainActor public func set_context(
    session: Int64,
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
            item.deallocate()
        }
    }
    listPtr.deallocate()
}
