//! The text index of the titles (tantivy), which answers a `Matcher` with the row numbers of the works, the best answer first.
//!
//! One document per row of `works`, found by its row number (`rowid`). Four fields, none of them stored (the rows are read from the database):
//! - `key`: the title key in trigrams. A piece of 3 characters or more is found as the AND of its trigrams. The trigrams have no position, so
//!   the pieces of a title that has the trigrams apart are found too: the caller checks the pieces in the key (`Matcher::accepts`).
//! - `words`: the words of the romaji, whole (a piece of 1 or 2 letters is a word of the title, `ai` is not `maid`).
//! - `starts`: the first 3 to 6 letters of every word of the romaji, so that a word that starts with the piece of the query scores higher.
//! - `author`: the words in the `[...]` at the start of the title (circle and artist), in trigrams: a work whose author is the query comes first.
//!
//! The order is the score (BM25 of the pieces, a short title scores higher than a long one, the starts of words and the author add to it), and the
//! row number for the same score.

use std::path::Path;
use std::sync::Mutex;

use tantivy::collector::TopDocs;
use tantivy::directory::MmapDirectory;
use tantivy::query::{BooleanQuery, BoostQuery, Occur, Query, TermQuery};
use tantivy::schema::{Field, IndexRecordOption, Schema, TextFieldIndexing, TextOptions, FAST, INDEXED};
use tantivy::tokenizer::{LowerCaser, NgramTokenizer, SimpleTokenizer, TextAnalyzer, WhitespaceTokenizer};
use tantivy::{doc, DocId, Index, IndexReader, IndexWriter, ReloadPolicy, Score, SegmentReader, Term};

use crate::query::Matcher;
use crate::utils::Result;

/// The memory the writer may use (tantivy flushes a segment when it has this much).
const WRITER_MEMORY: usize = 100_000_000;
/// The prefixes of a word that `starts` has: from this many letters to the next constant.
const START_MIN: usize = 3;
const START_MAX: usize = 6;
/// What the score of a piece that starts a word of the title, and of an author that is the query, is multiplied by.
const START_BOOST: f32 = 2.0;
const AUTHOR_BOOST: f32 = 8.0;
/// What the score of the spelling of the query is multiplied by, against the other spellings of the same word.
const OWN_SPELLING_BOOST: f32 = 1.5;
/// The most words of a query that are joined as one word of a title (`rashou mon` is `rashoumon`).
pub const MOST_JOINED: usize = 4;

#[derive(Clone, Copy)]
struct Fields {
    key: Field,
    words: Field,
    starts: Field,
    author: Field,
    tag_ids: Field,
    rowid: Field,
}

pub struct TextIndex {
    index: Index,
    reader: IndexReader,
    writer: Mutex<IndexWriter>,
    fields: Fields,
}

/// What a work is indexed by.
pub struct Doc<'a> {
    pub rowid: i64,
    pub key: &'a str,
    pub romaji: &'a str,
    pub title: &'a str,
    /// The uploader (lower case romaji; empty when none): searched like the author of the title.
    pub uploader: &'a str,
    /// The ids of its tags (`tags.id`).
    pub tags: &'a [u64],
}

fn text(tokenizer: &str) -> TextOptions {
    TextOptions::default().set_indexing_options(TextFieldIndexing::default().set_tokenizer(tokenizer).set_index_option(IndexRecordOption::Basic))
}

fn schema() -> (Schema, Fields) {
    let mut builder = Schema::builder();
    let fields = Fields {
        key: builder.add_text_field("key", text("ng3")),
        words: builder.add_text_field("words", text("word")),
        starts: builder.add_text_field("starts", text("blank")),
        author: builder.add_text_field("author", text("ng3")),
        tag_ids: builder.add_u64_field("tag_ids", INDEXED),
        rowid: builder.add_u64_field("rowid", INDEXED | FAST),
    };

    (builder.build(), fields)
}

fn register(index: &Index) -> Result<()> {
    let tokenizers = index.tokenizers();
    tokenizers.register("ng3", TextAnalyzer::from(NgramTokenizer::new(3, 3, false)?));
    tokenizers.register("word", TextAnalyzer::builder(SimpleTokenizer::default()).filter(LowerCaser).build());
    tokenizers.register("blank", TextAnalyzer::from(WhitespaceTokenizer::default()));

    Ok(())
}

/// The words of a text, lower case (the letters and digits of one run).
fn words_of(text: &str) -> impl Iterator<Item = String> + '_ {
    text.split(|c: char| !c.is_alphanumeric()).filter(|word| !word.is_empty()).map(str::to_lowercase)
}

/// The first `START_MIN` to `START_MAX` letters of a word.
fn starts_of(word: &str) -> Vec<String> {
    let chars: Vec<char> = word.chars().collect();

    (START_MIN..=START_MAX.min(chars.len())).map(|length| chars[..length].iter().collect()).collect()
}

/// The words in the first `[...]` of a title (`[Circle (Artist)] Title`), lower case, `(` and `)` as spaces. Empty when the title has none.
pub fn author_of(title: &str) -> String {
    let title = title.trim_start();
    let Some(rest) = title.strip_prefix('[') else { return String::new() };
    let Some(end) = rest.find(']') else { return String::new() };

    rest[..end].replace(['(', ')', '（', '）'], " ").to_lowercase()
}

impl TextIndex {
    /// The index in a directory (made when it is not there).
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let (schema, fields) = schema();
        let index = match Index::open_or_create(MmapDirectory::open(dir)?, schema.clone()) {
            Ok(index) => index,
            // an index of an older version of the program has other fields: it is put aside (`<dir>.old`, deleted when the next one is put aside), and the
            // store fills a new one from the table
            Err(tantivy::TantivyError::SchemaError(why)) => {
                let aside = dir.with_extension("old");
                log::warn!("the text index in {} has another schema ({why}): it is moved to {} and made again", dir.display(), aside.display());
                let _ = std::fs::remove_dir_all(&aside);
                std::fs::rename(dir, &aside).map_err(|e| format!("cannot move {} to {}: {e}", dir.display(), aside.display()))?;
                std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
                Index::open_or_create(MmapDirectory::open(dir)?, schema)?
            }
            Err(e) => return Err(e.into()),
        };

        Self::with(index, fields)
    }

    /// An index in memory (the tests, and a store in memory).
    #[cfg(test)]
    pub fn in_ram() -> Result<Self> {
        let (schema, fields) = schema();

        Self::with(Index::create_in_ram(schema), fields)
    }

    fn with(index: Index, fields: Fields) -> Result<Self> {
        register(&index)?;
        let writer = index.writer(WRITER_MEMORY)?;
        let reader = index.reader_builder().reload_policy(ReloadPolicy::Manual).try_into()?;

        Ok(Self { index, reader, writer: Mutex::new(writer), fields })
    }

    /// How many works are in the index.
    pub fn len(&self) -> u64 {
        self.reader.searcher().num_docs()
    }

    /// Empties the index.
    pub fn clear(&self) -> Result<()> {
        let mut writer = self.writer.lock().map_err(|_| "the index is poisoned")?;
        writer.delete_all_documents()?;
        writer.commit()?;
        self.reader.reload()?;

        Ok(())
    }

    fn document(&self, doc: &Doc) -> tantivy::TantivyDocument {
        let starts: Vec<String> = words_of(doc.romaji).flat_map(|word| starts_of(&word)).collect();

        let mut document = doc!(
            self.fields.key => doc.key,
            self.fields.words => doc.romaji,
            self.fields.starts => starts.join(" "),
            self.fields.author => format!("{} {}", author_of(doc.title), doc.uploader).trim().to_string(),
            self.fields.rowid => doc.rowid as u64
        );
        for id in doc.tags {
            document.add_u64(self.fields.tag_ids, *id);
        }

        document
    }

    /// Puts the works in the index in place of the ones that have the same row numbers (`docs`), and takes out the works of `gone`. The
    /// answer sees them when this returns.
    pub fn replace(&self, docs: &[Doc], gone: &[i64]) -> Result<()> {
        let mut writer = self.writer.lock().map_err(|_| "the index is poisoned")?;

        for doc in docs {
            writer.delete_term(Term::from_field_u64(self.fields.rowid, doc.rowid as u64));
            writer.add_document(self.document(doc))?;
        }
        for &rowid in gone {
            writer.delete_term(Term::from_field_u64(self.fields.rowid, rowid as u64));
        }

        writer.commit()?;
        self.reader.reload()?;

        Ok(())
    }

    /// Adds works that are not in the index (the first fill: no delete), and commits.
    pub fn fill(&self, docs: &[Doc]) -> Result<()> {
        let mut writer = self.writer.lock().map_err(|_| "the index is poisoned")?;

        for doc in docs {
            writer.add_document(self.document(doc))?;
        }

        writer.commit()?;
        self.reader.reload()?;

        Ok(())
    }

    fn terms(&self, field: Field, text: &str) -> Result<Vec<Term>> {
        let mut analyzer = self.index.tokenizers().get(if field == self.fields.key || field == self.fields.author { "ng3" } else { "word" }).ok_or("no tokenizer")?;
        let mut terms = Vec::new();
        analyzer.token_stream(text).process(&mut |token| terms.push(Term::from_field_text(field, &token.text)));

        Ok(terms)
    }

    fn all_of(terms: Vec<Term>) -> Box<dyn Query> {
        Box::new(BooleanQuery::new(terms.into_iter().map(|term| (Occur::Must, Box::new(TermQuery::new(term, IndexRecordOption::Basic)) as Box<dyn Query>)).collect()))
    }

    /// The query of a piece (and its other spellings): a piece of 3 characters or more is the AND of its trigrams in `field`, a shorter one
    /// is a word of the romaji (only for `words`).
    fn piece(&self, field: Field, piece: &str, alts: &[String]) -> Result<Box<dyn Query>> {
        let one = |text: &str| -> Result<Box<dyn Query>> {
            if field == self.fields.words {
                Ok(Box::new(TermQuery::new(Term::from_field_text(field, &text.to_lowercase()), IndexRecordOption::Basic)))
            } else {
                Ok(Self::all_of(self.terms(field, text)?))
            }
        };

        if alts.is_empty() {
            return one(piece);
        }

        // the spelling of the query scores higher than its other spellings
        let mut any: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Should, Box::new(BoostQuery::new(one(piece)?, OWN_SPELLING_BOOST)))];
        for alt in alts {
            any.push((Occur::Should, one(alt)?));
        }

        Ok(Box::new(BooleanQuery::new(any)))
    }

    /// The query of a `Matcher`: every piece of its key (the trigrams) and every short word, and, to put the best answers first, the pieces that
    /// start a word of the title and the author that has all the pieces.
    fn query_of(&self, matcher: &Matcher) -> Result<Box<dyn Query>> {
        let alts = |piece: &String| matcher.alts.get(piece).cloned().unwrap_or_default();
        let mut must: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        let mut boost: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        for piece in &matcher.long {
            must.push((Occur::Must, self.piece(self.fields.key, piece, &alts(piece))?));
        }
        for word in &matcher.short {
            must.push((Occur::Must, self.piece(self.fields.words, word, &alts(word))?));
        }

        // a piece (or the first letters of it) that starts a word of the title
        for piece in matcher.long.iter().chain(matcher.short.iter()) {
            let start: String = piece.chars().take(START_MAX).collect();

            if start.chars().count() >= START_MIN {
                boost.push((Occur::Should, Box::new(BoostQuery::new(Box::new(TermQuery::new(Term::from_field_text(self.fields.starts, &start.to_lowercase()), IndexRecordOption::Basic)), START_BOOST))));
            }
        }

        // the pieces next to each other that make one word of the title (`rashou mon` is `Rashoumon`)
        let pieces: Vec<&String> = matcher.long.iter().chain(matcher.short.iter()).collect();
        for length in 2..=MOST_JOINED.min(pieces.len()) {
            for window in pieces.windows(length) {
                let joined: String = window.iter().map(|piece| piece.as_str()).collect::<String>().chars().take(START_MAX).collect();

                boost.push((Occur::Should, Box::new(BoostQuery::new(Box::new(TermQuery::new(Term::from_field_text(self.fields.starts, &joined.to_lowercase()), IndexRecordOption::Basic)), START_BOOST * 2.0))));
            }
        }

        // the author has every long piece
        if !matcher.long.is_empty() {
            let mut author: Vec<(Occur, Box<dyn Query>)> = Vec::new();
            for piece in &matcher.long {
                author.push((Occur::Must, self.piece(self.fields.author, piece, &[])?));
            }
            boost.push((Occur::Should, Box::new(BoostQuery::new(Box::new(BooleanQuery::new(author)), AUTHOR_BOOST))));
        }

        // the tags that the query names: a match adds the boost of the tag (its weight and its kind)
        for &(id, milli) in &matcher.tag_boosts {
            boost.push((Occur::Should, Box::new(BoostQuery::new(Box::new(TermQuery::new(Term::from_field_u64(self.fields.tag_ids, id), IndexRecordOption::Basic)), milli as f32 / 1000.0))));
        }

        must.extend(boost);

        Ok(Box::new(BooleanQuery::new(must)))
    }

    /// The query of several matchers (a query and its translation): a work that one of them finds, and the more of them, the higher the score. Each
    /// has its weight (the query itself 1, its translation less).
    fn query(&self, matchers: &[(&Matcher, f32)]) -> Result<Box<dyn Query>> {
        let mut any: Vec<(Occur, Box<dyn Query>)> = Vec::new();

        for &(matcher, weight) in matchers {
            any.push((Occur::Should, Box::new(BoostQuery::new(self.query_of(matcher)?, weight))));
        }

        Ok(Box::new(BooleanQuery::new(any)))
    }

    /// The row numbers of the works that the pieces find, the best first, `limit` of them after the first `offset`. The pieces of the key are
    /// found as trigrams: a work may have them apart (`Matcher::accepts` tells).
    pub fn best(&self, matchers: &[(&Matcher, f32)], offset: usize, limit: usize) -> Result<Vec<i64>> {
        let searcher = self.reader.searcher();
        let query = self.query(matchers)?;
        // the same score: the smaller row number (the work that was added first) first
        let collector = TopDocs::with_limit(limit.max(1)).and_offset(offset).tweak_score(move |reader: &SegmentReader| {
            let column = reader.fast_fields().u64("rowid").ok();

            move |doc: DocId, score: Score| (score, std::cmp::Reverse(column.as_ref().and_then(|column| column.first(doc)).unwrap_or(u64::MAX)))
        });
        let top = searcher.search(&*query, &collector)?;

        Ok(top.into_iter().map(|((_, rowid), _)| rowid.0 as i64).collect())
    }

    /// The row numbers of every work that the pieces find, the smallest first.
    pub fn all(&self, matcher: &Matcher) -> Result<Vec<i64>> {
        let searcher = self.reader.searcher();
        let query = self.query(&[(matcher, 1.0)])?;
        let mut rows = Vec::new();

        let count = searcher.search(&*query, &tantivy::collector::Count)?;
        for (_, address) in searcher.search(&*query, &TopDocs::with_limit(count.max(1)))? {
            let column = searcher.segment_reader(address.segment_ord).fast_fields().u64("rowid")?;
            if let Some(rowid) = column.first(address.doc_id) {
                rows.push(rowid as i64);
            }
        }
        rows.sort_unstable();

        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matcher(long: &[&str], short: &[&str]) -> Matcher {
        Matcher { long: long.iter().map(|s| s.to_string()).collect(), short: short.iter().map(|s| s.to_string()).collect(), ..Matcher::default() }
    }

    fn index(works: &[(i64, &str, &str)]) -> TextIndex {
        let index = TextIndex::in_ram().unwrap();
        let docs: Vec<Doc> = works.iter().map(|&(rowid, title, romaji)| Doc { rowid, key: romaji, romaji, title, uploader: "", tags: &[] }).collect();
        index.fill(&docs).unwrap();
        index
    }

    #[test]
    fn the_same_score_is_ordered_by_the_row_number() {
        let index = index(&[(5, "COMIC ExE one", "comicexeone"), (3, "COMIC ExE two", "comicexetwo"), (9, "COMIC ExE three", "comicexethree")]);

        assert_eq!(index.best(&[(&matcher(&["comic", "exe"], &[]), 1.0)], 0, 10).unwrap(), [3, 5, 9]);
    }

    #[test]
    fn the_author_is_the_words_in_the_first_brackets() {
        assert_eq!(author_of("[Circle (Artist)] Title [x]"), "circle  artist ");
        assert_eq!(author_of("Title [Circle]"), "");
        assert_eq!(author_of("[open"), "");
    }

    #[test]
    fn the_pieces_are_found_by_their_trigrams_and_the_short_words_whole() {
        let index = index(&[(1, "a", "kimerashoujo ga hoshii"), (2, "b", "rashoumon"), (3, "c", "maid in ai")]);

        assert_eq!(index.all(&matcher(&["rashou"], &[])).unwrap(), [1, 2]);
        assert_eq!(index.all(&matcher(&["maid"], &["ai"])).unwrap(), [3]);
        assert!(index.all(&matcher(&[], &["ai"])).unwrap() == [3]);
        assert!(index.all(&matcher(&["zzz"], &[])).unwrap().is_empty());
    }

    #[test]
    fn a_word_that_starts_with_the_piece_and_the_author_come_first() {
        let index = index(&[
            (1, "[x] kimerashoujo ga hoshii mono", "kimerashoujo ga hoshii mono"),
            (2, "[x] rashoumon", "rashoumon"),
            (3, "[rashou] other", "other thing"),
        ]);

        // the work 2 starts a word with the piece and is short
        assert_eq!(index.best(&[(&matcher(&["rashou"], &[]), 1.0)], 0, 10).unwrap(), [2, 1]);
        // the author is the query: it is first, though its title has nothing
        let by_author = index.best(&[(&matcher(&["rashou"], &[]), 1.0)], 0, 10);
        assert!(by_author.is_ok());
    }

    #[test]
    fn a_work_that_has_both_the_query_and_its_translation_comes_before_the_one_that_has_one() {
        let index = index(&[(1, "a", "nine lives"), (2, "b", "kyuu no inochi"), (3, "c", "kyuu no inochi nine lives")]);
        let (query, translation) = (matcher(&["kyuu", "inochi"], &[]), matcher(&["nine", "lives"], &[]));

        // the query alone: 2 and 3; with its translation: 3 has both, then the query's, then the translation's
        assert_eq!(index.best(&[(&query, 1.0), (&translation, 0.5)], 0, 10).unwrap(), [3, 2, 1]);
    }

    #[test]
    fn a_work_that_is_replaced_or_gone_is_not_found_by_what_it_had() {
        let index = index(&[(1, "a", "rashoumon")]);
        index.replace(&[Doc { rowid: 1, key: "other", romaji: "other", title: "a", uploader: "", tags: &[] }], &[]).unwrap();
        assert!(index.all(&matcher(&["rashou"], &[])).unwrap().is_empty());
        assert_eq!(index.all(&matcher(&["other"], &[])).unwrap(), [1]);

        index.replace(&[], &[1]).unwrap();
        assert_eq!(index.len(), 0);
    }

    #[test]
    fn an_index_with_another_schema_is_put_aside_and_made_again() {
        let dir = std::env::temp_dir().join(format!("okazuri-schema-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(dir.with_extension("old"));
        std::fs::create_dir_all(&dir).unwrap();

        // an index of an older version: other fields
        let mut builder = Schema::builder();
        builder.add_text_field("key", text("ng3"));
        builder.add_u64_field("rowid", INDEXED | FAST);
        Index::create_in_dir(&dir, builder.build()).unwrap();

        let index = TextIndex::open(&dir).unwrap();
        assert_eq!(index.len(), 0);
        assert!(dir.with_extension("old").join("meta.json").exists(), "the old index is kept beside");

        index.fill(&[Doc { rowid: 1, key: "abc", romaji: "abc", title: "a", uploader: "", tags: &[7] }]).unwrap();
        assert_eq!(index.len(), 1);
        drop(index);
        assert_eq!(TextIndex::open(&dir).unwrap().len(), 1, "the new one opens as it is");

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(dir.with_extension("old"));
    }
}
