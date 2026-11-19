//! Japanese text to the romaji that is used as a search query.
//!
//! The base is a port of cutlet (`Cutlet.romaji_tokens` / `romaji_word`): words are split with
//! UniDic, read from the dictionary, and written in Hepburn romaji, with the foreign spelling of
//! katakana loanwords (チート is `cheat`). On top of that:
//!
//! - a word spelled in hiragana that UniDic cuts into pieces (ふらす / とれ / ー / しょ / ん) is joined
//!   again, and `ー` is written as `-` (`furasutore-shon`);
//! - `ー` is `-` in every word that has one (`rikabari-`, not `rikabarii`);
//! - `Loanwords::English` looks up the hiragana loanwords in katakana (`frustration`);
//! - a table of exceptions overrides the words whose foreign spelling in UniDic is wrong;
//! - the output is lowercase, with single spaces.
//!
//! The reasons for each of these are in `docs/ALGORITHM.md`.

use crate::romaji::{lookup, map_kana};
use crate::tokenizer::{Tokenizer, Word};
use crate::utils::{hira_to_kata, is_hiragana, is_hiragana_char, is_kana, is_katakana_char, kata_to_hira, Result};
use regex::Regex;
use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;
use unicode_normalization::UnicodeNormalization;

/// The small kana: a word that is one of them was cut off the kana before it.
const SMALL_KANA: &str = "ぁぃぅぇぉゃゅょゎァィゥェォャュョヮ";

/// Words that are written out instead of being read: cutlet's own, the ones where UniDic's foreign
/// spelling is a different foreign word with the same sound (ラーメン is `Rahmen`, German for
/// "frame"; メイド is `made`), and the ones UniDic 2.1.2 reads in an unusual way. More can be added in
/// `config.toml`.
const BUILT_IN_EXCEPTIONS: &[(&str, &str)] = &[
    ("ホッピー", "Hoppy"),
    ("東京", "Tokyo"),
    ("大阪", "Osaka"),
    ("京都", "Kyoto"),
    ("柔道", "judo"),
    ("弁当", "bento"),
    ("ヒルズ", "hills"),
    ("カービィ", "Kirby"),
    ("ラーメン", "ramen"),
    ("メイド", "maid"),
    // UniDic reads it わたくし (formal); in a title it is almost always わたし
    ("私", "watashi"),
    ("私たち", "watashitachi"),
    ("私達", "watashitachi"),
    // 電子版, 限定版: after a noun it is ban (UniDic 2.1.2 says han)
    ("版", "ban"),
    // a magazine of this catalogue: UniDic cuts it into クリ (clitoris) and ベロン
    ("クリベロン", "kuriberon"),
    // the other magazines of this catalogue, as the sites in the Latin alphabet write them
    // (COMIC Shitsurakuten, COMIC Kairakuten Beast, COMIC Penguin Club Sanzokuban)
    ("失楽天", "shitsurakuten"),
    ("ビースト", "beast"),
    ("山賊版", "sanzokuban"),
    // the names of the tags: [DL版] is [Digital], [中国翻訳] is [Chinese]
    ("DL版", "digital"),
    ("中国翻訳", "chinese"),
    // words that the sites in the Latin alphabet write in a fixed way (found by comparing the
    // romaji of the Japanese titles with the English titles of the same works)
    ("オルガ", "orga"),
    ("エロ", "ero"),
    ("コスプレ", "cosplay"),
    ("コスプレイヤー", "cosplayer"),
    ("プリキュア", "precure"),
    ("えっち", "ecchi"),
    ("エッチ", "ecchi"),
    ("ケイ", "kei"),
    ("お姉ちゃん", "oneechan"),
    ("おねえちゃん", "oneechan"),
    ("お姉さん", "oneesan"),
    ("お兄ちゃん", "oniichan"),
    ("お兄さん", "oniisan"),
    ("お母さん", "okaasan"),
    ("お父さん", "otousan"),
];

/// What to do with a loanword that is written in hiragana (ふらすとれーしょん).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Loanwords {
    /// Romaji of the kana as written: `furasutore-shon`.
    #[default]
    Romaji,
    /// The English word, if UniDic has the katakana spelling as a foreign word: `frustration`.
    English,
}

/// A piece of the output. `space` says that a space follows it.
#[derive(Debug, Clone)]
struct Token {
    surface: String,
    space: bool,
    /// It comes from a foreign lemma.
    foreign: bool,
}

impl Token {
    fn new(surface: impl Into<String>, space: bool) -> Self {
        Self { surface: surface.into(), space, foreign: false }
    }
}

pub struct Romanizer {
    tokenizer: Tokenizer,
    exceptions: HashMap<String, String>,
}

impl Romanizer {
    /// `exceptions` are added to the built-in ones (and win over them).
    pub fn new(exceptions: &BTreeMap<String, String>) -> Result<Self> {
        let mut table: HashMap<String, String> = BUILT_IN_EXCEPTIONS
            .iter()
            .map(|(surface, romaji)| (surface.to_string(), romaji.to_string()))
            .collect();
        table.extend(exceptions.iter().map(|(k, v)| (k.clone(), v.clone())));

        Ok(Self { tokenizer: Tokenizer::new()?, exceptions: table })
    }

    pub fn romanize(&self, text: &str, loanwords: Loanwords) -> Result<String> {
        // Unicode NFKC: full-width letters and digits become ASCII, half-width katakana full-width
        let text: String = text.nfkc().collect();
        if text.is_empty() {
            return Ok(String::new());
        }
        let text = issue_date(&text);

        let words = self.tokenizer.words(&text)?;
        let tokens = self.tokens(&words, loanwords)?;

        let joined: String = tokens
            .iter()
            .map(|t| if t.space { format!("{} ", t.surface) } else { t.surface.clone() })
            .collect();

        // a search does not care about the case, and cutlet capitalizes proper nouns
        Ok(joined.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" "))
    }

    fn tokens(&self, words: &[Word], loanwords: Loanwords) -> Result<Vec<Token>> {
        let words = &join_names(&self.join_exceptions(words));
        let mut tokens = self.cutlet_tokens(words);

        // cutlet joins a suffix to the word before it; after an English word that reads `maidsan`
        for i in 0..tokens.len().saturating_sub(1) {
            if tokens[i].foreign && !tokens[i].space && words[i + 1].pos1 == "接尾辞" {
                tokens[i].space = true;
            }
        }

        let groups = kana_groups(words);

        let mut out = Vec::with_capacity(tokens.len());
        let mut i = 0;
        while i < tokens.len() {
            let Some(&(_, end)) = groups.iter().find(|(start, _)| *start == i) else {
                // a prolonged sound mark that was written as one stays a hyphen (rikabarii -> rikabari-),
                // unless the word is an English word; a long vowel written in kana (ryokou) is kept
                let word = &words[i];
                if word.surface.contains('ー')
                    && is_kana(&word.surface)
                    && !word.has_foreign_lemma()
                    && !self.exceptions.contains_key(&word.surface)
                {
                    tokens[i].surface = hyphenated(&word.surface);

                    // サー|ヴァント: UniDic cut a katakana word after its mark; the word goes on
                    if is_katakana_word(&word.surface)
                        && words.get(i + 1).is_some_and(|n| n.surface.starts_with(is_katakana_char))
                    {
                        tokens[i].space = false;
                    }
                }

                out.push(tokens[i].clone());
                i += 1;
                continue;
            };

            let original: String = words[i..=end].iter().map(|w| w.surface.as_str()).collect();
            let english = if loanwords == Loanwords::English {
                self.loanword_in_english(&original)?
            } else {
                None
            };

            // the joined word takes the space of its last piece
            let mut merged = tokens[end].clone();
            merged.surface = english.unwrap_or_else(|| hyphenated(&original));
            out.push(merged);
            i = end + 1;
        }

        Ok(out)
    }

    /// An exception can be several words of UniDic (クリベロン is クリ + ベロン): the words that together
    /// are written like one are made one word, so that the exception finds it.
    fn join_exceptions(&self, words: &[Word]) -> Vec<Word> {
        /// The most words one exception can span.
        const MOST_WORDS: usize = 8;

        let mut out = Vec::with_capacity(words.len());
        let mut i = 0;

        while i < words.len() {
            let span = (2..=MOST_WORDS.min(words.len() - i)).rev().find_map(|len| {
                let surface: String = words[i..i + len].iter().map(|w| w.surface.as_str()).collect();

                self.exceptions.contains_key(&surface).then_some((len, surface))
            });

            match span {
                Some((len, surface)) => {
                    out.push(Word { surface, is_unk: false, pos1: "名詞".to_string(), ..words[i].clone() });
                    i += len;
                }
                None => {
                    out.push(words[i].clone());
                    i += 1;
                }
            }
        }

        out
    }

    /// A hiragana word, looked up as katakana: it is an English word if UniDic has it as a foreign
    /// word (ふらすとれーしょん -> フラストレーション -> `frustration`).
    fn loanword_in_english(&self, kana: &str) -> Result<Option<String>> {
        let words = self.tokenizer.words(&hira_to_kata(kana))?;

        if words.is_empty() || !words.iter().all(Word::has_foreign_lemma) {
            return Ok(None);
        }

        Ok(Some(words.iter().map(Word::foreign_spelling).collect::<Vec<_>>().join(" ")))
    }

    /// `Cutlet.romaji_tokens`: one token for every word, with the space that follows it.
    fn cutlet_tokens(&self, words: &[Word]) -> Vec<Token> {
        let mut out: Vec<Token> = Vec::with_capacity(words.len());

        for (i, word) in words.iter().enumerate() {
            let next = words.get(i + 1);

            // the possessive apostrophe sticks to both sides
            if word.surface == "'"
                && next.is_some_and(|n| n.surface.starts_with(|c: char| c.is_ascii_alphabetic()) && !n.white_space)
                && !word.white_space
            {
                if let Some(previous) = out.last_mut() {
                    previous.space = false;
                }
                out.push(Token::new(word.surface.clone(), false));
                continue;
            }

            // a small tsu that UniDic took for a symbol, before kana (た～っぷり): it sticks to the next word,
            // which doubles its first letter. Before anything else (スワッピングっ！) it is dropped as a symbol.
            if (word.surface == "っ" || word.surface == "ッ") && word.pos1 == "補助記号" && next.is_some_and(|n| is_kana(&n.surface)) {
                out.push(Token::new("っ", false));
                continue;
            }

            // a verb or adjective that was split: 乗っ|た
            let romaji = self.romaji_word(word);
            if let (Some(previous), Some(first)) = (out.last_mut(), romaji.chars().next()) {
                if previous.surface.ends_with('っ') {
                    previous.surface.pop();
                    previous.surface.push(first);
                }
            }

            let foreign = word.has_foreign_lemma();
            let mut token = Token { surface: romaji.clone(), space: false, foreign };

            // punctuation with a space of its own
            if "「『".contains(word.surface.as_str()) || "([".contains(romaji.as_str()) {
                if let Some(previous) = out.last_mut() {
                    previous.space = true;
                }
                out.push(token);
                continue;
            }
            if romaji == "/" {
                out.push(token);
                continue;
            }

            // keep the spaces between ASCII words as they were
            if word.surface.is_ascii() && next.is_some_and(|n| n.surface.is_ascii()) {
                out.push(Token::new(word.surface.clone(), next.is_some_and(|n| n.white_space)));
                continue;
            }

            // no space after some words: お酒 -> osake, 図書館 -> toshokan, 思えば -> omoeba, 333
            let no_space = word.pos1 == "接頭辞"
                || next.is_some_and(|n| n.pos1 == "補助記号" || n.pos1 == "接尾辞")
                || next.is_some_and(|n| n.surface == ",")
                || (foreign && romaji.ends_with('-'))
                || next.is_some_and(|n| n.pos2 == "接続助詞")
                || (is_digits(&word.surface) && next.is_some_and(|n| is_digits(&n.surface)))
                // そうでした -> sou deshita
                || (next.is_some_and(|n| {
                    matches!(word.pos1.as_str(), "動詞" | "助動詞" | "形容詞")
                        && n.pos1 == "助動詞"
                        && n.surface != "です"
                }));

            // the small tsu of た～っぷり sticks to the word after it
            token.space = !no_space;
            out.push(token);
        }

        // a っ that was not picked up by the next word
        for token in &mut out {
            token.surface = token.surface.replace('っ', "");
        }

        out
    }

    /// `Cutlet.romaji_word`: the romaji of one word.
    fn romaji_word(&self, word: &Word) -> String {
        if let Some(romaji) = self.exceptions.get(&word.surface) {
            return romaji.clone();
        }
        if is_digits(&word.surface) || word.surface.is_ascii() {
            return word.surface.clone();
        }
        if word.surface == "€" || word.surface == "¥" {
            return word.surface.clone();
        }

        if word.is_unk {
            // not in the dictionary: kana can still be read; anything else goes through
            return if is_kana(&word.surface) { map_kana(&kata_to_hira(&word.surface)) } else { word.surface.clone() };
        }

        if word.pos1 == "補助記号" {
            // a symbol the table does not know is dropped
            return lookup(&word.surface).unwrap_or("").to_string();
        }
        if word.pos1 == "助詞" && word.pron == "ワ" {
            return "wa".to_string(); // the particle は
        }
        if word.pos1 == "助詞" && word.pron == "エ" {
            return "e".to_string(); // the particle へ
        }
        if word.has_foreign_lemma() {
            return word.foreign_spelling().to_string();
        }

        let reading = word.reading();
        if reading.is_empty() { word.surface.clone() } else { map_kana(&kata_to_hira(&reading)) }
    }
}

/// `2024年8月号` is written `2024-08`, the way the sites that use the Latin alphabet write the issue of a
/// magazine (`COMIC Kairakuten 2024-08`). A search for `2024 8` still finds it: the numbers of a title are
/// its runs of digits.
fn issue_date(text: &str) -> String {
    static ISSUE: OnceLock<Regex> = OnceLock::new();
    let issue = ISSUE.get_or_init(|| Regex::new(r"(\d{4})年\s*(\d{1,2})月号").expect("a valid pattern"));

    issue.replace_all(text, |c: &regex::Captures| format!("{}-{:0>2}", &c[1], &c[2])).into_owned()
}

/// The most kana a name that is joined can have (ひつじだ, くまでら, むーにゃん).
const MOST_NAME_KANA: usize = 8;

/// A circle or an artist in brackets (`[ ]`, `( )`, `【 】`; not a quotation in 「 」), written in kana (`[ひつじだ (くまでら)]`), is one word to the sites that
/// write it in the Latin alphabet (`Hitsujida (Kumadela)`), but UniDic cuts it into the words it knows
/// (ひつじ / だ, くま / でら). Inside brackets, the pieces that are next to each other and are all hiragana (or
/// all katakana) are made one word, if they are not longer than a name (`MOST_NAME_KANA`). Anything else is
/// left alone: a foreign word (`ブルーアーカイブ` is `blue archive`), kanji words, and a hiragana piece next to a katakana one (`ちぃず` `オレ`).
fn join_names(words: &[Word]) -> Vec<Word> {
    fn closer(open: &str) -> &'static str {
        match open {
            "[" => "]",
            "(" => ")",
            "【" => "】",
            _ => "",
        }
    }

    // one script: all hiragana, or all katakana (with the prolonged sound mark)
    fn script(word: &Word) -> Option<bool> {
        if word.surface.chars().all(|c| is_hiragana_char(c) || c == 'ー') && word.surface.chars().any(is_hiragana_char) {
            Some(true)
        } else if is_katakana_word(&word.surface) {
            Some(false)
        } else {
            None
        }
    }

    let mut out: Vec<Word> = Vec::with_capacity(words.len());
    let mut close: Vec<&'static str> = Vec::new();
    // the position in `out` where the run of one script starts, and its script
    let mut run: Option<(usize, bool)> = None;

    for word in words {
        if close.last() == Some(&word.surface.as_str()) {
            close.pop();
        } else if !closer(&word.surface).is_empty() {
            close.push(closer(&word.surface));
        }

        // a foreign word (コミック, ブルー) keeps its foreign spelling (comic, blue)
        let kana = if close.is_empty() || word.white_space || word.has_foreign_lemma() { None } else { script(word) };

        if let (Some(kana), Some((start, same))) = (kana, run) {
            let joined: usize = out[start..].iter().map(|w| w.surface.chars().count()).sum::<usize>()
                + word.surface.chars().count();

            if kana == same && joined <= MOST_NAME_KANA {
                // the run is `out[start..]`: the pieces and the word are made one word
                let mut name = out[start].clone();
                for w in &out[start + 1..] {
                    name.surface.push_str(&w.surface);
                }
                name.surface.push_str(&word.surface);
                name.is_unk = true;
                name.pos1 = "名詞".to_string();
                name.pos2 = String::new();
                name.lemma = String::new();
                out.truncate(start);
                out.push(name);
                continue;
            }
        }

        run = kana.map(|k| (out.len(), k));
        out.push(word.clone());
    }

    out
}

/// Katakana, with the prolonged sound mark.
fn is_katakana_word(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| is_katakana_char(c) || c == 'ー')
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_digit())
}

/// A dictionary word, not a piece of a word: UniDic knows it, and it is a noun, adjective, ... A
/// word with a small kana in it is always a piece (ふぁん of ふぁんたじー).
fn is_ordinary_word(word: &Word) -> bool {
    if word.is_unk || word.surface.chars().any(|c| SMALL_KANA.contains(c)) {
        return false;
    }

    matches!(word.pos1.as_str(), "名詞" | "形容詞" | "副詞" | "連体詞" | "接続詞" | "代名詞")
        && word.pos2 != "数詞"
}

/// The words that are one word spelled in kana, as `(first, last)` positions.
///
/// - A prolonged sound mark after hiragana (ふらすとれーしょん): back to the dictionary word before
///   the pieces (よし), and forward over the end pieces (しょ, ん) but not over a particle (の).
/// - A small kana that was cut off (ふ / ぁ / みこん).
fn kana_groups(words: &[Word]) -> Vec<(usize, usize)> {
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let n = words.len();

    for (i, word) in words.iter().enumerate() {
        if word.surface == "ー" && i > 0 && is_hiragana(&words[i - 1].surface) {
            let mut start = i - 1;
            while start > 0 && is_hiragana(&words[start - 1].surface) && !is_ordinary_word(&words[start - 1]) {
                start -= 1;
            }

            let mut end = i;
            while end + 1 < n && is_hiragana(&words[end + 1].surface) {
                let next = &words[end + 1];
                if next.pos1 != "助詞" || next.surface == "ん" {
                    end += 1;
                } else {
                    break;
                }
            }

            spans.push((start, end));
        } else if word.surface.chars().count() == 1
            && SMALL_KANA.contains(word.surface.as_str())
            && i > 0
            && is_kana(&words[i - 1].surface)
        {
            let mut end = i;
            if i + 1 < n && is_hiragana(&words[i + 1].surface) && words[i + 1].pos1 != "助詞" {
                end = i + 1;
            }

            spans.push((i - 1, end));
        }
    }

    // spans that overlap become one
    spans.sort();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (start, end) in spans {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }

    merged
}

/// Kana to romaji, with `ー` as a hyphen (`ふらすとれーしょん` is `furasutore-shon`, not `furasutoreeshon`).
/// `n'` is written `n` here: a search does not need the apostrophe.
fn hyphenated(kana: &str) -> String {
    // a っ at the end has nothing to double: cutlet drops it too
    map_kana(&kana.replace('ー', "-")).replace("n'", "n").replace('っ', "")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::romanizer;

    fn romaji(text: &str) -> String {
        romanizer().romanize(text, Loanwords::Romaji).unwrap()
    }

    fn english(text: &str) -> String {
        romanizer().romanize(text, Loanwords::English).unwrap()
    }

    #[test]
    fn katakana_loanwords_become_english_words() {
        assert_eq!(romaji("チートアイテム管理局のお仕事"), "cheat item kanri kyoku no oshigoto");
        assert_eq!(romaji("トラップトリップ（2）"), "trap trip (2)");
        assert_eq!(romaji("ピュアなふたりの恋人事情"), "pure na futari no koibito jijou");
    }

    #[test]
    fn a_hiragana_loanword_cut_into_pieces_is_joined() {
        assert_eq!(romaji("ナカよしふらすとれーしょん！"), "naka yoshi furasutore-shon!");
        assert_eq!(romaji("ふらすとれーしょんの話"), "furasutore-shon no hanashi");
        assert_eq!(romaji("ふれーずの世界"), "fure-zu no sekai");
        assert_eq!(romaji("ふぁみこん（5）"), "famikon (5)");
    }

    #[test]
    fn a_prolonged_sound_mark_is_a_hyphen() {
        assert_eq!(romaji("失恋リカバリー"), "shitsuren rikabari-");
        assert_eq!(romaji("ありがとー"), "arigato-");
        // written with kana, not with the mark: kept
        assert_eq!(romaji("温泉旅行"), "onsen ryokou");
    }

    #[test]
    fn hiragana_loanwords_can_be_looked_up_in_katakana() {
        assert_eq!(english("ナカよしふらすとれーしょん！"), "naka yoshi frustration!");
        assert_eq!(english("かわいいふぁんたじーの世界"), "kawaii fantasy no sekai");
        // nothing to look up: the same as the default
        assert_eq!(english("チートアイテム管理局のお仕事"), "cheat item kanri kyoku no oshigoto");
    }

    #[test]
    fn exceptions_replace_the_wrong_foreign_spelling() {
        assert_eq!(romaji("ご主人様はメイドさん"), "goshujinsama wa maid san");
        assert_eq!(romaji("ラーメン屋さんの話"), "ramen yasan no hanashi");
    }

    #[test]
    fn an_exception_can_be_several_words_of_the_dictionary() {
        // UniDic cuts it into クリ (clitoris) and ベロン
        assert_eq!(romaji("comicクリベロン DUMA Vol.35"), "comic kuriberon duma vol.35");

        // the ones from config.toml work the same way, and win over the built-in ones
        let config = BTreeMap::from([
            ("アンロック・デ・ザイア".to_string(), "anrokku de zaia".to_string()),
            ("メイド".to_string(), "maido".to_string()),
        ]);
        let custom = Romanizer::new(&config).unwrap();
        assert_eq!(custom.romanize("アンロック・デ・ザイア", Loanwords::Romaji).unwrap(), "anrokku de zaia");
        assert_eq!(custom.romanize("メイド", Loanwords::Romaji).unwrap(), "maido");
        // a word that only starts like an exception is not one: 私立 is not 私
        assert_eq!(romaji("私立"), "shiritsu");
        assert_eq!(romaji("限定版"), "gentei ban");
    }

    #[test]
    fn ascii_and_symbols_go_through() {
        assert_eq!(romaji("COMIC E×E 73"), "comic e×e 73");
        assert_eq!(romaji("Fleur ♯ex"), "fleur ex");
        assert_eq!(romaji(""), "");
        assert_eq!(romaji("   "), "");
    }

    #[test]
    fn particles_are_read_as_they_are_spoken() {
        assert_eq!(romaji("私は学生です"), "watashi wa gakusei desu");
        // the particle を is o (not wo): the sites that write titles in the Latin alphabet write it so
        assert_eq!(romaji("本を読む"), "hon o yomu");
        assert_eq!(romaji("歪んだ三角の和を求めて"), "yuganda sankaku no wa o motomete");
        // katakana ヲ too; ウォ (a loanword sound) stays wo
        assert_eq!(romaji("ウォーター"), "water");
    }

    #[test]
    fn the_issue_of_a_magazine_is_written_year_dash_month() {
        assert_eq!(romaji("COMIC快楽天 2026年11月号"), "comic kairakuten 2026-11");
        assert_eq!(romaji("COMICペンギンクラブ2017年12月号"), "comic penguin club 2017-12");
        assert_eq!(romaji("COMIC快楽天ビースト 2024年8月号"), "comic kairakuten beast 2024-08");
        assert_eq!(romaji("2024年8月号"), "2024-08");
        // not an issue: no 号, or no year
        assert_eq!(romaji("2024年8月"), "2024 nen 8 gatsu");
        assert_eq!(romaji("8月号"), "8 gatsu gou");
    }

    #[test]
    fn a_name_in_brackets_written_in_kana_is_one_word() {
        assert_eq!(romaji("[ひつじだ (くまでら)]"), "[hitsujida (kumadera)]");
        assert_eq!(romaji("[むーにゃん] 影"), "[mu-nyan] kage");
        assert_eq!(romaji("【ずんだもん】"), "[zundamon]");
        // one script only: ひらがな next to カタカナ stays two words
        assert_eq!(romaji("[ちぃずオレ]"), "[chizu ore]");
        // a foreign word keeps its foreign spelling
        assert_eq!(romaji("(ブルーアーカイブ)"), "(blue archive)");
        assert_eq!(romaji("(コミックグレープ Vol.100)"), "(comic grape vol.100)");
        // outside brackets, and in a quotation, nothing is joined
        assert_eq!(romaji("ひつじだとくまでら"), "hitsuji da to kuma dera");
        assert_eq!(romaji("「ひつじだ」"), "\"hitsuji da\"");
        // longer than a name: left as UniDic cut it
        assert_eq!(romaji("[ほんとうにあったこわいはなし]"), "[hontouniatta kowaihanashi]");
    }

    #[test]
    fn the_magazines_of_the_catalogue_are_written_like_the_sites_write_them() {
        assert_eq!(romaji("COMIC失楽天 2023年03月号"), "comic shitsurakuten 2023-03");
        assert_eq!(romaji("COMIC快楽天ビースト 2026年11月号"), "comic kairakuten beast 2026-11");
        assert_eq!(romaji("COMICペンギンクラブ山賊版 2011年05月号"), "comic penguin club sanzokuban 2011-05");
    }

    #[test]
    fn a_small_tsu_after_a_wave_dash_doubles_the_next_letter() {
        assert_eq!(romaji("愛される話た～っぷり"), "aisareru hanashi ta~ppuri");
        assert_eq!(romaji("ちょっと待って"), "chotto matte");
    }

    #[test]
    fn words_that_the_sites_write_in_a_fixed_way() {
        assert_eq!(romaji("COMICオルガ vol.75"), "comic orga vol.75");
        assert_eq!(romaji("エロコスプレ"), "ero cosplay");
        assert_eq!(romaji("えっちなお姉さん"), "ecchi na oneesan");
        assert_eq!(romaji("お母さん"), "okaasan");
        assert_eq!(romaji("[DL版] [中国翻訳]"), "[digital] [chinese]");
    }

    #[test]
    fn long_vowels_are_resolved_to_the_spelling() {
        assert_eq!(romaji("遊ぼう"), "asobou");
        assert_eq!(romaji("しよう"), "shiyou");
        assert_eq!(romaji("言う"), "iu");
        assert_eq!(romaji("聞いて"), "kiite");
        assert_eq!(romaji("おねえちゃん"), "oneechan");
    }

    #[test]
    fn a_katakana_word_cut_after_its_mark_is_not_split_by_a_space() {
        assert_eq!(romaji("サーヴァント3"), "sa-vanto 3");
    }

    #[test]
    fn the_middle_dot_and_ui_are_romanized() {
        assert_eq!(romaji("チート・アイテム"), "cheat/item");
        assert_eq!(romaji("うぃんたー"), "winta-");
    }

    #[test]
    fn small_kana_pieces_are_joined_but_particles_are_not() {
        let words = romanizer().tokenizer.words("ふぁみこん").unwrap();
        let groups = kana_groups(&words);
        assert!(!groups.is_empty());
    }
}
