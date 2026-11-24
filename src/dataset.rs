//! The public dataset: `dataset <directory>` writes the saved works as a file anyone can read (`works.jsonl.gz`, JSON Lines compressed with
//! gzip) and `meta.json` (when it was made, how many works each site has, what the fields mean).
//!
//! Unlike `export`, which moves works between our own databases, this file is for others: a line has the address of the page, whether the site
//! is a legal one, and the language. It has no searches, no romaji and nothing of the users. The directory is written through temporary
//! files and renamed at the end, so a reader (or an upload) never sees a half-written file.

use crate::sites::Site;
use crate::store::SearchStore;
use crate::utils::{unix_now, Result};
use serde::Serialize;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

const PAGE: u64 = 5_000;

/// A line of `works.jsonl.gz`.
#[derive(Serialize)]
struct Line<'a> {
    site: &'a str,
    is_legal: bool,
    url: String,
    title: &'a str,
    /// Two-letter codes the title says the work is in; empty when it says none.
    language: Vec<&'a str>,
    /// When this crawler first saved the work (unix seconds). It is NOT the day the work was uploaded or published.
    first_seen: i64,
    /// When the page was last looked at to see if it is still there (unix seconds); null: never.
    checked_at: Option<i64>,
}

#[derive(Serialize)]
struct SiteCount {
    site: String,
    is_legal: bool,
    works: usize,
}

#[derive(Serialize)]
struct Meta {
    generated_at: i64,
    works: usize,
    file: &'static str,
    format: &'static str,
    sites: Vec<SiteCount>,
    fields: BTreeMap<&'static str, &'static str>,
    notes: Vec<&'static str>,
}

/// What `write` made.
#[derive(Debug, PartialEq, Eq)]
pub struct Written {
    pub works: usize,
    pub sites: usize,
}

pub async fn write(store: &SearchStore, sites: &[Site], dir: &Path) -> Result<Written> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot make {}: {e}", dir.display()))?;

    let by_name: BTreeMap<String, &Site> = sites.iter().map(|site| (site.def.name.to_lowercase(), site)).collect();
    let temporary = dir.join("works.jsonl.gz.tmp");
    let file = std::fs::File::create(&temporary).map_err(|e| format!("cannot write {}: {e}", temporary.display()))?;
    let mut writer = flate2::write::GzEncoder::new(file, flate2::Compression::new(9));
    let mut counts: BTreeMap<String, (bool, usize)> = BTreeMap::new();
    let mut works = 0;
    let mut after = None;

    loop {
        let page = store.works_after(after.as_ref(), PAGE).await?;
        let Some(last) = page.last() else { break };
        after = Some((last.website.clone(), last.path.clone()));

        for row in &page {
            // a work of a site this version does not know is not published: its address cannot be made
            let Some(site) = by_name.get(&row.website.to_lowercase()) else { continue };
            let line = Line {
                site: site.def.name,
                is_legal: site.def.legal,
                url: site.address(&row.path),
                title: &row.title,
                language: row.language.as_deref().map(|codes| codes.split(',').filter(|c| !c.is_empty()).collect()).unwrap_or_default(),
                first_seen: row.first_seen,
                checked_at: row.checked_at,
            };

            serde_json::to_writer(&mut writer, &line)?;
            writer.write_all(b"\n")?;
            works += 1;
            counts.entry(site.def.name.to_string()).or_insert((site.def.legal, 0)).1 += 1;
        }
    }

    writer.finish()?.flush()?;

    let meta = Meta {
        generated_at: unix_now(),
        works,
        file: "works.jsonl.gz",
        format: "JSON Lines (one work a line), compressed with gzip, UTF-8",
        sites: counts.into_iter().map(|(site, (is_legal, works))| SiteCount { site, is_legal, works }).collect(),
        fields: BTreeMap::from([
            ("site", "The name of the site that lists the work."),
            ("is_legal", "true for a legal (licensed) site, false for a site that is known for unlicensed uploads."),
            ("url", "The address of the work's page."),
            ("title", "The title as the site writes it."),
            ("language", "Two-letter codes of the languages the title says the work is in; empty when it says none."),
            ("first_seen", "Unix seconds when this crawler first saved the work. It is not the date the work was uploaded or published."),
            ("checked_at", "Unix seconds when the page was last looked at to see if it is still there; null if never."),
        ]),
        notes: vec![
            "Only works that were still listed when the file was made are included; a work whose page is gone is left out.",
            "The list is what the crawler of okazuri-search-api found, not a complete list of what exists on the sites.",
            "first_seen and checked_at are not evidence of when a work was uploaded or published.",
        ],
    };
    let meta_temporary = dir.join("meta.json.tmp");
    std::fs::write(&meta_temporary, serde_json::to_vec_pretty(&meta)?)?;

    // the data first, the description last: meta.json names the data it describes
    std::fs::rename(&temporary, dir.join("works.jsonl.gz"))?;
    std::fs::rename(&meta_temporary, dir.join("meta.json"))?;

    Ok(Written { works, sites: meta.sites.len() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewWork;
    use std::io::Read;

    fn work(path: &str, title: &str) -> NewWork {
        NewWork { path: path.to_string(), title: title.to_string(), romaji: title.to_string(), language: crate::language::detect(title), extra: Default::default() }
    }

    #[tokio::test]
    async fn the_dataset_has_the_works_with_their_address_and_legality_and_nothing_else() {
        let store = SearchStore::in_memory().await.unwrap();
        let sites = crate::sites::all();
        let legal = sites.iter().find(|s| s.def.legal).unwrap();
        let illegal = sites.iter().find(|s| !s.def.legal).unwrap();

        store.save_crawled(legal.def.name, 100, &[work("/a/1", "Alpha one")], None).await.unwrap();
        store.save_crawled(illegal.def.name, 200, &[work("/b/1", "Beta one [English]")], None).await.unwrap();
        store.save_crawled("Unknown Site", 300, &[work("/c/1", "Gamma")], None).await.unwrap();

        let dir = std::env::temp_dir().join(format!("okazuri-dataset-{}", std::process::id()));
        let written = write(&store, &sites, &dir).await.unwrap();
        assert_eq!(written, Written { works: 2, sites: 2 }, "the site this version does not know is left out");

        let mut text = String::new();
        flate2::read::GzDecoder::new(std::fs::File::open(dir.join("works.jsonl.gz")).unwrap()).read_to_string(&mut text).unwrap();
        let lines: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();

        assert_eq!(lines.len(), 2);
        let beta = lines.iter().find(|l| l["title"] == "Beta one [English]").unwrap();
        assert_eq!(beta["is_legal"], false);
        assert_eq!(beta["language"], serde_json::json!(["en"]));
        assert_eq!(beta["first_seen"], 200);
        assert_eq!(beta["url"], illegal.address("/b/1"));
        assert!(beta.get("title_romaji").is_none() && beta.get("path").is_none(), "no romaji, no internal fields");

        let meta: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("meta.json")).unwrap()).unwrap();
        assert_eq!(meta["works"], 2);
        assert_eq!(meta["sites"].as_array().unwrap().len(), 2);
        assert!(!dir.join("works.jsonl.gz.tmp").exists() && !dir.join("meta.json.tmp").exists());

        let _ = std::fs::remove_dir_all(dir);
    }
}
