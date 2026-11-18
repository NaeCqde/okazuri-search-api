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

static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);

fn wait_turn() {
    let mut last = LAST.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    if let Some(wait) = last.and_then(|at| MIN_DELAY.checked_sub(at.elapsed())) {
        std::thread::sleep(wait);
    }
    *last = Some(std::time::Instant::now());
}

/// Asks Google Translate.
pub fn translate(agent: &ureq::Agent, text: &str, from: Lang, to: Lang) -> Result<String, String> {
    wait_turn();
    let mut response = agent
        .post(URL)
        .send_form([("f.req", request(text, from, to).as_str())])
        .map_err(|e| format!("Google Translate: {e}"))?;
    let body = response.body_mut().read_to_string().map_err(|e| format!("Google Translate: {e}"))?;

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
    /// The title of a work that was translated by hand is the proof of the model: the Advanced (Gemini) model of the page says
    /// `The Phaethon Assault`, and the classic model says `The Battle of Phaeton`.
    #[test]
    #[ignore = "asks Google Translate"]
    fn the_advanced_model_translates_paeton_kyoushuusen_the_way_gemini_does() {
        let agent = ureq::Agent::new_with_defaults();
        let translated = translate(&agent, "パエトーン強襲戦", Lang::Japanese, Lang::English).unwrap();

        assert_ne!(translated, "The Battle of Phaeton", "this is the classic model");
        assert_eq!(translated, "The Phaethon Assault", "the Advanced (Gemini) model says this; the classic one says The Battle of Phaeton");
    }

    #[test]
    fn the_translation_is_read_from_the_answer() {
        let answer = ")]}'\n\n421\n[[\"wrb.fr\",\"MkEWBc\",\"[[null,null,null,[[[0,[[[null,8]],[true]]]],8],null,null,[\\\"パエトーン強襲戦\\\",\\\"ja\\\",\\\"en\\\",true,null,1]],[[[null,null,null,null,null,[[\\\"Assault on Phaeton\\\",null,null,null,null,null,\\\"パエトーン強襲戦\\\",1]],null,null,null,[]]],\\\"en\\\",1,\\\"ja\\\",[\\\"パエトーン強襲戦\\\",\\\"ja\\\",\\\"en\\\",true,null,1]],\\\"ja\\\",null,null,null,null,[[[0],[[6,1]]]]]\",null,null,null,\"generic\"],[\"di\",382]]\n25\n[[\"e\",4,null,null,505]]\n";

        assert_eq!(parse(answer).as_deref(), Some("Assault on Phaeton"));
        assert_eq!(parse(")]}'\n\n25\n[[\"e\",4,null,null,505]]\n"), None);
        assert_eq!(parse(""), None);
    }
}
