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

use serde::{Deserialize, Serialize};

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

/// One update to the candidate view, as the page receives it.
///
/// A field that is absent from the JSON means "unchanged" — the same rule the
/// RPC uses, carried through to the page so that moving the highlight does not
/// have to resend the list. `null` would read as a value in JS, so the fields
/// are skipped rather than nulled.
#[derive(Debug, Serialize, PartialEq)]
pub struct CandidateUpdate<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidates: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<i32>,
}

/// Serializes one update for [`apply_candidate_update_script`].
///
/// Separate from the script builder because the payload crosses the event
/// loop as a `UserEvent` (which owns its data) before the script is built.
pub fn candidate_update_json(candidates: Option<&[String]>, selection: Option<i32>) -> String {
    let update = CandidateUpdate {
        candidates,
        selection,
    };
    // A `Vec<String>` and an `i32` cannot fail to serialize; an update that
    // somehow did is not worth taking the UI down over, and an empty object
    // is a no-op on the page.
    serde_json::to_string(&update).unwrap_or_else(|_| "{}".to_string())
}

/// Applies one update — list, highlight, or both — in a single script
/// evaluation. Two evaluations meant two layout passes per keystroke.
pub fn apply_candidate_update_script(payload: &str) -> String {
    format!("applyUpdate({payload})")
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

    /// The keystroke case: one call carrying both halves, which is what
    /// spares the page a second layout pass.
    #[test]
    fn a_combined_update_carries_the_list_and_the_highlight() {
        let candidates = vec!["水".to_string(), "未".to_string()];
        let payload = candidate_update_json(Some(&candidates), Some(0));

        assert_eq!(payload, r#"{"candidates":["水","未"],"selection":0}"#);
        assert_eq!(
            apply_candidate_update_script(&payload),
            r#"applyUpdate({"candidates":["水","未"],"selection":0})"#
        );
    }

    /// An arrow key moves the highlight through a list the page already has.
    /// The list must be ABSENT, not null and not an empty array: either of
    /// those would make the page rebuild (or clear) a list nothing asked it
    /// to touch.
    #[test]
    fn a_selection_only_update_leaves_the_list_out() {
        assert_eq!(candidate_update_json(None, Some(3)), r#"{"selection":3}"#);
    }

    /// ...and the mirror case, which is what a composition ends with: an
    /// EMPTY list is a real value and must survive as one.
    #[test]
    fn an_empty_list_is_not_the_same_as_no_list() {
        assert_eq!(
            candidate_update_json(Some(&[]), None),
            r#"{"candidates":[]}"#
        );
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
