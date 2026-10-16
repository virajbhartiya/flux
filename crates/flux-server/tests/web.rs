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

// ───────────── collaboration pages ─────────────

async fn get_as(app: &axum::Router, uri: &str, token: &str) -> (StatusCode, String, axum::http::HeaderMap) {
    let req = Request::get(uri).header("authorization", format!("Bearer {token}")).body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), 64 << 20).await.unwrap();
    (status, String::from_utf8_lossy(&body).into_owned(), headers)
}

/// POST a form; returns the status and the redirect target.
async fn post_as(app: &axum::Router, uri: &str, token: &str, ctype: &str, body: Vec<u8>) -> (StatusCode, String) {
    let req = Request::post(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", ctype)
        .header("host", "localhost")
        .header("origin", "http://localhost")
        .body(Body::from(body))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let loc = resp.headers().get("location").map(|v| v.to_str().unwrap().to_string()).unwrap_or_default();
    (resp.status(), loc)
}

async fn post_form(app: &axum::Router, uri: &str, token: &str, form: &str) -> (StatusCode, String) {
    let (status, loc) = post_as(app, uri, token, "application/x-www-form-urlencoded", form.as_bytes().to_vec()).await;
    assert!(!loc.contains("err="), "{uri}: {}", flux_server_decode(&loc));
    (status, loc)
}

fn flux_server_decode(s: &str) -> String {
    s.replace("%20", " ").replace("%2C", ",").replace("%3A", ":")
}

async fn commit_files(
    flux: &Arc<Flux>,
    repo: flux_store::RepoId,
    parents: Vec<ChangesetId>,
    files: &[(&str, &str)],
    msg: &str,
) -> ChangesetId {
    let mut fc = BTreeMap::new();
    for (p, text) in files {
        let (id, m) = flux.put_content(text.as_bytes()).await.unwrap();
        fc.insert(
            RepoPath::try_from(*p).unwrap(),
            FileChange::Change { content_id: id, size: m.size, kind: FileKind::Regular, copy_from: None },
        );
    }
    let cs = Changeset {
        parents,
        author: b"Root <root@x.org>".to_vec(),
        author_time: Timestamp { unix_ms: 1_700_000_000_000, tz_offset_min: 0 },
        committer: None,
        committer_time: None,
        message: format!("{msg}\n").into_bytes(),
        file_changes: fc,
        extra: BTreeMap::new(),
    };
    ChangesetId::from_slice(&flux.create_changeset(repo, cs).await.unwrap().id).unwrap()
}

#[tokio::test]
async fn collaboration_pages() {
    use base64::Engine as _;
    let meta = flux_store::open_meta("sqlite::memory:").await.unwrap();
    let flux = Arc::new(Flux::new(meta, BlobStore::memory()).with_auth_mode(flux_server::auth::AuthMode::Token).with_settings(
        flux_server::settings::Settings {
            webhook_allow_private: true,
            secret_key: Some([7u8; 32]),
            ssh_host: Some("git.example.com:2222".into()),
            ..Default::default()
        },
    ));
    let (_, root) = flux.issue_token("root", true, "t").await.unwrap();
    let (_, bob) = flux.issue_token("bob", false, "t").await.unwrap();
    let root_p = flux.principal(Some(&root)).await.unwrap();
    let repo = flux.create_repo_as(&root_p, "root/demo", Some("public")).await.unwrap();
    let workflow =
        "name: CI\non:\n  push:\n  workflow_dispatch:\njobs:\n  test:\n    runs-on: linux\n    steps:\n      - run: echo hi\n";
    let main = commit_files(
        &flux,
        repo,
        vec![],
        &[("README.md", "# Demo\n"), ("src/lib.rs", "fn one() {}\nfn two() {}\n"), (".flux-ci/workflows/ci.yml", workflow)],
        "Initial",
    )
    .await;
    flux.update_ref(repo, b"main", RefExpect::Absent, Some(main), "root", "init".into()).await.unwrap();
    let feature =
        commit_files(&flux, repo, vec![main], &[("src/lib.rs", "fn one() {}\nfn two() {}\nfn three() {}\n")], "Add three").await;
    flux.update_ref(repo, b"feature", RefExpect::Absent, Some(feature), "bob", "push".into()).await.unwrap();
    let review = flux.create_review(repo, "Add three", "Adds `three`.", "bob", feature, b"main").await.unwrap();
    let app = flux_server::app(flux.clone(), ServeOptions { web: true });
    const FORM: &str = "application/x-www-form-urlencoded";

    // Issues: open, comment, label, assign.
    let (status, loc) = post_form(&app, "/root/demo/-/issues", &bob, "title=Crash+on+start&body=It+**crashes**").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let n: i64 = loc.rsplit('/').next().unwrap().parse().expect(&loc);
    let issue = format!("/root/demo/-/issues/{n}");
    post_form(&app, &format!("{issue}/comment"), &bob, "body=Still+happens+on+1.2").await;
    post_form(&app, "/root/demo/-/labels", &root, "name=bug&color=%23d73a4a&description=Broken").await;
    post_form(&app, &format!("{issue}/labels"), &root, "labels=&labels=bug").await;
    post_form(&app, &format!("{issue}/assignees"), &root, "names=bob").await;
    post_form(&app, "/root/demo/-/milestones", &root, "title=v1.0&due=2030-01-31&description=First").await;
    post_form(&app, &format!("{issue}/milestone"), &root, "milestone=1").await;
    let v = flux.issue_view(&root_p, repo, n).await.unwrap();
    assert_eq!(v.issue.labels, vec!["bug"]);
    assert_eq!(v.issue.assignees, vec!["bob"]);
    assert_eq!(v.issue.milestone, Some(1));
    assert!(v.timeline.iter().any(|e| e.kind == "comment" && e.body.contains("1.2")));
    // Readers can't label; the error comes back as a banner.
    let (status, loc) = post_as(&app, &format!("{issue}/labels"), &bob, FORM, b"labels=bug".to_vec()).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(loc.contains("err="), "{loc}");

    // Releases with an uploaded asset, archives, a fork.
    let multipart = "--B\r\nContent-Disposition: form-data; name=\"tag\"\r\n\r\nv1.0\r\n--B\r\nContent-Disposition: form-data; name=\"target\"\r\n\r\nmain\r\n--B\r\nContent-Disposition: form-data; name=\"body\"\r\n\r\nFirst *release*\r\n--B\r\nContent-Disposition: form-data; name=\"file\"; filename=\"demo.bin\"\r\nContent-Type: application/octet-stream\r\n\r\nBINARY\x00DATA\r\n--B--\r\n";
    let (status, loc) =
        post_as(&app, "/root/demo/-/releases", &root, "multipart/form-data; boundary=B", multipart.as_bytes().to_vec()).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(loc.contains("ok="), "{loc}");
    let (status, body, headers) = get_as(&app, "/root/demo/-/releases/download/v1.0/demo.bin", &bob).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_bytes(), b"BINARY\x00DATA");
    assert!(headers["content-disposition"].to_str().unwrap().starts_with("attachment"));
    let (status, body, _) = get_as(&app, "/root/demo/-/archive/main.zip", &bob).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.as_bytes().starts_with(b"PK"));
    let (status, _, headers) = get_as(&app, "/root/demo/-/archive/tags/v1.0.tar.gz", &bob).await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers["content-disposition"].to_str().unwrap().contains("demo-v1.0.tar.gz"));
    let (status, loc) = post_form(&app, "/root/demo/-/fork", &bob, "").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(loc.starts_with("/bob/demo"), "{loc}");
    assert!(flux.repo("bob/demo").await.is_ok());

    // CI: a manual run, its page and its log.
    let (status, loc) =
        post_form(&app, "/root/demo/-/runs/dispatch", &root, "workflow=.flux-ci%2Fworkflows%2Fci.yml&ref=main").await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let run: i64 = loc.rsplit('/').next().unwrap().parse().expect(&loc);
    let job = flux.meta.ci_jobs(run).await.unwrap()[0].id;
    flux.meta.append_ci_log(job, 1, b"\x1b[32mhello\x1b[0m from the runner\n").await.unwrap();
    let (status, body, headers) = get_as(&app, &format!("/root/demo/-/runs/{run}/jobs/{job}/log?after=0"), &bob).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "hello from the runner\n");
    assert_eq!(headers["x-flux-log-seq"], "1");

    // Reviews: a suggestion applied, approval, squash landing.
    let rv = format!("/root/demo/-/reviews/{}", review.number);
    let body =
        "path=src%2Flib.rs&line=3&body=Name+it+better%3A%0A%60%60%60suggestion%0Afn+three_better%28%29+%7B%7D%0A%60%60%60%0A";
    post_form(&app, &format!("{rv}/comment"), &root, body).await;
    let seq =
        flux.review_detail(repo, review.number).await.unwrap().events.iter().rev().find(|e| e.kind == "comment").unwrap().seq;
    let (_, page_body, _) = get_as(&app, &format!("{rv}/files"), &root).await;
    assert!(page_body.contains("Apply suggestion"), "suggestion button");
    post_form(&app, &format!("{rv}/suggestion"), &root, &format!("seq={seq}")).await;
    let head = flux.review(repo, review.number).await.unwrap().head;
    assert_ne!(head, feature);
    let e = flux.entry_at(repo, &head, &RepoPath::try_from("src/lib.rs").unwrap()).await.unwrap().unwrap();
    assert_eq!(
        flux.blobs.get_content(&e.content_id().unwrap()).await.unwrap().unwrap(),
        b"fn one() {}\nfn two() {}\nfn three_better() {}\n"
    );
    assert_eq!(flux.meta.get_ref(repo, b"feature").await.unwrap().unwrap().target, head, "the branch follows");
    let (_, page_body, _) = get_as(&app, &format!("{rv}/files"), &root).await;
    assert!(page_body.contains("Applied"));
    post_form(&app, &format!("{rv}/labels"), &root, "labels=bug").await;
    post_form(&app, &format!("{rv}/reviewers"), &root, "names=bob").await;
    let (status, loc) = post_as(&app, &format!("{rv}/approve"), &bob, FORM, b"body=".to_vec()).await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    assert!(loc.contains("err="), "the author can't approve: {loc}");
    post_form(&app, &format!("{rv}/approve"), &root, "body=LGTM").await;
    let (_, page_body, _) = get_as(&app, &rv, &root).await;
    assert!(page_body.contains("Squash and land") && page_body.contains("Approved by root"), "gate shown");
    post_form(&app, &format!("{rv}/land"), &root, "method=squash").await;
    let landed = flux.review(repo, review.number).await.unwrap();
    assert_eq!(landed.state, "landed");
    let main_now = flux.meta.get_ref(repo, b"main").await.unwrap().unwrap().target;
    assert_eq!(flux.load_one_arc(repo, &main_now).await.unwrap().changeset.parents, vec![main], "one squashed changeset");

    // Settings: webhook, protection, secret.
    let (status, loc) = post_form(
        &app,
        "/root/demo/-/settings/hooks",
        &root,
        "url=http%3A%2F%2F127.0.0.1%3A9%2Fhook&secret=s&events=push&events=issues&active=1",
    )
    .await;
    assert_eq!(status, StatusCode::SEE_OTHER);
    let hooks = flux.list_hooks(&root_p, flux_server::hooks::Scope::Repo(repo)).await.unwrap();
    assert_eq!(hooks.len(), 1);
    assert_eq!(hooks[0].events, vec!["push", "issues"]);
    assert!(
        loc.ends_with(&format!("/settings/hooks/{}?ok=Webhook%20added%3B%20a%20ping%20is%20on%20its%20way", hooks[0].id))
            || loc.contains("ok="),
        "{loc}"
    );
    post_form(
        &app,
        "/root/demo/-/settings/branches",
        &root,
        "action=add&pattern=release%2F**&require_review=1&required_approvals=2&required_checks=ci%2Ftest",
    )
    .await;
    let rules = flux.repo_config(repo).await.unwrap().branch_rules.clone().unwrap();
    assert!(rules.iter().any(|r| r.pattern == "release/**" && r.required_approvals == 2 && r.required_checks == ["ci/test"]));
    post_form(&app, "/root/demo/-/settings/secrets", &root, "name=DEPLOY_TOKEN&value=s3cr3t-value").await;
    assert_eq!(flux.list_secrets(&root_p, ("repo", repo.0)).await.unwrap()[0].name, "DEPLOY_TOKEN");
    let (status, _) = post_as(&app, "/root/demo/-/settings/secrets", &bob, FORM, b"name=X&value=y".to_vec()).await;
    assert_eq!(status, StatusCode::SEE_OTHER, "refused with a banner");
    assert_eq!(flux.list_secrets(&root_p, ("repo", repo.0)).await.unwrap().len(), 1);

    // SSH keys and notifications.
    let mut blob = vec![0, 0, 0, 11];
    blob.extend_from_slice(b"ssh-ed25519");
    blob.extend_from_slice(&[0, 0, 0, 32]);
    blob.extend_from_slice(&[9u8; 32]);
    let key = format!("ssh-ed25519 {} bob@laptop", base64::engine::general_purpose::STANDARD.encode(&blob));
    post_form(
        &app,
        "/-/settings/keys",
        &bob,
        &format!("title=Laptop&key={}", key.replace('+', "%2B").replace('/', "%2F").replace(' ', "+").replace('=', "%3D")),
    )
    .await;
    let bob_id = flux.meta.get_user("bob").await.unwrap().unwrap().id;
    assert_eq!(flux.meta.list_ssh_keys(bob_id).await.unwrap().len(), 1);
    let (_, body, _) = get_as(&app, "/root/demo", &bob).await;
    assert!(body.contains("unread-dot"), "bell shows unread notifications");
    post_form(&app, "/root/demo/-/watch", &bob, "state=watching&next=%2Fdemo").await;
    assert_eq!(flux.meta.subscription(bob_id, repo, 0).await.unwrap().as_deref(), Some("subscribed"));

    // Every new page renders.
    for (uri, token, want) in [
        ("/root/demo/-/issues".to_string(), &bob, "Crash on start"),
        ("/root/demo/-/issues?state=closed".into(), &bob, "No closed issues"),
        ("/root/demo/-/issues?label=bug&q=crash".into(), &bob, "Crash on start"),
        (issue.clone(), &bob, "Still happens on 1.2"),
        ("/root/demo/-/issues/new".into(), &bob, "New issue"),
        ("/root/demo/-/labels".into(), &root, "Broken"),
        ("/root/demo/-/milestones".into(), &root, "v1.0"),
        ("/root/demo/-/releases".into(), &bob, "demo.bin"),
        ("/root/demo/-/releases/1/edit".into(), &root, "Upload files"),
        ("/root/demo/-/releases/new".into(), &root, "New release"),
        ("/root/demo/-/runs".into(), &bob, "CI"),
        (format!("/root/demo/-/runs/{run}"), &bob, "hello from the runner"),
        ("/root/demo/-/reviews?state=landed".into(), &bob, "Add three"),
        (rv.clone(), &bob, "Reviewers"),
        (format!("/root/demo/-/commit/{}", main.to_hex()), &bob, "Initial"),
        ("/root/demo/-/commits/main".into(), &bob, "Initial"),
        ("/root/demo/-/settings".into(), &root, "Danger zone"),
        ("/root/demo/-/settings/hooks".into(), &root, "127.0.0.1:9/hook"),
        (format!("/root/demo/-/settings/hooks/{}", hooks[0].id), &root, "Recent deliveries"),
        ("/root/demo/-/settings/branches".into(), &root, "release/**"),
        ("/root/demo/-/settings/secrets".into(), &root, "DEPLOY_TOKEN"),
        ("/-/notifications".into(), &bob, "Crash on start"),
        ("/-/settings/keys".into(), &bob, "SHA256:"),
        ("/root/demo".into(), &bob, "ssh://git@git.example.com:2222/root/demo.git"),
    ] {
        let (status, body, headers) = get_as(&app, &uri, token).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert!(body.contains(want), "{uri}: expected {want:?}");
        assert!(headers.get("content-security-policy").is_some(), "{uri}");
        assert!(!body.contains("s3cr3t-value"), "{uri}: secret value leaked");
        assert!(!body.contains(" style=\""), "{uri}: inline style");
    }
    // Admin pages stay closed to others.
    let (status, _, _) = get_as(&app, "/root/demo/-/settings/secrets", &bob).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    post_form(&app, "/-/notifications/read", &bob, "").await;
    assert_eq!(flux.meta.unread_count(bob_id).await.unwrap(), 0);
}
