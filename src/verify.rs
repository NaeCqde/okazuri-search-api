//! Looking at the page of every saved work of the sites that tell when a work is gone, once in `interval_days`, and marking the works whose
//! page is gone (`[verify]`).
//!
//! Each site is asked on its own, one page every `interval / works` (900 000 works in 30 days are a page every 2.9 s), never closer than `min_pause_ms` (5 s: a site with more
//! than 518 400 works is looked at over more than `interval_days`) and the delay of the site. The works are taken in the order of their paths (no sort of the whole site), the ones looked at within the interval skipped. A site still lists a work whose page is gone, so the
//! row stays with no title (`works.deleted`): a list, the crawl or an import does not save the work again, and no search finds it.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::VerifyConfig;
use crate::engine::{gone_rule, Engine, GoneRule};
use crate::utils::Result;

/// How many pages are looked at before the answers are written.
const BATCH: u64 = 50;

pub struct Verifier {
    engine: Arc<Engine>,
    config: VerifyConfig,
    /// (index of the site in the engine, its name, how its pages say that a work is gone)
    sites: Vec<(usize, &'static str, GoneRule)>,
    /// The host of workers (`[distribute]`): the pages are looked at by the workers (this many at the same time).
    hub: Option<(Arc<crate::dist::Hub>, usize)>,
}

/// Every site that has a rule, by name.
const NAMES: &[&str] = &["RokuHentai", "HentaiEnvy", "Nya:Hentai", "momon:GA", "Sukebei", "AsmHentai", "HentaiEra", "Wnacg", "177Picyy"];

impl Verifier {
    /// An unknown site, or one whose pages do not tell, is an error.
    pub fn new(engine: Arc<Engine>, config: VerifyConfig, disabled: &[String]) -> Result<Self> {
        let names: Vec<String> = if config.sites.is_empty() { NAMES.iter().map(|s| s.to_string()).collect() } else { config.sites.clone() };
        let mut sites = Vec::new();

        for name in names {
            let Some(known) = NAMES.iter().find(|known| **known == name) else {
                return Err(format!("[crawl.verify] sites: {name} is not a site whose pages tell that a work is gone ({})", NAMES.join(", ")).into());
            };
            let index = engine.site_named(known).ok_or_else(|| format!("[crawl.verify] sites: {known} is not one of the sites of this program"))?;
            let rule = gone_rule(known).ok_or_else(|| format!("[crawl.verify] sites: no rule for {known}"))?;
            if disabled.iter().any(|off| off.eq_ignore_ascii_case(known)) {
                log::info!("verify: {known} is in [crawl] disabled_sites, left out");
                continue;
            }
            sites.push((index, *known, rule));
        }

        Ok(Self { engine, config, sites, hub: None })
    }

    pub fn with_hub(mut self, hub: Arc<crate::dist::Hub>, workers: usize) -> Self {
        self.hub = Some((hub, workers.max(1)));
        self
    }

    /// The names of the sites that this verification looks at.
    pub fn site_names(&self) -> Vec<&'static str> {
        self.sites.iter().map(|(_, name, _)| *name).collect()
    }

    pub async fn run(self: Arc<Self>) {
        log::info!("verify: {} sites, every page once in {} days", self.sites.len(), self.config.interval_days);

        let mut tasks = Vec::new();
        for &(index, name, rule) in &self.sites {
            tasks.push(tokio::spawn(self.clone().site(index, name, rule)));
        }
        for task in tasks {
            let _ = task.await;
        }
    }

    /// The loop of one site.
    async fn site(self: Arc<Self>, index: usize, name: &'static str, rule: GoneRule) {
        let mut works = 0u64;
        let mut batches = 0u64;
        // where the walk over the paths of the site is (the due pages are taken in the order of the paths, no sort of the whole site)
        let mut after = String::new();

        loop {
            // the number of works changes slowly (the crawl): it is counted again now and then
            if batches % 20 == 0 {
                works = self.engine.store().counts_of(name).await.unwrap_or(works).max(1);
            }
            batches += 1;

            match self.batch(index, name, rule, works, &mut after).await {
                Ok(0) => tokio::time::sleep(Duration::from_secs(3600)).await,
                Ok(_) => {}
                Err(e) => {
                    log::warn!("verify {name}: {e}, again in 10 minutes");
                    tokio::time::sleep(Duration::from_secs(600)).await;
                }
            }
        }
    }

    /// Looks at the pages of `name` that are due, `BATCH` of them at the most. Returns how many were looked at.
    async fn batch(&self, index: usize, name: &'static str, rule: GoneRule, works: u64, after: &mut String) -> Result<usize> {
        let before = now() - self.config.interval_days.max(1) as i64 * 86_400;
        // with workers, a batch for each of them is taken at once
        let count = BATCH * self.hub.as_ref().map_or(1, |(_, workers)| *workers as u64);
        let mut due = self.engine.store().due_checks(name, before, after, count).await?;
        if (due.len() as u64) < count && !after.is_empty() {
            // the end of the paths: go on from the first one again
            let more = self.engine.store().due_checks(name, before, "", count - due.len() as u64).await?;
            *after = more.last().cloned().unwrap_or_default();
            due.extend(more);
        } else {
            *after = due.last().cloned().unwrap_or_default();
        }
        let spacing = Duration::from_millis((self.config.interval_days.max(1) * 86_400_000 / works).max(self.config.min_pause_ms));
        let mut results = Vec::new();

        if let Some((hub, workers)) = &self.hub {
            // each worker looks at its part of the batch, as far apart as one machine would, so the site sees one page in `spacing` from each of them
            // and the whole batch takes a worker's share of the time
            let size = due.len().div_ceil(*workers).max(1);
            let mut jobs = tokio::task::JoinSet::new();
            for part in due.chunks(size) {
                let (hub, part) = (hub.clone(), part.to_vec());
                jobs.spawn(async move { hub.submit(name, crate::dist::Job::Probe { paths: part, spacing_ms: spacing.as_millis() as u64 }).await });
            }
            while let Some(done) = jobs.join_next().await {
                if let Ok(Ok(crate::dist::Done::Probe(part))) = done {
                    results.extend(part);
                }
            }
            let removed = results.iter().filter(|(_, gone)| *gone).count();
            self.engine.store().record_checks(name, &results, now()).await?;
            if removed > 0 {
                log::info!("verify {name}: {} pages looked at, {removed} gone", results.len());
            }

            return Ok(results.len());
        }

        for path in due {
            let started = Instant::now();
            let url = self.engine.site(index).address(&path);

            // a page that cannot be read now is looked at again by a later batch
            if let Some(gone) = self.engine.probe(index, &url, rule).await {
                results.push((path, gone));
            }

            // the pages of the site are spread over the whole interval
            if let Some(rest) = spacing.checked_sub(started.elapsed()) {
                tokio::time::sleep(rest).await;
            }
        }

        let removed = results.iter().filter(|(_, gone)| *gone).count();
        self.engine.store().record_checks(name, &results, now()).await?;

        if removed > 0 {
            log::info!("verify {name}: {} pages looked at, {removed} gone", results.len());
        }

        Ok(results.len())
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}
