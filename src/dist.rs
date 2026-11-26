//! Handing the reading of the sites to other machines (`[distribute]`).
//!
//! A site sees one IP address asking for its pages, and answers a limited number of them. The host (the server that keeps the database) cuts the
//! work into jobs, and each worker (a machine with an address of its own) asks the sites and gives the answer back:
//! - a **step** is some pages of a list of the crawl, `pages` of them (`Crawler::step_local`): the host gets the links and saves the works;
//! - a **probe** is a batch of pages of saved works that are looked at for the sign that the work is gone (`Engine::probe`): the host records them.
//!
//! The workers keep no state of the crawl: the host has the cursors and the database. A worker asks the host for a job (`GET /dist/claim`,
//! answered when there is one, or after some seconds with nothing) and sends what it found (`POST /dist/report/{id}`). A job that nobody reports
//! while its worker has not been heard of for `LOST_AFTER` (a worker says it is alive every `HEARTBEAT`: `POST /dist/alive`) is given to another worker. One task of a worker for each site and kind of job, so a site is never asked more often than
//! its delay allows, and a slow site does not hold the others.

use crate::api::AppState;
use crate::config::DistributeConfig;
use crate::crawl::{Crawler, Step};
use crate::engine::{gone_rule, Engine};
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::oneshot;

/// How often a worker says it is alive.
const HEARTBEAT: Duration = Duration::from_secs(10 * 60);
/// How long a worker may be silent (claims, reports and heartbeats all count) before its jobs are given to another worker: two heartbeats and a half.
const LOST_AFTER: Duration = Duration::from_secs(25 * 60);
/// The name of the host as a worker of its own jobs: it is never lost.
const HOST: &str = "host";
/// How long `claim` waits for a job before it answers that there is none.
const LONG_POLL: Duration = Duration::from_secs(25);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Job {
    /// One page of the list of a site: the cursor, the shard and the ids of E-Hentai (`lo`, `hi`) of the part that is read.
    /// `pages` pages are read before the answer (0 is one page).
    Step { cursor: String, shard: (u64, u64), ids: (Option<u64>, Option<u64>), #[serde(default)] pages: usize, #[serde(default)] seconds: u64, #[serde(default)] oldest: bool },
    /// Pages of saved works to look at, `spacing_ms` apart.
    Probe { paths: Vec<String>, spacing_ms: u64 },
}

impl Job {
    fn kind(&self) -> &'static str {
        match self {
            Job::Step { .. } => "step",
            Job::Probe { .. } => "probe",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Done {
    Step(Result<Step, String>),
    /// (path, gone) of each page that could be read.
    Probe(Vec<(String, bool)>),
}

struct Item {
    id: u64,
    site: String,
    job: Job,
    /// The answer, and the time (unix ms of the clock of the host) the job was handed to a worker.
    done: oneshot::Sender<(Done, i64)>,
    /// When the job was handed to a worker (unix ms of the clock of the host); none while it waits.
    taken: Option<i64>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

#[derive(Default)]
struct Queue {
    next: u64,
    waiting: VecDeque<Item>,
    /// The jobs that are taken, with the worker that has them.
    running: HashMap<u64, (String, Item)>,
    /// When each worker was heard of last.
    seen: HashMap<String, Instant>,
}

/// The host's list of jobs.
pub struct Hub {
    token: String,
    queue: std::sync::Mutex<Queue>,
}

impl Hub {
    pub fn new(token: String) -> Self {
        Self { token, queue: Default::default() }
    }

    /// Puts a job of `site` in the list and waits for the worker's answer.
    pub async fn submit(&self, site: &str, job: Job) -> Result<Done, String> {
        self.submit_timed(site, job).await.map(|(done, _)| done)
    }

    /// `submit`, and the time (unix ms of the clock of the host) at which a worker took the job: the pages the job read were read after it (the time a job waited
    /// for a worker is not part of the time it read).
    pub async fn submit_timed(&self, site: &str, job: Job) -> Result<(Done, i64), String> {
        let (done, wait) = oneshot::channel();
        {
            let mut queue = self.queue.lock().unwrap();
            queue.next += 1;
            let id = queue.next;
            queue.waiting.push_back(Item { id, site: site.to_string(), job, done, taken: None });
        }

        wait.await.map_err(|_| "the job was dropped".to_string())
    }

    /// The first job of `site` and `kind` that waits (or whose worker was lost).
    pub(crate) fn take(&self, site: &str, kind: &str, worker: &str) -> Option<(u64, Job)> {
        let mut queue = self.queue.lock().unwrap();
        queue.seen.insert(worker.to_string(), Instant::now());
        let silent = |queue: &Queue, name: &str| name != HOST && queue.seen.get(name).is_none_or(|at| at.elapsed() > LOST_AFTER);
        let lost: Vec<u64> = queue.running.iter().filter(|(_, (name, _))| silent(&queue, name)).map(|(id, _)| *id).collect();
        for id in lost {
            if let Some((_, mut item)) = queue.running.remove(&id) {
                item.taken = None;
                log::warn!("dist: the job {id} of {} was not reported, given again", item.site);
                queue.waiting.push_front(item);
            }
        }

        let place = queue.waiting.iter().position(|item| item.site == site && item.job.kind() == kind)?;
        let mut item = queue.waiting.remove(place)?;
        item.taken = Some(now_ms());
        let taken = (item.id, item.job.clone());
        queue.running.insert(item.id, (worker.to_string(), item));

        Some(taken)
    }

    pub(crate) fn report(&self, id: u64, done: Done) -> bool {
        let mut queue = self.queue.lock().unwrap();

        match queue.running.remove(&id) {
            Some((worker, item)) => {
                queue.seen.insert(worker, Instant::now());
                let taken = item.taken.unwrap_or_else(now_ms);
                item.done.send((done, taken)).is_ok()
            }
            None => false,
        }
    }

    /// A worker says it is alive.
    fn alive(&self, worker: &str) {
        self.queue.lock().unwrap().seen.insert(worker.to_string(), Instant::now());
    }

    fn allows(&self, headers: &HeaderMap) -> bool {
        let sent = headers.get("authorization").and_then(|value| value.to_str().ok()).and_then(|value| value.strip_prefix("Bearer ")).unwrap_or("");

        !self.token.is_empty() && sent.len() == self.token.len() && sent.bytes().zip(self.token.bytes()).fold(0u8, |same, (a, b)| same | (a ^ b)) == 0
    }
}

/// A worker's way to the names of the tags that the host keeps (`site_tag_names`).
#[derive(Clone)]
pub struct Remote {
    client: reqwest::Client,
    base: String,
    token: String,
}

impl Remote {
    pub fn new(config: &DistributeConfig) -> Self {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(60)).build().unwrap_or_default();

        Self { client, base: config.host.trim_end_matches('/').to_string(), token: config.token.clone() }
    }

    /// `GET /dist/tag-names?site=`: (attribute, id, name) of the site.
    pub async fn names(&self, site: &str) -> Result<Vec<(String, u64, String)>, String> {
        let response = self.client.get(format!("{}/dist/tag-names", self.base)).query(&[("site", site)]).bearer_auth(&self.token).send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("HTTP {}", response.status()));
        }
        response.json().await.map_err(|e| e.to_string())
    }

    /// `POST /dist/tag-names`: a name this worker learned.
    pub async fn learned(&self, site: &str, attribute: &str, id: u64, name: &str) -> Result<(), String> {
        let body = Learned { site: site.to_string(), attribute: attribute.to_string(), id, name: name.to_string() };
        let response = self.client.post(format!("{}/dist/tag-names", self.base)).bearer_auth(&self.token).json(&body).send().await.map_err(|e| e.to_string())?;
        if response.status().is_success() { Ok(()) } else { Err(format!("HTTP {}", response.status())) }
    }
}

#[derive(Serialize, Deserialize)]
pub struct Learned {
    site: String,
    attribute: String,
    id: u64,
    name: String,
}

#[derive(Deserialize)]
pub struct NamesQuery {
    site: String,
}

/// `GET /dist/tag-names?site=`: the names of the numbers the site gives its tags, as `[attribute, id, name]`.
pub async fn tag_names(State(state): State<Arc<AppState>>, headers: HeaderMap, Query(query): Query<NamesQuery>) -> impl IntoResponse {
    let Some(engine) = state.hub.clone().filter(|hub| hub.allows(&headers)).and(state.search.clone()) else { return StatusCode::UNAUTHORIZED.into_response() };

    match engine.store().site_tag_names(&query.site).await {
        Ok(names) => Json(names).into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// `POST /dist/tag-names`: keeps a name that a worker learned.
pub async fn learn_tag_name(State(state): State<Arc<AppState>>, headers: HeaderMap, Json(learned): Json<Learned>) -> impl IntoResponse {
    let Some(engine) = state.hub.clone().filter(|hub| hub.allows(&headers)).and(state.search.clone()) else { return StatusCode::UNAUTHORIZED.into_response() };

    match engine.store().save_site_tag_name(&learned.site, &learned.attribute, learned.id, &learned.name).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct ClaimQuery {
    site: String,
    kind: String,
}

/// `GET /dist/claim?site=&kind=`: 200 with `{id, job}`, or 204 when there is none (after some seconds).
pub async fn claim(State(state): State<Arc<AppState>>, headers: HeaderMap, Query(query): Query<ClaimQuery>) -> impl IntoResponse {
    let Some(hub) = state.hub.clone().filter(|hub| hub.allows(&headers)) else { return StatusCode::UNAUTHORIZED.into_response() };
    let started = Instant::now();

    loop {
        let worker = headers.get("x-worker").and_then(|value| value.to_str().ok()).unwrap_or("unnamed");

        if let Some((id, job)) = hub.take(&query.site, &query.kind, worker) {
            return Json(serde_json::json!({ "id": id, "job": job })).into_response();
        }
        if started.elapsed() > LONG_POLL {
            return StatusCode::NO_CONTENT.into_response();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// `POST /dist/report/{id}` with the `Done` as JSON.
pub async fn report(State(state): State<Arc<AppState>>, headers: HeaderMap, Path(id): Path<u64>, Json(done): Json<Done>) -> impl IntoResponse {
    let Some(hub) = state.hub.clone().filter(|hub| hub.allows(&headers)) else { return StatusCode::UNAUTHORIZED };

    if hub.report(id, done) { StatusCode::OK } else { StatusCode::GONE }
}

/// `POST /dist/alive` (the name of the worker in `X-Worker`): the worker says it is alive.
pub async fn alive(State(state): State<Arc<AppState>>, headers: HeaderMap) -> impl IntoResponse {
    let Some(hub) = state.hub.clone().filter(|hub| hub.allows(&headers)) else { return StatusCode::UNAUTHORIZED };

    hub.alive(headers.get("x-worker").and_then(|value| value.to_str().ok()).unwrap_or("unnamed"));

    StatusCode::OK
}

/// The worker: one task for each site and kind of job. Never returns.
pub async fn work(engine: Arc<Engine>, crawler: Arc<Crawler>, config: DistributeConfig, verified: Vec<&'static str>) {
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(60)).build() {
        Ok(client) => client,
        Err(e) => {
            log::error!("dist: {e}");
            return;
        }
    };
    let mut tasks = tokio::task::JoinSet::new();
    let crawled = crawler.site_names();
    let name = worker_name();
    tasks.spawn(heartbeat(client.clone(), config.clone(), name.clone()));
    let me = name.clone();
    log::info!("dist: worker {name} of {}, {} sites crawled, {} verified", config.host, crawled.len(), verified.len());

    for name in crawled {
        tasks.spawn(worker_loop(engine.clone(), crawler.clone(), config.clone(), client.clone(), me.clone(), name.to_string(), "step"));
    }
    for name in verified {
        tasks.spawn(worker_loop(engine.clone(), crawler.clone(), config.clone(), client.clone(), me.clone(), name.to_string(), "probe"));
    }
    while tasks.join_next().await.is_some() {}
}

/// The name of this worker in the logs of the host: the name of the machine and the id of the process (`vultr-2738203`).
fn worker_name() -> String {
    let machine = std::env::var("HOSTNAME")
        .ok()
        .or_else(|| std::fs::read_to_string("/etc/hostname").ok())
        .or_else(|| std::process::Command::new("hostname").output().ok().and_then(|out| String::from_utf8(out.stdout).ok()))
        // it travels in a header: letters, digits and a few marks only
        .map(|name| name.trim().chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_')).collect::<String>())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "worker".to_string());

    format!("{machine}-{}", std::process::id())
}

/// Says to the host that this worker is alive, every `HEARTBEAT`.
async fn heartbeat(client: reqwest::Client, config: DistributeConfig, name: String) {
    let url = format!("{}/dist/alive", config.host.trim_end_matches('/'));

    loop {
        match client.post(&url).header("x-worker", &name).bearer_auth(&config.token).send().await {
            Ok(response) if response.status().is_success() => log::debug!("dist: the host knows that {name} is alive"),
            Ok(response) => log::warn!("dist: the host answered {} to the heartbeat", response.status()),
            Err(e) => log::warn!("dist: the heartbeat failed: {e}"),
        }
        tokio::time::sleep(HEARTBEAT).await;
    }
}

async fn worker_loop(engine: Arc<Engine>, crawler: Arc<Crawler>, config: DistributeConfig, client: reqwest::Client, me: String, site: String, kind: &'static str) {
    let base = config.host.trim_end_matches('/').to_string();

    loop {
        let answer = client.get(format!("{base}/dist/claim")).query(&[("site", site.as_str()), ("kind", kind)]).header("x-worker", &me).bearer_auth(&config.token).send().await;
        let response = match answer {
            Ok(response) if response.status() == StatusCode::NO_CONTENT => continue,
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                log::warn!("dist {site}: the host answered {}", response.status());
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
            Err(e) => {
                log::warn!("dist {site}: {e}");
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
        };
        let Ok(claimed) = response.json::<serde_json::Value>().await else { continue };
        let (Some(id), Some(job)) = (claimed.get("id").and_then(|id| id.as_u64()), claimed.get("job").and_then(|job| serde_json::from_value::<Job>(job.clone()).ok())) else { continue };

        let what = match &job {
            Job::Step { cursor, shard, pages, .. } => format!("step of {pages} pages of part {}/{} from {cursor:?}", shard.0, shard.1),
            Job::Probe { paths, .. } => format!("probe of {} pages", paths.len()),
        };
        let started = std::time::Instant::now();
        log::info!("dist {site}: job {id} taken from the host ({what})");
        let done = run(&engine, &crawler, &site, job).await;
        log::info!("dist {site}: job {id} done in {} ms, reporting", started.elapsed().as_millis());

        // a report that fails is tried again: the page was read, and asking the site again costs more than asking the host
        for attempt in 0..5 {
            match client.post(format!("{base}/dist/report/{id}")).header("x-worker", &me).bearer_auth(&config.token).json(&done).send().await {
                Ok(response) if response.status().is_success() || response.status() == StatusCode::GONE => break,
                other => {
                    log::warn!("dist {site}: the report of {id} failed ({}), try {attempt}", other.map_or_else(|e| e.to_string(), |r| r.status().to_string()));
                    tokio::time::sleep(Duration::from_secs(10)).await;
                }
            }
        }
    }
}

async fn run(engine: &Arc<Engine>, crawler: &Arc<Crawler>, site: &str, job: Job) -> Done {
    match job {
        Job::Step { cursor, shard, ids, pages, seconds, oldest } => Done::Step(crawler.step_local(site, &cursor, shard, ids, pages, seconds, oldest).await),
        Job::Probe { paths, spacing_ms } => Done::Probe(probe(engine, site, paths, Duration::from_millis(spacing_ms)).await),
    }
}

/// The host reads pages itself too: one task for each site and kind of job, taking the jobs of its own list as a worker does.
pub async fn work_local(hub: Arc<Hub>, engine: Arc<Engine>, crawler: Arc<Crawler>, verified: Vec<&'static str>) {
    let mut tasks = tokio::task::JoinSet::new();
    let crawled = crawler.site_names();
    log::info!("dist: the host reads too, {} sites crawled, {} verified", crawled.len(), verified.len());

    let sites = crawled.into_iter().map(|name| (name, "step")).chain(verified.into_iter().map(|name| (name, "probe")));
    for (site, kind) in sites {
        let (hub, engine, crawler) = (hub.clone(), engine.clone(), crawler.clone());
        tasks.spawn(async move {
            loop {
                match hub.take(site, kind, HOST) {
                    Some((id, job)) => {
                        let done = run(&engine, &crawler, site, job).await;
                        hub.report(id, done);
                    }
                    None => tokio::time::sleep(Duration::from_millis(500)).await,
                }
            }
        });
    }
    while tasks.join_next().await.is_some() {}
}

/// Looks at the pages of `paths` of `site`, `spacing` apart (the pages of the site are spread over the interval of the verification).
pub(crate) async fn probe(engine: &Arc<Engine>, site: &str, paths: Vec<String>, spacing: Duration) -> Vec<(String, bool)> {
    let (Some(index), Some(rule)) = (engine.site_named(site), gone_rule(site)) else { return Vec::new() };
    let mut results = Vec::new();

    for path in paths {
        let started = Instant::now();
        let url = engine.site(index).address(&path);

        if let Some(gone) = engine.probe(index, &url, rule).await {
            results.push((path, gone));
        }
        if let Some(rest) = spacing.checked_sub(started.elapsed()) {
            tokio::time::sleep(rest).await;
        }
    }

    results
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step() -> Job {
        Job::Step { cursor: "1".to_string(), shard: (0, 1), ids: (None, None), pages: 2, seconds: 0, oldest: false }
    }

    #[tokio::test]
    async fn submit_timed_returns_the_take_time_and_a_requeued_job_gets_a_new_one() {
        let hub = Arc::new(Hub::new("t".to_string()));
        let waiting = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.submit_timed("A", step()).await })
        };

        // the job waits for a worker: the time it waits is not in the time it was taken
        tokio::time::sleep(Duration::from_millis(60)).await;
        let before = now_ms();
        let (id, _) = loop {
            if let Some(taken) = hub.take("A", "step", "w1") {
                break taken;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        let first = hub.queue.lock().unwrap().running[&id].1.taken.unwrap();
        assert!(first >= before, "taken at the moment of the take");

        // the worker is lost: the job goes back to the list and is taken again later, with its own time
        hub.queue.lock().unwrap().seen.insert("w1".to_string(), Instant::now().checked_sub(LOST_AFTER + Duration::from_secs(1)).unwrap_or_else(Instant::now));
        tokio::time::sleep(Duration::from_millis(30)).await;
        let (again, _) = hub.take("A", "step", "w2").expect("the lost job is given again");
        assert_eq!(again, id);
        let second = hub.queue.lock().unwrap().running[&id].1.taken.unwrap();
        assert!(second > first, "a requeued job gets a new time ({first} then {second})");

        assert!(hub.report(id, Done::Probe(Vec::new())));
        let (done, taken) = waiting.await.unwrap().unwrap();
        assert!(matches!(done, Done::Probe(_)));
        assert_eq!(taken, second);

        // `submit` is the same without the time
        let plain = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.submit("A", step()).await })
        };
        let (id, _) = loop {
            if let Some(taken) = hub.take("A", "step", "w2") {
                break taken;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        hub.report(id, Done::Probe(Vec::new()));
        assert!(matches!(plain.await.unwrap().unwrap(), Done::Probe(_)));
    }
}
