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

use crate::entity::{magazine, mirror_meta, number};
use crate::utils::Result;
use sea_orm::sea_query::OnConflict;
use sea_orm::sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, EntityTrait, PaginatorTrait, QueryOrder, Schema, Set, SqlxSqliteConnector,
};
use std::fs;
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

/// Builds the mirror from the SQL that D1 exported, and replaces the old file with it in one step.
/// The bookmark of the data is kept in it, to know later whether the data changed.
pub async fn rebuild_mirror(path: &Path, sql: &str, bookmark: &str, now: i64) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent).map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }

    let temporary = temporary_path(path);
    let _ = fs::remove_file(&temporary);

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

            fs::rename(temporary, path)
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
    db.execute_unprepared(sql).await.map_err(|e| format!("cannot load the exported SQL: {e}"))?;

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

#[cfg(test)]
mod tests {
    use super::*;

    const EXPORT: &str = "PRAGMA defer_foreign_keys=TRUE;
CREATE TABLE `magazines` (`id` text PRIMARY KEY NOT NULL, `name` text NOT NULL, `cursor_number_id` text, `completed` integer DEFAULT false NOT NULL);
CREATE TABLE `numbers` (`id` text PRIMARY KEY NOT NULL, `name` text NOT NULL, `num` integer NOT NULL, `date` integer NOT NULL, `magazine_id` text NOT NULL, `name_raw` text DEFAULT '' NOT NULL, FOREIGN KEY (`magazine_id`) REFERENCES `magazines`(`id`));
INSERT INTO \"magazines\" (\"id\",\"name\",\"cursor_number_id\",\"completed\") VALUES('LBL1','COMIC快楽天',NULL,1);
INSERT INTO \"numbers\" (\"id\",\"name\",\"num\",\"date\",\"magazine_id\",\"name_raw\") VALUES('BJ1','COMIC快楽天 2026年06月号',202606,1000,'LBL1','x');
INSERT INTO \"numbers\" (\"id\",\"name\",\"num\",\"date\",\"magazine_id\",\"name_raw\") VALUES('BJ2','COMIC快楽天 2026年07月号',202607,2000,'LBL1','y');
INSERT INTO \"numbers\" (\"id\",\"name\",\"num\",\"date\",\"magazine_id\",\"name_raw\") VALUES('BJ3','Lost label',202607,1500,'GONE','z');
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
}
