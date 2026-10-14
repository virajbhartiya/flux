//! Web UI smoke tests: every page renders, security headers are present,
//! repository content can't inject markup.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use flux_core::{Changeset, ChangesetId, FileChange, FileKind, RepoPath, Timestamp};
use flux_server::{Flux, ServeOptions};
use flux_store::{BlobStore, RefExpect};
use tower::ServiceExt;

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, String, axum::http::HeaderMap) {
    let resp = app.clone().oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), 64 << 20).await.unwrap();
    (status, String::from_utf8_lossy(&body).into_owned(), headers)
}

#[tokio::test]
async fn pages_render() {
    let meta = flux_store::open_meta("sqlite::memory:").await.unwrap();
    let flux = Arc::new(Flux::new(meta, BlobStore::memory()));
    let repo = flux.create_repo("demo").await.unwrap();
    let mut fc = BTreeMap::new();
    for (p, text) in [
        ("README.md", "# Demo\n\n<script>alert(1)</script>\n\n[x](javascript:alert(1))\n"),
        ("src/main.rs", "fn main() {\n    println!(\"<b>hi</b>\");\n}\n"),
        ("docs/guide.md", "Guide"),
    ] {
        let (id, m) = flux.put_content(text.as_bytes()).await.unwrap();
        fc.insert(
            RepoPath::try_from(p).unwrap(),
            FileChange::Change { content_id: id, size: m.size, kind: FileKind::Regular, copy_from: None },
        );
    }
    let cs = Changeset {
        parents: vec![],
        author: b"Ada <ada@x.org>".to_vec(),
        author_time: Timestamp { unix_ms: 1_700_000_000_000, tz_offset_min: 0 },
        committer: None,
        committer_time: None,
        message: b"Initial <commit>\n".to_vec(),
        file_changes: fc,
        extra: BTreeMap::new(),
    };
    let id = ChangesetId::from_slice(&flux.create_changeset(repo, cs).await.unwrap().id).unwrap();
    flux.update_ref(repo, b"main", RefExpect::Absent, Some(id), "t", "init".into()).await.unwrap();
    flux.lastmod(repo, &id).await.unwrap();
    flux.create_review(repo, "A review", "**bold**", "ada", id, b"main").await.unwrap();

    let app = flux_server::app(flux.clone(), ServeOptions { web: true });
    let hex = id.to_hex();
    for uri in [
        "/".to_string(),
        "/demo".into(),
        "/demo/-/tree/main".into(),
        "/demo/-/tree/main/src".into(),
        "/demo/-/blob/main/src/main.rs".into(),
        "/demo/-/blob/main/README.md".into(),
        "/demo/-/blob/main/README.md?plain=1".into(),
        "/demo/-/blame/main/src/main.rs".into(),
        "/demo/-/commits/main".into(),
        "/demo/-/commits/main/src/main.rs".into(),
        format!("/demo/-/commit/{hex}"),
        format!("/demo/-/compare/main...{hex}"),
        "/demo/-/branches".into(),
        "/demo/-/activity".into(),
        "/demo/-/search?q=println".into(),
        "/demo/-/reviews".into(),
        "/demo/-/reviews/1".into(),
        "/demo/-/reviews/1/files".into(),
        "/demo/-/reviews/new?head=main".into(),
        "/-/login".into(),
        "/demo/-/edit/main/src/main.rs".into(),
        "/demo/-/new/main/src".into(),
        "/demo/-/settings".into(),
    ] {
        let (status, body, headers) = get(&app, &uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert!(body.contains("</html>"), "{uri}");
        assert!(headers.get("content-security-policy").is_some(), "{uri}");
        assert!(headers.get("server-timing").is_some(), "{uri}");
        assert!(!body.contains("<script>alert"), "{uri}: unescaped repository content");
        assert!(!body.to_ascii_lowercase().contains("href=\"javascript"), "{uri}: unsafe link");
    }
    let (_, body, _) = get(&app, "/demo/-/search?q=println").await;
    assert!(body.contains("<mark>println</mark>"));
    let (_, body, _) = get(&app, "/demo/-/blob/main/src/main.rs").await;
    assert!(body.contains("hl-"), "highlighted");
    assert!(body.contains("&lt;b&gt;hi&lt;/b&gt;"));

    let (status, body, headers) = get(&app, "/demo/-/raw/main/src/main.rs").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.starts_with("fn main"));
    assert!(headers["content-security-policy"].to_str().unwrap().contains("sandbox"));

    let (status, body, _) = get(&app, "/demo/-/files/main").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("src/main.rs"));

    for uri in ["/nope", "/demo/-/blob/main/missing.txt", "/demo/-/reviews/99", "/demo/-/tree/no-such-rev"] {
        let (status, _, _) = get(&app, uri).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
    // Edit a file in the browser, committing directly to main.
    let form = format!("base={hex}&branch=main&path=src%2Fmain.rs&content=fn+main%28%29+%7B%7D%0A&message=Simplify&mode=direct");
    let resp = app
        .clone()
        .oneshot(
            Request::post("/demo/-/edit")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("host", "localhost")
                .header("origin", "http://localhost")
                .body(Body::from(form))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let main = flux.meta.get_ref(repo, b"main").await.unwrap().unwrap().target;
    let e = flux.entry_at(repo, &main, &RepoPath::try_from("src/main.rs").unwrap()).await.unwrap().unwrap();
    assert_eq!(flux.blobs.get_content(&e.content_id().unwrap()).await.unwrap().unwrap(), b"fn main() {}\n");

    // Cross-origin form posts are refused.
    let resp = app
        .clone()
        .oneshot(
            Request::post("/-/repos")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("host", "localhost")
                .header("origin", "http://evil.example")
                .body(Body::from("name=x"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    let (_, body, _) = get(&app, "/-/repos.json").await;
    assert!(body.contains("\"demo\""));
    let (_, body, _) = get(&app, "/demo/-/refs.json").await;
    assert!(body.contains("\"main\""));

    // Inline review comment, then it renders in the review's diff.
    let resp = app
        .clone()
        .oneshot(
            Request::post("/demo/-/reviews/1/comment")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("host", "localhost")
                .header("origin", "http://localhost")
                .body(Body::from("path=src%2Fmain.rs&line=1&body=Nice+line"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let (_, body, _) = get(&app, "/demo/-/reviews/1").await;
    assert!(body.contains("Nice line") && body.contains("src/main.rs:1"));

    // Settings round trip.
    let resp = app
        .clone()
        .oneshot(
            Request::post("/demo/-/settings")
                .header("content-type", "application/x-www-form-urlencoded")
                .header("host", "localhost")
                .header("origin", "http://localhost")
                .body(Body::from("description=A+demo+repo&default_branch=main&acl=secrets+%7C+admins+%7C+admins"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let cfg = flux.repo_config(repo).await.unwrap();
    assert_eq!(cfg.description, "A demo repo");
    assert_eq!(cfg.acl[0].path, "secrets");
    let (_, body, _) = get(&app, "/demo").await;
    assert!(body.contains("A demo repo"));

    let (status, body, _) = get(&app, "/-/metrics").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("flux_requests_total"));
}
