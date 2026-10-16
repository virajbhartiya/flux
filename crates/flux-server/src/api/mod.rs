//! The REST API (`/api/v1`): JSON over HTTP for everything the web UI
//! and CLI can do, shaped after GitHub's API where that helps tools and
//! people migrate. Agents get extra endpoints built for them: one-call
//! multi-file commits (`POST …/changes`), batch file reads, a repository
//! context summary, merge checks, waiting on checks, and an MCP server
//! (`/api/mcp`).
//!
//! Authentication: `Authorization: Bearer <token>` (or `token <token>`, or
//! HTTP Basic with the token as password). Browser sessions work for
//! reads, and for writes from the same origin.
//!
//! Repository names may contain one slash (`owner/name`); the API resolves
//! `/api/v1/repos/<owner>/<name>/...` and `/api/v1/repos/<name>/...`.

mod ci;
mod code;
mod collab;
mod import;
pub mod mcp;

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, post};
use flux_store::RepoId;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::auth::Principal;
use crate::core::{Flux, FluxError};

/// Largest request body (release assets and bulk imports are the big ones).
pub const MAX_BODY: usize = 512 << 20;

#[derive(Debug)]
pub struct ApiError(pub StatusCode, pub String);

impl From<FluxError> for ApiError {
    fn from(e: FluxError) -> Self {
        match e {
            FluxError::NotFound(m) => ApiError(StatusCode::NOT_FOUND, m),
            FluxError::InvalidArgument(m) => ApiError(StatusCode::UNPROCESSABLE_ENTITY, m),
            FluxError::FailedPrecondition(m) => ApiError(StatusCode::CONFLICT, m),
            FluxError::AlreadyExists(m) => ApiError(StatusCode::CONFLICT, m),
            FluxError::Unauthenticated(m) => ApiError(StatusCode::UNAUTHORIZED, m),
            FluxError::PermissionDenied(m) => ApiError(StatusCode::FORBIDDEN, m),
            FluxError::Internal(e) => {
                tracing::error!(error = ?e, "api: internal error");
                ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
            }
        }
    }
}

impl From<flux_store::StoreError> for ApiError {
    fn from(e: flux_store::StoreError) -> Self {
        FluxError::from(e).into()
    }
}

impl From<flux_derive::DeriveError> for ApiError {
    fn from(e: flux_derive::DeriveError) -> Self {
        FluxError::from(e).into()
    }
}

impl From<flux_core::Error> for ApiError {
    fn from(e: flux_core::Error) -> Self {
        FluxError::from(e).into()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let code = match self.0 {
            StatusCode::NOT_FOUND => "not_found",
            StatusCode::UNPROCESSABLE_ENTITY | StatusCode::BAD_REQUEST => "invalid",
            StatusCode::CONFLICT => "conflict",
            StatusCode::UNAUTHORIZED => "unauthenticated",
            StatusCode::FORBIDDEN => "forbidden",
            StatusCode::METHOD_NOT_ALLOWED => "method_not_allowed",
            _ => "internal",
        };
        let mut r = (self.0, axum::Json(json!({ "message": self.1, "error": code }))).into_response();
        r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        r
    }
}

pub type ApiResult<T = Response> = Result<T, ApiError>;

pub fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::UNPROCESSABLE_ENTITY, msg.into())
}

pub fn not_found(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, msg.into())
}

pub fn reply<T: Serialize>(v: T) -> ApiResult {
    Ok(axum::Json(v).into_response())
}

pub fn created<T: Serialize>(v: T) -> ApiResult {
    Ok((StatusCode::CREATED, axum::Json(v)).into_response())
}

pub fn no_content() -> ApiResult {
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// One API request.
pub struct Ctx {
    pub flux: Arc<Flux>,
    pub who: Principal,
    pub method: Method,
    pub headers: HeaderMap,
    pub query: HashMap<String, String>,
    pub body: Bytes,
    /// `scheme://host` as the client sees it.
    pub origin: String,
}

impl Ctx {
    pub fn parse<T: DeserializeOwned>(&self) -> ApiResult<T> {
        let body: &[u8] = if self.body.is_empty() { b"{}" } else { &self.body };
        serde_json::from_slice(body).map_err(|e| bad(format!("invalid JSON body: {e}")))
    }

    pub fn q(&self, k: &str) -> Option<&str> {
        self.query.get(k).map(String::as_str).filter(|v| !v.is_empty())
    }

    pub fn flag(&self, k: &str) -> bool {
        matches!(self.q(k), Some("1" | "true" | "yes"))
    }

    pub fn num(&self, k: &str, default: i64) -> ApiResult<i64> {
        match self.q(k) {
            Some(v) => v.parse().map_err(|_| bad(format!("{k} must be a number"))),
            None => Ok(default),
        }
    }

    /// `(limit, offset)` from `per_page` (≤ 100, or `max`) and `page`.
    pub fn page(&self, default: u32, max: u32) -> ApiResult<(u32, u32)> {
        let per = self.num("per_page", default as i64)?.clamp(1, max as i64) as u32;
        let page = self.num("page", 1)?.max(1) as u32;
        Ok((per, (page - 1) * per))
    }

    pub fn user(&self) -> ApiResult<&flux_store::User> {
        self.who.user.as_ref().ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "authentication required".into()))
    }

    pub fn base_url(&self) -> String {
        self.flux.settings.public_url.clone().unwrap_or_else(|| self.origin.clone()).trim_end_matches('/').to_string()
    }
}

pub fn router(flux: Arc<Flux>) -> Router {
    Router::new()
        .route("/api/v1/{*rest}", any(dispatch))
        .route("/api/v1", any(dispatch_root))
        .route("/api/mcp", post(mcp::handle).get(mcp::handle_get))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY))
        .with_state(flux)
}

fn token_from(headers: &HeaderMap) -> Option<(String, bool)> {
    if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        let v = v.trim();
        if let Some(t) = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("token ")).or_else(|| v.strip_prefix("bearer ")) {
            return Some((t.trim().to_string(), false));
        }
        if let Some(b) = v.strip_prefix("Basic ") {
            use base64::Engine as _;
            let raw = base64::engine::general_purpose::STANDARD.decode(b.trim()).ok()?;
            let s = String::from_utf8(raw).ok()?;
            let (_, pw) = s.split_once(':')?;
            return Some((pw.to_string(), false));
        }
    }
    crate::web::cookie(headers, crate::web::COOKIE).map(|c| (c.to_string(), true))
}

pub async fn principal(flux: &Flux, headers: &HeaderMap, method: &Method) -> ApiResult<Principal> {
    match token_from(headers) {
        Some((t, from_cookie)) => {
            if from_cookie && !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
                // Cookie-authenticated writes must come from our own pages.
                let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
                let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()).unwrap_or("");
                let same = url::Url::parse(origin).ok().and_then(|u| {
                    u.host_str().map(|h| match u.port() {
                        Some(p) => format!("{h}:{p}"),
                        None => h.to_string(),
                    })
                });
                if same.as_deref() != Some(host) {
                    return Err(ApiError(StatusCode::FORBIDDEN, "cross-origin request with a session cookie".into()));
                }
            }
            match flux.principal(Some(&t)).await {
                Ok(p) => Ok(p),
                // A stale session cookie is anonymous, an invalid token is an error.
                Err(_) if from_cookie => Ok(Principal { user: None, mode: flux.auth_mode, ..Default::default() }),
                Err(e) => Err(e.into()),
            }
        }
        None => Ok(Principal { user: None, mode: flux.auth_mode, ..Default::default() }),
    }
}

fn parse_query(q: Option<&str>) -> HashMap<String, String> {
    url::form_urlencoded::parse(q.unwrap_or("").as_bytes()).into_owned().collect()
}

async fn dispatch_root() -> Response {
    axum::Json(json!({
        "name": "flux",
        "version": env!("CARGO_PKG_VERSION"),
        "docs": "https://github.com/virajbhartiya/flux/blob/main/docs/api.md",
        "mcp": "/api/mcp",
    }))
    .into_response()
}

async fn dispatch(State(flux): State<Arc<Flux>>, req: axum::extract::Request) -> Response {
    let started = std::time::Instant::now();
    let (parts, body) = req.into_parts();
    let path = parts.uri.path().trim_start_matches("/api/v1/").to_string();
    let query = parse_query(parts.uri.query());
    let method = parts.method.clone();
    let body = match axum::body::to_bytes(body, MAX_BODY).await {
        Ok(b) => b,
        Err(_) => return ApiError(StatusCode::PAYLOAD_TOO_LARGE, "request body too large".into()).into_response(),
    };
    let host = parts.headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("localhost").to_string();
    let scheme = parts.headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).unwrap_or("http").to_string();
    // Runner endpoints use runner tokens, not user tokens.
    let segs: Vec<String> = path.split('/').filter(|s| !s.is_empty()).map(percent_encoding_decode).collect();
    let who = if segs.first().map(String::as_str) == Some("ci") {
        Principal::default()
    } else {
        match principal(&flux, &parts.headers, &method).await {
            Ok(p) => p,
            Err(e) => return e.into_response(),
        }
    };
    let ctx = Ctx { flux, who, method, headers: parts.headers, query, body, origin: format!("{scheme}://{host}") };
    let seg: Vec<&str> = segs.iter().map(String::as_str).collect();
    let result = route(&ctx, &seg).await;
    let mut resp = result.unwrap_or_else(|e| e.into_response());
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&format!("app;dur={:.2}", started.elapsed().as_secs_f64() * 1000.0)) {
        h.insert("server-timing", v);
    }
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    resp
}

fn percent_encoding_decode(s: &str) -> String {
    url::form_urlencoded::parse(format!("x={}", s.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_else(|| s.to_string())
}

fn method_not_allowed() -> ApiError {
    ApiError(StatusCode::METHOD_NOT_ALLOWED, "method not allowed".into())
}

async fn route(c: &Ctx, seg: &[&str]) -> ApiResult {
    let m = &c.method;
    match seg {
        ["user"] => collab::me(c).await,
        ["user", "repos"] => code::list_repos(c).await,
        ["user", "keys"] if m == Method::GET => collab::list_keys(c).await,
        ["user", "keys"] if m == Method::POST => collab::add_key(c).await,
        ["user", "keys", id] if m == Method::DELETE => collab::delete_key(c, id).await,
        ["user", "tokens"] if m == Method::POST => collab::create_token(c).await,
        ["notifications"] if m == Method::GET => collab::notifications(c).await,
        ["notifications"] if m == Method::PATCH || m == Method::PUT => collab::mark_read(c, None).await,
        ["notifications", id] if m == Method::PATCH => collab::mark_read(c, Some(id)).await,
        ["repos"] if m == Method::GET => code::list_repos(c).await,
        ["repos"] if m == Method::POST => code::create_repo(c).await,
        ["repos", rest @ ..] => {
            let (repo, name, tail) = resolve_repo(&c.flux, rest).await?;
            repo_route(c, repo, &name, tail).await
        }
        ["orgs", org, "hooks", tail @ ..] => {
            let o = c.flux.org_named(org).await?;
            collab::hooks(c, crate::hooks::Scope::Org(o.id), tail).await
        }
        ["orgs", org, "secrets", tail @ ..] => {
            let o = c.flux.org_named(org).await?;
            collab::secrets(c, ("org", o.id), tail).await
        }
        ["runners"] | ["runners", ..] => ci::runners(c, &seg[1..]).await,
        ["ci", tail @ ..] => ci::runner_protocol(c, tail).await,
        ["events"] => code::events(c).await,
        _ => Err(not_found("no such endpoint")),
    }
}

/// Split `owner/name/rest...` or `name/rest...` into a repository and the
/// remaining path.
async fn resolve_repo<'a>(flux: &Flux, rest: &'a [&'a str]) -> ApiResult<(RepoId, String, &'a [&'a str])> {
    if rest.len() >= 2 {
        let two = format!("{}/{}", rest[0], rest[1].trim_end_matches(".git"));
        if let Ok(id) = flux.repo(&two).await {
            return Ok((id, two, &rest[2..]));
        }
    }
    if let Some(first) = rest.first() {
        let one = first.trim_end_matches(".git").to_string();
        if let Ok(id) = flux.repo(&one).await {
            return Ok((id, one, &rest[1..]));
        }
    }
    Err(not_found("repository not found"))
}

async fn repo_route(c: &Ctx, repo: RepoId, name: &str, t: &[&str]) -> ApiResult {
    let m = &c.method;
    match t {
        [] if m == Method::GET => code::repo_info(c, repo).await,
        [] if m == Method::PATCH => code::update_repo(c, repo).await,
        [] if m == Method::DELETE => {
            c.flux.delete_repo_as(&c.who, repo).await?;
            no_content()
        }
        [] => Err(method_not_allowed()),
        ["refs"] => code::list_refs(c, repo, "").await,
        ["branches"] => code::list_refs(c, repo, "branches").await,
        ["tags"] => code::list_refs(c, repo, "tags").await,
        ["refs", rest @ ..] => code::ref_op(c, repo, &rest.join("/")).await,
        ["commits"] => code::log(c, repo).await,
        ["commits", rev] => code::commit(c, repo, rev).await,
        ["commits", rev, "status"] => code::combined_status(c, repo, rev).await,
        ["commits", rev, "statuses"] => code::list_statuses(c, repo, rev).await,
        ["statuses", rev] if m == Method::POST => code::post_status(c, repo, rev).await,
        ["tree", rev, path @ ..] => code::tree(c, repo, rev, &path.join("/")).await,
        ["tree"] => code::tree(c, repo, "", "").await,
        ["contents", path @ ..] => code::contents(c, repo, &path.join("/")).await,
        ["raw", rev, path @ ..] => code::raw(c, repo, rev, &path.join("/")).await,
        ["files"] if m == Method::POST => code::read_files(c, repo).await,
        ["changes"] if m == Method::POST => code::changes(c, repo).await,
        ["compare", spec] => code::compare(c, repo, spec).await,
        ["search"] => code::search(c, repo).await,
        ["find"] => code::find(c, repo).await,
        ["blame", rev, path @ ..] => code::blame(c, repo, rev, &path.join("/")).await,
        ["context"] => code::context(c, repo, name).await,
        ["merge-check"] if m == Method::POST => code::merge_check(c, repo).await,
        ["archive", file] => code::archive(c, repo, name, file).await,
        ["subscription"] if m == Method::PUT => collab::subscribe(c, repo, 0).await,
        ["issues", tail @ ..] => collab::issues(c, repo, tail).await,
        ["labels", tail @ ..] => collab::labels(c, repo, tail).await,
        ["milestones", tail @ ..] => collab::milestones(c, repo, tail).await,
        ["reviews", tail @ ..] | ["pulls", tail @ ..] => collab::reviews(c, repo, tail).await,
        ["hooks", tail @ ..] => collab::hooks(c, crate::hooks::Scope::Repo(repo), tail).await,
        ["protection"] => collab::protection(c, repo).await,
        ["releases", tail @ ..] => collab::releases(c, repo, tail).await,
        ["forks"] => collab::forks(c, repo).await,
        ["secrets", tail @ ..] => collab::secrets(c, ("repo", repo.0), tail).await,
        ["runs", tail @ ..] => ci::runs(c, repo, tail).await,
        ["jobs", id, "log"] => ci::job_log(c, repo, id).await,
        ["import"] if m == Method::POST => import::import(c, repo).await,
        _ => Err(not_found("no such endpoint")),
    }
}
