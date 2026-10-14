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

mod diff;
pub mod highlight;
mod markdown;
mod pages;
mod view;

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
    pub started: Instant,
}

pub type WebState = Arc<Web>;

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
            refs: moka::sync::Cache::builder().time_to_live(std::time::Duration::from_millis(1500)).max_capacity(10_000).build(),
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
        .route("/-/login", get(pages::login_form).post(pages::login))
        .route("/-/logout", post(pages::logout))
        .route("/-/healthz", get(|| async { "ok" }))
        .route("/-/readyz", get(pages::readyz))
        .route("/-/metrics", get(pages::metrics))
        .route("/{*rest}", get(pages::repo_get).post(pages::repo_post))
        .fallback(|| async { WebError::NotFound("page not found".into()) })
        .with_state(web)
}

async fn static_asset(State(web): State<WebState>, axum::extract::Path(file): axum::extract::Path<String>) -> Response {
    let immutable = HeaderValue::from_static("public, max-age=31536000, immutable");
    if format!("/-/static/{file}") == web.css_path {
        return ([(header::CONTENT_TYPE, HeaderValue::from_static("text/css; charset=utf-8")), (header::CACHE_CONTROL, immutable)], web.css.clone())
            .into_response();
    }
    if format!("/-/static/{file}") == web.js_path {
        return (
            [(header::CONTENT_TYPE, HeaderValue::from_static("text/javascript; charset=utf-8")), (header::CACHE_CONTROL, immutable)],
            APP_JS,
        )
            .into_response();
    }
    if file == "favicon.svg" {
        return ([(header::CONTENT_TYPE, HeaderValue::from_static("image/svg+xml")), (header::CACHE_CONTROL, immutable)], view::FAVICON)
            .into_response();
    }
    WebError::NotFound("no such asset".into()).into_response()
}

/// Per-request context: start time and the caller.
pub struct Req {
    pub started: Instant,
    pub principal: Principal,
    pub path: String,
    pub query: String,
    pub partial: bool,
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
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
            Err(_) => Principal { user: None, mode: web.flux.auth_mode },
        };
        Ok(Req {
            started,
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
