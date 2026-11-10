//! Small helpers shared by the modules.

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Seconds since 1970-01-01 UTC.
pub fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The distance between a hiragana and the katakana of the same sound.
const KANA_OFFSET: u32 = 0x60;

/// Katakana to hiragana (`jaconv.kata2hira`): the letters U+30A1..=U+30F6 and the iteration marks
/// U+30FD / U+30FE move by 0x60. Everything else, the prolonged sound mark included, is left as it is.
pub fn kata_to_hira(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{30A1}'..='\u{30F6}' | '\u{30FD}' | '\u{30FE}' => shift(c, false),
            _ => c,
        })
        .collect()
}

/// Hiragana to katakana (`jaconv.hira2kata`), the other way round.
pub fn hira_to_kata(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '\u{3041}'..='\u{3096}' | '\u{309D}' | '\u{309E}' => shift(c, true),
            _ => c,
        })
        .collect()
}

fn shift(c: char, up: bool) -> char {
    let code = if up { c as u32 + KANA_OFFSET } else { c as u32 - KANA_OFFSET };
    char::from_u32(code).unwrap_or(c)
}

pub fn is_hiragana_char(c: char) -> bool {
    ('\u{3041}'..='\u{3096}').contains(&c)
}

pub fn is_katakana_char(c: char) -> bool {
    ('\u{30A1}'..='\u{30FA}').contains(&c)
}

/// Only hiragana (`[ぁ-ゖ]+`).
pub fn is_hiragana(text: &str) -> bool {
    !text.is_empty() && text.chars().all(is_hiragana_char)
}

/// Hiragana, katakana, the prolonged sound mark and the middle dot between katakana words
/// (`[ぁ-ゖァ-ヺー・]+`). MeCab puts all of them in the kana character types.
pub fn is_kana(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| is_hiragana_char(c) || is_katakana_char(c) || c == 'ー' || c == '・')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_between_the_kana() {
        assert_eq!(kata_to_hira("チートアイテム"), "ちーとあいてむ");
        assert_eq!(hira_to_kata("ふらすとれーしょん"), "フラストレーション");
        assert_eq!(kata_to_hira("ヴ"), "ゔ");
        // the mark and other scripts are not touched
        assert_eq!(kata_to_hira("ABCー漢字"), "ABCー漢字");
    }

    #[test]
    fn tells_the_scripts_apart() {
        assert!(is_hiragana("ふぁみこん"));
        assert!(!is_hiragana("ふぁミコン"));
        assert!(!is_hiragana("ふー"));
        assert!(is_kana("ふーミコン"));
        assert!(!is_kana("ふ漢"));
        assert!(!is_kana(""));
    }
}
