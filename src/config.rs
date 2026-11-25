//! `config.toml`: where the server listens, which of its functions are on, and the settings of each.
//!
//! Every key is optional; a missing file is the same as an empty one. The template with the
//! comments is `config.example.toml`.

use crate::utils::Result;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub romaji: RomajiConfig,
    pub search: SearchConfig,
    pub magazine: MagazineConfig,
    pub translation: TranslationConfig,
    pub crawl: CrawlConfig,
    pub verify: VerifyConfig,
    pub distribute: DistributeConfig,
    pub cron: CronConfig,
    pub log: LogConfig,
}

/// Which log lines have what a person searched for, or what it was translated to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogQueries {
    /// No line has it (the lines say that there was a query, with the time and the error).
    Off,
    /// The lines of a failure (warnings and errors) have it, so that the failed query can be found.
    #[default]
    Errors,
    /// Every line that talks of a query has it (also the timings).
    All,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    pub queries: LogQueries,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

impl Default for ServerConfig {
    fn default() -> Self {
        // Only this machine, so that it has to be opened up on purpose (behind a reverse proxy).
        Self { host: "127.0.0.1".to_string(), port: 8000 }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RomajiConfig {
    /// The longest query of `/romaji`, in characters.
    pub max_query_chars: usize,
    /// Word (as UniDic splits it) to the romaji it is replaced by.
    pub exceptions: BTreeMap<String, String>,
}

impl Default for RomajiConfig {
    fn default() -> Self {
        Self { max_query_chars: 300, exceptions: BTreeMap::new() }
    }
}

/// The search engine (`engine.rs`): `GET /okazuri/search`. Off unless `enabled` is true.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SearchConfig {
    pub enabled: bool,
    /// The SQLite file with the results of the searches (made if it is not there).
    pub db_path: PathBuf,
    /// A search younger than this is answered from the database, without asking the sites.
    pub ttl_hours: u64,
    /// A site that failed is not asked the same query again before this.
    pub retry_failed_minutes: u64,
    /// The longest query, in characters.
    pub max_query_chars: usize,
    /// Searches in flight at the same time (every site is one).
    pub concurrency: usize,
    /// How long a site may take to answer.
    pub timeout_secs: u64,
    /// The shortest time between two requests to the same site.
    pub min_delay_ms: u64,
    /// The shortest time between two requests to one site, by the name of the site (`GET /okazuri/sites`; case does not matter),
    /// in place of `min_delay_ms` and of the delay the program knows for the site. Every request of the program to the site (a
    /// search, the prefetch, the crawl) keeps it together.
    pub site_delay_ms: BTreeMap<String, u64>,
    /// The most results kept from one site.
    pub max_results_per_site: usize,
    /// The most works in one answer (a request may ask for fewer with limit).
    pub max_results: usize,
    /// The most saved works one search reads from the database, whatever the words are (the newest ones are kept). A word that is in more works than this is cut.
    pub max_candidates: usize,
    /// How many of the first works that an answer finds are put in order by how good they are (the words start words of the title, not the middle of one);
    /// the good ones first, each group in the order of the rows. The works after them stay in the order of the rows. 0: no such order.
    pub rank_window: usize,
    /// How much a query that names a tag adds to the score, by what the tag is (`tag`, `author`, `circle`, `weak`; the weight of the tag in the
    /// `tags` table counts too). A kind that is not here keeps its default (1, 6, 4, 0.25).
    pub tag_boost: BTreeMap<String, f64>,
    /// Names of the sites (`GET /okazuri/sites`) that are left out.
    pub disabled_sites: Vec<String>,
    /// A site whose whole list the crawl has read (`crawls.done`) is answered from the database and not asked live; a request with `live=force`
    /// still asks it. The sites that have no list, or whose list is not read yet, are asked as before.
    pub skip_crawled: bool,
    /// Keep the pages of the database file (`db_path`, not its WAL) in memory: the file is mapped and locked (`mlock`), so the system never takes
    /// its pages out of the page cache, and a search never waits for the disk. Needs the right to lock that much memory (in Docker:
    /// `cap_add: IPC_LOCK` and `ulimits: memlock: -1`); without it the server only warns and goes on. The file is locked again when it grows.
    pub pin_db: bool,
    /// Also try the sites that cannot be searched with a plain request (a Cloudflare challenge ...).
    pub try_unsupported: bool,
    /// The bridge of Google Translate: not a key of `[search]`, it is `[translation]` (`open_engine` copies it here).
    /// The address of `tools/google-translate-bridge` (a Chrome that makes the token of Google Translate), such as
    /// `http://100.64.0.1:8130`. Empty: Google Translate is asked without the token, which gets the answer of the classic
    /// model.
    #[serde(skip)]
    pub google_bridge_url: String,
    /// What the bridge wants in `Authorization: Bearer` (its `BRIDGE_TOKEN`); empty for none.
    #[serde(skip)]
    pub google_bridge_token: String,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            db_path: PathBuf::from("search.db"),
            ttl_hours: 24 * 30,
            retry_failed_minutes: 60,
            max_query_chars: 300,
            concurrency: 8,
            timeout_secs: 20,
            min_delay_ms: 1000,
            site_delay_ms: BTreeMap::new(),
            max_results_per_site: 50,
            max_results: 100,
            max_candidates: 800_000,
            rank_window: 2000,
            tag_boost: BTreeMap::new(),
            disabled_sites: Vec::new(),
            skip_crawled: false,
            pin_db: false,
            try_unsupported: false,
            google_bridge_url: String::new(),
            google_bridge_token: String::new(),
        }
    }
}

/// Where the translations come from (`GET /hitomila/translate`, `/google/translate`, `/auto/translate`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct TranslationConfig {
    /// The address of `tools/google-translate-bridge` (a Chrome that makes the token of Google Translate), such as
    /// `http://100.64.0.1:8130`. Empty: Google Translate is asked without the token, which gets the answer of the classic
    /// model.
    pub google_bridge_url: String,
    /// What the bridge wants in `Authorization: Bearer` (its `BRIDGE_TOKEN`); empty for none.
    pub google_bridge_token: String,
    /// Translating the titles of the saved works ahead of time (`[translation.fill]`).
    pub fill: FillConfig,
}

/// Translating the Japanese titles of the saved works with Google Translate, a few at a time, so that a result has its English title and
/// a search in English finds it (`fill.rs`). The newest works come first, and a title that has a translation is skipped. Off unless
/// `enabled` is true.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FillConfig {
    pub enabled: bool,
    /// The wait after each title that Google was asked for.
    pub pause_ms: u64,
    /// The most titles asked in one round; the next round starts again from the newest works (the ones saved meanwhile come first).
    pub max_per_round: usize,
    /// The wait before the next round when the last one did not use `max_per_round` (the titles are all translated, or Google does not answer).
    pub round_interval_minutes: u64,
}

impl Default for FillConfig {
    fn default() -> Self {
        Self { enabled: false, pause_ms: 3000, max_per_round: 1000, round_interval_minutes: 60 }
    }
}

/// The magazines and the works of the Worker's D1 database, copied (`d1.rs`, `mirror.rs`). Off unless `enabled` is true.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MagazineConfig {
    pub enabled: bool,
    pub account_id: String,
    pub database_id: String,
    /// A Cloudflare API token that can edit D1. `${NAME}` is the environment variable NAME.
    pub api_token: String,
    /// The tables to export (the exported SQL of a database with a virtual table is refused by D1, so
    /// the tables are named).
    pub tables: Vec<String>,
    /// The SQLite file the exported SQL is loaded into.
    pub mirror_path: PathBuf,
    /// How often to ask D1 whether the data changed (a call that costs no reads).
    pub check_interval_minutes: u64,
    /// Searching ahead of time for the issues of the copy (`[magazine.prefetch]`).
    pub prefetch: PrefetchConfig,
}

impl Default for MagazineConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            account_id: String::new(),
            database_id: String::new(),
            api_token: "${CLOUDFLARE_API_TOKEN}".to_string(),
            tables: ["magazines", "numbers", "authors", "works"].map(String::from).to_vec(),
            mirror_path: PathBuf::from("d1-mirror.db"),
            check_interval_minutes: 10,
            prefetch: PrefetchConfig::default(),
        }
    }
}

/// Searching ahead of time for the issues of the magazines in the mirror (`prefetch.rs`): the section `[magazine.prefetch]`.
/// Off unless `enabled` is true.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PrefetchConfig {
    pub enabled: bool,
    /// The wait between two queries, so that no site is asked more than the `min_delay_ms` allows.
    pub query_delay_ms: u64,
    /// The most queries in one round (0 is all of them); the next round goes on.
    pub max_queries_per_round: usize,
    /// The wait between two rounds when everything is searched already.
    pub round_interval_minutes: u64,
}

impl Default for PrefetchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            query_delay_ms: 2000,
            max_queries_per_round: 0,
            round_interval_minutes: 60,
        }
    }
}

/// Handing the work of the crawl and of the verification to other machines (`dist.rs`): the host (the server that keeps the database) cuts the
/// reading of the pages into jobs, and the workers (machines with their own IP addresses) ask the sites and give the pages back. Off unless `role` is set.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DistributeConfig {
    /// `off`, `host` or `worker`.
    pub role: String,
    /// The secret that a worker sends (`Authorization: Bearer`). The host has no job routes without it.
    pub token: String,
    /// Of a worker: the address of the host (`http://100.67.47.81:8010`).
    pub host: String,
    /// Of the host: how many workers there are (the verification cuts its pages for them).
    pub workers: usize,
    /// Of the host: in how many parts a site is cut for the crawl. The parts are jobs that any worker takes, so the number of workers can change
    /// at any time without changing the parts (the state of a part is kept by its number); more parts than workers is the usual case.
    pub parts: usize,
    /// Of the host: how many works a job of the crawl reads before the worker answers (the pages are read one after another; 0 is one page).
    pub job_works: usize,
}

impl Default for DistributeConfig {
    fn default() -> Self {
        Self { role: "off".to_string(), token: String::new(), host: String::new(), workers: 3, parts: 12, job_works: 1000 }
    }
}

/// Reading every work of the sites that list them all, in the background of the server (`crawl.rs`). Off unless `enabled` is true.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CrawlConfig {
    pub enabled: bool,
    /// When the newest works are read again, once the whole list was read (five fields, as in `[cron]`; the clock is `utc_offset`).
    pub schedule: String,
    pub utc_offset: String,
    /// The sites to crawl, by name; empty is every site that can be.
    pub sites: Vec<String>,
    /// `k/N`: this machine reads the pages whose number leaves k when divided by N, so N machines (each with its own IP address) read
    /// every site together without any one of them being the only one a site sees. `0/1` is the whole list.
    pub shard: String,
    /// `lo-hi`: the galleries of E-Hentai that this machine reads, by id (either side may be empty). The list is read with the id of
    /// the last gallery of the page before (`?next=`), which can be any id, so the ids are cut into ranges, one for each machine: a
    /// machine reads from `hi` down to `lo` and stops (the one with no `hi` also reads the newest pages again, as every walk does).
    /// Empty is the whole list.
    pub gallery_ids: String,
    /// The index file of Hitomi.La that lists the galleries: `index-all.nozomi`, or `index-japanese.nozomi` for the Japanese ones only.
    pub hitomi_index: String,
    /// How many galleries of Hitomi.La are read before the walk is saved.
    pub hitomi_batch: usize,
    /// The most pages that one reading of the newest works goes back. It stops at the first page it knows (every work of it saved), so
    /// after a long pause it goes back as far as the pause was; this is only the safety against a list that never shows a known page.
    pub refresh_pages: usize,
    /// The wait after a site failed, before it is tried again.
    pub retry_minutes: u64,
}

impl Default for CrawlConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            schedule: "17 * * * *".to_string(),
            utc_offset: "UTC".to_string(),
            sites: Vec::new(),
            shard: "0/1".to_string(),
            gallery_ids: String::new(),
            hitomi_index: "index-all.nozomi".to_string(),
            hitomi_batch: 100,
            refresh_pages: 1000,
            retry_minutes: 5,
        }
    }
}

/// Looking at the page of every saved work now and then, and removing the works whose page is gone (`verify.rs`). Off unless `enabled` is true.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VerifyConfig {
    pub enabled: bool,
    /// The sites to look at, by name; empty is every site whose pages say that a work is gone (`engine::gone_rule`).
    pub sites: Vec<String>,
    /// Every page is looked at once in this many days: a site is asked one page every `days / works` (the works of a site that are saved
    /// now), never closer than the delay of the site.
    pub interval_days: u64,
    /// A site is never asked two pages closer than this (a site with more works than `interval_days` allow is looked at over more days).
    pub min_pause_ms: u64,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self { enabled: false, sites: Vec::new(), interval_days: 30, min_pause_ms: 5000 }
    }
}

/// Scheduled HTTP requests (`cron.rs`). Off unless `enabled` is true.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CronConfig {
    pub enabled: bool,
    /// The clock the schedules are read on: `+09:00`, `-05:30`, `UTC`.
    pub utc_offset: String,
    pub jobs: Vec<JobConfig>,
}

impl Default for CronConfig {
    fn default() -> Self {
        Self { enabled: false, utc_offset: "UTC".to_string(), jobs: Vec::new() }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct JobConfig {
    /// Names the job in the log and in errors. Has to be different for each job.
    pub name: String,
    /// `minute hour day-of-month month day-of-week`
    pub schedule: String,
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    /// `${NAME}` in a value is the environment variable NAME.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_method() -> String {
    "GET".to_string()
}

fn default_timeout_secs() -> u64 {
    30
}

fn default_true() -> bool {
    true
}

/// `config.toml` in the current directory, and only there.
pub fn default_path() -> PathBuf {
    PathBuf::from("config.toml")
}

/// Reads the file; a file that does not exist gives the defaults.
pub async fn load(path: &Path) -> Result<Config> {
    let text = match tokio::fs::read_to_string(path).await {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(format!("cannot read {}: {}", path.display(), e).into()),
    };

    let config: Config = toml::from_str(&text).map_err(|e| format!("{}: {}", path.display(), e))?;
    crate::utils::set_log_queries(config.log.queries);

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_queries_is_errors_unless_the_file_says_otherwise() {
        assert_eq!(toml::from_str::<Config>("").unwrap().log.queries, LogQueries::Errors);
        assert_eq!(toml::from_str::<Config>("[log]\nqueries = \"errors\"").unwrap().log.queries, LogQueries::Errors);
        assert_eq!(toml::from_str::<Config>("[log]\nqueries = \"all\"").unwrap().log.queries, LogQueries::All);
        assert!(toml::from_str::<Config>("[log]\nqueries = \"sometimes\"").is_err());
    }

    #[test]
    fn an_empty_file_is_the_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 8000);
        assert_eq!(config.romaji.max_query_chars, 300);
        assert!(config.romaji.exceptions.is_empty());
        // the cron is off until it is asked for
        assert!(!config.cron.enabled);
        assert!(config.cron.jobs.is_empty());
        // the search engine and what feeds it are off
        assert!(!config.search.enabled && !config.magazine.enabled && !config.magazine.prefetch.enabled);
        assert_eq!(config.magazine.tables, ["magazines", "numbers", "authors", "works"]);
        assert_ne!(config.search.db_path, config.magazine.mirror_path, "two database files");
    }

    #[test]
    fn the_translation_has_its_own_section_and_the_old_keys_of_search_are_not_read() {
        let config: Config = toml::from_str("[translation]\ngoogle_bridge_url = \"http://b\"\ngoogle_bridge_token = \"t\"\n[search]\ngoogle_bridge_url = \"http://old\"\n").unwrap();

        assert_eq!((config.translation.google_bridge_url.as_str(), config.translation.google_bridge_token.as_str()), ("http://b", "t"));
        assert_eq!(config.search.google_bridge_url, "", "the engine gets it from [translation] when it is opened");
    }

    #[test]
    fn the_functions_are_switched_in_the_file() {
        let config: Config = toml::from_str(
            "[search]\nenabled = true\nttl_hours = 1\ndisabled_sites = [\"Sukebei\"]\n\
             [magazine]\nenabled = true\naccount_id = \"a\"\ndatabase_id = \"d\"\n[magazine.prefetch]\nenabled = true\n",
        )
        .unwrap();

        assert!(config.search.enabled && config.magazine.enabled && config.magazine.prefetch.enabled);
        assert_eq!((config.search.ttl_hours, config.search.concurrency), (1, 8));
        assert_eq!(config.search.disabled_sites, ["Sukebei"]);
    }

    #[test]
    fn a_job_has_defaults_for_what_is_left_out() {
        let config: Config = toml::from_str(
            "[cron]
enabled = true

[[cron.jobs]]
name = \"a\"
schedule = \"* * * * *\"
url = \"http://x\"
",
        )
        .unwrap();
        let job = &config.cron.jobs[0];
        assert!(config.cron.enabled);
        assert_eq!((job.method.as_str(), job.timeout_secs, job.enabled), ("GET", 30, true));
        assert!(job.headers.is_empty() && job.body.is_none());
    }

    #[test]
    fn a_job_needs_a_name_a_schedule_and_a_url() {
        assert!(toml::from_str::<Config>("[[cron.jobs]]
name = \"a\"
url = \"http://x\"
").is_err());
    }

    #[test]
    fn reads_what_is_given_and_defaults_the_rest() {
        let config: Config = toml::from_str(
            "[server]\nport = 9000\n\n[romaji.exceptions]\n\"ナース\" = \"nurse\"\n",
        )
        .unwrap();
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 9000);
        assert_eq!(config.romaji.exceptions.get("ナース").map(String::as_str), Some("nurse"));
    }

    #[tokio::test]
    async fn a_missing_file_is_the_defaults() {
        let config = load(Path::new("no-such-dir/config.toml")).await.unwrap();
        assert_eq!(config.server.port, 8000);
    }

    #[tokio::test]
    async fn a_broken_file_is_an_error_that_names_the_file() {
        let path = std::env::temp_dir().join("okazuri-search-api-broken-config.toml");
        std::fs::write(&path, "[server\nport = ").unwrap();
        let error = load(&path).await.unwrap_err().to_string();
        std::fs::remove_file(&path).ok();
        assert!(error.contains("okazuri-search-api-broken-config.toml"), "{error}");
    }
}
