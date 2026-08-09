//! The engine FFI is declared in three places that must agree: the canonical
//! C header (`server-swift/Sources/ffi/include/ffi.h`), the Rust `extern "C"`
//! mirror (`crates/server/src/ffi.rs`), and the Swift `@_cdecl` exports
//! (`server-swift/Sources/azookey-server/FFIExports.swift`). Nothing links
//! them at build time — a function added to two of the three, or a name
//! typo'd in one, compiles fine and only fails as a missing symbol at load
//! time on a live server.
//!
//! This test closes that gap for the two properties a text scan can check
//! reliably: the exported function NAMES and their ARITY. A wrong argument
//! count is worse than a missing symbol — it does not fail to link, it
//! corrupts the stack — and it is exactly what editing one of the three and
//! forgetting the others produces. `MoveCursor` gaining its `cursorPtr`
//! (#82) had to touch all three.
//!
//! It deliberately still does not check argument TYPES: a regex over C, Rust
//! and Swift type syntax would be brittle in a way that costs more than it
//! catches. The `--ignored` smoke tests (ipc_smoke.rs) cover the calls end to
//! end against the real engine.

use std::collections::BTreeMap;

const HEADER: &str = include_str!("../../../server-swift/Sources/ffi/include/ffi.h");
const RUST_FFI: &str = include_str!("../src/ffi.rs");
const SWIFT_EXPORTS: &str =
    include_str!("../../../server-swift/Sources/azookey-server/FFIExports.swift");

/// One source's view of the FFI: exported name -> number of parameters.
type Surface = BTreeMap<String, usize>;

/// The text between the `(` at `open` and its matching `)`.
///
/// `None` when the parentheses do not balance before the end of the input,
/// which for these three files means the extraction is broken rather than the
/// source.
fn parameter_list(chars: &[char], open: usize) -> Option<String> {
    let mut depth = 0usize;
    for (idx, &c) in chars.iter().enumerate().skip(open) {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(chars[open + 1..idx].iter().collect());
                }
            }
            _ => {}
        }
    }
    None
}

/// How many parameters a parameter list declares.
///
/// Counts the non-empty segments between TOP-LEVEL commas, not the commas
/// themselves. Two reasons for the distinction: a nested type
/// (`UnsafeMutablePointer<Int32>`, `*mut *mut FFICandidate`) must not inflate
/// the count, and the Rust mirror writes its multi-line signatures with a
/// TRAILING comma, which counting commas reads as one parameter too many.
/// C's `(void)` is the spelling for none.
fn arity(params: &str) -> usize {
    let trimmed = params.trim();
    if trimmed == "void" {
        return 0;
    }

    let mut depth = 0usize;
    let mut segments = 0usize;
    let mut current_has_content = false;
    for c in trimmed.chars() {
        match c {
            '(' | '<' | '[' => depth += 1,
            ')' | '>' | ']' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                if current_has_content {
                    segments += 1;
                }
                current_has_content = false;
                continue;
            }
            _ => {}
        }
        if !c.is_whitespace() {
            current_has_content = true;
        }
    }
    segments + usize::from(current_has_content)
}

/// Removes `/* ... */` and `// ...` comments so a source can be scanned for
/// declarations without matching prose that happens to name a function.
fn strip_comments(src: &str) -> String {
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

/// The C header: after comments are stripped it holds only the struct (whose
/// members carry no parens) and the function declarations, so the identifier
/// before every `(` is an exported function.
fn c_header_surface(src: &str) -> Surface {
    let chars: Vec<char> = strip_comments(src).chars().collect();
    let mut surface = Surface::new();
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
        if j == end {
            continue;
        }
        let name: String = chars[j..end].iter().collect();
        if let Some(params) = parameter_list(&chars, idx) {
            surface.insert(name, arity(&params));
        }
    }
    surface
}

/// Reads a surface out of a source by finding each declaration's name and the
/// `(` that opens its parameter list.
///
/// `locate` is given the source and a byte offset to resume from, and answers
/// `(name, offset of its `(`, offset to resume from)`. Shared by the Rust and
/// Swift extractors, which differ only in how they find those two things.
fn surface_from(
    src: &str,
    mut locate: impl FnMut(&str, usize) -> Option<(String, usize, usize)>,
) -> Surface {
    let stripped = strip_comments(src);
    // These files are ASCII once comments are stripped (the Japanese lives in
    // comments and string literals the declarations do not contain), so byte
    // offsets and char indices coincide. `parameter_list` walks chars.
    let chars: Vec<char> = stripped.chars().collect();
    let mut surface = Surface::new();
    let mut from = 0usize;
    while let Some((name, open, next)) = locate(&stripped, from) {
        from = next;
        if chars.get(open) == Some(&'(')
            && let Some(params) = parameter_list(&chars, open)
        {
            surface.insert(name, arity(&params));
        }
    }
    surface
}

/// The Rust side: every `pub(crate) fn Name(..)` in the `extern "C"` block
/// (the only functions in ffi.rs).
fn rust_extern_surface(src: &str) -> Surface {
    const MARKER: &str = "pub(crate) fn ";
    surface_from(src, |stripped, from| {
        let offset = stripped[from..].find(MARKER)?;
        let name_start = from + offset + MARKER.len();
        let name: String = stripped[name_start..]
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        let after = name_start + name.len();
        (!name.is_empty()).then_some((name, after, after))
    })
}

/// The Swift side: every `@_cdecl("Name")`, whose arity comes from the `func`
/// signature that follows it — past any further attributes (`@MainActor`,
/// `public`), which is why this looks for `func` rather than assuming the
/// next line.
fn swift_cdecl_surface(src: &str) -> Surface {
    const MARKER: &str = "@_cdecl(\"";
    surface_from(src, |stripped, from| {
        let offset = stripped[from..].find(MARKER)?;
        let name_start = from + offset + MARKER.len();
        let name_len = stripped[name_start..].find('"')?;
        let name = stripped[name_start..name_start + name_len].to_string();
        let after_name = name_start + name_len;

        let func_offset = stripped[after_name..].find("func ")?;
        let after_func = after_name + func_offset + "func ".len();
        let paren_offset = stripped[after_func..].find('(')?;
        Some((name, after_func + paren_offset, after_func + paren_offset))
    })
}

/// Renders a disagreement per function, because "these two maps differ" over
/// twelve entries is not a diagnosis.
fn differences(left: &Surface, right: &Surface) -> Vec<String> {
    let mut out = Vec::new();
    for (name, l) in left {
        match right.get(name) {
            None => out.push(format!("{name}: missing on the right")),
            Some(r) if r != l => {
                out.push(format!("{name}: {l} args on the left, {r} on the right"))
            }
            Some(_) => {}
        }
    }
    for name in right.keys() {
        if !left.contains_key(name) {
            out.push(format!("{name}: missing on the left"));
        }
    }
    out
}

#[test]
fn the_ffi_surface_agrees_across_the_three_sources() {
    let header = c_header_surface(HEADER);
    let rust = rust_extern_surface(RUST_FFI);
    let swift = swift_cdecl_surface(SWIFT_EXPORTS);

    // A parser that finds nothing agrees with everything, so the extraction
    // has to prove it is still working before its verdict means anything.
    assert!(
        !header.is_empty(),
        "parsed no functions from ffi.h; the extraction is broken, not the sources"
    );
    assert!(
        header.values().any(|arity| *arity > 0),
        "every function parsed as taking no arguments; the arity extraction is broken"
    );

    let against_rust = differences(&header, &rust);
    assert!(
        against_rust.is_empty(),
        "ffi.h and crates/server/src/ffi.rs disagree (left = ffi.h):\n  {}",
        against_rust.join("\n  ")
    );

    let against_swift = differences(&header, &swift);
    assert!(
        against_swift.is_empty(),
        "ffi.h and FFIExports.swift disagree (left = ffi.h):\n  {}",
        against_swift.join("\n  ")
    );
}

mod extraction {
    //! The extraction is the part that can silently stop working. The
    //! assertions above catch a total failure; these pin the shapes that
    //! actually appear in the three sources.
    use super::*;

    #[test]
    fn arity_counts_parameters_not_commas() {
        assert_eq!(arity(""), 0);
        assert_eq!(arity("void"), 0);
        assert_eq!(arity("int64_t session"), 1);
        assert_eq!(arity("int64_t session, int32_t *cursorPtr"), 2);
        // the nested-type shapes each language contributes
        assert_eq!(
            arity("session: Int64, cursorPtr: UnsafeMutablePointer<Int32>"),
            2
        );
        assert_eq!(arity("listPtr: *mut *mut FFICandidate, length: c_int"), 2);
        assert_eq!(arity("struct FFICandidate **listPtr, int32_t length"), 2);
    }

    /// The Rust mirror's multi-line signatures carry a trailing comma —
    /// rustfmt puts it there — so counting commas reads three parameters as
    /// four. This is the case that broke the first version of the check.
    #[test]
    fn a_trailing_comma_is_not_a_parameter() {
        assert_eq!(
            arity("\n    session: i64,\n    input: *const c_char,\n    cursorPtr: *mut c_int,\n"),
            3
        );
        assert_eq!(arity("session: i64,"), 1);
    }

    #[test]
    fn the_c_header_yields_the_declared_functions() {
        let surface = c_header_surface(HEADER);
        // one parameter, the settings document as text: the engine no longer
        // opens settings.json itself, so this RPC reads it once (#105)
        assert_eq!(surface.get("LoadConfig"), Some(&1));
        assert_eq!(surface.get("Initialize"), Some(&1));
        // the third is `confirmedCandidate`, the index the user accepted
        assert_eq!(surface.get("ShrinkText"), Some(&3));
        assert_eq!(surface.get("AppendText"), Some(&3));
        assert_eq!(surface.get("MoveCursor"), Some(&3));
        // `(void)` — the spelling for none, and the only declaration here
        // that uses it
        assert_eq!(surface.get("ResetLearning"), Some(&0));
    }

    /// Multi-line signatures are the shape both the Rust mirror and the Swift
    /// exports use for the longer calls, so the extraction must not be
    /// line-based.
    #[test]
    fn multi_line_signatures_are_read_whole() {
        assert_eq!(rust_extern_surface(RUST_FFI).get("AppendText"), Some(&3));
        assert_eq!(
            swift_cdecl_surface(SWIFT_EXPORTS).get("AppendText"),
            Some(&3)
        );
    }

    /// The attribute stack between `@_cdecl` and `func` must not throw the
    /// search off the declaration the attribute belongs to.
    #[test]
    fn swift_attributes_between_the_cdecl_and_the_func_are_skipped() {
        let surface = swift_cdecl_surface(SWIFT_EXPORTS);
        assert_eq!(surface.get("ClearText"), Some(&2));
        assert_eq!(surface.get("FreeComposedText"), Some(&2));
    }

    /// The check has to FAIL when the sources disagree, which is the one
    /// property the real test can never demonstrate while they agree.
    #[test]
    fn a_disagreement_is_reported_per_function() {
        let left = Surface::from([
            ("Same".into(), 2),
            ("Widened".into(), 2),
            ("Gone".into(), 1),
        ]);
        let right = Surface::from([("Same".into(), 2), ("Widened".into(), 3), ("New".into(), 0)]);

        let reported = differences(&left, &right);

        assert_eq!(
            reported,
            vec![
                "Gone: missing on the right".to_string(),
                "Widened: 2 args on the left, 3 on the right".to_string(),
                "New: missing on the left".to_string(),
            ]
        );
    }
}
