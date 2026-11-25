//! The search database (`[search] db_path`): the works the sites listed, and when each query was asked to
//! each site. Made and written by the engine. The tables are the entities `work` and `search`.
//!
//! A work is what a list page shows of it: the title as the site writes it, and the path of its page.
//! No image, and no address of an image.
//!
//! (The other SQLite file, the copy of D1, is `mirror.rs`.)

use crate::entity::{crawl_gap, crawl_growth, crawl_probe, crawl_range, crawl_read, crawl_state, crawl_watch, listing, search, tag, translation, translation_lookup, uploader, work};
use crate::language;
use crate::query::{outside_key, title_key};
use crate::sites::toggle_slash;
use crate::textindex::{Doc, TextIndex};
use crate::utils::Result;
use sea_orm::sea_query::{Expr, Index, OnConflict};
use sea_orm::sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, IntoActiveModel, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, Schema, Set, SqlxSqliteConnector, Statement, TransactionTrait,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What is known of one search (a query on a site).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteState {
    pub searched_at: i64,
    /// False when the last try failed (the works of an earlier try are still saved).
    pub ok: bool,
    pub error: Option<String>,
    pub count: usize,
}

impl From<search::Model> for SiteState {
    fn from(row: search::Model) -> Self {
        Self { searched_at: row.searched_at, ok: row.ok, error: row.error, count: row.count.max(0) as usize }
    }
}

/// A work to save: its path, its title as the site writes it, and the romaji of that title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewWork {
    pub path: String,
    /// The title without its flags (`language::strip_flags`).
    pub title: String,
    pub romaji: String,
    /// The language, read from the title *with* its flags (`language::detect`).
    pub language: Option<String>,
    /// The tags, the uploader and the posted date the list page shows (empty when it shows none).
    pub extra: crate::sites::Extra,
}

/// A saved work.
pub type Work = work::Model;

/// When a page of a list was read by the first walk, as the earliest and the latest time it may have been (unix ms, the clock of the host), and the job
/// that read it (the pages of a job are read in order, from the top).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRead {
    pub page: u64,
    pub job: i64,
    pub lo_ms: i64,
    pub hi_ms: i64,
}

/// A boundary of two neighbouring pages (`k` and `k + 1`) that were read by different jobs, the page after it possibly before it: the latest time page `k` may
/// have been read, and the earliest time page `k + 1` may have been.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Boundary {
    pub k: u64,
    pub a_hi: i64,
    pub b_lo: i64,
}

/// A look at the top of a list that saw works added: between `since` and `at` (unix ms), `added` works came (`saturated`: at least that many).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sample {
    pub since: i64,
    pub at: i64,
    pub added: i64,
    pub saturated: bool,
}

/// The most rows `find` reads: a word that is in nearly every title would read the whole table.
const MAX_CANDIDATES: u64 = 50_000;

/// How many connections read the database for the searches.
const READERS: u32 = 4;


/// The version of the keys and the indexes (`PRAGMA user_version`).
const INDEX_VERSION: i64 = 2;

/// How many works are put in the text index at a time.
const INDEX_FILL_CHUNK: i64 = 50_000;

/// The text index is committed after about this many works of a fill.
const FILL_COMMIT_EVERY: u64 = 1_000_000;

/// The candidates of the words asked lately (`SearchStore::candidates`), the oldest first.
/// A work the index found, with its row number.
#[derive(Debug, sea_orm::FromQueryResult)]
struct RawRow {
    r: i64,
    website: String,
    path: String,
    title: String,
    title_romaji: String,
    language: Option<String>,
    tags: Option<String>,
    uploader_romaji: Option<String>,
}

/// A work the index found, with the romaji of its tags and uploader.
#[derive(Debug)]
struct IndexedRow {
    r: i64,
    website: String,
    path: String,
    title: String,
    title_romaji: String,
    language: Option<String>,
    /// The romaji of its tags and of its uploader, with a space between.
    extra: String,
}

impl IndexedRow {
    /// What the search matches (`index_key`).
    fn key(&self) -> String {
        index_key(&self.title_romaji, &self.extra)
    }
}

/// What a work is found by: the key of what is outside the parentheses of its romaji (`query::outside_key`), and, after a `|`, the key of its
/// tags and uploader (a query piece may be in any of them).
fn index_key(romaji: &str, extra: &str) -> String {
    let mut key = outside_key(romaji);

    if !extra.trim().is_empty() {
        key.push('|');
        key.push_str(&title_key(extra));
    }

    key
}

/// The ids in the `tags` of a work (` 12 45 `).
fn tag_ids_of(tags: &str) -> impl Iterator<Item = i64> + '_ {
    tags.split_whitespace().filter_map(|id| id.parse().ok())
}

/// A work a search reads, as it is kept: the site is a number (a search of 700,000 works does not hash 700,000 names), and the one thing that
/// every row is asked is kept beside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The place of the name of its site in `Candidates::websites`.
    pub site: u16,
    pub path: String,
    pub title: String,
    pub title_romaji: String,
    /// The key of what is outside the parentheses of the romaji (`query::outside_key`): what the search narrows with and matches.
    pub title_key: String,
    pub language: Option<String>,
}

/// The candidates of a word, newest first, and the names of their sites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidates {
    pub works: Vec<Candidate>,
    pub websites: Vec<String>,
}

impl std::ops::Deref for Candidates {
    type Target = Vec<Candidate>;

    fn deref(&self) -> &Vec<Candidate> {
        &self.works
    }
}

pub struct SearchStore {
    db: DatabaseConnection,
    /// Read-only connections, several: a search does not wait for a write (or for the long read of the verification) of the one connection
    /// of `db` (the database is in WAL mode, so a read does not block a write either). The same connection as `db` for a database in memory.
    reader: DatabaseConnection,
    /// The most rows one search of the database reads (`[search] max_candidates`).
    max_candidates: std::sync::atomic::AtomicU64,
    /// The text index of the titles (`textindex.rs`), in the directory beside the database, and kept up to date from `index_queue`.
    text: Option<TextIndex>,
    /// One task at a time brings the index up to date.
    sync: tokio::sync::Mutex<()>,
    /// The romaji of the tags (`tags.romaji`) by id, kept in memory: the tags are some tens of thousands, and every search reads them.
    tag_romaji: std::sync::Mutex<HashMap<i64, String>>,
    /// The tags by their key (what a query piece is compared with), with the boost of each in thousandths, and when they were read.
    tag_keys: std::sync::Mutex<Option<(Instant, HashMap<String, Vec<(u64, u32)>>)>>,
    /// The boost of a match by the kind of the tag (`[search] tag_boost`).
    kind_boost: std::sync::Mutex<[f64; 4]>,
    /// The text index is kept (false when the environment says `OKAZURI_TEXT_INDEX=off`: a machine that only crawls does not answer searches, so it neither fills
    /// the index at start nor takes in the queue; the works are exported to the server that answers).
    index_on: bool,
}

/// How long the tags kept in memory for the boosts are used before they are read again.
const TAG_KEYS_FOR: Duration = Duration::from_secs(60);

/// How much a match of a tag adds to the score, by kind (`TagKind`: a tag, an author, a circle, a weak tag), before the weight of the tag.
pub const KIND_BOOST: [f64; 4] = [1.0, 6.0, 4.0, 0.25];

impl SearchStore {
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent).await.map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }

        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            // a reader (the CLI, a backup) does not block the writer
            .journal_mode(SqliteJournalMode::Wal)
            .busy_timeout(Duration::from_secs(10));

        let readers = SqlitePoolOptions::new()
            .max_connections(READERS)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_lazy_with(options.clone().create_if_missing(false).read_only(true));

        let dir = path.with_file_name(format!("{}.index", path.file_name().map_or("search.db".into(), |name| name.to_string_lossy().into_owned())));

        // a process that does not keep the text index (`OKAZURI_TEXT_INDEX=off`: a crawl, an import) does not open the directory of it either (the server
        // that answers searches on the same database holds the lock of its writer)
        let kept = std::env::var("OKAZURI_TEXT_INDEX").map_or(true, |value| value != "off");
        let text = if kept { Some(TextIndex::open(&dir)?) } else { None };

        Self::init(options, Some(readers), text, kept.then_some(dir)).await.map_err(|e| format!("cannot open {}: {e}", path.display()).into())
    }

    #[cfg(test)]
    pub async fn in_memory() -> Result<Self> {
        Self::init(SqliteConnectOptions::new().in_memory(true), None, Some(TextIndex::in_ram()?), None).await
    }

    async fn init(options: SqliteConnectOptions, readers: Option<sea_orm::sqlx::SqlitePool>, text: Option<TextIndex>, dir: Option<std::path::PathBuf>) -> Result<Self> {
        // one connection: the writes go one after the other, as the engine's tasks come
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await?;
        let db = SqlxSqliteConnector::from_sqlx_sqlite_pool(pool);

        Self::create_tables(&db).await?;

        let reader = match readers {
            Some(pool) => SqlxSqliteConnector::from_sqlx_sqlite_pool(pool),
            None => db.clone(),
        };

        let text_kept = text.is_some();
        let store = Self { db, reader, max_candidates: std::sync::atomic::AtomicU64::new(MAX_CANDIDATES), text, sync: tokio::sync::Mutex::new(()), tag_romaji: Default::default(), tag_keys: Default::default(), kind_boost: std::sync::Mutex::new(KIND_BOOST), index_on: text_kept };
        store.fill_text_index(dir.as_deref()).await?;

        Ok(store)
    }

    async fn create_tables(db: &DatabaseConnection) -> Result<()> {
        let backend = db.get_database_backend();
        let schema = Schema::new(backend);

        let mut works = schema.create_table_from_entity(work::Entity);
        let mut searches = schema.create_table_from_entity(search::Entity);
        let mut listings = schema.create_table_from_entity(listing::Entity);
        let mut translation_lookups = schema.create_table_from_entity(translation_lookup::Entity);
        let mut translations = schema.create_table_from_entity(translation::Entity);
        let mut tags = schema.create_table_from_entity(tag::Entity);
        let mut uploaders = schema.create_table_from_entity(uploader::Entity);
        db.execute(backend.build(works.if_not_exists())).await?;
        db.execute(backend.build(tags.if_not_exists())).await?;
        db.execute_unprepared("CREATE UNIQUE INDEX IF NOT EXISTS tags_kind_name ON tags (kind, name)").await?;
        db.execute(backend.build(uploaders.if_not_exists())).await?;
        // the names of the numbers a site gives its tags (AsmHentai, HentaiEra): only the ones that works carry are kept
        // the pages of a list that the first walk of a site has read, as ranges (`lo`..=`hi`): the pages are read in any order, by any worker
        db.execute_unprepared("CREATE TABLE IF NOT EXISTS crawl_ranges (website TEXT NOT NULL, lo INTEGER NOT NULL, hi INTEGER NOT NULL, PRIMARY KEY (website, lo))").await?;
        // the gap check (`crawl.rs`): when each page was read, the look at the top of the list, the growth seen there, the pages to read again, and the ids looked at
        db.execute_unprepared("CREATE TABLE IF NOT EXISTS crawl_reads (website TEXT NOT NULL, page INTEGER NOT NULL, job INTEGER NOT NULL, lo_ms INTEGER NOT NULL, hi_ms INTEGER NOT NULL, PRIMARY KEY (website, page))").await?;
        db.execute_unprepared("CREATE TABLE IF NOT EXISTS crawl_watch (website TEXT NOT NULL PRIMARY KEY, since0 INTEGER NOT NULL, at INTEGER NOT NULL, top TEXT NOT NULL, per_page INTEGER NOT NULL)").await?;
        db.execute_unprepared("CREATE TABLE IF NOT EXISTS crawl_growth (website TEXT NOT NULL, since INTEGER NOT NULL, at INTEGER NOT NULL, added INTEGER NOT NULL, saturated INTEGER NOT NULL, PRIMARY KEY (website, at))").await?;
        db.execute_unprepared("CREATE TABLE IF NOT EXISTS crawl_gap_plan (website TEXT NOT NULL, lo INTEGER NOT NULL, hi INTEGER NOT NULL, done_at INTEGER, tries INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (website, lo))").await?;
        db.execute_unprepared("CREATE TABLE IF NOT EXISTS crawl_probes (website TEXT NOT NULL, id INTEGER NOT NULL, checked_at INTEGER NOT NULL, found INTEGER NOT NULL, PRIMARY KEY (website, id))").await?;
        db.execute_unprepared("CREATE TABLE IF NOT EXISTS site_tag_names (website TEXT NOT NULL, attribute TEXT NOT NULL, id INTEGER NOT NULL, name TEXT NOT NULL, PRIMARY KEY (website, attribute, id))").await?;
        let mut crawls = schema.create_table_from_entity(crawl_state::Entity);
        db.execute(backend.build(searches.if_not_exists())).await?;
        db.execute(backend.build(crawls.if_not_exists())).await?;
        db.execute(backend.build(listings.if_not_exists())).await?;
        db.execute(backend.build(translation_lookups.if_not_exists())).await?;
        db.execute(backend.build(translations.if_not_exists())).await?;

        let by_update = Index::create()
            .name("works_updated_at")
            .table(work::Entity)
            .col(work::Column::UpdatedAt)
            .if_not_exists()
            .to_owned();
        db.execute(backend.build(&by_update)).await?;

        // the English title of a result is looked up by the Japanese key of its title (`english_titles`)
        let by_japanese = Index::create()
            .name("translations_japanese_key")
            .table(translation::Entity)
            .col(translation::Column::JapaneseKey)
            .if_not_exists()
            .to_owned();
        db.execute(backend.build(&by_japanese)).await?;

        Self::create_word_index(db).await
    }

    /// The two indexes of the titles, made again when the database has the old ones (`user_version` 0): the key of a work is then the key of
    /// what is outside its parentheses (`query::outside_key`), as the search matches it, and both indexes have the name of the site.
    async fn create_word_index(db: &DatabaseConnection) -> Result<()> {
        let backend = db.get_database_backend();
        let version: i64 = db
            .query_one(Statement::from_string(backend, "PRAGMA user_version"))
            .await?
            .ok_or("no version")?
            .try_get("", "user_version")?;

        if version < INDEX_VERSION {
            log::info!("the search database is changed to version {INDEX_VERSION}: the FTS5 indexes are dropped");
            for sql in [
                "DROP TRIGGER IF EXISTS works_fts_insert",
                "DROP TRIGGER IF EXISTS works_fts_delete",
                "DROP TRIGGER IF EXISTS works_fts_update",
                "DROP TABLE IF EXISTS works_fts",
                "DROP TRIGGER IF EXISTS works_words_insert",
                "DROP TRIGGER IF EXISTS works_words_delete",
                "DROP TRIGGER IF EXISTS works_words_update",
                "DROP TABLE IF EXISTS works_words",
            ] {
                db.execute_unprepared(sql).await?;
            }

        }

        // the key of a title is made from its romaji when it is needed (`query::outside_key`): the column that kept it is dropped (the table is written
        // again, which takes a few minutes for ten million works)
        let has_key = db
            .query_one(Statement::from_string(db.get_database_backend(), "SELECT 1 AS x FROM pragma_table_info('works') WHERE name = 'title_key'"))
            .await?
            .is_some();

        if has_key {
            log::info!("the column title_key is dropped from the works");
            db.execute_unprepared("DROP TRIGGER IF EXISTS works_queue_update").await?;
            db.execute_unprepared("ALTER TABLE works DROP COLUMN title_key").await?;
        }

        // the columns of what a list page shows besides the title (tags, uploader, posted date) are added to a database that has not got them
        for (column, kind) in [("tags", "TEXT"), ("uploader", "TEXT"), ("posted_at", "INTEGER")] {
            let has = db
                .query_one(Statement::from_sql_and_values(db.get_database_backend(), "SELECT 1 AS x FROM pragma_table_info('works') WHERE name = ?", [column.into()]))
                .await?
                .is_some();

            if !has {
                db.execute_unprepared(&format!("ALTER TABLE works ADD COLUMN {column} {kind}")).await?;
            }
        }
        db.execute_unprepared("DROP TRIGGER IF EXISTS works_queue_update").await?;

        // the text index is a directory beside the database (`textindex.rs`): the changes of the works are queued by three triggers, and the
        // index takes them in before it answers (`sync_index`). The FTS5 indexes of the older versions are not kept up to date any more.
        for sql in [
            "DROP TRIGGER IF EXISTS works_fts_insert",
            "DROP TRIGGER IF EXISTS works_fts_delete",
            "DROP TRIGGER IF EXISTS works_fts_update",
            "DROP TRIGGER IF EXISTS works_words_insert",
            "DROP TRIGGER IF EXISTS works_words_delete",
            "DROP TRIGGER IF EXISTS works_words_update",
            "CREATE TABLE IF NOT EXISTS index_queue (seq INTEGER PRIMARY KEY AUTOINCREMENT, row INTEGER NOT NULL)",
            "CREATE TRIGGER IF NOT EXISTS works_queue_insert AFTER INSERT ON works BEGIN INSERT INTO index_queue(row) VALUES (new.rowid); END",
            "CREATE TRIGGER IF NOT EXISTS works_queue_delete AFTER DELETE ON works BEGIN INSERT INTO index_queue(row) VALUES (old.rowid); END",
            "CREATE TRIGGER IF NOT EXISTS works_queue_update AFTER UPDATE OF title, title_romaji, tags, uploader ON works BEGIN INSERT INTO index_queue(row) VALUES (new.rowid); END",
        ] {
            db.execute_unprepared(sql).await?;
        }

        // (after the triggers: the index takes the changed rows in) DLsite has no uploader: what was saved as one is the circle, which is a tag of the work (removed once the databases are migrated)
        db.execute_unprepared("UPDATE works SET uploader = NULL WHERE website = 'DLsite' AND uploader IS NOT NULL").await?;
        db.execute_unprepared("DELETE FROM uploaders WHERE website = 'DLsite'").await?;

        db.execute_unprepared(&format!("PRAGMA user_version = {INDEX_VERSION}")).await?;

        Ok(())
    }

    /// The names that an old database has the FTS5 indexes by: they are dropped by `drop_legacy_index`.
    pub async fn drop_legacy_index(&self) -> Result<()> {
        for sql in ["DROP TABLE IF EXISTS works_fts", "DROP TABLE IF EXISTS works_words"] {
            self.db.execute_unprepared(sql).await?;
        }

        Ok(())
    }

    /// Fills the text index from the table when it is not complete (it has not the marker file, or has not as many works as the table) and takes
    /// in the changes that were queued: it takes a few minutes for ten million works, and a search waits for it.
    /// The text index, which a process with `OKAZURI_TEXT_INDEX=off` does not have (it does not answer searches).
    fn text(&self) -> Result<&TextIndex> {
        self.text.as_ref().ok_or_else(|| "the text index is off (OKAZURI_TEXT_INDEX=off)".into())
    }

    async fn fill_text_index(&self, dir: Option<&Path>) -> Result<()> {
        if !self.index_on {
            log::info!("OKAZURI_TEXT_INDEX=off: the text index is not kept (this process crawls, it does not answer searches)");
            return Ok(());
        }
        let marker = dir.map(|dir| dir.join("filled"));
        self.sync_index().await?;
        let works = self.count_works().await?;
        let complete = marker.as_ref().is_none_or(|marker| marker.exists()) && self.text()?.len() == works;

        if complete {
            return Ok(());
        }

        log::info!("the text index has {} works, the table {works}: the index is filled again", self.text()?.len());
        let _guard = self.sync.lock().await;

        if let Some(marker) = &marker {
            let _ = std::fs::remove_file(marker);
        }

        let started = Instant::now();
        let last: Option<i64> = self.db.query_one(Statement::from_string(self.db.get_database_backend(), "SELECT max(seq) AS m FROM index_queue")).await?.and_then(|row| row.try_get("", "m").ok());
        self.text()?.clear()?;
        let (mut after, mut filled) = (0_i64, 0_u64);

        loop {
            let rows = self
                .db
                .query_all(Statement::from_sql_and_values(
                    self.db.get_database_backend(),
                    "SELECT w.rowid AS r, w.title, w.title_romaji, w.tags, u.romaji AS uploader_romaji FROM works w LEFT JOIN uploaders u ON u.website = w.website AND u.id = w.uploader WHERE w.rowid > ? ORDER BY w.rowid LIMIT ?",
                    [after.into(), INDEX_FILL_CHUNK.into()],
                ))
                .await?;
            let Some(end) = rows.last() else { break };
            after = end.try_get("", "r")?;

            let rows: Vec<(i64, String, String, Option<String>, Option<String>)> = rows
                .iter()
                .map(|row| Ok((row.try_get("", "r")?, row.try_get("", "title")?, row.try_get("", "title_romaji")?, row.try_get("", "tags")?, row.try_get("", "uploader_romaji")?)))
                .collect::<Result<_>>()?;
            let keys = self.keys_of(&rows).await?;
            let docs: Vec<Doc> = rows.iter().zip(&keys).map(|((rowid, title, romaji, _, _), (key, uploader, tags))| Doc { rowid: *rowid, key, romaji, title, uploader, tags }).collect();
            self.text()?.add(&docs)?;
            filled += docs.len() as u64;
            if filled % FILL_COMMIT_EVERY < INDEX_FILL_CHUNK as u64 {
                self.text()?.commit()?;
            }
        }
        self.text()?.commit()?;

        if let Some(last) = last {
            self.db.execute(Statement::from_sql_and_values(self.db.get_database_backend(), "DELETE FROM index_queue WHERE seq <= ?", [last.into()])).await?;
        }
        if let Some(marker) = &marker {
            std::fs::write(marker, b"")?;
        }
        log::info!("the text index is filled: {filled} works in {} s", started.elapsed().as_secs());

        Ok(())
    }

    /// The matcher with the tags it names (`Matcher::tag_boosts`): the tags whose key is a piece of the query, or some pieces next to each other.
    /// The tags are read from the table and kept in memory for a minute (a weight that is changed counts after that).
    async fn with_tag_boosts(&self, matcher: &crate::query::Matcher) -> Result<crate::query::Matcher> {
        let stale = self.tag_keys.lock().map_err(|_| "the tags are poisoned")?.as_ref().is_none_or(|(at, _)| at.elapsed() > TAG_KEYS_FOR);

        if stale {
            let rows = self.reader.query_all(Statement::from_string(self.reader.get_database_backend(), "SELECT id, kind, romaji, weight FROM tags")).await?;
            let mut keys: HashMap<String, Vec<(u64, u32)>> = HashMap::new();
            let kind_boost = *self.kind_boost.lock().map_err(|_| "the boosts are poisoned")?;

            for row in rows {
                let (id, kind, romaji, weight): (i64, i64, String, f64) = (row.try_get("", "id")?, row.try_get("", "kind")?, row.try_get("", "romaji")?, row.try_get("", "weight")?);
                let milli = (kind_boost.get(kind as usize).copied().unwrap_or(1.0) * weight * 1000.0).round().max(0.0) as u32;

                keys.entry(title_key(&romaji)).or_default().push((id as u64, milli));
            }
            *self.tag_keys.lock().map_err(|_| "the tags are poisoned")? = Some((Instant::now(), keys));
        }

        let guard = self.tag_keys.lock().map_err(|_| "the tags are poisoned")?;
        let Some((_, keys)) = guard.as_ref() else { return Ok(matcher.clone()) };
        let pieces: Vec<&String> = matcher.long.iter().chain(&matcher.short).collect();
        let mut boosted = matcher.clone();

        for length in 1..=crate::textindex::MOST_JOINED.min(pieces.len()) {
            for window in pieces.windows(length) {
                let joined: String = window.iter().map(|piece| piece.as_str()).collect();

                if let Some(found) = keys.get(&joined) {
                    boosted.tag_boosts.extend(found.iter().copied());
                }
            }
        }

        Ok(boosted)
    }

    /// The romaji of the tags with these ids, in the order of the ids: from memory, and the ones that are not there are read from the table.
    async fn tag_romaji_of(&self, wanted: &[i64]) -> Result<Vec<String>> {
        let missing: Vec<i64> = {
            let known = self.tag_romaji.lock().map_err(|_| "the tags are poisoned")?;
            let mut missing: Vec<i64> = wanted.iter().copied().filter(|id| !known.contains_key(id)).collect();
            missing.sort_unstable();
            missing.dedup();
            missing
        };

        for chunk in missing.chunks(20_000) {
            let rows = self
                .reader
                .query_all(Statement::from_sql_and_values(
                    self.reader.get_database_backend(),
                    "SELECT t.id AS id, t.romaji AS romaji FROM json_each(?) j JOIN tags t ON t.id = j.value",
                    [serde_json::to_string(chunk)?.into()],
                ))
                .await?;
            let mut known = self.tag_romaji.lock().map_err(|_| "the tags are poisoned")?;

            for row in rows {
                known.insert(row.try_get("", "id")?, row.try_get("", "romaji")?);
            }
            // an id that has no row (a tag that was removed) is a tag with no name
            for id in chunk {
                known.entry(*id).or_default();
            }
        }

        let known = self.tag_romaji.lock().map_err(|_| "the tags are poisoned")?;

        Ok(wanted.iter().map(|id| known.get(id).cloned().unwrap_or_default()).collect())
    }

    /// The text that the tags (by id) and the uploader add to a work: their romaji with a space between.
    async fn extras_of(&self, rows: &[(Option<&str>, Option<&str>)]) -> Result<Vec<String>> {
        let ids: Vec<i64> = rows.iter().flat_map(|(tags, _)| tags.into_iter().flat_map(|tags| tag_ids_of(tags))).collect();
        let _ = self.tag_romaji_of(&ids).await?;
        let known = self.tag_romaji.lock().map_err(|_| "the tags are poisoned")?;

        Ok(rows
            .iter()
            .map(|(tags, uploader)| {
                let mut text: Vec<&str> = tags.into_iter().flat_map(|tags| tag_ids_of(tags)).filter_map(|id| known.get(&id).map(String::as_str)).collect();
                text.extend(uploader);
                text.join(" ")
            })
            .collect())
    }

    /// What the index has of the works: their keys, and their uploaders (lower case) for the field of the authors.
    async fn keys_of(&self, rows: &[(i64, String, String, Option<String>, Option<String>)]) -> Result<Vec<(String, String, Vec<u64>)>> {
        let extras = self.extras_of(&rows.iter().map(|(_, _, _, tags, uploader)| (tags.as_deref(), uploader.as_deref())).collect::<Vec<_>>()).await?;

        Ok(rows
            .iter()
            .zip(extras)
            .map(|((_, _, romaji, tags, uploader), extra)| {
                (index_key(romaji, &extra), uploader.clone().unwrap_or_default().to_lowercase(), tags.as_deref().into_iter().flat_map(tag_ids_of).map(|id| id as u64).collect())
            })
            .collect())
    }

    pub async fn count_works(&self) -> Result<u64> {
        let row = self.db.query_one(Statement::from_string(self.db.get_database_backend(), "SELECT count(*) AS n FROM works")).await?.ok_or("no count")?;

        Ok(row.try_get::<i64>("", "n")?.max(0) as u64)
    }

    /// Takes the changes of the works that were queued since the last time into the text index (a search does this first, and it costs a
    /// look at an empty table when nothing was written).
    pub async fn sync_index(&self) -> Result<()> {
        if !self.index_on {
            return Ok(());
        }
        let pending = self.reader.query_one(Statement::from_string(self.reader.get_database_backend(), "SELECT 1 AS x FROM index_queue LIMIT 1")).await?;
        if pending.is_none() {
            return Ok(());
        }

        let _guard = self.sync.lock().await;
        let backend = self.db.get_database_backend();
        let Some(last) = self.db.query_one(Statement::from_string(backend, "SELECT max(seq) AS m FROM index_queue")).await?.and_then(|row| row.try_get::<i64>("", "m").ok()) else {
            return Ok(());
        };
        let mut after = 0_i64;

        loop {
            let queued = self
                .db
                .query_all(Statement::from_sql_and_values(
                    backend,
                    "SELECT DISTINCT row AS r FROM index_queue WHERE seq <= ? AND row > ? ORDER BY row LIMIT ?",
                    [last.into(), after.into(), INDEX_FILL_CHUNK.into()],
                ))
                .await?;
            let Some(end) = queued.last() else { break };
            after = end.try_get("", "r")?;
            let rowids: Vec<i64> = queued.iter().map(|row| row.try_get("", "r")).collect::<std::result::Result<_, _>>()?;

            let rows = self
                .db
                .query_all(Statement::from_sql_and_values(
                    backend,
                    "SELECT w.rowid AS r, w.title, w.title_romaji, w.tags, u.romaji AS uploader_romaji FROM json_each(?) j JOIN works w ON w.rowid = j.value LEFT JOIN uploaders u ON u.website = w.website AND u.id = w.uploader",
                    [serde_json::to_string(&rowids)?.into()],
                ))
                .await?;
            let rows: Vec<(i64, String, String, Option<String>, Option<String>)> = rows
                .iter()
                .map(|row| Ok((row.try_get("", "r")?, row.try_get("", "title")?, row.try_get("", "title_romaji")?, row.try_get("", "tags")?, row.try_get("", "uploader_romaji")?)))
                .collect::<Result<_>>()?;
            let found: HashSet<i64> = rows.iter().map(|row| row.0).collect();
            let gone: Vec<i64> = rowids.iter().copied().filter(|rowid| !found.contains(rowid)).collect();
            let keys = self.keys_of(&rows).await?;
            let docs: Vec<Doc> = rows.iter().zip(&keys).map(|((rowid, title, romaji, _, _), (key, uploader, tags))| Doc { rowid: *rowid, key, romaji, title, uploader, tags }).collect();
            self.text()?.replace(&docs, &gone)?;
        }

        self.db.execute(Statement::from_sql_and_values(backend, "DELETE FROM index_queue WHERE seq <= ?", [last.into()])).await?;

        Ok(())
    }

    /// The state of every site that has been asked the query.
    pub async fn states(&self, query: &str) -> Result<HashMap<String, SiteState>> {
        let rows = search::Entity::find().filter(search::Column::Query.eq(query)).all(&self.reader).await?;

        Ok(rows.into_iter().map(|row| (row.website.clone(), row.into())).collect())
    }

    /// Saves the answer of a site: the works that are new are added, a work whose title changed is updated, and the
    /// others are left as they are. Nothing is deleted before the whole answer is in hand: it is all written in one
    /// transaction, so a search sees the old works until the new ones are there.
    ///
    /// A work that the query listed the last time and does not list now is **gone** when `complete` (the list was not cut
    /// short, so the page is not there any more) and no other query lists it: it is removed. With a list that may be cut
    /// (it has as many works as the most that is kept) a work that is missing may be just beyond the cut, so it stays. A
    /// work that no query is known to have listed (it was saved by an older version) is never removed.
    ///
    /// The search is noted. A failure only notes the search. Returns the paths of the works that were new.
    pub async fn save(
        &self,
        query: &str,
        website: &str,
        now: i64,
        outcome: &std::result::Result<Vec<NewWork>, String>,
        complete: bool,
    ) -> Result<Vec<String>> {
        let transaction = self.db.begin().await?;
        let mut new = Vec::new();

        // a page that is saved already with the other form of its path (`/g/1/` and `/g/1`) is that page
        let same;
        let outcome = match outcome {
            Ok(listed) => {
                same = Ok(Self::same_pages(&transaction, website, listed).await?);
                &same
            }
            Err(_) => outcome,
        };

        match outcome {
            Ok(listed) => {
                new = Self::save_works(&transaction, website, now, listed).await?;

                let removed = Self::save_listing(&transaction, query, website, listed, complete).await?;
                if removed > 0 {
                    text_log!(info, "search {website}: {removed} works are not on the list of {query:?} any more, removed"; "search {website}: {removed} works are not on the list of a query any more, removed");
                }

                // the search worked: the count is the one of this try
                let noted = search::ActiveModel {
                    query: Set(query.to_string()),
                    website: Set(website.to_string()),
                    searched_at: Set(now),
                    ok: Set(true),
                    error: Set(None),
                    count: Set(listed.len() as i64),
                };
                search::Entity::insert(noted)
                    .on_conflict(
                        OnConflict::columns([search::Column::Query, search::Column::Website])
                            .update_columns([search::Column::SearchedAt, search::Column::Ok, search::Column::Error, search::Column::Count])
                            .to_owned(),
                    )
                    .exec_without_returning(&transaction)
                    .await?;
            }
            Err(error) => {
                // the search failed: the count of the last good try stays
                let noted = search::ActiveModel {
                    query: Set(query.to_string()),
                    website: Set(website.to_string()),
                    searched_at: Set(now),
                    ok: Set(false),
                    error: Set(Some(error.clone())),
                    count: Set(0),
                };
                search::Entity::insert(noted)
                    .on_conflict(
                        OnConflict::columns([search::Column::Query, search::Column::Website])
                            .update_columns([search::Column::SearchedAt, search::Column::Ok, search::Column::Error])
                            .to_owned(),
                    )
                    .exec_without_returning(&transaction)
                    .await?;
            }
        }

        transaction.commit().await?;

        Ok(new)
    }

    /// Notes the works the query listed, and removes the ones it listed before that are gone. Returns how many works
    /// were removed.
    async fn save_listing(db: &impl ConnectionTrait, query: &str, website: &str, listed: &[NewWork], complete: bool) -> Result<usize> {
        let before: HashSet<String> = listing::Entity::find()
            .filter(listing::Column::Query.eq(query))
            .filter(listing::Column::Website.eq(website))
            .all(db)
            .await?
            .into_iter()
            .map(|row| row.path)
            .collect();
        let now: HashSet<&str> = listed.iter().map(|work| work.path.as_str()).collect();

        let added: Vec<listing::ActiveModel> = now
            .iter()
            .filter(|path| !before.contains(**path))
            .map(|path| listing::ActiveModel { query: Set(query.to_string()), website: Set(website.to_string()), path: Set(path.to_string()) })
            .collect();
        if !added.is_empty() {
            listing::Entity::insert_many(added).exec_without_returning(db).await?;
        }

        if !complete {
            return Ok(0);
        }

        let mut removed = 0;
        for path in before.iter().filter(|path| !now.contains(path.as_str())) {
            listing::Entity::delete_by_id((query.to_string(), website.to_string(), path.clone())).exec(db).await?;

            let others = listing::Entity::find()
                .filter(listing::Column::Website.eq(website))
                .filter(listing::Column::Path.eq(path.as_str()))
                .count(db)
                .await?;
            if others == 0 {
                work::Entity::delete_by_id((website.to_string(), path.clone())).exec(db).await?;
                removed += 1;
            }
        }

        Ok(removed)
    }

    /// The listed works with the path they are saved under. `/g/1/` and `/g/1` are one page, so a site that writes the
    /// address in two ways does not make two works:
    /// - a page listed in both forms is listed once, without the slash (a site that writes it that way does not need it);
    /// - a page that is saved with the slash and is listed now without it is **renamed**: the saved row (and the lists
    ///   that name it) get the path without the slash;
    /// - a page that is listed with the slash and is saved without it stays as it is saved.
    /// A page that is listed only with the slash and is not saved is saved with the slash.
    async fn same_pages(db: &impl ConnectionTrait, website: &str, listed: &[NewWork]) -> Result<Vec<NewWork>> {
        let ends_with_slash = |path: &str| path.split('?').next().is_some_and(|base| base.ends_with('/') && base != "/");

        // each page once, in the order of the list; the form without the slash when the list has it
        let mut places: HashMap<String, usize> = HashMap::new();
        let mut kept: Vec<NewWork> = Vec::new();

        for item in listed {
            let key = if ends_with_slash(&item.path) { toggle_slash(&item.path) } else { item.path.clone() };

            match places.get(&key) {
                Some(&place) => {
                    if ends_with_slash(&kept[place].path) && !ends_with_slash(&item.path) {
                        kept[place].path = item.path.clone();
                    }
                }
                None => {
                    places.insert(key, kept.len());
                    kept.push(item.clone());
                }
            }
        }

        let wanted: Vec<String> = kept.iter().flat_map(|w| [w.path.clone(), toggle_slash(&w.path)]).collect();
        let saved: HashSet<String> = work::Entity::find()
            .filter(work::Column::Website.eq(website))
            .filter(work::Column::Path.is_in(wanted))
            .all(db)
            .await?
            .into_iter()
            .map(|row| row.path)
            .collect();

        for item in &mut kept {
            let other = toggle_slash(&item.path);

            if saved.contains(&item.path) || !saved.contains(&other) {
                continue;
            }

            if ends_with_slash(&item.path) {
                // listed with the slash, saved without: the saved form stays
                item.path = other;
            } else {
                // listed without the slash, saved with it: the saved page gets the path without
                work::Entity::update_many()
                    .col_expr(work::Column::Path, Expr::value(item.path.clone()))
                    .filter(work::Column::Website.eq(website))
                    .filter(work::Column::Path.eq(other.as_str()))
                    .exec(db)
                    .await?;
                listing::Entity::update_many()
                    .col_expr(listing::Column::Path, Expr::value(item.path.clone()))
                    .filter(listing::Column::Website.eq(website))
                    .filter(listing::Column::Path.eq(other.as_str()))
                    .exec(db)
                    .await?;
            }
        }

        Ok(kept)
    }

    /// Saves the tags and the accounts that the list shows (a tag that is new gets its own id; an account is by the site and its id on the site),
    /// and returns, by the path of a work, the ids of its tags as they are written in `work.tags` (` 12 45 `).
    async fn save_extras(db: &impl ConnectionTrait, website: &str, listed: &[NewWork]) -> Result<HashMap<String, String>> {
        let backend = db.get_database_backend();
        let mut tags: Vec<(i64, &str, &str)> = Vec::new();
        let mut accounts: Vec<(&str, &str, &str)> = Vec::new();

        for item in listed.iter().filter(|item| !item.extra.is_empty()) {
            for (place, tag) in item.extra.tags.iter().enumerate() {
                tags.push((tag.kind.code(), tag.name.as_str(), item.extra.romaji.get(place).map_or(tag.name.as_str(), String::as_str)));
            }
            if let Some(account) = &item.extra.uploader {
                accounts.push((account.id.as_str(), account.name.as_str(), item.extra.romaji.get(item.extra.tags.len()).map_or(account.name.as_str(), String::as_str)));
            }
        }
        if tags.is_empty() && accounts.is_empty() {
            return Ok(HashMap::new());
        }

        tags.sort_unstable();
        tags.dedup_by_key(|(kind, name, _)| (*kind, *name));
        for chunk in tags.chunks(300) {
            let sql = format!("INSERT OR IGNORE INTO tags (kind, name, romaji, weight) VALUES {}", vec!["(?, ?, ?, 1.0)"; chunk.len()].join(", "));
            let values: Vec<sea_orm::Value> = chunk.iter().flat_map(|(kind, name, romaji)| [(*kind).into(), (*name).into(), (*romaji).into()]).collect();
            db.execute(Statement::from_sql_and_values(backend, sql, values)).await?;
        }
        accounts.sort_unstable();
        accounts.dedup_by_key(|(id, _, _)| *id);
        for chunk in accounts.chunks(200) {
            let sql = format!(
                "INSERT INTO uploaders (website, id, name, romaji) VALUES {} ON CONFLICT(website, id) DO UPDATE SET name = excluded.name, romaji = excluded.romaji",
                vec!["(?, ?, ?, ?)"; chunk.len()].join(", ")
            );
            let values: Vec<sea_orm::Value> = chunk.iter().flat_map(|(id, name, romaji)| [website.into(), (*id).into(), (*name).into(), (*romaji).into()]).collect();
            db.execute(Statement::from_sql_and_values(backend, sql, values)).await?;
        }

        let names: Vec<&str> = tags.iter().map(|(_, name, _)| *name).collect();
        let mut ids: HashMap<(i64, String), i64> = HashMap::new();
        for chunk in names.chunks(5_000) {
            let rows = db
                .query_all(Statement::from_sql_and_values(backend, "SELECT t.id AS id, t.kind AS kind, t.name AS name FROM json_each(?) j JOIN tags t ON t.name = j.value", [serde_json::to_string(chunk)?.into()]))
                .await?;
            for row in rows {
                ids.insert((row.try_get("", "kind")?, row.try_get("", "name")?), row.try_get("", "id")?);
            }
        }

        Ok(listed
            .iter()
            .filter(|item| !item.extra.tags.is_empty())
            .map(|item| {
                let list: Vec<String> = item.extra.tags.iter().filter_map(|tag| ids.get(&(tag.kind.code(), tag.name.clone()))).map(i64::to_string).collect();
                (item.path.clone(), format!(" {} ", list.join(" ")))
            })
            .collect())
    }

    /// Adds the works that are not saved, updates the ones whose title changed. Returns the paths of the new ones.
    async fn save_works(db: &impl ConnectionTrait, website: &str, now: i64, listed: &[NewWork]) -> Result<Vec<String>> {
        let paths: Vec<&str> = listed.iter().map(|w| w.path.as_str()).collect();
        let saved: HashMap<String, work::Model> = work::Entity::find()
            .filter(work::Column::Website.eq(website))
            .filter(work::Column::Path.is_in(paths))
            .all(db)
            .await?
            .into_iter()
            .map(|row| (row.path.clone(), row))
            .collect();

        let extras = Self::save_extras(db, website, listed).await?;
        let mut added = Vec::new();

        for item in listed {
            match saved.get(&item.path) {
                None => added.push(work::ActiveModel {
                    website: Set(website.to_string()),
                    path: Set(item.path.clone()),
                    title: Set(item.title.clone()),
                    title_romaji: Set(item.romaji.clone()),
                    first_seen: Set(now),
                    updated_at: Set(now),
                    language: Set(item.language.clone()),
                    deleted: Set(false),
                    checked_at: Set(None),
                    tags: Set(extras.get(&item.path).cloned()),
                    uploader: Set(item.extra.uploader.as_ref().map(|account| account.id.clone())),
                    posted_at: Set(item.extra.posted_at),
                }),
                // a work whose page is gone is not saved again
                Some(known) if known.deleted => {}
                Some(known) if known.title != item.title || known.title_romaji != item.romaji || (!item.extra.is_empty() && (known.tags != extras.get(&item.path).cloned() || known.uploader.as_deref() != item.extra.uploader.as_ref().map(|account| account.id.as_str()) || known.posted_at != item.extra.posted_at)) => {
                    let mut changed = known.clone().into_active_model();
                    if !item.extra.is_empty() {
                        changed.tags = Set(extras.get(&item.path).cloned());
                        changed.uploader = Set(item.extra.uploader.as_ref().map(|account| account.id.clone()));
                        changed.posted_at = Set(item.extra.posted_at);
                    }
                    changed.title = Set(item.title.clone());
                    changed.title_romaji = Set(item.romaji.clone());
                    changed.language = Set(item.language.clone());
                    changed.updated_at = Set(now);
                    changed.update(db).await?;
                }
                // the same work again: its row is left as it is
                Some(_) => {}
            }
        }

        let new = added.iter().filter_map(|row| row.path.clone().take()).collect();

        if !added.is_empty() {
            // the answer lists the last added first: the works of a page are added from the last of the list, so the first of the list is the
            // last added, and a page keeps the order the site gave it
            added.reverse();
            work::Entity::insert_many(added).exec_without_returning(db).await?;
        }

        Ok(new)
    }

    /// How many works of `website` are saved (the ones whose page is gone are not counted).
    pub async fn counts_of(&self, website: &str) -> Result<u64> {
        Ok(work::Entity::find().filter(work::Column::Website.eq(website)).filter(work::Column::Deleted.eq(false)).count(&self.reader).await?)
    }

    /// The paths of the works of `website` after `after` (in the order of the paths) whose page was not looked at since `before`, at most
    /// `limit`. Nothing is sorted: the rows come in the order of the index of the paths, and the caller goes on from the last path it got (from
    /// "" again when fewer than `limit` came).
    pub async fn due_checks(&self, website: &str, before: i64, after: &str, limit: u64) -> Result<Vec<String>> {
        let rows = self
            .reader
            .query_all(Statement::from_sql_and_values(
                self.reader.get_database_backend(),
                "SELECT path FROM works WHERE website = ? AND path > ? AND deleted = 0 AND (checked_at IS NULL OR checked_at < ?) ORDER BY path LIMIT ?",
                [website.into(), after.into(), before.into(), (limit as i64).into()],
            ))
            .await?;

        rows.iter().map(|row| Ok(row.try_get::<String>("", "path")?)).collect()
    }

    /// Notes that the pages were looked at (`true`: the page is gone). A work whose page is gone keeps its row as a mark with no title (so no
    /// search finds it, and a list, the crawl and an import do not save it again), and the lists forget it.
    pub async fn record_checks(&self, website: &str, results: &[(String, bool)], now: i64) -> Result<()> {
        let transaction = self.db.begin().await?;

        for (path, gone) in results {
            let update = work::Entity::update_many()
                .col_expr(work::Column::CheckedAt, Expr::value(now))
                .filter(work::Column::Website.eq(website))
                .filter(work::Column::Path.eq(path.as_str()));

            if *gone {
                update
                    .col_expr(work::Column::Deleted, Expr::value(true))
                    .col_expr(work::Column::Title, Expr::value(""))
                    .col_expr(work::Column::TitleRomaji, Expr::value(""))
                    .col_expr(work::Column::Language, Expr::value(Option::<String>::None))
                    .exec(&transaction)
                    .await?;
                listing::Entity::delete_many().filter(listing::Column::Website.eq(website)).filter(listing::Column::Path.eq(path.as_str())).exec(&transaction).await?;
            } else {
                update.exec(&transaction).await?;
            }
        }

        transaction.commit().await?;

        Ok(())
    }

    /// The saved English titles of Japanese titles (`squash`ed): the English side of a translation that has the title as its Japanese
    /// side, as (English, source). Hitomi.La's is the one when both have it. Nothing is asked.
    pub async fn english_titles(&self, japanese_keys: &[String]) -> Result<HashMap<String, (String, String)>> {
        let rows = translation::Entity::find().filter(translation::Column::JapaneseKey.is_in(japanese_keys.to_vec())).order_by_desc(translation::Column::GalleryId).all(&self.reader).await?;
        let mut found: HashMap<String, (String, String)> = HashMap::new();

        for row in rows {
            let better = found.get(&row.japanese_key).is_none_or(|(_, source)| source != "hitomi" && row.source == "hitomi");

            if better && !row.english.is_empty() {
                found.insert(row.japanese_key.clone(), (row.english, row.source));
            }
        }

        Ok(found)
    }

    /// Makes the translations of Hitomi.La (`translations`, source `hitomi`) from the saved works: the title of a gallery that has both
    /// (`English | 日本語`, `hitomi::title_pair`) is its translation in both directions. It is what `/hitomila/translate` answers
    /// with `live=off`, with no request to the site. A pair whose English side is only the romaji of the Japanese side is left out. A pair that is
    /// saved already stays. Returns how many pairs were new.
    pub async fn build_hitomi_translations(&self) -> Result<usize> {
        let mut new = 0;
        let mut after: Option<String> = None;

        loop {
            let mut query = work::Entity::find().filter(work::Column::Website.eq("Hitomi.La")).filter(work::Column::Deleted.eq(false));
            if let Some(path) = &after {
                query = query.filter(work::Column::Path.gt(path.as_str()));
            }
            let page = query.order_by_asc(work::Column::Path).limit(5000).all(&self.db).await?;
            let Some(last) = page.last() else { break };
            after = Some(last.path.clone());

            let rows: Vec<translation::ActiveModel> = page
                .iter()
                .filter_map(|row| {
                    let (english, japanese) = crate::hitomi::title_pair(&row.title)?;
                    let id: i64 = row.path.strip_prefix("/galleries/")?.split('.').next()?.parse().ok()?;
                    let (english_key, japanese_key) = (crate::hitomi::squash(&english), crate::hitomi::squash(&japanese));
                    // the English side that is only the romaji of the Japanese one (the romaji of the title says the English side twice) is no
                    // translation: it is not saved, and the Google translation is asked for the title instead
                    if crate::hitomi::squash(&row.title_romaji) == format!("{english_key}{english_key}") {
                        return None;
                    }

                    (!english_key.is_empty() && !japanese_key.is_empty()).then(|| {
                        translation::Model {
                            key: english_key.clone(),
                            gallery_id: id,
                            position: 0,
                            url: format!("https://hitomi.la{}", row.path),
                            english,
                            japanese,
                            english_key,
                            japanese_key,
                            exact: true,
                            source: "hitomi".to_string(),
                        }
                        .into_active_model()
                    })
                })
                .collect();

            if !rows.is_empty() {
                let transaction = self.db.begin().await?;
                new += translation::Entity::insert_many(rows)
                    .on_conflict(OnConflict::columns([translation::Column::Key, translation::Column::GalleryId]).do_nothing().to_owned())
                    .exec_without_returning(&transaction)
                    .await? as usize;
                transaction.commit().await?;
            }
        }

        Ok(new)
    }

    /// Drops the Google translations (and the lookups of them) of the titles that Hitomi.La translates now, so the Hitomi.La
    /// title wins, and the lookups of Hitomi.La that found nothing for a text that has a Hitomi.La translation now (`diff` would
    /// ask the site again for them). Small transactions, a page at a time, so it is safe while the server runs. Returns how many
    /// Google rows and how many lookups were dropped.
    pub async fn drop_google_for_hitomi(&self) -> Result<(usize, usize)> {
        const PAGE: u64 = 500;
        let (mut rows, mut lookups) = (0, 0);
        let mut after: Option<(String, i64)> = None;

        // the Google rows whose Japanese title has a Hitomi.La translation
        loop {
            let mut query =
                translation::Entity::find().filter(translation::Column::Source.eq("google"));
            if let Some((key, id)) = &after {
                query = query.filter(Expr::cust_with_values(
                    "(key, gallery_id) > (?, ?)",
                    [
                        sea_orm::Value::from(key.as_str()),
                        sea_orm::Value::from(*id),
                    ],
                ));
            }
            let page = query
                .order_by_asc(translation::Column::Key)
                .order_by_asc(translation::Column::GalleryId)
                .limit(PAGE)
                .all(&self.db)
                .await?;
            let Some(last) = page.last() else { break };
            after = Some((last.key.clone(), last.gallery_id));

            let japanese: Vec<String> = page
                .iter()
                .map(|row| row.japanese_key.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let have: HashSet<String> = translation::Entity::find()
                .filter(translation::Column::Source.eq("hitomi"))
                .filter(translation::Column::JapaneseKey.is_in(japanese))
                .all(&self.db)
                .await?
                .into_iter()
                .map(|row| row.japanese_key)
                .collect();
            let dropped: Vec<&translation::Model> = page
                .iter()
                .filter(|row| have.contains(&row.japanese_key))
                .collect();

            if dropped.is_empty() {
                continue;
            }

            let keys: Vec<String> = dropped
                .iter()
                .map(|row| row.key.clone())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            let transaction = self.db.begin().await?;

            rows += translation::Entity::delete_many()
                .filter(translation::Column::Source.eq("google"))
                .filter(translation::Column::Key.is_in(keys.clone()))
                .filter(
                    translation::Column::JapaneseKey
                        .is_in(have.iter().cloned().collect::<Vec<_>>()),
                )
                .exec(&transaction)
                .await?
                .rows_affected as usize;
            lookups += translation_lookup::Entity::delete_many()
                .filter(translation_lookup::Column::Source.eq("google"))
                .filter(translation_lookup::Column::Key.is_in(keys))
                .exec(&transaction)
                .await?
                .rows_affected as usize;
            transaction.commit().await?;
        }

        // the lookups of a text that has a Hitomi.La translation now: the ones of Google (also the ones that had no row) and the
        // ones of Hitomi.La that found nothing
        let mut after: Option<(String, String)> = None;

        loop {
            let mut query = translation_lookup::Entity::find().filter(
                sea_orm::Condition::any()
                    .add(translation_lookup::Column::Source.eq("google"))
                    .add(
                        sea_orm::Condition::all()
                            .add(translation_lookup::Column::Source.eq("hitomi"))
                            .add(translation_lookup::Column::Count.eq(0)),
                    ),
            );
            if let Some((key, source)) = &after {
                query = query.filter(Expr::cust_with_values(
                    "(key, source) > (?, ?)",
                    [key.as_str(), source.as_str()],
                ));
            }
            let page = query
                .order_by_asc(translation_lookup::Column::Key)
                .order_by_asc(translation_lookup::Column::Source)
                .limit(PAGE)
                .all(&self.db)
                .await?;
            let Some(last) = page.last() else { break };
            after = Some((last.key.clone(), last.source.clone()));

            let keys: Vec<String> = page.iter().map(|row| row.key.clone()).collect();
            let mut have: HashSet<String> = HashSet::new();
            for row in translation::Entity::find()
                .filter(translation::Column::Source.eq("hitomi"))
                .filter(translation::Column::JapaneseKey.is_in(keys.clone()))
                .all(&self.db)
                .await?
            {
                have.insert(row.japanese_key);
            }
            for row in translation::Entity::find()
                .filter(translation::Column::Source.eq("hitomi"))
                .filter(translation::Column::EnglishKey.is_in(keys))
                .all(&self.db)
                .await?
            {
                have.insert(row.english_key);
            }

            for row in page.iter().filter(|row| have.contains(&row.key)) {
                lookups += translation_lookup::Entity::delete_many()
                    .filter(translation_lookup::Column::Key.eq(row.key.as_str()))
                    .filter(translation_lookup::Column::Source.eq(row.source.as_str()))
                    .exec(&self.db)
                    .await?
                    .rows_affected as usize;
            }
        }

        Ok((rows, lookups))
    }

    /// Drops the Hitomi.La translations whose English side is only the romaji of the Japanese side (what `build_hitomi_translations` leaves
    /// out now), so that the fill asks Google for those titles. A page at a time, each in a transaction of its own. Returns how many rows
    /// were dropped.
    pub async fn drop_romaji_hitomi(&self) -> Result<usize> {
        let mut dropped = 0;
        let mut after: (i64, String) = (-1, String::new());

        loop {
            let rows = self
                .db
                .query_all(Statement::from_sql_and_values(
                    self.db.get_database_backend(),
                    "SELECT t.key AS key, t.gallery_id AS gallery_id, t.english_key AS english_key, w.title_romaji AS romaji FROM translations t \
                     JOIN works w ON w.website = 'Hitomi.La' AND w.path = '/galleries/' || t.gallery_id || '.html' \
                     WHERE t.source = 'hitomi' AND (t.gallery_id, t.key) > (?, ?) ORDER BY t.gallery_id, t.key LIMIT 5000",
                    [after.0.into(), after.1.clone().into()],
                ))
                .await?;
            let Some(last) = rows.last() else { break };
            after = (last.try_get("", "gallery_id")?, last.try_get("", "key")?);

            let mut romaji: Vec<(String, i64)> = Vec::new();
            for row in &rows {
                let (english_key, title_romaji): (String, String) = (row.try_get("", "english_key")?, row.try_get("", "romaji")?);
                if crate::hitomi::squash(&title_romaji) == format!("{english_key}{english_key}") {
                    romaji.push((row.try_get("", "key")?, row.try_get("", "gallery_id")?));
                }
            }

            if !romaji.is_empty() {
                let transaction = self.db.begin().await?;
                for (key, gallery_id) in romaji {
                    dropped += translation::Entity::delete_by_id((key, gallery_id)).exec(&transaction).await?.rows_affected as usize;
                }
                transaction.commit().await?;
            }
        }

        Ok(dropped)
    }

    /// Drops every translation of `source` and every lookup of it, so that whoever asked for them asks again (the fill asks Google for
    /// the titles that have no translation). Returns how many translations and how many lookups were dropped.
    pub async fn drop_source(&self, source: &str) -> Result<(usize, usize)> {
        let (mut rows, mut lookups) = (0, 0);

        // a page of keys at a time, each in a transaction of its own: the server is not blocked for long
        loop {
            let keys: Vec<String> = translation::Entity::find()
                .filter(translation::Column::Source.eq(source))
                .limit(1000)
                .all(&self.db)
                .await?
                .into_iter()
                .map(|row| row.key)
                .collect();
            if keys.is_empty() {
                break;
            }
            rows += translation::Entity::delete_many()
                .filter(translation::Column::Source.eq(source))
                .filter(translation::Column::Key.is_in(keys))
                .exec(&self.db)
                .await?
                .rows_affected as usize;
        }

        loop {
            let keys: Vec<String> = translation_lookup::Entity::find()
                .filter(translation_lookup::Column::Source.eq(source))
                .limit(1000)
                .all(&self.db)
                .await?
                .into_iter()
                .map(|row| row.key)
                .collect();
            if keys.is_empty() {
                break;
            }
            lookups += translation_lookup::Entity::delete_many()
                .filter(translation_lookup::Column::Source.eq(source))
                .filter(translation_lookup::Column::Key.is_in(keys))
                .exec(&self.db)
                .await?
                .rows_affected as usize;
        }

        Ok((rows, lookups))
    }

    /// Where the crawl of a site stands (None: it never ran).
    /// The names of the numbers a site gives its tags, learned before: (attribute, id, name).
    pub async fn site_tag_names(&self, website: &str) -> Result<Vec<(String, u64, String)>> {
        let rows = self
            .reader
            .query_all(Statement::from_sql_and_values(self.reader.get_database_backend(), "SELECT attribute, id, name FROM site_tag_names WHERE website = ?", [website.into()]))
            .await?;
        let mut names = Vec::new();

        for row in rows {
            names.push((row.try_get::<String>("", "attribute")?, row.try_get::<i64>("", "id")? as u64, row.try_get::<String>("", "name")?));
        }

        Ok(names)
    }

    /// The names of the numbers of every site (for `transfer export`): (website, attribute, id, name).
    pub async fn all_site_tag_names(&self) -> Result<Vec<(String, String, u64, String)>> {
        let rows = self.reader.query_all(Statement::from_string(self.reader.get_database_backend(), "SELECT website, attribute, id, name FROM site_tag_names ORDER BY website, attribute, id")).await?;
        let mut names = Vec::new();

        for row in rows {
            names.push((row.try_get::<String>("", "website")?, row.try_get::<String>("", "attribute")?, row.try_get::<i64>("", "id")? as u64, row.try_get::<String>("", "name")?));
        }

        Ok(names)
    }

    /// Keeps the name of one number of a site.
    pub async fn save_site_tag_name(&self, website: &str, attribute: &str, id: u64, name: &str) -> Result<()> {
        self.db
            .execute(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "INSERT OR REPLACE INTO site_tag_names (website, attribute, id, name) VALUES (?, ?, ?, ?)",
                [website.into(), attribute.into(), (id as i64).into(), name.into()],
            ))
            .await?;

        Ok(())
    }

    pub async fn crawl_state(&self, website: &str) -> Result<Option<crawl_state::Model>> {
        Ok(crawl_state::Entity::find_by_id(website.to_string()).one(&self.reader).await?)
    }

    /// Reads the language of every title again (`language.rs`), for when its rules changed, and takes the flags out of the titles
    /// (and their romaji and key). Only the works that are different now are written. Returns how many works were looked at and how many were written.
    pub async fn relanguage(&self) -> Result<(usize, usize)> {
        let (mut looked, mut changed) = (0, 0);
        let mut after: Option<(String, String)> = None;

        loop {
            let page = self.works_after(after.as_ref(), 2000).await?;
            let Some(last) = page.last() else { break };
            after = Some((last.website.clone(), last.path.clone()));
            looked += page.len();

            let transaction = self.db.begin().await?;
            for row in page {
                // the flags are taken out of the titles (the language they name stays in `language`), so a title that names nothing
                // keeps the language it has: the flag that named it is gone
                let now = language::detect(&row.title).or(row.language.clone());
                let title = language::strip_flags(&row.title);

                if title != row.title {
                    let romaji = language::strip_flags(&row.title_romaji);
                    work::Entity::update_many()
                        .col_expr(work::Column::Title, Expr::value(title))
                        .col_expr(work::Column::TitleRomaji, Expr::value(romaji))
                        .col_expr(work::Column::Language, Expr::value(now))
                        .filter(work::Column::Website.eq(row.website.as_str()))
                        .filter(work::Column::Path.eq(row.path.as_str()))
                        .exec(&transaction)
                        .await?;
                    changed += 1;
                } else if now != row.language {
                    work::Entity::update_many()
                        .col_expr(work::Column::Language, Expr::value(now))
                        .filter(work::Column::Website.eq(row.website.as_str()))
                        .filter(work::Column::Path.eq(row.path.as_str()))
                        .exec(&transaction)
                        .await?;
                    changed += 1;
                }
            }
            transaction.commit().await?;
        }

        Ok((looked, changed))
    }

    /// Writes works that came from another database (`transfer.rs`) as they are; the ones that are saved already stay as they
    /// are. Returns how many were new. The index of the titles follows by its triggers.
    pub async fn import_works(&self, works: Vec<Work>) -> Result<usize> {
        if works.is_empty() {
            return Ok(0);
        }

        let rows: Vec<work::ActiveModel> = works.into_iter().map(IntoActiveModel::into_active_model).collect();
        let transaction = self.db.begin().await?;
        let new = work::Entity::insert_many(rows)
            .on_conflict(OnConflict::columns([work::Column::Website, work::Column::Path]).do_nothing().to_owned())
            .exec_without_returning(&transaction)
            .await?;
        transaction.commit().await?;

        Ok(new as usize)
    }

    /// Notes that the whole list of a site is read (the works came from the files of every shard): the crawl then only reads the newest
    /// pages again, and the prefetch leaves the site to it.
    pub async fn mark_crawled(&self, website: &str, now: i64) -> Result<()> {
        let transaction = self.db.begin().await?;
        let before = crawl_state::Entity::find_by_id(website.to_string()).one(&transaction).await?;
        let row = crawl_state::ActiveModel {
            website: Set(website.to_string()),
            cursor: Set(String::new()),
            done: Set(true),
            saved: Set(before.map_or(0, |row| row.saved)),
            updated_at: Set(now),
        };
        crawl_state::Entity::insert(row)
            .on_conflict(
                OnConflict::column(crawl_state::Column::Website)
                    .update_columns([crawl_state::Column::Cursor, crawl_state::Column::Done, crawl_state::Column::UpdatedAt])
                    .to_owned(),
            )
            .exec_without_returning(&transaction)
            .await?;
        // what the gap check kept for the walk is not needed any more (`crawl_probes` stays: an id that was looked at is not asked again)
        crawl_read::Entity::delete_many().filter(crawl_read::Column::Website.eq(website)).exec(&transaction).await?;
        crawl_watch::Entity::delete_many().filter(crawl_watch::Column::Website.eq(website)).exec(&transaction).await?;
        crawl_growth::Entity::delete_many().filter(crawl_growth::Column::Website.eq(website)).exec(&transaction).await?;
        crawl_gap::Entity::delete_many().filter(crawl_gap::Column::Website.eq(website)).exec(&transaction).await?;
        transaction.commit().await?;

        Ok(())
    }

    /// The sites whose crawl read the whole list (so the works they list are saved, and a query does not need to ask them).
    pub async fn crawl_done(&self) -> Result<Vec<String>> {
        Ok(crawl_state::Entity::find().filter(crawl_state::Column::Done.eq(true)).all(&self.db).await?.into_iter().map(|row| row.website).collect())
    }

    /// The saved title of the page of a work at one of `paths` (the same page written with and without the slash at the end).
    /// `None` when no such work is saved, or its page is gone.
    pub async fn title_at(&self, website: &str, paths: &[String]) -> Result<Option<String>> {
        Ok(work::Entity::find()
            .filter(work::Column::Website.eq(website))
            .filter(work::Column::Path.is_in(paths.iter().map(String::as_str)))
            .filter(work::Column::Deleted.eq(false))
            .filter(work::Column::Title.ne(""))
            .one(&self.reader)
            .await?
            .map(|row| row.title))
    }

    /// The paths among `paths` that are saved already.
    pub async fn known_paths(&self, website: &str, paths: &[String]) -> Result<HashSet<String>> {
        Ok(work::Entity::find()
            .filter(work::Column::Website.eq(website))
            .filter(work::Column::Path.is_in(paths.iter().map(String::as_str)))
            .all(&self.reader)
            .await?
            .into_iter()
            .map(|row| row.path)
            .collect())
    }

    /// Saves the works of a page of the crawl (no query listed them, so no query can take them away again), and, in the same
    /// transaction, where the walk stands (`state`: the cursor and whether the list is read to its end; None leaves it as it is).
    /// Returns how many of the works were new.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn save_crawled(&self, website: &str, now: i64, works: &[NewWork], state: Option<(&str, bool)>) -> Result<usize> {
        self.save_crawled_in(website, website, now, works, state).await
    }

    /// `save_crawled`, with the walk saved under `key` (a site read in parts by workers has a place for each part: `EHentai#1/3`).
    pub async fn save_crawled_in(&self, key: &str, website: &str, now: i64, works: &[NewWork], state: Option<(&str, bool)>) -> Result<usize> {
        self.save_crawled_inner(key, website, now, works, state, None, &[]).await
    }

    /// `save_crawled`, and the pages `lo..=hi` of the list are noted as read, in the same transaction as the works.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn save_crawled_range(&self, website: &str, now: i64, works: &[NewWork], lo: u64, hi: u64) -> Result<usize> {
        self.save_crawled_inner(website, website, now, works, None, Some((lo, hi)), &[]).await
    }

    /// `save_crawled_range`, and when each page of the range was read (`reads`) is kept in the same transaction. A page that has a row already keeps it
    /// (a lost work needs every read of a page after a shift, and the pages are read once in a walk).
    pub async fn save_crawled_range_read(&self, website: &str, now: i64, works: &[NewWork], lo: u64, hi: u64, reads: &[PageRead]) -> Result<usize> {
        self.save_crawled_inner(website, website, now, works, None, Some((lo, hi)), reads).await
    }

    /// The pages of the list of a site that the first walk has read, as ranges (`lo`, `hi`), the lowest first and joined when they touch.
    pub async fn crawl_ranges(&self, website: &str) -> Result<Vec<(u64, u64)>> {
        let rows = crawl_range::Entity::find().filter(crawl_range::Column::Website.eq(website)).order_by_asc(crawl_range::Column::Lo).all(&self.reader).await?;
        let mut joined: Vec<(u64, u64)> = Vec::new();

        for row in rows {
            let (lo, hi) = (row.lo as u64, row.hi as u64);
            match joined.last_mut() {
                Some(last) if lo <= last.1 + 1 => last.1 = last.1.max(hi),
                _ => joined.push((lo, hi)),
            }
        }

        Ok(joined)
    }

    /// Writes the ranges of a site as they are (the rows that a walk adds are one for each job: this joins them).
    pub async fn save_crawl_ranges(&self, website: &str, ranges: &[(u64, u64)]) -> Result<()> {
        let transaction = self.db.begin().await?;

        crawl_range::Entity::delete_many().filter(crawl_range::Column::Website.eq(website)).exec(&transaction).await?;
        for chunk in ranges.chunks(500) {
            let rows = chunk.iter().map(|(lo, hi)| crawl_range::ActiveModel { website: Set(website.to_string()), lo: Set(*lo as i64), hi: Set(*hi as i64) });
            crawl_range::Entity::insert_many(rows).exec_without_returning(&transaction).await?;
        }
        transaction.commit().await?;

        Ok(())
    }

    // ---- the gap check (`crawl.rs`) ----

    /// The boundaries of pages `k` and `k + 1` (`k` over `after`, the lowest first, at most `limit`) that were read by different jobs with page `k + 1`
    /// possibly read before page `k` was done.
    pub async fn crawl_pairs(&self, website: &str, after: u64, limit: u64) -> Result<Vec<Boundary>> {
        let rows = self
            .reader
            .query_all(Statement::from_sql_and_values(
                self.reader.get_database_backend(),
                "SELECT a.page AS k, a.hi_ms AS a_hi, b.lo_ms AS b_lo FROM crawl_reads a JOIN crawl_reads b ON b.website = a.website AND b.page = a.page + 1 \
                 WHERE a.website = ? AND a.page > ? AND a.job != b.job AND b.lo_ms < a.hi_ms ORDER BY a.page LIMIT ?",
                [website.into(), (after as i64).into(), (limit as i64).into()],
            ))
            .await?;

        rows.iter().map(|row| Ok(Boundary { k: row.try_get::<i64>("", "k")? as u64, a_hi: row.try_get("", "a_hi")?, b_lo: row.try_get("", "b_lo")? })).collect()
    }

    /// The last page that has a read time (the last page that was asked for: the one that was empty and ended the list, or the last one).
    pub async fn crawl_read_top(&self, website: &str) -> Result<u64> {
        let row = self
            .reader
            .query_one(Statement::from_sql_and_values(self.reader.get_database_backend(), "SELECT max(page) AS top FROM crawl_reads WHERE website = ?", [website.into()]))
            .await?;

        Ok(row.and_then(|row| row.try_get::<Option<i64>>("", "top").ok().flatten()).unwrap_or(0).max(0) as u64)
    }

    /// How many pages have a read time saved.
    #[cfg(test)]
    pub async fn crawl_read_count(&self, website: &str) -> Result<u64> {
        Ok(crawl_read::Entity::find().filter(crawl_read::Column::Website.eq(website)).count(&self.reader).await?)
    }

    /// The last look at the top of the list of a site.
    pub async fn watch(&self, website: &str) -> Result<Option<crawl_watch::Model>> {
        Ok(crawl_watch::Entity::find_by_id(website.to_string()).one(&self.reader).await?)
    }

    /// Saves a look at the top of the list (`top`, `per_page`) at `at`, and the growth it saw (`sample`), in one transaction. The first look of a site
    /// is the start of the watch (`since0`).
    pub async fn save_watch(&self, website: &str, at: i64, top: &str, per_page: i64, sample: Option<Sample>) -> Result<()> {
        let transaction = self.db.begin().await?;
        let row = crawl_watch::ActiveModel { website: Set(website.to_string()), since0: Set(at), at: Set(at), top: Set(top.to_string()), per_page: Set(per_page) };

        crawl_watch::Entity::insert(row)
            .on_conflict(OnConflict::column(crawl_watch::Column::Website).update_columns([crawl_watch::Column::At, crawl_watch::Column::Top, crawl_watch::Column::PerPage]).to_owned())
            .exec_without_returning(&transaction)
            .await?;
        if let Some(sample) = sample {
            let row = crawl_growth::ActiveModel { website: Set(website.to_string()), since: Set(sample.since), at: Set(sample.at), added: Set(sample.added), saturated: Set(sample.saturated) };

            crawl_growth::Entity::insert(row)
                .on_conflict(OnConflict::columns([crawl_growth::Column::Website, crawl_growth::Column::At]).update_columns([crawl_growth::Column::Since, crawl_growth::Column::Added, crawl_growth::Column::Saturated]).to_owned())
                .exec_without_returning(&transaction)
                .await?;
        }
        transaction.commit().await?;

        Ok(())
    }

    /// The works that came to the top in `(lo, hi)` (unix ms): the sum of the samples that overlap it, and whether one of them is only a lower bound.
    pub async fn growth(&self, website: &str, lo: i64, hi: i64) -> Result<(i64, bool)> {
        let rows = crawl_growth::Entity::find().filter(crawl_growth::Column::Website.eq(website)).filter(crawl_growth::Column::Since.lt(hi)).filter(crawl_growth::Column::At.gt(lo)).all(&self.reader).await?;

        Ok((rows.iter().map(|row| row.added).sum(), rows.iter().any(|row| row.saturated)))
    }

    /// The works that came to the top after `t` (unix ms), as `growth`.
    pub async fn growth_since(&self, website: &str, t: i64) -> Result<(i64, bool)> {
        self.growth(website, t, i64::MAX).await
    }

    /// Makes the plan of the pages to read again: one row for each span `(lo, hi)`, or, with no span, the row `(0, 0)` that is done at `now`.
    /// The plan that was there is replaced.
    pub async fn save_gap_plan(&self, website: &str, spans: &[(u64, u64)], now: i64) -> Result<()> {
        let transaction = self.db.begin().await?;

        crawl_gap::Entity::delete_many().filter(crawl_gap::Column::Website.eq(website)).exec(&transaction).await?;
        let rows: Vec<crawl_gap::ActiveModel> = if spans.is_empty() {
            vec![crawl_gap::ActiveModel { website: Set(website.to_string()), lo: Set(0), hi: Set(0), done_at: Set(Some(now)), tries: Set(0) }]
        } else {
            spans.iter().map(|(lo, hi)| crawl_gap::ActiveModel { website: Set(website.to_string()), lo: Set(*lo as i64), hi: Set(*hi as i64), done_at: Set(None), tries: Set(0) }).collect()
        };
        for chunk in rows.chunks(500) {
            crawl_gap::Entity::insert_many(chunk.to_vec()).exec_without_returning(&transaction).await?;
        }
        transaction.commit().await?;

        Ok(())
    }

    /// The plan of the pages to read again, the lowest span first (empty: no plan yet).
    pub async fn gap_plan(&self, website: &str) -> Result<Vec<crawl_gap::Model>> {
        Ok(crawl_gap::Entity::find().filter(crawl_gap::Column::Website.eq(website)).order_by_asc(crawl_gap::Column::Lo).all(&self.reader).await?)
    }

    /// Notes that reading the span at `lo` failed; at `max` tries it is given up (done at `now`, left to the refresher). Returns the tries.
    pub async fn bump_gap_tries(&self, website: &str, lo: u64, max: i64, now: i64) -> Result<i64> {
        let transaction = self.db.begin().await?;
        let row = crawl_gap::Entity::find_by_id((website.to_string(), lo as i64)).one(&transaction).await?;
        let Some(row) = row else { return Ok(0) };
        let tries = row.tries + 1;
        let mut changed = row.into_active_model();

        changed.tries = Set(tries);
        if tries >= max {
            changed.done_at = Set(Some(now));
        }
        changed.update(&transaction).await?;
        transaction.commit().await?;

        Ok(tries)
    }

    /// Saves the works of pages that were read again, and notes in the same transaction that the span at `lo` is done. No page range and no read time is written
    /// (the pages were read in the walk). Returns how many of the works were new.
    pub async fn save_gap_reread(&self, website: &str, now: i64, works: &[NewWork], lo: u64) -> Result<usize> {
        let transaction = self.db.begin().await?;
        let works = Self::same_pages(&transaction, website, works).await?;
        let new = Self::save_works(&transaction, website, now, &works).await?.len();

        transaction
            .execute(Statement::from_sql_and_values(self.db.get_database_backend(), "UPDATE crawls SET saved = saved + ?, updated_at = ? WHERE website = ?", [(new as i64).into(), now.into(), website.into()]))
            .await?;
        transaction
            .execute(Statement::from_sql_and_values(self.db.get_database_backend(), "UPDATE crawl_gap_plan SET done_at = ? WHERE website = ? AND lo = ?", [now.into(), website.into(), (lo as i64).into()]))
            .await?;
        transaction.commit().await?;

        Ok(new)
    }

    /// The numbers in the paths of the works of a site (`id_of` reads one; a path that has none is left out; the works whose page is gone are in it), sorted and without repeats.
    /// The paths are read in chunks, so the paths of a big site are never all in memory.
    pub async fn site_ids(&self, website: &str, id_of: impl Fn(&str) -> Option<u32>) -> Result<Vec<u32>> {
        let mut ids: Vec<u32> = Vec::new();
        let mut after = String::new();

        loop {
            let rows = self
                .reader
                .query_all(Statement::from_sql_and_values(
                    self.reader.get_database_backend(),
                    "SELECT path FROM works WHERE website = ? AND path > ? ORDER BY path LIMIT 20000",
                    [website.into(), after.clone().into()],
                ))
                .await?;
            let Some(last) = rows.last() else { break };

            after = last.try_get::<String>("", "path")?;
            let count = rows.len();
            for row in &rows {
                if let Some(id) = id_of(&row.try_get::<String>("", "path")?) {
                    ids.push(id);
                }
            }
            if count < 20000 {
                break;
            }
        }
        ids.sort_unstable();
        ids.dedup();

        Ok(ids)
    }

    /// The ids between the smallest and the largest of `ids` (sorted) that are not in it and have not been looked at (`crawl_probes`), the newest first, at most `limit`.
    pub async fn missing_ids(&self, website: &str, ids: &[u32], limit: usize) -> Result<Vec<u32>> {
        let (Some(&min), Some(&max)) = (ids.first(), ids.last()) else { return Ok(Vec::new()) };
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut missing = Vec::new();
        let mut top = max;

        loop {
            let bottom = top.saturating_sub(1999).max(min);
            let probed: HashSet<i64> = crawl_probe::Entity::find()
                .filter(crawl_probe::Column::Website.eq(website))
                .filter(crawl_probe::Column::Id.between(bottom as i64, top as i64))
                .all(&self.reader)
                .await?
                .into_iter()
                .map(|row| row.id)
                .collect();

            for id in (bottom..=top).rev() {
                if ids.binary_search(&id).is_err() && !probed.contains(&(id as i64)) {
                    missing.push(id);
                    if missing.len() >= limit {
                        return Ok(missing);
                    }
                }
            }
            if bottom == min {
                break;
            }
            top = bottom - 1;
        }

        Ok(missing)
    }

    /// Notes that the ids were looked at (`true`: the page exists and was saved). They are not asked again.
    pub async fn save_probes(&self, website: &str, results: &[(u32, bool)], now: i64) -> Result<()> {
        let transaction = self.db.begin().await?;

        for chunk in results.chunks(500) {
            let rows = chunk.iter().map(|(id, found)| crawl_probe::ActiveModel { website: Set(website.to_string()), id: Set(*id as i64), checked_at: Set(now), found: Set(*found) });
            crawl_probe::Entity::insert_many(rows)
                .on_conflict(OnConflict::columns([crawl_probe::Column::Website, crawl_probe::Column::Id]).update_columns([crawl_probe::Column::CheckedAt, crawl_probe::Column::Found]).to_owned())
                .exec_without_returning(&transaction)
                .await?;
        }
        transaction.commit().await?;

        Ok(())
    }

    /// The ids that were looked at, with whether the page exists.
    #[cfg(test)]
    pub async fn probes(&self, website: &str) -> Result<Vec<(u32, bool)>> {
        Ok(crawl_probe::Entity::find().filter(crawl_probe::Column::Website.eq(website)).order_by_asc(crawl_probe::Column::Id).all(&self.reader).await?.into_iter().map(|row| (row.id as u32, row.found)).collect())
    }

    async fn save_crawled_inner(&self, key: &str, website: &str, now: i64, works: &[NewWork], state: Option<(&str, bool)>, range: Option<(u64, u64)>, reads: &[PageRead]) -> Result<usize> {
        let transaction = self.db.begin().await?;
        let works = Self::same_pages(&transaction, website, works).await?;
        let new = Self::save_works(&transaction, website, now, &works).await?.len();

        if let Some((lo, hi)) = range {
            let row = crawl_range::ActiveModel { website: Set(website.to_string()), lo: Set(lo as i64), hi: Set(hi as i64) };
            crawl_range::Entity::insert(row)
                .on_conflict(OnConflict::columns([crawl_range::Column::Website, crawl_range::Column::Lo]).update_column(crawl_range::Column::Hi).to_owned())
                .exec_without_returning(&transaction)
                .await?;
        }
        for chunk in reads.chunks(500) {
            let rows = chunk.iter().map(|read| crawl_read::ActiveModel { website: Set(website.to_string()), page: Set(read.page as i64), job: Set(read.job), lo_ms: Set(read.lo_ms), hi_ms: Set(read.hi_ms) });
            crawl_read::Entity::insert_many(rows)
                .on_conflict(OnConflict::columns([crawl_read::Column::Website, crawl_read::Column::Page]).do_nothing().to_owned())
                .exec_without_returning(&transaction)
                .await?;
        }
        let before = crawl_state::Entity::find_by_id(key.to_string()).one(&transaction).await?;
        let saved = before.as_ref().map_or(0, |row| row.saved) + new as i64;
        let (cursor, done) = match (state, &before) {
            (Some((cursor, done)), _) => (cursor.to_string(), done),
            (None, Some(row)) => (row.cursor.clone(), row.done),
            (None, None) => (String::new(), false),
        };
        let row = crawl_state::ActiveModel { website: Set(key.to_string()), cursor: Set(cursor), done: Set(done), saved: Set(saved), updated_at: Set(now) };
        crawl_state::Entity::insert(row)
            .on_conflict(
                OnConflict::column(crawl_state::Column::Website)
                    .update_columns([crawl_state::Column::Cursor, crawl_state::Column::Done, crawl_state::Column::Saved, crawl_state::Column::UpdatedAt])
                    .to_owned(),
            )
            .exec_without_returning(&transaction)
            .await?;

        transaction.commit().await?;

        Ok(new)
    }

    /// The translations of a source (`hitomi` or `google`) that were saved for a text, in the order of the answer.
    pub async fn translations(&self, key: &str, source: &str) -> Result<Vec<translation::Model>> {
        Ok(translation::Entity::find()
            .filter(translation::Column::Key.eq(key))
            .filter(translation::Column::Source.eq(source))
            .order_by_asc(translation::Column::Position)
            .all(&self.reader)
            .await?)
    }

    /// The titles with Japanese letters, the newest saved first, `limit` of them after the place `before` (the `(updated_at, rowid)` of
    /// the last title of the page before; none for the first page). Each is `(updated_at, rowid, title)`. The works whose page is gone have
    /// no title and are not there.
    pub async fn japanese_titles(&self, before: Option<(i64, i64)>, limit: u64) -> Result<Vec<(i64, i64, String)>> {
        let (updated_at, rowid) = before.unwrap_or((i64::MAX, i64::MAX));
        let statement = Statement::from_sql_and_values(
            self.db.get_database_backend(),
            "SELECT updated_at, rowid, title FROM works WHERE deleted = 0 AND title GLOB ? AND (updated_at, rowid) < (?, ?) \
             ORDER BY updated_at DESC, rowid DESC LIMIT ?",
            ["*[\u{3040}-\u{30FF}\u{3400}-\u{4DBF}\u{4E00}-\u{9FFF}]*".into(), updated_at.into(), rowid.into(), (limit as i64).into()],
        );

        self.db
            .query_all(statement)
            .await?
            .iter()
            .map(|row| Ok((row.try_get::<i64>("", "updated_at")?, row.try_get::<i64>("", "rowid")?, row.try_get::<String>("", "title")?)))
            .collect()
    }

    /// The saved translations of a source (`hitomi` or `google`) that have the text (as `hitomi::squash` writes it) in
    /// their English title (`english`) or in their Japanese title, whatever text they were asked for: a translation
    /// Japanese to English answers English to Japanese too. With `exact` the title is the whole text (a machine translation
    /// is the whole text). The newest saved first; the caller puts the whole titles first.
    pub async fn translations_with(&self, source: &str, english: bool, key: &str, exact: bool, limit: u64) -> Result<Vec<translation::Model>> {
        let column = if english { translation::Column::EnglishKey } else { translation::Column::JapaneseKey };
        let condition = if exact { column.eq(key) } else { column.contains(key) };

        Ok(translation::Entity::find()
            .filter(condition)
            .filter(translation::Column::Source.eq(source))
            .order_by_desc(translation::Column::GalleryId)
            .limit(limit)
            .all(&self.reader)
            .await?)
    }

    /// Saves the answer for a text: the rows of an earlier answer are replaced (a gallery that is gone is gone), and the
    /// lookup is noted, also when nothing answered. `from` is the language of the text (`ja` or `en`).
    pub async fn save_translations(&self, key: &str, source: &str, text: &str, from: &str, now: i64, rows: &[translation::Model]) -> Result<()> {
        let transaction = self.db.begin().await?;

        translation::Entity::delete_many()
            .filter(translation::Column::Key.eq(key))
            .filter(translation::Column::Source.eq(source))
            .exec(&transaction)
            .await?;

        if !rows.is_empty() {
            let active: Vec<translation::ActiveModel> = rows.iter().cloned().map(|row| translation::Model { key: key.to_string(), source: source.to_string(), ..row }.into_active_model()).collect();

            translation::Entity::insert_many(active).exec_without_returning(&transaction).await?;
        }

        let noted = translation_lookup::ActiveModel {
            key: Set(key.to_string()),
            source: Set(source.to_string()),
            text: Set(text.to_string()),
            from_lang: Set(from.to_string()),
            looked_at: Set(now),
            count: Set(rows.len() as i64),
        };
        translation_lookup::Entity::insert(noted)
            .on_conflict(
                OnConflict::columns([translation_lookup::Column::Key, translation_lookup::Column::Source])
                    .update_columns([
                        translation_lookup::Column::Text,
                        translation_lookup::Column::FromLang,
                        translation_lookup::Column::LookedAt,
                        translation_lookup::Column::Count,
                    ])
                    .to_owned(),
            )
            .exec_without_returning(&transaction)
            .await?;

        transaction.commit().await?;

        Ok(())
    }

    /// The next `limit` works after `after` (a website and a path), in the order of their keys. Reads the table page
    /// by page, so a recount of every title does not need every row in memory.
    pub async fn works_after(&self, after: Option<&(String, String)>, limit: u64) -> Result<Vec<Work>> {
        let mut query = work::Entity::find().filter(work::Column::Deleted.eq(false));

        if let Some((website, path)) = after {
            query = query.filter(Expr::cust_with_values("(website, path) > (?, ?)", [website.as_str(), path.as_str()]));
        }

        Ok(query
            .order_by_asc(work::Column::Website)
            .order_by_asc(work::Column::Path)
            .limit(limit)
            .all(&self.db)
            .await?)
    }

    /// Every tag by id: (name, kind). The tags are some tens of thousands.
    pub async fn tag_table(&self) -> Result<HashMap<i64, (String, i64)>> {
        let rows = self.reader.query_all(Statement::from_string(self.reader.get_database_backend(), "SELECT id, name, kind FROM tags")).await?;

        rows.iter().map(|row| Ok((row.try_get("", "id")?, (row.try_get("", "name")?, row.try_get("", "kind")?)))).collect()
    }

    /// Every account by site and id: its name.
    pub async fn uploader_table(&self) -> Result<HashMap<(String, String), String>> {
        let rows = self.reader.query_all(Statement::from_string(self.reader.get_database_backend(), "SELECT website, id, name FROM uploaders")).await?;

        rows.iter().map(|row| Ok(((row.try_get("", "website")?, row.try_get("", "id")?), row.try_get("", "name")?))).collect()
    }

    /// Writes the tags, the account and the posted date of works that are saved already (an import of works that came with them): the tags and
    /// accounts are saved when they are new, and the three columns of the work are set. Works with nothing of it are left as they are.
    pub async fn import_extras(&self, website: &str, works: &[NewWork]) -> Result<()> {
        let with: Vec<NewWork> = works.iter().filter(|work| !work.extra.is_empty()).cloned().collect();
        if with.is_empty() {
            return Ok(());
        }

        let transaction = self.db.begin().await?;
        let tags = Self::save_extras(&transaction, website, &with).await?;

        for work in &with {
            transaction
                .execute(Statement::from_sql_and_values(
                    transaction.get_database_backend(),
                    "UPDATE works SET tags = ?, uploader = ?, posted_at = ? WHERE website = ? AND path = ? AND (tags IS NOT ? OR uploader IS NOT ? OR posted_at IS NOT ?)",
                    [
                        tags.get(&work.path).cloned().into(),
                        work.extra.uploader.as_ref().map(|account| account.id.clone()).into(),
                        work.extra.posted_at.into(),
                        website.into(),
                        work.path.as_str().into(),
                        tags.get(&work.path).cloned().into(),
                        work.extra.uploader.as_ref().map(|account| account.id.clone()).into(),
                        work.extra.posted_at.into(),
                    ],
                ))
                .await?;
        }
        transaction.commit().await?;

        Ok(())
    }

    /// Writes a new romaji for works (the title, `first_seen` and `updated_at` stay as they are: the order of the
    /// results does not change), and forgets that the queries which listed them were searched, so the next search
    /// (the prefetch is one) asks those sites again. Returns how many searches were forgotten.
    pub async fn rewrite_romaji(&self, changes: &[(Work, String)]) -> Result<usize> {
        let transaction = self.db.begin().await?;
        let mut searches: HashSet<(String, String)> = HashSet::new();

        for (known, romaji) in changes {
            let mut changed = known.clone().into_active_model();
            changed.title_romaji = Set(romaji.clone());
            changed.language = Set(language::detect(&known.title).or(known.language.clone()));
            changed.update(&transaction).await?;

            for row in listing::Entity::find()
                .filter(listing::Column::Website.eq(known.website.as_str()))
                .filter(listing::Column::Path.eq(known.path.as_str()))
                .all(&transaction)
                .await?
            {
                searches.insert((row.query, row.website));
            }
        }

        let mut forgotten = 0;
        for (query, website) in searches {
            forgotten += search::Entity::delete_by_id((query, website)).exec(&transaction).await?.rows_affected as usize;
        }

        transaction.commit().await?;

        Ok(forgotten)
    }

    /// The rows of the works with these row numbers, in the order of the list (the best first for a search): one query, the list is its parameter.
    async fn load(&self, rowids: &[i64]) -> Result<Vec<IndexedRow>> {
        use sea_orm::FromQueryResult;

        if rowids.is_empty() {
            return Ok(Vec::new());
        }

        let statement = Statement::from_sql_and_values(
            self.reader.get_database_backend(),
            "SELECT w.rowid AS r, w.website, w.path, w.title, w.title_romaji, w.language, w.tags, u.romaji AS uploader_romaji FROM json_each(?) j JOIN works w ON w.rowid = j.value \
             LEFT JOIN uploaders u ON u.website = w.website AND u.id = w.uploader ORDER BY j.key",
            [serde_json::to_string(rowids)?.into()],
        );
        let raw = RawRow::find_by_statement(statement).all(&self.reader).await?;
        let extras = self.extras_of(&raw.iter().map(|row| (row.tags.as_deref(), row.uploader_romaji.as_deref())).collect::<Vec<_>>()).await?;

        Ok(raw
            .into_iter()
            .zip(extras)
            .map(|(row, extra)| IndexedRow { r: row.r, website: row.website, path: row.path, title: row.title, title_romaji: row.title_romaji, language: row.language, extra })
            .collect())
    }

    /// The websites of every work that `matcher` finds: the index finds the pieces as trigrams, and a work that has them apart is not counted.
    async fn found_websites(&self, matcher: &crate::query::Matcher) -> Result<Vec<String>> {
        self.sync_index().await?;
        let rowids = self.text()?.all(matcher)?;
        let mut websites = Vec::new();

        for chunk in rowids.chunks(INDEX_FILL_CHUNK as usize) {
            for row in self.load(chunk).await? {
                if matcher.accepts(&row.key(), &row.title_romaji) {
                    websites.push(row.website);
                }
            }
        }

        Ok(websites)
    }

    /// How many works `matcher` finds in the index (the `count` command: a search does not count).
    pub async fn count_found(&self, matcher: &crate::query::Matcher) -> Result<usize> {
        Ok(self.found_websites(matcher).await?.len())
    }

    /// How many works of each site `matcher` finds in the index: (site, works), the sites with none left out (the `count` command).
    pub async fn count_found_by_site(&self, matcher: &crate::query::Matcher) -> Result<Vec<(String, usize)>> {
        let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();

        for website in self.found_websites(matcher).await? {
            *counts.entry(website).or_default() += 1;
        }

        Ok(counts.into_iter().collect())
    }

    /// For every site: (name, works, works whose page is gone, works never looked at by the verification).
    pub async fn site_stats(&self) -> Result<Vec<(String, i64, i64, i64)>> {
        let rows = self
            .reader
            .query_all(Statement::from_string(
                self.reader.get_database_backend(),
                "SELECT website, count(*) AS n, sum(deleted) AS gone, sum(checked_at IS NULL AND deleted = 0) AS unchecked FROM works GROUP BY website ORDER BY n DESC",
            ))
            .await?;

        rows.iter().map(|row| Ok((row.try_get("", "website")?, row.try_get("", "n")?, row.try_get("", "gone")?, row.try_get("", "unchecked")?))).collect()
    }

    /// The translations by source: (source, rows).
    pub async fn translation_stats(&self) -> Result<Vec<(String, i64)>> {
        let rows = self
            .reader
            .query_all(Statement::from_string(self.reader.get_database_backend(), "SELECT source, count(*) AS n FROM translations GROUP BY source ORDER BY n DESC"))
            .await?;

        rows.iter().map(|row| Ok((row.try_get("", "source")?, row.try_get("", "n")?))).collect()
    }

    /// The crawl states of the sites, by name.
    pub async fn crawl_states(&self) -> Result<Vec<crawl_state::Model>> {
        Ok(crawl_state::Entity::find().order_by_asc(crawl_state::Column::Website).all(&self.reader).await?)
    }

    /// The `limit` works that `matcher` finds, the best answer first: (site, work). The index scores them (a short title, a word that starts with
    /// the piece, and an author that is the query score higher), and the same request is answered in the same order while the works are the same.
    pub async fn top_found(&self, matchers: &[(&crate::query::Matcher, f32)], limit: usize) -> Result<Vec<(String, Candidate)>> {
        self.sync_index().await?;
        let mut boosted: Vec<(crate::query::Matcher, f32)> = Vec::new();
        for (matcher, weight) in matchers {
            boosted.push((self.with_tag_boosts(matcher).await?, *weight));
        }
        let limit = limit.max(1);
        // the trigrams of a piece may be apart in a title that does not have the piece: a few more than `limit` are read, and more when the
        // check leaves fewer
        let mut read = limit + limit / 4 + 16;

        loop {
            let rowids = self.text()?.best(&boosted.iter().map(|(matcher, weight)| (matcher, *weight)).collect::<Vec<_>>(), 0, read)?;
            let exhausted = rowids.len() < read;
            let found: Vec<IndexedRow> =
                self.load(&rowids).await?.into_iter().filter(|row| matchers.iter().any(|(matcher, _)| matcher.accepts(&row.key(), &row.title_romaji))).collect();

            if found.len() >= limit || exhausted {
                return Ok(found
                    .into_iter()
                    .take(limit)
                    .map(|row| (row.website, Candidate { site: 0, path: row.path, title: row.title, title_key: outside_key(&row.title_romaji), title_romaji: row.title_romaji, language: row.language }))
                    .collect());
            }

            read *= 2;
        }
    }

    /// The works whose title key has every one of `parts` in it (all works for none), the last added first, at most `max_candidates`. This only
    /// narrows the rows; the caller still matches every word of its query. Nothing is kept: every call reads the database.
    pub async fn candidates(&self, parts: &[&str]) -> Result<Arc<Candidates>> {
        let started = Instant::now();
        let found = Arc::new(self.candidates_up_to(parts, self.max_candidates.load(std::sync::atomic::Ordering::Relaxed)).await?);
        text_log!(info, "candidates of {parts:?}: {} read from the database in {} ms", found.len(), started.elapsed().as_millis(); "candidates of a query: {} read from the database in {} ms", found.len(), started.elapsed().as_millis());

        Ok(found)
    }

    /// The most rows a search of the database reads when the indexes cannot answer it (the newest ones are kept).
    /// Sets the boost of a match by the kind of the tag (the names of `[search] tag_boost`: `tag`, `author`, `circle`, `weak`). An unknown name is an error.
    pub fn set_kind_boost(&self, boosts: &std::collections::BTreeMap<String, f64>) -> Result<()> {
        let mut now = KIND_BOOST;

        for (name, boost) in boosts {
            let place = ["tag", "author", "circle", "weak"].iter().position(|kind| kind == name).ok_or_else(|| format!("[search] tag_boost: {name} is not a kind of tag (tag, author, circle, weak)"))?;
            now[place] = *boost;
        }
        *self.kind_boost.lock().map_err(|_| "the boosts are poisoned")? = now;
        *self.tag_keys.lock().map_err(|_| "the tags are poisoned")? = None;

        Ok(())
    }

    pub fn set_max_candidates(&self, most: usize) {
        self.max_candidates.store(most as u64, std::sync::atomic::Ordering::Relaxed);
    }

    /// The row numbers of the works whose title key has every one of `parts` in it, the last added first, at most `most`. The index finds the
    /// pieces as trigrams (a work may have them apart): the caller matches the words again.
    async fn candidate_rowids(&self, parts: &[&str], most: u64) -> Result<Vec<i64>> {
        self.sync_index().await?;
        // the index has the pieces of 3 characters or more, and the short words made of letters and digits (`ai` is not `maid`); any other short
        // piece (Japanese, a symbol) is looked for in the keys of the works the others leave (in all the works when there is no other)
        let (long, short): (Vec<&str>, Vec<&str>) = parts.iter().copied().partition(|part| part.chars().count() >= 3);
        let (words, loose): (Vec<&str>, Vec<&str>) = short.into_iter().partition(|part| part.chars().all(|c| c.is_ascii_alphanumeric()));
        let matcher = crate::query::Matcher { long: long.iter().map(|s| s.to_string()).collect(), short: words.iter().map(|s| s.to_string()).collect(), ..Default::default() };
        let narrowed = !matcher.long.is_empty() || !matcher.short.is_empty();
        let backend = self.reader.get_database_backend();

        // the works that the index finds, the last added first (all the works when it has nothing to look for)
        if loose.is_empty() {
            if narrowed {
                let mut rowids = self.text()?.all(&matcher)?;
                rowids.reverse();
                rowids.truncate(most as usize);

                return Ok(rowids);
            }

            let rows = self.reader.query_all(Statement::from_sql_and_values(backend, "SELECT rowid AS r FROM works ORDER BY rowid DESC LIMIT ?", [(most as i64).into()])).await?;

            return rows.iter().map(|row| Ok(row.try_get("", "r")?)).collect();
        }

        // a short piece that is not letters or digits is looked for in the keys of the works the others leave, chunk by chunk
        let mut found: Vec<i64> = Vec::new();
        let mut rest: Vec<i64> = if narrowed { self.text()?.all(&matcher)? } else { Vec::new() };
        rest.reverse();
        let mut before = i64::MAX;

        loop {
            let chunk: Vec<i64> = if narrowed {
                let at = rest.len().min(INDEX_FILL_CHUNK as usize);
                rest.drain(..at).collect()
            } else {
                let rows = self
                    .reader
                    .query_all(Statement::from_sql_and_values(backend, "SELECT rowid AS r FROM works WHERE rowid < ? ORDER BY rowid DESC LIMIT ?", [before.into(), INDEX_FILL_CHUNK.into()]))
                    .await?;
                let ids: Vec<i64> = rows.iter().map(|row| Ok(row.try_get("", "r")?)).collect::<Result<_>>()?;
                before = ids.last().copied().unwrap_or(i64::MIN);
                ids
            };

            if chunk.is_empty() {
                break;
            }

            for row in self.load(&chunk).await? {
                let key = row.key();

                if loose.iter().all(|part| key.contains(part)) {
                    found.push(row.r);
                }
            }

            if found.len() as u64 >= most {
                found.truncate(most as usize);
                break;
            }
        }

        Ok(found)
    }

    async fn candidates_up_to(&self, parts: &[&str], most: u64) -> Result<Candidates> {
        let rowids = self.candidate_rowids(parts, most).await?;
        let mut rows = Vec::with_capacity(rowids.len());
        for chunk in rowids.chunks(INDEX_FILL_CHUNK as usize) {
            rows.extend(self.load(chunk).await?);
        }
        let mut websites: Vec<String> = Vec::new();
        let mut works = Vec::with_capacity(rows.len());

        for row in rows {
            // a few sites: the name is looked for in the names so far
            let site = match websites.iter().position(|name| *name == row.website) {
                Some(place) => place,
                None => {
                    websites.push(row.website);
                    websites.len() - 1
                }
            };

            works.push(Candidate {
                site: site as u16,
                path: row.path,
                title: row.title,
                title_key: outside_key(&row.title_romaji),
                title_romaji: row.title_romaji,
                language: row.language,
            });
        }

        Ok(Candidates { works, websites })
    }

    /// The candidates with every column of the works, read from the database every time (for the tests).
    #[cfg(test)]
    pub async fn candidates_owned(&self, parts: &[&str]) -> Result<Vec<Work>> {
        let rowids = self.candidate_rowids(parts, self.max_candidates.load(std::sync::atomic::Ordering::Relaxed)).await?;
        let statement = Statement::from_sql_and_values(
            self.reader.get_database_backend(),
            "SELECT w.* FROM json_each(?) j JOIN works w ON w.rowid = j.value ORDER BY j.key",
            [serde_json::to_string(&rowids)?.into()],
        );

        Ok(work::Entity::find().from_raw_sql(statement).all(&self.reader).await?)
    }

    /// How many queries and works are saved (for the tests).
    #[cfg(test)]
    pub async fn counts(&self) -> Result<(u64, u64)> {
        use sea_orm::PaginatorTrait;

        let queries = search::Entity::find().select_only().column(search::Column::Query).distinct().count(&self.reader).await?;
        let works = work::Entity::find().count(&self.reader).await?;

        Ok((queries, works))
    }

    /// The names of the columns of a table (for the tests).
    #[cfg(test)]
    pub async fn columns_of(&self, table: &str) -> Result<Vec<String>> {
        use sea_orm::Statement;

        let statement = Statement::from_string(
            self.db.get_database_backend(),
            format!("SELECT name FROM pragma_table_info('{table}')"),
        );

        self.db.query_all(statement).await?.iter().map(|row| Ok(row.try_get::<String>("", "name")?)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn link(path: &str, title: &str) -> NewWork {
        NewWork { path: path.into(), title: title.into(), romaji: title.to_lowercase(), language: language::detect(title), extra: Default::default() }
    }

    fn titles(works: &[Work]) -> Vec<&str> {
        works.iter().map(|w| w.title.as_str()).collect()
    }

    #[tokio::test]
    async fn only_the_path_and_the_title_are_saved() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save("q", "A", 100, &Ok(vec![link("/g/2/", "Two  ＣＯＭＩＣ"), link("/g/1/", "One")]), false).await.unwrap();

        // the columns of the table are all there is
        assert_eq!(
            store.columns_of("works").await.unwrap(),
            ["website", "path", "title", "title_romaji", "first_seen", "updated_at", "language", "deleted", "checked_at", "tags", "uploader", "posted_at"]
        );

        let works = store.candidates_owned(&[]).await.unwrap();
        assert_eq!(works.len(), 2);
        let two = works.iter().find(|w| w.path == "/g/2/").unwrap();
        assert_eq!(two.title, "Two  ＣＯＭＩＣ", "the title is saved as the site wrote it");
        assert_eq!(
            store.states("q").await.unwrap()["A"],
            SiteState { searched_at: 100, ok: true, error: None, count: 2 }
        );
        assert!(store.states("other").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_work_is_one_row_whatever_query_found_it() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save("q1", "A", 100, &Ok(vec![link("/g/1/", "Old title")]), false).await.unwrap();
        store.save("q2", "A", 200, &Ok(vec![link("/g/1/", "New title"), link("/g/2/", "Other")]), false).await.unwrap();

        let works = store.candidates_owned(&[]).await.unwrap();
        assert_eq!(works.len(), 2, "/g/1/ is not saved twice");
        let one = works.iter().find(|w| w.path == "/g/1/").unwrap();
        assert_eq!((one.title.as_str(), one.updated_at), ("New title", 200), "the title is the last one the site gave");
        assert_eq!(one.first_seen, 100);
        assert_eq!(store.counts().await.unwrap(), (2, 2));
    }

    #[tokio::test]
    async fn the_titles_with_japanese_letters_come_newest_first_in_pages() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save("q", "A", 100, &Ok(vec![link("/g/1/", "日本語 one"), link("/g/2/", "English only")]), false).await.unwrap();
        store.save("q", "A", 200, &Ok(vec![link("/g/3/", "カタカナ"), link("/g/4/", "漢字")]), false).await.unwrap();

        let first = store.japanese_titles(None, 2).await.unwrap();
        assert_eq!(first.iter().map(|(_, _, title)| title.as_str()).collect::<Vec<_>>(), ["カタカナ", "漢字"], "the newest first, the same second last rowid first");
        let (updated_at, rowid, _) = first.last().unwrap().clone();

        let second = store.japanese_titles(Some((updated_at, rowid)), 2).await.unwrap();
        assert_eq!(second.iter().map(|(_, _, title)| title.as_str()).collect::<Vec<_>>(), ["日本語 one"], "a title in Latin letters is not there");
        assert!(store.japanese_titles(Some((second[0].0, second[0].1)), 2).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn only_a_work_that_is_new_or_changed_is_written() {
        let store = SearchStore::in_memory().await.unwrap();

        let new = store.save("q", "A", 100, &Ok(vec![link("/g/1/", "One"), link("/g/2/", "Two")]), false).await.unwrap();
        assert_eq!(new, ["/g/1/", "/g/2/"]);

        // the same works again: they are not new, and their rows are left as they are
        let new = store.save("q", "A", 200, &Ok(vec![link("/g/1/", "One"), link("/g/3/", "Three")]), false).await.unwrap();
        assert_eq!(new, ["/g/3/"]);
        let works = store.candidates_owned(&[]).await.unwrap();
        assert_eq!(works.iter().find(|w| w.path == "/g/1/").unwrap().updated_at, 100, "untouched");
        assert_eq!(works.iter().find(|w| w.path == "/g/3/").unwrap().updated_at, 200);

        // a title that changed is written, and it is not a new work
        let new = store.save("q", "A", 300, &Ok(vec![link("/g/1/", "One (2nd)")]), false).await.unwrap();
        assert!(new.is_empty());
        let one = store.candidates_owned(&[]).await.unwrap().into_iter().find(|w| w.path == "/g/1/").unwrap();
        assert_eq!((one.title.as_str(), one.updated_at, one.first_seen), ("One (2nd)", 300, 100));
        assert_eq!(outside_key(&one.title_romaji), "one", "the key is the key of what is outside the parentheses");
    }

    #[tokio::test]
    async fn a_page_written_with_and_without_the_slash_is_one_work_and_a_slash_less_path_replaces_the_one_with_it() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save("q", "A", 100, &Ok(vec![link("/g/1/", "One"), link("/g/3", "Three")]), false).await.unwrap();
        // later the site writes /g/1 without the slash, /g/3 with it; and /g/2 in both forms at once
        let new = store
            .save("q2", "A", 200, &Ok(vec![link("/g/1", "One (new title)"), link("/g/3/", "Three"), link("/g/2/", "Two"), link("/g/2", "Two")]), false)
            .await
            .unwrap();

        let saved = store.candidates_owned(&[]).await.unwrap();
        let mut paths: Vec<_> = saved.iter().map(|w| (w.path.as_str(), w.title.as_str(), w.first_seen)).collect();
        paths.sort();

        assert_eq!(
            paths,
            [("/g/1", "One (new title)", 100), ("/g/2", "Two", 200), ("/g/3", "Three", 100)],
            "/g/1 replaces /g/1/ (the same row: first_seen stays), /g/3 stays without the slash, /g/2 is saved once, without the slash"
        );
        assert_eq!(new, ["/g/2"]);

        // the list of the first query follows the rename, so it is not read as a work that is gone
        let gone = store.save("q", "A", 300, &Ok(vec![link("/g/1", "One (new title)")]), true).await.unwrap();
        assert!(gone.is_empty());
        assert!(store.candidates_owned(&[]).await.unwrap().iter().any(|w| w.path == "/g/1"));
    }

    #[tokio::test]
    async fn a_work_whose_page_is_gone_is_removed_and_not_saved_again_by_a_list_the_crawl_or_an_import() {
        let store = SearchStore::in_memory().await.unwrap();
        let work = |path: &str| NewWork { path: path.to_string(), title: format!("title {path}"), romaji: format!("title {path}"), language: None, extra: Default::default() };

        store.save("q", "Site", 100, &Ok(vec![work("/a"), work("/b")]), false).await.unwrap();
        assert_eq!(store.due_checks("Site", 0, "", 10).await.unwrap(), ["/a", "/b"], "never looked at: all of them");

        store.record_checks("Site", &[("/a".into(), true), ("/b".into(), false)], 200).await.unwrap();
        assert_eq!(paths(store.candidates_owned(&["title"]).await.unwrap()).await, ["/b"], "the work that is gone is not found: its title is blank");
        assert!(store.due_checks("Site", 150, "", 10).await.unwrap().is_empty(), "looked at since 150");
        assert_eq!(store.due_checks("Site", 300, "", 10).await.unwrap(), ["/b"], "a look that is old is made again; /a is not a work any more");
        assert!(store.works_after(None, 10).await.unwrap().iter().all(|w| w.path != "/a"), "an export or a rewrite does not see it either");

        // the list, the crawl and an import all bring /a again: it is left out
        store.save("q2", "Site", 300, &Ok(vec![work("/a"), work("/c")]), false).await.unwrap();
        store.save_crawled("Site", 300, &[work("/a"), work("/d")], None).await.unwrap();
        let gone = Work { website: "Site".into(), path: "/a".into(), title: "title /a".into(), title_romaji: "title /a".into(), first_seen: 1, updated_at: 1, language: None, deleted: false, checked_at: None, tags: None, uploader: None, posted_at: None };
        assert_eq!(store.import_works(vec![gone]).await.unwrap(), 0);
        let mut found = paths(store.candidates_owned(&["title"]).await.unwrap()).await;
        found.sort();
        assert_eq!(found, ["/b", "/c", "/d"]);
        assert_eq!(store.due_checks("Site", 400, "", 10).await.unwrap(), ["/b", "/c", "/d"], "in the order of the paths");
        assert_eq!(store.due_checks("Site", 400, "/b", 1).await.unwrap(), ["/c"], "going on after a path, a page at a time");
    }

    fn tr(source: &str, id: i64, english: &str, japanese: &str) -> translation::Model {
        use crate::hitomi::squash;

        translation::Model {
            key: if source == "hitomi" {
                squash(english)
            } else {
                squash(japanese)
            },
            gallery_id: id,
            position: 0,
            url: String::new(),
            english: english.into(),
            japanese: japanese.into(),
            english_key: squash(english),
            japanese_key: squash(japanese),
            exact: source == "hitomi",
            source: source.into(),
        }
    }

    async fn counts(store: &SearchStore) -> (usize, usize) {
        let rows = translation::Entity::find()
            .all(&store.db)
            .await
            .unwrap()
            .len();
        let lookups = translation_lookup::Entity::find()
            .all(&store.db)
            .await
            .unwrap()
            .len();

        (rows, lookups)
    }

    #[tokio::test]
    async fn a_google_row_is_dropped_when_hitomi_translates_the_same_title() {
        use crate::hitomi::squash;
        let store = SearchStore::in_memory().await.unwrap();
        let (a, b) = (
            tr("google", 0, "Machine A", "題名A"),
            tr("google", 0, "Machine B", "題名B"),
        );
        store
            .save_translations(&a.key.clone(), "google", "題名A", "ja", 1, &[a])
            .await
            .unwrap();
        store
            .save_translations(&b.key.clone(), "google", "題名B", "ja", 1, &[b])
            .await
            .unwrap();
        // Google found nothing for C, and Hitomi.La found nothing for it before
        store
            .save_translations(&squash("題名C"), "google", "題名C", "ja", 1, &[])
            .await
            .unwrap();
        store
            .save_translations(&squash("題名C"), "hitomi", "題名C", "ja", 1, &[])
            .await
            .unwrap();
        store
            .save_translations(&squash("題名D"), "hitomi", "題名D", "ja", 1, &[])
            .await
            .unwrap();
        let h = tr("hitomi", 5, "Real A", "題名A");
        let c = tr("hitomi", 6, "Real C", "題名C");
        store
            .save_translations(&h.key.clone(), "hitomi", "Real A", "en", 1, &[h])
            .await
            .unwrap();
        store
            .save_translations(&c.key.clone(), "hitomi", "Real C", "en", 1, &[c])
            .await
            .unwrap();

        // A: the google row and its lookup; C: the google lookup and the hitomi lookup that found nothing; the lookup of "Real C" stays
        let (rows, lookups) = store.drop_google_for_hitomi().await.unwrap();
        assert_eq!((rows, lookups), (1, 3));
        assert_eq!(
            store
                .translations(&squash("題名A"), "google")
                .await
                .unwrap()
                .len(),
            0
        );
        assert_eq!(
            store
                .translations(&squash("題名B"), "google")
                .await
                .unwrap()
                .len(),
            1,
            "no Hitomi.La translation: stays"
        );
        assert_eq!(
            store
                .translations(&squash("Real A"), "hitomi")
                .await
                .unwrap()
                .len(),
            1
        );
        let left: Vec<(String, String)> = translation_lookup::Entity::find()
            .all(&store.db)
            .await
            .unwrap()
            .into_iter()
            .map(|l| (l.key, l.source))
            .collect();
        assert!(
            left.contains(&(squash("題名D"), "hitomi".into())),
            "nothing found and still no translation: stays"
        );
        assert!(left.contains(&(squash("題名B"), "google".into())));

        // again: nothing changes
        assert_eq!(store.drop_google_for_hitomi().await.unwrap(), (0, 0));
    }

    #[tokio::test]
    async fn dropping_google_leaves_the_hitomi_rows() {
        let store = SearchStore::in_memory().await.unwrap();
        let g = tr("google", 0, "Machine", "題名");
        let h = tr("hitomi", 5, "Real", "別題");
        store
            .save_translations(&g.key.clone(), "google", "題名", "ja", 1, &[g])
            .await
            .unwrap();
        store
            .save_translations(&h.key.clone(), "hitomi", "Real", "en", 1, &[h])
            .await
            .unwrap();

        assert_eq!(store.drop_source("google").await.unwrap(), (1, 1));
        assert_eq!(counts(&store).await, (1, 1));
        assert_eq!(store.drop_source("google").await.unwrap(), (0, 0));
        assert_eq!(counts(&store).await, (1, 1));
    }

    #[tokio::test]
    async fn the_translations_of_a_text_are_saved_and_replaced() {
        let store = SearchStore::in_memory().await.unwrap();
        let row = |id: i64, position: i64, english: &str| translation::Model {
            key: String::new(),
            gallery_id: id,
            position,
            url: format!("https://hitomi.la/galleries/{id}.html"),
            english: english.into(),
            japanese: "日本語".into(),
            english_key: english.to_lowercase(),
            japanese_key: "日本語".into(),
            exact: position == 0,
            source: "hitomi".into(),
        };

        store.save_translations("nihongo", "hitomi", "日本語", "ja", 100, &[row(2, 0, "Two"), row(1, 1, "One")]).await.unwrap();
        let saved = store.translations("nihongo", "hitomi").await.unwrap();
        assert_eq!(saved.iter().map(|r| (r.gallery_id, r.english.as_str(), r.key.as_str())).collect::<Vec<_>>(), [(2, "Two", "nihongo"), (1, "One", "nihongo")]);

        // a later answer replaces the rows; another text is left alone
        store.save_translations("other", "hitomi", "他", "ja", 100, &[row(9, 0, "Nine")]).await.unwrap();
        store.save_translations("nihongo", "hitomi", "日本語", "ja", 200, &[row(3, 0, "Three")]).await.unwrap();
        assert_eq!(store.translations("nihongo", "hitomi").await.unwrap().iter().map(|r| r.gallery_id).collect::<Vec<_>>(), [3]);
        assert_eq!(store.translations("other", "hitomi").await.unwrap().len(), 1);

        // Google Translate is another source: its row of the same text stays when the rows of Hitomi.La are replaced
        let google = translation::Model { source: "google".into(), gallery_id: 0, ..row(0, 0, "Two (machine)") };
        store.save_translations("nihongo", "google", "日本語", "ja", 250, &[google]).await.unwrap();
        store.save_translations("nihongo", "hitomi", "日本語", "ja", 260, &[row(3, 0, "Three")]).await.unwrap();
        assert_eq!(store.translations("nihongo", "google").await.unwrap().len(), 1);
        assert_eq!(store.translations("nihongo", "hitomi").await.unwrap().len(), 1);

        // found from the other side: by the English title and by the Japanese one, whatever text was asked
        assert_eq!(store.translations_with("hitomi", true, "three", false, 10).await.unwrap().iter().map(|r| r.gallery_id).collect::<Vec<_>>(), [3]);
        assert_eq!(store.translations_with("hitomi", false, "日本", false, 10).await.unwrap().len(), 2, "the one of nihongo (replaced) and the one of other");
        assert!(store.translations_with("hitomi", true, "zzz", false, 10).await.unwrap().is_empty());
        // Google's row: found by the whole title only
        assert_eq!(store.translations_with("google", true, "two (machine)", true, 10).await.unwrap().len(), 1);
        assert!(store.translations_with("google", true, "two", true, 10).await.unwrap().is_empty());

        // nothing answered: the lookup is noted all the same
        store.save_translations("nothing", "hitomi", "なし", "ja", 300, &[]).await.unwrap();
        assert!(store.translations("nothing", "hitomi").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_same_path_on_two_sites_is_two_works() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save("q", "A", 1, &Ok(vec![link("/g/1/", "on A")]), false).await.unwrap();
        store.save("q", "B", 1, &Ok(vec![link("/g/1/", "on B")]), false).await.unwrap();

        assert_eq!(store.candidates_owned(&[]).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_work_that_may_be_beyond_the_cut_of_the_list_stays() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save("q", "A", 100, &Ok(vec![link("/g/1/", "One"), link("/g/2/", "Two")]), false).await.unwrap();
        store.save("q", "A", 200, &Ok(vec![link("/g/2/", "Two")]), false).await.unwrap();

        assert_eq!(store.candidates_owned(&[]).await.unwrap().len(), 2, "the list may have been cut short");
    }

    #[tokio::test]
    async fn a_work_that_is_gone_from_a_whole_list_is_removed_but_not_before_the_new_list_is_in() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save("q", "A", 100, &Ok(vec![link("/g/1/", "One"), link("/g/2/", "Two"), link("/g/3/", "Three")]), true).await.unwrap();
        assert_eq!(store.candidates_owned(&[]).await.unwrap().len(), 3);

        // a failure changes nothing: the old works are all there
        store.save("q", "A", 150, &Err("HTTP 500".to_string()), true).await.unwrap();
        assert_eq!(store.candidates_owned(&[]).await.unwrap().len(), 3);

        // the next whole list has no /g/1/ and no /g/3/: they are gone; /g/4/ is new
        let new = store.save("q", "A", 200, &Ok(vec![link("/g/2/", "Two"), link("/g/4/", "Four")]), true).await.unwrap();

        assert_eq!(new, ["/g/4/"]);
        let mut paths: Vec<String> = store.candidates_owned(&[]).await.unwrap().into_iter().map(|w| w.path).collect();
        paths.sort();
        assert_eq!(paths, ["/g/2/", "/g/4/"]);
    }

    #[tokio::test]
    async fn a_work_that_another_query_lists_stays_and_so_does_the_work_of_another_site() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save("q1", "A", 100, &Ok(vec![link("/g/1/", "One"), link("/g/2/", "Two")]), true).await.unwrap();
        store.save("q2", "A", 100, &Ok(vec![link("/g/1/", "One")]), true).await.unwrap();
        store.save("q1", "B", 100, &Ok(vec![link("/g/2/", "Two on B")]), true).await.unwrap();

        // q1 no longer lists anything on A: /g/2/ of A is gone, /g/1/ of A is still listed by q2, /g/2/ of B is not A's
        store.save("q1", "A", 200, &Ok(vec![]), true).await.unwrap();

        let mut found: Vec<(String, String)> = store.candidates_owned(&[]).await.unwrap().into_iter().map(|w| (w.website, w.path)).collect();
        found.sort();
        assert_eq!(found, [("A".to_string(), "/g/1/".to_string()), ("B".to_string(), "/g/2/".to_string())]);
    }

    #[tokio::test]
    async fn a_work_saved_without_a_known_list_is_never_removed() {
        use sea_orm::ActiveModelTrait;

        let store = SearchStore::in_memory().await.unwrap();

        // a row of an older version: no query is known to have listed it
        work::ActiveModel {
            website: Set("A".into()),
            path: Set("/old".into()),
            title: Set("Old".into()),
            title_romaji: Set("old".into()),
            language: Set(None),
            first_seen: Set(1),
            updated_at: Set(1),
            deleted: Set(false),
            checked_at: Set(None),
            ..Default::default()
        }
        .insert(&store.db)
        .await
        .unwrap();

        store.save("q", "A", 100, &Ok(vec![link("/g/1/", "One")]), true).await.unwrap();
        store.save("q", "A", 200, &Ok(vec![]), true).await.unwrap();

        let paths: Vec<String> = store.candidates_owned(&[]).await.unwrap().into_iter().map(|w| w.path).collect();
        assert_eq!(paths, ["/old"]);
    }

    #[tokio::test]
    async fn a_failure_only_notes_the_search() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save("q", "A", 100, &Ok(vec![link("/g/1/", "One")]), false).await.unwrap();
        store.save("q", "A", 200, &Err("HTTP 500".to_string()), false).await.unwrap();

        let state = &store.states("q").await.unwrap()["A"];
        assert_eq!((state.ok, state.error.as_deref(), state.searched_at, state.count), (false, Some("HTTP 500"), 200, 1));
        assert_eq!(store.candidates_owned(&[]).await.unwrap().len(), 1);

        store.save("q", "B", 300, &Err("blocked".to_string()), false).await.unwrap();
        assert!(!store.states("q").await.unwrap()["B"].ok);
        assert_eq!(store.counts().await.unwrap(), (1, 1));

        // and a good try after a failure clears it
        store.save("q", "A", 400, &Ok(vec![]), false).await.unwrap();
        assert_eq!(store.states("q").await.unwrap()["A"], SiteState { searched_at: 400, ok: true, error: None, count: 0 });
    }

    #[tokio::test]
    async fn the_prefilter_narrows_by_the_key_of_the_title() {
        let store = SearchStore::in_memory().await.unwrap();

        store
            .save("q", "A", 1, &Ok(vec![link("/1", "COMIC 快楽天 2026年7月号"), link("/2", "COMIC E×E 11"), link("/3", "Other")]), false)
            .await
            .unwrap();

        assert_eq!(titles(&store.candidates_owned(&["comic快楽天"]).await.unwrap()), ["COMIC 快楽天 2026年7月号"], "spaces are not in the key");
        assert_eq!(titles(&store.candidates_owned(&["comicexe"]).await.unwrap()), ["COMIC E×E 11"], "nor is the sign");
        assert_eq!(store.candidates_owned(&["nothing"]).await.unwrap().len(), 0);
        assert_eq!(store.candidates_owned(&[]).await.unwrap().len(), 3);
        // a % or a _ in the prefilter is a character, not a wildcard
        assert_eq!(store.candidates_owned(&["%"]).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn the_newest_works_come_first() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save("old", "A", 100, &Ok(vec![link("/1", "older")]), false).await.unwrap();
        store.save("new", "A", 200, &Ok(vec![link("/2", "newer")]), false).await.unwrap();
        store.save("same", "A", 200, &Ok(vec![link("/3", "also newer")]), false).await.unwrap();

        assert_eq!(titles(&store.candidates_owned(&[]).await.unwrap()), ["also newer", "newer", "older"], "the last added first");
    }

    #[tokio::test]
    async fn the_file_is_made_and_opened_again_with_its_data() {
        let dir = std::env::temp_dir().join(format!("okazuri-search-api-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("search.db");

        let store = SearchStore::open(&path).await.unwrap();
        store.save("q", "A", 1, &Ok(vec![link("/1", "One")]), false).await.unwrap();
        drop(store);

        let again = SearchStore::open(&path).await.unwrap();
        assert_eq!(titles(&again.candidates_owned(&[]).await.unwrap()), ["One"]);
        drop(again);

        let _ = fs::remove_dir_all(dir);
    }
    /// What `instr` says: the works (the newest first) whose key has the piece.
    async fn by_instr(store: &SearchStore, part: &str, most: usize) -> Vec<String> {
        store.candidates_owned(&[]).await.unwrap().into_iter().filter(|w| outside_key(&w.title_romaji).contains(part)).take(most).map(|w| w.path).collect()
    }

    async fn paths(works: Vec<Work>) -> Vec<String> {
        works.into_iter().map(|w| w.path).collect()
    }

    async fn paths_of_candidates(found: Candidates) -> Vec<String> {
        found.works.into_iter().map(|w| w.path).collect()
    }

    async fn some_works() -> SearchStore {
        let store = SearchStore::in_memory().await.unwrap();

        store
            .save(
                "q",
                "A",
                100,
                &Ok(vec![
                    link("/g/1/", "Kyoudai de Shitara Ikemasen"),
                    link("/g/2/", "COMIC Kyoudai"),
                    link("/g/3/", "Ane to Otouto (2nd)"),
                    link("/g/4/", "Comic Shitara"),
                    link("/g/5/", "ab"),
                ]),
                false,
            )
            .await
            .unwrap();
        store.save("q2", "A", 200, &Ok(vec![link("/g/6/", "Newest Kyoudai Comic")]), false).await.unwrap();

        store
    }

    #[tokio::test]
    async fn the_index_finds_the_same_works_as_instr() {
        let store = some_works().await;

        // 3 characters or more: by the index; 2 or fewer: by instr; with a character that is not a letter
        for part in ["kyoudai", "comic", "shitara", "deshi", "idai", "kyo", "ane", "(2nd)", "nothere", "\"", "kyoudai\""] {
            let found = paths(store.candidates_owned(&[part]).await.unwrap()).await;

            assert_eq!(found, by_instr(&store, part, usize::MAX).await, "{part}");
        }
        // the newest first
        assert_eq!(paths(store.candidates_owned(&["kyoudai"]).await.unwrap()).await, ["/g/6/", "/g/1/", "/g/2/"]);
    }

    #[tokio::test]
    async fn a_short_piece_of_letters_is_a_word_of_the_romaji_and_not_a_part_of_one() {
        let store = SearchStore::in_memory().await.unwrap();
        let work = |n: usize, title: &str| NewWork { path: format!("/g/{n}/"), title: title.into(), romaji: title.into(), language: None, extra: Default::default() };

        store
            .save_crawled("A", 1, &[work(1, "AI generated"), work(2, "maid and main"), work(3, "love ai, ai!"), work(4, "kai")], None)
            .await
            .unwrap();

        assert_eq!(paths(store.candidates_owned(&["ai"]).await.unwrap()).await, ["/g/1/", "/g/3/"], "not maid, main or kai (saved in the same second: the order they were saved in)");
        // a title that changes leaves and enters the index
        let one = store.candidates_owned(&["ai"]).await.unwrap().into_iter().find(|w| w.path == "/g/1/").unwrap();
        store.rewrite_romaji(&[(one, "plain".into())]).await.unwrap();
        assert_eq!(paths(store.candidates_owned(&["ai"]).await.unwrap()).await, ["/g/3/"]);
        // a short piece that is not letters and digits is still a part of the key
        assert_eq!(store.candidates_owned(&["!"]).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_most_rows_is_a_setting_for_every_word() {
        let store = SearchStore::in_memory().await.unwrap();
        let work = |n: usize| NewWork { path: format!("/g/{n}/"), title: "ai item".into(), romaji: "ai item".into(), language: None, extra: Default::default() };

        store.save_crawled("A", 1, &(0..10).map(work).collect::<Vec<_>>(), None).await.unwrap();
        assert_eq!(store.candidates_owned(&["item"]).await.unwrap().len(), 10);

        store.set_max_candidates(4);
        assert_eq!(store.candidates_owned(&["item"]).await.unwrap().len(), 4, "a long word, and the saved set is forgotten");
        assert_eq!(store.candidates_owned(&["ai"]).await.unwrap().len(), 4, "a short word");
        assert_eq!(store.candidates_owned(&[]).await.unwrap().len(), 4, "no word at all");
    }

    #[tokio::test]
    async fn a_piece_that_many_works_have_gives_the_newest_up_to_the_most() {
        let store = some_works().await;

        // "kyoudai" is in 3 works: with at most 2 it is the newest 2; with 3 it is all of them
        assert_eq!(paths_of_candidates(store.candidates_up_to(&["kyoudai"], 2).await.unwrap()).await, by_instr(&store, "kyoudai", 2).await);
        assert_eq!(paths_of_candidates(store.candidates_up_to(&["kyoudai"], 3).await.unwrap()).await, by_instr(&store, "kyoudai", 3).await);
        assert_eq!(store.candidates_up_to(&["kyoudai"], 3).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn the_index_follows_a_new_title_and_a_removed_work() {
        let store = some_works().await;

        // a new romaji of /g/2/ (the same row, another key)
        let work = store.candidates_owned(&["comic"]).await.unwrap().into_iter().find(|w| w.path == "/g/2/").unwrap();
        store.rewrite_romaji(&[(work, "totally different".into())]).await.unwrap();
        assert_eq!(paths(store.candidates_owned(&["kyoudai"]).await.unwrap()).await, ["/g/6/", "/g/1/"]);
        assert_eq!(paths(store.candidates_owned(&["different"]).await.unwrap()).await, ["/g/2/"]);

        // a removed work
        store.db.execute_unprepared("DELETE FROM works WHERE path = '/g/1/'").await.unwrap();
        assert_eq!(paths(store.candidates_owned(&["kyoudai"]).await.unwrap()).await, ["/g/6/"]);
        assert_eq!(paths(store.candidates_owned(&["shitara"]).await.unwrap()).await, ["/g/4/"]);
    }

    #[tokio::test]
    async fn a_database_of_the_old_version_gets_the_keys_outside_the_parentheses_and_the_new_indexes() {
        let dir = std::env::temp_dir().join(format!("okazuri-version-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("search.db");

        let store = SearchStore::open(&path).await.unwrap();
        // the database of version 0: the key is the key of the whole title, and the indexes have no site
        for sql in [
            "PRAGMA user_version = 0",
            "ALTER TABLE works ADD COLUMN title_key TEXT NOT NULL DEFAULT ''",
            "INSERT INTO works (website, path, title, title_romaji, title_key, first_seen, updated_at, language, deleted, checked_at) VALUES ('Site', '/g/1/', 'One (Comic Two)', 'one (comic two)', 'one(comictwo)', 1, 1, NULL, 0, NULL)",
            "INSERT INTO works (website, path, title, title_romaji, title_key, first_seen, updated_at, language, deleted, checked_at) VALUES ('Site', '/g/2/', 'Comic Two', 'comic two', 'comictwo', 2, 2, NULL, 0, NULL)",
        ] {
            store.db.execute_unprepared(sql).await.unwrap();
        }
        drop(store);

        let store = SearchStore::open(&path).await.unwrap();
        let works = store.candidates_owned(&[]).await.unwrap();
        assert_eq!(outside_key(&works.iter().find(|w| w.path == "/g/1/").unwrap().title_romaji), "one", "the key is what is outside the parentheses");
        assert!(!store.columns_of("works").await.unwrap().contains(&"title_key".to_string()), "the column of the key is dropped");

        // the index has the new keys and the site: the work whose title has the words only in its parentheses is not found
        let matcher = crate::query::Matcher { long: vec!["comic".into()], ..Default::default() };
        assert_eq!(store.count_found(&matcher).await.unwrap(), 1);
        assert_eq!(store.count_found_by_site(&matcher).await.unwrap(), [("Site".to_string(), 1)]);
        let top = store.top_found(&[(&matcher, 1.0)], 5).await.unwrap();
        assert_eq!(top.iter().map(|(site, w)| (site.as_str(), w.path.as_str())).collect::<Vec<_>>(), [("Site", "/g/2/")]);

        // opened again, nothing is made again
        drop(store);
        let store = SearchStore::open(&path).await.unwrap();
        assert_eq!(store.count_found(&matcher).await.unwrap(), 1);

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_database_without_the_index_gets_it_when_it_is_opened() {
        let dir = std::env::temp_dir().join(format!("okazuri-fts-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("search.db");

        let store = SearchStore::open(&path).await.unwrap();
        // the database of a version that had no index: the works are there, the index is not (the directory of the index is gone)
        store
            .db
            .execute_unprepared("INSERT INTO works (website, path, title, title_romaji, first_seen, updated_at, language, deleted, checked_at) VALUES ('A', '/g/1/', 'Kyoudai', 'kyoudai', 1, 1, NULL, 0, NULL), ('A', '/g/2/', 'Other', 'other', 2, 2, NULL, 0, NULL)")
            .await
            .unwrap();
        drop(store);
        fs::remove_dir_all(dir.join("search.db.index")).unwrap();

        let store = SearchStore::open(&path).await.unwrap();
        assert_eq!(paths(store.candidates_owned(&["kyoudai"]).await.unwrap()).await, ["/g/1/"]);
        assert_eq!(paths(store.candidates_owned(&["other"]).await.unwrap()).await, ["/g/2/"]);

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn relanguage_writes_only_the_works_whose_language_changed() {
        let store = SearchStore::in_memory().await.unwrap();
        let work = |path: &str, title: &str| NewWork { path: path.into(), title: title.into(), romaji: title.into(), language: language::detect(title), extra: Default::default() };

        store.save_crawled("A", 1, &[work("/1", "x [English]"), work("/2", "plain"), work("/3", "y [中国翻訳]")], None).await.unwrap();
        // the rules were different when these were saved
        store.db.execute_unprepared("UPDATE works SET language = 'ko' WHERE path = '/1'").await.unwrap();
        store.db.execute_unprepared("UPDATE works SET language = 'fr' WHERE path = '/2'").await.unwrap();

        // a language that the title no longer names stays (the flag that named it was taken out of the title): '/2' keeps its 'fr'
        assert_eq!(store.relanguage().await.unwrap(), (3, 1));
        let found: Vec<_> = store.works_after(None, 10).await.unwrap().into_iter().map(|w| w.language).collect();
        assert_eq!(found, [Some("en".to_string()), Some("fr".to_string()), Some("zh".to_string())]);
        assert_eq!(store.relanguage().await.unwrap(), (3, 0), "nothing changed the second time");
    }

    #[tokio::test]
    async fn relanguage_takes_the_flags_out_of_the_titles_and_keeps_the_language_they_named() {
        let store = SearchStore::in_memory().await.unwrap();
        // saved by a version that kept the flags in the title and in the romaji
        let old = |path: &str, title: &str| NewWork { path: path.into(), title: title.into(), romaji: title.to_lowercase(), language: language::detect(title), extra: Default::default() };

        store.save_crawled("A", 1, &[old("/1", "\u{1F1EF}\u{1F1F5} COMIC Shitsurakuten 2026-11"), old("/2", "\u{1F1E8}\u{1F1F3} Title B"), old("/3", "plain")], None).await.unwrap();

        assert_eq!(store.relanguage().await.unwrap(), (3, 2));
        let found: Vec<_> = store.works_after(None, 10).await.unwrap().into_iter().map(|w| (w.title, w.title_romaji, w.language)).collect();
        assert_eq!(
            found,
            [
                ("COMIC Shitsurakuten 2026-11".to_string(), "comic shitsurakuten 2026-11".to_string(), Some("ja".to_string())),
                ("Title B".to_string(), "title b".to_string(), Some("zh".to_string())),
                ("plain".to_string(), "plain".to_string(), None),
            ]
        );
        assert_eq!(store.relanguage().await.unwrap(), (3, 0), "the second time nothing changes, and the language stays");
    }

    fn work_pair_row(id: i64, english: &str, japanese: &str) -> translation::Model {
        use crate::hitomi::squash;

        translation::Model {
            key: squash(english),
            gallery_id: id,
            position: 0,
            url: format!("https://hitomi.la/galleries/{id}.html"),
            english: english.into(),
            japanese: japanese.into(),
            english_key: squash(english),
            japanese_key: squash(japanese),
            exact: true,
            source: "hitomi".into(),
        }
    }

    #[tokio::test]
    async fn the_translations_of_hitomi_are_made_from_the_titles_that_have_both() {
        let store = SearchStore::in_memory().await.unwrap();
        let work = |path: &str, title: &str| NewWork { path: path.into(), title: title.into(), romaji: title.into(), language: None, extra: Default::default() };
        let romanized = |path: &str, title: &str, romaji: &str| NewWork { path: path.into(), title: title.into(), romaji: romaji.into(), language: None, extra: Default::default() };

        store
            .save_crawled(
                "Hitomi.La",
                1,
                &[
                    romanized("/galleries/10.html", "Brothers | きょうだい", "Brothers | kyoudai"),
                    work("/galleries/11.html", "Only English"),
                    work("/galleries/12.html", "Chinese | 雖然來了"),
                    // the English side is the romaji of the Japanese side: no translation
                    romanized("/galleries/14.html", "Kyoudai | きょうだい", "Kyoudai | kyoudai"),
                ],
                None,
            )
            .await
            .unwrap();
        store.save_crawled("Other", 1, &[work("/galleries/13.html", "Other | 別のサイト")], None).await.unwrap();

        assert_eq!(store.build_hitomi_translations().await.unwrap(), 1);
        assert_eq!(store.build_hitomi_translations().await.unwrap(), 0, "the same pair is not made twice");
        let by_english = store.translations("brothers", "hitomi").await.unwrap();
        assert_eq!((by_english.len(), by_english[0].japanese.as_str(), by_english[0].gallery_id), (1, "きょうだい", 10));
        assert_eq!(store.translations_with("hitomi", false, "きょうだい", false, 5).await.unwrap().len(), 1, "and from the Japanese side");

        // a pair that was saved before the rule is dropped by the clean-up, and only that one
        let old = work_pair_row(14, "Kyoudai", "きょうだい");
        store.save_translations("kyoudai", "hitomi", "kyoudai", "en", 1, &[old]).await.unwrap();
        assert_eq!(store.drop_romaji_hitomi().await.unwrap(), 1);
        assert_eq!(store.translations("kyoudai", "hitomi").await.unwrap().len(), 0);
        assert_eq!(store.translations("brothers", "hitomi").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn words_that_are_common_alone_find_the_few_works_that_have_all_of_them() {
        let store = SearchStore::in_memory().await.unwrap();
        let work = |n: usize, title: &str| NewWork { path: format!("/g/{n}/"), title: title.into(), romaji: title.into(), language: language::detect(title), extra: Default::default() };
        let mut works: Vec<NewWork> = (0..30).map(|n| work(n, &format!("alpha item {n}"))).collect();
        works.extend((30..60).map(|n| work(n, &format!("beta item {n}"))));
        // the oldest work is the only one with both
        works.insert(0, work(100, "alpha beta rare"));
        for (n, one) in works.iter().enumerate() {
            store.save_crawled("A", n as i64 + 1, std::slice::from_ref(one), None).await.unwrap();
        }

        // each word is in more than 5 works; read by the longest only, the 5 newest would not have the rare one
        let found = store.candidates_up_to(&["alpha", "beta"], 5).await.unwrap();
        assert_eq!(found.iter().map(|w| w.path.as_str()).collect::<Vec<_>>(), ["/g/100/"]);

        // a short word is a word, looked for in the works the long ones leave: `ra` is inside `rare` and is not a word of it
        assert_eq!(store.candidates_up_to(&["alpha", "ra"], 5).await.unwrap().len(), 0);
        assert_eq!(store.candidates_up_to(&["alpha", "zz"], 5).await.unwrap().len(), 0);
        // the words that are common together: the newest, up to the most
        assert_eq!(store.candidates_up_to(&["item", "alpha"], 4).await.unwrap().len(), 4);
        assert_eq!(store.candidates_up_to(&["item", "30"], 4).await.unwrap().len(), 1, "a number is a word too");
    }

    #[tokio::test]
    async fn a_work_is_found_by_its_tags_and_its_uploader() {
        use crate::query::Matcher;
        use crate::sites::Extra;

        let store = SearchStore::in_memory().await.unwrap();
        let tagged = |path: &str, title: &str, uploader: &str, tags: &[&str]| {
            let mut work = link(path, title);
            let tags: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
            work.extra = Extra { romaji: tags.iter().cloned().chain([uploader.to_string()]).collect(), tags: tags.into_iter().map(|t| crate::sites::Tag::new(t, crate::sites::TagKind::Tag)).collect(), uploader: Some(crate::sites::Account { id: format!("id-{uploader}"), name: uploader.to_string() }), posted_at: Some(1_700_000_000) };
            work
        };

        store.save("q", "A", 1, &Ok(vec![tagged("/g/1/", "Plain Title", "fini", &["cosplay", "big breasts"]), link("/g/2/", "Another Title")]), false).await.unwrap();

        let rows = store.candidates_owned(&[]).await.unwrap();
        assert_eq!(rows.len(), 2);

        let by = |query: &str| {
            let matcher = Matcher { long: query.split(' ').map(str::to_string).collect(), ..Default::default() };
            let store = &store;
            async move { store.count_found(&matcher).await.unwrap() }
        };
        assert_eq!(by("cosplay").await, 1, "a tag finds the work");
        assert_eq!(by("fini").await, 1, "the uploader finds the work");
        assert_eq!(by("plain cosplay").await, 1, "a word of the title and a tag together");
        assert_eq!(by("another cosplay").await, 0, "a tag of another work does not count");

        let (tags, uploader, posted): (Option<String>, Option<String>, Option<i64>) = {
            let row = store.db.query_one(Statement::from_string(store.db.get_database_backend(), "SELECT tags, uploader, posted_at FROM works WHERE path = '/g/1/'")).await.unwrap().unwrap();
            (row.try_get("", "tags").unwrap(), row.try_get("", "uploader").unwrap(), row.try_get("", "posted_at").unwrap())
        };
        assert_eq!((tags.as_deref(), uploader.as_deref(), posted), (Some(" 2 1 "), Some("id-fini"), Some(1_700_000_000)));

        let names: Vec<(i64, String)> = store.db.query_all(Statement::from_string(store.db.get_database_backend(), "SELECT id, name FROM tags ORDER BY id")).await.unwrap().iter().map(|r| (r.try_get("", "id").unwrap(), r.try_get("", "name").unwrap())).collect();
        assert_eq!(names, [(1, "big breasts".to_string()), (2, "cosplay".to_string())], "a tag has its own id");
        let account: String = store.db.query_one(Statement::from_string(store.db.get_database_backend(), "SELECT name FROM uploaders WHERE website = 'A' AND id = 'id-fini'")).await.unwrap().unwrap().try_get("", "name").unwrap();
        assert_eq!(account, "fini", "the account is by the site and its id on the site");
    }

    #[tokio::test]
    async fn a_tag_that_is_an_author_scores_higher_and_the_weight_is_changeable() {
        use crate::query::Matcher;
        use crate::sites::{Extra, Tag, TagKind};

        let store = SearchStore::in_memory().await.unwrap();
        let with = |path: &str, kind: TagKind| {
            let mut work = link(path, &format!("Title {path}"));
            work.extra = Extra { tags: vec![Tag::new("reiki", kind)], romaji: vec!["reiki".into()], ..Default::default() };
            work
        };

        // the plain tag is saved first: with the same score it would be first
        store.save("q", "A", 1, &Ok(vec![with("/g/1/", TagKind::Tag), with("/g/2/", TagKind::Author), with("/g/3/", TagKind::Weak)]), false).await.unwrap();
        let matcher = Matcher { long: vec!["reiki".into()], ..Default::default() };
        async fn paths(store: &SearchStore, matcher: &Matcher) -> Vec<String> {
            store.top_found(&[(matcher, 1.0)], 10).await.unwrap().into_iter().map(|(_, work)| work.path).collect()
        }
        assert_eq!(paths(&store, &matcher).await, ["/g/2/", "/g/1/", "/g/3/"], "the author, the tag, the weak tag");

        // the weight of a tag can be changed: the plain tag now counts ten times as much (the memory of the tags is read again after a minute)
        store.db.execute_unprepared("UPDATE tags SET weight = 10 WHERE kind = 0").await.unwrap();
        *store.tag_keys.lock().unwrap() = None;
        assert_eq!(paths(&store, &matcher).await, ["/g/1/", "/g/2/", "/g/3/"]);
    }

    // ---- the gap check ----

    fn read(page: u64, job: i64, lo_ms: i64, hi_ms: i64) -> PageRead {
        PageRead { page, job, lo_ms, hi_ms }
    }

    async fn reads_of(store: &SearchStore) -> Vec<(i64, i64, i64, i64)> {
        let rows = crawl_read::Entity::find().filter(crawl_read::Column::Website.eq("A")).order_by_asc(crawl_read::Column::Page).all(&store.db).await.unwrap();

        rows.into_iter().map(|row| (row.page, row.job, row.lo_ms, row.hi_ms)).collect()
    }

    #[tokio::test]
    async fn page_reads_are_saved_with_the_works_and_the_first_read_is_kept() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save_crawled_range_read("A", 1, &[link("/g/1/", "One")], 1, 2, &[read(1, 7, 100, 110), read(2, 7, 110, 120)]).await.unwrap();
        assert_eq!(reads_of(&store).await, [(1, 7, 100, 110), (2, 7, 110, 120)]);
        assert_eq!(store.crawl_ranges("A").await.unwrap(), [(1, 2)]);
        assert_eq!(store.counts_of("A").await.unwrap(), 1);

        // a page that is saved again keeps its first read (a lost work needs every read of a page after a shift), and a page that is new is added
        store.save_crawled_range_read("A", 2, &[], 2, 3, &[read(2, 9, 500, 510), read(3, 9, 500, 510)]).await.unwrap();
        assert_eq!(reads_of(&store).await, [(1, 7, 100, 110), (2, 7, 110, 120), (3, 9, 500, 510)]);

        // the plain save writes no read
        store.save_crawled_range("A", 3, &[], 4, 4).await.unwrap();
        assert_eq!(store.crawl_read_count("A").await.unwrap(), 3);
    }

    #[tokio::test]
    async fn a_failed_save_writes_no_page_reads() {
        let store = SearchStore::in_memory().await.unwrap();

        // the table of the state is not there: the save fails after the reads were written, and the transaction takes them back
        store.db.execute_unprepared("DROP TABLE crawls").await.unwrap();
        let failed = store.save_crawled_range_read("A", 1, &[link("/g/1/", "One")], 1, 1, &[read(1, 7, 100, 110)]).await;

        assert!(failed.is_err());
        assert_eq!(store.crawl_read_count("A").await.unwrap(), 0);
        assert!(store.crawl_ranges("A").await.unwrap().is_empty());
        assert_eq!(store.counts_of("A").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn crawl_pairs_returns_only_out_of_order_boundaries_of_different_jobs() {
        let store = SearchStore::in_memory().await.unwrap();
        let reads = [
            // job 1: pages 1 and 2 (one job, in order, so no boundary inside it)
            read(1, 1, 1000, 1100),
            read(2, 1, 1100, 1200),
            // job 2 read page 3 before page 2 was done: out of order
            read(3, 2, 1000, 1150),
            // job 3 read page 4 after page 3 was done: in order
            read(4, 3, 1300, 1400),
            // page 5 of job 4 began when page 4 was done (the same time): in order
            read(5, 4, 1400, 1500),
            // page 6 may have been read before page 5 was done
            read(6, 5, 1450, 1460),
            // page 8 has no page 7 next to it
            read(8, 6, 0, 10),
        ];
        store.save_crawled_range_read("A", 1, &[], 1, 8, &reads).await.unwrap();

        let pairs = store.crawl_pairs("A", 0, 100).await.unwrap();

        assert_eq!(pairs, [Boundary { k: 2, a_hi: 1200, b_lo: 1000 }, Boundary { k: 5, a_hi: 1500, b_lo: 1450 }]);
        assert_eq!(store.crawl_pairs("A", 2, 100).await.unwrap().len(), 1, "after page 2");
        assert_eq!(store.crawl_pairs("A", 0, 1).await.unwrap().len(), 1, "at most the limit");
        assert!(store.crawl_pairs("B", 0, 100).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn growth_sums_overlapping_samples_only() {
        let store = SearchStore::in_memory().await.unwrap();
        let sample = |since, at, added, saturated| Some(Sample { since, at, added, saturated });

        store.save_watch("A", 0, "x", 20, None).await.unwrap();
        store.save_watch("A", 100, "y", 20, sample(0, 100, 2, false)).await.unwrap();
        store.save_watch("A", 200, "z", 20, sample(100, 200, 3, true)).await.unwrap();
        store.save_watch("A", 300, "z", 20, sample(200, 300, 5, false)).await.unwrap();

        // a window inside the second sample
        assert_eq!(store.growth("A", 120, 180).await.unwrap(), (3, true));
        // a window over the first two
        assert_eq!(store.growth("A", 50, 150).await.unwrap(), (5, true));
        // a window that only touches the end of a sample does not take it
        assert_eq!(store.growth("A", 100, 200).await.unwrap(), (3, true));
        assert_eq!(store.growth("A", 0, 100).await.unwrap(), (2, false));
        // after the last look
        assert_eq!(store.growth("A", 300, 400).await.unwrap(), (0, false));
        assert_eq!(store.growth_since("A", 150).await.unwrap(), (8, true));
        assert_eq!(store.growth_since("A", 250).await.unwrap(), (5, false));
        assert_eq!(store.growth("B", 0, 1000).await.unwrap(), (0, false));
        assert_eq!(store.watch("A").await.unwrap().map(|row| (row.since0, row.at, row.top)), Some((0, 300, "z".to_string())), "the start of the watch stays");
    }

    #[tokio::test]
    async fn watch_row_and_sample_are_one_transaction() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save_watch("A", 10, "a", 20, None).await.unwrap();
        store.db.execute_unprepared("DROP TABLE crawl_growth").await.unwrap();
        let failed = store.save_watch("A", 20, "b", 20, Some(Sample { since: 10, at: 20, added: 1, saturated: false })).await;

        assert!(failed.is_err());
        let row = store.watch("A").await.unwrap().unwrap();
        assert_eq!((row.at, row.top.as_str()), (10, "a"), "the look is not saved without its sample");
    }

    #[tokio::test]
    async fn gap_tables_appear_on_an_old_database() {
        let dir = std::env::temp_dir().join(format!("okazuri-gap-tables-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("search.db");

        let store = SearchStore::open(&path).await.unwrap();
        for table in ["crawl_reads", "crawl_watch", "crawl_growth", "crawl_gap_plan", "crawl_probes"] {
            store.db.execute_unprepared(&format!("DROP TABLE {table}")).await.unwrap();
        }
        store.save("q", "A", 1, &Ok(vec![link("/1", "One")]), false).await.unwrap();
        drop(store);

        let again = SearchStore::open(&path).await.unwrap();
        again.save_crawled_range_read("A", 1, &[], 1, 1, &[read(1, 1, 1, 2)]).await.unwrap();
        again.save_watch("A", 1, "t", 1, Some(Sample { since: 0, at: 1, added: 1, saturated: false })).await.unwrap();
        again.save_gap_plan("A", &[(2, 3)], 1).await.unwrap();
        again.save_probes("A", &[(5, true)], 1).await.unwrap();
        assert_eq!(titles(&again.candidates_owned(&[]).await.unwrap()), ["One"], "the data is kept");
        drop(again);

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn mark_crawled_clears_the_gap_tables_but_keeps_probes() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save_crawled_range_read("A", 1, &[link("/g/1/", "One")], 1, 1, &[read(1, 1, 1, 2)]).await.unwrap();
        store.save_crawled_range_read("B", 1, &[], 1, 1, &[PageRead { page: 1, job: 1, lo_ms: 1, hi_ms: 2 }]).await.unwrap();
        store.save_watch("A", 1, "t", 1, Some(Sample { since: 0, at: 1, added: 1, saturated: false })).await.unwrap();
        store.save_gap_plan("A", &[(2, 3)], 1).await.unwrap();
        store.save_probes("A", &[(5, false)], 1).await.unwrap();

        store.mark_crawled("A", 9).await.unwrap();

        assert_eq!(store.crawl_read_count("A").await.unwrap(), 0);
        assert!(store.watch("A").await.unwrap().is_none());
        assert_eq!(store.growth("A", 0, 100).await.unwrap(), (0, false));
        assert!(store.gap_plan("A").await.unwrap().is_empty());
        assert_eq!(store.probes("A").await.unwrap(), [(5, false)]);
        let state = store.crawl_state("A").await.unwrap().unwrap();
        assert!(state.done);
        assert_eq!(state.saved, 1, "what was saved is still counted");
        assert_eq!(store.crawl_read_count("B").await.unwrap(), 1, "another site is not touched");
    }

    #[tokio::test]
    async fn save_gap_reread_marks_the_plan_row_in_the_same_transaction() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save_crawled_range_read("A", 1, &[link("/g/1/", "One")], 1, 4, &[read(1, 1, 1, 2)]).await.unwrap();
        store.save_gap_plan("A", &[(3, 3), (5, 6)], 1).await.unwrap();

        let new = store.save_gap_reread("A", 50, &[link("/g/1/", "One"), link("/g/2/", "Two")], 3).await.unwrap();

        assert_eq!(new, 1);
        let plan = store.gap_plan("A").await.unwrap();
        assert_eq!(plan.iter().map(|row| (row.lo, row.hi, row.done_at)).collect::<Vec<_>>(), [(3, 3, Some(50)), (5, 6, None)]);
        assert_eq!(store.crawl_state("A").await.unwrap().unwrap().saved, 2);
        assert_eq!(store.crawl_read_count("A").await.unwrap(), 1, "no read time is written by a read again");
        assert_eq!(store.crawl_ranges("A").await.unwrap(), [(1, 4)], "and no range");

        // a failed save leaves the plan row as it was
        store.db.execute_unprepared("DROP TABLE crawls").await.unwrap();
        assert!(store.save_gap_reread("A", 60, &[link("/g/9/", "Nine")], 5).await.is_err());
        assert_eq!(store.gap_plan("A").await.unwrap()[1].done_at, None);

        // tries: the span is given up at the third
        assert_eq!(store.bump_gap_tries("A", 5, 3, 70).await.unwrap(), 1);
        assert_eq!(store.bump_gap_tries("A", 5, 3, 71).await.unwrap(), 2);
        assert_eq!(store.gap_plan("A").await.unwrap()[1].done_at, None);
        assert_eq!(store.bump_gap_tries("A", 5, 3, 72).await.unwrap(), 3);
        assert_eq!(store.gap_plan("A").await.unwrap()[1].done_at, Some(72));
    }

    #[tokio::test]
    async fn a_plan_with_no_spans_keeps_the_sentinel() {
        let store = SearchStore::in_memory().await.unwrap();

        assert!(store.gap_plan("A").await.unwrap().is_empty(), "no plan yet");
        store.save_gap_plan("A", &[], 33).await.unwrap();

        let plan = store.gap_plan("A").await.unwrap();
        assert_eq!(plan.iter().map(|row| (row.lo, row.hi, row.done_at)).collect::<Vec<_>>(), [(0, 0, Some(33))]);

        // a plan that is made again replaces it
        store.save_gap_plan("A", &[(4, 5)], 34).await.unwrap();
        assert_eq!(store.gap_plan("A").await.unwrap().len(), 1);
        assert_eq!(store.gap_plan("A").await.unwrap()[0].done_at, None);
    }

    #[tokio::test]
    async fn missing_ids_skip_probed_ids_and_stay_inside_min_max() {
        let store = SearchStore::in_memory().await.unwrap();
        let ids = [1u32, 2, 5, 9, 12];

        // the newest first, only between the smallest and the largest
        assert_eq!(store.missing_ids("A", &ids, 100).await.unwrap(), [11, 10, 8, 7, 6, 4, 3]);
        assert_eq!(store.missing_ids("A", &ids, 2).await.unwrap(), [11, 10]);
        assert!(store.missing_ids("A", &ids, 0).await.unwrap().is_empty());
        assert!(store.missing_ids("A", &[], 5).await.unwrap().is_empty());
        assert!(store.missing_ids("A", &[7], 5).await.unwrap().is_empty());

        // an id that was looked at (found or not) is not missing any more; another site's probes do not count
        store.save_probes("A", &[(11, false), (8, true)], 1).await.unwrap();
        store.save_probes("B", &[(10, false)], 1).await.unwrap();
        assert_eq!(store.missing_ids("A", &ids, 4).await.unwrap(), [10, 7, 6, 4]);

        // a range wider than a window of ids
        let wide = [1u32, 5000];
        assert_eq!(store.missing_ids("A", &wide, 3).await.unwrap(), [4999, 4998, 4997]);
        store.save_probes("A", &(1000..=4999).map(|id| (id, false)).collect::<Vec<_>>(), 1).await.unwrap();
        assert_eq!(store.missing_ids("A", &wide, 2).await.unwrap(), [999, 998]);
    }

    #[tokio::test]
    async fn the_ids_of_a_site_are_read_from_its_paths() {
        let store = SearchStore::in_memory().await.unwrap();
        let works: Vec<NewWork> = [3, 1, 2, 3].iter().map(|id| link(&format!("/gallery/{id}/"), "t")).chain([link("/tags/", "x")]).collect();

        store.save_crawled("A", 1, &works, None).await.unwrap();
        store.save_crawled("B", 1, &[link("/gallery/99/", "t")], None).await.unwrap();

        assert_eq!(store.site_ids("A", |path| crate::sites::work_id("HentaiEra", path)).await.unwrap(), [1, 2, 3]);
    }
}
