//! GitHub REST client: token auth, `Link`-header pagination, rate limits
//! (primary and secondary), retries with backoff, and ETag caching so
//! mirror re-syncs of unchanged lists cost no rate limit.

use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use reqwest::header::{self, HeaderMap};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const DEFAULT_API: &str = "https://api.github.com";
const JSON: &str = "application/vnd.github+json";
const PER_PAGE: u32 = 100;
const MAX_ATTEMPTS: u32 = 6;

/// A non-success response from GitHub.
#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    pub path: String,
    pub message: String,
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "GitHub returned {} for {}: {}", self.status, self.path, self.message)
    }
}

impl std::error::Error for HttpError {}

/// The HTTP status of a GitHub error, if `e` is one.
pub fn status_of(e: &anyhow::Error) -> Option<u16> {
    e.downcast_ref::<HttpError>().map(|h| h.status)
}

/// A cached response page for conditional requests.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedPage {
    pub etag: String,
    pub body: String,
    #[serde(default)]
    pub next: Option<String>,
}

pub struct GitHub {
    http: reqwest::Client,
    api: String,
    token: Option<String>,
    cache: Mutex<HashMap<String, CachedPage>>,
    date_ms: AtomicI64,
    requests: AtomicU64,
}

impl fmt::Debug for GitHub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GitHub")
            .field("api", &self.api)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

struct Resp {
    status: u16,
    headers: HeaderMap,
    body: Bytes,
}

fn header_u64(h: &HeaderMap, k: &str) -> Option<u64> {
    h.get(k)?.to_str().ok()?.trim().parse().ok()
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1 << attempt.min(5))
}

/// How long to wait before retrying a failed request, or `None` to give
/// up. `now` is unix seconds; `attempt` counts from 0.
///
/// Primary rate limits wait for `x-ratelimit-reset`; secondary limits honor
/// `retry-after` or back off from a minute; 5xx responses back off
/// exponentially from a second.
pub fn retry_wait(status: u16, h: &HeaderMap, body: &str, now: u64, attempt: u32) -> Option<Duration> {
    if attempt + 1 >= MAX_ATTEMPTS {
        return None;
    }
    let exhausted = header_u64(h, "x-ratelimit-remaining") == Some(0);
    let limited = status == 429
        || (status == 403
            && (exhausted || h.contains_key(header::RETRY_AFTER) || body.to_ascii_lowercase().contains("rate limit")));
    if limited {
        if let Some(s) = header_u64(h, header::RETRY_AFTER.as_str()) {
            return Some(Duration::from_secs(s.max(1)));
        }
        if exhausted && let Some(reset) = header_u64(h, "x-ratelimit-reset") {
            return Some(Duration::from_secs(reset.saturating_sub(now) + 1));
        }
        return Some(Duration::from_secs((60u64 << attempt).min(900)));
    }
    (status >= 500).then(|| backoff(attempt))
}

/// After a successful response that used the last request of the window:
/// how long to wait for the reset.
pub fn exhausted_wait(h: &HeaderMap, now: u64) -> Option<Duration> {
    if header_u64(h, "x-ratelimit-remaining") != Some(0) {
        return None;
    }
    header_u64(h, "x-ratelimit-reset").map(|reset| Duration::from_secs(reset.saturating_sub(now) + 1))
}

/// The `rel="next"` URL of a `Link` header.
pub fn next_link(link: &str) -> Option<String> {
    link.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        let next = params.split(';').any(|p| {
            p.trim()
                .strip_prefix("rel=")
                .is_some_and(|v| v.trim_matches('"').split_whitespace().any(|r| r.eq_ignore_ascii_case("next")))
        });
        next.then(|| url.trim().trim_start_matches('<').trim_end_matches('>').to_string())
    })
}

fn with_per_page(url: String) -> String {
    if url.contains("per_page=") {
        url
    } else {
        let sep = if url.contains('?') { '&' } else { '?' };
        format!("{url}{sep}per_page={PER_PAGE}")
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The host of GitHub's web UI and git remote for an API base URL.
pub fn web_host(api: &str) -> String {
    let host = reqwest::Url::parse(api).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_else(|| "github".into());
    if host == "api.github.com" { "github.com".into() } else { host }
}

/// The GitHub token: `explicit`, else `$GITHUB_TOKEN` / `$GH_TOKEN` (or the
/// enterprise variants for other hosts), else `gh auth token`.
pub fn resolve_token(explicit: Option<&str>, host: &str) -> Option<String> {
    let env = |k: &str| std::env::var(k).ok();
    let mut vars = vec!["GITHUB_TOKEN", "GH_TOKEN"];
    if host != "github.com" {
        vars.splice(0..0, ["GH_ENTERPRISE_TOKEN", "GITHUB_ENTERPRISE_TOKEN"]);
    }
    explicit
        .map(str::to_string)
        .into_iter()
        .chain(vars.into_iter().filter_map(env))
        .map(|t| t.trim().to_string())
        .find(|t| !t.is_empty())
        .or_else(|| {
            let out = std::process::Command::new("gh")
                .args(["auth", "token", "--hostname", host])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .ok()?;
            let t = String::from_utf8(out.stdout).ok()?.trim().to_string();
            (out.status.success() && !t.is_empty()).then_some(t)
        })
}

impl GitHub {
    pub fn new(api: &str, token: Option<String>) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("flux-migrate/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(900))
            .build()?;
        Ok(Self {
            http,
            api: api.trim_end_matches('/').to_string(),
            token: token.filter(|t| !t.is_empty()),
            cache: Mutex::default(),
            date_ms: AtomicI64::new(0),
            requests: AtomicU64::new(0),
        })
    }

    pub fn api(&self) -> &str {
        &self.api
    }

    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    pub fn host(&self) -> String {
        web_host(&self.api)
    }

    /// Requests sent so far (retries included).
    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// GitHub's clock, from the last response's `Date` header.
    pub fn server_time_ms(&self) -> Option<i64> {
        Some(self.date_ms.load(Ordering::Relaxed)).filter(|t| *t > 0)
    }

    pub fn load_cache(&self, pages: HashMap<String, CachedPage>) {
        *self.cache.lock().unwrap_or_else(|e| e.into_inner()) = pages;
    }

    pub fn take_cache(&self) -> HashMap<String, CachedPage> {
        std::mem::take(&mut *self.cache.lock().unwrap_or_else(|e| e.into_inner()))
    }

    fn url(&self, path: &str) -> String {
        if path.starts_with("http://") || path.starts_with("https://") { path.to_string() } else { format!("{}{path}", self.api) }
    }

    fn shown<'a>(&self, url: &'a str) -> &'a str {
        url.strip_prefix(self.api.as_str()).unwrap_or(url)
    }

    async fn send(&self, url: &str, accept: &str, etag: Option<&str>) -> Result<Resp> {
        let mut attempt = 0;
        loop {
            let mut req = self.http.get(url).header(header::ACCEPT, accept).header("x-github-api-version", "2022-11-28");
            if let Some(t) = &self.token {
                req = req.bearer_auth(t);
            }
            if let Some(e) = etag {
                req = req.header(header::IF_NONE_MATCH, e);
            }
            self.requests.fetch_add(1, Ordering::Relaxed);
            let sent = async {
                let r = req.send().await?;
                let (status, headers) = (r.status().as_u16(), r.headers().clone());
                Ok::<_, reqwest::Error>(Resp { status, headers, body: r.bytes().await? })
            }
            .await;
            let r = match sent {
                Ok(r) => r,
                Err(e) if attempt + 1 < MAX_ATTEMPTS => {
                    tracing::warn!(error = %e, "GitHub request failed; retrying");
                    tokio::time::sleep(backoff(attempt)).await;
                    attempt += 1;
                    continue;
                }
                Err(e) => return Err(anyhow!(e).context(format!("requesting {}", self.shown(url)))),
            };
            if let Some(d) = r.headers.get(header::DATE).and_then(|v| v.to_str().ok()).and_then(crate::time::parse_http_date) {
                self.date_ms.store(d, Ordering::Relaxed);
            }
            let now = unix_now();
            if (200..300).contains(&r.status) || r.status == 304 {
                if let Some(w) = exhausted_wait(&r.headers, now) {
                    tracing::warn!("GitHub rate limit used up; waiting {}s for the reset", w.as_secs());
                    tokio::time::sleep(w).await;
                }
                return Ok(r);
            }
            let text = String::from_utf8_lossy(&r.body);
            if let Some(w) = retry_wait(r.status, &r.headers, &text, now, attempt) {
                tracing::warn!(status = r.status, "GitHub {}; retrying in {}s", self.shown(url), w.as_secs());
                tokio::time::sleep(w).await;
                attempt += 1;
                continue;
            }
            let message = serde_json::from_slice::<serde_json::Value>(&r.body)
                .ok()
                .and_then(|v| v["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| text.chars().take(200).collect());
            return Err(HttpError { status: r.status, path: self.shown(url).to_string(), message }.into());
        }
    }

    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let r = self.send(&self.url(path), JSON, None).await?;
        serde_json::from_slice(&r.body).with_context(|| format!("decoding {path}"))
    }

    /// Like `get`, with 404 as `None`.
    pub async fn get_opt<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>> {
        match self.get(path).await {
            Ok(v) => Ok(Some(v)),
            Err(e) if status_of(&e) == Some(404) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Every page of a list.
    pub async fn list<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>> {
        self.list_until(path, |_| true).await
    }

    /// Pages of a list until `keep` rejects an item (for lists sorted by
    /// update time without a `since` parameter).
    pub async fn list_until<T: DeserializeOwned>(&self, path: &str, mut keep: impl FnMut(&T) -> bool) -> Result<Vec<T>> {
        let mut url = Some(with_per_page(self.url(path)));
        let mut out = Vec::new();
        while let Some(u) = url.take() {
            let r = self.send(&u, JSON, None).await?;
            let page: Vec<T> = serde_json::from_slice(&r.body).with_context(|| format!("decoding {}", self.shown(&u)))?;
            if page.is_empty() {
                break;
            }
            for item in page {
                if !keep(&item) {
                    return Ok(out);
                }
                out.push(item);
            }
            url = r.headers.get(header::LINK).and_then(|v| v.to_str().ok()).and_then(next_link);
        }
        Ok(out)
    }

    /// Every page of a list, with conditional requests: unchanged pages come
    /// from the cache. Returns the items and whether any page changed.
    pub async fn list_cached<T: DeserializeOwned>(&self, path: &str) -> Result<(Vec<T>, bool)> {
        let mut url = Some(with_per_page(self.url(path)));
        let (mut out, mut changed) = (Vec::new(), false);
        while let Some(u) = url.take() {
            let cached = self.cache.lock().unwrap_or_else(|e| e.into_inner()).get(&u).cloned();
            let r = self.send(&u, JSON, cached.as_ref().map(|c| c.etag.as_str())).await?;
            let live_next = r.headers.get(header::LINK).and_then(|v| v.to_str().ok()).and_then(next_link);
            let (body, next) = match cached {
                Some(c) if r.status == 304 => (Bytes::from(c.body), live_next.or(c.next)),
                _ => {
                    changed = true;
                    if let Some(etag) = r.headers.get(header::ETAG).and_then(|v| v.to_str().ok()) {
                        let page = CachedPage {
                            etag: etag.to_string(),
                            body: String::from_utf8_lossy(&r.body).into_owned(),
                            next: live_next.clone(),
                        };
                        self.cache.lock().unwrap_or_else(|e| e.into_inner()).insert(u.clone(), page);
                    }
                    (r.body, live_next)
                }
            };
            let page: Vec<T> = serde_json::from_slice(&body).with_context(|| format!("decoding {}", self.shown(&u)))?;
            out.extend(page);
            url = next;
        }
        Ok((out, changed))
    }

    /// Download a release asset (its API URL).
    pub async fn download(&self, url: &str) -> Result<Bytes> {
        Ok(self.send(&self.url(url), "application/octet-stream", None).await?.body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(*k, v.parse().unwrap());
        }
        h
    }

    #[test]
    fn link_header_pagination() {
        let link = r#"<https://api.github.com/repositories/1/issues?page=2&per_page=100>; rel="next", <https://api.github.com/repositories/1/issues?page=9&per_page=100>; rel="last""#;
        assert_eq!(next_link(link).as_deref(), Some("https://api.github.com/repositories/1/issues?page=2&per_page=100"));
        let last = r#"<https://api.github.com/x?page=1>; rel="first", <https://api.github.com/x?page=8>; rel="prev""#;
        assert_eq!(next_link(last), None);
        assert_eq!(next_link(r#"<https://h/x?page=3>; rel=next"#).as_deref(), Some("https://h/x?page=3"));
        assert_eq!(next_link(""), None);
        assert_eq!(with_per_page("https://h/x".into()), "https://h/x?per_page=100");
        assert_eq!(with_per_page("https://h/x?state=all".into()), "https://h/x?state=all&per_page=100");
        assert_eq!(with_per_page("https://h/x?per_page=5".into()), "https://h/x?per_page=5");
    }

    #[test]
    fn rate_limit_waits() {
        let now = 1_000_000;
        // Primary limit: wait for the reset.
        let h = headers(&[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "1000030")]);
        assert_eq!(retry_wait(403, &h, "API rate limit exceeded", now, 0), Some(Duration::from_secs(31)));
        assert_eq!(retry_wait(429, &h, "", now, 0), Some(Duration::from_secs(31)));
        // A reset in the past still waits a moment.
        let h = headers(&[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "10")]);
        assert_eq!(retry_wait(403, &h, "", now, 0), Some(Duration::from_secs(1)));
        // Secondary limit: retry-after wins, else back off from a minute.
        let h = headers(&[("retry-after", "17"), ("x-ratelimit-remaining", "4000")]);
        assert_eq!(retry_wait(403, &h, "", now, 0), Some(Duration::from_secs(17)));
        let h = headers(&[("x-ratelimit-remaining", "4000")]);
        let body = "You have exceeded a secondary rate limit";
        assert_eq!(retry_wait(403, &h, body, now, 0), Some(Duration::from_secs(60)));
        assert_eq!(retry_wait(403, &h, body, now, 2), Some(Duration::from_secs(240)));
        // Plain permission errors are not retried.
        assert_eq!(retry_wait(403, &h, "Resource not accessible by integration", now, 0), None);
        assert_eq!(retry_wait(404, &h, "", now, 0), None);
        // Server errors back off exponentially, then give up.
        assert_eq!(retry_wait(502, &h, "", now, 0), Some(Duration::from_secs(1)));
        assert_eq!(retry_wait(503, &h, "", now, 3), Some(Duration::from_secs(8)));
        assert_eq!(retry_wait(500, &h, "", now, MAX_ATTEMPTS - 1), None);
        // Proactive waits after the last request of a window.
        let h = headers(&[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "1000009")]);
        assert_eq!(exhausted_wait(&h, now), Some(Duration::from_secs(10)));
        assert_eq!(exhausted_wait(&headers(&[("x-ratelimit-remaining", "1")]), now), None);
    }

    #[test]
    fn hosts_and_redaction() {
        assert_eq!(web_host(DEFAULT_API), "github.com");
        assert_eq!(web_host("https://ghe.example.com/api/v3"), "ghe.example.com");
        let gh = GitHub::new(DEFAULT_API, Some("ghp_secret".into())).unwrap();
        assert!(!format!("{gh:?}").contains("ghp_secret"));
        assert_eq!(resolve_token(Some(" t0k "), "github.com").as_deref(), Some("t0k"));
    }
}
