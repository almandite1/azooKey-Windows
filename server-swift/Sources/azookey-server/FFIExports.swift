import KanaKanjiConverterModule
import Foundation
import ffi

// FFI THREADING CONTRACT:
// All exported functions below mutate the @MainActor global state in
// EngineState.swift, but the annotation is NOT enforced across the C
// boundary. The Rust server MUST call every exported function from a single
// thread, serially (it runs a current_thread tokio runtime on the process
// main thread).
//
// The canonical declaration of every exported signature is
// Sources/ffi/include/ffi.h; keep the @_cdecl exports below (and the Rust
// mirror in crates/server/src/ffi.rs) in sync with it. ffi_surface.rs in
// crates/server/tests checks that the three sets of function names agree.

/// The state's reading, allocated for the caller to hand back to
/// `FreeString`. Every composing-text export returns exactly this.
@MainActor private func reading(_ state: SessionState) -> UnsafeMutablePointer<CChar>? {
    _strdup(kanaReading(state.composingText.convertTarget))
}

/// [`reading`] plus the cursor out-parameter: the epilogue of every export
/// that has one, in one place.
///
/// `cursorPtr` always receives the ABSOLUTE cursor position in kana from the
/// start of the reading — never a delta, MoveCursor included (#82). That
/// contract now has a single enforcement point instead of being restated at
/// three separate returns.
@MainActor private func readingResult(
    _ state: SessionState,
    _ cursorPtr: UnsafeMutablePointer<Int32>
) -> UnsafeMutablePointer<CChar>? {
    cursorPtr.pointee = Int32(state.composingText.convertTargetCursorPosition)
    return reading(state)
}

/// One conversion, thrown away, so that whatever the current options need
/// loading is loaded before a keystroke waits on it.
///
/// The expensive part is the zenz model: gguf load and the first inference run
/// on a cold model take seconds, and every FFI call is serialized onto the
/// server's single thread — so that cost lands on whichever keystroke happens
/// to be first, with the whole desktop's typing behind it.
@MainActor private func warmUpConverter(_ engine: KanaKanjiConverter) {
    var warmup = ComposingText()
    warmup.insertAtCursorPosition("a", inputStyle: .roman2kana)
    _ = engine.requestCandidates(warmup, options: getOptions())

    // a missing/corrupt zenz.gguf degrades silently to non-neural
    // conversion inside the converter; surface its status in the log
    if !engine.zenzStatus.isEmpty {
        enginePrint(level: .info, "zenzai status: \(engine.zenzStatus)")
    }
}

@_cdecl("LoadConfig")
@MainActor public func load_config(json: UnsafePointer<CChar>) -> Bool {
    guard let settings = decodeSettings(String(cString: json)) else {
        // nothing applied: the caller reports the failure rather than letting
        // the settings app claim the save took effect
        return false
    }

    let wasEnabled = config.zenzaiEnabled
    // only the keys that are present are overridden, so a partial document
    // keeps the current values — see applySettings
    applySettings(settings)

    // Turning Zenzai on at runtime used to leave the gguf load for the first
    // keystroke after it, which blocks the single-threaded server for seconds
    // with every application's typing behind it. Warm-up used to happen only
    // in Initialize, i.e. only for a session that started with Zenzai already
    // on. `converter` is nil when this runs before Initialize (the startup
    // order), and that case is covered by Initialize's own warm-up.
    if !wasEnabled, config.zenzaiEnabled, let engine = converter {
        enginePrint(level: .info, "zenzai was switched on; warming the model up now")
        warmUpConverter(engine)
    }
    return true
}

@_cdecl("Initialize")
@MainActor public func initialize(
    path: UnsafePointer<CChar>
) {
    let path = String(cString: path)
    execURL = URL(filePath: path)

    // NOT load_config: the engine no longer reads settings.json, so the Rust
    // side applies the configuration before calling this — which it must,
    // because the warm-up below builds its options out of it.

    // the dictionary belongs to the converter instance now, so this is the
    // earliest point it can be built: `path` is where the installer put
    // Dictionary/ and EmojiDictionary/
    let engine = KanaKanjiConverter(
        dictionaryURL: execURL.appendingPathComponent("Dictionary"),
        preloadDictionary: true
    )
    converter = engine

    warmUpConverter(engine)
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
        return readingResult(state, cursorPtr)
    }
}

@_cdecl("RemoveText")
@MainActor public func remove_text(
    session: Int64,
    cursorPtr: UnsafeMutablePointer<Int32>
) -> UnsafeMutablePointer<CChar>? {
    withSession(session) { state in
        state.composingText.deleteBackwardFromCursorPosition(count: 1)
        return readingResult(state, cursorPtr)
    }
}

@_cdecl("MoveCursor")
@MainActor public func move_cursor(
    session: Int64,
    offset: Int32,
    cursorPtr: UnsafeMutablePointer<Int32>
) -> UnsafeMutablePointer<CChar>? {
    withSession(session) { state in
        // moveCursorFromCursorPosition returns the clamped DISTANCE it
        // moved, not the position it arrived at. Discard it: readingResult
        // reports the absolute position, the same as for every other export,
        // so the out-parameter cannot mean two different things depending on
        // which call filled it in.
        _ = state.composingText.moveCursorFromCursorPosition(count: Int(offset))
        return readingResult(state, cursorPtr)
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
///
/// This is process-wide, not per-session: `stopComposition` clears the one
/// shared converter's caches, so ending a composition in one app also drops
/// the incremental state of any other app mid-composition. Scoping the reset
/// to a session needs a per-session cache handle the converter does not yet
/// expose; it is coming upstream (feat/session_api) and is the right fix — do
/// not try to fake it from here.
@MainActor func endComposition() {
    converter?.stopComposition()
}

/// Copies the candidates into a C array of pointers for the caller to hand
/// back to `FreeComposedText`.
///
/// Every slot is initialized, which the loop alone did not do: an empty list
/// still allocates one slot (`max(count, 1)` — `allocate(capacity: 0)` is not
/// something to rely on), and that slot was left holding whatever the
/// allocator had. Nothing read it, because the length out-parameter says zero,
/// but "correct as long as nobody looks" is not a property to leave in an FFI
/// buffer.
func to_list_pointer(_ list: [FFICandidate]) -> UnsafeMutablePointer<UnsafeMutablePointer<FFICandidate>?> {
    let capacity = max(list.count, 1)
    let pointer = UnsafeMutablePointer<UnsafeMutablePointer<FFICandidate>?>.allocate(capacity: capacity)
    pointer.initialize(repeating: nil, count: capacity)
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

        // no cursor out-parameter on this one (see ffi.h), so the reading
        // alone — the cursor half of the epilogue has nothing to write to
        return reading(state)
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
