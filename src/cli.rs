//! Command-line interface: argument parsing and the command table.
//!
//! No arguments starts the server (see `main.rs`). Parsing is separate from running so it can be
//! tested without loading the dictionary or opening a socket.

use crate::api::{self, AppState};
use crate::config::{self, Config};
use crate::cron::{expand, Scheduler};
use crate::d1::D1Client;
use crate::engine::{Engine, Live, Request};
use crate::crawl::Crawler;
use crate::verify::Verifier;
use crate::fill::TitleTranslator;
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
serve                                  Start the server on the port of the config file, and what it switches on: [search],\n\
                                       [magazine] with [magazine.prefetch], and [cron]\n\
    --config <path>      Config file (default: config.toml in the current directory;\n\
                         a file that does not exist means the defaults)\n\
    --host <address>     Address to listen on (default: [server] host, 127.0.0.1)\n\
    --port <port>        Port to listen on (default: [server] port, 8000)\n\
title <url>...                         Print the title of the page of each URL (a saved work first, else the page is asked for),\n\
                                       with its Japanese title, romaji and English translation\n\
    --live <off|diff|force> off: only saved works; diff (default): ask the page when the work is not saved; force: ask the page\n\
    --config <path>      Config file\n\
magazines                              Print the magazines of the copy of the catalogue ([magazine]), with their newest issue\n\
    --config <path>      Config file\n\
magazine <title>                       Print the issues that printed the work with this title (a part of the title is enough)\n\
    --limit <n>          The most works (default: 10)\n\
    --config <path>      Config file\n\
works <issue>                          Print the works of the issue with this name or id, in the order of the issue\n\
    --config <path>      Config file\n\
search <words>...                      Search the sites for the words (in romaji), save what they list, and print the\n\
                                       saved works whose title has all the words\n\
    --live <off|diff|force> off: do not ask the sites, answer from the database; diff (default): ask the\n\
                         queries and sites that are missing or older than [search] ttl_hours; force: ask every\n\
                         site\n\
    --limit <n>          The most works to print (default: [search] max_results)\n\
    --config <path>      Config file\n\
count <words>...                       Count the saved works whose title has all the words (the search itself only tells whether
                                       there is a next page), and how many each site has
    --config <path>      Config file
cloudflare-check                       Ask the top page of each site behind Cloudflare, to see which ones this machine
                                       gets through to
    --config <path>      Config file
rewalk <site>...                       Read the whole list of each site again from the first page (a site whose tags were not read before, say): the done mark and the
                                       pages read are cleared, the saved works stay. Works while the server runs; the server then needs a restart to start the walk
    --config <path>      Config file
stats                                  Print what the database holds: the works of each site (gone, not yet looked at by the
                                       verification), the crawl of each site, the translations by source. Safe while the server runs
    --config <path>      Config file
build-index                            Fill the text index (the directory `<db_path>.index`) from the works when it is not complete, and exit. The
                                       server does the same when it starts: this does it before, while the server runs the old version
    --config <path>      Config file
drop-legacy-index                      Drop the FTS5 tables of the older versions from the database (the free space stays in the file)
    --config <path>      Config file
sync                                   Copy the magazine tables of the D1 database now ([magazine])\n\
    --config <path>      Config file\n\
prefetch                               Search for the issues of the copy once through ([magazine.prefetch])\n\
    --limit <n>          The most queries (default: [magazine.prefetch] max_queries_per_round)\n\
    --config <path>      Config file\n\
reromanize                             Make the romaji of the saved works again with the rules of this version, and\n\
                                       forget the searches that listed a work whose romaji changed (the prefetch,\n\
                                       which the server runs at its start, asks the sites for them again). Stop the\n\
                                       server first\n\
    --config <path>      Config file\n\
relanguage                             Read the language of every saved title again with the rules of this version (`works.language`),
                                       and write the works whose language is different now. Safe while the server runs
    --config <path>      Config file
translations                           Make the translations of Hitomi.La from the saved works (the titles `English | 日本語`), so
                                       `/hitomila/translate` can answer from the database (`import --mark-done Hitomi.La` does it too)
    --config <path>      Config file
retranslate --source <hitomi|google>    Make the translations of a source again. `hitomi`: build the Hitomi.La pairs again, then drop the Google
                                       translations of the titles that Hitomi.La translates now, and the lookups that found nothing. `google`:
                                       drop every Google translation and lookup, so `[translation.fill]` asks Google again (Hitomi.La's stay).
                                       Safe while the server runs
    --config <path>      Config file
export <file>                          Write the saved works to a file (JSON Lines, compressed with brotli), to put the works
                                       that a machine crawled into the database of another (`import`)
    --site <name>        Only the works of this site (more than once for more sites)
    --since <unix time>  Only the works saved at this time or later
    --config <path>      Config file
dataset <directory>                    Write the saved works as a public dataset: works.jsonl.gz (the site, whether it is legal, the
                                       address, the title, the language, first_seen, checked_at) and meta.json. No searches, no romaji
    --config <path>      Config file
import <file>...                        Read the files that `export` wrote into the database; the works saved already stay as they are
    --mark-done <name>   The whole list of this site is read now (the files of every shard are in): the crawl then reads only its
                         newest pages again, and the prefetch leaves the site to it (more than once for more sites)
    --config <path>      Config file
convert [text]...                      Print the romaji of the text. Without text, every line of the\n\
                                       standard input is converted and printed as a line\n\
    --loanwords <romaji|english>       How to write loanwords spelled in hiragana (default: romaji)\n\
    --config <path>      Config file, for [romaji]\n\
--help, -h                             Show this help\n\n\
GET /romaji?q=<text>[&loanwords=romaji|english]   the romaji as text/plain\n\
GET /okazuri/search?q=<words>[&live=off|diff|force][&limit=n]\n\
                                                     the works whose title has all the words ([search])\n\
GET /okazuri/sites                                   the sites                          ([search])\n\
GET /health                                          ok";

#[derive(Debug, PartialEq)]
pub enum Command {
    Serve { config: Option<PathBuf>, host: Option<String>, port: Option<u16> },
    Convert { texts: Vec<String>, loanwords: Loanwords, config: Option<PathBuf> },
    Search { query: String, live: Live, limit: Option<usize>, config: Option<PathBuf> },
    Title { urls: Vec<String>, live: Live, config: Option<PathBuf> },
    Magazines { config: Option<PathBuf> },
    Magazine { title: String, limit: u64, config: Option<PathBuf> },
    Works { issue: String, config: Option<PathBuf> },
    Count { query: String, config: Option<PathBuf> },
    Stats { config: Option<PathBuf> },
    Rewalk { sites: Vec<String>, config: Option<PathBuf> },
    CloudflareCheck { config: Option<PathBuf> },
    BuildIndex { config: Option<PathBuf> },
    DropLegacyIndex { config: Option<PathBuf> },
    Sync { config: Option<PathBuf> },
    Prefetch { limit: Option<usize>, config: Option<PathBuf> },
    Reromanize { config: Option<PathBuf> },
    Relanguage { config: Option<PathBuf> },
    Translations { config: Option<PathBuf> },
    Retranslate { source: String, config: Option<PathBuf> },
    Export { file: PathBuf, sites: Vec<String>, since: i64, config: Option<PathBuf> },
    Import { files: Vec<PathBuf>, mark_done: Vec<String>, config: Option<PathBuf> },
    Dataset { dir: PathBuf, config: Option<PathBuf> },
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
        "title" => parse_title(rest),
        "magazines" => parse_config_only(rest, "magazines").map(|config| Command::Magazines { config }),
        "magazine" => parse_magazine(rest),
        "works" => parse_works(rest),
        "count" => parse_count(rest),
        "stats" => parse_config_only(rest, "stats").map(|config| Command::Stats { config }),
        "rewalk" => parse_rewalk(rest),
        "cloudflare-check" => parse_config_only(rest, "cloudflare-check").map(|config| Command::CloudflareCheck { config }),
        "build-index" => parse_config_only(rest, "build-index").map(|config| Command::BuildIndex { config }),
        "drop-legacy-index" => parse_config_only(rest, "drop-legacy-index").map(|config| Command::DropLegacyIndex { config }),
        "sync" => parse_sync(rest),
        "prefetch" => parse_prefetch(rest),
        "reromanize" => parse_reromanize(rest),
        "relanguage" => parse_relanguage(rest),
        "translations" => parse_translations(rest),
        "retranslate" => parse_retranslate(rest),
        "export" => parse_export(rest),
        "import" => parse_import(rest),
        "dataset" => parse_dataset(rest),
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

                live = Live::parse(&text).ok_or_else(|| format!("--live is off, diff or force, not {text:?}"))?;
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

fn parse_count(args: &[String]) -> Result<Command> {
    let (mut words, mut config) = (Vec::new(), None);
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => config = Some(PathBuf::from(value(&mut iter, arg)?)),
            other if other.starts_with("--") => return Err(format!("unknown option: {other}\n\n{USAGE}").into()),
            other => words.push(other.to_string()),
        }
    }

    if words.is_empty() {
        return Err(format!("count needs the words to count the works of\n\n{USAGE}").into());
    }

    Ok(Command::Count { query: words.join(" "), config })
}

fn parse_title(args: &[String]) -> Result<Command> {
    let (mut urls, mut live, mut config) = (Vec::new(), Live::Diff, None);
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--live" => {
                let text = value(&mut iter, arg)?;

                live = Live::parse(&text).ok_or_else(|| format!("--live is off, diff or force, not {text:?}"))?;
            }
            "--config" => config = Some(PathBuf::from(value(&mut iter, arg)?)),
            other if other.starts_with("--") => return Err(format!("unknown option: {other}\n\n{USAGE}").into()),
            other => urls.push(other.to_string()),
        }
    }

    if urls.is_empty() {
        return Err(format!("title needs the URLs of the pages\n\n{USAGE}").into());
    }

    Ok(Command::Title { urls, live, config })
}

fn parse_config_only(args: &[String], command: &str) -> Result<Option<PathBuf>> {
    let mut config = None;
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => config = Some(PathBuf::from(value(&mut iter, arg)?)),
            other => return Err(format!("{command} takes only --config, not {other}\n\n{USAGE}").into()),
        }
    }

    Ok(config)
}

fn parse_magazine(args: &[String]) -> Result<Command> {
    let (mut words, mut limit, mut config) = (Vec::new(), 10, None);
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--limit" => limit = number(&mut iter, arg)? as u64,
            "--config" => config = Some(PathBuf::from(value(&mut iter, arg)?)),
            other if other.starts_with("--") => return Err(format!("unknown option: {other}\n\n{USAGE}").into()),
            other => words.push(other.to_string()),
        }
    }

    if words.is_empty() {
        return Err(format!("magazine needs the title of a work\n\n{USAGE}").into());
    }

    if limit == 0 || limit > crate::mirror::MAX_PRINTED_WORKS {
        return Err(format!("--limit is 1 to {}", crate::mirror::MAX_PRINTED_WORKS).into());
    }

    Ok(Command::Magazine { title: words.join(" "), limit, config })
}

fn parse_works(args: &[String]) -> Result<Command> {
    let (mut words, mut config) = (Vec::new(), None);
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => config = Some(PathBuf::from(value(&mut iter, arg)?)),
            other if other.starts_with("--") => return Err(format!("unknown option: {other}\n\n{USAGE}").into()),
            other => words.push(other.to_string()),
        }
    }

    if words.is_empty() {
        return Err(format!("works needs the name or id of an issue\n\n{USAGE}").into());
    }

    Ok(Command::Works { issue: words.join(" "), config })
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

fn parse_reromanize(args: &[String]) -> Result<Command> {
    let mut config = None;
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => config = Some(PathBuf::from(value(&mut iter, arg)?)),
            other => return Err(format!("unknown option: {other}\n\n{USAGE}").into()),
        }
    }

    Ok(Command::Reromanize { config })
}

fn parse_translations(args: &[String]) -> Result<Command> {
    match parse_reromanize(args)? {
        Command::Reromanize { config } => Ok(Command::Translations { config }),
        other => Ok(other),
    }
}

fn parse_retranslate(args: &[String]) -> Result<Command> {
    let (mut source, mut config) = (None, None);
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--source" => source = Some(value(&mut iter, arg)?),
            "--config" => config = Some(PathBuf::from(value(&mut iter, arg)?)),
            other => return Err(format!("unknown option: {other}\n\n{USAGE}").into()),
        }
    }

    match source.as_deref() {
        Some(name @ ("hitomi" | "google")) => Ok(Command::Retranslate {
            source: name.to_string(),
            config,
        }),
        Some(other) => Err(format!("--source is hitomi or google, not {other}\n\n{USAGE}").into()),
        None => Err(format!("retranslate needs --source <hitomi|google>\n\n{USAGE}").into()),
    }
}

fn parse_relanguage(args: &[String]) -> Result<Command> {
    match parse_reromanize(args)? {
        Command::Reromanize { config } => Ok(Command::Relanguage { config }),
        other => Ok(other),
    }
}

fn parse_export(args: &[String]) -> Result<Command> {
    let (mut file, mut sites, mut since, mut config) = (None, Vec::new(), 0, None);
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--site" => sites.push(value(&mut iter, arg)?),
            "--since" => {
                let text = value(&mut iter, arg)?;
                since = text.parse::<i64>().map_err(|_| format!("--since needs a unix time, got {text}"))?;
            }
            "--config" => config = Some(PathBuf::from(value(&mut iter, arg)?)),
            other if other.starts_with("--") => return Err(format!("unknown option: {other}\n\n{USAGE}").into()),
            other if file.is_none() => file = Some(PathBuf::from(other)),
            other => return Err(format!("export writes one file, got {other} too").into()),
        }
    }

    Ok(Command::Export { file: file.ok_or("export needs the file to write")?, sites, since, config })
}

fn parse_dataset(args: &[String]) -> Result<Command> {
    let (mut dir, mut config) = (None, None);
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => config = Some(PathBuf::from(value(&mut iter, arg)?)),
            other if other.starts_with("--") => return Err(format!("unknown option: {other}\n\n{USAGE}").into()),
            other if dir.is_none() => dir = Some(PathBuf::from(other)),
            other => return Err(format!("dataset takes one directory, not also {other}\n\n{USAGE}").into()),
        }
    }

    Ok(Command::Dataset { dir: dir.ok_or("dataset needs the directory to write")?, config })
}

fn parse_import(args: &[String]) -> Result<Command> {
    let (mut files, mut mark_done, mut config) = (Vec::new(), Vec::new(), None);
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--mark-done" => mark_done.push(value(&mut iter, arg)?),
            "--config" => config = Some(PathBuf::from(value(&mut iter, arg)?)),
            other if other.starts_with("--") => return Err(format!("unknown option: {other}\n\n{USAGE}").into()),
            other => files.push(PathBuf::from(other)),
        }
    }
    if files.is_empty() {
        return Err("import needs the files to read".into());
    }

    Ok(Command::Import { files, mark_done, config })
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
        Command::Convert { texts, loanwords, config } => convert(texts, loanwords, config).await,
        Command::Search { query, live, limit, config } => {
            search(query, live, limit, config).await
        }
        Command::Title { urls, live, config } => title(urls, live, config).await,
        Command::Magazines { config } => magazines(config).await,
        Command::Magazine { title, limit, config } => magazine(title, limit, config).await,
        Command::Works { issue, config } => works(issue, config).await,
        Command::Count { query, config } => count(query, config).await,
        Command::Stats { config } => stats(config).await,
        Command::Rewalk { sites, config } => rewalk(sites, config).await,
        Command::CloudflareCheck { config } => cloudflare_check(config).await,
        Command::BuildIndex { config } => build_index(config).await,
        Command::DropLegacyIndex { config } => drop_legacy_index(config).await,
        Command::Sync { config } => sync(config).await,
        Command::Prefetch { limit, config } => prefetch(limit, config).await,
        Command::Reromanize { config } => reromanize(config).await,
        Command::Relanguage { config } => relanguage(config).await,
        Command::Translations { config } => translations(config).await,
        Command::Retranslate { source, config } => retranslate(&source, config).await,
        Command::Export { file, sites, since, config } => export(file, sites, since, config).await,
        Command::Import { files, mark_done, config } => import(files, mark_done, config).await,
        Command::Dataset { dir, config } => dataset(dir, config).await,
    }
}

async fn load(config: Option<PathBuf>) -> Result<Config> {
    config::load(&config.unwrap_or_else(config::default_path)).await
}

/// The romaji conversion, which the server always has. Loading the dictionary takes a moment.
fn load_romanizer(config: &Config) -> Result<Arc<Romanizer>> {
    log::info!("loading the dictionary");

    Ok(Arc::new(Romanizer::new(&config.romaji.exceptions)?))
}

/// The engine. It turns every query into romaji, so it needs the romaji conversion.
async fn open_engine(config: &Config, romanizer: Arc<Romanizer>) -> Result<Arc<Engine>> {
    let store = SearchStore::open(&config.search.db_path).await?;
    store.set_max_candidates(config.search.max_candidates);
    store.set_kind_boost(&config.search.tag_boost)?;

    // the bridge of Google Translate is in [translation], the engine reads it with the settings of the search
    let mut search = config.search.clone();
    search.google_bridge_url = config.translation.google_bridge_url.clone();
    search.google_bridge_token = config.translation.google_bridge_token.clone();
    search.cloudflare = config.cloudflare.clone();
    if config.cloudflare.enabled && config.cloudflare.api.trim().is_empty() {
        return Err("[cloudflare] enabled = true needs api (the address of the server that gives the cookies)".into());
    }

    Ok(Engine::new(&search, Arc::new(store), romanizer))
}

fn d1_client(config: &Config) -> Result<D1Client> {
    let d1 = &config.magazine;

    if d1.account_id.trim().is_empty() || d1.database_id.trim().is_empty() {
        return Err("[magazine] needs account_id and database_id".into());
    }

    let token = expand(&d1.api_token, &|name| std::env::var(name).ok())?;

    if token.trim().is_empty() {
        return Err("[magazine] api_token is empty".into());
    }

    Ok(D1Client::new(d1, token.trim().to_string()))
}

/// Everything `serve` runs next to the HTTP server, checked before the dictionary is loaded.
struct Background {
    d1: Option<Arc<D1Client>>,
    prefetch: bool,
    crawl: bool,
    verify: bool,
    fill: bool,
}

impl Background {
    fn check(config: &Config) -> Result<Self> {
        let d1 = if config.magazine.enabled { Some(Arc::new(d1_client(config)?)) } else { None };

        if config.magazine.prefetch.enabled {
            if !config.search.enabled {
                return Err("[magazine.prefetch] needs [search] enabled = true".into());
            }
            if !config.magazine.enabled && !config.magazine.mirror_path.exists() {
                log::warn!("[magazine.prefetch] is on, but there is no {} and [magazine] is off", config.magazine.mirror_path.display());
            }
        }

        if config.crawl.enabled && !config.search.enabled {
            return Err("[crawl] needs [search] enabled = true".into());
        }

        if config.crawl.verify.enabled && !config.search.enabled {
            return Err("[crawl.verify] needs [search] enabled = true".into());
        }

        if config.translation.fill.enabled && !config.search.enabled {
            return Err("[translation.fill] needs [search] enabled = true".into());
        }

        Ok(Self {
            d1,
            prefetch: config.magazine.prefetch.enabled,
            crawl: config.crawl.enabled,
            verify: config.crawl.verify.enabled,
            fill: config.translation.fill.enabled,
        })
    }
}

/// Asks D1 now and then whether the data changed, and copies it when it did. Wakes the prefetch then.
async fn sync_loop(d1: Arc<D1Client>, mirror: PathBuf, interval: Duration, wake: Arc<Notify>) {
    loop {
        match d1.sync(&mirror).await {
            Ok(synced) if synced.changed => {
                log::info!("magazine: the data changed, the copy was rebuilt (bookmark {})", synced.bookmark);
                wake.notify_one();
            }
            Ok(synced) if synced.exported => log::info!(
                "magazine: the bookmark moved ({}) but magazines and numbers are the same: the copy and the prefetch are left as they are",
                synced.bookmark
            ),
            Ok(_) => log::debug!("magazine: the data did not change"),
            Err(e) => log::warn!("magazine: {e}"),
        }

        tokio::time::sleep(interval).await;
    }
}

async fn serve(config: Option<PathBuf>, host: Option<String>, port: Option<u16>) -> Result<()> {
    let config = load(config).await?;
    let host = host.unwrap_or(config.server.host.clone());
    let port = port.unwrap_or(config.server.port);

    // all of this is checked before the dictionary is loaded, so a mistake in the settings shows at once
    let scheduler = Scheduler::new(&config.cron, &|name| std::env::var(name).ok())?;
    let background = Background::check(&config)?;

    let romanizer = load_romanizer(&config)?;
    let engine = if config.search.enabled { Some(open_engine(&config, romanizer.clone()).await?) } else { None };

    let hub = (config.distribute.role == "host").then(|| Arc::new(crate::dist::Hub::new(config.distribute.token.clone())));
    if config.distribute.role == "host" && config.distribute.token.is_empty() {
        return Err("[distribute] role = \"host\" needs a token".into());
    }
    if !matches!(config.distribute.role.as_str(), "off" | "host" | "worker") {
        return Err(format!("[distribute] role must be off, host or worker, not {:?}", config.distribute.role).into());
    }

    // a worker, and a process that does not keep the text index, only read pages: they answer /health and nothing else
    let lean = config.distribute.role == "worker" || std::env::var("OKAZURI_TEXT_INDEX").is_ok_and(|value| value == "off");
    let state = Arc::new(AppState {
        lean,
        hub: hub.clone(),
        romanizer: romanizer.clone(),
        max_query_chars: config.romaji.max_query_chars,
        search: engine.clone().filter(|_| !lean),
        crawling: if config.crawl.enabled { crate::crawl::crawled_sites(&config.crawl) } else { Default::default() },
        mirror_path: (!lean && (config.magazine.enabled || config.magazine.mirror_path.exists())).then(|| config.magazine.mirror_path.clone()),
    });

    // an IPv6 address has to be in brackets
    let address = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
    let listener = TcpListener::bind(&address)
        .await
        .map_err(|e| format!("cannot listen on {address}: {e}"))?;
    log::info!("listening on http://{}", listener.local_addr()?);
    log::info!(
        "search: {}, magazine: {}, prefetch: {}, crawl: {}, verify: {}, fill: {}, cron: {}",
        on_off(config.search.enabled),
        on_off(config.magazine.enabled),
        on_off(config.magazine.prefetch.enabled),
        on_off(config.crawl.enabled),
        on_off(config.crawl.verify.enabled),
        on_off(config.translation.fill.enabled),
        on_off(scheduler.is_some()),
    );

    let mut tasks: Vec<JoinHandle<()>> = Vec::new();
    let wake = Arc::new(Notify::new());

    if let Some(cron) = scheduler {
        tasks.push(cron.spawn());
    }
    if config.search.enabled && config.search.pin_db {
        tasks.push(tokio::spawn(crate::pin::run(config.search.db_path.clone())));
    }
    if let Some(d1) = background.d1 {
        let interval = Duration::from_secs(config.magazine.check_interval_minutes.max(1) * 60);

        tasks.push(tokio::spawn(sync_loop(d1, config.magazine.mirror_path.clone(), interval, wake.clone())));
    }
    if let (true, Some(engine)) = (background.crawl, engine.clone()) {
        // checked here, with the engine: a site that is not known or a shard that is wrong stops the server before it listens
        let mut crawler = Crawler::new(engine.clone(), config.crawl.clone())?;
        let mut verifier = if background.verify { Some(Verifier::new(engine.clone(), config.crawl.verify.clone(), &config.crawl.disabled_sites)?) } else { None };

        if let Some(hub) = &hub {
            crawler = crawler
                .with_hub(hub.clone(), config.distribute.workers * 2, config.distribute.job_pages, config.distribute.job_seconds)
                .with_probe_sites(verifier.as_ref().map(|verifier| verifier.site_names()).unwrap_or_default());
            verifier = verifier.map(|verifier| verifier.with_hub(hub.clone(), config.distribute.workers));
        }
        let crawler = Arc::new(crawler);

        if config.distribute.role == "worker" {
            // the pages are read for the host: no walk of its own, no verification
            let verified = verifier.as_ref().map(|verifier| verifier.site_names()).unwrap_or_default();
            if config.distribute.host.is_empty() || config.distribute.token.is_empty() {
                return Err("[distribute] role = \"worker\" needs host and token".into());
            }
            tasks.push(tokio::spawn(crate::dist::work(engine.clone(), crawler, config.distribute.clone(), verified)));
        } else {
            // the host reads pages too (as one of the workers)
            if let Some(hub) = &hub {
                let verified = verifier.as_ref().map(|verifier| verifier.site_names()).unwrap_or_default();
                tasks.push(tokio::spawn(crate::dist::work_local(hub.clone(), engine.clone(), crawler.clone(), verified)));
            }
            tasks.push(tokio::spawn(crawler.run()));
            if let Some(verifier) = verifier {
                tasks.push(tokio::spawn(Arc::new(verifier).run()));
            }
        }
    } else if let (true, Some(engine)) = (background.verify, engine.clone()) {
        let mut verifier = Verifier::new(engine.clone(), config.crawl.verify.clone(), &config.crawl.disabled_sites)?;
        if let Some(hub) = &hub {
            verifier = verifier.with_hub(hub.clone(), config.distribute.workers);
        }
        if config.distribute.role == "worker" {
            let crawler = Arc::new(Crawler::new(engine.clone(), config.crawl.clone())?);
            tasks.push(tokio::spawn(crate::dist::work(engine, crawler, config.distribute.clone(), verifier.site_names())));
        } else {
            tasks.push(tokio::spawn(Arc::new(verifier).run()));
        }
    }
    if let (true, Some(engine)) = (background.fill, engine.clone()) {
        tasks.push(tokio::spawn(Arc::new(TitleTranslator::new(engine, config.translation.fill.clone())).run()));
    }
    if let (true, Some(engine)) = (background.prefetch, engine) {
        let prefetcher =
            Arc::new(Prefetcher::new(engine, config.magazine.mirror_path.clone(), config.magazine.prefetch.clone()).with_crawl(config.crawl.enabled));

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

async fn convert(texts: Vec<String>, loanwords: Loanwords, config: Option<PathBuf>) -> Result<()> {
    let config = load(config).await?;
    let romanizer = Romanizer::new(&config.romaji.exceptions)?;

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
    let mut config = load(config).await?;
    // the command is the wish to search (and the engine needs the romaji conversion)
    config.search.enabled = true;
    let engine = open_engine(&config, Arc::new(Romanizer::new(&config.romaji.exceptions)?)).await?;

    let limit = limit.unwrap_or(config.search.max_results);
    let outcome = engine.search(&Request { query: &text, live, limit, translator: crate::engine::Translator::None, exclude: &[] }).await?;

    for site in &outcome.sites {
        println!(
            "{:<12} {:<12} {:>3} works{}",
            site.website,
            site.status,
            site.count,
            site.error.as_ref().map(|e| format!("  ({e})")).unwrap_or_default()
        );
    }

    println!("\n{} works have all of {:?}{}", outcome.results.len(), outcome.terms, if outcome.has_more { " (and more)" } else { "" });
    for hit in &outcome.results {
        println!("{:<12} {}\n{:<12} {}", hit.website, hit.title, "", hit.url);
    }

    Ok(())
}

/// The titles of the pages, one block per URL (the same lookup as `GET /okazuri/title`).
async fn title(urls: Vec<String>, live: Live, config: Option<PathBuf>) -> Result<()> {
    let mut config = load(config).await?;
    config.search.enabled = true;
    let engine = open_engine(&config, Arc::new(Romanizer::new(&config.romaji.exceptions)?)).await?;

    for url in urls {
        println!("{url}");

        match engine.title_of(&url, live).await {
            Ok(info) => {
                println!("  website        {}", info.website);
                println!("  title          {}", info.title);
                println!("  title_japanese {}", info.title_japanese.as_deref().unwrap_or("-"));
                println!("  title_romaji   {}", info.title_romaji);
                match &info.title_english {
                    Some(english) => println!("  title_english  {} ({})", english.text, english.source),
                    None => println!("  title_english  -"),
                }
                println!("  source         {}", info.source);
            }
            Err(None) => println!("  unsupported website"),
            Err(Some(why)) => println!("  {why}"),
        }
    }

    Ok(())
}

fn parse_rewalk(args: &[String]) -> Result<Command> {
    let (mut sites, mut config) = (Vec::new(), None);
    let mut iter = args.iter();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => config = Some(PathBuf::from(value(&mut iter, arg)?)),
            other if other.starts_with("--") => return Err(format!("unknown option: {other}\n\n{USAGE}").into()),
            site => sites.push(site.to_string()),
        }
    }
    if sites.is_empty() {
        return Err(format!("rewalk needs the names of the sites\n\n{USAGE}").into());
    }

    Ok(Command::Rewalk { sites, config })
}

/// Reads the whole list of the sites again: the state of their crawl is cleared (the database only, so it works while the server runs).
async fn rewalk(sites: Vec<String>, config: Option<PathBuf>) -> Result<()> {
    let config = load(config).await?;
    let store = SearchStore::open_database(&config.search.db_path).await?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let known: Vec<&str> = crate::sites::SITES.iter().map(|site| site.name).collect();

    for site in &sites {
        let Some(name) = known.iter().find(|name| name.eq_ignore_ascii_case(site)) else {
            println!("{site}: no such site");
            continue;
        };
        let (states, ranges) = store.rewalk(name, now).await?;

        println!("{name}: {states} states and {ranges} ranges of pages cleared");
    }
    println!("restart the server: the walk starts when it does");

    Ok(())
}

/// Asks the top page of each site behind Cloudflare, to see which ones this machine gets through to.
async fn cloudflare_check(config: Option<PathBuf>) -> Result<()> {
    let mut config = load(config).await?;
    config.search.enabled = true;
    let engine = open_engine(&config, Arc::new(Romanizer::new(&config.romaji.exceptions)?)).await?;

    for (name, result) in engine.check_cloudflare_sites().await {
        match result {
            Ok(size) => println!("{name}: ok ({size} bytes)"),
            Err(why) => println!("{name}: {why}"),
        }
    }

    Ok(())
}

/// The copy of the catalogue, or the reason there is none.
async fn mirror_file(config: Option<PathBuf>) -> Result<PathBuf> {
    let config = load(config).await?;

    if !config.magazine.mirror_path.exists() {
        return Err(format!("there is no copy of the catalogue at {} (run `sync` first)", config.magazine.mirror_path.display()).into());
    }

    Ok(config.magazine.mirror_path)
}

fn issue_url(id: &str) -> String {
    format!("https://www.dlsite.com/books/work/=/product_id/{id}.html")
}

async fn magazines(config: Option<PathBuf>) -> Result<()> {
    let path = mirror_file(config).await?;
    let found = crate::mirror::mirror_magazines(&path).await?.ok_or("the copy has no magazines yet")?;

    for magazine in &found {
        println!("{:<28} {:>4} issues", magazine.name, magazine.issues);

        if let Some(latest) = &magazine.latest {
            println!("    latest: {} ({})", latest.name, issue_url(&latest.id));
        }
    }

    Ok(())
}

async fn magazine(title: String, limit: u64, config: Option<PathBuf>) -> Result<()> {
    let path = mirror_file(config).await?;
    let found = crate::mirror::mirror_find_works(&path, &title, limit).await?.ok_or("the copy has no works yet")?;

    if found.is_empty() {
        return Err("no such work".into());
    }

    for work in &found {
        println!("{}  ({})", work.name, work.author);
        println!("    {} #{}  {}", work.issue.name, work.position, issue_url(&work.issue.id));
    }

    Ok(())
}

async fn works(issue: String, config: Option<PathBuf>) -> Result<()> {
    let path = mirror_file(config).await?;
    let (issue, works) = crate::mirror::mirror_issue_works(&path, &issue)
        .await?
        .ok_or("the copy has no works yet")?
        .ok_or("no such issue")?;

    println!("{} ({})", issue.name, issue_url(&issue.id));

    for work in &works {
        println!("{:>3}. {}  ({})", work.position, work.name, work.author);
    }

    Ok(())
}

async fn sync(config: Option<PathBuf>) -> Result<()> {
    let config = load(config).await?;
    let synced = Arc::new(d1_client(&config)?).sync(&config.magazine.mirror_path).await?;

    println!(
        "{} (bookmark {}) -> {}",
        match (synced.changed, synced.exported) {
            (true, _) => "the data changed, the copy was rebuilt",
            (false, true) => "the bookmark moved, but the tables are the same",
            (false, false) => "the data did not change",
        },
        synced.bookmark,
        config.magazine.mirror_path.display()
    );

    Ok(())
}

/// Makes the romaji of the saved works again and forgets the searches of the ones that changed. Nothing is asked to
/// a site: the prefetch of the server does that, when it starts.
async fn reromanize(config: Option<PathBuf>) -> Result<()> {
    let mut config = load(config).await?;
    config.search.enabled = true;

    let engine = open_engine(&config, Arc::new(Romanizer::new(&config.romaji.exceptions)?)).await?;
    let done = engine.reromanize_works().await?;

    println!(
        "{} works looked at, {} written again, {} searches to do again (the prefetch does them when the server starts)",
        done.works, done.changed, done.forgotten
    );

    Ok(())
}

/// Makes the translations of Hitomi.La from the saved works.
async fn translations(config: Option<PathBuf>) -> Result<()> {
    let config = load(config).await?;
    let store = SearchStore::open(&config.search.db_path).await?;

    println!("{} translations made", store.build_hitomi_translations().await?);

    Ok(())
}

/// Makes the translations of a source again: `hitomi` builds the pairs of Hitomi.La again and drops what they replace (the Google
/// rows of the same titles, and the lookups that said "nothing found"), `google` drops the Google translations so that the fill asks again.
async fn retranslate(source: &str, config: Option<PathBuf>) -> Result<()> {
    let config = load(config).await?;
    let store = SearchStore::open(&config.search.db_path).await?;

    if source == "hitomi" {
        let new = store.build_hitomi_translations().await?;
        let (rows, lookups) = store.drop_google_for_hitomi().await?;

        let romaji = store.drop_romaji_hitomi().await?;

        println!("Hitomi.La: {new} new pairs, {rows} Google translations dropped, {lookups} lookups dropped, {romaji} pairs dropped whose English side is only the romaji");
    } else {
        let (rows, lookups) = store.drop_source("google").await?;

        println!("Google: {rows} translations dropped, {lookups} lookups dropped (the fill asks for them again)");
    }

    Ok(())
}

/// Counts the works that have all the words of the query, and the works of each site.
async fn count(query: String, config: Option<PathBuf>) -> Result<()> {
    let mut config = load(config).await?;
    config.search.enabled = true;
    let engine = open_engine(&config, Arc::new(Romanizer::new(&config.romaji.exceptions)?)).await?;
    let (total, sites) = engine.count(&query).await?;
    let legal: usize = sites.iter().filter(|(_, legal, _)| *legal).map(|(_, _, n)| n).sum();

    for (name, is_legal, n) in &sites {
        println!("{name:<14} {n:>9}{}", if *is_legal { "  (legal)" } else { "" });
    }
    println!("\n{total} works have all of {query:?} ({legal} on legal sites, {} on the others)", total.saturating_sub(legal));

    Ok(())
}

/// Prints what the database holds.
async fn stats(config: Option<PathBuf>) -> Result<()> {
    let config = load(config).await?;
    let store = SearchStore::open(&config.search.db_path).await?;
    let crawls = store.crawl_states().await?;

    println!("{:<14} {:>10} {:>9} {:>10}  crawl", "site", "works", "gone", "unchecked");
    for (site, works, gone, unchecked) in store.site_stats().await? {
        let crawl = crawls
            .iter()
            .find(|state| state.website == site)
            .map(|state| format!("{} ({} saved)", if state.done { "done" } else { "walking" }, state.saved))
            .unwrap_or_default();

        println!("{site:<14} {works:>10} {gone:>9} {unchecked:>10}  {crawl}");
    }

    println!();
    for (source, rows) in store.translation_stats().await? {
        println!("translations {source:<8} {rows:>9}");
    }

    Ok(())
}

/// Opens the database: the text index beside it is filled when it is not complete (the same as the start of the server, without the server).
async fn build_index(config: Option<PathBuf>) -> Result<()> {
    let config = load(config).await?;
    let started = std::time::Instant::now();
    let store = SearchStore::open(&config.search.db_path).await?;
    let works = store.count_works().await?;

    println!("{works} works, the text index is complete ({} s)", started.elapsed().as_secs());

    Ok(())
}

/// Drops the FTS5 tables of the older versions from the database (their triggers are gone): the space is free inside the file.
async fn drop_legacy_index(config: Option<PathBuf>) -> Result<()> {
    let config = load(config).await?;
    let store = SearchStore::open(&config.search.db_path).await?;
    store.drop_legacy_index().await?;

    println!("the FTS5 tables are dropped");

    Ok(())
}

/// Reads the language of every saved title again.
async fn relanguage(config: Option<PathBuf>) -> Result<()> {
    let config = load(config).await?;
    let store = SearchStore::open(&config.search.db_path).await?;
    let (looked, changed) = store.relanguage().await?;

    println!("{looked} works looked at, {changed} written again");

    Ok(())
}

async fn export(file: PathBuf, sites: Vec<String>, since: i64, config: Option<PathBuf>) -> Result<()> {
    let config = load(config).await?;
    let store = SearchStore::open(&config.search.db_path).await?;
    let moved = crate::transfer::export(&store, &file, &sites, since).await?;
    let size = tokio::fs::metadata(&file).await.map(|m| m.len()).unwrap_or(0);

    println!("{} works of {} sites written to {} ({size} bytes)", moved.works, moved.sites.len(), file.display());

    Ok(())
}

async fn dataset(dir: PathBuf, config: Option<PathBuf>) -> Result<()> {
    let config = load(config).await?;
    let store = SearchStore::open(&config.search.db_path).await?;
    let written = crate::dataset::write(&store, &crate::sites::all(), &dir).await?;

    println!("{} works of {} sites written to {}", written.works, written.sites, dir.display());

    Ok(())
}

async fn import(files: Vec<PathBuf>, mark_done: Vec<String>, config: Option<PathBuf>) -> Result<()> {
    let config = load(config).await?;
    let store = SearchStore::open(&config.search.db_path).await?;
    // the romaji of the titles is made here, with the rules of this program
    let romanizer = load_romanizer(&config)?;

    for file in &files {
        let moved = crate::transfer::import(&store, file, &romanizer).await?;

        println!("{}: {} works read, {} of them new", file.display(), moved.works, moved.new);
    }
    for site in &mark_done {
        store.mark_crawled(site, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)).await?;
        println!("{site}: marked as read to its end");

        // the whole of Hitomi.La is in the database now: its translations are made from it
        if site == "Hitomi.La" {
            println!("Hitomi.La: {} translations made", store.build_hitomi_translations().await?);
        }
    }

    Ok(())
}

async fn prefetch(limit: Option<usize>, config: Option<PathBuf>) -> Result<()> {
    let mut config = load(config).await?;
    config.search.enabled = true;
    config.magazine.prefetch.enabled = true;
    if let Some(limit) = limit {
        config.magazine.prefetch.max_queries_per_round = limit;
    }

    let engine = open_engine(&config, Arc::new(Romanizer::new(&config.romaji.exceptions)?)).await?;
    let prefetcher = Prefetcher::new(engine, config.magazine.mirror_path.clone(), config.magazine.prefetch.clone()).with_crawl(config.crawl.enabled);
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
    fn count_takes_the_words_and_stats_only_the_config() {
        assert_eq!(parse(&args(&["count", "maid", "ai", "--config", "a.toml"])).unwrap(), Command::Count { query: "maid ai".into(), config: Some(PathBuf::from("a.toml")) });
        assert!(parse(&args(&["count"])).is_err());
        assert_eq!(parse(&args(&["stats"])).unwrap(), Command::Stats { config: None });
        assert!(parse(&args(&["stats", "--limit", "1"])).is_err());
        assert_eq!(
            parse(&args(&["rewalk", "Wnacg", "Sukebei", "--config", "a.toml"])).unwrap(),
            Command::Rewalk { sites: args(&["Wnacg", "Sukebei"]), config: Some(PathBuf::from("a.toml")) }
        );
        assert!(parse(&args(&["rewalk"])).is_err());
    }

    #[test]
    fn reromanize_takes_only_the_config() {
        assert_eq!(parse(&args(&["reromanize"])).unwrap(), Command::Reromanize { config: None });
        assert_eq!(
            parse(&args(&["reromanize", "--config", "a.toml"])).unwrap(),
            Command::Reromanize { config: Some(PathBuf::from("a.toml")) }
        );
        assert!(parse(&args(&["reromanize", "--limit", "1"])).is_err());
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
            parse(&args(&["title", "http://a", "--live", "off", "http://b"])).unwrap(),
            Command::Title { urls: args(&["http://a", "http://b"]), live: Live::Off, config: None }
        );
        assert!(parse(&args(&["title"])).is_err());
        assert_eq!(parse(&args(&["magazines", "--config", "a.toml"])).unwrap(), Command::Magazines { config: Some(PathBuf::from("a.toml")) });
        assert!(parse(&args(&["magazines", "x"])).is_err());
        assert_eq!(
            parse(&args(&["magazine", "My", "Work", "--limit", "3"])).unwrap(),
            Command::Magazine { title: "My Work".into(), limit: 3, config: None }
        );
        assert!(parse(&args(&["magazine", "x", "--limit", "0"])).is_err());
        assert!(parse(&args(&["magazine"])).is_err());
        assert_eq!(
            parse(&args(&["works", "COMIC", "2026"])).unwrap(),
            Command::Works { issue: "COMIC 2026".into(), config: None }
        );
        assert!(parse(&args(&["works"])).is_err());
        assert_eq!(parse(&args(&["dataset", "out"])).unwrap(), Command::Dataset { dir: PathBuf::from("out"), config: None });
        assert!(parse(&args(&["dataset"])).is_err());
        assert!(parse(&args(&["dataset", "a", "b"])).is_err());
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
        config.magazine.prefetch.enabled = true;
        assert!(Background::check(&config).err().expect("an error").to_string().contains("[search]"));

        // a D1 copy needs the ids and a token
        let mut config = Config::default();
        config.magazine.enabled = true;
        assert!(Background::check(&config).err().expect("an error").to_string().contains("account_id"));
        config.magazine.account_id = "a".into();
        config.magazine.database_id = "d".into();
        config.magazine.api_token = "${OKAZURI_SEARCH_API_TEST_TOKEN_THAT_IS_NOT_SET}".into();
        assert!(Background::check(&config).err().expect("an error").to_string().contains("is not set"));
        config.magazine.api_token = "token".into();
        assert!(Background::check(&config).is_ok());
    }
}
