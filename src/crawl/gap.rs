//! The gap check of the first walk of a numbered list that shows the newest works first (docs/SPEC.md, "Gap check").
//!
//! When a work is added at the top of such a list, every page moves down by one work. A page that is read later than the page after it can lose the work that
//! moved from the end of page `k` to the start of page `k + 1` (page `k + 1` was read before the move, page `k` after it): the work is on no page that was read. A
//! page that is read in order (`k` before `k + 1`) only gets a work twice, which does no harm. So, while the first walk is not done:
//! - the host keeps when each page was read, as the earliest and the latest time it may have been (`crawl_reads`);
//! - the host looks at page 1 now and then and keeps when works came to the top (`crawl_watch`, `crawl_growth`);
//! - when the list is read, each boundary of pages that may have been read out of order while the top grew is read again, from the page after it down (a plan
//!   that is made once: `crawl_gap_plan`).
//!
//! Besides, once a list is read, the ids that are missing between the smallest and the largest saved are looked at once each (`crawl_probes`).

use super::*;
use crate::dist::{Done, Hub, Job};
use crate::entity::crawl_gap;
use crate::hitomi::Fetch;
use crate::store::{Boundary, PageRead, Sample};
use std::sync::atomic::{AtomicI64, Ordering};

/// How many times a span is read again before it is given up (what is left is for the refresher, which reads the newest pages again).
pub(super) const GAP_TRIES: i64 = 3;

/// The most boundaries read from the database at once.
const PAIR_CHUNK: u64 = 5000;

/// The ids looked at in one job of probes.
const PROBE_BATCH: usize = 10;

/// The pages of these lists move when a work is added at the top, and a walk of them reads the pages in any order: the oldest-first lists (their pages do not move), the lists
/// with a cursor (read in order) and the sites without a full list are not checked.
pub(super) fn gap_checked(source: &Source) -> bool {
    numbered(source) && !can_oldest(source)
}

pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// A number that is not the same twice in this process, near the time now in unix ms (it names a job; it is not a time to compare with).
pub(super) fn next_ms() -> i64 {
    static LAST: AtomicI64 = AtomicI64::new(0);
    let now = now_ms();
    let mut last = LAST.load(Ordering::SeqCst);

    loop {
        let next = now.max(last + 1);

        match LAST.compare_exchange(last, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return next,
            Err(seen) => last = seen,
        }
    }
}

/// When each page of a job was read: the job took the pages `first..first + pages` one after another from `taken` (the host handed the job out) to `reported`
/// (the host got the answer), and `at` says how long after `taken` each page was done (the clock of the worker is never used). Without `at` (an old worker, a job
/// that stopped on a repeated page) every page may have been read at any time of the job.
pub(super) fn page_reads(first: u64, pages: u64, taken: i64, reported: i64, at: &[u32], job: i64) -> Vec<PageRead> {
    let exact = pages > 0 && at.len() as u64 == pages;
    let total = at.last().copied().unwrap_or(0) as i64;

    (0..pages)
        .map(|i| {
            let (mut lo, mut hi) = if exact {
                let before = if i == 0 { 0 } else { at[i as usize - 1] as i64 };
                (taken + before, reported - (total - at[i as usize] as i64))
            } else {
                (taken, reported)
            };

            if lo > hi {
                std::mem::swap(&mut lo, &mut hi);
            }
            PageRead { page: first + i, job, lo_ms: lo, hi_ms: hi }
        })
        .collect()
}

/// Page `b` (the page after `a`) may have been read before page `a` was done, and by another job: the pair that the database query `crawl_pairs` returns.
#[cfg(test)]
pub(super) fn out_of_order(a: &PageRead, b: &PageRead) -> bool {
    a.job != b.job && b.lo_ms < a.hi_ms
}

/// What the host knows of the top of the list: it has looked since `since0`, and last at `at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Seen {
    pub since0: i64,
    pub at: i64,
    pub per_page: i64,
}

/// Whether a boundary is dangerous, and which pages are read again for it: `(first page, last page, growth since the later page was read)`.
/// - `window` is the growth between the earliest time page `k + 1` may have been read and the latest time page `k` may have been (a sum of samples that overlap it, so
///   too much rather than too little);
/// - `since_later` is the growth after the earliest read of page `k + 1` (and whether it is only a lower bound): the works that came after page `k + 1` was read
///   moved the list down by that many places, so that many places (rounded up in pages, and half a page more) below page `k` are read again;
/// - the top may have grown in a time that was not watched (`uncovered`): the span is the longest.
pub(super) fn judge(pair: &Boundary, seen: Option<&Seen>, window: i64, since_later: (i64, bool), max_pages: usize, top: u64) -> Option<(u64, u64, i64)> {
    let uncovered = seen.is_none_or(|seen| pair.b_lo < seen.since0 || pair.a_hi > seen.at);

    if !uncovered && window <= 0 {
        return None;
    }
    let per_page = seen.map(|seen| seen.per_page).filter(|per_page| *per_page > 0).unwrap_or(20);
    let max = max_pages.max(1) as i64;
    let (growth, saturated) = since_later;
    let pages = if uncovered || saturated { max } else { ((growth + per_page / 2 + per_page - 1) / per_page).clamp(1, max) };
    let (lo, hi) = (pair.k + 1, (pair.k + pages as u64).min(top));

    (lo <= hi).then_some((lo, hi, growth))
}

/// Joins the spans that overlap or touch, then keeps the ones with the most growth while their pages fit in `budget`. Returns the spans (the lowest first) and how many
/// were left out.
pub(super) fn plan_spans(mut raw: Vec<(u64, u64, i64)>, budget: usize) -> (Vec<(u64, u64)>, usize) {
    raw.sort_unstable();
    let mut merged: Vec<(u64, u64, i64)> = Vec::new();

    for (lo, hi, growth) in raw {
        match merged.last_mut() {
            Some(last) if lo <= last.1 + 1 => {
                last.1 = last.1.max(hi);
                last.2 = last.2.max(growth);
            }
            _ => merged.push((lo, hi, growth)),
        }
    }

    let mut order: Vec<usize> = (0..merged.len()).collect();
    order.sort_by(|a, b| merged[*b].2.cmp(&merged[*a].2).then(merged[*a].0.cmp(&merged[*b].0)));
    let (mut used, mut kept, mut dropped) = (0u64, Vec::new(), 0);

    for place in order {
        let (lo, hi, _) = merged[place];
        let pages = hi - lo + 1;

        if used + pages <= budget as u64 {
            used += pages;
            kept.push((lo, hi));
        } else {
            dropped += 1;
        }
    }
    kept.sort_unstable();

    (kept, dropped)
}

/// How many works came to the top since the look before, from page 1 now: `(added, saturated)`. `old` is the saved top (the paths of page 1 joined by a newline, or for
/// Hitomi.La the length of the index file), `top` and `links` are the new one. `saturated`: no work of the page before is on the page now, so more works came than the
/// page shows and `added` is only a lower bound.
pub(super) fn growth_of(old: &str, top: &str, links: &[String], hitomi: bool) -> (i64, bool) {
    if hitomi {
        let (Ok(old), Ok(new)) = (old.parse::<u64>(), top.parse::<u64>()) else { return (0, false) };

        // an id is 4 bytes
        return ((new.saturating_sub(old) / 4) as i64, false);
    }

    let known: HashSet<&str> = old.split('\n').filter(|path| !path.is_empty()).collect();
    let added = links.iter().filter(|path| !known.contains(path.as_str())).count();

    (added as i64, !known.is_empty() && added >= links.len() && !links.is_empty())
}

impl Crawler {
    /// A site that said it bans the address is left alone for the time the ban lasts.
    pub(super) fn note_ban(&self, index: usize, error: &str) {
        if let Some(wait) = ban_wait(error) {
            self.bans.lock().unwrap().insert(index, std::time::Instant::now() + wait);
        }
    }

    pub(super) fn banned(&self, index: usize) -> bool {
        self.bans.lock().unwrap().get(&index).is_some_and(|until| *until > std::time::Instant::now())
    }

    /// The sites whose top is watched while their first walk is not done: only the host of workers does it, and only for the lists that the gap check is for.
    pub fn watched_sites(&self) -> Vec<&'static str> {
        if self.hub.is_none() || !self.config.gap_check || self.config.watch_seconds == 0 {
            return Vec::new();
        }

        self.sources.iter().filter(|(_, source)| gap_checked(source)).map(|(index, _)| self.engine.site(*index).def.name).collect()
    }

    /// Looks at page 1 of the list of a site, once, and keeps what it shows (and the growth since the last look). `force`: the look is made even when the ban of
    /// the site has not ended (and an error says so). The first look of a site is only the start of the watch.
    pub(super) async fn watch_once(&self, index: usize, source: &Source, force: bool) -> std::result::Result<(), String> {
        let name = self.engine.site(index).def.name;
        let store = self.engine.store();

        if self.banned(index) {
            return if force { Err(format!("{name} is banned, no look at page 1")) } else { Ok(()) };
        }

        let (top, per_page, links): (String, i64, Vec<String>) = match source {
            Source::Hitomi => {
                let path = format!("/{}", self.config.hitomi_index.trim_start_matches('/'));
                let size = self.engine.hitomi(index).size(&path).await.inspect_err(|e| self.note_ban(index, e))?;

                (size.to_string(), self.config.hitomi_batch.max(1) as i64, Vec::new())
            }
            Source::Pages(template) => {
                let (page_url, body) = self.engine.get_page(index, &template.replace("{n}", "1")).await.inspect_err(|e| self.note_ban(index, e))?;
                let links: Vec<String> = self.engine.site(index).read(&page_url, &body, MOST_LINKS)?.into_iter().map(|link| link.path).collect();

                if links.is_empty() {
                    return Err(format!("{name}: page 1 shows no work"));
                }
                (links.join("\n"), links.len() as i64, links)
            }
            _ => return Ok(()),
        };

        // the walk may have ended while the page was asked for: its rows are gone, and are not made again
        if store.crawl_state(name).await.map_err(|e| e.to_string())?.is_some_and(|row| row.done) {
            return Ok(());
        }
        let now = now_ms();

        match store.watch(name).await.map_err(|e| e.to_string())? {
            None => {
                store.save_watch(name, now, &top, per_page, None).await.map_err(|e| e.to_string())?;
                log::info!("gap watch {name}: baseline ({} links)", links.len());
            }
            Some(old) => {
                let (added, saturated) = growth_of(&old.top, &top, &links, matches!(source, Source::Hitomi));
                let sample = (added > 0).then_some(Sample { since: old.at, at: now, added, saturated });

                store.save_watch(name, now, &top, per_page, sample).await.map_err(|e| e.to_string())?;
                if added > 0 {
                    log::info!("gap watch {name}: +{added} at the top");
                }
            }
        }

        Ok(())
    }

    /// Looks at page 1 of a site every `watch_seconds` (and never closer than 4 delays of the site) while its first walk is not done.
    pub(super) async fn watch_site(self: Arc<Self>, place: usize) {
        let (index, source) = &self.sources[place];
        let name = self.engine.site(*index).def.name;
        let wait = Duration::from_secs(self.config.watch_seconds).max(self.engine.delay(*index) * 4);

        loop {
            let done = self.engine.store().crawl_state(name).await.ok().flatten().is_some_and(|row| row.done);

            if done {
                tokio::time::sleep(Duration::from_secs(600)).await;
                continue;
            }
            if let Err(e) = self.watch_once(*index, source, false).await {
                log::debug!("gap watch {name}: failed ({e})");
            }
            tokio::time::sleep(wait).await;
        }
    }

    /// Makes the plan of the pages to read again (once: a restart goes on with the plan it finds), from the boundaries of pages that were read out of order while the top grew.
    async fn make_plan(&self, name: &str) -> std::result::Result<(), String> {
        let store = self.engine.store();
        let top = store.crawl_read_top(name).await.map_err(|e| e.to_string())?;
        let seen = store.watch(name).await.map_err(|e| e.to_string())?.filter(|_| self.config.watch_seconds > 0).map(|row| Seen { since0: row.since0, at: row.at, per_page: row.per_page });
        let (mut raw, mut boundaries, mut after) = (Vec::new(), 0usize, 0u64);

        loop {
            let pairs = store.crawl_pairs(name, after, PAIR_CHUNK).await.map_err(|e| e.to_string())?;
            let Some(last) = pairs.last() else { break };

            after = last.k;
            boundaries += pairs.len();
            for pair in &pairs {
                let (window, _) = store.growth(name, pair.b_lo, pair.a_hi).await.map_err(|e| e.to_string())?;
                let since = store.growth_since(name, pair.b_lo).await.map_err(|e| e.to_string())?;

                if let Some(span) = judge(pair, seen.as_ref(), window, since, self.config.gap_reread_max_pages, top) {
                    raw.push(span);
                }
            }
        }

        let dangerous = raw.len();
        let (spans, dropped) = plan_spans(raw, self.config.gap_budget_pages);
        let pages: u64 = spans.iter().map(|(lo, hi)| hi - lo + 1).sum();

        log::info!("gap check {name}: {dangerous} dangerous of {boundaries} boundaries, re-reading {pages} pages in {} spans (budget {})", spans.len(), self.config.gap_budget_pages);
        if dropped > 0 {
            log::warn!("gap check {name}: budget reached, {dropped} spans left to the refresher");
        }
        store.save_gap_plan(name, &spans, unix_now()).await.map_err(|e| e.to_string())?;

        Ok(())
    }

    /// The pages read again after the list was read, before it is marked as read: makes the plan if there is none, then reads the spans of it that are not done. An error
    /// (the walk starts again later and goes on with the plan) when a span could not be read and has tries left.
    pub(super) async fn gap_pass(&self, index: usize, source: &Source, hub: &Arc<Hub>, lookahead: usize) -> std::result::Result<(), String> {
        let name = self.engine.site(index).def.name;
        let store = self.engine.store();
        let mut plan = store.gap_plan(name).await.map_err(|e| e.to_string())?;

        if plan.is_empty() {
            // the top is looked at now, so that the last reads of the walk are inside the watched time (no look when no boundary was read out of order: there is nothing to judge)
            if self.config.watch_seconds > 0 && !store.crawl_pairs(name, 0, 1).await.map_err(|e| e.to_string())?.is_empty() {
                self.watch_once(index, source, true).await?;
            }
            self.make_plan(name).await?;
            plan = store.gap_plan(name).await.map_err(|e| e.to_string())?;
        }

        let width = self.job_pages.max(1) as u64;
        let mut waiting: std::collections::VecDeque<crawl_gap::Model> = plan.into_iter().filter(|row| row.done_at.is_none()).collect();
        let mut tasks: JoinSet<(i64, i64, std::result::Result<Vec<(u64, u64, super::Step)>, String>)> = JoinSet::new();
        let mut failure: Option<String> = None;

        loop {
            while tasks.len() < lookahead.max(1) {
                let Some(row) = waiting.pop_front() else { break };
                let (lo, hi) = (row.lo as u64, row.hi as u64);
                let mut jobs = Vec::new();
                let mut first = lo;

                while first <= hi {
                    let last = (first + width - 1).min(hi);
                    jobs.push((self.cursor_of(source, first), first, last));
                    first = last + 1;
                }
                let (hub, site, seconds) = (hub.clone(), name.to_string(), self.job_seconds);

                tasks.spawn(async move { (row.lo, row.hi, read_span(hub, site, jobs, seconds).await) });
            }

            let Some(finished) = tasks.join_next().await else { break };
            let (lo, hi, outcome) = finished.map_err(|e| e.to_string())?;
            let (lo, hi) = (lo as u64, hi as u64);
            let outcome = match outcome {
                Ok(steps) => self.save_span(index, source, lo, hi, steps).await,
                Err(e) => Err(e),
            };

            match outcome {
                Ok(new) if new > 0 => log::warn!("gap re-read {name} pages {lo}-{hi}: {new} new works"),
                Ok(new) => log::info!("gap re-read {name} pages {lo}-{hi}: {new} new works"),
                Err(e) => {
                    // a ban says it is not the span that fails: the tries are kept
                    self.note_ban(index, &e);
                    let tries = if ban_wait(&e).is_some() { 0 } else { store.bump_gap_tries(name, lo, GAP_TRIES, unix_now()).await.map_err(|e| e.to_string())? };

                    if tries >= GAP_TRIES {
                        log::error!("gap re-read {name} pages {lo}-{hi} gave up after {GAP_TRIES} tries ({e})");
                    } else {
                        log::warn!("gap re-read {name} pages {lo}-{hi}: {e}");
                        failure.get_or_insert(e);
                    }
                }
            }
        }

        match failure {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Saves the works of a span that was read again (and notes that the span is done). Returns how many were new.
    async fn save_span(&self, index: usize, source: &Source, lo: u64, hi: u64, steps: Vec<(u64, u64, super::Step)>) -> std::result::Result<usize, String> {
        let name = self.engine.site(index).def.name;
        let mut links = Vec::new();

        for (first, last, step) in steps {
            // a job that stopped early (a page could not be read) did not read the span
            if let Some(next) = &step.next {
                if self.page_of(source, next).is_some_and(|page| page <= last) {
                    return Err(format!("the job from page {first} stopped before page {last}"));
                }
            }
            links.extend(step.links);
        }
        let works = self.engine.works_of(links).await?;

        self.engine.store().save_gap_reread(name, unix_now(), &works, lo).await.map_err(|e| format!("{e} (pages {lo}-{hi})"))
    }

    /// After the list of a site is read: looks at the ids that are missing between the smallest and the largest saved, newest first, `id_gap_per_cycle` of them. A page that is
    /// gone is noted (and never asked again); a page that exists is read and saved. An id that could not be looked at is left for the next time.
    pub(super) async fn id_gap_pass(&self, index: usize) -> std::result::Result<(), String> {
        let name = self.engine.site(index).def.name;
        let per_cycle = self.config.id_gap_per_cycle;

        if per_cycle == 0 || !self.config.id_gap_sites.iter().any(|site| site.eq_ignore_ascii_case(name)) || crate::engine::gone_rule(name).is_none() || crate::sites::path_of(name, 1).is_none() {
            return Ok(());
        }
        if self.hub.is_some() && !self.probe_sites.iter().any(|site| site == name) {
            log::debug!("id gap {name}: no worker looks at pages of this site ([crawl.verify] has to name it)");
            return Ok(());
        }

        let store = self.engine.store();
        let ids = store.site_ids(name, |path| crate::sites::work_id(name, path)).await.map_err(|e| e.to_string())?;
        let missing = store.missing_ids(name, &ids, per_cycle).await.map_err(|e| e.to_string())?;

        if missing.is_empty() {
            return Ok(());
        }
        let (mut probed, mut found) = (0, 0);

        for batch in missing.chunks(PROBE_BATCH) {
            let paths: Vec<String> = batch.iter().filter_map(|id| crate::sites::path_of(name, *id)).collect();
            let mut results: Vec<(u32, bool)> = Vec::new();

            for (path, gone) in self.probe_paths(index, paths).await? {
                let Some(id) = crate::sites::work_id(name, &path) else { continue };

                if gone {
                    results.push((id, false));
                } else if self.save_found(index, &path).await {
                    log::warn!("id gap {name}: id {id} exists, added");
                    results.push((id, true));
                    found += 1;
                }
            }
            probed += results.len();
            store.save_probes(name, &results, unix_now()).await.map_err(|e| e.to_string())?;
        }
        log::info!("id gap {name}: missing {}, probed {probed}, found {found}", missing.len());

        Ok(())
    }

    /// Looks at the pages: by the workers when there are some (one job), else here. `(path, gone)` of each page that could be looked at.
    async fn probe_paths(&self, index: usize, paths: Vec<String>) -> std::result::Result<Vec<(String, bool)>, String> {
        let name = self.engine.site(index).def.name;

        match &self.hub {
            Some((hub, _)) => match hub.submit(name, Job::Probe { paths, spacing_ms: 0 }).await? {
                Done::Probe(results) => Ok(results),
                _ => Err("a worker answered with something that is not a probe".to_string()),
            },
            None => Ok(crate::dist::probe(&self.engine, name, paths, Duration::ZERO).await),
        }
    }

    /// Reads the page of a work that the list does not show and saves it. False when it could not be read (nothing is noted, the id is asked again).
    async fn save_found(&self, index: usize, path: &str) -> bool {
        let site = self.engine.site(index);
        let name = site.def.name;
        let page = match self.engine.get_page(index, &site.address(path)).await {
            Ok((_, page)) => page,
            Err(e) => {
                self.note_ban(index, &e);
                log::debug!("id gap {name}: {path}: {e}");
                return false;
            }
        };
        let Some(title) = crate::titles::title_of_page(name, &page) else {
            log::debug!("id gap {name}: {path} has no title");
            return false;
        };
        let Ok(works) = self.engine.works_of(vec![Link { path: path.to_string(), title, extra: Default::default() }]).await else { return false };

        self.engine.store().save_crawled(name, unix_now(), &works, None).await.is_ok()
    }
}

/// The jobs of one span, one after another from the top (`cursor`, first page, last page): the answers.
async fn read_span(hub: Arc<Hub>, site: String, jobs: Vec<(String, u64, u64)>, seconds: u64) -> std::result::Result<Vec<(u64, u64, super::Step)>, String> {
    let mut steps = Vec::new();

    for (cursor, first, last) in jobs {
        let job = Job::Step { cursor, shard: (0, 1), ids: (None, None), pages: (last - first + 1) as usize, seconds, oldest: false };

        match hub.submit(&site, job).await? {
            Done::Step(step) => steps.push((first, last, step?)),
            _ => return Err("a worker answered with something that is not a page".to_string()),
        }
    }

    Ok(steps)
}
