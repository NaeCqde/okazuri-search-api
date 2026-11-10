//! The HTTP side.
//!
//! - `GET /romaji?q=`: the romaji of the text as `text/plain`. This is what
//!   `okazuri_bot` called when it was the romaji API alone (`src/romaji.ts`).
//! - `GET /okazuri/search?q=` (`[search] enabled`): the works found on the sites for the query, as JSON.
//! - `GET /okazuri/sites` (`[search] enabled`): the sites, and whether each can be searched.
//! - `GET /health`: `ok`.
//!
//! A function that is off has no route, so it answers 404. A bad request is a 422 with
//! `{"detail": "..."}`, as the FastAPI server this started from answered.

use crate::engine::{Engine, Live, Request};
use crate::romanizer::{Loanwords, Romanizer};
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use std::sync::Arc;

pub struct AppState {
    /// The Japanese to romaji conversion. It is always there: it has no switch.
    pub romanizer: Arc<Romanizer>,
    /// The longest `q` of `/romaji`, in characters.
    pub max_query_chars: usize,
    /// The search engine; `None` when `[search]` is off.
    pub search: Option<Arc<Engine>>,
}

pub fn router(state: Arc<AppState>) -> Router {
    let mut router = Router::new().route("/health", get(health)).route("/romaji", get(romaji));

    if state.search.is_some() {
        router = router.route("/okazuri/search", get(search)).route("/okazuri/sites", get(sites));
    }

    router.with_state(state)
}

#[derive(Debug, Deserialize)]
struct RomajiQuery {
    q: Option<String>,
    loanwords: Option<String>,
}

async fn romaji(State(state): State<Arc<AppState>>, Query(query): Query<RomajiQuery>) -> Response {
    let romanizer = &state.romanizer;
    let text = query.q.unwrap_or_default();
    let length = text.chars().count();

    if length == 0 {
        return invalid("q is required (at least 1 character)");
    }
    if length > state.max_query_chars {
        return invalid(&format!("q is too long (at most {} characters)", state.max_query_chars));
    }

    let loanwords = match query.loanwords.as_deref() {
        None | Some("romaji") => Loanwords::Romaji,
        Some("english") => Loanwords::English,
        Some(_) => return invalid("loanwords must be romaji or english"),
    };

    match romanizer.romanize(&text, loanwords) {
        Ok(romaji) => plain_text(StatusCode::OK, romaji),
        Err(e) => {
            log::error!("cannot convert {:?}: {}", text, e);
            plain_text(StatusCode::INTERNAL_SERVER_ERROR, "internal error".to_string())
        }
    }
}

#[derive(Debug, Deserialize)]
struct SearchQuery {
    q: Option<String>,
    live: Option<String>,
    limit: Option<usize>,
}

async fn search(State(state): State<Arc<AppState>>, Query(params): Query<SearchQuery>) -> Response {
    let Some(engine) = &state.search else { return StatusCode::NOT_FOUND.into_response() };
    let text = params.q.unwrap_or_default();
    let length = text.trim().chars().count();

    if length == 0 {
        return invalid("q is required (at least 1 character)");
    }
    if length > engine.max_query_chars() {
        return invalid(&format!("q is too long (at most {} characters)", engine.max_query_chars()));
    }

    let Some(live) = Live::parse(params.live.as_deref().unwrap_or("")) else {
        return invalid("live is off, diff or on");
    };

    let limit = params.limit.unwrap_or(engine.max_results()).clamp(1, engine.max_results().max(1));
    // the engine turns the query into romaji first (nothing is added to it), and searches for that
    let request = Request { query: &text, live, limit };

    match engine.search(&request).await {
        Ok(outcome) => json(StatusCode::OK, &outcome),
        Err(e) => {
            log::error!("cannot search {:?}: {}", text, e);
            server_error()
        }
    }
}

async fn sites(State(state): State<Arc<AppState>>) -> Response {
    let Some(engine) = &state.search else { return StatusCode::NOT_FOUND.into_response() };

    json(StatusCode::OK, &engine.sites_info())
}

async fn health() -> Response {
    plain_text(StatusCode::OK, "ok".to_string())
}

const JSON_UTF8: &str = "application/json; charset=utf-8";

/// axum's `Json` sends a bare `application/json`, and axum has no option to add a charset.
/// Some browsers (Orion on iOS) then guess Shift_JIS when they show the body as text, so the charset is set here.
fn json<T: serde::Serialize>(status: StatusCode, value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => (status, [(header::CONTENT_TYPE, JSON_UTF8)], body).into_response(),
        Err(e) => {
            log::error!("cannot serialize the response: {}", e);
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn plain_text(status: StatusCode, body: String) -> Response {
    (status, [(header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

fn server_error() -> Response {
    json(StatusCode::INTERNAL_SERVER_ERROR, &serde_json::json!({ "detail": "internal error" }))
}

fn invalid(detail: &str) -> Response {
    let body = format!("{{\"detail\":{}}}", json_string(detail));
    (StatusCode::UNPROCESSABLE_ENTITY, [(header::CONTENT_TYPE, JSON_UTF8)], body).into_response()
}

/// A JSON string literal (the messages here are plain ASCII, but a quote must not break the body).
fn json_string(text: &str) -> String {
    let mut out = String::from("\"");
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SearchConfig;
    use crate::testing::romanizer_state;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn get_from(state: Arc<AppState>, path: &str) -> (StatusCode, String, String) {
        let response = router(state)
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let content_type = response
            .headers()
            .get(header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap().to_string())
            .unwrap_or_default();
        let body = response.into_body().collect().await.unwrap().to_bytes();

        (status, content_type, String::from_utf8(body.to_vec()).unwrap())
    }

    async fn get_path(path: &str) -> (StatusCode, String, String) {
        get_from(romanizer_state(), path).await
    }

    fn encode(text: &str) -> String {
        text.bytes().map(|b| format!("%{b:02X}")).collect()
    }

    #[tokio::test]
    async fn answers_the_romaji_as_plain_text() {
        let (status, content_type, body) =
            get_path(&format!("/romaji?q={}", encode("チートアイテム管理局のお仕事"))).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "text/plain; charset=utf-8");
        assert_eq!(body, "cheat item kanri kyoku no oshigoto");
    }

    #[tokio::test]
    async fn loanwords_english_looks_the_hiragana_words_up() {
        let q = encode("ふらすとれーしょんの話");
        assert_eq!(get_path(&format!("/romaji?q={q}")).await.2, "furasutore-shon no hanashi");
        assert_eq!(
            get_path(&format!("/romaji?q={q}&loanwords=english")).await.2,
            "frustration no hanashi"
        );
    }

    #[tokio::test]
    async fn a_bad_request_is_a_422() {
        let (status, content_type, body) = get_path("/romaji").await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(content_type, "application/json; charset=utf-8");
        assert!(body.contains("\"detail\""), "{body}");

        assert_eq!(get_path("/romaji?q=").await.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(get_path("/romaji?q=a&loanwords=xx").await.0, StatusCode::UNPROCESSABLE_ENTITY);

        let long = "あ".repeat(301);
        assert_eq!(
            get_path(&format!("/romaji?q={}", encode(&long))).await.0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
        // the limit counts characters, not bytes
        let ok = "あ".repeat(300);
        assert_eq!(get_path(&format!("/romaji?q={}", encode(&ok))).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn health_and_unknown_paths() {
        assert_eq!(get_path("/health").await, (StatusCode::OK, "text/plain; charset=utf-8".into(), "ok".into()));
        assert_eq!(get_path("/nothing").await.0, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn the_search_has_no_route_while_it_is_off_and_the_romaji_is_always_there() {
        // the search is off in the state of the romaji tests
        assert_eq!(get_path("/okazuri/search?q=a").await.0, StatusCode::NOT_FOUND);
        assert_eq!(get_path("/okazuri/sites").await.0, StatusCode::NOT_FOUND);
        assert_eq!(get_path("/romaji?q=a").await.0, StatusCode::OK);
    }

    async fn search_state() -> (Arc<AppState>, Arc<crate::engine::tests::Hits>) {
        let (engine, hits, _) = crate::engine::tests::setup(&SearchConfig { min_delay_ms: 0, ..SearchConfig::default() }).await;

        (Arc::new(AppState { romanizer: romanizer_state().romanizer.clone(), max_query_chars: 300, search: Some(engine) }), hits)
    }

    #[tokio::test]
    async fn searches_the_sites_and_answers_json() {
        let (state, hits) = search_state().await;

        let (status, content_type, body) = get_from(state.clone(), &format!("/okazuri/search?q={}", encode("COMIC ExE"))).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json; charset=utf-8");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["query"], "COMIC ExE");
        assert_eq!(json["results"][0]["website"], "Good");
        assert_eq!(json["results"][0]["isLegal"], true);
        assert_eq!(json["results"][0]["title"], "COMIC ExE one");
        assert_eq!(json["results"][0]["path"], "/work/1");
        assert_eq!(json["results"][0]["url"], "http://127.0.0.1/work/1");
        assert_eq!(json["terms"], serde_json::json!(["comic", "exe"]));
        assert_eq!(json["total"], 2);
        assert_eq!(json["results"][0]["isNew"], true, "this request saved it");
        assert_eq!(json["sites"].as_array().unwrap().len(), 3);
        assert_eq!(json["sites"][1]["status"], "failed");

        // live=diff (the default) does not ask a site again within the validity of the answer (a month): the database answers
        let (_, _, body) = get_from(state.clone(), &format!("/okazuri/search?q={}", encode("COMIC ExE"))).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(hits.ok.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(json["sites"][0]["status"], "cached");
        assert_eq!((json["total"].as_u64().unwrap(), &json["results"][0]["isNew"]), (2, &serde_json::json!(false)));

        // live=on asks every site, cached or not; the works are found again and are not new
        let (_, _, body) = get_from(state.clone(), &format!("/okazuri/search?q={}&live=on", encode("COMIC ExE"))).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(hits.ok.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(json["sites"][0]["status"], "searched");

        // live=off asks nobody
        let (_, _, body) = get_from(state, &format!("/okazuri/search?q={}&live=off", encode("COMIC ExE"))).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["sites"][0]["status"], "not searched");
        assert_eq!(hits.ok.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn the_query_is_always_turned_into_romaji_and_the_input_is_kept() {
        let (state, hits) = search_state().await;

        let (_, _, body) = get_from(state.clone(), &format!("/okazuri/search?q={}", encode("チート"))).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(json["input"], "チート");
        assert_eq!(json["query"], "cheat", "the romaji is the query: no hentai or anything else is added");
        assert_eq!(json["terms"], serde_json::json!(["cheat"]));
        assert_eq!(json["results"][0]["title"], "cheat one", "the sites were asked the romaji, and the titles are matched with it");
        assert_eq!(json["results"][0]["titleRomaji"], "cheat one");
        assert_eq!(json["sites"][0]["queries"], serde_json::json!(["チート", "cheat"]), "a site that finds Japanese titles is asked both");
        assert_eq!(hits.ok.load(std::sync::atomic::Ordering::SeqCst), 2);

        // the same words in romaji are asked in romaji only (it has no Japanese in it)
        let (_, _, body) = get_from(state, "/okazuri/search?q=cheat").await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["input"], "cheat");
        assert_eq!(json["sites"][0]["queries"], serde_json::json!(["cheat"]));
        assert_eq!(json["sites"][0]["status"], "cached", "the romaji form was asked with the Japanese one already");
        assert_eq!(hits.ok.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn the_answer_can_come_from_the_database_alone_and_be_cut_short() {
        let (state, hits) = search_state().await;
        let q = encode("COMIC ExE");

        // live=off: the sites are not asked, and nothing is saved yet
        let (_, _, body) = get_from(state.clone(), &format!("/okazuri/search?q={q}&live=off")).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["total"], 0);
        assert_eq!(json["sites"][0]["status"], "not searched");
        assert_eq!(hits.ok.load(std::sync::atomic::Ordering::SeqCst), 0);

        // limit=1 cuts the answer, and the total says how many there are
        let (_, _, body) = get_from(state, &format!("/okazuri/search?q={q}&limit=1")).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!((json["results"].as_array().unwrap().len(), json["total"].as_u64().unwrap()), (1, 2));
    }

    #[tokio::test]
    async fn a_bad_search_request_is_a_422() {
        let (state, _) = search_state().await;

        for path in [
            "/okazuri/search".to_string(),
            "/okazuri/search?q=".to_string(),
            "/okazuri/search?q=%20%20".to_string(),
            "/okazuri/search?q=a&live=maybe".to_string(),
            format!("/okazuri/search?q={}", "a".repeat(301)),
        ] {
            assert_eq!(get_from(state.clone(), &path).await.0, StatusCode::UNPROCESSABLE_ENTITY, "{path}");
        }
    }

    #[tokio::test]
    async fn lists_the_sites() {
        let (state, _) = search_state().await;
        let (status, _, body) = get_from(state, "/okazuri/sites").await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(json[0]["name"], "Good");
        assert_eq!(json[0]["active"], true);
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\u000a\"");
    }
}
