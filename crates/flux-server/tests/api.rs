//! The REST API end to end: agent commits, batch reads, branch protection
//! with reviews, code owners and required checks, built-in CI with a
//! runner, webhooks with signatures, issues and notifications, MCP,
//! releases with archives, forks and restricted tokens.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use flux_server::auth::AuthMode;
use serde_json::{Value, json};

struct Api {
    base: String,
    http: reqwest::Client,
}

impl Api {
    async fn call(&self, token: &str, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut req = self.http.request(method, format!("{}/api/v1/{path}", self.base)).bearer_auth(token);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }
    async fn get(&self, t: &str, p: &str) -> (u16, Value) {
        self.call(t, reqwest::Method::GET, p, None).await
    }
    async fn post(&self, t: &str, p: &str, b: Value) -> (u16, Value) {
        self.call(t, reqwest::Method::POST, p, Some(b)).await
    }
    async fn put(&self, t: &str, p: &str, b: Value) -> (u16, Value) {
        self.call(t, reqwest::Method::PUT, p, Some(b)).await
    }
    async fn patch(&self, t: &str, p: &str, b: Value) -> (u16, Value) {
        self.call(t, reqwest::Method::PATCH, p, Some(b)).await
    }
}

/// Retry until `f` returns Some (the reactor works asynchronously).
async fn eventually<T, F, Fut>(what: &str, mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    for _ in 0..100 {
        if let Some(v) = f().await {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

const WORKFLOW: &str =
    "name: CI\non: [push, pull_request]\njobs:\n  test:\n    runs-on: linux\n    steps:\n      - run: echo hi\n";

#[tokio::test]
async fn api_end_to_end() {
    let _ = tracing_subscriber::fmt().with_env_filter("error").try_init();
    let dir = tempfile::tempdir().unwrap();
    let cfg = flux_server::StorageConfig::data_dir(dir.path()).unwrap();
    let flux = flux_server::open_with(&cfg, AuthMode::Token).await.unwrap();
    let flux = Arc::new(Arc::try_unwrap(flux).ok().unwrap().with_settings(flux_server::settings::Settings {
        webhook_allow_private: true,
        secret_key: Some([7u8; 32]),
        ..Default::default()
    }));
    let (_, root) = flux.issue_token("root", true, "t").await.unwrap();
    let (_, alice) = flux.issue_token("alice", false, "t").await.unwrap();
    let (_, bob) = flux.issue_token("bob", false, "t").await.unwrap();
    let (listener, addr) = flux_server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(flux_server::serve(flux.clone(), listener, std::future::pending()));
    let api = Api { base: format!("http://{addr}"), http: reqwest::Client::new() };

    // A webhook receiver.
    let received: Arc<Mutex<Vec<(String, String, Vec<u8>)>>> = Default::default();
    let hook_addr = {
        let received = received.clone();
        let app = axum::Router::new().route(
            "/hook",
            axum::routing::post(move |headers: axum::http::HeaderMap, body: axum::body::Bytes| {
                let received = received.clone();
                async move {
                    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
                    received.lock().unwrap().push((h("x-flux-event"), h("x-hub-signature-256"), body.to_vec()));
                    "ok"
                }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        a
    };

    // ── repository, first commit through the agent write path ──
    let (s, v) = api.post(&root, "repos", json!({ "name": "app", "visibility": "public" })).await;
    assert_eq!(s, 201, "{v}");
    let (s, v) = api
        .post(&root, "repos/app/hooks", json!({ "url": format!("http://{hook_addr}/hook"), "secret": "s3cret", "events": ["*"] }))
        .await;
    assert_eq!(s, 201, "{v}");
    let (s, v) = api
        .post(
            &root,
            "repos/app/changes",
            json!({
                "branch": "main",
                "message": "initial",
                "files": [
                    { "path": "README.md", "content": "# app\n" },
                    { "path": "AGENTS.md", "content": "Run cargo test.\n" },
                    { "path": "src/lib.rs", "content": "pub fn answer() -> u32 {\n    41\n}\n" },
                    { "path": "CODEOWNERS", "content": "/src/ @bob\n" },
                    { "path": ".flux-ci/workflows/ci.yml", "content": WORKFLOW },
                    { "path": "run.sh", "content": "#!/bin/sh\n", "executable": true },
                ],
            }),
        )
        .await;
    assert_eq!(s, 201, "{v}");
    assert_eq!(v["files_changed"], 6);

    // ── reading: batch, context, tree, search, blame ──
    let (s, v) = api
        .post(&alice, "repos/app/files", json!({ "paths": ["README.md", "src/lib.rs", "nope"], "files": [{ "path": "src/lib.rs", "start_line": 2, "end_line": 2 }] }))
        .await;
    assert_eq!(s, 200, "{v}");
    assert_eq!(v["files"][0]["content"], "# app\n");
    assert_eq!(v["files"][2]["error"], "not found");
    assert_eq!(v["files"][3]["content"], "    41");
    let (_, ctx) = api.get(&alice, "repos/app/context").await;
    assert_eq!(ctx["agent_instructions"]["path"], "AGENTS.md");
    assert_eq!(ctx["default_branch"], "main");
    assert_eq!(ctx["workflows"][0], ".flux-ci/workflows/ci.yml");
    assert!(ctx["has_codeowners"].as_bool().unwrap());
    let (_, tree) = api.get(&alice, "repos/app/tree?recursive=1").await;
    assert_eq!(tree["entries"].as_array().unwrap().len(), 6);
    assert!(tree["entries"].as_array().unwrap().iter().any(|e| e["path"] == "run.sh" && e["type"] == "executable"));
    let (_, found) = api.get(&alice, "repos/app/search?q=answer").await;
    assert_eq!(found["matches"][0]["path"], "src/lib.rs");
    let (s, blame) = api.get(&alice, "repos/app/blame/main/src/lib.rs").await;
    assert_eq!(s, 200, "{blame}");
    assert_eq!(blame["lines"], 3);

    // ── branch protection ──
    let (s, v) = api
        .put(
            &root,
            "repos/app/protection",
            json!({ "rules": [{ "pattern": "main", "require_review": true, "required_approvals": 1, "require_codeowners": true, "required_checks": ["CI / test"] }] }),
        )
        .await;
    assert_eq!(s, 200, "{v}");
    let (s, v) = api
        .post(
            &alice,
            "repos/app/changes",
            json!({ "message": "sneaky", "land": "main", "files": [{ "path": "x", "content": "x" }] }),
        )
        .await;
    assert_eq!(s, 403, "direct landing on a protected branch: {v}");

    // An agent-style change: edits + branch + review in one call.
    let (s, v) = api
        .post(
            &alice,
            "repos/app/changes",
            json!({
                "branch": "agent/fix",
                "message": "fix the answer",
                "files": [{ "path": "src/lib.rs", "edits": [{ "old": "41", "new": "42" }] }],
                "review": { "title": "Fix the answer", "body": "cc @bob" },
            }),
        )
        .await;
    assert_eq!(s, 201, "{v}");
    let number = v["review"]["number"].as_i64().unwrap();
    let head = v["changeset"].as_str().unwrap().to_string();
    let (s, v) = api
        .post(&alice, "repos/app/changes", json!({ "message": "x", "branch": "agent/fix2", "files": [{ "path": "src/lib.rs", "edits": [{ "old": "nope", "new": "x" }] }] }))
        .await;
    assert_eq!(s, 422, "edits must match: {v}");

    // Code owners are requested and notified.
    let bob_notes = eventually("bob's review request", || async {
        let (_, n) = api.get(&bob, "notifications").await;
        n.as_array().filter(|a| !a.is_empty()).cloned()
    })
    .await;
    assert!(bob_notes.iter().any(|n| n["number"] == number), "{bob_notes:?}");
    let (_, gate) = api.get(&alice, &format!("repos/app/reviews/{number}/gate")).await;
    let blockers = gate["blockers"].as_array().unwrap();
    assert!(blockers.len() >= 3, "{gate}");

    // ── built-in CI: a runner claims the job and reports success ──
    let (s, runner) = api.post(&root, "runners", json!({ "name": "r1", "labels": ["linux"] })).await;
    assert_eq!(s, 201, "{runner}");
    let rtoken = runner["token"].as_str().unwrap().to_string();
    let mut finished_for_head = false;
    for _ in 0..4 {
        let (s, job) = api.post(&rtoken, "ci/claim", json!({ "labels": ["linux"], "wait": 2 })).await;
        if s == 204 {
            break;
        }
        assert_eq!(s, 200, "{job}");
        assert_eq!(job["spec"]["steps"][0]["run"], "echo hi");
        assert!(job["token"].as_str().unwrap().starts_with("flx_"));
        let id = job["id"].as_i64().unwrap();
        let resp = api
            .http
            .post(format!("{}/api/v1/ci/jobs/{id}/log?seq=1", api.base))
            .bearer_auth(&rtoken)
            .body("hi\n")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let (s, _) = api.post(&rtoken, &format!("ci/jobs/{id}/finish"), json!({ "conclusion": "success", "exit_code": 0 })).await;
        assert_eq!(s, 204);
        if job["changeset"] == head {
            finished_for_head = true;
            let log =
                api.http.get(format!("{}/api/v1/repos/app/jobs/{id}/log", api.base)).bearer_auth(&alice).send().await.unwrap();
            assert_eq!(log.text().await.unwrap(), "hi\n");
        }
    }
    assert!(finished_for_head, "the review's head ran CI");
    let (_, st) = api.get(&alice, &format!("repos/app/commits/{head}/status?wait=5")).await;
    assert_eq!(st["state"], "success", "{st}");

    // Approval by the code owner opens the gate; land squashed.
    let (s, v) = api.post(&alice, &format!("repos/app/reviews/{number}/approve"), json!({})).await;
    assert_eq!(s, 422, "authors cannot approve themselves: {v}");
    let (s, gate) = api.post(&bob, &format!("repos/app/reviews/{number}/approve"), json!({ "body": "lgtm" })).await;
    assert_eq!(s, 200);
    assert_eq!(gate["blockers"], json!([]), "{gate}");
    let (s, v) = api.post(&alice, &format!("repos/app/reviews/{number}/land"), json!({ "method": "squash" })).await;
    assert_eq!(s, 200, "{v}");
    let (_, f) = api.get(&alice, "repos/app/contents/src/lib.rs").await;
    assert!(f["content"].as_str().unwrap().contains("42"));
    let (_, r) = api.get(&alice, &format!("repos/app/reviews/{number}")).await;
    assert_eq!(r["state"], "landed");

    // ── issues, mentions, labels ──
    let (s, issue) =
        api.post(&alice, "repos/app/issues", json!({ "title": "Crash on start", "body": "@bob can you look?" })).await;
    assert_eq!(s, 201, "{issue}");
    let n = issue["number"].as_i64().unwrap();
    assert!(n > number, "issues share the review number space");
    eventually("mention notification", || async {
        let (_, notes) = api.get(&bob, "notifications").await;
        notes.as_array().unwrap().iter().any(|x| x["number"] == n && x["reason"] == "mention").then_some(())
    })
    .await;
    let (s, _) = api.patch(&root, &format!("repos/app/issues/{n}"), json!({ "labels": ["bug"], "assignees": ["bob"] })).await;
    assert_eq!(s, 200);
    let (s, _) = api.post(&bob, &format!("repos/app/issues/{n}/comments"), json!({ "body": "on it" })).await;
    assert_eq!(s, 201);
    let (_, list) = api.get(&alice, "repos/app/issues?label=bug").await;
    assert_eq!(list.as_array().unwrap().len(), 1);
    let (_, v) = api.patch(&bob, &format!("repos/app/issues/{n}"), json!({ "state": "closed" })).await;
    assert_eq!(v["state"], "closed", "{v}");
    let (_, iv) = api.get(&alice, &format!("repos/app/issues/{n}")).await;
    let kinds: Vec<&str> = iv["timeline"].as_array().unwrap().iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"labeled") && kinds.contains(&"comment") && kinds.contains(&"closed"), "{kinds:?}");

    // ── webhooks were delivered and signed ──
    let events = eventually("webhook deliveries", || async {
        let got = received.lock().unwrap().clone();
        let names: Vec<String> = got.iter().map(|(e, _, _)| e.clone()).collect();
        (names.contains(&"issues".to_string())
            && names.contains(&"push".to_string())
            && names.contains(&"pull_request".to_string()))
        .then_some(got)
    })
    .await;
    for (_, sig, body) in &events {
        assert_eq!(sig, &flux_server::hooks::sign("s3cret", body));
    }
    let push = events.iter().find(|(e, _, _)| e == "push").unwrap();
    let push: Value = serde_json::from_slice(&push.2).unwrap();
    assert_eq!(push["ref"], "refs/heads/main");
    assert_eq!(push["after"].as_str().unwrap().len(), 40);

    // ── MCP ──
    let mcp = |body: Value| {
        let api = &api;
        let alice = alice.clone();
        async move {
            let r = api.http.post(format!("{}/api/mcp", api.base)).bearer_auth(alice).json(&body).send().await.unwrap();
            r.json::<Value>().await.unwrap()
        }
    };
    let init =
        mcp(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": { "protocolVersion": "2025-06-18" } })).await;
    assert_eq!(init["result"]["serverInfo"]["name"], "flux");
    let tools = mcp(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" })).await;
    assert!(tools["result"]["tools"].as_array().unwrap().len() >= 20);
    let read = mcp(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": { "name": "read_files", "arguments": { "repo": "app", "paths": ["README.md"] } } })).await;
    assert_eq!(read["result"]["isError"], false, "{read}");
    assert!(read["result"]["content"][0]["text"].as_str().unwrap().contains("# app"));
    let wrote =
        mcp(json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": { "name": "commit_changes", "arguments": {
        "repo": "app", "message": "docs", "branch": "agent/docs", "files": [{ "path": "docs/x.md", "content": "x" }] } } }))
        .await;
    assert_eq!(wrote["result"]["isError"], false, "{wrote}");
    let denied =
        mcp(json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": { "name": "commit_changes", "arguments": {
        "repo": "app", "message": "x", "land": "main", "files": [{ "path": "y", "content": "y" }] } } }))
        .await;
    assert_eq!(denied["result"]["isError"], true, "protection applies to agents too");

    // ── restricted tokens ──
    let (s, t) = api
        .post(&alice, "user/tokens", json!({ "label": "agent", "scope": "write", "repos": ["app"], "refs": ["agent/*"] }))
        .await;
    assert_eq!(s, 201, "{t}");
    let agent = t["token"].as_str().unwrap().to_string();
    let (s, _) = api
        .post(
            &agent,
            "repos/app/changes",
            json!({ "message": "ok", "branch": "agent/a", "files": [{ "path": "a", "content": "a" }] }),
        )
        .await;
    assert_eq!(s, 201);
    let (s, _) = api
        .post(
            &agent,
            "repos/app/changes",
            json!({ "message": "no", "branch": "feature/b", "files": [{ "path": "b", "content": "b" }] }),
        )
        .await;
    assert_eq!(s, 403);
    let (s, _) = api.post(&root, "repos", json!({ "name": "other" })).await;
    assert_eq!(s, 201);
    let (s, _) = api.get(&agent, "repos/other/context").await;
    assert!(s == 404 || s == 403, "bound to its repository");

    // ── releases, archives, forks ──
    let (s, rel) = api.post(&root, "repos/app/releases", json!({ "tag": "v1.0.0", "name": "First", "body": "notes" })).await;
    assert_eq!(s, 201, "{rel}");
    let rid = rel["id"].as_i64().unwrap();
    let up = api
        .http
        .post(format!("{}/api/v1/repos/app/releases/{rid}/assets?name=app.bin", api.base))
        .bearer_auth(&root)
        .body(vec![1u8, 2, 3])
        .send()
        .await
        .unwrap();
    assert_eq!(up.status(), 201);
    let dl = api
        .http
        .get(format!("{}/api/v1/repos/app/releases/download/v1.0.0/app.bin", api.base))
        .bearer_auth(&alice)
        .send()
        .await
        .unwrap();
    assert_eq!(dl.bytes().await.unwrap().as_ref(), &[1u8, 2, 3]);
    for (file, magic) in [("v1.0.0.tar.gz", &[0x1f, 0x8b][..]), ("main.zip", &b"PK"[..])] {
        let a = api.http.get(format!("{}/api/v1/repos/app/archive/{file}", api.base)).bearer_auth(&alice).send().await.unwrap();
        assert_eq!(a.status(), 200, "{file}");
        assert!(a.bytes().await.unwrap().starts_with(magic), "{file}");
    }
    let (_, tags) = api.get(&alice, "repos/app/tags").await;
    assert_eq!(tags[0]["name"], "v1.0.0");
    let (s, fork) = api.post(&bob, "repos/app/forks", json!({})).await;
    assert_eq!(s, 201, "{fork}");
    let (_, ff) = api.get(&bob, "repos/bob/app/contents/src/lib.rs").await;
    assert!(ff["content"].as_str().unwrap().contains("42"), "{ff}");

    // A review from the fork onto the original.
    let (s, v) = api
        .post(&bob, "repos/bob/app/changes", json!({ "message": "from fork", "branch": "feature", "files": [{ "path": "src/lib.rs", "edits": [{ "old": "42", "new": "43" }] }] }))
        .await;
    assert_eq!(s, 201, "{v}");
    let (s, rv) =
        api.post(&bob, "repos/app/reviews", json!({ "title": "From my fork", "head": "feature", "source": "bob/app" })).await;
    assert_eq!(s, 201, "{rv}");
    assert_eq!(rv["source_repo"], "bob/app");
    let (_, diff) = api.get(&alice, &format!("repos/app/reviews/{}/diff", rv["number"])).await;
    assert!(diff.as_str().unwrap().contains("+    43"), "{diff}");

    // ── push protection ──
    let leaked = format!("token = \"{}_{}\"\n", "ghp", "x".repeat(36));
    let (s, v) = api
        .post(
            &alice,
            "repos/app/changes",
            json!({ "message": "oops", "branch": "agent/leak", "files": [{ "path": "cfg.toml", "content": leaked }] }),
        )
        .await;
    assert_eq!(s, 403, "{v}");
    assert!(v["message"].as_str().unwrap().contains("cfg.toml:1: GitHub token"), "{v}");
    let fixture = format!("{} # flux:allow-secret\n", leaked.trim());
    let (s, _) = api
        .post(
            &alice,
            "repos/app/changes",
            json!({ "message": "fixture", "branch": "agent/fixture", "files": [{ "path": "t.toml", "content": fixture }] }),
        )
        .await;
    assert_eq!(s, 201);
    // Admins can turn it off for a repository.
    let (s, _) = api.patch(&alice, "repos/app", json!({ "secret_scanning": false })).await;
    assert_eq!(s, 403, "needs admin");
    let (s, v) = api.patch(&root, "repos/app", json!({ "secret_scanning": false })).await;
    assert_eq!((s, &v["secret_scanning"]), (200, &json!(false)), "{v}");
    let (s, v) = api
        .post(
            &alice,
            "repos/app/changes",
            json!({ "message": "allowed", "branch": "agent/leak", "files": [{ "path": "cfg.toml", "content": leaked }] }),
        )
        .await;
    assert_eq!(s, 201, "{v}");
}

#[tokio::test]
async fn rate_limits() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = flux_server::StorageConfig::data_dir(dir.path()).unwrap();
    let settings = flux_server::settings::Settings { api_rate_per_minute: 20, ..Default::default() };
    let flux = flux_server::open_with_settings(&cfg, AuthMode::Token, Default::default(), settings).await.unwrap();
    let (_, tok) = flux.issue_token("alice", true, "t").await.unwrap();
    let (listener, addr) = flux_server::bind("127.0.0.1:0".parse().unwrap()).await.unwrap();
    tokio::spawn(flux_server::serve(flux.clone(), listener, std::future::pending()));
    let http = reqwest::Client::new();
    let get = |auth: bool| {
        let r = http.get(format!("http://{addr}/api/v1/repos"));
        if auth { r.bearer_auth(&tok) } else { r }
    };
    // Anonymous: a tenth of the limit (at least 10).
    let mut codes = Vec::new();
    for _ in 0..11 {
        codes.push(get(false).send().await.unwrap().status().as_u16());
    }
    assert_eq!(codes.iter().filter(|c| **c == 200).count(), 10, "{codes:?}");
    assert_eq!(*codes.last().unwrap(), 429);
    // Signed in: its own budget, with headers.
    let r = get(true).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["x-ratelimit-limit"], "20");
    assert_eq!(r.headers()["x-ratelimit-remaining"], "19");
}
