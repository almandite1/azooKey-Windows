#ifndef ffi_h
#define ffi_h

#include <stdbool.h>
#include <stdint.h>

/*
 * THE canonical declaration of the engine's C FFI.
 *
 * The Swift side (Sources/azookey-server/FFIExports.swift, on top of
 * EngineConfig.swift / EngineState.swift / Conversion.swift) implements
 * these with @_cdecl exports; the Rust side (crates/server/src/ffi.rs)
 * mirrors them in an extern "C" block. Swift imports this header
 * (module `ffi`), so the
 * struct layout is single-sourced, but the FUNCTION signatures are not
 * checked mechanically against either implementation — when you add or
 * change a function, update all three places and run the --ignored smoke
 * tests (crates/server/tests/ipc_smoke.rs) against a live server.
 *
 * Contract:
 * - all exported engine functions must be called from a single thread,
 *   serially; the Swift side keeps unsynchronized global state
 * - all out-parameters (cursor, length) are 32-bit ints
 * - the leading `session` (int64) parameter selects the per-client
 *   composing state; sessions are created on first use and discarded
 *   via RemoveSession. It is 64-bit so the monotonic per-connection
 *   counter that produces it cannot realistically wrap onto a live
 *   session and cross-wire two applications' composing state
 * - every `const char *` INPUT must be non-null and NUL-terminated; the
 *   Swift side dereferences it without checking. Every returned pointer,
 *   by contrast, MAY be null (an empty reading, an allocation the engine
 *   declined), so the Rust side treats null as the empty result rather
 *   than as an error
 * - the char* members below and every returned string/list are owned by
 *   the Swift side; the caller copies them and hands them back to
 *   FreeString / FreeComposedText
 */
/*
 * A candidate, with the reading it covers counted in both units: kana for
 * the engine (ShrinkText spends it) and romaji keystrokes for the client
 * (it drops that many characters from its raw input). They differ whenever
 * a clause boundary falls inside a romaji cluster.
 */
struct FFICandidate {
    char *text;
    char *subtext;
    int correspondingCount;
    int surfaceCount;
};

/* engine lifecycle */
void Initialize(const char *path);
/*
 * Applies the settings in `json` — the whole settings.json document as text.
 *
 * Takes the text rather than reading the file itself, and returns whether it
 * worked, for three reasons that were all one bug. The engine used to read
 * settings.json on its own while the Rust caller read it too, in the same
 * UpdateConfig, so a save landing between the two reads applied half of one
 * version and half of the other. A decode failure was kept to itself, so the
 * settings app reported success while conversion carried on with the old
 * values. And the path `%APPDATA%\Azookey\settings.json` was spelled out on
 * both sides of the boundary.
 *
 * False means nothing was applied and the engine kept the configuration it
 * had; the caller reports that rather than claiming the save took effect.
 */
bool LoadConfig(const char *json);

/*
 * per-session composing text
 *
 * `cursorPtr` always receives the ABSOLUTE cursor position in the reading
 * after the call, counted in kana from its start — never a delta. MoveCursor
 * takes a relative `offset` but reports where the cursor ended up, clamped to
 * the reading, exactly as AppendText and RemoveText do (#82).
 */
void SetContext(int64_t session, const char *context);
char *AppendText(int64_t session, const char *input, int32_t *cursorPtr);
char *RemoveText(int64_t session, int32_t *cursorPtr);
char *MoveCursor(int64_t session, int32_t offset, int32_t *cursorPtr);
/*
 * `confirmedCandidate` (ShrinkText and ClearText) says which candidate the
 * user accepted, so the engine can learn from it.
 *
 * It indexes the RAW engine list — the `mainResults` order this session was
 * last handed by GetComposedText — and NOT what the candidate window showed:
 * the Rust side dedupes that list and lets plugins insert rows into it, so
 * the two orders differ. Translating one to the other is crates/server's job
 * (service.rs), and it sends -1 for a row the engine never produced.
 *
 * -1 means "learn nothing", which is also what an older client that sends no
 * index at all comes out as. Anything out of range is ignored rather than
 * trapped: the pipe carrying these calls can be opened by any local process.
 *
 * Learning happens BEFORE the composition is torn down, inside the same
 * call, because ending a composition drops the converter state the learned
 * entry is chained onto.
 */
char *ShrinkText(int64_t session, int32_t surfaceOffset, int32_t confirmedCandidate);
void ClearText(int64_t session, int32_t confirmedCandidate);
struct FFICandidate **GetComposedText(int64_t session, int32_t *lengthPtr);
void RemoveSession(int64_t session);

/*
 * Forgets everything learned so far, and reports whether it happened.
 *
 * False when there is no memory directory configured or it does not exist —
 * the engine never creates it (the Rust side does, and locks its DACL down),
 * so a reset with nowhere to reset is reported rather than silently
 * succeeding.
 */
bool ResetLearning(void);

/* ownership hand-back */
void FreeString(char *ptr);
void FreeComposedText(struct FFICandidate **listPtr, int32_t length);

#endif /* ffi_h */
