//! The engine FFI is declared in three places that must agree: the canonical
//! C header (`server-swift/Sources/ffi/include/ffi.h`), the Rust `extern "C"`
//! mirror (`crates/server/src/ffi.rs`), and the Swift `@_cdecl` exports
//! (`server-swift/Sources/azookey-server/FFIExports.swift`). Nothing links
//! them at build time — a function added to two of the three, or a name
//! typo'd in one, compiles fine and only fails as a missing symbol at load
//! time on a live server.
//!
//! This test closes that one gap: it extracts the exported function *names*
//! from all three sources and asserts the sets are identical (there are
//! twelve today). It deliberately does not check argument types — a regex
//! over C/Rust/Swift signatures would be brittle, and the failure this
//! guards against is the missing-or-misspelled function, not a subtly wrong
//! argument. The `--ignored` smoke tests (ipc_smoke.rs) cover the calls end
//! to end against the real engine.

use std::collections::BTreeSet;

const HEADER: &str = include_str!("../../../server-swift/Sources/ffi/include/ffi.h");
const RUST_FFI: &str = include_str!("../src/ffi.rs");
const SWIFT_EXPORTS: &str =
    include_str!("../../../server-swift/Sources/azookey-server/FFIExports.swift");

/// Names exported from the Swift side: every `@_cdecl("Name")`.
fn swift_cdecl_names(src: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let marker = "@_cdecl(\"";
    let mut rest = src;
    while let Some(start) = rest.find(marker) {
        let after = &rest[start + marker.len()..];
        if let Some(end) = after.find('"') {
            names.insert(after[..end].to_string());
            rest = &after[end..];
        } else {
            break;
        }
    }
    names
}

/// Names declared on the Rust side: every `pub(crate) fn Name` in the
/// `extern "C"` block (the only functions in ffi.rs).
fn rust_extern_names(src: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let marker = "pub(crate) fn ";
    let mut rest = src;
    while let Some(start) = rest.find(marker) {
        let after = &rest[start + marker.len()..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if !name.is_empty() {
            names.insert(name);
        }
        rest = after;
    }
    names
}

/// Removes `/* ... */` and `// ...` comments so the C header can be scanned
/// for declarations without matching prose that happens to name a function.
fn strip_c_comments(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else if bytes[i] == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

/// Names declared in the C header: each identifier that directly precedes a
/// `(`. After comments are stripped the header holds only the struct (whose
/// members carry no parens) and the function declarations, so the identifier
/// before every `(` is an exported function.
fn c_header_names(src: &str) -> BTreeSet<String> {
    let stripped = strip_c_comments(src);
    let chars: Vec<char> = stripped.chars().collect();
    let mut names = BTreeSet::new();
    for (idx, &c) in chars.iter().enumerate() {
        if c != '(' {
            continue;
        }
        // walk back over whitespace, then collect the identifier
        let mut j = idx;
        while j > 0 && chars[j - 1].is_whitespace() {
            j -= 1;
        }
        let end = j;
        while j > 0 && (chars[j - 1].is_alphanumeric() || chars[j - 1] == '_') {
            j -= 1;
        }
        if j < end {
            names.insert(chars[j..end].iter().collect::<String>());
        }
    }
    names
}

#[test]
fn ffi_function_names_agree_across_the_three_sources() {
    let header = c_header_names(HEADER);
    let rust = rust_extern_names(RUST_FFI);
    let swift = swift_cdecl_names(SWIFT_EXPORTS);

    assert!(
        !header.is_empty(),
        "parsed no function names from ffi.h; the extraction is broken, not the sources"
    );

    assert_eq!(
        header,
        rust,
        "ffi.h and crates/server/src/ffi.rs disagree.\n  only in ffi.h: {:?}\n  only in ffi.rs: {:?}",
        header.difference(&rust).collect::<Vec<_>>(),
        rust.difference(&header).collect::<Vec<_>>(),
    );

    assert_eq!(
        header,
        swift,
        "ffi.h and FFIExports.swift disagree.\n  only in ffi.h: {:?}\n  only in @_cdecl: {:?}",
        header.difference(&swift).collect::<Vec<_>>(),
        swift.difference(&header).collect::<Vec<_>>(),
    );
}
