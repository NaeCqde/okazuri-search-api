//! The search database (`[search] db_path`): the works the sites listed, and when each query was asked to
//! each site. Made and written by the engine. The tables are the entities `work` and `search`.
//!
//! A work is what a list page shows of it: the title as the site writes it, and the path of its page.
//! No image, and no address of an image.
//!
//! (The other SQLite file, the copy of D1, is `mirror.rs`.)

use crate::entity::{listing, search, work};
use crate::query::title_key;
use crate::utils::Result;
use sea_orm::sea_query::{Expr, Index, OnConflict};
use sea_orm::sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, IntoActiveModel, PaginatorTrait, QueryFilter,
    QueryOrder, QuerySelect, Schema, Set, SqlxSqliteConnector, TransactionTrait,
};
use std::collections::{HashMap, HashSet};
use std::fs;
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
            fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
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
        db.execute(backend.build(works.if_not_exists())).await?;
        db.execute(backend.build(searches.if_not_exists())).await?;
        db.execute(backend.build(listings.if_not_exists())).await?;

        let by_update = Index::create()
            .name("works_updated_at")
            .table(work::Entity)
            .col(work::Column::UpdatedAt)
            .if_not_exists()
            .to_owned();
        db.execute(backend.build(&by_update)).await?;

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
                }),
                Some(known) if known.title != item.title || known.title_romaji != item.romaji => {
                    let mut changed = known.clone().into_active_model();
                    changed.title = Set(item.title.clone());
                    changed.title_romaji = Set(item.romaji.clone());
                    changed.title_key = Set(title_key(&item.romaji));
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

    /// The works whose title key has `prefilter` in it (all works without one), the most recently saved first.
    /// This only narrows the rows; the caller still matches every word of its query.
    pub async fn candidates(&self, prefilter: Option<&str>) -> Result<Vec<Work>> {
        let mut query = work::Entity::find();

        if let Some(part) = prefilter {
            query = query.filter(Expr::cust_with_values("instr(title_key, ?) > 0", [part]));
        }

        Ok(query
            .order_by_desc(work::Column::UpdatedAt)
            // works saved at the same second keep the order they were saved in
            .order_by_asc(Expr::cust("rowid"))
            .limit(MAX_CANDIDATES)
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
            ["website", "path", "title", "title_romaji", "title_key", "first_seen", "updated_at"]
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
}
