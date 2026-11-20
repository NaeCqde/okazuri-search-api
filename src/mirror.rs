//! The mirror (`[d1] mirror_path`): a copy of the magazine tables of the Worker's D1 database (`magazines` and
//! `numbers`), rebuilt from the SQL file that D1 exports whenever the data changed. It is only read, to know which
//! issues to search for ahead of time. The tables are the entities `magazine` and `number`; `mirror_meta` keeps the
//! bookmark of the data the copy was built from.
//!
//! The bookmark of D1 moves with a write to *any* table (the Worker writes its works all day), so a moved bookmark does
//! not mean that `magazines` and `numbers` changed. The mirror keeps a hash of the SQL it was built from too
//! (`content_hash`): an export that has the same hash is not loaded again.
//!
//! The file is built under another name and replaces the old one in one step, so a broken export leaves the old copy
//! alone. A reader opens the file for as long as it reads, and closes it. Windows does not replace a file that is open,
//! so the readers and the replacing take turns (`MIRROR`): the long part, loading the export, is done in the other file
//! with no lock, and only the moving of the file waits for the readers.

use crate::entity::{catalog_author, catalog_work, magazine, mirror_meta, number};
use crate::utils::Result;
use sea_orm::sea_query::OnConflict;
use sea_orm::sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, Schema, Set,
    SqlxSqliteConnector,
};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::RwLock;

/// Many readers at once, or the one that replaces the file. (Another program that has the file open is not covered; the
/// replacing tries a few times for it.)
static MIRROR: RwLock<()> = RwLock::const_new(());

/// How many times the replacing is tried, and the wait between two tries.
const REPLACE_TRIES: u32 = 10;
const REPLACE_WAIT: Duration = Duration::from_millis(300);

/// A magazine issue of the catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    pub id: String,
    /// The name every issue of a magazine shares (`COMIC快楽天 2026年07月号`).
    pub name: String,
    pub magazine_id: String,
    pub magazine_name: String,
    /// The release date, in seconds since the epoch.
    pub date: i64,
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".tmp");

    path.with_file_name(name)
}

/// One connection to the file: the mirror is small, and a copy that is being built must not be shared.
async fn connect(options: SqliteConnectOptions) -> Result<DatabaseConnection> {
    let pool = SqlitePoolOptions::new().max_connections(1).connect_with(options).await?;

    Ok(SqlxSqliteConnector::from_sqlx_sqlite_pool(pool))
}

/// A hash of the exported SQL (FNV-1a, 64 bits; the same on every run). Two exports of the same data have the same one.
pub fn content_hash(sql: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;

    for byte in sql.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }

    format!("{hash:016x}")
}

/// The SQL without its `CREATE TRIGGER ... END;` statements. The Worker's `works` table has triggers that feed
/// `works_fts`, which is not exported (D1 refuses the virtual table); in the copy they would fail on every insert.
fn strip_triggers(sql: &str) -> String {
    let mut kept = String::with_capacity(sql.len());
    let mut in_trigger = false;

    for line in sql.split_inclusive('\n') {
        let trimmed = line.trim();

        if !in_trigger && trimmed.to_ascii_uppercase().starts_with("CREATE TRIGGER") {
            in_trigger = true;
        }

        if in_trigger {
            // the body ends with `END;` (a statement inside it ends with `;` too, but never with `END;`)
            if trimmed.to_ascii_uppercase().ends_with("END;") {
                in_trigger = false;
            }
            continue;
        }

        kept.push_str(line);
    }

    kept
}

/// Builds the mirror from the SQL that D1 exported, and replaces the old file with it in one step.
/// The bookmark of the data is kept in it, to know later whether the data changed.
pub async fn rebuild_mirror(path: &Path, sql: &str, bookmark: &str, now: i64) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        tokio::fs::create_dir_all(parent).await.map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }

    let temporary = temporary_path(path);
    let _ = tokio::fs::remove_file(&temporary).await;

    let options = SqliteConnectOptions::new()
        .filename(&temporary)
        .create_if_missing(true)
        // one file, without a journal beside it, so that it can be moved
        .journal_mode(SqliteJournalMode::Delete)
        // the export lists the tables in an order that is right for D1; the keys are not checked here
        .foreign_keys(false);
    let db = connect(options).await.map_err(|e| format!("cannot create {}: {e}", temporary.display()))?;

    let loaded = load(&db, sql, bookmark, now).await;

    // the file is closed before it is moved (Windows does not move an open file)
    let _ = db.close().await;
    let issues = loaded?;
    log::info!("mirror: {issues} issues loaded");

    replace(&temporary, path).await?;

    Ok(())
}

/// Moves the new file over the old one (there is no moment without a mirror), when nobody is reading it.
async fn replace(temporary: &Path, path: &Path) -> Result<()> {
    let mut tries = 0;

    loop {
        let moved = {
            let _writing = MIRROR.write().await;

            tokio::fs::rename(temporary, path).await
        };

        match moved {
            Ok(()) => return Ok(()),
            Err(e) if tries + 1 >= REPLACE_TRIES => {
                return Err(format!("cannot replace {}: {e}", path.display()).into());
            }
            Err(e) => {
                tries += 1;
                log::debug!("mirror: cannot replace {} yet ({e}), trying again", path.display());
                tokio::time::sleep(REPLACE_WAIT).await;
            }
        }
    }
}

/// Runs the export in the new file and notes where it comes from. Returns how many issues it has.
async fn load(db: &DatabaseConnection, sql: &str, bookmark: &str, now: i64) -> Result<u64> {
    db.execute_unprepared(&strip_triggers(sql)).await.map_err(|e| format!("cannot load the exported SQL: {e}"))?;

    let backend = db.get_database_backend();
    let mut table = Schema::new(backend).create_table_from_entity(mirror_meta::Entity);
    table.if_not_exists();
    db.execute(backend.build(&table)).await?;

    set_meta(db, "bookmark", bookmark).await?;
    set_meta(db, "synced_at", &now.to_string()).await?;
    set_meta(db, "content_hash", &content_hash(sql)).await?;

    // a file without the issues is not a mirror: the old one is kept
    number::Entity::find()
        .count(db)
        .await
        .map_err(|e| format!("the exported SQL has no numbers table: {e}").into())
}

async fn set_meta(db: &DatabaseConnection, key: &str, value: &str) -> Result<()> {
    mirror_meta::Entity::insert(mirror_meta::ActiveModel { key: Set(key.to_string()), value: Set(value.to_string()) })
        .on_conflict(OnConflict::column(mirror_meta::Column::Key).update_column(mirror_meta::Column::Value).to_owned())
        .exec_without_returning(db)
        .await?;

    Ok(())
}

/// The mirror, open for reading. `None` when there is no mirror yet.
async fn open(path: &Path) -> Result<Option<DatabaseConnection>> {
    if !path.exists() {
        return Ok(None);
    }

    let db = connect(SqliteConnectOptions::new().filename(path).read_only(true))
        .await
        .map_err(|e| format!("cannot open {}: {e}", path.display()))?;

    Ok(Some(db))
}

/// What the mirror says about itself (`bookmark`, `content_hash`). `None` when there is no mirror yet, or it does not say.
async fn meta_value(path: &Path, key: &str) -> Result<Option<String>> {
    let _reading = MIRROR.read().await;
    let Some(db) = open(path).await? else { return Ok(None) };

    // a file that has no meta table (it was not built here) says nothing
    let row = mirror_meta::Entity::find_by_id(key).one(&db).await.ok().flatten();
    let _ = db.close().await;

    Ok(row.map(|row| row.value))
}

/// The bookmark of the data the mirror was built from. `None` when there is no mirror yet.
pub async fn mirror_bookmark(path: &Path) -> Result<Option<String>> {
    meta_value(path, "bookmark").await
}

/// The hash of the SQL the mirror was built from. `None` when there is no mirror yet.
pub async fn mirror_content_hash(path: &Path) -> Result<Option<String>> {
    meta_value(path, "content_hash").await
}

/// The data is the same, but D1 is at another bookmark now: the mirror keeps the new one, so that it is not exported
/// again for the same bookmark.
pub async fn mirror_note_bookmark(path: &Path, bookmark: &str, now: i64) -> Result<()> {
    let _writing = MIRROR.write().await;
    let options = SqliteConnectOptions::new().filename(path).journal_mode(SqliteJournalMode::Delete);
    let db = connect(options).await.map_err(|e| format!("cannot open {}: {e}", path.display()))?;

    let noted = async {
        set_meta(&db, "bookmark", bookmark).await?;
        set_meta(&db, "synced_at", &now.to_string()).await
    }
    .await;
    let _ = db.close().await;

    noted
}

/// Every issue, the newest first.
pub async fn mirror_issues(path: &Path) -> Result<Vec<Issue>> {
    let _reading = MIRROR.read().await;
    let Some(db) = open(path).await? else { return Ok(Vec::new()) };

    let rows = number::Entity::find()
        .find_also_related(magazine::Entity)
        .order_by_desc(number::Column::Date)
        .order_by_asc(number::Column::Id)
        .all(&db)
        .await;
    let _ = db.close().await;

    Ok(rows?
        .into_iter()
        .map(|(issue, magazine)| Issue {
            id: issue.id,
            name: issue.name,
            magazine_id: issue.magazine_id,
            magazine_name: magazine.map(|m| m.name).unwrap_or_default(),
            date: issue.date,
        })
        .collect())
}

/// A magazine of the catalogue, with how many issues the copy has and the newest one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MagazineSummary {
    pub id: String,
    pub name: String,
    pub issues: u64,
    /// The newest issue; `None` for a magazine that has no issue yet.
    pub latest: Option<Issue>,
}

/// Every magazine of the copy, by name. `None` when there is no copy yet.
pub async fn mirror_magazines(path: &Path) -> Result<Option<Vec<MagazineSummary>>> {
    // the issues come newest first, so the first one of a magazine is its latest
    let issues = mirror_issues(path).await?;
    let _reading = MIRROR.read().await;
    let Some(db) = open(path).await? else { return Ok(None) };
    let magazines = magazine::Entity::find().order_by_asc(magazine::Column::Name).order_by_asc(magazine::Column::Id).all(&db).await;
    let _ = db.close().await;

    Ok(Some(
        magazines?
            .into_iter()
            .map(|m| {
                let own = issues.iter().filter(|i| i.magazine_id == m.id);
                MagazineSummary { issues: own.clone().count() as u64, latest: own.cloned().next(), id: m.id, name: m.name }
            })
            .collect(),
    ))
}

/// A work of an issue (its issue is known by the caller).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueWork {
    pub id: String,
    pub name: String,
    pub author: String,
    pub position: i64,
}

/// The works of the issue with this name (or id), in the order of the issue. The outer `None`: no copy, or a copy
/// without works yet; the inner `None`: no such issue.
#[allow(clippy::type_complexity)]
pub async fn mirror_issue_works(path: &Path, name: &str) -> Result<Option<Option<(Issue, Vec<IssueWork>)>>> {
    let _reading = MIRROR.read().await;
    let Some(db) = open(path).await? else { return Ok(None) };

    let issue = number::Entity::find()
        .filter(number::Column::Name.eq(name))
        .order_by_desc(number::Column::Date)
        .one(&db)
        .await?;
    let issue = match issue {
        Some(issue) => Some(issue),
        None => number::Entity::find_by_id(name.to_string()).one(&db).await?,
    };
    let Some(issue) = issue else {
        let _ = db.close().await;
        return Ok(Some(None));
    };

    let rows = catalog_work::Entity::find()
        .filter(catalog_work::Column::NumberId.eq(issue.id.clone()))
        .order_by_asc(catalog_work::Column::Position)
        .order_by_asc(catalog_work::Column::Id)
        .all(&db)
        .await;
    let rows = match rows {
        Ok(rows) => rows,
        Err(e) if e.to_string().contains("no such table") => {
            let _ = db.close().await;
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };

    let magazine = magazine::Entity::find_by_id(issue.magazine_id.clone()).one(&db).await?;
    let mut works = Vec::with_capacity(rows.len());

    for work in rows {
        let author = catalog_author::Entity::find_by_id(work.author_id).one(&db).await?;
        works.push(IssueWork { id: work.id, name: work.name, author: author.map(|a| a.name).unwrap_or_default(), position: work.position });
    }

    let _ = db.close().await;

    let issue = Issue {
        id: issue.id,
        name: issue.name,
        magazine_id: issue.magazine_id,
        magazine_name: magazine.map(|m| m.name).unwrap_or_default(),
        date: issue.date,
    };

    Ok(Some(Some((issue, works))))
}

/// A work printed in an issue, with the issue and the magazine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintedWork {
    pub id: String,
    pub name: String,
    pub author: String,
    /// The place in the issue: 1, 2, 3, ...
    pub position: i64,
    pub issue: Issue,
}

/// The most works `mirror_find_works` returns, whatever is asked.
pub const MAX_PRINTED_WORKS: u64 = 50;

/// The works that have exactly this title, in the issues they were printed in, the newest issue first (a title can be
/// printed more than once). `None` when the copy has no works: there is no copy yet, or it was built before the
/// `works` and `authors` tables were exported too.
pub async fn mirror_find_works(path: &Path, title: &str, limit: u64) -> Result<Option<Vec<PrintedWork>>> {
    let _reading = MIRROR.read().await;
    let Some(db) = open(path).await? else { return Ok(None) };

    let rows = catalog_work::Entity::find()
        .filter(catalog_work::Column::Name.eq(title))
        .find_also_related(number::Entity)
        .order_by_desc(number::Column::Date)
        .order_by_asc(catalog_work::Column::Position)
        .limit(limit.clamp(1, MAX_PRINTED_WORKS))
        .all(&db)
        .await;

    let found = match rows {
        Ok(rows) => rows,
        Err(e) => {
            // a copy from before the works were exported has no such table
            let _ = db.close().await;
            return if e.to_string().contains("no such table") { Ok(None) } else { Err(e.into()) };
        }
    };

    let mut works = Vec::with_capacity(found.len());

    for (work, issue) in found {
        let Some(issue) = issue else { continue };
        let magazine = magazine::Entity::find_by_id(issue.magazine_id.clone()).one(&db).await?;
        let author = catalog_author::Entity::find_by_id(work.author_id).one(&db).await?;

        works.push(PrintedWork {
            id: work.id,
            name: work.name,
            author: author.map(|a| a.name).unwrap_or_default(),
            position: work.position,
            issue: Issue {
                id: issue.id,
                name: issue.name,
                magazine_id: issue.magazine_id,
                magazine_name: magazine.map(|m| m.name).unwrap_or_default(),
                date: issue.date,
            },
        });
    }

    let _ = db.close().await;

    Ok(Some(works))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const EXPORT: &str = "PRAGMA defer_foreign_keys=TRUE;
CREATE TABLE `magazines` (`id` text PRIMARY KEY NOT NULL, `name` text NOT NULL, `cursor_number_id` text, `completed` integer DEFAULT false NOT NULL);
CREATE TABLE `numbers` (`id` text PRIMARY KEY NOT NULL, `name` text NOT NULL, `num` integer NOT NULL, `date` integer NOT NULL, `magazine_id` text NOT NULL, `name_raw` text DEFAULT '' NOT NULL, FOREIGN KEY (`magazine_id`) REFERENCES `magazines`(`id`));
INSERT INTO \"magazines\" (\"id\",\"name\",\"cursor_number_id\",\"completed\") VALUES('LBL1','COMIC快楽天',NULL,1);
INSERT INTO \"numbers\" (\"id\",\"name\",\"num\",\"date\",\"magazine_id\",\"name_raw\") VALUES('BJ1','COMIC快楽天 2026年06月号',202606,1000,'LBL1','x');
INSERT INTO \"numbers\" (\"id\",\"name\",\"num\",\"date\",\"magazine_id\",\"name_raw\") VALUES('BJ2','COMIC快楽天 2026年07月号',202607,2000,'LBL1','y');
INSERT INTO \"numbers\" (\"id\",\"name\",\"num\",\"date\",\"magazine_id\",\"name_raw\") VALUES('BJ3','Lost label',202607,1500,'GONE','z');
";

    const WORKS: &str = "CREATE TABLE `authors` (`id` text PRIMARY KEY NOT NULL, `name` text NOT NULL);
CREATE TABLE `works` (`id` text PRIMARY KEY NOT NULL, `name` text NOT NULL, `date` integer NOT NULL, `position` integer DEFAULT 0 NOT NULL, `number_id` text NOT NULL, `author_id` text NOT NULL);
CREATE TRIGGER `works_fts_ai` AFTER INSERT ON `works` BEGIN
    INSERT INTO `works_fts`(rowid, name) VALUES (new.rowid, new.name);
END;
INSERT INTO \"authors\" (\"id\",\"name\") VALUES('A1','Someone');
INSERT INTO \"works\" (\"id\",\"name\",\"date\",\"position\",\"number_id\",\"author_id\") VALUES('W1','Same title',1000,3,'BJ1','A1');
INSERT INTO \"works\" (\"id\",\"name\",\"date\",\"position\",\"number_id\",\"author_id\") VALUES('W2','Same title',2000,1,'BJ2','A1');
INSERT INTO \"works\" (\"id\",\"name\",\"date\",\"position\",\"number_id\",\"author_id\") VALUES('W3','Other',2000,2,'BJ2','GONE');
";

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("okazuri-search-api-test-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[tokio::test]
    async fn the_mirror_is_built_from_the_exported_sql_and_read_newest_first() {
        let dir = temp_dir("mirror");
        let path = dir.join("d1-mirror.db");

        assert_eq!(mirror_bookmark(&path).await.unwrap(), None, "no mirror yet");
        assert!(mirror_issues(&path).await.unwrap().is_empty());

        rebuild_mirror(&path, EXPORT, "bm-1", 5).await.unwrap();

        assert_eq!(mirror_bookmark(&path).await.unwrap().as_deref(), Some("bm-1"));
        let issues = mirror_issues(&path).await.unwrap();
        assert_eq!(
            issues.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            ["COMIC快楽天 2026年07月号", "Lost label", "COMIC快楽天 2026年06月号"]
        );
        assert_eq!(issues[0].magazine_name, "COMIC快楽天");
        assert_eq!(issues[1].magazine_name, "", "an issue whose magazine is not in the copy is still an issue");
        assert!(!temporary_path(&path).exists(), "the temporary file is gone");

        // a second build replaces the first
        rebuild_mirror(&path, EXPORT, "bm-2", 6).await.unwrap();
        assert_eq!(mirror_bookmark(&path).await.unwrap().as_deref(), Some("bm-2"));

        let _ = fs::remove_dir_all(dir);
    }

    /// The prefetch reads the mirror while the sync replaces it.
    #[tokio::test]
    async fn the_mirror_can_be_read_while_it_is_replaced() {
        let dir = temp_dir("race");
        let path = dir.join("d1-mirror.db");
        rebuild_mirror(&path, EXPORT, "bm-0", 1).await.unwrap();

        let reader_path = path.clone();
        let reader = tokio::spawn(async move {
            for _ in 0..200 {
                assert_eq!(mirror_issues(&reader_path).await.unwrap().len(), 3, "never half a mirror, never no mirror");
                assert!(mirror_bookmark(&reader_path).await.unwrap().is_some());
            }
        });

        for n in 1..=15 {
            rebuild_mirror(&path, EXPORT, &format!("bm-{n}"), n).await.unwrap();
        }
        reader.await.unwrap();

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_content_hash_tells_the_same_data_and_the_bookmark_can_be_noted_alone() {
        let dir = temp_dir("hash");
        let path = dir.join("d1-mirror.db");

        assert_eq!(content_hash(EXPORT), content_hash(EXPORT));
        assert_ne!(content_hash(EXPORT), content_hash(&EXPORT.replace("2000", "2001")));
        assert_eq!(mirror_content_hash(&path).await.unwrap(), None);

        rebuild_mirror(&path, EXPORT, "bm-1", 1).await.unwrap();
        assert_eq!(mirror_content_hash(&path).await.unwrap(), Some(content_hash(EXPORT)));

        mirror_note_bookmark(&path, "bm-9", 2).await.unwrap();
        assert_eq!(mirror_bookmark(&path).await.unwrap().as_deref(), Some("bm-9"));
        assert_eq!(mirror_content_hash(&path).await.unwrap(), Some(content_hash(EXPORT)), "the data is the one it was");
        assert_eq!(mirror_issues(&path).await.unwrap().len(), 3);

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_broken_export_leaves_the_old_mirror_alone() {
        let dir = temp_dir("broken");
        let path = dir.join("d1-mirror.db");

        rebuild_mirror(&path, EXPORT, "good", 1).await.unwrap();
        assert!(rebuild_mirror(&path, "CREATE TABLE other (x);", "bad", 2).await.is_err(), "no numbers table");
        assert!(rebuild_mirror(&path, "THIS IS NOT SQL", "bad", 2).await.is_err());

        assert_eq!(mirror_bookmark(&path).await.unwrap().as_deref(), Some("good"));
        assert_eq!(mirror_issues(&path).await.unwrap().len(), 3);

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_works_are_found_by_their_title_with_their_issue_newest_first() {
        let dir = temp_dir("works");
        let path = dir.join("d1-mirror.db");

        assert_eq!(mirror_find_works(&path, "Same title", 10).await.unwrap(), None, "no mirror yet");

        // the old copy has no works: nothing to answer with, which is not the same as no such work
        rebuild_mirror(&path, EXPORT, "bm-1", 1).await.unwrap();
        assert_eq!(mirror_find_works(&path, "Same title", 10).await.unwrap(), None);

        // the triggers of the export are dropped: they would feed a works_fts that the copy does not have
        rebuild_mirror(&path, &format!("{EXPORT}{WORKS}"), "bm-2", 2).await.unwrap();

        let found = mirror_find_works(&path, "Same title", 10).await.unwrap().unwrap();
        assert_eq!(found.iter().map(|w| (w.id.as_str(), w.issue.name.as_str(), w.position)).collect::<Vec<_>>(), [
            ("W2", "COMIC快楽天 2026年07月号", 1),
            ("W1", "COMIC快楽天 2026年06月号", 3)
        ]);
        assert_eq!((found[0].author.as_str(), found[0].issue.magazine_name.as_str()), ("Someone", "COMIC快楽天"));
        assert_eq!(mirror_find_works(&path, "Same title", 1).await.unwrap().unwrap().len(), 1, "limit");
        assert_eq!(mirror_find_works(&path, "same title", 10).await.unwrap().unwrap().len(), 0, "the title is exact");
        // a work whose author is not in the copy is still a work
        assert_eq!(mirror_find_works(&path, "Other", 10).await.unwrap().unwrap()[0].author, "");

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_magazines_are_listed_with_their_issue_count_and_newest_issue() {
        let dir = temp_dir("magazines");
        let path = dir.join("d1-mirror.db");

        assert_eq!(mirror_magazines(&path).await.unwrap(), None, "no mirror yet");
        rebuild_mirror(&path, &format!("{EXPORT}INSERT INTO \"magazines\" (\"id\",\"name\") VALUES('LBL2','Empty');"), "bm", 1).await.unwrap();

        let magazines = mirror_magazines(&path).await.unwrap().unwrap();
        assert_eq!(magazines.iter().map(|m| (m.name.as_str(), m.issues)).collect::<Vec<_>>(), [("COMIC快楽天", 2), ("Empty", 0)]);
        assert_eq!(magazines[0].latest.as_ref().unwrap().name, "COMIC快楽天 2026年07月号");
        assert_eq!(magazines[1].latest, None);

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn only_the_trigger_statements_are_dropped() {
        let sql = "CREATE TABLE a (x);\nCREATE TRIGGER t AFTER INSERT ON a BEGIN\n INSERT INTO f VALUES (1);\nEND;\nINSERT INTO a VALUES (1);\n";
        assert_eq!(strip_triggers(sql), "CREATE TABLE a (x);\nINSERT INTO a VALUES (1);\n");
    }
}
