//! Command-line interface: argument parsing and the command table.
//!
//! No arguments starts the server (see `main.rs`). Parsing is separate from running so it can be
//! tested without loading the dictionary or opening a socket.

use crate::api::{self, AppState};
use crate::config::{self, Config};
use crate::cron::{expand, Scheduler};
use crate::d1::D1Client;
use crate::engine::{Engine, Live, Request};
use crate::prefetch::Prefetcher;
use crate::romanizer::{Loanwords, Romanizer};
use crate::store::SearchStore;
use crate::utils::Result;
use std::io::BufRead;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

pub const USAGE: &str = "okazuri-search-api\n\n\
Usage: okazuri-search-api [command]\n\n\
(no command)                           Start the server (same as `serve`)\n\
serve                                  Start the server, and what the config switches on: [search],\n\
                                       [d1] with [prefetch], and [cron]\n\
    --config <path>      Config file (default: config.toml in the current directory;\n\
                         a file that does not exist means the defaults)\n\
    --host <address>     Address to listen on (default: [server] host, 127.0.0.1)\n\
    --port <port>        Port to listen on (default: [server] port, 8000)\n\
search <words>...                      Search the sites for the words (in romaji), save what they list, and print the\n\
                                       saved works whose title has all the words\n\
    --live <off|diff|on> off: do not ask the sites, answer from the database; diff (default): ask the\n\
                         queries and sites that are missing or older than [search] ttl_hours; on: ask every\n\
                         site\n\
    --limit <n>          The most works to print (default: [search] max_results)\n\
    --config <path>      Config file\n\
sync                                   Copy the magazine tables of the D1 database now ([d1])\n\
    --config <path>      Config file\n\
prefetch                               Search for the issues of the copy once through ([prefetch])\n\
    --limit <n>          The most queries (default: [prefetch] max_queries_per_round)\n\
    --config <path>      Config file\n\
convert [text]...                      Print the romaji of the text. Without text, every line of the\n\
                                       standard input is converted and printed as a line\n\
    --loanwords <romaji|english>       How to write loanwords spelled in hiragana (default: romaji)\n\
    --config <path>      Config file, for [exceptions]\n\
--help, -h                             Show this help\n\n\
GET /romaji?q=<text>[&loanwords=romaji|english]   the romaji as text/plain\n\
GET /okazuri/search?q=<words>[&live=off|diff|on][&limit=n]\n\
                                                     the works whose title has all the words ([search])\n\
GET /okazuri/sites                                   the sites                          ([search])\n\
GET /health                                          ok";

#[derive(Debug, PartialEq)]
pub enum Command {
    Serve { config: Option<PathBuf>, host: Option<String>, port: Option<u16> },
    Convert { texts: Vec<String>, loanwords: Loanwords, config: Option<PathBuf> },
    Search { query: String, live: Live, limit: Option<usize>, config: Option<PathBuf> },
    Sync { config: Option<PathBuf> },
    Prefetch { limit: Option<usize>, config: Option<PathBuf> },
    Help,
}

pub fn parse(args: &[String]) -> Result<Command> {
    let Some(first) = args.first() else {
        return Ok(Command::Serve { config: None, host: None, port: None });
    };
    let rest = &args[1..];

    match first.as_str() {
        "--help" | "-h" | "help" => Ok(Command::Help),
        "serve" => parse_serve(rest),
        "convert" => parse_convert(rest),
        "search" => parse_search(rest),
        "sync" => parse_sync(rest),
        "prefetch" => parse_prefetch(rest),
        // options with no command are the options of `serve`
        arg if arg.starts_with("--") => parse_serve(args),
        other => Err(format!("unknown command: {other}\n\n{USAGE}").into()),
    }
}

fn parse_serve(args: &[String]) -> Result<Command> {
    let (mut config, mut host, mut port) = (None, None, None);
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => config = Some(PathBuf::from(value(&mut iter, arg)?)),
            "--host" => host = Some(value(&mut iter, arg)?),
            "--port" => {
                let text = value(&mut iter, arg)?;
                port = Some(text.parse::<u16>().map_err(|_| format!("--port needs a port number, got {text}"))?);
            }
            other => return Err(format!("unknown option: {other}\n\n{USAGE}").into()),
        }
    }

    Ok(Command::Serve { config, host, port })
}

fn parse_convert(args: &[String]) -> Result<Command> {
    let (mut texts, mut loanwords, mut config) = (Vec::new(), Loanwords::Romaji, None);
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--loanwords" => {
                loanwords = match value(&mut iter, arg)?.as_str() {
                    "romaji" => Loanwords::Romaji,
                    "english" => Loanwords::English,
                    other => return Err(format!("--loanwords is romaji or english, got {other}").into()),
                }
            }
            "--config" => config = Some(PathBuf::from(value(&mut iter, arg)?)),
            other => texts.push(other.to_string()),
        }
    }

    Ok(Command::Convert { texts, loanwords, config })
}

fn parse_search(args: &[String]) -> Result<Command> {
    let (mut words, mut live, mut limit, mut config) = (Vec::new(), Live::Diff, None, None);
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--live" => {
                let text = value(&mut iter, arg)?;

                live = Live::parse(&text).ok_or_else(|| format!("--live is off, diff or on, not {text:?}"))?;
            }
            "--limit" => limit = Some(number(&mut iter, arg)?),
            "--config" => config = Some(PathBuf::from(value(&mut iter, arg)?)),
            other if other.starts_with("--") => return Err(format!("unknown option: {other}\n\n{USAGE}").into()),
            other => words.push(other.to_string()),
        }
    }

    if words.is_empty() {
        return Err(format!("search needs the words to search for\n\n{USAGE}").into());
    }

    Ok(Command::Search { query: words.join(" "), live, limit, config })
}

fn parse_sync(args: &[String]) -> Result<Command> {
    let mut config = None;
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => config = Some(PathBuf::from(value(&mut iter, arg)?)),
            other => return Err(format!("unknown option: {other}\n\n{USAGE}").into()),
        }
    }

    Ok(Command::Sync { config })
}

fn parse_prefetch(args: &[String]) -> Result<Command> {
    let (mut limit, mut config) = (None, None);
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--limit" => limit = Some(number(&mut iter, arg)?),
            "--config" => config = Some(PathBuf::from(value(&mut iter, arg)?)),
            other => return Err(format!("unknown option: {other}\n\n{USAGE}").into()),
        }
    }

    Ok(Command::Prefetch { limit, config })
}

fn value(iter: &mut std::slice::Iter<String>, option: &str) -> Result<String> {
    iter.next().cloned().ok_or_else(|| format!("{option} needs a value").into())
}

fn number(iter: &mut std::slice::Iter<String>, option: &str) -> Result<usize> {
    let text = value(iter, option)?;

    text.parse::<usize>().map_err(|_| format!("{option} needs a number, got {text}").into())
}

pub async fn run(command: Command) -> Result<()> {
    match command {
        Command::Help => {
            println!("{USAGE}");
            Ok(())
        }
        Command::Serve { config, host, port } => serve(config, host, port).await,
        Command::Convert { texts, loanwords, config } => convert(texts, loanwords, config),
        Command::Search { query, live, limit, config } => {
            search(query, live, limit, config).await
        }
        Command::Sync { config } => sync(config).await,
        Command::Prefetch { limit, config } => prefetch(limit, config).await,
    }
}

fn load(config: Option<PathBuf>) -> Result<Config> {
    config::load(&config.unwrap_or_else(config::default_path))
}

/// The romaji conversion, which the server always has. Loading the dictionary takes a moment.
fn load_romanizer(config: &Config) -> Result<Arc<Romanizer>> {
    log::info!("loading the dictionary");

    Ok(Arc::new(Romanizer::new(&config.exceptions)?))
}

/// The engine. It turns every query into romaji, so it needs the romaji conversion.
async fn open_engine(config: &Config, romanizer: Arc<Romanizer>) -> Result<Arc<Engine>> {
    let store = SearchStore::open(&config.search.db_path).await?;

    Ok(Engine::new(&config.search, Arc::new(store), romanizer))
}

fn d1_client(config: &Config) -> Result<D1Client> {
    let d1 = &config.d1;

    if d1.account_id.trim().is_empty() || d1.database_id.trim().is_empty() {
        return Err("[d1] needs account_id and database_id".into());
    }

    let token = expand(&d1.api_token, &|name| std::env::var(name).ok())?;

    if token.trim().is_empty() {
        return Err("[d1] api_token is empty".into());
    }

    Ok(D1Client::new(d1, token.trim().to_string()))
}

/// Everything `serve` runs next to the HTTP server, checked before the dictionary is loaded.
struct Background {
    d1: Option<Arc<D1Client>>,
    prefetch: bool,
}

impl Background {
    fn check(config: &Config) -> Result<Self> {
        let d1 = if config.d1.enabled { Some(Arc::new(d1_client(config)?)) } else { None };

        if config.prefetch.enabled {
            if !config.search.enabled {
                return Err("[prefetch] needs [search] enabled = true".into());
            }
            if !config.d1.enabled && !config.d1.mirror_path.exists() {
                log::warn!("[prefetch] is on, but there is no {} and [d1] is off", config.d1.mirror_path.display());
            }
        }

        Ok(Self { d1, prefetch: config.prefetch.enabled })
    }
}

/// Asks D1 now and then whether the data changed, and copies it when it did. Wakes the prefetch then.
async fn sync_loop(d1: Arc<D1Client>, mirror: PathBuf, interval: Duration, wake: Arc<Notify>) {
    loop {
        match d1.sync(&mirror).await {
            Ok(synced) if synced.changed => {
                log::info!("d1: the data changed, the copy was rebuilt (bookmark {})", synced.bookmark);
                wake.notify_one();
            }
            Ok(synced) if synced.exported => log::info!(
                "d1: the bookmark moved ({}) but magazines and numbers are the same: the copy and the prefetch are left as they are",
                synced.bookmark
            ),
            Ok(_) => log::debug!("d1: the data did not change"),
            Err(e) => log::warn!("d1: {e}"),
        }

        tokio::time::sleep(interval).await;
    }
}

async fn serve(config: Option<PathBuf>, host: Option<String>, port: Option<u16>) -> Result<()> {
    let config = load(config)?;
    let host = host.unwrap_or(config.server.host.clone());
    let port = port.unwrap_or(config.server.port);

    // all of this is checked before the dictionary is loaded, so a mistake in the settings shows at once
    let scheduler = Scheduler::new(&config.cron, &|name| std::env::var(name).ok())?;
    let background = Background::check(&config)?;

    let romanizer = load_romanizer(&config)?;
    let engine = if config.search.enabled { Some(open_engine(&config, romanizer.clone()).await?) } else { None };

    let state = Arc::new(AppState {
        romanizer: romanizer.clone(),
        max_query_chars: config.convert.max_query_chars,
        search: engine.clone(),
    });

    // an IPv6 address has to be in brackets
    let address = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    let listener = TcpListener::bind(&address)
        .await
        .map_err(|e| format!("cannot listen on {address}: {e}"))?;
    log::info!("listening on http://{}", listener.local_addr()?);
    log::info!(
        "search: {}, d1: {}, prefetch: {}, cron: {}",
        on_off(config.search.enabled),
        on_off(config.d1.enabled),
        on_off(config.prefetch.enabled),
        on_off(scheduler.is_some()),
    );

    let mut tasks: Vec<JoinHandle<()>> = Vec::new();
    let wake = Arc::new(Notify::new());

    if let Some(cron) = scheduler {
        tasks.push(cron.spawn());
    }
    if let Some(d1) = background.d1 {
        let interval = Duration::from_secs(config.d1.check_interval_minutes.max(1) * 60);

        tasks.push(tokio::spawn(sync_loop(d1, config.d1.mirror_path.clone(), interval, wake.clone())));
    }
    if let (true, Some(engine)) = (background.prefetch, engine) {
        let prefetcher =
            Arc::new(Prefetcher::new(engine, config.d1.mirror_path.clone(), config.prefetch.clone()));

        tasks.push(tokio::spawn(prefetcher.run(wake)));
    }

    axum::serve(listener, api::router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    for task in tasks {
        task.abort();
    }

    log::info!("stopped");
    Ok(())
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

fn convert(texts: Vec<String>, loanwords: Loanwords, config: Option<PathBuf>) -> Result<()> {
    let config = load(config)?;
    let romanizer = Romanizer::new(&config.exceptions)?;

    if texts.is_empty() {
        for line in std::io::stdin().lock().lines() {
            println!("{}", romanizer.romanize(&line?, loanwords)?);
        }
    } else {
        for text in texts {
            println!("{}", romanizer.romanize(&text, loanwords)?);
        }
    }

    Ok(())
}

/// One search from the command line, whether `[search]` is on or not (the settings are the config's).
async fn search(
    text: String,
    live: Live,
    limit: Option<usize>,
    config: Option<PathBuf>,
) -> Result<()> {
    let mut config = load(config)?;
    // the command is the wish to search (and the engine needs the romaji conversion)
    config.search.enabled = true;
    let engine = open_engine(&config, Arc::new(Romanizer::new(&config.exceptions)?)).await?;

    let limit = limit.unwrap_or(config.search.max_results);
    let outcome = engine.search(&Request { query: &text, live, limit }).await?;

    for site in &outcome.sites {
        println!(
            "{:<12} {:<12} {:>3} works{}",
            site.website,
            site.status,
            site.count,
            site.error.as_ref().map(|e| format!("  ({e})")).unwrap_or_default()
        );
    }

    println!("\n{} works have all of {:?} ({} shown)", outcome.total, outcome.terms, outcome.results.len());
    for hit in &outcome.results {
        println!("{:<12} {}\n{:<12} {}", hit.website, hit.title, "", hit.url);
    }

    Ok(())
}

async fn sync(config: Option<PathBuf>) -> Result<()> {
    let config = load(config)?;
    let synced = Arc::new(d1_client(&config)?).sync(&config.d1.mirror_path).await?;

    println!(
        "{} (bookmark {}) -> {}",
        match (synced.changed, synced.exported) {
            (true, _) => "the data changed, the copy was rebuilt",
            (false, true) => "the bookmark moved, but the tables are the same",
            (false, false) => "the data did not change",
        },
        synced.bookmark,
        config.d1.mirror_path.display()
    );

    Ok(())
}

async fn prefetch(limit: Option<usize>, config: Option<PathBuf>) -> Result<()> {
    let mut config = load(config)?;
    config.search.enabled = true;
    config.prefetch.enabled = true;
    if let Some(limit) = limit {
        config.prefetch.max_queries_per_round = limit;
    }

    let engine = open_engine(&config, Arc::new(Romanizer::new(&config.exceptions)?)).await?;
    let prefetcher = Prefetcher::new(engine, config.d1.mirror_path.clone(), config.prefetch.clone());
    let round = prefetcher.round().await?;

    println!(
        "{} queries searched, {} already searched, {} failed{}",
        round.searched,
        round.skipped,
        round.failed,
        if round.more { " (more to do)" } else { "" }
    );

    Ok(())
}

/// Ctrl+C, and on Unix SIGTERM too (what `systemctl stop` and `docker stop` send).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_arguments_starts_the_server() {
        assert_eq!(parse(&[]).unwrap(), Command::Serve { config: None, host: None, port: None });
    }

    #[test]
    fn serve_options() {
        let command = parse(&args(&["serve", "--host", "0.0.0.0", "--port", "9000", "--config", "a.toml"])).unwrap();
        assert_eq!(
            command,
            Command::Serve {
                config: Some(PathBuf::from("a.toml")),
                host: Some("0.0.0.0".into()),
                port: Some(9000)
            }
        );
        // options alone are the options of `serve`
        assert_eq!(
            parse(&args(&["--port", "1"])).unwrap(),
            Command::Serve { config: None, host: None, port: Some(1) }
        );
    }

    #[test]
    fn convert_options() {
        assert_eq!(
            parse(&args(&["convert", "--loanwords", "english", "あ", "い"])).unwrap(),
            Command::Convert { texts: args(&["あ", "い"]), loanwords: Loanwords::English, config: None }
        );
        assert_eq!(
            parse(&args(&["convert"])).unwrap(),
            Command::Convert { texts: vec![], loanwords: Loanwords::Romaji, config: None }
        );
    }

    #[test]
    fn search_takes_the_words_and_the_options() {
        assert_eq!(
            parse(&args(&["search", "快楽天", "2026", "--limit", "5", "--live", "off", "--config", "c.toml"]))
                .unwrap(),
            Command::Search {
                query: "快楽天 2026".into(),
                live: Live::Off,
                limit: Some(5),
                config: Some(PathBuf::from("c.toml")),
            }
        );
        assert_eq!(
            parse(&args(&["search", "ExE"])).unwrap(),
            Command::Search { query: "ExE".into(), live: Live::Diff, limit: None, config: None }
        );
    }

    #[test]
    fn sync_and_prefetch() {
        assert_eq!(parse(&args(&["sync"])).unwrap(), Command::Sync { config: None });
        assert_eq!(
            parse(&args(&["prefetch", "--limit", "20"])).unwrap(),
            Command::Prefetch { limit: Some(20), config: None }
        );
    }

    #[test]
    fn mistakes_are_errors() {
        assert!(parse(&args(&["nope"])).is_err());
        assert!(parse(&args(&["serve", "--port"])).is_err());
        assert!(parse(&args(&["serve", "--port", "x"])).is_err());
        assert!(parse(&args(&["serve", "--port", "70000"])).is_err());
        assert!(parse(&args(&["serve", "--bogus"])).is_err());
        assert!(parse(&args(&["convert", "--loanwords", "latin"])).is_err());
        assert!(parse(&args(&["search"])).is_err(), "no words");
        assert!(parse(&args(&["search", "--live", "on"])).is_err(), "options are not words");
        assert!(parse(&args(&["search", "ExE", "--live", "sometimes"])).is_err());
        assert!(parse(&args(&["search", "ExE", "--live"])).is_err(), "--live needs a value");
        assert!(parse(&args(&["search", "a", "--limit", "x"])).is_err());
        assert!(parse(&args(&["search", "a", "--bogus"])).is_err());
        assert!(parse(&args(&["sync", "--bogus"])).is_err());
        assert!(parse(&args(&["prefetch", "--limit"])).is_err());
    }

    #[test]
    fn help() {
        assert_eq!(parse(&args(&["--help"])).unwrap(), Command::Help);
        assert_eq!(parse(&args(&["-h"])).unwrap(), Command::Help);
    }

    #[test]
    fn the_background_work_is_checked_before_anything_is_loaded() {
        let off = Config::default();
        assert!(Background::check(&off).is_ok());

        // the prefetch needs the search
        let mut config = Config::default();
        config.prefetch.enabled = true;
        assert!(Background::check(&config).err().expect("an error").to_string().contains("[search]"));

        // a D1 copy needs the ids and a token
        let mut config = Config::default();
        config.d1.enabled = true;
        assert!(Background::check(&config).err().expect("an error").to_string().contains("account_id"));
        config.d1.account_id = "a".into();
        config.d1.database_id = "d".into();
        config.d1.api_token = "${OKAZURI_SEARCH_API_TEST_TOKEN_THAT_IS_NOT_SET}".into();
        assert!(Background::check(&config).err().expect("an error").to_string().contains("is not set"));
        config.d1.api_token = "token".into();
        assert!(Background::check(&config).is_ok());
    }
}
