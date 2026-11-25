//! The sites behind a Cloudflare challenge (`Support::Cloudflare`).
//!
//! With `[cloudflare] enabled = true` they are read like any other site, with what a server of the user's own gives (`[cloudflare] api`): it is asked for
//! the address of the site and answers the cookie that gets past the challenge (`cf_clearance` ...), the User-Agent the cookie was made with (Cloudflare
//! ties the two together) and the address of the proxy the cookie was made behind (the cookie is also tied to the address of the one who asked), or `null`
//! for none:
//!
//! ```text
//! GET {api}?url=https://example.com
//! {"cookie": "cf_clearance=...; ...", "user_agent": "Mozilla/5.0 ...", "proxy": "http://host:3128"}
//! ```
//!
//! The answer is kept for `cache_minutes`; a page that Cloudflare still turns away (403, 429 or 503) makes the engine forget it and ask once more.

use crate::config::CloudflareConfig;
use serde::Deserialize;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// What the server answers.
#[derive(Debug, Deserialize)]
struct Passport {
    /// The `Cookie` header to send.
    #[serde(default)]
    cookie: String,
    /// The User-Agent the cookie belongs to; empty: the one of the engine.
    #[serde(default)]
    user_agent: Option<String>,
    /// The address of a proxy (`http://`, `https://`, `socks5://`); `null` or empty: none.
    #[serde(default)]
    proxy: Option<String>,
}

struct Entry {
    client: reqwest::Client,
    cookie: String,
    at: Instant,
}

pub struct Solver {
    api: String,
    ttl: Duration,
    timeout: Duration,
    user_agent: &'static str,
    /// Asks the server of the cookies (never through a proxy).
    http: reqwest::Client,
    /// By the address of the site.
    kept: Mutex<HashMap<String, Entry>>,
}

impl Solver {
    /// `None` when `[cloudflare]` is off. An address that is missing is an error, so that it shows at once.
    pub fn new(config: &CloudflareConfig, timeout: Duration, user_agent: &'static str) -> Result<Option<Self>, String> {
        if !config.enabled {
            return Ok(None);
        }
        if config.api.trim().is_empty() {
            return Err("[cloudflare] enabled = true needs api (the address of the server that gives the cookies)".to_string());
        }
        let http = reqwest::Client::builder().timeout(timeout).user_agent(user_agent).build().map_err(|e| format!("[cloudflare]: {e}"))?;

        Ok(Some(Self { api: config.api.trim().to_string(), ttl: Duration::from_secs(config.cache_minutes.max(1) * 60), timeout, user_agent, http, kept: Mutex::new(HashMap::new()) }))
    }

    /// The client (with the User-Agent and the proxy of the passport) and the cookie to send to the site at `origin`.
    pub async fn client_for(&self, origin: &str) -> Result<(reqwest::Client, String), String> {
        let mut kept = self.kept.lock().await;

        if let Some(entry) = kept.get(origin) {
            if entry.at.elapsed() < self.ttl {
                return Ok((entry.client.clone(), entry.cookie.clone()));
            }
        }

        let passport = self.ask(origin).await?;
        let mut builder = reqwest::Client::builder().timeout(self.timeout).user_agent(passport.user_agent.filter(|ua| !ua.trim().is_empty()).unwrap_or_else(|| self.user_agent.to_string()));
        if let Some(proxy) = passport.proxy.as_deref().map(str::trim).filter(|proxy| !proxy.is_empty()) {
            builder = builder.proxy(reqwest::Proxy::all(proxy).map_err(|_| "[cloudflare] the proxy that the server answered is not an address".to_string())?);
        }
        let client = builder.build().map_err(|e| format!("[cloudflare]: {e}"))?;

        kept.insert(origin.to_string(), Entry { client: client.clone(), cookie: passport.cookie.clone(), at: Instant::now() });

        Ok((client, passport.cookie))
    }

    /// The passport is no good any more (Cloudflare turned a page away): the next request asks for a new one.
    pub async fn forget(&self, origin: &str) {
        self.kept.lock().await.remove(origin);
    }

    async fn ask(&self, origin: &str) -> Result<Passport, String> {
        let response = self.http.get(&self.api).query(&[("url", origin)]).send().await.map_err(|e| format!("[cloudflare] cannot reach the server of the cookies: {}", e.without_url()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!("[cloudflare] the server of the cookies answered HTTP {}", status.as_u16()));
        }

        response.json::<Passport>().await.map_err(|_| "[cloudflare] the server of the cookies did not answer {cookie, user_agent, proxy} as JSON".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    async fn serve(body: &'static str, hits: Arc<AtomicUsize>) -> String {
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(move |axum::extract::Query(q): axum::extract::Query<HashMap<String, String>>| {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(q.get("url").map(String::as_str), Some("https://site.example"));
                    ([("content-type", "application/json")], body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        format!("http://{address}/")
    }

    fn config(api: String) -> CloudflareConfig {
        CloudflareConfig { enabled: true, api, cache_minutes: 30 }
    }

    #[test]
    fn it_is_off_unless_enabled_and_needs_an_address() {
        assert!(Solver::new(&CloudflareConfig::default(), Duration::from_secs(5), "ua").unwrap().is_none());
        assert!(Solver::new(&CloudflareConfig { enabled: true, ..Default::default() }, Duration::from_secs(5), "ua").is_err());
    }

    #[tokio::test]
    async fn the_cookie_is_asked_once_kept_and_asked_again_after_forgetting() {
        let hits = Arc::new(AtomicUsize::new(0));
        let api = serve(r#"{"cookie":"cf_clearance=abc","user_agent":"UA/1","proxy":null}"#, hits.clone()).await;
        let solver = Solver::new(&config(api), Duration::from_secs(5), "ua").unwrap().unwrap();

        assert_eq!(solver.client_for("https://site.example").await.unwrap().1, "cf_clearance=abc");
        solver.client_for("https://site.example").await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1, "kept");

        solver.forget("https://site.example").await;
        solver.client_for("https://site.example").await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_proxy_that_is_no_address_and_an_answer_that_is_no_json_are_errors() {
        let bad_proxy = serve(r#"{"cookie":"a=b","proxy":"not an address"}"#, Arc::new(AtomicUsize::new(0))).await;
        let solver = Solver::new(&config(bad_proxy), Duration::from_secs(5), "ua").unwrap().unwrap();
        assert!(solver.client_for("https://site.example").await.unwrap_err().contains("proxy"));

        let no_json = serve("hello", Arc::new(AtomicUsize::new(0))).await;
        let solver = Solver::new(&config(no_json), Duration::from_secs(5), "ua").unwrap().unwrap();
        assert!(solver.client_for("https://site.example").await.unwrap_err().contains("JSON"));
    }

    #[tokio::test]
    async fn a_proxy_that_is_an_address_is_accepted() {
        let api = serve(r#"{"cookie":"a=b","user_agent":"UA/2","proxy":"http://127.0.0.1:3128"}"#, Arc::new(AtomicUsize::new(0))).await;
        let solver = Solver::new(&config(api), Duration::from_secs(5), "ua").unwrap().unwrap();

        assert!(solver.client_for("https://site.example").await.is_ok());
    }
}
