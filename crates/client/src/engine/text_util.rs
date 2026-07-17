// thanks to https://github.com/ensan-hcl/AzooKeyKanaKanjiConverter/blob/develop/Sources/KanaKanjiConverterModule/Roman2Kana.swift
use std::collections::HashMap;
use std::sync::LazyLock;

use super::full_width::to_halfwidth;

#[allow(dead_code)]
static KANA_MAP: LazyLock<HashMap<&'static str, (&'static str, &'static str)>> =
    LazyLock::new(|| {
        HashMap::from([
            ("あ", ("ア", "ｱ")),
            ("ぁ", ("ァ", "ｧ")),
            ("い", ("イ", "ｲ")),
            ("ぃ", ("ィ", "ｨ")),
            ("う", ("ウ", "ｳ")),
            ("ゔ", ("ヴ", "ｳﾞ")),
            ("ぅ", ("ゥ", "ｩ")),
            ("え", ("エ", "ｴ")),
            ("ぇ", ("ェ", "ｪ")),
            ("お", ("オ", "ｵ")),
            ("ぉ", ("ォ", "ｫ")),
            ("か", ("カ", "ｶ")),
            ("が", ("ガ", "ｶﾞ")),
            ("か゚", ("カ゚", "ｶﾟ")),
            ("ゕ", ("ヵ", "ｶ")),
            ("き", ("キ", "ｷ")),
            ("ぎ", ("ギ", "ｷﾞ")),
            ("き゚", ("キ゚", "ｷﾟ")),
            ("く", ("ク", "ｸ")),
            ("ぐ", ("グ", "ｸﾞ")),
            ("く゚", ("ク゚", "ｸﾟ")),
            ("け", ("ケ", "ｹ")),
            ("げ", ("ゲ", "ｹﾞ")),
            ("け゚", ("ケ゚", "ｹﾟ")),
            ("ゖ", ("ヶ", "ｹ")),
            ("こ", ("コ", "ｺ")),
            ("ご", ("ゴ", "ｺﾞ")),
            ("こ゚", ("コ゚", "ｺﾟ")),
            ("さ", ("サ", "ｻ")),
            ("ざ", ("ザ", "ｻﾞ")),
            ("さ゚", ("サ゚", "ｻﾟ")),
            ("し", ("シ", "ｼ")),
            ("じ", ("ジ", "ｼﾞ")),
            ("し゚", ("シ゚", "ｼﾟ")),
            ("す", ("ス", "ｽ")),
            ("ず", ("ズ", "ｽﾞ")),
            ("す゚", ("ス゚", "ｽﾟ")),
            ("せ", ("セ", "ｾ")),
            ("ぜ", ("ゼ", "ｾﾞ")),
            ("せ゚", ("セ゚", "ｾﾟ")),
            ("そ", ("ソ", "ｿ")),
            ("ぞ", ("ゾ", "ｿﾞ")),
            ("そ゚", ("ソ゚", "ｿﾟ")),
            ("た", ("タ", "ﾀ")),
            ("だ", ("ダ", "ﾀﾞ")),
            ("た゚", ("タ゚", "ﾀﾟ")),
            ("ち", ("チ", "ﾁ")),
            ("ぢ", ("ヂ", "ﾁﾞ")),
            ("ち゚", ("チ゚", "ﾁﾟ")),
            ("つ", ("ツ", "ﾂ")),
            ("づ", ("ヅ", "ﾂﾞ")),
            ("つ゚", ("ツ゚", "ﾂﾟ")),
            ("っ", ("ッ", "ｯ")),
            ("て", ("テ", "ﾃ")),
            ("で", ("デ", "ﾃﾞ")),
            ("て゚", ("テ゚", "ﾃﾟ")),
            ("と", ("ト", "ﾄ")),
            ("ど", ("ド", "ﾄﾞ")),
            ("と゚", ("ト゚", "ﾄﾟ")),
            ("な", ("ナ", "ﾅ")),
            ("に", ("ニ", "ﾆ")),
            ("ぬ", ("ヌ", "ﾇ")),
            ("ね", ("ネ", "ﾈ")),
            ("の", ("ノ", "ﾉ")),
            ("は", ("ハ", "ﾊ")),
            ("ば", ("バ", "ﾊﾞ")),
            ("ぱ", ("パ", "ﾊﾟ")),
            ("ひ", ("ヒ", "ﾋ")),
            ("び", ("ビ", "ﾋﾞ")),
            ("ぴ", ("ピ", "ﾋﾟ")),
            ("ふ", ("フ", "ﾌ")),
            ("ぶ", ("ブ", "ﾌﾞ")),
            ("ぷ", ("プ", "ﾌﾟ")),
            ("へ", ("ヘ", "ﾍ")),
            ("べ", ("ベ", "ﾍﾞ")),
            ("ぺ", ("ペ", "ﾍﾟ")),
            ("ほ", ("ホ", "ﾎ")),
            ("ぼ", ("ボ", "ﾎﾞ")),
            ("ぽ", ("ポ", "ﾎﾟ")),
            ("ま", ("マ", "ﾏ")),
            ("み", ("ミ", "ﾐ")),
            ("む", ("ム", "ﾑ")),
            ("め", ("メ", "ﾒ")),
            ("も", ("モ", "ﾓ")),
            ("や", ("ヤ", "ﾔ")),
            ("ゃ", ("ャ", "ｬ")),
            ("ゆ", ("ユ", "ﾕ")),
            ("ゅ", ("ュ", "ｭ")),
            ("よ", ("ヨ", "ﾖ")),
            ("ょ", ("ョ", "ｮ")),
            ("ら", ("ラ", "ﾗ")),
            ("ら゚", ("ラ゚", "ﾗﾟ")),
            ("り", ("リ", "ﾘ")),
            ("り゚", ("リ゚", "ﾘﾟ")),
            ("る", ("ル", "ﾙ")),
            ("る゚", ("ル゚", "ﾙﾟ")),
            ("れ", ("レ", "ﾚ")),
            ("れ゚", ("レ゚", "ﾚﾟ")),
            ("ろ", ("ロ", "ﾛ")),
            ("ろ゚", ("ロ゚", "ﾛﾟ")),
            ("わ", ("ワ", "ﾜ")),
            ("ゎ", ("ヮ", "ﾜ")),
            ("ゐ", ("ヰ", "ｲ")),
            ("ゑ", ("ヱ", "ｴ")),
            ("を", ("ヲ", "ｦ")),
            ("ん", ("ン", "ﾝ")),
        ])
    });

pub fn to_katakana(s: &str) -> String {
    let mut result = String::new();

    for c in s.chars() {
        if let Some(&(katakana, _)) = KANA_MAP.get(&c.to_string().as_str()) {
            result.push_str(katakana);
        } else {
            result.push(c);
        }
    }

    result
}

pub fn to_half_katakana(s: &str) -> String {
    let mut result = String::new();

    for c in s.chars() {
        // to_halfwidth maps one char to one char today, but guard against an
        // empty mapping rather than panicking inside a COM callback
        let c = to_halfwidth(&c.to_string()).chars().next().unwrap_or(c);

        if let Some(&(_, hankaku_katakana)) = KANA_MAP.get(&c.to_string().as_str()) {
            result.push_str(hankaku_katakana);
        } else {
            result.push(c);
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hiragana_becomes_katakana() {
        assert_eq!(to_katakana("かな"), "カナ");
        assert_eq!(to_katakana("がぎゃんゔ"), "ガギャンヴ");
    }

    #[test]
    fn katakana_conversion_passes_through_unmapped_chars() {
        assert_eq!(to_katakana("カナ漢a1"), "カナ漢a1");
        assert_eq!(to_katakana(""), "");
    }

    #[test]
    fn hiragana_becomes_half_katakana() {
        assert_eq!(to_half_katakana("かな"), "ｶﾅ");
        // voiced/semi-voiced marks become separate halfwidth codepoints
        assert_eq!(to_half_katakana("がぱ"), "ｶﾞﾊﾟ");
    }

    #[test]
    fn half_katakana_converts_fullwidth_symbols_too() {
        // characterization: the long-vowel mark is halfwidth-ized to "-"
        // (not the halfwidth-kana "ｰ") by the current implementation
        assert_eq!(to_half_katakana("らーめん"), "ﾗ-ﾒﾝ");
        assert_eq!(to_half_katakana(""), "");
    }
}
