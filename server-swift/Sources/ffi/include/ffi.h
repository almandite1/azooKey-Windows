#ifndef ffi_h
#define ffi_h

#include <stdio.h>

#endif /* ffi_h */

/*
 * FFI contract (see azookey_server.swift and crates/server/src/main.rs):
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
    char *hiragana;
    int correspondingCount;
};
