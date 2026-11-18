//! Moving the works between search databases: `export` writes them to a file, `import` reads such a file.
//!
//! The crawl of N machines (`[crawl] shard = "k/N"`) fills one database on each machine. The works of all of them are put in the database
//! of the server by exporting each one and importing the files there. The file is JSON Lines (a work a line) compressed with
//! brotli, which is what the titles of one site are good for: the files are about a fifth of the size of the text.
//!
//! A work that is saved already is left as it is (the shards do not overlap, and a work does not change when it is read again).
//! The queries and the translations are not moved: only the works, which is what the crawl made.

use crate::entity::work;
use crate::store::SearchStore;
use crate::utils::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;

/// The works read from the table at a time.
const PAGE: u64 = 5_000;
/// The works written to the table in one transaction.
const BATCH: usize = 2_000;
/// brotli: 0 to 11. 9 is much smaller than gzip at a speed that does not matter here.
const QUALITY: u32 = 9;
const WINDOW_BITS: u32 = 22;

/// A line of the file.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Line {
    website: String,
    path: String,
    title: String,
    romaji: String,
    first_seen: i64,
    updated_at: i64,
}

impl From<work::Model> for Line {
    fn from(row: work::Model) -> Self {
        Self { website: row.website, path: row.path, title: row.title, romaji: row.title_romaji, first_seen: row.first_seen, updated_at: row.updated_at }
    }
}

/// What an export or an import did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Moved {
    /// Works written to the file, or read from it.
    pub works: usize,
    /// Works that were not in the database yet (import only).
    pub new: usize,
    /// The sites of those works.
    pub sites: HashSet<String>,
}

/// Writes the works of `sites` (all of them when empty) that were saved at `since` (unix seconds) or later to `out`.
pub async fn export(store: &SearchStore, out: &Path, sites: &[String], since: i64) -> Result<Moved> {
    let file = std::fs::File::create(out).map_err(|e| format!("cannot write {}: {e}", out.display()))?;
    let mut writer = brotli::CompressorWriter::new(file, 64 * 1024, QUALITY, WINDOW_BITS);
    let wanted: HashSet<String> = sites.iter().map(|site| site.to_lowercase()).collect();
    let mut moved = Moved::default();
    let mut after = None;

    loop {
        let page = store.works_after(after.as_ref(), PAGE).await?;
        let Some(last) = page.last() else { break };
        after = Some((last.website.clone(), last.path.clone()));

        for row in page {
            if (!wanted.is_empty() && !wanted.contains(&row.website.to_lowercase())) || row.updated_at < since {
                continue;
            }
            moved.sites.insert(row.website.clone());
            moved.works += 1;
            serde_json::to_writer(&mut writer, &Line::from(row))?;
            writer.write_all(b"\n")?;
        }
    }

    // the end of the stream is written when the writer is dropped; flush so that an error shows
    writer.flush()?;
    drop(writer);

    Ok(moved)
}

/// Reads the works of a file into the database.
pub async fn import(store: &SearchStore, file: &Path) -> Result<Moved> {
    let opened = std::fs::File::open(file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let reader = BufReader::new(brotli::Decompressor::new(opened, 64 * 1024));
    let mut moved = Moved::default();
    let mut batch = Vec::with_capacity(BATCH);

    for (number, text) in reader.lines().enumerate() {
        let text = text.map_err(|e| format!("{}: {e}", file.display()))?;
        if text.trim().is_empty() {
            continue;
        }
        let line: Line = serde_json::from_str(&text).map_err(|e| format!("{}: line {}: {e}", file.display(), number + 1))?;

        moved.works += 1;
        moved.sites.insert(line.website.clone());
        batch.push(work::Model {
            website: line.website,
            path: line.path,
            title: line.title,
            title_romaji: line.romaji.clone(),
            title_key: crate::query::title_key(&line.romaji),
            first_seen: line.first_seen,
            updated_at: line.updated_at,
        });

        if batch.len() >= BATCH {
            moved.new += store.import_works(std::mem::take(&mut batch)).await?;
        }
    }
    moved.new += store.import_works(batch).await?;

    Ok(moved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewWork;

    fn work(path: &str, title: &str) -> NewWork {
        NewWork { path: path.to_string(), title: title.to_string(), romaji: title.to_string() }
    }

    async fn filled() -> SearchStore {
        let store = SearchStore::in_memory().await.unwrap();

        store.save_crawled("A", 100, &[work("/a/1", "Alpha one"), work("/a/2", "Alpha two")], None).await.unwrap();
        store.save_crawled("B", 200, &[work("/b/1", "Beta one")], None).await.unwrap();

        store
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("okazuri-transfer-{}-{name}", std::process::id()))
    }

    #[tokio::test]
    async fn the_works_go_through_a_file_into_another_database_and_can_be_found() {
        let from = filled().await;
        let file = scratch("all.jsonl.br");

        let exported = export(&from, &file, &[], 0).await.unwrap();
        assert_eq!(exported.works, 3);

        let to = SearchStore::in_memory().await.unwrap();
        let imported = import(&to, &file).await.unwrap();
        assert_eq!((imported.works, imported.new), (3, 3));
        assert_eq!(to.counts().await.unwrap().1, 3);
        assert_eq!(to.candidates(Some("alphatwo")).await.unwrap().len(), 1, "the index follows the imported works");

        // the same file again changes nothing
        let again = import(&to, &file).await.unwrap();
        assert_eq!((again.works, again.new), (3, 0));
        assert_eq!(to.counts().await.unwrap().1, 3);

        let _ = std::fs::remove_file(file);
    }

    #[tokio::test]
    async fn an_export_can_be_limited_to_sites_and_to_a_time() {
        let from = filled().await;
        let file = scratch("b.jsonl.br");

        let only_b = export(&from, &file, &["b".to_string()], 0).await.unwrap();
        assert_eq!((only_b.works, only_b.sites.len()), (1, 1));

        let recent = export(&from, &file, &[], 150).await.unwrap();
        assert_eq!(recent.works, 1, "the works saved at 150 or later");

        let _ = std::fs::remove_file(file);
    }

    #[tokio::test]
    async fn a_file_that_is_not_an_export_is_an_error() {
        let file = scratch("bad.jsonl.br");
        std::fs::write(&file, b"not brotli at all").unwrap();

        assert!(import(&SearchStore::in_memory().await.unwrap(), &file).await.is_err());

        let _ = std::fs::remove_file(file);
    }
}
