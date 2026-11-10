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
use crate::sites::{self, is_challenge, Link, Site, Support};
use crate::store::{NewWork, SearchStore, SiteState};
use crate::utils::{unix_now, Result};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use ureq::ResponseExt;

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
#[serde(rename_all = "camelCase")]
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
    /// This request found the work on a site and saved it: it was not in the database before.
    pub is_new: bool,
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
#[serde(rename_all = "camelCase")]
pub struct SiteReport {
    pub website: String,
    /// `searched` (asked now), `cached` (asked less than `ttl_hours` ago, so the database answers), `failed` (a query
    /// failed), or `not searched` (the request said not to ask the sites).
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
#[serde(rename_all = "camelCase")]
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
    pub results: Vec<Hit>,
    pub sites: Vec<SiteReport>,
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
    On,
}

impl Live {
    /// `off`, `diff` and `on`; `0`, `false`, `no` are off, `1`, `true`, `yes` are on. `None` for anything else.
    pub fn parse(text: &str) -> Option<Live> {
        match text.trim().to_ascii_lowercase().as_str() {
            "off" | "0" | "false" | "no" => Some(Live::Off),
            "diff" | "" => Some(Live::Diff),
            "on" | "1" | "true" | "yes" => Some(Live::On),
            _ => None,
        }
    }
}

/// A request to the engine.
#[derive(Debug, Clone)]
pub struct Request<'a> {
    /// The words the titles have to have. This is also what the sites are asked.
    pub query: &'a str,
    /// Which sites are asked. The works that are new are saved, and the answer is from the database, which has them
    /// and what was saved before.
    pub live: Live,
    /// The most works in the answer.
    pub limit: usize,
}

#[cfg(test)]
impl<'a> Request<'a> {
    pub fn new(query: &'a str) -> Self {
        Self { query, live: Live::Diff, limit: 100 }
    }
}

/// A site as `GET /okazuri/sites` lists it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
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
    /// The place of a searched site in the list, to put the works in the order of the sites.
    order: HashMap<&'static str, usize>,
    /// When the site was asked last, to keep `min_delay_ms` between two requests.
    gates: Vec<Mutex<Option<Instant>>>,
    store: Arc<SearchStore>,
    agent: ureq::Agent,
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
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(config.timeout_secs.max(1))))
            // a 403 or a 500 is an answer to be told apart, not an error of the client
            .http_status_as_error(false)
            .user_agent(USER_AGENT)
            .build()
            .into();

        Arc::new(Self {
            gates: sites.iter().map(|_| Mutex::new(None)).collect(),
            sites,
            active,
            order,
            store,
            agent,
            config: config.clone(),
            permits: Semaphore::new(config.concurrency.max(1)),
            romanizer,
        })
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

    /// The works of a list page with the romaji of their titles, which is what a query is matched with. A title
    /// that cannot be converted is its own romaji.
    fn with_romaji(&self, links: Vec<Link>) -> Vec<NewWork> {
        links
            .into_iter()
            .map(|link| {
                let converted = romaji(&link.title, &self.romanizer).unwrap_or_else(|_| link.title.clone());

                NewWork { path: link.path, title: link.title, romaji: converted }
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
    async fn due(&self, plan: &Plan, slow_too: bool, refresh: bool) -> Result<Vec<(usize, Vec<String>)>> {
        let now = unix_now();
        let mut states: HashMap<String, HashMap<String, SiteState>> = HashMap::new();
        let mut due = Vec::new();

        for &index in &self.active {
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
    pub async fn is_fresh(&self, query: &str) -> Result<bool> {
        let plan = self.plan(query)?;

        Ok(self.due(&plan, false, false).await?.is_empty())
    }

    /// Asks `query` to the sites whose answer is missing or older than `ttl_hours` (a failure: `retry_failed_minutes`),
    /// and saves the works that are new. A request asks the slow sites too (`slow_too`), the prefetch does not.
    pub async fn fetch(self: &Arc<Self>, query: &str, slow_too: bool) -> Result<Fetched> {
        self.fetch_with(query, slow_too, false).await
    }

    /// Like `fetch`; with `refresh` every site is asked, whether its answer is up to date or not.
    pub async fn fetch_with(self: &Arc<Self>, query: &str, slow_too: bool, refresh: bool) -> Result<Fetched> {
        let plan = self.plan(query)?;

        if plan.query.is_empty() {
            return Err("the query is empty".into());
        }

        let mut fetched = Fetched::default();
        let mut tasks = tokio::task::JoinSet::new();

        for (index, texts) in self.due(&plan, slow_too, refresh).await? {
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
    pub async fn search(self: &Arc<Self>, request: &Request<'_>) -> Result<Outcome> {
        let plan = self.plan(request.query)?;
        let terms = Terms::new(&plan.query);

        if terms.is_empty() {
            return Err("the query is empty".into());
        }

        let fetched = match request.live {
            Live::Off => Fetched::default(),
            Live::Diff => self.fetch_with(request.query, true, false).await?,
            Live::On => self.fetch_with(request.query, true, true).await?,
        };

        let mut hits: Vec<Hit> = Vec::new();
        for work in self.store.candidates(terms.prefilter()).await? {
            // a site that is left out does not answer, whatever was saved of it
            let Some(&index) = self.active.iter().find(|&&i| self.sites[i].def.name == work.website) else { continue };

            // the name has to be in the title outside its parentheses ("work (COMIC kairakuten 2018 8)" is a work of the issue)
            if terms.matches_title(&work.title_romaji) {
                let site = &self.sites[index];
                let is_new = fetched.new.contains(&(work.website.clone(), work.path.clone()));

                hits.push(Hit {
                    website: work.website,
                    is_legal: site.def.legal,
                    url: site.address(&work.path),
                    path: work.path,
                    title: work.title,
                    title_romaji: work.title_romaji,
                    is_new,
                });
            }
        }

        // the sites in their order; in a site, the works seen last come first (the order of the database)
        hits.sort_by_key(|hit| self.order.get(hit.website.as_str()).copied().unwrap_or(usize::MAX));

        let total = hits.len();
        hits.truncate(request.limit.max(1));

        Ok(Outcome {
            sites: self.reports(&plan, &fetched.asked, request.live != Live::Off).await?,
            input: plan.input,
            terms: terms.keys().into_iter().map(str::to_string).collect(),
            total,
            results: hits,
            query: plan.query,
        })
    }

    async fn reports(&self, plan: &Plan, asked: &HashSet<usize>, may_ask: bool) -> Result<Vec<SiteReport>> {
        let mut states: HashMap<String, HashMap<String, SiteState>> = HashMap::new();
        let mut reports = Vec::new();

        for &index in &self.active {
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

            let worker = self.clone();
            let text = query.clone();
            let outcome = tokio::task::spawn_blocking(move || worker.request(index, &text).map(|links| worker.with_romaji(links)))
                .await
                .unwrap_or_else(|e| Err(format!("the request task died: {e}")));

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

    /// The request to a site (blocking). Waits for its turn first.
    fn request(&self, index: usize, query: &str) -> std::result::Result<Vec<Link>, String> {
        let site = &self.sites[index];
        let url = site.search_url(query).ok_or_else(|| "the site has no search".to_string())?;

        let mut asked_again = false;
        let (status, page_url, body) = loop {
            self.wait_turn(index);

            let mut request = self.agent.get(&url).header("Accept-Language", "ja,en;q=0.8").header(
                "Accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            );
            if let Some(cookie) = site.def.cookie {
                request = request.header("Cookie", cookie);
            }

            let mut response = request.call().map_err(|e| e.to_string())?;
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
                std::thread::sleep(Duration::from_secs(wait));
                continue;
            }

            let page_url = response.get_uri().to_string();
            let bytes = response.body_mut().read_to_vec().map_err(|e| e.to_string())?;

            break (status, page_url, String::from_utf8_lossy(&bytes).into_owned());
        };

        if is_challenge(status, &body) {
            return Err("blocked by a Cloudflare challenge".to_string());
        }
        if status != 200 {
            return Err(format!("HTTP {status}"));
        }

        site.read(&page_url, &body, self.config.max_results_per_site)
    }

    /// Keeps `min_delay_ms` (or the longer delay of the site) between two requests to the same site.
    fn wait_turn(&self, index: usize) {
        let delay = Duration::from_millis(self.config.min_delay_ms.max(self.sites[index].def.delay_ms));
        let mut last = self.gates[index].lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(at) = *last {
            if let Some(wait) = delay.checked_sub(at.elapsed()) {
                std::thread::sleep(wait);
            }
        }
        *last = Some(Instant::now());
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
    async fn the_multiplication_sign_stays_only_for_the_sites_that_need_it() {
        let (port, _) = server().await;
        let work = format!(r"^http://127\.0\.0\.1:{port}/work/\d+");
        let search = format!("http://127.0.0.1:{port}/ok?q={{q}}");
        let times = SiteDef { keep_times: true, ..def("Times", work.clone(), search.clone(), true, true) };
        let sites = vec![Site::new(leak("Plain", work, search, true)), Site::new(Box::leak(Box::new(times)))];
        let store = Arc::new(SearchStore::in_memory().await.unwrap());
        let engine = Engine::with_sites(sites, &config(), store.clone(), crate::testing::romanizer_arc());

        let outcome = engine.search(&Request { query: "COMIC  E×E 11", live: Live::Diff, limit: 10 }).await.unwrap();

        assert_eq!(outcome.query, "COMIC ExE 11", "the answer is written with x");
        assert_eq!(store.states("COMIC ExE 11").await.unwrap().keys().collect::<Vec<_>>(), ["Plain"]);
        assert_eq!(store.states("COMIC E×E 11").await.unwrap().keys().collect::<Vec<_>>(), ["Times"]);
        let asked: Vec<_> = outcome.sites.iter().map(|site| (site.website.as_str(), site.queries.clone())).collect();
        assert_eq!(asked, [("Plain", vec!["COMIC ExE 11".to_string()]), ("Times", vec!["COMIC E×E 11".to_string()])]);
        assert_eq!(outcome.total, 4, "both sites answer, and the sign does not matter to the match");

        // the bot writes the sign as x already: between capitals it is put back for the site
        let outcome = engine.search(&Request { query: "COMIC ExE 12", live: Live::Diff, limit: 10 }).await.unwrap();
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

        // live=on asks the sites whose answer is up to date too
        let all = engine.search(&Request { live: Live::On, ..Request::new("q") }).await.unwrap();
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
    async fn the_works_that_were_saved_before_are_answered_with_the_new_ones() {
        let (engine, _, store) = setup(&config()).await;

        // an earlier search saved a work of the same words that this request will not find again
        store.save("old", "Good", 1, &Ok(vec![NewWork { path: "/work/77".into(), title: "q earlier".into(), romaji: "q earlier".into() }]), false).await.unwrap();

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
    async fn titles_of_different_pages_are_all_searched() {
        let (engine, _, store) = setup(&config()).await;

        // pages that are not the same page, as different queries find them
        store.save("a", "Good", 1, &Ok(vec![NewWork { path: "/work/10".into(), title: "alpha beta one".into(), romaji: "alpha beta one".into() }]), false).await.unwrap();
        store.save("b", "Good", 2, &Ok(vec![NewWork { path: "/work/11".into(), title: "gamma one".into(), romaji: "gamma one".into() }]), false).await.unwrap();

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

        let saved = store.candidates(None).await.unwrap();
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

        let work = |path: &str, title: &str| NewWork { path: path.into(), title: title.into(), romaji: title.to_lowercase() };
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
        assert!(!info.iter().find(|s| s.name == "Hitomi.La").unwrap().active);

        let trying = Engine::new(
            &SearchConfig { try_unsupported: true, ..config() },
            Arc::new(SearchStore::in_memory().await.unwrap()),
            crate::testing::romanizer_arc(),
        );
        assert!(trying.sites_info().iter().find(|s| s.name == "IMHentai").unwrap().active);
        assert!(!trying.sites_info().iter().find(|s| s.name == "NijiGazo").unwrap().active, "no search URL");
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
    async fn the_works_of_a_site_that_is_left_out_are_not_in_the_answer() {
        let (engine, _, store) = setup(&SearchConfig { disabled_sites: vec!["good".into()], ..config() }).await;
        store.save("q", "Good", 1, &Ok(vec![NewWork { path: "/work/1".into(), title: "saved title".into(), romaji: "saved title".into() }]), false).await.unwrap();

        let outcome = engine.search(&Request { live: Live::Off, ..Request::new("saved") }).await.unwrap();

        assert_eq!(outcome.total, 0);
        assert_eq!(store.counts().await.unwrap().1, 1, "the work is still saved");
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
}
