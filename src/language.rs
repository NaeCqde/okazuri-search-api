//! The language of a work, read from its title: the flag a site puts first (🇨🇳), the marks of a translation (`[中国翻訳]`, `[汉化]`) and the
//! brackets that only name languages (`[EN]`, `[English]`, `[CN,EN,JP,KR]`). It is a guess from the title, so it is kept apart from
//! the title (`works.language`) and made again by `import` and `reromanize`.
//!
//! The answer is the two-letter codes of the languages the title names, joined with `,` in the order it names them (`zh`, `en,ko`; a pack
//! `[CN,EN,JP,KR]` is `zh,en,ja,ko`), or None when the title says nothing (most titles are Japanese and do not say so).

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
    let mut text = title.trim();

    // the flags that the site put first
    loop {
        let rest = text.trim_start();
        let mut chars = rest.chars();

        match (chars.next(), chars.next()) {
            (Some(first), Some(second)) if is_indicator(first) && is_indicator(second) => {
                if let Some(code) = code_of_flag(first, second) {
                    add(&mut found, code);
                }
                text = &rest[first.len_utf8() + second.len_utf8()..];
            }
            _ => break,
        }
    }

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
                    add(&mut found, code);
                }
            }
        }
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
    fn a_title_that_names_several_languages_has_them_all_in_the_order_it_names_them() {
        is("x [CN,EN,JP,KR]", "zh,en,ja,ko");
        is("x [jap / eng / rus]", "ja,en,ru");
        is("x [English, Japanese]", "en,ja");
        is("x [ENG VIE RUS subs]", "en,vi,ru");
        is("[A] B [English] [無邪気漢化組]", "zh,en");
    }

    #[test]
    fn a_flag_that_the_site_put_first_counts_once() {
        is("🇨🇳 [Hamao] Fallin' [Chinese]", "zh");
        is("🇯🇵 A [EN]", "ja,en");
        is("🇯🇵 A", "ja");
        is("🇺🇸🇰🇷 A", "en,ko");
        is("🇹🇼 A", "zh");
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
