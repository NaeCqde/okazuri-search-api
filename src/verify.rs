//! Looking at the page of every saved work of some sites, once in `interval_days`, and removing the works whose page is gone (`[verify]`).
//!
//! Only the sites whose pages say that a work is gone can be asked (`Engine::probe`: RokuHentai answers 503 with "temporarily
//! unavailable ... Here are some results that might be useful to you"). The site still lists such a work, so the work is not only removed:
//! `works.deleted` marks it (the row stays, with no title), and a list, the crawl or an import does not save it again.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::VerifyConfig;
use crate::engine::Engine;
use crate::utils::Result;

/// The sites whose pages tell that a work is gone.
const SITES: &[&str] = &["RokuHentai"];

/// How many pages are looked at before the answers are written.
const BATCH: u64 = 50;

pub struct Verifier {
    engine: Arc<Engine>,
    config: VerifyConfig,
    /// (index of the site in the engine, its name)
    sites: Vec<(usize, &'static str)>,
}

impl Verifier {
    /// An unknown site, or one whose pages do not tell, is an error.
    pub fn new(engine: Arc<Engine>, config: VerifyConfig) -> Result<Self> {
        let names: Vec<String> = if config.sites.is_empty() { SITES.iter().map(|s| s.to_string()).collect() } else { config.sites.clone() };
        let mut sites = Vec::new();

        for name in names {
            let Some(known) = SITES.iter().find(|known| **known == name) else {
                return Err(format!("[verify] sites: {name} is not a site whose pages tell that a work is gone ({})", SITES.join(", ")).into());
            };
            let index = engine.site_named(known).ok_or_else(|| format!("[verify] sites: {known} is not one of the sites of this program"))?;
            sites.push((index, *known));
        }

        Ok(Self { engine, config, sites })
    }

    pub async fn run(self: Arc<Self>) {
        log::info!("verify: {} sites, every page once in {} days", self.sites.len(), self.config.interval_days);

        loop {
            let looked = match self.round().await {
                Ok(looked) => looked,
                Err(e) => {
                    log::warn!("verify: {e}, again in 10 minutes");
                    tokio::time::sleep(Duration::from_secs(600)).await;
                    continue;
                }
            };

            // nothing is due: the next page falls due later
            if looked == 0 {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        }
    }

    /// Looks at the pages that are due, a batch of each site. Returns how many.
    async fn round(&self) -> Result<usize> {
        let mut looked = 0;

        for &(index, name) in &self.sites {
            let before = now() - self.config.interval_days.max(1) as i64 * 86_400;
            let due = self.engine.store().due_checks(name, before, BATCH).await?;
            let mut results = Vec::new();

            for path in due {
                let url = self.engine.site(index).address(&path);

                // a page that cannot be read now is looked at again by the next round
                if let Some(gone) = self.engine.probe(index, &url).await {
                    results.push((path, gone));
                }
                tokio::time::sleep(Duration::from_millis(self.config.pause_ms)).await;
            }

            looked += results.len();
            let removed = results.iter().filter(|(_, gone)| *gone).count();
            self.engine.store().record_checks(name, &results, now()).await?;

            if removed > 0 {
                log::info!("verify {name}: {} pages looked at, {removed} gone and removed", results.len());
            }
        }

        Ok(looked)
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}
