//! End to end: migrate a small repository from a mock GitHub API (canned
//! JSON, a local git repository as the clone URL) into an in-process flux
//! server, then re-sync and check nothing is duplicated.

use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use flux_migrate::{GitHub, Options, migrate};
use flux_server::auth::AuthMode;
use serde_json::{Value, json};

fn sh(dir: &Path, script: &str) -> String {
    let out = Command::new("sh")
        .arg("-ec")
        .arg(script)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Ada")
        .env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada")
        .env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "script failed: {script}\n{}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

const FIXTURE: &str = r#"
git init -q -b main .
git config commit.gpgsign false
git config tag.gpgsign false
c() { GIT_AUTHOR_DATE="$1" GIT_COMMITTER_DATE="$1" git commit -q -m "$2"; }
echo hello > README.md && git add -A && c "1700000000 +0000" initial
mkdir -p .github/workflows
printf 'on: [push, schedule]\njobs:\n  t:\n    steps:\n      - uses: actions/checkout@v4\n      - uses: actions/setup-go@v5\n' > .github/workflows/ci.yml
git add -A && c "1700000100 +0000" "add ci"
git checkout -q -b feature
echo 'fn main() {}' > main.rs && git add -A && c "1700000200 +0000" "feature"
git checkout -q main
GIT_AUTHOR_DATE="1700000300 +0000" GIT_COMMITTER_DATE="1700000300 +0000" git merge -q --no-ff feature -m "Merge pull request #1"
git tag -a v1.0 -m "release 1.0"
git update-ref refs/pull/1/head feature
git update-ref refs/pull/1/merge main
git checkout -q -b pr2
echo two > two.txt && git add -A && c "1700000400 +0000" "pr two"
git update-ref refs/pull/2/head pr2
git checkout -q main
git branch -q -D pr2
"#;

struct Mock {
    addr: OnceLock<String>,
    shas: OnceLock<(String, String, String)>,
    log: Mutex<Vec<String>>,
    not_modified: AtomicUsize,
    round: AtomicUsize,
}

fn user(login: &str, id: u64) -> Value {
    json!({ "login": login, "id": id, "type": if login.ends_with("[bot]") { "Bot" } else { "User" } })
}

async fn github(State(m): State<Arc<Mock>>, uri: Uri, headers: HeaderMap) -> Response {
    m.log.lock().unwrap().push(uri.to_string());
    let base = m.addr.get().unwrap().clone();
    let (feature, merge, pr2) = m.shas.get().unwrap().clone();
    let api = format!("{base}/repos/acme/widget");
    let page2 = uri.query().unwrap_or("").contains("page=2");
    let round = m.round.load(Ordering::SeqCst);
    let (ada, bob, carol, bot) = (user("ada", 11), user("bob", 12), user("carol", 13), user("dependabot[bot]", 99));
    let path = uri.path().to_string();
    let ok = |v: Value| {
        // Small lists carry ETags, like GitHub's.
        if path.ends_with("/labels") || path.ends_with("/milestones") || path.ends_with("/releases") {
            let tag = format!("\"{}\"", v.to_string().len());
            if headers.get(header::IF_NONE_MATCH).is_some_and(|h| h.to_str().unwrap() == tag) {
                m.not_modified.fetch_add(1, Ordering::SeqCst);
                return StatusCode::NOT_MODIFIED.into_response();
            }
            return ([(header::ETAG, tag)], axum::Json(v)).into_response();
        }
        axum::Json(v).into_response()
    };
    let v = match uri.path() {
        "/repos/acme/widget" => json!({
            "name": "widget", "full_name": "acme/widget", "owner": user("acme", 1),
            "description": "Widgets", "default_branch": "main", "homepage": "https://widget.dev",
            "topics": ["widgets", "rust"], "visibility": "public", "private": false, "archived": false,
            "has_issues": true, "has_pages": true,
            "html_url": "https://github.com/acme/widget", "clone_url": "https://github.com/acme/widget.git",
        }),
        "/repos/acme/widget/labels" => json!([
            { "name": "bug", "color": "d73a4a", "description": "Something is broken" },
            { "name": "enhancement", "color": "a2eeef", "description": null },
        ]),
        "/repos/acme/widget/milestones" => {
            json!([{ "number": 1, "title": "v1", "state": "open", "due_on": "2024-06-01T00:00:00Z" }])
        }
        "/repos/acme/widget/issues" if !page2 => {
            let body = json!([
                { "number": 1, "title": "Add main", "user": ada, "state": "closed", "created_at": "2024-01-01T00:00:00Z",
                  "updated_at": "2024-01-02T00:00:00Z", "pull_request": { "url": format!("{api}/pulls/1") } },
                { "number": 3, "title": "Crash on start", "body": "Broken since https://github.com/acme/widget/pull/1.",
                  "user": ada, "state": "open", "created_at": "2024-01-03T00:00:00Z", "updated_at": "2024-01-04T00:00:00Z",
                  "labels": [{ "name": "bug", "color": "d73a4a" }], "assignees": [bob], "milestone": { "number": 1 } },
            ]);
            let link = format!("<{api}/issues?state=all&per_page=100&page=2>; rel=\"next\"");
            return ([(header::LINK, link)], axum::Json(body)).into_response();
        }
        "/repos/acme/widget/issues" => json!([
            { "number": 4, "title": "Old bug", "user": bob, "state": "closed", "state_reason": "completed",
              "created_at": "2024-01-05T00:00:00Z", "updated_at": "2024-01-06T00:00:00Z", "closed_at": "2024-01-06T00:00:00Z" },
            { "number": 2, "title": "Two", "user": bob, "state": "open", "created_at": "2024-01-07T00:00:00Z",
              "updated_at": "2024-01-08T00:00:00Z", "pull_request": {} },
        ]),
        "/repos/acme/widget/issues/comments" => {
            let mut c = vec![
                json!({ "id": 100, "user": bob, "body": "Same here", "created_at": "2024-01-03T01:00:00Z", "issue_url": format!("{api}/issues/3") }),
                json!({ "id": 101, "user": bot, "body": "Bump", "created_at": "2024-01-03T02:00:00Z", "issue_url": format!("{api}/issues/3") }),
                json!({ "id": 102, "user": bob, "body": "Nice", "created_at": "2024-01-01T01:00:00Z", "issue_url": format!("{api}/issues/1") }),
            ];
            if round >= 2 {
                c.push(json!({ "id": 103, "user": carol, "body": "Fixed?", "created_at": "2024-02-01T00:00:00Z", "issue_url": format!("{api}/issues/3") }));
            }
            json!(c)
        }
        "/repos/acme/widget/pulls" => json!([
            { "number": 2, "title": "Two", "user": bob, "state": "open", "draft": true, "created_at": "2024-01-07T00:00:00Z",
              "updated_at": "2024-01-08T00:00:00Z", "head": { "sha": pr2, "ref": "pr2" }, "base": { "ref": "main" },
              "requested_reviewers": [carol], "merge_commit_sha": "0".repeat(40) },
            { "number": 5, "title": "From a deleted fork", "user": bob, "state": "closed", "created_at": "2024-01-02T00:00:00Z",
              "updated_at": "2024-01-02T00:00:00Z", "closed_at": "2024-01-02T00:00:00Z",
              "head": { "sha": "d".repeat(40), "ref": "gone" }, "base": { "ref": "main" } },
            { "number": 1, "title": "Add main", "body": "Closes https://github.com/acme/widget/issues/3", "user": ada,
              "state": "closed", "created_at": "2024-01-01T00:00:00Z", "updated_at": "2024-01-02T00:00:00Z",
              "closed_at": "2024-01-02T00:00:00Z", "merged_at": "2024-01-02T00:00:00Z", "merge_commit_sha": merge,
              "head": { "sha": feature, "ref": "feature" }, "base": { "ref": "main" },
              "labels": [{ "name": "enhancement" }], "assignees": [ada] },
        ]),
        "/repos/acme/widget/pulls/comments" => json!([
            { "id": 200, "user": carol, "body": "rename?", "created_at": "2024-01-01T02:00:00Z", "path": "main.rs",
              "line": 1, "side": "RIGHT", "pull_request_url": format!("{api}/pulls/1") },
            { "id": 201, "user": carol, "body": "why remove?", "created_at": "2024-01-01T02:00:01Z", "path": "README.md",
              "line": null, "original_line": 1, "side": "LEFT", "pull_request_url": format!("{api}/pulls/1") },
        ]),
        "/repos/acme/widget/pulls/1/reviews" => json!([
            { "id": 300, "user": carol, "state": "COMMENTED", "body": "", "submitted_at": "2024-01-01T02:00:02Z" },
            { "id": 301, "user": carol, "state": "APPROVED", "body": "LGTM", "submitted_at": "2024-01-01T03:00:00Z" },
        ]),
        "/repos/acme/widget/pulls/2/reviews" => json!([
            { "id": 302, "user": carol, "state": "CHANGES_REQUESTED", "body": "needs tests", "submitted_at": "2024-01-07T05:00:00Z" },
        ]),
        "/repos/acme/widget/pulls/5/reviews" => json!([]),
        "/repos/acme/widget/releases" => json!([{
            "id": 400, "tag_name": "v1.0", "name": "Version 1.0", "body": "First release", "draft": false, "prerelease": false,
            "author": ada, "created_at": "2024-01-02T00:00:00Z", "published_at": "2024-01-02T01:00:00Z",
            "assets": [{ "id": 500, "name": "widget.tar.gz", "size": 5, "content_type": "application/gzip",
                         "url": format!("{api}/releases/assets/500") }],
        }]),
        "/repos/acme/widget/releases/assets/500" => {
            assert_eq!(headers.get(header::ACCEPT).unwrap(), "application/octet-stream");
            return "hello".into_response();
        }
        "/repos/acme/widget/branches/main/protection" => json!({
            "required_status_checks": { "strict": true, "contexts": ["CI / test"] },
            "enforce_admins": { "enabled": false },
            "required_pull_request_reviews": { "required_approving_review_count": 1, "require_code_owner_reviews": true },
            "allow_force_pushes": { "enabled": false },
            "allow_deletions": { "enabled": false },
        }),
        "/repos/acme/widget/hooks" => {
            json!([{ "id": 1, "active": true, "events": ["push"], "config": { "url": "https://ci.example.com/hook" } }])
        }
        "/repos/acme/widget/actions/secrets" => json!({ "total_count": 1, "secrets": [{ "name": "DEPLOY_KEY" }] }),
        "/repos/acme/widget/rulesets" => json!([]),
        "/repos/acme/widget/collaborators" => json!([ada, bob]),
        p if p.starts_with("/users/") => {
            let login = &p["/users/".len()..];
            json!({ "login": login, "name": format!("{login} (GitHub)") })
        }
        _ => return (StatusCode::NOT_FOUND, axum::Json(json!({ "message": "Not Found" }))).into_response(),
    };
    ok(v)
}

struct Api {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl Api {
    async fn get(&self, path: &str) -> Value {
        let r = self.http.get(format!("{}/api/v1/{path}", self.base)).bearer_auth(&self.token).send().await.unwrap();
        assert!(r.status().is_success(), "GET {path}: {}", r.status());
        r.json().await.unwrap()
    }
}

fn kinds(events: &Value) -> Vec<(String, String)> {
    events
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["kind"].as_str().unwrap().to_string(), e["author"].as_str().unwrap().to_string()))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn migrate_and_resync() {
    let _ = tracing_subscriber::fmt().with_env_filter("error").try_init();
    let data = tempfile::tempdir().unwrap();
    let source = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    sh(source.path(), FIXTURE);
    let rev = |r: &str| sh(source.path(), &format!("git rev-parse {r}"));
    let (feature, merge, pr2) = (rev("feature"), rev("main"), rev("refs/pull/2/head"));

    // The mock GitHub API.
    let mock = Arc::new(Mock {
        addr: OnceLock::new(),
        shas: OnceLock::new(),
        log: Mutex::default(),
        not_modified: AtomicUsize::new(0),
        round: AtomicUsize::new(1),
    });
    mock.shas.set((feature.clone(), merge.clone(), pr2.clone())).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    mock.addr.set(format!("http://{}", listener.local_addr().unwrap())).unwrap();
    let app = axum::Router::new().fallback(github).with_state(mock.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // flux.
    let flux =
        flux_server::open_with(&flux_server::StorageConfig::data_dir(data.path()).unwrap(), AuthMode::Token).await.unwrap();
    let (_, root) = flux.issue_token("root", true, "t").await.unwrap();
    let (listener, addr) = flux_server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(flux_server::serve(flux, listener, async {
        let _ = stop_rx.await;
    }));
    let base = format!("http://{addr}");
    let api = Api { base: base.clone(), token: root.clone(), http: reqwest::Client::new() };

    let gh = GitHub::new(mock.addr.get().unwrap(), None).unwrap();
    let mut opts = Options::new("acme/widget", &base, "widget");
    opts.token = Some(root.clone());
    opts.create_users = true;
    opts.cache_dir = Some(cache.path().to_path_buf());
    opts.clone_url = Some(source.path().display().to_string());

    // ── first run ──
    let r = migrate(&gh, &opts).await.unwrap();
    assert!(r.created && !r.incremental);
    assert_eq!(r.count("commits"), 5, "{r:#?}");
    assert_eq!(r.count("refs"), 5, "main, feature, tags/v1.0, pull/1/head, pull/2/head: {r:#?}");
    assert_eq!((r.count("issues"), r.count("reviews"), r.count("labels"), r.count("milestones")), (2, 3, 2, 1));
    assert_eq!((r.count("releases"), r.count("assets"), r.count("protection_rules")), (1, 1, 1));
    assert_eq!(r.count("users"), 3, "ada, bob and carol; no bots: {r:#?}");
    assert_eq!(r.secrets, ["DEPLOY_KEY"]);
    let warned = |s: &str| r.warnings.iter().any(|w| w.contains(s));
    assert!(warned("pull request #5: head commit dddddddddddd"), "{:#?}", r.warnings);
    assert!(warned("webhook https://ci.example.com/hook"));
    assert!(warned("DEPLOY_KEY"));
    assert!(warned("schedule") && warned("actions/setup-go") && !warned("actions/checkout"));
    assert!(warned("GitHub Pages"));
    assert_eq!(r.clone_url, format!("{base}/widget.git"));
    assert!(flux_migrate::next_steps(&r)[0].starts_with("git remote set-url origin "));
    assert!(cache.path().join("127.0.0.1/acme/widget.git/HEAD").exists());
    assert!(cache.path().join("127.0.0.1/acme/widget.state.json").exists());

    // Repository settings.
    let repo = api.get("repos/widget").await;
    assert_eq!(repo["description"], "Widgets");
    assert_eq!(repo["homepage"], "https://widget.dev");
    assert_eq!(repo["topics"], json!(["widgets", "rust"]));
    assert_eq!(repo["default_branch"], "main");
    assert_eq!(repo["visibility"], "public");

    // Refs: branches, tags and pull request heads; no test merges.
    let refs: Vec<String> = api
        .get("repos/widget/refs?all=1")
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap().to_string())
        .collect();
    for want in ["main", "feature", "tags/v1.0", "pull/1/head", "pull/2/head"] {
        assert!(refs.contains(&want.to_string()), "{want} missing from {refs:?}");
    }
    assert!(!refs.iter().any(|r| r.ends_with("/merge")), "{refs:?}");
    let main = api.get("repos/widget/commits/main").await;
    assert_eq!(main["git_sha"], merge, "commit SHAs are preserved");

    // Issues, with links rewritten and comments in order.
    let i3 = api.get("repos/widget/issues/3").await;
    assert_eq!(
        (i3["title"].as_str(), i3["author"].as_str(), i3["state"].as_str()),
        (Some("Crash on start"), Some("ada"), Some("open"))
    );
    assert_eq!(i3["body"], format!("Broken since {base}/widget/-/reviews/1."));
    assert_eq!(
        (i3["labels"].clone(), i3["assignees"].clone(), i3["milestone"].clone()),
        (json!(["bug"]), json!(["bob"]), json!(1))
    );
    assert_eq!(i3["created_at"], 1_704_240_000_000i64);
    let comments = |v: &Value| v["timeline"].as_array().unwrap().iter().filter(|e| e["kind"] == "comment").count();
    assert_eq!(comments(&i3), 2);
    let i4 = api.get("repos/widget/issues/4").await;
    assert_eq!((i4["state"].as_str(), i4["state_reason"].as_str()), (Some("closed"), Some("completed")));

    // Pull requests → reviews, numbers preserved.
    let r1 = api.get("repos/widget/reviews/1").await;
    assert_eq!(r1["state"], "landed");
    assert_eq!(r1["body"], format!("Closes {base}/widget/-/issues/3"));
    assert_eq!(r1["labels"], json!(["enhancement"]));
    let k1 = kinds(&r1["events"]);
    assert!(k1.contains(&("approved".into(), "carol".into())), "{k1:?}");
    assert!(k1.contains(&("comment".into(), "bob".into())), "{k1:?}");
    assert!(k1.contains(&("landed".into(), "ada".into())), "{k1:?}");
    let lines: Vec<(String, i64)> = r1["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["path"].is_string())
        .map(|e| (e["path"].as_str().unwrap().to_string(), e["line"].as_i64().unwrap()))
        .collect();
    assert_eq!(lines, [("main.rs".to_string(), 1), ("README.md".to_string(), -1)]);
    let r2 = api.get("repos/widget/reviews/2").await;
    assert_eq!((r2["state"].as_str(), r2["draft"].as_bool()), (Some("open"), Some(true)));
    assert!(kinds(&r2["events"]).contains(&("changes_requested".into(), "carol".into())));
    assert!(r2["reviewers"].as_array().unwrap().contains(&json!("carol")));
    let r5 = api.get("repos/widget/reviews/5").await;
    assert_eq!(r5["state"], "closed");

    // Labels, releases with assets, protection.
    let labels = api.get("repos/widget/labels").await;
    let names: Vec<&str> = labels.as_array().unwrap().iter().map(|l| l["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["bug", "enhancement"]);
    let rel = api.get("repos/widget/releases/tags/v1.0").await;
    assert_eq!(rel["name"], "Version 1.0");
    assert_eq!(rel["target"].as_str().map(str::len), Some(64), "the tag resolved: {rel}");
    assert_eq!(rel["assets"][0]["name"], "widget.tar.gz");
    assert_eq!(rel["assets"][0]["size"], 5);
    let prot = api.get("repos/widget/protection").await;
    let main_rule = prot["rules"].as_array().unwrap().iter().find(|r| r["pattern"] == "main").unwrap().clone();
    assert_eq!(main_rule["required_approvals"], 1);
    assert_eq!(main_rule["required_checks"], json!(["CI / test"]));
    assert_eq!(main_rule["require_codeowners"], true);

    // ── second run: incremental, nothing new, nothing duplicated ──
    let before_r1 = r1["events"].as_array().unwrap().len();
    let r = migrate(&gh, &opts).await.unwrap();
    assert!(r.incremental);
    for k in ["commits", "refs", "labels", "milestones", "releases", "assets", "users", "protection_rules"] {
        assert_eq!(r.count(k), 0, "{k}: {r:#?}");
    }
    assert_eq!(mock.not_modified.load(Ordering::SeqCst), 3, "labels, milestones and releases came from the ETag cache");
    let log = mock.log.lock().unwrap().clone();
    assert!(log.iter().any(|u| u.starts_with("/repos/acme/widget/issues?") && u.contains("since=")), "{log:?}");
    assert!(r.warnings.is_empty(), "warnings are reported once: {:#?}", r.warnings);
    assert_eq!(comments(&api.get("repos/widget/issues/3").await), 2);
    assert_eq!(api.get("repos/widget/reviews/1").await["events"].as_array().unwrap().len(), before_r1);

    // ── third run: a new commit, a new comment, a deleted branch ──
    sh(source.path(), "echo more >> README.md && git commit -qam more && git branch -q -D feature");
    mock.round.store(2, Ordering::SeqCst);
    let r = migrate(&gh, &opts).await.unwrap();
    assert_eq!((r.count("commits"), r.count("refs"), r.count("refs_deleted")), (1, 1, 1), "{r:#?}");
    assert_eq!(comments(&api.get("repos/widget/issues/3").await), 3);
    assert_eq!(api.get("repos/widget/reviews/1").await["events"].as_array().unwrap().len(), before_r1);
    let refs = api.get("repos/widget/refs?all=1").await;
    assert!(!refs.as_array().unwrap().iter().any(|r| r["name"] == "feature"), "{refs}");

    let _ = stop_tx.send(());
    server.await.unwrap().unwrap();
}
