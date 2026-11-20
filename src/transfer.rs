//! Moving the works between search databases: `export` writes them to a file, `import` reads such a file.
//!
//! The crawl of N machines (`[crawl] shard = "k/N"`) fills one database on each machine. The works of all of them are put in the database
//! of the server by exporting each one and importing the files there. The file is JSON Lines (a work a line) compressed with
//! brotli. A line has only what the sites gave (the site, the path of the page, the title) and when the work was saved; the romaji, the key
//! and the language of the title are made again when the file is read, with the rules of the program that reads it.
//!
//! A work that is saved already is left as it is (the shards do not overlap, and a work does not change when it is read again).
//! The queries and the translations are not moved: only the works, which is what the crawl made.

use crate::entity::work;
use crate::query::romaji;
use crate::romanizer::Romanizer;
use crate::store::SearchStore;
use crate::utils::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::Arc;

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
    first_seen: i64,
    updated_at: i64,
    /// The language (the flags are taken out of the titles, so it is sent along; a file of an older version has none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    language: Option<String>,
}

impl From<work::Model> for Line {
    fn from(row: work::Model) -> Self {
        Self { website: row.website, path: row.path, title: row.title, first_seen: row.first_seen, updated_at: row.updated_at, language: row.language }
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
pub async fn import(store: &SearchStore, file: &Path, romanizer: &Arc<Romanizer>) -> Result<Moved> {
    let opened = std::fs::File::open(file).map_err(|e| format!("cannot read {}: {e}", file.display()))?;
    let reader = BufReader::new(brotli::Decompressor::new(opened, 64 * 1024));
    let mut moved = Moved::default();
    let mut batch: Vec<Line> = Vec::with_capacity(BATCH);

    for (number, text) in reader.lines().enumerate() {
        let text = text.map_err(|e| format!("{}: {e}", file.display()))?;
        if text.trim().is_empty() {
            continue;
        }
        let line: Line = serde_json::from_str(&text).map_err(|e| format!("{}: line {}: {e}", file.display(), number + 1))?;

        moved.works += 1;
        moved.sites.insert(line.website.clone());
        batch.push(line);

        if batch.len() >= BATCH {
            moved.new += save(store, std::mem::take(&mut batch), romanizer).await?;
        }
    }
        moved.new += save(store, batch, romanizer).await?;

    Ok(moved)
}

/// Makes the romaji and the key of the titles (CPU work, so on a thread of its own) and writes the works.
async fn save(store: &SearchStore, lines: Vec<Line>, romanizer: &Arc<Romanizer>) -> Result<usize> {
    let romanizer = romanizer.clone();
    let works = tokio::task::spawn_blocking(move || {
        lines
            .into_iter()
            .map(|line| {
                // the language is read from the title with its flags; the title is kept without them
                let language = crate::language::detect(&line.title).or(line.language.clone());
                let title = crate::language::strip_flags(&line.title);
                let converted = romaji(&title, &romanizer).unwrap_or_else(|_| title.clone());

                work::Model {
                    website: line.website,
                    path: line.path,
                    title_key: crate::query::title_key(&converted),
                    title_romaji: converted,
                    title,
                    first_seen: line.first_seen,
                    updated_at: line.updated_at,
                    language,
                    deleted: false,
                    checked_at: None,
                }
            })
            .collect::<Vec<_>>()
    })
    .await?;

    store.import_works(works).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewWork;

    fn work(path: &str, title: &str) -> NewWork {
        NewWork { path: path.to_string(), title: title.to_string(), romaji: title.to_string(), language: crate::language::detect(title) }
    }

    async fn filled() -> SearchStore {
        let store = SearchStore::in_memory().await.unwrap();

        store.save_crawled("A", 100, &[work("/a/1", "Alpha one"), work("/a/2", "Alpha two")], None).await.unwrap();
        store.save_crawled("B", 200, &[work("/b/1", "Beta one [English]")], None).await.unwrap();

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
        let imported = import(&to, &file, &crate::testing::romanizer_arc()).await.unwrap();
        assert_eq!((imported.works, imported.new), (3, 3));
        assert_eq!(to.counts().await.unwrap().1, 3);
        assert_eq!(to.candidates(&["alphatwo"]).await.unwrap().len(), 1, "the index follows the imported works");

        let languages: Vec<_> = to.works_after(None, 10).await.unwrap().into_iter().map(|w| (w.path, w.language)).collect();
        assert_eq!(languages, [("/a/1".to_string(), None), ("/a/2".to_string(), None), ("/b/1".to_string(), Some("en".to_string()))], "the language is made on import: it is not in the file");

        // the same file again changes nothing
        let again = import(&to, &file, &crate::testing::romanizer_arc()).await.unwrap();
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

        assert!(import(&SearchStore::in_memory().await.unwrap(), &file, &crate::testing::romanizer_arc()).await.is_err());

        let _ = std::fs::remove_file(file);
    }
}
