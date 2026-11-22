//! The search engine.
//!
//! The query is turned into romaji first, always, and what titles are matched with is the romaji. Two steps.
//! **Fetch**: the query is asked to the search of every site, the works on the list page it answers with
//! are read (the title as the page writes it, and the path of the work's page) and saved in the search
//! database with the romaji of the title. A site that finds works by their Japanese title (`SiteDef::japanese`)
//! is asked the query as it was written **and** its romaji when the query has Japanese in it; the other sites,
//! and every query without Japanese, are asked the romaji only. **Find**: the answer to a request is every saved
//! work, whatever query found it, whose romaji title has all the words of the romaji query (AND, any order).
//!
//! A query is fetched again only when its last fetch is old (`[search] ttl_hours`, a month): that is so for a request
//! (`live`) as well as for the prefetch. The sites are asked
//! at the same time (but a site is never asked twice within `min_delay_ms`), and a site that fails does
//! not stop the others: it is reported, and not asked again before `retry_failed_minutes`.

use crate::config::SearchConfig;
use crate::query::{has_japanese, normalize, restore_times, romaji, tidy, Terms};
use crate::romanizer::Romanizer;
use crate::sites::{self, is_challenge, Format, Link, Site, Support};
use crate::store::{Candidate, Candidates, NewWork, SearchStore, SiteState};
use crate::utils::{unix_now, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Semaphore};

/// A site that answers 429 is asked once more after the seconds of its Retry-After (this when it gives none), but
/// never after more than the longest wait.
const DEFAULT_RETRY_WAIT_SECS: u64 = 5;
const MAX_RETRY_WAIT_SECS: u64 = 30;

/// A site that has to be asked this slowly (`SiteDef::delay_ms`) or more is slow: it is asked one form of a query, and the
/// prefetch leaves it out (a round would wait for it at every query). A request asks it like the others.
const SLOW_SITE_MS: u64 = 10_000;

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// A work found: the site, the title as the site writes it, and its page.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Hit {
    pub website: String,
    pub is_legal: bool,
    /// The path of the page, without the scheme and the domain.
    pub path: String,
    /// The address of the page (the domain of the site and the path).
    pub url: String,
    /// The title as the site writes it.
    pub title: String,
    /// The romaji of the title: what the query was matched with.
    pub title_romaji: String,
    /// The English title: the title itself when it is English, the English side of a title that has both (`English | 日本語`), or a
    /// saved translation (Hitomi.La's, else Google's); None when there is none. It is never asked for: only what is saved.
    /// `source` is `official` (the title, or Hitomi.La's, which is the work's own title) or `google` (a machine translation).
    pub title_english: Option<Translated>,
    /// The languages the title names (`language.rs`), as two-letter codes in the order it names them (`["zh", "en"]`); `[]` when it
    /// names none.
    pub language: Vec<String>,
    /// This request found the work on a site and saved it: it was not in the database before.
    pub is_new: bool,
}

/// A work that matched, as a place in a list of candidates: nothing of it is copied until it is in the answer.
#[derive(Debug, Clone, Copy)]
struct Item {
    /// Which list of candidates (0: the query, 1: the other title of it).
    set: u8,
    /// The place in the list.
    index: u32,
    /// The index of its site (`Engine::sites`).
    site: u16,
    is_new: bool,
}

/// What the search before this one asked: the indexes (and which words), or the rows one by one.
struct Searched<'a> {
    indexed: bool,
    matcher: Option<&'a crate::query::Matcher>,
    /// How many works the answer has so far, and how many of them are in `Merged::items`.
    total: usize,
    items: usize,
}

/// How many works an answer has, and how many of them are of the legal sites.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Counts {
    total: usize,
    legal: usize,
}

/// The matching works of one search: the candidates they stand in, in the order of the answer.
struct Lite {
    set: Arc<Candidates>,
    items: Vec<Item>,
    /// How many works each site has in the answer (by `Engine::sites`), when the indexes of the database were asked: `set` is then only the
    /// head of the answer. None: `items` is all of it.
    counts: Option<Counts>,
    /// What the indexes were asked (to leave it out of the other title's search).
    matcher: Option<crate::query::Matcher>,
    /// The answer but its works (`results` is empty).
    meta: Outcome,
}

/// The matching works of a request: the lists of candidates (the query, and its other title) and the places in them.
struct Merged {
    sets: Vec<Arc<Candidates>>,
    items: Vec<Item>,
    /// As `Lite::counts`, of all the searches together.
    counts: Option<Counts>,
    matcher: Option<crate::query::Matcher>,
    meta: Outcome,
}

/// The title of a work from the address of its page (`Engine::title_of`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TitleInfo {
    pub website: &'static str,
    /// The title the way the bot reads it (a saved work: as the site lists it).
    pub title: String,
    /// `database` (a saved work) or `page` (the page was asked for).
    pub source: &'static str,
    /// The Japanese title: the title itself, or the Japanese side of a title that has both (`English | 日本語`); None when it has none.
    pub title_japanese: Option<String>,
    pub title_romaji: String,
    /// As in a search result: `{text, source}`.
    pub title_english: Option<Translated>,
}

/// A query as the sites are asked it: with `×` written as `x`, and for the sites that need it (`keep_times`) with the `×`
/// put back (as it was typed, or where an `x` stands between capitals: `ExE`).
struct Plan {
    /// The tidied text, `×` as `x`.
    input: String,
    /// Its romaji.
    query: String,
    /// The text and its romaji with the `×` in them; only for a text that has one to put.
    times: Option<(String, String)>,
}

impl Plan {
    /// The text and the romaji a site is asked.
    fn for_site(&self, site: &Site) -> (&str, &str) {
        match &self.times {
            Some((input, romaji)) if site.def.keep_times => (input, romaji),
            _ => (&self.input, &self.query),
        }
    }
}

/// `The Phaethon Assault` -> `Phaethon Assault`: the English of a title does not need the `The` that a translation puts in front.
fn without_the(english: &str) -> String {
    let trimmed = english.trim();

    match trimmed.split_once(char::is_whitespace) {
        Some((first, rest)) if first.eq_ignore_ascii_case("the") && !rest.trim().is_empty() => rest.trim().to_string(),
        _ => trimmed.to_string(),
    }
}

/// The two languages that are translated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Japanese,
    English,
}

impl Lang {
    pub fn code(self) -> &'static str {
        match self {
            Lang::Japanese => "ja",
            Lang::English => "en",
        }
    }

    pub fn other(self) -> Lang {
        match self {
            Lang::Japanese => Lang::English,
            Lang::English => Lang::Japanese,
        }
    }

    /// The language of a text: Japanese when it has kana or kanji, English when its letters are all Latin (and it has some).
    /// Any other text (Korean, Cyrillic, a text of marks and digits) is not translated.
    pub fn of(text: &str) -> Option<Lang> {
        if crate::query::has_japanese(text) {
            return Some(Lang::Japanese);
        }

        let mut letters = text.chars().filter(|c| c.is_alphabetic()).peekable();

        (letters.peek().is_some() && letters.all(|c| c.is_ascii_alphabetic() || ('\u{00C0}'..='\u{024F}').contains(&c))).then_some(Lang::English)
    }
}

/// A translation of a text: the title of a work of Hitomi.La in English and in Japanese (`translate`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Translation {
    pub id: u32,
    pub url: String,
    pub english: String,
    pub japanese: String,
    /// The text is the whole title.
    pub exact: bool,
    /// `hitomi` (the titles of a work of Hitomi.La) or `google` (Google Translate, when Hitomi.La has nothing: `id` is 0 and
    /// `url` is empty).
    pub source: String,
}

impl Translation {
    /// The text in the language that is asked for.
    pub fn text(&self, lang: Lang) -> &str {
        match lang {
            Lang::Japanese => &self.japanese,
            Lang::English => &self.english,
        }
    }

    /// The row of the table `translations` (the key is put when it is saved).
    fn row(&self, position: i64) -> crate::entity::translation::Model {
        crate::entity::translation::Model {
            key: String::new(),
            gallery_id: i64::from(self.id),
            position,
            url: self.url.clone(),
            english: self.english.clone(),
            japanese: self.japanese.clone(),
            english_key: crate::hitomi::squash(&self.english),
            japanese_key: crate::hitomi::squash(&self.japanese),
            exact: self.exact,
            source: self.source.clone(),
        }
    }
}

impl From<crate::entity::translation::Model> for Translation {
    fn from(row: crate::entity::translation::Model) -> Self {
        Translation { id: row.gallery_id as u32, url: row.url, english: row.english, japanese: row.japanese, exact: row.exact, source: row.source }
    }
}

/// What `reromanize_works` did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Reromanized {
    /// Works looked at.
    pub works: usize,
    /// Works whose romaji came out different.
    pub changed: usize,
    /// Searches forgotten, to be done again.
    pub forgotten: usize,
}

/// What the sites were asked for a query.
#[derive(Debug, Default)]
pub struct Fetched {
    /// The indexes of the sites that were asked.
    pub asked: HashSet<usize>,
    /// The (site, path) of the works that were new.
    pub new: HashSet<(String, String)>,
}

/// What came of the search on one site.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SiteReport {
    pub website: String,
    /// `searched` (asked now), `cached` (asked less than `ttl_hours` ago, so the database answers), `failed` (a query
    /// failed), `not searched` (the request said not to ask the sites) or `unavailable` (Hitomi.La, while its whole list is not saved).
    pub status: &'static str,
    /// The queries the site is asked: the query as it was written and its romaji for a site that finds Japanese
    /// titles (when the query has Japanese in it), the romaji alone otherwise.
    pub queries: Vec<String>,
    /// How many works the last searches of the site listed (the sum over its queries).
    pub count: usize,
    pub error: Option<String>,
    pub searched_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Outcome {
    /// The query as it was sent (tidied up).
    pub input: String,
    /// The query as it was used: the romaji of the input. This is asked to the sites, and its words are matched
    /// with the romaji of the titles.
    pub query: String,
    /// The words every title has to have.
    pub terms: Vec<String>,
    /// How many works have all the words (`results` may be cut short of it).
    pub total: usize,
    /// How many of `total` are on a legal site, and how many on an illegal one (`is_legal`).
    pub total_legal: usize,
    pub total_illegal: usize,
    pub results: Vec<Hit>,
    pub sites: Vec<SiteReport>,
    /// The English title of the query that was searched too (the works of both are in `results`). None when the query has no
    /// Japanese in it, the translator has no title, or the request has no translator.
    pub translation: Option<Translated>,
    /// The Japanese title of a query without Japanese (English, or romaji) that was searched too (the works of both are in `results`):
    /// the same as `translation`, the other way. None when the query has Japanese in it or no letter, the translator has no title, or
    /// the request has no translator.
    pub translation_ja: Option<Translated>,
}

/// Where the English title of a query in Japanese comes from (`translator` of a search): the sites are asked for it too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Translator {
    /// Hitomi.La, and Google Translate when Hitomi.La fails or has nothing.
    Auto,
    /// Hitomi.La only: when it has no English title, the English search is left out.
    Hitomi,
    /// Google Translate only.
    Google,
    /// No English search.
    None,
}

impl Translator {
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "" | "auto" => Some(Translator::Auto),
            "hitomi" => Some(Translator::Hitomi),
            "google" => Some(Translator::Google),
            "none" => Some(Translator::None),
            _ => None,
        }
    }
}

/// An English (or Japanese) title with where it is from: the title that was searched too (`Outcome::translation`: `hitomi` or
/// `google`), or the English title of a result (`Hit::title_english`: `official` or `google`).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Translated {
    pub text: String,
    pub source: String,
}

/// What a request does about the sites (`live`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Live {
    /// Asks nobody: the database answers, even when what it has is out of date.
    Off,
    /// Asks only the combinations of query and site that are missing or older than `ttl_hours` (a month); for the others
    /// the database answers (`cached`).
    Diff,
    /// Asks every site, whether its answer is up to date or not.
    Force,
}

impl Live {
    /// `off`, `diff` (or nothing) and `force`. `None` for anything else.
    pub fn parse(text: &str) -> Option<Live> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" => Some(Live::Off),
            "diff" | "" => Some(Live::Diff),
            "force" => Some(Live::Force),
            _ => None,
        }
    }
}

/// A request to the engine.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// The words the titles have to have. This is also what the sites are asked.
    pub query: &'a str,
    /// Which sites are asked. The works that are new are saved, and the answer is from the database, which has them
    /// and what was saved before.
    pub live: Live,
    /// The most works in the answer.
    pub limit: usize,
    /// Where the English title of a query in Japanese comes from; the sites are searched for it too.
    pub translator: Translator,
    /// The sites (their indexes, `Engine::site_indexes`) that are left out: not asked, and their saved works are not in the
    /// answer.
    pub exclude: &'a [usize],
}

#[cfg(test)]
impl<'a> Request<'a> {
    pub fn new(query: &'a str) -> Self {
        Self { query, live: Live::Diff, limit: 100, translator: Translator::None, exclude: &[] }
    }
}

/// A site as `GET /okazuri/sites` lists it.
#[derive(Debug, Clone, Serialize)]
pub struct SiteInfo {
    pub name: &'static str,
    pub is_legal: bool,
    pub support: &'static str,
    pub has_search: bool,
    /// The search finds works by their Japanese title, so a query in Japanese is asked as it is as well as in romaji.
    pub japanese: bool,
    /// Searched by this engine with the current settings.
    pub active: bool,
    pub note: &'static str,
}

pub struct Engine {
    sites: Vec<Site>,
    /// Indexes into `sites` of the ones that are searched.
    active: Vec<usize>,
    /// How the indexes of the database are asked for the works of each site (`sites` order).
    keys: Vec<crate::store::SiteKey>,
    /// The place of a searched site in the list, to put the works in the order of the sites.
    order: HashMap<&'static str, usize>,
    /// When the site was asked last, to keep `min_delay_ms` between two requests.
    gates: Vec<Mutex<Option<Instant>>>,
    /// The shortest time between two requests to each site: the one every request of the program keeps (`wait_turn`).
    delays: Vec<Duration>,
    store: Arc<SearchStore>,
    client: reqwest::Client,
    config: SearchConfig,
    permits: Semaphore,
    romanizer: Arc<Romanizer>,
}

impl Engine {
    pub fn new(config: &SearchConfig, store: Arc<SearchStore>, romanizer: Arc<Romanizer>) -> Arc<Self> {
        Self::with_sites(sites::all(), config, store, romanizer)
    }

    pub fn with_sites(
        sites: Vec<Site>,
        config: &SearchConfig,
        store: Arc<SearchStore>,
        romanizer: Arc<Romanizer>,
    ) -> Arc<Self> {
        let disabled: HashSet<String> = config.disabled_sites.iter().map(|n| n.to_lowercase()).collect();
        let active: Vec<usize> = sites
            .iter()
            .enumerate()
            .filter(|(_, site)| {
                let searchable = site.def.search.is_some()
                    && (site.def.support == Support::Ready || config.try_unsupported);

                searchable && !disabled.contains(&site.def.name.to_lowercase())
            })
            .map(|(index, _)| index)
            .collect();
        let order = active.iter().enumerate().map(|(place, &index)| (sites[index].def.name, place)).collect();
        // a 403 or a 500 is an answer to be told apart, not an error of the client
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.timeout_secs.max(1)))
            .user_agent(USER_AGENT)
            .build()
            .expect("the HTTP client");

        let delays = sites.iter().map(|site| Self::delay_of(site, config)).collect();

        let keys = crate::store::SiteKey::of(&sites.iter().map(|site| site.def.name).collect::<Vec<_>>());

        Arc::new(Self {
            keys,
            delays,
            gates: sites.iter().map(|_| Mutex::new(None)).collect(),
            sites,
            active,
            order,
            store,
            client,
            config: config.clone(),
            permits: Semaphore::new(config.concurrency.max(1)),
            romanizer,
        })
    }

    /// Where the token of Google Translate is made (`gtranslate::Bridge`; empty: nowhere).
    fn bridge(&self) -> crate::gtranslate::Bridge<'_> {
        crate::gtranslate::Bridge { url: &self.config.google_bridge_url, token: &self.config.google_bridge_token }
    }

    pub fn max_query_chars(&self) -> usize {
        self.config.max_query_chars
    }

    pub fn max_results(&self) -> usize {
        self.config.max_results
    }

    /// The query as it is asked and matched: tidied up, and in romaji if there is Japanese in it. Text that is
    /// romaji already stays as it is, so this can be done twice.
    pub fn romanize_query(&self, text: &str) -> Result<String> {
        Ok(normalize(&romaji(&normalize(text), &self.romanizer)?))
    }

    /// The index of a site by its name, searched or not (the crawl reads sites that have no search too).
    pub(crate) fn site_named(&self, name: &str) -> Option<usize> {
        self.sites.iter().position(|site| site.def.name == name)
    }

    pub(crate) fn site(&self, index: usize) -> &Site {
        &self.sites[index]
    }

    pub(crate) fn store(&self) -> &Arc<SearchStore> {
        &self.store
    }

    /// The files of the index host of Hitomi.La, each one read after the turn of the site (`site` is its index).
    pub(crate) fn hitomi(&self, index: usize) -> HitomiFetch<'_> {
        HitomiFetch { engine: self, index }
    }

    /// The links of a page as works, with the romaji of their titles. The romaji is CPU work (the dictionary), so it is done
    /// on a thread of its own.
    pub(crate) async fn works_of(self: &Arc<Self>, links: Vec<Link>) -> std::result::Result<Vec<NewWork>, String> {
        let worker = self.clone();

        tokio::task::spawn_blocking(move || worker.with_romaji(links)).await.map_err(|e| e.to_string())
    }

    /// The works of a list page with the romaji of their titles, which is what a query is matched with. A title
    /// that cannot be converted is its own romaji.
    fn with_romaji(&self, links: Vec<Link>) -> Vec<NewWork> {
        links
            .into_iter()
            .map(|link| {
                // the language is read from the title with its flags; the title is kept without them
                let language = crate::language::detect(&link.title);
                let title = crate::language::strip_flags(&link.title);
                let converted = romaji(&title, &self.romanizer).unwrap_or_else(|_| title.clone());

                NewWork { path: link.path, title, romaji: converted, language }
            })
            .collect()
    }

    /// Makes the romaji of every saved work again, with the rules of this version of the program. A work whose romaji
    /// came out different is rewritten, and the searches that listed it are forgotten, so the prefetch (or the next
    /// request) asks the sites for them again. Does nothing to the works that came out the same, so it is cheap to do on
    /// every start.
    pub async fn reromanize_works(&self) -> Result<Reromanized> {
        const PAGE: u64 = 500;

        let mut done = Reromanized::default();
        let mut after: Option<(String, String)> = None;

        loop {
            let works = self.store.works_after(after.as_ref(), PAGE).await?;
            let Some(last) = works.last() else { break };

            after = Some((last.website.clone(), last.path.clone()));
            done.works += works.len();

            let changes: Vec<_> = works
                .into_iter()
                .filter_map(|work| {
                    let converted = romaji(&work.title, &self.romanizer).unwrap_or_else(|_| work.title.clone());

                    (converted != work.title_romaji).then_some((work, converted))
                })
                .collect();

            if !changes.is_empty() {
                done.changed += changes.len();
                done.forgotten += self.store.rewrite_romaji(&changes).await?;
            }

            // the server keeps answering between two pages
            tokio::task::yield_now().await;
        }

        Ok(done)
    }

    /// The indexes of the sites with these names (case does not matter), for `Request::exclude`. The name that is no site is
    /// the error.
    pub fn site_indexes(&self, names: &[String]) -> std::result::Result<Vec<usize>, String> {
        names
            .iter()
            .map(|name| {
                self.sites
                    .iter()
                    .position(|site| site.def.name.eq_ignore_ascii_case(name.trim()))
                    .ok_or_else(|| name.trim().to_string())
            })
            .collect()
    }

    pub fn sites_info(&self) -> Vec<SiteInfo> {
        self.sites
            .iter()
            .enumerate()
            .map(|(index, site)| SiteInfo {
                name: site.def.name,
                is_legal: site.def.legal,
                support: site.def.support.as_str(),
                has_search: site.def.search.is_some(),
                japanese: site.def.japanese,
                active: self.active.contains(&index),
                note: site.def.note,
            })
            .collect()
    }

    fn needs_search(&self, state: Option<&SiteState>, now: i64, refresh: bool) -> bool {
        let Some(state) = state else { return true };
        let age = now.saturating_sub(state.searched_at);
        let limit = if state.ok { self.config.ttl_hours * 3600 } else { self.config.retry_failed_minutes * 60 };

        refresh || age >= limit as i64
    }

    /// The queries a site is asked for a text: the text as written and its romaji when the text has Japanese in it and
    /// the site finds Japanese titles; the romaji alone otherwise.
    fn queries_for(&self, site: &Site, plan: &Plan) -> Vec<String> {
        let (input, romaji) = plan.for_site(site);
        let written = site.def.japanese && has_japanese(input) && input != romaji;

        if written && Self::is_slow(site) {
            vec![input.to_string()]
        } else if written {
            vec![input.to_string(), romaji.to_string()]
        } else {
            vec![romaji.to_string()]
        }
    }

    fn is_slow(site: &Site) -> bool {
        site.def.delay_ms >= SLOW_SITE_MS
    }

    /// The tidied text and its romaji, with `×` as `x` (and, for the sites that want it, as it is).
    fn plan(&self, text: &str) -> Result<Plan> {
        let input = normalize(text);
        let query = self.romanize_query(&input)?;
        let tidied = restore_times(&tidy(text));
        let kept = restore_times(&tidy(&romaji(&tidied, &self.romanizer)?));
        let times = (tidied != input || kept != query).then_some((tidied, kept));

        Ok(Plan { input, query, times })
    }

    /// The sites to ask, each with the queries that are missing or old (all of them with `refresh`). A slow site is left
    /// out unless `slow_too`.
    async fn due(&self, plan: &Plan, slow_too: bool, refresh: bool, exclude: &[usize]) -> Result<Vec<(usize, Vec<String>)>> {
        let now = unix_now();
        let mut states: HashMap<String, HashMap<String, SiteState>> = HashMap::new();
        let mut due = Vec::new();
        // the sites whose list is read to its end are in the database already
        let crawled = if self.config.skip_crawled && !refresh { self.store.crawl_done().await? } else { Vec::new() };

        for &index in &self.active {
            // a site that the request leaves out is neither asked nor looked up
            if exclude.contains(&index) || crawled.iter().any(|name| name == self.sites[index].def.name) {
                continue;
            }

            let site = &self.sites[index];
            let mut missing = Vec::new();

            // the prefetch does not wait for a slow site
            if !slow_too && Self::is_slow(site) {
                continue;
            }

            for text in self.queries_for(site, plan) {
                if !states.contains_key(&text) {
                    let known = self.store.states(&text).await?;
                    states.insert(text.clone(), known);
                }

                if self.needs_search(states[&text].get(site.def.name), now, refresh) {
                    missing.push(text);
                }
            }

            if !missing.is_empty() {
                due.push((index, missing));
            }
        }

        Ok(due)
    }

    /// Is the query fetched from every site (in every form a site is asked), and not too long ago? (The prefetch
    /// skips such a query.)
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn is_fresh(&self, query: &str) -> Result<bool> {
        self.is_fresh_without(query, &[]).await
    }

    /// Like `is_fresh`, without the sites in `exclude`.
    pub async fn is_fresh_without(&self, query: &str, exclude: &[usize]) -> Result<bool> {
        let plan = self.plan(query)?;

        Ok(self.due(&plan, false, false, exclude).await?.is_empty())
    }

    /// The searched sites whose whole list the crawl has read: their works are saved already, so the prefetch leaves them out and
    /// only asks the sites that have no list.
    pub async fn crawled_sites(&self) -> Result<Vec<usize>> {
        let done = self.store.crawl_done().await?;

        Ok(self.active.iter().copied().filter(|&index| done.iter().any(|name| name == self.sites[index].def.name)).collect())
    }

    /// Asks `query` to the sites whose answer is missing or older than `ttl_hours` (a failure: `retry_failed_minutes`),
    /// and saves the works that are new. A request asks the slow sites too (`slow_too`), the prefetch does not.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn fetch(self: &Arc<Self>, query: &str, slow_too: bool) -> Result<Fetched> {
        self.fetch_with(query, slow_too, false, &[]).await
    }

    /// Like `fetch`; with `refresh` every site is asked, whether its answer is up to date or not.
    pub async fn fetch_with(self: &Arc<Self>, query: &str, slow_too: bool, refresh: bool, exclude: &[usize]) -> Result<Fetched> {
        let plan = self.plan(query)?;

        if plan.query.is_empty() {
            return Err("the query is empty".into());
        }

        let mut fetched = Fetched::default();
        let mut tasks = tokio::task::JoinSet::new();

        for (index, texts) in self.due(&plan, slow_too, refresh, exclude).await? {
            fetched.asked.insert(index);

            let engine = self.clone();
            tasks.spawn(async move { engine.search_site(index, texts).await });
        }

        while let Some(joined) = tasks.join_next().await {
            match joined {
                Ok(new) => fetched.new.extend(new),
                Err(e) => log::error!("a search task died: {e}"),
            }
        }

        Ok(fetched)
    }

    /// The answer to a request: the sites are asked (unless the request says not to), and then the saved works
    /// whose title has every word of the query are the result.
    ///
    /// The works that match are places in the lists of candidates until the end: only the works of the answer (`limit`) are copied into a `Hit`
    /// (a word that 700,000 works have costs a few million copies otherwise).
    pub async fn search(self: &Arc<Self>, request: &Request<'_>) -> Result<Outcome> {
        let mut laps = crate::utils::Laps::new();
        let Merged { sets, items, mut meta, counts, .. } = self.search_all(request).await?;
        laps.lap("search_all");

        // `total` is how many works there are, whatever the limit cuts
        match counts {
            Some(counts) => {
                meta.total = counts.total;
                meta.total_legal = counts.legal;
            }
            None => {
                meta.total = items.len();
                meta.total_legal = items.iter().filter(|item| self.sites[item.site as usize].def.legal).count();
            }
        }
        meta.total_illegal = meta.total - meta.total_legal;
        meta.results = items.iter().take(request.limit.max(1)).map(|item| self.hit_of(&sets[item.set as usize], item)).collect();
        laps.lap("count");
        self.fill_english(&mut meta.results).await?;
        laps.lap("fill_english");

        log::info!("timing of the search {:?} (live {:?}, {} works): {laps}", request.query, request.live, meta.total);

        Ok(meta)
    }

    /// The work of a place of a list of candidates, copied.
    fn hit_of(&self, set: &Candidates, item: &Item) -> Hit {
        let work = &set.works[item.index as usize];
        let site = &self.sites[item.site as usize];

        Hit {
            website: set.websites[work.site as usize].clone(),
            is_legal: site.def.legal,
            url: site.address(&work.path),
            path: work.path.clone(),
            title: work.title.clone(),
            title_romaji: work.title_romaji.clone(),
            title_english: None,
            language: work.language.as_deref().map(|codes| codes.split(',').map(str::to_string).collect()).unwrap_or_default(),
            is_new: item.is_new,
        }
    }

    /// `search` with every work in `items`: the works of the English (or Japanese) title are in it, once.
    async fn search_all(self: &Arc<Self>, request: &Request<'_>) -> Result<Merged> {
        let mut laps = crate::utils::Laps::new();
        let first = self.search_one(request, None).await?;
        laps.lap("first search");
        let merged = Merged { sets: vec![first.set], items: first.items, counts: first.counts, matcher: first.matcher, meta: first.meta };
        let merged = self.search_all_more(request, merged, &mut laps).await;
        log::info!("timing of the two searches of {:?}: {laps}", request.query);

        merged
    }

    /// The search of the other title of a request (`search_all` does it for a query that is not in Japanese, and for one that is): the
    /// works that the answer does not have yet are put after it.
    async fn search_title(self: &Arc<Self>, request: &Request<'_>, text: &str, merged: &mut Merged) -> bool {
        // the indexes answer the other title too when they answered the first one: what the first found is left out, by the indexes
        let after = Searched {
            indexed: merged.counts.is_some(),
            matcher: merged.matcher.as_ref(),
            total: merged.counts.map_or(0, |counts| counts.total),
            items: merged.items.len(),
        };
        match self.search_one(&Request { query: text, translator: Translator::None, ..*request }, Some(after)).await {
            Ok(more) => {
                if let (Some(counts), Some(more_counts)) = (merged.counts.as_mut(), more.counts) {
                    counts.total += more_counts.total;
                    counts.legal += more_counts.legal;
                }

                // a work that is in the answer already (the same address) is not in it twice
                let known: HashSet<(&str, &str)> = merged.items.iter().map(|item| self.address_of(&merged.sets, item)).collect();
                let set = merged.sets.len() as u8;
                let added: Vec<Item> = more
                    .items
                    .into_iter()
                    .filter(|item| !known.contains(&self.address_of(std::slice::from_ref(&more.set), &Item { set: 0, ..*item })))
                    .map(|item| Item { set, ..item })
                    .collect();

                merged.items.extend(added);
                merged.sets.push(more.set);

                true
            }
            Err(e) => {
                log::warn!("cannot search the title {text:?} of {:?}: {e}", request.query);

                false
            }
        }
    }

    /// The origin of the site and the path of a work: its address in two pieces, with no copy.
    fn address_of<'a>(&self, sets: &'a [Arc<Candidates>], item: &Item) -> (&'a str, &'a str)
    where
        Self: 'a,
    {
        (self.sites[item.site as usize].def.origin, sets[item.set as usize].works[item.index as usize].path.as_str())
    }

    /// The rest of `search_all`: the other title is looked for, and searched.
    async fn search_all_more(self: &Arc<Self>, request: &Request<'_>, mut merged: Merged, laps: &mut crate::utils::Laps) -> Result<Merged> {
        if request.translator == Translator::None {
            return Ok(merged);
        }

        // a query without Japanese: its Japanese title is searched too (the same as the English title of a query in Japanese)
        if !crate::query::has_japanese(request.query) {
            if !request.query.chars().any(|c| c.is_ascii_alphabetic()) {
                return Ok(merged);
            }

            let found = self.japanese_of(request.query, request.translator, request.live).await;
            laps.lap("japanese title");
            let Some(found) = found else { return Ok(merged) };
            let text = found.japanese.trim().to_string();

            if text.is_empty() || crate::hitomi::squash(&text) == crate::hitomi::squash(request.query) {
                return Ok(merged);
            }

            let searched = self.search_title(request, &text, &mut merged).await;
            laps.lap("search of the Japanese title");
            if searched {
                merged.meta.translation_ja = Some(Translated { text, source: found.source });
            }

            return Ok(merged);
        }

        // the English title: its works are in the answer too. A failure leaves the English search out, not the search
        let english = self.english_of(request.query, request.translator, request.live).await;
        laps.lap("english title");
        let Some(english) = english else { return Ok(merged) };
        let text = english.english.trim().to_string();

        if text.is_empty() || crate::hitomi::squash(&text) == crate::hitomi::squash(request.query) {
            return Ok(merged);
        }

        let searched = self.search_title(request, &text, &mut merged).await;
        laps.lap("search of the English title");
        if searched {
            merged.meta.translation = Some(Translated { text, source: english.source });
        }

        Ok(merged)
    }

    /// The search of one query (`search` does it for the English title too). The works are places in the candidates (`items`); `meta.results`
    /// is empty.
    async fn search_one(self: &Arc<Self>, request: &Request<'_>, after: Option<Searched<'_>>) -> Result<Lite> {
        let mut laps = crate::utils::Laps::new();
        let plan = self.plan(request.query)?;
        let terms = Terms::new(&plan.query);
        laps.lap("plan");

        if terms.is_empty() {
            return Err("the query is empty".into());
        }

        // Hitomi.La is searched only when its whole list is saved: asked live (its index files, a few dozen requests) a search takes far too long
        let mut exclude: Vec<usize> = request.exclude.to_vec();
        let unlisted = self.hitomi_unlisted().await?;
        if let Some(index) = unlisted {
            exclude.push(index);
        }
        let exclude = exclude.as_slice();
        laps.lap("hitomi list");

        let fetched = match request.live {
            Live::Off => Fetched::default(),
            Live::Diff => self.fetch_with(request.query, true, false, exclude).await?,
            Live::Force => self.fetch_with(request.query, true, true, exclude).await?,
        };

        laps.lap("sites asked");

        // the indexes answer a search whose words they know (counts and the head of the answer in milliseconds); any other is read row by row.
        // The other title of a search that the indexes answered is asked of them too (what the first search found is left out)
        // (the trigram index has no key for a name of less than 3 characters: the sites of the tests have such names)
        let matcher = match &after {
            Some(after) if !after.indexed => None,
            _ if self.sites.iter().any(|site| site.def.name.chars().count() < 3) => None,
            _ => terms.matcher(),
        };
        if matcher.is_none() && after.as_ref().is_some_and(|after| after.indexed) {
            return Err("the indexes cannot answer the other title".into());
        }

        let (candidates, items, counts, rows) = match &matcher {
            Some(matcher) => self.indexed_items(request, matcher, after.as_ref(), &fetched, request.exclude).await?,
            None => {
                let (candidates, items) = self.scanned_items(&terms, &fetched, request.exclude).await?;
                laps.lap("candidates and match");
                let rows = candidates.len();

                (candidates, items, None, rows)
            }
        };
        laps.lap("found");

        let total = counts.map_or(items.len(), |counts| counts.total);

        let reports = self.reports(&plan, &fetched.asked, request.live != Live::Off, exclude).await?;
        laps.lap("site reports");
        log::info!("timing of the search of {:?} ({rows} candidates, {total} works): {laps}", request.query);

        let meta = Outcome {
            sites: {
                let mut sites = reports;
                if let Some(index) = unlisted.filter(|index| !request.exclude.contains(index)) {
                    sites.push(SiteReport {
                        website: self.sites[index].def.name.to_string(),
                        status: "unavailable",
                        queries: Vec::new(),
                        count: 0,
                        error: Some("the whole list of the site is not saved yet".to_string()),
                        searched_at: None,
                    });
                }
                sites
            },
            input: plan.input,
            terms: terms.keys().into_iter().map(str::to_string).collect(),
            total,
            total_legal: 0,
            total_illegal: 0,
            results: Vec::new(),
            query: plan.query,
            translation: None,
            translation_ja: None,
        };

        Ok(Lite { set: candidates, items, counts, matcher, meta })
    }

    /// The sites that are not left out by the request, in the order of the answer (the searched sites first).
    fn sites_in_order(&self, exclude: &[usize]) -> Vec<usize> {
        // every site that is not left out by the request: the works that are saved of a site that is not searched (it is off in the config, or
        // its list is not whole yet) are in the answer too, after the works of the searched sites
        let mut sites: Vec<usize> = (0..self.sites.len()).filter(|index| !exclude.contains(index)).collect();
        sites.sort_by_key(|index| self.order.get(self.sites[*index].def.name).copied().unwrap_or(usize::MAX));

        sites
    }

    /// The works of the answer by reading the saved rows that have the words one by one (a word that is a number, a short piece that is not
    /// letters): the matching works as places in their candidates, in the order of the answer, once each address.
    async fn scanned_items(&self, terms: &Terms, fetched: &Fetched, exclude: &[usize]) -> Result<(Arc<Candidates>, Vec<Item>)> {
        let candidates = self.store.candidates(&terms.prefilters()).await?;

        // the searched site of each name the candidates have: found once, not for each of the rows (a site that is left out does not answer,
        // whatever was saved of it)
        let site_of: Vec<Option<usize>> = candidates
            .websites
            .iter()
            .map(|name| (0..self.sites.len()).find(|index| !exclude.contains(index) && self.sites[*index].def.name == name))
            .collect();
        let nothing_is_new = fetched.new.is_empty();
        let mut items: Vec<Item> = Vec::new();

        for (index, work) in candidates.works.iter().enumerate() {
            let Some(site) = site_of[work.site as usize] else { continue };

            // the name has to be in the title outside its parentheses ("work (COMIC kairakuten 2018 8)" is a work of the issue)
            if terms.matches_key(&work.title_romaji, &work.title_key) {
                let is_new = !nothing_is_new && fetched.new.contains(&(candidates.websites[work.site as usize].clone(), work.path.clone()));

                items.push(Item { set: 0, index: index as u32, site: site as u16, is_new });
            }
        }

        // the sites in their order; in a site, the works seen last come first (the order of the database)
        let rank: Vec<usize> = self.sites.iter().map(|site| self.order.get(site.def.name).copied().unwrap_or(usize::MAX)).collect();
        items.sort_by_key(|item| rank[item.site as usize]);

        let origins: Vec<String> = self.sites.iter().map(|site| site.def.origin.to_lowercase()).collect();
        let items = dedupe_items(&candidates.works, &origins, items);

        Ok((candidates, items))
    }

    /// How many works `matcher` finds in the sites that are searched, and how many of them are of the legal sites. A count with a site in it is
    /// slow (the index walks the long list of the works of the site), so the whole table is counted and the works of the sites that are left out
    /// are taken off, and only the few legal sites are named.
    async fn count_answer(&self, matcher: &crate::query::Matcher, left_out: &[crate::store::SiteKey], legal: &[crate::store::SiteKey]) -> Result<Counts> {
        let mut total = self.store.count_found(matcher, None).await?;
        if !left_out.is_empty() {
            total = total.saturating_sub(self.store.count_found(matcher, Some(left_out)).await?);
        }
        let legal = if legal.is_empty() { 0 } else { self.store.count_found(matcher, Some(legal)).await? };

        Ok(Counts { total, legal })
    }

    /// The works of the answer from the indexes of the database: how many works there are (no work is read for that), and the head of the
    /// answer (the last added first in a site, the sites in their order), `request.limit` works. `after` is an earlier search of the same
    /// request: its works are not counted again, and its place in the answer is not filled again. Returns the head as candidates and places,
    /// the counts, and how many rows were read.
    async fn indexed_items(
        &self,
        request: &Request<'_>,
        matcher: &crate::query::Matcher,
        after: Option<&Searched<'_>>,
        fetched: &Fetched,
        exclude: &[usize],
    ) -> Result<(Arc<Candidates>, Vec<Item>, Option<Counts>, usize)> {
        let order = self.sites_in_order(exclude);
        let left_out: Vec<crate::store::SiteKey> = (0..self.sites.len()).filter(|index| !order.contains(index)).map(|index| self.keys[index].clone()).collect();
        let legal: Vec<crate::store::SiteKey> = order.iter().filter(|index| self.sites[**index].def.legal).map(|index| self.keys[*index].clone()).collect();

        let mut counts = self.count_answer(matcher, &left_out, &legal).await?;
        // the works that the earlier search has are not counted again: what both find is counted once
        let both = after.and_then(|after| after.matcher).map(|earlier| matcher.and(earlier));
        if let Some(both) = &both {
            let twice = self.count_answer(both, &left_out, &legal).await?;
            counts = Counts { total: counts.total.saturating_sub(twice.total), legal: counts.legal.saturating_sub(twice.legal) };
        }

        let mut websites: Vec<String> = Vec::new();
        let mut works: Vec<Candidate> = Vec::new();
        let mut items: Vec<Item> = Vec::new();
        // the places of the answer that the earlier search left; it has every one of its works in `Merged::items` when it did not fill the answer,
        // and those that this search also finds are filtered out of what it reads, so that many more are read
        let mut left = request.limit.max(1).saturating_sub(after.map_or(0, |after| after.items));
        let spare = after.map_or(0, |after| after.total);

        for index in order {
            if counts.total == 0 || left == 0 {
                break;
            }

            let name = self.sites[index].def.name;
            let rows = self.store.top_found(matcher, &self.keys[index], left + spare).await?;
            if rows.is_empty() {
                continue;
            }
            websites.push(name.to_string());
            // (some of the rows may be the works of the earlier search, which are filtered out: only the ones above its number are sure to stay)
            left = left.saturating_sub(rows.len().saturating_sub(spare));

            for mut row in rows {
                row.site = (websites.len() - 1) as u16;
                let is_new = !fetched.new.is_empty() && fetched.new.contains(&(name.to_string(), row.path.clone()));

                items.push(Item { set: 0, index: works.len() as u32, site: index as u16, is_new });
                works.push(row);
            }
        }

        let rows = works.len();

        Ok((Arc::new(Candidates { works, websites }), items, Some(counts), rows))
    }

    /// Whether Hitomi.La is one of the sites and its whole list is not saved yet (its crawl has not reached the end): its search and its
    /// translations are not used then.
    pub async fn hitomi_waits_for_its_list(&self) -> Result<bool> {
        let Some(site) = self.sites.iter().find(|site| site.def.format == Format::Hitomi) else { return Ok(false) };
        let done = self.store.crawl_done().await?;

        Ok(!done.iter().any(|name| name == site.def.name))
    }

    /// The index of Hitomi.La when it is one of the searched sites and its whole list is not saved (the crawl has not reached the end):
    /// it is not searched then.
    async fn hitomi_unlisted(&self) -> Result<Option<usize>> {
        let Some(&index) = self.active.iter().find(|&&index| self.sites[index].def.format == Format::Hitomi) else { return Ok(None) };
        let done = self.store.crawl_done().await?;

        Ok((!done.iter().any(|name| name == self.sites[index].def.name)).then_some(index))
    }

    /// Writes the English title of the works (`Hit::title_english`) from what is saved: no request to a site.
    async fn fill_english(&self, hits: &mut [Hit]) -> Result<()> {
        let key_of = |title: &str| crate::hitomi::squash(&crate::query::work_title(title, true));
        let mut wanted: Vec<String> = Vec::new();

        for hit in hits.iter_mut() {
            if let Some((english, _)) = crate::hitomi::title_pair(&hit.title) {
                // both titles are the work's own
                hit.title_english = Some(Translated { text: english, source: "official".to_string() });
            } else if !crate::query::has_japanese(&hit.title) {
                // a title in Latin letters, or in the language it names: only an English one is the English title
                if hit.language.is_empty() || hit.language.iter().any(|code| code == "en") {
                    hit.title_english = Some(Translated { text: hit.title.clone(), source: "official".to_string() });
                }
            } else {
                wanted.push(key_of(&hit.title));
            }
        }

        if wanted.is_empty() {
            return Ok(());
        }
        let found = self.store.english_titles(&wanted).await?;

        for hit in hits.iter_mut().filter(|hit| hit.title_english.is_none() && crate::query::has_japanese(&hit.title)) {
            if let Some((english, source)) = found.get(&key_of(&hit.title)) {
                // Hitomi.La's English title is the work's own; Google's is a translation
                hit.title_english = Some(Translated { text: english.clone(), source: if source == "google" { "google" } else { "official" }.to_string() });
            }
        }

        Ok(())
    }

    async fn reports(&self, plan: &Plan, asked: &HashSet<usize>, may_ask: bool, exclude: &[usize]) -> Result<Vec<SiteReport>> {
        let mut states: HashMap<String, HashMap<String, SiteState>> = HashMap::new();
        let mut reports = Vec::new();

        for &index in &self.active {
            if exclude.contains(&index) {
                continue;
            }

            let site = &self.sites[index];
            let name = site.def.name;
            let queries = self.queries_for(site, plan);
            let mut known: Vec<SiteState> = Vec::new();

            for text in &queries {
                if !states.contains_key(text) {
                    states.insert(text.clone(), self.store.states(text).await?);
                }
                known.extend(states[text].get(name).cloned());
            }

            let failed = known.iter().find(|state| !state.ok);

            reports.push(SiteReport {
                website: name.to_string(),
                status: match failed {
                    Some(_) => "failed",
                    None if asked.contains(&index) => "searched",
                    None if may_ask => "cached",
                    None => "not searched",
                },
                queries,
                count: known.iter().map(|s| s.count).sum(),
                error: failed.and_then(|s| s.error.clone()),
                searched_at: known.iter().map(|s| s.searched_at).max(),
            });
        }

        Ok(reports)
    }

    /// Asks one site its queries, one after the other, and saves what it answered. Returns the (site, path) of the
    /// works that were new.
    async fn search_site(self: Arc<Self>, index: usize, queries: Vec<String>) -> Vec<(String, String)> {
        let Ok(_permit) = self.permits.acquire().await else { return Vec::new() };
        let name = self.sites[index].def.name;
        let mut found = Vec::new();

        for query in queries {
            let started = Instant::now();

            // the request is async; the romaji (the analyzer) is CPU work, so it runs on a blocking thread
            let outcome = match self.request(index, &query).await {
                Ok(links) => {
                    let worker = self.clone();

                    tokio::task::spawn_blocking(move || worker.with_romaji(links))
                        .await
                        .map_err(|e| format!("the romaji task died: {e}"))
                }
                Err(e) => Err(e),
            };

            match &outcome {
                Ok(links) => log::debug!("search {name}: {} works in {:?} ({query})", links.len(), started.elapsed()),
                Err(error) => log::warn!("search {name}: {error} ({query})"),
            }

            // a list as long as the most that is kept may have been cut: then a work that is missing is not gone
            let complete = outcome.as_ref().is_ok_and(|links| links.len() < self.config.max_results_per_site);

            match self.store.save(&query, name, unix_now(), &outcome, complete).await {
                Ok(new) => found.extend(new.into_iter().map(|path| (name.to_string(), path))),
                Err(e) => log::error!("cannot save the search of {name}: {e}"),
            }
        }

        found
    }

    /// The request to a site. Waits for its turn first.
    async fn request(&self, index: usize, query: &str) -> std::result::Result<Vec<Link>, String> {
        let site = &self.sites[index];

        if site.def.format == Format::Hitomi {
            return self.request_hitomi(index, query).await;
        }

        let url = site.search_url(query).ok_or_else(|| "the site has no search".to_string())?;
        let (page_url, body) = self.get_page(index, &url).await?;

        site.read(&page_url, &body, self.config.max_results_per_site)
    }

    /// A page of a site (the address it ended at, and its text), after the turn of the site. A 429 is waited for once. A
    /// Cloudflare challenge and any status but 200 are errors.
    pub(crate) async fn get_page(&self, index: usize, url: &str) -> std::result::Result<(String, String), String> {
        let site = &self.sites[index];
        let mut asked_again = false;
        let (status, page_url, body) = loop {
            self.wait_turn(index).await;

            let mut request = self.client.get(url).header("Accept-Language", "ja,en;q=0.8").header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            );
            if let Some(cookie) = site.def.cookie {
                request = request.header("Cookie", cookie);
            }

            let response = request.send().await.map_err(|e| e.to_string())?;
            let status = response.status().as_u16();

            // too many requests: wait as long as the site says (not too long), and ask once more
            if status == 429 && !asked_again {
                let wait = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .unwrap_or(DEFAULT_RETRY_WAIT_SECS)
                    .min(MAX_RETRY_WAIT_SECS);

                log::debug!("search {}: HTTP 429, asking again in {wait} s", site.def.name);
                asked_again = true;
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }

            let page_url = response.url().to_string();
            let bytes = response.bytes().await.map_err(|e| e.to_string())?;

            break (status, page_url, String::from_utf8_lossy(&bytes).into_owned());
        };

        if is_challenge(status, &body) {
            return Err("blocked by a Cloudflare challenge".to_string());
        }
        // E-Hentai answers 200 with a short page that says the address is banned for a while: not a page without works
        if body.len() < 2000 && body.contains("temporarily banned") {
            return Err(format!("the address is banned by the site for a while ({})", body.trim().chars().take(120).collect::<String>()));
        }
        if status != 200 {
            return Err(format!("HTTP {status}"));
        }

        Ok((page_url, body))
    }

    /// Asks for the page of a work, after the turn of the site, and says whether the page is gone (`rule`): `Some(true)` it is gone, `Some(false)`
    /// it is there, `None` nothing can be said (the request failed, or the answer is no sign).
    pub(crate) async fn probe(&self, index: usize, url: &str, rule: GoneRule) -> Option<bool> {
        self.wait_turn(index).await;

        let response = self.client.get(url).header("Accept-Language", "ja,en;q=0.8").send().await.ok()?;
        let status = response.status().as_u16();
        let final_path = response.url().path().to_string();
        let body = response.text().await.ok()?;

        page_status(rule, status, &final_path, &body)
    }

    /// The search of Hitomi.La (`hitomi.rs`): a few dozen small requests to its index files, each one after its turn.
    async fn request_hitomi(&self, index: usize, query: &str) -> std::result::Result<Vec<Link>, String> {
        crate::hitomi::search(&mut HitomiFetch { engine: self, index }, query, self.config.max_results_per_site).await
    }

    /// The title of the work whose page is at `url` (`read_title`), with its Japanese title, its romaji and its English title
    /// (what is saved: nothing is asked for them, as in a search).
    pub async fn title_of(&self, url: &str, live: Live) -> std::result::Result<TitleInfo, Option<String>> {
        let (website, title, source) = self.read_title(url, live).await?;
        let title_romaji = self.romanize_query(&title).map_err(|e| Some(e.to_string()))?;
        let title_japanese = match crate::hitomi::title_pair(&title) {
            Some((_, japanese)) => Some(japanese),
            None => crate::query::has_japanese(&title).then(|| title.clone()),
        };
        let mut hits = [Hit {
            website: website.to_string(),
            is_legal: false,
            path: String::new(),
            url: url.to_string(),
            language: crate::language::detect_all(&title).into_iter().map(str::to_string).collect(),
            title: title.clone(),
            title_romaji: title_romaji.clone(),
            title_english: None,
            is_new: false,
        }];
        self.fill_english(&mut hits).await.map_err(|e| Some(e.to_string()))?;
        let [hit] = hits;

        Ok(TitleInfo { website, title, source, title_japanese, title_romaji, title_english: hit.title_english })
    }

    /// The title of the work whose page is at `url`. The saved works answer first: the address is cleaned (the fragment, the
    /// locale and the parameters that do not name the work are dropped, `/view/1/2` is `/gallery/1`) and looked up by its path,
    /// with and without the slash at the end, so an address with more in it than the page needs still finds the work. A work
    /// that is not saved is read from its page, the way the bot reads it (Hitomi.La: from the file of the gallery). `live`:
    /// `off` answers from the database alone, `diff` (the default) falls back to the page, `force` always asks the page.
    /// Returns the site, the title and where it is from (`database` or `page`). `Err(None)`: no site has such a page;
    /// `Err(Some(why))`: no title.
    async fn read_title(&self, url: &str, live: Live) -> std::result::Result<(&'static str, String, &'static str), Option<String>> {
        let Some((index, site)) = self.sites.iter().enumerate().find(|(_, site)| site.matches(url)) else { return Err(None) };
        let name = site.def.name;
        let hitomi = site.def.format == Format::Hitomi;
        let gallery_id: Option<u32> = if hitomi {
            static ID: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            let id = ID.get_or_init(|| regex::Regex::new(r"/(?:[^/]*-)?(\d+)\.html").unwrap());

            id.captures(url).and_then(|c| c[1].parse().ok())
        } else {
            None
        };
        let cleaned = site.clean(url);
        let path = match gallery_id {
            Some(id) => Some(format!("/galleries/{id}.html")),
            None => site.path_of(&cleaned),
        };

        if live != Live::Force {
            if let Some(path) = &path {
                let paths = [path.clone(), sites::toggle_slash(path)];

                if let Some(saved) = self.store.title_at(name, &paths).await.map_err(|e| Some(e.to_string()))? {
                    // Hitomi.La saves `English | 日本語`: the bot reads the Japanese title
                    let title = match hitomi.then(|| crate::hitomi::title_pair(&saved)).flatten() {
                        Some((_, japanese)) => japanese,
                        None => saved,
                    };

                    return Ok((name, title, "database"));
                }
            }
        }

        if live == Live::Off {
            return Err(Some("the work is not saved".to_string()));
        }

        if hitomi {
            use crate::hitomi::Fetch as _;
            let id = gallery_id.ok_or_else(|| Some("no gallery id in the address".to_string()))?;
            let body = HitomiFetch { engine: self, index }.get(&format!("/galleries/{id}.js"), None).await.map_err(Some)?;
            let gallery = crate::hitomi::parse_gallery(id, &String::from_utf8_lossy(&body)).ok_or_else(|| Some("the gallery has no title".to_string()))?;
            let mut title = gallery.japanese_title.clone().unwrap_or_else(|| gallery.title.clone());

            if let Some(artist) = gallery.artists.first() {
                title.push(' ');
                title.push_str(artist);
            }

            return Ok((name, title, "page"));
        }

        let (_, page) = self.get_page(index, &cleaned).await.map_err(Some)?;

        crate::titles::title_of_page(name, &page).map(|title| (name, title, "page")).ok_or_else(|| Some("the page has no title".to_string()))
    }

    /// Whether the translation can be asked: Hitomi.La is one of the sites.
    pub fn can_translate(&self) -> bool {
        self.sites.iter().any(|site| site.def.format == Format::Hitomi)
    }

    /// The translation of a text, Japanese to English or English to Japanese, from the titles of Hitomi.La (a work has both).
    /// The answer is saved in the tables `translation_lookups` and `translations` (source `hitomi`): `live` is the one of the
    /// search, a saved answer is kept for good and is the one that is given unless it is `on`, and a translation that was
    /// saved for the other direction answers too. A text that Hitomi.La has nothing for is asked again every time (`diff`):
    /// the site may have the work now. `off` never asks the site. Returns the translations (the ones that are the whole
    /// title first) and whether they are the saved ones.
    pub async fn translate_hitomi(self: &Arc<Self>, text: &str, from: Lang, live: Live) -> Result<(Vec<Translation>, bool)> {
        // only the title of the work: the circle, the artist, the event, the series and the tags around it are left out
        let title = crate::query::work_title(text, from == Lang::Japanese);
        let text = title.as_str();
        let key = crate::hitomi::squash(text);

        if key.is_empty() {
            return Ok((Vec::new(), false));
        }

        // Hitomi.La is used only when its whole list is saved (asked live, a translation takes far too long): nothing is answered
        // before, and `translate_auto` goes on to Google
        if self.hitomi_waits_for_its_list().await? {
            return Ok((Vec::new(), true));
        }

        let mut laps = crate::utils::Laps::new();
        let saved = self.store.translations(&key, "hitomi").await?;
        laps.lap("saved");

        if live != Live::Force && !saved.is_empty() {
            log::info!("timing of the Hitomi.La translation of {text:?}: {laps}");
            return Ok((saved.into_iter().map(Translation::from).collect(), true));
        }

        // the other direction, asked before (a Japanese to English translation answers English to Japanese): `diff` and `off`
        if live != Live::Force {
            let others = self.store.translations_with("hitomi", from == Lang::English, &key, false, 20).await?;
            laps.lap("other direction");

            if !others.is_empty() {
                log::info!("timing of the Hitomi.La translation of {text:?}: {laps}");
                let side = |row: &crate::entity::translation::Model| if from == Lang::English { row.english_key.clone() } else { row.japanese_key.clone() };
                let mut found: Vec<Translation> = others
                    .into_iter()
                    .map(|row| {
                        let exact = side(&row) == key;

                        Translation { exact, ..Translation::from(row) }
                    })
                    .collect();

                found.sort_by_key(|translation| !translation.exact);

                return Ok((found, true));
            }
        }

        if live == Live::Off {
            return Ok((Vec::new(), true));
        }

        let found = self.ask_translations(text, from).await?;
        laps.lap("asked the site");
        let rows: Vec<_> = found.iter().enumerate().map(|(place, found)| found.row(place as i64)).collect();

        self.store.save_translations(&key, "hitomi", text.trim(), from.code(), unix_now(), &rows).await?;
        laps.lap("save");
        log::info!("timing of the Hitomi.La translation of {text:?}: {laps}");

        Ok((found, false))
    }

    /// The translation of Google Translate (`gtranslate.rs`), saved in the same tables (source `google`, kept for good):
    /// the saved one is the answer unless `live` is `on`, and so is a saved translation of the other direction when its title
    /// is the whole text. Google is always asked when nothing is saved (there is no `off` for it: `off` is `diff`). In
    /// English a translation from Japanese does not start with `The`: it is taken off (Google puts it in front of a title).
    pub async fn translate_google(self: &Arc<Self>, text: &str, from: Lang, live: Live) -> Result<(Translation, bool)> {
        let key = crate::hitomi::squash(text);

        if key.is_empty() {
            return Err("nothing to translate".into());
        }

        let mut laps = crate::utils::Laps::new();

        if live != Live::Force {
            let row = self.store.translations(&key, "google").await?.into_iter().next();
            laps.lap("saved");
            if let Some(row) = row {
                log::info!("timing of the Google translation of {text:?}: {laps}");
                return Ok((Translation::from(row), true));
            }

            // the other direction, asked before: its other side is the text
            let row = self.store.translations_with("google", from == Lang::English, &key, true, 1).await?.into_iter().next();
            laps.lap("other direction");
            if let Some(row) = row {
                log::info!("timing of the Google translation of {text:?}: {laps}");
                return Ok((Translation { exact: true, ..Translation::from(row) }, true));
            }
        }

        let translated = crate::gtranslate::translate(&self.client, self.bridge(), text.trim(), from, from.other()).await?;
        laps.lap("asked Google");
        let translated = if from == Lang::Japanese { without_the(&translated) } else { translated };

        let (english, japanese) = if from == Lang::English { (text.trim().to_string(), translated) } else { (translated, text.trim().to_string()) };
        let translation = Translation { id: 0, url: String::new(), english, japanese, exact: false, source: "google".into() };

        self.store.save_translations(&key, "google", text.trim(), from.code(), unix_now(), &[translation.row(0)]).await?;
        laps.lap("save");
        log::info!("timing of the Google translation of {text:?}: {laps}");

        Ok((translation, false))
    }

    /// The translation from Hitomi.La, and from Google Translate when Hitomi.La fails or has nothing (`translator=auto`).
    /// The first translation and whether it is the saved one; an error when neither has one.
    pub async fn translate_auto(self: &Arc<Self>, text: &str, from: Lang, live: Live) -> Result<(Translation, bool)> {
        match self.translate_hitomi(text, from, live).await {
            Ok((found, saved)) => {
                if let Some(first) = found.into_iter().next() {
                    return Ok((first, saved));
                }
            }
            Err(e) => log::warn!("no translation of {text:?} from Hitomi.La: {e}"),
        }

        self.translate_google(text, from, live).await
    }

    /// The English title of a Japanese text by the translator of a search (`Translator`): None when the translator has none.
    /// `auto` asks Hitomi.La, and Google Translate when Hitomi.La fails or has nothing; `hitomi` and `google` only that one.
    async fn english_of(self: &Arc<Self>, text: &str, translator: Translator, live: Live) -> Option<Translation> {
        if translator != Translator::Google {
            match self.translate_hitomi(text, Lang::Japanese, live).await {
                Ok((found, _)) => {
                    if let Some(first) = found.into_iter().next() {
                        return Some(first);
                    }
                }
                Err(e) => log::warn!("no English title of {text:?} from Hitomi.La: {e}"),
            }
        }

        if translator != Translator::Hitomi {
            match self.translate_google(text, Lang::Japanese, live).await {
                Ok((found, _)) => return Some(found),
                Err(e) => log::warn!("no English title of {text:?} from Google Translate: {e}"),
            }
        }

        None
    }

    /// The Japanese title of an English (or romaji) text by the translator of a search: `english_of` the other way.
    async fn japanese_of(self: &Arc<Self>, text: &str, translator: Translator, live: Live) -> Option<Translation> {
        if translator != Translator::Google {
            match self.translate_hitomi(text, Lang::English, live).await {
                Ok((found, _)) => {
                    if let Some(first) = found.into_iter().next() {
                        return Some(first);
                    }
                }
                Err(e) => log::warn!("no Japanese title of {text:?} from Hitomi.La: {e}"),
            }
        }

        if translator != Translator::Hitomi {
            match self.translate_google(text, Lang::English, live).await {
                Ok((found, _)) => return Some(found),
                Err(e) => log::warn!("no Japanese title of {text:?} from Google Translate: {e}"),
            }
        }

        None
    }

    /// The galleries of Hitomi.La for a query.
    async fn hitomi_galleries(self: &Arc<Self>, index: usize, text: &str) -> Result<Vec<crate::hitomi::Gallery>> {
        let words = normalize(text);
        let limit = self.config.max_results_per_site.clamp(1, 20);

        Ok(crate::hitomi::galleries(&mut HitomiFetch { engine: self, index }, &words, limit).await?)
    }

    /// Asks the site. The galleries are the same work in different languages when their artists, groups, parodies and kind are
    /// the same and the pages are about as many (`Gallery::same_work`); the romaji in the titles is not needed. The Japanese
    /// gallery has the Japanese title, and the galleries in English have the English one (the site keeps the Japanese words in
    /// the index of the galleries of the other languages too, so the text in Japanese finds them all):
    /// - Japanese to English: the gallery whose Japanese title has the text, and the English galleries that are the same work;
    /// - English to Japanese: the English galleries whose title has the text, and the Japanese gallery that is the same work (when
    ///   the first search does not have it, the romaji after the bar of the title is looked up).
    /// The ones that are the same title come first, then the newest.
    async fn ask_translations(self: &Arc<Self>, text: &str, from: Lang) -> Result<Vec<Translation>> {
        use crate::hitomi::{squash, Gallery};

        let Some(index) = self.sites.iter().position(|site| site.def.format == Format::Hitomi) else {
            return Err("Hitomi.La is not one of the sites".into());
        };

        let wanted = squash(text);

        if wanted.is_empty() {
            return Ok(Vec::new());
        }

        let english_of = |gallery: &Gallery| gallery.english_title();
        let is_english = |gallery: &Gallery| gallery.language.as_deref() == Some("english");
        let japanese_of = |gallery: &Gallery| gallery.japanese_title.as_deref().map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);

        let mut pool = self.hitomi_galleries(index, text).await?;
        let mut found: Vec<Translation> = Vec::new();

        if from == Lang::English {
            // the Japanese gallery is not always in the answer: the romaji of the English titles finds it
            let anchors: Vec<Gallery> = pool.iter().filter(|g| is_english(g) && squash(&english_of(g)).contains(&wanted)).cloned().collect();
            let mut asked = HashSet::new();

            for anchor in anchors.iter().take(3) {
                let Some((_, tail)) = anchor.title.rsplit_once(" | ") else { continue };

                if Lang::of(tail) == Some(Lang::English)
                    && asked.insert(squash(tail))
                    && !pool.iter().any(|g| japanese_of(g).is_some() && g.same_work(anchor))
                {
                    for gallery in self.hitomi_galleries(index, tail).await? {
                        if !pool.iter().any(|g| g.id == gallery.id) {
                            pool.push(gallery);
                        }
                    }
                }
            }
        }

        for anchor in &pool {
            let Some(japanese) = japanese_of(anchor) else { continue };

            for other in &pool {
                if !is_english(other) || !anchor.same_work(other) {
                    continue;
                }

                let english = english_of(other);
                let side = squash(if from == Lang::Japanese { &japanese } else { &english });

                if side.contains(&wanted) {
                    found.push(Translation {
                        id: other.id,
                        url: self.sites[index].address(&other.path()),
                        exact: side == wanted,
                        english,
                        japanese: japanese.clone(),
                        source: "hitomi".into(),
                    });
                }
            }
        }

        // the same answer once; the same title first, the order of the galleries (the newest first) otherwise
        let mut seen = HashSet::new();
        found.retain(|t| seen.insert((squash(&t.english), squash(&t.japanese))));
        found.sort_by_key(|translation| !translation.exact);

        Ok(found)
    }

    /// The time that is kept between two requests to a site: the one `[search] site_delay_ms` names; else the delay of the site
    /// when it has its own (Hitomi.La: a few dozen small files on a static host); else the longer of `min_delay_ms` and the delay
    /// the program knows for the site.
    fn delay_of(site: &Site, config: &SearchConfig) -> Duration {
        let named = config.site_delay_ms.iter().find(|(name, _)| name.eq_ignore_ascii_case(site.def.name));

        Duration::from_millis(match named {
            Some((_, ms)) => *ms,
            None if site.def.format == Format::Hitomi => site.def.delay_ms,
            None => config.min_delay_ms.max(site.def.delay_ms),
        })
    }

    /// Keeps `min_delay_ms` (or the longer delay of the site) between two requests to the same site.
    async fn wait_turn(&self, index: usize) {
        let delay = self.delays[index];
        let mut last = self.gates[index].lock().await;

        if let Some(at) = *last {
            if let Some(wait) = delay.checked_sub(at.elapsed()) {
                tokio::time::sleep(wait).await;
            }
        }
        *last = Some(Instant::now());
    }
}

/// Reads the files of the index host of Hitomi.La (the whole answer, or a range of it), each one after the turn of the site.
pub(crate) struct HitomiFetch<'a> {
    engine: &'a Engine,
    index: usize,
}

impl crate::hitomi::Fetch for HitomiFetch<'_> {
    async fn get(&mut self, path: &str, range: Option<(u64, u64)>) -> std::result::Result<Vec<u8>, String> {
        self.engine.wait_turn(self.index).await;

        // the version is asked with a changing number, so that no cache answers with an old one
        let url = if path.ends_with("/version") {
            format!("{}{path}?_={}", crate::hitomi::ORIGIN, unix_now() * 1000)
        } else {
            format!("{}{path}", crate::hitomi::ORIGIN)
        };

        let mut request = self.engine.client.get(&url).header("Referer", "https://hitomi.la/");
        if let Some((from, to)) = range {
            // a range of a file is read as it is: the bytes are counted in the file, and a compressed answer would not be them
            request = request.header("Range", format!("bytes={from}-{to}")).header("Accept-Encoding", "identity");
        }

        let response = request.send().await.map_err(|e| format!("{path}: {e}"))?;
        let status = response.status().as_u16();

        if status != 200 && status != 206 {
            return Err(format!("{path}: HTTP {status}"));
        }
        if range.is_some() && status == 200 {
            // the whole file of an index is far too big to read
            return Err(format!("{path}: the site does not answer a Range request"));
        }

        response.bytes().await.map(|bytes| bytes.to_vec()).map_err(|e| format!("{path}: {e}"))
    }
}

/// One hit for each address: the same page saved twice (`/g/1/` and `/g/1`, two sites that answer with the same address) is
/// one result. The first one (the order of the answer) stays, and is new when any of them is.
fn dedupe_items<'a>(candidates: &'a [Candidate], origins: &'a [String], items: Vec<Item>) -> Vec<Item> {
    // the key is the address in two pieces (the origin of the site, and the path without its `#` and its last `/`), folded to lower case:
    // the same as the address as one text, and no copy for a path that is already in lower case
    let key = |item: &Item| -> (&'a str, std::borrow::Cow<'a, str>) {
        let path = candidates[item.index as usize].path.as_str();
        let path = path.split('#').next().unwrap_or(path).trim_end_matches('/');
        let path = if path.is_ascii() && !path.bytes().any(|b| b.is_ascii_uppercase()) { std::borrow::Cow::Borrowed(path) } else { std::borrow::Cow::Owned(path.to_lowercase()) };

        (origins[item.site as usize].as_str(), path)
    };
    let mut places: HashMap<(&str, std::borrow::Cow<str>), usize> = HashMap::with_capacity(items.len());
    let mut kept: Vec<Item> = Vec::with_capacity(items.len());

    for item in items {
        match places.entry(key(&item)) {
            std::collections::hash_map::Entry::Occupied(place) => kept[*place.get()].is_new |= item.is_new,
            std::collections::hash_map::Entry::Vacant(place) => {
                place.insert(kept.len());
                kept.push(item);
            }
        }
    }

    kept
}

/// The same as `dedupe_items` for the hits themselves: what the answer was before it was made of places (for the tests).
#[cfg(test)]
fn dedupe_by_url(hits: Vec<Hit>) -> Vec<Hit> {
    let mut places: HashMap<String, usize> = HashMap::new();
    let mut kept: Vec<Hit> = Vec::new();

    for hit in hits {
        let key = hit.url.split('#').next().unwrap_or(&hit.url).trim_end_matches('/').to_lowercase();

        match places.get(&key) {
            Some(&place) => kept[place].is_new |= hit.is_new,
            None => {
                places.insert(key, kept.len());
                kept.push(hit);
            }
        }
    }

    kept
}

/// How a site tells that the page of a work is gone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoneRule {
    /// The page answers 404 or 410 (RokuHentai: or the 503 with "temporarily unavailable ... Here are some results that might be useful to you").
    NotFound,
    /// The page redirects to `/404/` (AsmHentai, HentaiEra).
    RedirectsTo404,
    /// The page answers 200 with a sentence that says the work is not there (Wnacg).
    Sentence(&'static str),
}

/// The sites whose pages tell that a work is gone, and how. (Hitomi.La's page is the same shell for any id: its index file says which
/// galleries there are. E-Hentai answers 200 with a short text, and it bans an address that asks more than a page every few seconds.)
pub fn gone_rule(site: &str) -> Option<GoneRule> {
    match site {
        "RokuHentai" | "DLsite" | "HentaiEnvy" | "Nya:Hentai" | "momon:GA" | "Sukebei" => Some(GoneRule::NotFound),
        "AsmHentai" | "HentaiEra" => Some(GoneRule::RedirectsTo404),
        "Wnacg" => Some(GoneRule::Sentence("您要訪問的漫畫不存在")),
        _ => None,
    }
}

/// What the answer for the page of a work says under `rule`: `Some(true)` it is gone, `Some(false)` it is there (a 200 without the sign), `None`
/// no sign (another 503, 429, a failure of the site, ...).
fn page_status(rule: GoneRule, status: u16, final_path: &str, body: &str) -> Option<bool> {
    match rule {
        GoneRule::NotFound => {
            let lower = body.to_lowercase();

            match status {
                200 => Some(false),
                404 | 410 => Some(true),
                503 if lower.contains("temporarily unavailable") && lower.contains("results that might be useful") => Some(true),
                _ => None,
            }
        }
        GoneRule::RedirectsTo404 if final_path.trim_end_matches('/') == "/404" => Some(true),
        GoneRule::RedirectsTo404 => (status == 200).then_some(false),
        GoneRule::Sentence(sentence) if status == 200 => Some(body.contains(sentence)),
        GoneRule::Sentence(_) => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::sites::{Clean, Format, SiteDef};
    use axum::extract::{Query, State};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::Router;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    pub(crate) struct Hits {
        pub(crate) ok: AtomicUsize,
        failing: AtomicUsize,
        blocked: AtomicUsize,
        limited: AtomicUsize,
    }

    /// Three "sites" on one local server: one lists works, one answers with an error, one with a challenge.
    async fn server() -> (u16, Arc<Hits>) {
        let hits = Arc::new(Hits::default());
        let router = Router::new()
            .route(
                "/ok",
                get(|State(hits): State<Arc<Hits>>, Query(q): Query<HashMap<String, String>>| async move {
                    hits.ok.fetch_add(1, Ordering::SeqCst);
                    let word = q.get("q").cloned().unwrap_or_default();

                    format!(
                        r#"<a href="/work/1">{word} one</a><a href="/work/2"><img src="https://img.example/a.jpg" alt="{word} two"></a><a href="/about">about</a>"#
                    )
                }),
            )
            .route("/ja", get(|| async { r#"<a href="/work/9">COMIC快楽天 2026年7月号</a>"# }))
            .route(
                "/failing",
                get(|State(hits): State<Arc<Hits>>| async move {
                    hits.failing.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response()
                }),
            )
            .route(
                "/limited",
                get(|State(hits): State<Arc<Hits>>| async move {
                    // too many requests the first time, then the page
                    if hits.limited.fetch_add(1, Ordering::SeqCst) == 0 {
                        return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "0")], "slow down").into_response();
                    }

                    r#"<a href="/work/5">slow work</a>"#.into_response()
                }),
            )
            .route(
                "/blocked",
                get(|State(hits): State<Arc<Hits>>| async move {
                    hits.blocked.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::FORBIDDEN, "<title>Just a moment...</title>").into_response()
                }),
            )
            .with_state(hits.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        (port, hits)
    }

    fn leak_as(name: &'static str, pattern: String, search: String, legal: bool, japanese: bool) -> &'static SiteDef {
        Box::leak(Box::new(def(name, pattern, search, legal, japanese)))
    }

    fn def(name: &'static str, pattern: String, search: String, legal: bool, japanese: bool) -> SiteDef {
        SiteDef {
            name,
            legal,
            origin: "http://127.0.0.1",
            keep_query: &[],
            pattern: Box::leak(pattern.into_boxed_str()),
            filter: None,
            search: Some(Box::leak(search.into_boxed_str())),
            cookie: None,
            clean: Clean::Plain,
            format: Format::Html,
            delay_ms: 0,
            japanese,
            keep_times: false,
            support: Support::Ready,
            note: "",
        }
    }

    fn leak(name: &'static str, pattern: String, search: String, legal: bool) -> &'static SiteDef {
        leak_as(name, pattern, search, legal, true)
    }

    fn config() -> SearchConfig {
        SearchConfig { min_delay_ms: 0, ..SearchConfig::default() }
    }

    pub(crate) async fn setup(config: &SearchConfig) -> (Arc<Engine>, Arc<Hits>, Arc<SearchStore>) {
        let (port, hits) = server().await;
        let work = format!(r"^http://127\.0\.0\.1:{port}/work/\d+");
        let sites = vec![
            Site::new(leak("Good", work.clone(), format!("http://127.0.0.1:{port}/ok?q={{q}}"), true)),
            Site::new(leak("Failing", work.clone(), format!("http://127.0.0.1:{port}/failing?q={{q}}"), false)),
            Site::new(leak("Blocked", work, format!("http://127.0.0.1:{port}/blocked?q={{q}}"), false)),
        ];
        let store = Arc::new(SearchStore::in_memory().await.unwrap());

        (Engine::with_sites(sites, config, store.clone(), crate::testing::romanizer_arc()), hits, store)
    }

    fn requests(hits: &Hits) -> (usize, usize, usize) {
        (hits.ok.load(Ordering::SeqCst), hits.failing.load(Ordering::SeqCst), hits.blocked.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn an_excluded_site_is_not_asked_not_looked_up_and_not_reported() {
        let (engine, hits, _) = setup(&config()).await;
        let leave_out = engine.site_indexes(&["good".to_string()]).unwrap();
        assert_eq!(leave_out.len(), 1, "the names are matched without regard to case");
        assert_eq!(engine.site_indexes(&["Nowhere".to_string()]), Err("Nowhere".to_string()));

        // the other two sites are asked, the good one is not, and its works are not in the answer
        let outcome = engine.search(&Request { exclude: &leave_out, ..Request::new("q") }).await.unwrap();
        assert_eq!(requests(&hits), (0, 1, 1));
        assert_eq!(outcome.sites.iter().map(|s| s.website.as_str()).collect::<Vec<_>>(), ["Failing", "Blocked"]);
        assert!(outcome.results.is_empty());

        // asked later without the exclusion, the site is asked (nothing was saved for it), and it answers
        let all = engine.search(&Request::new("q")).await.unwrap();
        assert_eq!(all.results.len(), 2);
        assert_eq!(requests(&hits).0, 1);

        // its saved works stay out of the answer while it is excluded: nothing is looked up
        let again = engine.search(&Request { live: Live::Off, exclude: &leave_out, ..Request::new("q") }).await.unwrap();
        assert!(again.results.is_empty());
    }

    #[tokio::test]
    async fn the_multiplication_sign_stays_only_for_the_sites_that_need_it() {
        let (port, _) = server().await;
        let work = format!(r"^http://127\.0\.0\.1:{port}/work/\d+");
        let search = format!("http://127.0.0.1:{port}/ok?q={{q}}");
        let times = SiteDef { keep_times: true, ..def("Times", work.clone(), search.clone(), true, true) };
        let sites = vec![Site::new(leak("Plain", work, search, true)), Site::new(Box::leak(Box::new(times)))];
        let store = Arc::new(SearchStore::in_memory().await.unwrap());
        let engine = Engine::with_sites(sites, &config(), store.clone(), crate::testing::romanizer_arc());

        let outcome = engine.search(&Request { query: "COMIC  E×E 11", live: Live::Diff, limit: 10, translator: Translator::None, exclude: &[] }).await.unwrap();

        assert_eq!(outcome.query, "COMIC ExE 11", "the answer is written with x");
        assert_eq!(store.states("COMIC ExE 11").await.unwrap().keys().collect::<Vec<_>>(), ["Plain"]);
        assert_eq!(store.states("COMIC E×E 11").await.unwrap().keys().collect::<Vec<_>>(), ["Times"]);
        let asked: Vec<_> = outcome.sites.iter().map(|site| (site.website.as_str(), site.queries.clone())).collect();
        assert_eq!(asked, [("Plain", vec!["COMIC ExE 11".to_string()]), ("Times", vec!["COMIC E×E 11".to_string()])]);
        // both sites answer (the same two addresses: they are 2 results, not 4), and the sign does not matter to the match
        assert_eq!(outcome.total, 2);

        // the bot writes the sign as x already: between capitals it is put back for the site
        let outcome = engine.search(&Request { query: "COMIC ExE 12", live: Live::Diff, limit: 10, translator: Translator::None, exclude: &[] }).await.unwrap();
        let asked: Vec<_> = outcome.sites.iter().map(|site| site.queries.clone()).collect();
        assert_eq!(asked, [vec!["COMIC ExE 12".to_string()], vec!["COMIC E×E 12".to_string()]]);
    }

    #[tokio::test]
    async fn every_site_is_asked_and_a_failing_one_does_not_stop_the_others() {
        let (engine, hits, store) = setup(&config()).await;

        let outcome = engine.search(&Request::new("  COMIC   ExE  ")).await.unwrap();

        assert_eq!(outcome.query, "COMIC ExE", "the query is tidied up");
        assert_eq!(outcome.input, "COMIC ExE", "and it has no Japanese in it, so it is its own romaji");
        assert_eq!(outcome.terms, ["comic", "exe"]);
        assert_eq!(requests(&hits), (1, 1, 1));

        let found: Vec<_> = outcome.results.iter().map(|h| (h.website.as_str(), h.is_legal, h.path.as_str(), h.title.as_str())).collect();
        assert_eq!(found, [("Good", true, "/work/1", "COMIC ExE one"), ("Good", true, "/work/2", "COMIC ExE two")]);
        assert_eq!(outcome.results[0].url, "http://127.0.0.1/work/1", "the address is the origin and the path");
        assert_eq!(outcome.total, 2);

        let report = |name: &str| outcome.sites.iter().find(|s| s.website == name).unwrap().clone();
        assert_eq!((report("Good").status, report("Good").count), ("searched", 2));
        assert_eq!((report("Failing").status, report("Failing").error.as_deref()), ("failed", Some("HTTP 500")));
        assert_eq!(report("Blocked").error.as_deref(), Some("blocked by a Cloudflare challenge"));

        // the works are in the database, with a path and a title and nothing else
        assert_eq!(store.counts().await.unwrap(), (1, 2));
    }

    #[tokio::test]
    async fn live_asks_a_site_once_in_the_validity_of_its_answer_and_reads_the_database_too() {
        let (engine, hits, _) = setup(&config()).await;

        let first = engine.search(&Request::new("q")).await.unwrap();
        let again = engine.search(&Request::new("q")).await.unwrap();

        assert_eq!(requests(&hits), (1, 1, 1), "a month (ttl_hours) has not passed: nobody is asked again, a failure included");
        assert_eq!(again.results.len(), 2);
        assert_eq!(again.sites.iter().find(|s| s.website == "Good").unwrap().status, "cached");
        assert_eq!(first.sites.iter().find(|s| s.website == "Good").unwrap().status, "searched");
        // the works the first request found were not in the database; the second request asked nobody
        assert!(first.results.iter().all(|h| h.is_new));
        assert!(again.results.iter().all(|h| !h.is_new), "and saved nothing");

        // live=force asks the sites whose answer is up to date too
        let all = engine.search(&Request { live: Live::Force, ..Request::new("q") }).await.unwrap();
        assert_eq!(requests(&hits), (2, 2, 2), "every site is asked again, a failure included");
        assert!(all.sites.iter().filter(|s| s.status != "failed").all(|s| s.status == "searched"));
        assert_eq!(all.results.len(), 2);

        // when the answer is older than the validity, the sites are asked again
        let (old, hits_old, _) = setup(&SearchConfig { ttl_hours: 0, retry_failed_minutes: 0, ..config() }).await;
        old.search(&Request::new("q")).await.unwrap();
        old.search(&Request::new("q")).await.unwrap();
        assert_eq!(requests(&hits_old), (2, 2, 2));

        // live=off: nobody is asked, and the database still answers
        let stored = engine.search(&Request { live: Live::Off, ..Request::new("q") }).await.unwrap();
        assert_eq!(requests(&hits), (2, 2, 2));
        assert_eq!(stored.results.len(), 2);
        assert!(stored.sites.iter().all(|s| s.status == "not searched" || s.status == "failed"), "a failure of an earlier try is still told");
        assert!(stored.results.iter().all(|h| !h.is_new));

        // another query is another answer
        engine.search(&Request::new("other")).await.unwrap();
        assert_eq!(requests(&hits), (3, 3, 3));
    }

    #[tokio::test]
    async fn a_work_whose_romaji_changed_is_written_again_and_its_search_is_forgotten() {
        let (engine, _, store) = setup(&config()).await;
        let work = |path: &str, title: &str, romaji: &str| NewWork { path: path.into(), title: title.into(), romaji: romaji.into(), language: None };

        // saved by an older version: the romaji of 彼女を was "kanojo wo"; the second work is the same as now
        store.save("q1", "Good", 7, &Ok(vec![work("/work/1", "彼女を", "kanojo wo")]), false).await.unwrap();
        store.save("q2", "Good", 8, &Ok(vec![work("/work/2", "comic", "comic")]), false).await.unwrap();

        let done = engine.reromanize_works().await.unwrap();

        assert_eq!(done, Reromanized { works: 2, changed: 1, forgotten: 1 });
        let saved = store.candidates_owned(&[]).await.unwrap();
        let one = saved.iter().find(|w| w.path == "/work/1").unwrap();
        assert_eq!((one.title_romaji.as_str(), one.title_key.as_str(), one.updated_at), ("kanojo o", "kanojoo", 7));
        assert!(store.states("q1").await.unwrap().is_empty(), "the search that listed it is done again");
        assert!(!store.states("q2").await.unwrap().is_empty(), "the other one stays");

        // the second time there is nothing to do
        assert_eq!(engine.reromanize_works().await.unwrap(), Reromanized { works: 2, changed: 0, forgotten: 0 });
    }

    #[tokio::test]
    async fn the_works_that_were_saved_before_are_answered_with_the_new_ones() {
        let (engine, _, store) = setup(&config()).await;

        // an earlier search saved a work of the same words that this request will not find again
        store.save("old", "Good", 1, &Ok(vec![NewWork { path: "/work/77".into(), title: "q earlier".into(), romaji: "q earlier".into(), language: None }]), false).await.unwrap();

        let outcome = engine.search(&Request::new("q")).await.unwrap();
        let by_path = |path: &str| outcome.results.iter().find(|h| h.path == path).map(|h| h.is_new);

        assert_eq!(outcome.total, 3);
        assert_eq!(by_path("/work/1"), Some(true), "found now");
        assert_eq!(by_path("/work/2"), Some(true));
        assert_eq!(by_path("/work/77"), Some(false), "from the database");
    }

    #[tokio::test]
    async fn fetch_skips_a_query_that_is_young_and_asks_again_when_it_is_old() {
        // this is what the prefetch does; a request asks every site
        let (engine, hits, _) = setup(&config()).await;
        engine.fetch("q", false).await.unwrap();
        engine.fetch("q", false).await.unwrap();
        assert_eq!(requests(&hits), (1, 1, 1), "a failure is not asked again at once either");
        assert!(engine.is_fresh("q").await.unwrap());
        assert!(!engine.is_fresh("never searched").await.unwrap());

        // everything is old at once with a ttl of 0, and so are the failures with a retry time of 0
        let (engine, hits, _) = setup(&SearchConfig { ttl_hours: 0, retry_failed_minutes: 0, ..config() }).await;
        engine.fetch("q", false).await.unwrap();
        engine.fetch("q", false).await.unwrap();
        assert_eq!(requests(&hits), (2, 2, 2));
        assert!(!engine.is_fresh("q").await.unwrap());
    }

    #[tokio::test]
    async fn the_answer_is_every_saved_title_that_has_all_the_words_whatever_query_found_it() {
        let (engine, hits, _) = setup(&config()).await;

        // two earlier queries saved "alpha beta one/two" and "gamma one/two"
        engine.search(&Request::new("alpha beta")).await.unwrap();
        engine.search(&Request::new("gamma")).await.unwrap();

        // the words in the other order, from the database alone
        let outcome = engine.search(&Request { live: Live::Off, ..Request::new("beta alpha") }).await.unwrap();
        let titles: Vec<_> = outcome.results.iter().map(|h| h.title.as_str()).collect();
        assert_eq!(titles.len(), 0, "the paths are the same for every query, so a page keeps the last title the site gave");
        assert_eq!(outcome.sites[0].status, "not searched");
        assert_eq!(requests(&hits), (2, 2, 2));
    }

    #[tokio::test]
    async fn a_page_of_the_answer_is_a_part_of_the_whole_answer() {
        let (engine, _, _) = setup(&config()).await;
        let works: Vec<NewWork> = (1..=7).map(|n| NewWork { path: format!("/work/{n}"), title: format!("Alpha {n}"), romaji: format!("alpha {n}"), language: None }).collect();
        engine.store.save("q", "Good", 1, &Ok(works), false).await.unwrap();

        let whole = engine.search(&Request { live: Live::Off, limit: 100, ..Request::new("alpha") }).await.unwrap();
        assert_eq!(whole.total, 7);
        assert_eq!(whole.results.len(), 7);

        // the first works of a short answer are the first works of the whole one, and the totals are those of the whole answer
        let few = engine.search(&Request { live: Live::Off, limit: 3, ..Request::new("alpha") }).await.unwrap();
        assert_eq!(few.results, whole.results[..3]);
        assert_eq!((few.total, few.total_legal, few.total_illegal), (whole.total, whole.total_legal, whole.total_illegal));

        // the page 2 of 3 works (the api asks for offset + limit and drops the offset)
        let upto = engine.search(&Request { live: Live::Off, limit: 6, ..Request::new("alpha") }).await.unwrap();
        assert_eq!(upto.results[3..], whole.results[3..6]);
    }

    #[test]
    fn the_places_of_the_works_are_deduplicated_like_the_works_themselves() {
        let work = |website: &str, path: &str| (website.to_string(), Candidate { site: 0, path: path.into(), title: "t".into(), title_romaji: "t".into(), title_key: "t".into(), language: None });
        let pairs = vec![
            work("A", "/g/1/"),
            work("A", "/g/1"),
            work("A", "/G/1#frag"),
            work("B", "/g/1"),
            work("A", "/g/2"),
            work("B", "/g/2/"),
            work("B", "/"),
            work("B", ""),
            work("A", "/ü/1"),
            work("A", "/Ü/1/"),
        ];
        let candidates: Vec<Candidate> = pairs.iter().map(|(_, work)| work.clone()).collect();
        let origins = vec!["https://a.example".to_string(), "https://a.example".to_string()];
        let new = [false, true, false, false, false, false, false, true, false, false];
        let sites = [0u16, 0, 0, 1, 0, 1, 1, 1, 0, 0];
        let items: Vec<Item> = (0..candidates.len()).map(|n| Item { set: 0, index: n as u32, site: sites[n], is_new: new[n] }).collect();

        let hits: Vec<Hit> = items
            .iter()
            .map(|item| {
                let work = &candidates[item.index as usize];

                Hit {
                    website: pairs[item.index as usize].0.clone(),
                    is_legal: false,
                    url: format!("{}{}", origins[item.site as usize], work.path),
                    path: work.path.clone(),
                    title: work.title.clone(),
                    title_romaji: work.title_romaji.clone(),
                    title_english: None,
                    language: Vec::new(),
                    is_new: item.is_new,
                }
            })
            .collect();

        let by_hits: Vec<(String, bool)> = dedupe_by_url(hits).into_iter().map(|hit| (hit.url, hit.is_new)).collect();
        let by_items: Vec<(String, bool)> = dedupe_items(&candidates, &origins, items).into_iter().map(|item| (format!("{}{}", origins[item.site as usize], candidates[item.index as usize].path), item.is_new)).collect();

        assert_eq!(by_items, by_hits);
        assert!(by_hits.len() < candidates.len());
    }

    #[tokio::test]
    async fn an_address_that_no_site_owns_has_no_title() {
        let (engine, hits, _) = setup(&config()).await;

        assert_eq!(engine.title_of("https://example.com/some/page", Live::Diff).await, Err(None));
        assert_eq!(requests(&hits), (0, 0, 0), "no site is asked");
    }

    #[tokio::test]
    async fn a_saved_work_answers_whatever_the_address_carries_besides_the_page() {
        let (engine, hits, _) = setup(&config()).await;
        engine.store.save("q", "Good", 1, &Ok(vec![NewWork { path: "/work/12".into(), title: "Saved title".into(), romaji: "saved title".into(), language: None }]), false).await.unwrap();
        let origin = engine.sites[0].search_url("q").unwrap().split("/ok").next().unwrap().to_string();
        let page = format!("{origin}/work/12");

        // a fragment, a query string and a slash at the end do not matter
        for url in [page.clone(), format!("{page}/"), format!("{page}?utm_source=x&ref=y#top")] {
            let info = engine.title_of(&url, Live::Diff).await.unwrap();
            assert_eq!((info.website, info.title.as_str(), info.source, info.title_romaji.as_str()), ("Good", "Saved title", "database", "Saved title"), "{url}");
            assert_eq!((info.title_japanese, info.title_english.as_ref().map(|t| t.text.as_str())), (None, Some("Saved title")), "a title in Latin letters is its own English title");
        }
        assert_eq!(requests(&hits), (0, 0, 0), "the database answered, no page was asked");

        // a work that is not saved is not answered by `off`
        assert!(matches!(engine.title_of(&format!("{origin}/work/13"), Live::Off).await, Err(Some(_))));
    }

    #[test]
    fn the_the_of_a_translated_title_is_taken_off() {
        assert_eq!(without_the("The Phaethon Assault"), "Phaethon Assault");
        assert_eq!(without_the("the sister"), "sister");
        assert_eq!(without_the("Theory of Love"), "Theory of Love");
        assert_eq!(without_the("The"), "The");
        assert_eq!(without_the("  Then and Now "), "Then and Now");
    }

    #[test]
    fn the_language_of_a_text_is_japanese_or_english_only() {
        assert_eq!(Lang::of("姉弟でシたらいけません"), Some(Lang::Japanese));
        assert_eq!(Lang::of("COMIC 快楽天 2026"), Some(Lang::Japanese));
        assert_eq!(Lang::of("Siblings Shouldn't Do It 2"), Some(Lang::English));
        assert_eq!(Lang::of("Café"), Some(Lang::English));
        assert_eq!(Lang::of("형제"), None);
        assert_eq!(Lang::of("Привет"), None);
        assert_eq!(Lang::of("123 !?"), None);
    }

    #[tokio::test]
    async fn the_translations_are_saved_for_good() {
        let (engine, _, store) = setup(&config()).await;
        let row = crate::entity::translation::Model {
            key: String::new(),
            gallery_id: 7,
            position: 0,
            url: "https://hitomi.la/galleries/7.html".into(),
            english: "Seven".into(),
            japanese: "七".into(),
            english_key: "seven".into(),
            japanese_key: "七".into(),
            exact: true,
            source: "hitomi".into(),
        };

        // never asked, and the site is not to be asked: nothing
        assert_eq!(engine.translate_hitomi("七", Lang::Japanese, Live::Off).await.unwrap(), (vec![], true));
        // punctuation alone is nothing to look for
        assert_eq!(engine.translate_hitomi(" !? ", Lang::Japanese, Live::Force).await.unwrap(), (vec![], false));

        // a saved answer is the one that is given, whatever `live` says unless it is `on`
        store.save_translations("七", "hitomi", "七", "ja", unix_now(), &[row.clone()]).await.unwrap();
        let (found, saved) = engine.translate_hitomi("七", Lang::Japanese, Live::Diff).await.unwrap();
        assert!(saved);
        assert_eq!((found[0].id, found[0].text(Lang::English), found[0].text(Lang::Japanese), found[0].exact), (7, "Seven", "七", true));
        assert_eq!(engine.translate_hitomi("七", Lang::Japanese, Live::Off).await.unwrap().0.len(), 1);

        // a saved answer never gets old: only `on` asks the site again (the engine of the tests has no Hitomi.La: that is an error)
        store.save_translations("七", "hitomi", "七", "ja", 1, &[row.clone()]).await.unwrap();
        assert_eq!(engine.translate_hitomi("七", Lang::Japanese, Live::Diff).await.unwrap().0.len(), 1);
        assert!(engine.translate_hitomi("七", Lang::Japanese, Live::Force).await.is_err());
        assert_eq!(engine.translate_hitomi("七", Lang::Japanese, Live::Off).await.unwrap().0.len(), 1);

        // a translation that was asked from the other side answers too: Japanese to English saved, English to Japanese asked
        store.save_translations("九", "hitomi", "九", "ja", unix_now(), &[crate::entity::translation::Model { gallery_id: 9, english: "Nine Lives".into(), japanese: "九つの命".into(), english_key: "ninelives".into(), japanese_key: "九つの命".into(), ..row.clone() }]).await.unwrap();
        let (found, saved) = engine.translate_hitomi("Nine", Lang::English, Live::Diff).await.unwrap();
        assert!(saved);
        assert_eq!((found[0].japanese.as_str(), found[0].exact), ("九つの命", false));
        assert_eq!(engine.translate_hitomi("九つの命", Lang::Japanese, Live::Force).await.is_err(), true, "`on` asks the site");
        assert_eq!(engine.translate_hitomi("Nine Lives", Lang::English, Live::Off).await.unwrap().0[0].exact, true);

        // Google Translate is cached the same way: the saved translation is the answer (no request), `off` needs a saved one
        store.save_translations("十", "google", "十", "ja", unix_now(), &[crate::entity::translation::Model { gallery_id: 0, url: String::new(), english: "Ten".into(), japanese: "十".into(), english_key: "ten".into(), japanese_key: "十".into(), source: "google".into(), ..row.clone() }]).await.unwrap();
        let (translation, saved) = engine.translate_google("十", Lang::Japanese, Live::Diff).await.unwrap();
        assert_eq!((translation.english.as_str(), translation.source.as_str(), saved), ("Ten", "google", true));
        assert_eq!(engine.translate_google("十", Lang::Japanese, Live::Off).await.unwrap().0.english, "Ten", "`off` is `diff` for Google");
        // the other direction: Japanese to English was saved, so English to Japanese is the saved one (the whole text only)
        let (back, saved) = engine.translate_google("Ten", Lang::English, Live::Diff).await.unwrap();
        assert_eq!((back.japanese.as_str(), saved), ("十", true));

        // a text that found nothing is asked again (`diff`), not taken from the lookup; `off` never asks
        store.save_translations("八", "hitomi", "八", "ja", unix_now(), &[]).await.unwrap();
        assert!(engine.translate_hitomi("八", Lang::Japanese, Live::Diff).await.is_err());
        assert_eq!(engine.translate_hitomi("八", Lang::Japanese, Live::Off).await.unwrap(), (vec![], true));
    }

    #[tokio::test]
    async fn a_query_without_japanese_is_searched_in_japanese_too() {
        let (engine, _, store) = setup(&config()).await;
        let row = crate::entity::translation::Model {
            key: String::new(),
            gallery_id: 9,
            position: 0,
            url: "https://hitomi.la/galleries/9.html".into(),
            english: "Nine Lives".into(),
            japanese: "九つの命".into(),
            english_key: "ninelives".into(),
            japanese_key: "九つの命".into(),
            exact: true,
            source: "hitomi".into(),
        };
        store.save_translations("九つの命", "hitomi", "九つの命", "ja", unix_now(), &[row]).await.unwrap();

        let outcome = engine.search(&Request { live: Live::Off, translator: Translator::Auto, ..Request::new("Nine Lives") }).await.unwrap();
        assert_eq!(outcome.translation_ja.as_ref().map(|t| (t.text.as_str(), t.source.as_str())), Some(("九つの命", "hitomi")));
        assert!(outcome.translation.is_none());

        // a query in Japanese has the English title and no Japanese one; a query with no letter has neither
        let outcome = engine.search(&Request { live: Live::Off, translator: Translator::Auto, ..Request::new("九つの命") }).await.unwrap();
        assert!(outcome.translation_ja.is_none());
        let outcome = engine.search(&Request { live: Live::Off, translator: Translator::Auto, ..Request::new("2026 7") }).await.unwrap();
        assert!(outcome.translation_ja.is_none() && outcome.translation.is_none());
        // no translator: no Japanese title
        let outcome = engine.search(&Request { live: Live::Off, translator: Translator::None, ..Request::new("Nine Lives") }).await.unwrap();
        assert!(outcome.translation_ja.is_none());
    }

    /// The saved works the way the words are defined, row by row: the sites in their order, the last added first in a site. A word of 3
    /// characters or more is in the key of what is outside the parentheses of the title; a shorter one is a word of the whole romaji.
    async fn expected_titles(engine: &Arc<Engine>, query: &str) -> Vec<String> {
        let words: Vec<String> = Terms::new(&engine.plan(query).unwrap().query).keys().into_iter().map(str::to_string).collect();
        let works = engine.store.candidates_owned(&[]).await.unwrap();
        let mut titles = Vec::new();

        for index in engine.sites_in_order(&[]) {
            titles.extend(
                works
                    .iter()
                    .filter(|w| w.website == engine.sites[index].def.name)
                    .filter(|w| {
                        words.iter().all(|word| {
                            if word.chars().count() >= 3 {
                                w.title_key.contains(word.as_str())
                            } else {
                                w.title_romaji.split(|c: char| !c.is_ascii_alphanumeric()).any(|piece| piece == word)
                            }
                        })
                    })
                    .map(|w| w.title.clone()),
            );
        }

        titles
    }

    #[tokio::test]
    async fn the_indexes_answer_as_the_words_are_defined() {
        let (engine, _, store) = setup(&config()).await;
        let work = |n: usize, title: &str| NewWork { path: format!("/work/{n}"), title: title.into(), romaji: title.to_lowercase(), language: None };
        let titles = ["Maid ai", "work (COMIC kairakuten 2018 8)", "COMIC kairakuten 2026 11", "ai no uta", "main maid", "kairakuten (ai)", "tokyo kairakuten", "Maid kairakuten ai", "ai"];

        // the same titles on two sites
        for (site, shift) in [("Good", 0), ("Failing", 100)] {
            let works: Vec<NewWork> = titles.iter().enumerate().map(|(n, title)| work(shift + n, title)).collect();
            store.save("q", site, 1, &Ok(works), false).await.unwrap();
        }

        for query in ["ai", "maid", "kairakuten", "maid ai", "kairakuten ai", "kairakuten maid ai", "comic kairakuten", "nothing"] {
            assert!(Terms::new(query).matcher().is_some(), "{query}: the indexes can answer it");

            let outcome = engine.search(&Request { live: Live::Off, ..Request::new(query) }).await.unwrap();
            let found: Vec<String> = outcome.results.iter().map(|hit| hit.title.clone()).collect();
            let expected = expected_titles(&engine, query).await;

            assert_eq!(found, expected, "{query}");
            assert_eq!(outcome.total, expected.len(), "{query}");
            // Good is the legal site, and it has half of the works
            assert_eq!((outcome.total_legal, outcome.total_illegal), (expected.len() / 2, expected.len() - expected.len() / 2), "{query}");

            // a site that is left out is not counted
            let leave_out = engine.site_indexes(&["good".to_string()]).unwrap();
            let without = engine.search(&Request { live: Live::Off, exclude: &leave_out, ..Request::new(query) }).await.unwrap();
            assert_eq!((without.total, without.total_legal), (expected.len() / 2, 0), "{query}");

            // a page that is cut: the totals are those of the whole answer, and the page is the head of it
            let head = engine.search(&Request { live: Live::Off, limit: 3, ..Request::new(query) }).await.unwrap();
            assert_eq!(head.total, expected.len(), "{query}");
            assert_eq!(head.results.iter().map(|hit| hit.title.clone()).collect::<Vec<_>>(), expected[..expected.len().min(3)], "{query}");
        }

        // the last added work of a site is its first
        let outcome = engine.search(&Request { live: Live::Off, ..Request::new("kairakuten") }).await.unwrap();
        assert_eq!(outcome.results[0].path, "/work/2", "the first of the list of the page is the last added");
    }

    #[tokio::test]
    async fn the_works_of_the_other_title_are_counted_once_by_the_indexes() {
        let (engine, _, store) = setup(&config()).await;
        let row = crate::entity::translation::Model {
            key: String::new(),
            gallery_id: 9,
            position: 0,
            url: "https://hitomi.la/galleries/9.html".into(),
            english: "Nine Lives".into(),
            japanese: "九つの命".into(),
            english_key: "ninelives".into(),
            japanese_key: "九つの命".into(),
            exact: true,
            source: "hitomi".into(),
        };
        store.save_translations("九つの命", "hitomi", "九つの命", "ja", unix_now(), &[row]).await.unwrap();

        // the romaji of the Japanese title is what is searched; one work has the English words, one the Japanese ones, one both
        let japanese = engine.plan("九つの命").unwrap().query;
        assert!(Terms::new(&japanese).matcher().is_some() && Terms::new("Nine Lives").matcher().is_some());
        let work = |n: usize, romaji: String| NewWork { path: format!("/work/{n}"), title: romaji.clone(), romaji, language: None };
        let works = vec![work(1, "nine lives one".into()), work(2, format!("{japanese} two")), work(3, format!("nine lives {japanese} three")), work(4, "other".into())];
        store.save("q", "Good", 1, &Ok(works), false).await.unwrap();

        let titles = |outcome: &Outcome| outcome.results.iter().map(|hit| hit.path.clone()).collect::<Vec<_>>();
        let whole = engine.search(&Request { live: Live::Off, translator: Translator::Auto, ..Request::new("Nine Lives") }).await.unwrap();
        // the English title's works (the first of the list of the page is the last added), and then the works of the Japanese title that are not in the answer yet
        assert_eq!(titles(&whole), ["/work/1", "/work/3", "/work/2"]);
        assert_eq!(whole.total, 3, "the work of both titles is counted once");

        // a page that is cut inside the first title, and one that is cut at its end: the total stays the total of the whole answer
        for (limit, expected) in [(1, vec!["/work/1"]), (2, vec!["/work/1", "/work/3"])] {
            let page = engine.search(&Request { live: Live::Off, translator: Translator::Auto, limit, ..Request::new("Nine Lives") }).await.unwrap();
            assert_eq!((titles(&page), page.total), (expected.into_iter().map(String::from).collect::<Vec<_>>(), 3), "limit {limit}");
        }
    }

    #[tokio::test]
    async fn the_same_address_is_one_result() {
        let (engine, _, store) = setup(&config()).await;
        let work = |path: &str, title: &str| NewWork { path: path.into(), title: title.into(), romaji: title.into(), language: None };

        // one page saved with and without the slash at the end, and a different page
        store.save("a", "Good", 1, &Ok(vec![work("/work/10/", "alpha one"), work("/work/11", "alpha two")]), false).await.unwrap();
        store.save("b", "Good", 2, &Ok(vec![work("/work/10", "alpha one")]), false).await.unwrap();

        let outcome = engine.search(&Request { live: Live::Off, ..Request::new("alpha") }).await.unwrap();

        assert_eq!(outcome.total, 2, "the total counts a page once");
        assert_eq!(outcome.results.len(), 2);
        assert_eq!(outcome.results.iter().filter(|hit| hit.title == "alpha one").count(), 1);
    }

    #[tokio::test]
    async fn titles_of_different_pages_are_all_searched() {
        let (engine, _, store) = setup(&config()).await;

        // pages that are not the same page, as different queries find them
        store.save("a", "Good", 1, &Ok(vec![NewWork { path: "/work/10".into(), title: "alpha beta one".into(), romaji: "alpha beta one".into(), language: None }]), false).await.unwrap();
        store.save("b", "Good", 2, &Ok(vec![NewWork { path: "/work/11".into(), title: "gamma one".into(), romaji: "gamma one".into(), language: None }]), false).await.unwrap();

        let outcome = engine.search(&Request { live: Live::Off, ..Request::new("beta alpha") }).await.unwrap();
        assert_eq!(outcome.results.iter().map(|h| h.title.as_str()).collect::<Vec<_>>(), ["alpha beta one"], "any order");

        // AND: a word that is in none of the titles finds nothing
        let none = engine.search(&Request { live: Live::Off, ..Request::new("alpha gamma") }).await.unwrap();
        assert_eq!((none.total, none.results.len()), (0, 0));

        // a word alone finds the titles of any query, the last seen first
        let one = engine.search(&Request { live: Live::Off, ..Request::new("ONE") }).await.unwrap();
        assert_eq!(one.results.iter().map(|h| h.title.as_str()).collect::<Vec<_>>(), ["gamma one", "alpha beta one"]);
    }

    #[tokio::test]
    async fn the_query_is_always_turned_into_romaji_before_it_is_asked_and_matched() {
        let (engine, hits, _) = setup(&config()).await;

        let outcome = engine.search(&Request::new(" 快楽天 ")).await.unwrap();

        assert_eq!((outcome.input.as_str(), outcome.query.as_str()), ("快楽天", "kairakuten"));
        assert_eq!(outcome.terms, ["kairakuten"]);
        // the site finds Japanese titles: it was asked what was written, then the romaji (the last answer is kept)
        assert_eq!(outcome.sites[0].queries, ["快楽天", "kairakuten"]);
        assert_eq!(outcome.results[0].title, "kairakuten one");
        assert_eq!(requests(&hits).0, 2);

        // the same query written in romaji is the same search
        let again = engine.search(&Request::new("kairakuten")).await.unwrap();
        assert_eq!(again.query, "kairakuten");
        assert_eq!(again.sites[0].queries, ["kairakuten"], "no Japanese in it: the romaji alone");
        assert_eq!(again.results.len(), outcome.results.len());
        assert!(engine.is_fresh("快楽天").await.unwrap());
        assert_eq!(requests(&hits).0, 2, "the romaji form was asked with the Japanese one: it is not asked again");
    }

    #[tokio::test]
    async fn a_site_that_says_429_is_asked_once_more_after_its_retry_after() {
        let (port, hits) = server().await;
        let site = Site::new(leak("Limited", format!(r"^http://127\.0\.0\.1:{port}/work/\d+"), format!("http://127.0.0.1:{port}/limited?q={{q}}"), false));
        let engine = Engine::with_sites(vec![site], &config(), Arc::new(SearchStore::in_memory().await.unwrap()), crate::testing::romanizer_arc());

        let outcome = engine.search(&Request::new("slow")).await.unwrap();

        assert_eq!(hits.limited.load(Ordering::SeqCst), 2, "asked again once");
        assert_eq!(outcome.sites[0].status, "searched");
        assert_eq!(outcome.results.len(), 1);
    }

    #[tokio::test]
    async fn a_slow_site_is_asked_one_form_and_not_by_the_prefetch() {
        let (port, hits) = server().await;
        let work = format!(r"^http://127\.0\.0\.1:{port}/work/\d+");
        let slow = Box::leak(Box::new(SiteDef { delay_ms: 20_000, ..*leak("Slow", work.clone(), format!("http://127.0.0.1:{port}/ok?q={{q}}"), false) }));
        let fast = leak("Fast", work, format!("http://127.0.0.1:{port}/failing?q={{q}}"), false);
        let engine = Engine::with_sites(
            vec![Site::new(slow), Site::new(fast)],
            &config(),
            Arc::new(SearchStore::in_memory().await.unwrap()),
            crate::testing::romanizer_arc(),
        );

        // the prefetch asks the fast site only, in both forms
        let fetched = engine.fetch("快楽天", false).await.unwrap();
        assert_eq!(fetched.asked.len(), 1);
        assert_eq!((hits.ok.load(Ordering::SeqCst), hits.failing.load(Ordering::SeqCst)), (0, 2));
        assert!(engine.is_fresh("快楽天").await.unwrap(), "and a query is fresh without the slow site");

        // a request asks the slow site too, once, as it was written
        let outcome = engine.search(&Request::new("快楽天")).await.unwrap();
        let queries = |name: &str| outcome.sites.iter().find(|s| s.website == name).unwrap().queries.clone();
        assert_eq!(queries("Slow"), ["快楽天"]);
        assert_eq!(queries("Fast"), ["快楽天", "kairakuten"]);
        assert_eq!(hits.ok.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn only_a_site_that_finds_japanese_titles_is_asked_in_japanese() {
        let (port, hits) = server().await;
        let work = format!(r"^http://127.0.0.1:{port}/work/d+");
        let sites = vec![
            Site::new(leak_as("Japanese", work.clone(), format!("http://127.0.0.1:{port}/ok?q={{q}}"), false, true)),
            Site::new(leak_as("RomajiOnly", work, format!("http://127.0.0.1:{port}/failing?q={{q}}"), false, false)),
        ];
        let engine = Engine::with_sites(sites, &config(), Arc::new(SearchStore::in_memory().await.unwrap()), crate::testing::romanizer_arc());

        let outcome = engine.search(&Request::new("快楽天 2026")).await.unwrap();
        let queries = |name: &str| outcome.sites.iter().find(|s| s.website == name).unwrap().queries.clone();

        assert_eq!(queries("Japanese"), ["快楽天 2026", "kairakuten 2026"]);
        assert_eq!(queries("RomajiOnly"), ["kairakuten 2026"]);
        assert_eq!((requests(&hits).0, requests(&hits).1), (2, 1));

        // a query without Japanese is the romaji for everybody
        let outcome = engine.search(&Request::new("COMIC ExE")).await.unwrap();
        assert!(outcome.sites.iter().all(|s| s.queries == ["COMIC ExE"]));

        // fresh means every form of the query is fetched: the Japanese form is not
        engine.fetch("kairakuten 2026", false).await.unwrap();
        assert!(!engine.is_fresh("快楽天 2026 7").await.unwrap());
        engine.fetch("快楽天 2026 7", false).await.unwrap();
        assert!(engine.is_fresh("快楽天 2026 7").await.unwrap());
    }

    #[tokio::test]
    async fn a_title_is_saved_as_written_and_with_its_romaji_and_found_by_the_romaji() {
        let (port, _) = server().await;
        let site = Site::new(leak(
            "Ja",
            format!(r"^http://127\.0\.0\.1:{port}/work/\d+"),
            format!("http://127.0.0.1:{port}/ja?q={{q}}"),
            false,
        ));
        let store = Arc::new(SearchStore::in_memory().await.unwrap());
        let engine = Engine::with_sites(vec![site], &config(), store.clone(), crate::testing::romanizer_arc());

        // whatever the query, this site lists the same work
        engine.search(&Request::new("anything")).await.unwrap();

        let saved = store.candidates_owned(&[]).await.unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].title, "COMIC快楽天 2026年7月号", "the title as the site writes it");
        assert!(saved[0].title_romaji.contains("kairakuten"), "{}", saved[0].title_romaji);

        // a query in Japanese and a query in romaji are the same words, and both find the work
        for query in ["快楽天 2026 7", "kairakuten 2026 07", "COMIC 快楽天"] {
            let outcome = engine.search(&Request { live: Live::Off, ..Request::new(query) }).await.unwrap();

            assert_eq!(outcome.results.len(), 1, "{query} -> {:?}", outcome.query);
            assert_eq!(outcome.results[0].title, "COMIC快楽天 2026年7月号");
            assert_eq!(outcome.results[0].title_romaji, saved[0].title_romaji);
        }
    }

    #[tokio::test]
    async fn a_title_that_has_the_query_in_parentheses_is_not_an_answer() {
        let (engine, _, store) = setup(&config()).await;

        let work = |path: &str, title: &str| NewWork { path: path.into(), title: title.into(), romaji: title.to_lowercase(), language: None };
        store
            .save(
                "q",
                "Good",
                1,
                &Ok(vec![
                    work("/work/20", "COMIC kairakuten 2018 8"),
                    work("/work/21", "COMIC kairakuten 2018 8 (digital)"),
                    work("/work/22", "kanojo (COMIC kairakuten 2018 8)"),
                    work("/work/23", "[x] kanojo 8-po (COMIC kairakuten 2018) [Chinese]"),
                    work("/work/24", "COMIC kairakuten (2018 8)"),
                ]),
                false,
            )
            .await
            .unwrap();

        let outcome = engine.search(&Request { live: Live::Off, ..Request::new("comic kairakuten 2018 8") }).await.unwrap();
        let mut paths: Vec<&str> = outcome.results.iter().map(|h| h.path.as_str()).collect();
        paths.sort();

        assert_eq!(paths, ["/work/20", "/work/21", "/work/24"], "the works of the issue are left out, the number in parentheses is not");
        assert_eq!(outcome.total, 3);
    }

    #[tokio::test]
    async fn the_limit_cuts_the_answer_but_not_the_total() {
        let (engine, _, _) = setup(&config()).await;

        let outcome = engine.search(&Request { limit: 1, ..Request::new("q") }).await.unwrap();

        assert_eq!((outcome.results.len(), outcome.total), (1, 2));
        assert_eq!(outcome.total_legal + outcome.total_illegal, 2);
    }

    #[tokio::test]
    async fn sites_can_be_left_out_and_unsupported_ones_need_asking_for() {
        let (engine, hits, _) = setup(&SearchConfig { disabled_sites: vec!["failing".into(), "BLOCKED".into()], ..config() }).await;

        let outcome = engine.search(&Request::new("q")).await.unwrap();

        assert_eq!(requests(&hits), (1, 0, 0), "the names match whatever the case");
        assert_eq!(outcome.sites.len(), 1);

        // the real list: only the sites that can be searched are active, and a site with no search never is
        let real = Engine::new(&config(), Arc::new(SearchStore::in_memory().await.unwrap()), crate::testing::romanizer_arc());
        let info = real.sites_info();
        assert_eq!(info.len(), 28);
        assert!(info.iter().filter(|s| s.active).all(|s| s.support == "ready" && s.has_search));
        assert!(info.iter().any(|s| s.name == "IMHentai" && !s.active && s.support == "cloudflare"));
        assert!(info.iter().find(|s| s.name == "Hitomi.La").unwrap().active, "it is searched through its index files");

        let trying = Engine::new(
            &SearchConfig { try_unsupported: true, ..config() },
            Arc::new(SearchStore::in_memory().await.unwrap()),
            crate::testing::romanizer_arc(),
        );
        assert!(trying.sites_info().iter().find(|s| s.name == "IMHentai").unwrap().active);
        assert!(!trying.sites_info().iter().find(|s| s.name == "NijiGazo").unwrap().active, "no search URL");
    }

    #[test]
    fn a_page_that_is_gone_is_told_by_the_rule_of_its_site_and_a_site_that_is_down_is_no_sign() {
        let notice = "Roku Hentai is temporarily unavailable. Please try again shortly. Here are some results that might be useful to you.";

        assert_eq!(page_status(GoneRule::NotFound, 503, "/x", notice), Some(true));
        assert_eq!(page_status(GoneRule::NotFound, 404, "/x", ""), Some(true));
        assert_eq!(page_status(GoneRule::NotFound, 410, "/x", ""), Some(true));
        assert_eq!(page_status(GoneRule::NotFound, 200, "/x", ""), Some(false));
        assert_eq!(page_status(GoneRule::NotFound, 503, "/x", "Service Unavailable"), None, "a 503 without the notice is a site that is down");
        assert_eq!(page_status(GoneRule::NotFound, 429, "/x", ""), None);

        assert_eq!(page_status(GoneRule::RedirectsTo404, 200, "/404/", ""), Some(true));
        assert_eq!(page_status(GoneRule::RedirectsTo404, 404, "/404", ""), Some(true));
        assert_eq!(page_status(GoneRule::RedirectsTo404, 200, "/g/1/", ""), Some(false));
        assert_eq!(page_status(GoneRule::RedirectsTo404, 503, "/g/1/", ""), None);

        let rule = GoneRule::Sentence("not here");
        assert_eq!(page_status(rule, 200, "/p", "<p>the work is not here!</p>"), Some(true));
        assert_eq!(page_status(rule, 200, "/p", "<p>a work</p>"), Some(false));
        assert_eq!(page_status(rule, 500, "/p", "not here"), None);
    }

    #[tokio::test]
    async fn a_crawled_site_is_not_asked_live_when_the_setting_is_on_unless_live_is_force() {
        let (engine, hits, _) = setup(&SearchConfig { skip_crawled: true, ..config() }).await;
        engine.store().mark_crawled("Good", unix_now()).await.unwrap();

        engine.search(&Request::new("q")).await.unwrap();
        assert_eq!(requests(&hits).0, 0, "the list of the site is in the database: it is not asked");
        assert_eq!(requests(&hits).1, 1, "a site whose list is not read is asked as before");

        engine.search(&Request { live: Live::Force, ..Request::new("q") }).await.unwrap();
        assert_eq!(requests(&hits).0, 1, "live=force asks it");

        // the setting is off: the crawled site is asked like any other
        let (plain, hits_plain, _) = setup(&config()).await;
        plain.store().mark_crawled("Good", unix_now()).await.unwrap();
        plain.search(&Request::new("q")).await.unwrap();
        assert_eq!(requests(&hits_plain).0, 1);
    }

    #[tokio::test]
    async fn the_prefetch_leaves_out_the_disabled_sites_too() {
        // the prefetch is fetch and is_fresh: they work on the sites that are active, like a request
        let (engine, hits, _) = setup(&SearchConfig { disabled_sites: vec!["failing".into(), "BLOCKED".into()], ..config() }).await;

        let fetched = engine.fetch("q", false).await.unwrap();

        assert_eq!(fetched.asked.len(), 1);
        assert_eq!(requests(&hits), (1, 0, 0));
        assert!(engine.is_fresh("q").await.unwrap(), "a site that is left out is not waited for");
    }

    #[tokio::test]
    async fn the_saved_works_of_a_site_that_is_off_are_in_the_answer_and_those_of_a_site_the_request_leaves_out_are_not() {
        let (engine, _, store) = setup(&SearchConfig { disabled_sites: vec!["good".into()], ..config() }).await;
        let work = |n: usize| NewWork { path: format!("/work/{n}"), title: format!("saved title {n}"), romaji: format!("saved title {n}"), language: None };
        store.save("q", "Good", 1, &Ok(vec![work(1)]), false).await.unwrap();
        store.save("q", "Failing", 1, &Ok(vec![work(2)]), false).await.unwrap();

        // the site is off in the config (it is not asked), and what is saved of it is in the answer, after the works of the sites that are asked
        let outcome = engine.search(&Request { live: Live::Off, ..Request::new("saved") }).await.unwrap();
        assert_eq!((outcome.total, outcome.total_legal), (2, 1));
        assert_eq!(outcome.results.iter().map(|hit| hit.website.as_str()).collect::<Vec<_>>(), ["Failing", "Good"]);

        // the request leaves it out: it is not in the answer
        let leave_out = engine.site_indexes(&["good".to_string()]).unwrap();
        let outcome = engine.search(&Request { live: Live::Off, exclude: &leave_out, ..Request::new("saved") }).await.unwrap();
        assert_eq!((outcome.total, outcome.total_legal), (1, 0));
    }

    #[tokio::test]
    async fn an_empty_query_is_an_error() {
        let (engine, hits, _) = setup(&config()).await;

        assert!(engine.search(&Request::new("   ")).await.is_err());
        assert!(engine.fetch("   ", false).await.is_err());
        assert_eq!(requests(&hits), (0, 0, 0));
    }

    #[tokio::test]
    async fn sites_are_not_asked_closer_than_the_minimum_delay() {
        let (engine, _, _) = setup(&SearchConfig { min_delay_ms: 300, ..config() }).await;
        let started = Instant::now();

        engine.search(&Request::new("one")).await.unwrap();
        engine.search(&Request::new("two")).await.unwrap();

        assert!(started.elapsed() >= Duration::from_millis(300), "{:?}", started.elapsed());
    }

    #[tokio::test]
    async fn the_delay_of_a_site_in_the_settings_replaces_the_minimum_and_is_kept_by_every_kind_of_request() {
        // 'good' (case does not matter) waits 400 ms, the others the 0 of min_delay_ms
        let config = SearchConfig { site_delay_ms: [("good".to_string(), 400)].into(), ..config() };
        let (engine, _, _) = setup(&config).await;
        let good = engine.site_named("Good").unwrap();
        let failing = engine.site_named("Failing").unwrap();
        let (_, url) = {
            let site = engine.site(good);
            (site.def.name, site.search_url("x").unwrap())
        };
        let started = Instant::now();

        // a search and a page of the crawl are two kinds of request to one site: they share its turn
        engine.search(&Request::new("one")).await.unwrap();
        engine.get_page(good, &url).await.unwrap();
        assert!(started.elapsed() >= Duration::from_millis(400), "the second request waited for the first: {:?}", started.elapsed());

        let started = Instant::now();
        let _ = engine.get_page(failing, &engine.site(failing).search_url("x").unwrap()).await;
        let _ = engine.get_page(failing, &engine.site(failing).search_url("y").unwrap()).await;
        assert!(started.elapsed() < Duration::from_millis(400), "another site is not slowed: {:?}", started.elapsed());
    }

    #[tokio::test]
    async fn the_language_of_a_result_is_a_list_and_empty_when_the_title_names_none() {
        let (engine, _, store) = setup(&config()).await;
        let work = |path: &str, title: &str| NewWork { path: path.into(), title: title.into(), romaji: title.into(), language: crate::language::detect(title) };

        store.save_crawled("Good", 1, &[work("/work/1", "kanojo [CN,EN,JP,KR]"), work("/work/2", "kanojo plain")], None).await.unwrap();
        let outcome = engine.search(&Request { live: Live::Off, ..Request::new("kanojo") }).await.unwrap();
        let languages: Vec<_> = outcome.results.iter().map(|hit| (hit.path.as_str(), hit.language.clone())).collect();

        assert!(languages.contains(&("/work/1", vec!["zh".to_string(), "en".to_string(), "ja".to_string(), "ko".to_string()])), "{languages:?}");
        assert!(languages.contains(&("/work/2", Vec::new())), "{languages:?}");
        assert_eq!(serde_json::to_value(&outcome.results[0]).unwrap()["language"].is_array(), true);
    }

    #[tokio::test]
    async fn the_english_title_of_a_result_is_the_title_a_pair_or_a_saved_translation_and_says_where_it_is_from() {
        let (engine, _, store) = setup(&config()).await;
        let work = |path: &str, title: &str| NewWork { path: path.into(), title: title.into(), romaji: title.into(), language: crate::language::detect(title) };
        let row = |english: &str, japanese: &str, source: &str| crate::entity::translation::Model {
            key: crate::hitomi::squash(japanese),
            gallery_id: 0,
            position: 0,
            url: String::new(),
            english: english.into(),
            japanese: japanese.into(),
            english_key: crate::hitomi::squash(english),
            japanese_key: crate::hitomi::squash(japanese),
            exact: true,
            source: source.into(),
        };

        store
            .save_crawled("Good", 1, &[work("/1", "kanojo plain"), work("/2", "kanojo | 彼女です"), work("/3", "彼女A kanojo"), work("/4", "彼女B kanojo"), work("/5", "彼女C kanojo")], None)
            .await
            .unwrap();
        store.save_translations("k3", "hitomi", "彼女A kanojo", "ja", 1, &[row("Her A", "彼女A kanojo", "hitomi")]).await.unwrap();
        store.save_translations("k4", "google", "彼女B kanojo", "ja", 1, &[row("Her B", "彼女B kanojo", "google")]).await.unwrap();

        let outcome = engine.search(&Request { live: Live::Off, ..Request::new("kanojo") }).await.unwrap();
        let english: Vec<_> = outcome.results.iter().map(|hit| (hit.path.as_str(), hit.title_english.as_ref().map(|t| t.text.as_str()), hit.title_english.as_ref().map(|t| t.source.as_str()))).collect();

        assert!(english.contains(&("/1", Some("kanojo plain"), Some("official"))), "{english:?}");
        assert!(english.contains(&("/2", Some("kanojo"), Some("official"))), "{english:?}");
        assert!(english.contains(&("/3", Some("Her A"), Some("official"))), "Hitomi.La's is the work's own title: {english:?}");
        assert!(english.contains(&("/4", Some("Her B"), Some("google"))), "{english:?}");
        assert!(english.contains(&("/5", None, None)), "nothing saved, nothing asked: {english:?}");
        // an object with the text and where it is from, `null` when there is none
        let of = |path: &str| serde_json::to_value(outcome.results.iter().find(|hit| hit.path == path).unwrap()).unwrap();
        assert_eq!(of("/4")["title_english"], serde_json::json!({ "text": "Her B", "source": "google" }));
        assert!(of("/5")["title_english"].is_null());
        assert!(of("/5").get("title_english_source").is_none());
    }
}
