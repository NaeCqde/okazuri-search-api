//! Kana to Hepburn romaji: the table and the rules that look at the neighbouring kana.
//!
//! This is a port of the Hepburn part of cutlet (`cutlet/mapping.py` and `Cutlet.map_kana`), so that
//! the output matches the Python version this server replaces. The text it gets is hiragana
//! (`utils::kata_to_hira`); see `docs/ALGORITHM.md`.

use crate::utils::kata_to_hira;
use std::collections::HashMap;
use std::sync::LazyLock;

/// The small kana that make a digraph with the kana before them (しゃ, ふぁ, ...).
const SUTEGANA: &str = "ゃゅょぁぃぅぇぉ";
/// The iteration marks. Only the first two can be resolved from the kana before them.
const ODORI: &str = "々〃ゝゞヽゞ";
const UNVOICED: &str = "かきくけこさしすせそたちつてとはひふへほ";
const VOICED: &str = "がぎぐげござじずぜぞだぢづでどばびぶべぼ";

/// The Hepburn table. Where a key is written twice the later one wins, as in the Python dict.
static HEPBURN: LazyLock<HashMap<&'static str, &'static str>> = LazyLock::new(|| {
    [
        // single characters
        ("あ", "a"), ("い", "i"), ("う", "u"), ("え", "e"), ("お", "o"),
        ("か", "ka"), ("き", "ki"), ("く", "ku"), ("け", "ke"), ("こ", "ko"),
        ("が", "ga"), ("ぎ", "gi"), ("ぐ", "gu"), ("げ", "ge"), ("ご", "go"),
        ("さ", "sa"), ("し", "shi"), ("す", "su"), ("せ", "se"), ("そ", "so"),
        ("ざ", "za"), ("じ", "ji"), ("ず", "zu"), ("ぜ", "ze"), ("ぞ", "zo"),
        ("た", "ta"), ("ち", "chi"), ("つ", "tsu"), ("て", "te"), ("と", "to"),
        ("だ", "da"), ("ぢ", "ji"), ("づ", "zu"), ("で", "de"), ("ど", "do"),
        ("な", "na"), ("に", "ni"), ("ぬ", "nu"), ("ね", "ne"), ("の", "no"),
        ("は", "ha"), ("ひ", "hi"), ("ふ", "fu"), ("へ", "he"), ("ほ", "ho"),
        ("ば", "ba"), ("び", "bi"), ("ぶ", "bu"), ("べ", "be"), ("ぼ", "bo"),
        ("ぱ", "pa"), ("ぴ", "pi"), ("ぷ", "pu"), ("ぺ", "pe"), ("ぽ", "po"),
        ("ま", "ma"), ("み", "mi"), ("む", "mu"), ("め", "me"), ("も", "mo"),
        ("や", "ya"), ("ゆ", "yu"), ("よ", "yo"),
        ("ら", "ra"), ("り", "ri"), ("る", "ru"), ("れ", "re"), ("ろ", "ro"),
        ("わ", "wa"), ("ゐ", "yi"), ("ゑ", "ye"), ("を", "o"), ("ん", "n"),
        // obsolete / unusual
        ("ゑ", "we"), ("ゐ", "wi"), ("ゔ", "vu"), ("ヸ", "vi"), ("ヷ", "va"), ("わ\u{3099}", "va"),
        ("ヹ", "ve"), ("ゑ\u{3099}", "ve"), ("ヺ", "vo"), ("を\u{3099}", "vo"),
        // sutegana
        ("ゃ", "ya"), ("ゅ", "yu"), ("ょ", "yo"), ("ぁ", "a"), ("ぃ", "i"), ("ぅ", "u"), ("ぇ", "e"), ("ぉ", "o"),
        // unusual sutegana (Ainu, Taiwanese): included to avoid errors
        ("ゕ", "ka"), ("ㇰ", "k"), ("ゖ", "ke"), ("ヶ", "ga"),
        ("ㇱ", "shi"), ("ㇲ", "su"), ("ㇳ", "to"), ("ㇴ", "nu"), ("ㇵ", "ha"), ("ㇶ", "hi"), ("ㇷ", "fu"),
        ("ㇸ", "he"), ("ㇹ", "ho"), ("ㇺ", "mu"), ("ㇻ", "ra"), ("ㇼ", "ri"), ("ㇽ", "ru"), ("ㇾ", "re"),
        ("ㇿ", "ro"), ("ゎ", "wa"), ("ㇷ\u{309A}", "p"),
        // small kana extension
        ("\u{1B150}", "wi"), ("\u{1B151}", "we"), ("\u{1B152}", "wo"),
        ("\u{1B164}", "wi"), ("\u{1B165}", "we"), ("\u{1B166}", "wo"), ("\u{1B167}", "n"),
        // digraphs
        ("しゃ", "sha"), ("しゅ", "shu"), ("しょ", "sho"),
        ("じゃ", "ja"), ("じゅ", "ju"), ("じょ", "jo"),
        ("ちゃ", "cha"), ("ちゅ", "chu"), ("ちょ", "cho"),
        // cutlet has no rule for these: う + a small vowel would lose the w (うぃ -> i)
        ("うぃ", "wi"), ("うぇ", "we"), ("うぉ", "wo"),
        // symbols
        ("ー", "-"), ("。", "."), ("、", ","), ("？", "?"), ("！", "!"),
        ("「", "\""), ("」", "\""), ("『", "\""), ("』", "\""),
        ("：", ":"), ("（", "("), ("）", ")"), ("《", "("), ("》", ")"),
        ("【", "["), ("】", "]"), ("・", "/"), ("，", ","),
        // a combining (han)dakuten by itself is discarded
        ("\u{309A}", ""), ("\u{3099}", ""),
    ]
    .into_iter()
    .collect()
});

/// The romaji of a key (one kana, or the two of a digraph), if the table has it.
pub fn lookup(key: &str) -> Option<&'static str> {
    HEPBURN.get(key).copied()
}

/// The romaji of one character; a character the table does not have (a symbol, a letter) goes through.
fn lookup_char(c: char) -> String {
    let mut buf = [0u8; 4];
    lookup(c.encode_utf8(&mut buf)).map(str::to_string).unwrap_or_else(|| c.to_string())
}

/// Hiragana (or katakana, converted first) to romaji: `Cutlet.map_kana`.
///
/// The romaji of a kana depends on its neighbours (しゃ, っち, ん before a vowel, ー), so every
/// character is looked at together with the ones around it.
pub fn map_kana(kana: &str) -> String {
    let chars: Vec<char> = kata_to_hira(kana).chars().collect();
    let mut out = String::new();

    for (i, &c) in chars.iter().enumerate() {
        let prev = if i > 0 { Some(chars[i - 1]) } else { None };
        let next = chars.get(i + 1).copied();
        out.push_str(&single_mapping(prev, c, next));
    }

    out
}

/// `Cutlet.get_single_mapping`: the romaji of `kk`, given the kana before and after it.
fn single_mapping(prev: Option<char>, kk: char, next: Option<char>) -> String {
    // the iteration marks
    if ODORI.contains(kk) {
        return match kk {
            // cutlet returns the kana itself here, which leaves kana in the romaji; this writes its romaji
            'ゝ' | 'ヽ' => prev.map(lookup_char).unwrap_or_default(),
            'ゞ' | 'ヾ' => prev
                .and_then(add_dakuten)
                .map(lookup_char)
                .unwrap_or_default(),
            // 々 and 〃 cannot be resolved
            _ => String::new(),
        };
    }

    // digraphs: the kana before makes one with this one (output there), or this one makes one with the next
    if let Some(p) = prev {
        if let Some(v) = lookup(&format!("{p}{kk}")) {
            return v.to_string();
        }
    }
    if let Some(n) = next {
        if lookup(&format!("{kk}{n}")).is_some() {
            return String::new();
        }
    }

    if let Some(n) = next {
        if SUTEGANA.contains(n) {
            if kk == 'っ' {
                return String::new(); // never valid
            }
            let head = lookup_char(kk);
            let mut head_chars = head.chars();
            head_chars.next_back();
            return format!("{}{}", head_chars.as_str(), lookup_char(n));
        }
    }
    if SUTEGANA.contains(kk) {
        return String::new();
    }

    if kk == 'ー' {
        // the prolonged sound mark repeats the vowel before it
        return match prev.and_then(|p| lookup(&p.to_string())) {
            Some(romaji) => romaji.chars().last().map(|c| c.to_string()).unwrap_or_default(),
            None => "-".to_string(),
        };
    }

    if kk == 'っ' {
        return match next {
            Some('ち') => "t".to_string(), // tch: まっちゃ -> matcha
            Some(n) if "あいうえおっ".contains(n) => "-".to_string(),
            Some(n) => lookup_char(n).chars().next().map(|c| c.to_string()).unwrap_or_default(),
            // 乗っ|た is two words: the word level picks this up
            None => "っ".to_string(),
        };
    }

    if kk == 'ん' {
        return match next {
            Some(n) if "あいうえおやゆよ".contains(n) => "n'".to_string(),
            _ => "n".to_string(),
        };
    }

    lookup_char(kk)
}

/// The voiced kana of an unvoiced one (か -> が), for the iteration mark ゞ.
fn add_dakuten(kana: char) -> Option<char> {
    let at = UNVOICED.chars().position(|c| c == kana)?;
    VOICED.chars().nth(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_plain_kana() {
        assert_eq!(map_kana("しごと"), "shigoto");
        assert_eq!(map_kana("カンリ"), "kanri");
        assert_eq!(map_kana("ふぁみこん"), "famikon");
    }

    #[test]
    fn maps_digraphs_and_small_kana() {
        assert_eq!(map_kana("しょうがっこう"), "shougakkou");
        assert_eq!(map_kana("ちゃ"), "cha");
        assert_eq!(map_kana("てぃ"), "ti");
        assert_eq!(map_kana("ゔぁ"), "va");
        assert_eq!(map_kana("うぃんたー"), "wintaa");
    }

    #[test]
    fn doubles_the_consonant_after_a_small_tsu() {
        assert_eq!(map_kana("きっぷ"), "kippu");
        // tch: the consonant is t, not c
        assert_eq!(map_kana("まっちゃ"), "matcha");
        // before a vowel it can only be a hyphen
        assert_eq!(map_kana("っあ"), "-a");
    }

    #[test]
    fn separates_n_before_a_vowel() {
        assert_eq!(map_kana("しんあい"), "shin'ai");
        assert_eq!(map_kana("ほんや"), "hon'ya");
        assert_eq!(map_kana("ほんか"), "honka");
        assert_eq!(map_kana("さん"), "san");
    }

    #[test]
    fn repeats_the_vowel_for_the_prolonged_sound_mark() {
        assert_eq!(map_kana("りかばりー"), "rikabarii");
        assert_eq!(map_kana("らーめん"), "raamen");
        // nothing before it: a hyphen
        assert_eq!(map_kana("ー"), "-");
    }

    #[test]
    fn resolves_the_iteration_marks() {
        assert_eq!(map_kana("こゝろ"), "kokoro");
        assert_eq!(map_kana("はゞ"), "haba");
        assert_eq!(map_kana("々"), "");
    }

    #[test]
    fn passes_unknown_characters_through() {
        assert_eq!(map_kana("ふ-ふ"), "fu-fu");
        assert_eq!(map_kana("a"), "a");
    }
}
