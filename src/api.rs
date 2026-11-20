//! The HTTP side.
//!
//! - `GET /romaji?q=`: the romaji of the text as `text/plain`. This is what
//!   `okazuri_bot` called when it was the romaji API alone (`src/romaji.ts`).
//! - `GET /okazuri/search?q=` (`[search] enabled`): the works found on the sites for the query, as JSON.
//! - `GET /okazuri/sites` (`[search] enabled`): the sites, and whether each can be searched.
//! - `GET /hitomila/translate?q=` (`[search] enabled`): Japanese to English or English to Japanese, from the titles of Hitomi.La, as JSON.
//! - `GET /google/translate?q=` (`[search] enabled`): the same, from Google Translate.
//! - `GET /auto/translate?q=` (`[search] enabled`): from Hitomi.La, and from Google Translate when Hitomi.La fails or has nothing.
//! - `GET /health`: `ok`.
//!
//! A function that is off has no route, so it answers 404. A bad request is a 422 with
//! `{"detail": "..."}`, as the FastAPI server this started from answered.

use crate::engine::{Engine, Lang, Live, Request, Translator};
use crate::romanizer::{Loanwords, Romanizer};
use axum::extract::RawQuery;
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
    /// The copy of the Worker's D1 database (`[magazine] mirror_path`); `None` when `[magazine]` is off and there is no file.
    pub mirror_path: Option<std::path::PathBuf>,
}

pub fn router(state: Arc<AppState>) -> Router {
    let mut router = Router::new().route("/health", get(health)).route("/romaji", get(romaji));

    if state.search.is_some() {
        router = router.route("/okazuri/search", get(search)).route("/okazuri/sites", get(sites)).route("/okazuri/title", get(title)).route("/hitomila/translate", get(hitomila_translate)).route("/google/translate", get(google_translate)).route("/auto/translate", get(auto_translate));
    }

    if state.mirror_path.is_some() {
        router = router.route("/okazuri/magazine", get(magazine)).route("/okazuri/magazines", get(magazines)).route("/okazuri/works", get(issue_works));
    }

    // The same routes are under `/api/v1` too (https://okazuri.nae.quest/api/v1/...), so a proxy in front needs no rewrite.
    let router = router.layer(axum::middleware::from_fn(cache_control));
    Router::new().nest("/api/v1", router.clone()).merge(router).with_state(state)
}

/// Says how long a cache (Cloudflare in front of this server, a browser) may keep an answer. Only a `200` is kept;
/// a search that asks every site again (`live=force`) and the health check are never kept.
async fn cache_control(request: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let path = request.uri().path().to_string();
    // Read the query the way the handlers do (percent-decoded, case-insensitive), so `live=ON` or `live=%6Fn` is not kept either.
    let forced = axum::extract::Query::<Vec<(String, String)>>::try_from_uri(request.uri()).map_or(true, |query| query.0.iter().any(|(key, value)| key == "live" && Live::parse(value) == Some(Live::Force)));
    let mut response = next.run(request).await;
    let seconds = match path.as_str() {
        "/okazuri/search" if !forced => 300,
        "/hitomila/translate" | "/google/translate" | "/auto/translate" | "/romaji" => 86_400,
        "/okazuri/sites" => 3_600,
        "/okazuri/title" => 3_600,
        "/okazuri/magazine" | "/okazuri/magazines" | "/okazuri/works" => 300,
        _ => 0,
    };
    let value = if seconds > 0 && response.status() == StatusCode::OK { format!("public, max-age=60, s-maxage={seconds}") } else { "no-store".to_string() };
    response.headers_mut().insert(header::CACHE_CONTROL, value.parse().expect("a valid header value"));
    response
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
    /// The English title of a query in Japanese is searched too: `auto` (default), `hitomi`, `google` or `none`.
    translator: Option<String>,
    /// The sites that are left out, by name, separated by commas: they are not asked, and their saved works are not in the answer.
    exclude: Option<String>,
}

async fn search(State(state): State<Arc<AppState>>, Query(params): Query<SearchQuery>) -> Response {
    let Some(engine) = &state.search else { return StatusCode::NOT_FOUND.into_response() };
    let names: Vec<String> = params.exclude.as_deref().unwrap_or("").split(',').map(str::trim).filter(|name| !name.is_empty()).map(str::to_string).collect();
    let exclude = match engine.site_indexes(&names) {
        Ok(indexes) => indexes,
        Err(unknown) => return invalid(&format!("exclude has a site that does not exist: {unknown} (GET /okazuri/sites lists them)")),
    };

    run_search(engine, params, &exclude).await
}

async fn run_search(engine: &Arc<Engine>, params: SearchQuery, exclude: &[usize]) -> Response {
    let text = params.q.unwrap_or_default();
    let length = text.trim().chars().count();

    if length == 0 {
        return invalid("q is required (at least 1 character)");
    }
    if length > engine.max_query_chars() {
        return invalid(&format!("q is too long (at most {} characters)", engine.max_query_chars()));
    }

    let Some(live) = Live::parse(params.live.as_deref().unwrap_or("")) else {
        return invalid("live is off, diff or force");
    };

    let Some(translator) = Translator::parse(params.translator.as_deref().unwrap_or("")) else {
        return invalid("translator is auto, hitomi, google or none");
    };

    let limit = params.limit.unwrap_or(engine.max_results()).clamp(1, engine.max_results().max(1));
    // the engine turns the query into romaji first (nothing is added to it), and searches for that
    let request = Request { query: &text, live, limit, translator, exclude };

    match engine.search(&request).await {
        Ok(outcome) => json(StatusCode::OK, &outcome),
        Err(e) => {
            log::error!("cannot search {:?}: {}", text, e);
            server_error()
        }
    }
}

#[derive(Debug, Deserialize)]
struct TranslateQuery {
    q: Option<String>,
    live: Option<String>,
}

/// The text of a translation request: not empty, not too long, in Japanese or English (the direction); and `live`.
fn translate_input(engine: &Engine, params: TranslateQuery) -> std::result::Result<(String, Lang, Live), Response> {
    let text = params.q.unwrap_or_default();
    let length = text.trim().chars().count();

    if length == 0 {
        return Err(invalid("q is required (at least 1 character)"));
    }
    if length > engine.max_query_chars() {
        return Err(invalid(&format!("q is too long (at most {} characters)", engine.max_query_chars())));
    }

    let Some(from) = Lang::of(&text) else {
        return Err(invalid("q must be Japanese or English (only Japanese to English and English to Japanese are translated)"));
    };

    // like the search: the saved answer (diff, the default), only the saved one (off), the source always (on)
    let Some(live) = Live::parse(params.live.as_deref().unwrap_or("")) else {
        return Err(invalid("live is off, diff or force"));
    };

    Ok((text, from, live))
}

/// A translation, from Japanese to English or from English to Japanese (the language of `q` says which): the titles of
/// the works of Hitomi.La, which has both. The translations are saved.
async fn hitomila_translate(State(state): State<Arc<AppState>>, Query(params): Query<TranslateQuery>) -> Response {
    let Some(engine) = &state.search else { return StatusCode::NOT_FOUND.into_response() };
    let (text, from, live) = match translate_input(engine, params) {
        Ok(input) => input,
        Err(response) => return response,
    };

    if live == Live::Off {
        return invalid("live=off is not available for /hitomila/translate: diff or force");
    }
    // Hitomi.La is used only when its whole list is saved
    match engine.hitomi_waits_for_its_list().await {
        Ok(false) => {}
        Ok(true) => return json(StatusCode::SERVICE_UNAVAILABLE, &serde_json::json!({ "detail": "the whole list of Hitomi.La is not saved yet" })),
        Err(_) => return server_error(),
    }

    match engine.translate_hitomi(&text, from, live).await {
        Ok((results, saved)) => {
            let to = from.other();
            let translation = results.first().map(|best| best.text(to));

            json(
                StatusCode::OK,
                &serde_json::json!({ "input": text.trim(), "title": crate::query::work_title(&text, from == Lang::Japanese), "from": from.code(), "to": to.code(), "translation": translation, "saved": saved, "results": results }),
            )
        }
        Err(e) => {
            log::warn!("cannot translate {:?} with Hitomi.La: {}", text, e);
            json(StatusCode::BAD_GATEWAY, &serde_json::json!({ "detail": "Hitomi.La did not answer" }))
        }
    }
}

/// The same from Google Translate (the model of its page that is Gemini). The translation is saved: `live=force` asks again.
async fn google_translate(State(state): State<Arc<AppState>>, Query(params): Query<TranslateQuery>) -> Response {
    let Some(engine) = &state.search else { return StatusCode::NOT_FOUND.into_response() };
    let (text, from, live) = match translate_input(engine, params) {
        Ok(input) => input,
        Err(response) => return response,
    };

    if live == Live::Off {
        return invalid("live=off is not available for Google Translate (it is asked when nothing is saved): diff or force");
    }

    match engine.translate_google(&text, from, live).await {
        Ok((found, saved)) => {
            let to = from.other();

            json(
                StatusCode::OK,
                &serde_json::json!({ "input": text.trim(), "from": from.code(), "to": to.code(), "translation": found.text(to), "saved": saved }),
            )
        }
        Err(e) => {
            log::warn!("cannot translate {:?} with Google Translate: {}", text, e);
            json(StatusCode::BAD_GATEWAY, &serde_json::json!({ "detail": "Google Translate did not answer" }))
        }
    }
}

/// Hitomi.La first, Google Translate when Hitomi.La fails or has nothing. `source` says which one answered.
async fn auto_translate(State(state): State<Arc<AppState>>, Query(params): Query<TranslateQuery>) -> Response {
    let Some(engine) = &state.search else { return StatusCode::NOT_FOUND.into_response() };
    let (text, from, live) = match translate_input(engine, params) {
        Ok(input) => input,
        Err(response) => return response,
    };

    if live == Live::Off {
        return invalid("live=off is not available for /auto/translate (Google Translate is asked when nothing is saved): diff or force");
    }

    match engine.translate_auto(&text, from, live).await {
        Ok((found, saved)) => {
            let to = from.other();

            json(
                StatusCode::OK,
                &serde_json::json!({ "input": text.trim(), "from": from.code(), "to": to.code(), "translation": found.text(to), "source": found.source, "saved": saved }),
            )
        }
        Err(e) => {
            log::warn!("cannot translate {:?}: {}", text, e);
            json(StatusCode::BAD_GATEWAY, &serde_json::json!({ "detail": "neither Hitomi.La nor Google Translate answered" }))
        }
    }
}

/// The title of the work at each address (`url`, once or more): what the bot reads from the page of a work. An address that
/// is no page of a site, or a page that gives no title, has `title: null` and says why in `error`. The saved works answer first
/// (`source: "database"`); `live=off` never asks a page, `live=force` always does.
async fn title(State(state): State<Arc<AppState>>, RawQuery(raw): RawQuery) -> Response {
    let Some(engine) = &state.search else { return StatusCode::NOT_FOUND.into_response() };
    let pairs: Vec<(String, String)> = Query::<Vec<(String, String)>>::try_from_uri(&format!("/?{}", raw.unwrap_or_default()).parse().unwrap_or_default())
        .map(|query| query.0)
        .unwrap_or_default();
    let urls: Vec<String> = pairs.iter().filter(|(key, _)| key == "url").map(|(_, value)| value.trim().to_string()).filter(|url| !url.is_empty()).collect();
    let live = match pairs.iter().find(|(key, _)| key == "live") {
        None => Live::Diff,
        Some((_, value)) => match Live::parse(value) {
            Some(live) => live,
            None => return invalid("live is off, diff or force"),
        },
    };

    if urls.is_empty() || urls.len() > MAX_TITLE_URLS {
        return invalid(&format!("url is required, 1 to {MAX_TITLE_URLS} times"));
    }

    let mut tasks = tokio::task::JoinSet::new();

    for (place, url) in urls.iter().cloned().enumerate() {
        let engine = engine.clone();

        tasks.spawn(async move {
            let found = match engine.title_of(&url, live).await {
                Ok(info) => serde_json::json!({
                    "url": url,
                    "website": info.website,
                    "title": info.title,
                    "title_japanese": info.title_japanese,
                    "title_romaji": info.title_romaji,
                    "title_english": info.title_english,
                    "source": info.source,
                    "error": null,
                }),
                Err(None) => serde_json::json!({ "url": url, "website": null, "title": null, "title_japanese": null, "title_romaji": null, "title_english": null, "source": null, "error": "unsupported website" }),
                Err(Some(why)) => serde_json::json!({ "url": url, "website": null, "title": null, "title_japanese": null, "title_romaji": null, "title_english": null, "source": null, "error": why }),
            };

            (place, found)
        });
    }

    let mut results = vec![serde_json::Value::Null; urls.len()];

    while let Some(done) = tasks.join_next().await {
        if let Ok((place, found)) = done {
            results[place] = found;
        }
    }

    json(StatusCode::OK, &serde_json::json!({ "total": results.len(), "results": results }))
}

/// The longest `q` of the lookups in the copy of the catalogue (`/okazuri/magazine`, `/okazuri/works`), in characters.
const MAX_LOOKUP_CHARS: usize = 300;

/// The most addresses one request to `/okazuri/title` may carry.
const MAX_TITLE_URLS: usize = 10;
#[derive(Deserialize)]
struct MagazineQuery {
    q: Option<String>,
    limit: Option<u64>,
}

/// The magazine issues a work was printed in, from the copy of the Worker's D1 database: the work is found by its
/// exact title, the way the bot's `/from_work` finds it.
async fn magazine(State(state): State<Arc<AppState>>, Query(params): Query<MagazineQuery>) -> Response {
    let Some(path) = &state.mirror_path else { return StatusCode::NOT_FOUND.into_response() };
    let title = params.q.as_deref().map(str::trim).unwrap_or_default();

    if title.is_empty() || title.chars().count() > MAX_LOOKUP_CHARS {
        return invalid(&format!("q is required, 1 to {} characters", MAX_LOOKUP_CHARS));
    }

    let limit = params.limit.unwrap_or(10);

    if limit == 0 || limit > crate::mirror::MAX_PRINTED_WORKS {
        return invalid(&format!("limit is 1 to {}", crate::mirror::MAX_PRINTED_WORKS));
    }

    let found = match crate::mirror::mirror_find_works(path, title, limit).await {
        Ok(Some(found)) => found,
        Ok(None) => return json(StatusCode::SERVICE_UNAVAILABLE, &serde_json::json!({ "detail": "the copy of the works is not ready yet" })),
        Err(e) => {
            log::error!("cannot look up the work {:?}: {}", title, e);
            return json(StatusCode::INTERNAL_SERVER_ERROR, &serde_json::json!({ "detail": "cannot read the copy of the catalogue" }));
        }
    };

    if found.is_empty() {
        return json(StatusCode::NOT_FOUND, &serde_json::json!({ "detail": "no such work" }));
    }

    let product = |id: &str| format!("https://www.dlsite.com/books/work/=/product_id/{id}.html");
    let results: Vec<_> = found
        .iter()
        .map(|work| {
            serde_json::json!({
                "id": work.id,
                "title": work.name,
                "url": product(&work.id),
                "author": work.author,
                "position": work.position,
                "magazine": { "id": work.issue.magazine_id, "name": work.issue.magazine_name },
                "issue": { "id": work.issue.id, "name": work.issue.name, "url": product(&work.issue.id), "date": work.issue.date },
            })
        })
        .collect();

    json(StatusCode::OK, &serde_json::json!({ "input": title, "total": results.len(), "results": results }))
}

/// The magazines of the copy of the Worker's D1 database, with how many issues each has and its newest issue.
async fn magazines(State(state): State<Arc<AppState>>) -> Response {
    let Some(path) = &state.mirror_path else { return StatusCode::NOT_FOUND.into_response() };

    match crate::mirror::mirror_magazines(path).await {
        Ok(Some(found)) => {
            let results: Vec<_> = found
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "id": m.id,
                        "name": m.name,
                        "issues": m.issues,
                        "latest": m.latest.as_ref().map(|i| serde_json::json!({
                            "id": i.id,
                            "name": i.name,
                            "url": format!("https://www.dlsite.com/books/work/=/product_id/{}.html", i.id),
                            "date": i.date,
                        })),
                    })
                })
                .collect();

            json(StatusCode::OK, &serde_json::json!({ "total": results.len(), "results": results }))
        }
        Ok(None) => json(StatusCode::SERVICE_UNAVAILABLE, &serde_json::json!({ "detail": "the copy of the catalogue is not ready yet" })),
        Err(e) => {
            log::error!("cannot list the magazines: {}", e);
            json(StatusCode::INTERNAL_SERVER_ERROR, &serde_json::json!({ "detail": "cannot read the copy of the catalogue" }))
        }
    }
}

/// The works printed in an issue, in the order of the issue (the bot's `/works`). `q` is the name of the issue as the
/// Worker keeps it (`COMIC快楽天 2026年07月号`), or its id.
async fn issue_works(State(state): State<Arc<AppState>>, Query(params): Query<MagazineQuery>) -> Response {
    let Some(path) = &state.mirror_path else { return StatusCode::NOT_FOUND.into_response() };
    let name = params.q.as_deref().map(str::trim).unwrap_or_default();

    if name.is_empty() || name.chars().count() > MAX_LOOKUP_CHARS {
        return invalid(&format!("q is required, 1 to {} characters", MAX_LOOKUP_CHARS));
    }

    match crate::mirror::mirror_issue_works(path, name).await {
        Ok(Some(Some((issue, works)))) => {
            let product = |id: &str| format!("https://www.dlsite.com/books/work/=/product_id/{id}.html");
            let results: Vec<_> = works
                .iter()
                .map(|w| serde_json::json!({ "id": w.id, "title": w.name, "url": product(&w.id), "author": w.author, "position": w.position }))
                .collect();

            json(
                StatusCode::OK,
                &serde_json::json!({
                    "input": name,
                    "issue": { "id": issue.id, "name": issue.name, "url": product(&issue.id), "date": issue.date },
                    "magazine": { "id": issue.magazine_id, "name": issue.magazine_name },
                    "total": results.len(),
                    "results": results,
                }),
            )
        }
        Ok(Some(None)) => json(StatusCode::NOT_FOUND, &serde_json::json!({ "detail": "no such issue" })),
        Ok(None) => json(StatusCode::SERVICE_UNAVAILABLE, &serde_json::json!({ "detail": "the copy of the works is not ready yet" })),
        Err(e) => {
            log::error!("cannot list the works of {:?}: {}", name, e);
            json(StatusCode::INTERNAL_SERVER_ERROR, &serde_json::json!({ "detail": "cannot read the copy of the catalogue" }))
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
    async fn only_an_answer_that_may_be_kept_has_a_max_age() {
        let cache = |path: &'static str| async move {
            let response = router(romanizer_state()).oneshot(Request::builder().uri(path).body(Body::empty()).unwrap()).await.unwrap();
            response.headers().get(header::CACHE_CONTROL).unwrap().to_str().unwrap().to_string()
        };
        assert_eq!(cache("/romaji?q=a").await, "public, max-age=60, s-maxage=86400");
        assert_eq!(cache("/romaji?q=").await, "no-store", "an error is not kept");
        assert_eq!(cache("/health").await, "no-store");
    }

    #[tokio::test]
    async fn the_routes_are_under_api_v1_too() {
        assert_eq!(get_path("/api/v1/romaji?q=a").await, get_path("/romaji?q=a").await);
        assert_eq!(get_path("/api/v1/health").await.2, "ok");
        assert_eq!(get_path("/api/v1/nothing").await.0, StatusCode::NOT_FOUND);
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
        assert_eq!(get_path("/hitomila/translate?q=a").await.0, StatusCode::NOT_FOUND);
        assert_eq!(get_path("/romaji?q=a").await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn the_title_route_says_what_it_cannot_read() {
        let (state, _) = search_state().await;

        assert_eq!(get_from(state.clone(), "/okazuri/title").await.0, StatusCode::UNPROCESSABLE_ENTITY);

        let (status, _, body) = get_from(state.clone(), "/api/v1/okazuri/title?url=https%3A%2F%2Fexample.com%2Fx&url=https%3A%2F%2Fexample.org").await;
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value["total"], 2);
        assert_eq!(value["results"][0]["url"], "https://example.com/x");
        assert_eq!(value["results"][1]["error"], "unsupported website");
        assert!(value["results"][0]["title"].is_null());
        assert_eq!(get_from(state.clone(), "/okazuri/title?url=https%3A%2F%2Fexample.com&live=maybe").await.0, StatusCode::UNPROCESSABLE_ENTITY);

        let many: String = (0..11).map(|n| format!("url=https%3A%2F%2Fexample.com%2F{n}&")).collect();
        assert_eq!(get_from(state, &format!("/okazuri/title?{many}")).await.0, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn the_magazine_route_finds_the_issues_of_a_work_in_the_copy() {
        let dir = std::env::temp_dir().join(format!("okazuri-search-api-test-magazine-route-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("d1-mirror.db");
        let sql = "CREATE TABLE `magazines` (`id` text PRIMARY KEY NOT NULL, `name` text NOT NULL);
CREATE TABLE `numbers` (`id` text PRIMARY KEY NOT NULL, `name` text NOT NULL, `date` integer NOT NULL, `magazine_id` text NOT NULL);
CREATE TABLE `authors` (`id` text PRIMARY KEY NOT NULL, `name` text NOT NULL);
CREATE TABLE `works` (`id` text PRIMARY KEY NOT NULL, `name` text NOT NULL, `date` integer NOT NULL, `position` integer NOT NULL, `number_id` text NOT NULL, `author_id` text NOT NULL);
INSERT INTO magazines VALUES('LBL1','COMIC快楽天');
INSERT INTO numbers VALUES('BJ2','COMIC快楽天 2026年07月号',2000,'LBL1');
INSERT INTO authors VALUES('A1','作者');
INSERT INTO works VALUES('BJ9','作品名',2000,4,'BJ2','A1');";
        crate::mirror::rebuild_mirror(&path, sql, "bm", 1).await.unwrap();
        let state = Arc::new(AppState { mirror_path: Some(path), ..Arc::into_inner(romanizer_state()).unwrap() });

        // the route is there only when there is a copy
        assert_eq!(get_path("/okazuri/magazine?q=a").await.0, StatusCode::NOT_FOUND);

        let (status, content_type, body) = get_from(state.clone(), &format!("/okazuri/magazine?q={}", encode("作品名"))).await;
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.starts_with("application/json"));
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["total"], 1);
        assert_eq!(value["results"][0]["issue"]["name"], "COMIC快楽天 2026年07月号");
        assert_eq!(value["results"][0]["magazine"]["name"], "COMIC快楽天");
        assert_eq!(value["results"][0]["position"], 4);
        assert_eq!(value["results"][0]["author"], "作者");
        assert_eq!(value["results"][0]["issue"]["url"], "https://www.dlsite.com/books/work/=/product_id/BJ2.html");

        let (status, _, body) = get_from(state.clone(), "/okazuri/magazines").await;
        assert_eq!(status, StatusCode::OK);
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!((value["total"].clone(), value["results"][0]["name"].clone(), value["results"][0]["issues"].clone()), (1.into(), "COMIC快楽天".into(), 1.into()));
        assert_eq!(value["results"][0]["latest"]["name"], "COMIC快楽天 2026年07月号");
        assert_eq!(get_path("/okazuri/magazines").await.0, StatusCode::NOT_FOUND, "only with a copy");

        // the works of an issue, by the name of the issue (or its id), in the order of the issue
        let (status, _, body) = get_from(state.clone(), &format!("/okazuri/works?q={}", encode("COMIC快楽天 2026年07月号"))).await;
        assert_eq!(status, StatusCode::OK);
        let value: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!((value["issue"]["id"].clone(), value["total"].clone(), value["results"][0]["title"].clone(), value["results"][0]["author"].clone()), ("BJ2".into(), 1.into(), "作品名".into(), "作者".into()));
        assert_eq!(get_from(state.clone(), "/okazuri/works?q=BJ2").await.0, StatusCode::OK, "the id of the issue works too");
        assert_eq!(get_from(state.clone(), "/okazuri/works?q=none").await.0, StatusCode::NOT_FOUND);
        assert_eq!(get_from(state.clone(), "/okazuri/works").await.0, StatusCode::UNPROCESSABLE_ENTITY);

        assert_eq!(get_from(state.clone(), &format!("/okazuri/magazine?q={}", encode("ない"))).await.0, StatusCode::NOT_FOUND);
        assert_eq!(get_from(state.clone(), "/okazuri/magazine").await.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(get_from(state.clone(), "/okazuri/magazine?q=a&limit=0").await.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(get_from(state, "/api/v1/okazuri/magazine?q=x").await.0, StatusCode::NOT_FOUND, "also under /api/v1 (and no such work)");

        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn the_translate_route_needs_a_text_and_says_when_the_site_does_not_answer() {
        // the engine of the tests has no Hitomi.La among its sites
        let (state, _) = search_state().await;

        assert_eq!(get_from(state.clone(), "/hitomila/translate").await.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(get_from(state.clone(), "/hitomila/translate?q=%20").await.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(get_from(state.clone(), &format!("/hitomila/translate?q={}&live=maybe", encode("姉弟"))).await.0, StatusCode::UNPROCESSABLE_ENTITY);
        // only Japanese and English
        assert_eq!(get_from(state.clone(), &format!("/hitomila/translate?q={}", encode("형제"))).await.0, StatusCode::UNPROCESSABLE_ENTITY);
        // no `off` here: the saved answers are the answer of `diff`, and the site is asked when there is none
        assert_eq!(get_from(state.clone(), &format!("/hitomila/translate?q={}&live=off", encode("姉弟"))).await.0, StatusCode::UNPROCESSABLE_ENTITY);
        // Google Translate: the same input rules, and `off` with nothing saved is no translation
        assert_eq!(get_from(state.clone(), "/google/translate").await.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(get_from(state.clone(), &format!("/google/translate?q={}", encode("형제"))).await.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(get_from(state.clone(), &format!("/google/translate?q={}&live=off", encode("姉弟"))).await.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(get_from(state.clone(), "/auto/translate").await.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(get_from(state.clone(), &format!("/auto/translate?q={}&live=off", encode("姉弟"))).await.0, StatusCode::UNPROCESSABLE_ENTITY);
        // the sites that are left out of a search
        assert_eq!(get_from(state.clone(), "/okazuri/search?q=a&exclude=Nowhere").await.0, StatusCode::UNPROCESSABLE_ENTITY);
        // Hitomi.La is not one of the sites of the tests: no route
        // the translator of a search
        assert_eq!(get_from(state.clone(), "/okazuri/search?q=a&translator=bing").await.0, StatusCode::UNPROCESSABLE_ENTITY);
        let (status, content_type, body) = get_from(state, &format!("/hitomila/translate?q={}&live=force", encode("姉弟"))).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(content_type.starts_with("application/json"));
        assert!(body.contains("did not answer"), "{body}");
    }

    async fn search_state() -> (Arc<AppState>, Arc<crate::engine::tests::Hits>) {
        let (engine, hits, _) = crate::engine::tests::setup(&SearchConfig { min_delay_ms: 0, ..SearchConfig::default() }).await;

        (Arc::new(AppState { romanizer: romanizer_state().romanizer.clone(), max_query_chars: 300, search: Some(engine), mirror_path: None }), hits)
    }

    #[tokio::test]
    async fn searches_the_sites_and_answers_json() {
        let (state, hits) = search_state().await;

        let (status, content_type, body) = get_from(state.clone(), &format!("/okazuri/search?translator=none&q={}", encode("COMIC ExE"))).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(content_type, "application/json; charset=utf-8");
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(json["query"], "COMIC ExE");
        assert_eq!(json["results"][0]["website"], "Good");
        assert_eq!(json["results"][0]["is_legal"], true);
        assert_eq!(json["results"][0]["title"], "COMIC ExE one");
        assert_eq!(json["results"][0]["path"], "/work/1");
        assert_eq!(json["results"][0]["url"], "http://127.0.0.1/work/1");
        assert_eq!(json["terms"], serde_json::json!(["comic", "exe"]));
        assert_eq!(json["total"], 2);
        assert_eq!(json["results"][0]["is_new"], true, "this request saved it");
        assert_eq!(json["sites"].as_array().unwrap().len(), 3);
        assert_eq!(json["sites"][1]["status"], "failed");

        // live=diff (the default) does not ask a site again within the validity of the answer (a month): the database answers
        let (_, _, body) = get_from(state.clone(), &format!("/okazuri/search?translator=none&q={}", encode("COMIC ExE"))).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(hits.ok.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(json["sites"][0]["status"], "cached");
        assert_eq!((json["total"].as_u64().unwrap(), &json["results"][0]["is_new"]), (2, &serde_json::json!(false)));

        // live=force asks every site, cached or not; the works are found again and are not new
        let (_, _, body) = get_from(state.clone(), &format!("/okazuri/search?translator=none&q={}&live=force", encode("COMIC ExE"))).await;
        let json: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(hits.ok.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(json["sites"][0]["status"], "searched");

        // live=off asks nobody
        let (_, _, body) = get_from(state, &format!("/okazuri/search?translator=none&q={}&live=off", encode("COMIC ExE"))).await;
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
        assert_eq!(json["results"][0]["title_romaji"], "cheat one");
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
