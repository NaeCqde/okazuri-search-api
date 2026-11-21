//! The search database (`[search] db_path`): the works the sites listed, and when each query was asked to
//! each site. Made and written by the engine. The tables are the entities `work` and `search`.
//!
//! A work is what a list page shows of it: the title as the site writes it, and the path of its page.
//! No image, and no address of an image.
//!
//! (The other SQLite file, the copy of D1, is `mirror.rs`.)

use crate::entity::{crawl_state, listing, search, translation, translation_lookup, work};
use crate::language;
use crate::query::title_key;
use crate::sites::toggle_slash;
use crate::utils::Result;
use sea_orm::sea_query::{Expr, Index, OnConflict};
use sea_orm::sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, IntoActiveModel, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, Schema, Set, SqlxSqliteConnector, Statement, TransactionTrait,
};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

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
}

/// A saved work.
pub type Work = work::Model;

/// The most rows `find` reads: a word that is in nearly every title would read the whole table.
const MAX_CANDIDATES: u64 = 50_000;

pub struct SearchStore {
    db: DatabaseConnection,
}

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

        Self::init(options).await.map_err(|e| format!("cannot open {}: {e}", path.display()).into())
    }

    #[cfg(test)]
    pub async fn in_memory() -> Result<Self> {
        Self::init(SqliteConnectOptions::new().in_memory(true)).await
    }

    async fn init(options: SqliteConnectOptions) -> Result<Self> {
        // one connection: the writes go one after the other, as the engine's tasks come
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(options)
            .await?;
        let db = SqlxSqliteConnector::from_sqlx_sqlite_pool(pool);

        Self::create_tables(&db).await?;

        Ok(Self { db })
    }

    async fn create_tables(db: &DatabaseConnection) -> Result<()> {
        let backend = db.get_database_backend();
        let schema = Schema::new(backend);

        let mut works = schema.create_table_from_entity(work::Entity);
        let mut searches = schema.create_table_from_entity(search::Entity);
        let mut listings = schema.create_table_from_entity(listing::Entity);
        let mut translation_lookups = schema.create_table_from_entity(translation_lookup::Entity);
        let mut translations = schema.create_table_from_entity(translation::Entity);
        db.execute(backend.build(works.if_not_exists())).await?;
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

        Self::create_word_index(db).await
    }

    /// The index of the title keys (`works_fts`): FTS5 with the trigram tokenizer, which finds the works whose key has a
    /// piece of text in it (3 characters or more) without reading every row. It has no copy of the keys (the table `works` is its
    /// content: only the pieces of 3 characters are kept in it), and three triggers keep it up to date. It is made again
    /// when it has not as many works as the table (a new index, an older database).
    async fn create_word_index(db: &DatabaseConnection) -> Result<()> {
        for sql in [
            "CREATE VIRTUAL TABLE IF NOT EXISTS works_fts USING fts5(title_key, content='works', content_rowid='rowid', tokenize='trigram')",
            "CREATE TRIGGER IF NOT EXISTS works_fts_insert AFTER INSERT ON works BEGIN
                INSERT INTO works_fts(rowid, title_key) VALUES (new.rowid, new.title_key);
            END",
            "CREATE TRIGGER IF NOT EXISTS works_fts_delete AFTER DELETE ON works BEGIN
                INSERT INTO works_fts(works_fts, rowid, title_key) VALUES ('delete', old.rowid, old.title_key);
            END",
            "CREATE TRIGGER IF NOT EXISTS works_fts_update AFTER UPDATE OF title_key ON works BEGIN
                INSERT INTO works_fts(works_fts, rowid, title_key) VALUES ('delete', old.rowid, old.title_key);
                INSERT INTO works_fts(rowid, title_key) VALUES (new.rowid, new.title_key);
            END",
        ] {
            db.execute_unprepared(sql).await?;
        }

        let counts = db
            .query_one(Statement::from_string(
                db.get_database_backend(),
                "SELECT (SELECT count(*) FROM works) AS works, (SELECT count(*) FROM works_fts_docsize) AS indexed",
            ))
            .await?
            .ok_or("no count")?;
        let (works, indexed): (i64, i64) = (counts.try_get("", "works")?, counts.try_get("", "indexed")?);

        if works != indexed {
            log::info!("works_fts: indexing {works} works ({indexed} were indexed)");
            db.execute_unprepared("INSERT INTO works_fts(works_fts) VALUES ('rebuild')").await?;
        }

        Ok(())
    }

    /// The state of every site that has been asked the query.
    pub async fn states(&self, query: &str) -> Result<HashMap<String, SiteState>> {
        let rows = search::Entity::find().filter(search::Column::Query.eq(query)).all(&self.db).await?;

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
                    log::info!("search {website}: {removed} works are not on the list of {query:?} any more, removed");
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

        let mut added = Vec::new();

        for item in listed {
            match saved.get(&item.path) {
                None => added.push(work::ActiveModel {
                    website: Set(website.to_string()),
                    path: Set(item.path.clone()),
                    title: Set(item.title.clone()),
                    title_romaji: Set(item.romaji.clone()),
                    title_key: Set(title_key(&item.romaji)),
                    first_seen: Set(now),
                    updated_at: Set(now),
                    language: Set(item.language.clone()),
                    deleted: Set(false),
                    checked_at: Set(None),
                }),
                // a work whose page is gone is not saved again
                Some(known) if known.deleted => {}
                Some(known) if known.title != item.title || known.title_romaji != item.romaji => {
                    let mut changed = known.clone().into_active_model();
                    changed.title = Set(item.title.clone());
                    changed.title_romaji = Set(item.romaji.clone());
                    changed.title_key = Set(title_key(&item.romaji));
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
            work::Entity::insert_many(added).exec_without_returning(db).await?;
        }

        Ok(new)
    }

    /// How many works of `website` are saved (the ones whose page is gone are not counted).
    pub async fn counts_of(&self, website: &str) -> Result<u64> {
        Ok(work::Entity::find().filter(work::Column::Website.eq(website)).filter(work::Column::Deleted.eq(false)).count(&self.db).await?)
    }

    /// The paths of the works of `website` whose page was not looked at since `before` (never looked at first, then the oldest look), at most `limit`.
    pub async fn due_checks(&self, website: &str, before: i64, limit: u64) -> Result<Vec<String>> {
        let rows = self
            .db
            .query_all(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "SELECT path FROM works WHERE website = ? AND deleted = 0 AND (checked_at IS NULL OR checked_at < ?) \
                 ORDER BY checked_at IS NOT NULL, checked_at, path LIMIT ?",
                [website.into(), before.into(), (limit as i64).into()],
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
                    .col_expr(work::Column::TitleKey, Expr::value(""))
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
        let rows = translation::Entity::find().filter(translation::Column::JapaneseKey.is_in(japanese_keys.to_vec())).order_by_desc(translation::Column::GalleryId).all(&self.db).await?;
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
    /// with `live=off`, with no request to the site. A pair that is saved already stays. Returns how many pairs were new.
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

    /// Where the crawl of a site stands (None: it never ran).
    pub async fn crawl_state(&self, website: &str) -> Result<Option<crawl_state::Model>> {
        Ok(crawl_state::Entity::find_by_id(website.to_string()).one(&self.db).await?)
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
                        .col_expr(work::Column::TitleKey, Expr::value(title_key(&romaji)))
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
        let before = crawl_state::Entity::find_by_id(website.to_string()).one(&self.db).await?;
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
            .exec_without_returning(&self.db)
            .await?;

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
            .one(&self.db)
            .await?
            .map(|row| row.title))
    }

    /// The paths among `paths` that are saved already.
    pub async fn known_paths(&self, website: &str, paths: &[String]) -> Result<HashSet<String>> {
        Ok(work::Entity::find()
            .filter(work::Column::Website.eq(website))
            .filter(work::Column::Path.is_in(paths.iter().map(String::as_str)))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|row| row.path)
            .collect())
    }

    /// Saves the works of a page of the crawl (no query listed them, so no query can take them away again), and, in the same
    /// transaction, where the walk stands (`state`: the cursor and whether the list is read to its end; None leaves it as it is).
    /// Returns how many of the works were new.
    pub async fn save_crawled(&self, website: &str, now: i64, works: &[NewWork], state: Option<(&str, bool)>) -> Result<usize> {
        let transaction = self.db.begin().await?;
        let works = Self::same_pages(&transaction, website, works).await?;
        let new = Self::save_works(&transaction, website, now, &works).await?.len();

        let before = crawl_state::Entity::find_by_id(website.to_string()).one(&transaction).await?;
        let saved = before.as_ref().map_or(0, |row| row.saved) + new as i64;
        let (cursor, done) = match (state, &before) {
            (Some((cursor, done)), _) => (cursor.to_string(), done),
            (None, Some(row)) => (row.cursor.clone(), row.done),
            (None, None) => (String::new(), false),
        };
        let row = crawl_state::ActiveModel { website: Set(website.to_string()), cursor: Set(cursor), done: Set(done), saved: Set(saved), updated_at: Set(now) };
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
            .all(&self.db)
            .await?)
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
            .all(&self.db)
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

    /// Writes a new romaji for works (the title, `first_seen` and `updated_at` stay as they are: the order of the
    /// results does not change), and forgets that the queries which listed them were searched, so the next search
    /// (the prefetch is one) asks those sites again. Returns how many searches were forgotten.
    pub async fn rewrite_romaji(&self, changes: &[(Work, String)]) -> Result<usize> {
        let transaction = self.db.begin().await?;
        let mut searches: HashSet<(String, String)> = HashSet::new();

        for (known, romaji) in changes {
            let mut changed = known.clone().into_active_model();
            changed.title_romaji = Set(romaji.clone());
            changed.title_key = Set(title_key(romaji));
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

    /// The works whose title key has every one of `parts` in it (all works for none), the most recently saved first, at most
    /// `MAX_CANDIDATES`. This only narrows the rows; the caller still matches every word of its query.
    pub async fn candidates(&self, parts: &[&str]) -> Result<Vec<Work>> {
        self.candidates_up_to(parts, MAX_CANDIDATES).await
    }

    async fn candidates_up_to(&self, parts: &[&str], most: u64) -> Result<Vec<Work>> {
        let mut query = work::Entity::find();
        // the index is made of pieces of 3 characters: a shorter piece can only be looked for in the rows the others leave
        let (long, short): (Vec<&str>, Vec<&str>) = parts.iter().copied().partition(|part| part.chars().count() >= 3);
        if !long.is_empty() {
            // all the pieces in one MATCH: the works that have every one of them. Whatever the number of works is, the index is used: the
            // newest `most` of them are read through it (walking the works from the newest and looking at every title reads a table of
            // gigabytes, row by row, for a piece that many works have)
            let phrases = long.iter().map(|part| format!("\"{}\"", part.replace('"', "\"\""))).collect::<Vec<_>>().join(" AND ");

            query = query.filter(Expr::cust_with_values("rowid IN (SELECT rowid FROM works_fts WHERE works_fts MATCH ?)", [phrases]));
        }
        // the pieces of less than 3 characters are looked for in the works the others leave (in all the works when there is no other)
        for part in short {
            query = query.filter(Expr::cust_with_values("instr(title_key, ?) > 0", [part]));
        }

        Ok(query
            .order_by_desc(work::Column::UpdatedAt)
            // works saved at the same second keep the order they were saved in
            .order_by_asc(Expr::cust("rowid"))
            .limit(most)
            .all(&self.db)
            .await?)
    }

    /// How many queries and works are saved (for the tests).
    #[cfg(test)]
    pub async fn counts(&self) -> Result<(u64, u64)> {
        use sea_orm::PaginatorTrait;

        let queries = search::Entity::find().select_only().column(search::Column::Query).distinct().count(&self.db).await?;
        let works = work::Entity::find().count(&self.db).await?;

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
        NewWork { path: path.into(), title: title.into(), romaji: title.to_lowercase(), language: language::detect(title) }
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
            ["website", "path", "title", "title_romaji", "title_key", "first_seen", "updated_at", "language", "deleted", "checked_at"]
        );

        let works = store.candidates(&[]).await.unwrap();
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

        let works = store.candidates(&[]).await.unwrap();
        assert_eq!(works.len(), 2, "/g/1/ is not saved twice");
        let one = works.iter().find(|w| w.path == "/g/1/").unwrap();
        assert_eq!((one.title.as_str(), one.updated_at), ("New title", 200), "the title is the last one the site gave");
        assert_eq!(one.first_seen, 100);
        assert_eq!(store.counts().await.unwrap(), (2, 2));
    }

    #[tokio::test]
    async fn only_a_work_that_is_new_or_changed_is_written() {
        let store = SearchStore::in_memory().await.unwrap();

        let new = store.save("q", "A", 100, &Ok(vec![link("/g/1/", "One"), link("/g/2/", "Two")]), false).await.unwrap();
        assert_eq!(new, ["/g/1/", "/g/2/"]);

        // the same works again: they are not new, and their rows are left as they are
        let new = store.save("q", "A", 200, &Ok(vec![link("/g/1/", "One"), link("/g/3/", "Three")]), false).await.unwrap();
        assert_eq!(new, ["/g/3/"]);
        let works = store.candidates(&[]).await.unwrap();
        assert_eq!(works.iter().find(|w| w.path == "/g/1/").unwrap().updated_at, 100, "untouched");
        assert_eq!(works.iter().find(|w| w.path == "/g/3/").unwrap().updated_at, 200);

        // a title that changed is written, and it is not a new work
        let new = store.save("q", "A", 300, &Ok(vec![link("/g/1/", "One (2nd)")]), false).await.unwrap();
        assert!(new.is_empty());
        let one = store.candidates(&[]).await.unwrap().into_iter().find(|w| w.path == "/g/1/").unwrap();
        assert_eq!((one.title.as_str(), one.updated_at, one.first_seen), ("One (2nd)", 300, 100));
        assert_eq!(one.title_key, "one(2nd)", "the key follows the title");
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

        let saved = store.candidates(&[]).await.unwrap();
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
        assert!(store.candidates(&[]).await.unwrap().iter().any(|w| w.path == "/g/1"));
    }

    #[tokio::test]
    async fn a_work_whose_page_is_gone_is_removed_and_not_saved_again_by_a_list_the_crawl_or_an_import() {
        let store = SearchStore::in_memory().await.unwrap();
        let work = |path: &str| NewWork { path: path.to_string(), title: format!("title {path}"), romaji: format!("title {path}"), language: None };

        store.save("q", "Site", 100, &Ok(vec![work("/a"), work("/b")]), false).await.unwrap();
        assert_eq!(store.due_checks("Site", 0, 10).await.unwrap(), ["/a", "/b"], "never looked at: all of them");

        store.record_checks("Site", &[("/a".into(), true), ("/b".into(), false)], 200).await.unwrap();
        assert_eq!(paths(store.candidates(&["title"]).await.unwrap()).await, ["/b"], "the work that is gone is not found: its title is blank");
        assert!(store.due_checks("Site", 150, 10).await.unwrap().is_empty(), "looked at since 150");
        assert_eq!(store.due_checks("Site", 300, 10).await.unwrap(), ["/b"], "a look that is old is made again; /a is not a work any more");
        assert!(store.works_after(None, 10).await.unwrap().iter().all(|w| w.path != "/a"), "an export or a rewrite does not see it either");

        // the list, the crawl and an import all bring /a again: it is left out
        store.save("q2", "Site", 300, &Ok(vec![work("/a"), work("/c")]), false).await.unwrap();
        store.save_crawled("Site", 300, &[work("/a"), work("/d")], None).await.unwrap();
        let gone = Work { website: "Site".into(), path: "/a".into(), title: "title /a".into(), title_romaji: "title /a".into(), title_key: "titlea".into(), first_seen: 1, updated_at: 1, language: None, deleted: false, checked_at: None };
        assert_eq!(store.import_works(vec![gone]).await.unwrap(), 0);
        let mut found = paths(store.candidates(&["title"]).await.unwrap()).await;
        found.sort();
        assert_eq!(found, ["/b", "/c", "/d"]);
        assert_eq!(store.due_checks("Site", 400, 10).await.unwrap(), ["/c", "/d", "/b"], "the works never looked at first, then the oldest look");
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

        assert_eq!(store.candidates(&[]).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_work_that_may_be_beyond_the_cut_of_the_list_stays() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save("q", "A", 100, &Ok(vec![link("/g/1/", "One"), link("/g/2/", "Two")]), false).await.unwrap();
        store.save("q", "A", 200, &Ok(vec![link("/g/2/", "Two")]), false).await.unwrap();

        assert_eq!(store.candidates(&[]).await.unwrap().len(), 2, "the list may have been cut short");
    }

    #[tokio::test]
    async fn a_work_that_is_gone_from_a_whole_list_is_removed_but_not_before_the_new_list_is_in() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save("q", "A", 100, &Ok(vec![link("/g/1/", "One"), link("/g/2/", "Two"), link("/g/3/", "Three")]), true).await.unwrap();
        assert_eq!(store.candidates(&[]).await.unwrap().len(), 3);

        // a failure changes nothing: the old works are all there
        store.save("q", "A", 150, &Err("HTTP 500".to_string()), true).await.unwrap();
        assert_eq!(store.candidates(&[]).await.unwrap().len(), 3);

        // the next whole list has no /g/1/ and no /g/3/: they are gone; /g/4/ is new
        let new = store.save("q", "A", 200, &Ok(vec![link("/g/2/", "Two"), link("/g/4/", "Four")]), true).await.unwrap();

        assert_eq!(new, ["/g/4/"]);
        let mut paths: Vec<String> = store.candidates(&[]).await.unwrap().into_iter().map(|w| w.path).collect();
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

        let mut found: Vec<(String, String)> = store.candidates(&[]).await.unwrap().into_iter().map(|w| (w.website, w.path)).collect();
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
            title_key: Set("old".into()),
            language: Set(None),
            first_seen: Set(1),
            updated_at: Set(1),
            deleted: Set(false),
            checked_at: Set(None),
        }
        .insert(&store.db)
        .await
        .unwrap();

        store.save("q", "A", 100, &Ok(vec![link("/g/1/", "One")]), true).await.unwrap();
        store.save("q", "A", 200, &Ok(vec![]), true).await.unwrap();

        let paths: Vec<String> = store.candidates(&[]).await.unwrap().into_iter().map(|w| w.path).collect();
        assert_eq!(paths, ["/old"]);
    }

    #[tokio::test]
    async fn a_failure_only_notes_the_search() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save("q", "A", 100, &Ok(vec![link("/g/1/", "One")]), false).await.unwrap();
        store.save("q", "A", 200, &Err("HTTP 500".to_string()), false).await.unwrap();

        let state = &store.states("q").await.unwrap()["A"];
        assert_eq!((state.ok, state.error.as_deref(), state.searched_at, state.count), (false, Some("HTTP 500"), 200, 1));
        assert_eq!(store.candidates(&[]).await.unwrap().len(), 1);

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

        assert_eq!(titles(&store.candidates(&["comic快楽天"]).await.unwrap()), ["COMIC 快楽天 2026年7月号"], "spaces are not in the key");
        assert_eq!(titles(&store.candidates(&["comicexe"]).await.unwrap()), ["COMIC E×E 11"], "nor is the sign");
        assert_eq!(store.candidates(&["nothing"]).await.unwrap().len(), 0);
        assert_eq!(store.candidates(&[]).await.unwrap().len(), 3);
        // a % or a _ in the prefilter is a character, not a wildcard
        assert_eq!(store.candidates(&["%"]).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn the_newest_works_come_first() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save("old", "A", 100, &Ok(vec![link("/1", "older")]), false).await.unwrap();
        store.save("new", "A", 200, &Ok(vec![link("/2", "newer")]), false).await.unwrap();
        store.save("same", "A", 200, &Ok(vec![link("/3", "also newer")]), false).await.unwrap();

        assert_eq!(titles(&store.candidates(&[]).await.unwrap()), ["newer", "also newer", "older"], "ties keep the order they were saved in");
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
        assert_eq!(titles(&again.candidates(&[]).await.unwrap()), ["One"]);
        drop(again);

        let _ = fs::remove_dir_all(dir);
    }
    /// What `instr` says: the works (the newest first) whose key has the piece.
    async fn by_instr(store: &SearchStore, part: &str, most: usize) -> Vec<String> {
        store.candidates(&[]).await.unwrap().into_iter().filter(|w| w.title_key.contains(part)).take(most).map(|w| w.path).collect()
    }

    async fn paths(works: Vec<Work>) -> Vec<String> {
        works.into_iter().map(|w| w.path).collect()
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
        for part in ["kyoudai", "comic", "shitara", "deshi", "idai", "kyo", "ane", "(2nd)", "de", "ab", "k", "nothere", "\"", "kyoudai\""] {
            let found = paths(store.candidates(&[part]).await.unwrap()).await;

            assert_eq!(found, by_instr(&store, part, usize::MAX).await, "{part}");
        }
        // the newest first
        assert_eq!(paths(store.candidates(&["kyoudai"]).await.unwrap()).await, ["/g/6/", "/g/1/", "/g/2/"]);
    }

    #[tokio::test]
    async fn a_piece_that_many_works_have_gives_the_newest_up_to_the_most() {
        let store = some_works().await;

        // "kyoudai" is in 3 works: with at most 2 it is the newest 2; with 3 it is all of them
        assert_eq!(paths(store.candidates_up_to(&["kyoudai"], 2).await.unwrap()).await, by_instr(&store, "kyoudai", 2).await);
        assert_eq!(paths(store.candidates_up_to(&["kyoudai"], 3).await.unwrap()).await, by_instr(&store, "kyoudai", 3).await);
        assert_eq!(store.candidates_up_to(&["kyoudai"], 3).await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn the_index_follows_a_new_title_and_a_removed_work() {
        let store = some_works().await;

        // a new romaji of /g/2/ (the same row, another key)
        let work = store.candidates(&["comic"]).await.unwrap().into_iter().find(|w| w.path == "/g/2/").unwrap();
        store.rewrite_romaji(&[(work, "totally different".into())]).await.unwrap();
        assert_eq!(paths(store.candidates(&["kyoudai"]).await.unwrap()).await, ["/g/6/", "/g/1/"]);
        assert_eq!(paths(store.candidates(&["different"]).await.unwrap()).await, ["/g/2/"]);

        // a removed work
        store.db.execute_unprepared("DELETE FROM works WHERE path = '/g/1/'").await.unwrap();
        assert_eq!(paths(store.candidates(&["kyoudai"]).await.unwrap()).await, ["/g/6/"]);
        assert_eq!(paths(store.candidates(&["shitara"]).await.unwrap()).await, ["/g/4/"]);
    }

    #[tokio::test]
    async fn a_database_without_the_index_gets_it_when_it_is_opened() {
        let dir = std::env::temp_dir().join(format!("okazuri-fts-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("search.db");

        let store = SearchStore::open(&path).await.unwrap();
        // the database of a version that had no index: the works are there, the index is not
        for sql in ["DROP TRIGGER works_fts_insert", "DROP TRIGGER works_fts_delete", "DROP TRIGGER works_fts_update", "DROP TABLE works_fts"] {
            store.db.execute_unprepared(sql).await.unwrap();
        }
        store
            .db
            .execute_unprepared("INSERT INTO works VALUES ('A', '/g/1/', 'Kyoudai', 'kyoudai', 'kyoudai', 1, 1, NULL, 0, NULL), ('A', '/g/2/', 'Other', 'other', 'other', 2, 2, NULL, 0, NULL)")
            .await
            .unwrap();
        drop(store);

        let store = SearchStore::open(&path).await.unwrap();
        assert_eq!(paths(store.candidates(&["kyoudai"]).await.unwrap()).await, ["/g/1/"]);
        assert_eq!(paths(store.candidates(&["other"]).await.unwrap()).await, ["/g/2/"]);

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn relanguage_writes_only_the_works_whose_language_changed() {
        let store = SearchStore::in_memory().await.unwrap();
        let work = |path: &str, title: &str| NewWork { path: path.into(), title: title.into(), romaji: title.into(), language: language::detect(title) };

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
        let old = |path: &str, title: &str| NewWork { path: path.into(), title: title.into(), romaji: title.to_lowercase(), language: language::detect(title) };

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

    #[tokio::test]
    async fn the_translations_of_hitomi_are_made_from_the_titles_that_have_both() {
        let store = SearchStore::in_memory().await.unwrap();
        let work = |path: &str, title: &str| NewWork { path: path.into(), title: title.into(), romaji: title.into(), language: None };

        store
            .save_crawled("Hitomi.La", 1, &[work("/galleries/10.html", "Kyoudai | きょうだい"), work("/galleries/11.html", "Only English"), work("/galleries/12.html", "Chinese | 雖然來了")], None)
            .await
            .unwrap();
        store.save_crawled("Other", 1, &[work("/galleries/13.html", "Other | 別のサイト")], None).await.unwrap();

        assert_eq!(store.build_hitomi_translations().await.unwrap(), 1);
        assert_eq!(store.build_hitomi_translations().await.unwrap(), 0, "the same pair is not made twice");
        let by_english = store.translations("kyoudai", "hitomi").await.unwrap();
        assert_eq!((by_english.len(), by_english[0].japanese.as_str(), by_english[0].gallery_id), (1, "きょうだい", 10));
        assert_eq!(store.translations_with("hitomi", false, "きょうだい", false, 5).await.unwrap().len(), 1, "and from the Japanese side");
    }

    #[tokio::test]
    async fn words_that_are_common_alone_find_the_few_works_that_have_all_of_them() {
        let store = SearchStore::in_memory().await.unwrap();
        let work = |n: usize, title: &str| NewWork { path: format!("/g/{n}/"), title: title.into(), romaji: title.into(), language: language::detect(title) };
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

        // a short word is looked for in the works the long ones leave, and a piece that nothing has finds nothing
        assert_eq!(store.candidates_up_to(&["alpha", "ra"], 5).await.unwrap().len(), 1);
        assert_eq!(store.candidates_up_to(&["alpha", "zz"], 5).await.unwrap().len(), 0);
        // the words that are common together: the newest, up to the most
        assert_eq!(store.candidates_up_to(&["item", "ite"], 4).await.unwrap().len(), 4);
    }
}
