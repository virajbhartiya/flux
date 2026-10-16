//! Form posts for issues, releases, CI, review sidebars and repository
//! administration: body parsing (URL-encoded with repeated fields, or
//! multipart with file uploads) and the result redirects.

use axum::body::Body;
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Redirect, Response};
use bytes::Bytes;

use super::pages::{self, url_decode};
use super::view::enc;
use super::{Req, WebError, WebResult, WebState};

/// URL-encoded forms are small; uploads may be large.
const MAX_FORM: usize = 4 << 20;
const MAX_UPLOAD: usize = 512 << 20;

pub struct Upload {
    pub field: String,
    pub filename: String,
    pub content_type: String,
    pub data: Bytes,
}

/// A parsed form: fields in order (names may repeat) and uploaded files.
#[derive(Default)]
pub struct Fields {
    pairs: Vec<(String, String)>,
    pub files: Vec<Upload>,
}

impl Fields {
    pub async fn read(headers: &HeaderMap, body: Body) -> WebResult<Self> {
        let ctype = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let multipart = ctype.starts_with("multipart/form-data");
        let limit = if multipart { MAX_UPLOAD } else { MAX_FORM };
        let data = axum::body::to_bytes(body, limit).await.map_err(|_| WebError::BadRequest("the form is too large".into()))?;
        if multipart { parse_multipart(&ctype, data) } else { Ok(Self::urlencoded(&data)) }
    }

    pub fn urlencoded(data: &[u8]) -> Self {
        let s = String::from_utf8_lossy(data);
        let pairs = s
            .split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| {
                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                (url_decode(k), url_decode(v))
            })
            .collect();
        Self { pairs, files: vec![] }
    }

    /// The first value of a field ("" when absent).
    pub fn get(&self, k: &str) -> &str {
        self.pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str()).unwrap_or("")
    }

    /// The trimmed first value.
    pub fn trim(&self, k: &str) -> &str {
        self.get(k).trim()
    }

    pub fn has(&self, k: &str) -> bool {
        self.pairs.iter().any(|(n, _)| n == k)
    }

    /// Every non-empty value of a repeated field.
    pub fn all(&self, k: &str) -> Vec<String> {
        self.pairs.iter().filter(|(n, v)| n == k && !v.trim().is_empty()).map(|(_, v)| v.trim().to_string()).collect()
    }

    /// A checkbox.
    pub fn checked(&self, k: &str) -> bool {
        matches!(self.get(k), "on" | "1" | "true" | "yes")
    }

    /// A comma, space or newline separated list.
    pub fn list(&self, k: &str) -> Vec<String> {
        self.get(k)
            .split([',', ' ', '\n', '\r'])
            .map(|s| s.trim().trim_start_matches('@'))
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    let first = needle[0];
    let mut i = from;
    while i + needle.len() <= hay.len() {
        match hay[i..hay.len() - needle.len() + 1].iter().position(|b| *b == first) {
            Some(off) => {
                let at = i + off;
                if &hay[at..at + needle.len()] == needle {
                    return Some(at);
                }
                i = at + 1;
            }
            None => return None,
        }
    }
    None
}

/// `name="value"` from a Content-Disposition header.
fn disposition_param(h: &str, key: &str) -> Option<String> {
    h.split(';').map(str::trim).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        (k.trim().eq_ignore_ascii_case(key)).then(|| v.trim().trim_matches('"').replace("\\\"", "\""))
    })
}

/// A small multipart/form-data parser (RFC 7578): enough for browser
/// forms with a few fields and files.
fn parse_multipart(ctype: &str, body: Bytes) -> WebResult<Fields> {
    let bad = || WebError::BadRequest("malformed multipart form".into());
    let boundary = ctype
        .split(';')
        .map(str::trim)
        .find_map(|p| p.strip_prefix("boundary="))
        .map(|b| b.trim_matches('"').to_string())
        .filter(|b| !b.is_empty() && b.len() <= 200)
        .ok_or_else(bad)?;
    let delim = format!("--{boundary}").into_bytes();
    let mut next_delim = b"\r\n".to_vec();
    next_delim.extend_from_slice(&delim);
    let mut out = Fields::default();
    let mut pos = find(&body, &delim, 0).ok_or_else(bad)? + delim.len();
    loop {
        if body[pos..].starts_with(b"--") {
            break;
        }
        if !body[pos..].starts_with(b"\r\n") {
            return Err(bad());
        }
        pos += 2;
        let head_end = find(&body, b"\r\n\r\n", pos).ok_or_else(bad)?;
        let head = String::from_utf8_lossy(&body[pos..head_end]).into_owned();
        let start = head_end + 4;
        let end = find(&body, &next_delim, start).ok_or_else(bad)?;
        let (mut name, mut filename, mut part_type) = (None, None, String::new());
        for line in head.split("\r\n") {
            let Some((k, v)) = line.split_once(':') else { continue };
            if k.trim().eq_ignore_ascii_case("content-disposition") {
                name = disposition_param(v, "name");
                filename = disposition_param(v, "filename");
            } else if k.trim().eq_ignore_ascii_case("content-type") {
                part_type = v.trim().to_string();
            }
        }
        let name = name.ok_or_else(bad)?;
        match filename {
            Some(f) => {
                if !f.is_empty() {
                    out.files.push(Upload { field: name, filename: f, content_type: part_type, data: body.slice(start..end) });
                }
            }
            None => out.pairs.push((name, String::from_utf8_lossy(&body[start..end]).into_owned())),
        }
        pos = end + next_delim.len();
        if pos > body.len() {
            return Err(bad());
        }
    }
    Ok(out)
}

/// Repository POST paths handled here (the rest go through `pages`).
pub fn handles(segs: &[String]) -> bool {
    let s: Vec<&str> = segs.iter().map(String::as_str).collect();
    matches!(
        s.as_slice(),
        ["fork"]
            | ["watch"]
            | ["issues", ..]
            | ["labels", ..]
            | ["milestones", ..]
            | ["releases", ..]
            | ["runs", ..]
            | ["settings", "hooks" | "branches" | "secrets" | "rename" | "delete" | "archive", ..]
            | ["reviews", _, "labels" | "reviewers" | "assignees" | "ready" | "draft" | "suggestion"]
    )
}

pub async fn repo_post(web: &WebState, req: &Req, repo: &str, segs: &[String], f: Fields) -> WebResult {
    let ctx = pages::repo_ctx(web, req, repo).await?;
    let s: Vec<&str> = segs.iter().map(String::as_str).collect();
    match s.as_slice() {
        ["issues", ..] | ["labels", ..] | ["milestones", ..] => super::issues::post(web, req, &ctx, &s, &f).await,
        ["reviews", n, "suggestion"] => pages::apply_suggestion(web, req, &ctx, pages::parse_num(n)?, &f).await,
        ["reviews", n, what] => super::issues::review_sidebar_post(web, req, &ctx, pages::parse_num(n)?, what, &f).await,
        ["releases", ..] => super::releases::post(web, req, &ctx, &s, f).await,
        ["runs", ..] => super::ci::post(web, req, &ctx, &s, &f).await,
        _ => super::repo_admin::post(web, req, &ctx, &s, &f).await,
    }
}

/// Redirect to `to` with a result banner (`?ok=` or `?err=`). Signed-out
/// callers are sent to sign in instead.
pub fn done<E: Into<WebError>>(to: &str, r: Result<String, E>) -> WebResult {
    let sep = if to.contains('?') { "&" } else { "?" };
    Ok(match r.map_err(Into::into) {
        Ok(m) if m.is_empty() => Redirect::to(to).into_response(),
        Ok(m) => Redirect::to(&format!("{to}{sep}ok={}", enc(&m))).into_response(),
        Err(WebError::Unauthorized(_)) => Redirect::to(&format!("/-/login?next={}", enc(to))).into_response(),
        Err(e) => Redirect::to(&format!("{to}{sep}err={}", enc(&error_text(&e)))).into_response(),
    })
}

pub fn error_text(e: &WebError) -> String {
    match e {
        WebError::NotFound(m)
        | WebError::BadRequest(m)
        | WebError::Unauthorized(m)
        | WebError::Forbidden(m)
        | WebError::Conflict(m)
        | WebError::Internal(m) => {
            let mut c = m.chars();
            match c.next() {
                Some(f) => f.to_uppercase().chain(c).collect(),
                None => "Something went wrong".into(),
            }
        }
    }
}

/// Plain redirect.
pub fn to(url: &str) -> Response {
    Redirect::to(url).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multipart() {
        let body = "--XyZ\r\nContent-Disposition: form-data; name=\"name\"\r\n\r\nnotes.txt\r\n--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a b.bin\"\r\nContent-Type: application/octet-stream\r\n\r\n\x00\x01\r\n--\r\n--XyZ--\r\n";
        let f = parse_multipart("multipart/form-data; boundary=XyZ", Bytes::from(body.as_bytes().to_vec())).unwrap();
        assert_eq!(f.get("name"), "notes.txt");
        assert_eq!(f.files.len(), 1);
        assert_eq!(f.files[0].filename, "a b.bin");
        assert_eq!(&f.files[0].data[..], b"\x00\x01\r\n--");
    }

    #[test]
    fn repeated_fields() {
        let f = Fields::urlencoded(b"labels=bug&labels=good+first+issue&x=");
        assert_eq!(f.all("labels"), vec!["bug", "good first issue"]);
        assert!(f.has("x") && f.get("x").is_empty());
    }
}
