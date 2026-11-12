//! Splits text into words with lindera and its embedded UniDic, and keeps what the romanizer needs.
//!
//! cutlet gets the same things from MeCab through fugashi (`word.surface`, `word.is_unk`,
//! `word.white_space`, `word.feature.pos1`, ...). The columns of UniDic 2.1.2, which lindera embeds,
//! are: pos1 pos2 pos3 pos4 cType cForm lForm lemma orth pron orthBase pronBase goshu iType iForm fType fForm.

use crate::utils::{is_kana, Result};
use lindera::dictionary::load_dictionary;
use lindera::mode::Mode;
use lindera::segmenter::Segmenter;
use std::borrow::Cow;

const POS1: usize = 0;
const POS2: usize = 1;
const C_FORM: usize = 5;
const L_FORM: usize = 6;
const LEMMA: usize = 7;
const PRON: usize = 9;

#[derive(Debug, Clone, Default)]
pub struct Word {
    pub surface: String,
    /// The dictionary has no entry for it (digits and Latin text mostly, but kana too).
    pub is_unk: bool,
    /// There was white space before it in the text. UniDic skips white space, so this is
    /// worked out from the positions.
    pub white_space: bool,
    pub pos1: String,
    pub pos2: String,
    /// How the word is inflected (`連用形-一般`, `終止形-一般`, ...). Empty for a word that does not inflect.
    pub c_form: String,
    /// UniDic's lemma. For a foreign word it is `<katakana>-<foreign spelling>`, e.g. `チート-cheat`.
    pub lemma: String,
    /// The pronunciation of the word as written, in katakana. A long vowel is `ー` (号 is ゴー).
    pub pron: String,
    /// The reading of the dictionary form, in katakana, long vowels spelled out (号 is ゴウ).
    pub l_form: String,
}

impl Word {
    /// The word is a foreign word with a known foreign spelling: UniDic puts it after a `-` in the
    /// lemma, and a foreign spelling is ASCII. The lemma is overloaded: `私-代名詞` is not one.
    pub fn has_foreign_lemma(&self) -> bool {
        if self.surface.contains('-') {
            return false;
        }

        match self.lemma.split_once('-') {
            Some((_, spelling)) => spelling.is_ascii(),
            None => false,
        }
    }

    /// `チート-cheat` -> `cheat`
    pub fn foreign_spelling(&self) -> &str {
        self.lemma.split_once('-').map(|(_, spelling)| spelling).unwrap_or("")
    }

    /// The reading of the word, in katakana, as it is spelled (what MeCab with UniDic 2.3 calls `kana`:
    /// `ゴウ`, not the pronunciation `ゴー`).
    ///
    /// UniDic 2.1.2 only has the pronunciation (`pron`), where a long vowel is `ー` (号 is ゴー, しよう is
    /// ショー), and the reading of the dictionary form (`l_form`: 見込んで is ミコム). So:
    ///
    /// 1. A word written in kana is read as it is written: をはへ aside, the spelling is the reading
    ///    (しよう is shiyou, を is o). An interjection keeps its pronunciation (こんにちは is こんにちワ).
    /// 2. A word that is not inflected (or is in its dictionary form) is read from `l_form`, which spells
    ///    the long vowels out (号 is ゴウ, 言う is イウ). A numeral is not: ふ is フ, not フタ.
    /// 3. An inflected word is read from `pron` (見込んで is ミコン), and each `ー` becomes the vowel
    ///    that `l_form` has there if it has one (大きく is オオキク), or else the vowel the kana before it
    ///    ends with (聞い is キー, so キイ; 行こう is イコー, so イコウ).
    pub fn reading(&self) -> String {
        if is_kana(&self.surface) {
            return if self.pos1 == "感動詞" { self.pron.clone() } else { self.written_kana() };
        }

        let uninflected =
            self.c_form.is_empty() || self.c_form.starts_with("終止形") || self.c_form.starts_with("連体形");
        if uninflected && !self.l_form.is_empty() && self.pos2 != "数詞" {
            return self.l_form.clone();
        }

        resolve_long_vowels(&self.pron, &self.l_form)
    }

    /// The word as written, except that the is-pronounced-differently kana of a word are read as they
    /// sound (…は in a word that ends with ワ).
    fn written_kana(&self) -> String {
        let pron: Vec<char> = self.pron.chars().collect();
        let written: Vec<char> = self.surface.chars().collect();
        let same_length = pron.len() == written.len();

        written
            .iter()
            .enumerate()
            .map(|(i, &c)| match (c, pron.get(i)) {
                ('は', Some('ワ')) if same_length => 'わ',
                ('へ', Some('エ')) if same_length => 'え',
                _ => c,
            })
            .collect()
    }
}

/// The vowel a kana ends with, as the katakana of that vowel, for what a `ー` after it stands for.
/// お-row is ウ (`ou`) and え-row is イ (`ei`), the usual spelling of a long o and a long e.
fn long_vowel_after(kana: char) -> Option<char> {
    let row = |chars: &str| chars.contains(kana);

    if row("アカサタナハマヤラワガザダバパァャヮ") {
        Some('ア')
    } else if row("イキシチニヒミリギジヂビピィ") {
        Some('イ')
    } else if row("ウクスツヌフムユルグズヅブプゥュヴ") {
        Some('ウ')
    } else if row("エケセテネヘメレゲゼデベペェ") {
        Some('イ')
    } else if row("オコソトノホモヨロヲゴゾドボポォョ") {
        Some('ウ')
    } else {
        None
    }
}

fn resolve_long_vowels(pron: &str, l_form: &str) -> String {
    let l_form: Vec<char> = l_form.chars().collect();
    let mut out: Vec<char> = Vec::new();

    for (i, c) in pron.chars().enumerate() {
        if c != 'ー' {
            out.push(c);
            continue;
        }

        let spelled_out = l_form.get(i).copied().filter(|v| "アイウエオ".contains(*v));
        let after_previous = out.last().and_then(|&p| long_vowel_after(p));

        out.push(spelled_out.or(after_previous).unwrap_or('ー'));
    }

    out.into_iter().collect()
}

pub struct Tokenizer {
    segmenter: Segmenter,
}

impl Tokenizer {
    /// Loads the embedded dictionary (takes a moment, so it is done once).
    pub fn new() -> Result<Self> {
        let dictionary = load_dictionary("embedded://unidic").map_err(|e| e.to_string())?;

        Ok(Self { segmenter: Segmenter::new(Mode::Normal, dictionary, None) })
    }

    pub fn words(&self, text: &str) -> Result<Vec<Word>> {
        let mut tokens = self.segmenter.segment(Cow::Borrowed(text)).map_err(|e| e.to_string())?;

        let mut words = Vec::with_capacity(tokens.len());
        let mut previous_end = 0;

        for token in tokens.iter_mut() {
            let (start, end) = (token.byte_start, token.byte_end);
            let surface = token.surface.to_string();
            let is_unk = token.word_id.is_unknown();
            let details: Vec<String> = token.details().iter().map(|d| column(d)).collect();
            let detail = |i: usize| details.get(i).cloned().unwrap_or_default();

            words.push(Word {
                surface,
                is_unk,
                white_space: start > previous_end,
                pos1: detail(POS1),
                pos2: detail(POS2),
                c_form: detail(C_FORM),
                lemma: detail(LEMMA),
                pron: detail(PRON),
                l_form: detail(L_FORM),
            });

            previous_end = end;
        }

        Ok(words)
    }
}

/// UniDic writes `*` for an empty column.
fn column(value: &str) -> String {
    if value == "*" { String::new() } else { value.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(lemma: &str, surface: &str) -> Word {
        Word { surface: surface.to_string(), lemma: lemma.to_string(), ..Default::default() }
    }

    #[test]
    fn finds_the_foreign_spelling_in_the_lemma() {
        let cheat = word("チート-cheat", "チート");
        assert!(cheat.has_foreign_lemma());
        assert_eq!(cheat.foreign_spelling(), "cheat");
    }

    #[test]
    fn a_hyphen_that_is_not_a_foreign_spelling_does_not_count() {
        // 私-代名詞, and 外国 in place of a spelling
        assert!(!word("私-代名詞", "私").has_foreign_lemma());
        assert!(!word("ジル-外国", "ジル").has_foreign_lemma());
        assert!(!word("管理", "管理").has_foreign_lemma());
        assert!(!word("チート-cheat", "チ-ト").has_foreign_lemma());
    }

    fn w(surface: &str, pron: &str, l_form: &str, c_form: &str) -> Word {
        Word {
            surface: surface.into(),
            pron: pron.into(),
            l_form: l_form.into(),
            c_form: c_form.into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_word_written_in_kana_is_read_as_it_is_written() {
        // the pronunciation has ー (ショー), the spelling is what is wanted
        assert_eq!(w("しよう", "ショー", "スル", "意志推量形").reading(), "しよう");
        assert_eq!(w("おねえちゃん", "オネーチャン", "オネエチャン", "").reading(), "おねえちゃん");
        // をはへ: the spelling (を is o), except when the word is read ワ / エ
        assert_eq!(w("を", "オ", "ヲ", "").reading(), "を");
        assert_eq!(w("ヲタ", "オタ", "ヲタ", "").reading(), "ヲタ");
        // …は at the end of a word that is pronounced ワ
        assert_eq!(w("こんにちは", "コンニチワ", "コンニチハ", "").reading(), "こんにちわ");
    }

    #[test]
    fn an_interjection_keeps_its_pronunciation() {
        let hello = Word { pos1: "感動詞".into(), ..w("こんにちは", "コンニチワ", "コンニチハ", "") };
        assert_eq!(hello.reading(), "コンニチワ");
    }

    #[test]
    fn a_word_that_does_not_inflect_is_read_from_the_dictionary_form() {
        assert_eq!(w("号", "ゴー", "ゴウ", "").reading(), "ゴウ");
        // a verb in its dictionary form: the pronunciation is how it is said, not how it is spelled
        assert_eq!(w("言う", "ユー", "イウ", "終止形-一般").reading(), "イウ");
        // a numeral is read from the pronunciation: ふ is フ, not フタ
        let two = Word { pos2: "数詞".into(), ..w("二", "フ", "フタ", "") };
        assert_eq!(two.reading(), "フ");
    }

    #[test]
    fn an_inflected_word_has_its_long_vowels_resolved() {
        // not the reading of the dictionary form (見込む is ミコム)
        assert_eq!(w("見込ん", "ミコン", "ミコム", "連用形-撥音便").reading(), "ミコン");
        // a vowel the dictionary form has there: 大きく is オオキク
        assert_eq!(w("大きく", "オーキク", "オオキイ", "連用形-一般").reading(), "オオキク");
        // otherwise the vowel before it is repeated: i / u / a, and ou / ei for o / e
        assert_eq!(w("聞い", "キー", "キク", "連用形-イ音便").reading(), "キイ");
        assert_eq!(w("行こう", "イコー", "イク", "意志推量形").reading(), "イコウ");
        assert_eq!(w("読め", "ヨメー", "ヨム", "命令形").reading(), "ヨメイ");
        // nothing to repeat after ン: the mark stays
        assert_eq!(w("x", "ンー", "", "連用形").reading(), "ンー");
    }
}
