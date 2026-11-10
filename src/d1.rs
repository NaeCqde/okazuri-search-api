//! A copy of the magazine tables of the Worker's D1 database, kept up to date.
//!
//! D1 can export a database as an SQL file through the Cloudflare API: the export is started, polled
//! until it is complete, and the SQL is downloaded from a signed URL. A database that has a virtual
//! table (the Worker's has `works_fts`, an FTS5 index) is refused as a whole, but the tables can be
//! named, so only `magazines` and `numbers` are exported. An export blocks the database for its
//! duration (a second or so for these two tables), so it is done only when the data changed.
//!
//! Whether the data changed is told by the Time Travel bookmark of the database: a call that costs
//! no row reads, and a value that moves on with every write. The bookmark of the data in the mirror is
//! kept in the mirror itself.

use crate::config::D1Config;
use crate::mirror::{content_hash, mirror_bookmark, mirror_content_hash, mirror_note_bookmark, rebuild_mirror};
use crate::utils::{unix_now, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const API: &str = "https://api.cloudflare.com/client/v4";

/// An export that has not finished after this many polls is given up.
const MAX_POLLS: usize = 120;

pub struct D1Client {
    agent: ureq::Agent,
    base: String,
    database_path: String,
    token: String,
    tables: Vec<String>,
    poll_interval: Duration,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Synced {
    /// The mirror was rebuilt: the data of the tables is not what it was.
    pub changed: bool,
    /// The tables were exported (the bookmark of D1 had moved). With `changed` false it means that D1 was written to
    /// somewhere else (the bookmark is of the whole database): the exported data is the same, and nothing was rebuilt.
    pub exported: bool,
    pub bookmark: String,
}

impl D1Client {
    pub fn new(config: &D1Config, token: String) -> Self {
        Self::with_base(config, token, API.to_string(), Duration::from_secs(1))
    }

    pub fn with_base(config: &D1Config, token: String, base: String, poll_interval: Duration) -> Self {
        Self {
            agent: ureq::Agent::config_builder()
                .timeout_global(Some(Duration::from_secs(120)))
                .http_status_as_error(false)
                .build()
                .into(),
            base,
            database_path: format!("/accounts/{}/d1/database/{}", config.account_id, config.database_id),
            token,
            tables: config.tables.clone(),
            poll_interval,
        }
    }

    /// The JSON an API call answered; an answer that says it failed is an error with the message of Cloudflare.
    fn parse(mut response: ureq::http::Response<ureq::Body>) -> Result<Value> {
        let status = response.status().as_u16();
        let text = response.body_mut().read_to_string().map_err(|e| format!("cannot read the answer: {e}"))?;
        let value: Value = serde_json::from_str(&text)
            .map_err(|_| format!("Cloudflare answered {status}, which is not JSON: {}", text.chars().take(200).collect::<String>()))?;

        if status >= 400 || value["success"] == Value::Bool(false) {
            let message = value["errors"][0]["message"].as_str().unwrap_or("no message");

            return Err(format!("Cloudflare answered {status}: {message}").into());
        }

        Ok(value)
    }

    fn get(&self, path: &str) -> Result<Value> {
        let response = self
            .agent
            .get(&format!("{}{}{}", self.base, self.database_path, path))
            .header("Authorization", &format!("Bearer {}", self.token))
            .call()
            .map_err(|e| format!("cannot reach Cloudflare: {e}"))?;

        Self::parse(response).map_err(|e| format!("GET {path}: {e}").into())
    }

    fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let response = self
            .agent
            .post(&format!("{}{}{}", self.base, self.database_path, path))
            .header("Authorization", &format!("Bearer {}", self.token))
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .map_err(|e| format!("cannot reach Cloudflare: {e}"))?;

        Self::parse(response).map_err(|e| format!("POST {path}: {e}").into())
    }

    /// The bookmark of the data now.
    pub fn bookmark(&self) -> Result<String> {
        let value = self.get("/time_travel/bookmark")?;

        value["result"]["bookmark"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "the answer has no bookmark".into())
    }

    /// The SQL of the tables: starts the export, polls it until it is complete, downloads it.
    pub fn export(&self) -> Result<String> {
        let mut at_bookmark: Option<String> = None;

        for _ in 0..MAX_POLLS {
            let mut body = json!({
                "output_format": "polling",
                "dump_options": { "tables": self.tables, "no_schema": false, "no_data": false },
            });
            if let Some(bookmark) = &at_bookmark {
                body["current_bookmark"] = json!(bookmark);
            }

            let value = self.post("/export", &body)?;
            // the answer is wrapped once for the API and once for the export
            let export = &value["result"];

            if export["success"] == Value::Bool(false) {
                return Err(format!("the export failed: {}", export["error"].as_str().unwrap_or("no message")).into());
            }

            match export["status"].as_str() {
                Some("complete") => {
                    let url = export["result"]["signed_url"].as_str().ok_or("the export has no download URL")?;

                    return self.download(url);
                }
                Some("error") => {
                    return Err(format!("the export failed: {}", export["error"].as_str().unwrap_or("no message")).into());
                }
                _ => {
                    at_bookmark = export["at_bookmark"].as_str().map(str::to_string).or(at_bookmark);
                    std::thread::sleep(self.poll_interval);
                }
            }
        }

        Err("the export did not finish".into())
    }

    /// The SQL file at the signed URL. The URL is the authority, so no token is sent to it.
    fn download(&self, url: &str) -> Result<String> {
        let mut response = self.agent.get(url).call().map_err(|e| format!("cannot download the export: {e}"))?;
        let status = response.status().as_u16();

        if status != 200 {
            return Err(format!("the download answered {status}").into());
        }

        Ok(response.body_mut().read_to_string().map_err(|e| format!("cannot read the export: {e}"))?)
    }

    /// Rebuilds the mirror if the data changed since it was built (or there is no mirror). The calls to Cloudflare
    /// block, so they run on the blocking threads of tokio.
    pub async fn sync(self: &Arc<Self>, mirror: &Path) -> Result<Synced> {
        let client = self.clone();
        let bookmark = tokio::task::spawn_blocking(move || client.bookmark()).await??;

        if mirror_bookmark(mirror).await?.as_deref() == Some(bookmark.as_str()) {
            return Ok(Synced { changed: false, exported: false, bookmark });
        }

        let client = self.clone();
        let sql = tokio::task::spawn_blocking(move || client.export()).await??;

        // another table was written to: the same data is not loaded again, and whoever waits for a new mirror is not woken
        if mirror_content_hash(mirror).await?.as_deref() == Some(content_hash(&sql).as_str()) {
            mirror_note_bookmark(mirror, &bookmark, unix_now()).await?;

            return Ok(Synced { changed: false, exported: true, bookmark });
        }

        rebuild_mirror(mirror, &sql, &bookmark, unix_now()).await?;

        Ok(Synced { changed: true, exported: true, bookmark })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mirror::mirror_issues;
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    const EXPORT: &str = "PRAGMA defer_foreign_keys=TRUE;
CREATE TABLE `magazines` (`id` text PRIMARY KEY NOT NULL, `name` text NOT NULL);
CREATE TABLE `numbers` (`id` text PRIMARY KEY NOT NULL, `name` text NOT NULL, `date` integer NOT NULL, `magazine_id` text NOT NULL);
INSERT INTO \"magazines\" (\"id\",\"name\") VALUES('LBL1','COMIC快楽天');
INSERT INTO \"numbers\" (\"id\",\"name\",\"date\",\"magazine_id\") VALUES('BJ1','COMIC快楽天 2026年07月号',2000,'LBL1');
";

    struct Mock {
        bookmark: Mutex<String>,
        exports: AtomicUsize,
        polls: AtomicUsize,
        download_had_authorization: Mutex<bool>,
        port: Mutex<u16>,
        /// The SQL the download answers with.
        sql: Mutex<String>,
    }

    fn unauthorized() -> axum::response::Response {
        (StatusCode::FORBIDDEN, Json(json!({"success": false, "errors": [{"code": 10000, "message": "Authentication error"}]})))
            .into_response()
    }

    async fn mock() -> (u16, Arc<Mock>) {
        let state = Arc::new(Mock {
            bookmark: Mutex::new("bm-1".into()),
            exports: AtomicUsize::new(0),
            polls: AtomicUsize::new(0),
            download_had_authorization: Mutex::new(false),
            port: Mutex::new(0),
            sql: Mutex::new(EXPORT.to_string()),
        });
        let router = Router::new()
            .route(
                "/accounts/acc/d1/database/db/time_travel/bookmark",
                get(|State(s): State<Arc<Mock>>, headers: HeaderMap| async move {
                    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer secret") {
                        return unauthorized();
                    }
                    Json(json!({"success": true, "result": {"bookmark": *s.bookmark.lock().unwrap()}})).into_response()
                }),
            )
            .route(
                "/accounts/acc/d1/database/db/export",
                post(|State(s): State<Arc<Mock>>, headers: HeaderMap, Json(body): Json<Value>| async move {
                    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some("Bearer secret") {
                        return unauthorized();
                    }
                    assert_eq!(body["output_format"], "polling");
                    assert_eq!(body["dump_options"]["tables"], json!(["magazines", "numbers"]));

                    s.polls.fetch_add(1, Ordering::SeqCst);

                    // the first call starts the export; the second one, which carries the bookmark of the
                    // first, finds it complete
                    if body.get("current_bookmark").is_none() {
                        return Json(json!({"success": true, "result": {"success": true, "status": "active", "at_bookmark": "p1", "messages": ["Started"]}}))
                            .into_response();
                    }
                    assert_eq!(body["current_bookmark"], "p1");
                    s.exports.fetch_add(1, Ordering::SeqCst);
                    let port = *s.port.lock().unwrap();

                    Json(json!({"success": true, "result": {"success": true, "status": "complete", "at_bookmark": "p2",
                        "result": {"filename": "x.sql", "signed_url": format!("http://127.0.0.1:{port}/download/x.sql")}}}))
                        .into_response()
                }),
            )
            .route(
                "/download/x.sql",
                get(|State(s): State<Arc<Mock>>, headers: HeaderMap| async move {
                    *s.download_had_authorization.lock().unwrap() = headers.contains_key("authorization");
                    s.sql.lock().unwrap().clone()
                }),
            )
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();

        *state.port.lock().unwrap() = port;
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        (port, state)
    }

    fn client(port: u16, token: &str) -> Arc<D1Client> {
        let config = D1Config {
            account_id: "acc".into(),
            database_id: "db".into(),
            ..D1Config::default()
        };

        Arc::new(D1Client::with_base(&config, token.into(), format!("http://127.0.0.1:{port}"), Duration::from_millis(1)))
    }

    fn mirror_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("okazuri-search-api-d1-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);

        dir.join("mirror.sqlite")
    }

    #[tokio::test]
    async fn the_mirror_is_built_once_and_again_only_when_the_data_changed() {
        let (port, state) = mock().await;
        let path = mirror_path("sync");
        let path2 = path.clone();

        let client = client(port, "secret");
        let first = client.sync(&path2).await.unwrap();
        let second = client.sync(&path2).await.unwrap();

        assert_eq!(first, Synced { changed: true, exported: true, bookmark: "bm-1".into() });
        assert_eq!(second, Synced { changed: false, exported: false, bookmark: "bm-1".into() }, "same bookmark, nothing exported");
        assert_eq!(state.exports.load(Ordering::SeqCst), 1);
        assert_eq!(mirror_issues(&path).await.unwrap()[0].name, "COMIC快楽天 2026年07月号");
        assert!(!*state.download_had_authorization.lock().unwrap(), "the signed URL needs no token");

        // a write to another table moves the bookmark on: the tables are exported, found the same, and not loaded again
        *state.bookmark.lock().unwrap() = "bm-2".into();
        let third = client.sync(&path).await.unwrap();

        assert_eq!(third, Synced { changed: false, exported: true, bookmark: "bm-2".into() });
        assert_eq!(state.exports.load(Ordering::SeqCst), 2);
        assert_eq!(crate::mirror::mirror_bookmark(&path).await.unwrap().as_deref(), Some("bm-2"), "and it is not exported for bm-2 again");
        assert_eq!(client.sync(&path).await.unwrap(), Synced { changed: false, exported: false, bookmark: "bm-2".into() });
        assert_eq!(state.exports.load(Ordering::SeqCst), 2);

        // a write to the tables changes the data: the mirror is rebuilt
        *state.bookmark.lock().unwrap() = "bm-3".into();
        *state.sql.lock().unwrap() = EXPORT.replace("2026年07月号", "2026年08月号");
        let fourth = client.sync(&path).await.unwrap();

        assert_eq!(fourth, Synced { changed: true, exported: true, bookmark: "bm-3".into() });
        assert_eq!(mirror_issues(&path).await.unwrap()[0].name, "COMIC快楽天 2026年08月号");

        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn a_wrong_token_is_an_error_with_the_message_of_cloudflare() {
        let (port, _) = mock().await;
        let path = mirror_path("token");

        let error = client(port, "wrong").sync(&path).await.unwrap_err().to_string();

        assert!(error.contains("403") && error.contains("Authentication error"), "{error}");
    }

    #[test]
    fn an_unreachable_server_is_an_error() {
        let error = client(1, "secret").bookmark().unwrap_err().to_string();

        assert!(error.contains("cannot reach Cloudflare"), "{error}");
    }
}
