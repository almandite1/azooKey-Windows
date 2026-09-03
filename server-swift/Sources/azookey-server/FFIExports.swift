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

/// A sentence's worth of reading, not a syllable.
///
/// Length is the whole point: see `warmUpConverter`. Roman input because that
/// is what the warm-up feeds `ComposingText`, and the kana it produces
/// (けさはいいてんきなのでこうえんまであるいていきました) is ordinary prose,
/// so the conversion exercises the dictionary the way real typing does.
private let warmUpReading = "kesahaiitenkinanodekouenmadearuiteikimashita"

/// One conversion, thrown away, so that whatever the current options need
/// loading is loaded before a keystroke waits on it.
///
/// The expensive part is the zenz model: gguf load and the first inference run
/// on a cold model take seconds, and every FFI call is serialized onto the
/// server's single thread — so that cost lands on whichever keystroke happens
/// to be first, with the whole desktop's typing behind it.
///
/// The input has to be sentence-length for that to work. It was one character,
/// which loaded the gguf but only ever ran a single-token batch — and with the
/// model offloaded to a GPU backend, a Vulkan driver compiles its
/// matrix-multiply pipelines per batch-size regime, on first submission,
/// taking seconds. So the first real sentence anyone typed after installing
/// still stalled, which is the exact failure this function exists to prevent.
/// Measured once per machine (drivers cache compiled pipelines on disk) and
/// again after every driver update.
@MainActor private func warmUpConverter(_ engine: KanaKanjiConverter) {
    var warmup = ComposingText()
    warmup.insertAtCursorPosition(warmUpReading, inputStyle: .roman2kana)
    // logged because it is otherwise invisible: this is the one place that
    // pays the cold-start cost, and the number tells whoever reads the log
    // whether it stayed here or leaked onto someone's first keystroke
    let elapsed = ContinuousClock().measure {
        _ = engine.requestCandidates(warmup, options: getOptions())
    }
    enginePrint(level: .info, "converter warm-up took \(elapsed)")

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
    //
    // Timed for the same reason the warm-up below is, except that this is the
    // half nobody had a number for. `preloadDictionary` reads the whole
    // dictionary before the server can open its pipe, and measured against the
    // server's own startup log it -- not the warm-up -- is what owns that
    // window: on ten consecutive starts the warm-up ran 0.46-2.08s while the
    // phase before it ran 1.58-6.83s. Right after an install the file cache is
    // cold and none of it has been read before, which is the state issue #108
    // describes.
    let buildStart = ContinuousClock().now
    let engine = KanaKanjiConverter(
        dictionaryURL: execURL.appendingPathComponent("Dictionary"),
        preloadDictionary: true
    )
    enginePrint(
        level: .info,
        "converter construction took \(ContinuousClock().now - buildStart)"
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

/// Whether the converter is both allowed to learn and has somewhere to keep
/// what it learns. Both halves, everywhere — a commit with no directory
/// writes into the relative placeholder path.
@MainActor private var learningIsLive: Bool {
    config.learningEnabled && config.memoryDirectory != nil
}

/// Feeds one accepted candidate back into the converter's memory.
///
/// MUST be called before `endComposition()` in the same export.
/// `stopComposition` drops the converter's `lastData`, which is what chains
/// this entry to the previous one — learning after it still records the
/// candidate but loses the bigram, so the ordering here is the contract, not
/// a style preference.
///
/// RAM only: `updateLearningData` does not touch the disk. Persisting is
/// `commitUpdateLearningData`, which the confirming ClearText and
/// RemoveSession do.
///
/// `index` is whatever arrived over the FFI, so every guard below is against
/// untrusted input rather than against a bug: negative means "learn nothing"
/// and out of range is ignored.
///
/// ACCEPTED: the `lastData` chain the bigram hangs off is the CONVERTER's,
/// and one converter serves every application. Two apps confirming in
/// alternation can therefore chain across each other — the same
/// process-wide-state caveat as `endComposition` below, and the same real
/// fix: the per-session converter state coming upstream (feat/session_api).
/// The candidate itself is always learned correctly; only the pairing with
/// what came before it can be another window's.
@MainActor private func learn(session: Int64, candidateIndex index: Int32) {
    guard index >= 0, learningIsLive, let converter else { return }
    let candidate = withSession(session) { state -> Candidate? in
        let i = Int(index)
        return state.lastCandidates.indices.contains(i) ? state.lastCandidates[i] : nil
    }
    guard let candidate else { return }
    converter.updateLearningData(candidate)
    hasUnsavedLearning = true
}

/// Writes what has been learned to the memory directory — and only then.
///
/// The engine's commit has no notion of "nothing to do": it hands the
/// temporary trie to `LongTermLearningMemory.merge`, which always reaches
/// `update(trie:directoryURL:)` and rewrites every file in the directory,
/// empty trie or not. So an unconditional commit leaves the memory store's
/// timestamps saying the user's input was recorded when nothing was.
///
/// That is not a cosmetic difference. This directory is the user's input
/// history, kept behind a locked-down DACL for that reason, and "was that
/// cancelled conversion learned?" is answered by looking at it. A cancel
/// arriving while an unrelated idle session happened to be retired used to
/// rewrite all of it (#110).
@MainActor private func commitLearning() {
    guard hasUnsavedLearning, learningIsLive else { return }
    converter?.commitUpdateLearningData()
    hasUnsavedLearning = false
}

@_cdecl("ClearText")
@MainActor public func clear_text(session: Int64, confirmedCandidate: Int32) {
    // Order matters at every step here.
    //
    // 1. Learn first: `endComposition` below drops the converter state this
    //    hangs off (see `learn`).
    learn(session: session, candidateIndex: confirmedCandidate)
    // 2. Persist, but only for a confirmation. This is the end of a sentence
    //    — Enter — so it is the natural place to pay a disk write, and it
    //    bounds what a crash or a watchdog restart can lose to the sentence
    //    in progress. Not per keystroke: that would write on every key.
    //
    //    `commitLearning` adds the other half of the condition, and it is
    //    what makes the client's idempotent RETRY of this call free: the
    //    retry carries the same index, but step 3 below has already emptied
    //    the list so its `learn` finds nothing, and with nothing learned
    //    there is nothing to write a second time.
    if confirmedCandidate >= 0 {
        commitLearning()
    }
    withSession(session) { state in
        state.composingText = ComposingText()
        // 3. Emptied so a RETRY of this same call learns nothing. The client
        //    retries ClearText as idempotent (it is, for composing state),
        //    and without this a resent confirmation would be counted twice.
        state.lastCandidates = []
    }
    // 4. Last, for the reason in step 1.
    endComposition()
}

@_cdecl("RemoveSession")
@MainActor public func remove_session(session: Int64) {
    // An idle session being retired is the other moment worth a disk write:
    // whatever it learned since its last confirmation would otherwise sit in
    // RAM until the process exits, which a crash does not wait for.
    //
    // This is not a moment the user chose. Eviction runs off any request
    // from any application (`session_of` in the Rust server), so the write
    // has to be conditional on there being something to write — see
    // `commitLearning`.
    commitLearning()
    sessions.removeValue(forKey: session)
    endComposition()
}

/// Forgets everything the converter has learned.
///
/// Deliberately does NOT create the directory when it is missing: creating
/// it belongs to the Rust server, which also locks its DACL down (see
/// `memory_dir.rs`), and a directory created here would hold the user's
/// input history with default permissions. The server's handler runs that
/// step before calling this, so by the time we get here the directory either
/// exists or could not be made — and `false` says which.
@_cdecl("ResetLearning")
@MainActor public func reset_learning() -> Bool {
    guard let directory = config.memoryDirectory,
          FileManager.default.fileExists(atPath: directory.path),
          let converter
    else {
        return false
    }
    converter.resetMemory()
    // `resetMemory` empties the temporary trie along with the store, so
    // there is nothing left for a later commit to write — and saying so
    // stops that commit rewriting the freshly emptied directory.
    hasUnsavedLearning = false
    return true
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
    // Kept whole, before anything below turns it into C strings and throws
    // the objects away: learning needs the `Candidate`, and the index the
    // client eventually confirms is an index into exactly this (see ffi.h).
    withSession(session) { state in
        state.lastCandidates = converted.mainResults
    }
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
    surfaceOffset: Int32,
    confirmedCandidate: Int32
) -> UnsafeMutablePointer<CChar>? {
    // Before `spend` below, for the same reason ClearText learns first: the
    // candidate is about to stop being what this composition is about. No
    // commit here — a clause confirmation is mid-sentence, and the write
    // happens when the sentence ends.
    learn(session: session, candidateIndex: confirmedCandidate)
    return withSession(session) { state in
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
