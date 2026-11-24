//! Translating the Japanese titles of the saved works ahead of time with Google Translate (`[translation.fill]`).
//!
//! A result shows the English title of the work (`Hit::title_english`) and a search in English finds a work by it only when the
//! title has a translation saved. Hitomi.La's titles have one (the work has both). The titles of the other sites have none until a
//! text is translated, so this goes through the works, the newest first, and asks Google Translate (`Engine::translate_google`,
//! saved for good, source `google`) for the title of every work that has no translation. One title is asked for at a time, with
//! `pause_ms` after it: the bridge (`tools/google-translate-bridge`) answers a search with the same Chrome.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::config::FillConfig;
use crate::engine::{Engine, Lang, Live};
use crate::hitomi::{squash, title_pair};
use crate::query::{has_japanese, work_title};
use crate::utils::Result;

/// How many titles are read before the ones that are not translated are asked for.
const PAGE: u64 = 500;

/// The most characters the bridge of Google Translate takes in a text (`MAX_TEXT_CHARS` of `tools/google-translate-bridge/server.mjs`): a longer
/// title (E-Hentai's have the tags of the work after the title) is answered with a 422. That is not Google failing, so it is not asked at all;
/// asked, five of them in a row stopped the round, and the newest titles are such ones.
const MAX_TEXT_CHARS: usize = 300;

/// A round stops when Google does not answer this many times in a row.
const MAX_FAILURES_IN_A_ROW: usize = 5;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Round {
    /// The titles that were read.
    pub looked: usize,
    /// The titles that were translated now.
    pub translated: usize,
    /// The titles Google had no answer for.
    pub failed: usize,
    /// The round stopped because `max_per_round` titles were asked for (the others wait for the next round), not because it was at the end.
    pub more: bool,
}

pub struct TitleTranslator {
    engine: Arc<Engine>,
    config: FillConfig,
}

impl TitleTranslator {
    pub fn new(engine: Arc<Engine>, config: FillConfig) -> Self {
        Self { engine, config }
    }

    pub async fn run(self: Arc<Self>) {
        log::info!("fill: the titles are translated, {} ms apart, {} in a round", self.config.pause_ms, self.config.max_per_round);
        // a title that Google gave no answer for is not asked again until the program starts again
        let mut failed: HashSet<String> = HashSet::new();

        loop {
            let more = match self.round(&mut failed).await {
                Ok(round) => {
                    log::info!(
                        "fill: {} titles looked at, {} translated, {} without an answer{}",
                        round.looked,
                        round.translated,
                        round.failed,
                        if round.more { ", more to do" } else { "" }
                    );
                    round.more
                }
                Err(e) => {
                    log::warn!("fill: {e}");
                    false
                }
            };

            if !more {
                tokio::time::sleep(Duration::from_secs(self.config.round_interval_minutes.max(1) * 60)).await;
            }
        }
    }

    /// The texts of `titles` that Google is to translate: the ones with Japanese letters and no English title yet (`title_pair`, or
    /// a saved translation), each once, without the ones in `failed`. Each is `(text, key)`: the text as it is asked for and its key.
    pub async fn missing(&self, titles: &[String], failed: &HashSet<String>) -> Result<Vec<String>> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut wanted: Vec<(String, String)> = Vec::new();

        for title in titles {
            if !has_japanese(title) || title_pair(title).is_some() {
                continue;
            }

            let text = work_title(title, true);
            let key = squash(&text);

            if key.is_empty() || !has_japanese(&text) || text.chars().count() > MAX_TEXT_CHARS || failed.contains(&key) || !seen.insert(key.clone()) {
                continue;
            }
            wanted.push((text, key));
        }

        if wanted.is_empty() {
            return Ok(Vec::new());
        }

        let keys: Vec<String> = wanted.iter().map(|(_, key)| key.clone()).collect();
        let known = self.engine.store().english_titles(&keys).await?;

        Ok(wanted.into_iter().filter(|(_, key)| !known.contains_key(key)).map(|(text, _)| text).collect())
    }

    /// Goes through the works from the newest and translates the titles that have no translation, until `max_per_round` are asked for or
    /// the works are at their end.
    pub async fn round(&self, failed: &mut HashSet<String>) -> Result<Round> {
        let mut round = Round::default();
        let mut before: Option<(i64, i64)> = None;
        let mut in_a_row = 0;

        loop {
            let page = self.engine.store().japanese_titles(before, PAGE).await?;
            let Some(&(updated_at, rowid, _)) = page.last() else { return Ok(round) };
            before = Some((updated_at, rowid));
            round.looked += page.len();

            let titles: Vec<String> = page.into_iter().map(|(_, _, title)| title).collect();

            for text in self.missing(&titles, failed).await? {
                if round.translated + round.failed >= self.config.max_per_round {
                    round.more = true;
                    return Ok(round);
                }

                match self.engine.translate_google(&text, Lang::Japanese, Live::Diff).await {
                    Ok((translation, saved)) => {
                        in_a_row = 0;
                        if !saved {
                            round.translated += 1;
                            text_log!(debug, "fill: {text:?} -> {:?}", translation.english; "fill: a title translated");
                            tokio::time::sleep(Duration::from_millis(self.config.pause_ms)).await;
                        }
                    }
                    Err(e) => {
                        text_log!(warn, "fill: {text:?}: {e}"; "fill: a title failed: {e}");
                        failed.insert(squash(&text));
                        round.failed += 1;
                        in_a_row += 1;

                        if in_a_row >= MAX_FAILURES_IN_A_ROW {
                            log::warn!("fill: {MAX_FAILURES_IN_A_ROW} titles in a row without an answer: the round stops");
                            return Ok(round);
                        }
                        tokio::time::sleep(Duration::from_millis(self.config.pause_ms)).await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SearchConfig;
    use crate::entity::translation;
    use crate::store::SearchStore;

    async fn translator() -> (Arc<SearchStore>, TitleTranslator) {
        let store = Arc::new(SearchStore::in_memory().await.unwrap());
        let engine = Engine::new(&SearchConfig::default(), store.clone(), crate::testing::romanizer_arc());

        (store, TitleTranslator::new(engine, FillConfig::default()))
    }

    fn google(japanese: &str, english: &str) -> translation::Model {
        translation::Model {
            key: squash(japanese),
            gallery_id: 0,
            position: 0,
            url: String::new(),
            english: english.into(),
            japanese: japanese.into(),
            english_key: squash(english),
            japanese_key: squash(japanese),
            exact: false,
            source: "google".into(),
        }
    }

    #[tokio::test]
    async fn only_a_japanese_title_without_a_translation_is_to_be_translated() {
        let (store, fill) = translator().await;
        store.save_translations(&squash("翻訳済み"), "google", "翻訳済み", "ja", 1, &[google("翻訳済み", "Translated")]).await.unwrap();

        let long = format!("長い題名 {}", "x".repeat(MAX_TEXT_CHARS));
        let titles: Vec<String> = [
            "翻訳済み",                          // translated already
            "未翻訳 (Series) [English]",         // the title alone is asked for
            "未翻訳",                            // the same title: asked once
            "Latin only",                        // nothing to translate
            "Both | りょうほう",                 // the work has its English title
            "失敗",                              // Google had no answer for it
            &long,                               // more than the bridge takes: not asked
        ]
        .map(String::from)
        .to_vec();
        let failed: HashSet<String> = [squash("失敗")].into();

        assert_eq!(fill.missing(&titles, &failed).await.unwrap(), ["未翻訳"]);
    }

    #[tokio::test]
    async fn a_round_over_titles_that_are_all_translated_asks_for_nothing() {
        let (store, fill) = translator().await;
        store.save("q", "A", 100, &Ok(vec![crate::store::NewWork { path: "/g/1/".into(), title: "翻訳済み".into(), romaji: "honyakuzumi".into(), language: None, extra: Default::default() }]), false).await.unwrap();
        store.save_translations(&squash("翻訳済み"), "google", "翻訳済み", "ja", 1, &[google("翻訳済み", "Translated")]).await.unwrap();

        let round = fill.round(&mut HashSet::new()).await.unwrap();

        assert_eq!(round, Round { looked: 1, translated: 0, failed: 0, more: false });
    }
}
