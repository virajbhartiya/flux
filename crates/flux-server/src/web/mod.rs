//! The flux web UI: a fast, server-rendered repository browser with code
//! review, search and live updates.
//!
//! Design for speed:
//! * every page is rendered on the server from in-memory caches (changesets,
//!   trees, lastmod nodes, highlighted files, file lists) in ~1 ms;
//! * a ~10 KB script turns navigation into partial page swaps with hover
//!   prefetching, so clicks feel instant;
//! * static assets are content-hashed and cached forever;
//! * all responses carry `Server-Timing`, and the footer shows render time.
//!
//! URLs follow GitLab's `/-/` convention so repository names may contain
//! slashes: `/{repo}/-/tree/{rev}/{path}`.

mod account;
mod diff;
pub mod highlight;
mod lang;
mod markdown;
mod pages;
mod view;

pub use diff::is_binary;
pub use lang::language;

use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::extract::{FromRequestParts, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header, request::Parts};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::auth::Principal;
use crate::core::{Flux, FluxError};

pub(crate) const APP_JS: &str = include_str!("assets/app.js");
pub(crate) const APP_CSS: &str = include_str!("assets/app.css");
pub(crate) const COOKIE: &str = "flux_token";

pub struct Web {
    pub flux: Arc<Flux>,
    pub hl: highlight::Highlighter,
    css: String,
    pub css_path: String,
    pub js_path: String,
    pub refs: moka::sync::Cache<flux_store::RepoId, Arc<Vec<(String, flux_core::ChangesetId)>>>,
    pub langs: moka::sync::Cache<flux_core::TreeId, Arc<Vec<(String, String, f64)>>>,
    /// Newly created tokens awaiting their one-time display, by a random
    /// handle (never put a secret in a URL).
    pub new_tokens: moka::sync::Cache<String, (i64, String)>,
    /// Rendered diffs by (old tree, new tree, path filter, line budget).
    diffs: moka::sync::Cache<DiffKey, Arc<Vec<pages::RenderedFile>>>,
    pub started: Instant,
}

pub type WebState = Arc<Web>;
type DiffKey = (Option<flux_core::TreeId>, Option<flux_core::TreeId>, Option<flux_core::RepoPath>, usize);

impl Web {
    pub fn new(flux: Arc<Flux>) -> Arc<Self> {
        highlight::warm_up();
        let css = format!("{APP_CSS}\n/* syntax themes */\n{}", highlight::theme_css());
        let short = |s: &str| blake3::hash(s.as_bytes()).to_hex()[..12].to_string();
        Arc::new(Self {
            css_path: format!("/-/static/app.{}.css", short(&css)),
            js_path: format!("/-/static/app.{}.js", short(APP_JS)),
            css,
            flux,
            hl: highlight::Highlighter::default(),
            langs: moka::sync::Cache::new(10_000),
            refs: moka::sync::Cache::builder().time_to_live(std::time::Duration::from_millis(1500)).max_capacity(10_000).build(),
            diffs: moka::sync::Cache::builder()
                .weigher(|_, v: &Arc<Vec<pages::RenderedFile>>| (v.len() as u32).saturating_mul(4096).max(1024))
                .max_capacity(256 << 20)
                .build(),
            new_tokens: moka::sync::Cache::builder()
                .time_to_live(std::time::Duration::from_secs(300))
                .max_capacity(10_000)
                .build(),
            started: Instant::now(),
        })
    }
}

pub fn router(flux: Arc<Flux>) -> Router {
    let web = Web::new(flux.clone());
    // Keep the ref cache fresh on ref moves (TTL covers other instances).
    {
        let web = web.clone();
        let mut rx = flux.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(crate::core::Event::RefMoved { .. }) => web.refs.invalidate_all(),
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => web.refs.invalidate_all(),
                    Err(_) => return,
                }
            }
        });
    }
    Router::new()
        .route("/", get(pages::home))
        .route("/-/static/{file}", get(static_asset))
        .route("/-/static/fonts/{file}", get(font_asset))
        .route("/-/login", get(account::login_form).post(account::login))
        .route("/-/logout", post(account::logout))
        .route("/-/signup", get(account::signup_form).post(account::signup))
        .route("/-/settings", get(account::profile))
        .route("/-/settings/tokens", get(account::tokens).post(account::token_create))
        .route("/-/settings/tokens/{id}/revoke", post(account::token_revoke))
        .route("/-/settings/orgs", get(account::my_orgs))
        .route("/-/settings/{what}", post(account::profile_post))
        .route("/-/orgs", post(account::org_create))
        .route("/-/orgs/{org}/people", get(account::org_people).post(account::org_people_post))
        .route("/-/orgs/{org}/teams", get(account::org_teams).post(account::org_team_create))
        .route("/-/orgs/{org}/teams/{team}", get(account::org_team).post(account::org_team_post))
        .route("/-/orgs/{org}/settings", get(account::org_settings).post(account::org_settings_post))
        .route("/-/new", get(account::new_repo))
        .route("/-/repos", post(account::create_repo))
        .route("/-/admin", get(account::admin_users))
        .route("/-/admin/users", post(account::admin_users_post))
        .route("/-/admin/users/{name}", post(account::admin_user_post))
        .route("/-/admin/audit", get(account::admin_audit))
        .route("/-/repos.json", get(pages::repos_json))
        .route("/-/healthz", get(|| async { "ok" }))
        .route("/-/readyz", get(pages::readyz))
        .route("/-/metrics", get(pages::metrics))
        .route("/{*rest}", get(dispatch_get).post(dispatch_post))
        .fallback(|| async { WebError::NotFound("page not found".into()) })
        .with_state(web)
}

/// git smart-HTTP requests share the repository URL space with the web UI.
async fn dispatch_get(State(web): State<WebState>, req: axum::extract::Request) -> Response {
    let path = req.uri().path().trim_start_matches('/').to_string();
    if let Some((repo, op)) = crate::git::http::route(&path) {
        let query = req.uri().query().unwrap_or("").to_string();
        let (parts, body) = req.into_parts();
        return crate::git::http::handle(web.flux.clone(), &pages::decode_repo(&repo), op, &parts.headers, &query, body).await;
    }
    let (mut parts, body) = req.into_parts();
    let r = Req::from_request_parts(&mut parts, &web).await;
    let p = axum::extract::Path::<String>::from_request_parts(&mut parts, &web).await;
    drop(body);
    match (r, p) {
        (Ok(r), Ok(p)) => pages::repo_get(State(web), r, p).await.into_response(),
        (Err(e), _) => e.into_response(),
        (_, Err(e)) => e.into_response(),
    }
}

async fn dispatch_post(State(web): State<WebState>, req: axum::extract::Request) -> Response {
    use axum::extract::FromRequest;
    let path = req.uri().path().trim_start_matches('/').to_string();
    if let Some((repo, op)) = crate::git::http::route(&path) {
        let query = req.uri().query().unwrap_or("").to_string();
        let (parts, body) = req.into_parts();
        return crate::git::http::handle(web.flux.clone(), &pages::decode_repo(&repo), op, &parts.headers, &query, body).await;
    }
    let (mut parts, body) = req.into_parts();
    let r = match Req::from_request_parts(&mut parts, &web).await {
        Ok(r) => r,
        Err(e) => return e.into_response(),
    };
    let p = match axum::extract::Path::<String>::from_request_parts(&mut parts, &web).await {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    let headers = parts.headers.clone();
    if let Some(repo) = path.strip_suffix("/-/settings/access") {
        let f = match axum::extract::Form::<account::AccountForm>::from_request(
            axum::extract::Request::from_parts(parts, body),
            &web,
        )
        .await
        {
            Ok(f) => f.0,
            Err(e) => return e.into_response(),
        };
        if let Err(e) = account::check_origin(&headers) {
            return e.into_response();
        }
        return pages::repo_access_post(&web, &r, &pages::decode_repo(repo), &f).await.into_response();
    }
    let form = match axum::extract::Form::<pages::ReviewForm>::from_request(axum::extract::Request::from_parts(parts, body), &web)
        .await
    {
        Ok(f) => f,
        Err(e) => return e.into_response(),
    };
    pages::repo_post(State(web), r, headers, p, form).await.into_response()
}

async fn static_asset(State(web): State<WebState>, axum::extract::Path(file): axum::extract::Path<String>) -> Response {
    let immutable = HeaderValue::from_static("public, max-age=31536000, immutable");
    if format!("/-/static/{file}") == web.css_path {
        return (
            [(header::CONTENT_TYPE, HeaderValue::from_static("text/css; charset=utf-8")), (header::CACHE_CONTROL, immutable)],
            web.css.clone(),
        )
            .into_response();
    }
    if format!("/-/static/{file}") == web.js_path {
        return (
            [
                (header::CONTENT_TYPE, HeaderValue::from_static("text/javascript; charset=utf-8")),
                (header::CACHE_CONTROL, immutable),
            ],
            APP_JS,
        )
            .into_response();
    }
    if file == "favicon.svg" {
        return (
            [(header::CONTENT_TYPE, HeaderValue::from_static("image/svg+xml")), (header::CACHE_CONTROL, immutable)],
            view::FAVICON,
        )
            .into_response();
    }
    WebError::NotFound("no such asset".into()).into_response()
}

async fn font_asset(axum::extract::Path(file): axum::extract::Path<String>) -> Response {
    let data: &'static [u8] = match file.as_str() {
        "inter.woff2" => include_bytes!("assets/fonts/inter.woff2"),
        "mono.woff2" => include_bytes!("assets/fonts/mono.woff2"),
        _ => return WebError::NotFound("no such font".into()).into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static("font/woff2")),
            (header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=31536000, immutable")),
        ],
        data,
    )
        .into_response()
}

/// Per-request context: start time and the caller.
pub struct Req {
    pub started: Instant,
    /// `scheme://host` as seen by the client.
    pub origin: String,
    pub principal: Principal,
    pub path: String,
    pub query: String,
    pub partial: bool,
}

pub(crate) fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == name).then_some(v)
    })
}

impl FromRequestParts<WebState> for Req {
    type Rejection = WebError;

    async fn from_request_parts(parts: &mut Parts, web: &WebState) -> Result<Self, WebError> {
        let started = Instant::now();
        let token = cookie(&parts.headers, COOKIE).map(str::to_string).or_else(|| {
            parts
                .headers
                .get(header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.strip_prefix("Bearer ").unwrap_or(v).to_string())
        });
        let principal = match web.flux.principal(token.as_deref()).await {
            Ok(p) => p,
            // A stale cookie shouldn't lock anyone out: treat as anonymous.
            Err(_) => Principal { user: None, mode: web.flux.auth_mode, ..Default::default() },
        };
        let host = parts.headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("localhost");
        let scheme = parts.headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).unwrap_or("http");
        Ok(Req {
            started,
            origin: format!("{scheme}://{host}"),
            principal,
            path: parts.uri.path().to_string(),
            query: parts.uri.query().unwrap_or("").to_string(),
            partial: parts.headers.contains_key("x-flux-partial"),
        })
    }
}

#[derive(Debug)]
pub enum WebError {
    NotFound(String),
    BadRequest(String),
    Unauthorized(String),
    Forbidden(String),
    Conflict(String),
    Internal(String),
}

impl From<FluxError> for WebError {
    fn from(e: FluxError) -> Self {
        match e {
            FluxError::NotFound(m) => WebError::NotFound(m),
            FluxError::InvalidArgument(m) => WebError::BadRequest(m),
            FluxError::FailedPrecondition(m) => WebError::Conflict(m),
            FluxError::AlreadyExists(m) => WebError::Conflict(m),
            FluxError::Unauthenticated(m) => WebError::Unauthorized(m),
            FluxError::PermissionDenied(m) => WebError::Forbidden(m),
            FluxError::Internal(e) => {
                tracing::error!(error = ?e, "web: internal error");
                WebError::Internal(e.to_string())
            }
        }
    }
}

impl From<flux_derive::DeriveError> for WebError {
    fn from(e: flux_derive::DeriveError) -> Self {
        FluxError::from(e).into()
    }
}

impl From<flux_store::StoreError> for WebError {
    fn from(e: flux_store::StoreError) -> Self {
        FluxError::from(e).into()
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let (status, title, msg) = match self {
            WebError::NotFound(m) => (StatusCode::NOT_FOUND, "Not found", m),
            WebError::BadRequest(m) => (StatusCode::BAD_REQUEST, "Bad request", m),
            WebError::Unauthorized(m) => (StatusCode::UNAUTHORIZED, "Sign in required", m),
            WebError::Forbidden(m) => (StatusCode::FORBIDDEN, "Forbidden", m),
            WebError::Conflict(m) => (StatusCode::CONFLICT, "Conflict", m),
            WebError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, "Something went wrong", m),
        };
        let body = view::error_page(status.as_u16(), title, &msg);
        let mut r = (status, body).into_response();
        let h = r.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
        security_headers(h);
        // The error page is self-contained (inline styles, no scripts).
        h.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("default-src 'none'; img-src 'self'; style-src 'unsafe-inline'; frame-ancestors 'none'"),
        );
        r
    }
}

pub type WebResult<T = Response> = Result<T, WebError>;

/// Content-Security-Policy for app pages: no inline scripts, no third-party
/// anything.
pub fn security_headers(h: &mut HeaderMap) {
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; img-src 'self' data: https:; style-src 'self'; script-src 'self'; \
             connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    );
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
}
