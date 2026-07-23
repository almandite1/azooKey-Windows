// https://www.unicode.org/charts/nameslist/n_FF00.html
// extracted with scripts/extract_fullwidth.py

use std::{collections::HashMap, sync::LazyLock};

// azooKey never converts ASCII letters or digits to full width, so 0-9, A-Z
// and a-z are intentionally omitted from this map (they map to themselves).
// The full mapping that does include them is HALF_FULL below.
static HALF_FULL_AZOOKEY: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    HashMap::from([
        ("!", "！"),
        ("\"", "”"),
        ("#", "＃"),
        ("$", "＄"),
        ("%", "％"),
        ("&", "＆"),
        ("'", "’"),
        ("(", "（"),
        (")", "）"),
        ("*", "＊"),
        ("+", "＋"),
        (",", "、"),
        ("-", "ー"),
        (".", "。"),
        ("/", "・"),
        (":", "："),
        (";", "；"),
        ("<", "＜"),
        ("=", "＝"),
        (">", "＞"),
        ("?", "？"),
        ("@", "＠"),
        ("[", "「"),
        ("\\", "￥"),
        ("]", "」"),
        ("^", "＾"),
        ("_", "＿"),
        ("`", "｀"),
        ("{", "｛"),
        ("|", "｜"),
        ("}", "｝"),
        ("~", "～"),
    ])
});

static HALF_FULL: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    HashMap::from([
        ("a", "ａ"),
        ("b", "ｂ"),
        ("c", "ｃ"),
        ("d", "ｄ"),
        ("e", "ｅ"),
        ("f", "ｆ"),
        ("g", "ｇ"),
        ("h", "ｈ"),
        ("i", "ｉ"),
        ("j", "ｊ"),
        ("k", "ｋ"),
        ("l", "ｌ"),
        ("m", "ｍ"),
        ("n", "ｎ"),
        ("o", "ｏ"),
        ("p", "ｐ"),
        ("q", "ｑ"),
        ("r", "ｒ"),
        ("s", "ｓ"),
        ("t", "ｔ"),
        ("u", "ｕ"),
        ("v", "ｖ"),
        ("w", "ｗ"),
        ("x", "ｘ"),
        ("y", "ｙ"),
        ("z", "ｚ"),
    ])
});

pub fn to_halfwidth(s: &str) -> String {
    s.chars()
        .map(|c| {
            let key = c.to_string();
            if let Some((&k, _)) = HALF_FULL_AZOOKEY.iter().find(|&(_, &v)| v == key) {
                k.to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

pub fn to_fullwidth(s: &str, process_alphabet: bool) -> String {
    s.chars()
        .map(|c| {
            let key = c.to_string();

            if process_alphabet && let Some(&v) = HALF_FULL.get(key.as_str()) {
                return v.to_string();
            }

            if let Some(&v) = HALF_FULL_AZOOKEY.get(key.as_str()) {
                v.to_string()
            } else {
                c.to_string()
            }
        })
        .collect()
}

/// Full-width ASCII conversion, MS-IME F9 style: the printable ASCII range
/// U+0021..=U+007E maps to its full-width twin at +0xFEE0, and the space maps
/// to the ideographic space U+3000. Unlike [`to_fullwidth`], symbols become
/// their plain full-width ASCII forms (`-` → `－`, `,` → `，`, `.` → `．`,
/// `/` → `／`) rather than the kana punctuation the kana-input map produces
/// (`ー`, `、`, `。`, `・`). Non-ASCII characters pass through unchanged.
pub fn to_fullwidth_ascii(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ' ' => '\u{3000}',
            '!'..='~' => char::from_u32(c as u32 + 0xFEE0).unwrap_or(c),
            _ => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbols_are_converted_to_fullwidth() {
        assert_eq!(to_fullwidth("!?", false), "！？");
        assert_eq!(to_fullwidth("-", false), "ー");
        assert_eq!(to_fullwidth(",.", false), "、。");
        assert_eq!(to_fullwidth("[]", false), "「」");
    }

    #[test]
    fn alphabet_is_kept_halfwidth_unless_requested() {
        // roman input is sent to the engine as halfwidth ASCII
        assert_eq!(to_fullwidth("ka", false), "ka");
        assert_eq!(to_fullwidth("ka", true), "ｋａ");
    }

    #[test]
    fn unmapped_characters_pass_through() {
        assert_eq!(to_fullwidth("あ漢1", false), "あ漢1");
        assert_eq!(to_fullwidth("", false), "");
        assert_eq!(to_halfwidth("あ漢A"), "あ漢A");
        assert_eq!(to_halfwidth(""), "");
    }

    #[test]
    fn halfwidth_reverses_fullwidth_symbols() {
        for half in ["!", "?", "(", ")", "[", "]", "-", ",", "."] {
            let full = to_fullwidth(half, false);
            assert_eq!(to_halfwidth(&full), half, "roundtrip failed for {half}");
        }
    }

    #[test]
    fn fullwidth_ascii_uses_plain_forms_not_kana_punctuation() {
        // F9: alphanumerics and symbols become full-width ASCII, NOT the
        // kana-input punctuation (which would give ー、。・ for -,./)
        assert_eq!(to_fullwidth_ascii("a-b"), "ａ－ｂ");
        assert_eq!(to_fullwidth_ascii("1,2.3/4"), "１，２．３／４");
        assert_eq!(to_fullwidth_ascii("Az9"), "Ａｚ９");
        // space becomes the ideographic space, non-ASCII passes through
        assert_eq!(to_fullwidth_ascii("a b"), "ａ\u{3000}ｂ");
        assert_eq!(to_fullwidth_ascii("あ漢"), "あ漢");
        assert_eq!(to_fullwidth_ascii(""), "");
    }
}
