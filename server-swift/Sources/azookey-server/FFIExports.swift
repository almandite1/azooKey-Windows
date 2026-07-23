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

@_cdecl("LoadConfig")
@MainActor public func load_config() {
    // only the keys that are present are overridden, so a partial file keeps
    // the current values — see applySettings
    applySettings(loadSettingsFile())
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
