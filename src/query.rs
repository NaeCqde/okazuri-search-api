//! The text that is searched for: how a query is tidied up, how the name of a magazine issue becomes a
//! query (the same way `okazuri_bot` does it, so that the results a prefetch saves are the ones a search
//! of the bot asks for), and how a query is matched against the titles that are saved.
//!
//! No word is added to the query. The sites have a search of their own and are all about this kind
//! of work; `hentai` is only for the web search engines.

use crate::utils::Result;
use unicode_normalization::UnicodeNormalization;

/// Trims the query and folds runs of white space into one space. `×` stays (DLsite and FANZA want it as it is).
pub fn tidy(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `tidy`, and a multiplication sign is written as `x` (most sites do not agree on `×`, and `COMIC E×E` is `COMIC ExE`).
pub fn normalize(text: &str) -> String {
    tidy(&text.replace('×', "x"))
}

/// Writes the `x` between two capital letters as `×` (`COMIC ExE 74` is `COMIC E×E 74`). The bot and the magazines
/// of D1 write the sign as `x`, and DLsite and FANZA do not find `ExE`. A word like `Sexy` has no capital next to its `x`.
pub fn restore_times(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();

    chars
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            let capital_at = |at: Option<usize>| at.and_then(|at| chars.get(at)).is_some_and(|c| c.is_ascii_uppercase());

            if c == 'x' && capital_at(i.checked_sub(1)) && capital_at(Some(i + 1)) {
                '×'
            } else {
                c
            }
        })
        .collect()
}

pub fn has_japanese(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(c,
            '\u{3040}'..='\u{309F}' // hiragana
            | '\u{30A0}'..='\u{30FF}' // katakana
            | '\u{4E00}'..='\u{9FFF}' // Han
            | '\u{3400}'..='\u{4DBF}')
    })
}

/// `COMIC快楽天 2026年07月号` -> `COMIC快楽天 2026 07`: the year and month marks are only a way of
/// writing the numbers (`okazuri_bot`: `name.replaceAll(/年|月号?/g, " ")`).
pub fn issue_query(name: &str) -> String {
    let mut text = String::with_capacity(name.len());
    let mut chars = name.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '年' => text.push(' '),
            '月' => {
                if chars.peek() == Some(&'号') {
                    chars.next();
                }
                text.push(' ');
            }
            c => text.push(c),
        }
    }

    normalize(&text)
}

/// The query for the pronunciation of the text: the romaji of the Japanese in it. Text without Japanese
/// stays as it is. A `×` stays too; `normalize` is what writes it as `x`.
pub fn romaji(text: &str, romanizer: &crate::romanizer::Romanizer) -> Result<String> {
    if !has_japanese(text) {
        return Ok(text.to_string());
    }

    Ok(tidy(&romanizer.romanize(text, crate::romanizer::Loanwords::Romaji)?))
}

// ---------------------------------------------------------------------------------------------
// Matching a query against saved titles
// ---------------------------------------------------------------------------------------------

/// The text a title (or a word of the query) is compared by: width and compatibility forms folded
/// (NFKC: `ＣＯＭＩＣ` is `COMIC`), lower case, `×` as `x`, and no white space at all (sites put a space
/// into `COMIC 快楽天` and not into `COMIC快楽天`).
pub fn title_key(text: &str) -> String {
    text.replace('×', "x")
        .nfkc()
        .flat_map(char::to_lowercase)
        .filter(|c| !c.is_whitespace())
        .collect()
}

/// A title without its parts in parentheses, and those parts: `work (COMIC kairakuten 2026 11)` is `work` and
/// `COMIC kairakuten 2026 11`. Full width parentheses count too. A part inside a part belongs to the outer one; a
/// parenthesis that is never closed holds the rest of the title.
fn split_parentheses(title: &str) -> (String, Vec<String>) {
    let mut outside = String::new();
    let mut groups: Vec<String> = Vec::new();
    let mut depth = 0usize;

    for c in title.nfkc() {
        match c {
            '(' => {
                if depth == 0 {
                    groups.push(String::new());
                    outside.push(' ');
                } else if let Some(group) = groups.last_mut() {
                    group.push(c);
                }
                depth += 1;
            }
            ')' if depth > 0 => {
                depth -= 1;
                if depth > 0 {
                    if let Some(group) = groups.last_mut() {
                        group.push(c);
                    }
                }
            }
            _ if depth > 0 => {
                if let Some(group) = groups.last_mut() {
                    group.push(c);
                }
            }
            _ => outside.push(c),
        }
    }

    (outside, groups)
}

/// The runs of digits of a title, without the zeros in front (`2026年07月号` has 2026 and 7).
fn numbers(title: &str) -> Vec<String> {
    let folded: String = title.nfkc().collect();

    folded
        .split(|c: char| !c.is_ascii_digit())
        .filter(|run| !run.is_empty())
        .map(strip_zeros)
        .collect()
}

fn strip_zeros(digits: &str) -> String {
    let stripped = digits.trim_start_matches('0');

    if stripped.is_empty() { "0".to_string() } else { stripped.to_string() }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Term {
    key: String,
    /// A word that is a number alone (`07`): it is also found as the number 7 in `2026年7月号`.
    number: Option<String>,
}

/// The words of a query. A title matches when it matches every one of them (AND), in any order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Terms(Vec<Term>);

impl Terms {
    pub fn new(query: &str) -> Self {
        let terms = query
            .split_whitespace()
            .filter_map(|word| {
                let key = title_key(word);

                if key.is_empty() {
                    return None;
                }

                let number = key.chars().all(|c| c.is_ascii_digit()).then(|| strip_zeros(&key));

                Some(Term { key, number })
            })
            .collect();

        Terms(terms)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The words, folded the way titles are (for the response).
    pub fn keys(&self) -> Vec<&str> {
        self.0.iter().map(|term| term.key.as_str()).collect()
    }

    /// A word that every matching title has in its key, to narrow the rows read from the database: the
    /// longest one that is not a number (a number is also matched by value). `None` when all are numbers.
    pub fn prefilter(&self) -> Option<&str> {
        self.0
            .iter()
            .filter(|term| term.number.is_none())
            .map(|term| term.key.as_str())
            .max_by_key(|key| key.chars().count())
    }

    /// Does the title have every word? `key` is `title_key(title)`.
    pub fn matches(&self, title: &str, key: &str) -> bool {
        if self.0.is_empty() {
            return false;
        }

        let mut in_title: Option<Vec<String>> = None;

        self.0.iter().all(|term| match &term.number {
            // a number is a number of the title, not a few digits of a longer one (6 is not in 2026)
            Some(number) => in_title.get_or_insert_with(|| numbers(title)).contains(number),
            None => key.contains(&term.key),
        })
    }

    /// Does the title have the query? The words (the name of the magazine) have to be **outside the parentheses** of the
    /// title; the numbers (the issue) may be anywhere in it. The parts in parentheses are not the title:
    /// `kanojo ni dekiru koto (comic kairakuten 2018 8)` is a work of the issue, and `meguri dokoro 11-po (comic exe 61)`
    /// is not the issue `comic exe 11` (`comic exe` is in the parentheses), but `comic kairakuten (2026 nen 11 gatsu gou)`
    /// is the issue `comic kairakuten 2026 11`.
    pub fn matches_title(&self, title: &str) -> bool {
        let (outside, _) = split_parentheses(title);

        self.matches(title, &title_key(&outside))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_has_to_be_outside_the_parentheses_of_the_title_and_the_number_may_be_anywhere() {
        let issue = Terms::new("comic exe 11");

        assert!(issue.matches_title("comic exe 11"));
        assert!(issue.matches_title("[x] comic exe 11 [digital]"));
        assert!(issue.matches_title("comic exe 11 (digital tokusouban)"), "a note in parentheses");
        // the name of the magazine is in the parentheses: a work of the issue, or of another one
        assert!(!issue.matches_title("[mutsutake] meguri dokoro 11-po (comic exe 61) [chinese] [digital]"));
        assert!(!issue.matches_title("meguri dokoro (comic exe 11)"), "a work of the issue");
        assert!(!issue.matches_title("meguri dokoro （comic exe 11）"), "full width parentheses");
        assert!(!issue.matches_title("a (b (comic exe 11) c) d"), "a part inside a part");
        assert!(!issue.matches_title("a (comic exe 11"), "a parenthesis never closed holds the rest");
        // the name is outside: the number may be in the parentheses
        let kairakuten = Terms::new("comic kairakuten 2026 11");
        assert!(kairakuten.matches_title("comic kairakuten (2026 nen 11 gatsu gou)"));
        assert!(kairakuten.matches_title("comic kairakuten 2026 (11 gatsu gou)"));
        assert!(!kairakuten.matches_title("comic kairakuten (2026 nen 10 gatsu gou)"), "the number is not in the title");
        // brackets are not parentheses
        assert!(issue.matches_title("[comic exe 11] a"));
        // nothing to find
        assert!(!Terms::new("").matches_title("a (b)"));
    }

    #[test]
    fn a_query_is_tidied_up() {
        assert_eq!(normalize("  COMIC   E×E \t 11\n"), "COMIC ExE 11");
        assert_eq!(normalize(""), "");
        assert_eq!(tidy(" COMIC  E×E "), "COMIC E×E");
        assert_eq!(restore_times("COMIC ExE 74"), "COMIC E×E 74");
        assert_eq!(restore_times("COMIC E×E 74"), "COMIC E×E 74");
        assert_eq!(restore_times("Sexy Next ex xE x"), "Sexy Next ex xE x");
        // nothing is added
        assert_eq!(normalize("快楽天"), "快楽天");
    }

    #[test]
    fn japanese_is_told_from_the_rest() {
        assert!(has_japanese("COMIC快楽天"));
        assert!(has_japanese("ふらすと"));
        assert!(has_japanese("チート"));
        assert!(!has_japanese("COMIC ExE 11"));
        assert!(!has_japanese(""));
    }

    #[test]
    fn the_name_of_an_issue_becomes_the_query_the_bot_asks_for() {
        assert_eq!(issue_query("COMIC快楽天 2026年07月号"), "COMIC快楽天 2026 07");
        assert_eq!(issue_query("COMICペンギンクラブ 2022年08+09月合併号"), "COMICペンギンクラブ 2022 08+09 合併号");
        assert_eq!(issue_query("COMIC ExE 11"), "COMIC ExE 11");
        assert_eq!(issue_query("コミックグレープ Vol.100"), "コミックグレープ Vol.100");
        assert_eq!(issue_query("COMICエウロパ 2014年05月号"), "COMICエウロパ 2014 05");
    }

    #[test]
    fn romaji_leaves_text_without_japanese_alone() {
        assert_eq!(romaji("COMIC ExE 11", crate::testing::romanizer()).unwrap(), "COMIC ExE 11");
        assert_eq!(romaji("チート 2026", crate::testing::romanizer()).unwrap(), "cheat 2026");
    }

    fn matches(query: &str, title: &str) -> bool {
        Terms::new(query).matches(title, &title_key(title))
    }

    #[test]
    fn every_word_of_the_query_has_to_be_in_the_title_in_any_order() {
        assert!(matches("ヒミツ 関係", "ヒミツの関係"));
        assert!(matches("関係 ヒミツ", "ヒミツの関係"), "the order does not matter");
        assert!(!matches("ヒミツ 秘密", "ヒミツの関係"), "one missing word is no match");
        assert!(matches("ヒミツの関係", "ヒミツの関係 (1)"), "a part of the title");
        assert!(!matches("", "anything"), "an empty query matches nothing");
        assert!(!matches("   ", "anything"));
    }

    #[test]
    fn case_width_spaces_and_the_multiplication_sign_do_not_matter() {
        assert!(matches("comic exe", "COMIC E×E 11"));
        assert!(matches("COMIC E×E", "COMIC ExE 11"));
        assert!(matches("ＣＯＭＩＣ", "comic 快楽天"), "full width");
        assert!(matches("COMIC快楽天", "COMIC 快楽天 2026年7月号"), "the site puts a space in it");
        assert!(matches("comic 快楽天", "COMIC快楽天 2026年7月号"), "and the query does");
    }

    #[test]
    fn a_number_alone_is_found_as_a_number_whatever_the_zeros() {
        assert!(matches("快楽天 2026 07", "COMIC快楽天 2026年7月号"));
        assert!(matches("快楽天 2026 7", "COMIC快楽天 2026年07月号"));
        assert!(!matches("快楽天 2026 8", "COMIC快楽天 2026年07月号"));
        // the number is not any digit of another number
        assert!(!matches("快楽天 6 7", "COMIC快楽天 2026年07月号"), "6 is a digit of 2026, not a number of the title");
        // but a number inside a word is the word
        assert!(matches("vol.100", "コミックグレープ Vol.100"));
        assert!(matches("100", "コミックグレープ Vol.100"));
        assert!(matches("100", "COMIC ExE 100"));
    }

    #[test]
    fn the_prefilter_is_the_longest_word_that_is_not_a_number() {
        assert_eq!(Terms::new("COMIC快楽天 2026 07").prefilter(), Some("comic快楽天"));
        assert_eq!(Terms::new("a abc ab").prefilter(), Some("abc"));
        assert_eq!(Terms::new("2026 07").prefilter(), None, "all numbers: nothing to narrow by");
        assert_eq!(Terms::new("").prefilter(), None);
        assert_eq!(Terms::new("COMIC E×E").keys(), ["comic", "exe"]);
    }
}
