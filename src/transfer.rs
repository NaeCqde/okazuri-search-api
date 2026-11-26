//! Moving the works between search databases: `export` writes them to a file, `import` reads such a file.
//!
//! The crawl of N machines (`[crawl] shard = "k/N"`) fills one database on each machine. The works of all of them are put in the database
//! of the server by exporting each one and importing the files there. The file is JSON Lines (a work a line) compressed with
//! brotli. A line has only what the sites gave (the site, the path of the page, the title) and when the work was saved; the romaji, the key
//! and the language of the title are made again when the file is read, with the rules of the program that reads it.
//!
//! A work that is saved already is left as it is (the shards do not overlap, and a work does not change when it is read again), except that
//! its tags, account and posted date are set when the file has them (a database filled before the tags were read). The tags go by name and kind,
//! the ids are the ids of one database; a file of an older version has none.
//! The names of the numbers that AsmHentai and HentaiEra give their tags (`site_tag_names`) go in the file too, before the works.
//! The queries and the translations are not moved: only the works, which is what the crawl made.

use crate::entity::work;
use crate::query::romaji;
use crate::romanizer::Romanizer;
use crate::sites::{Account, Extra, Tag, TagKind};
use crate::store::{NewWork, SearchStore};
use crate::utils::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
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
    /// The language (the flags are taken out of the titles, so it is sent along).
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<String>,
    /// The tags by name (the ids are the ids of one database), with their kind (`sites::TagKind`). A file of an older version has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tags: Vec<(String, i64)>,
    /// The account that posted the work: its id on the site and its name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    uploader: Option<(String, String)>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    posted_at: Option<i64>,
}

/// A line of a file: a work, or the name of a number that a site gives its tags (`site_tag_names`, which AsmHentai and HentaiEra need to name
/// the tags of their works). A file of an older version has works only.
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum Entry {
    Name { site_tag_name: (String, String, u64, String) },
    Work(Line),
}

/// The tags and the accounts of a database, to write the names in place of the ids.
struct Names {
    tags: HashMap<i64, (String, i64)>,
    uploaders: HashMap<(String, String), String>,
}

impl Line {
    fn new(row: work::Model, names: &Names) -> Self {
        let tags = row
            .tags
            .as_deref()
            .into_iter()
            .flat_map(str::split_whitespace)
            .filter_map(|id| names.tags.get(&id.parse::<i64>().ok()?).cloned())
            .collect();
        let uploader = row.uploader.as_ref().map(|id| (id.clone(), names.uploaders.get(&(row.website.clone(), id.clone())).cloned().unwrap_or_else(|| id.clone())));

        Self { website: row.website, path: row.path, title: row.title, first_seen: row.first_seen, updated_at: row.updated_at, language: row.language, tags, uploader, posted_at: row.posted_at }
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
    let mut allowed: Option<(String, bool)> = None;
    let names = Names { tags: store.tag_table().await?, uploaders: store.uploader_table().await? };

    // the names of the numbers of the sites come first
    for name in store.all_site_tag_names().await? {
        if wanted.is_empty() || wanted.contains(&name.0.to_lowercase()) {
            serde_json::to_writer(&mut writer, &Entry::Name { site_tag_name: name })?;
            writer.write_all(b"\n")?;
        }
    }

    loop {
        let page = store.works_after(after.as_ref(), PAGE).await?;
        let Some(last) = page.last() else { break };
        after = Some((last.website.clone(), last.path.clone()));

        for row in page {
            // the rows come site by site: the name is looked up when the site changes, not for every row
            if allowed.as_ref().map_or(true, |(site, _)| *site != row.website) {
                allowed = Some((row.website.clone(), wanted.is_empty() || wanted.contains(&row.website.to_lowercase())));
            }
            if !allowed.as_ref().is_some_and(|(_, yes)| *yes) || row.updated_at < since {
                continue;
            }
            if !moved.sites.contains(&row.website) {
                moved.sites.insert(row.website.clone());
            }
            moved.works += 1;
            serde_json::to_writer(&mut writer, &Line::new(row, &names))?;
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
        let entry: Entry = serde_json::from_str(&text).map_err(|e| format!("{}: line {}: {e}", file.display(), number + 1))?;
        let line = match entry {
            Entry::Name { site_tag_name: (website, attribute, id, name) } => {
                store.save_site_tag_name(&website, &attribute, id, &name).await?;
                continue;
            }
            Entry::Work(line) => line,
        };

        moved.works += 1;
        if !moved.sites.contains(&line.website) {
            moved.sites.insert(line.website.clone());
        }
        batch.push(line);

        if batch.len() >= BATCH {
            moved.new += save(store, std::mem::take(&mut batch), romanizer).await?;
        }
    }
        moved.new += save(store, batch, romanizer).await?;

    Ok(moved)
}

/// Makes the romaji and the key of the titles (CPU work, so on a thread of its own) and writes the works, and the tags and accounts that came
/// with them.
async fn save(store: &SearchStore, lines: Vec<Line>, romanizer: &Arc<Romanizer>) -> Result<usize> {
    let romanizer = romanizer.clone();
    let (works, mut extras) = tokio::task::spawn_blocking(move || {
        let mut extras: Vec<(String, NewWork)> = Vec::new();
        let works = lines
            .into_iter()
            .map(|line| {
                // the language is read from the title with its flags; the title is kept without them
                let language = crate::language::detect(&line.title).or(line.language);
                let title = crate::language::strip_flags(&line.title);
                let converted = romaji(&title, &romanizer).unwrap_or_else(|_| title.clone());

                if !line.tags.is_empty() || line.uploader.is_some() || line.posted_at.is_some() {
                    let mut extra = Extra {
                        tags: line.tags.iter().map(|(name, kind)| Tag::new(name.clone(), TagKind::from_code(*kind))).collect(),
                        uploader: line.uploader.as_ref().map(|(id, name)| Account { id: id.clone(), name: name.clone() }),
                        posted_at: line.posted_at,
                        romaji: Vec::new(),
                        site_id: None,
                    };
                    extra.romaji = extra.texts().iter().map(|text| romaji(text, &romanizer).unwrap_or_else(|_| text.clone())).collect();
                    extras.push((line.website.clone(), NewWork { path: line.path.clone(), title: String::new(), romaji: String::new(), language: None, extra }));
                }

                work::Model {
                    website: line.website,
                    path: line.path,
                    title_romaji: converted,
                    title,
                    first_seen: line.first_seen,
                    updated_at: line.updated_at,
                    language,
                    deleted: false,
                    checked_at: None,
                    tags: None,
                    uploader: None,
                    posted_at: None,
                    site_id: None,
                }
            })
            .collect::<Vec<_>>();

        (works, extras)
    })
    .await?;

    let new = store.import_works(works).await?;

    // the works that are saved already get the tags too (a database that was filled before the tags were read)
    extras.sort_by(|a, b| a.0.cmp(&b.0));
    for group in extras.chunk_by(|a, b| a.0 == b.0) {
        let works: Vec<NewWork> = group.iter().map(|(_, work)| work.clone()).collect();
        store.import_extras(&group[0].0, &works).await?;
    }

    Ok(new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(path: &str, title: &str) -> NewWork {
        NewWork { path: path.to_string(), title: title.to_string(), romaji: title.to_string(), language: crate::language::detect(title), extra: Default::default() }
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

    #[tokio::test]
    async fn tags_uploaders_and_dates_go_through_the_file_by_name_and_fill_works_that_are_saved() {
        let mut tagged = work("/a/1", "Alpha one");
        tagged.extra = Extra {
            tags: vec![Tag::new("cosplay", TagKind::Tag), Tag::new("fini", TagKind::Author)],
            uploader: Some(Account { id: "u1".into(), name: "Uploader One".into() }),
            posted_at: Some(1_700_000_000),
            romaji: vec!["cosplay".into(), "fini".into(), "uploader one".into()],
            site_id: None,
        };
        let from = SearchStore::in_memory().await.unwrap();
        from.save_crawled("A", 100, &[tagged, work("/a/2", "Alpha two")], None).await.unwrap();
        let file = scratch("tags.jsonl.br");
        export(&from, &file, &[], 0).await.unwrap();

        // a database that has the works already, with no tags: the import fills them
        let to = SearchStore::in_memory().await.unwrap();
        to.save_crawled("A", 50, &[work("/a/1", "Alpha one")], None).await.unwrap();
        let imported = import(&to, &file, &crate::testing::romanizer_arc()).await.unwrap();
        assert_eq!((imported.works, imported.new), (2, 1));

        let rows = to.works_after(None, 10).await.unwrap();
        let one = rows.iter().find(|w| w.path == "/a/1").unwrap();
        assert!(one.tags.is_some(), "the tags are set on the work that was saved");
        assert_eq!((one.uploader.as_deref(), one.posted_at), (Some("u1"), Some(1_700_000_000)));
        assert!(rows.iter().find(|w| w.path == "/a/2").unwrap().tags.is_none());

        // and goes out again with the names, so the file of the second database says the same
        let again = scratch("tags2.jsonl.br");
        export(&to, &again, &[], 0).await.unwrap();
        let third = SearchStore::in_memory().await.unwrap();
        import(&third, &again, &crate::testing::romanizer_arc()).await.unwrap();
        let names: Vec<(String, i64)> = third.tag_table().await.unwrap().into_values().collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&("fini".to_string(), 1)));

        let _ = std::fs::remove_file(file);
        let _ = std::fs::remove_file(again);
    }

    #[tokio::test]
    async fn the_names_of_the_numbers_of_a_site_go_through_the_file() {
        let from = SearchStore::in_memory().await.unwrap();
        from.save_crawled("AsmHentai", 100, &[work("/g/1/", "Alpha one")], None).await.unwrap();
        from.save_site_tag_name("AsmHentai", "tags", 1, "big breasts").await.unwrap();
        from.save_site_tag_name("HentaiEra", "artists", 7, "someone").await.unwrap();
        let file = scratch("names.jsonl.br");
        let moved = export(&from, &file, &["AsmHentai".to_string()], 0).await.unwrap();
        assert_eq!(moved.works, 1);

        let to = SearchStore::in_memory().await.unwrap();
        let imported = import(&to, &file, &crate::testing::romanizer_arc()).await.unwrap();
        assert_eq!(imported.works, 1);
        assert_eq!(to.all_site_tag_names().await.unwrap(), vec![("AsmHentai".to_string(), "tags".to_string(), 1, "big breasts".to_string())]);

        let _ = std::fs::remove_file(file);
    }
}
