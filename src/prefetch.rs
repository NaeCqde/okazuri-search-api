//! Searching ahead of time for the magazine issues of the mirror (the copy of the Worker's D1 tables).
//!
//! The name of every issue becomes a query the way the bot does it (`query::issue_query`), and the query
//! is searched like any other, so the answer is in the search database when the bot asks. A query
//! that is searched already (and young) is skipped, so the work is only for the issues that are new or
//! whose answers got old, newest issues first.

use crate::config::PrefetchConfig;
use crate::engine::Engine;
use crate::query::issue_query;
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
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Round {
    /// Queries asked to the sites in this round.
    pub searched: usize,
    /// Queries that were searched already.
    pub skipped: usize,
    /// Queries that failed altogether (no site was asked).
    pub failed: usize,
    /// The round stopped at `max_queries_per_round` with queries left.
    pub more: bool,
}

impl Prefetcher {
    pub fn new(engine: Arc<Engine>, mirror_path: PathBuf, config: PrefetchConfig) -> Self {
        Self { engine, mirror_path, config }
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

        for (number, query) in queries.into_iter().enumerate() {
            if self.engine.is_fresh(&query).await? {
                round.skipped += 1;
                continue;
            }
            if self.config.max_queries_per_round > 0 && round.searched >= self.config.max_queries_per_round {
                round.more = true;
                break;
            }

            match self.engine.fetch(&query, false).await {
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

            tokio::time::sleep(Duration::from_millis(self.config.query_delay_ms)).await;
        }

        Ok(round)
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
                        "prefetch: {} searched, {} already searched, {} failed{}",
                        round.searched,
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

        assert_eq!(prefetcher.round().await.unwrap(), Round { searched: 3, skipped: 0, failed: 0, more: false });
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
        assert_eq!(prefetcher.round().await.unwrap(), Round { searched: 0, skipped: 3, failed: 0, more: false });
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

        assert_eq!(prefetcher.round().await.unwrap(), Round { searched: 2, skipped: 0, failed: 0, more: true });
        assert_eq!(prefetcher.round().await.unwrap(), Round { searched: 1, skipped: 2, failed: 0, more: false });

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn no_mirror_is_a_round_with_nothing_to_do() {
        let (engine, _, _) = setup(&SearchConfig::default()).await;
        let prefetcher = prefetcher(PrefetchConfig::default(), engine, PathBuf::from("no-such-dir/mirror.sqlite"));

        assert_eq!(prefetcher.round().await.unwrap(), Round::default());
    }
}
