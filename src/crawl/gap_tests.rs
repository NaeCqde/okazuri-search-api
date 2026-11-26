//! The tests of the gap check (`gap.rs`): the rules on their own, then walks of a mock site whose list grows at the top.

use super::gap::*;
use super::*;
use crate::config::SearchConfig;
use crate::dist::{Done, Hub, Job};
use crate::sites::{Clean, Format, SiteDef, Support};
use crate::store::{Boundary, NewWork, PageRead, Sample, SearchStore};
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

// ---- the rules ----

fn read(page: u64, job: i64, lo_ms: i64, hi_ms: i64) -> PageRead {
    PageRead { page, job, lo_ms, hi_ms }
}

fn pair(k: u64, a_hi: i64, b_lo: i64) -> Boundary {
    Boundary { k, a_hi, b_lo }
}

/// The top was watched from 0 to 10 000, 20 works a page.
const SEEN: Seen = Seen { since0: 0, at: 10_000, per_page: 20 };

#[test]
fn in_order_boundaries_are_safe() {
    // the same job: the pages of a job are read in order
    assert!(!out_of_order(&read(1, 5, 100, 200), &read(2, 5, 50, 300)));
    // the later page began when the earlier one was done, or after
    assert!(!out_of_order(&read(1, 5, 100, 200), &read(2, 6, 200, 300)));
    assert!(!out_of_order(&read(1, 5, 100, 200), &read(2, 6, 500, 600)));
    // the later page may have been read before the earlier one was done
    assert!(out_of_order(&read(1, 5, 100, 200), &read(2, 6, 199, 600)));
}

#[test]
fn out_of_order_without_growth_is_safe() {
    assert_eq!(judge(&pair(4, 2000, 1000), Some(&SEEN), 0, (0, false), 10, 100), None);
}

#[test]
fn out_of_order_with_growth_is_dangerous() {
    // 3 works came after the later page was read: 3 places, so one page (half a page more) below page 4 is read again
    assert_eq!(judge(&pair(4, 2000, 1000), Some(&SEEN), 1, (3, false), 10, 100), Some((5, 5, 3)));
}

#[tokio::test]
async fn growth_outside_the_window_is_ignored() {
    let store = SearchStore::in_memory().await.unwrap();

    store.save_watch("A", 0, "t", 20, None).await.unwrap();
    // 4 works came between 500 and 600, and 1 between 8000 and 9000
    store.save_watch("A", 600, "t", 20, Some(Sample { since: 500, at: 600, added: 4, saturated: false })).await.unwrap();
    store.save_watch("A", 9000, "t", 20, Some(Sample { since: 8000, at: 9000, added: 1, saturated: false })).await.unwrap();

    // page 4 was read at 1000-2000 (page 5 at 1000): the works that came before and after that are not in the window
    let b = pair(4, 2000, 1000);
    let (window, _) = store.growth("A", b.b_lo, b.a_hi).await.unwrap();
    assert_eq!(window, 0);
    assert_eq!(judge(&b, Some(&SEEN), window, (0, false), 10, 100), None);
    // a window that has the first sample is dangerous
    let (window, _) = store.growth("A", 550, 700).await.unwrap();
    assert!(judge(&pair(4, 700, 550), Some(&SEEN), window, (5, false), 10, 100).is_some());
}

#[test]
fn a_window_before_the_baseline_is_uncovered_and_dangerous() {
    let seen = Seen { since0: 1000, at: 10_000, per_page: 20 };

    // page 5 may have been read at 900, before the top was watched: no growth is known, and the longest span is read
    assert_eq!(judge(&pair(4, 2000, 900), Some(&seen), 0, (0, false), 10, 100), Some((5, 14, 0)));
}

#[test]
fn a_window_after_the_last_poll_is_uncovered_and_dangerous() {
    assert_eq!(judge(&pair(4, 10_001, 9000), Some(&SEEN), 0, (0, false), 10, 100), Some((5, 14, 0)));
    // and no look at all is the same
    assert_eq!(judge(&pair(4, 2000, 1000), None, 0, (0, false), 10, 100), Some((5, 14, 0)));
}

#[test]
fn the_span_follows_the_growth_and_is_capped() {
    let span = |growth: i64, max: usize, top: u64| judge(&pair(10, 2000, 1000), Some(&SEEN), 1, (growth, false), max, top).map(|(lo, hi, _)| (lo, hi));

    // pages = ceil((growth + half a page) / 20), at least 1
    assert_eq!(span(1, 10, 100), Some((11, 11)));
    assert_eq!(span(10, 10, 100), Some((11, 11)));
    assert_eq!(span(11, 10, 100), Some((11, 12)));
    assert_eq!(span(45, 10, 100), Some((11, 13)));
    // the cap, and the last page of the list
    assert_eq!(span(1000, 10, 100), Some((11, 20)));
    assert_eq!(span(1000, 3, 100), Some((11, 13)));
    assert_eq!(span(1000, 10, 12), Some((11, 12)));
    // no page after the boundary
    assert_eq!(span(5, 10, 10), None);
}

#[test]
fn saturated_growth_gives_the_cap() {
    // more works came than the page shows: the number is a lower bound, so the longest span is read
    assert_eq!(judge(&pair(10, 2000, 1000), Some(&SEEN), 1, (5, true), 7, 100), Some((11, 17, 5)));
}

#[test]
fn a_job_without_offsets_gets_the_whole_job_window() {
    let reads = page_reads(5, 3, 1000, 2000, &[], 77);

    assert_eq!(reads, [read(5, 77, 1000, 2000), read(6, 77, 1000, 2000), read(7, 77, 1000, 2000)]);
    // offsets that are not one for each page are not used either
    assert_eq!(page_reads(5, 3, 1000, 2000, &[10, 20], 77), reads);
}

#[test]
fn offsets_give_page_windows_inside_the_job_window() {
    let reads = page_reads(1, 3, 1000, 2000, &[100, 300, 600], 9);

    assert_eq!(reads, [read(1, 9, 1000, 1500), read(2, 9, 1100, 1700), read(3, 9, 1300, 2000)]);
    assert!(reads.iter().all(|r| r.lo_ms >= 1000 && r.hi_ms <= 2000 && r.lo_ms <= r.hi_ms));
    // the clocks of the host seen twice may disagree a little: the window is turned the right way
    assert!(page_reads(1, 1, 2000, 1990, &[50], 9).iter().all(|r| r.lo_ms <= r.hi_ms));
    // every job has a number of its own
    assert!(next_ms() < next_ms());
}

#[test]
fn spans_merge_and_the_budget_drops_the_smallest_growth() {
    let raw = vec![(3, 4, 1), (5, 6, 9), (10, 12, 5), (20, 21, 2), (11, 13, 1)];

    // 3-4 and 5-6 touch, 10-12 and 11-13 overlap
    assert_eq!(plan_spans(raw.clone(), 100), (vec![(3, 6), (10, 13), (20, 21)], 0));
    // 4 + 4 pages fit in 8, and the one with the least growth is left out
    assert_eq!(plan_spans(raw.clone(), 8), (vec![(3, 6), (10, 13)], 1));
    // 4 pages are the most growth, then the next one does not fit, and the small one does
    assert_eq!(plan_spans(raw, 6), (vec![(3, 6), (20, 21)], 1));
    assert_eq!(plan_spans(Vec::new(), 10), (Vec::new(), 0));
}

#[test]
fn the_growth_of_the_top_is_counted_from_page_one() {
    let top = "/work/11\n/work/10\n/work/9";
    let new = |ids: &[u32]| -> Vec<String> { ids.iter().map(|id| format!("/work/{id}")).collect() };

    assert_eq!(growth_of(top, "", &new(&[11, 10, 9]), false), (0, false));
    assert_eq!(growth_of(top, "", &new(&[12, 11, 10]), false), (1, false));
    // nothing of the page before is on the page: more came than it shows
    assert_eq!(growth_of(top, "", &new(&[15, 14, 13]), false), (3, true));
    // Hitomi.La: the index is longer by 4 bytes for each gallery
    assert_eq!(growth_of("1000", "1016", &[], true), (4, false));
    assert_eq!(growth_of("1000", "1000", &[], true), (0, false));
    assert_eq!(growth_of("1000", "996", &[], true), (0, false), "an index that is shorter is not growth");
}

#[test]
fn the_hitomi_watch_uses_the_index_length() {
    use crate::engine::size_in_content_range;

    // `Content-Range` of the read of the first 4 bytes of the index
    assert_eq!(size_in_content_range("bytes 0-3/123456"), Some(123_456));
    assert_eq!(size_in_content_range("bytes 0-3/*"), None);
    assert_eq!(size_in_content_range("garbage"), None);

    struct Index(u64);
    impl crate::hitomi::Fetch for Index {
        async fn get(&mut self, _: &str, _: Option<(u64, u64)>) -> std::result::Result<Vec<u8>, String> {
            Err("not needed".to_string())
        }
        async fn size(&mut self, _: &str) -> std::result::Result<u64, String> {
            Ok(self.0)
        }
    }
    struct NoSize;
    impl crate::hitomi::Fetch for NoSize {
        async fn get(&mut self, _: &str, _: Option<(u64, u64)>) -> std::result::Result<Vec<u8>, String> {
            Err("not needed".to_string())
        }
    }

    let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
    rt.block_on(async {
        use crate::hitomi::Fetch;
        let before = Index(4000).size("/index-all.nozomi").await.unwrap();
        let after = Index(4040).size("/index-all.nozomi").await.unwrap();

        // 10 galleries came to the top
        assert_eq!(growth_of(&before.to_string(), &after.to_string(), &[], true), (10, false));
        assert!(NoSize.size("/x").await.is_err());
    });
    assert!(gap_checked(&Source::Hitomi), "new galleries are at the start of the index (offset 0), so it moves like a list of pages");
}

#[test]
fn an_answer_without_offsets_is_refused_and_a_job_that_stopped_early_has_the_whole_window() {
    // `at` is part of the answer of a worker: there is no worker from before it to be handled
    assert!(serde_json::from_str::<Step>(r#"{"links":[],"next":null}"#).is_err());

    let done: Done = serde_json::from_str(r#"{"Step":{"Ok":{"links":[],"next":"5","at":[]}}}"#).unwrap();
    assert!(matches!(done, Done::Step(Ok(step)) if step.at.is_empty() && step.next.as_deref() == Some("5")));

    // a job that stopped on a repeated page has no offsets for the pages it did not read: they may have been read at any time of it
    assert_eq!(page_reads(1, 2, 10, 20, &[], 3), [read(1, 3, 10, 20), read(2, 3, 10, 20)]);
}

#[test]
fn only_the_lists_that_move_and_are_read_out_of_order_are_checked() {
    let pages = |template: &str| Source::Pages(template.to_string());

    assert!(gap_checked(&pages("https://wnacg/albums-index-page-{n}.html")));
    assert!(gap_checked(&Source::Hitomi));
    // the oldest first: the pages do not move
    assert!(!gap_checked(&Source::WordPress("https://x/wp-json/wp/v2/posts?page={n}&order=desc".to_string())));
    assert!(!gap_checked(&pages("https://www.dlsite.com/books/fsr/order/release_d/page/{n}/")));
    // read in order
    assert!(!gap_checked(&Source::Cards("https://x".to_string())));
    assert!(!gap_checked(&Source::GalleryIds("https://x".to_string())));
    assert!(!gap_checked(&Source::Lists(vec!["https://x/{n}".to_string()])));
}

// ---- a mock site whose list grows at the top ----

/// A list of `base + added` works, the newest (the biggest id) first, `per_page` on a page; `/list/{n}` is page n and is empty beyond the last.
struct Grow {
    port: u16,
    base: usize,
    per_page: usize,
    added: AtomicUsize,
    hits: Mutex<HashMap<usize, usize>>,
    /// The pages that answer 500.
    fail: Mutex<HashSet<usize>>,
    /// (page, milliseconds): every request of the page waits that long before the page is made.
    delay: Mutex<Option<(usize, u64)>>,
    /// (page, n): a work is added at the top when the page is asked for the n-th time (before the answer is made).
    bump: Mutex<Option<(usize, usize)>>,
}

impl Grow {
    fn hits(&self, page: usize) -> usize {
        self.hits.lock().unwrap().get(&page).copied().unwrap_or(0)
    }

    fn add(&self) {
        self.added.fetch_add(1, Ordering::SeqCst);
    }
}

async fn list(State(grow): State<Arc<Grow>>, UrlPath(n): UrlPath<usize>) -> impl IntoResponse {
    let nth = {
        let mut hits = grow.hits.lock().unwrap();
        let count = hits.entry(n).or_default();
        *count += 1;
        *count
    };
    if *grow.bump.lock().unwrap() == Some((n, nth)) {
        grow.add();
    }
    let delay = *grow.delay.lock().unwrap();
    if let Some((page, ms)) = delay.filter(|(page, _)| *page == n) {
        let _ = page;
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
    if grow.fail.lock().unwrap().contains(&n) {
        return (StatusCode::INTERNAL_SERVER_ERROR, String::new());
    }

    let total = grow.base + grow.added.load(Ordering::SeqCst);
    let links: String = (0..grow.per_page)
        .filter_map(|i| {
            let position = (n.max(1) - 1) * grow.per_page + i;
            (position < total).then(|| total - position)
        })
        .map(|id| format!("<a href=\"http://127.0.0.1:{}/work/{id}\">Work number {id}</a>", grow.port))
        .collect();

    (StatusCode::OK, format!("<html><body>{links}</body></html>"))
}

async fn growing(base: usize, per_page: usize) -> Arc<Grow> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let grow = Arc::new(Grow { port, base, per_page, added: AtomicUsize::new(0), hits: Default::default(), fail: Default::default(), delay: Default::default(), bump: Default::default() });
    let router = Router::new().route("/list/{n}", get(list)).with_state(grow.clone());

    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    grow
}

fn def(name: &'static str, port: u16) -> &'static SiteDef {
    def_at(name, "http://127.0.0.1", port)
}

fn def_at(name: &'static str, origin: &'static str, port: u16) -> &'static SiteDef {
    Box::leak(Box::new(SiteDef {
        name,
        legal: false,
        origin,
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
        plain_agent: false,
    }))
}

struct Rig {
    crawler: Arc<Crawler>,
    store: Arc<SearchStore>,
    /// The cursors of the jobs that the workers answer with an error (a job of the pages read again is `3` when it starts at page 3).
    fail_cursors: Arc<Mutex<HashSet<String>>>,
}

impl Rig {
    async fn walk(&self) -> std::result::Result<Walk, String> {
        let (index, source) = self.crawler.sources[0].clone();

        self.crawler.walk(index, &source).await
    }

    async fn watch(&self, force: bool) -> std::result::Result<(), String> {
        let (index, source) = self.crawler.sources[0].clone();

        self.crawler.watch_once(index, &source, force).await
    }

    async fn has(&self, id: usize) -> bool {
        !self.store.known_paths("Listed", &[format!("/work/{id}")]).await.unwrap().is_empty()
    }

    /// A job read by hand: the pages `first..first + pages`, saved as a job of the walk would (with the times of the reads when `timed`).
    async fn read_by_hand(&self, first: u64, pages: usize, timed: bool) {
        let taken = now_ms();
        let step = self.crawler.step_local("Listed", &first.to_string(), (0, 1), (None, None), pages, 0, false).await.unwrap();
        let answered = now_ms();
        let works = self.crawler.engine.works_of(step.links.clone()).await.unwrap();

        if timed {
            let reads = page_reads(first, pages as u64, taken, answered, &step.at, next_ms());
            self.store.save_crawled_range_read("Listed", 1, &works, first, first + pages as u64 - 1, &reads).await.unwrap();
        } else {
            self.store.save_crawled_range("Listed", 1, &works, first, first + pages as u64 - 1).await.unwrap();
        }
    }
}

/// A host of workers (two of them, as the host reads too) for the list of `grow`; `job_pages` pages a job, three jobs out at a time.
async fn rig(grow: &Arc<Grow>, config: CrawlConfig, job_pages: usize, store: Option<Arc<SearchStore>>) -> Rig {
    let store = match store {
        Some(store) => store,
        None => Arc::new(SearchStore::in_memory().await.unwrap()),
    };
    let search = SearchConfig { min_delay_ms: 0, ..SearchConfig::default() };
    let engine = Engine::with_sites(vec![Site::new(def("Listed", grow.port))], &search, store.clone(), crate::testing::romanizer_arc());
    let source = Source::Pages(format!("http://127.0.0.1:{}/list/{{n}}", grow.port));
    let hub = Arc::new(Hub::new("token".to_string()));
    let crawler = Arc::new(Crawler::with_sources(engine, config, vec![("Listed".to_string(), source)]).unwrap().with_hub(hub.clone(), 3, job_pages, 0));
    let fail_cursors: Arc<Mutex<HashSet<String>>> = Default::default();

    for worker in 0..2 {
        let (hub, crawler, fail) = (hub.clone(), crawler.clone(), fail_cursors.clone());
        tokio::spawn(async move {
            loop {
                match hub.take("Listed", "step", &format!("test-{worker}")) {
                    Some((id, Job::Step { cursor, shard, ids, pages, seconds, oldest })) => {
                        let done = if fail.lock().unwrap().contains(&cursor) {
                            Done::Step(Err("the worker failed".to_string()))
                        } else {
                            Done::Step(crawler.step_local("Listed", &cursor, shard, ids, pages, seconds, oldest).await)
                        };
                        hub.report(id, done);
                    }
                    _ => tokio::time::sleep(Duration::from_millis(5)).await,
                }
            }
        });
    }

    Rig { crawler, store, fail_cursors }
}

/// The walk that loses a work: a list of 11 works (6 pages of 2) with `grow` the work added at the top when `grow_between`. Pages 3 to 6 are read first (one job, by hand), then
/// the work is added, then the walk reads the rest (pages 1 and 2 after the shift). Work 8 was the second of page 2 and is the first of page 3 now: it is on no page that was read.
async fn lost_work(config: CrawlConfig, grow_between: bool) -> (Rig, Arc<Grow>, std::result::Result<Walk, String>) {
    let grow = growing(11, 2).await;
    let rig = rig(&grow, config.clone(), 2, None).await;

    if config.gap_check && config.watch_seconds > 0 {
        rig.watch(true).await.unwrap();
    }
    rig.read_by_hand(3, 4, config.gap_check).await;
    if grow_between {
        grow.add();
    }
    let walk = rig.walk().await;

    (rig, grow, walk)
}

#[tokio::test]
async fn a_lost_work_at_a_job_boundary_is_found_by_the_gap_pass() {
    // without the check: the bug. Work 8 is saved by no page, and the walk says it is done
    let (rig, _, walk) = lost_work(CrawlConfig { gap_check: false, ..CrawlConfig::default() }, true).await;
    assert!(walk.unwrap().finished);
    assert!(!rig.has(8).await, "work 8 moved from the end of page 2 to the start of page 3 between the two reads");
    assert_eq!(rig.store.count_works().await.unwrap(), 11);

    // with the check: the boundary of pages 2 and 3 is read again
    let (rig, grow, walk) = lost_work(CrawlConfig::default(), true).await;
    assert!(walk.unwrap().finished);
    assert!(rig.has(8).await, "work 8 is saved by the pages that were read again");
    assert_eq!(rig.store.count_works().await.unwrap(), 12, "the 11 works and the one that came to the top (page 1 was read after the shift)");
    assert!(rig.store.crawl_state("Listed").await.unwrap().unwrap().done);
    assert!(rig.store.gap_plan("Listed").await.unwrap().is_empty(), "what the walk kept is cleared when the list is read");
    // the extra requests are the span: page 3 (the growth is one work, so one page) and the looks at page 1 (the start and the end of the watch)
    assert_eq!(grow.hits(3), 2, "page 3 by hand and once more");
    for page in [2, 4, 5, 6] {
        assert_eq!(grow.hits(page), 1, "page {page}");
    }
    assert_eq!(grow.hits(1), 3, "the start of the watch, the walk, and the end of the watch");
}

#[tokio::test]
async fn no_reread_when_out_of_order_but_the_top_did_not_grow() {
    let (rig, grow, walk) = lost_work(CrawlConfig::default(), false).await;

    assert!(walk.unwrap().finished);
    assert_eq!(rig.store.count_works().await.unwrap(), 11);
    assert_eq!(grow.hits(3), 1, "page 3 is not read again: the top did not grow while the pages were read");
    assert_eq!(grow.hits(1), 3);
}

#[tokio::test]
async fn no_reread_when_read_in_order_even_if_the_top_grew() {
    let grow = growing(11, 2).await;
    let rig = rig(&grow, CrawlConfig::default(), 2, None).await;

    rig.watch(true).await.unwrap();
    // pages 1 and 2 first, then the work comes, then pages 3 to 6: a work that moves down is read twice (it is on page 2 and on page 3), none is lost
    rig.read_by_hand(1, 2, true).await;
    grow.add();
    tokio::time::sleep(Duration::from_millis(5)).await;
    rig.read_by_hand(3, 4, true).await;

    assert!(rig.walk().await.unwrap().finished);
    assert_eq!(grow.hits(3), 1, "in order: nothing is read again");
    assert_eq!(grow.hits(2), 1);
    for id in 1..=11 {
        assert!(rig.has(id).await, "work {id}");
    }
}

#[tokio::test]
async fn the_plan_runs_once_and_ends() {
    let grow = growing(11, 2).await;
    // a work is added at the top again while page 3 is read again
    *grow.bump.lock().unwrap() = Some((3, 2));
    let rig = rig(&grow, CrawlConfig::default(), 2, None).await;

    rig.watch(true).await.unwrap();
    rig.read_by_hand(3, 4, true).await;
    grow.add();
    let walk = rig.walk().await.unwrap();

    assert!(walk.finished);
    assert_eq!(grow.added.load(Ordering::SeqCst), 2, "the list grew during the pass");
    assert_eq!(grow.hits(3), 2, "the plan was read once: the growth during the pass makes no new boundary");
    assert!(rig.store.crawl_state("Listed").await.unwrap().unwrap().done);
    assert!(rig.has(8).await);
    assert!(rig.store.gap_plan("Listed").await.unwrap().is_empty());
    assert_eq!(rig.store.crawl_read_count("Listed").await.unwrap(), 0);
}

#[tokio::test]
async fn a_plan_resumes_after_a_restart() {
    let grow = growing(11, 2).await;
    let rig1 = rig(&grow, CrawlConfig::default(), 2, None).await;

    rig1.watch(true).await.unwrap();
    rig1.read_by_hand(3, 4, true).await;
    grow.add();
    // the worker cannot read the pages again
    rig1.fail_cursors.lock().unwrap().insert("3".to_string());
    assert!(rig1.walk().await.is_err());

    let plan = rig1.store.gap_plan("Listed").await.unwrap();
    assert_eq!(plan.iter().map(|row| (row.lo, row.hi, row.done_at, row.tries)).collect::<Vec<_>>(), [(3, 3, None, 1)]);
    assert!(!rig1.has(8).await);
    assert!(!rig1.store.crawl_state("Listed").await.unwrap().unwrap().done);

    // a new process over the same database: the plan is not made again (page 1 is not looked at again), only its row that is not done is read
    let hits_of_page_1 = grow.hits(1);
    let rig2 = rig(&grow, CrawlConfig::default(), 2, Some(rig1.store.clone())).await;
    assert!(rig2.walk().await.unwrap().finished);

    assert!(rig2.has(8).await);
    assert_eq!(grow.hits(3), 2, "by hand, and once when the plan went on");
    assert_eq!(grow.hits(1), hits_of_page_1, "no new look at the top");
    assert!(rig2.store.crawl_state("Listed").await.unwrap().unwrap().done);
}

#[tokio::test]
async fn a_failing_reread_gives_up_after_three_tries_and_marks_done() {
    let grow = growing(11, 2).await;
    let rig = rig(&grow, CrawlConfig::default(), 2, None).await;

    rig.watch(true).await.unwrap();
    rig.read_by_hand(3, 4, true).await;
    grow.add();
    rig.fail_cursors.lock().unwrap().insert("3".to_string());

    assert!(rig.walk().await.is_err());
    assert_eq!(rig.store.gap_plan("Listed").await.unwrap()[0].tries, 1);
    assert!(rig.walk().await.is_err());
    assert_eq!(rig.store.gap_plan("Listed").await.unwrap()[0].tries, 2);
    // the third try fails too: the span is left to the refresher, and the list is marked as read
    let walk = rig.walk().await.unwrap();

    assert!(walk.finished);
    assert!(rig.store.crawl_state("Listed").await.unwrap().unwrap().done);
    assert!(!rig.has(8).await, "the span was not read");
}

#[tokio::test]
async fn the_budget_limits_the_rereads_and_warns() {
    let grow = growing(15, 2).await;
    let config = CrawlConfig { gap_budget_pages: 2, ..CrawlConfig::default() };
    let rig = rig(&grow, config, 2, None).await;

    rig.watch(true).await.unwrap();
    // three jobs, each read before the one above it: the boundaries 6|7, 4|5 and 2|3 are read out of order
    rig.read_by_hand(7, 2, true).await;
    rig.read_by_hand(5, 2, true).await;
    rig.read_by_hand(3, 2, true).await;
    grow.add();

    assert!(rig.walk().await.unwrap().finished);
    // two pages fit: the spans with the same growth are taken from the top (3 and 5), and the span of page 7 is left to the refresher
    assert_eq!(grow.hits(3), 2);
    assert_eq!(grow.hits(5), 2);
    assert_eq!(grow.hits(7), 1);
}

#[tokio::test]
async fn a_failed_forced_poll_returns_an_error_and_does_not_mark_done() {
    let grow = growing(11, 2).await;
    let rig = rig(&grow, CrawlConfig::default(), 2, None).await;

    rig.watch(true).await.unwrap();
    rig.read_by_hand(3, 4, true).await;
    rig.read_by_hand(1, 2, true).await;
    grow.fail.lock().unwrap().insert(1);

    let error = rig.walk().await.unwrap_err();
    assert!(error.contains("500"), "{error}");
    assert!(!rig.store.crawl_state("Listed").await.unwrap().unwrap().done);
    assert!(rig.store.gap_plan("Listed").await.unwrap().is_empty(), "no plan was made without the look");

    // the site answers again: the next walk makes the plan and ends
    grow.fail.lock().unwrap().clear();
    assert!(rig.walk().await.unwrap().finished);
}

#[tokio::test]
async fn a_walk_end_to_end_in_ranges_with_slow_first_job() {
    let grow = growing(11, 2).await;
    // the job with page 1 is slow, so the other jobs are read before it; the work comes when page 2 is asked for, after pages 3 to 6 were read
    *grow.delay.lock().unwrap() = Some((1, 300));
    *grow.bump.lock().unwrap() = Some((2, 1));
    let rig = rig(&grow, CrawlConfig::default(), 2, None).await;

    let walk = rig.walk().await.unwrap();

    assert!(walk.finished);
    assert_eq!(grow.added.load(Ordering::SeqCst), 1);
    assert!(rig.has(8).await, "the work that moved between the jobs is found");
    assert_eq!(rig.store.count_works().await.unwrap(), 11, "the 11 works that were there when the walk began (the new one at the top is for the refresher)");
    assert!(rig.store.crawl_state("Listed").await.unwrap().unwrap().done);
}

#[tokio::test]
async fn oldest_first_and_cursor_sites_get_no_rows_and_no_polls() {
    // the WordPress list is read oldest first: its pages do not move
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let first_page = Arc::new(AtomicUsize::new(0));
    let (counted, counted_first) = (hits.clone(), first_page.clone());
    let router = Router::new().route(
        "/wp/{n}",
        get(move |UrlPath(n): UrlPath<usize>| {
            let (counted, counted_first) = (counted.clone(), counted_first.clone());
            async move {
                counted.fetch_add(1, Ordering::SeqCst);
                if n == 1 {
                    counted_first.fetch_add(1, Ordering::SeqCst);
                }
                if n > 2 {
                    return (StatusCode::BAD_REQUEST, String::new());
                }
                let posts: Vec<String> = (0..2).map(|i| format!(r#"{{"link":"http://127.0.0.1:{port}/work/{}","title":{{"rendered":"Post {}"}},"tags":[],"date":"2026-01-01T00:00:00"}}"#, n * 10 + i, n * 10 + i)).collect();
                (StatusCode::OK, format!("[{}]", posts.join(",")))
            }
        }),
    );
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let store = Arc::new(SearchStore::in_memory().await.unwrap());
    let search = SearchConfig { min_delay_ms: 0, ..SearchConfig::default() };
    let engine = Engine::with_sites(vec![Site::new(def("Listed", port))], &search, store.clone(), crate::testing::romanizer_arc());
    let source = Source::WordPress(format!("http://127.0.0.1:{port}/wp/{{n}}?order=desc"));
    let hub = Arc::new(Hub::new("token".to_string()));
    let crawler = Arc::new(Crawler::with_sources(engine, CrawlConfig::default(), vec![("Listed".to_string(), source.clone())]).unwrap().with_hub(hub.clone(), 3, 2, 0));
    assert!(crawler.watched_sites().is_empty(), "no polls for a list that is read oldest first");

    let worker = {
        let (hub, crawler) = (hub.clone(), crawler.clone());
        tokio::spawn(async move {
            loop {
                match hub.take("Listed", "step", "w") {
                    Some((id, Job::Step { cursor, shard, ids, pages, seconds, oldest })) => {
                        let done = Done::Step(crawler.step_local("Listed", &cursor, shard, ids, pages, seconds, oldest).await);
                        hub.report(id, done);
                    }
                    _ => tokio::time::sleep(Duration::from_millis(5)).await,
                }
            }
        })
    };
    let (index, _) = crawler.sources[0].clone();
    let walk = crawler.walk(index, &source).await.unwrap();
    worker.abort();

    assert!(walk.finished);
    assert_eq!(store.count_works().await.unwrap(), 4);
    assert_eq!(store.crawl_read_count("Listed").await.unwrap(), 0);
    assert!(store.watch("Listed").await.unwrap().is_none());
    assert!(store.gap_plan("Listed").await.unwrap().is_empty());
    assert_eq!(first_page.load(Ordering::SeqCst), 1, "page 1 is read once, by the walk: no look at the top");
    assert!(hits.load(Ordering::SeqCst) <= 6, "pages 1 and 2 and the end");
}

#[tokio::test]
async fn role_off_and_role_worker_start_no_watch() {
    let grow = growing(11, 2).await;
    let rig = rig(&grow, CrawlConfig::default(), 2, None).await;

    // the host of workers watches the list
    assert_eq!(rig.crawler.watched_sites(), ["Listed"]);

    // role off and role worker have no hub, so there is nothing to watch (a worker only reads the pages of the jobs it is given)
    let search = SearchConfig { min_delay_ms: 0, ..SearchConfig::default() };
    let engine = Engine::with_sites(vec![Site::new(def("Listed", grow.port))], &search, rig.store.clone(), crate::testing::romanizer_arc());
    let source = Source::Pages(format!("http://127.0.0.1:{}/list/{{n}}", grow.port));
    let alone = Crawler::with_sources(engine, CrawlConfig::default(), vec![("Listed".to_string(), source)]).unwrap();
    assert!(alone.watched_sites().is_empty());

    // and the switches
    let off = rig_config(&grow, CrawlConfig { gap_check: false, ..CrawlConfig::default() }).await;
    assert!(off.watched_sites().is_empty());
    let quiet = rig_config(&grow, CrawlConfig { watch_seconds: 0, ..CrawlConfig::default() }).await;
    assert!(quiet.watched_sites().is_empty());
}

async fn rig_config(grow: &Arc<Grow>, config: CrawlConfig) -> Arc<Crawler> {
    rig(grow, config, 2, None).await.crawler
}

#[tokio::test]
async fn the_watch_stops_when_the_site_is_done() {
    let grow = growing(11, 2).await;
    let rig = rig(&grow, CrawlConfig { watch_seconds: 1, ..CrawlConfig::default() }, 2, None).await;
    let task = tokio::spawn(rig.crawler.clone().watch_site(0));

    // while the walk is not done the top is looked at
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(grow.hits(1) >= 1);
    assert!(rig.store.watch("Listed").await.unwrap().is_some());

    rig.store.mark_crawled("Listed", 1).await.unwrap();
    // the poll that was waiting for its time sees that the site is done
    tokio::time::sleep(Duration::from_millis(1300)).await;
    let seen = grow.hits(1);
    tokio::time::sleep(Duration::from_millis(1300)).await;
    task.abort();

    assert_eq!(grow.hits(1), seen, "no look at a site that is done");
    assert!(rig.store.watch("Listed").await.unwrap().is_none());
    // a look that was on its way when the walk ended writes nothing either
    rig.watch(false).await.unwrap();
    assert!(rig.store.watch("Listed").await.unwrap().is_none());
}

#[tokio::test]
async fn the_watch_counts_new_links_of_page_one() {
    let grow = growing(20, 5).await;
    let rig = rig(&grow, CrawlConfig::default(), 2, None).await;

    // the first look is the start of the watch: no growth
    rig.watch(true).await.unwrap();
    let row = rig.store.watch("Listed").await.unwrap().unwrap();
    assert_eq!((row.per_page, row.top.lines().count(), row.since0), (5, 5, row.at));
    assert_eq!(rig.store.growth_since("Listed", 0).await.unwrap(), (0, false));

    // two works came to the top
    grow.add();
    grow.add();
    tokio::time::sleep(Duration::from_millis(3)).await;
    rig.watch(true).await.unwrap();
    assert_eq!(rig.store.growth_since("Listed", 0).await.unwrap(), (2, false));
    assert!(rig.store.watch("Listed").await.unwrap().unwrap().top.starts_with("/work/22\n/work/21"));

    // nothing changed: no new row
    tokio::time::sleep(Duration::from_millis(3)).await;
    rig.watch(true).await.unwrap();
    assert_eq!(rig.store.growth_since("Listed", 0).await.unwrap(), (2, false));

    // more than the page shows: the number is a lower bound
    for _ in 0..7 {
        grow.add();
    }
    tokio::time::sleep(Duration::from_millis(3)).await;
    rig.watch(true).await.unwrap();
    assert_eq!(rig.store.growth_since("Listed", 0).await.unwrap(), (7, true));
}

#[tokio::test]
async fn the_watch_covers_a_downtime_with_one_sample() {
    let grow = growing(20, 5).await;
    let rig = rig(&grow, CrawlConfig::default(), 2, None).await;

    rig.watch(true).await.unwrap();
    let start = rig.store.watch("Listed").await.unwrap().unwrap().at;
    // the server was not looked at for a while, and 3 works came
    for _ in 0..3 {
        grow.add();
    }
    tokio::time::sleep(Duration::from_millis(40)).await;
    rig.watch(true).await.unwrap();
    let end = rig.store.watch("Listed").await.unwrap().unwrap().at;

    // one sample from the look before to the look after: any window inside the downtime sees the 3 works
    assert_eq!(rig.store.growth("Listed", start + 10, start + 20).await.unwrap(), (3, false));
    assert_eq!(rig.store.growth("Listed", start, end).await.unwrap(), (3, false));
    assert_eq!(rig.store.growth("Listed", end, end + 1000).await.unwrap(), (0, false), "after the look");
    assert_eq!(rig.store.watch("Listed").await.unwrap().unwrap().since0, start);
}

#[tokio::test]
async fn the_watch_skips_a_poll_during_a_ban() {
    let grow = growing(11, 2).await;
    let rig = rig(&grow, CrawlConfig::default(), 2, None).await;

    rig.crawler.note_ban(0, "the address is banned by the site for a while (The ban expires in 5 minutes and 0 seconds)");
    assert!(rig.crawler.banned(0));

    rig.watch(false).await.unwrap();
    assert_eq!(grow.hits(1), 0, "no request to a site that bans the address");
    assert!(rig.store.watch("Listed").await.unwrap().is_none());
    // a look that is needed (the end of the walk) says that it cannot be made
    assert!(rig.watch(true).await.is_err());
    assert_eq!(grow.hits(1), 0);
}

#[tokio::test]
async fn watch_seconds_zero_treats_boundaries_as_uncovered() {
    let grow = growing(11, 2).await;
    let rig = rig(&grow, CrawlConfig { watch_seconds: 0, ..CrawlConfig::default() }, 2, None).await;

    // pages 3 to 6 first, the top did not grow, then pages 1 and 2: with no watch nothing is known, so the boundary is read again, as far as the cap says
    rig.read_by_hand(3, 4, true).await;
    assert!(rig.walk().await.unwrap().finished);

    assert_eq!(grow.hits(1), 1, "no look at the top at all");
    assert_eq!(grow.hits(3), 2);
    assert_eq!(grow.hits(6), 2, "the cap is 10 pages, up to the last page");
    assert!(rig.store.watch("Listed").await.unwrap().is_none());
}

#[tokio::test]
async fn a_walk_from_before_the_change_has_no_pairs() {
    // compat: delete with the compat code. Pages that were read before the read times were kept have no row, so no boundary is known
    let grow = growing(11, 2).await;
    let rig = rig(&grow, CrawlConfig::default(), 2, None).await;

    rig.read_by_hand(3, 4, false).await;
    grow.add();

    assert!(rig.walk().await.unwrap().finished);
    assert_eq!(grow.hits(3), 1);
    assert!(!rig.has(8).await, "the old walk keeps its gap: the refresher is what is left");
    assert!(rig.store.crawl_state("Listed").await.unwrap().unwrap().done);
}

// ---- the ids that no list showed ----

struct Pages {
    exist: Mutex<HashSet<u32>>,
    error: Mutex<bool>,
    hits: Mutex<Vec<u32>>,
}

async fn gallery(State(pages): State<Arc<Pages>>, UrlPath(id): UrlPath<u32>) -> impl IntoResponse {
    pages.hits.lock().unwrap().push(id);

    if *pages.error.lock().unwrap() {
        (StatusCode::SERVICE_UNAVAILABLE, "no".to_string())
    } else if pages.exist.lock().unwrap().contains(&id) {
        (StatusCode::OK, format!("<html><h1>Found {id}</h1></html>"))
    } else {
        (StatusCode::NOT_FOUND, String::new())
    }
}

/// A site with the name of HentaiEnvy (its pages say that they are gone with a 404) whose works 10, 12 and 15 are saved.
async fn id_rig(config: CrawlConfig) -> (Arc<Crawler>, Arc<SearchStore>, Arc<Pages>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let pages = Arc::new(Pages { exist: Default::default(), error: Default::default(), hits: Default::default() });
    let router = Router::new().route("/gallery/{id}/", get(gallery)).with_state(pages.clone());
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

    let origin: &'static str = Box::leak(format!("http://127.0.0.1:{port}").into_boxed_str());
    let site = def_at("HentaiEnvy", origin, port);
    let store = Arc::new(SearchStore::in_memory().await.unwrap());
    let search = SearchConfig { min_delay_ms: 0, ..SearchConfig::default() };
    let engine = Engine::with_sites(vec![Site::new(site)], &search, store.clone(), crate::testing::romanizer_arc());
    let source = Source::Pages(format!("{origin}/list/{{n}}"));
    let crawler = Arc::new(Crawler::with_sources(engine, config, vec![("HentaiEnvy".to_string(), source)]).unwrap());
    let works: Vec<NewWork> = [10, 12, 15].iter().map(|id| NewWork { path: format!("/gallery/{id}/"), title: format!("Work {id}"), romaji: format!("work {id}"), language: None, extra: Default::default() }).collect();

    store.save_crawled("HentaiEnvy", 1, &works, None).await.unwrap();

    (crawler, store, pages)
}

#[tokio::test]
async fn a_gone_id_is_probed_once() {
    let (crawler, store, pages) = id_rig(CrawlConfig::default()).await;

    crawler.id_gap_pass(0).await.unwrap();

    // 11, 13 and 14 are between 10 and 15, and are not saved: the newest first
    assert_eq!(*pages.hits.lock().unwrap(), [14, 13, 11]);
    assert_eq!(store.probes("HentaiEnvy").await.unwrap(), [(11, false), (13, false), (14, false)]);

    // the second pass has nothing to look at
    crawler.id_gap_pass(0).await.unwrap();
    assert_eq!(pages.hits.lock().unwrap().len(), 3, "a gone page is not asked again");
    assert_eq!(store.counts_of("HentaiEnvy").await.unwrap(), 3);
}

#[tokio::test]
async fn an_existing_missing_id_is_fetched_and_saved() {
    let (crawler, store, pages) = id_rig(CrawlConfig::default()).await;
    pages.exist.lock().unwrap().insert(13);

    crawler.id_gap_pass(0).await.unwrap();

    assert_eq!(store.probes("HentaiEnvy").await.unwrap(), [(11, false), (13, true), (14, false)]);
    assert_eq!(*pages.hits.lock().unwrap(), [14, 13, 11, 13], "the probe, and then the page itself");
    assert_eq!(store.title_at("HentaiEnvy", &["/gallery/13/".to_string()]).await.unwrap().as_deref(), Some("Found 13"));
    assert_eq!(store.counts_of("HentaiEnvy").await.unwrap(), 4);
}

#[tokio::test]
async fn a_probe_error_records_nothing() {
    let (crawler, store, pages) = id_rig(CrawlConfig::default()).await;
    *pages.error.lock().unwrap() = true;

    crawler.id_gap_pass(0).await.unwrap();

    assert_eq!(pages.hits.lock().unwrap().len(), 3);
    assert!(store.probes("HentaiEnvy").await.unwrap().is_empty(), "an answer that says nothing is asked again next time");
    assert_eq!(store.counts_of("HentaiEnvy").await.unwrap(), 3);

    // a page that exists but has no title is not recorded either
    *pages.error.lock().unwrap() = false;
    pages.exist.lock().unwrap().insert(14);
    pages.hits.lock().unwrap().clear();
    crawler.id_gap_pass(0).await.unwrap();
    assert_eq!(store.probes("HentaiEnvy").await.unwrap().iter().filter(|(id, _)| *id == 14).count(), 1);
}

#[tokio::test]
async fn id_probes_are_capped_per_cycle_and_only_for_listed_sites() {
    let (crawler, store, pages) = id_rig(CrawlConfig { id_gap_per_cycle: 2, ..CrawlConfig::default() }).await;

    crawler.id_gap_pass(0).await.unwrap();
    assert_eq!(*pages.hits.lock().unwrap(), [14, 13], "two ids, the newest");
    crawler.id_gap_pass(0).await.unwrap();
    assert_eq!(*pages.hits.lock().unwrap(), [14, 13, 11]);
    assert_eq!(store.probes("HentaiEnvy").await.unwrap().len(), 3);

    // a site that is not in the list, and the switch of an empty list
    let (crawler, _, pages) = id_rig(CrawlConfig { id_gap_sites: vec!["Wnacg".to_string()], ..CrawlConfig::default() }).await;
    crawler.id_gap_pass(0).await.unwrap();
    assert!(pages.hits.lock().unwrap().is_empty());
    let (crawler, _, pages) = id_rig(CrawlConfig { id_gap_sites: Vec::new(), ..CrawlConfig::default() }).await;
    crawler.id_gap_pass(0).await.unwrap();
    assert!(pages.hits.lock().unwrap().is_empty());
}
