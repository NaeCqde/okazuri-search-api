//! `config.toml`: where the server listens, which of its functions are on, and the settings of each.
//!
//! Every key is optional; a missing file is the same as an empty one. The template with the
//! comments is `config.example.toml`.

use crate::utils::Result;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub convert: ConvertConfig,
    pub search: SearchConfig,
    pub d1: D1Config,
    pub prefetch: PrefetchConfig,
    pub cron: CronConfig,
    /// Word (as UniDic splits it) to the romaji it is replaced by.
    pub exceptions: BTreeMap<String, String>,
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
pub struct ConvertConfig {
    /// The longest query, in characters.
    pub max_query_chars: usize,
}

impl Default for ConvertConfig {
    fn default() -> Self {
        Self { max_query_chars: 300 }
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
    /// The most results kept from one site.
    pub max_results_per_site: usize,
    /// The most works in one answer (a request may ask for fewer with limit).
    pub max_results: usize,
    /// Names of the sites (`GET /okazuri/sites`) that are left out.
    pub disabled_sites: Vec<String>,
    /// Also try the sites that cannot be searched with a plain request (a Cloudflare challenge ...).
    pub try_unsupported: bool,
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
            max_results_per_site: 50,
            max_results: 100,
            disabled_sites: Vec::new(),
            try_unsupported: false,
        }
    }
}

/// The copy of the Worker's D1 database (`d1.rs`). Off unless `enabled` is true.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct D1Config {
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
}

impl Default for D1Config {
    fn default() -> Self {
        Self {
            enabled: false,
            account_id: String::new(),
            database_id: String::new(),
            api_token: "${CLOUDFLARE_API_TOKEN}".to_string(),
            tables: vec!["magazines".to_string(), "numbers".to_string()],
            mirror_path: PathBuf::from("d1-mirror.db"),
            check_interval_minutes: 10,
        }
    }
}

/// Searching ahead of time for the issues of the magazines in the mirror (`prefetch.rs`).
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
pub fn load(path: &Path) -> Result<Config> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(format!("cannot read {}: {}", path.display(), e).into()),
    };

    toml::from_str(&text).map_err(|e| format!("{}: {}", path.display(), e).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_is_the_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 8000);
        assert_eq!(config.convert.max_query_chars, 300);
        assert!(config.exceptions.is_empty());
        // the cron is off until it is asked for
        assert!(!config.cron.enabled);
        assert!(config.cron.jobs.is_empty());
        // the search engine and what feeds it are off
        assert!(!config.search.enabled && !config.d1.enabled && !config.prefetch.enabled);
        assert_eq!(config.d1.tables, ["magazines", "numbers"]);
        assert_ne!(config.search.db_path, config.d1.mirror_path, "two database files");
    }

    #[test]
    fn the_romaji_conversion_has_no_switch_and_an_old_section_is_ignored() {
        // [romaji] (and the older [pronounce]) can no longer turn it off: they are not keys of the config
        let config: Config = toml::from_str("[romaji]\nenabled = false\n[pronounce]\nenabled = false\n[server]\nport = 9000\n").unwrap();

        assert_eq!(config.server.port, 9000);
    }

    #[test]
    fn the_functions_are_switched_in_the_file() {
        let config: Config = toml::from_str(
            "[search]\nenabled = true\nttl_hours = 1\ndisabled_sites = [\"Sukebei\"]\n\
             [d1]\nenabled = true\naccount_id = \"a\"\ndatabase_id = \"d\"\n[prefetch]\nenabled = true\n",
        )
        .unwrap();

        assert!(config.search.enabled && config.d1.enabled && config.prefetch.enabled);
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
            "[server]\nport = 9000\n\n[exceptions]\n\"ナース\" = \"nurse\"\n",
        )
        .unwrap();
        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 9000);
        assert_eq!(config.exceptions.get("ナース").map(String::as_str), Some("nurse"));
    }

    #[test]
    fn a_missing_file_is_the_defaults() {
        let config = load(Path::new("no-such-dir/config.toml")).unwrap();
        assert_eq!(config.server.port, 8000);
    }

    #[test]
    fn a_broken_file_is_an_error_that_names_the_file() {
        let path = std::env::temp_dir().join("okazuri-search-api-broken-config.toml");
        fs::write(&path, "[server\nport = ").unwrap();
        let error = load(&path).unwrap_err().to_string();
        fs::remove_file(&path).ok();
        assert!(error.contains("okazuri-search-api-broken-config.toml"), "{error}");
    }
}
