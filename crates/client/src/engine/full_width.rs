// https://www.unicode.org/charts/nameslist/n_FF00.html
// extracted with scripts/extract_fullwidth.py

use std::{collections::HashMap, sync::LazyLock};

// azooKey never converts ASCII letters or digits to full width, so 0-9, A-Z
// and a-z are intentionally omitted from this map (they map to themselves).
// F9-style full-width ASCII is a separate transform — see to_fullwidth_ascii.
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

/// [`HALF_FULL_AZOOKEY`] the other way round. Built once instead of scanning
/// the 32-entry map per character, which is what `to_halfwidth` used to do —
/// it runs over the whole reading on every F10.
///
/// The map is injective (no two half-width symbols produce the same
/// full-width one), so inverting it loses nothing.
static FULL_HALF_AZOOKEY: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| HALF_FULL_AZOOKEY.iter().map(|(&k, &v)| (v, k)).collect());

pub fn to_halfwidth(s: &str) -> String {
    s.chars()
        .map(|c| match FULL_HALF_AZOOKEY.get(c.to_string().as_str()) {
            Some(&half) => half.to_string(),
            None => c.to_string(),
        })
        .collect()
}

/// Kana-input keystroke mapping: the symbols azooKey types as their kana
/// punctuation, everything else (ASCII letters and digits included) unchanged.
pub fn to_fullwidth(s: &str) -> String {
    s.chars()
        .map(|c| {
            let key = c.to_string();

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
        assert_eq!(to_fullwidth("!?"), "！？");
        assert_eq!(to_fullwidth("-"), "ー");
        assert_eq!(to_fullwidth(",."), "、。");
        assert_eq!(to_fullwidth("[]"), "「」");
    }

    #[test]
    fn alphabet_is_kept_halfwidth() {
        // roman input is sent to the engine as halfwidth ASCII
        assert_eq!(to_fullwidth("ka"), "ka");
    }

    #[test]
    fn unmapped_characters_pass_through() {
        assert_eq!(to_fullwidth("あ漢1"), "あ漢1");
        assert_eq!(to_fullwidth(""), "");
        assert_eq!(to_halfwidth("あ漢A"), "あ漢A");
        assert_eq!(to_halfwidth(""), "");
    }

    #[test]
    fn halfwidth_reverses_fullwidth_symbols() {
        for half in ["!", "?", "(", ")", "[", "]", "-", ",", "."] {
            let full = to_fullwidth(half);
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
