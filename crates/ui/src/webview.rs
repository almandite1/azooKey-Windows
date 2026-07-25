//! The two directions of Rust ↔ webview traffic, both of them typed.
//!
//! [`WebviewMessage`] is what the pages post to us; the `*_script` builders
//! are what we evaluate in them. Neither used to be one thing: the inbound
//! side was hand-dug out of a `serde_json::Value` in main.rs and the outbound
//! side was one tested constructor plus two raw `format!`s. A mismatch on the
//! inbound side is a SILENT failure — ui.exe waits out the 120 ms grace period
//! for a height that never parses and then shows the window anyway — which is
//! why the shape is pinned by a test against the literal JSON candidate.js
//! sends.

use serde::Deserialize;

/// A message posted by one of the webviews via `window.ipc.postMessage`.
///
/// Untagged variants would silently accept the wrong shape; `type` is the
/// discriminator candidate.js already sends.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum WebviewMessage {
    /// candidate.js measured itself and wants the window that tall, in CSS px.
    Resize { height: f64 },
}

/// Parses one posted message, or `None` when it is not one we know.
pub fn parse_webview_message(body: &str) -> Option<WebviewMessage> {
    serde_json::from_str(body).ok()
}

/// Pushes a fresh candidate list into the candidate webview. `candidates` is
/// the already-serialized JSON array (window_actions builds it from the RPC's
/// `Vec<String>`).
pub fn update_candidates_script(candidates: &str) -> String {
    format!("updateCandidates({candidates})")
}

/// Moves the highlight in the candidate webview.
pub fn update_selection_script(index: i32) -> String {
    format!("updateSelection({index})")
}

/// The script that pushes a new input mode into the indicator webview.
///
/// The mode string ("あ"/"A") arrives over the `SetInputMode` RPC, which any
/// local process on the pipe can call with an arbitrary payload, so it is
/// untrusted input: `serde_json` turns it into a properly quoted and escaped
/// JS string literal. A raw `"{}"` interpolation let a `"` or `\` break out
/// of the literal and inject script into the (UIAccess) webview.
pub fn update_input_method_script(mode: &str) -> String {
    // an un-serializable mode is not worth dropping the update over; an
    // empty indicator is the safe answer
    let arg = serde_json::to_string(mode).unwrap_or_else(|_| "\"\"".to_string());
    format!("updateInputMethod({arg})")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE message candidate.js posts, copied from `postHeight` in
    /// assets/candidate.js. If the two ever disagree, the height never
    /// arrives, `CandidatePlacement` never sees `on_height`, and every
    /// composition sits out the full grace period before appearing — with
    /// nothing logged anywhere.
    #[test]
    fn the_resize_message_candidate_js_posts_parses() {
        assert_eq!(
            parse_webview_message(r#"{"type":"resize","height":168}"#),
            Some(WebviewMessage::Resize { height: 168.0 })
        );
    }

    #[test]
    fn an_unknown_or_malformed_message_is_ignored() {
        assert_eq!(parse_webview_message(r#"{"type":"whatever"}"#), None);
        assert_eq!(parse_webview_message(r#"{"height":168}"#), None);
        assert_eq!(parse_webview_message("not json at all"), None);
        // the height is required: a resize with no height is not a resize
        assert_eq!(parse_webview_message(r#"{"type":"resize"}"#), None);
    }

    #[test]
    fn the_candidate_scripts_call_the_page_functions() {
        assert_eq!(
            update_candidates_script(r#"["水","未"]"#),
            r#"updateCandidates(["水","未"])"#
        );
        assert_eq!(update_selection_script(2), "updateSelection(2)");
    }

    #[test]
    fn the_mode_string_reaches_the_webview_quoted() {
        assert_eq!(
            update_input_method_script("あ"),
            r#"updateInputMethod("あ")"#
        );
        assert_eq!(update_input_method_script("A"), r#"updateInputMethod("A")"#);
    }

    /// The injection this guards against: a mode string carrying a quote used
    /// to close the JS literal and let everything after it run as script. The
    /// payload must come out as data — one string argument, escaped.
    #[test]
    fn a_quote_in_the_mode_string_cannot_break_out_of_the_literal() {
        let script = update_input_method_script(r#""); alert(1); //"#);

        assert_eq!(
            script, r#"updateInputMethod("\"); alert(1); //")"#,
            "the quote must be escaped, not terminate the argument"
        );
    }

    /// A trailing backslash is the other half of the same trick: unescaped it
    /// would escape the closing quote instead.
    #[test]
    fn backslashes_and_newlines_are_escaped() {
        assert_eq!(
            update_input_method_script(r"back\slash"),
            r#"updateInputMethod("back\\slash")"#
        );
        assert_eq!(
            update_input_method_script("two\nlines"),
            r#"updateInputMethod("two\nlines")"#,
            "a raw newline inside a JS string literal is a syntax error"
        );
    }
}
