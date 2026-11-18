//! Google Translate, the way its web page asks for a translation: the RPC `MkEWBc` of
//! `translate.google.com/_/TranslateWebserverUi/data/batchexecute`, with the model that the page calls 高度 / Advanced (the one
//! that is "Gemini による正確性重視の翻訳"; the request has a `1` where the classic model has a `2`).
//!
//! This is the page's own request, not an API: it has no key, no promise, and Google can change it. The caller treats a failure as
//! "no translation". Seen in 2026-11: a title of one line comes back the same from both models.

use crate::engine::Lang;

const URL: &str = "https://translate.google.com/_/TranslateWebserverUi/data/batchexecute?rpcids=MkEWBc&source-path=%2F&hl=en&rt=c";

/// The model of the page that is "Advanced" (Gemini).
const MODEL_ADVANCED: u8 = 1;

/// The value of the form field `f.req`.
pub fn request(text: &str, from: Lang, to: Lang) -> String {
    let inner = serde_json::json!([[text, from.code(), to.code(), 1, null, MODEL_ADVANCED], []]).to_string();

    serde_json::json!([[["MkEWBc", inner, null, "generic"]]]).to_string()
}

/// The translation in the answer: the text of every sentence, joined. None when the answer has none.
pub fn parse(body: &str) -> Option<String> {
    for line in body.lines() {
        let Ok(serde_json::Value::Array(entries)) = serde_json::from_str::<serde_json::Value>(line) else { continue };

        for entry in entries {
            if entry.get(0).and_then(|v| v.as_str()) != Some("wrb.fr") || entry.get(1).and_then(|v| v.as_str()) != Some("MkEWBc") {
                continue;
            }
            let Some(inner) = entry.get(2).and_then(|v| v.as_str()) else { continue };
            let Ok(inner) = serde_json::from_str::<serde_json::Value>(inner) else { continue };

            let sentences = inner.get(1)?.get(0)?.get(0)?.get(5)?.as_array()?;
            let text: String = sentences.iter().filter_map(|s| s.get(0)?.as_str()).collect::<Vec<_>>().join(" ");
            let text = text.trim().to_string();

            return (!text.is_empty()).then_some(text);
        }
    }

    None
}

/// The least time between two requests to Google Translate (the server is open: `live=force` must not make it a relay).
const MIN_DELAY: std::time::Duration = std::time::Duration::from_millis(1000);

static LAST: tokio::sync::Mutex<Option<tokio::time::Instant>> = tokio::sync::Mutex::const_new(None);

/// Waits for the turn: the lock is held while sleeping, so the requests of all tasks line up (and no thread is blocked).
async fn wait_turn() {
    let mut last = LAST.lock().await;

    if let Some(wait) = last.and_then(|at| MIN_DELAY.checked_sub(at.elapsed())) {
        tokio::time::sleep(wait).await;
    }
    *last = Some(tokio::time::Instant::now());
}

/// Asks Google Translate.
pub async fn translate(client: &reqwest::Client, text: &str, from: Lang, to: Lang) -> Result<String, String> {
    wait_turn().await;

    let failed = |e: reqwest::Error| format!("Google Translate: {e}");
    let body = client
        .post(URL)
        .form(&[("f.req", request(text, from, to))])
        .send()
        .await
        .map_err(failed)?
        .text()
        .await
        .map_err(failed)?;

    parse(&body).ok_or_else(|| "Google Translate: no translation in the answer".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_request_has_the_text_the_languages_and_the_advanced_model() {
        assert_eq!(
            request("姉弟", Lang::Japanese, Lang::English),
            r#"[[["MkEWBc","[[\"姉弟\",\"ja\",\"en\",1,null,1],[]]",null,"generic"]]]"#
        );
    }

    /// Asks Google Translate (the network): `cargo test -- --ignored`.
    ///
    /// Only the Advanced (Gemini) model is used, and this is how it is known to work: it says `The Phaethon Assault` for this title
    /// (the classic model says `The Battle of Phaeton`). Any other answer means the page changed (the request, the model, or what the
    /// model says), and the code of this file has to be looked at again.
    #[tokio::test]
    #[ignore = "asks Google Translate"]
    async fn the_advanced_model_translates_paeton_kyoushuusen_the_way_gemini_does() {
        let client = reqwest::Client::new();
        let translated = translate(&client, "パエトーン強襲戦", Lang::Japanese, Lang::English).await.unwrap();

        assert_eq!(translated, "The Phaethon Assault", "Google Translate changed: it is not the answer of the Advanced (Gemini) model any more");
    }

    #[test]
    fn the_translation_is_read_from_the_answer() {
        let answer = ")]}'\n\n421\n[[\"wrb.fr\",\"MkEWBc\",\"[[null,null,null,[[[0,[[[null,8]],[true]]]],8],null,null,[\\\"パエトーン強襲戦\\\",\\\"ja\\\",\\\"en\\\",true,null,1]],[[[null,null,null,null,null,[[\\\"Assault on Phaeton\\\",null,null,null,null,null,\\\"パエトーン強襲戦\\\",1]],null,null,null,[]]],\\\"en\\\",1,\\\"ja\\\",[\\\"パエトーン強襲戦\\\",\\\"ja\\\",\\\"en\\\",true,null,1]],\\\"ja\\\",null,null,null,null,[[[0],[[6,1]]]]]\",null,null,null,\"generic\"],[\"di\",382]]\n25\n[[\"e\",4,null,null,505]]\n";

        assert_eq!(parse(answer).as_deref(), Some("Assault on Phaeton"));
        assert_eq!(parse(")]}'\n\n25\n[[\"e\",4,null,null,505]]\n"), None);
        assert_eq!(parse(""), None);
    }
}
