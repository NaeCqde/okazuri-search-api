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
    pub title: String,
    pub romaji: String,
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

        Self::add_language(db).await?;

        Self::create_word_index(db).await
    }

    /// A database from before `works.language`: the column is added, and the language of every title is read. It is quick (no request).
    async fn add_language(db: &DatabaseConnection) -> Result<()> {
        let has = db
            .query_one(Statement::from_string(db.get_database_backend(), "SELECT count(*) AS n FROM pragma_table_info('works') WHERE name = 'language'"))
            .await?
            .ok_or("no count")?
            .try_get::<i64>("", "n")?;

        if has > 0 {
            return Ok(());
        }

        log::info!("works.language: adding the column and reading the language of every title");
        db.execute_unprepared("ALTER TABLE works ADD COLUMN language TEXT NOT NULL DEFAULT 'unknown'").await?;

        let transaction = db.begin().await?;
        let mut after: Option<(String, String)> = None;

        loop {
            let mut query = work::Entity::find();
            if let Some((website, path)) = &after {
                query = query.filter(Expr::cust_with_values("(website, path) > (?, ?)", [website.as_str(), path.as_str()]));
            }
            let page = query.order_by_asc(work::Column::Website).order_by_asc(work::Column::Path).limit(2000).all(&transaction).await?;
            let Some(last) = page.last() else { break };
            after = Some((last.website.clone(), last.path.clone()));

            for row in &page {
                let found = language::detect(&row.title);

                if found != language::UNKNOWN {
                    work::Entity::update_many()
                        .col_expr(work::Column::Language, Expr::value(found))
                        .filter(work::Column::Website.eq(row.website.as_str()))
                        .filter(work::Column::Path.eq(row.path.as_str()))
                        .exec(&transaction)
                        .await?;
                }
            }
        }
        transaction.commit().await?;

        Ok(())
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
                    language: Set(language::detect(&item.title)),
                }),
                Some(known) if known.title != item.title || known.title_romaji != item.romaji => {
                    let mut changed = known.clone().into_active_model();
                    changed.title = Set(item.title.clone());
                    changed.title_romaji = Set(item.romaji.clone());
                    changed.title_key = Set(title_key(&item.romaji));
                    changed.language = Set(language::detect(&item.title));
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

    /// Where the crawl of a site stands (None: it never ran).
    pub async fn crawl_state(&self, website: &str) -> Result<Option<crawl_state::Model>> {
        Ok(crawl_state::Entity::find_by_id(website.to_string()).one(&self.db).await?)
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
        let mut query = work::Entity::find();

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
            changed.language = Set(language::detect(&known.title));
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

    /// The works whose title key has `prefilter` in it (all works without one), the most recently saved first, at most
    /// `MAX_CANDIDATES`. This only narrows the rows; the caller still matches every word of its query.
    pub async fn candidates(&self, prefilter: Option<&str>) -> Result<Vec<Work>> {
        self.candidates_up_to(prefilter, MAX_CANDIDATES).await
    }

    async fn candidates_up_to(&self, prefilter: Option<&str>, most: u64) -> Result<Vec<Work>> {
        let mut query = work::Entity::find();

        if let Some(part) = prefilter {
            query = query.filter(match self.indexed(part, most).await? {
                Some(filter) => filter,
                None => Expr::cust_with_values("instr(title_key, ?) > 0", [part]),
            });
        }

        Ok(query
            .order_by_desc(work::Column::UpdatedAt)
            // works saved at the same second keep the order they were saved in
            .order_by_asc(Expr::cust("rowid"))
            .limit(most)
            .all(&self.db)
            .await?)
    }

    /// The condition "the key has `part`" by the index of the keys, when that is the quick way: the piece has 3 characters or more
    /// (the index is made of pieces of 3), and no more than `most` works have it. A piece that most works have is better read by
    /// walking the works from the newest, which stops after `most`: None, and the caller uses `instr`.
    async fn indexed(&self, part: &str, most: u64) -> Result<Option<sea_orm::sea_query::SimpleExpr>> {
        if part.chars().count() < 3 {
            return Ok(None);
        }

        // a phrase: the piece as it is, whatever characters it has
        let phrase = format!("\"{}\"", part.replace('"', "\"\""));
        let row = self
            .db
            .query_one(Statement::from_sql_and_values(
                self.db.get_database_backend(),
                "SELECT count(*) AS n FROM (SELECT 1 FROM works_fts WHERE works_fts MATCH ? LIMIT ?)",
                [phrase.clone().into(), (most + 1).into()],
            ))
            .await?
            .ok_or("no count")?;
        let found: i64 = row.try_get("", "n")?;

        Ok((found as u64 <= most).then(|| Expr::cust_with_values("rowid IN (SELECT rowid FROM works_fts WHERE works_fts MATCH ?)", [phrase])))
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
        NewWork { path: path.into(), title: title.into(), romaji: title.to_lowercase() }
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
            ["website", "path", "title", "title_romaji", "title_key", "first_seen", "updated_at", "language"]
        );

        let works = store.candidates(None).await.unwrap();
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

        let works = store.candidates(None).await.unwrap();
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
        let works = store.candidates(None).await.unwrap();
        assert_eq!(works.iter().find(|w| w.path == "/g/1/").unwrap().updated_at, 100, "untouched");
        assert_eq!(works.iter().find(|w| w.path == "/g/3/").unwrap().updated_at, 200);

        // a title that changed is written, and it is not a new work
        let new = store.save("q", "A", 300, &Ok(vec![link("/g/1/", "One (2nd)")]), false).await.unwrap();
        assert!(new.is_empty());
        let one = store.candidates(None).await.unwrap().into_iter().find(|w| w.path == "/g/1/").unwrap();
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

        let saved = store.candidates(None).await.unwrap();
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
        assert!(store.candidates(None).await.unwrap().iter().any(|w| w.path == "/g/1"));
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

        assert_eq!(store.candidates(None).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_work_that_may_be_beyond_the_cut_of_the_list_stays() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save("q", "A", 100, &Ok(vec![link("/g/1/", "One"), link("/g/2/", "Two")]), false).await.unwrap();
        store.save("q", "A", 200, &Ok(vec![link("/g/2/", "Two")]), false).await.unwrap();

        assert_eq!(store.candidates(None).await.unwrap().len(), 2, "the list may have been cut short");
    }

    #[tokio::test]
    async fn a_work_that_is_gone_from_a_whole_list_is_removed_but_not_before_the_new_list_is_in() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save("q", "A", 100, &Ok(vec![link("/g/1/", "One"), link("/g/2/", "Two"), link("/g/3/", "Three")]), true).await.unwrap();
        assert_eq!(store.candidates(None).await.unwrap().len(), 3);

        // a failure changes nothing: the old works are all there
        store.save("q", "A", 150, &Err("HTTP 500".to_string()), true).await.unwrap();
        assert_eq!(store.candidates(None).await.unwrap().len(), 3);

        // the next whole list has no /g/1/ and no /g/3/: they are gone; /g/4/ is new
        let new = store.save("q", "A", 200, &Ok(vec![link("/g/2/", "Two"), link("/g/4/", "Four")]), true).await.unwrap();

        assert_eq!(new, ["/g/4/"]);
        let mut paths: Vec<String> = store.candidates(None).await.unwrap().into_iter().map(|w| w.path).collect();
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

        let mut found: Vec<(String, String)> = store.candidates(None).await.unwrap().into_iter().map(|w| (w.website, w.path)).collect();
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
            language: Set("unknown".into()),
            first_seen: Set(1),
            updated_at: Set(1),
        }
        .insert(&store.db)
        .await
        .unwrap();

        store.save("q", "A", 100, &Ok(vec![link("/g/1/", "One")]), true).await.unwrap();
        store.save("q", "A", 200, &Ok(vec![]), true).await.unwrap();

        let paths: Vec<String> = store.candidates(None).await.unwrap().into_iter().map(|w| w.path).collect();
        assert_eq!(paths, ["/old"]);
    }

    #[tokio::test]
    async fn a_failure_only_notes_the_search() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save("q", "A", 100, &Ok(vec![link("/g/1/", "One")]), false).await.unwrap();
        store.save("q", "A", 200, &Err("HTTP 500".to_string()), false).await.unwrap();

        let state = &store.states("q").await.unwrap()["A"];
        assert_eq!((state.ok, state.error.as_deref(), state.searched_at, state.count), (false, Some("HTTP 500"), 200, 1));
        assert_eq!(store.candidates(None).await.unwrap().len(), 1);

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

        assert_eq!(titles(&store.candidates(Some("comic快楽天")).await.unwrap()), ["COMIC 快楽天 2026年7月号"], "spaces are not in the key");
        assert_eq!(titles(&store.candidates(Some("comicexe")).await.unwrap()), ["COMIC E×E 11"], "nor is the sign");
        assert_eq!(store.candidates(Some("nothing")).await.unwrap().len(), 0);
        assert_eq!(store.candidates(None).await.unwrap().len(), 3);
        // a % or a _ in the prefilter is a character, not a wildcard
        assert_eq!(store.candidates(Some("%")).await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn the_newest_works_come_first() {
        let store = SearchStore::in_memory().await.unwrap();

        store.save("old", "A", 100, &Ok(vec![link("/1", "older")]), false).await.unwrap();
        store.save("new", "A", 200, &Ok(vec![link("/2", "newer")]), false).await.unwrap();
        store.save("same", "A", 200, &Ok(vec![link("/3", "also newer")]), false).await.unwrap();

        assert_eq!(titles(&store.candidates(None).await.unwrap()), ["newer", "also newer", "older"], "ties keep the order they were saved in");
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
        assert_eq!(titles(&again.candidates(None).await.unwrap()), ["One"]);
        drop(again);

        let _ = fs::remove_dir_all(dir);
    }
    /// What `instr` says: the works (the newest first) whose key has the piece.
    async fn by_instr(store: &SearchStore, part: &str, most: usize) -> Vec<String> {
        store.candidates(None).await.unwrap().into_iter().filter(|w| w.title_key.contains(part)).take(most).map(|w| w.path).collect()
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
            let found = paths(store.candidates(Some(part)).await.unwrap()).await;

            assert_eq!(found, by_instr(&store, part, usize::MAX).await, "{part}");
        }
        // the newest first
        assert_eq!(paths(store.candidates(Some("kyoudai")).await.unwrap()).await, ["/g/6/", "/g/1/", "/g/2/"]);
    }

    #[tokio::test]
    async fn a_piece_that_too_many_works_have_is_read_from_the_newest_up_to_the_most() {
        let store = some_works().await;

        // "kyoudai" is in 3 works: with at most 2 it is the newest 2 (by instr, which stops there); with 3 it is all of them (by the index)
        assert_eq!(paths(store.candidates_up_to(Some("kyoudai"), 2).await.unwrap()).await, by_instr(&store, "kyoudai", 2).await);
        assert_eq!(paths(store.candidates_up_to(Some("kyoudai"), 3).await.unwrap()).await, by_instr(&store, "kyoudai", 3).await);
        assert_eq!(store.candidates_up_to(Some("kyoudai"), 3).await.unwrap().len(), 3);
        assert!(store.indexed("kyoudai", 3).await.unwrap().is_some());
        assert!(store.indexed("kyoudai", 2).await.unwrap().is_none(), "3 works have it, and 2 is the most");
        assert!(store.indexed("de", 100).await.unwrap().is_none(), "2 characters are not in the index");
    }

    #[tokio::test]
    async fn the_index_follows_a_new_title_and_a_removed_work() {
        let store = some_works().await;

        // a new romaji of /g/2/ (the same row, another key)
        let work = store.candidates(Some("comic")).await.unwrap().into_iter().find(|w| w.path == "/g/2/").unwrap();
        store.rewrite_romaji(&[(work, "totally different".into())]).await.unwrap();
        assert_eq!(paths(store.candidates(Some("kyoudai")).await.unwrap()).await, ["/g/6/", "/g/1/"]);
        assert_eq!(paths(store.candidates(Some("different")).await.unwrap()).await, ["/g/2/"]);

        // a removed work
        store.db.execute_unprepared("DELETE FROM works WHERE path = '/g/1/'").await.unwrap();
        assert_eq!(paths(store.candidates(Some("kyoudai")).await.unwrap()).await, ["/g/6/"]);
        assert_eq!(paths(store.candidates(Some("shitara")).await.unwrap()).await, ["/g/4/"]);
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
            .execute_unprepared("INSERT INTO works VALUES ('A', '/g/1/', 'Kyoudai', 'kyoudai', 'kyoudai', 1, 1, 'unknown'), ('A', '/g/2/', 'Other', 'other', 'other', 2, 2, 'unknown')")
            .await
            .unwrap();
        drop(store);

        let store = SearchStore::open(&path).await.unwrap();
        assert_eq!(paths(store.candidates(Some("kyoudai")).await.unwrap()).await, ["/g/1/"]);
        assert_eq!(paths(store.candidates(Some("other")).await.unwrap()).await, ["/g/2/"]);

        fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_database_without_the_language_gets_it_read_from_the_titles_when_it_is_opened() {
        let dir = std::env::temp_dir().join(format!("okazuri-language-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("search.db");

        let store = SearchStore::open(&path).await.unwrap();
        store
            .save_crawled("A", 1, &[NewWork { path: "/g/1/".into(), title: "[x] y [中国翻訳]".into(), romaji: "y".into() }, NewWork { path: "/g/2/".into(), title: "plain".into(), romaji: "plain".into() }], None)
            .await
            .unwrap();
        // a database of a version that had no language
        store.db.execute_unprepared("ALTER TABLE works DROP COLUMN language").await.unwrap();
        drop(store);

        let store = SearchStore::open(&path).await.unwrap();
        let works = store.works_after(None, 10).await.unwrap();
        assert_eq!(works.iter().map(|w| (w.path.as_str(), w.language.as_str())).collect::<Vec<_>>(), [("/g/1/", "zh"), ("/g/2/", "unknown")]);

        fs::remove_dir_all(&dir).ok();
    }
}
