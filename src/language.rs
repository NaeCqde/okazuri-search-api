//! The language of a work, read from its title: the flag a site puts first (🇨🇳), the marks of a translation (`[中国翻訳]`, `[汉化]`) and the
//! brackets that only name languages (`[EN]`, `[English]`, `[CN,EN,JP,KR]`). It is a guess from the title, so it is kept apart from
//! the title (`works.language`) and made again by `import` and `reromanize`.
//!
//! A title that is written in the script of a language is that language too, whatever it says: Cyrillic is Russian, Hangul Korean, Thai
//! Thai, Arabic Arabic, the letters that only Vietnamese has Vietnamese, and a character that only Chinese writes (a simplified form, or a
//! traditional form that Japanese does not use) Chinese, since a work with such a title was translated for them. Japanese and English are not
//! read from the script: the kana and the Latin letters are in the titles of every language.
//!
//! The answer is the two-letter codes of the languages the title names, joined with `,` in the order it names them (`zh`, `en,ko`; a pack
//! `[CN,EN,JP,KR]` is `zh,en,ja,ko`), or None when the title says nothing (most titles are Japanese and do not say so). The flag of Japan that
//! a site puts first does not make a title Japanese when the title names another language (`🇯🇵 【EN】Kosuzu-Chan` is `en`): Japanese
//! stays only when the title has a Japanese tag of its own (`[Japanese, Chinese]`, `[jap / eng]`) or names no other language.

use regex::Regex;
use std::sync::OnceLock;

/// The words that name a language in the brackets of a title (upper case) and the code of that language.
fn code_of_word(word: &str) -> Option<&'static str> {
    Some(match word {
        "EN" | "ENG" | "ENGLISH" => "en",
        "JP" | "JA" | "JPN" | "JAP" | "JAPANESE" => "ja",
        "CN" | "ZH" | "TW" | "CHINESE" => "zh",
        "KR" | "KO" | "KOREAN" => "ko",
        "ES" | "SPA" | "SPANISH" => "es",
        "FR" | "FRENCH" => "fr",
        "DE" | "GERMAN" => "de",
        "RU" | "RUS" | "RUSSIAN" => "ru",
        "IT" | "ITALIAN" => "it",
        "TH" | "THAI" => "th",
        "PT" | "PORTUGUESE" => "pt",
        "ID" | "INDONESIAN" => "id",
        "PL" | "POLISH" => "pl",
        "TR" | "TURKISH" => "tr",
        "VN" | "VI" | "VIE" | "VIETNAMESE" => "vi",
        "ARABIC" => "ar",
        _ => return None,
    })
}

/// The language of the country that a flag (two regional indicators) stands for.
fn code_of_flag(first: char, second: char) -> Option<&'static str> {
    let letter = |c: char| char::from_u32(c as u32 - 0x1F1E6 + 'A' as u32).unwrap_or('?');
    let country: String = [letter(first), letter(second)].iter().collect();

    Some(match country.as_str() {
        "US" | "GB" => "en",
        "JP" => "ja",
        "CN" | "TW" => "zh",
        "KR" => "ko",
        "ES" => "es",
        "FR" => "fr",
        "DE" => "de",
        "RU" => "ru",
        "IT" => "it",
        "TH" => "th",
        "PT" => "pt",
        "ID" => "id",
        "PL" => "pl",
        "TR" => "tr",
        "VN" => "vi",
        "SA" => "ar",
        _ => return None,
    })
}

fn is_indicator(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

/// The title without its flags (two regional indicators, 🇯🇵 🇨🇳 ...), wherever they are: the language they name is in
/// `works.language` (`detect`), so the title does not need them. The white space they leave is tidied up: one space where a
/// flag was between two words, none at the start or the end.
pub fn strip_flags(title: &str) -> String {
    let chars: Vec<char> = title.chars().collect();
    let mut out = String::with_capacity(title.len());
    let mut at = 0;

    while at < chars.len() {
        if at + 1 < chars.len() && is_indicator(chars[at]) && is_indicator(chars[at + 1]) {
            at += 2;
            // the white space after the flag goes when nothing (or white space) is before it
            while at < chars.len() && chars[at].is_whitespace() && (out.is_empty() || out.ends_with(char::is_whitespace)) {
                at += 1;
            }
        } else {
            out.push(chars[at]);
            at += 1;
        }
    }

    out.trim().to_string()
}

struct Patterns {
    chinese: Regex,
    chinese_label: Regex,
    brackets: Regex,
}

fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();

    PATTERNS.get_or_init(|| Patterns {
        // the marks of a translation into Chinese
        chinese: Regex::new("中国翻訳|中文|汉化|漢化|简体|簡體|繁體|繁体|嵌字").unwrap(),
        chinese_label: Regex::new("^(中国翻訳|中国語|中文|简体中文|簡體中文|繁體中文|繁体中文|汉化|漢化)$").unwrap(),
        // [EN] [JP] [English] [CN,EN,JP,KR] [jap / eng] (and with round brackets and 【 】)
        brackets: Regex::new(r"[\[(【]([^\[\]()【】]{1,40})[\])】]").unwrap(),
    })
}

/// Characters of Chinese that Japanese does not write: the simplified forms, and the traditional forms that are not the kanji of Japanese.
const CHINESE_ONLY: &str = "们这个说吗么还对见书车长门问间马东开关电时经为产业华从众传优伤价兴册军凉动务劳势协单卫历县变叹听启员响团园图坏场坚处备头夸夺奋妇决净发\
    這們說嗎麼點沒對關經產從眾傳價淨勞歷縣變聽啟團圖壞處";

/// The languages whose script the text is written in (see the top of the file), in the order of their first letter.
fn by_script(text: &str) -> Vec<&'static str> {
    let (mut cyrillic, mut hangul, mut thai, mut arabic) = (0, 0, 0, 0);
    let mut found: Vec<(usize, &'static str)> = Vec::new();

    for (at, c) in text.char_indices() {
        let code = match c {
            '\u{0400}'..='\u{04FF}' => {
                cyrillic += 1;
                (cyrillic == 2).then_some("ru")
            }
            '\u{AC00}'..='\u{D7AF}' => {
                hangul += 1;
                (hangul == 2).then_some("ko")
            }
            '\u{0E00}'..='\u{0E7F}' => {
                thai += 1;
                (thai == 2).then_some("th")
            }
            '\u{0600}'..='\u{06FF}' => {
                arabic += 1;
                (arabic == 2).then_some("ar")
            }
            _ if "ăơưđạảấầẩẫậắằẳẵặẹẻẽếềểễệịỉọỏốồổỗộớờởỡợụủứừửữựỳỵỷỹ".contains(c.to_ascii_lowercase()) || "ĂƠƯĐẠẢẤẦẨẪẬẮẰẲẴẶẸẺẼẾỀỂỄỆỊỈỌỎỐỒỔỖỘỚỜỞỠỢỤỦỨỪỬỮỰỲỴỶỸ".contains(c) => Some("vi"),
            _ if CHINESE_ONLY.contains(c) => Some("zh"),
            _ => None,
        };

        if let Some(code) = code {
            if !found.iter().any(|(_, known)| *known == code) {
                found.push((at, code));
            }
        }
    }

    found.sort_by_key(|(at, _)| *at);
    found.into_iter().map(|(_, code)| code).collect()
}

fn add(found: &mut Vec<&'static str>, code: &'static str) {
    if !found.contains(&code) {
        found.push(code);
    }
}

/// The language codes of a title, joined with `,`; None when it names none.
pub fn detect(title: &str) -> Option<String> {
    let found = detect_all(title);

    (!found.is_empty()).then(|| found.join(","))
}

/// Every language the title names, in the order it names them.
pub fn detect_all(title: &str) -> Vec<&'static str> {
    let patterns = patterns();
    let mut found: Vec<&'static str> = Vec::new();
    // the title says Japanese itself (in a bracket), not only the flag a site put first
    let mut japanese_tag = false;

    // the flags the title has (a site puts one first, RokuHentai; one may be anywhere), and the title without them
    let chars: Vec<char> = title.chars().collect();
    let mut at = 0;
    while at + 1 < chars.len() {
        if is_indicator(chars[at]) && is_indicator(chars[at + 1]) {
            if let Some(code) = code_of_flag(chars[at], chars[at + 1]) {
                add(&mut found, code);
            }
            at += 2;
        } else {
            at += 1;
        }
    }
    let without_flags = strip_flags(title);
    let text = without_flags.as_str();

    if patterns.chinese.is_match(text) {
        add(&mut found, "zh");
    }

    for group in patterns.brackets.captures_iter(text) {
        let body = group[1].trim();

        if patterns.chinese_label.is_match(body) {
            add(&mut found, "zh");
            continue;
        }

        // a bracket that has any other word in it is a name or a title ([It Girl]), not a language
        let words: Vec<String> = body.to_uppercase().split(|c: char| c.is_whitespace() || ",/&+-".contains(c)).filter(|w| !w.is_empty()).map(str::to_string).collect();
        let named = !words.is_empty() && words.iter().all(|word| code_of_word(word).is_some() || word == "SUB" || word == "SUBS");

        if named {
            for word in &words {
                if let Some(code) = code_of_word(word) {
                    japanese_tag |= code == "ja";
                    add(&mut found, code);
                }
            }
        }
    }

    // a title in the script of a language is that language too
    for code in by_script(text) {
        add(&mut found, code);
    }

    // the flag of Japan next to another language is the site's flag, not a tag of the title
    if found.len() > 1 && !japanese_tag {
        found.retain(|code| *code != "ja");
    }

    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is(title: &str, expected: &str) {
        assert_eq!(detect(title).as_deref(), Some(expected), "{title}");
    }

    #[test]
    fn marks_of_chinese() {
        is("[A] B [Chinese] [Digital]", "zh");
        is("[A] B [RanaMycin个人汉化] [DL版]", "zh");
        is("[A] B [中国翻訳] [DL版]", "zh");
        is("[A] B [無邪気漢化組]", "zh");
    }

    #[test]
    fn codes_and_names_in_brackets() {
        is("x [EN]", "en");
        is("x [en]", "en");
        is("x (JP)", "ja");
        is("x 【KR】", "ko");
        is("x [English - EN] [Digital]", "en");
        is("x [French][Italian]", "fr,it");
        is("x [Japanese]", "ja");
    }

    #[test]
    fn a_title_that_names_several_languages_has_them_all_in_the_order_it_names_them_without_japanese() {
        is("x [CN,EN,JP,KR]", "zh,en,ja,ko");
        is("x [jap / eng / rus]", "ja,en,ru");
        is("x [jap / rus / eng]", "ja,ru,en");
        is("x [English, Japanese]", "en,ja");
        is("x [Japanese, Chinese]", "ja,zh");
        is("x [ENG VIE RUS subs]", "en,vi,ru");
        is("[A] B [English] [無邪気漢化組]", "zh,en");
    }

    #[test]
    fn a_flag_that_the_site_put_first_counts_once() {
        is("🇨🇳 [Hamao] Fallin' [Chinese]", "zh");
        is("🇯🇵 A [EN]", "en");
        is("🇯🇵 A [JP]", "ja");
        is("🇯🇵 A [JP] [EN]", "ja,en");
        is("🇯🇵 【EN】Kosuzu-Chan", "en");
        is("🇯🇵 [KR] U", "ko");
        is("🇯🇵 A", "ja");
        is("🇺🇸🇰🇷 A", "en,ko");
        is("🇹🇼 A", "zh");
    }

    #[test]
    fn a_flag_anywhere_names_its_language_and_is_taken_out_of_the_title() {
        is("A [Hamao] 🇨🇳", "zh");
        is("A 🇰🇷 B", "ko");
        assert_eq!(strip_flags("🇯🇵 COMIC Shitsurakuten 2026-11"), "COMIC Shitsurakuten 2026-11");
        assert_eq!(strip_flags("🇺🇸🇰🇷 A"), "A");
        assert_eq!(strip_flags("A 🇰🇷 B"), "A B");
        assert_eq!(strip_flags("A B 🇰🇷"), "A B");
        assert_eq!(strip_flags("A🇰🇷B"), "AB");
        assert_eq!(strip_flags("No flag  here"), "No flag  here");
        // one indicator alone is not a flag
        assert_eq!(strip_flags("A \u{1F1E8} B"), "A \u{1F1E8} B");
    }

    #[test]
    fn a_title_in_the_script_of_a_language_is_that_language() {
        is("Рассвет над городом", "ru");
        is("[Автор] Название [Digital]", "ru");
        is("제목이 한국어", "ko");
        is("ชื่อเรื่องภาษาไทย", "th");
        is("عنوان عربي", "ar");
        is("Tình yêu của chúng ta", "vi");
        is("[A] 这个故事 [DL版]", "zh");
        is("這們故事", "zh");
        is("[Автор] 这个 [English]", "en,ru,zh");
        // the flag of Japan stays out when the script names another language
        is("🇯🇵 Название", "ru");
        // Japanese and English are not read from the script
        assert_eq!(detect("[A] 普通の日本語タイトル"), None);
        assert_eq!(detect("COMIC 快楽天 2026年1月号"), None);
        assert_eq!(detect("An English title"), None);
        // one letter (a decoration such as Я) does not make a title Russian
        assert_eq!(detect("ЯBIT"), None);
    }

    #[test]
    fn a_title_that_says_nothing_has_no_language() {
        assert_eq!(detect("[A] 普通の日本語タイトル"), None);
        for title in ["x [HD]", "x [NR]", "x [UC]", "x [AI]", "x [VR]", "x [KS]", "x [It Girl]", "x [ID Card]", "x (mr)", "x EN", "x [English Edition Deluxe]"] {
            assert_eq!(detect(title), None, "{title}");
        }
    }

    /// Lists the works of a database whose title names more than one language: `OKAZURI_DB=search.db cargo test list_packs -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn list_packs() {
        let Ok(path) = std::env::var("OKAZURI_DB") else { return };
        let db = rusqlite_free_read(&path);

        for (website, path, title) in db {
            let all = detect_all(&title);

            if all.len() > 1 {
                println!("{}\t{website}\t{path}\t{title}", all.join(","));
            }
        }
    }

    /// The (website, path, title) rows of a database, read with the `sqlite3` program.
    fn rusqlite_free_read(path: &str) -> Vec<(String, String, String)> {
        let out = std::process::Command::new("sqlite3").args(["-separator", "\t", path, "select website, path, replace(title, char(10), ' ') from works"]).output().expect("sqlite3");

        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, '\t');
                Some((parts.next()?.to_string(), parts.next()?.to_string(), parts.next()?.to_string()))
            })
            .collect()
    }
}
