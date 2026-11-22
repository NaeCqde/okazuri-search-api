//! Searching ahead of time for the magazine issues of the mirror (the copy of the Worker's D1 tables).
//!
//! The name of every issue becomes a query the way the bot does it (`query::issue_query`), and the query
//! is searched like any other, so the answer is in the search database when the bot asks. A query
//! that is searched already (and young) is skipped, so the work is only for the issues that are new or
//! whose answers got old, newest issues first.

use crate::config::PrefetchConfig;
use crate::engine::{Engine, Lang, Live};
use crate::query::{has_japanese, issue_query};
use crate::mirror::{mirror_issues, Issue};
use crate::utils::Result;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;

pub struct Prefetcher {
    engine: Arc<Engine>,
    mirror_path: PathBuf,
    config: PrefetchConfig,
    /// The crawl (`crawl.rs`) is on: the sites whose list it has read are not searched, only the sites that have no list.
    crawled: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Round {
    /// Queries asked to the sites in this round.
    pub searched: usize,
    /// Queries that were searched already.
    pub skipped: usize,
    /// Queries that failed altogether (no site was asked).
    pub failed: usize,
    /// English titles (from Hitomi.La) that were searched, besides the queries.
    pub english: usize,
    /// The round stopped at `max_queries_per_round` with queries left.
    pub more: bool,
}

impl Prefetcher {
    pub fn new(engine: Arc<Engine>, mirror_path: PathBuf, config: PrefetchConfig) -> Self {
        Self { engine, mirror_path, config, crawled: false }
    }

    /// Leaves out the sites the crawl has read to their end (their works are saved), so the prefetch goes on only for the others.
    pub fn with_crawl(mut self, on: bool) -> Self {
        self.crawled = on;
        self
    }

    /// The sites a query is not asked to in this round.
    async fn left_out(&self) -> Result<Vec<usize>> {
        if self.crawled {
            self.engine.crawled_sites().await
        } else {
            Ok(Vec::new())
        }
    }

    /// The queries of the issues, the newest issue first, each once. The engine turns them into romaji.
    pub fn queries(&self, issues: &[Issue]) -> Vec<String> {
        let mut seen = HashSet::new();

        issues
            .iter()
            .map(|issue| issue_query(&issue.name))
            .filter(|query| !query.is_empty() && seen.insert(query.clone()))
            .collect()
    }

    /// Searches the queries that are not searched yet (or are old), once through.
    pub async fn round(&self) -> Result<Round> {
        let issues = mirror_issues(&self.mirror_path).await?;
        let queries = self.queries(&issues);
        let mut round = Round::default();

        if issues.is_empty() {
            log::info!("prefetch: the mirror has no issues (yet)");
        } else {
            log::info!("prefetch: a round over {} issues ({} queries)", issues.len(), queries.len());
        }

        let total = queries.len();
        let left_out = self.left_out().await?;
        if !left_out.is_empty() {
            log::info!("prefetch: {} sites are read by the crawl, not searched", left_out.len());
        }

        for (number, query) in queries.into_iter().enumerate() {
            if self.engine.is_fresh_without(&query, &left_out).await? {
                round.skipped += 1;
                // the English title is looked up once (it is saved): a query that was searched before this existed gets it too
                if has_japanese(&query) {
                    self.search_english(&query, &left_out, &mut round).await;
                }
                continue;
            }
            if self.config.max_queries_per_round > 0 && round.searched >= self.config.max_queries_per_round {
                round.more = true;
                break;
            }

            match self.engine.fetch_with(&query, false, &left_out).await {
                Ok(fetched) => {
                    round.searched += 1;
                    log::info!(
                        "prefetch: {}/{total} {query:?}: {} sites asked, {} new works",
                        number + 1,
                        fetched.asked.len(),
                        fetched.new.len()
                    );
                }
                Err(e) => {
                    round.failed += 1;
                    log::warn!("prefetch: {query:?}: {e}");
                }
            }

            if has_japanese(&query) {
                self.search_english(&query, &left_out, &mut round).await;
            }

            tokio::time::sleep(Duration::from_millis(self.config.query_delay_ms)).await;
        }

        Ok(round)
    }

    /// A query in Japanese is searched in English too: Hitomi.La has the English title of a work (`Engine::translate`, saved,
    /// so a title that was asked is not asked again while it is young), and the sites are searched for that title as well
    /// (the Japanese title and its romaji were searched before). A failure here is a warning: the query itself is done.
    async fn search_english(&self, query: &str, left_out: &[usize], round: &mut Round) {
        if !self.engine.can_translate() || Lang::of(query) != Some(Lang::Japanese) {
            return;
        }

        let found = match self.engine.translate_hitomi(query, Lang::Japanese, Live::Diff).await {
            Ok((found, _)) => found,
            Err(e) => {
                log::warn!("prefetch: no English title for {query:?}: {e}");
                return;
            }
        };

        let mut asked = HashSet::new();

        for translation in found.iter().take(3) {
            let english = translation.english.trim();

            if english.is_empty() || english == query || !asked.insert(english.to_lowercase()) {
                continue;
            }
            match self.engine.is_fresh_without(english, left_out).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    log::warn!("prefetch: {english:?}: {e}");
                    continue;
                }
            }

            match self.engine.fetch_with(english, false, left_out).await {
                Ok(fetched) => {
                    round.english += 1;
                    log::info!("prefetch: {query:?} in English {english:?}: {} sites asked, {} new works", fetched.asked.len(), fetched.new.len());
                }
                Err(e) => log::warn!("prefetch: {english:?}: {e}"),
            }

            tokio::time::sleep(Duration::from_millis(self.config.query_delay_ms)).await;
        }
    }

    /// Rounds without end: a new one when the mirror changed (`wake`), when a round left queries, and at least
    /// every `round_interval_minutes`. When the mirror changes while a round is running, the round is stopped and a new
    /// one starts at once, over the new issues: what was searched is remembered (`is_fresh`), so nothing is done twice.
    /// That is also how a restart of the program goes on where it stopped.
    pub async fn run(self: Arc<Self>, wake: Arc<Notify>) {
        loop {
            let round = tokio::select! {
                round = self.round() => round,
                _ = wake.notified() => {
                    log::info!("prefetch: the copy of D1 changed, starting over");
                    continue;
                }
            };

            match round {
                Ok(round) => {
                    log::info!(
                        "prefetch: {} searched ({} English titles too), {} already searched, {} failed{}",
                        round.searched,
                        round.english,
                        round.skipped,
                        round.failed,
                        if round.more { ", more to do" } else { "" }
                    );

                    if round.more {
                        continue;
                    }
                }
                Err(e) => log::warn!("prefetch: {e}"),
            }

            let interval = Duration::from_secs(self.config.round_interval_minutes.max(1) * 60);

            tokio::select! {
                _ = wake.notified() => {}
                _ = tokio::time::sleep(interval) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SearchConfig;
    use crate::engine::tests::setup;
    use crate::mirror::rebuild_mirror;
    use std::fs;

    const EXPORT: &str = "CREATE TABLE `magazines` (`id` text PRIMARY KEY NOT NULL, `name` text NOT NULL);
CREATE TABLE `numbers` (`id` text PRIMARY KEY NOT NULL, `name` text NOT NULL, `date` integer NOT NULL, `magazine_id` text NOT NULL);
INSERT INTO `magazines` VALUES('LBL1','COMIC快楽天');
INSERT INTO `numbers` VALUES('BJ1','COMIC快楽天 2026年06月号',1000,'LBL1');
INSERT INTO `numbers` VALUES('BJ2','COMIC快楽天 2026年07月号',2000,'LBL1');
INSERT INTO `numbers` VALUES('BJ3','COMIC ExE 11',500,'LBL1');
";

    fn issue(name: &str) -> Issue {
        Issue { id: name.into(), name: name.into(), magazine_id: "L".into(), magazine_name: "M".into(), date: 0 }
    }

    fn prefetcher(config: PrefetchConfig, engine: Arc<Engine>, mirror: PathBuf) -> Prefetcher {
        Prefetcher::new(engine, mirror, PrefetchConfig { query_delay_ms: 0, ..config })
    }

    #[tokio::test]
    async fn the_queries_are_the_issues_newest_first_each_once() {
        let (engine, _, _) = setup(&SearchConfig::default()).await;
        let prefetcher = prefetcher(PrefetchConfig::default(), engine, PathBuf::new());
        let issues = [issue("COMIC快楽天 2026年07月号"), issue("COMIC ExE 11"), issue("COMIC快楽天 2026年07月号")];

        // the engine turns them into romaji when it searches
        assert_eq!(prefetcher.queries(&issues), ["COMIC快楽天 2026 07", "COMIC ExE 11"]);
    }

    #[tokio::test]
    async fn a_round_searches_what_is_new_and_skips_what_is_searched() {
        let (engine, hits, store) = setup(&SearchConfig { min_delay_ms: 0, ..SearchConfig::default() }).await;
        let dir = std::env::temp_dir().join(format!("okazuri-search-api-prefetch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mirror = dir.join("mirror.sqlite");
        rebuild_mirror(&mirror, EXPORT, "bm", 1).await.unwrap();
        let prefetcher = prefetcher(PrefetchConfig::default(), engine, mirror);

        assert_eq!(prefetcher.round().await.unwrap(), Round { searched: 3, skipped: 0, failed: 0, english: 0, more: false });
        let asked = hits.ok.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(asked, 5, "the site finds Japanese titles: a query with Japanese in it is asked as written and in romaji (2 x 2), the other once");

        // what was asked is the romaji of the name of the issue
        let queries: Vec<String> = ["COMIC快楽天 2026 07", "COMIC快楽天 2026 06", "COMIC ExE 11"]
            .iter()
            .map(|q| prefetcher.engine.romanize_query(q).unwrap())
            .collect();
        assert!(queries[0].contains("kairakuten"), "{queries:?}");
        for query in queries.iter().map(String::as_str).chain(["COMIC快楽天 2026 07", "COMIC快楽天 2026 06"]) {
            assert!(store.states(query).await.unwrap().values().any(|s| s.ok), "{query} was searched");
        }

        // the second round has nothing to do
        assert_eq!(prefetcher.round().await.unwrap(), Round { searched: 0, skipped: 3, failed: 0, english: 0, more: false });
        assert_eq!(hits.ok.load(std::sync::atomic::Ordering::SeqCst), asked);

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn a_round_stops_at_the_limit_and_the_next_goes_on() {
        let (engine, _, _) = setup(&SearchConfig { min_delay_ms: 0, ..SearchConfig::default() }).await;
        let dir = std::env::temp_dir().join(format!("okazuri-search-api-prefetch-limit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mirror = dir.join("mirror.sqlite");
        rebuild_mirror(&mirror, EXPORT, "bm", 1).await.unwrap();
        let prefetcher = prefetcher(PrefetchConfig { max_queries_per_round: 2, ..PrefetchConfig::default() }, engine, mirror);

        assert_eq!(prefetcher.round().await.unwrap(), Round { searched: 2, skipped: 0, failed: 0, english: 0, more: true });
        assert_eq!(prefetcher.round().await.unwrap(), Round { searched: 1, skipped: 2, failed: 0, english: 0, more: false });

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn no_mirror_is_a_round_with_nothing_to_do() {
        let (engine, _, _) = setup(&SearchConfig::default()).await;
        let prefetcher = prefetcher(PrefetchConfig::default(), engine, PathBuf::from("no-such-dir/mirror.sqlite"));

        assert_eq!(prefetcher.round().await.unwrap(), Round::default());
    }
}
