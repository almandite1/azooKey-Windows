#ifndef ffi_h
#define ffi_h

#include <stdint.h>

/*
 * THE canonical declaration of the engine's C FFI.
 *
 * The Swift side (azookey_server.swift) implements these with @_cdecl
 * exports; the Rust side (crates/server/src/ffi.rs) mirrors them in an
 * extern "C" block. Swift imports this header (module `ffi`), so the
 * struct layout is single-sourced, but the FUNCTION signatures are not
 * checked mechanically against either implementation — when you add or
 * change a function, update all three places and run the --ignored smoke
 * tests (crates/server/tests/ipc_smoke.rs) against a live server.
 *
 * Contract:
 * - all exported engine functions must be called from a single thread,
 *   serially; the Swift side keeps unsynchronized global state
 * - all out-parameters (cursor, length) are 32-bit ints
 * - the leading `session` (int32) parameter selects the per-client
 *   composing state; sessions are created on first use and discarded
 *   via RemoveSession
 * - the char* members below and every returned string/list are owned by
 *   the Swift side; the caller copies them and hands them back to
 *   FreeString / FreeComposedText
 */
struct FFICandidate {
    char *text;
    char *subtext;
    int correspondingCount;
};

/* engine lifecycle */
void Initialize(const char *path);
void LoadConfig(void);

/* per-session composing text */
void SetContext(int32_t session, const char *context);
char *AppendText(int32_t session, const char *input, int32_t *cursorPtr);
char *RemoveText(int32_t session, int32_t *cursorPtr);
char *MoveCursor(int32_t session, int32_t offset, int32_t *cursorPtr);
char *ShrinkText(int32_t session, int32_t offset);
void ClearText(int32_t session);
struct FFICandidate **GetComposedText(int32_t session, int32_t *lengthPtr);
void RemoveSession(int32_t session);

/* ownership hand-back */
void FreeString(char *ptr);
void FreeComposedText(struct FFICandidate **listPtr, int32_t length);

#endif /* ffi_h */
