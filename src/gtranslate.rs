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

/// What the model that made the translation is called in the answer (`[7][0][0][0]`): 3 for the Advanced model, 0 for the classic one. The
/// classic one answers when Google does not honour the token (a headless Chrome on Linux makes such tokens about every other time).
const MODEL_MARK_CLASSIC: i64 = 0;

/// How many times a text is asked when the classic model answers it: a new token is made for each try, and it may be honoured.
const ATTEMPTS: usize = 3;

/// A translation, and whether the classic model made it (the token was not honoured). The answer does not say it when Google changes it:
/// then the translation is taken as it is.
#[derive(Debug, PartialEq, Eq)]
pub struct Answer {
    pub text: String,
    pub classic: bool,
}

/// The translation in the answer: the text of every sentence, joined. None when the answer has none.
pub fn parse_answer(body: &str) -> Option<Answer> {
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
            let classic = inner.get(7).and_then(|v| v.get(0)?.get(0)?.get(0)?.as_i64()) == Some(MODEL_MARK_CLASSIC);

            return (!text.is_empty()).then_some(Answer { text, classic });
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

/// Where the token is made: `tools/google-translate-bridge` (a Chrome), or nowhere (the url is empty).
#[derive(Debug, Clone, Copy, Default)]
pub struct Bridge<'a> {
    pub url: &'a str,
    pub token: &'a str,
}

/// The request that the page of Google Translate made for a text, held back by the bridge.
#[derive(Debug, serde::Deserialize)]
struct PageRequest {
    url: String,
    headers: std::collections::HashMap<String, String>,
    body: String,
}

#[derive(Debug, serde::Deserialize)]
struct Token {
    model: u8,
    request: PageRequest,
}

/// The headers of the page's request that the token is made for (the browser makes the rest, and the token is checked against these).
const TOKEN_HEADERS: [&str; 5] = ["x-goog-batchexecute-bgr", "content-type", "x-same-domain", "user-agent", "referer"];

/// How long the bridge is waited for. It answers in a few seconds (it takes the texts one at a time, with a pause between two); a bridge that
/// does not answer is a bridge that is stuck, and a search must not wait a minute for it: the translation is left out and the search goes on.
const BRIDGE_TIMEOUT_SECS: u64 = 20;

/// The request of the page for this text, with its token, from the bridge.
async fn token(client: &reqwest::Client, bridge: Bridge<'_>, text: &str, from: Lang, to: Lang) -> Result<Token, String> {
    let mut request = client
        .get(format!("{}/token", bridge.url.trim_end_matches('/')))
        .query(&[("from", from.code()), ("to", to.code()), ("text", text)])
        .timeout(std::time::Duration::from_secs(BRIDGE_TIMEOUT_SECS));
    if !bridge.token.is_empty() {
        request = request.bearer_auth(bridge.token);
    }

    let response = request.send().await.map_err(|e| format!("the bridge of Google Translate: {e}"))?;
    let status = response.status();

    if !status.is_success() {
        let said = response.text().await.unwrap_or_default();

        return Err(format!("the bridge of Google Translate answered {status}: {}", said.chars().take(200).collect::<String>()));
    }

    let token: Token = response.json().await.map_err(|e| format!("the bridge of Google Translate: {e}"))?;

    if token.model != MODEL_ADVANCED {
        return Err(format!("the bridge made the request of the model {}, not the Advanced one", token.model));
    }

    Ok(token)
}

/// The text as it is sent to Google Translate. A text with no white space and with dots (`FapHouse.NuCosplay.25.02.28.Maid.in.a.Room`, the
/// name of a torrent) is taken for an address: the answer is a link to translate the page, with no translation in it. The dots are spaces
/// then. Only what is sent changes: the title and the key of the translation are the ones that were asked.
fn sent_text(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains('.') && !text.contains(char::is_whitespace) {
        let spaced = text.replace('.', " ").split_whitespace().collect::<Vec<_>>().join(" ");

        // a text of dots only has nothing else to send
        if !spaced.is_empty() {
            return std::borrow::Cow::Owned(spaced);
        }
    }

    std::borrow::Cow::Borrowed(text)
}

/// Asks Google Translate. With a bridge, the request is the one that the page made (the browser only made its token, and its own
/// request was held back), sent from here; without one it is the request of this program, which has no token and gets the answer of
/// the classic model.
pub async fn translate(client: &reqwest::Client, bridge: Bridge<'_>, text: &str, from: Lang, to: Lang) -> Result<String, String> {
    // without a bridge the classic model is the answer that is asked for
    let attempts = if bridge.url.is_empty() { 1 } else { ATTEMPTS };
    let sent = sent_text(text);
    let text = sent.as_ref();
    let mut answer = ask(client, bridge, text, from, to).await?;

    for attempt in 2..=attempts {
        if !answer.classic {
            break;
        }
        log::info!("Google Translate: the classic model answered {text:?} (the token was not honoured): try {attempt} of {attempts}");
        answer = ask(client, bridge, text, from, to).await?;
    }

    if answer.classic && !bridge.url.is_empty() {
        log::warn!("Google Translate: the classic model answered {text:?} {attempts} times: its answer is taken");
    }

    Ok(answer.text)
}

/// One request to Google Translate, with a new token from the bridge.
async fn ask(client: &reqwest::Client, bridge: Bridge<'_>, text: &str, from: Lang, to: Lang) -> Result<Answer, String> {
    let failed = |e: reqwest::Error| format!("Google Translate: {e}");

    wait_turn().await;

    let response = if bridge.url.is_empty() {
        client.post(URL).form(&[("f.req", request(text, from, to))]).send().await.map_err(failed)?
    } else {
        let page = token(client, bridge, text, from, to).await?.request;
        let mut sent = client.post(&page.url).body(page.body);

        for (name, value) in &page.headers {
            if TOKEN_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
                sent = sent.header(name, value);
            }
        }

        sent.send().await.map_err(failed)?
    };

    let body = response.text().await.map_err(failed)?;

    parse_answer(&body).ok_or_else(|| "Google Translate: no translation in the answer".to_string())
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

    #[test]
    fn a_text_of_dots_with_no_space_is_sent_with_spaces_and_any_other_text_as_it_is() {
        assert_eq!(sent_text("FapHouse.NuCosplay.25.02.28.藤野りん.Horny.Maid.in.a.Hotel.Room"), "FapHouse NuCosplay 25 02 28 藤野りん Horny Maid in a Hotel Room");
        assert_eq!(sent_text("a..b."), "a b");
        assert_eq!(sent_text("Ver.1.0 の話"), "Ver.1.0 の話", "a text with a space is a text");
        assert_eq!(sent_text("姉弟"), "姉弟");
        assert_eq!(sent_text("..."), "...", "nothing else to send");
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
        // the token needs the bridge: `OKAZURI_GOOGLE_BRIDGE=http://127.0.0.1:8130` (and `OKAZURI_GOOGLE_BRIDGE_TOKEN`)
        let (url, secret) = (std::env::var("OKAZURI_GOOGLE_BRIDGE").unwrap_or_default(), std::env::var("OKAZURI_GOOGLE_BRIDGE_TOKEN").unwrap_or_default());
        let translated = translate(&client, Bridge { url: &url, token: &secret }, "パエトーン強襲戦", Lang::Japanese, Lang::English).await.unwrap();

        assert_eq!(translated, "The Phaethon Assault", "Google Translate changed: it is not the answer of the Advanced (Gemini) model any more");
    }

    /// A bridge and a "Google" on one local server: the request that the bridge hands over is sent by `translate`, with the headers of the
    /// token and no others of the page, and the answer of Google is the translation.
    #[tokio::test]
    async fn the_request_of_the_page_is_sent_from_here_with_its_token() {
        use axum::extract::Query;
        use axum::http::HeaderMap;
        use axum::routing::{get, post};
        use axum::{Json, Router};
        use std::collections::HashMap;

        let answer = ")]}'\n\n100\n[[\"wrb.fr\",\"MkEWBc\",\"[[null,null,null,null,null,null,[\\\"姉弟\\\",\\\"ja\\\",\\\"en\\\",true,null,1]],[[[null,null,null,null,null,[[\\\"Siblings\\\",null,null,null,null,null,\\\"姉弟\\\",1]],null,null,null,[]]],\\\"en\\\",1,\\\"ja\\\"],\\\"ja\\\",null,null,null,null,[[[3],[[6,3]]]]]\",null,null,null,\"generic\"]]\n";
        let router = Router::new()
            .route(
                "/token",
                get(|headers: HeaderMap, Query(asked): Query<HashMap<String, String>>| async move {
                    let host = headers["host"].to_str().unwrap().to_string();
                    assert_eq!(headers["authorization"], "Bearer secret");
                    assert_eq!((asked["from"].as_str(), asked["to"].as_str(), asked["text"].as_str()), ("ja", "en", "姉弟"));

                    Json(serde_json::json!({
                        "model": 1,
                        "request": {
                            "url": format!("http://{host}/google"),
                            "headers": { "x-goog-batchexecute-bgr": "TOKEN", "Referer": "https://translate.google.com/", "User-Agent": "Chrome", "cookie": "never sent" },
                            "body": "f.req=BODY&"
                        }
                    }))
                }),
            )
            .route(
                "/google",
                post(move |headers: HeaderMap, body: String| async move {
                    assert_eq!(headers["x-goog-batchexecute-bgr"], "TOKEN");
                    assert_eq!(headers["user-agent"], "Chrome");
                    assert!(headers.get("cookie").is_none(), "only the headers of the token are sent");
                    assert_eq!(body, "f.req=BODY&");

                    answer
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let translated = translate(&reqwest::Client::new(), Bridge { url: &url, token: "secret" }, "姉弟", Lang::Japanese, Lang::English).await.unwrap();

        assert_eq!(translated, "Siblings");
    }

    #[tokio::test]
    async fn a_bridge_that_is_down_is_an_error_not_the_classic_answer() {
        let error = translate(&reqwest::Client::new(), Bridge { url: "http://127.0.0.1:1", token: "" }, "姉弟", Lang::Japanese, Lang::English).await.unwrap_err();

        assert!(error.contains("the bridge of Google Translate"), "{error}");
    }

    #[test]
    fn the_translation_is_read_from_the_answer() {
        let answer = ")]}'\n\n421\n[[\"wrb.fr\",\"MkEWBc\",\"[[null,null,null,[[[0,[[[null,8]],[true]]]],8],null,null,[\\\"パエトーン強襲戦\\\",\\\"ja\\\",\\\"en\\\",true,null,1]],[[[null,null,null,null,null,[[\\\"Assault on Phaeton\\\",null,null,null,null,null,\\\"パエトーン強襲戦\\\",1]],null,null,null,[]]],\\\"en\\\",1,\\\"ja\\\",[\\\"パエトーン強襲戦\\\",\\\"ja\\\",\\\"en\\\",true,null,1]],\\\"ja\\\",null,null,null,null,[[[0],[[6,1]]]]]\",null,null,null,\"generic\"],[\"di\",382]]\n25\n[[\"e\",4,null,null,505]]\n";

        // the mark of this answer is 0: the classic model made it
        assert_eq!(parse_answer(answer), Some(Answer { text: "Assault on Phaeton".into(), classic: true }));
        assert_eq!(parse_answer(")]}'\n\n25\n[[\"e\",4,null,null,505]]\n"), None);
        assert_eq!(parse_answer(""), None);
    }

    /// A bridge and a "Google" on one local server, where the first `classic` answers are the classic model's (the mark 0), and then the
    /// Advanced model's (the mark 3). Returns the address and the number of requests that "Google" got.
    async fn google(classic: usize) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use axum::extract::State;
        use axum::http::HeaderMap;
        use axum::routing::{get, post};
        use axum::{Json, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let answer = |mark: u8, text: &str| format!(")]}}'\n\n100\n[[\"wrb.fr\",\"MkEWBc\",\"[[null,null,null,null,null,null,[\\\"姉弟\\\",\\\"ja\\\",\\\"en\\\",true,null,1]],[[[null,null,null,null,null,[[\\\"{text}\\\",null,null,null,null,null,\\\"姉弟\\\",1]],null,null,null,[]]],\\\"en\\\",1,\\\"ja\\\"],\\\"ja\\\",null,null,null,null,[[[{mark}],[[6,1]]]]]\",null,null,null,\"generic\"]]\n");
        let (classic_answer, advanced_answer) = (answer(0, "Siblings (classic)"), answer(3, "Siblings"));
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = asked.clone();
        let router = Router::new()
            .route(
                "/token",
                get(|headers: HeaderMap| async move {
                    let host = headers["host"].to_str().unwrap().to_string();

                    Json(serde_json::json!({ "model": 1, "request": { "url": format!("http://{host}/google"), "headers": {}, "body": "f.req=BODY&" } }))
                }),
            )
            .route(
                "/google",
                post(move |State(counter): State<Arc<AtomicUsize>>| async move {
                    if counter.fetch_add(1, Ordering::SeqCst) < classic { classic_answer.clone() } else { advanced_answer.clone() }
                }),
            )
            .with_state(counter);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        (url, asked)
    }

    #[tokio::test]
    async fn a_text_that_the_classic_model_answered_is_asked_again_with_a_new_token() {
        let (url, asked) = google(2).await;

        let translated = translate(&reqwest::Client::new(), Bridge { url: &url, token: "" }, "姉弟", Lang::Japanese, Lang::English).await.unwrap();

        assert_eq!(translated, "Siblings", "the third try is the Advanced model's");
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn the_classic_answer_is_taken_after_the_last_try() {
        let (url, asked) = google(usize::MAX).await;

        let translated = translate(&reqwest::Client::new(), Bridge { url: &url, token: "" }, "姉弟", Lang::Japanese, Lang::English).await.unwrap();

        assert_eq!(translated, "Siblings (classic)");
        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), ATTEMPTS);
    }

    #[tokio::test]
    async fn an_advanced_answer_is_asked_once() {
        let (url, asked) = google(0).await;

        translate(&reqwest::Client::new(), Bridge { url: &url, token: "" }, "姉弟", Lang::Japanese, Lang::English).await.unwrap();

        assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}
