//! Git LFS: the batch API, basic transfers, verify, and the locks endpoints
//! git-lfs probes, at `<repo>.git/info/lfs/...`.
//!
//! Objects are stored as flux content (chunked when large; uploads stream
//! into chunks while their SHA-256 is checked), so identical files across
//! repositories are stored once. A repository's LFS index records which
//! oids it has; reads need read access, uploads write access. Over SSH,
//! `git-lfs-authenticate` hands out a short-lived token for these URLs.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use flux_core::ContentId;
use flux_store::{CHUNK_THRESHOLD, ContentLayout, RepoId};
use futures::StreamExt as _;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::Digest as _;

use crate::auth::Principal;
use crate::core::{Flux, FluxError, FluxResult};
use crate::rbac::Role;

const CTYPE: &str = "application/vnd.git-lfs+json";
/// Largest object accepted.
pub const MAX_OBJECT: u64 = 16 << 30;

/// Split `<repo>.git/info/lfs/<rest>` (or without `.git`).
pub fn route(path: &str) -> Option<(String, String)> {
    let (repo, rest) = path.trim_start_matches('/').split_once("/info/lfs")?;
    let repo = repo.strip_suffix(".git").unwrap_or(repo);
    (!repo.is_empty()).then(|| (repo.to_string(), rest.trim_start_matches('/').to_string()))
}

fn valid_oid(oid: &str) -> bool {
    oid.len() == 64 && oid.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

fn reply(status: StatusCode, v: Value) -> Response {
    let mut r = (status, v.to_string()).into_response();
    r.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(CTYPE));
    r
}

fn error(status: StatusCode, msg: &str) -> Response {
    let mut r = reply(status, json!({ "message": msg }));
    if status == StatusCode::UNAUTHORIZED {
        r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"flux\""));
    }
    r
}

fn from_flux(e: FluxError) -> Response {
    match e {
        FluxError::Unauthenticated(m) => error(StatusCode::UNAUTHORIZED, &m),
        FluxError::PermissionDenied(m) => error(StatusCode::FORBIDDEN, &m),
        FluxError::NotFound(m) => error(StatusCode::NOT_FOUND, &m),
        FluxError::InvalidArgument(m) => error(StatusCode::UNPROCESSABLE_ENTITY, &m),
        FluxError::FailedPrecondition(m) | FluxError::AlreadyExists(m) => error(StatusCode::CONFLICT, &m),
        FluxError::Internal(e) => {
            tracing::error!(error = ?e, "lfs: internal error");
            error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
        }
    }
}

#[derive(Deserialize)]
struct Batch {
    operation: String,
    objects: Vec<ObjSpec>,
}

#[derive(Deserialize)]
struct ObjSpec {
    oid: String,
    size: u64,
}

fn base_url(flux: &Flux, headers: &HeaderMap) -> String {
    if let Some(u) = &flux.settings.public_url {
        return u.trim_end_matches('/').to_string();
    }
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("localhost");
    let scheme = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).unwrap_or("http");
    format!("{scheme}://{host}")
}

/// Handle any request under `<repo>.git/info/lfs/`.
pub async fn handle(
    flux: Arc<Flux>,
    who: Principal,
    repo_name: &str,
    rest: &str,
    method: &Method,
    headers: &HeaderMap,
    body: Body,
) -> Response {
    let repo = match flux.repo(repo_name).await {
        Ok(r) => r,
        Err(_) if who.user.is_none() => return error(StatusCode::UNAUTHORIZED, "authentication required"),
        Err(_) => return error(StatusCode::NOT_FOUND, "repository not found"),
    };
    // Reading is the floor; uploads check write access themselves.
    if let Err(e) = flux.authorize(&who, repo, Role::Read).await {
        return if who.user.is_none() { error(StatusCode::UNAUTHORIZED, "authentication required") } else { from_flux(e) };
    }
    let result = match (method, rest) {
        (&Method::POST, "objects/batch") => match axum::body::to_bytes(body, 16 << 20).await {
            Ok(b) => batch(&flux, &who, repo, repo_name, headers, &b).await,
            Err(e) => return error(StatusCode::BAD_REQUEST, &e.to_string()),
        },
        (&Method::POST, "verify") => match axum::body::to_bytes(body, 1 << 20).await {
            Ok(b) => verify(&flux, repo, &b).await,
            Err(e) => return error(StatusCode::BAD_REQUEST, &e.to_string()),
        },
        (&Method::GET, r) if r.starts_with("objects/") => download(&flux, repo, &r["objects/".len()..]).await,
        (&Method::PUT, r) if r.starts_with("objects/") => match flux.authorize(&who, repo, Role::Write).await {
            Ok(_) => upload(&flux, repo, &r["objects/".len()..], headers, body).await,
            Err(e) => Err(e),
        },
        // No file locking yet: report none, so pushes proceed.
        (&Method::POST, "locks/verify") => Ok(reply(StatusCode::OK, json!({ "ours": [], "theirs": [] }))),
        (&Method::GET, "locks") => Ok(reply(StatusCode::OK, json!({ "locks": [] }))),
        (&Method::POST, "locks") => Ok(error(StatusCode::NOT_IMPLEMENTED, "file locking is not supported yet")),
        _ => Ok(error(StatusCode::NOT_FOUND, "no such LFS endpoint")),
    };
    match result {
        Ok(r) => r,
        // Clients send credentials only after a challenge.
        Err(FluxError::PermissionDenied(_) | FluxError::NotFound(_)) if who.user.is_none() => {
            error(StatusCode::UNAUTHORIZED, "authentication required")
        }
        Err(e) => from_flux(e),
    }
}

async fn batch(
    flux: &Flux,
    who: &Principal,
    repo: RepoId,
    repo_name: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> FluxResult<Response> {
    let b: Batch = serde_json::from_slice(body).map_err(|e| FluxError::InvalidArgument(format!("invalid batch request: {e}")))?;
    let upload = match b.operation.as_str() {
        "download" => false,
        "upload" => true,
        o => return Err(FluxError::InvalidArgument(format!("unknown operation {o:?}"))),
    };
    if upload {
        flux.authorize(who, repo, Role::Write).await?;
        flux.check_writable(repo).await?;
    }
    if b.objects.len() > 10_000 {
        return Err(FluxError::InvalidArgument("at most 10000 objects per batch".into()));
    }
    let oids: Vec<String> = b.objects.iter().map(|o| o.oid.clone()).collect();
    let have = flux.meta.lfs_get(repo, &oids).await?;
    let base = format!("{}/{repo_name}.git/info/lfs", base_url(flux, headers));
    // Hand the caller's credentials back for the transfer URLs.
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).map(str::to_string);
    let header = auth.map(|a| json!({ "Authorization": a })).unwrap_or(json!({}));
    let mut out = Vec::with_capacity(b.objects.len());
    for (o, have) in b.objects.iter().zip(have) {
        if !valid_oid(&o.oid) {
            out.push(json!({ "oid": o.oid, "size": o.size, "error": { "code": 422, "message": "invalid oid" } }));
            continue;
        }
        if o.size > MAX_OBJECT {
            out.push(json!({ "oid": o.oid, "size": o.size, "error": { "code": 422, "message": "object too large" } }));
            continue;
        }
        let href = format!("{base}/objects/{}", o.oid);
        match (upload, have) {
            (false, Some((_, size))) => out.push(json!({ "oid": o.oid, "size": size, "authenticated": true,
                "actions": { "download": { "href": href, "header": header, "expires_in": 3600 } } })),
            (false, None) => {
                out.push(json!({ "oid": o.oid, "size": o.size, "error": { "code": 404, "message": "object not found" } }))
            }
            // Already stored: nothing to send.
            (true, Some((_, size))) => out.push(json!({ "oid": o.oid, "size": size })),
            (true, None) => out.push(json!({ "oid": o.oid, "size": o.size, "authenticated": true, "actions": {
                "upload": { "href": href, "header": header, "expires_in": 3600 },
                "verify": { "href": format!("{base}/verify"), "header": header, "expires_in": 3600 },
            } })),
        }
    }
    Ok(reply(StatusCode::OK, json!({ "transfer": "basic", "objects": out, "hash_algo": "sha256" })))
}

async fn verify(flux: &Flux, repo: RepoId, body: &[u8]) -> FluxResult<Response> {
    let o: ObjSpec = serde_json::from_slice(body).map_err(|e| FluxError::InvalidArgument(e.to_string()))?;
    match flux.meta.lfs_get(repo, std::slice::from_ref(&o.oid)).await?.pop().flatten() {
        Some((_, size)) if size as u64 == o.size => Ok(reply(StatusCode::OK, json!({}))),
        Some(_) => Err(FluxError::InvalidArgument("size mismatch".into())),
        None => Err(FluxError::NotFound("object not found".into())),
    }
}

async fn download(flux: &Arc<Flux>, repo: RepoId, oid: &str) -> FluxResult<Response> {
    let (cid, size) = flux
        .meta
        .lfs_get(repo, &[oid.to_string()])
        .await?
        .pop()
        .flatten()
        .ok_or_else(|| FluxError::NotFound("object not found".into()))?;
    let layout = flux
        .blobs
        .content_layout(&cid)
        .await?
        .ok_or_else(|| FluxError::Internal(anyhow::anyhow!("LFS content {cid} missing")))?;
    let body = match layout {
        ContentLayout::Whole(b) => Body::from(b),
        ContentLayout::Chunked { chunks, .. } => {
            let blobs = flux.blobs.clone();
            let stream = futures::stream::iter(chunks.into_iter().map(|c| c.0))
                .map(move |id| {
                    let blobs = blobs.clone();
                    async move {
                        blobs
                            .get_chunk(&id)
                            .await
                            .map_err(std::io::Error::other)?
                            .ok_or_else(|| std::io::Error::other(format!("chunk {id} missing")))
                    }
                })
                .buffered(4);
            Body::from_stream(stream)
        }
    };
    let mut r = Response::new(body);
    let h = r.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    if let Ok(v) = HeaderValue::from_str(&size.to_string()) {
        h.insert(header::CONTENT_LENGTH, v);
    }
    Ok(r)
}

/// Stream an upload into storage, checking its SHA-256 against the oid.
async fn upload(flux: &Flux, repo: RepoId, oid: &str, headers: &HeaderMap, body: Body) -> FluxResult<Response> {
    if !valid_oid(oid) {
        return Err(FluxError::InvalidArgument("invalid oid".into()));
    }
    let declared: Option<u64> = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse().ok());
    if declared.is_some_and(|n| n > MAX_OBJECT) {
        return Err(FluxError::InvalidArgument("object too large".into()));
    }
    let mut stream = body.into_data_stream();
    let mut sha = sha2::Sha256::new();
    let mut id = ContentId::hasher();
    let mut small: Vec<u8> = Vec::new();
    let mut sink: Option<flux_store::ChunkSink> = None;
    let mut total: u64 = 0;
    while let Some(chunk) = stream.next().await {
        let chunk: Bytes = chunk.map_err(|e| FluxError::InvalidArgument(format!("upload interrupted: {e}")))?;
        total += chunk.len() as u64;
        if total > MAX_OBJECT {
            return Err(FluxError::InvalidArgument("object too large".into()));
        }
        sha.update(&chunk);
        id.update(&chunk);
        match &mut sink {
            Some(s) => s.push(&chunk).await?,
            None => {
                small.extend_from_slice(&chunk);
                if small.len() > CHUNK_THRESHOLD {
                    let mut s = flux.blobs.chunk_sink();
                    s.push(&small).await?;
                    small = Vec::new();
                    sink = Some(s);
                }
            }
        }
    }
    if hex::encode(sha.finalize()) != oid {
        return Err(FluxError::InvalidArgument("the upload does not match its oid (SHA-256)".into()));
    }
    let cid: ContentId = id.finish();
    match sink {
        Some(s) => s.finish(cid, total).await?,
        None => {
            let (stored, _) = flux.blobs.put_contents(vec![(Bytes::from(small), None)]).await?.remove(0);
            debug_assert_eq!(stored, cid);
        }
    }
    flux.meta.lfs_put(repo, oid, total as i64, &cid).await?;
    Ok(StatusCode::OK.into_response())
}

/// `git-lfs-authenticate <repo> <download|upload>` over SSH: the endpoint
/// and a short-lived, repository-bound token for it.
pub async fn ssh_authenticate(flux: &Flux, who: &Principal, repo_name: &str, operation: &str) -> FluxResult<String> {
    let repo = flux.repo(repo_name).await?;
    let need = if operation == "upload" { Role::Write } else { Role::Read };
    let role = flux.authorize(who, repo, need).await?;
    let user = who.user.as_ref().ok_or_else(|| FluxError::Unauthenticated("no user".into()))?;
    let token = crate::auth::new_token();
    let scope = if role >= Role::Write && operation == "upload" { "write" } else { "read" };
    flux.meta
        .add_token_ext(
            user.id,
            &crate::auth::hash_token(&token),
            "git-lfs over ssh",
            "api",
            &format!("{scope} repo:{}", repo.0),
            Some(crate::auth::now_ms() + 3_600_000),
        )
        .await?;
    let base = flux.settings.base_url();
    Ok(json!({
        "href": format!("{base}/{repo_name}.git/info/lfs"),
        "header": { "Authorization": format!("Bearer {token}") },
        "expires_in": 3600,
    })
    .to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes() {
        assert_eq!(route("acme/app.git/info/lfs/objects/batch"), Some(("acme/app".into(), "objects/batch".into())));
        assert_eq!(route("app/info/lfs/objects/abc"), Some(("app".into(), "objects/abc".into())));
        assert_eq!(route("app/-/tree/main"), None);
        assert!(valid_oid(&"a".repeat(64)));
        assert!(!valid_oid(&"A".repeat(64)));
        assert!(!valid_oid("../x"));
    }
}
