//! Reading every work of the sites that list them all, in the background of the server (`[crawl]`).
//!
//! A search only knows the works that a query listed. This walks the lists the sites have for their newest works (a
//! page after a page, or an index file) and saves the title of every work, so that a query finds a work before anyone asked
//! for it. The search of every site stays: a work that is newer than the last walk, and a site that has no list, are found
//! by it.
//!
//! The walk of a site has two parts, and its state is saved (`crawls`), so a restart goes on where it stopped:
//! - **the first walk** goes from the newest work to the oldest, a page after a page, and is done when the list ends;
//! - **the next walks** (at the `schedule` of `[crawl]`) start from the newest page again, and stop at the first page that has
//!   nothing new in it, so they are a few requests.
//!
//! Every request waits for the turn of its site (`min_delay_ms`, the delay of the site), the same turn the searches wait for, so
//! the crawl and the searches together never ask a site more often than that. The sites are walked at the same time.
//! `shard = "k/N"` splits the first walk between N machines (page n is read by the machine whose k is the rest of n / N).

use crate::config::CrawlConfig;
use crate::cron::{parse_utc_offset, Minute, Schedule};
use crate::engine::Engine;
use crate::sites::{decode, Link, Site};
use crate::utils::Result;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;

/// How a site lists all of its works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// The index file `index-all.nozomi` of Hitomi.La (the ids, the newest first), and the file of each gallery for its title.
    Hitomi,
    /// WordPress: `{api}`, 100 posts a page, the newest first. `{n}` is the number of the page.
    WordPress(String),
    /// A page of a list: the address with `{n}` for the number of the page (from 1), read the way a page of search results is.
    Pages(String),
    /// A JSON answer with the cards of the works and the address of the next page in `next` (RokuHentai).
    Cards(String),
    /// E-Hentai: the list page, and `?next=` the id of the last gallery of the page before.
    GalleryIds(String),
}

/// The lists of the sites (checked in 2026-11). A site that is not here has no list that can be read: it is only searched.
pub fn sources() -> Vec<(&'static str, Source)> {
    let pages = |template: &str| Source::Pages(template.to_string());

    vec![
        ("Hitomi.La", Source::Hitomi),
        ("momon:GA", Source::WordPress("https://momon-ga.com/wp-json/wp/v2/posts?per_page=100&page={n}&orderby=date&order=desc&_fields=id,link,title".to_string())),
        ("Nya:Hentai", Source::WordPress("https://nyahentai.one/wp-json/wp/v2/posts?per_page=100&page={n}&orderby=date&order=desc&_fields=id,link,title".to_string())),
        ("DLsite", pages("https://www.dlsite.com/books/fsr/=/language/jp/sex_category[0]/male/ana_flg/all/order/release_d/per_page/100/page/{n}/")),
        ("Wnacg", pages("https://www.wnacg.com/albums-index-page-{n}.html")),
        ("AsmHentai", pages("https://asmhentai.com/?page={n}")),
        ("HentaiEra", pages("https://hentaiera.com/?page={n}")),
        ("HentaiEnvy", pages("https://hentaienvy.com/?page={n}")),
        ("RokuHentai", Source::Cards("https://rokuhentai.com/_search?sort=recent".to_string())),
        ("EHentai", Source::GalleryIds("https://e-hentai.org/".to_string())),
    ]
}

/// The most links one page of a list is read for.
const MOST_LINKS: usize = 1000;

/// One page of a list: its works, and where the next page is (None: the list ended).
#[derive(Debug, PartialEq, Eq)]
struct Step {
    links: Vec<Link>,
    next: Option<String>,
}

/// `k/N` of `[crawl] shard`.
pub fn parse_shard(text: &str) -> Result<(u64, u64)> {
    let error = || format!("[crawl] shard must look like 0/1 or 2/4 (k/N with k < N), got {text:?}");
    let (k, n) = text.trim().split_once('/').ok_or_else(error)?;
    let (k, n): (u64, u64) = (k.trim().parse().map_err(|_| error())?, n.trim().parse().map_err(|_| error())?);

    if n == 0 || k >= n {
        return Err(error().into());
    }

    Ok((k, n))
}

/// What one walk of a site did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Walk {
    pub pages: usize,
    pub new: usize,
    /// The first walk reached the end of the list.
    pub finished: bool,
}

pub struct Crawler {
    engine: Arc<Engine>,
    config: CrawlConfig,
    sources: Vec<(usize, Source)>,
    shard: (u64, u64),
    schedule: Schedule,
    offset_seconds: i64,
}

impl Crawler {
    /// Checks `[crawl]` and finds its sites. The sites that the engine does not have are an error, so a typo shows at once.
    pub fn new(engine: Arc<Engine>, config: CrawlConfig) -> Result<Self> {
        let named: Vec<(&str, Source)> = sources();

        Self::with_sources(engine, config, named.into_iter().map(|(name, source)| (name.to_string(), source)).collect())
    }

    pub fn with_sources(engine: Arc<Engine>, config: CrawlConfig, sources: Vec<(String, Source)>) -> Result<Self> {
        let shard = parse_shard(&config.shard)?;
        let schedule = Schedule::parse(&config.schedule).map_err(|e| format!("[crawl] schedule: {e}"))?;
        let offset_seconds = parse_utc_offset(&config.utc_offset).map_err(|e| format!("[crawl] {e}"))?;

        for name in &config.sites {
            if !sources.iter().any(|(known, _)| known.eq_ignore_ascii_case(name)) {
                return Err(format!("[crawl] sites: {name:?} has no list that can be read ({})", sources.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", ")).into());
            }
        }

        let wanted: HashSet<String> = config.sites.iter().map(|name| name.to_lowercase()).collect();
        let mut chosen = Vec::new();

        for (name, source) in sources {
            if !wanted.is_empty() && !wanted.contains(&name.to_lowercase()) {
                continue;
            }
            match engine.site_named(&name) {
                Some(index) => chosen.push((index, source)),
                None => log::warn!("crawl: {name} is not one of the sites of this program, left out"),
            }
        }

        Ok(Self { engine, config, sources: chosen, shard, schedule, offset_seconds })
    }

    /// Walks every site, each in a task of its own, for as long as the server runs.
    pub async fn run(self: Arc<Self>) {
        let mut tasks = JoinSet::new();

        log::info!("crawl: {} sites, shard {}/{}", self.sources.len(), self.shard.0, self.shard.1);

        for place in 0..self.sources.len() {
            tasks.spawn(self.clone().run_site(place));
        }
        while tasks.join_next().await.is_some() {}
    }

    async fn run_site(self: Arc<Self>, place: usize) {
        let (index, source) = &self.sources[place];
        let name = self.engine.site(*index).def.name;

        // a list that is read with the cursor of the page before cannot be shared: the first machine reads it
        if matches!(source, Source::Cards(_) | Source::GalleryIds(_)) && self.shard.0 != 0 {
            log::info!("crawl {name}: the pages follow one another, so shard 0 reads this site");
            return;
        }

        let retry = Duration::from_secs(self.config.retry_minutes.max(1) * 60);

        loop {
            let done = match self.engine.store().crawl_state(name).await {
                Ok(state) => state.is_some_and(|row| row.done),
                Err(e) => {
                    log::warn!("crawl {name}: {e}");
                    tokio::time::sleep(retry).await;
                    continue;
                }
            };

            if done {
                self.wait_for_schedule().await;
            }

            match self.walk(*index, source).await {
                Ok(walk) => {
                    if walk.finished {
                        log::info!("crawl {name}: the list is read to its end ({} new works in the last walk)", walk.new);
                    } else if done {
                        log::info!("crawl {name}: {} pages read again, {} new works", walk.pages, walk.new);
                    }
                }
                Err(e) => {
                    log::warn!("crawl {name}: {e}, again in {} minutes", retry.as_secs() / 60);
                    tokio::time::sleep(retry).await;
                }
            }
        }
    }

    /// Waits for the next minute that the schedule names.
    async fn wait_for_schedule(&self) {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;

            if self.schedule.matches(&Minute::from_unix(unix_now(), self.offset_seconds)) {
                return;
            }
        }
    }

    /// One walk of a site: the rest of the first walk when it is not done, the newest pages again when it is. Stops at the first
    /// error (what was saved stays saved, and the cursor with it).
    pub async fn walk(&self, index: usize, source: &Source) -> std::result::Result<Walk, String> {
        let site = self.engine.site(index);
        let name = site.def.name;
        let state = self.engine.store().crawl_state(name).await.map_err(|e| e.to_string())?;
        let first = !state.as_ref().is_some_and(|row| row.done);
        let (shard, shards) = if first { self.shard } else { (0, 1) };
        let mut cursor = if first { state.map(|row| row.cursor).unwrap_or_default() } else { String::new() };
        let mut walk = Walk::default();

        loop {
            let step = self.step(index, source, &cursor, (shard, shards)).await?;
            let next = step.next.clone();
            let listed = step.links.len();
            let works = self.engine.works_of(step.links).await?;
            // the first walk goes on to the next page, whatever this one had; the next walks do not go past a page of known works
            let finished = next.is_none();
            let position = if first { Some((next.as_deref().unwrap_or(""), finished)) } else { None };
            let new = self.engine.store().save_crawled(name, unix_now(), &works, position).await.map_err(|e| e.to_string())?;

            walk.pages += 1;
            walk.new += new;
            log::debug!("crawl {name}: {listed} works on the page after {cursor:?}, {new} new");
            if first && walk.pages % 50 == 0 {
                log::info!("crawl {name}: {} pages read, {} new works, at {:?}", walk.pages, walk.new, next.as_deref().unwrap_or("the end"));
            }

            match next {
                None => {
                    walk.finished = first;
                    return Ok(walk);
                }
                Some(next) => cursor = next,
            }
            if !first && (new == 0 || walk.pages >= self.config.refresh_pages.max(1)) {
                return Ok(walk);
            }
        }
    }

    /// The page of the list at `cursor` (empty: the first), and where the next one is.
    async fn step(&self, index: usize, source: &Source, cursor: &str, shard: (u64, u64)) -> std::result::Result<Step, String> {
        let site = self.engine.site(index);

        match source {
            Source::Hitomi => self.hitomi_step(index, cursor, shard).await,
            Source::WordPress(template) => {
                let number = page_number(cursor, shard);
                let url = template.replace("{n}", &number.to_string());
                // a page beyond the last one is a 400 of WordPress: the end of the list
                let body = match self.engine.get_page(index, &url).await {
                    Ok((_, body)) => body,
                    Err(e) if e == "HTTP 400" => return Ok(Step { links: Vec::new(), next: None }),
                    Err(e) => return Err(e),
                };

                let links = wordpress_links(site, &body)?;
                let next = (!links.is_empty()).then(|| (number + shard.1).to_string());

                Ok(Step { links, next })
            }
            Source::Pages(template) => {
                let number = page_number(cursor, shard);
                let (page_url, body) = self.engine.get_page(index, &template.replace("{n}", &number.to_string())).await?;
                let links = site.read(&page_url, &body, MOST_LINKS)?;
                let next = (!links.is_empty()).then(|| (number + shard.1).to_string());

                Ok(Step { links, next })
            }
            Source::Cards(first) => {
                let url = if cursor.is_empty() { first.as_str() } else { cursor };
                let (page_url, body) = self.engine.get_page(index, url).await?;
                let links = site.read(&page_url, &body, MOST_LINKS)?;
                let next = serde_json::from_str::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|json| json.get("next")?.as_str().map(str::to_string))
                    .filter(|next| !links.is_empty() && !next.is_empty());

                Ok(Step { links, next })
            }
            Source::GalleryIds(first) => {
                let url = if cursor.is_empty() { first.clone() } else { format!("{first}?next={cursor}") };
                let (page_url, body) = self.engine.get_page(index, &url).await?;
                let links = site.read(&page_url, &body, MOST_LINKS)?;
                let next = links.iter().filter_map(|link| gallery_id(&link.path)).min().map(|id| id.to_string());

                Ok(Step { links, next })
            }
        }
    }

    /// A batch of the galleries of Hitomi.La: the ids at the cursor (an offset in the index, in ids), and the title of each id that is not saved.
    async fn hitomi_step(&self, index: usize, cursor: &str, shard: (u64, u64)) -> std::result::Result<Step, String> {
        use crate::hitomi::{parse_gallery, Fetch};

        let batch = self.config.hitomi_batch.max(1) as u64;
        // the cursor is the offset of the next batch of this machine; the first one is the k-th batch of the index
        let offset = if cursor.is_empty() { shard.0 * batch } else { cursor.parse::<u64>().map_err(|_| format!("the cursor {cursor:?} is not an offset"))? };
        let mut fetch = self.engine.hitomi(index);
        let path = format!("/{}", self.config.hitomi_index.trim_start_matches('/'));

        let bytes = match fetch.get(&path, Some((offset * 4, (offset + batch) * 4 - 1))).await {
            Ok(bytes) => bytes,
            // an offset beyond the end of the file is a 416
            Err(e) if e.contains("416") => Vec::new(),
            Err(e) => return Err(e),
        };
        let ids: Vec<u32> = bytes.chunks_exact(4).map(|id| u32::from_be_bytes([id[0], id[1], id[2], id[3]])).collect();

        if ids.is_empty() {
            return Ok(Step { links: Vec::new(), next: None });
        }

        let paths: Vec<String> = ids.iter().map(|id| format!("/galleries/{id}.html")).collect();
        let known = self.engine.store().known_paths(self.engine.site(index).def.name, &paths).await.map_err(|e| e.to_string())?;
        let mut links = Vec::new();
        let mut failed = None;

        for (id, path) in ids.iter().zip(&paths) {
            if known.contains(path) {
                continue;
            }
            match fetch.get(&format!("/galleries/{id}.js"), None).await {
                Ok(body) => links.extend(parse_gallery(*id, &String::from_utf8_lossy(&body)).map(|gallery| gallery.link())),
                // a gallery whose file is gone (HTTP 404) is not an error: it is left out
                Err(e) if e.contains("404") => {}
                Err(e) => failed = Some(e),
            }
        }
        if let Some(error) = failed.filter(|_| links.is_empty() && known.len() < ids.len()) {
            return Err(error);
        }

        let more = bytes.len() as u64 >= batch * 4;

        Ok(Step { links, next: more.then(|| (offset + batch * shard.1).to_string()) })
    }
}

/// The number of the page to read: the first page of this machine, or the one the cursor names.
fn page_number(cursor: &str, shard: (u64, u64)) -> u64 {
    cursor.parse().unwrap_or(shard.0 + 1)
}

/// The id in `/g/3000000/abcdef0123/`.
fn gallery_id(path: &str) -> Option<u64> {
    path.strip_prefix("/g/")?.split('/').next()?.parse().ok()
}

/// The posts of a page of the WordPress API.
fn wordpress_links(site: &Site, body: &str) -> std::result::Result<Vec<Link>, String> {
    let posts: Vec<serde_json::Value> = serde_json::from_str(body).map_err(|e| format!("not the answer of WordPress: {e}"))?;

    Ok(posts
        .iter()
        .filter_map(|post| {
            let path = site.path_of(post.get("link")?.as_str()?)?;
            let title = decode(post.get("title")?.get("rendered")?.as_str()?).trim().to_string();

            (!title.is_empty()).then_some(Link { path, title })
        })
        .collect())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SearchConfig;
    use crate::sites::{Clean, Format, SiteDef, Support};
    use crate::store::SearchStore;
    use axum::extract::Path as UrlPath;
    use axum::routing::get;
    use axum::Router;

    fn def(name: &'static str, port: u16) -> &'static SiteDef {
        Box::leak(Box::new(SiteDef {
            name,
            legal: false,
            origin: "http://127.0.0.1",
            keep_query: &[],
            pattern: Box::leak(format!(r"^http://127\.0\.0\.1:{port}/work/\d+").into_boxed_str()),
            filter: None,
            search: Some("http://127.0.0.1/?q={q}"),
            cookie: None,
            clean: Clean::Plain,
            format: Format::Html,
            delay_ms: 0,
            japanese: true,
            keep_times: false,
            support: Support::Ready,
            note: "",
        }))
    }

    /// Three pages of two works, then an empty page; the newest works are on page 1. `moved` adds a work to the top of page 1.
    async fn server(works_per_page: usize, pages: usize) -> (u16, Arc<std::sync::atomic::AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = hits.clone();
        let router = Router::new()
            .route(
                "/list/{n}",
                get(move |UrlPath(n): UrlPath<usize>| {
                    let counted = counted.clone();
                    async move {
                        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let links: String = if n <= pages {
                            (0..works_per_page)
                                .map(|i| format!("<a href=\"http://127.0.0.1:{port}/work/{}\">Work number {}</a>", n * 100 + i, n * 100 + i))
                                .collect()
                        } else {
                            String::new()
                        };
                        format!("<html><body>{links}</body></html>")
                    }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        (port, hits)
    }

    async fn crawler(port: u16, config: CrawlConfig) -> (Arc<Crawler>, Arc<SearchStore>) {
        let store = Arc::new(SearchStore::in_memory().await.unwrap());
        let search = SearchConfig { min_delay_ms: 0, ..SearchConfig::default() };
        let engine = Engine::with_sites(vec![Site::new(def("Listed", port))], &search, store.clone(), crate::testing::romanizer_arc());
        let source = Source::Pages(format!("http://127.0.0.1:{port}/list/{{n}}"));

        (Arc::new(Crawler::with_sources(engine, config, vec![("Listed".to_string(), source)]).unwrap()), store)
    }

    #[test]
    fn a_shard_is_k_of_n() {
        assert_eq!(parse_shard("0/1").unwrap(), (0, 1));
        assert_eq!(parse_shard(" 2 / 4 ").unwrap(), (2, 4));
        assert!(parse_shard("4/4").is_err());
        assert!(parse_shard("1/0").is_err());
        assert!(parse_shard("one").is_err());
    }

    #[test]
    fn the_id_of_a_gallery_is_in_its_path() {
        assert_eq!(gallery_id("/g/3000000/abcdef0123/"), Some(3_000_000));
        assert_eq!(gallery_id("/s/abc/1-2"), None);
    }

    #[tokio::test]
    async fn the_first_walk_reads_the_list_to_its_end_and_the_next_one_stops_at_a_known_page() {
        let (port, hits) = server(2, 3).await;
        let (crawler, store) = crawler(port, CrawlConfig::default()).await;
        let (index, source) = crawler.sources[0].clone();

        let first = crawler.walk(index, &source).await.unwrap();
        assert_eq!(first, Walk { pages: 4, new: 6, finished: true }, "three pages of works and the empty page that ends the list");
        assert_eq!(store.counts().await.unwrap().1, 6);
        assert!(store.crawl_state("Listed").await.unwrap().unwrap().done);
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 4);

        // the list is done: a walk reads page 1, finds nothing new, and stops
        let next = crawler.walk(index, &source).await.unwrap();
        assert_eq!(next, Walk { pages: 1, new: 0, finished: false });
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn a_walk_that_stops_goes_on_where_it_stopped() {
        let (port, hits) = server(2, 3).await;
        let (crawler, store) = crawler(port, CrawlConfig::default()).await;
        let (index, source) = crawler.sources[0].clone();

        // what the first walk had saved before the server stopped: page 1 is read, the cursor is at page 2
        let page = crawler.step(index, &source, "", (0, 1)).await.unwrap();
        let works = crawler.engine.works_of(page.links).await.unwrap();
        store.save_crawled("Listed", 1, &works, Some((page.next.as_deref().unwrap(), false))).await.unwrap();
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);

        let rest = crawler.walk(index, &source).await.unwrap();
        assert_eq!(rest, Walk { pages: 3, new: 4, finished: true });
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 4, "page 1 was not read again");
        assert_eq!(store.counts().await.unwrap().1, 6);
    }

    #[tokio::test]
    async fn after_a_pause_the_next_walk_goes_back_to_the_first_known_page() {
        let (port, hits) = server(2, 5).await;
        let (crawler, store) = crawler(port, CrawlConfig::default()).await;
        let (index, source) = crawler.sources[0].clone();

        // the list was read to its end a while ago, when it was only page 4 and 5 (the works of pages 1 to 3 came after)
        for n in [4, 5] {
            let page = crawler.step(index, &source, &n.to_string(), (0, 1)).await.unwrap();
            let works = crawler.engine.works_of(page.links).await.unwrap();
            store.save_crawled("Listed", 1, &works, Some(("", true))).await.unwrap();
        }
        hits.store(0, std::sync::atomic::Ordering::SeqCst);

        let walk = crawler.walk(index, &source).await.unwrap();
        assert_eq!(walk, Walk { pages: 4, new: 6, finished: false }, "pages 1 to 3 are new, page 4 is known");
        assert_eq!(store.counts().await.unwrap().1, 10);

        // the safety limit stops it early, and the next walk goes on from the newest page again
        let limited = CrawlConfig { refresh_pages: 2, ..CrawlConfig::default() };
        let (crawler, store) = self::crawler(port, limited).await;
        let page = crawler.step(index, &source, "5", (0, 1)).await.unwrap();
        let works = crawler.engine.works_of(page.links).await.unwrap();
        store.save_crawled("Listed", 1, &works, Some(("", true))).await.unwrap();
        assert_eq!(crawler.walk(index, &source).await.unwrap().pages, 2);
    }

    #[tokio::test]
    async fn a_shard_reads_every_nth_page() {
        let (port, hits) = server(2, 4).await;
        let (crawler, store) = crawler(port, CrawlConfig { shard: "1/2".to_string(), ..CrawlConfig::default() }).await;
        let (index, source) = crawler.sources[0].clone();

        let walk = crawler.walk(index, &source).await.unwrap();

        // pages 2 and 4, then page 6 is empty
        assert_eq!(walk, Walk { pages: 3, new: 4, finished: true });
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 3);
        assert_eq!(store.counts().await.unwrap().1, 4);
    }

    #[tokio::test]
    async fn the_works_a_crawl_saved_can_be_searched_by_the_index() {
        let (port, _) = server(2, 1).await;
        let (crawler, store) = crawler(port, CrawlConfig::default()).await;
        let (index, source) = crawler.sources[0].clone();

        crawler.walk(index, &source).await.unwrap();

        let found = store.candidates(Some("worknumber100")).await.unwrap();
        assert_eq!(found.iter().map(|work| work.path.as_str()).collect::<Vec<_>>(), ["/work/100"]);
    }

    #[tokio::test]
    async fn a_site_that_is_not_in_the_list_is_an_error() {
        let (port, _) = server(1, 1).await;
        let store = Arc::new(SearchStore::in_memory().await.unwrap());
        let engine = Engine::with_sites(vec![Site::new(def("Listed", port))], &SearchConfig::default(), store, crate::testing::romanizer_arc());
        let config = CrawlConfig { sites: vec!["Nowhere".to_string()], ..CrawlConfig::default() };

        let error = Crawler::with_sources(engine, config, vec![("Listed".to_string(), Source::Hitomi)]).err().unwrap();
        assert!(error.to_string().contains("Nowhere"), "{error}");
    }
}
